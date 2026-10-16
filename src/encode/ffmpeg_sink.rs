//! FFmpeg-backed export: raw BGRA frames are piped to a child process.
//!
//! Used for the containers and codecs Media Foundation cannot produce
//! (Matroska, WebM, QuickTime, AV1/VP9/ProRes …). Audio is recorded to a
//! temporary WAV next to the output and muxed in a second FFmpeg pass, because
//! a single child process cannot receive two independent raw streams without a
//! named pipe.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};

use super::{FrameData, FrameSink, SinkParams};
use crate::formats::{AudioCodec, Container, QualityPreset, VideoCodec};
use crate::paths::PlannedOutput;

pub struct FfmpegSink {
    ffmpeg: PathBuf,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    final_path: PathBuf,
    video_path: PathBuf,
    audio: Option<AudioTrack>,
    container: Container,
    audio_codec: AudioCodec,
    audio_bitrate: u32,
    log_path: PathBuf,
    frames: u64,
}

struct AudioTrack {
    writer: WavWriter,
    path: PathBuf,
}

impl FfmpegSink {
    pub fn new(
        ffmpeg: &Path,
        container: Container,
        codec: VideoCodec,
        params: SinkParams,
        output: &PlannedOutput,
        quality: QualityPreset,
        prefer_hardware: bool,
    ) -> anyhow::Result<Self> {
        output.prepare()?;
        let final_path = output.path.clone();
        let extension = container.extension();

        let audio = match params.audio {
            Some(audio_params) => {
                let path = final_path.with_extension(format!("{extension}.audio.tmp.wav"));
                let writer = WavWriter::create(&path, audio_params.sample_rate, audio_params.channels)?;
                Some(AudioTrack { writer, path })
            }
            None => None,
        };

        let video_path = if audio.is_some() {
            final_path.with_extension(format!("{extension}.video.tmp"))
        } else {
            final_path.clone()
        };
        let log_path = final_path.with_extension(format!("{extension}.ffmpeg.log"));

        let args = video_args(container, codec, &params, quality, &video_path, prefer_hardware);
        let log = File::create(&log_path)?;
        let stderr = Stdio::from(log);

        let mut command = Command::new(ffmpeg);
        command
            .args(&args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(stderr);
        hide_console(&mut command);

        log::info!("启动 FFmpeg: {} {}", ffmpeg.display(), args.join(" "));

        let mut child = command
            .spawn()
            .map_err(|err| anyhow::anyhow!("无法启动 FFmpeg ({}): {err}", ffmpeg.display()))?;
        let stdin = child.stdin.take();

        Ok(Self {
            ffmpeg: ffmpeg.to_path_buf(),
            child: Some(child),
            stdin,
            final_path,
            video_path,
            audio,
            container,
            audio_codec: params.audio.map(|a| a.codec).unwrap_or(AudioCodec::Aac),
            audio_bitrate: params.audio.map(|a| a.bitrate).unwrap_or(192_000),
            log_path,
            frames: 0,
        })
    }

    /// Runs the second pass that merges the temporary video and WAV.
    fn mux_audio(&self, track: &mut AudioTrack) -> anyhow::Result<()> {
        track.writer.finish()?;

        let args = mux_args(
            &self.video_path,
            &track.path,
            self.container,
            self.audio_codec,
            self.audio_bitrate,
            &self.final_path,
        );
        let log = File::create(&self.log_path)?;
        let mut command = Command::new(&self.ffmpeg);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        hide_console(&mut command);

        log::info!("合并音轨: {}", args.join(" "));
        let status = command
            .status()
            .map_err(|err| anyhow::anyhow!("无法运行 FFmpeg 合并音轨: {err}"))?;
        anyhow::ensure!(
            status.success(),
            "FFmpeg 合并音轨失败（退出码 {:?}）: {}",
            status.code(),
            self.log_tail()
        );
        Ok(())
    }

    fn log_tail(&self) -> String {
        std::fs::read_to_string(&self.log_path)
            .map(|text| {
                let lines: Vec<&str> = text.lines().rev().take(6).collect();
                lines.into_iter().rev().collect::<Vec<_>>().join(" | ")
            })
            .unwrap_or_default()
    }

    fn cleanup_temps(&self, audio_path: Option<&Path>) {
        let _ = std::fs::remove_file(&self.video_path);
        if let Some(path) = audio_path {
            let _ = std::fs::remove_file(path);
        }
    }
}

impl FrameSink for FfmpegSink {
    fn push(&mut self, frame: &FrameData<'_>) -> anyhow::Result<()> {
        let stdin = self
            .stdin
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("FFmpeg 输入管道已经关闭"))?;
        let expected = (frame.width as usize) * (frame.height as usize) * 4;
        anyhow::ensure!(
            frame.pixels.len() >= expected,
            "帧大小不匹配: {} < {}",
            frame.pixels.len(),
            expected
        );
        stdin
            .write_all(&frame.pixels[..expected])
            .map_err(|err| anyhow::anyhow!("向 FFmpeg 写入帧失败: {err}"))?;
        self.frames += 1;
        Ok(())
    }

