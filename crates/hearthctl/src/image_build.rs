use crate::{
    client::hearth_request,
    image_lint::{self, LintCtx},
    oci::{
        buildah_bud_args, buildah_push_args, command, data_dir, parent, read_oci_process,
        remove_dir_if_exists, remove_file_if_exists, run_status, umoci_unpack_args_with_rootless,
        BuildNetwork,
    },
};
use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use hearth_proto::{ImageManifest, Verb};
use serde_json::{json, Map, Value};
use std::{ffi::OsStr, fs};

/// External tools the build shells out to, each paired with a distro-generic
/// package hint. Checked up front so a missing tool fails immediately instead
/// of after a multi-minute buildah run.
const BUILD_TOOLS: &[(&str, &str)] = &[
    ("buildah", "buildah"),
    ("umoci", "umoci"),
    (
        "qemu-img",
        "qemu-utils (Debian/Ubuntu) or qemu-img (Fedora/RHEL)",
    ),
    ("mkfs.ext4", "e2fsprogs"),
];

#[derive(Debug, Clone)]
pub struct BuildOptions {
    pub name: String,
    pub dockerfile: Utf8PathBuf,
    pub context: Utf8PathBuf,
    pub disk_gib: u64,
    pub rootless: bool,
    pub network: BuildNetwork,
    pub build_args: Vec<String>,
    /// Skip the §2.2 rootfs linter. Documented for images that boot something
    /// other than systemd, whose contract the linter does not model.
    pub skip_lint: bool,
    pub socket: Utf8PathBuf,
}

#[derive(Debug, Clone)]
struct BuildPaths {
    image_layout: Utf8PathBuf,
    bundle: Utf8PathBuf,
    rootfs: Utf8PathBuf,
    output_dir: Utf8PathBuf,
    raw_disk: Utf8PathBuf,
    qcow2: Utf8PathBuf,
    manifest: Utf8PathBuf,
}

