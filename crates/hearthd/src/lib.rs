pub mod cloud_init;
pub mod config;
pub mod error;
pub mod host;
pub mod image;
pub mod notify;
pub mod registry;
pub mod vsock;

use crate::{
    config::Config,
    error::{code_of, coded},
    host::{sanitize_image_name, unit_name, wait_for_inactive, Host},
    image::ImageMetadata,
    registry::{validate_name, Allocations, CloudInit, Registry, RestartPolicy, Service},
};
use anyhow::{anyhow, bail, Context, Result};
use camino::Utf8PathBuf;
use chrono::Utc;
use hearth_proto::{version_result, Request, Response, Verb};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::{collections::HashMap, sync::Arc, time::Instant};
#[cfg(target_os = "linux")]
use std::{mem, os::fd::AsRawFd};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{unix::OwnedWriteHalf, UnixListener, UnixStream},
    sync::{Mutex, OwnedMutexGuard},
    time::Duration,
};
use tracing::{error, info, warn};
use walkdir::WalkDir;

pub struct Daemon<H> {
    cfg: Config,
    host: Arc<H>,
    registry_lock: Arc<Mutex<()>>,
    service_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
}

impl<H> Clone for Daemon<H> {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            host: Arc::clone(&self.host),
            registry_lock: Arc::clone(&self.registry_lock),
            service_locks: Arc::clone(&self.service_locks),
        }
    }
}

