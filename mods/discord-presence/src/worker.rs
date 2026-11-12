// MIT License
//
// Copyright (c) 2026 Yume-13
//
// Permission is hereby granted, free of charge, to any person obtaining a copy
// of this software and associated documentation files (the "Software"), to deal
// in the Software without restriction, including without limitation the rights
// to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
// copies of the Software, and to permit persons to whom the Software is
// furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in all
// copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
// IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
// OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
// SOFTWARE.

//! Background thread that owns the Discord connection.
//!
//! The game thread only ever calls [`Worker::publish`], which is a non-blocking
//! channel send. All pipe IO, reconnects and rate limiting happen here, so
//! Discord being slow, absent or restarted can never stall a game frame.
//!
//! Rate limiting: Discord accepts about [`RATE_MAX`] updates per [`RATE_WINDOW`]
//! and drops the rest. Routine changes (score, timer) are spaced by the
//! configured interval; **urgent** ones (phase or boss changed) go out as soon
//! as the window has room, so short boss fights aren't missed.

#![forbid(unsafe_code)]

use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

use crate::ipc::Client;

/// What the game wants shown: a JSON activity object, or `None` to clear.
pub type Desired = Option<String>;

/// One request from the game thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Update {
    pub desired: Desired,
    /// Send as soon as the rate limit allows (phase or boss changed).
    pub urgent: bool,
}

/// Handle to the worker thread.
#[derive(Debug)]
pub struct Worker {
    tx: Sender<Update>,
}

/// Discord's activity rate limit: at most this many updates per window.
pub const RATE_MAX: usize = 5;
pub const RATE_WINDOW: Duration = Duration::from_secs(20);

/// Sliding-window limiter for `SET_ACTIVITY` calls.
#[derive(Debug, Default)]
pub struct RateLimit {
    sent: VecDeque<Instant>,
}

impl RateLimit {
    /// Whether another update fits in the window at `now`.
    pub fn allows(&mut self, now: Instant) -> bool {
        while self
            .sent
            .front()
            .is_some_and(|&t| now.saturating_duration_since(t) >= RATE_WINDOW)
        {
            self.sent.pop_front();
        }
        self.sent.len() < RATE_MAX
    }

    pub fn record(&mut self, now: Instant) {
        self.sent.push_back(now);
    }
}

/// Whether to send a pending change now: it must fit the rate limit, and a
/// routine (non-urgent) change also waits `interval` after the last send.
pub fn should_send(
    limit: &mut RateLimit,
    now: Instant,
    last_send: Option<Instant>,
    urgent: bool,
    interval: Duration,
) -> bool {
    limit.allows(now)
        && (urgent || last_send.is_none_or(|t| now.saturating_duration_since(t) >= interval))
}

/// Reconnect delays: first retry after 5 s, doubling up to 60 s.
pub const BACKOFF_MIN: Duration = Duration::from_secs(5);
pub const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Next backoff after a failed connect.
#[must_use]
pub fn next_backoff(current: Duration) -> Duration {
    (current * 2).clamp(BACKOFF_MIN, BACKOFF_MAX)
}

impl Worker {
    /// Start the thread. `min_interval` spaces out routine `SET_ACTIVITY`
    /// calls; urgent ones only wait for the rate limit.
    pub fn spawn(client_id: String, min_interval: Duration) -> std::io::Result<Self> {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("ndml-discord".into())
            .spawn(move || run(&client_id, min_interval, &rx))?;
        Ok(Self { tx })
    }

    /// Queue the latest desired presence. Never blocks.
    pub fn publish(&self, update: Update) {
        let _ = self.tx.send(update);
    }
}

fn run(client_id: &str, min_interval: Duration, rx: &Receiver<Update>) {
    let pid = std::process::id();
    let mut client: Option<Client> = None;
    let mut desired: Option<Desired> = None; // None = nothing requested yet
    let mut sent: Option<Desired> = None; // last activity Discord acknowledged
    let mut last_send: Option<Instant> = None;
    let mut urgent = false; // an urgent change is waiting to be sent
    let mut limit = RateLimit::default();
    let mut backoff = BACKOFF_MIN;
    let mut next_connect = Instant::now();
    let mut warned_offline = false;

    loop {
        // Wait for news, then keep only the newest request.
        match rx.recv_timeout(Duration::from_millis(250)) {
            Ok(u) => {
                urgent |= u.urgent;
                desired = Some(u.desired);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
        while let Ok(u) = rx.try_recv() {
            urgent |= u.urgent;
            desired = Some(u.desired);
        }

        let Some(want) = desired.as_ref() else {
            continue;
        };
        if sent.as_ref() == Some(want) {
            urgent = false;
            continue;
        }
        if !should_send(&mut limit, Instant::now(), last_send, urgent, min_interval) {
            continue;
        }

        if client.is_none() {
            if Instant::now() < next_connect {
                continue;
            }
            match Client::connect(client_id) {
                Ok(c) => {
                    log::info!("connected to Discord");
                    client = Some(c);
                    backoff = BACKOFF_MIN;
                    warned_offline = false;
                }
                Err(e) => {
                    if warned_offline {
                        log::debug!("Discord connect failed: {e}");
                    } else {
                        log::info!("Discord not reachable ({e}); retrying in the background");
                        warned_offline = true;
                    }
                    next_connect = Instant::now() + backoff;
                    backoff = next_backoff(backoff);
                    continue;
                }
            }
        }

        let Some(c) = client.as_mut() else { continue };
        let now = Instant::now();
        last_send = Some(now);
        limit.record(now);
        urgent = false;
        match c.set_activity(pid, want.as_deref()) {
            Ok(()) => {
                log::trace!("presence updated: {want:?}");
                sent = Some(want.clone());
            }
            Err(e) => {
                log::warn!("Discord update failed ({e}); reconnecting");
                client = None;
                sent = None;
                next_connect = Instant::now() + BACKOFF_MIN;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urgent_updates_skip_the_interval_but_not_the_rate_limit() {
        let t0 = Instant::now();
        let mut limit = RateLimit::default();
        let interval = Duration::from_secs(5);
        let mut last = None;
        // Five urgent sends in a burst are fine...
        for i in 0..RATE_MAX {
            let now = t0 + Duration::from_millis(100 * i as u64);
            assert!(
                should_send(&mut limit, now, last, true, interval),
                "burst {i}"
            );
            limit.record(now);
            last = Some(now);
        }
        // ...the sixth waits for the window.
        let now = t0 + Duration::from_secs(1);
        assert!(!should_send(&mut limit, now, last, true, interval));
        assert!(should_send(
            &mut limit,
            t0 + RATE_WINDOW,
            last,
            true,
            interval
        ));
    }

    #[test]
    fn routine_updates_wait_for_the_interval() {
        let t0 = Instant::now();
        let mut limit = RateLimit::default();
        let interval = Duration::from_secs(5);
        limit.record(t0);
        let soon = t0 + Duration::from_secs(2);
        assert!(!should_send(&mut limit, soon, Some(t0), false, interval));
        assert!(
            should_send(&mut limit, soon, Some(t0), true, interval),
            "urgent goes now"
        );
        assert!(should_send(
            &mut limit,
            t0 + interval,
            Some(t0),
            false,
            interval
        ));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(next_backoff(BACKOFF_MIN), Duration::from_secs(10));
        assert_eq!(next_backoff(Duration::from_secs(40)), BACKOFF_MAX);
        assert_eq!(next_backoff(Duration::ZERO), BACKOFF_MIN);
    }
}
