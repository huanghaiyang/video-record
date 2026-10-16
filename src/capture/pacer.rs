//! Constant-frame-rate pacing for a variable-rate capture source.
//!
//! The Windows Graphics Capture API only delivers a frame when something
//! changed (bounded below by the requested minimum update interval), so a naive
//! "one captured frame = one encoded frame" pipeline produces files whose
//! duration does not match the wall-clock recording time, and whose frame rate
//! varies with the content.
//!
//! The pacer fixes that by deciding, for every captured frame, how many output
//! frames *should* exist by now. Missing frames are filled with the newest
//! pixels; surplus frames are dropped. Timestamps are derived from the frame
//! index, so the output timeline is exact.

use std::time::{Duration, Instant};

/// 100-nanosecond ticks per second (the unit used by Media Foundation).
const TICKS_PER_SECOND: i64 = 10_000_000;

#[derive(Debug, Clone)]
pub struct Pacer {
    fps: u32,
    /// Set when the first frame is actually pushed, so a slow capture start
    /// does not create a burst of filler frames.
    start: Option<Instant>,
    emitted: u64,
    /// Time spent paused, added back onto the timeline.
    paused: Duration,
}

impl Pacer {
    pub fn new(fps: u32) -> Self {
        Self {
            fps: fps.clamp(1, 240),
            start: None,
            emitted: 0,
            paused: Duration::ZERO,
        }
    }

    pub fn fps(&self) -> u32 {
        self.fps
    }

    /// Starts the timeline on the first frame.
    pub fn ensure_started(&mut self) -> bool {
        if self.start.is_none() {
            self.start = Some(Instant::now());
            return true;
        }
        false
    }

    pub fn is_started(&self) -> bool {
        self.start.is_some()
    }

    pub fn emitted(&self) -> u64 {
        self.emitted
    }

    /// Wall-clock time on the recording timeline.
    pub fn elapsed(&self) -> Duration {
        match self.start {
            Some(start) => Instant::now().saturating_duration_since(start).saturating_sub(self.paused),
            None => Duration::ZERO,
        }
    }

    /// Number of frames that should exist after `elapsed`.
    pub fn frames_due(&self, elapsed: Duration) -> u64 {
        ((elapsed.as_micros() * self.fps as u128) / 1_000_000) as u64
    }

    /// Presentation timestamp, in 100 ns units, of output frame `index`.
    pub fn timestamp_100ns(&self, index: u64) -> i64 {
        ((index as i128 * TICKS_PER_SECOND as i128) / self.fps as i128) as i64
    }

    /// Marks that the next frame was consumed from the encoder.
    pub fn advance(&mut self) {
        self.emitted += 1;
    }

    /// Shifts the timeline forward, used to skip a pause.
    pub fn add_paused(&mut self, duration: Duration) {
        self.paused += duration;
    }

    /// Clears the timeline (used when a lazy sink is created on the first frame).
    pub fn restart(&mut self) {
        self.start = Some(Instant::now());
        self.emitted = 0;
        self.paused = Duration::ZERO;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_are_due_at_the_target_rate() {
        let pacer = Pacer::new(30);
        assert_eq!(pacer.frames_due(Duration::from_millis(0)), 0);
        assert_eq!(pacer.frames_due(Duration::from_millis(999)), 29);
        assert_eq!(pacer.frames_due(Duration::from_millis(1000)), 30);
        assert_eq!(pacer.frames_due(Duration::from_secs(60)), 1800);
    }

    #[test]
    fn timestamps_are_exact_and_monotonic() {
        let pacer = Pacer::new(30);
        assert_eq!(pacer.timestamp_100ns(0), 0);
        assert_eq!(pacer.timestamp_100ns(30), TICKS_PER_SECOND);
        assert_eq!(pacer.timestamp_100ns(60), 2 * TICKS_PER_SECOND);

        let pacer = Pacer::new(60);
        assert_eq!(pacer.timestamp_100ns(60), TICKS_PER_SECOND);

        // 25 fps does not divide 10_000_000 evenly; timestamps must still rise.
        let pacer = Pacer::new(25);
        let mut previous = -1;
        for index in 0..100 {
            let ts = pacer.timestamp_100ns(index);
            assert!(ts > previous, "timestamp went backwards at {index}");
            previous = ts;
        }
    }

    #[test]
    fn odd_frame_rates_still_track_wall_clock() {
        let pacer = Pacer::new(15);
        assert_eq!(pacer.frames_due(Duration::from_secs(2)), 30);
        let pacer = Pacer::new(144);
        assert_eq!(pacer.frames_due(Duration::from_secs(1)), 144);
    }

    #[test]
    fn advancing_counts_frames() {
        let mut pacer = Pacer::new(30);
        for _ in 0..5 {
            pacer.advance();
        }
        assert_eq!(pacer.emitted(), 5);
        pacer.restart();
        assert_eq!(pacer.emitted(), 0);
    }

    #[test]
    fn paused_time_is_removed_from_the_timeline() {
        let mut pacer = Pacer::new(30);
        pacer.ensure_started();
        std::thread::sleep(Duration::from_millis(30));
        let before = pacer.elapsed();
        pacer.add_paused(before);
        assert!(pacer.elapsed() < Duration::from_millis(15));
    }

    #[test]
    fn frame_rate_is_clamped() {
        assert_eq!(Pacer::new(0).fps(), 1);
        assert_eq!(Pacer::new(10_000).fps(), 240);
    }
}
