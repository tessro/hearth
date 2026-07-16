//! Per-peer-UID verb policy for the host unix socket (docs/agent-plane.md §10).
//! Until now `peer_credentials()` fed audit fields only; this makes dispatch
//! authorized. Built-in default preserves the existing reality (the socket is
//! `0660 root:hearth`): root and the `hearth` group may issue every verb.
//! Explicit entries — matched in file order, before the defaults — carve out
//! restricted peers like the `hearth-agent` user.

use anyhow::{bail, Context, Result};
use camino::Utf8Path;
use hearth_proto::Verb;
use serde::Deserialize;
use std::collections::HashSet;

#[derive(Debug, Deserialize)]
struct PolicyFile {
    #[serde(default)]
    peer: Vec<PeerEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PeerEntry {
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    uid: Option<u32>,
    #[serde(default)]
    group: Option<String>,
    #[serde(default)]
    gid: Option<u32>,
    verbs: Vec<Verb>,
}

#[derive(Debug, Clone)]
struct ResolvedEntry {
    uid: Option<u32>,
    gid: Option<u32>,
    verbs: HashSet<Verb>,
}

#[derive(Debug, Clone, Default)]
pub struct VerbPolicy {
    entries: Vec<ResolvedEntry>,
    hearth_gid: Option<u32>,
}

impl VerbPolicy {
    /// Load the policy file; a missing file yields the built-in default.
    /// A present-but-invalid file is a hard error — silently falling back to
    /// default-allow would be a policy bypass.
    pub async fn load(path: &Utf8Path) -> Result<Self> {
        let text = match tokio::fs::read_to_string(path).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => return Err(err).with_context(|| format!("read verb policy {path}")),
        };
        Self::parse(&text).with_context(|| format!("parse verb policy {path}"))
    }

    pub fn parse(text: &str) -> Result<Self> {
        let file: PolicyFile = toml::from_str(text)?;
        let mut entries = Vec::new();
        for entry in file.peer {
            let uid = match (&entry.user, entry.uid) {
                (Some(user), None) => Some(
                    resolve_user(user)
                        .with_context(|| format!("verb policy user {user:?} does not exist"))?,
                ),
                (None, uid) => uid,
                (Some(_), Some(_)) => bail!("policy entry sets both user and uid"),
            };
            let gid = match (&entry.group, entry.gid) {
                (Some(group), None) => Some(
                    resolve_group(group)
                        .with_context(|| format!("verb policy group {group:?} does not exist"))?,
                ),
                (None, gid) => gid,
                (Some(_), Some(_)) => bail!("policy entry sets both group and gid"),
            };
            if uid.is_none() && gid.is_none() {
                bail!("policy entry matches nobody: set user/uid or group/gid");
            }
            entries.push(ResolvedEntry {
                uid,
                gid,
                verbs: entry.verbs.into_iter().collect(),
            });
        }
        Ok(Self {
            entries,
            hearth_gid: resolve_group("hearth").ok(),
        })
    }

    /// Whether a peer with these credentials may issue `verb`. `None`
    /// credentials (non-Linux dev hosts) fall through to allow, matching the
    /// pre-policy behavior there.
    ///
    /// `SO_PEERCRED` reports only the peer's *primary* gid, but the normal way
    /// to grant access is `usermod -aG hearth` (a supplementary group). So both
    /// explicit `gid` entries and the built-in hearth-group default are checked
    /// against the peer's **full** group membership (resolved via
    /// `getgrouplist`), not just its primary gid — otherwise a hearth-group
    /// operator with a different primary group is wrongly denied every verb.
    pub fn allows(&self, uid: Option<u32>, gid: Option<u32>, verb: &Verb) -> bool {
        let (Some(uid), Some(gid)) = (uid, gid) else {
            return true;
        };
        let gids = peer_group_set(uid, gid);
        for entry in &self.entries {
            let uid_match = entry.uid.is_some_and(|u| u == uid);
            let gid_match = entry.gid.is_some_and(|g| gids.contains(&g));
            if uid_match || gid_match {
                return entry.verbs.contains(verb);
            }
        }
        // Built-in default: root and any member of the hearth group (primary or
        // supplementary) keep full access.
        uid == 0 || self.hearth_gid.is_some_and(|g| gids.contains(&g))
    }
}