pub async fn build(opts: BuildOptions) -> Result<()> {
    preflight_tools()?;
    validate_image_name(&opts.name)?;
    for build_arg in &opts.build_args {
        validate_build_arg(build_arg)?;
    }
    if opts.disk_gib == 0 {
        bail!("--disk must be at least 1 GiB");
    }
    if !opts.dockerfile.exists() {
        bail!("Dockerfile not found: {}", opts.dockerfile);
    }
    if !opts.context.exists() {
        bail!("build context not found: {}", opts.context);
    }
    if !opts.context.is_dir() {
        bail!("build context is not a directory: {}", opts.context);
    }

    let data_dir = data_dir()?;
    let paths = BuildPaths::new(&data_dir, &opts.name);

    run_status(
        command(
            "buildah",
            buildah_bud_args(
                &opts.name,
                &opts.dockerfile,
                &opts.context,
                opts.network,
                &opts.build_args,
            ),
        ),
        "buildah bud",
    )
    .await?;

    remove_dir_if_exists(&paths.image_layout)
        .with_context(|| format!("remove old image layout {}", paths.image_layout))?;
    fs::create_dir_all(parent(&paths.image_layout)?)?;
    run_status(
        command(
            "buildah",
            buildah_push_args(&opts.name, &paths.image_layout),
        ),
        "buildah push",
    )
    .await?;

    remove_dir_if_exists(&paths.bundle)
        .with_context(|| format!("remove old bundle {}", paths.bundle))?;
    fs::create_dir_all(parent(&paths.bundle)?)?;
    run_status(
        command(
            "umoci",
            umoci_unpack_args_with_rootless(&paths.image_layout, &paths.bundle, opts.rootless),
        ),
        "umoci unpack",
    )
    .await?;

    let process = read_oci_process(&paths.bundle)?;
    let mut manifest =
        ImageManifest::from_oci_process(process).map_err(|message| anyhow!(message))?;
    // Declare guestd = true when the rootfs actually carries the agent-plane
    // daemon and enables it (docs/agent-plane.md §2.5) — automatic for images
    // built on the current vm-base. Only a guestd-declaring image may back an
    // `agent = true` service; the linter warns when it is absent.
    manifest.guestd = image_lint::rootfs_has_guestd(&paths.rootfs);

    // Validate the unpacked tree before we spend minutes turning it into a disk
    // (§2.2). Runs after umoci unpack, before mkfs.
    if opts.skip_lint {
        eprintln!("hearthctl: --skip-lint set, skipping rootfs lint");
    } else {
        image_lint::enforce(&image_lint::lint(&LintCtx {
            rootfs: paths.rootfs.clone(),
            manifest: manifest.clone(),
        }))?;
    }

    materialize_rootfs(&paths, opts.disk_gib).await?;
    fs::write(&paths.manifest, toml::to_string_pretty(&manifest)?)?;

    let result = import_image(&opts, &paths).await?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

impl BuildPaths {
    fn new(data_dir: &Utf8Path, name: &str) -> Self {
        let output_dir = data_dir.join("image-builds").join(name);
        let bundle = data_dir.join("bundles").join(name);
        Self {
            image_layout: data_dir.join("oci-layouts").join(name),
            rootfs: bundle.join("rootfs"),
            bundle,
            raw_disk: output_dir.join(format!("{name}.raw")),
            qcow2: output_dir.join(format!("{name}.qcow2")),
            manifest: output_dir.join(format!("{name}.hearth.toml")),
            output_dir,
        }
    }
}

async fn materialize_rootfs(paths: &BuildPaths, disk_gib: u64) -> Result<()> {
    remove_dir_if_exists(&paths.output_dir)
        .with_context(|| format!("remove old build output {}", paths.output_dir))?;
    fs::create_dir_all(&paths.output_dir)?;
    run_status(
        command(
            "qemu-img",
            qemu_img_create_raw_args(&paths.raw_disk, disk_gib),
        ),
        "qemu-img create raw root disk",
    )
    .await?;
    run_status(
        command("mkfs.ext4", mkfs_ext4_args(&paths.rootfs, &paths.raw_disk)),
        "mkfs.ext4 rootfs",
    )
    .await?;
    run_status(
        command(
            "qemu-img",
            qemu_img_convert_args(&paths.raw_disk, &paths.qcow2),
        ),
        "qemu-img convert qcow2 root disk",
    )
    .await?;
    remove_file_if_exists(&paths.raw_disk)?;
    Ok(())
}

async fn import_image(opts: &BuildOptions, paths: &BuildPaths) -> Result<Value> {
    hearth_request(
        &opts.socket,
        Verb::ImageImport,
        Map::from_iter([
            ("name".to_string(), json!(opts.name)),
            ("qcow2_path".to_string(), json!(paths.qcow2)),
            ("manifest_path".to_string(), json!(paths.manifest)),
        ]),
    )
    .await
}

fn qemu_img_create_raw_args(raw_disk: &Utf8Path, disk_gib: u64) -> Vec<String> {
    vec![
        "create".to_string(),
        "-f".to_string(),
        "raw".to_string(),
        raw_disk.to_string(),
        format!("{disk_gib}G"),
    ]
}

fn mkfs_ext4_args(rootfs: &Utf8Path, raw_disk: &Utf8Path) -> Vec<String> {
    vec![
        "-F".to_string(),
        "-d".to_string(),
        rootfs.to_string(),
        raw_disk.to_string(),
    ]
}

fn qemu_img_convert_args(raw_disk: &Utf8Path, qcow2: &Utf8Path) -> Vec<String> {
    vec![
        "convert".to_string(),
        "-f".to_string(),
        "raw".to_string(),
        "-O".to_string(),
        "qcow2".to_string(),
        raw_disk.to_string(),
        qcow2.to_string(),
    ]
}

fn preflight_tools() -> Result<()> {
    let missing = missing_tools(std::env::var_os("PATH").as_deref());
    if missing.is_empty() {
        return Ok(());
    }
    bail!(
        "missing build tools required by `image build`:\n  {}",
        missing.join("\n  ")
    );
}

/// Returns a `"<tool> not found — install <pkg>"` line for each `BUILD_TOOLS`
/// entry absent from `path`. Split from `preflight_tools` so the PATH is
/// injectable in tests.
fn missing_tools(path: Option<&OsStr>) -> Vec<String> {
    BUILD_TOOLS
        .iter()
        .filter(|(tool, _)| !command_on_path(tool, path))
        .map(|(tool, pkg)| format!("{tool} not found — install {pkg}"))
        .collect()
}

fn command_on_path(command: &str, path: Option<&OsStr>) -> bool {
    path.map(|path| std::env::split_paths(&path).any(|dir| dir.join(command).is_file()))
        .unwrap_or(false)
}

fn validate_build_arg(arg: &str) -> Result<()> {
    if !arg.contains('=') {
        bail!("--build-arg must be KEY=VALUE (got {arg:?})");
    }
    Ok(())
}

fn validate_image_name(name: &str) -> Result<()> {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        bail!("image names must be kebab-case and start with a letter");
    };
    if !first.is_ascii_lowercase() {
        bail!("image names must be kebab-case and start with a letter");
    }
    let mut last_was_dash = false;
    for c in chars {
        match c {
            'a'..='z' | '0'..='9' => last_was_dash = false,
            '-' if !last_was_dash => last_was_dash = true,
            _ => bail!("image names must be kebab-case and start with a letter"),
        }
    }
    if last_was_dash {
        bail!("image names must be kebab-case and start with a letter");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn materialization_uses_raw_ext4_then_qcow2() {
        assert_eq!(
            qemu_img_create_raw_args(Utf8Path::new("/tmp/root.raw"), 40),
            vec!["create", "-f", "raw", "/tmp/root.raw", "40G"]
        );
        assert_eq!(
            mkfs_ext4_args(Utf8Path::new("/tmp/rootfs"), Utf8Path::new("/tmp/root.raw")),
            vec!["-F", "-d", "/tmp/rootfs", "/tmp/root.raw"]
        );
        assert_eq!(
            qemu_img_convert_args(
                Utf8Path::new("/tmp/root.raw"),
                Utf8Path::new("/tmp/root.qcow2")
            ),
            vec![
                "convert",
                "-f",
                "raw",
                "-O",
                "qcow2",
                "/tmp/root.raw",
                "/tmp/root.qcow2"
            ]
        );
    }

    #[test]
    fn image_names_are_kebab_case() {
        assert!(validate_image_name("exeuntu").is_ok());
        assert!(validate_image_name("debian-12").is_ok());
        assert!(validate_image_name("Bad").is_err());
        assert!(validate_image_name("bad_name").is_err());
        assert!(validate_image_name("bad-").is_err());
    }

    #[test]
    fn build_args_require_an_equals() {
        assert!(validate_build_arg("HERMES_BRANCH=main").is_ok());
        // Splitting on the first '=' only: values may themselves contain '='.
        assert!(validate_build_arg("URL=https://x/?a=b").is_ok());
        let err = validate_build_arg("HERMES_BRANCH").unwrap_err();
        assert!(err.to_string().contains("KEY=VALUE"));
    }

    #[test]
    fn missing_tools_lists_every_gap_with_a_hint() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("bin");
        std::fs::create_dir(&bin).unwrap();
        // Only two of the four tools are present on this fake PATH.
        std::fs::write(bin.join("buildah"), b"").unwrap();
        std::fs::write(bin.join("qemu-img"), b"").unwrap();

        let path = std::env::join_paths([&bin]).unwrap();
        let missing = missing_tools(Some(path.as_os_str()));

        assert_eq!(missing.len(), 2);
        assert!(missing.iter().any(|m| m.starts_with("umoci not found")));
        assert!(missing
            .iter()
            .any(|m| m == "mkfs.ext4 not found — install e2fsprogs"));
        assert!(!missing.iter().any(|m| m.starts_with("buildah")));
        assert!(!missing.iter().any(|m| m.starts_with("qemu-img")));
    }

    #[test]
    fn missing_tools_reports_all_when_path_absent() {
        assert_eq!(missing_tools(None).len(), BUILD_TOOLS.len());
    }
}
