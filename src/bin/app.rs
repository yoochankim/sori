//! Sori recording core.
//!
//! The SwiftUI process owns every visible surface and the global shortcut.
//! This child process owns recording lifecycle, state, and the local CLI socket.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions, Permissions};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::PathBuf;
use std::sync::{Mutex, mpsc};
use std::time::{Duration, Instant};

use fs2::FileExt;

use sori_app::devices;
use sori_app::hook;
use sori_app::recorder::{self, LEVEL_OK_THRESHOLD, RecorderEvent, RecorderHandle};
use sori_app::{AppState, Request, Response, Settings, StateMic, StateSystem};

enum CoreEvent {
    Recorder(RecorderEvent),
    Ipc(Request, tokio::sync::oneshot::Sender<Response>),
}

const START_TIMEOUT: Duration = Duration::from_secs(25);

struct PendingStart {
    reply: Option<tokio::sync::oneshot::Sender<Response>>,
    folder: PathBuf,
    deadline: Instant,
}

struct PendingStop {
    reply: Option<tokio::sync::oneshot::Sender<Response>>,
    folder: PathBuf,
}

struct Core {
    parent_pid: u32,
    settings: Settings,
    recorder: Option<RecorderHandle>,
    capture_ready: bool,
    capture_started_at: Option<chrono::DateTime<chrono::Local>>,
    pending_start: Option<PendingStart>,
    pending_stop: Option<PendingStop>,
    current_mic: String,
    current_system: String,
    mic_levels: VecDeque<f32>,
    system_levels: VecDeque<f32>,
    last_error: Option<String>,
    event_tx: mpsc::Sender<CoreEvent>,
}

impl Core {
    fn new(event_tx: mpsc::Sender<CoreEvent>) -> Self {
        let mut core = Self {
            parent_pid: std::os::unix::process::parent_id(),
            settings: Settings::load(),
            recorder: None,
            capture_ready: false,
            capture_started_at: None,
            pending_start: None,
            pending_stop: None,
            current_mic: String::new(),
            current_system: String::new(),
            mic_levels: VecDeque::new(),
            system_levels: VecDeque::new(),
            last_error: None,
            event_tx,
        };
        let stale = sori_app::mark_stale_recordings();
        if stale > 0 {
            tracing::warn!(stale, "marked_interrupted_recordings");
        }
        core.refresh_devices();
        core.write_state();
        core
    }

    fn active(&self) -> bool {
        self.recorder.is_some()
    }

    fn mic_level_ok(&self) -> bool {
        self.mic_levels.is_empty()
            || self
                .mic_levels
                .iter()
                .any(|&level| level >= LEVEL_OK_THRESHOLD)
    }

    fn refresh_devices(&mut self) {
        if self.active() {
            return;
        }
        self.current_mic = devices::resolve_mic(self.settings.mic_override.as_deref())
            .map(|device| device.name)
            .unwrap_or_else(|| "No microphone".into());
        self.current_system = devices::default_output_name();
    }

    fn write_state(&self) {
        let mut state = AppState::idle(&self.current_mic, &self.current_system);
        state.last_error = self.last_error.clone();
        if let Some(recorder) = &self.recorder {
            state.folder = Some(recorder.folder.clone());
            if self.capture_ready {
                if let Some(started_at) = self.capture_started_at {
                    state.status = "recording".into();
                    state.started_at = Some(started_at);
                    state.elapsed_sec =
                        (chrono::Local::now() - started_at).num_seconds().max(0) as u64;
                }
            } else {
                state.status = if self.pending_stop.is_some() {
                    "stopping".into()
                } else {
                    "starting".into()
                };
            }
            if self.capture_ready {
                state.status = "recording".into();
                state.mic = StateMic {
                    device: self.current_mic.clone(),
                    level_ok: self.mic_level_ok(),
                    level: self.mic_levels.back().copied().unwrap_or(0.0),
                };
                state.system = StateSystem {
                    device: self.current_system.clone(),
                    level: self.system_levels.back().copied().unwrap_or(0.0),
                };
            }
        }
        if let Err(error) = state.save() {
            tracing::warn!(%error, "state_save_failed");
        }
    }

