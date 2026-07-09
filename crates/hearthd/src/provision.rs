//! Offline per-VM provisioning (REFACTOR_PROPOSAL.md §3). The daemon mounts a
//! docker-rootfs VM's raw disk once at create time and applies a [`ProvisionPlan`]:
//! write secret/config files, reset machine-id and SSH host keys, set hostname.
//!
//! Plan *construction* ([`ProvisionPlan::from_provision`]) is a pure function,
//! unit-tested here. Plan *application* touches a mounted rootfs and chowns to
//! arbitrary uids, so it needs root and is exercised only end-to-end; it is kept
//! thin and obvious.

use crate::registry::{parse_mode, parse_owner, Provision};
use anyhow::{Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use tokio::fs;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    /// Inline content carried in the service TOML (may be a secret).
    Literal(String),
    /// An absolute path on the daemon host, read at apply time.
    Source(Utf8PathBuf),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub dest: Utf8PathBuf,
    pub content: FileContent,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
}

/// A fully-resolved, validated provisioning plan (modes and owners parsed to
/// numbers). Everything needed to mutate a rootfs, and nothing that requires
/// re-reading the service TOML.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProvisionPlan {
    pub files: Vec<PlannedFile>,
    pub reset_machine_id: bool,
    pub reset_ssh_hostkeys: bool,
    pub hostname: String,
}

impl ProvisionPlan {
    /// Build and validate a plan from a service's `[provision]` config. Pure: no
    /// filesystem access (source files are read later, at apply time).
    pub fn from_provision(provision: &Provision) -> Result<Self> {
        let mut files = Vec::with_capacity(provision.files.len());
        for f in &provision.files {
            f.validate()?;
            let mode = parse_mode(&f.mode)?;
            let (uid, gid) = parse_owner(&f.owner)?;
            let content = match (&f.source, &f.from_literal) {
                (Some(src), None) => FileContent::Source(src.clone()),
                (None, Some(lit)) => FileContent::Literal(lit.clone()),
                // validate() rejects both/neither, so this is unreachable.
                _ => anyhow::bail!("provision file {}: ambiguous content source", f.dest),
            };
            files.push(PlannedFile {
                dest: f.dest.clone(),
                content,
                mode,
                uid,
                gid,
            });
        }
        Ok(Self {
            files,
            reset_machine_id: provision.reset_machine_id,
            reset_ssh_hostkeys: provision.reset_ssh_hostkeys,
            hostname: provision.hostname.clone(),
        })
    }

    /// A one-line, secret-free description (literal contents shown as
    /// `<literal>`). Used for logs and to make `provision_disk` calls assertable
    /// in tests.
    pub fn describe(&self) -> String {
        let files: Vec<String> = self
            .files
            .iter()
            .map(|f| {
                let src = match &f.content {
                    FileContent::Literal(_) => "<literal>".to_string(),
                    FileContent::Source(path) => path.to_string(),
                };
                format!("{}<-{}:{:04o}:{}:{}", f.dest, src, f.mode, f.uid, f.gid)
            })
            .collect();
        format!(
            "files=[{}] reset_machine_id={} reset_ssh_hostkeys={} hostname={}",
            files.join(","),
            self.reset_machine_id,
            self.reset_ssh_hostkeys,
            self.hostname
        )
    }
}

/// Join an absolute in-guest destination under a mounted rootfs, stripping the
/// leading `/` so the path lands inside `root` rather than replacing it.
pub fn join_under_root(root: &Utf8Path, abs: &Utf8Path) -> Utf8PathBuf {
    let rel = abs.strip_prefix("/").unwrap_or(abs);
    root.join(rel)
}

/// Apply a plan to an already-mounted rootfs at `root`. Order: write files
/// (parents created, content written, mode set, numeric owner chowned), then
/// truncate `/etc/machine-id`, remove SSH host keys, and write `/etc/hostname`.
///
/// Requires root (chown to arbitrary uids); not unit-tested. Callers mount the
/// disk, invoke this, and unmount even on error.
pub async fn apply_to_root(root: &Utf8Path, plan: &ProvisionPlan) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for file in &plan.files {
        let dest = join_under_root(root, &file.dest);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .await
                .with_context(|| format!("create parent dirs for {}", file.dest))?;
        }
        match &file.content {
            FileContent::Literal(text) => fs::write(&dest, text)
                .await
                .with_context(|| format!("write {}", file.dest))?,
            FileContent::Source(src) => {
                fs::copy(src, &dest)
                    .await
                    .with_context(|| format!("copy {src} -> {}", file.dest))?;
            }
        }
        fs::set_permissions(&dest, std::fs::Permissions::from_mode(file.mode))
            .await
            .with_context(|| format!("chmod {}", file.dest))?;
        std::os::unix::fs::chown(dest.as_std_path(), Some(file.uid), Some(file.gid))
            .with_context(|| format!("chown {}", file.dest))?;
    }

    if plan.reset_machine_id {
        // An empty (0-byte) /etc/machine-id makes systemd regenerate it on boot.
        let mid = join_under_root(root, Utf8Path::new("/etc/machine-id"));
        fs::write(&mid, b"")
            .await
            .context("truncate /etc/machine-id")?;
    }

    if plan.reset_ssh_hostkeys {
        remove_ssh_hostkeys(&join_under_root(root, Utf8Path::new("/etc/ssh"))).await?;
    }

    if !plan.hostname.is_empty() {
        let hostname = join_under_root(root, Utf8Path::new("/etc/hostname"));
        fs::write(&hostname, format!("{}\n", plan.hostname))
            .await
            .context("write /etc/hostname")?;
    }

    Ok(())
}