impl<H: Host + 'static> Daemon<H> {
    pub fn new(cfg: Config, host: H) -> Self {
        Self {
            cfg,
            host: Arc::new(host),
            registry_lock: Arc::new(Mutex::new(())),
            service_locks: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub async fn serve(self) -> Result<()> {
        if let Some(parent) = self.cfg.socket.parent() {
            fs::create_dir_all(parent).await?;
        }
        match fs::remove_file(&self.cfg.socket).await {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("remove stale socket"),
        }
        let listener =
            UnixListener::bind(self.cfg.socket.as_str()).context("bind hearth socket")?;
        #[cfg(unix)]
        {
            set_socket_permissions(&self.cfg.socket)?;
        }
        info!(socket = %self.cfg.socket, "hearthd ready");
        let _vsock_thread = self.spawn_vsock_listener().await?;
        notify::ready()?;
        loop {
            let (stream, _) = listener.accept().await?;
            let daemon = self.clone();
            tokio::spawn(async move {
                if let Err(err) = daemon.handle_connection(stream).await {
                    error!(error = %err, "connection failed");
                }
            });
        }
    }

    async fn handle_connection(&self, stream: UnixStream) -> Result<()> {
        let caller = peer_credentials(&stream);
        let (read, mut write) = stream.into_split();
        let mut lines = BufReader::new(read).lines();
        while let Some(line) = lines.next_line().await? {
            if line.trim().is_empty() {
                continue;
            }
            let started = Instant::now();
            let req: Result<Request, _> = serde_json::from_str(&line);
            match req {
                Ok(req) => {
                    let id = req.id.clone();
                    let verb = req.verb.to_string();
                    let args = Value::Object(req.args.clone());
                    let ok = self.handle_and_write(req, &mut write).await?;
                    info!(
                        id = %id,
                        verb = %verb,
                        args = %args,
                        caller_transport = "unix",
                        caller_uid = caller.as_ref().map(|cred| cred.uid),
                        caller_gid = caller.as_ref().map(|cred| cred.gid),
                        caller_pid = caller.as_ref().and_then(|cred| cred.pid),
                        ok,
                        duration_ms = started.elapsed().as_millis() as u64,
                        "audit"
                    );
                }
                Err(err) => {
                    let resp = Response::failure("", "protocol.invalid_json", err.to_string());
                    write
                        .write_all(serde_json::to_string(&resp)?.as_bytes())
                        .await?;
                    write.write_all(b"\n").await?;
                    warn!(
                        error = %err,
                        caller_transport = "unix",
                        caller_uid = caller.as_ref().map(|cred| cred.uid),
                        caller_gid = caller.as_ref().map(|cred| cred.gid),
                        caller_pid = caller.as_ref().and_then(|cred| cred.pid),
                        "invalid request JSON"
                    );
                }
            }
        }
        Ok(())
    }

    pub async fn handle(&self, req: Request) -> Vec<Response> {
        let id = req.id.clone();
        match self.dispatch(req).await {
            Ok(Dispatch::One(value)) => vec![Response::success(id, value)],
            Ok(Dispatch::BufferedStream(values)) => {
                let mut out: Vec<Response> = values
                    .into_iter()
                    .map(|value| Response::stream_data(id.clone(), value))
                    .collect();
                out.push(Response::stream_end(id));
                out
            }
            Ok(Dispatch::FollowLog { .. }) => vec![Response::failure(
                id,
                "stream.requires_socket",
                "follow streams must be served over a live socket",
            )],
            Err(err) => vec![Response::failure(id, error_code(&err), err.to_string())],
        }
    }

    async fn handle_and_write(&self, req: Request, write: &mut OwnedWriteHalf) -> Result<bool> {
        let id = req.id.clone();
        match self.dispatch(req).await {
            Ok(Dispatch::One(value)) => {
                write_response(write, &Response::success(id, value)).await?;
                Ok(true)
            }
            Ok(Dispatch::BufferedStream(values)) => {
                for value in values {
                    write_response(write, &Response::stream_data(id.clone(), value)).await?;
                }
                write_response(write, &Response::stream_end(id)).await?;
                Ok(true)
            }
            Ok(Dispatch::FollowLog { path }) => {
                self.stream_log(write, id, path, true).await?;
                Ok(true)
            }
            Err(err) => {
                write_response(
                    write,
                    &Response::failure(id, error_code(&err), err.to_string()),
                )
                .await?;
                Ok(false)
            }
        }
    }

    async fn dispatch(&self, req: Request) -> Result<Dispatch> {
        match req.verb {
            Verb::Ping => Ok(Dispatch::One(json!({"pong": true}))),
            Verb::Version => Ok(Dispatch::One(version_result(env!("CARGO_PKG_VERSION")))),
            Verb::Ls => self.ls().await.map(Dispatch::One),
            Verb::Status => self
                .status(required_str(&req.args, "name")?)
                .await
                .map(Dispatch::One),
            Verb::Create => self.create(req.args).await.map(Dispatch::One),
            Verb::Destroy => self
                .destroy(required_str(&req.args, "name")?)
                .await
                .map(Dispatch::One),
            Verb::Start => self
                .start(required_str(&req.args, "name")?)
                .await
                .map(Dispatch::One),
            Verb::Stop => self
                .stop(required_str(&req.args, "name")?)
                .await
                .map(Dispatch::One),
            Verb::Restart => {
                let name = required_str(&req.args, "name")?;
                let _guard = self.service_guard(name).await;
                self.stop_unlocked(name).await?;
                self.start_unlocked(name).await.map(Dispatch::One)
            }
            Verb::Reboot => self
                .reboot(required_str(&req.args, "name")?)
                .await
                .map(Dispatch::One),
            Verb::Snapshot => self.snapshot(req.args).await.map(Dispatch::One),
            Verb::Restore => self.restore(req.args).await.map(Dispatch::One),
            Verb::Resize => self.resize(req.args).await.map(Dispatch::One),
            Verb::Logs => self.logs(req.args).await,
            Verb::ImageLs => self.image_ls().await.map(Dispatch::One),
            Verb::ImagePull => self.image_pull(req.args).await.map(Dispatch::One),
            Verb::ImageImport => self.image_import(req.args).await.map(Dispatch::One),
            Verb::ImageRm => self
                .image_rm(required_str(&req.args, "name")?)
                .await
                .map(Dispatch::One),
            Verb::NetSetup => self.net_setup(req.args).await.map(Dispatch::One),
            Verb::NetTeardown => self.net_teardown(req.args).await.map(Dispatch::One),
            Verb::HostCheck => self.host_check().await.map(Dispatch::One),
        }
    }

    async fn registry(&self) -> Result<Registry> {
        Registry::load(&self.cfg).await
    }

    async fn service_guard(&self, name: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.service_locks.lock().await;
            Arc::clone(
                locks
                    .entry(name.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        lock.lock_owned().await
    }

    async fn ls(&self) -> Result<Value> {
        let reg = self.registry().await?;
        let mut services = Vec::new();
        for svc in reg.services.values() {
            let running = self.is_running(&svc.name).await;
            services.push(service_summary(svc, running));
        }
        Ok(json!({ "services": services }))
    }

    async fn status(&self, name: &str) -> Result<Value> {
        let reg = self.registry().await?;
        let svc = reg.get(name)?;
        let running = self.is_running(name).await;
        let mut value = serde_json::to_value(svc)?;
        value["running"] = json!(running);
        if running {
            if let Ok(info) = self
                .host
                .chv_get(&self.cfg.vm_socket(name), "/api/v1/vm.info")
                .await
            {
                value["runtime"] = info;
            }
        }
        Ok(value)
    }

    async fn create(&self, args: Map<String, Value>) -> Result<Value> {
        let name = required_str(&args, "name")?;
        validate_name(name)?;
        let _service_guard = self.service_guard(name).await;
        let _registry_guard = self.registry_lock.lock().await;
        let mut reg = self.registry().await?;
        if reg.services.contains_key(name) {
            return Err(coded(
                "service.exists",
                format!("service {name} already exists"),
            ));
        }
        let image = optional_str(&args, "image")
            .unwrap_or("debian-12-cloud-amd64")
            .to_string();
        let cpu = optional_u64(&args, "cpu").unwrap_or(2) as u32;
        let memory_mib = optional_u64(&args, "memory_mib")
            .or_else(|| optional_u64(&args, "mem"))
            .unwrap_or(2048);
        let disk_gib = optional_u64(&args, "disk_gib")
            .or_else(|| optional_u64(&args, "disk"))
            .unwrap_or(20);
        let image_path = self.cfg.image_path(&image);
        if !image_path.exists() {
            return Err(coded(
                "image.not_found",
                format!("base image not found: {image_path}"),
            ));
        }
        let image_metadata = image::load(&self.cfg, &image).await?;
        let is_agent_in_charge = optional_bool(&args, "is_agent_in_charge").unwrap_or(false);
        if is_agent_in_charge && reg.services.values().any(|svc| svc.is_agent_in_charge) {
            return Err(coded(
                "service.duplicate_agent_in_charge",
                "at most one service may set is_agent_in_charge = true",
            ));
        }
        let (vsock_cid, mac) = reg.allocate(name);
        let svc = Service {
            name: name.to_string(),
            enabled: false,
            image: image.clone(),
            cpu,
            memory_mib,
            disk_gib,
            vsock_cid,
            mac,
            is_agent_in_charge,
            cloud_init: CloudInit {
                hostname: name.to_string(),
                ssh_keys: optional_array_str(&args, "ssh_keys"),
                user: optional_str(&args, "user").unwrap_or("agent").to_string(),
            },
            restart: RestartPolicy::default(),
        };
        let disk_path = self.cfg.disk_path(name);
        let seed_path = self.cfg.seed_path(name);
        if let Err(err) = self
            .host
            .qemu_img_create(&image_path, &disk_path, disk_gib)
            .await
        {
            reg.free(name);
            return Err(err);
        }
        if matches!(image_metadata, ImageMetadata::CloudImage) {
            let tmp = tempfile::tempdir()?;
            let user_data_path = Utf8PathBuf::from_path_buf(tmp.path().join("user-data"))
                .map_err(|_| anyhow!("non-utf8 temp path"))?;
            let meta_data_path = Utf8PathBuf::from_path_buf(tmp.path().join("meta-data"))
                .map_err(|_| anyhow!("non-utf8 temp path"))?;
            fs::write(&user_data_path, cloud_init::user_data(&svc)).await?;
            fs::write(&meta_data_path, cloud_init::meta_data(&svc)).await?;
            if let Err(err) = self
                .host
                .cloud_localds(&seed_path, &user_data_path, &meta_data_path)
                .await
            {
                let _ = remove_path_file(disk_path).await;
                reg.free(name);
                return Err(err);
            }
        }
        if let Err(err) = Registry::write_allocations(&self.cfg, &reg.allocations).await {
            let _ = remove_path_file(disk_path).await;
            let _ = remove_path_file(seed_path).await;
            return Err(err);
        }
        if let Err(err) = Registry::write_service(&self.cfg, &svc).await {
            let _ = remove_path_file(disk_path).await;
            let _ = remove_path_file(seed_path).await;
            reg.free(name);
            let _ = Registry::write_allocations(&self.cfg, &reg.allocations).await;
            return Err(err);
        }
        Ok(json!({ "created": service_summary(&svc, false) }))
    }

    async fn start(&self, name: &str) -> Result<Value> {
        let _guard = self.service_guard(name).await;
        self.start_unlocked(name).await
    }

    async fn start_unlocked(&self, name: &str) -> Result<Value> {
        let mut reg = self.registry().await?;
        let mut svc = reg.get(name)?.clone();
        if !self.is_running(name).await {
            let image_metadata = image::load(&self.cfg, &svc.image).await?;
            self.host
                .systemd_run_vm(&self.cfg, &svc, &image_metadata)
                .await?;
            self.host
                .wait_for_vm_socket(&self.cfg.vm_socket(name), Duration::from_secs(20))
                .await?;
        }
        svc.enabled = true;
        Registry::write_service(&self.cfg, &svc).await?;
        reg.services.insert(name.to_string(), svc);
        self.status(name).await
    }

    async fn stop(&self, name: &str) -> Result<Value> {
        let _guard = self.service_guard(name).await;
        self.stop_unlocked(name).await
    }

    async fn stop_unlocked(&self, name: &str) -> Result<Value> {
        let reg = self.registry().await?;
        let mut svc = reg.get(name)?.clone();
        if self.is_running(name).await {
            let socket = self.cfg.vm_socket(name);
            let unit = unit_name(name);
            let graceful_timeout = Duration::from_secs(30);
            let started = Instant::now();
            info!(service = %name, "sending vm.shutdown (ACPI)");
            if let Err(err) = self
                .host
                .chv_put(&socket, "/api/v1/vm.shutdown", json!({}))
                .await
            {
                warn!(service = %name, error = %err, "vm.shutdown request failed; waiting for unit to exit anyway");
            }
            if wait_for_inactive(self.host.as_ref(), &unit, graceful_timeout).await? {
                info!(
                    service = %name,
                    duration_ms = started.elapsed().as_millis() as u64,
                    "vm stopped gracefully"
                );
            } else {
                warn!(
                    service = %name,
                    waited_ms = started.elapsed().as_millis() as u64,
                    timeout_s = graceful_timeout.as_secs(),
                    "graceful shutdown timed out; escalating to vm.power-off"
                );
                if let Err(err) = self
                    .host
                    .chv_put(&socket, "/api/v1/vm.power-off", json!({}))
                    .await
                {
                    warn!(service = %name, error = %err, "vm.power-off request failed");
                }
                if let Err(err) = self.host.systemctl(&["stop", &unit]).await {
                    warn!(service = %name, error = %err, "systemctl stop failed");
                }
            }
        }
        svc.enabled = false;
        Registry::write_service(&self.cfg, &svc).await?;
        Ok(json!({ "name": name, "running": false, "enabled": false }))
    }

    async fn reboot(&self, name: &str) -> Result<Value> {
        let _guard = self.service_guard(name).await;
        let reg = self.registry().await?;
        reg.get(name)?;
        self.host
            .chv_put(&self.cfg.vm_socket(name), "/api/v1/vm.reboot", json!({}))
            .await?;
        self.status(name).await
    }

    async fn destroy(&self, name: &str) -> Result<Value> {
        self.registry().await?.get(name)?;
        let _service_guard = self.service_guard(name).await;
        self.stop_unlocked(name).await?;
        let _registry_guard = self.registry_lock.lock().await;
        let mut reg = self.registry().await?;
        remove_path_file(self.cfg.disk_path(name)).await?;
        remove_path_file(self.cfg.seed_path(name)).await?;
        remove_path_file(self.cfg.console_path(name)).await?;
        remove_path_dir(self.cfg.snapshots_dir.join(name)).await?;
        self.host.delete_tap(&host::tap_name(name)).await?;
        Registry::remove_service(&self.cfg, name).await?;
        reg.free(name);
        Registry::write_allocations(&self.cfg, &reg.allocations).await?;
        Ok(json!({ "destroyed": name }))
    }

    async fn snapshot(&self, args: Map<String, Value>) -> Result<Value> {
        let name = required_str(&args, "name")?;
        let _guard = self.service_guard(name).await;
        self.registry().await?.get(name)?;
        let tag = optional_str(&args, "tag")
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| Utc::now().format("%Y%m%d%H%M%S").to_string());
        let dest = self.cfg.snapshot_dir(name, &tag);
        fs::create_dir_all(&dest).await?;
        self.host
            .chv_put(
                &self.cfg.vm_socket(name),
                "/api/v1/vm.snapshot",
                json!({ "destination_url": format!("file://{dest}") }),
            )
            .await?;
        Ok(json!({ "name": name, "tag": tag, "path": dest }))
    }

    async fn restore(&self, args: Map<String, Value>) -> Result<Value> {
        let name = required_str(&args, "name")?;
        let _guard = self.service_guard(name).await;
        let tag = required_str(&args, "tag")?;
        let src = self.cfg.snapshot_dir(name, tag);
        if !src.exists() {
            return Err(coded(
                "snapshot.not_found",
                format!("snapshot not found: {src}"),
            ));
        }
        let _ = self.stop_unlocked(name).await;
        let reg = self.registry().await?;
        let mut svc = reg.get(name)?.clone();
        self.host.systemd_restore_vm(&self.cfg, &svc, &src).await?;
        self.host
            .wait_for_vm_socket(&self.cfg.vm_socket(name), Duration::from_secs(20))
            .await?;
        svc.enabled = true;
        Registry::write_service(&self.cfg, &svc).await?;
        Ok(json!({ "name": name, "tag": tag, "restored": true }))
    }

    async fn resize(&self, args: Map<String, Value>) -> Result<Value> {
        let name = required_str(&args, "name")?;
        let _guard = self.service_guard(name).await;
        let mut reg = self.registry().await?;
        let mut svc = reg.get(name)?.clone();
        if let Some(cpu) = optional_u64(&args, "cpu") {
            svc.cpu = cpu as u32;
        }
        if let Some(mem) = optional_u64(&args, "memory_mib").or_else(|| optional_u64(&args, "mem"))
        {
            svc.memory_mib = mem;
        }
        let mut body = Map::new();
        body.insert("desired_vcpus".to_string(), json!(svc.cpu));
        body.insert(
            "desired_ram".to_string(),
            json!(svc.memory_mib * 1024 * 1024),
        );
        if self.is_running(name).await {
            self.host
                .chv_put(
                    &self.cfg.vm_socket(name),
                    "/api/v1/vm.resize",
                    Value::Object(body),
                )
                .await?;
        }
        Registry::write_service(&self.cfg, &svc).await?;
        reg.services.insert(name.to_string(), svc);
        self.status(name).await
    }

    async fn logs(&self, args: Map<String, Value>) -> Result<Dispatch> {
        let name = required_str(&args, "name")?;
        let reg = self.registry().await?;
        reg.get(name)?;
        let follow = optional_bool(&args, "follow").unwrap_or(false);
        if follow {
            return Ok(Dispatch::FollowLog {
                path: self.cfg.console_path(name),
            });
        }
        let text = read_optional_string(self.cfg.console_path(name)).await?;
        let lines: Vec<Value> = text.lines().map(|line| json!({ "line": line })).collect();
        Ok(Dispatch::BufferedStream(lines))
    }

    async fn image_ls(&self) -> Result<Value> {
        fs::create_dir_all(&self.cfg.images_dir).await?;
        let mut images = Vec::new();
        let mut entries = fs::read_dir(&self.cfg.images_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|_| anyhow!("non-utf8 image path"))?;
            if path.extension() != Some("qcow2") {
                continue;
            }
            let name = path.file_stem().unwrap_or_default();
            images.push(self.image_info(name).await?);
        }
        images.sort_by(|left, right| {
            left.get("name")
                .and_then(Value::as_str)
                .cmp(&right.get("name").and_then(Value::as_str))
        });
        Ok(json!({ "images": images }))
    }

    async fn image_pull(&self, args: Map<String, Value>) -> Result<Value> {
        let url = required_str(&args, "url")?;
        let name = optional_str(&args, "name")
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| sanitize_image_name(url));
        validate_name(&name)?;
        fs::create_dir_all(&self.cfg.images_dir).await?;
        let dest = self.cfg.image_path(&name);
        if dest.exists() || self.cfg.image_manifest_path(&name).exists() {
            return Err(coded(
                "image.exists",
                format!("image {name} already exists"),
            ));
        }
        let tmp = self
            .cfg
            .images_dir
            .join(format!(".{name}.tmp-{}", std::process::id()));
        let mut response = reqwest::get(url).await?.error_for_status()?;
        let mut file = fs::File::create(&tmp).await?;
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        while let Some(chunk) = response.chunk().await? {
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
            total += chunk.len() as u64;
        }
        file.flush().await?;
        drop(file);
        if let Err(err) = fs::rename(&tmp, &dest).await {
            let _ = fs::remove_file(&tmp).await;
            return Err(err.into());
        }
        let sha256 = hex::encode(hasher.finalize());
        Ok(json!({
            "name": name,
            "kind": image::CLOUD_IMAGE_KIND,
            "path": dest,
            "bytes": total,
            "sha256": sha256
        }))
    }

    async fn image_import(&self, args: Map<String, Value>) -> Result<Value> {
        let name = required_str(&args, "name")?;
        validate_name(name)?;
        let qcow2_path = Utf8PathBuf::from(required_str(&args, "qcow2_path")?);
        let manifest_path = Utf8PathBuf::from(required_str(&args, "manifest_path")?);
        if !qcow2_path.is_file() {
            return Err(coded(
                "image.import_not_found",
                format!("qcow2 import source not found: {qcow2_path}"),
            ));
        }
        if !manifest_path.is_file() {
            return Err(coded(
                "image.import_not_found",
                format!("manifest import source not found: {manifest_path}"),
            ));
        }
        let _manifest = image::read_manifest(&manifest_path).await?;
        fs::create_dir_all(&self.cfg.images_dir).await?;
        let dest = self.cfg.image_path(name);
        let manifest_dest = self.cfg.image_manifest_path(name);
        if dest.exists() || manifest_dest.exists() {
            return Err(coded(
                "image.exists",
                format!("image {name} already exists"),
            ));
        }
        let qcow2_tmp = self.import_tmp_path(name, "qcow2");
        let manifest_tmp = self.import_tmp_path(name, "manifest");
        let _ = remove_path_file(qcow2_tmp.clone()).await;
        let _ = remove_path_file(manifest_tmp.clone()).await;
        if let Err(err) = fs::copy(&qcow2_path, &qcow2_tmp).await {
            let _ = remove_path_file(qcow2_tmp).await;
            return Err(err)
                .with_context(|| format!("copy imported qcow2 {qcow2_path} into image store"));
        }
        if let Err(err) = fs::copy(&manifest_path, &manifest_tmp).await {
            let _ = remove_path_file(qcow2_tmp).await;
            let _ = remove_path_file(manifest_tmp).await;
            return Err(err).with_context(|| {
                format!("copy imported manifest {manifest_path} into image store")
            });
        }
        if let Err(err) = fs::rename(&qcow2_tmp, &dest).await {
            let _ = remove_path_file(qcow2_tmp).await;
            let _ = remove_path_file(manifest_tmp).await;
            return Err(err).with_context(|| format!("install imported qcow2 {dest}"));
        }
        if let Err(err) = fs::rename(&manifest_tmp, &manifest_dest).await {
            let _ = remove_path_file(dest).await;
            let _ = remove_path_file(manifest_tmp).await;
            return Err(err).with_context(|| format!("install image manifest {manifest_dest}"));
        }
        self.image_info(name).await
    }

    async fn image_rm(&self, name: &str) -> Result<Value> {
        validate_name(image_base_name(name)?)?;
        let reg = self.registry().await?;
        if let Some(svc) = reg
            .services
            .values()
            .find(|svc| images_match(&svc.image, name))
        {
            return Err(coded(
                "image.in_use",
                format!("image {name} is still used by service {}", svc.name),
            ));
        }
        let path = self.cfg.image_path(name);
        if !path.exists() {
            return Err(coded("image.not_found", format!("image not found: {path}")));
        }
        fs::remove_file(path).await?;
        remove_path_file(self.cfg.image_manifest_path(name)).await?;
        Ok(json!({ "removed": name }))
    }

    async fn image_info(&self, name: &str) -> Result<Value> {
        let path = self.cfg.image_path(name);
        let metadata = fs::metadata(&path).await?;
        let sha256 = sha256_file(&path).await?;
        let image_metadata = image::load(&self.cfg, name).await?;
        Ok(json!({
            "name": name,
            "kind": image_metadata.kind(),
            "path": path,
            "bytes": metadata.len(),
            "sha256": sha256,
        }))
    }

    fn import_tmp_path(&self, name: &str, label: &str) -> Utf8PathBuf {
        self.cfg
            .images_dir
            .join(format!(".{name}.{label}.tmp-{}", std::process::id()))
    }

    async fn net_setup(&self, args: Map<String, Value>) -> Result<Value> {
        let tap = required_str(&args, "tap")?;
        validate_ifname("--tap", tap)?;
        let bridge = optional_str(&args, "bridge").unwrap_or(&self.cfg.bridge);
        validate_ifname("--bridge", bridge)?;
        let created = self.host.setup_tap(bridge, tap).await?;
        Ok(json!({ "bridge": bridge, "tap": tap, "created": created }))
    }

    async fn net_teardown(&self, args: Map<String, Value>) -> Result<Value> {
        let tap = required_str(&args, "tap")?;
        validate_ifname("--tap", tap)?;
        self.host.delete_tap(tap).await?;
        Ok(json!({ "tap": tap, "deleted": true }))
    }

    async fn host_check(&self) -> Result<Value> {
        let checks = vec![
            check_path("services_dir", &self.cfg.services_dir, true),
            check_path("images_dir", &self.cfg.images_dir, true),
            check_path("disks_dir", &self.cfg.disks_dir, true),
            check_path("seeds_dir", &self.cfg.seeds_dir, true),
            check_path("snapshots_dir", &self.cfg.snapshots_dir, true),
            check_path("run_dir", &self.cfg.run_dir, true),
            check_path("log_dir", &self.cfg.log_dir, true),
            check_path("firmware", &self.cfg.firmware, false),
            check_path("kvm_device", &Utf8PathBuf::from("/dev/kvm"), false),
            check_path(
                "bridge",
                &Utf8PathBuf::from(format!("/sys/class/net/{}", self.cfg.bridge)),
                true,
            ),
            check_command("cloud-hypervisor"),
            check_command("qemu-img"),
            check_command("cloud-localds"),
            check_command("socat"),
            check_kernel_module("kvm").await?,
            check_kernel_module("vhost_vsock").await?,
        ];
        Ok(json!({ "checks": checks }))
    }

    async fn is_running(&self, name: &str) -> bool {
        let unit = unit_name(name);
        self.host
            .systemctl(&["is-active", &unit])
            .await
            .map(|s| s.trim() == "active")
            .unwrap_or(false)
    }

    async fn stream_log(
        &self,
        write: &mut OwnedWriteHalf,
        id: String,
        path: Utf8PathBuf,
        follow: bool,
    ) -> Result<()> {
        loop {
            match fs::File::open(&path).await {
                Ok(file) => {
                    let mut reader = BufReader::new(file);
                    let mut line = String::new();
                    loop {
                        line.clear();
                        let read = reader.read_line(&mut line).await?;
                        if read == 0 {
                            if follow {
                                tokio::time::sleep(Duration::from_millis(250)).await;
                                continue;
                            }
                            write_response(write, &Response::stream_end(id)).await?;
                            return Ok(());
                        }
                        trim_newline(&mut line);
                        write_response(
                            write,
                            &Response::stream_data(id.clone(), json!({ "line": line })),
                        )
                        .await?;
                    }
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound && follow => {
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                    write_response(write, &Response::stream_end(id)).await?;
                    return Ok(());
                }
                Err(err) => return Err(err.into()),
            }
        }
    }
}

enum Dispatch {
    One(Value),
    BufferedStream(Vec<Value>),
    FollowLog { path: Utf8PathBuf },
}

async fn write_response(write: &mut OwnedWriteHalf, response: &Response) -> Result<()> {
    write
        .write_all(serde_json::to_string(response)?.as_bytes())
        .await?;
    write.write_all(b"\n").await?;
    Ok(())
}

fn service_summary(svc: &Service, running: bool) -> Value {
    json!({
        "name": svc.name,
        "enabled": svc.enabled,
        "running": running,
        "image": svc.image,
        "cpu": svc.cpu,
        "memory_mib": svc.memory_mib,
        "disk_gib": svc.disk_gib,
        "vsock_cid": svc.vsock_cid,
        "mac": svc.mac,
        "is_agent_in_charge": svc.is_agent_in_charge,
    })
}

fn required_str<'a>(args: &'a Map<String, Value>, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| coded("request.invalid", format!("missing string argument {key}")))
}

