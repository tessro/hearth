pub mod cloud_init;
pub mod config;
pub mod error;
pub mod host;
pub mod image;
pub mod net;
pub mod notify;
pub mod provision;
pub mod registry;
pub mod vsock;

use crate::{
    config::Config,
    error::{code_of, coded},
    host::{
        boot_config_status, cloud_hypervisor_argv, sanitize_image_name, unit_name,
        wait_for_inactive, DiskFormat, Host,
    },
    image::ImageMetadata,
    net::PublishTarget,
    provision::ProvisionPlan,
    registry::{
        validate_name, Allocations, CloudInit, Provision, Publish, Registry, RestartPolicy, Service,
    },
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
            Verb::Ping => Ok(Dispatch::One(json!({
                "pong": true,
                "version": env!("CARGO_PKG_VERSION"),
                "pid": std::process::id(),
            }))),
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
            Verb::Publish => self.add_publish(req.args).await.map(Dispatch::One),
            Verb::Unpublish => self.remove_publish(req.args).await.map(Dispatch::One),
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
        let leases = self.load_leases().await;
        let mut services = Vec::new();
        for svc in reg.services.values() {
            let running = self.is_running(&svc.name).await;
            let address = resolved_address(&reg, &leases, svc);
            services.push(service_summary(svc, running, address.map(|(ip, _)| ip)));
        }
        Ok(json!({ "services": services }))
    }

    async fn status(&self, name: &str) -> Result<Value> {
        let reg = self.registry().await?;
        let svc = reg.get(name)?;
        let running = self.is_running(name).await;
        let mut value = serde_json::to_value(svc)?;
        // Never echo provisioning literal contents back: replace the serialized
        // provision block with a redacted summary (dest/mode/owner + flags).
        value["provision"] = svc.provision.redacted_summary();
        // Always surface publishes (even when empty) and the guest address.
        value["publish"] = json!(svc.publish);
        let leases = self.load_leases().await;
        value["static_lease"] = json!(reg.allocations.ips.contains_key(name));
        match resolved_address(&reg, &leases, svc) {
            Some((ip, source)) => {
                value["address"] = json!(ip);
                value["address_source"] = json!(source);
            }
            None => {
                value["address"] = Value::Null;
            }
        }
        value["running"] = json!(running);
        if running {
            if let Ok(info) = self
                .host
                .chv_get(&self.cfg.vm_socket(name), "/api/v1/vm.info")
                .await
            {
                value["runtime"] = info;
            }
            if let Some(state) = boot_config_state(&self.cfg, self.host.as_ref(), svc).await {
                value["boot_config"] = json!(state);
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
        let is_docker = matches!(image_metadata, ImageMetadata::DockerRootfs(_));
        let is_agent_in_charge = optional_bool(&args, "is_agent_in_charge").unwrap_or(false);
        if is_agent_in_charge && reg.services.values().any(|svc| svc.is_agent_in_charge) {
            return Err(coded(
                "service.duplicate_agent_in_charge",
                "at most one service may set is_agent_in_charge = true",
            ));
        }

        // Provisioning args mirror the [provision] TOML shape; the CLI has
        // already resolved any client-side files into `from_literal` content.
        let mut provisioning = match args.get("provision") {
            Some(value) => serde_json::from_value::<Provision>(value.clone())
                .map_err(|e| coded("provision.invalid", format!("invalid provision args: {e}")))?,
            None => Provision::default(),
        };
        let hostname_arg = optional_str(&args, "hostname").map(str::to_string);
        // Provisioning is docker-rootfs only; cloud images keep cloud-init.
        let wants_provision = !provisioning.files.is_empty()
            || provisioning.reset_ssh_hostkeys
            || !provisioning.hostname.is_empty();
        if !is_docker && wants_provision {
            return Err(coded(
                "provision.unsupported_image",
                format!(
                    "service {name} uses a cloud image; per-service provisioning is only \
                     supported for docker-rootfs images (cloud images use cloud-init)"
                ),
            ));
        }
        // Resolve the hostname: an explicit --hostname wins, then a hostname set
        // inside the provision block, then the service name.
        let hostname = hostname_arg
            .clone()
            .filter(|h| !h.is_empty())
            .or_else(|| Some(provisioning.hostname.clone()).filter(|h| !h.is_empty()))
            .unwrap_or_else(|| name.to_string());
        let plan = if is_docker {
            provisioning.hostname = hostname.clone();
            // Build (and validate) the plan before any disk work so bad
            // provision args fail with no side effects.
            Some(
                ProvisionPlan::from_provision(&provisioning)
                    .map_err(|e| coded("provision.invalid", format!("{e:#}")))?,
            )
        } else {
            // Cloud images do not persist a provision block.
            provisioning = Provision::default();
            None
        };

        // Managed publishes mirror the [[publish]] TOML shape. Validate every
        // entry before any disk work so bad ports/protocols fail cleanly.
        let publish = match args.get("publish") {
            Some(value) => serde_json::from_value::<Vec<Publish>>(value.clone())
                .map_err(|e| coded("publish.invalid", format!("invalid publish args: {e}")))?,
            None => Vec::new(),
        };
        for entry in &publish {
            entry
                .validate()
                .map_err(|e| coded("publish.invalid", format!("{e:#}")))?;
        }

        let (vsock_cid, mac, static_ip) =
            reg.allocate(name, self.cfg.dhcp_static_start, self.cfg.dhcp_static_count);
        // Every per-VM boot disk is a standalone qcow2 (no backing chain, which
        // CHV rejects, and qcow2 avoids the raw write-path failures CHV hits on
        // some host filesystems such as ZFS). docker-rootfs images are
        // provisioned on a raw scratch and converted to qcow2; see
        // Host::build_docker_disk.
        let disk_filename = format!("{name}.qcow2");
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
            disk: Some(disk_filename.clone()),
            publish,
            cloud_init: CloudInit {
                hostname: hostname.clone(),
                ssh_keys: optional_array_str(&args, "ssh_keys"),
                user: optional_str(&args, "user").unwrap_or("agent").to_string(),
            },
            provision: provisioning,
            restart: RestartPolicy::default(),
        };
        let disk_path = self.cfg.disks_dir.join(&disk_filename);
        let seed_path = self.cfg.seed_path(name);
        // docker-rootfs (plan = Some) provisions a raw scratch and boots the
        // resulting qcow2; cloud images (plan = None) get a qcow2 disk plus a
        // cloud-init seed. On failure, clean up exactly like the other create()
        // error paths.
        match &plan {
            Some(plan) => {
                let scratch = self.cfg.disk_path_ext(name, "raw");
                if let Err(err) = self
                    .host
                    .build_docker_disk(&image_path, &disk_path, &scratch, disk_gib, plan)
                    .await
                {
                    let _ = remove_path_file(scratch).await;
                    let _ = remove_path_file(disk_path).await;
                    reg.free(name);
                    return Err(err);
                }
            }
            None => {
                if let Err(err) = self
                    .host
                    .qemu_img_create(&image_path, &disk_path, disk_gib, DiskFormat::Qcow2)
                    .await
                {
                    reg.free(name);
                    return Err(err);
                }
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
        // Register the static lease and (re)apply the NAT table. Neither failing
        // should undo a created service: reconcile re-writes missing drop-ins and
        // re-applies the table on the next daemon start (self-healing), so these
        // warn-and-continue.
        reg.services.insert(name.to_string(), svc.clone());
        if let Some(ip) = &static_ip {
            if let Err(err) = self.write_dnsmasq_dropin(name, &svc.mac, ip).await {
                warn!(service = %name, error = %err, "failed to write dnsmasq drop-in; reconcile will retry");
            }
        }
        self.rewrite_nat(&reg).await;
        Ok(json!({ "created": service_summary(&svc, false, static_ip) }))
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
            validate_boot_prerequisites(&self.cfg, &image_metadata).await?;
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
        // Re-apply the NAT table: the VM may have just picked up a lease, and its
        // publishes must be routed now that it is running.
        self.rewrite_nat(&reg).await;
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
        self.rewrite_nat(&reg).await;
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
        // The per-VM disk is raw (docker-rootfs) or qcow2 (cloud image / legacy
        // services). Remove whichever exists.
        remove_path_file(self.cfg.disk_path_ext(name, "raw")).await?;
        remove_path_file(self.cfg.disk_path_ext(name, "qcow2")).await?;
        remove_path_file(self.cfg.seed_path(name)).await?;
        remove_path_file(self.cfg.console_path(name)).await?;
        remove_path_dir(self.cfg.snapshots_dir.join(name)).await?;
        self.host.delete_tap(&host::tap_name(name)).await?;
        Registry::remove_service(&self.cfg, name).await?;
        reg.free(name);
        reg.services.remove(name);
        Registry::write_allocations(&self.cfg, &reg.allocations).await?;
        // Drop the static-lease drop-in and re-apply the NAT table without this
        // service's rules.
        if let Err(err) = self.remove_dnsmasq_dropin(name).await {
            warn!(service = %name, error = %err, "failed to remove dnsmasq drop-in");
        }
        self.rewrite_nat(&reg).await;
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
        // Re-apply the NAT table (mirror start_unlocked): the resumed guest may
        // have come up on a different lease than it held before the stop above,
        // so its publishes' DNAT rules must point at the current address.
        let reg = self.registry().await?;
        self.rewrite_nat(&reg).await;
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

    /// Add a named `[[publish]]` to a service and re-apply the nftables table
    /// live — no VM restart. The DNAT is host-side, so a service already
    /// listening on the guest port keeps running uninterrupted.
    async fn add_publish(&self, args: Map<String, Value>) -> Result<Value> {
        let service = required_str(&args, "name")?;
        let _guard = self.service_guard(service).await;
        let mut publish: crate::registry::Publish = serde_json::from_value(
            args.get("publish")
                .cloned()
                .ok_or_else(|| coded("request.invalid", "missing publish object"))?,
        )
        .map_err(|e| coded("publish.invalid", format!("invalid publish args: {e}")))?;
        publish.name = publish.name.trim().to_string();
        if publish.name.is_empty() {
            return Err(coded("publish.invalid", "publish name is required"));
        }
        validate_name(&publish.name)
            .map_err(|e| coded("publish.invalid", format!("publish name {e:#}")))?;
        publish
            .validate()
            .map_err(|e| coded("publish.invalid", format!("{e:#}")))?;
        let mut reg = self.registry().await?;
        let mut svc = reg.get(service)?.clone();
        if svc
            .publish
            .iter()
            .any(|p| p.effective_name() == publish.name)
        {
            return Err(coded(
                "publish.name_exists",
                format!(
                    "service {service} already has a publish named {}",
                    publish.name
                ),
            ));
        }
        // A duplicate (bind, protocol, host_port) across any service would
        // install two conflicting DNAT rules; reject it up front.
        for other in reg.services.values() {
            if let Some(clash) = other.publish.iter().find(|p| {
                p.protocol == publish.protocol
                    && p.host_port == publish.host_port
                    && p.bind == publish.bind
            }) {
                return Err(coded(
                    "publish.host_port_in_use",
                    format!(
                        "host port {}/{} is already published by {} ({})",
                        publish.host_port,
                        publish.protocol,
                        other.name,
                        clash.effective_name()
                    ),
                ));
            }
        }
        svc.publish.push(publish);
        Registry::write_service(&self.cfg, &svc).await?;
        reg.services.insert(service.to_string(), svc);
        self.rewrite_nat(&reg).await;
        self.status(service).await
    }

    /// Remove a service's publish by (effective) name and re-apply the nftables
    /// table live.
    async fn remove_publish(&self, args: Map<String, Value>) -> Result<Value> {
        let service = required_str(&args, "name")?;
        let publish_name = required_str(&args, "publish_name")?;
        let _guard = self.service_guard(service).await;
        let mut reg = self.registry().await?;
        let mut svc = reg.get(service)?.clone();
        let before = svc.publish.len();
        svc.publish.retain(|p| p.effective_name() != publish_name);
        if svc.publish.len() == before {
            return Err(coded(
                "publish.not_found",
                format!("service {service} has no publish named {publish_name}"),
            ));
        }
        Registry::write_service(&self.cfg, &svc).await?;
        reg.services.insert(service.to_string(), svc);
        self.rewrite_nat(&reg).await;
        self.status(service).await
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
            check_path("guest_kernel", &self.cfg.guest_kernel, false),
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
            check_command("nft"),
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

    /// Read + parse the dnsmasq lease file. A missing/unreadable file yields no
    /// leases (never an error): the address is simply reported as null.
    async fn load_leases(&self) -> Vec<net::Lease> {
        read_leases(&self.cfg).await
    }

    /// Fully rewrite the `hearth_nat` nftables table from the registry (every
    /// service's publishes joined to its resolved address). Idempotent — the
    /// same wholesale-rewrite pattern as tap setup — so it is safe to call on
    /// create/start/stop/destroy/reconcile. Failures warn and continue: an
    /// unreachable published port is not a reason to fail the whole operation.
    async fn rewrite_nat(&self, reg: &Registry) {
        apply_nat(&self.cfg, self.host.as_ref(), reg).await
    }

    /// Write the dnsmasq static-lease drop-in for a service and SIGHUP dnsmasq.
    /// If the drop-in dir is absent (dev host without a managed dnsmasq), warn
    /// and skip — the VM still works with dynamic DHCP.
    async fn write_dnsmasq_dropin(&self, name: &str, mac: &str, ip: &str) -> Result<()> {
        let dir = &self.cfg.dnsmasq_dropin_dir;
        if !dir.exists() {
            warn!(
                service = %name,
                dir = %dir,
                "dnsmasq drop-in dir absent; skipping static lease (dynamic DHCP still works)"
            );
            return Ok(());
        }
        let path = dir.join(format!("{name}.conf"));
        fs::write(&path, net::dhcp_host_line(mac, ip))
            .await
            .with_context(|| format!("write dnsmasq drop-in {path}"))?;
        self.reload_dnsmasq(name).await;
        Ok(())
    }

    /// Remove a service's dnsmasq drop-in and SIGHUP dnsmasq if one existed.
    async fn remove_dnsmasq_dropin(&self, name: &str) -> Result<()> {
        let path = self.cfg.dnsmasq_dropin_dir.join(format!("{name}.conf"));
        let existed = path.exists();
        remove_path_file(path).await?;
        if existed {
            self.reload_dnsmasq(name).await;
        }
        Ok(())
    }

    async fn reload_dnsmasq(&self, name: &str) {
        if let Err(err) = self.host.reload_dnsmasq().await {
            warn!(
                service = %name,
                error = %err,
                "failed to SIGHUP dnsmasq (unit may not exist); the static lease applies on the next dnsmasq restart"
            );
        }
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

fn service_summary(svc: &Service, running: bool, address: Option<String>) -> Value {
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
        "address": address,
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

/// Fail fast, daemon-side and before CHV is spawned, if a docker-rootfs image
/// cannot boot with the configured guest kernel. A clear `start` error beats a
/// kernel panic (`Unable to mount root fs`) or a busybox shell on serial. Cloud
/// images boot firmware and skip this. Lives here (not in RealHost) so FakeHost
/// tests exercise it.
pub async fn validate_boot_prerequisites(cfg: &Config, image: &ImageMetadata) -> Result<()> {
    let ImageMetadata::DockerRootfs(manifest) = image else {
        return Ok(());
    };
    if !cfg.guest_kernel.exists() {
        return Err(coded(
            "kernel.not_found",
            format!(
                "guest kernel not found at {}; build it with scripts/build-guest-kernel.sh",
                cfg.guest_kernel
            ),
        ));
    }
    if let Some(initramfs) = &cfg.guest_initramfs {
        if !initramfs.exists() {
            return Err(coded(
                "initramfs.not_found",
                format!(
                    "guest initramfs not found at {initramfs}; build it or unset --guest-initramfs (the vanilla guest kernel needs none)"
                ),
            ));
        }
    }
    let kernel_contract = read_kernel_contract(&cfg.guest_kernel).await?;
    if !image::kernel_contract_satisfies(manifest.min_kernel_contract, kernel_contract) {
        return Err(coded(
            "kernel.contract_too_old",
            format!(
                "image requires guest kernel contract {} but {} provides contract {}; rebuild with scripts/build-guest-kernel.sh",
                manifest.min_kernel_contract, cfg.guest_kernel, kernel_contract
            ),
        ));
    }
    Ok(())
}

/// Resolve the guest kernel's contract number: canonicalize the (possibly
/// `current`-symlinked) kernel path and read the `contract` file next to it. A
/// missing contract file means contract 1 (see `image::parse_kernel_contract`).
async fn read_kernel_contract(kernel: &Utf8PathBuf) -> Result<u32> {
    let resolved = fs::canonicalize(kernel.as_std_path())
        .await
        .with_context(|| format!("resolve guest kernel {kernel}"))?;
    let contents = match resolved.parent().map(|dir| dir.join("contract")) {
        Some(path) => match fs::read_to_string(&path).await {
            Ok(text) => Some(text),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => {
                return Err(
                    anyhow::Error::new(e).context(format!("read kernel contract next to {kernel}"))
                )
            }
        },
        None => None,
    };
    image::parse_kernel_contract(contents.as_deref())
}

/// Boot-config drift state of a *running* service: fetch the argv systemd
/// recorded in the transient unit's `ExecStart` and compare it to what we would
/// launch now. `Some("current")`/`Some("stale")` when determinable, `None`
/// (→ omitted from status; not warned in reconcile) when the image or ExecStart
/// can't be resolved. Free function so `status` and `reconcile` share it.
async fn boot_config_state<H: Host>(cfg: &Config, host: &H, svc: &Service) -> Option<&'static str> {
    let image = image::load(cfg, &svc.image).await.ok()?;
    let expected = cloud_hypervisor_argv(cfg, svc, &image);
    let unit = unit_name(&svc.name);
    let execstart = host
        .systemctl(&["show", "-p", "ExecStart", "--value", &unit])
        .await
        .ok()?;
    match boot_config_status(&execstart, &expected) {
        Some(true) => Some("current"),
        Some(false) => Some("stale"),
        None => None,
    }
}

/// Read + parse the dnsmasq lease file. A missing/unreadable file yields no
/// leases (never an error). Free function so `reconcile` (no `self`) shares it.
async fn read_leases(cfg: &Config) -> Vec<net::Lease> {
    match fs::read_to_string(&cfg.lease_file).await {
        Ok(text) => net::parse_leases(&text),
        Err(_) => Vec::new(),
    }
}

/// Resolve a service's address: an observed lease wins (ground truth), else the
/// static reservation (expected address). `(ip, "lease"|"static")` or `None`.
fn resolved_address(
    reg: &Registry,
    leases: &[net::Lease],
    svc: &Service,
) -> Option<(String, &'static str)> {
    let lease_ip = net::lease_for_mac(leases, &svc.mac).map(|l| l.ip.as_str());
    let static_ip = reg.allocations.ips.get(&svc.name).map(|s| s.as_str());
    net::resolve_address(lease_ip, static_ip).map(|(ip, source)| (ip.to_string(), source))
}

/// Fully rewrite the `hearth_nat` table from the registry. Shared by the
/// per-operation path (`Daemon::rewrite_nat`) and startup reconcile. Warns and
/// continues on any failure.
async fn apply_nat<H: Host>(cfg: &Config, host: &H, reg: &Registry) {
    let leases = read_leases(cfg).await;
    let targets: Vec<PublishTarget> = reg
        .services
        .values()
        .map(|svc| PublishTarget {
            service: svc.name.clone(),
            address: resolved_address(reg, &leases, svc).map(|(ip, _)| ip),
            publishes: svc.publish.clone(),
        })
        .collect();
    let ruleset = net::nat_ruleset(&targets);
    for service in &ruleset.skipped {
        warn!(
            service = %service,
            "service has publishes but no known address; its DNAT rules are omitted until it gets a lease"
        );
    }
    if let Err(err) = host.nft_apply(&ruleset.text).await {
        warn!(error = %err, "failed to apply nft hearth_nat table");
    }
}

/// Re-write any static-lease drop-in that is missing from the drop-in dir (e.g.
/// after a host reboot wiped a tmpfs-backed dir, or a config change). Returns
/// true if any drop-in was written, so the caller SIGHUPs dnsmasq once.
async fn rewrite_missing_dropins<H: Host>(cfg: &Config, host: &H, reg: &Registry) {
    let dir = &cfg.dnsmasq_dropin_dir;
    if !dir.exists() {
        return;
    }
    let mut wrote_any = false;
    for svc in reg.services.values() {
        let Some(ip) = reg.allocations.ips.get(&svc.name) else {
            continue;
        };
        let path = dir.join(format!("{}.conf", svc.name));
        if path.exists() {
            continue;
        }
        warn!(service = %svc.name, "re-writing missing dnsmasq drop-in");
        match fs::write(&path, net::dhcp_host_line(&svc.mac, ip)).await {
            Ok(()) => wrote_any = true,
            Err(err) => {
                warn!(service = %svc.name, error = %err, "failed to re-write dnsmasq drop-in")
            }
        }
    }
    if wrote_any {
        if let Err(err) = host.reload_dnsmasq().await {
            warn!(error = %err, "failed to SIGHUP dnsmasq during reconcile");
        }
    }
}

/// Bring one enabled-but-inactive service up during reconcile: load its image
/// metadata, verify boot prerequisites, then launch the transient VM unit.
/// Fallible so the reconcile loop can warn-and-continue instead of aborting the
/// whole daemon on a single service whose boot cannot proceed.
async fn start_enabled_service<H: Host>(cfg: &Config, host: &H, svc: &Service) -> Result<()> {
    let image_metadata = image::load(cfg, &svc.image).await?;
    validate_boot_prerequisites(cfg, &image_metadata).await?;
    host.systemd_run_vm(cfg, svc, &image_metadata).await?;
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
            // Warn-and-continue like the nft/dnsmasq self-heal below. If image
            // load, boot-prerequisite validation, or the VM launch fails (e.g.
            // a guest kernel wiped or bumped out of contract after a reboot),
            // one bad service must not abort reconcile: that would leave systemd
            // crash-looping the daemon so it never binds its socket, and would
            // skip the networking self-heal for every other service.
            if let Err(err) = start_enabled_service(cfg, host, svc).await {
                warn!(
                    service = %svc.name,
                    error = %err,
                    "failed to start enabled service during reconcile; leaving it down for operator action"
                );
            }
        } else if boot_config_state(cfg, host, svc).await == Some("stale") {
            // The running unit was booted with flags that differ from what we
            // would launch now (older daemon, changed kernel/cmdline). Surface
            // it instead of silently adopting; restart stays the operator's call.
            warn!(
                service = %svc.name,
                "adopting running unit booted with a stale boot config; restart to apply current flags"
            );
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
    // Self-heal host networking that does not survive a reboot (inventory #8):
    // runtime nft rules are gone, and a tmpfs-backed drop-in dir may be empty.
    // Re-write missing static-lease drop-ins and re-apply the NAT table.
    rewrite_missing_dropins(cfg, host, &reg).await;
    apply_nat(cfg, host, &reg).await;
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
    use crate::host::cloud_hypervisor_restore_argv;
    use crate::host::DiskFormat;
    use crate::host::Host;
    use crate::image::ImageMetadata;
    use crate::provision::ProvisionPlan;
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
            disk: None,
            publish: Vec::new(),
            cloud_init: CloudInit::default(),
            provision: Provision::default(),
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
            disk: Some("dev.qcow2".into()),
            publish: Vec::new(),
            cloud_init: CloudInit::default(),
            provision: Provision::default(),
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
        // docker-rootfs boots from the standalone qcow2 disk (provisioned via a
        // raw scratch at create time), and the filename must not lie.
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
            disk: None,
            publish: Vec::new(),
            cloud_init: CloudInit::default(),
            provision: Provision::default(),
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
        let (cid, mac, ip) = registry.allocate("web", "10.26.8.16".parse().unwrap(), 64);
        assert_eq!(cid, 101);
        assert_ne!(mac, "52:54:00:00:00:01");
        assert_eq!(ip.as_deref(), Some("10.26.8.16"));
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
        // Cloud images get a qcow2 per-VM disk and a cloud-init seed, and are
        // never provisioned via loop-mount.
        assert!(calls
            .iter()
            .any(|call| call.starts_with("qemu-img create ") && call.ends_with(" qcow2")));
        assert!(calls.iter().any(|call| call.starts_with("cloud-localds ")));
        assert!(!calls
            .iter()
            .any(|call| call.starts_with("build-docker-disk ")));
        assert!(!calls.iter().any(|call| call.starts_with("systemd-run ")));
        let registry = Registry::load(&cfg).await.unwrap();
        let web = registry.get("web").unwrap();
        assert!(!web.enabled);
        assert_eq!(web.cpu, 4);
        assert_eq!(web.memory_mib, 4096);
        assert_eq!(web.disk_gib, 30);
        assert_eq!(web.disk.as_deref(), Some("web.qcow2"));
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
        // docker-rootfs builds its qcow2 boot disk via a provisioned raw
        // scratch, with no cloud-init seed. Provisioning defaults apply (hostname
        // = service name, machine-id reset).
        assert!(!calls.iter().any(|call| call.starts_with("cloud-localds ")));
        assert!(calls.iter().any(|call| {
            call.starts_with("build-docker-disk ")
                && call.contains("dev.qcow2")
                && call.contains("scratch=")
                && call.contains("dev.raw")
                && call.contains("reset_machine_id=true")
                && call.contains("hostname=dev")
        }));
        let registry = Registry::load(&cfg).await.unwrap();
        let dev = registry.get("dev").unwrap();
        assert_eq!(dev.image, "exeuntu");
        assert_eq!(dev.disk_gib, 40);
        assert_eq!(dev.disk.as_deref(), Some("dev.qcow2"));
        assert_eq!(dev.provision.hostname, "dev");
    }

    #[tokio::test]
    async fn create_docker_rootfs_applies_provision_files_and_hostname() {
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
                ("name".into(), json!("hermes")),
                ("image".into(), json!("exeuntu")),
                ("hostname".into(), json!("hermes-a")),
                (
                    "provision".into(),
                    json!({
                        "reset_ssh_hostkeys": true,
                        "files": [{
                            "from_literal": "TOKEN=secret",
                            "dest": "/home/agent/.hermes/.env",
                            "mode": "0600",
                            "owner": "1000:1000"
                        }]
                    }),
                ),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok, "create failed: {:?}", responses[0].error);
        let calls = state.lock().unwrap().calls.clone();
        let provision_call = calls
            .iter()
            .find(|call| call.starts_with("build-docker-disk "))
            .expect("build-docker-disk call recorded");
        assert!(provision_call.contains("/home/agent/.hermes/.env<-<literal>:0600:1000:1000"));
        assert!(provision_call.contains("reset_ssh_hostkeys=true"));
        assert!(provision_call.contains("hostname=hermes-a"));
        // The literal secret must never appear in a recorded/emitted call.
        assert!(!provision_call.contains("secret"));

        // Persisted, and status redacts the literal content.
        let registry = Registry::load(&cfg).await.unwrap();
        let svc = registry.get("hermes").unwrap();
        assert_eq!(svc.provision.hostname, "hermes-a");
        assert!(svc.provision.reset_ssh_hostkeys);
        assert_eq!(svc.provision.files.len(), 1);
        assert_eq!(
            svc.provision.files[0].from_literal.as_deref(),
            Some("TOKEN=secret")
        );
        let status = daemon
            .handle(Request::new("2", Verb::Status, name_args("hermes")))
            .await;
        let value = status[0].result.as_ref().unwrap();
        let rendered = value["provision"].to_string();
        assert!(rendered.contains("<literal>"));
        assert!(!rendered.contains("TOKEN=secret"));
    }

    #[tokio::test]
    async fn create_cloud_image_rejects_provision_args() {
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
                (
                    "provision".into(),
                    json!({
                        "files": [{
                            "from_literal": "x",
                            "dest": "/etc/x",
                            "mode": "0600",
                            "owner": "0:0"
                        }]
                    }),
                ),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(!responses[0].ok);
        assert_eq!(
            responses[0].error.as_ref().unwrap().code,
            "provision.unsupported_image"
        );
        // No disk was created and nothing was allocated.
        let calls = state.lock().unwrap().calls.clone();
        assert!(!calls
            .iter()
            .any(|call| call.starts_with("qemu-img create ")));
        let registry = Registry::load(&cfg).await.unwrap();
        assert!(registry.get("web").is_err());
        assert!(!registry.allocations.macs.contains_key("web"));
    }

    // ---- §4 networking: managed addresses, static leases, managed publish ----

    #[tokio::test]
    async fn create_allocates_static_ip_writes_dropin_and_sighups_dnsmasq() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        // The drop-in dir must exist for a reservation to be written.
        tokio::fs::create_dir_all(&cfg.dnsmasq_dropin_dir)
            .await
            .unwrap();
        tokio::fs::create_dir_all(&cfg.images_dir).await.unwrap();
        tokio::fs::write(cfg.image_path("debian-12-cloud-amd64"), b"base")
            .await
            .unwrap();
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);

        let responses = daemon
            .handle(Request::new("1", Verb::Create, name_args("web")))
            .await;

        assert!(responses[0].ok, "create failed: {:?}", responses[0].error);
        // The static IP is recorded next to CID/MAC and returned as the address.
        let registry = Registry::load(&cfg).await.unwrap();
        let ip = registry.allocations.ips.get("web").cloned();
        assert_eq!(ip.as_deref(), Some("10.26.8.16"));
        assert_eq!(
            responses[0].result.as_ref().unwrap()["created"]["address"],
            json!("10.26.8.16")
        );
        // The drop-in file was written and dnsmasq was SIGHUP'd.
        let dropin = cfg.dnsmasq_dropin_dir.join("web.conf");
        let contents = tokio::fs::read_to_string(&dropin).await.unwrap();
        let mac = registry.allocations.macs.get("web").unwrap();
        assert_eq!(contents, format!("dhcp-host={mac},10.26.8.16\n"));
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.contains(&"reload-dnsmasq".to_string()));
        // The NAT table is (re)applied even with no publishes.
        assert!(calls.contains(&"nft-apply".to_string()));
    }

    #[tokio::test]
    async fn create_skips_dropin_when_dir_absent_but_still_allocates_ip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        // Deliberately do NOT create the drop-in dir (dev host without managed
        // dnsmasq): create must still succeed and skip the reservation.
        tokio::fs::create_dir_all(&cfg.images_dir).await.unwrap();
        tokio::fs::write(cfg.image_path("debian-12-cloud-amd64"), b"base")
            .await
            .unwrap();
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);

        let responses = daemon
            .handle(Request::new("1", Verb::Create, name_args("web")))
            .await;

        assert!(responses[0].ok, "create failed: {:?}", responses[0].error);
        assert!(!cfg.dnsmasq_dropin_dir.join("web.conf").exists());
        let calls = state.lock().unwrap().calls.clone();
        // No drop-in written -> no dnsmasq reload, but the IP is still reserved.
        assert!(!calls.contains(&"reload-dnsmasq".to_string()));
        let registry = Registry::load(&cfg).await.unwrap();
        assert_eq!(
            registry.allocations.ips.get("web").map(String::as_str),
            Some("10.26.8.16")
        );
    }

    #[tokio::test]
    async fn create_with_publish_renders_dnat_rules_and_persists_them() {
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
                (
                    "publish".into(),
                    json!([
                        { "host_port": 9119, "guest_port": 9119, "protocol": "tcp" },
                        { "host_port": 53, "guest_port": 53, "protocol": "udp", "bind": "100.121.19.41" }
                    ]),
                ),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok, "create failed: {:?}", responses[0].error);
        // The applied ruleset DNATs both ports to the reserved static IP.
        let ruleset = state.lock().unwrap().last_nft.clone().expect("nft applied");
        assert!(ruleset.starts_with("add table ip hearth_nat\nflush table ip hearth_nat\n"));
        assert!(ruleset
            .contains("add rule ip hearth_nat prerouting tcp dport 9119 dnat to 10.26.8.16:9119"));
        assert!(ruleset.contains(
            "add rule ip hearth_nat prerouting ip daddr 100.121.19.41 udp dport 53 dnat to 10.26.8.16:53"
        ));
        // Publishes persist and status surfaces them.
        let registry = Registry::load(&cfg).await.unwrap();
        assert_eq!(registry.get("web").unwrap().publish.len(), 2);
        let status = daemon
            .handle(Request::new("2", Verb::Status, name_args("web")))
            .await;
        let value = status[0].result.as_ref().unwrap();
        assert_eq!(value["publish"][0]["host_port"], json!(9119));
        assert_eq!(value["static_lease"], json!(true));
        assert_eq!(value["address"], json!("10.26.8.16"));
        assert_eq!(value["address_source"], json!("static"));
    }

    #[tokio::test]
    async fn publish_add_and_remove_apply_nat_live_by_name() {
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

        let created = daemon
            .handle(Request::new("1", Verb::Create, name_args("web")))
            .await;
        assert!(created[0].ok, "create failed: {:?}", created[0].error);

        // Add a named forward live: nft is re-applied and it persists by name.
        let add = daemon
            .handle(Request::new(
                "2",
                Verb::Publish,
                Map::from_iter([
                    ("name".into(), json!("web")),
                    (
                        "publish".into(),
                        json!({ "name": "dashboard", "host_port": 8080, "guest_port": 80, "protocol": "tcp" }),
                    ),
                ]),
            ))
            .await;
        assert!(add[0].ok, "publish failed: {:?}", add[0].error);
        let ruleset = state.lock().unwrap().last_nft.clone().expect("nft applied");
        assert!(ruleset.contains("tcp dport 8080 dnat to 10.26.8.16:80"));
        let reg = Registry::load(&cfg).await.unwrap();
        let pubs = reg.get("web").unwrap().publish.clone();
        assert_eq!(pubs.len(), 1);
        assert_eq!(pubs[0].name, "dashboard");

        // A duplicate name is rejected.
        let dup = daemon
            .handle(Request::new(
                "3",
                Verb::Publish,
                Map::from_iter([
                    ("name".into(), json!("web")),
                    (
                        "publish".into(),
                        json!({ "name": "dashboard", "host_port": 9090, "guest_port": 90, "protocol": "tcp" }),
                    ),
                ]),
            ))
            .await;
        assert_eq!(
            dup[0].error.as_ref().map(|e| e.code.as_str()),
            Some("publish.name_exists")
        );

        // A colliding host port (different name) is rejected.
        let clash = daemon
            .handle(Request::new(
                "4",
                Verb::Publish,
                Map::from_iter([
                    ("name".into(), json!("web")),
                    (
                        "publish".into(),
                        json!({ "name": "other", "host_port": 8080, "guest_port": 81, "protocol": "tcp" }),
                    ),
                ]),
            ))
            .await;
        assert_eq!(
            clash[0].error.as_ref().map(|e| e.code.as_str()),
            Some("publish.host_port_in_use")
        );

        // An invalid (non-kebab) name is rejected before any nft change.
        let bad = daemon
            .handle(Request::new(
                "5",
                Verb::Publish,
                Map::from_iter([
                    ("name".into(), json!("web")),
                    (
                        "publish".into(),
                        json!({ "name": "Bad Name", "host_port": 7000, "guest_port": 70, "protocol": "tcp" }),
                    ),
                ]),
            ))
            .await;
        assert_eq!(
            bad[0].error.as_ref().map(|e| e.code.as_str()),
            Some("publish.invalid")
        );

        // Remove by name: nft is re-applied without the rule and it is gone.
        let rm = daemon
            .handle(Request::new(
                "6",
                Verb::Unpublish,
                Map::from_iter([
                    ("name".into(), json!("web")),
                    ("publish_name".into(), json!("dashboard")),
                ]),
            ))
            .await;
        assert!(rm[0].ok, "unpublish failed: {:?}", rm[0].error);
        let ruleset = state.lock().unwrap().last_nft.clone().expect("nft applied");
        assert!(!ruleset.contains("dport 8080"));
        assert!(Registry::load(&cfg)
            .await
            .unwrap()
            .get("web")
            .unwrap()
            .publish
            .is_empty());

        // Removing an unknown name errors.
        let miss = daemon
            .handle(Request::new(
                "7",
                Verb::Unpublish,
                Map::from_iter([
                    ("name".into(), json!("web")),
                    ("publish_name".into(), json!("nope")),
                ]),
            ))
            .await;
        assert_eq!(
            miss[0].error.as_ref().map(|e| e.code.as_str()),
            Some("publish.not_found")
        );
    }

    #[tokio::test]
    async fn unpublish_targets_unnamed_forward_by_derived_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.images_dir).await.unwrap();
        tokio::fs::write(cfg.image_path("debian-12-cloud-amd64"), b"base")
            .await
            .unwrap();
        let daemon = Daemon::new(cfg.clone(), FakeHost::default());
        // A forward created without a name (as `spawn --publish` produces).
        let created = daemon
            .handle(Request::new(
                "1",
                Verb::Create,
                Map::from_iter([
                    ("name".into(), json!("web")),
                    (
                        "publish".into(),
                        json!([{ "host_port": 9119, "guest_port": 9119, "protocol": "tcp" }]),
                    ),
                ]),
            ))
            .await;
        assert!(created[0].ok, "create failed: {:?}", created[0].error);
        // It is addressable by its deterministic `{host}-{proto}` handle.
        let rm = daemon
            .handle(Request::new(
                "2",
                Verb::Unpublish,
                Map::from_iter([
                    ("name".into(), json!("web")),
                    ("publish_name".into(), json!("9119-tcp")),
                ]),
            ))
            .await;
        assert!(rm[0].ok, "unpublish failed: {:?}", rm[0].error);
        assert!(Registry::load(&cfg)
            .await
            .unwrap()
            .get("web")
            .unwrap()
            .publish
            .is_empty());
    }

    #[tokio::test]
    async fn create_rejects_invalid_publish() {
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
                (
                    "publish".into(),
                    json!([{ "host_port": 80, "guest_port": 80, "protocol": "sctp" }]),
                ),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(!responses[0].ok);
        assert_eq!(responses[0].error.as_ref().unwrap().code, "publish.invalid");
        // Nothing was allocated or applied.
        let calls = state.lock().unwrap().calls.clone();
        assert!(!calls
            .iter()
            .any(|call| call.starts_with("qemu-img create ")));
        assert!(!calls.contains(&"nft-apply".to_string()));
        let registry = Registry::load(&cfg).await.unwrap();
        assert!(!registry.allocations.ips.contains_key("web"));
    }

    #[tokio::test]
    async fn status_and_ls_report_lease_address_over_static() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", false).await;
        let cfg = test_config(&root);
        // Allocations carry a static IP; the lease file reports a (different)
        // observed address for the service MAC, which must win.
        Registry::write_allocations(
            &cfg,
            &Allocations {
                vsock_cids: std::iter::once(("mail".to_string(), 100)).collect(),
                macs: std::iter::once(("mail".to_string(), "52:54:00:00:00:01".to_string()))
                    .collect(),
                ips: std::iter::once(("mail".to_string(), "10.26.8.16".to_string())).collect(),
            },
        )
        .await
        .unwrap();
        tokio::fs::write(
            &cfg.lease_file,
            "1720500000 52:54:00:00:00:01 10.26.8.99 mail *\n",
        )
        .await
        .unwrap();
        let daemon = Daemon::new(cfg.clone(), FakeHost::default());

        let status = daemon
            .handle(Request::new("1", Verb::Status, name_args("mail")))
            .await;
        let value = status[0].result.as_ref().unwrap();
        assert_eq!(value["address"], json!("10.26.8.99"));
        assert_eq!(value["address_source"], json!("lease"));
        assert_eq!(value["static_lease"], json!(true));

        let ls = daemon.handle(Request::new("2", Verb::Ls, Map::new())).await;
        let services = ls[0].result.as_ref().unwrap()["services"]
            .as_array()
            .unwrap();
        assert_eq!(services[0]["address"], json!("10.26.8.99"));
    }

    #[tokio::test]
    async fn status_address_is_null_without_lease_or_static() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", false).await;
        let cfg = test_config(&root);
        let daemon = Daemon::new(cfg.clone(), FakeHost::default());

        let status = daemon
            .handle(Request::new("1", Verb::Status, name_args("mail")))
            .await;
        let value = status[0].result.as_ref().unwrap();
        assert_eq!(value["address"], Value::Null);
        assert_eq!(value["static_lease"], json!(false));
        assert!(value.get("address_source").is_none());
    }

    #[tokio::test]
    async fn destroy_removes_dropin_and_reapplies_nat() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", true).await;
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.dnsmasq_dropin_dir)
            .await
            .unwrap();
        tokio::fs::write(cfg.dnsmasq_dropin_dir.join("mail.conf"), "dhcp-host=x\n")
            .await
            .unwrap();
        Registry::write_allocations(
            &cfg,
            &Allocations {
                vsock_cids: std::iter::once(("mail".to_string(), 100)).collect(),
                macs: std::iter::once(("mail".to_string(), "52:54:00:00:00:01".to_string()))
                    .collect(),
                ips: std::iter::once(("mail".to_string(), "10.26.8.16".to_string())).collect(),
            },
        )
        .await
        .unwrap();
        let host = FakeHost::running();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);

        let responses = daemon
            .handle(Request::new("1", Verb::Destroy, name_args("mail")))
            .await;

        assert!(responses[0].ok);
        assert!(!cfg.dnsmasq_dropin_dir.join("mail.conf").exists());
        let registry = Registry::load(&cfg).await.unwrap();
        assert!(!registry.allocations.ips.contains_key("mail"));
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.contains(&"reload-dnsmasq".to_string()));
        assert!(calls.contains(&"nft-apply".to_string()));
    }

    #[tokio::test]
    async fn start_and_stop_reapply_the_nat_table() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", false).await;
        let cfg = test_config(&root);
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);

        let start = daemon
            .handle(Request::new("1", Verb::Start, name_args("mail")))
            .await;
        assert!(start[0].ok);
        assert!(state
            .lock()
            .unwrap()
            .calls
            .contains(&"nft-apply".to_string()));

        state.lock().unwrap().calls.clear();
        let stop = daemon
            .handle(Request::new("2", Verb::Stop, name_args("mail")))
            .await;
        assert!(stop[0].ok);
        assert!(state
            .lock()
            .unwrap()
            .calls
            .contains(&"nft-apply".to_string()));
    }

    #[tokio::test]
    async fn reconcile_rewrites_missing_dropin_and_reapplies_nat() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", false).await;
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.dnsmasq_dropin_dir)
            .await
            .unwrap();
        // Static reservation exists but the drop-in file is missing (simulating a
        // host reboot that wiped a tmpfs-backed drop-in dir).
        Registry::write_allocations(
            &cfg,
            &Allocations {
                vsock_cids: std::iter::once(("mail".to_string(), 100)).collect(),
                macs: std::iter::once(("mail".to_string(), "52:54:00:00:00:01".to_string()))
                    .collect(),
                ips: std::iter::once(("mail".to_string(), "10.26.8.16".to_string())).collect(),
            },
        )
        .await
        .unwrap();
        let host = FakeHost::default();
        let state = host.state.clone();

        reconcile(&cfg, &host).await.unwrap();

        let dropin = cfg.dnsmasq_dropin_dir.join("mail.conf");
        assert_eq!(
            tokio::fs::read_to_string(&dropin).await.unwrap(),
            "dhcp-host=52:54:00:00:00:01,10.26.8.16\n"
        );
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.contains(&"reload-dnsmasq".to_string()));
        assert!(calls.contains(&"nft-apply".to_string()));
    }

    #[tokio::test]
    async fn reconcile_survives_service_with_failed_boot_prerequisites() {
        // An enabled docker-rootfs service whose guest kernel is absent (fresh
        // deploy / wiped kernels dir) must not abort reconcile: the daemon has
        // to keep booting, skip only the broken service, and still self-heal
        // host networking for the rest.
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_docker_service(&root, "dev", true, docker_manifest_toml()).await;
        // Deliberately no write_guest_kernel: validate_boot_prerequisites fails
        // with kernel.not_found for this service.
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.dnsmasq_dropin_dir)
            .await
            .unwrap();
        let host = FakeHost::default();
        let state = host.state.clone();

        // The key assertion: reconcile returns Ok rather than propagating the
        // kernel.not_found error out to main() (which would crash-loop hearthd).
        reconcile(&cfg, &host).await.unwrap();

        let calls = state.lock().unwrap().calls.clone();
        // The broken service was skipped: its VM was never launched.
        assert!(
            !calls.iter().any(|c| c.starts_with("systemd-run ")),
            "service with missing kernel must not be launched: {calls:?}"
        );
        // The networking self-heal still ran for every service.
        assert!(
            calls.contains(&"nft-apply".to_string()),
            "reconcile must still re-apply NAT after skipping a broken service: {calls:?}"
        );
    }

    #[tokio::test]
    async fn start_docker_rootfs_boots_when_kernel_present_and_contract_satisfied() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_docker_service(&root, "dev", false, docker_manifest_toml()).await;
        write_guest_kernel(&root, "1").await;
        let cfg = test_config(&root);
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);

        let responses = daemon
            .handle(Request::new("1", Verb::Start, name_args("dev")))
            .await;

        assert!(responses[0].ok, "start failed: {:?}", responses[0].error);
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.contains(&"systemd-run dev".to_string()));
    }

    #[tokio::test]
    async fn start_docker_rootfs_fails_without_guest_kernel() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_docker_service(&root, "dev", false, docker_manifest_toml()).await;
        // No write_guest_kernel: the configured guest kernel does not exist.
        let cfg = test_config(&root);
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);

        let responses = daemon
            .handle(Request::new("1", Verb::Start, name_args("dev")))
            .await;

        assert!(!responses[0].ok);
        assert_eq!(
            responses[0].error.as_ref().unwrap().code,
            "kernel.not_found"
        );
        // The VM must not have been launched.
        let calls = state.lock().unwrap().calls.clone();
        assert!(!calls.iter().any(|c| c.starts_with("systemd-run ")));
    }

    #[tokio::test]
    async fn start_docker_rootfs_fails_when_contract_too_old() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let manifest = r#"
