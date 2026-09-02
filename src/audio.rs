//! Native audio capture. Both inputs produce timestamped PCM chunks; the
//! recorder places those chunks on one shared timeline before writing files.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use crossbeam_queue::ArrayQueue;

const MIC_POOL_BLOCKS: usize = 64;
const MIC_BLOCK_FRAMES: usize = 8192;

#[derive(Debug)]
pub struct AudioChunk {
    pub samples: Vec<f32>,
    pub sample_rate: u32,
    pub captured_host_nanos: u64,
}

#[derive(Debug)]
struct MicBlock {
    samples: [f32; MIC_BLOCK_FRAMES],
    len: usize,
    sample_rate: u32,
    captured_host_nanos: u64,
}

pub struct MicCapture {
    stream: cpal::Stream,
    free_blocks: Arc<ArrayQueue<Box<MicBlock>>>,
    filled_blocks: Arc<ArrayQueue<Box<MicBlock>>>,
    dropped: Arc<AtomicU64>,
    failed: Arc<AtomicBool>,
    name: String,
    sample_rate: u32,
}

impl MicCapture {
    pub fn open(requested_name: &str) -> Result<Self> {
        let host = cpal::default_host();
        let device = host
            .input_devices()
            .context("could not enumerate microphones")?
            .find(|device| device_name(device).as_deref() == Some(requested_name))
            .with_context(|| format!("microphone not found: {requested_name}"))?;
        let name = device_name(&device).unwrap_or_else(|| "Unknown microphone".into());
        let supported = device
            .default_input_config()
            .with_context(|| format!("could not read microphone format for {name}"))?;
        let sample_rate = supported.sample_rate();
        let channels = supported.channels() as usize;
        let sample_format = supported.sample_format();
        if !matches!(
            sample_format,
            cpal::SampleFormat::F32
                | cpal::SampleFormat::F64
                | cpal::SampleFormat::I16
                | cpal::SampleFormat::I32
                | cpal::SampleFormat::U16
                | cpal::SampleFormat::U32
        ) {
            anyhow::bail!("unsupported microphone sample format: {sample_format:?}");
        }
        let free_blocks = Arc::new(ArrayQueue::new(MIC_POOL_BLOCKS));
        let filled_blocks = Arc::new(ArrayQueue::new(MIC_POOL_BLOCKS));
        for _ in 0..MIC_POOL_BLOCKS {
            free_blocks
                .push(Box::new(MicBlock {
                    samples: [0.0; MIC_BLOCK_FRAMES],
                    len: 0,
                    sample_rate,
                    captured_host_nanos: 0,
                }))
                .expect("preallocated microphone block pool");
        }
        let dropped = Arc::new(AtomicU64::new(0));
        let failed = Arc::new(AtomicBool::new(false));
        let callback_free = free_blocks.clone();
        let callback_filled = filled_blocks.clone();
        let callback_dropped = dropped.clone();
        let callback_failed = failed.clone();
        let stream_epoch = cpal::StreamInstant::new(0, 0);
        let _ = host_time_nanos();

        let stream = device.build_input_stream_raw(
            &supported.config(),
            sample_format,
            move |data, info| {
                let total_frames = data.len() / channels.max(1);
                let Some(mut block) = callback_free.pop() else {
                    callback_dropped.fetch_add(total_frames as u64, Ordering::Relaxed);
                    return;
                };
                let written =
                    decode_mic_frames_into(data, sample_format, channels, &mut block.samples);
                if written == 0 {
                    let _ = callback_free.push(block);
                    return;
                }
                let duration = duration_for_samples(written, sample_rate);
                let capture_time = info.timestamp().capture;
                let captured_host_nanos = capture_time
                    .duration_since(&stream_epoch)
                    .map(|time| time.as_nanos().min(u64::MAX as u128) as u64)
                    .unwrap_or_else(|| {
                        host_time_nanos()
                            .saturating_sub(duration.as_nanos().min(u64::MAX as u128) as u64)
                    });
                block.len = written;
                block.sample_rate = sample_rate;
                block.captured_host_nanos = captured_host_nanos;
                if total_frames > written {
                    callback_dropped.fetch_add((total_frames - written) as u64, Ordering::Relaxed);
                }
                if let Err(block) = callback_filled.push(block) {
                    callback_dropped.fetch_add(written as u64, Ordering::Relaxed);
                    let _ = callback_free.push(block);
                }
            },
            move |error| {
                callback_failed.store(true, Ordering::Release);
                tracing::error!(%error, "microphone_stream_error");
            },
            None,
        )?;
        stream.play()?;

        Ok(Self {
            stream,
            free_blocks,
            filled_blocks,
            dropped,
            failed,
            name,
            sample_rate,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn pop_chunk(&self) -> Option<AudioChunk> {
        let mut block = self.filled_blocks.pop()?;
        let chunk = AudioChunk {
            samples: block.samples[..block.len].to_vec(),
            sample_rate: block.sample_rate,
            captured_host_nanos: block.captured_host_nanos,
        };
        block.len = 0;
        self.free_blocks
            .push(block)
            .expect("microphone pool has one returned slot");
        Some(chunk)
    }

    pub fn take_dropped(&self) -> u64 {
        self.dropped.swap(0, Ordering::Relaxed)
    }

    pub fn failed(&self) -> bool {
        self.failed.load(Ordering::Acquire)
    }

    pub fn stop(&self) -> Result<()> {
        self.stream
            .pause()
            .context("could not stop microphone capture")
    }
}

fn device_name(device: &cpal::Device) -> Option<String> {
    device
        .description()
        .ok()
        .map(|description| description.name().to_string())
}

fn decode_mic_frames_into(
    data: &cpal::Data,
    format: cpal::SampleFormat,
    channels: usize,
    output: &mut [f32],
) -> usize {
    macro_rules! convert {
        ($sample:ty, $to_f32:expr) => {
            data.as_slice::<$sample>()
                .map(|samples| downmix_interleaved_into(samples, channels, output, $to_f32))
                .unwrap_or(0)
        };
    }

    match format {
        cpal::SampleFormat::F32 => convert!(f32, |value: f32| value),
        cpal::SampleFormat::F64 => convert!(f64, |value: f64| value as f32),
        cpal::SampleFormat::I16 => {
            convert!(i16, |value: i16| value as f32 / i16::MAX as f32)
        }
        cpal::SampleFormat::I32 => {
            convert!(i32, |value: i32| value as f32 / i32::MAX as f32)
        }
        cpal::SampleFormat::U16 => convert!(u16, |value: u16| {
            (value as f32 / u16::MAX as f32) * 2.0 - 1.0
        }),
        cpal::SampleFormat::U32 => convert!(u32, |value: u32| {
            (value as f32 / u32::MAX as f32) * 2.0 - 1.0
        }),
        other => {
            tracing::error!(?other, "unsupported_microphone_sample_format");
            0
        }
    }
}

fn downmix_interleaved_into<T: Copy>(
    samples: &[T],
    channels: usize,
    output: &mut [f32],
    to_f32: impl Fn(T) -> f32,
) -> usize {
    let channels = channels.max(1);
    let mut written = 0;
    for (slot, frame) in output.iter_mut().zip(samples.chunks_exact(channels)) {
        *slot = frame.iter().copied().map(&to_f32).sum::<f32>() / channels as f32;
        written += 1;
    }
    written
}

#[cfg(test)]
fn downmix_interleaved<T: Copy>(
    samples: &[T],
    channels: usize,
    to_f32: impl Fn(T) -> f32,
) -> Vec<f32> {
    let channels = channels.max(1);
    samples
        .chunks_exact(channels)
        .map(|frame| frame.iter().copied().map(&to_f32).sum::<f32>() / channels as f32)
        .collect()
}

fn duration_for_samples(samples: usize, rate: u32) -> Duration {
    Duration::from_secs_f64(samples as f64 / rate.max(1) as f64)
}

pub fn host_time_nanos() -> u64 {
    sori_system_audio_cleanroom::current_host_nanos()
}

#[cfg(target_os = "macos")]
mod system {
    use super::*;

    pub struct SystemCapture {
        inner: sori_system_audio_cleanroom::SystemCapture,
    }

    impl SystemCapture {
        pub fn open() -> Result<Self> {
            let inner = sori_system_audio_cleanroom::SystemCapture::open().map_err(|error| {
                let message = error.to_string();
                if message.contains("'nope'") {
                    anyhow::anyhow!("System Audio Recording permission is required: {message}")
                } else {
                    anyhow::anyhow!("could not start system audio capture: {message}")
                }
            })?;
            Ok(Self { inner })
        }

        pub fn sample_rate(&self) -> u32 {
            self.inner.sample_rate()
        }

        pub fn pop_chunk(&self) -> Option<AudioChunk> {
            self.inner.pop_chunk().map(|chunk| AudioChunk {
                samples: chunk.samples,
                sample_rate: chunk.sample_rate,
                captured_host_nanos: chunk.captured_host_nanos,
            })
        }

        pub fn take_dropped(&self) -> u64 {
            self.inner.take_dropped()
        }

        pub fn take_error(&self) -> Option<String> {
            self.inner.take_error()
        }

        pub fn stop(&self) -> Result<()> {
            self.inner.stop()
        }
    }
}
#[cfg(target_os = "macos")]
pub use system::SystemCapture;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interleaved_samples_are_mixed_to_mono() {
        assert_eq!(
            downmix_interleaved(&[1.0f32, -1.0, 0.5, 0.5], 2, |value| value),
            [0.0, 0.5]
        );
    }
}
