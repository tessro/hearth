//! Build-time rootfs linter (REFACTOR_PROPOSAL.md §2.2).
//!
//! Three of the 2026-07 bring-up's four boot failures were image-content bugs
//! that only surfaced at runtime — one boot each (~10 min to build + boot +
//! read the serial console). This linter walks the unpacked rootfs after
//! `umoci unpack` and before `mkfs.ext4`, turning those runtime failures into a
//! build-time reject or warning. Every check is a pure function over the tree,
//! so the whole thing is unit-testable without KVM or root.
//!
//! REJECT aborts the build (all rejects are listed at once, so one build
//! surfaces every blocker). WARN prints and continues.

use anyhow::{bail, Result};
use camino::Utf8PathBuf;
use hearth_proto::ImageManifest;
use std::fs;
use std::os::unix::fs::PermissionsExt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Reject,
    Warn,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    pub check: &'static str,
    pub severity: Severity,
    pub message: String,
}

/// Everything a check needs: the unpacked rootfs tree and the resolved manifest
/// (owned, so checks are plain `fn` pointers with no lifetime to thread).
pub struct LintCtx {
    pub rootfs: Utf8PathBuf,
    pub manifest: ImageManifest,
}

type Check = fn(&LintCtx) -> Option<Finding>;

/// The check table. Order is the order findings are reported in.
const CHECKS: &[Check] = &[
    check_init_present_and_executable,
    check_fstab_root_entry,
    check_init_exec_form,
    check_udevd_enabled,
    check_network_matches_en,
    check_sshd_enabled,
    check_dbus_enabled,
    check_pam_systemd_present,
    check_serial_getty_masked,
    check_growfs_in_fstab,
];

/// Run every check, dropping the ones that pass.
pub fn lint(ctx: &LintCtx) -> Vec<Finding> {
    CHECKS.iter().filter_map(|check| check(ctx)).collect()
}

/// Print findings and fail if any is a REJECT, listing all rejects at once.
pub fn enforce(findings: &[Finding]) -> Result<()> {
    for finding in findings {
        let tag = match finding.severity {
            Severity::Reject => "REJECT",
            Severity::Warn => "WARN",
        };
        eprintln!(
            "hearthctl: image lint {tag} [{}] {}",
            finding.check, finding.message
        );
    }
    let rejects: Vec<&Finding> = findings
        .iter()
        .filter(|f| f.severity == Severity::Reject)
        .collect();
    if rejects.is_empty() {
        return Ok(());
    }
    let list = rejects
        .iter()
        .map(|f| format!("  - [{}] {}", f.check, f.message))
        .collect::<Vec<_>>()
        .join("\n");
    bail!(
        "image lint rejected {} check(s) (pass --skip-lint only for images that boot something other than systemd):\n{list}",
        rejects.len()
    );
}

// --- REJECT checks -------------------------------------------------------

/// The kernel's `init=` points at the manifest init. If it is missing or not
/// executable in the rootfs the guest panics on `run_init_process` with no
/// serial output past the kernel — the single least-diagnosable boot failure.
fn check_init_present_and_executable(ctx: &LintCtx) -> Option<Finding> {
    let init = &ctx.manifest.init;
    let path = under(ctx, init);
    match fs::metadata(&path) {
        Ok(md) if md.is_file() && md.permissions().mode() & 0o111 != 0 => None,
        Ok(_) => Some(reject(
            "init-executable",
            format!("resolved OCI init {init} is present but not executable in the rootfs"),
        )),
        Err(_) => Some(reject(
            "init-present",
            format!("resolved OCI init {init} is not present in the rootfs"),
        )),
    }
}

/// No `/` entry in fstab means systemd never remounts root read-write (and
/// x-systemd.growfs never runs) — the guest boots read-only and every write
/// unit fails in a confusing cascade.
fn check_fstab_root_entry(ctx: &LintCtx) -> Option<Finding> {
    let fstab = under(ctx, "/etc/fstab");
    let Ok(text) = fs::read_to_string(&fstab) else {
        return Some(reject(
            "fstab-root",
            "/etc/fstab is missing — no root filesystem mount is defined".to_string(),
        ));
    };
    if fstab_root_line(&text).is_some() {
        None
    } else {
        Some(reject(
            "fstab-root",
            "/etc/fstab has no entry mounting the root filesystem (mountpoint \"/\")".to_string(),
        ))
    }
}

