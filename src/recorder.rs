//! Recording engine. Capture callbacks enqueue PCM; this thread owns timing,
//! device switching, WAV writes, and finalization.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use tokio::sync::mpsc;

use crate::audio::{MicCapture, SystemCapture, host_time_nanos};
use crate::wav::WavWriter;
use crate::{DeviceSwitch, Meta, MetaDevices, MetaTracks, RecordingStatus};

pub const LEVEL_OK_THRESHOLD: f32 = 0.01;
const MIC_SILENCE_GRACE: Duration = Duration::from_secs(8);
const FLUSH_EVERY: Duration = Duration::from_secs(5);
const LEVEL_EVERY: Duration = Duration::from_millis(250);
const AUDIO_DRAIN_EVERY: Duration = Duration::from_millis(5);
const DEVICE_POLL_EVERY: Duration = Duration::from_secs(1);

#[derive(Debug, Clone)]
pub enum RecorderEvent {
    Started {
        folder: PathBuf,
        mic_device: String,
        system_device: String,
        started_at: chrono::DateTime<chrono::Local>,
    },
    Levels {
        mic: f32,
        system: f32,
    },
    MicSwitched {
        device: String,
        at_sec: u64,
    },
    Warning(String),
    Stopped {
        folder: PathBuf,
        duration_sec: u64,
    },
    Cancelled {
        folder: PathBuf,
    },
    Failed {
        folder: PathBuf,
        error: String,
    },
}

#[derive(Debug)]
pub enum Command {
    Stop,
    SwitchMic(Option<String>),
}

pub struct StartConfig {
    pub folder: PathBuf,
    pub mic_override: Option<String>,
}

pub struct RecorderHandle {
    cmd_tx: mpsc::UnboundedSender<Command>,
    cancelled: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    pub folder: PathBuf,
}

impl RecorderHandle {
    pub fn stop(&self) {
        let _ = self.cmd_tx.send(Command::Stop);
    }

    pub fn cancel_start(&self) {
        self.cancelled.store(true, Ordering::Release);
        let _ = self.cmd_tx.send(Command::Stop);
    }

    pub fn switch_mic(&self, override_name: Option<String>) {
        let _ = self.cmd_tx.send(Command::SwitchMic(override_name));
    }

    pub fn join(mut self) {
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

pub fn start(
    config: StartConfig,
    on_event: impl Fn(RecorderEvent) + Send + 'static,
) -> RecorderHandle {
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
    let cancelled = Arc::new(AtomicBool::new(false));
    let thread_cancelled = cancelled.clone();
    let folder = config.folder.clone();

    let join = std::thread::Builder::new()
        .name("sori-recorder".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .expect("recorder runtime");
            let folder = config.folder.clone();
            if let Err(error) =
                runtime.block_on(run(config, cmd_rx, &on_event, thread_cancelled.clone()))
            {
                if thread_cancelled.load(Ordering::Acquire) {
                    let _ = std::fs::remove_dir_all(&folder);
                    on_event(RecorderEvent::Cancelled { folder });
                } else {
                    tracing::error!(%error, "recorder_failed");
                    mark_failed(&folder, &error.to_string());
                    on_event(RecorderEvent::Failed {
                        folder,
                        error: error.to_string(),
                    });
                }
            }
        })
        .expect("spawn recorder thread");

    RecorderHandle {
        cmd_tx,
        cancelled,
        join: Some(join),
        folder,
    }
}

fn mark_failed(folder: &std::path::Path, error: &str) {
    let has_audio = ["mic.wav", "system.wav"]
        .iter()
        .any(|name| std::fs::metadata(folder.join(name)).is_ok_and(|metadata| metadata.len() > 44));
    if !has_audio {
        let _ = std::fs::remove_dir(folder);
        return;
    }
    let mut meta = Meta::load(folder).unwrap_or(Meta {
        status: RecordingStatus::Failed,
        started_at: chrono::Local::now(),
        duration_sec: 0,
        sample_rate: 0,
        tracks: MetaTracks {
            mic: "mic.wav".into(),
            system: "system.wav".into(),
        },
        devices: MetaDevices::default(),
        warnings: vec![],
    });
    meta.status = RecordingStatus::Failed;
    meta.warnings.push(format!("failed: {error}"));
    let _ = meta.save(folder);
}

struct Resampler {
    ratio: f64,
    pos: f64,
    last: f32,
    have_last: bool,
}

impl Resampler {
    fn new(from: u32, to: u32) -> Self {
        Self {
            ratio: from as f64 / to as f64,
            pos: 0.0,
            last: 0.0,
            have_last: false,
        }
    }

