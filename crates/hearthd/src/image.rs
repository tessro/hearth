use crate::config::Config;
use anyhow::{anyhow, Context, Result};
use camino::Utf8Path;
use hearth_proto::{ImageKind, ImageManifest};
use tokio::fs;

pub const CLOUD_IMAGE_KIND: &str = "cloud-image";
pub const DOCKER_ROOTFS_KIND: &str = "docker-rootfs";

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageMetadata {
    CloudImage,
    DockerRootfs(ImageManifest),
}

impl ImageMetadata {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::CloudImage => CLOUD_IMAGE_KIND,
            Self::DockerRootfs(_) => DOCKER_ROOTFS_KIND,
        }
    }
}

pub async fn load(cfg: &Config, image: &str) -> Result<ImageMetadata> {
    let manifest_path = cfg.image_manifest_path(image);
    if !manifest_path.exists() {
        return Ok(ImageMetadata::CloudImage);
    }
    read_manifest(&manifest_path)
        .await
        .map(ImageMetadata::DockerRootfs)
}

pub async fn read_manifest(path: &Utf8Path) -> Result<ImageManifest> {
    let text = fs::read_to_string(path)
        .await
        .with_context(|| format!("read image manifest {path}"))?;
    let mut manifest: ImageManifest =
        toml::from_str(&text).with_context(|| format!("parse image manifest {path}"))?;
    manifest
        .validate()
        .map_err(|message| anyhow!("invalid image manifest {path}: {message}"))?;
    match manifest.kind {
        ImageKind::DockerRootfs => Ok(manifest),
    }
}
