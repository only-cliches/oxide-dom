//! Single-finger touch gesture state: pan/tap classification + flick momentum.
//!
//! Touch is modelled as a pointer. solite's mouse-down path fires `click` on
//! press (not release), so we must decide *before* touching the document
//! whether a press is a tap or the start of a scroll — otherwise every attempt
//! to scroll content that sits inside a click handler would fire the click.
//!
//! [`Instance::classify_touch_start`](crate::Instance) inspects the hit target
//! non-mutatingly and returns a [`GestureMode`]:
//!
//! - [`GestureMode::Control`] — the press landed on a draggable control
//!   (range slider, scrollbar) or an interactive element. We run the real
//!   mouse-down immediately and forward later moves as mouse moves / the
//!   release as a mouse up.
//! - [`GestureMode::Pan`] — the press landed on plain/scrollable content.
//!   Dragging scrolls the hit node (blitz bubbles to the scrollable ancestor);
//!   a release that never travelled past [`TAP_SLOP`] is synthesised into a
//!   tap (mouse down+up), and a release after a drag seeds [`Momentum`].
//!
//! `Instance::advance_touch_momentum` integrates the fling each `tick()` until
//! friction brings it below [`MOMENTUM_MIN_SPEED`].

use std::time::Instant;

/// Distance (CSS px) a finger must travel before a press becomes a pan rather
/// than a tap.
pub(crate) const TAP_SLOP: f32 = 8.0;

/// Extra hit-test padding (CSS px) applied to small overlay controls
/// (spinners, scrollbar thumbs) on the touch path so fingers don't need pixel
/// precision. Mouse hit-testing passes `0.0` and stays exact.
pub(crate) const TOUCH_HIT_SLOP: f32 = 10.0;

/// Speed (px/s) at or below which a fling is considered stopped.
const MOMENTUM_MIN_SPEED: f32 = 6.0;

/// Fraction of velocity retained per second of coasting (exponential decay).
/// Small value ⇒ quick stop; tuned so a hard flick coasts roughly half a
/// second before dropping under [`MOMENTUM_MIN_SPEED`].
const MOMENTUM_RETENTION_PER_SEC: f32 = 0.0018;

/// Weight of the newest velocity sample in the EMA used to estimate fling
/// speed. Higher ⇒ more responsive to the final flick, less smoothing.
const VELOCITY_EMA_ALPHA: f32 = 0.7;

/// How a finger drag is routed after the initial press.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GestureMode {
    /// Press engaged a draggable control or a tappable element. Moves forward
    /// as mouse moves; the release forwards as a mouse up.
    Control,
    /// Press landed on content. Dragging pans `node_id` (blitz bubbles to the
    /// nearest scrollable ancestor); a no-move release is a tap.
    Pan { node_id: usize },
}

/// The finger currently in contact with the surface.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ActiveTouch {
    pub id: u64,
    pub mode: GestureMode,
    start: (f32, f32),
    last: (f32, f32),
    last_time: Instant,
    /// Exponential-moving-average finger velocity in px/s.
    velocity: (f32, f32),
    /// True once the finger has travelled past [`TAP_SLOP`] from `start`.
    panned: bool,
}

impl ActiveTouch {
    pub fn new(id: u64, mode: GestureMode, x: f32, y: f32, now: Instant) -> Self {
        Self {
            id,
            mode,
            start: (x, y),
            last: (x, y),
            last_time: now,
            velocity: (0.0, 0.0),
            panned: false,
        }
    }

    /// True once the finger has crossed the tap/pan threshold.
    pub fn panned(&self) -> bool {
        self.panned
    }

    /// Current smoothed velocity (px/s).
    pub fn velocity(&self) -> (f32, f32) {
        self.velocity
    }