    fn push(&mut self, sample: f32, out: &mut Vec<f32>) {
        if (self.ratio - 1.0).abs() < f64::EPSILON {
            out.push(sample);
            return;
        }
        if !self.have_last {
            self.last = sample;
            self.have_last = true;
            return;
        }
        while self.pos < 1.0 {
            out.push(self.last + (sample - self.last) * self.pos as f32);
            self.pos += self.ratio;
        }
        self.pos -= 1.0;
        self.last = sample;
    }
}

struct MicSession {
    capture: MicCapture,
    device: String,
    output_rate: u32,
    resampler: Resampler,
}

fn open_mic(name: &str, target_rate: Option<u32>) -> anyhow::Result<MicSession> {
    let capture = MicCapture::open(name)?;
    let device = capture.name().to_string();
    let rate = capture.sample_rate();
    let target_rate = target_rate.unwrap_or(rate);
    Ok(MicSession {
        capture,
        device,
        output_rate: target_rate,
        resampler: Resampler::new(rate, target_rate),
    })
}

fn ensure_not_cancelled(cancelled: &AtomicBool) -> anyhow::Result<()> {
    if cancelled.load(Ordering::Acquire) {
        anyhow::bail!("recording start cancelled");
    }
    Ok(())
}

async fn run(
    config: StartConfig,
    mut cmd_rx: mpsc::UnboundedReceiver<Command>,
    on_event: &(impl Fn(RecorderEvent) + Send + 'static),
    cancelled: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let folder = config.folder;
    let mut mic_override = config.mic_override;
    let mic_pick = crate::devices::resolve_mic(mic_override.as_deref())
        .ok_or_else(|| anyhow::anyhow!("selected microphone is not available"))?;
    let system_device = crate::devices::default_output_name();

    let system = SystemCapture::open()?;
    ensure_not_cancelled(&cancelled)?;
    let mut mic = open_mic(&mic_pick.name, None)?;
    ensure_not_cancelled(&cancelled)?;

    let mic_rate = mic.capture.sample_rate();
    let system_rate = system.sample_rate();
    while mic.capture.pop_chunk().is_some() {}
    while system.pop_chunk().is_some() {}
    ensure_not_cancelled(&cancelled)?;

    let started_at = chrono::Local::now();
    let timeline_origin = host_time_nanos();
    let started = Instant::now();
    let mut mic_wav = WavWriter::create(&folder.join("mic.wav"), mic_rate)?;
    let mut system_wav = WavWriter::create(&folder.join("system.wav"), system_rate)?;
    let mut meta = Meta {
        status: RecordingStatus::Recording,
        started_at,
        duration_sec: 0,
        sample_rate: mic_rate,
        tracks: MetaTracks {
            mic: "mic.wav".into(),
            system: "system.wav".into(),
        },
        devices: MetaDevices {
            mic: mic.device.clone(),
            system: system_device.clone(),
            switches: vec![],
        },
        warnings: vec![],
    };
    meta.save(&folder)?;
    ensure_not_cancelled(&cancelled)?;
    on_event(RecorderEvent::Started {
        folder: folder.clone(),
        mic_device: mic.device.clone(),
        system_device: system_device.clone(),
        started_at,
    });

    let mut flush_tick = tokio::time::interval(FLUSH_EVERY);
    let mut level_tick = tokio::time::interval(LEVEL_EVERY);
    let mut audio_tick = tokio::time::interval(AUDIO_DRAIN_EVERY);
    let mut device_tick = tokio::time::interval(DEVICE_POLL_EVERY);
    audio_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    device_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    flush_tick.tick().await;
    level_tick.tick().await;
    audio_tick.tick().await;
    device_tick.tick().await;

    let mut mic_peak = 0.0f32;
    let mut system_peak = 0.0f32;
    let mut mic_ever_ok = false;
    let mut mic_warned = false;
    let mut scratch = Vec::with_capacity(16);
    let mut system_input_rate = system_rate;
    let mut system_resampler = Resampler::new(system_input_rate, system_rate);

    loop {
        tokio::select! {
            biased;

            command = cmd_rx.recv() => match command {
                Some(Command::Stop) | None => break,
                Some(Command::SwitchMic(new_override)) => {
                    mic_override = new_override;
                    let Some(pick) = crate::devices::resolve_mic(mic_override.as_deref()) else {
                        on_event(RecorderEvent::Warning("Selected microphone is not available".into()));
                        continue;
                    };
                    if pick.name != mic.device {
                        drain_mic(&mut mic, &mut mic_wav, &mut mic_peak, timeline_origin, &mut scratch)?;
                        match open_mic(&pick.name, Some(mic_rate)) {
                            Ok(new_mic) => {
                                if cancelled.load(Ordering::Acquire) { break; }
                                mic = new_mic;
                                let at_sec = started.elapsed().as_secs();
                                meta.devices.switches.push(DeviceSwitch { at_sec, device: mic.device.clone() });
                                let _ = meta.save(&folder);
                                on_event(RecorderEvent::MicSwitched { device: mic.device.clone(), at_sec });
                            }
                            Err(error) => on_event(RecorderEvent::Warning(format!("Could not switch microphone: {error}"))),
                        }
                    }
                }
            },

            _ = device_tick.tick(), if mic_override.is_none() => {
                if let Some(pick) = crate::devices::automatic_mic()
                    && pick.name != mic.device
                {
                        drain_mic(&mut mic, &mut mic_wav, &mut mic_peak, timeline_origin, &mut scratch)?;
                        if let Ok(new_mic) = open_mic(&pick.name, Some(mic_rate)) {
                            if cancelled.load(Ordering::Acquire) { break; }
                            mic = new_mic;
                            let at_sec = started.elapsed().as_secs();
                            meta.devices.switches.push(DeviceSwitch { at_sec, device: mic.device.clone() });
                            let _ = meta.save(&folder);
                            on_event(RecorderEvent::MicSwitched { device: mic.device.clone(), at_sec });
                        }
                }
            },

            _ = audio_tick.tick() => {
                drain_mic(&mut mic, &mut mic_wav, &mut mic_peak, timeline_origin, &mut scratch)?;
                drain_system(
                    &system,
                    &mut system_wav,
                    &mut system_peak,
                    &mut system_input_rate,
                    &mut system_resampler,
                    timeline_origin,
                    &mut scratch,
                )?;
                if mic.capture.failed() {
                    anyhow::bail!("microphone stream stopped");
                }
                if let Some(error) = system.take_error() {
                    anyhow::bail!(error);
                }
                let mic_dropped = mic.capture.take_dropped();
                let system_dropped = system.take_dropped();
                if mic_dropped > 0 { tracing::warn!(dropped = mic_dropped, "microphone_samples_dropped"); }
                if system_dropped > 0 { tracing::warn!(dropped = system_dropped, "system_samples_dropped"); }
            },

            _ = level_tick.tick() => {
                on_event(RecorderEvent::Levels { mic: mic_peak, system: system_peak });
                if mic_peak >= LEVEL_OK_THRESHOLD { mic_ever_ok = true; }
                if !mic_ever_ok && !mic_warned && started.elapsed() >= MIC_SILENCE_GRACE {
                    mic_warned = true;
                    meta.warnings.push("mic_level_low".into());
                    let _ = meta.save(&folder);
                    on_event(RecorderEvent::Warning(format!(
                        "Microphone \"{}\" is silent — check input device / permission",
                        mic.device
                    )));
                }
                mic_peak = 0.0;
                system_peak = 0.0;
            },

            _ = flush_tick.tick() => {
                mic_wav.flush()?;
                system_wav.flush()?;
                meta.duration_sec = started.elapsed().as_secs();
                let _ = meta.save(&folder);
            },
        }
    }

    system.stop()?;
    mic.capture.stop()?;
    drain_mic(
        &mut mic,
        &mut mic_wav,
        &mut mic_peak,
        timeline_origin,
        &mut scratch,
    )?;
    drain_system(
        &system,
        &mut system_wav,
        &mut system_peak,
        &mut system_input_rate,
        &mut system_resampler,
        timeline_origin,
        &mut scratch,
    )?;

    let (mic_target, system_target) = aligned_target_samples(
        mic_wav.samples(),
        mic_rate,
        system_wav.samples(),
        system_rate,
        started.elapsed(),
    );
    pad_to_samples(&mut mic_wav, mic_target)?;
    pad_to_samples(&mut system_wav, system_target)?;
    mic_wav.finalize()?;
    system_wav.finalize()?;

    meta.status = RecordingStatus::Done;
    meta.duration_sec = (mic_target / mic_rate as u64).max(system_target / system_rate as u64);
    if !mic_ever_ok
        && !meta
            .warnings
            .iter()
            .any(|warning| warning == "mic_level_low")
    {
        meta.warnings.push("mic_level_low".into());
    }
    meta.save(&folder)?;
    on_event(RecorderEvent::Stopped {
        folder,
        duration_sec: meta.duration_sec,
    });
    Ok(())
}

fn drain_mic(
    mic: &mut MicSession,
    wav: &mut WavWriter,
    peak: &mut f32,
    origin_host_nanos: u64,
    scratch: &mut Vec<f32>,
) -> anyhow::Result<()> {
    while let Some(chunk) = mic.capture.pop_chunk() {
        let mut output = Vec::with_capacity(chunk.samples.len());
        for sample in chunk.samples {
            scratch.clear();
            mic.resampler.push(sample, scratch);
            output.extend_from_slice(scratch);
        }
        for &value in &output {
            *peak = peak.max(value.abs());
        }
        write_timed_samples(
            wav,
            mic.output_rate,
            origin_host_nanos,
            chunk.captured_host_nanos,
            &output,
        )?;
    }
    Ok(())
}

fn drain_system(
    system: &SystemCapture,
    wav: &mut WavWriter,
    peak: &mut f32,
    input_rate: &mut u32,
    resampler: &mut Resampler,
    origin_host_nanos: u64,
    scratch: &mut Vec<f32>,
) -> anyhow::Result<()> {
    while let Some(chunk) = system.pop_chunk() {
        if chunk.sample_rate != *input_rate {
            *input_rate = chunk.sample_rate;
            *resampler = Resampler::new(*input_rate, system.sample_rate());
        }
        let captured_host_nanos = chunk.captured_host_nanos;
        let mut output = Vec::with_capacity(chunk.samples.len());
        for sample in chunk.samples {
            scratch.clear();
            resampler.push(sample, scratch);
            output.extend_from_slice(scratch);
        }
        for &value in &output {
            *peak = peak.max(value.abs());
        }
        write_timed_samples(
            wav,
            system.sample_rate(),
            origin_host_nanos,
            captured_host_nanos,
            &output,
        )?;
    }
    Ok(())
}

fn write_timed_samples(
    wav: &mut WavWriter,
    rate: u32,
    origin_host_nanos: u64,
    captured_host_nanos: u64,
    samples: &[f32],
) -> std::io::Result<()> {
    let (padding, skip) = timeline_placement(
        wav.samples(),
        rate,
        origin_host_nanos,
        captured_host_nanos,
        samples.len(),
    );
    pad_to_samples(wav, wav.samples().saturating_add(padding))?;
    for &sample in &samples[skip..] {
        wav.write_sample(sample)?;
    }
    Ok(())
}

fn timeline_placement(
    current_samples: u64,
    rate: u32,
    origin_host_nanos: u64,
    captured_host_nanos: u64,
    chunk_len: usize,
) -> (u64, usize) {
    let signed_start = if captured_host_nanos >= origin_host_nanos {
        samples_for_nanos(rate, captured_host_nanos - origin_host_nanos) as i128
    } else {
        -(samples_for_nanos(rate, origin_host_nanos - captured_host_nanos) as i128)
    };
    let current = current_samples as i128;
    if signed_start > current {
        ((signed_start - current).min(u64::MAX as i128) as u64, 0)
    } else {
        let overlap = (current - signed_start).min(chunk_len as i128) as usize;
        (0, overlap)
    }
}

fn pad_to_samples(wav: &mut WavWriter, target: u64) -> std::io::Result<()> {
    while wav.samples() < target {
        wav.write_sample(0.0)?;
    }
    Ok(())
}

fn samples_for_duration(rate: u32, duration: Duration) -> u64 {
    samples_for_nanos(rate, duration.as_nanos().min(u64::MAX as u128) as u64)
}

fn samples_for_nanos(rate: u32, nanos: u64) -> u64 {
    let numerator = nanos as u128 * rate as u128;
    numerator.div_ceil(1_000_000_000).min(u64::MAX as u128) as u64
}

fn aligned_target_samples(
    mic_samples: u64,
    mic_rate: u32,
    system_samples: u64,
    system_rate: u32,
    elapsed: Duration,
) -> (u64, u64) {
    let mic_ns = (mic_samples as u128 * 1_000_000_000).div_ceil(mic_rate as u128);
    let system_ns = (system_samples as u128 * 1_000_000_000).div_ceil(system_rate as u128);
    let target_ns = elapsed.as_nanos().max(mic_ns).max(system_ns);
    let target = Duration::from_nanos(target_ns.min(u64::MAX as u128) as u64);
    (
        samples_for_duration(mic_rate, target),
        samples_for_duration(system_rate, target),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alignment_pads_both_tracks_to_the_same_duration() {
        let (mic, system) = aligned_target_samples(
            48_000 * 3,
            48_000,
            48_000 * 4,
            48_000,
            Duration::from_secs(2),
        );
        assert_eq!(mic, 48_000 * 4);
        assert_eq!(system, 48_000 * 4);
    }

    #[test]
    fn resampler_preserves_identity_samples() {
        let mut resampler = Resampler::new(48_000, 48_000);
        let mut output = Vec::new();
        for sample in [0.1, -0.2, 0.3] {
            resampler.push(sample, &mut output);
        }
        assert_eq!(output, [0.1, -0.2, 0.3]);
    }

    #[test]
    fn timeline_inserts_a_gap_where_a_chunk_was_missed() {
        let origin = 1_000_000_000;
        let captured_at = origin + 2_000_000_000;
        let (padding, skip) = timeline_placement(48_000, 48_000, origin, captured_at, 480);
        assert_eq!(padding, 48_000);
        assert_eq!(skip, 0);
    }

    #[test]
    fn timeline_trims_overlapping_samples_instead_of_shifting_later_audio() {
        let origin = 1_000_000_000;
        let captured_at = origin + 900_000_000;
        let (padding, skip) = timeline_placement(48_000, 48_000, origin, captured_at, 9_600);
        assert_eq!(padding, 0);
        assert_eq!(skip, 4_800);
    }

    #[test]
    fn normal_stop_is_not_classified_as_start_cancellation() {
        let (cmd_tx, _cmd_rx) = mpsc::unbounded_channel();
        let cancelled = Arc::new(AtomicBool::new(false));
        let handle = RecorderHandle {
            cmd_tx,
            cancelled: cancelled.clone(),
            join: None,
            folder: PathBuf::from("unused"),
        };
        handle.stop();
        assert!(!cancelled.load(Ordering::Acquire));
        handle.cancel_start();
        assert!(cancelled.load(Ordering::Acquire));
    }
}
