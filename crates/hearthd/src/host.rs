use crate::{config::Config, registry::Service};
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

#[async_trait]
pub trait Host: Send + Sync {
    async fn systemd_run_vm(&self, cfg: &Config, service: &Service) -> Result<()>;
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
    ) -> Result<()>;
    async fn cloud_localds(
        &self,
        seed: &Utf8Path,
        user_data: &Utf8Path,
        meta_data: &Utf8Path,
    ) -> Result<()>;
    async fn chv_get(&self, socket: &Utf8Path, path: &str) -> Result<Value>;
    async fn chv_put(&self, socket: &Utf8Path, path: &str, body: Value) -> Result<Value>;
}

#[derive(Debug, Default)]
pub struct RealHost;

#[async_trait]
impl Host for RealHost {
    async fn systemd_run_vm(&self, cfg: &Config, service: &Service) -> Result<()> {
        fs::create_dir_all(cfg.run_dir.join("vms")).await?;
        fs::create_dir_all(cfg.run_dir.join("vsock")).await?;
        fs::create_dir_all(&cfg.log_dir).await?;
        systemd_run_chv(service, cloud_hypervisor_argv(cfg, service)).await
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
    ) -> Result<()> {
        if let Some(parent) = disk.parent() {
            fs::create_dir_all(parent).await?;
        }
        let mut cmd = Command::new("qemu-img");
        cmd.args([
            "create",
            "-f",
            "qcow2",
            "-F",
            "qcow2",
            "-b",
            backing.as_str(),
            disk.as_str(),
            &format!("{disk_gib}G"),
        ]);
        run_status(cmd, "qemu-img").await
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
            service.memory_mib.saturating_add(512)
        ))
        .arg("--property=TimeoutStopSec=30s")
        .arg("--")
        .args(argv);
    run_status(cmd, "systemd-run").await
}

pub fn cloud_hypervisor_argv(cfg: &Config, service: &Service) -> Vec<String> {
    vec![
        "cloud-hypervisor".to_string(),
        "--api-socket".to_string(),
        cfg.vm_socket(&service.name).to_string(),
        "--kernel".to_string(),
        cfg.firmware.to_string(),
        "--disk".to_string(),
        format!("path={}", cfg.disk_path(&service.name)),
        "--disk".to_string(),
        format!("path={},readonly=on", cfg.seed_path(&service.name)),
        "--net".to_string(),
        format!("tap=,bridge={},mac={}", cfg.bridge, service.mac),
        "--vsock".to_string(),
        format!(
            "cid={},socket={}",
            service.vsock_cid,
            cfg.vm_vsock_socket(&service.name)
        ),
        "--serial".to_string(),
        format!("file={}", cfg.console_path(&service.name)),
        "--console".to_string(),
        "off".to_string(),
        "--cpus".to_string(),
        format!("boot={}", service.cpu),
        "--memory".to_string(),
        format!("size={}M", service.memory_mib),
    ]
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
    .map_err(|_| anyhow!("timed out waiting for {path}"))?
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

pub fn sanitize_image_name(url: &str) -> String {
    let tail = url.rsplit('/').next().unwrap_or("image.qcow2");
    tail.strip_suffix(".qcow2").unwrap_or(tail).to_string()
}