    fn start(
        &mut self,
        mic_override: Option<String>,
        reply: Option<tokio::sync::oneshot::Sender<Response>>,
    ) -> Result<(), (String, Option<tokio::sync::oneshot::Sender<Response>>)> {
        if self.active() {
            return Err(("already recording or starting".into(), reply));
        }
        if let Some(name) = mic_override.as_deref() {
            if !devices::list_inputs()
                .iter()
                .any(|device| device.name == name)
            {
                return Err((format!("microphone not found: {name}"), reply));
            }
            self.settings.mic_override = Some(name.to_string());
            if let Err(error) = self.settings.save() {
                return Err((error.to_string(), reply));
            }
        } else if devices::resolve_mic(self.settings.mic_override.as_deref()).is_none() {
            return Err(("selected microphone is not available".into(), reply));
        }

        let now = chrono::Local::now();
        let folder = match sori_app::new_recording_folder(now) {
            Ok(folder) => folder,
            Err(error) => return Err((error.to_string(), reply)),
        };
        let event_tx = self.event_tx.clone();
        self.recorder = Some(recorder::start(
            recorder::StartConfig {
                folder: folder.clone(),
                mic_override: self.settings.mic_override.clone(),
            },
            move |event| {
                let _ = event_tx.send(CoreEvent::Recorder(event));
            },
        ));
        self.capture_ready = false;
        self.capture_started_at = None;
        self.pending_start = Some(PendingStart {
            reply,
            folder,
            deadline: Instant::now() + START_TIMEOUT,
        });
        self.mic_levels.clear();
        self.system_levels.clear();
        self.last_error = None;
        self.write_state();
        Ok(())
    }

    fn stop(
        &mut self,
        reply: Option<tokio::sync::oneshot::Sender<Response>>,
    ) -> Result<(), (String, Option<tokio::sync::oneshot::Sender<Response>>)> {
        if self.pending_stop.is_some() {
            return Err(("stop already in progress".into(), reply));
        }
        let Some(recorder) = self.recorder.as_ref() else {
            return Err(("not recording".into(), reply));
        };
        let folder = recorder.folder.clone();
        if self.capture_ready {
            recorder.stop();
        } else {
            recorder.cancel_start();
        }
        self.pending_stop = Some(PendingStop { reply, folder });
        self.write_state();
        Ok(())
    }

    fn set_mic(&mut self, name: Option<String>) -> Result<(), String> {
        if let Some(name) = name.as_deref()
            && !devices::list_inputs()
                .iter()
                .any(|device| device.name == name)
        {
            return Err(format!("microphone not found: {name}"));
        }
        self.settings.mic_override = name.clone();
        self.settings.save().map_err(|error| error.to_string())?;
        if let Some(recorder) = &self.recorder {
            recorder.switch_mic(name);
        } else {
            self.refresh_devices();
        }
        self.write_state();
        Ok(())
    }

