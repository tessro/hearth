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
        env = "HEARTH_SEEDS_DIR",
        default_value = "/var/lib/hearth/seeds"
    )]
    pub seeds_dir: Utf8PathBuf,
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
    #[arg(
        long,
        env = "HEARTH_FIRMWARE",
        default_value = "/usr/share/hypervisor-fw/CLOUDHV.fd"
    )]
    pub firmware: Utf8PathBuf,
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
    /// dnsmasq drop-in dir where Hearth writes `<name>.conf` static-lease
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
    #[arg(
        long,
        env = "HEARTH_DHCP_STATIC_START",
        default_value = "10.26.8.16"
    )]
    pub dhcp_static_start: std::net::Ipv4Addr,
    /// Size of the static-lease slice starting at `dhcp_static_start`
    /// (10.26.8.16-10.26.8.79 by default). Must not overlap dnsmasq's dynamic
    /// range.
    #[arg(long, env = "HEARTH_DHCP_STATIC_COUNT", default_value_t = 64)]
    pub dhcp_static_count: u32,
    #[arg(long, env = "HEARTH_VSOCK_PORT", default_value_t = 1024)]
    pub vsock_port: u32,
    #[arg(long, env = "HEARTH_DISABLE_VSOCK", default_value_t = false)]
    pub disable_vsock: bool,
}

impl Config {
    pub fn vm_socket(&self, name: &str) -> Utf8PathBuf {
        self.run_dir.join("vms").join(format!("{name}.sock"))
    }

    pub fn vm_vsock_socket(&self, name: &str) -> Utf8PathBuf {
        self.run_dir.join("vsock").join(format!("{name}.sock"))
    }

    /// The per-VM disk path for a service. New services record their disk
    /// filename (`{name}.qcow2` — every boot disk is qcow2); services created
    /// before that field existed resolve to the legacy `{name}.qcow2`.
    pub fn disk_path(&self, svc: &Service) -> Utf8PathBuf {
        match &svc.disk {
            Some(file) => self.disks_dir.join(file),
            None => self.disks_dir.join(format!("{}.qcow2", svc.name)),
        }
    }

    /// Per-VM disk path with an explicit extension (`raw` or `qcow2`). Used at
    /// create/destroy time when the concrete filename, not the service record,
    /// is what matters.
    pub fn disk_path_ext(&self, name: &str, ext: &str) -> Utf8PathBuf {
        self.disks_dir.join(format!("{name}.{ext}"))
    }

    pub fn seed_path(&self, name: &str) -> Utf8PathBuf {
        self.seeds_dir.join(format!("{name}.iso"))
    }

    pub fn console_path(&self, name: &str) -> Utf8PathBuf {
        self.log_dir.join(format!("{name}.console"))
    }

    pub fn snapshot_dir(&self, name: &str, tag: &str) -> Utf8PathBuf {
        self.snapshots_dir.join(name).join(tag)
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
