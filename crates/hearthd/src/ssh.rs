//! Parsing and fingerprinting for Hearth-managed SSH authorized keys.
//!
//! Hearth accepts bare OpenSSH public-key lines (`algorithm base64 [comment]`).
//! AuthorizedKeys options are deliberately unsupported: recovery access should
//! be simple, inspectable, and identical whether it came from the host keyring
//! or a per-VM create request.

use anyhow::{anyhow, bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizedKey {
    pub canonical: String,
    pub fingerprint: String,
    blob: Vec<u8>,
}

/// Parse non-empty, non-comment lines from an authorized_keys-shaped document.
pub fn parse_authorized_keys(text: &str, source: &str) -> Result<Vec<AuthorizedKey>> {
    text.lines()
        .enumerate()
        .filter_map(|(index, line)| {
            let line = line.trim();
            (!line.is_empty() && !line.starts_with('#')).then_some((index + 1, line))
        })
        .map(|(line_number, line)| {
            parse_authorized_key(line)
                .with_context(|| format!("{source}:{line_number}: invalid SSH public key"))
        })
        .collect()
}

/// Parse one bare OpenSSH public key and return its canonical line and standard
/// `SHA256:...` fingerprint (SHA-256 of the decoded SSH wire blob).
pub fn parse_authorized_key(line: &str) -> Result<AuthorizedKey> {
    if line.contains("PRIVATE KEY") {
        bail!("private-key material is not an authorized key");
    }
    let mut fields = line.split_whitespace();
    let algorithm = fields.next().ok_or_else(|| anyhow!("missing algorithm"))?;
    let encoded = fields.next().ok_or_else(|| anyhow!("missing key blob"))?;
    if !is_key_algorithm(algorithm) {
        bail!("expected a bare OpenSSH public key (options are unsupported), got {algorithm:?}");
    }
    let blob = decode_base64(encoded).context("key blob is not valid base64")?;
    let blob_algorithm = ssh_wire_string(&blob).context("key blob has no SSH algorithm prefix")?;
    if blob_algorithm != algorithm.as_bytes() {
        bail!(
            "line algorithm {algorithm:?} does not match key blob algorithm {:?}",
            String::from_utf8_lossy(blob_algorithm)
        );
    }
    let comment = fields.collect::<Vec<_>>().join(" ");
    let canonical = if comment.is_empty() {
        format!("{algorithm} {encoded}")
    } else {
        format!("{algorithm} {encoded} {comment}")
    };
    let fingerprint = format!("SHA256:{}", encode_base64(&Sha256::digest(&blob), false));
    Ok(AuthorizedKey {
        canonical,
        fingerprint,
        blob,
    })
}

/// Merge key documents in input order, dropping duplicate key blobs while
/// preserving the first line/comment seen for each key.
pub fn merge_authorized_keys<'a>(
    sources: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> Result<Vec<AuthorizedKey>> {
    let mut seen = BTreeSet::new();
    let mut merged = Vec::new();
    for (source, text) in sources {
        for key in parse_authorized_keys(text, source)? {
            if seen.insert(key.blob.clone()) {
                merged.push(key);
            }
        }
    }
    Ok(merged)
}

fn is_key_algorithm(value: &str) -> bool {
    value.starts_with("ssh-")
        || value.starts_with("ecdsa-")
        || value.starts_with("sk-ssh-")
        || value.starts_with("sk-ecdsa-")
}

fn ssh_wire_string(blob: &[u8]) -> Option<&[u8]> {
    let len = u32::from_be_bytes(blob.get(..4)?.try_into().ok()?) as usize;
    blob.get(4..4 + len)
}

fn decode_base64(value: &str) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(value.len() * 3 / 4);
    let mut quartet = [0u8; 4];
    let mut used = 0;
    let mut saw_padding = false;
    for byte in value.bytes() {
        let decoded = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => {
                saw_padding = true;
                64
            }
            _ => bail!("invalid base64 character"),
        };
        if saw_padding && decoded != 64 {
            bail!("data follows base64 padding");
        }
        quartet[used] = decoded;
        used += 1;
        if used == 4 {
            decode_quartet(&quartet, &mut out)?;
            used = 0;
        }
    }
    if used == 1 {
        bail!("invalid base64 length");
    }
    if used > 1 {
        for slot in quartet.iter_mut().skip(used) {
            *slot = 64;
        }
        decode_quartet(&quartet, &mut out)?;
    }
    Ok(out)
}

fn decode_quartet(q: &[u8; 4], out: &mut Vec<u8>) -> Result<()> {
    if q[0] >= 64 || q[1] >= 64 || (q[2] == 64 && q[3] != 64) {
        bail!("invalid base64 padding");
    }
    out.push((q[0] << 2) | (q[1] >> 4));
    if q[2] < 64 {
        out.push((q[1] << 4) | (q[2] >> 2));
    }
    if q[3] < 64 {
        out.push((q[2] << 6) | q[3]);
    }
    Ok(())
}

fn encode_base64(bytes: &[u8], padding: bool) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let a = chunk[0];
        let b = chunk.get(1).copied().unwrap_or(0);
        let c = chunk.get(2).copied().unwrap_or(0);
        out.push(TABLE[(a >> 2) as usize] as char);
        out.push(TABLE[(((a & 0x03) << 4) | (b >> 4)) as usize] as char);
        if chunk.len() > 1 {
            out.push(TABLE[(((b & 0x0f) << 2) | (c >> 6)) as usize] as char);
        } else if padding {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(TABLE[(c & 0x3f) as usize] as char);
        } else if padding {
            out.push('=');
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPEVBr+XtUOuloYyDWGTcKPPHbVwpSIATl/mJ6RE7gdN hearth-test";

    #[test]
    fn parses_and_fingerprints_an_ed25519_key() {
        let key = parse_authorized_key(KEY).unwrap();
        assert_eq!(key.canonical, KEY);
        assert!(key.fingerprint.starts_with("SHA256:"));
        assert!(!key.fingerprint.ends_with('='));
    }

    #[test]
    fn ignores_comments_and_deduplicates_blobs() {
        let second_comment = KEY.replace("hearth-test", "same-key-different-comment");
        let text = format!("# recovery\n{KEY}\n\n{second_comment}\n");
        let keys = merge_authorized_keys([("test", text.as_str())]).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].canonical, KEY);
    }

    #[test]
    fn rejects_authorized_keys_options_and_mismatched_blobs() {
        assert!(parse_authorized_key(&format!("restrict {KEY}")).is_err());
        assert!(parse_authorized_key(&KEY.replacen("ssh-ed25519", "ssh-rsa", 1)).is_err());
    }
}
