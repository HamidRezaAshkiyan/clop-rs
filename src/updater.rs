use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// GitHub repository slug (`owner/repo`) used for release checks.
pub const GITHUB_REPO: &str = "HamidRezaAshkiyan/clop-rs";

/// GitHub REST API endpoint for the latest published release.
pub const LATEST_RELEASE_API_URL: &str =
    "https://api.github.com/repos/HamidRezaAshkiyan/clop-rs/releases/latest";

/// Fallback human-facing releases page URL.
pub const RELEASES_PAGE_URL: &str = "https://github.com/HamidRezaAshkiyan/clop-rs/releases";

/// Minimum interval between automatic background update checks (24 hours).
pub const AUTO_CHECK_INTERVAL_SECS: u64 = 24 * 60 * 60;

/// Result of checking GitHub Releases for a newer version of `clop-rs`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct UpdateStatus {
    /// Currently running `clop-rs` version (e.g. `"1.1.0"`).
    pub current_version: String,
    /// Latest published version on GitHub without leading `v` (e.g. `"1.2.0"`).
    pub latest_version: Option<String>,
    /// `true` if `latest_version` is strictly newer than `current_version`.
    pub update_available: bool,
    /// Direct URL to the GitHub Release page.
    pub release_url: String,
    /// Optional short excerpt of the release notes.
    pub release_notes: Option<String>,
    /// Optional ISO-8601 publication timestamp from GitHub.
    pub published_at: Option<String>,
    /// Unix timestamp (seconds) when this check completed.
    pub checked_at_unix: u64,
    /// Present if the network/API check failed.
    pub error: Option<String>,
}

impl UpdateStatus {
    /// Constructs an error status while preserving the running version and releases URL.
    pub fn error(current_version: &str, err: impl Into<String>) -> Self {
        Self {
            current_version: normalize_version(current_version).to_string(),
            latest_version: None,
            update_available: false,
            release_url: RELEASES_PAGE_URL.to_string(),
            release_notes: None,
            published_at: None,
            checked_at_unix: now_unix_secs(),
            error: Some(err.into()),
        }
    }
}

#[derive(Debug, Deserialize)]
struct GitHubReleaseResponse {
    tag_name: String,
    #[serde(default)]
    html_url: Option<String>,
    #[serde(default)]
    body: Option<String>,
    #[serde(default)]
    published_at: Option<String>,
    #[serde(default)]
    draft: bool,
    #[serde(default)]
    prerelease: bool,
}

/// Returns the current Unix timestamp in seconds.
pub fn now_unix_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Strips a leading `v` or `V` and surrounding whitespace from a version tag.
pub fn normalize_version(raw: &str) -> &str {
    let trimmed = raw.trim();
    trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed)
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SemVer {
    major: u64,
    minor: u64,
    patch: u64,
    prerelease: Option<String>,
}

fn parse_semver(raw: &str) -> Option<SemVer> {
    let cleaned = normalize_version(raw);
    // Strip optional build metadata (`+...`)
    let without_build = cleaned.split('+').next()?;
    let (core, prerelease) = match without_build.split_once('-') {
        Some((c, pre)) if !pre.is_empty() => (c, Some(pre.to_string())),
        Some((c, _)) => (c, None),
        None => (without_build, None),
    };

    let mut parts = core.split('.');
    let major: u64 = parts.next()?.parse().ok()?;
    let minor: u64 = parts.next().unwrap_or("0").parse().ok()?;
    let patch: u64 = parts.next().unwrap_or("0").parse().ok()?;
    if parts.next().is_some() {
        return None;
    }

    Some(SemVer {
        major,
        minor,
        patch,
        prerelease,
    })
}

fn compare_prerelease(a: &str, b: &str) -> Ordering {
    let a_ids: Vec<&str> = a.split('.').collect();
    let b_ids: Vec<&str> = b.split('.').collect();
    for (ai, bi) in a_ids.iter().zip(b_ids.iter()) {
        let ord = match (ai.parse::<u64>(), bi.parse::<u64>()) {
            (Ok(an), Ok(bn)) => an.cmp(&bn),
            (Ok(_), Err(_)) => Ordering::Less,
            (Err(_), Ok(_)) => Ordering::Greater,
            (Err(_), Err(_)) => ai.cmp(bi),
        };
        if ord != Ordering::Equal {
            return ord;
        }
    }
    a_ids.len().cmp(&b_ids.len())
}

impl Ord for SemVer {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
            .then_with(|| match (&self.prerelease, &other.prerelease) {
                (None, None) => Ordering::Equal,
                // A stable release has higher precedence than a pre-release of the same version
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(a), Some(b)) => compare_prerelease(a, b),
            })
    }
}

