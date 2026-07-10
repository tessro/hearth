use crate::config::Config;
use anyhow::{anyhow, Context, Result};
use camino::Utf8Path;
use hearth_proto::ImageManifest;
use tokio::fs;

pub async fn load(cfg: &Config, image: &str) -> Result<ImageManifest> {
    let manifest_path = cfg.image_manifest_path(image);
    if !manifest_path.exists() {
        return Err(anyhow!(
            "image {image} has no Hearth manifest at {manifest_path}"
        ));
    }
    read_manifest(&manifest_path).await
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
    Ok(manifest)
}

/// Parse the contents of a kernel `contract` file. A missing file (`None`)
/// resolves to contract 1, the original contract, so a hand-provided kernel with
/// no contract marker still boots images that only require contract 1.
pub fn parse_kernel_contract(contents: Option<&str>) -> Result<u32> {
    match contents {
        None => Ok(1),
        Some(text) => text
            .trim()
            .parse::<u32>()
            .map_err(|_| anyhow!("kernel contract file is not a number: {:?}", text.trim())),
    }
}

/// Whether a kernel advertising `kernel_contract` satisfies an image that
/// requires at least `min_required`.
pub fn kernel_contract_satisfies(min_required: u32, kernel_contract: u32) -> bool {
    kernel_contract >= min_required
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_contract_file_is_contract_one() {
        assert_eq!(parse_kernel_contract(None).unwrap(), 1);
    }

    #[test]
    fn contract_file_parses_trimmed_number() {
        assert_eq!(parse_kernel_contract(Some("2\n")).unwrap(), 2);
        assert_eq!(parse_kernel_contract(Some("  7  ")).unwrap(), 7);
    }

    #[test]
    fn non_numeric_contract_is_an_error() {
        assert!(parse_kernel_contract(Some("v1")).is_err());
    }

    #[test]
    fn contract_comparison() {
        assert!(kernel_contract_satisfies(1, 1));
        assert!(kernel_contract_satisfies(1, 2));
        assert!(!kernel_contract_satisfies(2, 1));
    }

    // `hearthctl image build` serializes the manifest with toml::to_string_pretty,
    // which errors if a scalar field is declared after the `[oci]` table. Pin the
    // ordering so min_kernel_contract stays a top-level scalar.
    #[test]
    fn manifest_serializes_to_toml_and_round_trips() {
        let mut manifest = ImageManifest::from_oci_process(hearth_proto::OciProcess {
            args: vec!["/usr/local/bin/init".to_string()],
            env: vec!["EXEUNTU=1".to_string()],
            cwd: "/home/exedev".to_string(),
        })
        .unwrap();
        manifest.min_kernel_contract = 2;
        let text = toml::to_string_pretty(&manifest).unwrap();
        assert!(text.contains("min_kernel_contract = 2"));
        let mut parsed: ImageManifest = toml::from_str(&text).unwrap();
        parsed.validate().unwrap();
        assert_eq!(parsed.min_kernel_contract, 2);
    }
}
