//! Polling an asynchronous job until it finishes, fails, times out or is cancelled.

use std::time::Duration;

use crate::{CancelToken, GenError, HttpRequest, HttpResponse, HttpTransport, Result};

/// How long a provider waits for a job by default.
pub const LIMIT: Duration = Duration::from_secs(120);
const FIRST: Duration = Duration::from_millis(500);
const LONGEST: Duration = Duration::from_secs(3);
/// Sleeps are cut into slices this long so a cancel is noticed quickly.
const SLICE: Duration = Duration::from_millis(100);

/// Time, injectable for tests.
pub trait Clock: Send + Sync {
    /// Time since some fixed start.
    fn now(&self) -> Duration;
    fn sleep(&self, d: Duration);
}

/// The real clock. `std::time::Instant` is unavailable on wasm32-unknown-unknown (it panics
/// there), which is fine: providers, and so this clock, are only built where a transport exists
/// (the desktop app).
pub struct SystemClock {
    start: std::time::Instant,
}

impl SystemClock {
    pub fn new() -> Self {
        Self { start: std::time::Instant::now() }
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.start.elapsed()
    }
    fn sleep(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// What one status reply said.
pub enum Step<T> {
    Pending,
    Done(T),
}

fn nap(clock: &dyn Clock, d: Duration, cancel: &CancelToken) -> Result<()> {
    let mut left = d;
    while !left.is_zero() {
        cancel.check()?;
        let s = left.min(SLICE);
        clock.sleep(s);
        left = left.saturating_sub(s);
    }
    cancel.check()
}

/// Sends `request()` until `parse` says done (or fails), waiting 0.5 s growing ×1.5 to 3 s
/// between tries. `Err(Timeout)` once `limit` would pass; `Err(Cancelled)` as soon as `cancel`
/// is set.
pub fn until<T>(
    http: &dyn HttpTransport,
    request: &dyn Fn() -> HttpRequest,
    parse: &mut dyn FnMut(&HttpResponse) -> Result<Step<T>>,
    limit: Duration,
    cancel: &CancelToken,
    clock: &dyn Clock,
) -> Result<T> {
    let start = clock.now();
    let mut wait = FIRST;
    loop {
        cancel.check()?;
        let reply = http.send(request())?;
        if let Step::Done(v) = parse(&reply)? {
            return Ok(v);
        }
        if clock.now().saturating_sub(start) + wait > limit {
            return Err(GenError::Timeout);
        }
        nap(clock, wait, cancel)?;
        wait = wait.mul_f32(1.5).min(LONGEST);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::*;
    use serde_json::json;

    fn status(r: &HttpResponse) -> Result<Step<String>> {
        let v: serde_json::Value = serde_json::from_slice(&r.body).unwrap();
        Ok(match v["status"].as_str().unwrap() {
            "done" => Step::Done("ok".into()),
            "bad" => return Err(GenError::Refused("bad".into())),
            _ => Step::Pending,
        })
    }

    #[test]
    fn polls_until_done_with_growing_waits() {
        let http = FakeHttp::new(vec![
            json_reply(200, json!({"status": "wait"})),
            json_reply(200, json!({"status": "wait"})),
            json_reply(200, json!({"status": "done"})),
        ]);
        let clock = FakeClock::default();
        let out = until(&*http, &|| HttpRequest::get("https://api.example/s"), &mut status, LIMIT, &CancelToken::default(), &clock).unwrap();
        assert_eq!(out, "ok");
        assert_eq!(http.requests().len(), 3);
        assert_eq!(clock.now(), Duration::from_millis(500 + 750), "0.5 s, then ×1.5");
    }

    #[test]
    fn failures_stop_polling_and_the_deadline_times_out() {
        let http = FakeHttp::new(vec![json_reply(200, json!({"status": "bad"}))]);
        let e = until(&*http, &|| HttpRequest::get("https://api.example/s"), &mut status, LIMIT, &CancelToken::default(), &FakeClock::default());
        assert_eq!(e, Err(GenError::Refused("bad".into())));

        let replies = (0..200).map(|_| json_reply(200, json!({"status": "wait"}))).collect();
        let http = FakeHttp::new(replies);
        let e =
            until(&*http, &|| HttpRequest::get("https://api.example/s"), &mut status, Duration::from_secs(10), &CancelToken::default(), &FakeClock::default());
        assert_eq!(e, Err(GenError::Timeout));
    }

    #[test]
    fn cancelling_stops_before_the_next_request() {
        let http = FakeHttp::new(vec![json_reply(200, json!({"status": "wait"})), json_reply(200, json!({"status": "done"}))]);
        let sent = http.clone();
        let cancel = CancelToken::new(move || !sent.requests().is_empty());
        let e = until(&*http, &|| HttpRequest::get("https://api.example/s"), &mut status, LIMIT, &cancel, &FakeClock::default());
        assert_eq!(e, Err(GenError::Cancelled));
        assert_eq!(http.requests().len(), 1);
    }
}
