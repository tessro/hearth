//! Signed task-ref helpers (docs/agent-plane.md §4.4), wrapping the proto's
//! sign/verify with agentd's key material and presenter checks. A ref is
//! opaque to guests and UIs; only agentd holds the key.

use anyhow::{bail, Result};
use hearth_agent_proto::taskref::{self, RefError, TaskRefClaims};

#[derive(Clone)]
pub struct RefKeys {
    pub current: Vec<u8>,
    pub previous: Option<Vec<u8>>,
    pub ttl_secs: i64,
}

impl RefKeys {
    fn verify_keys(&self) -> Vec<&[u8]> {
        let mut keys: Vec<&[u8]> = vec![&self.current];
        if let Some(prev) = &self.previous {
            keys.push(prev);
        }
        keys
    }

    pub fn mint(
        &self,
        target: &str,
        task_id: &str,
        initiator: &str,
        initiator_thread: Option<&str>,
        now: i64,
    ) -> String {
        let claims = TaskRefClaims {
            v: 1,
            target: target.to_string(),
            task_id: task_id.to_string(),
            initiator: initiator.to_string(),
            initiator_thread: initiator_thread.map(str::to_string),
            expiry: now + self.ttl_secs,
        };
        taskref::sign(&claims, &self.current)
    }

    pub fn verify(&self, token: &str, now: i64) -> Result<TaskRefClaims> {
        match taskref::verify(token, &self.verify_keys(), now) {
            Ok(claims) => Ok(claims),
            Err(RefError::Expired) => bail!("ref.expired: task ref has expired"),
            Err(RefError::BadSignature) => bail!("ref.invalid: task ref signature is invalid"),
            Err(RefError::Malformed) => bail!("ref.invalid: task ref is malformed"),
        }
    }

    /// Verify a ref and confirm the presenter is entitled to it: the ref's
    /// initiator VM, or a UI bearing the HTTP token (`presenter = "ui"`).
    pub fn verify_presenter(&self, token: &str, presenter: &str, now: i64) -> Result<TaskRefClaims> {
        let claims = self.verify(token, now)?;
        if presenter == "ui" && claims.initiator == "ui" {
            return Ok(claims);
        }
        if claims.initiator != presenter {
            bail!(
                "ref.forbidden: presenter {presenter:?} is not the ref's initiator {:?}",
                claims.initiator
            );
        }
        Ok(claims)
    }
}
