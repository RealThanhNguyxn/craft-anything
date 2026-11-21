//! Calling again when a call was interrupted. On Unix a blocking socket call returns
//! `ErrorKind::Interrupted` (EINTR) when a signal reaches its thread, such as a child process of
//! another thread ending; that says nothing about whoever is at the other end.

use std::time::{Duration, Instant};

/// Run `call` with the time left before `deadline`, and run it again while it fails with an error
/// `interrupted` says was an interrupted call, up to `tries` calls in all and only while time is
/// left. The first call always runs; any other error, the last interrupted one, or the result is
/// returned as it is. Nothing waits between calls.
///
/// Call again only what is safe to repeat: an interrupted call may already have reached the other end.
pub fn when_interrupted<T, E>(tries: u32, deadline: Instant, mut call: impl FnMut(Duration) -> Result<T, E>, interrupted: impl Fn(&E) -> bool) -> Result<T, E> {
    let mut made = 0u32;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        made = made.saturating_add(1);
        match call(left) {
            Err(e) if interrupted(&e) && made < tries && Instant::now() < deadline => continue,
            other => return other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Error, ErrorKind};

    fn is_interrupted(e: &Error) -> bool {
        e.kind() == ErrorKind::Interrupted
    }

    #[test]
    fn calls_again_until_it_answers() {
        let mut made = 0;
        let got = when_interrupted(
            3,
            Instant::now() + Duration::from_secs(60),
            |_| {
                made += 1;
                if made < 3 { Err(Error::from(ErrorKind::Interrupted)) } else { Ok("answer") }
            },
            is_interrupted,
        );
        assert_eq!(got.unwrap(), "answer");
        assert_eq!(made, 3);
    }

    #[test]
    fn gives_up_after_the_limit_with_the_interrupted_error() {
        let mut made = 0;
        let got: Result<(), Error> = when_interrupted(
            3,
            Instant::now() + Duration::from_secs(60),
            |_| {
                made += 1;
                Err(Error::new(ErrorKind::Interrupted, format!("try {made}")))
            },
            is_interrupted,
        );
        let e = got.unwrap_err();
        assert_eq!((e.kind(), e.to_string(), made), (ErrorKind::Interrupted, "try 3".to_string(), 3));
    }

    #[test]
    fn other_errors_are_not_called_again() {
        let mut made = 0;
        let got: Result<(), Error> = when_interrupted(
            3,
            Instant::now() + Duration::from_secs(60),
            |_| {
                made += 1;
                Err(Error::from(ErrorKind::ConnectionRefused))
            },
            is_interrupted,
        );
        assert_eq!((got.unwrap_err().kind(), made), (ErrorKind::ConnectionRefused, 1));
    }

    #[test]
    fn stops_once_the_deadline_has_passed() {
        let mut made = 0;
        let mut given = Vec::new();
        let past = Instant::now().checked_sub(Duration::from_secs(1)).unwrap_or_else(Instant::now);
        let started = Instant::now();
        let got: Result<(), Error> = when_interrupted(
            3,
            past,
            |left| {
                made += 1;
                given.push(left);
                Err(Error::from(ErrorKind::Interrupted))
            },
            is_interrupted,
        );
        assert_eq!((got.unwrap_err().kind(), made), (ErrorKind::Interrupted, 1));
        assert_eq!(given, [Duration::ZERO]);
        assert!(started.elapsed() < Duration::from_secs(1), "nothing waits between calls");
    }

    #[test]
    fn each_call_gets_only_the_time_left() {
        let deadline = Instant::now() + Duration::from_secs(60);
        let mut given = Vec::new();
        let _: Result<(), Error> = when_interrupted(
            3,
            deadline,
            |left| {
                given.push(left);
                Err(Error::from(ErrorKind::Interrupted))
            },
            is_interrupted,
        );
        assert_eq!(given.len(), 3);
        assert!(given.iter().all(|left| *left <= Duration::from_secs(60)));
        assert!(given.windows(2).all(|w| w[1] <= w[0]), "{given:?}");
    }
}
