//! Spacing a backend's requests out, for services that limit requests per second or minute.

use crate::model::{ChatModel, Delta, Stop, TurnRequest, TurnResult};
use craft_anything_evidence::ToolError;
use craft_anything_platform::sync::Mutex;
use std::collections::BTreeMap;
use std::sync::PoisonError;
use std::time::{Duration, Instant};

/// When each backend may send its next request. One table for the whole process, so every
/// conversation on a backend (a crew's workers, a build and a chat) stays inside its limit together.
static NEXT: Mutex<BTreeMap<String, Instant>> = Mutex::new(BTreeMap::new());

/// The wait between requests for a `requests_per_minute` setting; none when it's unset or zero.
pub(crate) fn gap(requests_per_minute: Option<u32>) -> Option<Duration> {
    requests_per_minute.filter(|n| *n > 0).map(|n| Duration::from_secs(60) / n)
}

/// Waits shorter than this pass without a word in the feed.
const SAY_WAIT: Duration = Duration::from_secs(2);

/// What the feed says while a request waits for its backend's rate limit.
fn waiting(backend: &str, wait: Duration) -> String {
    format!("Waiting {} s before the next request to {backend}, to stay within its requests-per-minute limit…", wait.as_secs_f64().ceil() as u64)
}

/// A model whose requests start at least `gap` apart.
pub(crate) struct Paced {
    pub inner: Box<dyn ChatModel>,
    pub backend: String,
    pub gap: Duration,
}

impl ChatModel for Paced {
    fn turn(&mut self, req: &TurnRequest<'_>, on_delta: &mut dyn FnMut(Delta), stop: &Stop) -> Result<TurnResult, ToolError> {
        // Take the next free time and book the one after it, so requests waiting together queue up.
        let at = {
            let mut next = NEXT.lock().unwrap_or_else(PoisonError::into_inner);
            let now = Instant::now();
            let at = next.get(&self.backend).map_or(now, |t| (*t).max(now));
            next.insert(self.backend.clone(), at + self.gap);
            at
        };
        // A wait the person would notice is said in the feed, so a paced build doesn't look stuck (#668).
        let wait = at.saturating_duration_since(Instant::now());
        if wait >= SAY_WAIT {
            on_delta(Delta::Status(waiting(&self.backend, wait)));
        }
        // A stop ends the wait; the model itself then answers it as it does any stopped turn.
        while !stop.is_stopped()
            && let Some(left) = at.checked_duration_since(Instant::now()).filter(|d| !d.is_zero())
        {
            std::thread::sleep(left.min(Duration::from_millis(100)));
        }
        self.inner.turn(req, on_delta, stop)
    }

    fn model(&self) -> String {
        self.inner.model()
    }

    fn context_window(&self) -> Option<crate::window::Window> {
        self.inner.context_window()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Effort;

    struct Refuses;

    impl ChatModel for Refuses {
        fn turn(&mut self, _: &TurnRequest<'_>, _: &mut dyn FnMut(Delta), _: &Stop) -> Result<TurnResult, ToolError> {
            Err(ToolError::internal("asked"))
        }

        fn model(&self) -> String {
            "m".into()
        }

        fn context_window(&self) -> Option<crate::window::Window> {
            Some(crate::window::Window { tokens: 32_768, source: "test" })
        }
    }

    /// Send one request; what it said in the feed while it waited.
    fn ask(backend: &str, gap: Duration, stop: &Stop) -> Vec<String> {
        let mut paced = Paced { inner: Box::new(Refuses), backend: backend.into(), gap };
        assert_eq!(paced.model(), "m");
        let req = TurnRequest { system: "", messages: &[], tools: &[], effort: Effort::Low, max_tokens: 1 };
        let mut said = Vec::new();
        let mut on_delta = |d| {
            if let Delta::Status(text) = d {
                said.push(text);
            }
        };
        assert_eq!(paced.turn(&req, &mut on_delta, stop).unwrap_err().message, "asked", "the request is still sent");
        said
    }

    /// #668: a request held back for its rate limit says so in the feed; one sent at once says nothing.
    #[test]
    fn a_wait_for_the_limit_is_said() {
        assert!(ask("pace-test-said", Duration::from_secs(60), &Stop::default()).is_empty(), "the first goes at once");
        let stop = Stop::default();
        stop.stop();
        assert_eq!(
            ask("pace-test-said", Duration::from_secs(60), &stop),
            ["Waiting 60 s before the next request to pace-test-said, to stay within its requests-per-minute limit…"]
        );
        assert!(ask("pace-test-short", Duration::from_millis(500), &Stop::default()).is_empty());
        assert!(ask("pace-test-short", Duration::from_millis(500), &Stop::default()).is_empty(), "half a second isn't worth a line");
    }

    #[test]
    fn the_servers_window_is_still_read_through_the_pacing() {
        let paced = Paced { inner: Box::new(Refuses), backend: "window".into(), gap: Duration::from_secs(60) };
        assert_eq!(paced.context_window().map(|w| w.tokens), Some(32_768));
    }

    #[test]
    fn a_rate_becomes_a_gap() {
        assert_eq!(gap(None), None);
        assert_eq!(gap(Some(0)), None);
        assert_eq!(gap(Some(120)), Some(Duration::from_millis(500)));
        assert_eq!(gap(Some(1)), Some(Duration::from_secs(60)));
    }

    #[test]
    fn requests_to_one_backend_start_a_gap_apart() {
        let gap = Duration::from_millis(150);
        let start = Instant::now();
        // Separate conversations build separate models: the backend's name is what they share.
        for _ in 0..3 {
            ask("pace-test-spaced", gap, &Stop::default());
        }
        assert!(start.elapsed() >= gap * 2, "three requests took {:?}", start.elapsed());
        let other = Instant::now();
        ask("pace-test-other", gap, &Stop::default());
        assert!(other.elapsed() < gap, "another backend doesn't wait");
    }

    #[test]
    fn a_stop_ends_the_wait() {
        let stop = Stop::default();
        ask("pace-test-stopped", Duration::from_secs(60), &stop);
        stop.stop();
        let start = Instant::now();
        ask("pace-test-stopped", Duration::from_secs(60), &stop);
        assert!(start.elapsed() < Duration::from_secs(5));
    }
}
