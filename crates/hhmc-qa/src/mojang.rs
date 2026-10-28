//! Fetching the pinned vanilla server jar via Mojang's version manifest
//! (<https://piston-meta.mojang.com/mc/game/version_manifest_v2.json>). The jar goes to
//! `tools/minecraft/<version>/server.jar`, which is gitignored and never shipped.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha1::{Digest, Sha1};
use tracing::info;

const MANIFEST: &str = "https://piston-meta.mojang.com/mc/game/version_manifest_v2.json";

#[derive(Deserialize)]
struct Manifest {
    versions: Vec<ManifestVersion>,
}

#[derive(Deserialize)]
struct ManifestVersion {
    id: String,
    url: String,
    sha1: String,
}

#[derive(Deserialize)]
struct VersionJson {
    downloads: Downloads,
    #[serde(rename = "javaVersion")]
    java_version: JavaVersion,
}

#[derive(Deserialize)]
struct Downloads {
    server: Download,
    client: Download,
}

#[derive(Deserialize)]
struct Download {
    sha1: String,
    size: u64,
    url: String,
}

#[derive(Deserialize)]
struct JavaVersion {
    #[serde(rename = "majorVersion")]
    major_version: u32,
}

fn get(url: &str) -> Result<Vec<u8>> {
    let mut resp = ureq::get(url).call().with_context(|| format!("GET {url}"))?;
    Ok(resp.body_mut().with_config().limit(256 << 20).read_to_vec()?)
}

fn sha1_hex(b: &[u8]) -> String {
    hex::encode(Sha1::digest(b))
}

pub fn jar_path(root: &Path, version: &str) -> PathBuf {
    root.join("tools/minecraft").join(version).join("server.jar")
}

/// Ensures the server jar for `version` is present and matches Mojang's sha1.
/// Returns its path and the Java major version it requires.
pub fn ensure_server_jar(root: &Path, version: &str) -> Result<(PathBuf, u32)> {
    let v = version_json(version)?;
    let path = jar_path(root, version);
    ensure_download(&path, &v.downloads.server, "server")?;
    Ok((path, v.java_version.major_version))
}

/// Ensures the client jar (block textures, models, colormaps) is present; it is only read
/// locally to derive average colors and is never committed or shipped.
pub fn ensure_client_jar(root: &Path, version: &str) -> Result<PathBuf> {
    let v = version_json(version)?;
    let path = root.join("tools/minecraft").join(version).join("client.jar");
    ensure_download(&path, &v.downloads.client, "client")?;
    Ok(path)
}

fn version_json(version: &str) -> Result<VersionJson> {
    let manifest: Manifest = serde_json::from_slice(&get(MANIFEST)?)?;
    let v = manifest
        .versions
        .iter()
        .find(|v| v.id == version)
        .with_context(|| format!("version {version} not in the Mojang manifest"))?;
    let bytes = get(&v.url)?;
    if sha1_hex(&bytes) != v.sha1 {
        bail!("version JSON for {version} does not match the manifest sha1");
    }
    Ok(serde_json::from_slice(&bytes)?)
}

fn ensure_download(path: &Path, d: &Download, what: &str) -> Result<()> {
    let ok = std::fs::read(path).map(|b| sha1_hex(&b) == d.sha1).unwrap_or(false);
    if !ok {
        info!(what, size = d.size, "downloading Minecraft jar");
        let bytes = get(&d.url)?;
        if sha1_hex(&bytes) != d.sha1 {
            bail!("{what} jar sha1 mismatch");
        }
        std::fs::create_dir_all(path.parent().expect("has parent"))?;
        std::fs::write(path, bytes)?;
    }
    Ok(())
}
