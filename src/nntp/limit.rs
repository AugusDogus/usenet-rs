//! How many background requests the pool keeps in flight.
//!
//! Requests on one link share its bandwidth, so every request in flight slows the others.
//! An urgent request started while hundreds of background ones stream gets a hundredth of
//! the link. Yet a download needs enough in flight to fill the link, and how many that is
//! depends on the link's speed, each connection's speed and the latency, none of which the
//! pool knows. So it finds out: it measures throughput while background requests are held to
//! a limit, and moves the limit to the smallest that keeps throughput within a few percent of
//! the best it has seen.
//!
//! The limit starts low and doubles while doubling raises throughput by a fifth or more. Then
//! it probes: now and then it tries three quarters of the limit, and keeps it if throughput
//! holds; otherwise it tries a third more, and keeps that if throughput rises. Each setting is
//! given a window to settle, since requests already in flight finish at the old one, and
//! measured over two more. Windows in which the limit did not hold anything back measure the
//! demand, not the link, so they do not count. Windows with urgent requests do: their bytes
//! cross the same link, and a limit tuned only while nothing is urgent keeps the download's
//! share where it was while a seek waits.

use std::time::{Duration, Instant};

/// How long throughput is counted for at a time.
pub(crate) const WINDOW: Duration = Duration::from_secs(1);
/// Windows skipped after the limit changes, while requests sent before it finish.
const SETTLE: u32 = 1;
/// Windows averaged to measure one setting.
const MEASURE: usize = 2;
/// Measurements at a setting before probing another.
const HOLD: u32 = 5;
/// The least the limit goes down to.
const FLOOR: usize = 2;
/// Where the limit starts.
const START: usize = 8;
/// How long the best throughput seen is remembered, so the limit follows a link that slows.
const MEMORY: Duration = Duration::from_secs(60);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    /// Doubling; the throughput at the setting before.
    Startup(Option<u64>),
    /// Holding a setting, after this many measurements; probing down next if `down`.
    Hold { measured: u32, down: bool },
    /// Trying a setting, from the one held before.
    Probe { from: usize, down: bool },
}

/// The window being counted.
#[derive(Clone, Copy, Debug)]
struct Window {
    start: Instant,
    /// Bytes the pool had received when the window began.
    received: u64,
    /// The limit held a background request back.
    bound: bool,
}

/// The limit and how it is being tuned.
#[derive(Debug)]
pub(crate) struct Limit {
    limit: usize,
    ceiling: usize,
    phase: Phase,
    window: Window,
    settling: u32,
    samples: Vec<u64>,
    /// Throughput, in bytes per second, at the setting held.
    held: u64,
    /// The best throughput measured lately, and when.
    best: Option<(u64, Instant)>,
}

impl Limit {
    /// A limit that never goes past `ceiling`, the pool's capacity.
    pub(crate) fn new(ceiling: usize, now: Instant) -> Self {
        let ceiling = ceiling.max(1);
        Self {
            limit: START.min(ceiling),
            ceiling,
            phase: Phase::Startup(None),
            window: Window { start: now, received: 0, bound: false },
            settling: 0,
            samples: Vec::new(),
            held: 0,
            best: None,
        }
    }

    pub(crate) fn get(&self) -> usize {
        self.limit
    }

    /// A background request had to wait for the limit.
    pub(crate) fn bound(&mut self) {
        self.window.bound = true;
    }

    /// Ends the window if it is over, given `received`, the bytes the pool has received in
    /// all, at `now`. Returns true when the limit changed.
    pub(crate) fn tick(&mut self, received: u64, now: Instant) -> bool {
        let elapsed = now.saturating_duration_since(self.window.start);
        if elapsed < WINDOW {
            return false;
        }
        let w = self.window;
        self.window = Window { start: now, received, bound: false };
        if !w.bound {
            // Measure this setting afresh from here.
            self.samples.clear();
            return false;
        }
        if self.settling > 0 {
            self.settling -= 1;
            return false;
        }
        self.samples.push((received.saturating_sub(w.received) as f64 / elapsed.as_secs_f64()) as u64);
        if self.samples.len() < MEASURE {
            return false;
        }
        let rate = self.samples.iter().sum::<u64>() / self.samples.len() as u64;
        self.samples.clear();
        let before = self.limit;
        self.decide(rate, now);
        if self.limit != before {
            self.settling = SETTLE;
        }
        self.limit != before
    }