    fn on_recorder(&mut self, event: RecorderEvent) {
        let mut replies = Vec::new();
        match event {
            RecorderEvent::Started {
                mic_device,
                system_device,
                started_at,
                ..
            } => {
                self.capture_ready = true;
                self.capture_started_at = Some(started_at);
                self.current_mic = mic_device;
                self.current_system = system_device;
                self.last_error = None;
                if let Some(pending) = self.pending_start.take()
                    && let Some(reply) = pending.reply
                {
                    replies.push((
                        reply,
                        Response::ok(serde_json::json!({ "folder": pending.folder })),
                    ));
                }
            }
            RecorderEvent::Levels { mic, system } => {
                push_level(&mut self.mic_levels, mic);
                push_level(&mut self.system_levels, system);
            }
            RecorderEvent::MicSwitched { device, .. } => self.current_mic = device,
            RecorderEvent::Warning(message) => tracing::warn!(%message, "recorder_warning"),
            RecorderEvent::Stopped { folder, .. } => {
                self.recorder = None;
                self.capture_ready = false;
                self.capture_started_at = None;
                self.pending_start = None;
                let _ = sori_app::update_latest_link(&folder);
                hook::run_finish_hook(&folder);
                self.refresh_devices();
                if let Some(pending) = self.pending_stop.take()
                    && let Some(reply) = pending.reply
                {
                    replies.push((
                        reply,
                        Response::ok(serde_json::json!({ "folder": pending.folder })),
                    ));
                }
            }
            RecorderEvent::Cancelled { folder } => {
                self.recorder = None;
                self.capture_ready = false;
                self.capture_started_at = None;
                if let Some(pending) = self.pending_start.take()
                    && let Some(reply) = pending.reply
                {
                    replies.push((reply, Response::err("recording start cancelled")));
                }
                if let Some(pending) = self.pending_stop.take()
                    && let Some(reply) = pending.reply
                {
                    replies.push((reply, Response::ok(serde_json::json!({ "folder": folder }))));
                }
                self.refresh_devices();
            }
            RecorderEvent::Failed { error, .. } => {
                self.recorder = None;
                self.capture_ready = false;
                self.capture_started_at = None;
                self.last_error = Some(error.clone());
                if let Some(pending) = self.pending_start.take()
                    && let Some(reply) = pending.reply
                {
                    replies.push((reply, Response::err(error.clone())));
                }
                if let Some(pending) = self.pending_stop.take()
                    && let Some(reply) = pending.reply
                {
                    replies.push((reply, Response::err(error)));
                }
                self.refresh_devices();
            }
        }
        self.write_state();
        for (reply, response) in replies {
            let _ = reply.send(response);
        }
    }

    fn on_ipc(&mut self, request: Request, reply: tokio::sync::oneshot::Sender<Response>) -> bool {
        let quit = matches!(request, Request::Quit);
        match request {
            Request::Start { mic } => {
                if let Err((error, reply)) = self.start(mic, Some(reply)) {
                    send_optional_reply(reply, Response::err(error));
                }
                return false;
            }
            Request::Stop => {
                if let Err((error, reply)) = self.stop(Some(reply)) {
                    send_optional_reply(reply, Response::err(error));
                }
                return false;
            }
            Request::Status => {
                self.write_state();
                let mut value =
                    serde_json::to_value(AppState::load().unwrap_or_else(|| {
                        AppState::idle(&self.current_mic, &self.current_system)
                    }))
                    .unwrap_or_default();
                if let Some(object) = value.as_object_mut() {
                    object.insert("core_running".into(), serde_json::Value::Bool(true));
                }
                let _ = reply.send(Response::ok(value));
            }
            Request::List { limit } => {
                let _ = reply.send(Response::ok(sori_app::list_recordings(limit)));
            }
            Request::Devices => {
                let _ = reply.send(Response::ok(devices::list_inputs()));
            }
            Request::SetMic { mic } => {
                let response = match self.set_mic(mic) {
                    Ok(()) => Response::ok(serde_json::json!({
                        "mic": self.current_mic,
                        "automatic": self.settings.mic_override.is_none()
                    })),
                    Err(error) => Response::err(error),
                };
                let _ = reply.send(response);
            }
            Request::Quit => {
                let _ = reply.send(Response::ok(serde_json::json!({ "quitting": true })));
            }
        }
        quit
    }

    fn check_start_timeout(&mut self) {
        let timed_out = self
            .pending_start
            .as_ref()
            .is_some_and(|pending| Instant::now() >= pending.deadline);
        if !timed_out {
            return;
        }
        if let Some(recorder) = &self.recorder {
            if self.pending_stop.is_none() {
                self.pending_stop = Some(PendingStop {
                    reply: None,
                    folder: recorder.folder.clone(),
                });
            }
            recorder.cancel_start();
        }
        if let Some(pending) = self.pending_start.take() {
            send_optional_reply(
                pending.reply,
                Response::err("recording start timed out and was cancelled"),
            );
        }
        self.last_error = Some("Recording start timed out and was cancelled".into());
        self.write_state();
    }

    fn shutdown(&mut self) {
        let capture_ready = self.capture_ready;
        if let Some(recorder) = self.recorder.take() {
            if capture_ready {
                recorder.stop();
            } else {
                recorder.cancel_start();
            }
            recorder.join();
        }
        self.capture_ready = false;
        self.capture_started_at = None;
        self.refresh_devices();
        self.write_state();
        let _ = std::fs::remove_file(sori_app::socket_path());
    }
}

