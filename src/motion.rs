//! Time-based motion (U10, feature 006 phase A).
//!
//! Everything animated reads the wall clock, never the frame counter. A
//! tick-modulo animation changes speed the moment the frame rate varies, which is
//! exactly what happens now that the frame clock ramps between idle and animating
//! (see `FRAME_IDLE` / `FRAME_ANIMATING` in `tui.rs`). A breath that takes 900ms
//! takes 900ms at 4fps and at 60fps; only its smoothness changes.
//!
//! The vocabulary is small on purpose:
//!
//! - [`Clock`] is the one time origin, held on `App`.
//! - [`cycle`] turns elapsed time into a `0.0..1.0` position within a period.
//! - the easing functions shape that position.
//! - [`Tween`] carries a value from one number to another over a fixed duration.

use std::f32::consts::TAU;
use std::time::{Duration, Instant};

/// The braille spinner, unchanged from the tick-modulo version — only its timing
/// moved. Ten frames over [`SPIN_PERIOD`] reads as one rotation per 800ms.
pub const BRAILLE: &[&str] = &["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// One full spinner rotation.
pub const SPIN_PERIOD: Duration = Duration::from_millis(800);

/// One full breath of a live gutter. Measured off grok at ~0.7s; a touch slower
/// reads as attention rather than urgency.
pub const BREATHE_PERIOD: Duration = Duration::from_millis(900);

/// One full traversal of a [`sweep`] highlight across a label.
pub const SWEEP_PERIOD: Duration = Duration::from_millis(1600);

/// The motion time origin. One per `App`, created at startup and never reset:
/// every animation derives its position from the same instant, so two effects
/// with the same period stay in phase for the life of the process instead of
/// drifting apart by however long each waited to first appear.
#[derive(Debug, Clone, Copy)]
pub struct Clock {
    start: Instant,
}

impl Clock {
    pub fn new() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Position within a repeating cycle of `period`, in `0.0..1.0`.
    pub fn cycle(&self, period: Duration) -> f32 {
        cycle(self.elapsed(), period)
    }
}

impl Default for Clock {
    fn default() -> Self {
        Self::new()
    }
}

/// Position of `elapsed` within a repeating cycle of `period`, in `0.0..1.0`.
/// A zero period has no cycle to be anywhere in, so it pins at the start.
pub fn cycle(elapsed: Duration, period: Duration) -> f32 {
    let p = period.as_secs_f32();
    if p <= 0.0 {
        return 0.0;
    }
    let e = elapsed.as_secs_f32();
    (e % p) / p
}

/// A symmetric breath: `0.0` at the start of the cycle, `1.0` at the midpoint,
/// back to `0.0` at the end, easing at both ends rather than at the middle. This
/// is the shape that makes a pulsing gutter read as breathing instead of blinking
/// — the value lingers at the extremes, which is what you see in grok's own
/// gutter when you sample it frame by frame.
pub fn breathe(phase: f32) -> f32 {
    0.5 - 0.5 * (TAU * phase).cos()
}

/// Pick the frame of an animation strip for a cycle position. Empty strips have
/// no frame to pick, so the caller gets `None` rather than a panic on an index.
pub fn frame<'a>(frames: &[&'a str], phase: f32) -> Option<&'a str> {
    if frames.is_empty() {
        return None;
    }
    let n = frames.len();
    let i = (phase.clamp(0.0, 1.0) * n as f32) as usize;
    Some(frames[i.min(n - 1)])
}