    fn decide(&mut self, rate: u64, now: Instant) {
        let best = match self.best {
            Some((b, at)) if b >= rate && now.saturating_duration_since(at) < MEMORY => b,
            _ => {
                self.best = Some((rate, now));
                rate
            }
        };
        match self.phase {
            Phase::Startup(before) => {
                if self.limit < self.ceiling && before.is_none_or(|b| rate >= b + b / 5) {
                    self.phase = Phase::Startup(Some(rate));
                    self.limit = (self.limit * 2).min(self.ceiling);
                } else {
                    // Doubling stopped paying; the last doubling may have overshot, which the
                    // probes down will find.
                    self.held = rate;
                    self.phase = Phase::Hold { measured: 1, down: true };
                }
            }
            Phase::Hold { measured, down } => {
                self.held = rate;
                if measured + 1 < HOLD {
                    self.phase = Phase::Hold { measured: measured + 1, down };
                    return;
                }
                let to =
                    if down { (self.limit * 3 / 4).max(FLOOR) } else { (self.limit * 4 / 3 + 1).min(self.ceiling) };
                if to == self.limit {
                    self.phase = Phase::Hold { measured: 0, down: !down };
                    return;
                }
                self.phase = Phase::Probe { from: self.limit, down };
                self.limit = to;
            }
            Phase::Probe { from, down } => {
                let kept = if down { rate * 100 >= best * 97 } else { rate * 100 >= self.held * 105 };
                if kept {
                    // Keep going the same way soon.
                    self.held = rate;
                    self.phase = Phase::Hold { measured: HOLD - 1, down };
                } else {
                    self.limit = from;
                    self.phase = Phase::Hold { measured: 0, down: !down };
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A link whose throughput rises with requests in flight up to `knee`, then stays flat.
    fn settle(knee: usize, ceiling: usize, seconds: u64) -> Vec<usize> {
        let mut now = Instant::now();
        let mut limit = Limit::new(ceiling, now);
        let mut seen = Vec::new();
        let (per_request, mut received) = (1_000_000u64, 0);
        for _ in 0..seconds * 10 {
            now += Duration::from_millis(100);
            limit.bound();
            received += per_request * limit.get().min(knee) as u64 / 10;
            limit.tick(received, now);
            seen.push(limit.get());
        }
        seen
    }

    #[test]
    fn the_limit_settles_near_the_knee_and_stays() {
        for (knee, ceiling) in [(3, 400), (12, 400), (50, 400), (300, 400), (32, 32)] {
            let seen = settle(knee, ceiling, 300);
            let late = &seen[seen.len() - 600..];
            let (lo, hi) = (*late.iter().min().unwrap(), *late.iter().max().unwrap());
            // Never below a probe's step under the knee, nor far above it.
            assert!(lo >= (knee * 3 / 4).max(FLOOR), "knee {knee}: dipped to {lo}");
            assert!(hi <= (knee * 2).max(FLOOR + 1).min(ceiling), "knee {knee}: rose to {hi}");
            let mean = late.iter().sum::<usize>() as f64 / late.len() as f64;
            let throughput = late.iter().map(|&l| l.min(knee)).sum::<usize>() as f64 / late.len() as f64;
            assert!(throughput >= knee as f64 * 0.93, "knee {knee}: throughput {throughput}, limit {mean}");
        }
    }

    #[test]
    fn windows_the_limit_did_not_bind_change_nothing() {
        let mut now = Instant::now();
        let mut limit = Limit::new(400, now);
        for k in 0..100 {
            now += Duration::from_millis(500);
            assert!(!limit.tick(1_000_000 * k, now));
        }
        assert_eq!(limit.get(), START);
    }
}
