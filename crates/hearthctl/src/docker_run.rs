use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use serde_json::{json, Value};
use std::{
    env, fs, io,
    os::unix::fs::FileTypeExt,
    path::PathBuf,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command,
    time::sleep,
};

const DEFAULT_DATA_DIR: &str = ".local/share/hearth";
const INITRAMFS_NAME: &str = "initramfs.cpio.gz";
const KERNEL_PATH: &str = "/run/current-system/kernel";

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub dockerfile: Utf8PathBuf,
    pub context: Utf8PathBuf,
    pub name: String,
    pub memory: String,
    pub cpus: u32,
}

#[derive(Debug, Clone)]
struct RunPaths {
    image_layout: Utf8PathBuf,
    bundle: Utf8PathBuf,
    rootfs: Utf8PathBuf,
    initramfs_image: Utf8PathBuf,
    runtime_dir: Utf8PathBuf,
    virtiofs_socket: Utf8PathBuf,
    chv_api_socket: Utf8PathBuf,
}

pub async fn run(opts: RunOptions) -> Result<()> {
    validate_name(&opts.name)?;
    validate_memory(&opts.memory)?;
    if opts.cpus == 0 {
        bail!("--cpus must be at least 1");
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
    let paths = RunPaths::new(&data_dir, &opts.name);

    run_status(
        command(
            "buildah",
            buildah_bud_args(&opts.name, &opts.dockerfile, &opts.context),
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
            umoci_unpack_args(&paths.image_layout, &paths.bundle),
        ),
        "umoci unpack",
    )
    .await?;
    ensure_guest_init(&paths.rootfs)?;

    ensure_initramfs_exists(&paths.initramfs_image)?;
    let kernel = resolve_kernel()?;

    prepare_runtime_dir(&paths.runtime_dir)?;
    let mut virtiofsd = match spawn_virtiofsd(&paths).await {
        Ok(child) => child,
        Err(err) => {
            let _ = cleanup_runtime_dir(&paths.runtime_dir);
            return Err(err);
        }
    };
    if let Err(err) = wait_for_socket(
        &paths.virtiofs_socket,
        &mut virtiofsd,
        Duration::from_secs(5),
    )
    .await
    {
        let _ = stop_child(&mut virtiofsd).await;
        let _ = cleanup_runtime_dir(&paths.runtime_dir);
        return Err(err);
    }

    let chv_status = run_cloud_hypervisor(&opts, &paths, &kernel).await;
    let virtiofsd_status = stop_child(&mut virtiofsd).await;
    let cleanup_status = cleanup_runtime_dir(&paths.runtime_dir);
    virtiofsd_status?;
    cleanup_status?;
    chv_status
}

impl RunPaths {
    fn new(data_dir: &Utf8Path, name: &str) -> Self {
        let runtime_dir = runtime_dir(name);
        Self::new_with_runtime_dir(data_dir, name, runtime_dir)
    }

    fn new_with_runtime_dir(data_dir: &Utf8Path, name: &str, runtime_dir: Utf8PathBuf) -> Self {
        let image_layout = data_dir.join("images").join(name);
        let bundle = data_dir.join("bundles").join(name);
        Self {
            rootfs: bundle.join("rootfs"),
            image_layout,
            bundle,
            initramfs_image: data_dir.join(INITRAMFS_NAME),
            virtiofs_socket: runtime_dir.join("rootfs.sock"),
            chv_api_socket: runtime_dir.join("chv.sock"),
            runtime_dir,
        }
    }
}

fn data_dir() -> Result<Utf8PathBuf> {
    if let Some(value) = env::var_os("HEARTH_DATA_DIR") {
        return Utf8PathBuf::from_path_buf(PathBuf::from(value))
            .map_err(|path| anyhow!("HEARTH_DATA_DIR is not valid UTF-8: {}", path.display()));
    }
    let home = env::var_os("HOME").ok_or_else(|| anyhow!("HOME is not set"))?;
    Utf8PathBuf::from_path_buf(PathBuf::from(home).join(DEFAULT_DATA_DIR))
        .map_err(|path| anyhow!("HOME is not valid UTF-8: {}", path.display()))
}

fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() || name == "." || name == ".." {
        bail!("--name must not be empty, '.', or '..'");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("--name may only contain ASCII letters, digits, '.', '_', and '-'");
    }
    Ok(())
}

fn validate_memory(memory: &str) -> Result<()> {
    if memory.is_empty()
        || !memory.ends_with('M')
        || !memory[..memory.len() - 1]
            .chars()
            .all(|c| c.is_ascii_digit())
    {
        bail!("--mem must use Cloud Hypervisor's MiB form, for example 512M");
    }
    Ok(())
}

fn buildah_bud_args(name: &str, dockerfile: &Utf8Path, context: &Utf8Path) -> Vec<String> {
    vec![
        "bud".to_string(),
        "-t".to_string(),
        name.to_string(),
        "-f".to_string(),
        dockerfile.to_string(),
        context.to_string(),
    ]
}

