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
        default_value = "/run/booted-system/kernel"
    )]
    pub guest_kernel: Utf8PathBuf,
    #[arg(long, env = "HEARTH_GUEST_INITRAMFS")]
    pub guest_initramfs: Option<Utf8PathBuf>,
    #[arg(long, env = "HEARTH_BRIDGE", default_value = "hearth0")]
    pub bridge: String,
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

    pub fn disk_path(&self, name: &str) -> Utf8PathBuf {
        self.disks_dir.join(format!("{name}.qcow2"))
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