impl PartialOrd for SemVer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

/// Returns `true` if `latest` represents a strictly newer semantic version than `current`.
pub fn is_newer_version(current: &str, latest: &str) -> bool {
    match (parse_semver(current), parse_semver(latest)) {
        (Some(cur), Some(lat)) => lat > cur,
        _ => false,
    }
}

/// Parses a GitHub `/releases/latest` JSON payload into an [`UpdateStatus`].
pub fn parse_github_release_json(current_version: &str, body: &str) -> Result<UpdateStatus> {
    let rel: GitHubReleaseResponse =
        serde_json::from_str(body).context("Failed to parse GitHub release response JSON")?;

    if rel.draft {
        bail!("Latest release is marked as a draft");
    }

    let current_norm = normalize_version(current_version).to_string();
    let latest_norm = normalize_version(&rel.tag_name).to_string();
    if parse_semver(&latest_norm).is_none() {
        bail!("Unrecognized release tag version: {}", rel.tag_name);
    }

    let update_available = !rel.prerelease && is_newer_version(&current_norm, &latest_norm);

    let release_url = rel
        .html_url
        .filter(|u| u.starts_with("https://") || u.starts_with("http://"))
        .unwrap_or_else(|| RELEASES_PAGE_URL.to_string());

    let release_notes = rel.body.and_then(|notes| {
        let trimmed = notes.trim();
        if trimmed.is_empty() {
            None
        } else if trimmed.chars().count() > 600 {
            let truncated: String = trimmed.chars().take(600).collect();
            Some(format!("{}…", truncated.trim_end()))
        } else {
            Some(trimmed.to_string())
        }
    });

    Ok(UpdateStatus {
        current_version: current_norm,
        latest_version: Some(latest_norm),
        update_available,
        release_url,
        release_notes,
        published_at: rel.published_at,
        checked_at_unix: now_unix_secs(),
        error: None,
    })
}

/// Fetches the latest release JSON from GitHub using the OS-provided `curl` binary
/// (built into macOS, Windows 10/11, and Linux) to keep the binary footprint minimal.
fn fetch_latest_release_json(current_version: &str) -> Result<String> {
    let user_agent = format!("clop-rs/{}", normalize_version(current_version));

    #[cfg(target_os = "windows")]
    let mut cmd = {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut c = Command::new("curl.exe");
        c.creation_flags(CREATE_NO_WINDOW);
        c
    };

    #[cfg(not(target_os = "windows"))]
    let mut cmd = Command::new("curl");

    let output = cmd
        .args([
            "-fsSL",
            "--max-time",
            "10",
            "-H",
            "Accept: application/vnd.github+json",
            "-H",
            &format!("User-Agent: {}", user_agent),
            LATEST_RELEASE_API_URL,
        ])
        .output()
        .context("Failed to execute curl to check for updates")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        if detail.is_empty() {
            bail!(
                "Update check request failed with exit code {:?}",
                output.status.code()
            );
        } else {
            bail!("Update check request failed: {}", detail);
        }
    }

    String::from_utf8(output.stdout).context("GitHub release response was not valid UTF-8")
}

/// Checks GitHub Releases for a newer version than the currently running `clop-rs` binary.
pub fn check_for_updates() -> UpdateStatus {
    check_for_updates_for_version(env!("CARGO_PKG_VERSION"))
}

/// Checks GitHub Releases against a specific `current_version` string.
pub fn check_for_updates_for_version(current_version: &str) -> UpdateStatus {
    match fetch_latest_release_json(current_version)
        .and_then(|json| parse_github_release_json(current_version, &json))
    {
        Ok(status) => status,
        Err(err) => {
            log::debug!("Update check failed: {}", err);
            UpdateStatus::error(current_version, err.to_string())
        }
    }
}

/// Returns the path to the persisted update check cache file (`update_cache.json`).
pub fn cache_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("Could not determine OS configuration directory")?;
    Ok(base.join("clop-rs").join("update_cache.json"))
}

/// Loads the last cached [`UpdateStatus`] if present and still matching the running version.
pub fn load_cached_status() -> Option<UpdateStatus> {
    let path = cache_path().ok()?;
    if !path.exists() {
        return None;
    }
    let raw = fs::read_to_string(path).ok()?;
    let mut cached: UpdateStatus = serde_json::from_str(&raw).ok()?;
    let running = normalize_version(env!("CARGO_PKG_VERSION"));
    if normalize_version(&cached.current_version) != running {
        // User upgraded since the cache was written; recompute against running version.
        cached.current_version = running.to_string();
        cached.update_available = cached
            .latest_version
            .as_deref()
            .is_some_and(|lat| is_newer_version(running, lat));
    }
    Some(cached)
}

