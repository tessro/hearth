//! Offline per-VM provisioning (REFACTOR_PROPOSAL.md §3). The daemon mounts a
//! VM's raw disk once at create time and applies a [`ProvisionPlan`]:
//! write secret/config files, reset machine-id and SSH host keys, set hostname.
//!
//! Plan *construction* ([`ProvisionPlan::from_provision`]) is a pure function,
//! unit-tested here. Plan *application* touches a mounted rootfs and chowns to
//! arbitrary uids, so it needs root and is exercised only end-to-end; it is kept
//! thin and obvious.

use crate::registry::{parse_mode, parse_owner, Provision};
use anyhow::{bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use std::{
    ffi::CString,
    fs::{File, OpenOptions},
    io::{Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::OpenOptionsExt,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    /// Inline content carried in the service TOML (may be a secret).
    Literal(String),
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
    /// Build and validate a plan from a service's `[provision]` config.
    pub fn from_provision(provision: &Provision) -> Result<Self> {
        let mut files = Vec::with_capacity(provision.files.len());
        for f in &provision.files {
            f.validate()?;
            let mode = parse_mode(&f.mode)?;
            let (uid, gid) = parse_owner(&f.owner)?;
            let content = FileContent::Literal(f.from_literal.clone());
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
                let src = "<literal>";
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

/// An fd for the mounted guest root. Every lookup below walks one path
/// component at a time with `openat(O_NOFOLLOW)`. That prevents an absolute or
/// relative symlink in the image from redirecting the root daemon onto the
/// host filesystem. Final write targets must also be regular files.
struct GuestRoot {
    root: File,
}

impl GuestRoot {
    fn open(root: &Utf8Path) -> Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
            .open(root)
            .with_context(|| format!("open mounted guest root {root}"))?;
        Ok(Self { root: file })
    }

    fn path_parts(path: &Utf8Path) -> Result<Vec<&str>> {
        if !path.is_absolute() {
            bail!("guest path must be absolute: {path}");
        }
        let mut parts = Vec::new();
        for component in path.components() {
            match component {
                camino::Utf8Component::RootDir => {}
                camino::Utf8Component::Normal(part) => parts.push(part),
                camino::Utf8Component::CurDir | camino::Utf8Component::ParentDir => {
                    bail!("guest path must be normalized: {path}")
                }
                camino::Utf8Component::Prefix(_) => bail!("invalid guest path: {path}"),
            }
        }
        Ok(parts)
    }

    fn open_dir_parts(&self, parts: &[&str], create: bool) -> Result<File> {
        let mut current = self.root.try_clone().context("clone guest root fd")?;
        for part in parts {
            let name = CString::new(*part).context("guest path contains NUL")?;
            match open_dir_at(&current, &name) {
                Ok(next) => current = next,
                Err(err) if create && err.kind() == std::io::ErrorKind::NotFound => {
                    let rc = unsafe { libc::mkdirat(current.as_raw_fd(), name.as_ptr(), 0o755) };
                    if rc != 0 {
                        let mkdir_err = std::io::Error::last_os_error();
                        if mkdir_err.kind() != std::io::ErrorKind::AlreadyExists {
                            return Err(mkdir_err)
                                .with_context(|| format!("create guest directory {part}"));
                        }
                    }
                    current = open_dir_at(&current, &name)
                        .with_context(|| format!("open newly created guest directory {part}"))?;
                }
                Err(err) => {
                    return Err(err).with_context(|| {
                        format!(
                            "open guest directory component {part:?} without following symlinks"
                        )
                    })
                }
            }
        }
        Ok(current)
    }

    fn open_dir(&self, path: &Utf8Path, create: bool) -> Result<File> {
        let parts = Self::path_parts(path)?;
        self.open_dir_parts(&parts, create)
            .with_context(|| format!("open guest directory {path}"))
    }

    fn open_parent(&self, path: &Utf8Path, create: bool) -> Result<(File, CString)> {
        let mut parts = Self::path_parts(path)?;
        let leaf = parts
            .pop()
            .ok_or_else(|| anyhow::anyhow!("guest file path names the root directory: {path}"))?;
        let parent = self.open_dir_parts(&parts, create)?;
        let leaf = CString::new(leaf).context("guest path contains NUL")?;
        Ok((parent, leaf))
    }

    fn write_file(
        &self,
        path: &Utf8Path,
        content: &[u8],
        mode: u32,
        uid: u32,
        gid: u32,
    ) -> Result<()> {
        let (parent, leaf) = self.open_parent(path, true)?;
        if let Some(stat) = stat_at(&parent, &leaf)? {
            if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
                bail!("guest write target {path} is not a regular file");
            }
        }
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_WRONLY
                    | libc::O_CREAT
                    | libc::O_TRUNC
                    | libc::O_CLOEXEC
                    | libc::O_NOFOLLOW
                    | libc::O_NONBLOCK,
                mode as libc::mode_t,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open guest file {path} without following symlinks"));
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            bail!("guest write target {path} is not a regular file");
        }
        file.write_all(content)
            .with_context(|| format!("write guest file {path}"))?;
        if unsafe { libc::fchown(file.as_raw_fd(), uid, gid) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("chown guest file {path}"));
        }
        if unsafe { libc::fchmod(file.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("chmod guest file {path}"));
        }
        Ok(())
    }

    fn read_to_string(&self, path: &Utf8Path) -> Result<String> {
        let (parent, leaf) = self.open_parent(path, false)?;
        let stat = stat_at(&parent, &leaf)?
            .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::NotFound))?;
        if stat.st_mode & libc::S_IFMT != libc::S_IFREG {
            bail!("guest read target {path} is not a regular file");
        }
        let fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                leaf.as_ptr(),
                libc::O_RDONLY | libc::O_CLOEXEC | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("open guest file {path} without following symlinks"));
        }
        let mut file = unsafe { File::from_raw_fd(fd) };
        if !file.metadata()?.is_file() {
            bail!("guest read target {path} is not a regular file");
        }
        let mut text = String::new();
        file.read_to_string(&mut text)
            .with_context(|| format!("read guest file {path}"))?;
        Ok(text)
    }

    fn entry_exists(&self, dir: &Utf8Path, name: &str) -> Result<bool> {
        let dir = match self.open_dir(dir, false) {
            Ok(dir) => dir,
            Err(err) if is_not_found(&err) => return Ok(false),
            Err(err) => return Err(err),
        };
        let name = CString::new(name).context("guest entry name contains NUL")?;
        Ok(stat_at(&dir, &name)?.is_some())
    }

    fn list_names(&self, dir: &Utf8Path) -> Result<Vec<String>> {
        let dir_fd = match self.open_dir(dir, false) {
            Ok(dir) => dir,
            Err(err) if is_not_found(&err) => return Ok(Vec::new()),
            Err(err) => return Err(err),
        };
        let proc_path = format!("/proc/self/fd/{}", dir_fd.as_raw_fd());
        let mut names = Vec::new();
        for entry in
            std::fs::read_dir(&proc_path).with_context(|| format!("list guest directory {dir}"))?
        {
            let entry = entry?;
            let name = entry
                .file_name()
                .into_string()
                .map_err(|_| anyhow::anyhow!("non-UTF-8 entry in guest directory {dir}"))?;
            names.push(name);
        }
        Ok(names)
    }

    fn set_dir_attrs(&self, path: &Utf8Path, mode: u32, uid: u32, gid: u32) -> Result<()> {
        let dir = self.open_dir(path, true)?;
        if unsafe { libc::fchown(dir.as_raw_fd(), uid, gid) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("chown guest directory {path}"));
        }
        if unsafe { libc::fchmod(dir.as_raw_fd(), mode as libc::mode_t) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("chmod guest directory {path}"));
        }
        Ok(())
    }

    fn remove_file(&self, path: &Utf8Path) -> Result<()> {
        let (parent, leaf) = match self.open_parent(path, false) {
            Ok(value) => value,
            Err(err) if is_not_found(&err) => return Ok(()),
            Err(err) => return Err(err),
        };
        if stat_at(&parent, &leaf)?.is_none() {
            return Ok(());
        }
        if unsafe { libc::unlinkat(parent.as_raw_fd(), leaf.as_ptr(), 0) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("remove guest file {path}"));
        }
        Ok(())
    }
}

