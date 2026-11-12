//! A quiet check for a newer Craft Anything release. It runs on a worker when the app opens and
//! becomes one line on Home with a link to the release. Nothing here blocks a screen, and every
//! failure (offline, rate limit, GitHub trouble) is silent: a notice is a nicety, and the next
//! launch tries again.

use craft_anything_evidence::{ErrorCode, ToolError};
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Where the newest release is announced (unauthenticated; one call a day at most).
pub const LATEST: &str = "https://api.github.com/repos/storytold/craft-anything/releases/latest";
/// Generic, non-identifying User-Agent (GitHub asks for one).
const USER_AGENT: &str = "CraftAnything-dev";
/// How long one saved answer counts before asking again.
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// How long the whole check may take: a slow network just means the next launch tries again.
const TIMEOUT: Duration = Duration::from_secs(10);
/// Most of the answer read (a release's JSON is a few KB).
const MAX_BODY: u64 = 256 * 1024;

/// A newer release than the one running.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Update {
    /// The release's version without its leading `v`, e.g. `0.6.1`.
    pub version: String,
    /// The release page, to open in the browser.
    pub page: String,
}

/// Where the saved answer lives: the app's config folder.
pub fn cache_path() -> std::path::PathBuf {
    craft_anything_engine::settings::config_dir().join("update-check.json")
}

/// Whether the release `latest` is newer than `current`; tags may carry a leading `v`. A version
/// that can't be read (a dev build) never gets a notice.
pub fn is_newer(current: &str, latest: &str) -> bool {
    parse(current).zip(parse(latest)).is_some_and(|(c, l)| l > c)
}

/// The `major.minor.patch` of a version or tag, ignoring a leading `v` and any pre-release or
/// build suffix.
fn parse(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim();
    let v = v.strip_prefix('v').unwrap_or(v);
    let v = v.split(['-', '+']).next().unwrap_or_default();
    let mut parts = v.split('.');
    let major = parts.next()?.trim().parse().ok()?;
    let minor = parts.next().unwrap_or("0").trim().parse().ok()?;
    let patch = parts.next().unwrap_or("0").trim().parse().ok()?;
    Some((major, minor, patch))
}

/// What the last check answered, kept in the config folder so launches don't ask again and a
/// notice outlives a day of bad network.
#[derive(Serialize, Deserialize)]
struct Saved {
    checked_ms: u64,
    version: String,
    page: String,
}

/// The newest release when it's newer than `current`, else `None`. `cache` is the saved-answer
/// file (see [`cache_path`]); every failure is silent.
pub fn check(current: &str, cache: &Path) -> Option<Update> {
    let saved = std::fs::read(cache).ok().and_then(|b| serde_json::from_slice::<Saved>(&b).ok());
    if let Some(s) = &saved
        && now_ms().saturating_sub(s.checked_ms) < EVERY.as_millis() as u64
    {
        return notice(current, s);
    }
    match fetch(LATEST) {
        Ok((tag, page)) => {
            let s = Saved { checked_ms: now_ms(), version: tag, page };
            if let Some(dir) = cache.parent() {
                let _ = std::fs::create_dir_all(dir);
            }
            if let Ok(bytes) = serde_json::to_vec(&s) {
                let _ = craft_anything_platform::fs::write_atomic(cache, &bytes);
            }
            notice(current, &s)
        }
        Err(e) => {
            log::info!("a newer version couldn't be checked: {}", e.message);
            saved.as_ref().and_then(|s| notice(current, s))
        }
    }
}

/// The notice `s` is, when its release is newer than `current`.
fn notice(current: &str, s: &Saved) -> Option<Update> {
    let version = s.version.trim().strip_prefix('v').unwrap_or(s.version.trim());
    is_newer(current, &s.version).then(|| Update { version: version.to_string(), page: s.page.clone() })
}

fn now_ms() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX)).unwrap_or(0)
}