fn optional_str<'a>(args: &'a Map<String, Value>, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
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

fn optional_u64(args: &Map<String, Value>, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn optional_bool(args: &Map<String, Value>, key: &str) -> Option<bool> {
    args.get(key).and_then(Value::as_bool)
}

fn optional_array_str(args: &Map<String, Value>, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(ToOwned::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

fn image_base_name(name: &str) -> Result<&str> {
    let base = name.strip_suffix(".qcow2").unwrap_or(name);
    if base.is_empty() || base.contains('/') {
        return Err(anyhow!("invalid image name {name}"));
    }
    Ok(base)
}

fn images_match(left: &str, right: &str) -> bool {
    image_base_name(left).ok() == image_base_name(right).ok()
}

fn error_code(err: &anyhow::Error) -> &'static str {
    code_of(err)
}

async fn remove_path_file(path: Utf8PathBuf) -> Result<()> {
    match fs::remove_file(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn remove_path_dir(path: Utf8PathBuf) -> Result<()> {
    match fs::remove_dir_all(path).await {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

async fn read_optional_string(path: Utf8PathBuf) -> Result<String> {
    match fs::read_to_string(path).await {
        Ok(text) => Ok(text),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(e.into()),
    }
}

async fn sha256_file(path: &Utf8PathBuf) -> Result<String> {
    let bytes = fs::read(path).await?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn check_path(name: &str, path: &Utf8PathBuf, should_be_dir: bool) -> Value {
    let exists = path.exists();
    let ok = exists
        && if should_be_dir {
            path.is_dir()
        } else {
            path.is_file()
        };
    json!({ "name": name, "path": path, "ok": ok })
}

fn check_command(command: &str) -> Value {
    let ok = std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|dir| {
                let candidate = dir.join(command);
                candidate.is_file()
            })
        })
        .unwrap_or(false);
    json!({ "name": format!("command:{command}"), "command": command, "ok": ok })
}

async fn check_kernel_module(module: &str) -> Result<Value> {
    let modules = read_optional_string(Utf8PathBuf::from("/proc/modules")).await?;
    let ok = modules
        .lines()
        .any(|line| line.split_whitespace().next() == Some(module));
    Ok(json!({ "name": format!("kernel_module:{module}"), "module": module, "ok": ok }))
}

fn trim_newline(line: &mut String) {
    while line.ends_with('\n') || line.ends_with('\r') {
        line.pop();
    }
}

#[derive(Debug, Clone, Copy)]
struct PeerCredentials {
    uid: u32,
    gid: u32,
    pid: Option<i32>,
}

#[cfg(target_os = "linux")]
fn peer_credentials(stream: &UnixStream) -> Option<PeerCredentials> {
    let mut cred: libc::ucred = unsafe { mem::zeroed() };
    let mut len = mem::size_of::<libc::ucred>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            stream.as_raw_fd(),
            libc::SOL_SOCKET,
            libc::SO_PEERCRED,
            &mut cred as *mut libc::ucred as *mut libc::c_void,
            &mut len,
        )
    };
    if rc == 0 {
        Some(PeerCredentials {
            uid: cred.uid,
            gid: cred.gid,
            pid: Some(cred.pid),
        })
    } else {
        None
    }
}

#[cfg(not(target_os = "linux"))]
fn peer_credentials(_stream: &UnixStream) -> Option<PeerCredentials> {
    None
}

#[cfg(unix)]
fn set_socket_permissions(path: &camino::Utf8Path) -> Result<()> {
    use std::ffi::CString;
    use std::os::unix::fs::PermissionsExt;

    let perms = std::fs::Permissions::from_mode(0o660);
    std::fs::set_permissions(path.as_str(), perms)?;

    #[cfg(target_os = "linux")]
    {
        let euid = unsafe { libc::geteuid() };
        if euid == 0 {
            let group = CString::new("hearth")?;
            let group = unsafe { libc::getgrnam(group.as_ptr()) };
            if !group.is_null() {
                let socket = CString::new(path.as_str())?;
                let gid = unsafe { (*group).gr_gid };
                let rc = unsafe { libc::chown(socket.as_ptr(), 0, gid) };
                if rc != 0 {
                    return Err(std::io::Error::last_os_error())
                        .context("set hearth socket ownership");
                }
            }
        }
    }
    Ok(())
}

pub async fn reconcile<H: Host>(cfg: &Config, host: &H) -> Result<()> {
    let reg = Registry::load(cfg).await?;
    for svc in reg.services.values().filter(|svc| svc.enabled) {
        let unit = unit_name(&svc.name);
        let running = host
            .systemctl(&["is-active", &unit])
            .await
            .map(|s| s.trim() == "active")
            .unwrap_or(false);
        if !running {
            warn!(service = %svc.name, "enabled service is not active; starting");
            let image_metadata = image::load(cfg, &svc.image).await?;
            host.systemd_run_vm(cfg, svc, &image_metadata).await?;
        }
    }
    for entry in WalkDir::new(cfg.run_dir.join("vms"))
        .max_depth(1)
        .into_iter()
        .flatten()
    {
        let path = entry.path();
        if path.extension().and_then(|s| s.to_str()) == Some("sock") {
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            if !reg.services.contains_key(stem) {
                warn!(
                    service = stem,
                    "runtime socket exists for service absent from registry"
                );
            }
        }
    }
    Ok(())
}

pub async fn ensure_dirs(cfg: &Config) -> Result<()> {
    for dir in [
        &cfg.services_dir,
        &cfg.images_dir,
        &cfg.disks_dir,
        &cfg.seeds_dir,
        &cfg.snapshots_dir,
        &cfg.run_dir,
        &cfg.log_dir,
    ] {
        fs::create_dir_all(dir).await?;
    }
    fs::create_dir_all(cfg.run_dir.join("vms")).await?;
    fs::create_dir_all(cfg.run_dir.join("vsock")).await?;
    if !cfg.allocations.exists() {
        Registry::write_allocations(cfg, &Allocations::default()).await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Host;
    use crate::host::{cloud_hypervisor_argv, cloud_hypervisor_restore_argv};
    use crate::image::ImageMetadata;
    use anyhow::Result;
    use async_trait::async_trait;
    use camino::Utf8Path;
    use clap::Parser;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn cloud_hypervisor_argv_contains_documented_flags() {
        let cfg = Config::parse_from(["hearthd"]);
        let svc = Service {
            name: "mail".into(),
            enabled: false,
            image: "debian".into(),
            cpu: 2,
            memory_mib: 2048,
            disk_gib: 20,
            vsock_cid: 100,
            mac: "52:54:00:12:34:56".into(),
            is_agent_in_charge: false,
            cloud_init: CloudInit::default(),
            restart: RestartPolicy::default(),
        };
        let argv = cloud_hypervisor_argv(&cfg, &svc, &ImageMetadata::CloudImage).join(" ");
        assert!(argv.contains("--api-socket /run/hearth/vms/mail.sock"));
        assert!(argv.contains("--vsock cid=100,socket=/run/hearth/vsock/mail.sock"));
        assert!(argv.contains("--cpus boot=2"));
        assert!(argv.contains("--memory size=2048M"));
        // CHV does not accept `bridge=...`; we pre-create the tap and pass it by name.
        assert!(argv.contains("--net tap=hrt-mail,mac=52:54:00:12:34:56"));
        assert!(!argv.contains("bridge="));
    }

    #[test]
    fn docker_rootfs_argv_uses_direct_kernel_boot() {
        let cfg = Config::parse_from([
            "hearthd",
            "--guest-kernel",
            "/run/booted-system/kernel",
            "--guest-initramfs",
            "/var/lib/hearth/initramfs.cpio.gz",
        ]);
        let svc = Service {
            name: "dev".into(),
            enabled: false,
            image: "exeuntu".into(),
            cpu: 4,
            memory_mib: 4096,
            disk_gib: 40,
            vsock_cid: 100,
            mac: "52:54:00:12:34:56".into(),
            is_agent_in_charge: false,
            cloud_init: CloudInit::default(),
            restart: RestartPolicy::default(),
        };
        let manifest = hearth_proto::ImageManifest::docker_rootfs(hearth_proto::OciProcess {
            args: vec!["/usr/local/bin/init".to_string()],
            env: vec!["EXEUNTU=1".to_string()],
            cwd: "/home/exedev".to_string(),
        })
        .unwrap();
        let argv =
            cloud_hypervisor_argv(&cfg, &svc, &ImageMetadata::DockerRootfs(manifest)).join(" ");

        assert!(argv.contains("--kernel /run/booted-system/kernel"));
        assert!(argv.contains("--initramfs /var/lib/hearth/initramfs.cpio.gz"));
        assert!(argv.contains("--disk path=/var/lib/hearth/disks/dev.qcow2"));
        assert!(argv.contains(
            "--cmdline console=ttyS0 root=/dev/vda rootfstype=ext4 rw init=/usr/local/bin/init"
        ));
        assert!(!argv.contains("/var/lib/hearth/seeds/dev.iso"));
    }

    #[test]
    fn cloud_hypervisor_restore_argv_uses_restore_flag() {
        let cfg = Config::parse_from(["hearthd"]);
        let svc = Service {
            name: "mail".into(),
            enabled: false,
            image: "debian".into(),
            cpu: 2,
            memory_mib: 2048,
            disk_gib: 20,
            vsock_cid: 100,
            mac: "52:54:00:12:34:56".into(),
            is_agent_in_charge: false,
            cloud_init: CloudInit::default(),
            restart: RestartPolicy::default(),
        };
        let argv =
            cloud_hypervisor_restore_argv(&cfg, &svc, &Utf8PathBuf::from("/snap/mail/before"))
                .join(" ");
        assert!(argv.contains("--api-socket /run/hearth/vms/mail.sock"));
        assert!(argv.contains("--restore source_url=file:///snap/mail/before,resume=true"));
        assert!(argv.contains("--serial file=/var/log/hearth/mail.console"));
    }

    #[tokio::test]
    async fn registry_loads_service_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let services = root.join("services");
        tokio::fs::create_dir_all(&services).await.unwrap();
        tokio::fs::write(
            services.join("mail.toml"),
            r#"
name = "mail"
enabled = true
image = "debian-12-cloud-amd64"
cpu = 2
memory_mib = 2048
disk_gib = 20
vsock_cid = 100
mac = "52:54:00:12:34:56"

[cloud_init]
hostname = "mail"
ssh_keys = []
user = "agent"

[restart]
policy = "on-failure"
max_retries = 5
backoff_sec = 10
"#,
        )
        .await
        .unwrap();
        let cfg = Config::parse_from([
            "hearthd",
            "--services-dir",
            services.as_str(),
            "--allocations",
            root.join("allocations.toml").as_str(),
        ]);
        let registry = Registry::load(&cfg).await.unwrap();
        let mail = registry.get("mail").unwrap();
        assert!(mail.enabled);
        assert_eq!(mail.vsock_cid, 100);
    }

    #[tokio::test]
    async fn allocator_avoids_existing_service_values_when_allocations_file_is_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let services = root.join("services");
        tokio::fs::create_dir_all(&services).await.unwrap();
        tokio::fs::write(
            services.join("mail.toml"),
            service_toml("mail", false, 100, "52:54:00:00:00:01"),
        )
        .await
        .unwrap();
        let cfg = Config::parse_from([
            "hearthd",
            "--services-dir",
            services.as_str(),
            "--allocations",
            root.join("allocations.toml").as_str(),
        ]);
        let mut registry = Registry::load(&cfg).await.unwrap();
        let (cid, mac) = registry.allocate("web");
        assert_eq!(cid, 101);
        assert_ne!(mac, "52:54:00:00:00:01");
    }

    #[tokio::test]
    async fn logs_non_follow_returns_stream_frames_and_end() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let services = root.join("services");
        let log_dir = root.join("log");
        tokio::fs::create_dir_all(&services).await.unwrap();
        tokio::fs::create_dir_all(&log_dir).await.unwrap();
        tokio::fs::write(
            services.join("mail.toml"),
            service_toml("mail", false, 100, "52:54:00:00:00:01"),
        )
        .await
        .unwrap();
        tokio::fs::write(log_dir.join("mail.console"), "first\nsecond\n")
            .await
            .unwrap();
        let cfg = Config::parse_from([
            "hearthd",
            "--services-dir",
            services.as_str(),
            "--allocations",
            root.join("allocations.toml").as_str(),
            "--log-dir",
            log_dir.as_str(),
        ]);
        let daemon = Daemon::new(cfg, FakeHost::default());
        let req = Request::new(
            "1",
            Verb::Logs,
            Map::from_iter([
                ("name".to_string(), json!("mail")),
                ("follow".to_string(), json!(false)),
            ]),
        );
        let responses = daemon.handle(req).await;
        assert_eq!(responses.len(), 3);
        assert_eq!(responses[0].stream, Some(hearth_proto::StreamKind::Data));
        assert_eq!(responses[0].result, Some(json!({ "line": "first" })));
        assert_eq!(responses[1].result, Some(json!({ "line": "second" })));
        assert_eq!(responses[2].stream, Some(hearth_proto::StreamKind::End));
    }

    #[tokio::test]
    async fn start_runs_systemd_waits_for_socket_and_persists_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", false).await;
        let cfg = test_config(&root);
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new("1", Verb::Start, name_args("mail"));

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.contains(&"systemctl is-active hearth-vm-mail.service".to_string()));
        assert!(calls.contains(&"systemd-run mail".to_string()));
        assert!(calls.iter().any(|call| call.starts_with("wait-socket ")));
        let registry = Registry::load(&cfg).await.unwrap();
        assert!(registry.get("mail").unwrap().enabled);
    }

    #[tokio::test]
    async fn stop_sends_shutdown_and_persists_disabled_when_running() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", true).await;
        let cfg = test_config(&root);
        let host = FakeHost::running();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new("1", Verb::Stop, name_args("mail"));

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls
            .iter()
            .any(|call| call == "chv-put /api/v1/vm.shutdown {}"));
        let registry = Registry::load(&cfg).await.unwrap();
        assert!(!registry.get("mail").unwrap().enabled);
    }

    #[tokio::test]
    async fn resize_running_vm_calls_chv_and_persists_config() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", true).await;
        let cfg = test_config(&root);
        let host = FakeHost::running();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new(
            "1",
            Verb::Resize,
            Map::from_iter([
                ("name".into(), json!("mail")),
                ("cpu".into(), json!(4)),
                ("memory_mib".into(), json!(4096)),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.iter().any(|call| {
            call == "chv-put /api/v1/vm.resize {\"desired_ram\":4294967296,\"desired_vcpus\":4}"
        }));
        let registry = Registry::load(&cfg).await.unwrap();
        let svc = registry.get("mail").unwrap();
        assert_eq!(svc.cpu, 4);
        assert_eq!(svc.memory_mib, 4096);
    }

    #[tokio::test]
    async fn create_allocates_disk_seed_registry_and_allocations_without_starting() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.images_dir).await.unwrap();
        tokio::fs::write(cfg.image_path("debian-12-cloud-amd64"), b"base")
            .await
            .unwrap();
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new(
            "1",
            Verb::Create,
            Map::from_iter([
                ("name".into(), json!("web")),
                ("cpu".into(), json!(4)),
                ("memory_mib".into(), json!(4096)),
                ("disk_gib".into(), json!(30)),
                ("ssh_keys".into(), json!(["ssh-ed25519 AAAA test"])),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls
            .iter()
            .any(|call| call.starts_with("qemu-img create ")));
        assert!(calls.iter().any(|call| call.starts_with("cloud-localds ")));
        assert!(!calls.iter().any(|call| call.starts_with("systemd-run ")));
        let registry = Registry::load(&cfg).await.unwrap();
        let web = registry.get("web").unwrap();
        assert!(!web.enabled);
        assert_eq!(web.cpu, 4);
        assert_eq!(web.memory_mib, 4096);
        assert_eq!(web.disk_gib, 30);
        assert_eq!(registry.allocations.vsock_cids.get("web"), Some(&100));
        assert!(registry.allocations.macs.contains_key("web"));
    }

    #[tokio::test]
    async fn create_from_docker_rootfs_image_skips_cloud_init_seed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.images_dir).await.unwrap();
        tokio::fs::write(cfg.image_path("exeuntu"), b"base")
            .await
            .unwrap();
        tokio::fs::write(cfg.image_manifest_path("exeuntu"), docker_manifest_toml())
            .await
            .unwrap();
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new(
            "1",
            Verb::Create,
            Map::from_iter([
                ("name".into(), json!("dev")),
                ("image".into(), json!("exeuntu")),
                ("disk_gib".into(), json!(40)),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls
            .iter()
            .any(|call| call.starts_with("qemu-img create ")));
        assert!(!calls.iter().any(|call| call.starts_with("cloud-localds ")));
        let registry = Registry::load(&cfg).await.unwrap();
        let dev = registry.get("dev").unwrap();
        assert_eq!(dev.image, "exeuntu");
        assert_eq!(dev.disk_gib, 40);
    }

    #[tokio::test]
    async fn destroy_stops_and_removes_service_artifacts_and_allocations() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", true).await;
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.disks_dir).await.unwrap();
        tokio::fs::create_dir_all(&cfg.seeds_dir).await.unwrap();
        tokio::fs::create_dir_all(&cfg.log_dir).await.unwrap();
        tokio::fs::create_dir_all(cfg.snapshots_dir.join("mail"))
            .await
            .unwrap();
        tokio::fs::write(cfg.disk_path("mail"), b"disk")
            .await
            .unwrap();
        tokio::fs::write(cfg.seed_path("mail"), b"seed")
            .await
            .unwrap();
        tokio::fs::write(cfg.console_path("mail"), b"log")
            .await
            .unwrap();
        Registry::write_allocations(
            &cfg,
            &Allocations {
                vsock_cids: std::iter::once(("mail".to_string(), 100)).collect(),
                macs: std::iter::once(("mail".to_string(), "52:54:00:00:00:01".to_string()))
                    .collect(),
            },
        )
        .await
        .unwrap();
        let host = FakeHost::running();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new("1", Verb::Destroy, name_args("mail"));

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls
            .iter()
            .any(|call| call == "chv-put /api/v1/vm.shutdown {}"));
        assert!(!cfg.disk_path("mail").exists());
        assert!(!cfg.seed_path("mail").exists());
        assert!(!cfg.console_path("mail").exists());
        assert!(!cfg.snapshots_dir.join("mail").exists());
        assert!(!cfg.services_dir.join("mail.toml").exists());
        let registry = Registry::load(&cfg).await.unwrap();
        assert!(!registry.allocations.vsock_cids.contains_key("mail"));
        assert!(!registry.allocations.macs.contains_key("mail"));
    }

    #[tokio::test]
    async fn snapshot_requires_service_and_calls_chv_with_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", true).await;
        let cfg = test_config(&root);
        let host = FakeHost::running();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new(
            "1",
            Verb::Snapshot,
            Map::from_iter([
                ("name".into(), json!("mail")),
                ("tag".into(), json!("before")),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        assert!(cfg.snapshot_dir("mail", "before").is_dir());
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.iter().any(|call| {
            call == &format!(
                "chv-put /api/v1/vm.snapshot {{\"destination_url\":\"file://{}\"}}",
                cfg.snapshot_dir("mail", "before")
            )
        }));

        let missing = daemon
            .handle(Request::new(
                "2",
                Verb::Snapshot,
                Map::from_iter([("name".into(), json!("missing"))]),
            ))
            .await;
        assert!(!missing[0].ok);
        assert_eq!(
            missing[0].error.as_ref().map(|err| err.code.as_str()),
            Some("service.not_found")
        );
    }

    #[tokio::test]
    async fn restore_stops_starts_chv_from_snapshot_and_marks_service_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", false).await;
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(cfg.snapshot_dir("mail", "before"))
            .await
            .unwrap();
        let host = FakeHost::running();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new(
            "1",
            Verb::Restore,
            Map::from_iter([
                ("name".into(), json!("mail")),
                ("tag".into(), json!("before")),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls
            .iter()
            .any(|call| call == "chv-put /api/v1/vm.shutdown {}"));
        assert!(calls.iter().any(|call| call == "systemd-restore mail"));
        assert!(calls.iter().any(|call| call.starts_with("wait-socket ")));
        let registry = Registry::load(&cfg).await.unwrap();
        assert!(registry.get("mail").unwrap().enabled);
    }

    #[tokio::test]
    async fn reboot_calls_chv_reboot() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", true).await;
        let cfg = test_config(&root);
        let host = FakeHost::running();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg, host);
        let req = Request::new("1", Verb::Reboot, name_args("mail"));

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls
            .iter()
            .any(|call| call == "chv-put /api/v1/vm.reboot {}"));
    }

    #[tokio::test]
    async fn net_setup_calls_host_tap_setup() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg, host);
        let req = Request::new(
            "1",
            Verb::NetSetup,
            Map::from_iter([
                ("bridge".into(), json!("hearth0")),
                ("tap".into(), json!("hrt-test")),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls
            .iter()
            .any(|call| call == "setup-tap hearth0 hrt-test"));
    }

    #[tokio::test]
    async fn net_teardown_calls_host_tap_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg, host);
        let req = Request::new(
            "1",
            Verb::NetTeardown,
            Map::from_iter([("tap".into(), json!("hrt-test"))]),
        );

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.iter().any(|call| call == "delete-tap hrt-test"));
    }

    #[tokio::test]
    async fn image_ls_returns_only_qcow2_images_sorted_with_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.images_dir).await.unwrap();
        tokio::fs::write(cfg.images_dir.join("zeta.qcow2"), b"z")
            .await
            .unwrap();
        tokio::fs::write(cfg.images_dir.join("alpha.qcow2"), b"a")
            .await
            .unwrap();
        tokio::fs::write(cfg.images_dir.join("ignore.txt"), b"x")
            .await
            .unwrap();
        let daemon = Daemon::new(cfg, FakeHost::default());

        let responses = daemon
            .handle(Request::new("1", Verb::ImageLs, Map::new()))
            .await;

        assert!(responses[0].ok);
        let images = responses[0]
            .result
            .as_ref()
            .and_then(|result| result.get("images"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(images.len(), 2);
        assert_eq!(images[0].get("name"), Some(&json!("alpha")));
        assert_eq!(images[1].get("name"), Some(&json!("zeta")));
        assert_eq!(images[0].get("kind"), Some(&json!("cloud-image")));
        assert_eq!(images[0].get("bytes"), Some(&json!(1)));
        assert!(images[0]
            .get("sha256")
            .and_then(Value::as_str)
            .is_some_and(|hash| hash.len() == 64));
    }

    #[tokio::test]
    async fn image_import_copies_qcow2_and_manifest_without_overwrite() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        let source_dir = root.join("source");
        tokio::fs::create_dir_all(&source_dir).await.unwrap();
        let source_qcow2 = source_dir.join("exeuntu.qcow2");
        let source_manifest = source_dir.join("exeuntu.hearth.toml");
        tokio::fs::write(&source_qcow2, b"disk").await.unwrap();
        tokio::fs::write(&source_manifest, docker_manifest_toml())
            .await
            .unwrap();
        let daemon = Daemon::new(cfg.clone(), FakeHost::default());

        let imported = daemon
            .handle(Request::new(
                "1",
                Verb::ImageImport,
                Map::from_iter([
                    ("name".into(), json!("exeuntu")),
                    ("qcow2_path".into(), json!(source_qcow2)),
                    ("manifest_path".into(), json!(source_manifest)),
                ]),
            ))
            .await;

        assert!(imported[0].ok);
        let result = imported[0].result.as_ref().unwrap();
        assert_eq!(result.get("kind"), Some(&json!("docker-rootfs")));
        assert!(cfg.image_path("exeuntu").exists());
        assert!(cfg.image_manifest_path("exeuntu").exists());

        let duplicate = daemon
            .handle(Request::new(
                "2",
                Verb::ImageImport,
                Map::from_iter([
                    ("name".into(), json!("exeuntu")),
                    ("qcow2_path".into(), json!(cfg.image_path("exeuntu"))),
                    (
                        "manifest_path".into(),
                        json!(cfg.image_manifest_path("exeuntu")),
                    ),
                ]),
            ))
            .await;
        assert!(!duplicate[0].ok);
        assert_eq!(
            duplicate[0].error.as_ref().map(|err| err.code.as_str()),
            Some("image.exists")
        );
    }

    #[tokio::test]
    async fn image_rm_refuses_referenced_images_and_deletes_unreferenced_images() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", false).await;
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.images_dir).await.unwrap();
        tokio::fs::write(cfg.image_path("debian-12-cloud-amd64"), b"base")
            .await
            .unwrap();
        tokio::fs::write(cfg.image_path("unused"), b"unused")
            .await
            .unwrap();
        tokio::fs::write(cfg.image_manifest_path("unused"), docker_manifest_toml())
            .await
            .unwrap();
        let daemon = Daemon::new(cfg.clone(), FakeHost::default());

        let referenced = daemon
            .handle(Request::new(
                "1",
                Verb::ImageRm,
                Map::from_iter([("name".into(), json!("debian-12-cloud-amd64.qcow2"))]),
            ))
            .await;
        assert!(!referenced[0].ok);
        assert!(cfg.image_path("debian-12-cloud-amd64").exists());

        let removed = daemon
            .handle(Request::new(
                "2",
                Verb::ImageRm,
                Map::from_iter([("name".into(), json!("unused"))]),
            ))
            .await;
        assert!(removed[0].ok);
        assert!(!cfg.image_path("unused").exists());
        assert!(!cfg.image_manifest_path("unused").exists());

        let missing = daemon
            .handle(Request::new(
                "3",
                Verb::ImageRm,
                Map::from_iter([("name".into(), json!("missing"))]),
            ))
            .await;
        assert!(!missing[0].ok);
    }

    fn docker_manifest_toml() -> &'static str {
        r#"
