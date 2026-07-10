//! Offline per-VM provisioning (REFACTOR_PROPOSAL.md §3). The daemon mounts a
//! VM's raw disk once at create time and applies a [`ProvisionPlan`]:
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
    pub authorized_keys: Vec<String>,
    pub allow_no_ssh: bool,
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
            authorized_keys: provision.authorized_keys.clone(),
            allow_no_ssh: provision.allow_no_ssh,
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
            "files=[{}] reset_machine_id={} reset_ssh_hostkeys={} hostname={} ssh_keys={} allow_no_ssh={}",
            files.join(","),
            self.reset_machine_id,
            self.reset_ssh_hostkeys,
            self.hostname,
            self.authorized_keys.len(),
            self.allow_no_ssh,
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

    // Managed SSH access is applied after generic files so a caller cannot
    // accidentally replace the canonical recovery key set through a second
    // provision-file entry targeting authorized_keys.
    if plan.authorized_keys.is_empty() {
        if plan.allow_no_ssh {
            remove_baked_authorized_keys(root).await?;
        }
    } else {
        install_authorized_keys(root, &plan.authorized_keys).await?;
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

const AGENT_USER: &str = "agent";
const AGENT_UID: u32 = 1000;
const AGENT_GID: u32 = 1000;
const AGENT_HOME: &str = "/home/agent";

/// Resolve and enforce Hearth's fixed agent-account contract, then install a
/// canonical authorized_keys file and verify the bytes, ownership, and modes
/// before the caller converts the scratch disk to its final qcow2.
async fn install_authorized_keys(root: &Utf8Path, keys: &[String]) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    validate_sshd_recovery_contract(root).await?;
    validate_agent_account(root).await?;
    let home = join_under_root(root, Utf8Path::new(AGENT_HOME));
    let home_meta = fs::symlink_metadata(&home)
        .await
        .with_context(|| format!("inspect {AGENT_HOME}"))?;
    if !home_meta.is_dir() || home_meta.file_type().is_symlink() {
        anyhow::bail!("{AGENT_HOME} must be a real directory");
    }

    let ssh_dir = home.join(".ssh");
    match fs::symlink_metadata(&ssh_dir).await {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {}
        Ok(_) => anyhow::bail!("{AGENT_HOME}/.ssh must be a real directory"),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir(&ssh_dir)
                .await
                .context("create /home/agent/.ssh")?;
        }
        Err(err) => return Err(err).context("inspect /home/agent/.ssh"),
    }
    fs::set_permissions(&ssh_dir, std::fs::Permissions::from_mode(0o700))
        .await
        .context("chmod /home/agent/.ssh")?;
    std::os::unix::fs::chown(ssh_dir.as_std_path(), Some(AGENT_UID), Some(AGENT_GID))
        .context("chown /home/agent/.ssh")?;

    let authorized_keys = ssh_dir.join("authorized_keys");
    let content = format!("{}\n", keys.join("\n"));
    fs::write(&authorized_keys, &content)
        .await
        .context("write /home/agent/.ssh/authorized_keys")?;
    fs::set_permissions(&authorized_keys, std::fs::Permissions::from_mode(0o600))
        .await
        .context("chmod /home/agent/.ssh/authorized_keys")?;
    std::os::unix::fs::chown(
        authorized_keys.as_std_path(),
        Some(AGENT_UID),
        Some(AGENT_GID),
    )
    .context("chown /home/agent/.ssh/authorized_keys")?;

    let written = fs::read_to_string(&authorized_keys)
        .await
        .context("verify /home/agent/.ssh/authorized_keys")?;
    if written != content {
        anyhow::bail!("authorized_keys verification failed: content changed after write");
    }
    let parsed = crate::ssh::parse_authorized_keys(&written, "installed authorized_keys")?;
    if parsed.len() != keys.len() {
        anyhow::bail!("authorized_keys verification failed: key count changed after write");
    }
    let dir_meta = fs::metadata(&ssh_dir).await?;
    let file_meta = fs::metadata(&authorized_keys).await?;
    if dir_meta.mode() & 0o7777 != 0o700
        || dir_meta.uid() != AGENT_UID
        || dir_meta.gid() != AGENT_GID
    {
        anyhow::bail!("authorized_keys verification failed: .ssh mode/owner is not 0700 1000:1000");
    }
    if file_meta.mode() & 0o7777 != 0o600
        || file_meta.uid() != AGENT_UID
        || file_meta.gid() != AGENT_GID
    {
        anyhow::bail!("authorized_keys verification failed: file mode/owner is not 0600 1000:1000");
    }
    Ok(())
}

