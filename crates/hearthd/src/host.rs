use crate::{config::Config, error::coded, provision::ProvisionPlan, registry::Service};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use camino::Utf8Path;
use hearth_proto::ImageManifest;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::{
    fs,
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    process::Command,
    time::{sleep, timeout, Duration, Instant},
};

/// Output format for a standalone per-VM disk built from a qcow2 base. All boot
/// disks are qcow2 (CHV's raw write path fails on some host FSes, e.g. ZFS);
/// `Raw` is used only for the transient, loop-mountable provisioning scratch in
/// [`Host::build_vm_disk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiskFormat {
    Qcow2,
    Raw,
}

impl DiskFormat {
    /// The `-O` argument for `qemu-img convert`.
    pub fn qemu_output(&self) -> &'static str {
        match self {
            DiskFormat::Qcow2 => "qcow2",
            DiskFormat::Raw => "raw",
        }
    }

    /// The per-VM disk filename extension.
    pub fn extension(&self) -> &'static str {
        self.qemu_output()
    }
}

#[async_trait]
pub trait Host: Send + Sync {
    async fn systemd_run_vm(
        &self,
        cfg: &Config,
        service: &Service,
        image: &ImageManifest,
    ) -> Result<()>;
    async fn wait_for_vm_socket(&self, path: &Utf8Path, dur: Duration) -> Result<()>;
    async fn systemctl(&self, args: &[&str]) -> Result<String>;
    async fn qemu_img_create(
        &self,
        backing: &Utf8Path,
        disk: &Utf8Path,
        disk_gib: u64,
        format: DiskFormat,
    ) -> Result<()>;
    /// Build a per-VM boot disk: convert the qcow2 base into a raw
    /// scratch copy, apply `plan` to it via a loop mount, then convert the
    /// provisioned scratch into the final qcow2 boot disk at `disk` and remove
    /// the scratch. VMs boot from qcow2, not raw: Cloud
    /// Hypervisor's raw write path triggers guest EXT4 I/O errors on some host
    /// filesystems (notably ZFS). A bare-ext4 raw image is used only as the
    /// intermediate because qcow2 is not loop-mountable for provisioning.
    async fn build_vm_disk(
        &self,
        backing: &Utf8Path,
        disk: &Utf8Path,
        scratch: &Utf8Path,
        disk_gib: u64,
        plan: &ProvisionPlan,
    ) -> Result<()>;
    async fn chv_get(&self, socket: &Utf8Path, path: &str) -> Result<Value>;
    async fn chv_put(&self, socket: &Utf8Path, path: &str, body: Value) -> Result<Value>;
    /// PUT with no request body. CHV's bare action endpoints (`vm.pause`,
    /// `vm.resume`) return HTTP 400 when a body — even `{}` — is present.
    async fn chv_put_empty(&self, socket: &Utf8Path, path: &str) -> Result<Value>;
    /// Copy a VM disk image byte-for-byte. Prefers a filesystem reflink so the
    /// paused window stays short on cloning-capable hosts (XFS, btrfs, recent
    /// ZFS); elsewhere this is a full copy and the VM stays paused for it.
    async fn copy_disk(&self, src: &Utf8Path, dest: &Utf8Path) -> Result<()>;
    async fn setup_tap(&self, bridge: &str, tap: &str) -> Result<bool>;
    async fn delete_tap(&self, tap: &str) -> Result<()>;
    /// Apply an nftables ruleset via `nft -f -` (stdin). The daemon feeds a full
    /// `add table` + `flush table` + rules transaction, so this is an idempotent
    /// rewrite of the `hearth_nat` table.
    async fn nft_apply(&self, ruleset: &str) -> Result<()>;
    /// SIGHUP dnsmasq so it re-reads its drop-in dir.
    /// Errs if the unit does not exist; callers warn-and-continue.
    async fn reload_dnsmasq(&self) -> Result<()>;
}

#[derive(Debug, Default)]
pub struct RealHost;