    fn push_audio(&mut self, pcm: &[u8]) -> anyhow::Result<()> {
        if let Some(track) = self.audio.as_mut() {
            track.writer.write(pcm)?;
        }
        Ok(())
    }

    fn accepts_audio(&self) -> bool {
        self.audio.is_some()
    }

    fn finish(mut self: Box<Self>) -> anyhow::Result<PathBuf> {
        // Closing stdin signals end-of-stream to FFmpeg.
        self.stdin.take();
        let status = match self.child.as_mut() {
            Some(child) => child.wait()?,
            None => anyhow::bail!("FFmpeg 进程已经结束"),
        };
        self.child = None;

        anyhow::ensure!(
            status.success(),
            "FFmpeg 编码失败（退出码 {:?}）: {}",
            status.code(),
            self.log_tail()
        );

        super::native_video::verify_output(&self.video_path)?;

        // Take the audio track out so `mux_audio` can borrow the rest of `self`.
        let mut track = self.audio.take();
        match track.as_mut() {
            Some(track) if track.writer.has_data() => {
                let audio_path = track.path.clone();
                self.mux_audio(track)?;
                self.cleanup_temps(Some(&audio_path));
            }
            Some(track) => {
                log::warn!("没有捕获到音频数据，只保留视频");
                std::fs::rename(&self.video_path, &self.final_path)?;
                let _ = std::fs::remove_file(&track.path);
            }
            None => {}
        }

        let size = super::native_video::verify_output(&self.final_path)?;
        let _ = std::fs::remove_file(&self.log_path);
        log::info!("FFmpeg 导出完成: {} 帧, {} 字节", self.frames, size);
        Ok(self.final_path.clone())
    }

    fn describe(&self) -> String {
        format!("FFmpeg · {} 帧", self.frames)
    }
}

impl Drop for FfmpegSink {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Builds the first-pass argument list. Pure so it can be unit-tested.
pub fn video_args(
    container: Container,
    codec: VideoCodec,
    params: &SinkParams,
    quality: QualityPreset,
    output: &Path,
    prefer_hardware: bool,
) -> Vec<String> {
    let mut args: Vec<String> = vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-f".into(),
        "rawvideo".into(),
        "-pix_fmt".into(),
        "bgra".into(),
        "-s".into(),
        format!("{}x{}", params.width, params.height),
        "-framerate".into(),
        params.fps.to_string(),
        "-i".into(),
        "pipe:0".into(),
        "-an".into(),
        "-c:v".into(),
        codec.ffmpeg_encoder(prefer_hardware).into(),
    ];

    args.extend(codec_quality_args(codec, quality, params.bitrate, prefer_hardware));
    args.push("-f".into());
    args.push(muxer_name(container).into());

    if matches!(container, Container::Mp4 | Container::Mov) {
        args.push("-movflags".into());
        args.push("+faststart".into());
    }

    args.push(output.to_string_lossy().into_owned());
    args
}

fn codec_quality_args(
    codec: VideoCodec,
    quality: QualityPreset,
    bitrate: u32,
    prefer_hardware: bool,
) -> Vec<String> {
    let crf = quality.crf().to_string();
    let mut args: Vec<String> = Vec::new();
    match codec {
        VideoCodec::H264 | VideoCodec::Hevc => {
            if prefer_hardware {
                args.extend(["-preset".into(), "p4".into(), "-cq".into(), crf]);
                args.extend(["-b:v".into(), "0".into()]);
            } else {
                args.extend(["-preset".into(), "veryfast".into(), "-crf".into(), crf]);
            }
            args.extend(["-pix_fmt".into(), "yuv420p".into()]);
        }
        VideoCodec::Av1 => {
            args.extend(["-crf".into(), crf, "-preset".into(), "8".into()]);
            args.extend(["-pix_fmt".into(), "yuv420p".into()]);
        }
        VideoCodec::Vp9 | VideoCodec::Vp8 => {
            args.extend(["-crf".into(), crf, "-b:v".into(), "0".into()]);
            args.extend(["-row-mt".into(), "1".into()]);
            // Without this the BGRA input reaches libvpx as `gbrap`, which it
            // rejects ("Pixel format 'gbrap' is not widely supported").
            args.extend(["-pix_fmt".into(), "yuv420p".into()]);
        }
        VideoCodec::ProRes => {
            args.extend(["-profile:v".into(), "3".into(), "-pix_fmt".into(), "yuv422p10le".into()]);
        }
        VideoCodec::Mjpeg => {
            args.extend(["-q:v".into(), "4".into(), "-pix_fmt".into(), "yuvj420p".into()]);
        }
        VideoCodec::Mpeg4 | VideoCodec::Mpeg2 | VideoCodec::Wmv3 => {
            args.extend(["-b:v".into(), bitrate.to_string()]);
            args.extend(["-pix_fmt".into(), "yuv420p".into()]);
        }
    }
    args
}