/// Brightness of cell `i` of `width` under a highlight travelling left to right,
/// in `0.0..1.0`. `0.0` is the label's resting weight, `1.0` the peak.
///
/// This is the muse-style sweep: a triangular kernel `half` cells wide either
/// side of a moving centre, wrapping so the label never goes fully dark between
/// passes. Used for work that is *pending* — dispatched but not yet producing —
/// where a breathing gutter would claim more than we know.
pub fn sweep(i: usize, width: usize, half: f32, phase: f32) -> f32 {
    if width == 0 || half <= 0.0 {
        return 0.0;
    }
    // Travel a little past both ends so the peak spends time off-label rather
    // than snapping from the last cell back to the first.
    let span = width as f32 + 2.0 * half;
    let centre = phase.clamp(0.0, 1.0) * span - half;
    let d = (i as f32 - centre).abs();
    (1.0 - d / half).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cycle_wraps_and_stays_in_range() {
        let p = Duration::from_millis(1000);
        assert!((cycle(Duration::from_millis(0), p) - 0.0).abs() < 1e-6);
        assert!((cycle(Duration::from_millis(500), p) - 0.5).abs() < 1e-6);
        // One full period later is the same place in the cycle, not off the end.
        assert!((cycle(Duration::from_millis(1500), p) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn a_zero_period_has_no_cycle_to_be_in() {
        assert_eq!(cycle(Duration::from_secs(9), Duration::ZERO), 0.0);
    }

    #[test]
    fn breathe_peaks_at_the_midpoint_and_rests_at_both_ends() {
        assert!(breathe(0.0).abs() < 1e-6);
        assert!((breathe(0.5) - 1.0).abs() < 1e-6);
        assert!(breathe(1.0).abs() < 1e-5);
        // Symmetric about the peak.
        assert!((breathe(0.25) - breathe(0.75)).abs() < 1e-5);
    }

    /// The shape that makes it read as breathing: more time near the extremes
    /// than a linear triangle would spend there.
    #[test]
    fn breathe_lingers_at_the_extremes() {
        let near_floor = breathe(0.05);
        let near_peak = breathe(0.45);
        assert!(near_floor < 0.05, "left {near_floor}");
        assert!(near_peak > 0.95, "left {near_peak}");
    }

    #[test]
    fn frame_covers_the_strip_and_never_indexes_off_the_end() {
        let strip = ["a", "b", "c"];
        assert_eq!(frame(&strip, 0.0), Some("a"));
        assert_eq!(frame(&strip, 0.5), Some("b"));
        assert_eq!(frame(&strip, 1.0), Some("c"));
        let empty: &[&str] = &[];
        assert_eq!(frame(empty, 0.5), None);
    }

    #[test]
    fn sweep_peaks_once_and_falls_to_nothing_away_from_the_centre() {
        let w = 10;
        let half = 3.0;
        let vals: Vec<f32> = (0..w).map(|i| sweep(i, w, half, 0.5)).collect();
        let peak = vals.iter().cloned().fold(f32::MIN, f32::max);
        assert!(peak > 0.9, "no peak in {vals:?}");
        assert!(vals.iter().all(|v| (0.0..=1.0).contains(v)));
        // Only one run of lit cells: a second bright patch would read as two
        // highlights chasing each other.
        let lit: Vec<bool> = vals.iter().map(|v| *v > 0.0).collect();
        let runs = lit.windows(2).filter(|w| w[0] != w[1]).count();
        assert!(runs <= 2, "more than one lit run in {lit:?}");
    }

    #[test]
    fn sweep_is_inert_for_a_zero_width_label() {
        assert_eq!(sweep(0, 0, 3.0, 0.5), 0.0);
        assert_eq!(sweep(0, 8, 0.0, 0.5), 0.0);
    }

    #[test]
    fn tween_retargets_from_the_displayed_value_and_zero_duration_settles() {
        let start = Instant::now();
        let mut tween = Tween::<f32>::settled(0.0);
        assert!(tween.done(start));
        assert_eq!(tween.value(start), 0.0);

        let period = Duration::from_millis(200);
        tween.retarget(10.0, period, start);
        let halfway = start + period / 2;
        assert!((tween.value(halfway) - 5.0).abs() < 1e-6);

        tween.retarget(20.0, period, halfway);
        assert!(
            (tween.value(halfway) - 5.0).abs() < 1e-6,
            "retargeting must start at the rendered position, not the old endpoint"
        );
        let after_retarget = tween.value(halfway + period / 2);
        assert!(
            (5.0..20.0).contains(&after_retarget),
            "retargeted tween jumped outside its displayed-to-target interval: {after_retarget}"
        );

        tween.retarget(-3.0, Duration::ZERO, halfway);
        assert!(tween.done(halfway));
        assert_eq!(tween.value(halfway), -3.0);
    }

    #[test]
    fn ease_in_out_is_clamped_monotone_and_symmetric() {
        assert_eq!(ease_in_out(-1.0), 0.0);
        assert_eq!(ease_in_out(0.0), 0.0);
        assert!((ease_in_out(0.5) - 0.5).abs() < 1e-6);
        assert_eq!(ease_in_out(1.0), 1.0);
        assert_eq!(ease_in_out(2.0), 1.0);

        let samples: Vec<f32> = (0..=100).map(|i| ease_in_out(i as f32 / 100.0)).collect();
        assert!(
            samples.windows(2).all(|pair| pair[0] <= pair[1]),
            "ease_in_out must not reverse: {samples:?}"
        );
        for &t in &[0.1, 0.25, 0.4] {
            assert!(
                (ease_in_out(t) + ease_in_out(1.0 - t) - 1.0).abs() < 1e-6,
                "ease_in_out lost midpoint symmetry at {t}"
            );
        }
    }
}
