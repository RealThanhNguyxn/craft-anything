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
/// Browser-facing fallback when GitHub omits or rejects `html_url`.
const RELEASES_LATEST: &str = "https://github.com/storytold/craft-anything/releases/latest";
/// The only place a release page may live: this repo's releases on github.com.
const RELEASES: &str = "https://github.com/storytold/craft-anything/releases/";
/// Generic, non-identifying User-Agent (GitHub asks for one).
const USER_AGENT: &str = "CraftAnything-dev";
/// How long after an attempt (successful or not) before asking again.
const EVERY: Duration = Duration::from_secs(24 * 60 * 60);
/// How long the whole check may take, DNS lookup through the last byte of the body: a slow
/// network just means the next launch tries again.
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

/// Whether `url` is a page of this repo's releases on github.com and nothing else. The prefix
/// ends in a `/`, so `github.com.evil.example`, `github.com@evil.example`, another repo, another
/// scheme and `..` segments that climb out of `/releases/` are all refused.
fn is_release_page(url: &str) -> bool {
    url.strip_prefix(RELEASES).is_some_and(|rest| {
        !rest.is_empty()
            && !rest.chars().any(|c| c.is_control() || c.is_whitespace() || c == '\\')
            && !rest.split(['/', '?', '#']).any(|seg| seg == ".." || seg == ".")
    })
}

/// What the config folder remembers between launches. Two separate facts:
/// * when we last *tried* to ask GitHub (any outcome), which is what the once-a-day limit reads;
/// * the last release GitHub *answered* with, which keeps a notice alive through bad network.
#[derive(Serialize, Deserialize, Default)]
struct Saved {
    /// Last attempt, successful or not. Older files called this `checked_ms`.
    #[serde(default, alias = "checked_ms")]
    attempted_ms: u64,
    /// Newest release tag seen so far; absent until one request has succeeded.
    #[serde(default)]
    version: Option<String>,
    /// That release's page.
    #[serde(default)]
    page: Option<String>,
}

impl Saved {
    /// Whether the last attempt is recent enough not to ask again. An attempt stamped in the
    /// future (clock moved back) doesn't count, or the check would stay quiet until the clock
    /// caught up.
    fn is_fresh(&self, now: u64) -> bool {
        now.checked_sub(self.attempted_ms).is_some_and(|age| age < EVERY.as_millis() as u64)
    }
}

/// The newest release when it's newer than `current`, else `None`. `cache` is the saved-answer
/// file (see [`cache_path`]); every failure is silent.
pub fn check(current: &str, cache: &Path) -> Option<Update> {
    check_with(LATEST, current, cache)
}

/// [`check`] against `url`, so tests can point it at a loopback server.
fn check_with(url: &str, current: &str, cache: &Path) -> Option<Update> {
    let mut saved = read_saved(cache).unwrap_or_default();
    if saved.is_fresh(now_ms()) {
        return notice(current, &saved);
    }
    // Save the attempt BEFORE the request: a kill or crash mid-request, a 403/429, a server error
    // or no network all still wait a day, and a release saved earlier keeps giving its notice.
    saved.attempted_ms = now_ms();
    write_saved(cache, &saved);
    match fetch(url) {
        Ok((tag, page)) => {
            saved.version = Some(tag);
            saved.page = Some(page);
            write_saved(cache, &saved);
        }
        Err(e) => log::info!("a newer version couldn't be checked: {}", e.message),
    }
    notice(current, &saved)
}

fn read_saved(cache: &Path) -> Option<Saved> {
    std::fs::read(cache).ok().and_then(|b| serde_json::from_slice(&b).ok())
}

fn write_saved(cache: &Path, saved: &Saved) {
    if let Some(dir) = cache.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(bytes) = serde_json::to_vec(saved) {
        let _ = craft_anything_platform::fs::write_atomic(cache, &bytes);
    }
}

/// The notice `s` is, when its release is newer than `current`. The page is checked again here,
/// so a saved file that was edited or written by an older build can't send the browser elsewhere.
fn notice(current: &str, s: &Saved) -> Option<Update> {
    let (tag, page) = (s.version.as_deref()?, s.page.as_deref()?);
    let version = tag.trim().strip_prefix('v').unwrap_or(tag.trim());
    let page = if is_release_page(page) { page } else { RELEASES_LATEST };
    is_newer(current, tag).then(|| Update { version: version.to_string(), page: page.to_string() })
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
        // One end-to-end limit, DNS lookup through the end of the body; the per-phase limits
        // below can't add up past it.
        .timeout_global(Some(TIMEOUT))
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
    let page = v.get("html_url").and_then(|u| u.as_str()).filter(|u| is_release_page(u)).unwrap_or(RELEASES_LATEST);
    Ok((tag.to_string(), page.to_string()))
}