fn buildah_push_args(name: &str, image_layout: &Utf8Path) -> Vec<String> {
    vec![
        "push".to_string(),
        name.to_string(),
        format!("oci:{image_layout}:latest"),
    ]
}

fn umoci_unpack_args(image_layout: &Utf8Path, bundle: &Utf8Path) -> Vec<String> {
    vec![
        "unpack".to_string(),
        "--rootless".to_string(),
        "--image".to_string(),
        format!("{image_layout}:latest"),
        bundle.to_string(),
    ]
}

fn cloud_hypervisor_args(opts: &RunOptions, paths: &RunPaths, kernel: &Utf8Path) -> Vec<String> {
    vec![
        "--api-socket".to_string(),
        format!("path={}", paths.chv_api_socket),
        "--kernel".to_string(),
        kernel.to_string(),
        "--initramfs".to_string(),
        paths.initramfs_image.to_string(),
        "--cmdline".to_string(),
        "console=ttyS0 init=/init".to_string(),
        "--memory".to_string(),
        format!("size={},shared=on", opts.memory),
        "--cpus".to_string(),
        format!("boot={}", opts.cpus),
        "--fs".to_string(),
        format!("tag=root,socket={}", paths.virtiofs_socket),
        "--console".to_string(),
        "off".to_string(),
        "--serial".to_string(),
        "tty".to_string(),
    ]
}

fn virtiofsd_args(paths: &RunPaths) -> Vec<String> {
    vec![
        format!("--socket-path={}", paths.virtiofs_socket),
        format!("--shared-dir={}", paths.rootfs),
    ]
}

fn ensure_guest_init(rootfs: &Utf8Path) -> Result<()> {
    let init = rootfs.join("init");
    let meta = fs::metadata(&init)
        .with_context(|| format!("OCI rootfs must contain /init for this milestone: {init}"))?;
    if !is_executable(&meta) {
        bail!("OCI rootfs /init is not executable: {init}");
    }
    Ok(())
}

#[cfg(unix)]
fn is_executable(meta: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;

    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn is_executable(_meta: &fs::Metadata) -> bool {
    true
}

fn command(program: &str, args: Vec<String>) -> Command {
    let mut cmd = Command::new(program);
    cmd.args(args);
    cmd.stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd
}

async fn run_status(mut cmd: Command, label: &str) -> Result<()> {
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

async fn spawn_virtiofsd(paths: &RunPaths) -> Result<tokio::process::Child> {
    eprintln!("hearthctl: virtiofsd");
    let mut cmd = Command::new("virtiofsd");
    cmd.args(virtiofsd_args(paths))
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    cmd.spawn().context("spawn virtiofsd")
}

async fn run_cloud_hypervisor(
    opts: &RunOptions,
    paths: &RunPaths,
    kernel: &Utf8Path,
) -> Result<()> {
    eprintln!("hearthctl: cloud-hypervisor");
    let mut cmd = Command::new("cloud-hypervisor");
    cmd.args(cloud_hypervisor_args(opts, paths, kernel))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().context("spawn cloud-hypervisor")?;
    let api_ready = wait_for_chv_api(
        &paths.chv_api_socket,
        &mut child,
        Duration::from_secs(5),
    )
    .await;
    if let Err(err) = api_ready {
        let _ = child.kill().await;
        let _ = child.wait().await;
        return Err(err);
    }
    let status = tokio::select! {
        status = child.wait() => status.context("wait for cloud-hypervisor")?,
        signal = tokio::signal::ctrl_c() => {
            signal.context("wait for Ctrl-C")?;
            shutdown_cloud_hypervisor(&paths.chv_api_socket, &mut child).await;
            bail!("interrupted");
        }
    };
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("cloud-hypervisor exited with {status}"))
    }
}

async fn wait_for_chv_api(
    path: &Utf8Path,
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("cloud-hypervisor exited before opening API socket {path}: {status}");
        }
        if UnixStream::connect(path.as_str()).await.is_ok() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for cloud-hypervisor API socket {path}");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