/// Saves an [`UpdateStatus`] atomically to `update_cache.json` (only when check succeeded).
pub fn save_cached_status(status: &UpdateStatus) -> Result<PathBuf> {
    let path = cache_path()?;
    let bytes = serde_json::to_vec_pretty(status)?;
    crate::optimizer::atomic_write_file(&path, &bytes)?;
    Ok(path)
}

/// Returns `true` if enough time has elapsed since the last check to run an automatic background check.
pub fn should_auto_check(cached: Option<&UpdateStatus>, now_unix: u64) -> bool {
    let Some(status) = cached else {
        return true;
    };
    if status.error.is_some() {
        return true;
    }
    now_unix.saturating_sub(status.checked_at_unix) >= AUTO_CHECK_INTERVAL_SECS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_semver_comparison() {
        assert!(is_newer_version("1.0.0", "1.0.1"));
        assert!(is_newer_version("1.0.0", "1.1.0"));
        assert!(is_newer_version("1.0.0", "2.0.0"));
        assert!(is_newer_version("1.0.9", "1.0.10"));
        assert!(is_newer_version("v1.0.0", "v1.1.0"));
        assert!(is_newer_version("1.1.0-beta.1", "1.1.0"));
        assert!(is_newer_version("1.1.0-alpha.1", "1.1.0-beta.1"));

        assert!(!is_newer_version("1.1.0", "1.1.0"));
        assert!(!is_newer_version("v1.1.0", "1.1.0"));
        assert!(!is_newer_version("1.2.0", "1.1.9"));
        assert!(!is_newer_version("1.1.0", "1.1.0-rc.1"));
        assert!(!is_newer_version("1.0.0", "not-a-version"));
    }

    #[test]
    fn test_parse_github_release_json_newer() {
        let json = r#"{
            "tag_name": "v1.2.0",
            "html_url": "https://github.com/HamidRezaAshkiyan/clop-rs/releases/tag/v1.2.0",
            "body": "Added awesome new features.",
            "published_at": "2026-12-01T10:00:00Z",
            "draft": false,
            "prerelease": false
        }"#;
        let status = parse_github_release_json("1.1.0", json).expect("valid json");
        assert_eq!(status.current_version, "1.1.0");
        assert_eq!(status.latest_version.as_deref(), Some("1.2.0"));
        assert!(status.update_available);
        assert_eq!(
            status.release_url,
            "https://github.com/HamidRezaAshkiyan/clop-rs/releases/tag/v1.2.0"
        );
        assert_eq!(
            status.release_notes.as_deref(),
            Some("Added awesome new features.")
        );
        assert!(status.error.is_none());
    }

    #[test]
    fn test_parse_github_release_json_up_to_date() {
        let json = r#"{
            "tag_name": "v1.1.0",
            "html_url": "https://github.com/HamidRezaAshkiyan/clop-rs/releases/tag/v1.1.0",
            "body": "",
            "draft": false,
            "prerelease": false
        }"#;
        let status = parse_github_release_json("1.1.0", json).expect("valid json");
        assert!(!status.update_available);
        assert_eq!(status.latest_version.as_deref(), Some("1.1.0"));
        assert!(status.release_notes.is_none());
    }

    #[test]
    fn test_parse_github_release_json_skips_draft_and_prerelease() {
        let draft = r#"{"tag_name":"v2.0.0","draft":true,"prerelease":false}"#;
        assert!(parse_github_release_json("1.1.0", draft).is_err());

        let pre = r#"{"tag_name":"v2.0.0-rc.1","draft":false,"prerelease":true}"#;
        let status = parse_github_release_json("1.1.0", pre).expect("valid json");
        assert!(!status.update_available);
    }

    #[test]
    fn test_should_auto_check_respects_interval() {
        assert!(should_auto_check(None, 1_000_000));

        let recent = UpdateStatus {
            current_version: "1.1.0".to_string(),
            latest_version: Some("1.1.0".to_string()),
            update_available: false,
            release_url: RELEASES_PAGE_URL.to_string(),
            release_notes: None,
            published_at: None,
            checked_at_unix: 1_000_000,
            error: None,
        };
        assert!(!should_auto_check(Some(&recent), 1_000_000 + 3600));
        assert!(should_auto_check(
            Some(&recent),
            1_000_000 + AUTO_CHECK_INTERVAL_SECS
        ));

        let errored = UpdateStatus::error("1.1.0", "timeout");
        assert!(should_auto_check(Some(&errored), errored.checked_at_unix));
    }
}