/// Remove every `ssh_host_*` file from a rootfs `/etc/ssh`. A missing directory
/// is not an error (the image may not ship sshd).
async fn remove_ssh_hostkeys(ssh_dir: &Utf8Path) -> Result<()> {
    let mut entries = match fs::read_dir(ssh_dir).await {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(anyhow::Error::new(e).context(format!("read {ssh_dir}"))),
    };
    while let Some(entry) = entries.next_entry().await? {
        if let Some(name) = entry.file_name().to_str() {
            if name.starts_with("ssh_host_") {
                fs::remove_file(entry.path())
                    .await
                    .with_context(|| format!("remove {name}"))?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::{Provision, ProvisionFile};

    fn literal(dest: &str, content: &str, mode: &str, owner: &str) -> ProvisionFile {
        ProvisionFile {
            source: None,
            from_literal: Some(content.to_string()),
            dest: Utf8PathBuf::from(dest),
            mode: mode.to_string(),
            owner: owner.to_string(),
        }
    }

    #[test]
    fn from_provision_resolves_modes_owners_and_content() {
        let provision = Provision {
            hostname: "hermes".to_string(),
            reset_ssh_hostkeys: true,
            files: vec![
                literal("/home/agent/.env", "TOKEN=abc", "0600", "1000:1000"),
                ProvisionFile {
                    source: Some(Utf8PathBuf::from("/etc/hearth/a.conf")),
                    from_literal: None,
                    dest: Utf8PathBuf::from("/etc/a.conf"),
                    mode: "0644".to_string(),
                    owner: "0:0".to_string(),
                },
            ],
            ..Provision::default()
        };

        let plan = ProvisionPlan::from_provision(&provision).unwrap();

        assert_eq!(plan.hostname, "hermes");
        assert!(plan.reset_machine_id);
        assert!(plan.reset_ssh_hostkeys);
        assert_eq!(plan.files.len(), 2);
        assert_eq!(plan.files[0].mode, 0o600);
        assert_eq!((plan.files[0].uid, plan.files[0].gid), (1000, 1000));
        assert_eq!(
            plan.files[0].content,
            FileContent::Literal("TOKEN=abc".to_string())
        );
        assert_eq!(
            plan.files[1].content,
            FileContent::Source(Utf8PathBuf::from("/etc/hearth/a.conf"))
        );
    }

    #[test]
    fn from_provision_rejects_an_invalid_file() {
        let mut provision = Provision::default();
        provision
            .files
            .push(literal("relative/path", "x", "0600", "0:0"));
        assert!(ProvisionPlan::from_provision(&provision).is_err());
    }

    #[test]
    fn describe_redacts_literals() {
        let mut provision = Provision::default();
        provision
            .files
            .push(literal("/home/agent/.env", "SECRET", "0600", "1000:1000"));
        let plan = ProvisionPlan::from_provision(&provision).unwrap();
        let described = plan.describe();
        assert!(described.contains("/home/agent/.env<-<literal>:0600:1000:1000"));
        assert!(!described.contains("SECRET"));
    }

    #[test]
    fn join_under_root_strips_leading_slash() {
        let root = Utf8Path::new("/mnt/rootfs");
        assert_eq!(
            join_under_root(root, Utf8Path::new("/etc/hostname")),
            Utf8PathBuf::from("/mnt/rootfs/etc/hostname")
        );
    }

    #[tokio::test]
    async fn remove_ssh_hostkeys_only_touches_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ssh = Utf8PathBuf::from_path_buf(tmp.path().join("etc/ssh")).unwrap();
        fs::create_dir_all(&ssh).await.unwrap();
        fs::write(ssh.join("ssh_host_rsa_key"), b"k").await.unwrap();
        fs::write(ssh.join("ssh_host_rsa_key.pub"), b"k").await.unwrap();
        fs::write(ssh.join("sshd_config"), b"cfg").await.unwrap();

        remove_ssh_hostkeys(&ssh).await.unwrap();

        assert!(!ssh.join("ssh_host_rsa_key").exists());
        assert!(!ssh.join("ssh_host_rsa_key.pub").exists());
        assert!(ssh.join("sshd_config").exists());
    }

    #[tokio::test]
    async fn remove_ssh_hostkeys_tolerates_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let ssh = Utf8PathBuf::from_path_buf(tmp.path().join("nope")).unwrap();
        assert!(remove_ssh_hostkeys(&ssh).await.is_ok());
    }
}