async fn shutdown_cloud_hypervisor(socket: &Utf8Path, child: &mut tokio::process::Child) {
    if let Err(err) = chv_request(socket, "PUT", "/api/v1/vm.power-off", Some(json!({}))).await {
        eprintln!("hearthctl: vm.power-off failed: {err:#}; sending SIGKILL");
        let _ = child.kill().await;
        let _ = child.wait().await;
        return;
    }
    match tokio::time::timeout(Duration::from_secs(5), child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(err)) => {
            eprintln!("hearthctl: waiting on cloud-hypervisor failed: {err:#}");
        }
        Err(_) => {
            eprintln!("hearthctl: cloud-hypervisor did not exit after vm.power-off; sending SIGKILL");
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

async fn chv_request(
    socket: &Utf8Path,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> Result<Value> {
    let mut stream = UnixStream::connect(socket.as_str())
        .await
        .with_context(|| format!("connect CHV API socket {socket}"))?;
    let body_text = body.map(|v| v.to_string()).unwrap_or_default();
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body_text.len(),
        body_text
    );
    stream.write_all(request.as_bytes()).await?;
    stream.shutdown().await?;
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await?;
    parse_http_response(&buf)
}

fn parse_http_response(bytes: &[u8]) -> Result<Value> {
    let text = String::from_utf8_lossy(bytes);
    let (head, body) = text
        .split_once("\r\n\r\n")
        .ok_or_else(|| anyhow!("malformed HTTP response from CHV"))?;
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status from CHV"))?;
    if !(200..300).contains(&status) {
        return Err(anyhow!("CHV API returned HTTP {status}: {body}"));
    }
    if body.trim().is_empty() {
        Ok(json!({}))
    } else {
        serde_json::from_str(body).context("parse CHV JSON response")
    }
}

async fn stop_child(child: &mut tokio::process::Child) -> Result<()> {
    if child.try_wait()?.is_none() {
        child.kill().await?;
    }
    let _ = child.wait().await;
    Ok(())
}

async fn wait_for_socket(
    path: &Utf8Path,
    child: &mut tokio::process::Child,
    timeout: Duration,
) -> Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(status) = child.try_wait()? {
            bail!("virtiofsd exited before creating {path}: {status}");
        }
        if let Ok(meta) = fs::metadata(path) {
            if meta.file_type().is_socket() {
                return Ok(());
            }
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for virtiofsd socket {path}");
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn ensure_initramfs_exists(path: &Utf8Path) -> Result<()> {
    if path.is_file() {
        return Ok(());
    }
    bail!("initramfs not found: {path}; build it first with scripts/build-initramfs.sh");
}

fn runtime_dir(name: &str) -> Utf8PathBuf {
    let dirname = format!("hearth-{name}-{}", std::process::id());
    Utf8PathBuf::from_path_buf(env::temp_dir().join(&dirname))
        .unwrap_or_else(|_| Utf8PathBuf::from(format!("/tmp/{dirname}")))
}

fn prepare_runtime_dir(path: &Utf8Path) -> Result<()> {
    remove_dir_if_exists(path)?;
    fs::create_dir_all(path)?;
    Ok(())
}

fn cleanup_runtime_dir(path: &Utf8Path) -> Result<()> {
    remove_dir_if_exists(path)
}

fn resolve_kernel() -> Result<Utf8PathBuf> {
    let path = fs::canonicalize(KERNEL_PATH).with_context(|| format!("resolve {KERNEL_PATH}"))?;
    Utf8PathBuf::from_path_buf(path)
        .map_err(|path| anyhow!("kernel path is not valid UTF-8: {}", path.display()))
}

fn parent(path: &Utf8Path) -> Result<&Utf8Path> {
    path.parent()
        .ok_or_else(|| anyhow!("path has no parent: {path}"))
}

fn remove_dir_if_exists(path: &Utf8Path) -> Result<()> {
    match fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths() -> RunPaths {
        RunPaths::new_with_runtime_dir(
            Utf8Path::new("/home/tess/.local/share/hearth"),
            "hearth-test",
            Utf8PathBuf::from("/tmp/hearth-test-run"),
        )
    }

    #[test]
    fn buildah_bud_matches_proven_command() {
        assert_eq!(
            buildah_bud_args(
                "hearth-test",
                Utf8Path::new("./Dockerfile"),
                Utf8Path::new(".")
            ),
            vec!["bud", "-t", "hearth-test", "-f", "./Dockerfile", "."]
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
    fn umoci_unpack_uses_rootless_bundle() {
        assert_eq!(
            umoci_unpack_args(
                Utf8Path::new("/home/tess/.local/share/hearth/images/hearth-test"),
                Utf8Path::new("/home/tess/.local/share/hearth/bundles/hearth-test")
            ),
            vec![
                "unpack",
                "--rootless",
                "--image",
                "/home/tess/.local/share/hearth/images/hearth-test:latest",
                "/home/tess/.local/share/hearth/bundles/hearth-test"
            ]
        );
    }

    #[test]
    fn cloud_hypervisor_args_match_proven_boot_shape() {
        let opts = RunOptions {
            dockerfile: Utf8PathBuf::from("./Dockerfile"),
            context: Utf8PathBuf::from("."),
            name: "hearth-test".to_string(),
            memory: "512M".to_string(),
            cpus: 1,
        };
        assert_eq!(
            cloud_hypervisor_args(&opts, &paths(), Utf8Path::new("/run/current-system/kernel")),
            vec![
                "--api-socket",
                "path=/tmp/hearth-test-run/chv.sock",
                "--kernel",
                "/run/current-system/kernel",
                "--initramfs",
                "/home/tess/.local/share/hearth/initramfs.cpio.gz",
                "--cmdline",
                "console=ttyS0 init=/init",
                "--memory",
                "size=512M,shared=on",
                "--cpus",
                "boot=1",
                "--fs",
                "tag=root,socket=/tmp/hearth-test-run/rootfs.sock",
                "--console",
                "off",
                "--serial",
                "tty",
            ]
        );
    }

    #[test]
    fn rejects_names_that_escape_artifact_directories() {
        assert!(validate_name("hearth-test").is_ok());
        assert!(validate_name("../bad").is_err());
        assert!(validate_name("").is_err());
    }
}
