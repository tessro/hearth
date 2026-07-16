//! Signed task references (§4.4). Every externally visible task handle is an
//! opaque, HMAC-signed, expiring token that self-routes to its target VM and
//! is bound to its initiator. Guests and UIs treat refs as opaque; only agentd
//! holds the key. Refs route; the delegation ledger authorizes.

use crate::{b64, hmac};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskRefClaims {
    pub v: u32,
    /// Target service (VM) name — resolves routing without scanning guests.
    pub target: String,
    pub task_id: String,
    /// Who may present this ref: a service name, or "ui" for HTTP clients.
    pub initiator: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub initiator_thread: Option<String>,
    /// Unix seconds.
    pub expiry: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefError {
    Malformed,
    BadSignature,
    Expired,
}

impl std::fmt::Display for RefError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            RefError::Malformed => "task ref is malformed",
            RefError::BadSignature => "task ref signature is invalid",
            RefError::Expired => "task ref is expired",
        };
        f.write_str(s)
    }
}

impl std::error::Error for RefError {}

pub fn sign(claims: &TaskRefClaims, key: &[u8]) -> String {
    let payload = serde_json::to_vec(claims).expect("task ref claims serialize");
    let mac = hmac::hmac_sha256(key, &payload);
    format!("{}.{}", b64::encode(&payload), b64::encode(&mac))
}

/// Verify signature (against the current key, then the previous key, to allow
/// rotation) and expiry. `now` is unix seconds.
pub fn verify(token: &str, keys: &[&[u8]], now: i64) -> Result<TaskRefClaims, RefError> {
    let (payload_b64, mac_b64) = token.split_once('.').ok_or(RefError::Malformed)?;
    let payload = b64::decode(payload_b64).ok_or(RefError::Malformed)?;
    let mac = b64::decode(mac_b64).ok_or(RefError::Malformed)?;
    let signed = keys
        .iter()
        .any(|key| hmac::constant_time_eq(&hmac::hmac_sha256(key, &payload), &mac));
    if !signed {
        return Err(RefError::BadSignature);
    }
    let claims: TaskRefClaims =
        serde_json::from_slice(&payload).map_err(|_| RefError::Malformed)?;
    if claims.expiry < now {
        return Err(RefError::Expired);
    }
    Ok(claims)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn claims() -> TaskRefClaims {
        TaskRefClaims {
            v: 1,
            target: "web-a".into(),
            task_id: "01J5TASK".into(),
            initiator: "boss".into(),
            initiator_thread: Some("th-1".into()),
            expiry: 2_000_000_000,
        }
    }

    #[test]
    fn signs_and_verifies_round_trip() {
        let token = sign(&claims(), b"k1");
        let verified = verify(&token, &[b"k1"], 1_000_000_000).unwrap();
        assert_eq!(verified, claims());
    }

    #[test]
    fn previous_key_still_verifies_during_rotation() {
        let token = sign(&claims(), b"old-key");
        assert!(verify(&token, &[b"new-key", b"old-key"], 0).is_ok());
        assert_eq!(
            verify(&token, &[b"new-key"], 0),
            Err(RefError::BadSignature)
        );
    }

    #[test]
    fn a_guest_cannot_mint_or_retarget_refs() {
        let token = sign(&claims(), b"k1");
        // Retarget: flip the payload, keep the mac.
        let (_, mac) = token.split_once('.').unwrap();
        let mut forged = claims();
        forged.target = "victim".into();
        let forged_payload = b64::encode(&serde_json::to_vec(&forged).unwrap());
        assert_eq!(
            verify(&format!("{forged_payload}.{mac}"), &[b"k1"], 0),
            Err(RefError::BadSignature)
        );
        // Mint: sign with a guessed key.
        let minted = sign(&forged, b"guessed");
        assert_eq!(verify(&minted, &[b"k1"], 0), Err(RefError::BadSignature));
    }

    #[test]
    fn expired_refs_are_rejected() {
        let token = sign(&claims(), b"k1");
        assert_eq!(
            verify(&token, &[b"k1"], 2_000_000_001),
            Err(RefError::Expired)
        );
    }
}