async fn validate_sshd_recovery_contract(root: &Utf8Path) -> Result<()> {
    let wants = join_under_root(
        root,
        Utf8Path::new("/etc/systemd/system/multi-user.target.wants"),
    );
    let mut enabled = false;
    for unit in ["ssh.service", "sshd.service"] {
        match fs::symlink_metadata(wants.join(unit)).await {
            Ok(_) => enabled = true,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).with_context(|| format!("inspect enabled {unit}")),
        }
    }
    if !enabled {
        anyhow::bail!("image has no ssh.service or sshd.service enabled for recovery access");
    }

    let mut configs = vec![join_under_root(root, Utf8Path::new("/etc/ssh/sshd_config"))];
    let dropins = join_under_root(root, Utf8Path::new("/etc/ssh/sshd_config.d"));
    match fs::read_dir(&dropins).await {
        Ok(mut entries) => {
            while let Some(entry) = entries.next_entry().await? {
                let path = Utf8PathBuf::from_path_buf(entry.path()).map_err(|path| {
                    anyhow::anyhow!("non-utf8 sshd config path: {}", path.display())
                })?;
                if path.extension() == Some("conf") {
                    configs.push(path);
                }
            }
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("read /etc/ssh/sshd_config.d"),
    }
    for config in configs {
        let text = match fs::read_to_string(&config).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err).with_context(|| format!("read {config}")),
        };
        if sshd_config_disables_managed_keys(&text) {
            anyhow::bail!(
                "{config} disables public-key authentication or does not use .ssh/authorized_keys"
            );
        }
    }
    Ok(())
}

fn sshd_config_disables_managed_keys(text: &str) -> bool {
    text.lines().any(|line| {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            return false;
        }
        let mut fields = line.split_whitespace();
        let directive = fields.next().unwrap_or_default();
        let value = fields.next().unwrap_or_default();
        if directive.eq_ignore_ascii_case("PubkeyAuthentication") {
            return value.eq_ignore_ascii_case("no");
        }
        if directive.eq_ignore_ascii_case("AuthorizedKeysFile") {
            return !std::iter::once(value).chain(fields).any(|path| {
                path.ends_with("/.ssh/authorized_keys") || path == ".ssh/authorized_keys"
            });
        }
        false
    })
}

async fn validate_agent_account(root: &Utf8Path) -> Result<()> {
    let passwd_path = join_under_root(root, Utf8Path::new("/etc/passwd"));
    let passwd = fs::read_to_string(&passwd_path)
        .await
        .context("read /etc/passwd for SSH provisioning")?;
    let fields = passwd
        .lines()
        .find_map(|line| {
            let fields = line.split(':').collect::<Vec<_>>();
            (fields.first() == Some(&AGENT_USER)).then_some(fields)
        })
        .ok_or_else(|| anyhow::anyhow!("image has no {AGENT_USER:?} account"))?;
    let uid = fields.get(2).and_then(|v| v.parse::<u32>().ok());
    let gid = fields.get(3).and_then(|v| v.parse::<u32>().ok());
    let home = fields.get(5).copied();
    if uid != Some(AGENT_UID) || gid != Some(AGENT_GID) || home != Some(AGENT_HOME) {
        anyhow::bail!(
            "image agent account must be uid={AGENT_UID} gid={AGENT_GID} home={AGENT_HOME}"
        );
    }
    Ok(())
}

async fn remove_baked_authorized_keys(root: &Utf8Path) -> Result<()> {
    let path = join_under_root(root, Utf8Path::new("/home/agent/.ssh/authorized_keys"));
    match fs::remove_file(&path).await {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err).context("remove baked /home/agent/.ssh/authorized_keys"),
    }
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

    #[test]
    fn sshd_config_must_leave_managed_authorized_keys_enabled() {
        assert!(!sshd_config_disables_managed_keys(
            "PubkeyAuthentication yes\nAuthorizedKeysFile .ssh/authorized_keys .ssh/authorized_keys2\n"
        ));
        assert!(sshd_config_disables_managed_keys(
            "PubkeyAuthentication no\n"
        ));
        assert!(sshd_config_disables_managed_keys(
            "AuthorizedKeysFile /etc/ssh/operator_keys\n"
        ));
    }

    #[tokio::test]
    async fn remove_ssh_hostkeys_only_touches_matching_files() {
        let tmp = tempfile::tempdir().unwrap();
        let ssh = Utf8PathBuf::from_path_buf(tmp.path().join("etc/ssh")).unwrap();
        fs::create_dir_all(&ssh).await.unwrap();
        fs::write(ssh.join("ssh_host_rsa_key"), b"k").await.unwrap();
        fs::write(ssh.join("ssh_host_rsa_key.pub"), b"k")
            .await
            .unwrap();
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
