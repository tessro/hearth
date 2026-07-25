use anyhow::{anyhow, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::ValueEnum;
use hearth_proto::OciProcess;
use serde::Deserialize;
use std::{env, fs, io, path::PathBuf, process::Stdio};
use tokio::process::Command;

const DEFAULT_DATA_DIR: &str = ".local/share/hearth";

/// Network namespace for `buildah bud` RUN steps. Kept a plain enum (not tied
/// to the arg builder's internals) so `buildah_bud_args` stays a pure,
/// testable function; the CLI exposes it as `--build-network`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum BuildNetwork {
    /// Share the host network namespace.
    Host,
    /// Isolated per-build netns via the default (netavark) backend.
    Netavark,
}

impl BuildNetwork {
    /// The value passed to buildah's `--network`. `netavark` maps to buildah's
    /// `private` (a fresh netns using the default backend), which is what the
    /// netavark path actually exercises.
    fn buildah_value(self) -> &'static str {
        match self {
            BuildNetwork::Host => "host",
            BuildNetwork::Netavark => "private",
        }
    }
}

#[derive(Debug, Deserialize)]
struct OciConfig {
    process: OciProcess,
}

pub fn data_dir() -> Result<Utf8PathBuf> {
    if let Some(value) = env::var_os("HEARTH_DATA_DIR") {
        return Utf8PathBuf::from_path_buf(PathBuf::from(value))
            .map_err(|path| anyhow!("HEARTH_DATA_DIR is not valid UTF-8: {}", path.display()));
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Utf8PathBuf::from_path_buf(PathBuf::from(home).join(DEFAULT_DATA_DIR))
        .map_err(|path| anyhow!("HOME is not valid UTF-8: {}", path.display()))
}

pub fn buildah_bud_args(
    name: &str,
    dockerfile: &Utf8Path,
    context: &Utf8Path,
    network: BuildNetwork,
    build_args: &[String],
) -> Vec<String> {
    let mut args = vec![
        "bud".to_string(),
        // Cache each RUN layer: a one-line Dockerfile change reuses the earlier
        // steps instead of re-running the whole build (the Hermes reinstall
        // cost ~6 min per iteration before this).
        "--layers".to_string(),
        // Run RUN steps in the selected network namespace. `host` shares the
        // host netns; the alternative per-build netns lets netavark race its
        // own iptables chains between consecutive RUN steps ("Chain already
        // exists"), which is why `host` is the default (see --build-network).
        "--network".to_string(),
        network.buildah_value().to_string(),
    ];
    for build_arg in build_args {
        args.push("--build-arg".to_string());
        args.push(build_arg.clone());
    }
    args.extend([
        "-t".to_string(),
        name.to_string(),
        "-f".to_string(),
        dockerfile.to_string(),
        context.to_string(),
    ]);
    args
}

pub fn buildah_push_args(name: &str, image_layout: &Utf8Path) -> Vec<String> {
    vec![
        "push".to_string(),
        name.to_string(),
        format!("oci:{image_layout}:latest"),
    ]
}

pub fn umoci_unpack_args(image_layout: &Utf8Path, bundle: &Utf8Path) -> Vec<String> {
    vec![
        "unpack".to_string(),
        "--image".to_string(),
        format!("{image_layout}:latest"),
        bundle.to_string(),
    ]
}

/// Set by buildah in every process it re-executes inside the rootless user
/// namespace (`buildah unshare` and buildah's own auto-unshare) — its
/// presence means uid 0 plus the subuid range are already mapped.
const USERNS_CONFIGURED_ENV: &str = "_CONTAINERS_USERNS_CONFIGURED";

/// Re-exec the whole CLI under `buildah unshare` when building unprivileged.
///
/// The unpack must chown files to many uids (root-owned system files, the
/// uid-1000 agent home) and the setuid bits must survive into `mkfs.ext4 -d`
/// — none of which needs real privilege, only a uid 0 with subuids mapped,
/// which is exactly what `buildah unshare` provides (from the same
/// /etc/subuid setup rootless `buildah bud` already requires). umoci's own
/// `--rootless` flatten mode is deliberately not offered: it records the
/// wrong ownership into the image and strips setuid, producing a rootfs that
/// boots but has a broken sudo — invisible until something needs it.
///
/// Returns only in the already-mapped cases (real root, or inside the
/// namespace after the re-exec); otherwise the process is replaced.
pub fn reexec_under_userns_if_unprivileged() -> Result<()> {
    let euid_is_root = unsafe { libc::geteuid() } == 0;
    if !needs_userns_reexec(euid_is_root, env::var_os(USERNS_CONFIGURED_ENV).is_some()) {
        return Ok(());
    }
    let exe = env::current_exe().context("resolve current executable for buildah unshare")?;
    eprintln!("hearthctl: unprivileged build; re-executing under `buildah unshare`");
    use std::os::unix::process::CommandExt;
    let err = std::process::Command::new("buildah")
        .arg("unshare")
        .arg("--")
        .arg(exe)
        .args(env::args_os().skip(1))
        .exec();
    Err(err).context("re-exec under buildah unshare")
}

/// Pure re-exec decision: unprivileged and not yet inside the namespace.
fn needs_userns_reexec(euid_is_root: bool, userns_configured: bool) -> bool {
    !euid_is_root && !userns_configured
}

pub fn read_oci_process(bundle: &Utf8Path) -> Result<OciProcess> {
    let config_path = bundle.join("config.json");
    let text = fs::read_to_string(&config_path).with_context(|| format!("read {config_path}"))?;
    let config: OciConfig =
        serde_json::from_str(&text).with_context(|| format!("parse {config_path}"))?;
    validate_oci_process(config.process)
}

pub fn validate_oci_process(mut process: OciProcess) -> Result<OciProcess> {
    process
        .validate_common()
        .map_err(|message| anyhow!(message))?;
    Ok(process)
}

pub fn command(program: &str, args: Vec<String>) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd
}

