//! WASAPI capture threads: system loopback and microphone.
//!
//! Both sources are opened in shared event-driven mode with automatic format
//! conversion, so the same 16-bit PCM layout reaches the encoder regardless of
//! what the audio driver's mix format happens to be.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use wasapi::{
    initialize_mta, AudioCaptureClient, AudioClient, Device, DeviceEnumerator, Direction, Handle,
    SampleType, StreamMode, WaveFormat,
};

use super::{AudioFormat, SampleRing};

/// Which endpoint to capture from.
#[derive(Debug, Clone)]
pub enum CaptureKind {
    /// Whatever the machine is playing (a loopback stream on the render device).
    SystemLoopback,
    /// A recording endpoint; the string is a substring of the device name, or
    /// empty for the system default.
    Microphone(String),
}

impl CaptureKind {
    fn label(&self) -> String {
        match self {
            Self::SystemLoopback => "系统声音（回环）".to_string(),
            Self::Microphone(name) if name.is_empty() => "默认麦克风".to_string(),
            Self::Microphone(name) => format!("麦克风 {name}"),
        }
    }
}

/// Starts a capture thread.
///
/// Waits for the stream to be opened so that device errors are reported to the
/// caller instead of vanishing into the thread.
pub fn spawn(
    kind: CaptureKind,
    format: AudioFormat,
    ring: Arc<SampleRing>,
    stop: Arc<AtomicBool>,
    thread_name: String,
) -> anyhow::Result<JoinHandle<()>> {
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<anyhow::Result<()>>();
    let label = kind.label();

    let handle = std::thread::Builder::new()
        .name(format!("audio-{thread_name}"))
        .spawn(move || run(kind, format, ring, stop, ready_tx, label))
        .map_err(|err| anyhow::anyhow!("无法创建音频采集线程: {err}"))?;

    match ready_rx.recv_timeout(Duration::from_secs(15)) {
        Ok(Ok(())) => Ok(handle),
        Ok(Err(err)) => {
            let _ = handle.join();
            Err(err)
        }
        Err(_) => Err(anyhow::anyhow!("音频设备初始化超时")),
    }
}

fn run(
    kind: CaptureKind,
    format: AudioFormat,
    ring: Arc<SampleRing>,
    stop: Arc<AtomicBool>,
    ready: Sender<anyhow::Result<()>>,
    label: String,
) {
    let opened = open(&kind, format);
    let (client, capture, event) = match opened {
        Ok(parts) => {
            let _ = ready.send(Ok(()));
            parts
        }
        Err(err) => {
            let _ = ready.send(Err(err));
            return;
        }
    };

    log::info!("音频采集已启动: {label} ({} Hz, {} 声道)", format.sample_rate, format.channels);

    let mut queue: VecDeque<u8> = VecDeque::with_capacity(1 << 16);
    let mut samples: Vec<i16> = Vec::with_capacity(16_384);
    let mut read_failures = 0u32;

    while !stop.load(Ordering::Relaxed) {
        match capture.read_from_device_to_deque(&mut queue) {
            Ok(_) => {}
            Err(err) => {
                read_failures += 1;
                log::warn!("读取音频数据失败 ({read_failures}): {err}");
                if read_failures > 20 {
                    break;
                }
            }
        }

        samples.clear();
        while queue.len() >= 2 {
            let low = queue.pop_front().unwrap_or(0);
            let high = queue.pop_front().unwrap_or(0);
            samples.push(i16::from_le_bytes([low, high]));
        }
        if !samples.is_empty() {
            ring.push(&samples);
        }

        // The event fires when a new buffer is ready; the timeout keeps the
        // loop responsive to the stop flag even on a silent device.
        let _ = event.wait_for_event(50);
    }

    if let Err(err) = client.stop_stream() {
        log::warn!("停止音频采集失败: {err}");
    }
    log::info!("音频采集已停止: {label}");
}

fn open(
    kind: &CaptureKind,
    format: AudioFormat,
) -> anyhow::Result<(AudioClient, AudioCaptureClient, Handle)> {
    // COM must be initialised on the thread that uses the endpoint.
    let _ = initialize_mta();

    let enumerator = DeviceEnumerator::new()?;
    let device = select_device(&enumerator, kind)?;
    let mut client = device.get_iaudioclient()?;

    let desired = WaveFormat::new(
        16,
        16,
        &SampleType::Int,
        format.sample_rate as usize,
        format.channels as usize,
        None,
    );

    let (_default_period, minimum_period) = client.get_device_period()?;
    let mode = StreamMode::EventsShared {
        autoconvert: true,
        buffer_duration_hns: minimum_period,
    };

    // Both sources open a *capture* stream. Loopback is selected by the device
    // endpoint (render) plus the loopback stream flag, which the wasapi crate
    // derives from `(device direction, initialize direction) == (Render,
    // Capture)`; passing `Render` here would set up a render stream instead and
    // fail with AUDCLNT_E_WRONG_ENDPOINT_TYPE.
    let direction = Direction::Capture;

    client.initialize_client(&desired, &direction, &mode)?;
    let event = client.set_get_eventhandle()?;
    let capture = client.get_audiocaptureclient()?;
    client.start_stream()?;

    Ok((client, capture, event))
}

fn select_device(enumerator: &DeviceEnumerator, kind: &CaptureKind) -> anyhow::Result<Device> {
    match kind {
        CaptureKind::SystemLoopback => Ok(enumerator.get_default_device(&Direction::Render)?),
        CaptureKind::Microphone(wanted) => {
            if wanted.trim().is_empty() {
                return Ok(enumerator.get_default_device(&Direction::Capture)?);
            }
            let wanted = wanted.to_lowercase();
            let collection = enumerator.get_device_collection(&Direction::Capture)?;
            let count = collection.get_nbr_devices()?;
            for index in 0..count {
                if let Ok(device) = collection.get_device_at_index(index) {
                    let name = device.get_friendlyname().unwrap_or_default();
                    if name.to_lowercase().contains(&wanted) {
                        return Ok(device);
                    }
                }
            }
            log::warn!("找不到名称包含“{wanted}”的录音设备，改用系统默认设备");
            Ok(enumerator.get_default_device(&Direction::Capture)?)
        }
    }
}