version = 1
kind = "docker-rootfs"
root_device = "/dev/vda"
root_fstype = "ext4"
init = "/usr/local/bin/init"
min_kernel_contract = 2

[oci]
args = ["/usr/local/bin/init"]
env = ["EXEUNTU=1"]
cwd = "/home/exedev"
"#;
        write_docker_service(&root, "dev", false, manifest).await;
        write_guest_kernel(&root, "1").await;
        let cfg = test_config(&root);
        let host = FakeHost::default();
        let daemon = Daemon::new(cfg.clone(), host);

        let responses = daemon
            .handle(Request::new("1", Verb::Start, name_args("dev")))
            .await;

        assert!(!responses[0].ok);
        assert_eq!(
            responses[0].error.as_ref().unwrap().code,
            "kernel.contract_too_old"
        );
    }

    #[tokio::test]
    async fn status_reports_boot_config_current_when_execstart_matches_launch_argv() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_docker_service(&root, "dev", false, docker_manifest_toml()).await;
        let cfg = test_config(&root);
        let svc = Registry::load(&cfg)
            .await
            .unwrap()
            .get("dev")
            .unwrap()
            .clone();
        let image = crate::image::load(&cfg, "exeuntu").await.unwrap();
        let argv = cloud_hypervisor_argv(&cfg, &svc, &image);
        let host = FakeHost::with_exec_start(fake_execstart(&argv));
        let daemon = Daemon::new(cfg, host);

        let responses = daemon
            .handle(Request::new("1", Verb::Status, name_args("dev")))
            .await;

        assert!(responses[0].ok, "status failed: {:?}", responses[0].error);
        assert_eq!(
            responses[0].result.as_ref().unwrap().get("boot_config"),
            Some(&json!("current"))
        );
    }

    #[tokio::test]
    async fn status_reports_boot_config_stale_when_execstart_drifts() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_docker_service(&root, "dev", false, docker_manifest_toml()).await;
        let cfg = test_config(&root);
        let svc = Registry::load(&cfg)
            .await
            .unwrap()
            .get("dev")
            .unwrap()
            .clone();
        let image = crate::image::load(&cfg, "exeuntu").await.unwrap();
        let mut argv = cloud_hypervisor_argv(&cfg, &svc, &image);
        // Simulate a unit booted by an older daemon: a different kernel path.
        let kernel = argv.iter().position(|arg| arg == "--kernel").unwrap();
        argv[kernel + 1] = "/var/lib/hearth/kernels/OLD/vmlinux".to_string();
        let host = FakeHost::with_exec_start(fake_execstart(&argv));
        let daemon = Daemon::new(cfg, host);

        let responses = daemon
            .handle(Request::new("1", Verb::Status, name_args("dev")))
            .await;

        assert!(responses[0].ok);
        assert_eq!(
            responses[0].result.as_ref().unwrap().get("boot_config"),
            Some(&json!("stale"))
        );
    }

    /// Render an argv the way `systemctl show -p ExecStart --value` would,
    /// quoting arguments that contain spaces (systemd's behavior for our
    /// `--cmdline`).
    fn fake_execstart(argv: &[String]) -> String {
        let rendered: Vec<String> = argv
            .iter()
            .map(|arg| {
                if arg.contains(' ') {
                    format!("\"{arg}\"")
                } else {
                    arg.clone()
                }
            })
            .collect();
        format!(
            "{{ path=/usr/bin/cloud-hypervisor ; argv[]={} ; ignore_errors=no ; start_time=[n/a] ; pid=0 ; code=(null) ; status=0/0 }}",
            rendered.join(" ")
        )
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
        tokio::fs::write(cfg.disk_path_ext("mail", "qcow2"), b"disk")
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
                ..Allocations::default()
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
        assert!(!cfg.disk_path_ext("mail", "qcow2").exists());
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
        // Restore must refresh the NAT table *after* bringing the VM back up, in
        // case the resumed guest took a different lease than it held before.
        let restore_idx = calls
            .iter()
            .position(|call| call == "systemd-restore mail")
            .unwrap();
        assert!(
            calls[restore_idx..].iter().any(|call| call == "nft-apply"),
            "restore must re-apply NAT after resuming the VM: {calls:?}"
        );
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
            "--guest-kernel",
            root.join("kernels/current/vmlinux").as_str(),
            "--lease-file",
            root.join("leases").as_str(),
            "--dnsmasq-dropin-dir",
            root.join("dnsmasq.d").as_str(),
            "--disable-vsock",
        ])
    }

    /// Write a docker-rootfs service plus its image + sidecar manifest so a
    /// `start` exercises the guest-kernel validation path.
    async fn write_docker_service(root: &Utf8Path, name: &str, enabled: bool, manifest_toml: &str) {
        let services = root.join("services");
        tokio::fs::create_dir_all(&services).await.unwrap();
        tokio::fs::write(
            services.join(format!("{name}.toml")),
            format!(
                r#"
name = "{name}"
enabled = {enabled}
image = "exeuntu"
cpu = 2
memory_mib = 2048
disk_gib = 20
vsock_cid = 100
mac = "52:54:00:00:00:01"

[cloud_init]
hostname = "{name}"
ssh_keys = []
user = "agent"

[restart]
policy = "on-failure"
max_retries = 5
backoff_sec = 10
"#
            ),
        )
        .await
        .unwrap();
        let images = root.join("images");
        tokio::fs::create_dir_all(&images).await.unwrap();
        tokio::fs::write(images.join("exeuntu.qcow2"), b"base")
            .await
            .unwrap();
        tokio::fs::write(images.join("exeuntu.hearth.toml"), manifest_toml)
            .await
            .unwrap();
    }

    /// Install a fake guest kernel (a plain file plus a `contract` sibling) at
    /// the path `test_config` points `--guest-kernel` to.
    async fn write_guest_kernel(root: &Utf8Path, contract: &str) {
        let dir = root.join("kernels/current");
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(dir.join("vmlinux"), b"vmlinux")
            .await
            .unwrap();
        tokio::fs::write(dir.join("contract"), contract)
            .await
            .unwrap();
    }

    #[derive(Clone, Default)]
    struct FakeHost {
        state: Arc<StdMutex<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        calls: Vec<String>,
        running: bool,
        exec_start: Option<String>,
        last_nft: Option<String>,
    }

    impl FakeHost {
        fn running() -> Self {
            Self {
                state: Arc::new(StdMutex::new(FakeState {
                    running: true,
                    ..FakeState::default()
                })),
            }
        }

        /// A running host whose transient unit reports `exec_start` from
        /// `systemctl show -p ExecStart --value`, so boot-config drift can be
        /// exercised without systemd.
        fn with_exec_start(exec_start: String) -> Self {
            Self {
                state: Arc::new(StdMutex::new(FakeState {
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

        async fn build_docker_disk(
            &self,
            _backing: &Utf8Path,
            disk: &Utf8Path,
            scratch: &Utf8Path,
            _disk_gib: u64,
            plan: &ProvisionPlan,
        ) -> Result<()> {
            self.state.lock().unwrap().calls.push(format!(
                "build-docker-disk {disk} scratch={scratch} {}",
                plan.describe()
            ));
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
}
