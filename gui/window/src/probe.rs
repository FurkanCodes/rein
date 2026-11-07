//! What the window measures of itself. A `canvas` in the root view paints
//! once a drawn frame, so frames are counted without causing any: an idle
//! window that asked for frames to count them would never be idle.

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

#[derive(Default)]
struct Samples {
    frames: u64,
    /// Time between drawn frames that came one after another.
    gaps: Vec<Duration>,
    last_frame: Option<Instant>,
    /// From a key going in to the frame that shows it.
    keys: Vec<Duration>,
    key_pending: Option<Instant>,
    /// How late a timer on the UI thread woke: how long that thread was busy.
    late: Vec<Duration>,
    /// From the start to the frame with everything in it.
    settled: Option<Duration>,
    settle_next: bool,
    /// When the first of it came, which is where opening starts.
    first_taken: Option<Instant>,
    /// Frames drawn by the time the mark was set.
    frames_at_mark: Option<u64>,
    /// Frames drawn in each second since the mark.
    since_mark: Vec<u32>,
    marked_at: Option<Instant>,
    /// What a scripted run found, by name.
    notes: serde_json::Map<String, Value>,
    /// Pointer moves over the window in each second since the mark: a window
    /// has to draw hover, so a frame then isn't a frame nobody asked for.
    pointer: Vec<u32>,
}

#[derive(Clone)]
pub(crate) struct Probe {
    started: Instant,
    samples: Rc<RefCell<Samples>>,
}

/// What a gap counts as: two frames with this between them weren't drawn
/// back to back, so there was nothing to draw between them.
const IDLE_GAP: Duration = Duration::from_millis(100);

/// A frame on this Mac's display, at 120 Hz.
const FRAME: Duration = Duration::from_micros(8_333);

fn ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

/// The value `share` of the way up `sorted`.
fn at(sorted: &[Duration], share: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    #[expect(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_precision_loss, reason = "an index into a list of samples")]
    let index = ((sorted.len() - 1) as f64 * share).round() as usize;
    ms(sorted[index])
}

fn summary(mut values: Vec<Duration>) -> Value {
    values.sort();
    json!({ "count": values.len(), "p50_ms": at(&values, 0.5), "p99_ms": at(&values, 0.99), "max_ms": at(&values, 1.0) })
}

impl Probe {
    pub(crate) fn new() -> Probe {
        Probe { started: Instant::now(), samples: Rc::default() }
    }

    /// A frame was drawn.
    pub(crate) fn painted(&self) {
        let mut samples = self.samples.borrow_mut();
        let now = Instant::now();
        samples.frames += 1;
        if let Some(marked) = samples.marked_at {
            let second = usize::try_from(now.duration_since(marked).as_secs()).unwrap_or(usize::MAX);
            if samples.since_mark.len() <= second {
                samples.since_mark.resize(second + 1, 0);
            }
            samples.since_mark[second] += 1;
        }
        if let Some(last) = samples.last_frame.replace(now) {
            let gap = now.duration_since(last);
            // A run that's marked draws all along, so a gap there is a freeze.
            if gap < IDLE_GAP || samples.marked_at.is_some() {
                samples.gaps.push(gap);
            }
        }
        if let Some(key) = samples.key_pending.take() {
            samples.keys.push(now.duration_since(key));
        }
        if std::mem::take(&mut samples.settle_next) {
            let settled = now.duration_since(samples.first_taken.unwrap_or(self.started));
            samples.settled.get_or_insert(settled);
        }
    }

    /// Keeps `value` under `name`, for the report.
    pub(crate) fn note(&self, name: &str, value: Value) {
        self.samples.borrow_mut().notes.insert(name.to_owned(), value);
    }

    pub(crate) fn pointer_moved(&self) {
        let mut samples = self.samples.borrow_mut();
        if let Some(marked) = samples.marked_at {
            let second = usize::try_from(marked.elapsed().as_secs()).unwrap_or(usize::MAX);
            if samples.pointer.len() <= second {
                samples.pointer.resize(second + 1, 0);
            }
            samples.pointer[second] += 1;
        }
    }

    pub(crate) fn key_sent(&self) {
        // Kept, if the last key isn't shown yet: the slowest would go uncounted.
        self.samples.borrow_mut().key_pending.get_or_insert_with(Instant::now);
    }

    pub(crate) fn woke_late(&self, by: Duration) {
        self.samples.borrow_mut().late.push(by);
    }

    /// Something's come to open. Only the first time counts.
    pub(crate) fn first_taken(&self) {
        self.samples.borrow_mut().first_taken.get_or_insert_with(Instant::now);
    }

    /// The next frame drawn is the one with everything in it.
    pub(crate) fn settle_on_next_frame(&self) {
        self.samples.borrow_mut().settle_next = true;
    }

    /// Whatever's drawn from here on is counted apart, and so are gaps.
    pub(crate) fn mark(&self) {
        let mut samples = self.samples.borrow_mut();
        samples.frames_at_mark = Some(samples.frames);
        samples.marked_at = Some(Instant::now());
        samples.gaps.clear();
        samples.late.clear();
    }

    pub(crate) fn is_settled(&self) -> bool {
        self.samples.borrow().settled.is_some()
    }

    /// Everything so far, which goes in a file at the end of a run.
    pub(crate) fn report(&self) -> Value {
        let samples = self.samples.borrow();
        // Apple's measure of a hitch: time past each frame's due, over time.
        let hitches: Duration = samples.gaps.iter().filter(|gap| **gap > FRAME.mul_f64(1.5)).map(|gap| gap.saturating_sub(FRAME)).sum();
        let spent: Duration = samples.gaps.iter().sum();
        json!({
            "seconds": self.started.elapsed().as_secs_f64(),
            "frames": samples.frames,
            "frames_after_mark": samples.frames_at_mark.map(|at| samples.frames - at),
            "frames_each_second": samples.since_mark,
            "pointer_each_second": samples.pointer,
            "notes": samples.notes,
            "hitch_ms_per_s": if spent.is_zero() { 0.0 } else { ms(hitches) / spent.as_secs_f64() },
            "settled_ms": samples.settled.map(ms),
            "frame_gap": summary(samples.gaps.clone()),
            "key_to_paint": summary(samples.keys.clone()),
            "ui_thread_late": summary(samples.late.clone()),
        })
    }
}
