//! A scripted [`Host`] for tests that need a real `Daemon` without KVM,
//! systemd, or root. Lives in the library (not `#[cfg(test)]`) so the
//! `hearth-e2e` acceptance crate can run a full in-process hearthd against
//! real unix sockets — under CHV's hybrid vsock model every host-side channel
//! is a plain unix socket, so everything except the VM boot itself exercises
//! production code paths.

use crate::{
    config::Config, host::DiskFormat, host::Host, provision::ProvisionPlan, registry::Service,
};
use anyhow::Result;
use async_trait::async_trait;
use camino::Utf8Path;
use hearth_proto::ImageManifest;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use tokio::time::Duration;

#[derive(Clone, Default)]
pub struct FakeHost {
    pub state: Arc<Mutex<FakeState>>,
}

#[derive(Default)]
pub struct FakeState {
    pub calls: Vec<String>,
    pub running: bool,
    pub exec_start: Option<String>,
    pub last_nft: Option<String>,
}

impl FakeHost {
    pub fn running() -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                running: true,
                ..FakeState::default()
            })),
        }
    }

    /// A running host whose transient unit reports `exec_start` from
    /// `systemctl show -p ExecStart --value`, so boot-config drift can be
    /// exercised without systemd.
    pub fn with_exec_start(exec_start: String) -> Self {
        Self {
            state: Arc::new(Mutex::new(FakeState {
                running: true,
                exec_start: Some(exec_start),
                ..FakeState::default()
            })),
        }
    }
}

#[async_trait]
impl Host for FakeHost {
    async fn systemd_run_vm(
        &self,
        _cfg: &Config,
        service: &Service,
        _image: &ImageManifest,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(format!("systemd-run {}", service.name));
        state.running = true;
        Ok(())
    }

    async fn systemd_restore_vm(
        &self,
        _cfg: &Config,
        service: &Service,
        _snapshot_dir: &Utf8Path,
    ) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state
            .calls
            .push(format!("systemd-restore {}", service.name));
        state.running = true;
        Ok(())
    }

    async fn wait_for_vm_socket(&self, path: &Utf8Path, _dur: Duration) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("wait-socket {path}"));
        Ok(())
    }

    async fn systemctl(&self, args: &[&str]) -> Result<String> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(format!("systemctl {}", args.join(" ")));
        if args.first() == Some(&"is-active") {
            Ok(if state.running {
                "active\n".to_string()
            } else {
                "inactive\n".to_string()
            })
        } else if args.first() == Some(&"show") {
            Ok(state.exec_start.clone().unwrap_or_default())
        } else {
            Ok(String::new())
        }
    }

    async fn qemu_img_create(
        &self,
        backing: &Utf8Path,
        disk: &Utf8Path,
        disk_gib: u64,
        format: DiskFormat,
    ) -> Result<()> {
        self.state.lock().unwrap().calls.push(format!(
            "qemu-img create {backing} {disk} {disk_gib} {}",
            format.extension()
        ));
        Ok(())
    }

    async fn build_vm_disk(
        &self,
        _backing: &Utf8Path,
        disk: &Utf8Path,
        scratch: &Utf8Path,
        _disk_gib: u64,
        plan: &ProvisionPlan,
    ) -> Result<()> {
        self.state.lock().unwrap().calls.push(format!(
            "build-vm-disk {disk} scratch={scratch} {}",
            plan.describe()
        ));
        Ok(())
    }

    async fn chv_get(&self, _socket: &Utf8Path, path: &str) -> Result<Value> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("chv-get {path}"));
        Ok(json!({}))
    }

    async fn chv_put(&self, _socket: &Utf8Path, path: &str, body: Value) -> Result<Value> {
        let mut state = self.state.lock().unwrap();
        state.calls.push(format!("chv-put {path} {body}"));
        if path == "/api/v1/vm.shutdown" || path == "/api/v1/vm.power-off" {
            state.running = false;
        }
        Ok(json!({}))
    }

    async fn setup_tap(&self, bridge: &str, tap: &str) -> Result<bool> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("setup-tap {bridge} {tap}"));
        Ok(true)
    }

    async fn delete_tap(&self, tap: &str) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push(format!("delete-tap {tap}"));
        Ok(())
    }

    async fn nft_apply(&self, ruleset: &str) -> Result<()> {
        let mut state = self.state.lock().unwrap();
        state.calls.push("nft-apply".to_string());
        state.last_nft = Some(ruleset.to_string());
        Ok(())
    }

    async fn reload_dnsmasq(&self) -> Result<()> {
        self.state
            .lock()
            .unwrap()
            .calls
            .push("reload-dnsmasq".to_string());
        Ok(())
    }
}
