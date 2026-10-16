//! A bounded queue of 16-bit PCM samples shared between a capture thread and
//! the encoding thread.

use std::collections::VecDeque;
use std::sync::Mutex;

/// Thread-safe ring of `i16` samples.
///
/// The oldest samples are dropped when the consumer falls behind, which is the
/// right trade-off for live audio: stale samples are worse than a gap.
#[derive(Debug)]
pub struct SampleRing {
    inner: Mutex<VecDeque<i16>>,
    capacity: usize,
    dropped: Mutex<u64>,
}

impl SampleRing {
    pub fn new(capacity_samples: usize) -> Self {
        let capacity = capacity_samples.max(1024);
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity.min(1 << 20))),
            capacity,
            dropped: Mutex::new(0),
        }
    }

    /// Appends samples, discarding the oldest ones if the buffer is full.
    pub fn push(&self, samples: &[i16]) {
        if samples.is_empty() {
            return;
        }
        let mut queue = match self.inner.lock() {
            Ok(queue) => queue,
            Err(poisoned) => poisoned.into_inner(),
        };

        let overflow = queue.len() + samples.len();
        if overflow > self.capacity {
            let buffered = queue.len();
            let to_drop = (overflow - self.capacity).min(buffered);
            queue.drain(..to_drop);
            if let Ok(mut dropped) = self.dropped.lock() {
                *dropped += to_drop as u64;
            }
        }
        queue.extend(samples.iter().copied());
    }

    /// Removes and returns up to `count` samples, zero-padded to `count`.
    pub fn take(&self, count: usize) -> Vec<i16> {
        let mut out = vec![0i16; count];
        if count == 0 {
            return out;
        }
        let mut queue = match self.inner.lock() {
            Ok(queue) => queue,
            Err(poisoned) => poisoned.into_inner(),
        };
        let available = queue.len().min(count);
        for slot in out.iter_mut().take(available) {
            *slot = queue.pop_front().unwrap_or(0);
        }
        out
    }

    /// Throws away everything buffered.
    pub fn clear(&self) {
        if let Ok(mut queue) = self.inner.lock() {
            queue.clear();
        }
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|queue| queue.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Number of samples discarded because the consumer was too slow.
    pub fn dropped_samples(&self) -> u64 {
        self.dropped.lock().map(|value| *value).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn push_then_take_returns_the_same_samples() {
        let ring = SampleRing::new(1024);
        ring.push(&[1, 2, 3, 4]);
        assert_eq!(ring.len(), 4);
        assert_eq!(ring.take(4), vec![1, 2, 3, 4]);
        assert!(ring.is_empty());
    }

    #[test]
    fn take_pads_with_silence() {
        let ring = SampleRing::new(1024);
        ring.push(&[7]);
        assert_eq!(ring.take(4), vec![7, 0, 0, 0]);
    }

    #[test]
    fn overflow_drops_the_oldest_samples() {
        let ring = SampleRing::new(1024);
        ring.push(&vec![1i16; 1024]);
        ring.push(&[2, 2, 2, 2]);
        assert_eq!(ring.len(), 1024);
        assert_eq!(ring.dropped_samples(), 4);

        let data = ring.take(1024);
        assert_eq!(&data[1020..], &[2, 2, 2, 2]);
    }

    #[test]
    fn clear_empties_the_ring() {
        let ring = SampleRing::new(1024);
        ring.push(&[1, 2, 3]);
        ring.clear();
        assert!(ring.is_empty());
    }

    #[test]
    fn minimum_capacity_is_respected() {
        let ring = SampleRing::new(1);
        ring.push(&vec![0i16; 5000]);
        assert!(ring.len() >= 1024);
    }
}
