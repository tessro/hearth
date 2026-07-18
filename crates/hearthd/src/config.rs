use crate::registry::Service;
use camino::Utf8PathBuf;
use clap::Parser;

#[derive(Debug, Clone, Parser)]
pub struct Config {
    #[arg(long, env = "HEARTH_SOCKET", default_value = "/run/hearth.sock")]
    pub socket: Utf8PathBuf,
    #[arg(
        long,
        env = "HEARTH_SERVICES_DIR",
        default_value = "/etc/hearth/services"
    )]
    pub services_dir: Utf8PathBuf,
    #[arg(
        long,
        env = "HEARTH_ALLOCATIONS",
        default_value = "/etc/hearth/allocations.toml"
    )]
    pub allocations: Utf8PathBuf,
    #[arg(
        long,
        env = "HEARTH_IMAGES_DIR",
        default_value = "/var/lib/hearth/images"
    )]
    pub images_dir: Utf8PathBuf,
    #[arg(
        long,
        env = "HEARTH_DISKS_DIR",
        default_value = "/var/lib/hearth/disks"
    )]
    pub disks_dir: Utf8PathBuf,
    #[arg(
        long,
        env = "HEARTH_SNAPSHOTS_DIR",
        default_value = "/var/lib/hearth/snapshots"
    )]
    pub snapshots_dir: Utf8PathBuf,
    #[arg(long, env = "HEARTH_RUN_DIR", default_value = "/run/hearth")]
    pub run_dir: Utf8PathBuf,
    #[arg(long, env = "HEARTH_LOG_DIR", default_value = "/var/log/hearth")]
    pub log_dir: Utf8PathBuf,
    /// Host-wide recovery keys added to every VM. A missing file contributes no
    /// keys; create still requires a per-VM key unless explicitly overridden.
    #[arg(
        long,
        env = "HEARTH_AUTHORIZED_KEYS_FILE",
        default_value = "/etc/hearth/authorized_keys"
    )]
    pub authorized_keys_file: Utf8PathBuf,
    #[arg(
        long,
        env = "HEARTH_GUEST_KERNEL",
        default_value = "/var/lib/hearth/kernels/current/vmlinux"
    )]
    pub guest_kernel: Utf8PathBuf,
    #[arg(long, env = "HEARTH_GUEST_INITRAMFS")]
    pub guest_initramfs: Option<Utf8PathBuf>,
    #[arg(long, env = "HEARTH_BRIDGE", default_value = "hearth0")]
    pub bridge: String,
    /// dnsmasq lease file, joined on the service MAC to report guest addresses
    /// (REFACTOR_PROPOSAL.md §4.1). A missing/unreadable file is not an error —
    /// the address is simply reported as null.
    #[arg(
        long,
        env = "HEARTH_LEASE_FILE",
        default_value = "/var/lib/dnsmasq/dnsmasq.leases"
    )]
    pub lease_file: Utf8PathBuf,
    /// dnsmasq drop-in dir where Hearth writes `<id>.conf` static-lease
    /// reservations (REFACTOR_PROPOSAL.md §4.2). If it is absent (a dev host
    /// without a Hearth-managed dnsmasq), reservations are skipped-with-warn and
    /// VMs fall back to dynamic DHCP.
    #[arg(
        long,
        env = "HEARTH_DNSMASQ_DROPIN_DIR",
        default_value = "/etc/dnsmasq.d/hearth"
    )]
    pub dnsmasq_dropin_dir: Utf8PathBuf,
    /// First IP of the static-lease slice Hearth assigns from. It MUST sit inside
    /// the `hearth0` bridge subnet (default gateway 10.26.8.1 → 10.26.8.0/24) and
    /// OUTSIDE dnsmasq's dynamic `dhcp-range`, or a reservation could collide with
    /// a dynamically handed-out lease.
    #[arg(long, env = "HEARTH_DHCP_STATIC_START", default_value = "10.26.8.16")]
    pub dhcp_static_start: std::net::Ipv4Addr,
    /// Size of the static-lease slice starting at `dhcp_static_start`
    /// (10.26.8.16-10.26.8.79 by default). Must not overlap dnsmasq's dynamic
    /// range.
    #[arg(long, env = "HEARTH_DHCP_STATIC_COUNT", default_value_t = 64)]
    pub dhcp_static_count: u32,
    /// Skip binding the per-VM hybrid vsock listeners (dev hosts without a
    /// writable run dir; unit tests).
    #[arg(long, env = "HEARTH_DISABLE_VSOCK", default_value_t = false)]
    pub disable_vsock: bool,
    /// Per-peer-UID verb policy (docs/agent-plane.md §10). Absent file: the
    /// built-in default applies (root and the hearth group may issue every
    /// verb; nobody else matches).
    #[arg(
        long,
        env = "HEARTH_VERB_POLICY",
        default_value = "/etc/hearth/verb-policy.toml"
    )]
    pub verb_policy: Utf8PathBuf,
}

impl Config {
    pub fn vm_socket(&self, id: &str) -> Utf8PathBuf {
        self.run_dir.join("vms").join(format!("{id}.sock"))
    }

    pub fn vm_vsock_socket(&self, id: &str) -> Utf8PathBuf {
        self.run_dir.join("vsock").join(format!("{id}.sock"))
    }

    /// Host-side unix socket where a guest-initiated vsock connection to host
    /// port `port` lands under CHV's hybrid model (`<id>.sock_<port>`, §6 of
    /// docs/agent-plane.md).
    pub fn vm_vsock_port_socket(&self, id: &str, port: u32) -> Utf8PathBuf {
        self.run_dir.join("vsock").join(format!("{id}.sock_{port}"))
    }

    /// The per-VM disk path for a service. Services normally record their disk
    /// filename; when absent, the fixed id gives the qcow2 filename.
    pub fn disk_path(&self, svc: &Service) -> Utf8PathBuf {
        match &svc.disk {
            Some(file) => self.disks_dir.join(file),
            None => self.disks_dir.join(format!("{}.qcow2", svc.id)),
        }
    }

    /// Per-VM disk path with an explicit extension (`raw` or `qcow2`). Used at
    /// create/destroy time when the concrete filename, not the service record,
    /// is what matters.
    pub fn disk_path_ext(&self, id: &str, ext: &str) -> Utf8PathBuf {
        self.disks_dir.join(format!("{id}.{ext}"))
    }

    pub fn console_path(&self, id: &str) -> Utf8PathBuf {
        self.log_dir.join(format!("{id}.console"))
    }

    pub fn snapshot_dir(&self, id: &str, tag: &str) -> Utf8PathBuf {
        self.snapshots_dir.join(id).join(tag)
    }

    pub fn image_path(&self, image: &str) -> Utf8PathBuf {
        let filename = if image.ends_with(".qcow2") {
            image.to_string()
        } else {
            format!("{image}.qcow2")
        };
        self.images_dir.join(filename)
    }

    pub fn image_manifest_path(&self, image: &str) -> Utf8PathBuf {
        let base = image.strip_suffix(".qcow2").unwrap_or(image);
        self.images_dir.join(format!("{base}.hearth.toml"))
    }
}