/// FFmpeg muxer for a container.
pub fn muxer_name(container: Container) -> &'static str {
    match container {
        Container::Mp4 => "mp4",
        Container::Mkv => "matroska",
        Container::Mov => "mov",
        Container::Webm => "webm",
        Container::Avi => "avi",
        Container::Wmv => "asf",
        Container::Mpg => "mpeg",
        Container::Gif => "gif",
        Container::PngSeq | Container::JpegSeq => "image2",
    }
}

/// Builds the audio-muxing pass. Pure so it can be unit-tested.
pub fn mux_args(
    video: &Path,
    audio: &Path,
    container: Container,
    codec: AudioCodec,
    audio_bitrate: u32,
    output: &Path,
) -> Vec<String> {
    vec![
        "-y".into(),
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-i".into(),
        video.to_string_lossy().into_owned(),
        "-i".into(),
        audio.to_string_lossy().into_owned(),
        "-c:v".into(),
        "copy".into(),
        "-c:a".into(),
        codec.ffmpeg_encoder().into(),
        "-b:a".into(),
        audio_bitrate.to_string(),
        "-shortest".into(),
        "-f".into(),
        muxer_name(container).into(),
        output.to_string_lossy().into_owned(),
    ]
}

#[cfg(windows)]
fn hide_console(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NO_WINDOW);
}

#[cfg(not(windows))]
fn hide_console(_command: &mut Command) {}

/// Minimal 16-bit PCM WAV writer.
struct WavWriter {
    file: Option<File>,
    data_bytes: u64,
    sample_rate: u32,
    channels: u16,
}

impl WavWriter {
    fn create(path: &Path, sample_rate: u32, channels: u16) -> std::io::Result<Self> {
        let mut file = File::create(path)?;
        file.write_all(&[0u8; 44])?;
        Ok(Self { file: Some(file), data_bytes: 0, sample_rate, channels })
    }

    fn has_data(&self) -> bool {
        self.data_bytes > 0
    }

    fn write(&mut self, pcm: &[u8]) -> std::io::Result<()> {
        if let Some(file) = self.file.as_mut() {
            file.write_all(pcm)?;
            self.data_bytes += pcm.len() as u64;
        }
        Ok(())
    }

    fn finish(&mut self) -> std::io::Result<()> {
        let Some(mut file) = self.file.take() else {
            return Ok(());
        };
        file.flush()?;
        let header = wav_header(self.sample_rate, self.channels, self.data_bytes);
        file.seek(SeekFrom::Start(0))?;
        file.write_all(&header)?;
        file.flush()?;
        Ok(())
    }
}

