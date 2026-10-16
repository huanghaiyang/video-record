//! Audio capture and mixing.
//!
//! Capture threads push 16-bit PCM into per-source ring buffers. The mixer
//! pulls exactly the number of samples that belong to each video frame, so the
//! audio timeline always matches the video timeline even when the two capture
//! sources start at different moments or the encoder stalls.

pub mod ring;
pub mod wasapi_capture;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

use crate::config::AudioConfig;
pub use ring::SampleRing;

/// Sample format used across the whole application.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioFormat {
    pub fn new(sample_rate: u32, channels: u16) -> Self {
        Self { sample_rate: sample_rate.max(8_000), channels: channels.max(1) }
    }

    /// Bytes per sample frame (16-bit samples).
    pub fn block_align(&self) -> usize {
        self.channels as usize * 2
    }

    pub fn bytes_per_second(&self) -> u32 {
        self.sample_rate * self.block_align() as u32
    }
}

/// One capture source feeding the mixer.
pub struct AudioStream {
    pub name: String,
    pub ring: Arc<SampleRing>,
    pub gain: f32,
}

/// Owns the running capture threads.
pub struct AudioCapture {
    streams: Vec<AudioStream>,
    stop: Arc<AtomicBool>,
    threads: Vec<JoinHandle<()>>,
    format: AudioFormat,
    warnings: Vec<String>,
}

impl AudioCapture {
    /// Starts every enabled source. Individual sources that cannot be opened
    /// (no device, driver refusal) are reported as warnings instead of failing
    /// the whole recording.
    pub fn start(config: &AudioConfig) -> anyhow::Result<Self> {
        let format = AudioFormat::new(config.sample_rate, config.channels);
        let stop = Arc::new(AtomicBool::new(false));
        let mut streams = Vec::new();
        let mut threads = Vec::new();
        let mut warnings = Vec::new();

        // Two seconds of buffered audio is plenty; the mixer keeps up.
        let capacity = (format.bytes_per_second() as usize) * 2 / 2;

        if config.has_system() {
            let ring = Arc::new(SampleRing::new(capacity));
            match wasapi_capture::spawn(
                wasapi_capture::CaptureKind::SystemLoopback,
                format,
                ring.clone(),
                stop.clone(),
                "系统声音".to_string(),
            ) {
                Ok(handle) => {
                    threads.push(handle);
                    streams.push(AudioStream {
                        name: "系统声音（回环）".to_string(),
                        ring,
                        gain: config.system_volume,
                    });
                }
                Err(err) => {
                    let message = format!("系统声音采集不可用：{err}");
                    log::warn!("{message}");
                    warnings.push(message);
                }
            }
        }

        if config.has_microphone() {
            let ring = Arc::new(SampleRing::new(capacity));
            match wasapi_capture::spawn(
                wasapi_capture::CaptureKind::Microphone(config.microphone_device.clone()),
                format,
                ring.clone(),
                stop.clone(),
                "麦克风".to_string(),
            ) {
                Ok(handle) => {
                    threads.push(handle);
                    streams.push(AudioStream {
                        name: "麦克风".to_string(),
                        ring,
                        gain: config.microphone_volume,
                    });
                }
                Err(err) => {
                    let message = format!("麦克风采集不可用：{err}");
                    log::warn!("{message}");
                    warnings.push(message);
                }
            }
        }

        Ok(Self { streams, stop, threads, format, warnings })
    }

