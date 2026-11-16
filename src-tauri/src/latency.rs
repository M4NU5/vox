//! Time to first sound: from the read trigger to the first sample handed to
//! the audio device. One trace runs at a time. [`start`] opens it, [`mark`]
//! records the first time each stage happens, and the first audio played
//! closes it with one log line, for example:
//!
//! ```text
//! first sound 812 ms: capture 160, open 2, chunk 0, ready 0, synth 627, decode 3, play 20
//! ```
//!
//! Each number is the milliseconds since the stage before it, so they add up
//! to the total. A stage that didn't happen (no capture for Test voice, no
//! engine wait for a cloud voice) is left out.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// The stages of a read, in the order they happen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Stage {
    /// The selection is on the clipboard and read back.
    Capture,
    /// The engine is open: reused while warm, or its process started.
    Open,
    /// The text is split into chunks.
    Chunk,
    /// A local engine has loaded its model and can take requests.
    Ready,
    /// The first chunk's audio is back from the engine, encoded.
    Synth,
    /// The first chunk's audio is decoded and on the timeline.
    Decode,
    /// The audio device took the first sample.
    Play,
}

impl Stage {
    fn label(self) -> &'static str {
        match self {
            Stage::Capture => "capture",
            Stage::Chunk => "chunk",
            Stage::Open => "open",
            Stage::Ready => "ready",
            Stage::Synth => "synth",
            Stage::Decode => "decode",
            Stage::Play => "play",
        }
    }
}

/// A finished trace.
#[derive(Clone, Debug)]
pub struct Report {
    pub total: Duration,
    /// Each stage that happened, with the time since the stage before it.
    pub stages: Vec<(Stage, Duration)>,
}

impl Report {
    /// The time spent in `stage`, zero if it didn't happen.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn stage(&self, stage: Stage) -> Duration {
        self.stages
            .iter()
            .find(|(s, _)| *s == stage)
            .map_or(Duration::ZERO, |(_, d)| *d)
    }
}

impl std::fmt::Display for Report {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "first sound {} ms:", self.total.as_millis())?;
        for (i, (stage, d)) in self.stages.iter().enumerate() {
            let sep = if i == 0 { " " } else { ", " };
            write!(f, "{sep}{} {}", stage.label(), d.as_millis())?;
        }
        Ok(())
    }
}

struct Trace {
    started: Instant,
    marks: Vec<(Stage, Instant)>,
}

/// Set while a trace is open, so the audio thread checks it without a lock.
static OPEN: AtomicBool = AtomicBool::new(false);
static TRACE: Mutex<Option<Trace>> = Mutex::new(None);
static LAST: Mutex<Option<Report>> = Mutex::new(None);

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Open a trace for a read triggered at `at`, replacing any still open.
pub fn start(at: Instant) {
    lock(&LAST).take();
    *lock(&TRACE) = Some(Trace {
        started: at,
        marks: Vec::new(),
    });
    OPEN.store(true, Ordering::Release);
}

/// Record `stage` now, if a trace is open and it hasn't been recorded yet.
/// Later chunks pass through the same stages and are ignored.
pub fn mark(stage: Stage) {
    if !OPEN.load(Ordering::Acquire) {
        return;
    }
    if let Some(trace) = lock(&TRACE).as_mut() {
        if !trace.marks.iter().any(|(s, _)| *s == stage) {
            trace.marks.push((stage, Instant::now()));
        }
    }
}

/// The audio device took its first sample: close the trace and log it.
pub fn first_sound() {
    if !OPEN.swap(false, Ordering::AcqRel) {
        return;
    }
    let Some(mut trace) = lock(&TRACE).take() else {
        return;
    };
    trace.marks.push((Stage::Play, Instant::now()));
    let report = report(&trace);
    log::info!("{report}");
    *lock(&LAST) = Some(report);
}

/// Drop the open trace: the read was stopped or failed before any sound.
pub fn cancel() {
    OPEN.store(false, Ordering::Release);
    lock(&TRACE).take();
}

/// The last finished trace.
#[cfg_attr(not(test), allow(dead_code))]
pub fn last() -> Option<Report> {
    lock(&LAST).clone()
}

fn report(trace: &Trace) -> Report {
    let mut marks = trace.marks.clone();
    marks.sort_by_key(|(s, _)| *s);
    let mut prev = trace.started;
    let stages = marks
        .iter()
        .map(|&(stage, at)| {
            let d = at.saturating_duration_since(prev);
            prev = prev.max(at);
            (stage, d)
        })
        .collect();
    Report {
        total: prev.saturating_duration_since(trace.started),
        stages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn stages_add_up_to_the_total_in_stage_order() {
        let t0 = Instant::now();
        let trace = Trace {
            started: t0,
            // Recorded out of order, as threads can.
            marks: vec![
                (Stage::Synth, t0 + ms(700)),
                (Stage::Capture, t0 + ms(150)),
                (Stage::Play, t0 + ms(720)),
                (Stage::Chunk, t0 + ms(151)),
            ],
        };
        let r = report(&trace);
        assert_eq!(r.total, ms(720));
        assert_eq!(r.stage(Stage::Capture), ms(150));
        assert_eq!(r.stage(Stage::Synth), ms(549));
        assert_eq!(r.stage(Stage::Open), Duration::ZERO);
        assert_eq!(
            r.to_string(),
            "first sound 720 ms: capture 150, chunk 1, synth 549, play 20"
        );
    }
}