/// Shell-form init (`CMD command` → args[0] is not an absolute path) cannot be
/// used as the kernel `init=`. This mirrors the manifest's own validation so a
/// shell-form image is rejected by the linter too, not just deep in `create`.
fn check_init_exec_form(ctx: &LintCtx) -> Option<Finding> {
    let arg0 = ctx
        .manifest
        .oci
        .args
        .first()
        .map(String::as_str)
        .unwrap_or("");
    if arg0.starts_with('/') {
        None
    } else {
        Some(reject(
            "init-exec-form",
            format!("OCI init is shell-form: args[0] {arg0:?} is not an absolute path"),
        ))
    }
}

// --- WARN checks ---------------------------------------------------------

/// Without systemd-udevd enabled the NIC is never initialized: `networkctl`
/// shows it `pending` forever and the VM never gets an address (inventory #4,
/// one full boot cycle lost to a NIC that never came up).
fn check_udevd_enabled(ctx: &LintCtx) -> Option<Finding> {
    let enabled = [
        "sysinit.target.wants",
        "sockets.target.wants",
        "multi-user.target.wants",
    ]
    .iter()
    .any(|wants| wants_has(ctx, wants, |name| name.starts_with("systemd-udevd")));
    if enabled {
        None
    } else {
        Some(warn(
            "udevd",
            "systemd-udevd is not enabled — the NIC will stay unmanaged and never get an address"
                .to_string(),
        ))
    }
}