/// One GitHub API call: the release's tag and its page. `url` is a parameter so tests can point
/// it at a loopback server.
fn fetch(url: &str) -> Result<(String, String), ToolError> {
    let agent = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .user_agent(USER_AGENT)
        .timeout_connect(Some(TIMEOUT))
        .timeout_recv_response(Some(TIMEOUT))
        .timeout_recv_body(Some(TIMEOUT))
        .build()
        .new_agent();
    let resp = agent.get(url).header("Accept", "application/vnd.github+json").call().map_err(unreachable)?;
    let status = resp.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(ToolError::new(ErrorCode::EngineMissing, format!("GitHub answered {status} for the newest release.")));
    }
    use std::io::Read as _;
    let mut body = String::new();
    resp.into_body()
        .into_reader()
        .take(MAX_BODY)
        .read_to_string(&mut body)
        .map_err(|e| ToolError::new(ErrorCode::EngineMissing, format!("The newest release couldn't be read: {e}")))?;
    let v: serde_json::Value =
        serde_json::from_str(&body).map_err(|e| ToolError::new(ErrorCode::GarbledOutput, format!("The newest release wasn't JSON: {e}")))?;
    let tag = v
        .get("tag_name")
        .and_then(|t| t.as_str())
        .filter(|t| !t.trim().is_empty())
        .ok_or_else(|| ToolError::new(ErrorCode::GarbledOutput, "The newest release has no tag."))?;
    let page = v.get("html_url").and_then(|u| u.as_str()).filter(|u| u.starts_with("https://")).unwrap_or(LATEST);
    Ok((tag.to_string(), page.to_string()))
}

fn unreachable(e: ureq::Error) -> ToolError {
    let code = if matches!(e, ureq::Error::Timeout(_)) { ErrorCode::TimedOut } else { ErrorCode::EngineMissing };
    ToolError::new(code, format!("Couldn't reach GitHub to check for a newer version: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_really_newer_version_is_newer() {
        assert!(is_newer("0.5.0", "v0.6.0"));
        assert!(is_newer("0.5.0", "0.5.1"));
        assert!(is_newer("0.5.9", "0.6.0"));
        assert!(is_newer("0.5.0", "v1.0.0"));
        assert!(!is_newer("0.6.0", "v0.6.0"), "the same release isn't new");
        assert!(!is_newer("0.6.0", "v0.5.0"), "an older release isn't new");
        assert!(!is_newer("0.6.0", "not a version"));
        assert!(!is_newer("dev", "v9.9.9"), "a build without a version gets no notice");
        assert!(!is_newer("0.6.0", "v0.6"), "a two-part tag reads as 0.6.0");
        assert!(!is_newer("0.6.1", "v0.6"), "so a patch past it is newer");
    }

    #[test]
    fn the_newest_release_is_read_from_githubs_answer() {
        use std::io::{Read as _, Write as _};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut s, _) = listener.accept().unwrap();
            let mut buf = [0u8; 4096];
            let _ = s.read(&mut buf);
            let body = r#"{"tag_name":"v9.9.9","html_url":"https://github.com/storytold/craft-anything/releases/tag/v9.9.9"}"#;
            let _ = write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
        });
        let (tag, page) = fetch(&format!("http://127.0.0.1:{port}/latest")).unwrap();
        server.join().unwrap();
        assert_eq!((tag.as_str(), page.as_str()), ("v9.9.9", "https://github.com/storytold/craft-anything/releases/tag/v9.9.9"));
    }

    #[test]
    fn a_saved_answer_is_used_without_asking_again() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("nested").join("update-check.json");
        let saved = Saved { checked_ms: now_ms(), version: "v9.9.9".into(), page: "https://example.com/x".into() };
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(&cache, serde_json::to_vec(&saved).unwrap()).unwrap();
        assert_eq!(check("0.1.0", &cache), Some(Update { version: "9.9.9".into(), page: "https://example.com/x".into() }));
        assert_eq!(check("9.9.9", &cache), None, "the running version is the saved one: nothing to say");
    }
}