    pub fn streams(&self) -> &[AudioStream] {
        &self.streams
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    pub fn is_empty(&self) -> bool {
        self.streams.is_empty()
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    pub fn names(&self) -> Vec<String> {
        self.streams.iter().map(|s| s.name.clone()).collect()
    }

    /// Signals the capture threads and waits for them to finish.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        for handle in self.threads.drain(..) {
            let _ = handle.join();
        }
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Mixes all capture sources onto the video timeline.
pub struct AudioMixer {
    format: AudioFormat,
    rings: Vec<(Arc<SampleRing>, f32)>,
    sent_frames: u64,
    mix: Vec<i32>,
    out: Vec<u8>,
    resync_pending: bool,
}

impl AudioMixer {
    pub fn new(format: AudioFormat) -> Self {
        Self {
            format,
            rings: Vec::new(),
            sent_frames: 0,
            mix: Vec::new(),
            out: Vec::new(),
            resync_pending: true,
        }
    }

    pub fn from_streams(format: AudioFormat, streams: &[AudioStream]) -> Self {
        let mut mixer = Self::new(format);
        for stream in streams {
            mixer.add(stream.ring.clone(), stream.gain);
        }
        mixer
    }

    pub fn add(&mut self, ring: Arc<SampleRing>, gain: f32) {
        self.rings.push((ring, gain));
    }

    pub fn is_empty(&self) -> bool {
        self.rings.is_empty()
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// Drops everything buffered so far (used after a pause).
    pub fn request_resync(&mut self) {
        self.resync_pending = true;
    }

    /// Returns the PCM bytes that belong to output frame `frame_index`.
    ///
    /// The length is always exactly `sample_rate / fps` sample frames (rounded
    /// by the running total), padded with silence when a source is behind.
    pub fn take_for_video_frame(&mut self, frame_index: u64, fps: u32) -> &[u8] {
        self.out.clear();
        if self.rings.is_empty() || fps == 0 {
            return &self.out;
        }

        if self.resync_pending {
            for (ring, _) in &self.rings {
                ring.clear();
            }
            self.resync_pending = false;
            self.sent_frames = 0;
        }

        let target = ((frame_index as u128 + 1) * self.format.sample_rate as u128
            / fps as u128) as u64;
        let wanted_frames = target.saturating_sub(self.sent_frames) as usize;
        self.sent_frames = self.sent_frames.max(target);
        if wanted_frames == 0 {
            return &self.out;
        }

        let samples = wanted_frames * self.format.channels as usize;
        self.mix.clear();
        self.mix.resize(samples, 0);

        for (ring, gain) in &self.rings {
            let chunk = ring.take(samples);
            for (accumulator, sample) in self.mix.iter_mut().zip(chunk.iter()) {
                *accumulator += (*sample as f32 * gain) as i32;
            }
        }

        self.out.reserve(samples * 2);
        for value in &self.mix {
            let clamped = (*value).clamp(i16::MIN as i32, i16::MAX as i32) as i16;
            self.out.extend_from_slice(&clamped.to_le_bytes());
        }
        &self.out
    }

    /// Total sample frames already handed to the encoder.
    pub fn sent_frames(&self) -> u64 {
        self.sent_frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_with(samples: &[i16]) -> Arc<SampleRing> {
        let ring = Arc::new(SampleRing::new(48_000 * 4));
        ring.push(samples);
        ring
    }

    #[test]
    fn mixer_returns_exactly_one_frame_of_audio() {
        let format = AudioFormat::new(48_000, 2);
        let mut mixer = AudioMixer::new(format);
        mixer.add(ring_with(&vec![1000i16; 48_000 * 2]), 1.0);

        let first = mixer.take_for_video_frame(0, 30);
        assert_eq!(first.len(), 48_000 / 30 * 2 * 2);

        let second = mixer.take_for_video_frame(1, 30);
        assert_eq!(second.len(), 48_000 / 30 * 2 * 2);
        assert_eq!(mixer.sent_frames(), 3200);

        // One second of frames must consume one second of samples (1600*30).
        let mut mixer = AudioMixer::new(format);
        mixer.add(ring_with(&vec![0i16; 48_000 * 4]), 1.0);
        for index in 0..30 {
            mixer.take_for_video_frame(index, 30);
        }
        assert_eq!(mixer.sent_frames(), 48_000);
    }

    #[test]
    fn mixer_pads_with_silence_when_a_source_is_behind() {
        let format = AudioFormat::new(48_000, 1);
        let mut mixer = AudioMixer::new(format);
        mixer.add(Arc::new(SampleRing::new(1024)), 1.0);

        let bytes = mixer.take_for_video_frame(0, 10);
        assert_eq!(bytes.len(), 4800 * 2);
        assert!(bytes.iter().all(|b| *b == 0), "expected silence padding");
    }

    #[test]
    fn mixer_sums_sources_and_clips() {
        let format = AudioFormat::new(48_000, 1);
        let left = ring_with(&[0i16; 4800]);
        let right = ring_with(&[0i16; 4800]);

        let mut mixer = AudioMixer::new(format);
        mixer.add(left.clone(), 1.0);
        mixer.add(right.clone(), 1.0);

        // Frame 0 clears the pre-roll, so audio for frame 1 is what is pushed
        // after that point.
        mixer.take_for_video_frame(0, 10);
        left.push(&[20_000i16; 4800]);
        right.push(&[20_000i16; 4800]);

        let bytes = mixer.take_for_video_frame(1, 10).to_vec();
        let first = i16::from_le_bytes([bytes[0], bytes[1]]);
        assert_eq!(first, i16::MAX, "sum should clip instead of wrapping");
    }

    #[test]
    fn mixer_applies_gain() {
        let format = AudioFormat::new(48_000, 1);
        let ring = ring_with(&[0i16; 4800]);

        let mut mixer = AudioMixer::new(format);
        mixer.add(ring.clone(), 0.5);
        mixer.take_for_video_frame(0, 10);
        ring.push(&[1000i16; 4800]);

        let bytes = mixer.take_for_video_frame(1, 10).to_vec();
        assert_eq!(i16::from_le_bytes([bytes[0], bytes[1]]), 500);
    }

    #[test]
    fn first_frame_discards_pre_roll() {
        let format = AudioFormat::new(48_000, 1);
        let ring = ring_with(&[7777i16; 4800]);
        let mut mixer = AudioMixer::new(format);
        mixer.add(ring, 1.0);

        // Pre-roll written before recording started is dropped.
        let bytes = mixer.take_for_video_frame(0, 10).to_vec();
        assert!(bytes.iter().all(|b| *b == 0));
    }

    #[test]
    fn empty_mixer_produces_nothing() {
        let mut mixer = AudioMixer::new(AudioFormat::new(48_000, 2));
        assert!(mixer.is_empty());
        assert!(mixer.take_for_video_frame(5, 30).is_empty());
    }
}