/// The full set of gids a peer belongs to: its primary gid plus every
/// supplementary group, resolved from its uid. Falls back to just the primary
/// gid when the uid has no passwd entry (e.g. synthetic test uids), which keeps
/// the pre-resolution behavior for those.
fn peer_group_set(uid: u32, primary_gid: u32) -> std::collections::HashSet<u32> {
    let mut gids = std::collections::HashSet::new();
    gids.insert(primary_gid);
    // uid → username, so getgrouplist can enumerate supplementary groups.
    let pw = unsafe { libc::getpwuid(uid) };
    if pw.is_null() {
        return gids;
    }
    let name = unsafe { (*pw).pw_name };
    if name.is_null() {
        return gids;
    }
    let mut ngroups: libc::c_int = 32;
    let mut buf = vec![0 as libc::gid_t; ngroups as usize];
    let rc = unsafe {
        libc::getgrouplist(name, primary_gid as libc::gid_t, buf.as_mut_ptr(), &mut ngroups)
    };
    if rc < 0 {
        // Buffer too small: ngroups now holds the needed size — retry once.
        buf = vec![0 as libc::gid_t; ngroups.max(0) as usize];
        let rc = unsafe {
            libc::getgrouplist(name, primary_gid as libc::gid_t, buf.as_mut_ptr(), &mut ngroups)
        };
        if rc < 0 {
            return gids;
        }
    }
    for gid in buf.into_iter().take(ngroups.max(0) as usize) {
        gids.insert(gid);
    }
    gids
}

fn resolve_user(name: &str) -> Result<u32> {
    let cname = std::ffi::CString::new(name)?;
    let pw = unsafe { libc::getpwnam(cname.as_ptr()) };
    if pw.is_null() {
        bail!("no such user");
    }
    Ok(unsafe { (*pw).pw_uid })
}

fn resolve_group(name: &str) -> Result<u32> {
    let cname = std::ffi::CString::new(name)?;
    let gr = unsafe { libc::getgrnam(cname.as_ptr()) };
    if gr.is_null() {
        bail!("no such group");
    }
    Ok(unsafe { (*gr).gr_gid })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_uid_entry_restricts_before_defaults() {
        let policy = VerbPolicy::parse(
            r#"
[[peer]]
uid = 993
verbs = ["ping", "version", "ls", "status", "agent-endpoints", "guest-listener", "guest-connect"]
"#,
        )
        .unwrap();
        assert!(policy.allows(Some(993), Some(0), &Verb::Ping));
        assert!(policy.allows(Some(993), Some(0), &Verb::GuestConnect));
        assert!(!policy.allows(Some(993), Some(0), &Verb::Destroy));
        assert!(!policy.allows(Some(993), Some(0), &Verb::Create));
        // Root stays default-allow.
        assert!(policy.allows(Some(0), Some(0), &Verb::Destroy));
    }

    // A uid with no passwd entry, so `peer_group_set` cannot resolve
    // supplementary groups and falls back to just the primary gid — keeping
    // these matrix assertions hermetic regardless of the test host's real
    // users and their group memberships.
    const SYNTHETIC_UID: u32 = 4_000_000_001;

    #[test]
    fn gid_entries_match_supplementary_and_unknown_peers_are_denied() {
        let policy = VerbPolicy::parse(
            r#"
[[peer]]
gid = 4242
verbs = ["ls"]
"#,
        )
        .unwrap();
        assert!(policy.allows(Some(SYNTHETIC_UID), Some(4242), &Verb::Ls));
        assert!(!policy.allows(Some(SYNTHETIC_UID), Some(4242), &Verb::Stop));
        // No entry, not root, not hearth group: denied.
        assert!(!policy.allows(Some(SYNTHETIC_UID), Some(1000), &Verb::Ls));
    }

    #[test]
    fn empty_and_missing_policies_keep_the_existing_reality() {
        let policy = VerbPolicy::parse("").unwrap();
        assert!(policy.allows(Some(0), Some(0), &Verb::Destroy));
        assert!(policy.allows(None, None, &Verb::Destroy));
        assert!(!policy.allows(Some(SYNTHETIC_UID), Some(4_000_000_002), &Verb::Ls));
    }

    #[test]
    fn invalid_policy_is_a_hard_error_not_a_bypass() {
        assert!(VerbPolicy::parse("[[peer]]\nverbs = [\"ls\"]\n").is_err());
        assert!(VerbPolicy::parse("[[peer]]\nuid = 1\ngid = 2\nuser = \"x\"\nverbs = []\n").is_err());
        assert!(VerbPolicy::parse("peer = 3\n").is_err());
    }
}
