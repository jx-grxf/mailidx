use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

const REPO: &str = "jx-grxf/mailidx";
const ASSET: &str = "mailidx-macos-universal.tar.gz";
const CHECK_EVERY_SECS: i64 = 24 * 3600;
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Status {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub checked_at: i64,
    pub release_url: String,
}

fn cache_path() -> Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join("Library/Caches/mailidx/update.json"))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn release_url(version: &str) -> String {
    format!("https://github.com/{REPO}/releases/tag/v{version}")
}

/// Last known result without touching the network.
pub fn cached() -> Option<Status> {
    let raw = std::fs::read(cache_path().ok()?).ok()?;
    let mut s: Status = serde_json::from_slice(&raw).ok()?;
    s.current = CURRENT.to_string();
    s.available = newer(&s.latest, CURRENT);
    Some(s)
}

/// Asks GitHub for the latest release, at most once a day unless `force`.
pub fn check(force: bool) -> Result<Status> {
    if !force
        && let Some(s) = cached()
        && now() - s.checked_at < CHECK_EVERY_SECS
    {
        return Ok(s);
    }
    let latest = fetch_latest()?;
    let s = Status {
        current: CURRENT.to_string(),
        available: newer(&latest, CURRENT),
        release_url: release_url(&latest),
        latest,
        checked_at: now(),
    };
    let p = cache_path()?;
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::write(&p, serde_json::to_vec(&s)?)?;
    Ok(s)
}

fn curl(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("/usr/bin/curl")
        .args(["-fsSL", "--max-time", "60", "-A", &format!("mailidx/{CURRENT}")])
        .args(args)
        .output()
        .context("cannot run curl")?;
    if !out.status.success() {
        bail!("download failed: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    Ok(out.stdout)
}

fn fetch_latest() -> Result<String> {
    let body = curl(&[
        "-H",
        "Accept: application/vnd.github+json",
        &format!("https://api.github.com/repos/{REPO}/releases/latest"),
    ])?;
    let v: serde_json::Value = serde_json::from_slice(&body).context("GitHub sent an unexpected answer")?;
    let tag = v["tag_name"].as_str().ok_or_else(|| anyhow!("latest release has no tag"))?;
    Ok(tag.trim_start_matches('v').to_string())
}

fn parts(v: &str) -> Vec<u64> {
    v.split(['.', '-']).take(3).map(|p| p.parse().unwrap_or(0)).collect()
}

pub fn newer(candidate: &str, current: &str) -> bool {
    parts(candidate) > parts(current)
}

/// Downloads `version`, checks its SHA-256 against the published checksum, makes
/// sure the new binary runs, then swaps it in for the running one with a rename,
/// so a process that still uses the old file keeps working.
pub fn install(version: &str) -> Result<PathBuf> {
    let target = std::env::current_exe()?.canonicalize()?;
    let dir = target.parent().ok_or_else(|| anyhow!("binary has no parent directory"))?;
    let tmp = std::env::temp_dir().join(format!("mailidx-update-{}", std::process::id()));
    std::fs::create_dir_all(&tmp)?;
    let res = install_into(version, &tmp, dir, &target);
    let _ = std::fs::remove_dir_all(&tmp);
    res?;
    Ok(target)
}

fn install_into(version: &str, tmp: &Path, dir: &Path, target: &Path) -> Result<()> {
    let base = format!("https://github.com/{REPO}/releases/download/v{version}");
    let archive = tmp.join(ASSET);
    let archive_s = archive.to_string_lossy().to_string();
    curl(&["-o", &archive_s, &format!("{base}/{ASSET}")])?;
    let sums = String::from_utf8(curl(&[&format!("{base}/{ASSET}.sha256")])?)?;
    let expected = sums.split_whitespace().next().ok_or_else(|| anyhow!("empty checksum file"))?;
    let out = Command::new("/usr/bin/shasum").args(["-a", "256", &archive_s]).output()?;
    let actual = String::from_utf8_lossy(&out.stdout);
    if actual.split_whitespace().next() != Some(expected) {
        bail!("checksum mismatch, nothing was installed");
    }
    let st = Command::new("/usr/bin/tar").args(["-xzf", &archive_s, "-C"]).arg(tmp).status()?;
    if !st.success() {
        bail!("could not unpack {ASSET}");
    }
    let new_bin = tmp.join("mailidx");
    let staged = dir.join(".mailidx-update");
    std::fs::copy(&new_bin, &staged).with_context(|| format!("cannot write to {}", dir.display()))?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755))?;
    let ver = Command::new(&staged).arg("--version").output();
    let ok = ver.as_ref().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(version));
    if !ok {
        let _ = std::fs::remove_file(&staged);
        bail!("the downloaded binary did not start, nothing was installed");
    }
    std::fs::rename(&staged, target)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::newer;

    #[test]
    fn compares_versions_numerically() {
        assert!(newer("0.2.0", "0.1.9"));
        assert!(newer("0.10.0", "0.9.3"));
        assert!(newer("1.0.0", "0.99.0"));
        assert!(!newer("0.2.0", "0.2.0"));
        assert!(!newer("0.1.1", "0.2.0"));
    }
}
