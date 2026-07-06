use crate::{
    client::hearth_request,
    oci::{
        buildah_bud_args, buildah_push_args, command, data_dir, parent, read_oci_process,
        remove_dir_if_exists, run_status, umoci_unpack_args, validate_oci_process,
    },
};
use anyhow::{anyhow, bail, Context, Result};
use camino::{Utf8Path, Utf8PathBuf};
use clap::ValueEnum;
use hearth_proto::{OciProcess, Verb};
use serde::Serialize;
use serde_json::{json, Map, Value};
use std::{
    env, fs, io,
    os::unix::fs::FileTypeExt,
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command,
    time::sleep,
};

const INITRAMFS_NAME: &str = "initramfs.cpio.gz";
const KERNEL_PATH: &str = "/run/booted-system/kernel";

#[derive(Debug, Clone)]
pub struct RunOptions {
    pub dockerfile: Utf8PathBuf,
    pub context: Utf8PathBuf,
    pub name: String,
    pub memory: String,
    pub cpus: u32,
    pub network: NetworkMode,
    pub bridge: String,
    pub tap: Option<String>,
    pub mac: Option<String>,
    pub tap_setup: bool,
    pub socket: Utf8PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum NetworkMode {
    None,
    Bridge,
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

#[derive(Debug, Serialize)]
struct RunManifest {
    args: Vec<String>,
    env: Vec<String>,
    cwd: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct NetworkConfig {
    bridge: String,
    tap: String,
    mac: String,
    setup: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TapLease {
    name: String,
    created: bool,
}

pub async fn run(opts: RunOptions) -> Result<()> {
    validate_name(&opts.name)?;
    validate_memory(&opts.memory)?;
    let network = resolve_network(&opts)?;
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
    prepare_oci_runtime(&paths)?;

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

    let tap = match setup_network(&opts.socket, network.as_ref()).await {
        Ok(tap) => tap,
        Err(err) => {
            let _ = stop_child(&mut virtiofsd).await;
            let _ = cleanup_runtime_dir(&paths.runtime_dir);
            return Err(err);
        }
    };
    let chv_status = run_cloud_hypervisor(&opts, &paths, &kernel, network.as_ref()).await;
    let guest_status = read_guest_exit_status(&paths.rootfs);
    let virtiofsd_status = stop_child(&mut virtiofsd).await;
    let tap_status = teardown_network(&opts.socket, tap.as_ref()).await;
    let cleanup_status = cleanup_runtime_dir(&paths.runtime_dir);
    virtiofsd_status?;
    tap_status?;
    cleanup_status?;
    chv_status?;
    if let Some(status) = guest_status? {
        if status != 0 {
            bail!("guest process exited with status {status}");
        }
    }
    Ok(())
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

fn validate_ifname(label: &str, name: &str) -> Result<()> {
    if name.is_empty() || name.len() > 15 {
        bail!("{label} must be 1-15 bytes");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        bail!("{label} may only contain ASCII letters, digits, '.', '_', and '-'");
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

fn validate_mac(mac: &str) -> Result<()> {
    let parts: Vec<_> = mac.split(':').collect();
    if parts.len() != 6 {
        bail!("--mac must be six hex octets, for example 52:54:00:12:34:56");
    }
    for part in parts {
        if part.len() != 2 || !part.chars().all(|c| c.is_ascii_hexdigit()) {
            bail!("--mac must be six hex octets, for example 52:54:00:12:34:56");
        }
    }
    Ok(())
}

fn resolve_network(opts: &RunOptions) -> Result<Option<NetworkConfig>> {
    match opts.network {
        NetworkMode::None => {
            if opts.tap.is_some() || opts.mac.is_some() {
                bail!("--tap and --mac require --network bridge");
            }
            Ok(None)
        }
        NetworkMode::Bridge => {
            validate_ifname("--bridge", &opts.bridge)?;
            let tap = opts
                .tap
                .clone()
                .unwrap_or_else(|| default_tap_name(&opts.name));
            validate_ifname("--tap", &tap)?;
            let mac = opts.mac.clone().unwrap_or_else(|| default_mac(&opts.name));
            validate_mac(&mac)?;
            Ok(Some(NetworkConfig {
                bridge: opts.bridge.clone(),
                tap,
                mac: mac.to_ascii_lowercase(),
                setup: opts.tap_setup,
            }))
        }
    }
}

fn default_tap_name(name: &str) -> String {
    let simple = format!("hrt-{name}");
    if simple.len() <= 15 {
        return simple;
    }
    let prefix: String = name.chars().take(4).collect();
    format!(
        "hrt-{prefix}-{:06x}",
        fnv1a32(name.as_bytes()) & 0x00ff_ffff
    )
}

fn default_mac(name: &str) -> String {
    let hash = fnv1a32(name.as_bytes());
    format!(
        "02:00:00:{:02x}:{:02x}:{:02x}",
        (hash >> 16) & 0xff,
        (hash >> 8) & 0xff,
        hash & 0xff
    )
}

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c9dc5u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

fn cloud_hypervisor_args(
    opts: &RunOptions,
    paths: &RunPaths,
    kernel: &Utf8Path,
    network: Option<&NetworkConfig>,
) -> Vec<String> {
    let mut args = vec![
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
    ];
    if let Some(network) = network {
        args.push("--net".to_string());
        args.push(format!("tap={},mac={}", network.tap, network.mac));
    }
    args
}

fn virtiofsd_args(paths: &RunPaths) -> Vec<String> {
    vec![
        format!("--socket-path={}", paths.virtiofs_socket),
        format!("--shared-dir={}", paths.rootfs),
    ]
}

fn prepare_oci_runtime(paths: &RunPaths) -> Result<()> {
    let process = read_oci_process(&paths.bundle)?;
    let meta_dir = paths.rootfs.join(".hearth");
    fs::create_dir_all(&meta_dir)?;
    let manifest = RunManifest {
        args: process.args,
        env: process.env,
        cwd: process.cwd,
    };
    fs::write(
        meta_dir.join("run.json"),
        serde_json::to_string_pretty(&manifest)?,
    )?;
    match fs::remove_file(meta_dir.join("exit-status")) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::NotFound => {}
        Err(err) => return Err(err.into()),
    }
    Ok(())
}

fn read_guest_exit_status(rootfs: &Utf8Path) -> Result<Option<i32>> {
    let path = rootfs.join(".hearth").join("exit-status");
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err.into()),
    };
    let status = text
        .trim()
        .parse::<i32>()
        .with_context(|| format!("parse {path}"))?;
    Ok(Some(status))
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

async fn setup_network(
    socket: &Utf8Path,
    network: Option<&NetworkConfig>,
) -> Result<Option<TapLease>> {
    let Some(network) = network else {
        return Ok(None);
    };
    eprintln!(
        "hearthctl: network bridge={} tap={} mac={} setup={}",
        network.bridge, network.tap, network.mac, network.setup
    );
    if !network.setup {
        return Ok(Some(TapLease {
            name: network.tap.clone(),
            created: false,
        }));
    }
    let value = hearth_request(
        socket,
        Verb::NetSetup,
        Map::from_iter([
            ("bridge".to_string(), json!(network.bridge)),
            ("tap".to_string(), json!(network.tap)),
        ]),
    )
    .await
    .context("ask hearthd to set up bridge networking")?;
    let created = value
        .get("created")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    Ok(Some(TapLease {
        name: network.tap.clone(),
        created,
    }))
}

async fn teardown_network(socket: &Utf8Path, tap: Option<&TapLease>) -> Result<()> {
    let Some(tap) = tap else {
        return Ok(());
    };
    if tap.created {
        hearth_request(
            socket,
            Verb::NetTeardown,
            Map::from_iter([("tap".to_string(), json!(tap.name))]),
        )
        .await
        .context("ask hearthd to tear down bridge networking")?;
    }
    Ok(())
}

async fn run_cloud_hypervisor(
    opts: &RunOptions,
    paths: &RunPaths,
    kernel: &Utf8Path,
    network: Option<&NetworkConfig>,
) -> Result<()> {
    eprintln!("hearthctl: cloud-hypervisor");
    let mut cmd = Command::new("cloud-hypervisor");
    cmd.args(cloud_hypervisor_args(opts, paths, kernel, network))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    let mut child = cmd.spawn().context("spawn cloud-hypervisor")?;
    let api_ready =
        wait_for_chv_api(&paths.chv_api_socket, &mut child, Duration::from_secs(5)).await;
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
            eprintln!(
                "hearthctl: cloud-hypervisor did not exit after vm.power-off; sending SIGKILL"
            );
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
            network: NetworkMode::None,
            bridge: "hearth0".to_string(),
            tap: None,
            mac: None,
            tap_setup: true,
            socket: Utf8PathBuf::from("/run/hearth.sock"),
        };
        assert_eq!(
            cloud_hypervisor_args(
                &opts,
                &paths(),
                Utf8Path::new("/run/booted-system/kernel"),
                None
            ),
            vec![
                "--api-socket",
                "path=/tmp/hearth-test-run/chv.sock",
                "--kernel",
                "/run/booted-system/kernel",
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
    fn bridge_network_adds_chv_net_arg() {
        let opts = RunOptions {
            dockerfile: Utf8PathBuf::from("./Dockerfile"),
            context: Utf8PathBuf::from("."),
            name: "hearth-test".to_string(),
            memory: "512M".to_string(),
            cpus: 1,
            network: NetworkMode::Bridge,
            bridge: "hearth0".to_string(),
            tap: None,
            mac: None,
            tap_setup: true,
            socket: Utf8PathBuf::from("/run/hearth.sock"),
        };
        let network = resolve_network(&opts).unwrap().unwrap();
        let args = cloud_hypervisor_args(
            &opts,
            &paths(),
            Utf8Path::new("/run/booted-system/kernel"),
            Some(&network),
        );
        let expected = format!("tap=hrt-hearth-test,mac={}", default_mac("hearth-test"));

        assert!(args
            .windows(2)
            .any(|pair| pair[0] == "--net" && pair[1] == expected));
    }

    #[test]
    fn generated_tap_names_fit_linux_ifname_limit() {
        let tap = default_tap_name("hearth-test-with-a-long-name");
        assert!(tap.len() <= 15);
        assert!(validate_ifname("--tap", &tap).is_ok());
    }

    #[test]
    fn bridge_network_can_use_preconfigured_tap() {
        let opts = RunOptions {
            dockerfile: Utf8PathBuf::from("./Dockerfile"),
            context: Utf8PathBuf::from("."),
            name: "hearth-test".to_string(),
            memory: "512M".to_string(),
            cpus: 1,
            network: NetworkMode::Bridge,
            bridge: "hearth0".to_string(),
            tap: Some("hrt-test".to_string()),
            mac: Some("02:00:00:12:34:56".to_string()),
            tap_setup: false,
            socket: Utf8PathBuf::from("/run/hearth.sock"),
        };
        let network = resolve_network(&opts).unwrap().unwrap();

        assert_eq!(network.tap, "hrt-test");
        assert_eq!(network.mac, "02:00:00:12:34:56");
        assert!(!network.setup);
    }

    #[test]
    fn tap_and_mac_require_bridge_network() {
        let opts = RunOptions {
            dockerfile: Utf8PathBuf::from("./Dockerfile"),
            context: Utf8PathBuf::from("."),
            name: "hearth-test".to_string(),
            memory: "512M".to_string(),
            cpus: 1,
            network: NetworkMode::None,
            bridge: "hearth0".to_string(),
            tap: Some("hrt-test".to_string()),
            mac: None,
            tap_setup: true,
            socket: Utf8PathBuf::from("/run/hearth.sock"),
        };

        assert!(resolve_network(&opts)
            .unwrap_err()
            .to_string()
            .contains("--network bridge"));
    }

    #[test]
    fn rejects_names_that_escape_artifact_directories() {
        assert!(validate_name("hearth-test").is_ok());
        assert!(validate_name("../bad").is_err());
        assert!(validate_name("").is_err());
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

    #[test]
    fn prepares_runner_manifest_from_oci_config() {
        let root = Utf8PathBuf::from(format!(
            "/tmp/hearth-oci-runtime-test-{}",
            std::process::id()
        ));
        let bundle = root.join("bundle");
        let rootfs = bundle.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(
            bundle.join("config.json"),
            r#"{
              "process": {
                "args": ["python3", "-m", "http.server"],
                "env": ["PATH=/usr/local/bin:/usr/bin:/bin"],
                "cwd": "/srv/public"
              }
            }"#,
        )
        .unwrap();
        let paths = RunPaths {
            image_layout: root.join("image"),
            bundle: bundle.clone(),
            rootfs: rootfs.clone(),
            initramfs_image: root.join("initramfs.cpio.gz"),
            runtime_dir: root.join("run"),
            virtiofs_socket: root.join("run/rootfs.sock"),
            chv_api_socket: root.join("run/chv.sock"),
        };

        prepare_oci_runtime(&paths).unwrap();
        let manifest = std::fs::read_to_string(rootfs.join(".hearth/run.json")).unwrap();
        let _ = std::fs::remove_dir_all(&root);

        assert!(manifest.contains(r#""python3""#));
        assert!(manifest.contains(r#""cwd": "/srv/public""#));
    }
}