fn open_dir_at(parent: &File, name: &CString) -> std::io::Result<File> {
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn stat_at(parent: &File, name: &CString) -> Result<Option<libc::stat>> {
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    let rc = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc == 0 {
        Ok(Some(unsafe { stat.assume_init() }))
    } else {
        let err = std::io::Error::last_os_error();
        if err.kind() == std::io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(err.into())
        }
    }
}

fn is_not_found(err: &anyhow::Error) -> bool {
    err.chain().any(|cause| {
        cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|io| io.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Apply a plan to an already-mounted rootfs at `root`. Order: write files
/// (parents created, content written, mode set, numeric owner chowned), then
/// truncate `/etc/machine-id`, remove SSH host keys, and write `/etc/hostname`.
///
/// Requires root when the plan changes ownership to other users. Callers mount
/// the disk, invoke this, and unmount even on error.
pub async fn apply_to_root(root: &Utf8Path, plan: &ProvisionPlan) -> Result<()> {
    let root = GuestRoot::open(root)?;

    for file in &plan.files {
        let FileContent::Literal(text) = &file.content;
        root.write_file(&file.dest, text.as_bytes(), file.mode, file.uid, file.gid)?;
    }

    // Managed SSH access is applied after generic files so a caller cannot
    // accidentally replace the canonical recovery key set through a second
    // provision-file entry targeting authorized_keys.
    if plan.authorized_keys.is_empty() {
        if plan.allow_no_ssh {
            remove_baked_authorized_keys(&root)?;
        }
    } else {
        install_authorized_keys(&root, &plan.authorized_keys)?;
    }

    if plan.reset_machine_id {
        // An empty (0-byte) /etc/machine-id makes systemd regenerate it on boot.
        root.write_file(Utf8Path::new("/etc/machine-id"), b"", 0o644, 0, 0)
            .context("truncate /etc/machine-id")?;
    }

    if plan.reset_ssh_hostkeys {
        remove_ssh_hostkeys(&root)?;
    }

    if !plan.hostname.is_empty() {
        root.write_file(
            Utf8Path::new("/etc/hostname"),
            format!("{}\n", plan.hostname).as_bytes(),
            0o644,
            0,
            0,
        )
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
fn install_authorized_keys(root: &GuestRoot, keys: &[String]) -> Result<()> {
    validate_sshd_recovery_contract(root)?;
    validate_agent_account(root)?;
    root.open_dir(Utf8Path::new(AGENT_HOME), false)
        .with_context(|| format!("{AGENT_HOME} must be a real directory"))?;
    let ssh_dir = Utf8Path::new("/home/agent/.ssh");
    root.set_dir_attrs(ssh_dir, 0o700, AGENT_UID, AGENT_GID)?;

    let authorized_keys = Utf8Path::new("/home/agent/.ssh/authorized_keys");
    let content = format!("{}\n", keys.join("\n"));
    root.write_file(
        authorized_keys,
        content.as_bytes(),
        0o600,
        AGENT_UID,
        AGENT_GID,
    )?;

    let written = root
        .read_to_string(authorized_keys)
        .context("verify /home/agent/.ssh/authorized_keys")?;
    if written != content {
        anyhow::bail!("authorized_keys verification failed: content changed after write");
    }
    let parsed = crate::ssh::parse_authorized_keys(&written, "installed authorized_keys")?;
    if parsed.len() != keys.len() {
        anyhow::bail!("authorized_keys verification failed: key count changed after write");
    }
    Ok(())
}

fn validate_sshd_recovery_contract(root: &GuestRoot) -> Result<()> {
    let wants = Utf8Path::new("/etc/systemd/system/multi-user.target.wants");
    let mut enabled = false;
    for unit in ["ssh.service", "sshd.service"] {
        enabled |= root
            .entry_exists(wants, unit)
            .with_context(|| format!("inspect enabled {unit}"))?;
    }
    if !enabled {
        anyhow::bail!("image has no ssh.service or sshd.service enabled for recovery access");
    }

    let mut configs = vec![Utf8PathBuf::from("/etc/ssh/sshd_config")];
    for name in root.list_names(Utf8Path::new("/etc/ssh/sshd_config.d"))? {
        if name.ends_with(".conf") {
            configs.push(Utf8Path::new("/etc/ssh/sshd_config.d").join(name));
        }
    }
    for config in configs {
        let text = match root.read_to_string(&config) {
            Ok(text) => text,
            Err(err) if is_not_found(&err) => continue,
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

fn validate_agent_account(root: &GuestRoot) -> Result<()> {
    let passwd = root
        .read_to_string(Utf8Path::new("/etc/passwd"))
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

fn remove_baked_authorized_keys(root: &GuestRoot) -> Result<()> {
    root.remove_file(Utf8Path::new("/home/agent/.ssh/authorized_keys"))
        .context("remove baked /home/agent/.ssh/authorized_keys")
}

/// Remove every `ssh_host_*` file from a rootfs `/etc/ssh`. A missing directory
/// is not an error (the image may not ship sshd).
fn remove_ssh_hostkeys(root: &GuestRoot) -> Result<()> {
    let ssh_dir = Utf8Path::new("/etc/ssh");
    for name in root.list_names(ssh_dir)? {
        if name.starts_with("ssh_host_") {
            root.remove_file(&ssh_dir.join(&name))
                .with_context(|| format!("remove {name}"))?;
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
            from_literal: content.to_string(),
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
            files: vec![literal(
                "/home/agent/.env",
                "TOKEN=abc",
                "0600",
                "1000:1000",
            )],
            ..Provision::default()
        };

        let plan = ProvisionPlan::from_provision(&provision).unwrap();

        assert_eq!(plan.hostname, "hermes");
        assert!(plan.reset_machine_id);
        assert!(plan.reset_ssh_hostkeys);
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.files[0].mode, 0o600);
        assert_eq!((plan.files[0].uid, plan.files[0].gid), (1000, 1000));
        assert_eq!(
            plan.files[0].content,
            FileContent::Literal("TOKEN=abc".to_string())
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

    fn plan_for_file(dest: &str, content: &str) -> ProvisionPlan {
        ProvisionPlan {
            files: vec![PlannedFile {
                dest: Utf8PathBuf::from(dest),
                content: FileContent::Literal(content.to_string()),
                mode: 0o600,
                uid: unsafe { libc::geteuid() },
                gid: unsafe { libc::getegid() },
            }],
            reset_machine_id: false,
            reset_ssh_hostkeys: false,
            hostname: String::new(),
            authorized_keys: Vec::new(),
            allow_no_ssh: false,
        }
    }

    #[tokio::test]
    async fn apply_writes_regular_files_beneath_guest_root() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(root.path().to_path_buf()).unwrap();
        apply_to_root(&root, &plan_for_file("/etc/app/token", "secret"))
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(root.join("etc/app/token")).unwrap(),
            "secret"
        );
        assert_eq!(
            std::fs::metadata(root.join("etc/app/token"))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777,
            0o600
        );
    }

    #[tokio::test]
    async fn apply_rejects_symlinked_parent_that_escapes_guest_root() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        let root_path = Utf8PathBuf::from_path_buf(root.path().to_path_buf()).unwrap();

        let err = apply_to_root(&root_path, &plan_for_file("/escape/owned", "no"))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("without following symlinks"));
        assert!(!outside.path().join("owned").exists());
    }

    #[tokio::test]
    async fn apply_rejects_symlinked_final_write_target() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::NamedTempFile::new().unwrap();
        std::fs::create_dir_all(root.path().join("etc")).unwrap();
        symlink(outside.path(), root.path().join("etc/token")).unwrap();
        let root_path = Utf8PathBuf::from_path_buf(root.path().to_path_buf()).unwrap();

        let err = apply_to_root(&root_path, &plan_for_file("/etc/token", "no"))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("not a regular file"));
        assert_eq!(std::fs::read(outside.path()).unwrap(), b"");
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
        std::fs::create_dir_all(&ssh).unwrap();
        std::fs::write(ssh.join("ssh_host_rsa_key"), b"k").unwrap();
        std::fs::write(ssh.join("ssh_host_rsa_key.pub"), b"k").unwrap();
        std::fs::write(ssh.join("sshd_config"), b"cfg").unwrap();

        let root = GuestRoot::open(&Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap())
            .unwrap();
        remove_ssh_hostkeys(&root).unwrap();

        assert!(!ssh.join("ssh_host_rsa_key").exists());
        assert!(!ssh.join("ssh_host_rsa_key.pub").exists());
        assert!(ssh.join("sshd_config").exists());
    }

    #[tokio::test]
    async fn remove_ssh_hostkeys_tolerates_missing_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let root = GuestRoot::open(&Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap())
            .unwrap();
        assert!(remove_ssh_hostkeys(&root).is_ok());
    }
}