fn push_level(levels: &mut VecDeque<f32>, value: f32) {
    levels.push_back(value);
    while levels.len() > 4 {
        levels.pop_front();
    }
}

fn send_optional_reply(reply: Option<tokio::sync::oneshot::Sender<Response>>, response: Response) {
    if let Some(reply) = reply {
        let _ = reply.send(response);
    }
}

fn spawn_ipc(event_tx: mpsc::Sender<CoreEvent>) {
    std::thread::Builder::new()
        .name("sori-ipc".into())
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("ipc runtime");
            runtime.block_on(async move {
                use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

                let path = sori_app::socket_path();
                let _ = std::fs::remove_file(&path);
                let listener = match tokio::net::UnixListener::bind(&path) {
                    Ok(listener) => listener,
                    Err(error) => {
                        tracing::error!(%error, "ipc_bind_failed");
                        return;
                    }
                };
                if let Err(error) = std::fs::set_permissions(&path, Permissions::from_mode(0o600)) {
                    tracing::error!(%error, "ipc_permissions_failed");
                    return;
                }
                tracing::info!(path = %path.display(), "ipc_listening");

                loop {
                    let Ok((stream, _)) = listener.accept().await else {
                        continue;
                    };
                    let event_tx = event_tx.clone();
                    tokio::spawn(async move {
                        let (reader, mut writer) = stream.into_split();
                        let mut lines = BufReader::new(reader).lines();
                        while let Ok(Some(line)) = lines.next_line().await {
                            let response = match serde_json::from_str::<Request>(&line) {
                                Ok(request) => {
                                    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
                                    if event_tx.send(CoreEvent::Ipc(request, reply_tx)).is_err() {
                                        Response::err("app is shutting down")
                                    } else {
                                        reply_rx
                                            .await
                                            .unwrap_or_else(|_| Response::err("no response"))
                                    }
                                }
                                Err(error) => Response::err(format!("bad request: {error}")),
                            };
                            let mut output = serde_json::to_string(&response).unwrap_or_default();
                            output.push('\n');
                            if writer.write_all(output.as_bytes()).await.is_err() {
                                break;
                            }
                        }
                    });
                }
            });
        })
        .expect("spawn ipc thread");
}

fn init_logging() {
    let _ = sori_app::ensure_dirs();
    let path = sori_app::sori_dir().join("sori.log");
    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .append(true)
        .mode(0o600)
        .open(path)
    {
        tracing_subscriber::fmt()
            .with_writer(Mutex::new(file))
            .with_ansi(false)
            .with_max_level(tracing::Level::DEBUG)
            .init();
    } else {
        tracing_subscriber::fmt().init();
    }
}

fn acquire_core_lock() -> std::io::Result<File> {
    sori_app::ensure_dirs()?;
    let path = sori_app::sori_dir().join("core.lock");
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(path)?;
    file.set_permissions(Permissions::from_mode(0o600))?;
    file.try_lock_exclusive()?;
    Ok(file)
}

fn main() {
    if let Err(error) = sori_app::secure_existing_data() {
        eprintln!("Sori could not secure its data directory: {error}");
        std::process::exit(1);
    }
    init_logging();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "sori_core_starting");
    let core_lock = match acquire_core_lock() {
        Ok(lock) => lock,
        Err(error) => {
            tracing::info!(%error, "another_core_is_running");
            return;
        }
    };

    let _core_lock = core_lock;
    let (event_tx, event_rx) = mpsc::channel();
    spawn_ipc(event_tx.clone());
    let mut core = Core::new(event_tx);

    loop {
        match event_rx.recv_timeout(Duration::from_secs(1)) {
            Ok(CoreEvent::Recorder(event)) => core.on_recorder(event),
            Ok(CoreEvent::Ipc(request, reply)) => {
                if core.on_ipc(request, reply) {
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }

        core.check_start_timeout();
        if std::os::unix::process::parent_id() != core.parent_pid {
            tracing::info!("shell_gone_exiting");
            break;
        }
        if core.capture_ready {
            core.write_state();
        } else if chrono::Local::now().timestamp() % 15 == 0 {
            core.refresh_devices();
            core.write_state();
        }
    }
    core.shutdown();
}
