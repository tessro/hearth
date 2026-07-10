use crate::{
    config::Config, error::coded, image::ImageMetadata, provision::ProvisionPlan, registry::Service,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use camino::Utf8Path;
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
/// [`Host::build_docker_disk`].
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
        image: &ImageMetadata,
    ) -> Result<()>;
    async fn systemd_restore_vm(
        &self,
        cfg: &Config,
        service: &Service,
        snapshot_dir: &Utf8Path,
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
    /// Loop-mount a raw per-VM disk and apply `plan`, then unmount. Docker-rootfs
    /// only; called once at create time.
    /// Build a docker-rootfs per-VM boot disk: convert the qcow2 base into a raw
    /// scratch copy, apply `plan` to it via a loop mount, then convert the
    /// provisioned scratch into the final qcow2 boot disk at `disk` and remove
    /// the scratch. docker-rootfs VMs boot from qcow2, not raw: Cloud
    /// Hypervisor's raw write path triggers guest EXT4 I/O errors on some host
    /// filesystems (notably ZFS). A bare-ext4 raw image is used only as the
    /// intermediate because qcow2 is not loop-mountable for provisioning.
    async fn build_docker_disk(
        &self,
        backing: &Utf8Path,
        disk: &Utf8Path,
        scratch: &Utf8Path,
        disk_gib: u64,
        plan: &ProvisionPlan,
    ) -> Result<()>;
    async fn cloud_localds(
        &self,
        seed: &Utf8Path,
        user_data: &Utf8Path,
        meta_data: &Utf8Path,
    ) -> Result<()>;
    async fn chv_get(&self, socket: &Utf8Path, path: &str) -> Result<Value>;
    async fn chv_put(&self, socket: &Utf8Path, path: &str, body: Value) -> Result<Value>;
    async fn setup_tap(&self, bridge: &str, tap: &str) -> Result<bool>;
    async fn delete_tap(&self, tap: &str) -> Result<()>;
    /// Apply an nftables ruleset via `nft -f -` (stdin). The daemon feeds a full
    /// `add table` + `flush table` + rules transaction, so this is an idempotent
    /// rewrite of the `hearth_nat` table (REFACTOR_PROPOSAL.md §4.3).
    async fn nft_apply(&self, ruleset: &str) -> Result<()>;
    /// SIGHUP dnsmasq so it re-reads its drop-in dir (REFACTOR_PROPOSAL.md §4.2).
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
        image: &ImageMetadata,
    ) -> Result<()> {
        fs::create_dir_all(cfg.run_dir.join("vms")).await?;
        fs::create_dir_all(cfg.run_dir.join("vsock")).await?;
        fs::create_dir_all(&cfg.log_dir).await?;
        unlink_stale(&cfg.vm_socket(&service.name)).await?;
        unlink_stale(&cfg.vm_vsock_socket(&service.name)).await?;
        let _ = ensure_tap(&cfg.bridge, &tap_name(&service.name)).await?;
        systemd_run_chv(service, cloud_hypervisor_argv(cfg, service, image)).await
    }

    async fn systemd_restore_vm(
        &self,
        cfg: &Config,
        service: &Service,
        snapshot_dir: &Utf8Path,
    ) -> Result<()> {
        fs::create_dir_all(cfg.run_dir.join("vms")).await?;
        fs::create_dir_all(cfg.run_dir.join("vsock")).await?;
        fs::create_dir_all(&cfg.log_dir).await?;
        unlink_stale(&cfg.vm_socket(&service.name)).await?;
        unlink_stale(&cfg.vm_vsock_socket(&service.name)).await?;
        let _ = ensure_tap(&cfg.bridge, &tap_name(&service.name)).await?;
        systemd_run_chv(
            service,
            cloud_hypervisor_restore_argv(cfg, service, snapshot_dir),
        )
        .await
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

    async fn build_docker_disk(
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

    async fn cloud_localds(
        &self,
        seed: &Utf8Path,
        user_data: &Utf8Path,
        meta_data: &Utf8Path,
    ) -> Result<()> {
        if let Some(parent) = seed.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut cmd = Command::new("cloud-localds");
        cmd.args([seed.as_str(), user_data.as_str(), meta_data.as_str()]);
        run_status(cmd, "cloud-localds").await
    }

    async fn chv_get(&self, socket: &Utf8Path, path: &str) -> Result<Value> {
        chv_request(socket, "GET", path, None).await
    }

    async fn chv_put(&self, socket: &Utf8Path, path: &str, body: Value) -> Result<Value> {
        chv_request(socket, "PUT", path, Some(body)).await
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
    let unit = format!("hearth-vm-{}", service.name);
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
        .arg(format!(
            "--property=MemoryMax={}M",
            memory_max_mib(service.memory_mib)
        ))
        .arg("--property=TimeoutStopSec=30s")
        .arg("--")
        .args(argv);
    run_status(cmd, "systemd-run").await
}

/// The MemoryMax cgroup cap for a VM's cloud-hypervisor process, in MiB.
///
/// cloud-hypervisor's host footprint is the guest RAM (fully resident once the
/// guest touches its pages) plus VMM overhead — device emulation, worker
/// threads, and tap/vsock buffers the cgroup also charges. A flat +512 MiB of
/// headroom is too tight: a busy 2 GiB guest pushed the process past
/// `2048+512` and the kernel OOM-killed it, crash-looping the whole VM. Give
/// overhead of half the guest RAM (min 512 MiB) so the cap only ever catches a
/// genuine runaway, not normal operation.
fn memory_max_mib(memory_mib: u64) -> u64 {
    let overhead = std::cmp::max(512, memory_mib / 2);
    memory_mib.saturating_add(overhead)
}

pub fn cloud_hypervisor_argv(
    cfg: &Config,
    service: &Service,
    image: &ImageMetadata,
) -> Vec<String> {
    match image {
        ImageMetadata::CloudImage => cloud_image_argv(cfg, service),
        ImageMetadata::DockerRootfs(manifest) => docker_rootfs_argv(cfg, service, manifest),
    }
}

fn cloud_image_argv(cfg: &Config, service: &Service) -> Vec<String> {
    let mut args = vec![
        "cloud-hypervisor".to_string(),
        "--api-socket".to_string(),
        cfg.vm_socket(&service.name).to_string(),
        "--kernel".to_string(),
        cfg.firmware.to_string(),
        "--disk".to_string(),
        format!("path={}", cfg.disk_path(service)),
        "--disk".to_string(),
        format!("path={},readonly=on", cfg.seed_path(&service.name)),
        "--net".to_string(),
        format!("tap={},mac={}", tap_name(&service.name), service.mac),
        "--serial".to_string(),
        format!("file={}", cfg.console_path(&service.name)),
        "--console".to_string(),
        "off".to_string(),
        "--cpus".to_string(),
        format!("boot={}", service.cpu),
        "--memory".to_string(),
        format!("size={}M", service.memory_mib),
    ];
    append_vsock(&mut args, cfg, service);
    args
}

fn docker_rootfs_argv(
    cfg: &Config,
    service: &Service,
    manifest: &hearth_proto::ImageManifest,
) -> Vec<String> {
    let mut args = vec![
        "cloud-hypervisor".to_string(),
        "--api-socket".to_string(),
        cfg.vm_socket(&service.name).to_string(),
        "--kernel".to_string(),
        cfg.guest_kernel.to_string(),
    ];
    if let Some(initramfs) = &cfg.guest_initramfs {
        args.push("--initramfs".to_string());
        args.push(initramfs.to_string());
    }
    args.extend([
        "--disk".to_string(),
        format!("path={}", cfg.disk_path(service)),
        "--cmdline".to_string(),
        docker_rootfs_cmdline(manifest),
        "--net".to_string(),
        format!("tap={},mac={}", tap_name(&service.name), service.mac),
    ]);
    append_vsock(&mut args, cfg, service);
    args.extend([
        "--serial".to_string(),
        format!("file={}", cfg.console_path(&service.name)),
        "--console".to_string(),
        "off".to_string(),
        "--cpus".to_string(),
        format!("boot={}", service.cpu),
        "--memory".to_string(),
        format!("size={}M", service.memory_mib),
    ]);
    args
}

fn append_vsock(args: &mut Vec<String>, cfg: &Config, service: &Service) {
    args.extend([
        "--vsock".to_string(),
        format!(
            "cid={},socket={}",
            service.vsock_cid,
            cfg.vm_vsock_socket(&service.name)
        ),
    ]);
}

fn docker_rootfs_cmdline(manifest: &hearth_proto::ImageManifest) -> String {
    format!(
        "console=ttyS0 root={} rootfstype={} rw init={}",
        manifest.root_device, manifest.root_fstype, manifest.init
    )
}

pub fn cloud_hypervisor_restore_argv(
    cfg: &Config,
    service: &Service,
    snapshot_dir: &Utf8Path,
) -> Vec<String> {
    vec![
        "cloud-hypervisor".to_string(),
        "--api-socket".to_string(),
        cfg.vm_socket(&service.name).to_string(),
        "--restore".to_string(),
        format!("source_url=file://{snapshot_dir},resume=true"),
        "--serial".to_string(),
        format!("file={}", cfg.console_path(&service.name)),
        "--console".to_string(),
        "off".to_string(),
    ]
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
    Some(running.join(" ") == expected_argv.join(" "))
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

pub fn sanitize_image_name(url: &str) -> String {
    let tail = url.rsplit('/').next().unwrap_or("image.qcow2");
    tail.strip_suffix(".qcow2").unwrap_or(tail).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn short_tap_names_keep_service_name() {
        assert_eq!(tap_name("web"), "hrt-web");
    }

    #[test]
    fn memory_max_leaves_headroom_for_vmm_overhead() {
        // Small guests get a 512 MiB floor; larger guests get 50% headroom, so
        // cloud-hypervisor's overhead never pushes the process past the cgroup
        // cap (which would OOM-kill and crash-loop the VM).
        assert_eq!(memory_max_mib(512), 1024);
        assert_eq!(memory_max_mib(2048), 3072);
        assert_eq!(memory_max_mib(4096), 6144);
        // Always strictly above the guest RAM.
        assert!(memory_max_mib(8192) > 8192);
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