#[async_trait]
impl Host for RealHost {
    async fn systemd_run_vm(
        &self,
        cfg: &Config,
        service: &Service,
        image: &ImageManifest,
    ) -> Result<()> {
        fs::create_dir_all(cfg.run_dir.join("vms")).await?;
        fs::create_dir_all(cfg.run_dir.join("vsock")).await?;
        fs::create_dir_all(&cfg.log_dir).await?;
        unlink_stale(&cfg.vm_socket(&service.id)).await?;
        unlink_stale(&cfg.vm_vsock_socket(&service.id)).await?;
        let _ = ensure_tap(&cfg.bridge, &tap_name(&service.id)).await?;
        systemd_run_chv(service, cloud_hypervisor_argv(cfg, service, image)).await
    }

    async fn wait_for_vm_socket(&self, path: &Utf8Path, dur: Duration) -> Result<()> {
        wait_for_socket(path, dur).await
    }

    async fn systemctl(&self, args: &[&str]) -> Result<String> {
        let mut cmd = Command::new("systemctl");
        cmd.args(args);
        run_output(cmd, "systemctl").await
    }

    async fn qemu_img_create(
        &self,
        backing: &Utf8Path,
        disk: &Utf8Path,
        disk_gib: u64,
        format: DiskFormat,
    ) -> Result<()> {
        if let Some(parent) = disk.parent() {
            fs::create_dir_all(parent).await?;
        }
        // CHV's qcow2 reader rejects any backing chain, so copy the base image
        // into a standalone per-VM disk and grow it to the requested size. Raw
        // stays sparse and is loop-mountable for provisioning.
        let mut convert = Command::new("qemu-img");
        convert.args([
            "convert",
            "-f",
            "qcow2",
            "-O",
            format.qemu_output(),
            backing.as_str(),
            disk.as_str(),
        ]);
        run_status(convert, "qemu-img convert").await?;
        let mut resize = Command::new("qemu-img");
        resize.args(["resize", disk.as_str(), &format!("{disk_gib}G")]);
        run_status(resize, "qemu-img resize").await
    }

    async fn build_vm_disk(
        &self,
        backing: &Utf8Path,
        disk: &Utf8Path,
        scratch: &Utf8Path,
        disk_gib: u64,
        plan: &ProvisionPlan,
    ) -> Result<()> {
        // 1. base qcow2 -> sized raw scratch (loop-mountable), reusing the same
        //    standalone convert+resize the cloud path uses.
        self.qemu_img_create(backing, scratch, disk_gib, DiskFormat::Raw)
            .await?;
        // 2. provision the scratch in place; drop it if provisioning fails.
        if let Err(err) = provision_raw_disk(scratch, plan).await {
            let _ = fs::remove_file(scratch.as_str()).await;
            return Err(err);
        }
        // 3. raw scratch -> qcow2 boot disk, then discard the scratch. Boot is
        //    qcow2 because CHV's raw write path fails on some host FSes (ZFS).
        if let Some(parent) = disk.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut convert = Command::new("qemu-img");
        convert.args([
            "convert",
            "-f",
            "raw",
            "-O",
            "qcow2",
            scratch.as_str(),
            disk.as_str(),
        ]);
        let converted = run_status(convert, "qemu-img convert raw->qcow2").await;
        let _ = fs::remove_file(scratch.as_str()).await;
        converted
    }

    async fn chv_get(&self, socket: &Utf8Path, path: &str) -> Result<Value> {
        chv_request(socket, "GET", path, None).await
    }

    async fn chv_put(&self, socket: &Utf8Path, path: &str, body: Value) -> Result<Value> {
        chv_request(socket, "PUT", path, Some(body)).await
    }

    async fn chv_put_empty(&self, socket: &Utf8Path, path: &str) -> Result<Value> {
        chv_request(socket, "PUT", path, None).await
    }

