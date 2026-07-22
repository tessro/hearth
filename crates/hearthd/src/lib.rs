pub mod config;
pub mod error;
pub mod guests;
pub mod host;
pub mod image;
pub mod net;
pub mod notify;
pub mod policy;
pub mod provision;
pub mod registry;
pub mod ssh;
pub mod testing;
pub mod vsock;

use crate::{
    config::Config,
    error::{code_of, coded},
    guests::GuestTable,
    host::{boot_config_status, cloud_hypervisor_argv, unit_name, wait_for_inactive, Host},
    net::PublishTarget,
    policy::VerbPolicy,
    provision::ProvisionPlan,
    registry::{
        generate_id, validate_hostname, validate_name, Allocations, Provision, Publish, Registry,
        RestartPolicy, Service,
    },
};
use anyhow::{anyhow, bail, Context, Result};
use camino::Utf8PathBuf;
use chrono::Utc;
#[cfg(test)]
use hearth_agent_proto::PORT_REPORT;
use hearth_agent_proto::{
    fdpass, hybrid, read_line_capped, AgentRequest, AgentVerb, Hello, MAX_LINE_BYTES, PORT_AGENT,
    PORT_GUESTD,
};
use hearth_proto::{version_result, ImageManifest, Request, Response, Verb};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::mem;
use std::os::fd::{AsRawFd, OwnedFd};
use std::{
    collections::{BTreeMap, HashMap},
    sync::Arc,
    time::Instant,
};
use tokio::{
    fs,
    io::{AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::{Mutex, OwnedMutexGuard},
    task::JoinHandle,
    time::{Duration, MissedTickBehavior},
};
use tracing::{error, info, warn};
use walkdir::WalkDir;

const LEASE_POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Boot-disk image captured inside a snapshot directory, alongside CHV's
/// config.json/state.json/memory-ranges. Written by `snapshot` in the same
/// paused window as the memory dump; required by `restore`.
const SNAPSHOT_DISK_FILE: &str = "disk.qcow2";

pub struct Daemon<H> {
    cfg: Config,
    host: Arc<H>,
    registry_lock: Arc<Mutex<()>>,
    service_locks: Arc<Mutex<HashMap<String, Arc<Mutex<()>>>>>,
    /// Guestd boot reports / heartbeats (agent plane §2.1).
    pub(crate) guests: Arc<GuestTable>,
    /// Per-service hybrid vsock listener tasks (agent plane §6).
    pub(crate) channels: Arc<Mutex<HashMap<String, Vec<JoinHandle<()>>>>>,
}

impl<H> Clone for Daemon<H> {
    fn clone(&self) -> Self {
        Self {
            cfg: self.cfg.clone(),
            host: Arc::clone(&self.host),
            registry_lock: Arc::clone(&self.registry_lock),
            service_locks: Arc::clone(&self.service_locks),
            guests: Arc::clone(&self.guests),
            channels: Arc::clone(&self.channels),
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
            guests: Arc::new(GuestTable::default()),
            channels: Arc::new(Mutex::new(HashMap::new())),
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
        let policy = Arc::new(VerbPolicy::load(&self.cfg.verb_policy).await?);
        info!(socket = %self.cfg.socket, "hearthd ready");
        self.bind_running_guest_channels().await;
        let lease_watcher = self.clone();
        tokio::spawn(async move { lease_watcher.watch_lease_changes().await });
        notify::ready()?;
        loop {
            let (stream, _) = listener.accept().await?;
            let daemon = self.clone();
            let policy = Arc::clone(&policy);
            tokio::spawn(async move {
                if let Err(err) = daemon.handle_connection(stream, policy).await {
                    error!(error = %err, "connection failed");
                }
            });
        }
    }

    async fn handle_connection(
        &self,
        mut stream: UnixStream,
        policy: Arc<VerbPolicy>,
    ) -> Result<()> {
        let caller = peer_credentials(&stream);
        loop {
            let Some(line) = read_line_capped(&mut stream, MAX_LINE_BYTES).await? else {
                return Ok(());
            };
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
                    let allowed = policy.allows(
                        caller.as_ref().map(|cred| cred.uid),
                        caller.as_ref().map(|cred| cred.gid),
                        &req.verb,
                    );
                    let ok = if allowed {
                        let (ok, fd) = self.handle_and_write(req, &mut stream).await?;
                        if let Some(fd) = fd {
                            fdpass::send_fd(&stream, fd.as_raw_fd()).await?;
                        }
                        ok
                    } else {
                        write_response(
                            &mut stream,
                            &Response::failure(
                                id.clone(),
                                "verb.denied",
                                format!("peer is not authorized for verb {verb}"),
                            ),
                        )
                        .await?;
                        false
                    };
                    info!(
                        id = %id,
                        verb = %verb,
                        args = %args,
                        caller_transport = "unix",
                        caller_uid = caller.as_ref().map(|cred| cred.uid),
                        caller_gid = caller.as_ref().map(|cred| cred.gid),
                        caller_pid = caller.as_ref().and_then(|cred| cred.pid),
                        allowed,
                        ok,
                        duration_ms = started.elapsed().as_millis() as u64,
                        "audit"
                    );
                }
                Err(err) => {
                    let resp = Response::failure("", "protocol.invalid_json", err.to_string());
                    stream
                        .write_all(serde_json::to_string(&resp)?.as_bytes())
                        .await?;
                    stream.write_all(b"\n").await?;
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
            Ok(Dispatch::PassFd { .. }) => vec![Response::failure(
                id,
                "stream.requires_socket",
                "fd passing requires a live unix socket",
            )],
            Err(err) => vec![Response::failure(id, error_code(&err), format!("{err:#}"))],
        }
    }

    /// Dispatch one request and write its response(s). Returns `(ok, fd)`;
    /// when `fd` is `Some`, the caller must SCM_RIGHTS it to the peer right
    /// after the already-written success line (broker verbs, §6).
    pub(crate) async fn handle_and_write<W: AsyncWrite + Unpin>(
        &self,
        req: Request,
        write: &mut W,
    ) -> Result<(bool, Option<OwnedFd>)> {
        let id = req.id.clone();
        match self.dispatch(req).await {
            Ok(Dispatch::One(value)) => {
                write_response(write, &Response::success(id, value)).await?;
                Ok((true, None))
            }
            Ok(Dispatch::BufferedStream(values)) => {
                for value in values {
                    write_response(write, &Response::stream_data(id.clone(), value)).await?;
                }
                write_response(write, &Response::stream_end(id)).await?;
                Ok((true, None))
            }
            Ok(Dispatch::FollowLog { path }) => {
                self.stream_log(write, id, path, true).await?;
                Ok((true, None))
            }
            Ok(Dispatch::PassFd { result, fd }) => {
                write_response(write, &Response::success(id, result)).await?;
                Ok((true, Some(fd)))
            }
            Err(err) => {
                write_response(
                    write,
                    &Response::failure(id, error_code(&err), format!("{err:#}")),
                )
                .await?;
                Ok((false, None))
            }
        }
    }

    async fn dispatch(&self, req: Request) -> Result<Dispatch> {
        match req.verb {
            Verb::Ping => Ok(Dispatch::One(json!({
                "pong": true,
                "version": hearth_proto::VERSION,
                "pid": std::process::id(),
            }))),
            Verb::Version => Ok(Dispatch::One(version_result(hearth_proto::VERSION))),
            Verb::Ls => self.ls().await.map(Dispatch::One),
            Verb::Status => self
                .status(required_str(&req.args, "name")?)
                .await
                .map(Dispatch::One),
            Verb::Create => self.create(req.args).await.map(Dispatch::One),
            Verb::Rename => self.rename(req.args).await.map(Dispatch::One),
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
                let id = self.service_id(name).await?;
                let _guard = self.service_guard(&id).await;
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
            Verb::Wait => self.wait(req.args).await.map(Dispatch::One),
            Verb::AgentEndpoints => self.agent_endpoints().await.map(Dispatch::One),
            Verb::GuestListener => self.guest_listener(req.args).await,
            Verb::GuestConnect => self.guest_connect(req.args).await,
        }
    }

    async fn registry(&self) -> Result<Registry> {
        Registry::load(&self.cfg).await
    }

    async fn watch_lease_changes(self) {
        let mut previous = None;
        let mut last_error = None;
        let mut interval = tokio::time::interval(LEASE_POLL_INTERVAL);
        interval.set_missed_tick_behavior(MissedTickBehavior::Skip);
        loop {
            interval.tick().await;
            match self.refresh_nat_for_lease_change(&mut previous).await {
                Ok(changed) => {
                    if last_error.take().is_some() {
                        info!(lease_file = %self.cfg.lease_file, "lease-file watch recovered");
                    }
                    if changed {
                        info!(lease_file = %self.cfg.lease_file, "lease addresses changed; re-applied nft hearth_nat table");
                    }
                }
                Err(err) => {
                    let message = format!("{err:#}");
                    if last_error.as_deref() != Some(message.as_str()) {
                        warn!(lease_file = %self.cfg.lease_file, error = %message, "lease-file watch failed; will retry");
                    }
                    last_error = Some(message);
                }
            }
        }
    }

    async fn refresh_nat_for_lease_change(
        &self,
        previous: &mut Option<BTreeMap<String, String>>,
    ) -> Result<bool> {
        let leases = read_leases_checked(&self.cfg).await?;
        let current = net::lease_addresses(&leases);
        if previous.as_ref() == Some(&current) {
            return Ok(false);
        }

        let _registry_guard = self.registry_lock.lock().await;
        let reg = self.registry().await?;
        apply_nat_with_leases(self.host.as_ref(), &reg, &leases).await?;
        *previous = Some(current);
        Ok(true)
    }

    async fn service_id(&self, hostname: &str) -> Result<String> {
        Ok(self.registry().await?.get(hostname)?.id.clone())
    }

    async fn service_guard(&self, id: &str) -> OwnedMutexGuard<()> {
        let lock = {
            let mut locks = self.service_locks.lock().await;
            Arc::clone(
                locks
                    .entry(id.to_string())
                    .or_insert_with(|| Arc::new(Mutex::new(()))),
            )
        };
        lock.lock_owned().await
    }

    async fn ls(&self) -> Result<Value> {
        let reg = self.registry().await?;
        let leases = self.load_leases().await;
        let mut services = Vec::new();
        let mut probes = Vec::new();
        for svc in reg.services.values() {
            let running = self.is_running(&svc.id).await;
            let address = resolved_address(&reg, &leases, svc);
            let mut summary = service_summary(svc, running, address.map(|(ip, _)| ip));
            if let Some(guest) = self.guests.get(&svc.id) {
                summary["guestd"] = guest.summary();
            } else if running {
                let daemon = self.clone();
                let id = svc.id.clone();
                let index = services.len();
                probes.push(async move {
                    let version = match tokio::time::timeout(
                        Duration::from_secs(1),
                        daemon.probe_guestd_version(&id),
                    )
                    .await
                    {
                        Ok(Ok(version)) => Some(version),
                        Ok(Err(_)) | Err(_) => None,
                    };
                    (index, version)
                });
            }
            services.push(summary);
        }
        for (index, version) in futures_util::future::join_all(probes).await {
            if let Some(version) = version {
                services[index]["guestd"] = json!({
                    "version": version,
                    "connected": true,
                    "source": "probe",
                });
            }
        }
        Ok(json!({ "services": services }))
    }

    async fn status(&self, hostname: &str) -> Result<Value> {
        let reg = self.registry().await?;
        let svc = reg.get(hostname)?;
        let running = self.is_running(&svc.id).await;
        let mut value = serde_json::to_value(svc)?;
        // Never echo provisioning literal contents back: replace the serialized
        // provision block with a redacted summary (dest/mode/owner + flags).
        value["provision"] = svc.provision.redacted_summary();
        value["ssh_access"] = json!(svc.provision.ssh_access_state());
        value["ssh_key_fingerprints"] = json!(svc.provision.ssh_key_fingerprints());
        // Always surface publishes (even when empty) and the guest address.
        value["publish"] = json!(svc.publish);
        let leases = self.load_leases().await;
        value["static_lease"] = json!(reg.allocations.ips.contains_key(&svc.id));
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
                .chv_get(&self.cfg.vm_socket(&svc.id), "/api/v1/vm.info")
                .await
            {
                value["runtime"] = info;
            }
            if let Some(state) = boot_config_state(&self.cfg, self.host.as_ref(), svc).await {
                value["boot_config"] = json!(state);
            }
        }
        // Agent-plane telemetry (§2.1): shown when a guestd has reported,
        // absent — absent, not unhealthy — for guestd-less images (§2.5).
        // Guest-reported addresses are corroborating only; a divergence from
        // the lease-resolved address is surfaced, and the lease wins.
        if let Some(guest) = self.guests.get(&svc.id) {
            let lease_ip = value
                .get("address")
                .and_then(Value::as_str)
                .map(str::to_string);
            let diverged = lease_ip.as_deref().is_some_and(|ip| {
                !guest.report.addrs.is_empty()
                    && !guest
                        .report
                        .addrs
                        .iter()
                        .any(|addr| addr.split('/').next() == Some(ip))
            });
            value["guestd"] = guest.summary();
            value["address_divergence"] = json!(diverged);
            if diverged {
                warn!(
                    hostname = %hostname,
                    id = %svc.id,
                    lease = ?lease_ip,
                    reported = ?guest.report.addrs,
                    "guest-reported addresses diverge from lease; lease wins"
                );
            }
        }
        Ok(value)
    }

    async fn create(&self, args: Map<String, Value>) -> Result<Value> {
        let hostname = required_str(&args, "hostname")?;
        validate_hostname(hostname)?;
        let id = generate_id();
        let _service_guard = self.service_guard(&id).await;
        let _registry_guard = self.registry_lock.lock().await;
        let mut reg = self.registry().await?;
        if reg.services.contains_key(hostname) {
            return Err(coded(
                "service.exists",
                format!("service with hostname {hostname} already exists"),
            ));
        }
        let image = required_str(&args, "image")?.to_string();
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
        let manifest = image::load(&self.cfg, &image).await?;
        // Agent-plane participation is declared, not guessed (§2.5): an
        // `agent = true` service requires an image that carries guestd.
        let agent = optional_bool(&args, "agent").unwrap_or(false);
        if agent && !manifest.guestd {
            return Err(coded(
                "agent.requires_guestd",
                format!(
                    "image {image} does not declare guestd; rebuild it on the current vm-base \
                     (or set guestd = true in its manifest after installing hearth-guestd)"
                ),
            ));
        }

        // Provisioning args mirror the [provision] TOML shape; the CLI has
        // already resolved any client-side files into `from_literal` content.
        let mut provisioning = match args.get("provision") {
            Some(value) => serde_json::from_value::<Provision>(value.clone())
                .map_err(|e| coded("provision.invalid", format!("invalid provision args: {e}")))?,
            None => Provision::default(),
        };
        let host_keys = match fs::read_to_string(&self.cfg.authorized_keys_file).await {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(err) => {
                return Err(coded(
                    "ssh.authorized_keys_unreadable",
                    format!("read {}: {err}", self.cfg.authorized_keys_file),
                ))
            }
        };
        let requested_keys = provisioning.authorized_keys.join("\n");
        let merged_keys = crate::ssh::merge_authorized_keys([
            (self.cfg.authorized_keys_file.as_str(), host_keys.as_str()),
            ("create request", requested_keys.as_str()),
        ])
        .map_err(|err| coded("ssh.authorized_keys_invalid", format!("{err:#}")))?;
        provisioning.authorized_keys = merged_keys
            .iter()
            .map(|key| key.canonical.clone())
            .collect();
        if provisioning.authorized_keys.is_empty() && !provisioning.allow_no_ssh {
            return Err(coded(
                "ssh.authorized_keys_required",
                format!(
                    "refusing to create VM without SSH recovery access; configure {}, pass \
                     --ssh-key/--authorized-keys-file, or explicitly pass --allow-no-ssh",
                    self.cfg.authorized_keys_file
                ),
            ));
        }
        if provisioning.authorized_keys.is_empty() {
            warn!(hostname = %hostname, id = %id, "creating VM with SSH recovery access explicitly disabled");
        } else {
            // The escape hatch only describes a genuinely keyless service.
            provisioning.allow_no_ssh = false;
        }
        provisioning.hostname = hostname.to_string();
        // Build (and validate) the plan before any disk work so bad provision
        // args fail with no side effects.
        let plan = ProvisionPlan::from_provision(&provisioning)
            .map_err(|e| coded("provision.invalid", format!("{e:#}")))?;

        // Managed publishes mirror the [[publish]] TOML shape. Validate the
        // whole batch against itself and the registry before any disk work.
        let publish = match args.get("publish") {
            Some(value) => serde_json::from_value::<Vec<Publish>>(value.clone())
                .map_err(|e| coded("publish.invalid", format!("invalid publish args: {e}")))?,
            None => Vec::new(),
        };
        validate_publish_candidates(&reg, hostname, &publish)?;

        let (vsock_cid, mac, static_ip) =
            reg.allocate(&id, self.cfg.dhcp_static_start, self.cfg.dhcp_static_count);
        // Every per-VM boot disk is a standalone qcow2 (no backing chain, which
        // CHV rejects, and qcow2 avoids the raw write-path failures CHV hits on
        // some host filesystems such as ZFS). Images are provisioned on a raw
        // scratch and converted to qcow2; see
        // Host::build_vm_disk.
        let disk_filename = format!("{id}.qcow2");
        let svc = Service {
            id: id.clone(),
            hostname: hostname.to_string(),
            enabled: false,
            image: image.clone(),
            cpu,
            memory_mib,
            disk_gib,
            vsock_cid,
            mac,
            agent,
            disk: Some(disk_filename.clone()),
            publish,
            provision: provisioning,
            restart: RestartPolicy::default(),
        };
        let disk_path = self.cfg.disks_dir.join(&disk_filename);
        let scratch = self.cfg.disk_path_ext(&id, "raw");
        if let Err(err) = self
            .host
            .build_vm_disk(&image_path, &disk_path, &scratch, disk_gib, &plan)
            .await
        {
            let _ = remove_path_file(scratch).await;
            let _ = remove_path_file(disk_path).await;
            reg.free(&id);
            return Err(err);
        }
        if let Err(err) = Registry::write_allocations(&self.cfg, &reg.allocations).await {
            let _ = remove_path_file(disk_path).await;
            return Err(err);
        }
        if let Err(err) = Registry::write_service(&self.cfg, &svc).await {
            let _ = remove_path_file(disk_path).await;
            reg.free(&id);
            let _ = Registry::write_allocations(&self.cfg, &reg.allocations).await;
            return Err(err);
        }
        // Register the static lease and (re)apply the NAT table. Neither failing
        // should undo a created service: reconcile re-writes missing drop-ins and
        // re-applies the table on the next daemon start (self-healing), so these
        // warn-and-continue.
        reg.services.insert(hostname.to_string(), svc.clone());
        if let Some(ip) = &static_ip {
            if let Err(err) = self.write_dnsmasq_dropin(&id, hostname, &svc.mac, ip).await {
                warn!(hostname = %hostname, id = %id, error = %err, "failed to write dnsmasq drop-in; reconcile will retry");
            }
        }
        self.rewrite_nat(&reg).await;
        Ok(json!({ "created": service_summary(&svc, false, static_ip) }))
    }

    async fn rename(&self, args: Map<String, Value>) -> Result<Value> {
        let new_hostname = required_str(&args, "hostname")?;
        validate_hostname(new_hostname)?;
        let id = match optional_str(&args, "id") {
            Some(id) => id.to_string(),
            None => self.service_id(required_str(&args, "name")?).await?,
        };
        let _service_guard = self.service_guard(&id).await;
        let _registry_guard = self.registry_lock.lock().await;
        let mut reg = self.registry().await?;
        let mut svc = reg.get_by_id(&id)?.clone();
        let old_hostname = svc.hostname.clone();
        if old_hostname == new_hostname {
            return Ok(json!({
                "id": id,
                "old_hostname": old_hostname,
                "hostname": new_hostname,
                "guest_hostname_updated": Value::Null,
            }));
        }
        if reg.services.contains_key(new_hostname) {
            return Err(coded(
                "service.exists",
                format!("service with hostname {new_hostname} already exists"),
            ));
        }
        svc.hostname = new_hostname.to_string();
        svc.provision.hostname = new_hostname.to_string();
        Registry::write_service(&self.cfg, &svc).await?;
        reg.services.remove(&old_hostname);
        reg.services.insert(new_hostname.to_string(), svc.clone());
        if let Some(ip) = reg.allocations.ips.get(&id) {
            if let Err(err) = self
                .write_dnsmasq_dropin(&id, new_hostname, &svc.mac, ip)
                .await
            {
                warn!(hostname = %new_hostname, id = %id, error = %err, "failed to update DNS after rename");
            }
        }
        self.rewrite_nat(&reg).await;
        drop(_registry_guard);

        let guest_hostname_updated = if self.is_running(&id).await {
            match self.set_guest_hostname(&id, new_hostname).await {
                Ok(()) => Value::Bool(true),
                Err(err) => {
                    warn!(hostname = %new_hostname, id = %id, error = %err, "host rename committed but guest hostname update failed");
                    Value::Bool(false)
                }
            }
        } else {
            Value::Null
        };
        Ok(json!({
            "id": id,
            "old_hostname": old_hostname,
            "hostname": new_hostname,
            "guest_hostname_updated": guest_hostname_updated,
        }))
    }

    async fn set_guest_hostname(&self, id: &str, hostname: &str) -> Result<()> {
        let mut stream = UnixStream::connect(self.cfg.vm_vsock_socket(id).as_str())
            .await
            .context("connect guest vsock for hostname update")?;
        hybrid::connect_handshake(&mut stream, PORT_GUESTD)
            .await
            .context("connect guestd hostname channel")?;
        let hello = Hello::new("agentd", hearth_proto::VERSION);
        stream
            .write_all((serde_json::to_string(&hello)? + "\n").as_bytes())
            .await?;
        read_success_response(&mut stream, "guestd hello").await?;
        let mut args = Map::new();
        args.insert("hostname".to_string(), json!(hostname));
        let req = AgentRequest::new(generate_id(), AgentVerb::SetHostname, args);
        stream
            .write_all((serde_json::to_string(&req)? + "\n").as_bytes())
            .await?;
        read_success_response(&mut stream, "set guest hostname")
            .await
            .map(|_| ())
    }

    async fn probe_guestd_version(&self, id: &str) -> Result<String> {
        let mut stream = UnixStream::connect(self.cfg.vm_vsock_socket(id).as_str())
            .await
            .context("connect guest vsock for version probe")?;
        hybrid::connect_handshake(&mut stream, PORT_GUESTD)
            .await
            .context("connect guestd version channel")?;
        let hello = Hello::new("agentd", hearth_proto::VERSION);
        stream
            .write_all((serde_json::to_string(&hello)? + "\n").as_bytes())
            .await?;
        read_success_response(&mut stream, "guestd hello").await?;
        let req = AgentRequest::new(generate_id(), AgentVerb::Version, Map::new());
        stream
            .write_all((serde_json::to_string(&req)? + "\n").as_bytes())
            .await?;
        read_success_response(&mut stream, "guestd version")
            .await?
            .get("version")
            .and_then(Value::as_str)
            .filter(|version| !version.is_empty())
            .map(str::to_string)
            .ok_or_else(|| anyhow!("guestd version response omitted version"))
    }

    async fn start(&self, hostname: &str) -> Result<Value> {
        let id = self.service_id(hostname).await?;
        let _guard = self.service_guard(&id).await;
        self.start_unlocked(hostname).await
    }

    async fn start_unlocked(&self, hostname: &str) -> Result<Value> {
        let mut reg = self.registry().await?;
        let mut svc = reg.get(hostname)?.clone();
        if svc.provision.ssh_access_state() != "configured" {
            warn!(
                hostname = %hostname,
                id = %svc.id,
                ssh_access = svc.provision.ssh_access_state(),
                "starting VM without confirmed SSH recovery access"
            );
        }
        if !self.is_running(&svc.id).await {
            let image_metadata = image::load(&self.cfg, &svc.image).await?;
            validate_boot_prerequisites(&self.cfg, &image_metadata).await?;
            // A fresh boot invalidates any previous guestd report; `wait` must
            // block on this boot's report, not the last one's.
            self.guests.forget(&svc.id);
            self.host
                .systemd_run_vm(&self.cfg, &svc, &image_metadata)
                .await?;
            self.host
                .wait_for_vm_socket(&self.cfg.vm_socket(&svc.id), Duration::from_secs(20))
                .await?;
        }
        self.ensure_guest_channels(&svc.id).await?;
        svc.enabled = true;
        Registry::write_service(&self.cfg, &svc).await?;
        reg.services.insert(hostname.to_string(), svc);
        // Re-apply the NAT table: the VM may have just picked up a lease, and its
        // publishes must be routed now that it is running.
        self.rewrite_nat(&reg).await;
        self.status(hostname).await
    }

    async fn stop(&self, hostname: &str) -> Result<Value> {
        let id = self.service_id(hostname).await?;
        let _guard = self.service_guard(&id).await;
        self.stop_unlocked(hostname).await
    }

    async fn stop_unlocked(&self, hostname: &str) -> Result<Value> {
        let reg = self.registry().await?;
        let mut svc = reg.get(hostname)?.clone();
        if self.is_running(&svc.id).await {
            let socket = self.cfg.vm_socket(&svc.id);
            let unit = unit_name(&svc.id);
            let graceful_timeout = Duration::from_secs(30);
            let started = Instant::now();
            info!(hostname = %hostname, id = %svc.id, "sending vm.shutdown (ACPI)");
            if let Err(err) = self
                .host
                .chv_put(&socket, "/api/v1/vm.shutdown", json!({}))
                .await
            {
                warn!(hostname = %hostname, id = %svc.id, error = %err, "vm.shutdown request failed; waiting for unit to exit anyway");
            }
            if wait_for_inactive(self.host.as_ref(), &unit, graceful_timeout).await? {
                info!(
                    hostname = %hostname,
                    id = %svc.id,
                    duration_ms = started.elapsed().as_millis() as u64,
                    "vm stopped gracefully"
                );
            } else {
                warn!(
                    hostname = %hostname,
                    id = %svc.id,
                    waited_ms = started.elapsed().as_millis() as u64,
                    timeout_s = graceful_timeout.as_secs(),
                    "graceful shutdown timed out; escalating to vm.power-off"
                );
                if let Err(err) = self
                    .host
                    .chv_put(&socket, "/api/v1/vm.power-off", json!({}))
                    .await
                {
                    warn!(hostname = %hostname, id = %svc.id, error = %err, "vm.power-off request failed");
                }
                if let Err(err) = self.host.systemctl(&["stop", &unit]).await {
                    warn!(hostname = %hostname, id = %svc.id, error = %err, "systemctl stop failed");
                }
            }
        }
        svc.enabled = false;
        Registry::write_service(&self.cfg, &svc).await?;
        self.drop_guest_channels(&svc.id).await;
        self.guests.forget(&svc.id);
        self.rewrite_nat(&reg).await;
        Ok(json!({ "id": svc.id, "hostname": hostname, "running": false, "enabled": false }))
    }

    async fn reboot(&self, hostname: &str) -> Result<Value> {
        let reg = self.registry().await?;
        let svc = reg.get(hostname)?;
        let _guard = self.service_guard(&svc.id).await;
        self.host
            .chv_put(&self.cfg.vm_socket(&svc.id), "/api/v1/vm.reboot", json!({}))
            .await?;
        self.status(hostname).await
    }

    async fn destroy(&self, hostname: &str) -> Result<Value> {
        let id = self.service_id(hostname).await?;
        let _service_guard = self.service_guard(&id).await;
        self.stop_unlocked(hostname).await?;
        let _registry_guard = self.registry_lock.lock().await;
        let mut reg = self.registry().await?;
        let svc = reg.get(hostname)?.clone();
        // Remove the qcow2 boot disk and any interrupted provisioning scratch.
        remove_path_file(self.cfg.disk_path_ext(&id, "raw")).await?;
        remove_path_file(self.cfg.disk_path(&svc)).await?;
        remove_path_file(self.cfg.console_path(&id)).await?;
        remove_path_dir(self.cfg.snapshots_dir.join(&id)).await?;
        self.host.delete_tap(&host::tap_name(&id)).await?;
        Registry::remove_service(&self.cfg, &id).await?;
        self.drop_agent_channel(&id).await;
        reg.free(&id);
        reg.services.remove(hostname);
        Registry::write_allocations(&self.cfg, &reg.allocations).await?;
        // Drop the static-lease drop-in and re-apply the NAT table without this
        // service's rules.
        if let Err(err) = self.remove_dnsmasq_dropin(&id).await {
            warn!(hostname = %hostname, id = %id, error = %err, "failed to remove dnsmasq drop-in");
        }
        self.rewrite_nat(&reg).await;
        Ok(json!({ "destroyed": hostname, "id": id }))
    }

    async fn snapshot(&self, args: Map<String, Value>) -> Result<Value> {
        let hostname = required_str(&args, "name")?;
        let id = self.service_id(hostname).await?;
        let _guard = self.service_guard(&id).await;
        let tag = optional_str(&args, "tag")
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| Utc::now().format("%Y%m%d%H%M%S").to_string());
        let reg = self.registry().await?;
        let svc = reg.get_by_id(&id)?.clone();
        let dest = self.cfg.snapshot_dir(&id, &tag);
        fs::create_dir_all(&dest).await?;
        let socket = self.cfg.vm_socket(&id);
        // CHV refuses to snapshot a running VM, so pause around the dump. The
        // resume is attempted even when the snapshot fails — a paused guest
        // must never be the residue of a failed snapshot.
        self.host
            .chv_put_empty(&socket, "/api/v1/vm.pause")
            .await
            .context("pause VM for snapshot")?;
        let snapshot = self
            .host
            .chv_put(
                &socket,
                "/api/v1/vm.snapshot",
                json!({ "destination_url": format!("file://{dest}") }),
            )
            .await;
        // CHV's snapshot is memory/device state only. Capture the boot disk in
        // the same paused window: resuming that state against a disk that kept
        // advancing wedges the guest (dead net/vsock), so a snapshot without
        // its disk is not restorable at all.
        let disk_copy = if snapshot.is_ok() {
            self.host
                .copy_disk(&self.cfg.disk_path(&svc), &dest.join(SNAPSHOT_DISK_FILE))
                .await
        } else {
            Ok(())
        };
        let resume = self.host.chv_put_empty(&socket, "/api/v1/vm.resume").await;
        snapshot.context("snapshot paused VM")?;
        disk_copy.context("copy boot disk into snapshot")?;
        resume.context("resume VM after snapshot")?;
        Ok(json!({ "id": id, "hostname": hostname, "tag": tag, "path": dest }))
    }

    async fn restore(&self, args: Map<String, Value>) -> Result<Value> {
        let hostname = required_str(&args, "name")?;
        let id = self.service_id(hostname).await?;
        let _guard = self.service_guard(&id).await;
        let tag = required_str(&args, "tag")?;
        let src = self.cfg.snapshot_dir(&id, tag);
        if !src.exists() {
            return Err(coded(
                "snapshot.not_found",
                format!("snapshot not found: {src}"),
            ));
        }
        // Refuse before touching the running VM: resuming CHV memory state
        // against a disk that advanced past the snapshot wedges the guest, so
        // a snapshot without its captured disk is not restorable.
        let disk_image = src.join(SNAPSHOT_DISK_FILE);
        if !disk_image.exists() {
            return Err(coded(
                "snapshot.no_disk",
                format!(
                    "snapshot {tag} has no captured boot disk at {disk_image}; \
                     retake the snapshot with this daemon"
                ),
            ));
        }
        let _ = self.stop_unlocked(hostname).await;
        let reg = self.registry().await?;
        let mut svc = reg.get(hostname)?.clone();
        self.host
            .copy_disk(&disk_image, &self.cfg.disk_path(&svc))
            .await
            .context("restore boot disk from snapshot")?;
        // The restored guest's next boot report gets `restored: true` in its
        // ack, so guestd rotates task incarnations and outstanding cursors go
        // cleanly stale (§3.4). Marked before the guest can possibly reconnect.
        self.guests.mark_pending_restore(&id);
        self.host.systemd_restore_vm(&self.cfg, &svc, &src).await?;
        self.host
            .wait_for_vm_socket(&self.cfg.vm_socket(&id), Duration::from_secs(20))
            .await?;
        self.ensure_guest_channels(&id).await?;
        svc.enabled = true;
        Registry::write_service(&self.cfg, &svc).await?;
        // Re-apply the NAT table (mirror start_unlocked): the resumed guest may
        // have come up on a different lease than it held before the stop above,
        // so its publishes' DNAT rules must point at the current address.
        let reg = self.registry().await?;
        self.rewrite_nat(&reg).await;
        Ok(json!({ "id": id, "hostname": hostname, "tag": tag, "restored": true }))
    }

    async fn resize(&self, args: Map<String, Value>) -> Result<Value> {
        let hostname = required_str(&args, "name")?;
        let id = self.service_id(hostname).await?;
        let _guard = self.service_guard(&id).await;
        let mut reg = self.registry().await?;
        let mut svc = reg.get(hostname)?.clone();
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
        if self.is_running(&id).await {
            self.host
                .chv_put(
                    &self.cfg.vm_socket(&id),
                    "/api/v1/vm.resize",
                    Value::Object(body),
                )
                .await?;
        }
        Registry::write_service(&self.cfg, &svc).await?;
        reg.services.insert(hostname.to_string(), svc);
        self.status(hostname).await
    }

    async fn logs(&self, args: Map<String, Value>) -> Result<Dispatch> {
        let hostname = required_str(&args, "name")?;
        let reg = self.registry().await?;
        let svc = reg.get(hostname)?;
        let follow = optional_bool(&args, "follow").unwrap_or(false);
        if follow {
            return Ok(Dispatch::FollowLog {
                path: self.cfg.console_path(&svc.id),
            });
        }
        let text = read_optional_string(self.cfg.console_path(&svc.id)).await?;
        let lines: Vec<Value> = text.lines().map(|line| json!({ "line": line })).collect();
        Ok(Dispatch::BufferedStream(lines))
    }

    async fn image_ls(&self) -> Result<Value> {
        fs::create_dir_all(&self.cfg.images_dir).await?;
        let mut images = Vec::new();
        let mut warnings = Vec::new();
        let mut entries = fs::read_dir(&self.cfg.images_dir).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = Utf8PathBuf::from_path_buf(entry.path())
                .map_err(|_| anyhow!("non-utf8 image path"))?;
            if path.extension() != Some("qcow2") {
                continue;
            }
            let name = path.file_stem().unwrap_or_default();
            match self.image_info(name).await {
                Ok(info) => images.push(info),
                Err(err) => {
                    warn!(image = %name, error = %err, "skipping invalid image entry");
                    warnings.push(json!({ "name": name, "error": err.to_string() }));
                }
            }
        }
        images.sort_by(|left, right| {
            left.get("name")
                .and_then(Value::as_str)
                .cmp(&right.get("name").and_then(Value::as_str))
        });
        Ok(json!({ "images": images, "warnings": warnings }))
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
                format!("image {name} is still used by service {}", svc.hostname),
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
        image::load(&self.cfg, name).await?;
        Ok(json!({
            "name": name,
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
        let id = self.service_id(service).await?;
        let _guard = self.service_guard(&id).await;
        let _registry_guard = self.registry_lock.lock().await;
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
        let mut reg = self.registry().await?;
        let mut svc = reg.get(service)?.clone();
        validate_publish_candidates(&reg, service, std::slice::from_ref(&publish))?;
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
        let id = self.service_id(service).await?;
        let _guard = self.service_guard(&id).await;
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

    /// Block until a guestd boot report marks the service ready (kills
    /// workaround #12). Readiness is declared, not guessed: an image that does
    /// not declare guestd gets a clean error telling the caller to keep using
    /// `--marker`.
    async fn wait(&self, args: Map<String, Value>) -> Result<Value> {
        let name = required_str(&args, "name")?;
        let timeout_secs = optional_u64(&args, "timeout").unwrap_or(300);
        let reg = self.registry().await?;
        let svc = reg.get(name)?;
        let manifest = image::load(&self.cfg, &svc.image).await?;
        if !manifest.guestd {
            return Err(coded(
                "wait.requires_marker",
                format!(
                    "image {} does not declare guestd; use wait --marker as before",
                    svc.image
                ),
            ));
        }
        if !self.is_running(&svc.id).await {
            return Err(coded(
                "wait.not_running",
                format!("service {name} is not running"),
            ));
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(timeout_secs);
        let mut changes = self.guests.subscribe();
        loop {
            if let Some(guest) = self.guests.get(&svc.id) {
                if guest.report.ready {
                    return Ok(json!({
                        "id": svc.id,
                        "hostname": name,
                        "ready": true,
                        "guestd": guest.summary(),
                    }));
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                return Err(coded(
                    "wait.timeout",
                    format!("timed out after {timeout_secs}s waiting for {name}'s boot report"),
                ));
            }
            if tokio::time::timeout(remaining, changes.changed())
                .await
                .is_err()
            {
                return Err(coded(
                    "wait.timeout",
                    format!("timed out after {timeout_secs}s waiting for {name}'s boot report"),
                ));
            }
        }
    }

    /// Agent-plane discovery: every `agent = true` service with its guestd
    /// telemetry. This is how agentd learns which VMs exist without ever
    /// touching the vsock directory itself.
    async fn agent_endpoints(&self) -> Result<Value> {
        let reg = self.registry().await?;
        let mut endpoints = Vec::new();
        for svc in reg.services.values().filter(|svc| svc.agent) {
            let running = self.is_running(&svc.id).await;
            let guest = self.guests.get(&svc.id);
            endpoints.push(json!({
                "id": svc.id,
                "hostname": svc.hostname,
                "running": running,
                "guestd": guest.map(|g| g.summary()),
            }));
        }
        Ok(json!({ "agents": endpoints }))
    }

    /// Broker verb (§6): bind `<id>.sock_<port>` and pass the listening fd to
    /// the caller. Only agent-plane ports may be brokered, and only for
    /// agent-enabled services; the vsock directory itself stays root-owned.
    async fn guest_listener(&self, args: Map<String, Value>) -> Result<Dispatch> {
        let id = required_str(&args, "id")?;
        let port = optional_u64(&args, "port").unwrap_or(PORT_AGENT as u64) as u32;
        if port != PORT_AGENT {
            return Err(coded(
                "broker.port_not_allowed",
                format!("port {port} is not an agent-plane broker port"),
            ));
        }
        let reg = self.registry().await?;
        let svc = reg.get_by_id(id)?;
        if !svc.agent {
            return Err(coded(
                "agent.not_enabled",
                format!("service {} is not agent-enabled", svc.hostname),
            ));
        }
        let path = self.cfg.vm_vsock_port_socket(id, port);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await?;
        }
        match fs::remove_file(&path).await {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err).context("remove stale brokered socket"),
        }
        let listener = std::os::unix::net::UnixListener::bind(path.as_str())
            .with_context(|| format!("bind brokered listener {path}"))?;
        Ok(Dispatch::PassFd {
            result: json!({ "id": id, "hostname": svc.hostname, "port": port }),
            fd: OwnedFd::from(listener),
        })
    }

    /// Broker verb (§6): connect to `<id>.sock` (the CHV hybrid vsock socket)
    /// and pass the connected fd; the caller performs the in-band
    /// `CONNECT <port>` handshake itself.
    async fn guest_connect(&self, args: Map<String, Value>) -> Result<Dispatch> {
        let id = required_str(&args, "id")?;
        let reg = self.registry().await?;
        let svc = reg.get_by_id(id)?;
        if !svc.agent {
            return Err(coded(
                "agent.not_enabled",
                format!("service {} is not agent-enabled", svc.hostname),
            ));
        }
        if !self.is_running(id).await {
            return Err(coded(
                "service.not_running",
                format!("service {} is not running", svc.hostname),
            ));
        }
        let path = self.cfg.vm_vsock_socket(id);
        let stream = UnixStream::connect(path.as_str())
            .await
            .with_context(|| format!("connect guest vsock socket {path}"))?;
        let stream = stream.into_std().context("detach guest vsock stream")?;
        Ok(Dispatch::PassFd {
            result: json!({ "id": id, "hostname": svc.hostname }),
            fd: OwnedFd::from(stream),
        })
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
            check_path("snapshots_dir", &self.cfg.snapshots_dir, true),
            check_path("run_dir", &self.cfg.run_dir, true),
            check_path("log_dir", &self.cfg.log_dir, true),
            check_path("guest_kernel", &self.cfg.guest_kernel, false),
            check_authorized_keys_file(&self.cfg.authorized_keys_file).await,
            check_character_device("kvm_device", &Utf8PathBuf::from("/dev/kvm")),
            check_path(
                "bridge",
                &Utf8PathBuf::from(format!("/sys/class/net/{}", self.cfg.bridge)),
                true,
            ),
            check_command_version("cloud-hypervisor").await,
            check_command("qemu-img"),
            check_command("nft"),
            check_kernel_module("kvm").await?,
            check_kernel_module("vhost_vsock").await?,
        ];
        Ok(json!({ "checks": checks }))
    }

    async fn is_running(&self, id: &str) -> bool {
        let unit = unit_name(id);
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
    async fn write_dnsmasq_dropin(
        &self,
        id: &str,
        hostname: &str,
        mac: &str,
        ip: &str,
    ) -> Result<()> {
        let dir = &self.cfg.dnsmasq_dropin_dir;
        if !dir.exists() {
            warn!(
                hostname = %hostname,
                id = %id,
                dir = %dir,
                "dnsmasq drop-in dir absent; skipping static lease (dynamic DHCP still works)"
            );
            return Ok(());
        }
        let path = dir.join(format!("{id}.conf"));
        fs::write(&path, net::dhcp_host_line(mac, ip, hostname))
            .await
            .with_context(|| format!("write dnsmasq drop-in {path}"))?;
        self.reload_dnsmasq(hostname).await;
        Ok(())
    }

    /// Remove a service's dnsmasq drop-in and SIGHUP dnsmasq if one existed.
    async fn remove_dnsmasq_dropin(&self, id: &str) -> Result<()> {
        let path = self.cfg.dnsmasq_dropin_dir.join(format!("{id}.conf"));
        let existed = path.exists();
        remove_path_file(path).await?;
        if existed {
            self.reload_dnsmasq(id).await;
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

    async fn stream_log<W: AsyncWrite + Unpin>(
        &self,
        write: &mut W,
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

pub(crate) enum Dispatch {
    One(Value),
    BufferedStream(Vec<Value>),
    FollowLog {
        path: Utf8PathBuf,
    },
    /// A success line followed by one SCM_RIGHTS fd (broker verbs, §6). Only
    /// deliverable over the host unix socket.
    PassFd {
        result: Value,
        fd: OwnedFd,
    },
}

async fn write_response<W: AsyncWrite + Unpin>(write: &mut W, response: &Response) -> Result<()> {
    write
        .write_all(serde_json::to_string(response)?.as_bytes())
        .await?;
    write.write_all(b"\n").await?;
    Ok(())
}

async fn read_success_response<R: tokio::io::AsyncRead + Unpin>(
    read: &mut R,
    context: &str,
) -> Result<Value> {
    let line = read_line_capped(read, MAX_LINE_BYTES)
        .await?
        .ok_or_else(|| anyhow!("{context}: connection closed"))?;
    let response: Response = serde_json::from_str(&line)?;
    if response.ok {
        Ok(response.result.unwrap_or(Value::Null))
    } else {
        let err = response
            .error
            .map(|e| format!("{}: {}", e.code, e.message))
            .unwrap_or_else(|| "unknown guest error".to_string());
        bail!("{context}: {err}")
    }
}

fn service_summary(svc: &Service, running: bool, address: Option<String>) -> Value {
    json!({
        "id": svc.id,
        "hostname": svc.hostname,
        "enabled": svc.enabled,
        "running": running,
        "image": svc.image,
        "cpu": svc.cpu,
        "memory_mib": svc.memory_mib,
        "disk_gib": svc.disk_gib,
        "vsock_cid": svc.vsock_cid,
        "mac": svc.mac,
        "address": address,
        "agent": svc.agent,
        "ssh_access": svc.provision.ssh_access_state(),
        "ssh_key_fingerprints": svc.provision.ssh_key_fingerprints(),
    })
}

/// Validate new forwards as one set before changing a disk or service record.
/// This covers both `create`/`spawn` batches and live `publish add` calls.
fn validate_publish_candidates(
    reg: &Registry,
    service: &str,
    candidates: &[Publish],
) -> Result<()> {
    let mut names: Vec<String> = reg
        .services
        .get(service)
        .into_iter()
        .flat_map(|svc| svc.publish.iter().map(Publish::effective_name))
        .collect();

    for (index, candidate) in candidates.iter().enumerate() {
        candidate
            .validate()
            .map_err(|err| coded("publish.invalid", format!("{err:#}")))?;
        let name = candidate.effective_name();
        if names.iter().any(|existing| existing == &name) {
            return Err(coded(
                "publish.name_exists",
                format!("service {service} already has a publish named {name}"),
            ));
        }

        for other in reg.services.values() {
            if let Some(clash) = other
                .publish
                .iter()
                .find(|publish| candidate.conflicts_with(publish))
            {
                return Err(publish_port_conflict(candidate, &other.hostname, clash));
            }
        }
        if let Some(clash) = candidates[..index]
            .iter()
            .find(|publish| candidate.conflicts_with(publish))
        {
            return Err(publish_port_conflict(candidate, service, clash));
        }
        names.push(name);
    }
    Ok(())
}

fn publish_port_conflict(candidate: &Publish, service: &str, clash: &Publish) -> anyhow::Error {
    coded(
        "publish.host_port_in_use",
        format!(
            "host port {}/{} overlaps a publish by {} ({})",
            candidate.host_port,
            candidate.protocol,
            service,
            clash.effective_name()
        ),
    )
}

async fn check_authorized_keys_file(path: &Utf8PathBuf) -> Value {
    match fs::read_to_string(path).await {
        Ok(text) => match crate::ssh::parse_authorized_keys(&text, path.as_str()) {
            Ok(keys) => json!({
                "name": "authorized_keys",
                "path": path,
                "ok": !keys.is_empty(),
                "keys": keys.len(),
            }),
            Err(err) => json!({
                "name": "authorized_keys",
                "path": path,
                "ok": false,
                "error": format!("{err:#}"),
            }),
        },
        Err(err) => json!({
            "name": "authorized_keys",
            "path": path,
            "ok": false,
            "error": err.to_string(),
        }),
    }
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

fn check_character_device(name: &str, path: &Utf8PathBuf) -> Value {
    use std::os::unix::fs::FileTypeExt;

    let ok = std::fs::metadata(path)
        .map(|metadata| metadata.file_type().is_char_device())
        .unwrap_or(false);
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

async fn check_command_version(command: &str) -> Value {
    let path = std::env::var_os("PATH").and_then(|path| {
        std::env::split_paths(&path)
            .map(|dir| dir.join(command))
            .find(|candidate| candidate.is_file())
    });
    let Some(path) = path else {
        return json!({
            "name": format!("command:{command}"),
            "command": command,
            "ok": false,
            "error": "not found in PATH",
        });
    };
    match tokio::process::Command::new(&path)
        .arg("--version")
        .output()
        .await
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout);
            let stderr = String::from_utf8_lossy(&output.stderr);
            let version = stdout
                .lines()
                .chain(stderr.lines())
                .find(|line| !line.trim().is_empty())
                .unwrap_or("version output was empty")
                .trim();
            json!({
                "name": format!("command:{command}"),
                "command": command,
                "path": path,
                "version": version,
                "ok": true,
            })
        }
        Ok(output) => json!({
            "name": format!("command:{command}"),
            "command": command,
            "path": path,
            "ok": false,
            "error": format!("--version exited with {}", output.status),
        }),
        Err(err) => json!({
            "name": format!("command:{command}"),
            "command": command,
            "path": path,
            "ok": false,
            "error": err.to_string(),
        }),
    }
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

/// Fail fast, daemon-side and before CHV is spawned, if an image cannot boot
/// with the configured guest kernel. A clear `start` error beats a kernel panic
/// (`Unable to mount root fs`) or a busybox shell on serial. Lives here (not in
/// RealHost) so FakeHost tests exercise it.
pub async fn validate_boot_prerequisites(cfg: &Config, manifest: &ImageManifest) -> Result<()> {
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
    let unit = unit_name(&svc.id);
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

/// Read leases for the change watcher. A missing file is the valid empty state;
/// other errors retain the last good routing state and are retried.
async fn read_leases_checked(cfg: &Config) -> Result<Vec<net::Lease>> {
    match fs::read_to_string(&cfg.lease_file).await {
        Ok(text) => Ok(net::parse_leases(&text)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(err) => Err(err).with_context(|| format!("read lease file {}", cfg.lease_file)),
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
    let static_ip = reg.allocations.ips.get(&svc.id).map(|s| s.as_str());
    net::resolve_address(lease_ip, static_ip).map(|(ip, source)| (ip.to_string(), source))
}

/// Fully rewrite the `hearth_nat` table from the registry. Shared by the
/// per-operation path (`Daemon::rewrite_nat`) and startup reconcile. Warns and
/// continues on any failure.
async fn apply_nat<H: Host>(cfg: &Config, host: &H, reg: &Registry) {
    let leases = read_leases(cfg).await;
    if let Err(err) = apply_nat_with_leases(host, reg, &leases).await {
        warn!(error = %err, "failed to apply nft hearth_nat table");
    }
}

async fn apply_nat_with_leases<H: Host>(
    host: &H,
    reg: &Registry,
    leases: &[net::Lease],
) -> Result<()> {
    let targets: Vec<PublishTarget> = reg
        .services
        .values()
        .map(|svc| PublishTarget {
            service: svc.hostname.clone(),
            address: resolved_address(reg, leases, svc).map(|(ip, _)| ip),
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
    host.nft_apply(&ruleset.text)
        .await
        .context("apply nft hearth_nat table")
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
        let Some(ip) = reg.allocations.ips.get(&svc.id) else {
            continue;
        };
        let path = dir.join(format!("{}.conf", svc.id));
        if path.exists() {
            continue;
        }
        warn!(hostname = %svc.hostname, id = %svc.id, "re-writing missing dnsmasq drop-in");
        match fs::write(&path, net::dhcp_host_line(&svc.mac, ip, &svc.hostname)).await {
            Ok(()) => wrote_any = true,
            Err(err) => {
                warn!(hostname = %svc.hostname, id = %svc.id, error = %err, "failed to re-write dnsmasq drop-in")
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
        let unit = unit_name(&svc.id);
        let running = host
            .systemctl(&["is-active", &unit])
            .await
            .map(|s| s.trim() == "active")
            .unwrap_or(false);
        if !running {
            warn!(hostname = %svc.hostname, id = %svc.id, "enabled service is not active; starting");
            // Warn-and-continue like the nft/dnsmasq self-heal below. If image
            // load, boot-prerequisite validation, or the VM launch fails (e.g.
            // a guest kernel wiped or bumped out of contract after a reboot),
            // one bad service must not abort reconcile: that would leave systemd
            // crash-looping the daemon so it never binds its socket, and would
            // skip the networking self-heal for every other service.
            if let Err(err) = start_enabled_service(cfg, host, svc).await {
                warn!(
                    hostname = %svc.hostname,
                    id = %svc.id,
                    error = %err,
                    "failed to start enabled service during reconcile; leaving it down for operator action"
                );
            }
        } else if boot_config_state(cfg, host, svc).await == Some("stale") {
            // The running unit was booted with flags that differ from what we
            // would launch now (older daemon, changed kernel/cmdline). Surface
            // it instead of silently adopting; restart stays the operator's call.
            warn!(
                hostname = %svc.hostname,
                id = %svc.id,
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
        &cfg.snapshots_dir,
        &cfg.dnsmasq_dropin_dir,
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
    use crate::testing::FakeHost;
    use camino::Utf8Path;
    use clap::Parser;

    const TEST_AUTHORIZED_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIPEVBr+XtUOuloYyDWGTcKPPHbVwpSIATl/mJ6RE7gdN hearth-test";

    #[test]
    fn image_argv_uses_direct_kernel_boot() {
        let cfg = Config::parse_from([
            "hearthd",
            "--guest-kernel",
            "/run/booted-system/kernel",
            "--guest-initramfs",
            "/var/lib/hearth/initramfs.cpio.gz",
        ]);
        let svc = Service {
            id: "vm-00000000000000000000000000000001".into(),
            hostname: "dev".into(),
            enabled: false,
            image: "exeuntu".into(),
            cpu: 4,
            memory_mib: 4096,
            disk_gib: 40,
            vsock_cid: 100,
            mac: "52:54:00:12:34:56".into(),
            agent: false,
            disk: Some("dev.qcow2".into()),
            publish: Vec::new(),
            provision: Provision::default(),
            restart: RestartPolicy::default(),
        };
        let manifest = hearth_proto::ImageManifest::from_oci_process(hearth_proto::OciProcess {
            args: vec!["/usr/local/bin/init".to_string()],
            env: vec!["EXEUNTU=1".to_string()],
            cwd: "/home/exedev".to_string(),
        })
        .unwrap();
        let argv = cloud_hypervisor_argv(&cfg, &svc, &manifest).join(" ");

        assert!(argv.contains("--kernel /run/booted-system/kernel"));
        assert!(argv.contains("--initramfs /var/lib/hearth/initramfs.cpio.gz"));
        // The standalone qcow2 disk is provisioned via a raw scratch at create
        // time, and the filename must not lie.
        assert!(argv.contains("--disk path=/var/lib/hearth/disks/dev.qcow2"));
        assert!(argv.contains(
            "--cmdline console=ttyS0 root=/dev/vda rootfstype=ext4 rw init=/usr/local/bin/init"
        ));
        assert!(argv.contains("--memory size=4096M"));
        assert!(argv.contains("--balloon size=0,free_page_reporting=on"));
    }

    #[test]
    fn cloud_hypervisor_restore_argv_uses_restore_flag() {
        let cfg = Config::parse_from(["hearthd"]);
        let svc = Service {
            id: "vm-00000000000000000000000000000002".into(),
            hostname: "mail".into(),
            enabled: false,
            image: "debian".into(),
            cpu: 2,
            memory_mib: 2048,
            disk_gib: 20,
            vsock_cid: 100,
            mac: "52:54:00:12:34:56".into(),
            agent: false,
            disk: None,
            publish: Vec::new(),
            provision: Provision::default(),
            restart: RestartPolicy::default(),
        };
        let argv =
            cloud_hypervisor_restore_argv(&cfg, &svc, &Utf8PathBuf::from("/snap/mail/before"))
                .join(" ");
        assert!(
            argv.contains("--api-socket /run/hearth/vms/vm-00000000000000000000000000000002.sock")
        );
        assert!(argv.contains("--restore source_url=file:///snap/mail/before,resume=true"));
        assert!(argv
            .contains("--serial file=/var/log/hearth/vm-00000000000000000000000000000002.console"));
    }

    #[tokio::test]
    async fn registry_loads_service_toml() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let services = root.join("services");
        tokio::fs::create_dir_all(&services).await.unwrap();
        tokio::fs::write(
            services.join(format!("{}.toml", test_id("mail"))),
            service_toml("mail", true, 100, "52:54:00:12:34:56"),
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
            services.join(format!("{}.toml", test_id("mail"))),
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
            services.join(format!("{}.toml", test_id("mail"))),
            service_toml("mail", false, 100, "52:54:00:00:00:01"),
        )
        .await
        .unwrap();
        tokio::fs::write(
            log_dir.join(format!("{}.console", test_id("mail"))),
            "first\nsecond\n",
        )
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
        assert!(calls.contains(&format!(
            "systemctl is-active hearth-vm-{}.service",
            test_id("mail")
        )));
        assert!(calls.contains(&format!("systemd-run {}", test_id("mail"))));
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
        let id = test_id("mail");
        for port in [PORT_REPORT, PORT_AGENT] {
            let path = cfg.vm_vsock_port_socket(&id, port);
            tokio::fs::create_dir_all(path.parent().unwrap())
                .await
                .unwrap();
            tokio::fs::write(path, b"listener").await.unwrap();
        }
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
        assert!(!cfg.vm_vsock_port_socket(&id, PORT_REPORT).exists());
        assert!(
            cfg.vm_vsock_port_socket(&id, PORT_AGENT).exists(),
            "stop must retain agentd's listener for the next start"
        );
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
    async fn create_from_image_builds_provisioned_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.images_dir).await.unwrap();
        tokio::fs::write(cfg.image_path("exeuntu"), b"base")
            .await
            .unwrap();
        tokio::fs::write(cfg.image_manifest_path("exeuntu"), image_manifest_toml())
            .await
            .unwrap();
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new(
            "1",
            Verb::Create,
            Map::from_iter([
                ("hostname".into(), json!("dev")),
                ("image".into(), json!("exeuntu")),
                ("disk_gib".into(), json!(40)),
            ]),
        );

        let responses = daemon.handle(req).await;

        assert!(responses[0].ok);
        let registry = Registry::load(&cfg).await.unwrap();
        let dev = registry.get("dev").unwrap();
        let calls = state.lock().unwrap().calls.clone();
        // The qcow2 boot disk is built via a provisioned raw scratch.
        // Provisioning defaults apply (hostname = service hostname, machine-id reset).
        assert!(calls.iter().any(|call| {
            call.starts_with("build-vm-disk ")
                && call.contains(&format!("{}.qcow2", dev.id))
                && call.contains("scratch=")
                && call.contains(&format!("{}.raw", dev.id))
                && call.contains("reset_machine_id=true")
                && call.contains("hostname=dev")
        }));
        assert_eq!(dev.image, "exeuntu");
        assert_eq!(dev.disk_gib, 40);
        assert_eq!(
            dev.disk.as_deref(),
            Some(format!("{}.qcow2", dev.id).as_str())
        );
        assert_eq!(dev.provision.hostname, "dev");
        assert_eq!(dev.provision.ssh_access_state(), "configured");
        assert_eq!(dev.provision.authorized_keys, vec![TEST_AUTHORIZED_KEY]);
    }

    #[tokio::test]
    async fn create_rejects_keyless_vm_before_disk_work() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        tokio::fs::remove_file(&cfg.authorized_keys_file)
            .await
            .unwrap();
        write_test_image(&cfg, "base").await;
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg, host);

        let responses = daemon
            .handle(Request::new("1", Verb::Create, create_args("keyless")))
            .await;

        assert!(!responses[0].ok);
        assert_eq!(
            responses[0].error.as_ref().unwrap().code,
            "ssh.authorized_keys_required"
        );
        assert!(!state
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|call| call.starts_with("build-vm-disk ")));
    }

    #[tokio::test]
    async fn create_explicit_no_ssh_is_persisted_and_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        tokio::fs::remove_file(&cfg.authorized_keys_file)
            .await
            .unwrap();
        write_test_image(&cfg, "base").await;
        let daemon = Daemon::new(cfg.clone(), FakeHost::default());
        let mut args = create_args("keyless");
        args.insert("provision".into(), json!({ "allow_no_ssh": true }));

        let responses = daemon.handle(Request::new("1", Verb::Create, args)).await;

        assert!(responses[0].ok);
        assert_eq!(
            responses[0].result.as_ref().unwrap()["created"]["ssh_access"],
            json!("intentionally-disabled")
        );
        let registry = Registry::load(&cfg).await.unwrap();
        assert!(registry.get("keyless").unwrap().provision.allow_no_ssh);
    }

    #[tokio::test]
    async fn create_merges_and_deduplicates_host_and_request_keys() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        write_test_image(&cfg, "base").await;
        let daemon = Daemon::new(cfg.clone(), FakeHost::default());
        let mut args = create_args("deduped");
        args.insert(
            "provision".into(),
            json!({
                "authorized_keys": [TEST_AUTHORIZED_KEY.replace("hearth-test", "request-comment")],
                "allow_no_ssh": true,
            }),
        );

        let responses = daemon.handle(Request::new("1", Verb::Create, args)).await;

        assert!(responses[0].ok);
        let registry = Registry::load(&cfg).await.unwrap();
        let provision = &registry.get("deduped").unwrap().provision;
        assert_eq!(provision.authorized_keys, vec![TEST_AUTHORIZED_KEY]);
        assert!(!provision.allow_no_ssh);
        assert_eq!(provision.ssh_key_fingerprints().len(), 1);
    }

    #[tokio::test]
    async fn create_rejects_malformed_request_key() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        write_test_image(&cfg, "base").await;
        let daemon = Daemon::new(cfg, FakeHost::default());
        let mut args = create_args("invalid-key");
        args.insert(
            "provision".into(),
            json!({ "authorized_keys": ["ssh-ed25519 not-base64"] }),
        );

        let responses = daemon.handle(Request::new("1", Verb::Create, args)).await;

        assert!(!responses[0].ok);
        assert_eq!(
            responses[0].error.as_ref().unwrap().code,
            "ssh.authorized_keys_invalid"
        );
    }

    #[tokio::test]
    async fn host_check_reports_missing_recovery_keyring() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        tokio::fs::remove_file(&cfg.authorized_keys_file)
            .await
            .unwrap();
        let daemon = Daemon::new(cfg, FakeHost::default());

        let responses = daemon
            .handle(Request::new("1", Verb::HostCheck, Map::new()))
            .await;

        assert!(responses[0].ok);
        let checks = responses[0].result.as_ref().unwrap()["checks"]
            .as_array()
            .unwrap();
        let keys = checks
            .iter()
            .find(|check| check["name"] == "authorized_keys")
            .unwrap();
        assert_eq!(keys["ok"], json!(false));
        assert!(keys["error"].as_str().unwrap().contains("No such file"));
    }

    #[test]
    fn character_device_check_accepts_device_nodes_and_rejects_regular_files() {
        assert_eq!(
            check_character_device("device", &Utf8PathBuf::from("/dev/null"))["ok"],
            json!(true)
        );

        let tmp = tempfile::NamedTempFile::new().unwrap();
        let path = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        assert_eq!(check_character_device("device", &path)["ok"], json!(false));
    }

    #[tokio::test]
    async fn create_applies_provision_files_and_hostname() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.images_dir).await.unwrap();
        tokio::fs::write(cfg.image_path("exeuntu"), b"base")
            .await
            .unwrap();
        tokio::fs::write(cfg.image_manifest_path("exeuntu"), image_manifest_toml())
            .await
            .unwrap();
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new(
            "1",
            Verb::Create,
            Map::from_iter([
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
            .find(|call| call.starts_with("build-vm-disk "))
            .expect("build-vm-disk call recorded");
        assert!(provision_call.contains("/home/agent/.hermes/.env<-<literal>:0600:1000:1000"));
        assert!(provision_call.contains("reset_ssh_hostkeys=true"));
        assert!(provision_call.contains("hostname=hermes-a"));
        // The literal secret must never appear in a recorded/emitted call.
        assert!(!provision_call.contains("secret"));

        // Persisted, and status redacts the literal content.
        let registry = Registry::load(&cfg).await.unwrap();
        let svc = registry.get("hermes-a").unwrap();
        assert_eq!(svc.provision.hostname, "hermes-a");
        assert!(svc.provision.reset_ssh_hostkeys);
        assert_eq!(svc.provision.files.len(), 1);
        assert_eq!(svc.provision.files[0].from_literal, "TOKEN=secret");
        let status = daemon
            .handle(Request::new("2", Verb::Status, name_args("hermes-a")))
            .await;
        let value = status[0].result.as_ref().unwrap();
        let rendered = value["provision"].to_string();
        assert!(rendered.contains("<literal>"));
        assert!(!rendered.contains("TOKEN=secret"));
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
        write_test_image(&cfg, "base").await;
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);

        let responses = daemon
            .handle(Request::new("1", Verb::Create, create_args("web")))
            .await;

        assert!(responses[0].ok, "create failed: {:?}", responses[0].error);
        // The static IP is recorded next to CID/MAC and returned as the address.
        let registry = Registry::load(&cfg).await.unwrap();
        let web_id = &registry.get("web").unwrap().id;
        let ip = registry.allocations.ips.get(web_id).cloned();
        assert_eq!(ip.as_deref(), Some("10.26.8.16"));
        assert_eq!(
            responses[0].result.as_ref().unwrap()["created"]["address"],
            json!("10.26.8.16")
        );
        // The drop-in file was written and dnsmasq was SIGHUP'd.
        let dropin = cfg.dnsmasq_dropin_dir.join(format!("{web_id}.conf"));
        let contents = tokio::fs::read_to_string(&dropin).await.unwrap();
        let mac = registry.allocations.macs.get(web_id).unwrap();
        assert_eq!(contents, format!("dhcp-host={mac},10.26.8.16,web\n"));
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.contains(&"reload-dnsmasq".to_string()));
        // The NAT table is (re)applied even with no publishes.
        assert!(calls.contains(&"nft-apply".to_string()));
    }

    #[tokio::test]
    async fn rename_changes_hostname_without_changing_machine_identity() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        tokio::fs::create_dir_all(&cfg.dnsmasq_dropin_dir)
            .await
            .unwrap();
        write_test_image(&cfg, "base").await;
        let daemon = Daemon::new(cfg.clone(), FakeHost::default());

        let created = daemon
            .handle(Request::new("1", Verb::Create, create_args("web")))
            .await;
        assert!(created[0].ok, "create failed: {:?}", created[0].error);
        let before = Registry::load(&cfg).await.unwrap();
        let id = before.get("web").unwrap().id.clone();
        let cid = before.allocations.vsock_cids.get(&id).copied();
        let mac = before.allocations.macs.get(&id).cloned();
        let ip = before.allocations.ips.get(&id).cloned();

        let renamed = daemon
            .handle(Request::new(
                "2",
                Verb::Rename,
                Map::from_iter([
                    ("name".to_string(), json!("web")),
                    ("hostname".to_string(), json!("api")),
                ]),
            ))
            .await;
        assert!(renamed[0].ok, "rename failed: {:?}", renamed[0].error);
        assert_eq!(renamed[0].result.as_ref().unwrap()["id"], json!(id));
        assert_eq!(
            renamed[0].result.as_ref().unwrap()["guest_hostname_updated"],
            Value::Null
        );

        let after = Registry::load(&cfg).await.unwrap();
        assert!(after.get("web").is_err());
        let api = after.get("api").unwrap();
        assert_eq!(api.id, id);
        assert_eq!(api.provision.hostname, "api");
        assert_eq!(after.allocations.vsock_cids.get(&id).copied(), cid);
        assert_eq!(after.allocations.macs.get(&id), mac.as_ref());
        assert_eq!(after.allocations.ips.get(&id), ip.as_ref());
        assert!(cfg.services_dir.join(format!("{id}.toml")).exists());
        assert_eq!(
            tokio::fs::read_to_string(cfg.dnsmasq_dropin_dir.join(format!("{id}.conf")))
                .await
                .unwrap(),
            format!(
                "dhcp-host={},{},api\n",
                mac.as_deref().unwrap(),
                ip.as_deref().unwrap()
            )
        );
    }

    #[tokio::test]
    async fn create_skips_dropin_when_dir_absent_but_still_allocates_ip() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        // Deliberately do NOT create the drop-in dir (dev host without managed
        // dnsmasq): create must still succeed and skip the reservation.
        write_test_image(&cfg, "base").await;
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);

        let responses = daemon
            .handle(Request::new("1", Verb::Create, create_args("web")))
            .await;

        assert!(responses[0].ok, "create failed: {:?}", responses[0].error);
        let registry = Registry::load(&cfg).await.unwrap();
        let web_id = &registry.get("web").unwrap().id;
        assert!(!cfg
            .dnsmasq_dropin_dir
            .join(format!("{web_id}.conf"))
            .exists());
        let calls = state.lock().unwrap().calls.clone();
        // No drop-in written -> no dnsmasq reload, but the IP is still reserved.
        assert!(!calls.contains(&"reload-dnsmasq".to_string()));
        assert_eq!(
            registry.allocations.ips.get(web_id).map(String::as_str),
            Some("10.26.8.16")
        );
    }

    #[tokio::test]
    async fn create_with_publish_renders_dnat_rules_and_persists_them() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        write_test_image(&cfg, "base").await;
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new(
            "1",
            Verb::Create,
            Map::from_iter([
                ("hostname".into(), json!("web")),
                ("image".into(), json!("base")),
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
    async fn lease_change_retargets_published_port() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        ensure_dirs(&cfg).await.unwrap();
        write_test_image(&cfg, "base").await;
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let create = daemon
            .handle(Request::new(
                "1",
                Verb::Create,
                Map::from_iter([
                    ("hostname".into(), json!("web")),
                    ("image".into(), json!("base")),
                    (
                        "publish".into(),
                        json!([{ "host_port": 9119, "guest_port": 9119, "protocol": "tcp" }]),
                    ),
                ]),
            ))
            .await;
        assert!(create[0].ok, "create failed: {:?}", create[0].error);
        let registry = Registry::load(&cfg).await.unwrap();
        let mac = registry.get("web").unwrap().mac.clone();
        let mut previous = None;

        tokio::fs::write(&cfg.lease_file, format!("1 {mac} 10.26.8.99 web *\n"))
            .await
            .unwrap();
        assert!(daemon
            .refresh_nat_for_lease_change(&mut previous)
            .await
            .unwrap());
        assert!(state
            .lock()
            .unwrap()
            .last_nft
            .as_ref()
            .unwrap()
            .contains("dnat to 10.26.8.99:9119"));

        tokio::fs::write(&cfg.lease_file, format!("2 {mac} 10.26.8.16 web *\n"))
            .await
            .unwrap();
        assert!(daemon
            .refresh_nat_for_lease_change(&mut previous)
            .await
            .unwrap());
        let applies = {
            let locked = state.lock().unwrap();
            let rules = locked.last_nft.as_ref().unwrap();
            assert!(rules.contains("dnat to 10.26.8.16:9119"));
            assert!(!rules.contains("dnat to 10.26.8.99:9119"));
            locked
                .calls
                .iter()
                .filter(|call| call.as_str() == "nft-apply")
                .count()
        };

        assert!(!daemon
            .refresh_nat_for_lease_change(&mut previous)
            .await
            .unwrap());
        assert_eq!(
            state
                .lock()
                .unwrap()
                .calls
                .iter()
                .filter(|call| call.as_str() == "nft-apply")
                .count(),
            applies
        );
    }

    #[tokio::test]
    async fn publish_add_and_remove_apply_nat_live_by_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        write_test_image(&cfg, "base").await;
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);

        let created = daemon
            .handle(Request::new("1", Verb::Create, create_args("web")))
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

        // An all-address bind also conflicts with a specific-address bind.
        let overlap = daemon
            .handle(Request::new(
                "4b",
                Verb::Publish,
                Map::from_iter([
                    ("name".into(), json!("web")),
                    (
                        "publish".into(),
                        json!({ "name": "loopback", "host_port": 8080, "guest_port": 82, "protocol": "tcp", "bind": "127.0.0.1" }),
                    ),
                ]),
            ))
            .await;
        assert_eq!(
            overlap[0].error.as_ref().map(|e| e.code.as_str()),
            Some("publish.host_port_in_use")
        );

        // Create/spawn uses the same registry-wide conflict check.
        let mut create = create_args("api");
        create.insert(
            "publish".into(),
            json!([{ "name": "api", "host_port": 8080, "guest_port": 8080, "protocol": "tcp", "bind": "127.0.0.1" }]),
        );
        let create_clash = daemon
            .handle(Request::new("4c", Verb::Create, create))
            .await;
        assert_eq!(
            create_clash[0]
                .error
                .as_ref()
                .map(|error| error.code.as_str()),
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
        write_test_image(&cfg, "base").await;
        let daemon = Daemon::new(cfg.clone(), FakeHost::default());
        // A forward created without a name (as `spawn --publish` produces).
        let created = daemon
            .handle(Request::new(
                "1",
                Verb::Create,
                Map::from_iter([
                    ("hostname".into(), json!("web")),
                    ("image".into(), json!("base")),
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
        write_test_image(&cfg, "base").await;
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg.clone(), host);
        let req = Request::new(
            "1",
            Verb::Create,
            Map::from_iter([
                ("hostname".into(), json!("web")),
                ("image".into(), json!("base")),
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
        assert!(registry.allocations.ips.is_empty());
    }

    #[tokio::test]
    async fn create_rejects_conflicting_publish_batch_before_disk_work() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        write_test_image(&cfg, "base").await;
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg, host);
        let mut args = create_args("web");
        args.insert(
            "publish".into(),
            json!([
                { "name": "public", "host_port": 8080, "guest_port": 80, "protocol": "tcp" },
                { "name": "loopback", "host_port": 8080, "guest_port": 81, "protocol": "tcp", "bind": "127.0.0.1" }
            ]),
        );

        let responses = daemon.handle(Request::new("1", Verb::Create, args)).await;

        assert_eq!(
            responses[0].error.as_ref().map(|error| error.code.as_str()),
            Some("publish.host_port_in_use")
        );
        assert!(!state
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|call| call.starts_with("build-vm-disk ")));
    }

    #[tokio::test]
    async fn create_rejects_daemon_side_provision_source_before_disk_work() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        write_test_image(&cfg, "base").await;
        let host = FakeHost::default();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg, host);
        let mut args = create_args("web");
        args.insert(
            "provision".into(),
            json!({
                "files": [{
                    "source": "/etc/shadow",
                    "dest": "/home/agent/shadow",
                    "mode": "0600",
                    "owner": "1000:1000"
                }]
            }),
        );

        let responses = daemon.handle(Request::new("1", Verb::Create, args)).await;

        assert_eq!(
            responses[0].error.as_ref().map(|error| error.code.as_str()),
            Some("provision.invalid")
        );
        assert!(responses[0]
            .error
            .as_ref()
            .unwrap()
            .message
            .contains("unknown field `source`"));
        assert!(!state
            .lock()
            .unwrap()
            .calls
            .iter()
            .any(|call| call.starts_with("build-vm-disk ")));
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
                vsock_cids: std::iter::once((test_id("mail"), 100)).collect(),
                macs: std::iter::once((test_id("mail"), "52:54:00:00:00:01".to_string())).collect(),
                ips: std::iter::once((test_id("mail"), "10.26.8.16".to_string())).collect(),
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
    async fn ls_includes_reported_guestd_version() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", false).await;
        let daemon = Daemon::new(test_config(&root), FakeHost::default());
        daemon.guests.update_report(
            &test_id("mail"),
            Some(&hearth_agent_proto::Hello::new("guestd", "0.1.0+3af0907")),
            hearth_agent_proto::BootReport {
                ready: true,
                ..Default::default()
            },
        );

        let ls = daemon.handle(Request::new("1", Verb::Ls, Map::new())).await;
        let service = &ls[0].result.as_ref().unwrap()["services"][0];
        assert_eq!(service["guestd"]["version"], json!("0.1.0+3af0907"));
        assert_eq!(service["guestd"]["connected"], json!(true));
    }

    #[tokio::test]
    async fn probes_guestd_version_over_the_direct_guest_channel() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let cfg = test_config(&root);
        let id = "vm-00000000000000000000000000000001";
        let socket = cfg.vm_vsock_socket(id);
        tokio::fs::create_dir_all(socket.parent().unwrap())
            .await
            .unwrap();
        let listener = UnixListener::bind(socket.as_str()).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert_eq!(
                hybrid::accept_handshake(&mut stream).await.unwrap(),
                PORT_GUESTD
            );
            let hello = read_line_capped(&mut stream, MAX_LINE_BYTES)
                .await
                .unwrap()
                .unwrap();
            let hello: Hello = serde_json::from_str(&hello).unwrap();
            assert_eq!(hello.component, "agentd");
            stream
                .write_all(
                    (serde_json::to_string(&Response::success(
                        "hello",
                        json!({"proto": hearth_agent_proto::AGENT_PROTOCOL_VERSION}),
                    ))
                    .unwrap()
                        + "\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
            let request = read_line_capped(&mut stream, MAX_LINE_BYTES)
                .await
                .unwrap()
                .unwrap();
            let request: AgentRequest = serde_json::from_str(&request).unwrap();
            assert!(matches!(request.verb, AgentVerb::Version));
            stream
                .write_all(
                    (serde_json::to_string(&Response::success(
                        request.id,
                        json!({"version": "0.1.0+3af0907"}),
                    ))
                    .unwrap()
                        + "\n")
                        .as_bytes(),
                )
                .await
                .unwrap();
        });
        let daemon = Daemon::new(cfg, FakeHost::default());

        assert_eq!(
            daemon.probe_guestd_version(id).await.unwrap(),
            "0.1.0+3af0907"
        );
        server.await.unwrap();
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
        tokio::fs::write(
            cfg.dnsmasq_dropin_dir
                .join(format!("{}.conf", test_id("mail"))),
            "dhcp-host=x\n",
        )
        .await
        .unwrap();
        Registry::write_allocations(
            &cfg,
            &Allocations {
                vsock_cids: std::iter::once((test_id("mail"), 100)).collect(),
                macs: std::iter::once((test_id("mail"), "52:54:00:00:00:01".to_string())).collect(),
                ips: std::iter::once((test_id("mail"), "10.26.8.16".to_string())).collect(),
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
        assert!(!cfg
            .dnsmasq_dropin_dir
            .join(format!("{}.conf", test_id("mail")))
            .exists());
        let registry = Registry::load(&cfg).await.unwrap();
        assert!(!registry.allocations.ips.contains_key(&test_id("mail")));
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
                vsock_cids: std::iter::once((test_id("mail"), 100)).collect(),
                macs: std::iter::once((test_id("mail"), "52:54:00:00:00:01".to_string())).collect(),
                ips: std::iter::once((test_id("mail"), "10.26.8.16".to_string())).collect(),
            },
        )
        .await
        .unwrap();
        let host = FakeHost::default();
        let state = host.state.clone();

        reconcile(&cfg, &host).await.unwrap();

        let dropin = cfg
            .dnsmasq_dropin_dir
            .join(format!("{}.conf", test_id("mail")));
        assert_eq!(
            tokio::fs::read_to_string(&dropin).await.unwrap(),
            "dhcp-host=52:54:00:00:00:01,10.26.8.16,mail\n"
        );
        let calls = state.lock().unwrap().calls.clone();
        assert!(calls.contains(&"reload-dnsmasq".to_string()));
        assert!(calls.contains(&"nft-apply".to_string()));
    }

    #[tokio::test]
    async fn reconcile_survives_service_with_failed_boot_prerequisites() {
        // An enabled service whose guest kernel is absent (fresh
        // deploy / wiped kernels dir) must not abort reconcile: the daemon has
        // to keep booting, skip only the broken service, and still self-heal
        // host networking for the rest.
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_image_service(&root, "dev", true, image_manifest_toml()).await;
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
    async fn start_boots_when_kernel_present_and_contract_satisfied() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_image_service(&root, "dev", false, image_manifest_toml()).await;
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
        assert!(calls.contains(&format!("systemd-run {}", test_id("dev"))));
    }

    #[tokio::test]
    async fn start_fails_without_guest_kernel() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_image_service(&root, "dev", false, image_manifest_toml()).await;
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
    async fn start_fails_when_contract_too_old() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        let manifest = r#"
version = 1
root_device = "/dev/vda"
root_fstype = "ext4"
init = "/usr/local/bin/init"
min_kernel_contract = 2

[oci]
args = ["/usr/local/bin/init"]
env = ["EXEUNTU=1"]
cwd = "/home/exedev"
"#;
        write_image_service(&root, "dev", false, manifest).await;
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
        write_image_service(&root, "dev", false, image_manifest_toml()).await;
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
        write_image_service(&root, "dev", false, image_manifest_toml()).await;
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
        tokio::fs::create_dir_all(&cfg.log_dir).await.unwrap();
        let id = test_id("mail");
        let agent_socket = cfg.vm_vsock_port_socket(&id, PORT_AGENT);
        tokio::fs::create_dir_all(agent_socket.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&agent_socket, b"listener").await.unwrap();
        tokio::fs::create_dir_all(cfg.snapshots_dir.join(&id))
            .await
            .unwrap();
        tokio::fs::write(cfg.disk_path_ext(&id, "qcow2"), b"disk")
            .await
            .unwrap();
        tokio::fs::write(cfg.console_path(&id), b"log")
            .await
            .unwrap();
        Registry::write_allocations(
            &cfg,
            &Allocations {
                vsock_cids: std::iter::once((id.clone(), 100)).collect(),
                macs: std::iter::once((id.clone(), "52:54:00:00:00:01".to_string())).collect(),
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
        assert!(!cfg.disk_path_ext(&id, "qcow2").exists());
        assert!(!cfg.console_path(&id).exists());
        assert!(!cfg.snapshots_dir.join(&id).exists());
        assert!(!cfg.services_dir.join(format!("{id}.toml")).exists());
        assert!(!agent_socket.exists());
        let registry = Registry::load(&cfg).await.unwrap();
        assert!(!registry.allocations.vsock_cids.contains_key(&id));
        assert!(!registry.allocations.macs.contains_key(&id));
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
        assert!(cfg.snapshot_dir(&test_id("mail"), "before").is_dir());
        let calls = state.lock().unwrap().calls.clone();
        // CHV refuses a running-VM snapshot: pause, dump, resume, in order.
        let pause = calls
            .iter()
            .position(|c| c == "chv-put /api/v1/vm.pause (empty)")
            .expect("vm.pause called");
        let snapshot = calls
            .iter()
            .position(|call| {
                call == &format!(
                    "chv-put /api/v1/vm.snapshot {{\"destination_url\":\"file://{}\"}}",
                    cfg.snapshot_dir(&test_id("mail"), "before")
                )
            })
            .expect("vm.snapshot called");
        let resume = calls
            .iter()
            .position(|c| c == "chv-put /api/v1/vm.resume (empty)")
            .expect("vm.resume called");
        // The boot disk is captured in the same paused window as the memory
        // dump; without it the snapshot is not restorable.
        let disk = calls
            .iter()
            .position(|c| c.starts_with("copy-disk") && c.contains(SNAPSHOT_DISK_FILE))
            .expect("boot disk captured");
        assert!(pause < snapshot && snapshot < disk && disk < resume);

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
    async fn failed_snapshot_still_resumes_the_vm() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", true).await;
        let cfg = test_config(&root);
        let host = FakeHost::running();
        host.state.lock().unwrap().chv_fail = Some("/api/v1/vm.snapshot".into());
        let state = host.state.clone();
        let daemon = Daemon::new(cfg, host);

        let responses = daemon
            .handle(Request::new(
                "1",
                Verb::Snapshot,
                Map::from_iter([("name".into(), json!("mail")), ("tag".into(), json!("bad"))]),
            ))
            .await;

        assert!(!responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        let snapshot = calls
            .iter()
            .position(|c| c.starts_with("chv-put /api/v1/vm.snapshot"))
            .expect("vm.snapshot attempted");
        let resume = calls
            .iter()
            .position(|c| c == "chv-put /api/v1/vm.resume (empty)")
            .expect("vm.resume still called");
        assert!(snapshot < resume);
        // No point copying a disk for a state dump that failed.
        assert!(!calls.iter().any(|c| c.starts_with("copy-disk")));
    }

    #[tokio::test]
    async fn failed_disk_copy_still_resumes_the_vm() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", true).await;
        let cfg = test_config(&root);
        let host = FakeHost::running();
        host.state.lock().unwrap().copy_fail = true;
        let state = host.state.clone();
        let daemon = Daemon::new(cfg, host);

        let responses = daemon
            .handle(Request::new(
                "1",
                Verb::Snapshot,
                Map::from_iter([("name".into(), json!("mail")), ("tag".into(), json!("bad"))]),
            ))
            .await;

        assert!(!responses[0].ok);
        let calls = state.lock().unwrap().calls.clone();
        let copy = calls
            .iter()
            .position(|c| c.starts_with("copy-disk"))
            .expect("disk copy attempted");
        let resume = calls
            .iter()
            .position(|c| c == "chv-put /api/v1/vm.resume (empty)")
            .expect("vm.resume still called");
        assert!(copy < resume);
    }

    #[tokio::test]
    async fn restore_without_captured_disk_is_refused_before_stopping() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", true).await;
        let cfg = test_config(&root);
        // Snapshot dir exists but holds no captured boot disk (e.g. taken by a
        // pre-disk-capture daemon).
        tokio::fs::create_dir_all(cfg.snapshot_dir(&test_id("mail"), "old"))
            .await
            .unwrap();
        let host = FakeHost::running();
        let state = host.state.clone();
        let daemon = Daemon::new(cfg, host);

        let responses = daemon
            .handle(Request::new(
                "1",
                Verb::Restore,
                Map::from_iter([("name".into(), json!("mail")), ("tag".into(), json!("old"))]),
            ))
            .await;

        assert!(!responses[0].ok);
        assert_eq!(
            responses[0].error.as_ref().map(|err| err.code.as_str()),
            Some("snapshot.no_disk")
        );
        // The refusal must come before the VM is touched: no shutdown, no
        // restore relaunch.
        let calls = state.lock().unwrap().calls.clone();
        assert!(!calls
            .iter()
            .any(|c| c.contains("vm.shutdown") || c.starts_with("systemd-restore")));
    }

    #[tokio::test]
    async fn restore_stops_starts_chv_from_snapshot_and_marks_service_enabled() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Utf8PathBuf::from_path_buf(tmp.path().to_path_buf()).unwrap();
        write_service(&root, "mail", false).await;
        let cfg = test_config(&root);
        let snap_dir = cfg.snapshot_dir(&test_id("mail"), "before");
        tokio::fs::create_dir_all(&snap_dir).await.unwrap();
        tokio::fs::write(snap_dir.join(SNAPSHOT_DISK_FILE), b"disk")
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
        let restore_call = format!("systemd-restore {}", test_id("mail"));
        assert!(calls.iter().any(|call| call == &restore_call));
        // The captured disk must be copied back before CHV resumes the memory
        // image, or the guest resumes against a diverged rootfs.
        let copy_idx = calls
            .iter()
            .position(|call| call.starts_with("copy-disk") && call.contains(SNAPSHOT_DISK_FILE))
            .expect("captured disk copied back");
        let restore_idx = calls.iter().position(|call| call == &restore_call).unwrap();
        assert!(copy_idx < restore_idx);
        assert!(calls.iter().any(|call| call.starts_with("wait-socket ")));
        // Restore must refresh the NAT table *after* bringing the VM back up, in
        // case the resumed guest took a different lease than it held before.
        let restore_idx = calls.iter().position(|call| call == &restore_call).unwrap();
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
    async fn image_ls_returns_manifest_backed_images_sorted_with_hashes() {
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
        tokio::fs::write(cfg.image_manifest_path("zeta"), image_manifest_toml())
            .await
            .unwrap();
        tokio::fs::write(cfg.image_manifest_path("alpha"), image_manifest_toml())
            .await
            .unwrap();
        tokio::fs::write(cfg.images_dir.join("ignore.txt"), b"x")
            .await
            .unwrap();
        // One old/manual disk must not make every valid image undiscoverable.
        tokio::fs::write(cfg.images_dir.join("legacy.qcow2"), b"old")
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
        assert_eq!(images[0].get("bytes"), Some(&json!(1)));
        assert!(images[0]
            .get("sha256")
            .and_then(Value::as_str)
            .is_some_and(|hash| hash.len() == 64));
        let warnings = responses[0]
            .result
            .as_ref()
            .and_then(|result| result.get("warnings"))
            .and_then(Value::as_array)
            .unwrap();
        assert_eq!(warnings.len(), 1);
        assert_eq!(warnings[0].get("name"), Some(&json!("legacy")));
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
        tokio::fs::write(&source_manifest, image_manifest_toml())
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
        write_test_image(&cfg, "base").await;
        tokio::fs::write(cfg.image_path("unused"), b"unused")
            .await
            .unwrap();
        tokio::fs::write(cfg.image_manifest_path("unused"), image_manifest_toml())
            .await
            .unwrap();
        let daemon = Daemon::new(cfg.clone(), FakeHost::default());

        let referenced = daemon
            .handle(Request::new(
                "1",
                Verb::ImageRm,
                Map::from_iter([("name".into(), json!("base.qcow2"))]),
            ))
            .await;
        assert!(!referenced[0].ok);
        assert!(cfg.image_path("base").exists());

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

    fn image_manifest_toml() -> &'static str {
        r#"
version = 1
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
        let id = test_id(name);
        format!(
            r#"
id = "{id}"
hostname = "{name}"
enabled = {enabled}
image = "base"
cpu = 2
memory_mib = 2048
disk_gib = 20
vsock_cid = {cid}
mac = "{mac}"

[provision]
hostname = "{name}"

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
            services.join(format!("{}.toml", test_id(name))),
            service_toml(name, enabled, 100, "52:54:00:00:00:01"),
        )
        .await
        .unwrap();
        write_test_image(&test_config(root), "base").await;
        write_guest_kernel(root, "1").await;
    }

    fn name_args(name: &str) -> Map<String, Value> {
        Map::from_iter([("name".to_string(), json!(name))])
    }

    fn create_args(name: &str) -> Map<String, Value> {
        Map::from_iter([
            ("hostname".to_string(), json!(name)),
            ("image".to_string(), json!("base")),
        ])
    }

    fn test_id(name: &str) -> String {
        let value = name.bytes().fold(0u128, |value, byte| {
            value.wrapping_mul(257) ^ u128::from(byte)
        });
        format!("vm-{value:032x}")
    }

    async fn write_test_image(cfg: &Config, name: &str) {
        tokio::fs::create_dir_all(&cfg.images_dir).await.unwrap();
        tokio::fs::write(cfg.image_path(name), b"base")
            .await
            .unwrap();
        tokio::fs::write(cfg.image_manifest_path(name), image_manifest_toml())
            .await
            .unwrap();
    }

    fn test_config(root: &Utf8Path) -> Config {
        let authorized_keys = root.join("authorized_keys");
        std::fs::write(&authorized_keys, format!("{TEST_AUTHORIZED_KEY}\n")).unwrap();
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
            "--snapshots-dir",
            root.join("snapshots").as_str(),
            "--run-dir",
            root.join("run").as_str(),
            "--log-dir",
            root.join("log").as_str(),
            "--authorized-keys-file",
            authorized_keys.as_str(),
            "--guest-kernel",
            root.join("kernels/current/vmlinux").as_str(),
            "--lease-file",
            root.join("leases").as_str(),
            "--dnsmasq-dropin-dir",
            root.join("dnsmasq.d").as_str(),
            "--disable-vsock",
        ])
    }

    /// Write a service plus its image + sidecar manifest so a
    /// `start` exercises the guest-kernel validation path.
    async fn write_image_service(root: &Utf8Path, name: &str, enabled: bool, manifest_toml: &str) {
        let services = root.join("services");
        let id = test_id(name);
        tokio::fs::create_dir_all(&services).await.unwrap();
        tokio::fs::write(
            services.join(format!("{id}.toml")),
            format!(
                r#"
id = "{id}"
hostname = "{name}"
enabled = {enabled}
image = "exeuntu"
cpu = 2
memory_mib = 2048
disk_gib = 20
vsock_cid = 100
mac = "52:54:00:00:00:01"

[provision]
hostname = "{name}"

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
}