/// systemd-networkd only configures an interface a `.network` matches. No file
/// matching `en*` means DHCP never runs on the (predictably-named) NIC.
fn check_network_matches_en(ctx: &LintCtx) -> Option<Finding> {
    let dir = under(ctx, "/etc/systemd/network");
    let found = fs::read_dir(&dir)
        .map(|entries| {
            entries.flatten().any(|entry| {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                name.ends_with(".network")
                    && fs::read_to_string(entry.path())
                        .map(|text| network_matches_en(&text))
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false);
    if found {
        None
    } else {
        Some(warn(
            "network-en",
            "no *.network under /etc/systemd/network matches en* — networkd will not configure the NIC"
                .to_string(),
        ))
    }
}

/// No enabled sshd unit means no way to log in after boot to inspect a failing
/// workload — the difference between a 1-minute fix and another boot cycle.
fn check_sshd_enabled(ctx: &LintCtx) -> Option<Finding> {
    let enabled = wants_has(ctx, "multi-user.target.wants", |name| {
        name == "ssh.service" || name == "sshd.service"
    });
    if enabled {
        None
    } else {
        Some(warn(
            "sshd",
            "no sshd unit enabled — there will be no way to SSH into the VM".to_string(),
        ))
    }
}

/// The system bus is what logind is reached through and what socket-activates
/// the per-user session bus. With no dbus.socket/dbus.service enabled the agent
/// gets no logind session: `systemctl --user`, `loginctl`, and XDG_RUNTIME_DIR
/// are all dead — the exact "container/VM lacks the infrastructure" the field
/// report described.
fn check_dbus_enabled(ctx: &LintCtx) -> Option<Finding> {
    let enabled = ["sockets.target.wants", "multi-user.target.wants"]
        .iter()
        .any(|wants| {
            wants_has(ctx, wants, |name| {
                name == "dbus.socket" || name == "dbus.service"
            })
        });
    if enabled {
        None
    } else {
        Some(warn(
            "dbus",
            "dbus is not enabled (no dbus.socket/dbus.service under the *.target.wants dirs) — user sessions and logind will not work"
                .to_string(),
        ))
    }
}

/// pam_systemd.so is what turns an SSH login into a registered logind session
/// with XDG_RUNTIME_DIR (/run/user/1000) and a user manager. Missing it, logins
/// still succeed but land in a session-less shell where `systemctl --user` and
/// $XDG_RUNTIME_DIR do not work — invisible until an agent tries to use them.
fn check_pam_systemd_present(ctx: &LintCtx) -> Option<Finding> {
    let present = ["usr/lib", "lib"]
        .iter()
        .any(|libdir| security_dir_has_pam_systemd(ctx, libdir));
    if present {
        None
    } else {
        Some(warn(
            "pam-systemd",
            "pam_systemd.so is absent (no /usr/lib/*/security/pam_systemd.so or /lib/*/security/pam_systemd.so) — SSH logins will not get XDG_RUNTIME_DIR / user managers"
                .to_string(),
        ))
    }
}

/// An unmasked serial-getty on ttyS0 races the kernel console and hangs boot for
/// ~90s (inventory #5). Masked = a symlink to /dev/null.
fn check_serial_getty_masked(ctx: &LintCtx) -> Option<Finding> {
    let path = under(ctx, "/etc/systemd/system/serial-getty@ttyS0.service");
    let masked = fs::symlink_metadata(&path)
        .map(|md| md.file_type().is_symlink())
        .unwrap_or(false)
        && fs::read_link(&path)
            .map(|target| target == std::path::Path::new("/dev/null"))
            .unwrap_or(false);
    if masked {
        None
    } else {
        Some(warn(
            "getty-mask",
            "serial-getty@ttyS0 is not masked to /dev/null — it can race the kernel console and hang boot ~90s"
                .to_string(),
        ))
    }
}

/// Without x-systemd.growfs on the root entry the filesystem stays the image's
/// build size and never expands to the `--disk` size, silently filling up.
fn check_growfs_in_fstab(ctx: &LintCtx) -> Option<Finding> {
    let fstab = under(ctx, "/etc/fstab");
    let text = fs::read_to_string(&fstab).unwrap_or_default();
    let has_growfs = fstab_root_line(&text)
        .and_then(|line| line.split_whitespace().nth(3))
        .map(|opts| opts.split(',').any(|opt| opt == "x-systemd.growfs"))
        .unwrap_or(false);
    if has_growfs {
        None
    } else {
        Some(warn(
            "growfs",
            "root fstab entry lacks x-systemd.growfs — the disk will not expand to the created size"
                .to_string(),
        ))
    }
}

// --- helpers -------------------------------------------------------------

/// Resolve an absolute guest path against the unpacked rootfs. The leading `/`
/// is stripped so `join` does not discard the rootfs prefix.
fn under(ctx: &LintCtx, abs: &str) -> Utf8PathBuf {
    ctx.rootfs.join(abs.trim_start_matches('/'))
}

/// The first non-comment fstab line whose mountpoint (field 2) is `/`.
fn fstab_root_line(text: &str) -> Option<&str> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .find(|line| line.split_whitespace().nth(1) == Some("/"))
}

/// True if any `.wants` entry name satisfies `pred`.
fn wants_has(ctx: &LintCtx, wants: &str, pred: impl Fn(&str) -> bool) -> bool {
    let dir = under(ctx, "/etc/systemd/system").join(wants);
    match fs::read_dir(&dir) {
        Ok(entries) => entries
            .flatten()
            .any(|entry| entry.file_name().to_str().map(&pred).unwrap_or(false)),
        Err(_) => false,
    }
}

/// True if `<rootfs>/<libdir>/*/security/pam_systemd.so` exists — the one-level
/// glob over the multiarch dir (e.g. `usr/lib/x86_64-linux-gnu/security`), the
/// canonical PAM module location on Debian/Ubuntu.
fn security_dir_has_pam_systemd(ctx: &LintCtx, libdir: &str) -> bool {
    let base = ctx.rootfs.join(libdir);
    match fs::read_dir(&base) {
        Ok(entries) => entries
            .flatten()
            .any(|entry| entry.path().join("security/pam_systemd.so").exists()),
        Err(_) => false,
    }
}

/// True if a `.network` file has a `[Match] Name=` value that would match an
/// `en*`-named interface (`en*`, `ens3`, `enp0s1`, ...).
fn network_matches_en(text: &str) -> bool {
    text.lines().map(str::trim).any(|line| {
        line.strip_prefix("Name=")
            .map(|value| {
                value
                    .split_whitespace()
                    .any(|token| token.starts_with("en"))
            })
            .unwrap_or(false)
    })
}

fn reject(check: &'static str, message: String) -> Finding {
    Finding {
        check,
        severity: Severity::Reject,
        message,
    }
}

fn warn(check: &'static str, message: String) -> Finding {
    Finding {
        check,
        severity: Severity::Warn,
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use camino::Utf8Path;
    use hearth_proto::OciProcess;
    use std::os::unix::fs::symlink;

    fn manifest() -> ImageManifest {
        ImageManifest::docker_rootfs(OciProcess {
            args: vec!["/usr/local/bin/init".to_string()],
            env: vec![],
            cwd: "/".to_string(),
        })
        .unwrap()
    }

    fn ctx(rootfs: &Utf8Path) -> LintCtx {
        LintCtx {
            rootfs: rootfs.to_owned(),
            manifest: manifest(),
        }
    }

    /// Populate `root` with a rootfs that passes every check.
    fn write_good_rootfs(root: &Utf8Path) {
        let bin = root.join("usr/local/bin");
        fs::create_dir_all(&bin).unwrap();
        let init = bin.join("init");
        fs::write(&init, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&init, fs::Permissions::from_mode(0o755)).unwrap();

        let etc = root.join("etc");
        fs::create_dir_all(&etc).unwrap();
        fs::write(
            etc.join("fstab"),
            "/dev/vda / ext4 defaults,x-systemd.growfs 0 1\n",
        )
        .unwrap();

        let net = etc.join("systemd/network");
        fs::create_dir_all(&net).unwrap();
        fs::write(
            net.join("20-dhcp.network"),
            "[Match]\nName=en*\nName=eth*\n\n[Network]\nDHCP=yes\n",
        )
        .unwrap();

        let sys = etc.join("systemd/system");
        for wants in [
            "sysinit.target.wants",
            "sockets.target.wants",
            "multi-user.target.wants",
        ] {
            fs::create_dir_all(sys.join(wants)).unwrap();
        }
        fs::write(sys.join("sysinit.target.wants/systemd-udevd.service"), b"").unwrap();
        fs::write(
            sys.join("sockets.target.wants/systemd-udevd-control.socket"),
            b"",
        )
        .unwrap();
        fs::write(sys.join("multi-user.target.wants/ssh.service"), b"").unwrap();
        fs::write(sys.join("sockets.target.wants/dbus.socket"), b"").unwrap();
        fs::write(sys.join("multi-user.target.wants/dbus.service"), b"").unwrap();
        symlink("/dev/null", sys.join("serial-getty@ttyS0.service")).unwrap();

        // pam_systemd.so under the multiarch security dir (Debian/Ubuntu layout).
        let security = root.join("usr/lib/x86_64-linux-gnu/security");
        fs::create_dir_all(&security).unwrap();
        fs::write(security.join("pam_systemd.so"), b"").unwrap();
    }

    fn tmp() -> (tempfile::TempDir, Utf8PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(dir.path().join("rootfs")).unwrap();
        fs::create_dir_all(&root).unwrap();
        write_good_rootfs(&root);
        (dir, root)
    }

    #[test]
    fn a_good_rootfs_has_no_findings() {
        let (_dir, root) = tmp();
        assert_eq!(lint(&ctx(&root)), Vec::new());
    }

    #[test]
    fn init_present_and_executable() {
        let (_dir, root) = tmp();
        assert!(check_init_present_and_executable(&ctx(&root)).is_none());

        // Missing entirely.
        fs::remove_file(root.join("usr/local/bin/init")).unwrap();
        let f = check_init_present_and_executable(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Reject);
        assert_eq!(f.check, "init-present");

        // Present but not executable.
        let init = root.join("usr/local/bin/init");
        fs::write(&init, b"#!/bin/sh\n").unwrap();
        fs::set_permissions(&init, fs::Permissions::from_mode(0o644)).unwrap();
        let f = check_init_present_and_executable(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Reject);
        assert_eq!(f.check, "init-executable");
    }

    #[test]
    fn fstab_root_entry() {
        let (_dir, root) = tmp();
        assert!(check_fstab_root_entry(&ctx(&root)).is_none());

        // No root mountpoint.
        fs::write(
            root.join("etc/fstab"),
            "# only comments\ntmpfs /tmp tmpfs 0 0\n",
        )
        .unwrap();
        let f = check_fstab_root_entry(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Reject);

        // Missing file.
        fs::remove_file(root.join("etc/fstab")).unwrap();
        let f = check_fstab_root_entry(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Reject);
    }

    #[test]
    fn init_exec_form() {
        let (_dir, root) = tmp();
        assert!(check_init_exec_form(&ctx(&root)).is_none());

        // Shell-form init (relative args[0]) is rejected. Built by hand because
        // ImageManifest::docker_rootfs refuses to construct a relative init.
        let mut c = ctx(&root);
        c.manifest.oci.args = vec!["python3".to_string()];
        let f = check_init_exec_form(&c).unwrap();
        assert_eq!(f.severity, Severity::Reject);
        assert_eq!(f.check, "init-exec-form");
    }

    #[test]
    fn udevd_enabled() {
        let (_dir, root) = tmp();
        assert!(check_udevd_enabled(&ctx(&root)).is_none());

        fs::remove_file(root.join("etc/systemd/system/sysinit.target.wants/systemd-udevd.service"))
            .unwrap();
        fs::remove_file(
            root.join("etc/systemd/system/sockets.target.wants/systemd-udevd-control.socket"),
        )
        .unwrap();
        let f = check_udevd_enabled(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.check, "udevd");
    }

    #[test]
    fn network_matches_en() {
        let (_dir, root) = tmp();
        assert!(check_network_matches_en(&ctx(&root)).is_none());

        // A .network that matches only eth* (not en*) still fails the en* check.
        fs::write(
            root.join("etc/systemd/network/20-dhcp.network"),
            "[Match]\nName=eth0\n\n[Network]\nDHCP=yes\n",
        )
        .unwrap();
        let f = check_network_matches_en(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.check, "network-en");
    }

    #[test]
    fn sshd_enabled() {
        let (_dir, root) = tmp();
        assert!(check_sshd_enabled(&ctx(&root)).is_none());

        fs::remove_file(root.join("etc/systemd/system/multi-user.target.wants/ssh.service"))
            .unwrap();
        let f = check_sshd_enabled(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.check, "sshd");
    }

    #[test]
    fn dbus_enabled() {
        let (_dir, root) = tmp();
        assert!(check_dbus_enabled(&ctx(&root)).is_none());

        // Removing both the socket and service enablement fires the warning.
        fs::remove_file(root.join("etc/systemd/system/sockets.target.wants/dbus.socket")).unwrap();
        fs::remove_file(root.join("etc/systemd/system/multi-user.target.wants/dbus.service"))
            .unwrap();
        let f = check_dbus_enabled(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.check, "dbus");
    }

    #[test]
    fn pam_systemd_present() {
        let (_dir, root) = tmp();
        assert!(check_pam_systemd_present(&ctx(&root)).is_none());

        fs::remove_file(root.join("usr/lib/x86_64-linux-gnu/security/pam_systemd.so")).unwrap();
        let f = check_pam_systemd_present(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.check, "pam-systemd");
    }

    #[test]
    fn serial_getty_masked() {
        let (_dir, root) = tmp();
        assert!(check_serial_getty_masked(&ctx(&root)).is_none());

        // A real unit file (not a /dev/null symlink) is unmasked.
        let getty = root.join("etc/systemd/system/serial-getty@ttyS0.service");
        fs::remove_file(&getty).unwrap();
        fs::write(&getty, b"[Service]\n").unwrap();
        let f = check_serial_getty_masked(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.check, "getty-mask");
    }

    #[test]
    fn growfs_in_fstab() {
        let (_dir, root) = tmp();
        assert!(check_growfs_in_fstab(&ctx(&root)).is_none());

        fs::write(root.join("etc/fstab"), "/dev/vda / ext4 defaults 0 1\n").unwrap();
        let f = check_growfs_in_fstab(&ctx(&root)).unwrap();
        assert_eq!(f.severity, Severity::Warn);
        assert_eq!(f.check, "growfs");
    }

    #[test]
    fn enforce_bails_on_reject_and_lists_all() {
        let findings = vec![
            reject("a", "first".to_string()),
            warn("b", "second".to_string()),
            reject("c", "third".to_string()),
        ];
        let err = enforce(&findings).unwrap_err().to_string();
        assert!(err.contains("2 check(s)"));
        assert!(err.contains("[a] first"));
        assert!(err.contains("[c] third"));
        assert!(err.contains("--skip-lint"));
    }

    #[test]
    fn enforce_passes_with_only_warnings() {
        assert!(enforce(&[warn("b", "just a warning".to_string())]).is_ok());
        assert!(enforce(&[]).is_ok());
    }
}