    /// Record a move to `(x, y)` at `now`. Returns the `(dx, dy)` finger delta
    /// since the previous sample (the amount to pan by) and updates the
    /// velocity EMA and the pan/tap flag.
    pub fn record_move(&mut self, x: f32, y: f32, now: Instant) -> (f32, f32) {
        let dx = x - self.last.0;
        let dy = y - self.last.1;
        let dt = now.duration_since(self.last_time).as_secs_f32().max(1e-4);
        let inst = (dx / dt, dy / dt);
        self.velocity = (
            self.velocity.0 * (1.0 - VELOCITY_EMA_ALPHA) + inst.0 * VELOCITY_EMA_ALPHA,
            self.velocity.1 * (1.0 - VELOCITY_EMA_ALPHA) + inst.1 * VELOCITY_EMA_ALPHA,
        );
        self.last = (x, y);
        self.last_time = now;
        if !self.panned {
            let tdx = x - self.start.0;
            let tdy = y - self.start.1;
            if (tdx * tdx + tdy * tdy).sqrt() > TAP_SLOP {
                self.panned = true;
            }
        }
        (dx, dy)
    }
}

/// A coasting fling left over after a panning release.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Momentum {
    pub node_id: usize,
    /// Velocity in px/s.
    velocity: (f32, f32),
    last_tick: Instant,
}

impl Momentum {
    /// Seed a fling from a finger velocity, or `None` if it's too slow to be
    /// worth animating.
    pub fn from_velocity(node_id: usize, velocity: (f32, f32), now: Instant) -> Option<Self> {
        let speed = (velocity.0 * velocity.0 + velocity.1 * velocity.1).sqrt();
        (speed > MOMENTUM_MIN_SPEED).then_some(Self {
            node_id,
            velocity,
            last_tick: now,
        })
    }

    /// Advance the fling by the time elapsed since the last step and return the
    /// `(dx, dy)` scroll delta to apply. Decays the velocity. Call
    /// [`Momentum::is_alive`] afterwards to decide whether to keep coasting.
    pub fn step(&mut self, now: Instant) -> (f32, f32) {
        let dt = now.duration_since(self.last_tick).as_secs_f32();
        if dt <= 0.0 {
            return (0.0, 0.0);
        }
        let delta = (self.velocity.0 * dt, self.velocity.1 * dt);
        let decay = MOMENTUM_RETENTION_PER_SEC.powf(dt);
        self.velocity = (self.velocity.0 * decay, self.velocity.1 * decay);
        self.last_tick = now;
        delta
    }

    /// True while the fling is still fast enough to keep animating.
    pub fn is_alive(&self) -> bool {
        let speed = (self.velocity.0 * self.velocity.0 + self.velocity.1 * self.velocity.1).sqrt();
        speed > MOMENTUM_MIN_SPEED
    }
}

/// Touch state held on the [`Instance`](crate::Instance): the active finger (if
/// any) and any coasting fling.
#[derive(Debug, Default)]
pub(crate) struct TouchState {
    pub active: Option<ActiveTouch>,
    pub momentum: Option<Momentum>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn small_move_stays_a_tap() {
        let now = Instant::now();
        let mut t = ActiveTouch::new(1, GestureMode::Pan { node_id: 5 }, 100.0, 100.0, now);
        t.record_move(103.0, 101.0, now + Duration::from_millis(10));
        assert!(!t.panned(), "movement under TAP_SLOP must not become a pan");
    }

    #[test]
    fn move_past_slop_becomes_a_pan() {
        let now = Instant::now();
        let mut t = ActiveTouch::new(1, GestureMode::Pan { node_id: 5 }, 100.0, 100.0, now);
        let (dx, dy) = t.record_move(100.0, 130.0, now + Duration::from_millis(16));
        assert!(t.panned());
        assert_eq!(
            (dx, dy),
            (0.0, 30.0),
            "delta is the per-sample finger travel"
        );
        assert!(
            t.velocity().1 > 0.0,
            "downward fling has positive y velocity"
        );
    }

    #[test]
    fn momentum_decays_to_a_stop() {
        let mut now = Instant::now();
        let mut m =
            Momentum::from_velocity(5, (0.0, 2000.0), now).expect("fast flick seeds momentum");
        let mut steps = 0;
        while m.is_alive() && steps < 10_000 {
            now += Duration::from_millis(16);
            m.step(now);
            steps += 1;
        }
        assert!(!m.is_alive(), "friction must eventually stop the fling");
        assert!(
            steps > 0 && steps < 10_000,
            "stops in bounded time, took {steps}"
        );
    }

    #[test]
    fn slow_flick_does_not_seed_momentum() {
        let now = Instant::now();
        assert!(
            Momentum::from_velocity(5, (1.0, 2.0), now).is_none(),
            "a near-stationary release should not coast"
        );
    }
}