/// Canonical 44-byte RIFF/WAVE header for 16-bit PCM.
fn wav_header(sample_rate: u32, channels: u16, data_bytes: u64) -> [u8; 44] {
    let mut header = [0u8; 44];
    let byte_rate = sample_rate * channels as u32 * 2;
    let block_align = channels * 2;
    let riff_size = (36 + data_bytes) as u32;
    let data_size = data_bytes as u32;

    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&riff_size.to_le_bytes());
    header[8..12].copy_from_slice(b"WAVE");
    header[12..16].copy_from_slice(b"fmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes()); // PCM
    header[22..24].copy_from_slice(&channels.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&byte_rate.to_le_bytes());
    header[32..34].copy_from_slice(&block_align.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data_size.to_le_bytes());
    header
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::encode::AudioParams;
    use crate::formats::AudioCodec;

    fn params(audio: bool) -> SinkParams {
        SinkParams {
            width: 1920,
            height: 1080,
            fps: 60,
            bitrate: 12_000_000,
            audio: audio.then_some(AudioParams {
                sample_rate: 48_000,
                channels: 2,
                codec: AudioCodec::Aac,
                bitrate: 192_000,
            }),
        }
    }

    #[test]
    fn video_args_describe_the_raw_input() {
        let args = video_args(
            Container::Webm,
            VideoCodec::Vp9,
            &params(true),
            QualityPreset::High,
            Path::new("out.webm"),
            false,
        );
        let joined = args.join(" ");
        assert!(joined.contains("-f rawvideo"), "{joined}");
        assert!(joined.contains("-pix_fmt bgra"), "{joined}");
        assert!(joined.contains("-s 1920x1080"), "{joined}");
        assert!(joined.contains("-framerate 60"), "{joined}");
        assert!(joined.contains("-c:v libvpx-vp9"), "{joined}");
        assert!(joined.contains("-f webm"), "{joined}");
        assert_eq!(args.last().unwrap(), "out.webm");
    }

    #[test]
    fn hardware_and_software_h264_use_different_encoders() {
        let hw = video_args(
            Container::Mp4,
            VideoCodec::H264,
            &params(false),
            QualityPreset::Medium,
            Path::new("a.mp4"),
            true,
        )
        .join(" ");
        let sw = video_args(
            Container::Mp4,
            VideoCodec::H264,
            &params(false),
            QualityPreset::Medium,
            Path::new("a.mp4"),
            false,
        )
        .join(" ");
        assert!(hw.contains("h264_nvenc"), "{hw}");
        assert!(sw.contains("libx264"), "{sw}");
    }

    #[test]
    fn mp4_gets_faststart() {
        let args = video_args(
            Container::Mp4,
            VideoCodec::H264,
            &params(false),
            QualityPreset::Medium,
            Path::new("a.mp4"),
            false,
        );
        assert!(args.join(" ").contains("-movflags +faststart"));
    }

    #[test]
    fn every_codec_asks_for_an_encodable_pixel_format() {
        // Raw input is BGRA; encoders reject it unless a pixel format is given.
        for codec in [
            VideoCodec::H264,
            VideoCodec::Hevc,
            VideoCodec::Av1,
            VideoCodec::Vp9,
            VideoCodec::Vp8,
            VideoCodec::Mpeg4,
            VideoCodec::Mjpeg,
            VideoCodec::Mpeg2,
            VideoCodec::Wmv3,
            VideoCodec::ProRes,
        ] {
            let container = if codec == VideoCodec::Vp9 || codec == VideoCodec::Vp8 {
                Container::Webm
            } else {
                Container::Mkv
            };
            let args = video_args(
                container,
                codec,
                &params(false),
                QualityPreset::Medium,
                Path::new("out.mkv"),
                false,
            );
            let joined = args.join(" ");
            assert!(joined.contains("-pix_fmt"), "{codec:?} missing -pix_fmt: {joined}");
        }
    }

    #[test]
    fn mux_args_copy_the_video_stream() {
        let args = mux_args(
            Path::new("v.tmp"),
            Path::new("a.wav"),
            Container::Mkv,
            AudioCodec::Opus,
            160_000,
            Path::new("out.mkv"),
        );
        let joined = args.join(" ");
        assert!(joined.contains("-c:v copy"), "{joined}");
        assert!(joined.contains("-c:a libopus"), "{joined}");
        assert!(joined.contains("-shortest"), "{joined}");
        assert!(joined.contains("-f matroska"), "{joined}");
    }

    #[test]
    fn wav_header_is_well_formed() {
        let header = wav_header(48_000, 2, 1_000);
        assert_eq!(&header[0..4], b"RIFF");
        assert_eq!(&header[8..12], b"WAVE");
        assert_eq!(u32::from_le_bytes([header[4], header[5], header[6], header[7]]), 1036);
        assert_eq!(u16::from_le_bytes([header[22], header[23]]), 2);
        assert_eq!(
            u32::from_le_bytes([header[24], header[25], header[26], header[27]]),
            48_000
        );
        assert_eq!(u32::from_le_bytes([header[40], header[41], header[42], header[43]]), 1000);
    }

    #[test]
    fn wav_writer_round_trips_a_file() {
        let dir = std::env::temp_dir().join(format!("screenrec_wav_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.wav");

        let mut writer = WavWriter::create(&path, 44_100, 1).unwrap();
        writer.write(&[1, 2, 3, 4]).unwrap();
        writer.write(&[5, 6]).unwrap();
        assert!(writer.has_data());
        writer.finish().unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(bytes.len(), 44 + 6);
        assert_eq!(&bytes[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([bytes[40], bytes[41], bytes[42], bytes[43]]), 6);
        std::fs::remove_dir_all(&dir).ok();
    }
}