pub async fn run_status(mut cmd: Command, label: &str) -> Result<()> {
    eprintln!("hearthctl: {label}");
    let status = cmd
        .status()
        .await
        .with_context(|| format!("spawn {label}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("{label} exited with {status}"))
    }
}

pub fn parent(path: &Utf8Path) -> Result<&Utf8Path> {
    path.parent()
        .ok_or_else(|| anyhow!("path has no parent: {path}"))
}

pub fn remove_dir_if_exists(path: &Utf8Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

pub fn remove_file_if_exists(path: &Utf8Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn buildah_bud_defaults_to_host_network_with_layers() {
        assert_eq!(
            buildah_bud_args(
                "hearth-test",
                Utf8Path::new("./Dockerfile"),
                Utf8Path::new("."),
                BuildNetwork::Host,
                &[],
            ),
            vec![
                "bud",
                "--layers",
                "--network",
                "host",
                "-t",
                "hearth-test",
                "-f",
                "./Dockerfile",
                "."
            ]
        );
    }

    #[test]
    fn buildah_bud_netavark_uses_private_netns() {
        let args = buildah_bud_args(
            "hearth-test",
            Utf8Path::new("./Dockerfile"),
            Utf8Path::new("."),
            BuildNetwork::Netavark,
            &[],
        );
        let network = args.iter().position(|a| a == "--network").unwrap();
        assert_eq!(args[network + 1], "private");
        // Layer caching is unconditional regardless of network mode.
        assert!(args.iter().any(|a| a == "--layers"));
    }

    #[test]
    fn buildah_bud_forwards_build_args() {
        assert_eq!(
            buildah_bud_args(
                "hearth-test",
                Utf8Path::new("./Dockerfile"),
                Utf8Path::new("."),
                BuildNetwork::Host,
                &[
                    "HERMES_BRANCH=main".to_string(),
                    "HERMES_COMMIT=abc123".to_string(),
                ],
            ),
            vec![
                "bud",
                "--layers",
                "--network",
                "host",
                "--build-arg",
                "HERMES_BRANCH=main",
                "--build-arg",
                "HERMES_COMMIT=abc123",
                "-t",
                "hearth-test",
                "-f",
                "./Dockerfile",
                "."
            ]
        );
    }

    #[test]
    fn buildah_push_uses_oci_layout() {
        assert_eq!(
            buildah_push_args(
                "hearth-test",
                Utf8Path::new("/home/tess/.local/share/hearth/images/hearth-test")
            ),
            vec![
                "push",
                "hearth-test",
                "oci:/home/tess/.local/share/hearth/images/hearth-test:latest"
            ]
        );
    }

    #[test]
    fn umoci_unpack_preserves_ownership_no_rootless_flatten() {
        assert_eq!(
            umoci_unpack_args(
                Utf8Path::new("/home/tess/.local/share/hearth/images/hearth-test"),
                Utf8Path::new("/home/tess/.local/share/hearth/bundles/hearth-test"),
            ),
            vec![
                "unpack",
                "--image",
                "/home/tess/.local/share/hearth/images/hearth-test:latest",
                "/home/tess/.local/share/hearth/bundles/hearth-test"
            ]
        );
    }

    #[test]
    fn userns_reexec_only_when_unprivileged_and_unmapped() {
        // Unprivileged outside the namespace: re-exec under buildah unshare.
        assert!(needs_userns_reexec(false, false));
        // Already inside `buildah unshare` (env marker set): run in place —
        // anything else would re-exec forever.
        assert!(!needs_userns_reexec(false, true));
        // Real root unpacks with full ownership directly.
        assert!(!needs_userns_reexec(true, false));
        assert!(!needs_userns_reexec(true, true));
    }

    #[test]
    fn rejects_empty_oci_process_args() {
        let err = validate_oci_process(OciProcess {
            args: Vec::new(),
            env: Vec::new(),
            cwd: "/".to_string(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("process.args"));
    }

    #[test]
    fn rejects_relative_oci_cwd() {
        let err = validate_oci_process(OciProcess {
            args: vec!["python3".to_string()],
            env: Vec::new(),
            cwd: "srv".to_string(),
        })
        .unwrap_err();
        assert!(err.to_string().contains("process.cwd"));
    }
}