version = 1
kind = "docker-rootfs"
root_device = "/dev/vda"
root_fstype = "ext4"
init = "/usr/local/bin/init"

[oci]
args = ["/usr/local/bin/init"]
env = ["EXEUNTU=1"]
cwd = "/home/exedev"
"#
    }

    fn service_toml(name: &str, enabled: bool, cid: u32, mac: &str) -> String {
        format!(
            r#"
name = "{name}"
enabled = {enabled}
image = "debian-12-cloud-amd64"
cpu = 2
memory_mib = 2048
disk_gib = 20
vsock_cid = {cid}
mac = "{mac}"

[cloud_init]
hostname = "{name}"
ssh_keys = []
user = "agent"

[restart]
policy = "on-failure"
max_retries = 5
backoff_sec = 10
"#
        )
    }

    async fn write_service(root: &Utf8Path, name: &str, enabled: bool) {
        let services = root.join("services");
        tokio::fs::create_dir_all(&services).await.unwrap();
        tokio::fs::write(
            services.join(format!("{name}.toml")),
            service_toml(name, enabled, 100, "52:54:00:00:00:01"),
        )
        .await
        .unwrap();
    }

    fn name_args(name: &str) -> Map<String, Value> {
        Map::from_iter([("name".to_string(), json!(name))])
    }

    fn test_config(root: &Utf8Path) -> Config {
        Config::parse_from([
            "hearthd",
            "--socket",
            root.join("hearth.sock").as_str(),
            "--services-dir",
            root.join("services").as_str(),
            "--allocations",
            root.join("allocations.toml").as_str(),
            "--images-dir",
            root.join("images").as_str(),
            "--disks-dir",
            root.join("disks").as_str(),
            "--seeds-dir",
            root.join("seeds").as_str(),
            "--snapshots-dir",
            root.join("snapshots").as_str(),
            "--run-dir",
            root.join("run").as_str(),
            "--log-dir",
            root.join("log").as_str(),
            "--firmware",
            root.join("CLOUDHV.fd").as_str(),
            "--disable-vsock",
        ])
    }

    #[derive(Clone, Default)]
    struct FakeHost {
        state: Arc<StdMutex<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        calls: Vec<String>,
        running: bool,
    }

    impl FakeHost {
        fn running() -> Self {
            Self {
                state: Arc::new(StdMutex::new(FakeState {
                    calls: Vec::new(),
                    running: true,
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
            _image: &ImageMetadata,
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
            } else {
                Ok(String::new())
            }
        }

        async fn qemu_img_create(
            &self,
            backing: &Utf8Path,
            disk: &Utf8Path,
            disk_gib: u64,
        ) -> Result<()> {
            self.state
                .lock()
                .unwrap()
                .calls
                .push(format!("qemu-img create {backing} {disk} {disk_gib}"));
            Ok(())
        }

        async fn cloud_localds(
            &self,
            seed: &Utf8Path,
            user_data: &Utf8Path,
            meta_data: &Utf8Path,
        ) -> Result<()> {
            self.state
                .lock()
                .unwrap()
                .calls
                .push(format!("cloud-localds {seed} {user_data} {meta_data}"));
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
    }
}