    async fn copy_disk(&self, src: &Utf8Path, dest: &Utf8Path) -> Result<()> {
        let mut cmd = Command::new("cp");
        cmd.args([
            "--reflink=auto",
            "--sparse=always",
            src.as_str(),
            dest.as_str(),
        ]);
        run_status(cmd, "cp disk image").await
    }

    async fn setup_tap(&self, bridge: &str, tap: &str) -> Result<bool> {
        ensure_tap(bridge, tap).await
    }

    async fn delete_tap(&self, tap: &str) -> Result<()> {
        delete_tap_device(tap).await
    }

    async fn nft_apply(&self, ruleset: &str) -> Result<()> {
        let mut child = Command::new("nft")
            .args(["-f", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("spawn nft -f -")?;
        let mut stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("nft stdin unavailable"))?;
        stdin.write_all(ruleset.as_bytes()).await?;
        drop(stdin); // close stdin so nft sees EOF and applies the transaction
        let output = child.wait_with_output().await.context("wait nft -f -")?;
        if output.status.success() {
            Ok(())
        } else {
            Err(anyhow!(
                "nft -f - failed: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            ))
        }
    }

    async fn reload_dnsmasq(&self) -> Result<()> {
        let mut cmd = Command::new("systemctl");
        cmd.args(["kill", "-s", "HUP", "dnsmasq.service"]);
        run_status(cmd, "systemctl kill -s HUP dnsmasq.service").await
    }
}

/// Apply a provisioning plan to a bare-ext4 raw disk via a loop mount. The mount
/// is always torn down, even when application fails (finally pattern), so a
/// failed provision never leaves the loop device attached.
async fn provision_raw_disk(disk: &Utf8Path, plan: &ProvisionPlan) -> Result<()> {
    let tmp = tempfile::tempdir()
        .map_err(|e| coded("provision.mount_failed", format!("create mount dir: {e}")))?;
    let mount_point = Utf8Path::from_path(tmp.path())
        .ok_or_else(|| coded("provision.mount_failed", "non-utf8 mount path"))?;
    // The rootfs is a bare whole-device ext4 (no partition table), so a plain
    // loop mount of the raw disk works.
    let mut mount = Command::new("mount");
    mount.args(["-o", "loop", disk.as_str(), mount_point.as_str()]);
    run_status(mount, "mount -o loop")
        .await
        .map_err(|e| coded("provision.mount_failed", format!("{e:#}")))?;
    let applied = crate::provision::apply_to_root(mount_point, plan).await;
    let mut umount = Command::new("umount");
    umount.arg(mount_point.as_str());
    let unmounted = run_status(umount, "umount").await;
    applied.map_err(|e| coded("provision.apply_failed", format!("{e:#}")))?;
    unmounted.map_err(|e| coded("provision.unmount_failed", format!("{e:#}")))?;
    Ok(())
}

async fn systemd_run_chv(service: &Service, argv: Vec<String>) -> Result<()> {
    let unit = unit_name(&service.id);
    // A transient unit outlives the hearthd that created it, and one that
    // crash-looped into `failed` stays loaded. Either leaves the name claimed,
    // so a fresh `systemd-run --unit=<name>` fails with "already loaded or has a
    // fragment file". Clear any stale instance first (best-effort — the unit
    // usually does not exist, and callers only reach here when the VM is not
    // running). `stop` handles an active/looping unit; `reset-failed` unloads a
    // failed one.
    for action in ["stop", "reset-failed"] {
        let _ = Command::new("systemctl")
            .args([action, &unit])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
    }
    let mut cmd = Command::new("systemd-run");
    cmd.arg(format!("--unit={unit}"))
        .arg("--collect")
        .arg(format!("--property=Restart={}", service.restart.policy))
        .arg(format!(
            "--property=RestartSec={}s",
            service.restart.backoff_sec
        ))
        .arg(format!(
            "--property=StartLimitBurst={}",
            service.restart.max_retries
        ))
        .arg("--property=StartLimitIntervalSec=300s")
        .arg("--property=TimeoutStopSec=30s")
        .arg("--")
        .args(argv);
    run_status(cmd, "systemd-run").await
}

pub fn cloud_hypervisor_argv(
    cfg: &Config,
    service: &Service,
    manifest: &ImageManifest,
) -> Vec<String> {
    image_argv(cfg, service, manifest)
}

fn image_argv(
    cfg: &Config,
    service: &Service,
    manifest: &hearth_proto::ImageManifest,
) -> Vec<String> {
    let mut args = vec![
        "cloud-hypervisor".to_string(),
        "--api-socket".to_string(),
        cfg.vm_socket(&service.id).to_string(),
        "--kernel".to_string(),
        cfg.guest_kernel.to_string(),
    ];
    if let Some(initramfs) = &cfg.guest_initramfs {
        args.push("--initramfs".to_string());
        args.push(initramfs.to_string());
    }
    args.extend([
        "--disk".to_string(),
        // Explicit image_type: CHV v52 deprecates disk-format autodetection
        // (slated for removal). All Hearth boot disks are qcow2 (see
        // DiskFormat).
        format!("path={},image_type=qcow2", cfg.disk_path(service)),
        "--cmdline".to_string(),
        kernel_cmdline(manifest),
        "--net".to_string(),
        format!("tap={},mac={}", tap_name(&service.id), service.mac),
    ]);
    append_vsock(&mut args, cfg, service);
    args.extend([
        "--serial".to_string(),
        format!("file={}", cfg.console_path(&service.id)),
        "--console".to_string(),
        "off".to_string(),
        "--cpus".to_string(),
        format!("boot={}", service.cpu),
        "--memory".to_string(),
        format!("size={}M", service.memory_mib),
        "--balloon".to_string(),
        "size=0,free_page_reporting=on".to_string(),
    ]);
    args
}

fn append_vsock(args: &mut Vec<String>, cfg: &Config, service: &Service) {
    args.extend([
        "--vsock".to_string(),
        format!(
            "cid={},socket={}",
            service.vsock_cid,
            cfg.vm_vsock_socket(&service.id)
        ),
    ]);
}

fn kernel_cmdline(manifest: &hearth_proto::ImageManifest) -> String {
    format!(
        "console=ttyS0 root={} rootfstype={} rw init={}",
        manifest.root_device, manifest.root_fstype, manifest.init
    )
}

pub async fn wait_for_socket(path: &Utf8Path, dur: Duration) -> Result<()> {
    timeout(dur, async {
        loop {
            if UnixStream::connect(path.as_str()).await.is_ok() {
                return Ok(());
            }
            sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .map_err(|_| coded("vm.boot_timeout", format!("timed out waiting for {path}")))?
}

pub async fn wait_for_inactive(host: &dyn Host, unit: &str, dur: Duration) -> Result<bool> {
    let deadline = Instant::now() + dur;
    loop {
        let active = host
            .systemctl(&["is-active", unit])
            .await
            .unwrap_or_default();
        if active.trim() != "active" {
            return Ok(true);
        }
        if Instant::now() >= deadline {
            return Ok(false);
        }
        sleep(Duration::from_millis(500)).await;
    }
}

async fn run_status(mut cmd: Command, label: &str) -> Result<()> {
    let output = cmd
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("spawn {label}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(anyhow!(
            "{label} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

async fn run_output(mut cmd: Command, label: &str) -> Result<String> {
    let output = cmd
        .stderr(Stdio::piped())
        .stdout(Stdio::piped())
        .output()
        .await
        .with_context(|| format!("spawn {label}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    } else {
        Err(anyhow!(
            "{label} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
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
    // Keep the write half open. CHV's API server answers keep-alive regardless
    // of `Connection: close`, and a slow endpoint (vm.snapshot dumps guest
    // memory) drops the request without replying if it sees client EOF first.
    // So read exactly one response, sized by Content-Length, then hang up.
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(end) = header_end(&buf) {
            break end;
        }
        if buf.len() > 64 * 1024 {
            return Err(anyhow!("oversized HTTP response head from CHV"));
        }
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(anyhow!(
                "CHV closed the API connection before sending a response"
            ));
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let (status, content_length) = parse_http_head(&String::from_utf8_lossy(&buf[..head_end]))?;
    while buf.len() < head_end + content_length {
        let n = stream.read(&mut chunk).await?;
        if n == 0 {
            return Err(anyhow!("CHV closed the API connection mid-body"));
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    let body = String::from_utf8_lossy(&buf[head_end..head_end + content_length]);
    if !(200..300).contains(&status) {
        return Err(anyhow!("CHV API returned HTTP {status}: {body}"));
    }
    if body.trim().is_empty() {
        Ok(json!({}))
    } else {
        serde_json::from_str(&body).context("parse CHV JSON response")
    }
}

/// Byte offset just past the `\r\n\r\n` header terminator, if present.
fn header_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n").map(|p| p + 4)
}

/// Parse an HTTP response head into (status, content-length). A missing
/// Content-Length means an empty body (CHV's 204s carry no body at all).
fn parse_http_head(head: &str) -> Result<(u16, usize)> {
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .ok_or_else(|| anyhow!("malformed HTTP status from CHV"))?;
    let content_length = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            if name.trim().eq_ignore_ascii_case("content-length") {
                value.trim().parse::<usize>().ok()
            } else {
                None
            }
        })
        .unwrap_or(0);
    Ok((status, content_length))
}

pub fn unit_name(name: &str) -> String {
    format!("hearth-vm-{name}.service")
}

/// Parse the `argv[]=` segment of a systemd `ExecStart` property value (as
/// printed by `systemctl show -p ExecStart --value <unit>`) into its argument
/// vector. The property looks like:
/// `{ path=/usr/bin/cloud-hypervisor ; argv[]=cloud-hypervisor --api-socket ... ; ignore_errors=no ; ... }`
/// Returns `None` when there is no `argv[]=` segment to read.
pub fn parse_execstart_argv(execstart: &str) -> Option<Vec<String>> {
    let start = execstart.find("argv[]=")? + "argv[]=".len();
    let rest = &execstart[start..];
    // The argv segment ends at the next " ; " systemd field separator.
    let segment = rest.split(" ; ").next().unwrap_or(rest);
    let argv = split_execstart_args(segment);
    if argv.is_empty() {
        None
    } else {
        Some(argv)
    }
}

/// Split a systemd `argv[]` segment into arguments, honoring the double quotes
/// systemd wraps around any argument containing whitespace (our `--cmdline`
/// value is the one that needs it). Quotes are stripped so the result
/// space-joins back to the plain command line `cloud_hypervisor_argv` builds.
fn split_execstart_args(segment: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_quotes = false;
    let mut has_token = false;
    for ch in segment.chars() {
        match ch {
            '"' => {
                in_quotes = !in_quotes;
                has_token = true;
            }
            ws if ws.is_whitespace() && !in_quotes => {
                if has_token {
                    args.push(std::mem::take(&mut current));
                    has_token = false;
                }
            }
            other => {
                current.push(other);
                has_token = true;
            }
        }
    }
    if has_token {
        args.push(current);
    }
    args
}

/// Compare the argv systemd recorded for a running unit against the argv we
/// would launch now. `Some(true)` = current, `Some(false)` = drifted, `None` =
/// couldn't determine (no parseable argv, or a resumed snapshot whose
/// `--restore` command line never matches a fresh boot).
pub fn boot_config_status(execstart: &str, expected_argv: &[String]) -> Option<bool> {
    let running = parse_execstart_argv(execstart)?;
    if running.iter().any(|arg| arg == "--restore") {
        return None;
    }
    let (Some(running_executable), Some(expected_executable)) =
        (running.first(), expected_argv.first())
    else {
        return Some(false);
    };
    Some(
        executable_name(running_executable) == executable_name(expected_executable)
            // `systemctl show` does not consistently preserve argument
            // boundaries for values containing whitespace. Compare the
            // flattened remainder exactly, as the property itself presents it.
            && running[1..].join(" ") == expected_argv[1..].join(" "),
    )
}

/// systemd may resolve argv[0] to an absolute executable path even when the
/// transient unit was launched with a PATH-resolved command name. Only the
/// executable's basename is relevant to boot-configuration drift; all other
/// arguments still require an exact match.
fn executable_name(executable: &str) -> &str {
    executable.rsplit('/').next().unwrap_or(executable)
}

/// Tap device name for a service. Linux interface names are capped at 15 bytes.
pub fn tap_name(name: &str) -> String {
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

fn fnv1a32(bytes: &[u8]) -> u32 {
    let mut hash = 0x811c9dc5u32;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(0x01000193);
    }
    hash
}

/// CHV refuses to bind a unix socket path that already exists. Remove it if a
/// previous CHV exited without cleanup.
async fn unlink_stale(path: &Utf8Path) -> Result<()> {
    match fs::remove_file(path.as_str()).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Create the tap (idempotent), attach it to the bridge, and bring it up.
async fn ensure_tap(bridge: &str, tap: &str) -> Result<bool> {
    let mut created = false;
    if !std::path::Path::new(&format!("/sys/class/net/{tap}")).exists() {
        let mut cmd = Command::new("ip");
        cmd.args(["tuntap", "add", "dev", tap, "mode", "tap"]);
        run_status(cmd, "ip tuntap add").await?;
        created = true;
    }
    let mut cmd = Command::new("ip");
    cmd.args(["link", "set", tap, "master", bridge]);
    if let Err(err) = run_status(cmd, "ip link set master").await {
        if created {
            let _ = delete_tap_device(tap).await;
        }
        return Err(err);
    }
    let mut cmd = Command::new("ip");
    cmd.args(["link", "set", tap, "up"]);
    if let Err(err) = run_status(cmd, "ip link set up").await {
        if created {
            let _ = delete_tap_device(tap).await;
        }
        return Err(err);
    }
    Ok(created)
}

async fn delete_tap_device(tap: &str) -> Result<()> {
    if !std::path::Path::new(&format!("/sys/class/net/{tap}")).exists() {
        return Ok(());
    }
    let mut cmd = Command::new("ip");
    cmd.args(["link", "del", tap]);
    run_status(cmd, "ip link del").await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_chv_head_with_keep_alive_and_content_length() {
        // Verbatim shape of CHV 52's error response: keep-alive despite the
        // client's `Connection: close`, body sized by Content-Length.
        let head = "HTTP/1.1 500\r\nServer: Cloud Hypervisor API\r\nConnection: keep-alive\r\nContent-Type: application/json\r\nContent-Length: 156\r\n\r\n";
        assert_eq!(parse_http_head(head).unwrap(), (500, 156));
    }

    #[test]
    fn missing_content_length_means_empty_body() {
        let head = "HTTP/1.1 204\r\nServer: Cloud Hypervisor API\r\nConnection: keep-alive\r\n\r\n";
        assert_eq!(parse_http_head(head).unwrap(), (204, 0));
    }

    #[test]
    fn header_end_requires_the_full_terminator() {
        assert_eq!(header_end(b"HTTP/1.1 204\r\n\r\nrest"), Some(16));
        assert!(header_end(b"HTTP/1.1 204\r\n").is_none());
    }

    #[test]
    fn short_tap_names_keep_service_name() {
        assert_eq!(tap_name("web"), "hrt-web");
    }

    #[test]
    fn long_tap_names_fit_linux_ifname_limit() {
        let tap = tap_name("http-server-with-long-name");

        assert!(tap.len() <= 15);
        assert!(tap.starts_with("hrt-http-"));
    }

    #[test]
    fn parses_argv_from_real_execstart_including_quoted_cmdline() {
        // Shape and quoting as produced by `systemctl show -p ExecStart --value`:
        // the space-containing `--cmdline` value is double-quoted by systemd.
        let execstart = "{ path=/usr/bin/cloud-hypervisor ; argv[]=cloud-hypervisor --api-socket /run/hearth/vms/mail.sock --kernel /var/lib/hearth/kernels/current/vmlinux --disk path=/var/lib/hearth/disks/mail.qcow2 --cmdline \"console=ttyS0 root=/dev/vda rootfstype=ext4 rw init=/usr/local/bin/init\" --net tap=hrt-mail,mac=52:54:00:12:34:56 ; ignore_errors=no ; start_time=[n/a] ; stop_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }";
        let argv = parse_execstart_argv(execstart).unwrap();
        assert_eq!(argv[0], "cloud-hypervisor");
        assert_eq!(argv[1], "--api-socket");
        // The quoted, space-containing cmdline resolves to a single argument.
        assert!(argv.contains(
            &"console=ttyS0 root=/dev/vda rootfstype=ext4 rw init=/usr/local/bin/init".to_string()
        ));
        assert_eq!(argv.last().unwrap(), "tap=hrt-mail,mac=52:54:00:12:34:56");
    }

    #[test]
    fn parse_returns_none_without_an_argv_segment() {
        assert!(parse_execstart_argv("{ path=/usr/bin/cloud-hypervisor }").is_none());
        assert!(parse_execstart_argv("").is_none());
    }

    #[test]
    fn boot_config_status_matches_and_detects_drift() {
        let expected: Vec<String> = [
            "cloud-hypervisor",
            "--api-socket",
            "/run/hearth/vms/mail.sock",
            "--cmdline",
            "console=ttyS0 root=/dev/vda rootfstype=ext4 rw init=/usr/local/bin/init",
        ]
        .iter()
        .map(|arg| arg.to_string())
        .collect();
        let current = "{ path=/x ; argv[]=cloud-hypervisor --api-socket /run/hearth/vms/mail.sock --cmdline \"console=ttyS0 root=/dev/vda rootfstype=ext4 rw init=/usr/local/bin/init\" ; ignore_errors=no }";
        assert_eq!(boot_config_status(current, &expected), Some(true));
        let nix_resolved = "{ path=/nix/store/abc-cloud-hypervisor/bin/cloud-hypervisor ; argv[]=/run/current-system/sw/bin/cloud-hypervisor --api-socket /run/hearth/vms/mail.sock --cmdline \"console=ttyS0 root=/dev/vda rootfstype=ext4 rw init=/usr/local/bin/init\" ; ignore_errors=no }";
        assert_eq!(boot_config_status(nix_resolved, &expected), Some(true));
        let nix_resolved_unquoted = "{ path=/run/current-system/sw/bin/cloud-hypervisor ; argv[]=/run/current-system/sw/bin/cloud-hypervisor --api-socket /run/hearth/vms/mail.sock --cmdline console=ttyS0 root=/dev/vda rootfstype=ext4 rw init=/usr/local/bin/init ; ignore_errors=no }";
        assert_eq!(
            boot_config_status(nix_resolved_unquoted, &expected),
            Some(true)
        );
        let drifted = "{ path=/x ; argv[]=cloud-hypervisor --api-socket /run/hearth/vms/mail.sock --kernel /old/vmlinux ; ignore_errors=no }";
        assert_eq!(boot_config_status(drifted, &expected), Some(false));
    }

    #[test]
    fn boot_config_status_ignores_resumed_snapshots() {
        let expected = vec!["cloud-hypervisor".to_string()];
        let restored = "{ path=/x ; argv[]=cloud-hypervisor --api-socket /run/hearth/vms/mail.sock --restore source_url=file:///snap/mail/before,resume=true ; ignore_errors=no }";
        assert_eq!(boot_config_status(restored, &expected), None);
    }
}