fn unreachable(e: ureq::Error) -> ToolError {
    let code = if matches!(e, ureq::Error::Timeout(_)) { ErrorCode::TimedOut } else { ErrorCode::EngineMissing };
    ToolError::new(code, format!("Couldn't reach GitHub to check for a newer version: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read as _, Write as _};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    /// A release page the app would really accept.
    const PAGE: &str = "https://github.com/storytold/craft-anything/releases/tag/v9.9.9";
    const OLD_PAGE: &str = "https://github.com/storytold/craft-anything/releases/tag/v1.0.0";

    /// A loopback server answering every request with `status` and `body`, counting requests and
    /// running `on_request` as each one arrives (before it is answered). Returns the URL and the
    /// counter. The thread ends with the test process.
    fn serve_with(status: &'static str, body: &'static str, on_request: impl Fn() + Send + 'static) -> (String, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&hits);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut s) = stream else { break };
                let mut buf = [0u8; 4096];
                let _ = s.read(&mut buf);
                on_request();
                counter.fetch_add(1, Ordering::SeqCst);
                let _ = write!(s, "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        (format!("http://127.0.0.1:{port}/latest"), hits)
    }

    fn serve(status: &'static str, body: &'static str) -> (String, Arc<AtomicUsize>) {
        serve_with(status, body, || {})
    }

    fn write_cache(cache: &Path, json: &str) {
        std::fs::create_dir_all(cache.parent().unwrap()).unwrap();
        std::fs::write(cache, json).unwrap();
    }

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
    fn only_this_repos_release_pages_on_github_are_accepted() {
        for ok in [PAGE, OLD_PAGE, "https://github.com/storytold/craft-anything/releases/latest"] {
            assert!(is_release_page(ok), "{ok} should be accepted");
        }
        for bad in [
            "https://github.com.attacker.example/storytold/craft-anything/releases/tag/v9.9.9",
            "https://github.com@attacker.example/storytold/craft-anything/releases/tag/v9.9.9",
            "https://attacker.example/storytold/craft-anything/releases/tag/v9.9.9",
            "http://github.com/storytold/craft-anything/releases/tag/v9.9.9",
            "https://github.com/other/craft-anything/releases/tag/v9.9.9",
            "https://github.com/storytold/craft-anything-evil/releases/tag/v9.9.9",
            "https://github.com/storytold/craft-anything/issues/1",
            "https://github.com/storytold/craft-anything/releases/",
            "https://github.com/storytold/craft-anything/releases/../../evil/repo",
            "https://github.com/storytold/craft-anything/releases/tag/v9.9.9 evil",
            "https://github.com/storytold/craft-anything/releases/tag/v9.9.9\\evil",
            "https://example.com/x",
            "",
        ] {
            assert!(!is_release_page(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn the_newest_release_is_read_from_githubs_answer() {
        let (url, hits) = serve("200 OK", r#"{"tag_name":"v9.9.9","html_url":"https://github.com/storytold/craft-anything/releases/tag/v9.9.9"}"#);
        let (tag, page) = fetch(&url).unwrap();
        assert_eq!((tag.as_str(), page.as_str()), ("v9.9.9", PAGE));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_page_outside_the_repos_releases_falls_back_to_the_browser_release_page() {
        let (url, _) =
            serve("200 OK", r#"{"tag_name":"v9.9.9","html_url":"https://github.com.attacker.example/storytold/craft-anything/releases/tag/v9.9.9"}"#);
        let (tag, page) = fetch(&url).unwrap();
        assert_eq!((tag.as_str(), page.as_str()), ("v9.9.9", RELEASES_LATEST));
    }

    #[test]
    fn a_tampered_page_in_the_saved_file_is_not_offered() {
        let saved = Saved { attempted_ms: now_ms(), version: Some("v9.9.9".into()), page: Some("https://attacker.example/x".into()) };
        assert_eq!(notice("0.1.0", &saved), Some(Update { version: "9.9.9".into(), page: RELEASES_LATEST.into() }));
    }

    #[test]
    fn a_saved_answer_is_used_without_asking_again() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("nested").join("update-check.json");
        let saved = Saved { attempted_ms: now_ms(), version: Some("v9.9.9".into()), page: Some(PAGE.into()) };
        write_cache(&cache, &serde_json::to_string(&saved).unwrap());
        let (url, hits) = serve("200 OK", r#"{"tag_name":"v10.0.0"}"#);
        assert_eq!(check_with(&url, "0.1.0", &cache), Some(Update { version: "9.9.9".into(), page: PAGE.into() }));
        assert_eq!(check_with(&url, "9.9.9", &cache), None, "the running version is the saved one: nothing to say");
        assert_eq!(hits.load(Ordering::SeqCst), 0, "a fresh answer means no request");
    }

    #[test]
    fn the_attempt_is_saved_before_the_request_goes_out() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("update-check.json");
        // What the file says at the moment the request arrives: a kill right here must still
        // leave the attempt on disk.
        let on_disk_during_request = Arc::new(AtomicU64::new(0));
        let (seen, path) = (Arc::clone(&on_disk_during_request), cache.clone());
        let (url, hits) = serve_with("500 Internal Server Error", "{}", move || {
            if let Some(s) = read_saved(&path) {
                seen.store(s.attempted_ms, Ordering::SeqCst);
            }
        });

        assert_eq!(check_with(&url, "0.1.0", &cache), None);
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert!(on_disk_during_request.load(Ordering::SeqCst) > 0, "the attempt was already saved when the request arrived");
    }

    #[test]
    fn a_failed_request_with_no_cache_is_not_repeated_on_the_next_launch() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("update-check.json");
        let (url, hits) = serve("429 Too Many Requests", "{}");

        assert_eq!(check_with(&url, "0.1.0", &cache), None);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the first launch asks");

        assert_eq!(check_with(&url, "0.1.0", &cache), None);
        assert_eq!(hits.load(Ordering::SeqCst), 1, "the second launch respects the daily limit");

        let saved = read_saved(&cache).expect("the attempt is saved even though it failed");
        assert!(saved.attempted_ms > 0);
        assert!(saved.version.is_none() && saved.page.is_none(), "no release was ever learned");
    }

    #[test]
    fn a_failed_request_keeps_an_older_notice_and_still_throttles() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("update-check.json");
        let stale = Saved { attempted_ms: now_ms() - 2 * DAY_MS, version: Some("v9.9.9".into()), page: Some(PAGE.into()) };
        write_cache(&cache, &serde_json::to_string(&stale).unwrap());
        let (url, hits) = serve("500 Internal Server Error", "{}");
        let notice = Some(Update { version: "9.9.9".into(), page: PAGE.into() });

        assert_eq!(check_with(&url, "0.1.0", &cache), notice, "the old notice survives the failure");
        assert_eq!(hits.load(Ordering::SeqCst), 1);

        assert_eq!(check_with(&url, "0.1.0", &cache), notice, "and is still shown on the next launch");
        assert_eq!(hits.load(Ordering::SeqCst), 1, "without asking again");

        let saved = read_saved(&cache).unwrap();
        assert_eq!(saved.version.as_deref(), Some("v9.9.9"), "the last good answer is kept");
        assert!(saved.attempted_ms > stale.attempted_ms, "the attempt time moved forward");
    }

    #[test]
    fn a_stale_answer_is_refreshed_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("update-check.json");
        let stale = Saved { attempted_ms: now_ms() - 2 * DAY_MS, version: Some("v1.0.0".into()), page: Some(OLD_PAGE.into()) };
        write_cache(&cache, &serde_json::to_string(&stale).unwrap());
        let (url, hits) = serve("200 OK", r#"{"tag_name":"v9.9.9","html_url":"https://github.com/storytold/craft-anything/releases/tag/v9.9.9"}"#);

        assert_eq!(check_with(&url, "0.1.0", &cache), Some(Update { version: "9.9.9".into(), page: PAGE.into() }));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        let saved = read_saved(&cache).unwrap();
        assert_eq!((saved.version.as_deref(), saved.page.as_deref()), (Some("v9.9.9"), Some(PAGE)));
    }

    #[test]
    fn a_cache_from_before_the_attempt_field_still_reads() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path().join("update-check.json");
        let json = format!(r#"{{"checked_ms":{},"version":"v9.9.9","page":"{PAGE}"}}"#, now_ms());
        write_cache(&cache, &json);
        let (url, hits) = serve("200 OK", "{}");

        assert_eq!(check_with(&url, "0.1.0", &cache), Some(Update { version: "9.9.9".into(), page: PAGE.into() }));
        assert_eq!(hits.load(Ordering::SeqCst), 0, "the old timestamp still throttles");
    }

    #[test]
    fn an_attempt_stamped_in_the_future_does_not_block_checks() {
        let s = Saved { attempted_ms: 10_000, ..Saved::default() };
        assert!(!s.is_fresh(5_000), "the clock went backwards: ask again");
        assert!(s.is_fresh(10_000 + 1));
        assert!(!s.is_fresh(10_000 + DAY_MS));
    }
}
