//! Shared pieces of Sori: paths, on-disk contracts (meta.json / state.json),
//! the CLI↔app IPC protocol, the recording engine, and helpers.

pub mod audio;
pub mod devices;
pub mod hook;
pub mod recorder;
pub mod wav;

use std::fs::{DirBuilder, OpenOptions, Permissions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Paths
// ---------------------------------------------------------------------------

pub fn home() -> PathBuf {
    PathBuf::from(std::env::var("HOME").expect("HOME is not set"))
}

/// `~/Sori`
pub fn sori_dir() -> PathBuf {
    home().join("Sori")
}

/// `~/Sori/recordings`
pub fn recordings_dir() -> PathBuf {
    sori_dir().join("recordings")
}

/// `~/Sori/recordings/latest` (symlink to the newest finished recording)
pub fn latest_link() -> PathBuf {
    recordings_dir().join("latest")
}

/// `~/Sori/state.json` — mirrored app state, rewritten on every change.
pub fn state_file() -> PathBuf {
    sori_dir().join("state.json")
}

/// `~/Sori/sori.sock` — unix socket the CLI talks to.
pub fn socket_path() -> PathBuf {
    sori_dir().join("sori.sock")
}

/// `~/Sori/on-finish` — user hook, executed with the folder as `$1` if present.
pub fn hook_path() -> PathBuf {
    sori_dir().join("on-finish")
}

/// `~/Sori/settings.json`
pub fn settings_file() -> PathBuf {
    sori_dir().join("settings.json")
}

pub fn ensure_dirs() -> std::io::Result<()> {
    create_private_dir(&sori_dir())?;
    create_private_dir(&recordings_dir())
}

fn create_private_dir(path: &Path) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(path)?;
    std::fs::set_permissions(path, Permissions::from_mode(0o700))
}

/// Tighten data created by older versions without following symlinks or
/// changing the user-owned `on-finish` hook.
pub fn secure_existing_data() -> std::io::Result<()> {
    ensure_dirs()?;
    for path in [
        state_file(),
        settings_file(),
        sori_dir().join("sori.log"),
        sori_dir().join("core.lock"),
    ] {
        if std::fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_file()) {
            std::fs::set_permissions(path, Permissions::from_mode(0o600))?;
        }
    }
    for entry in std::fs::read_dir(recordings_dir())? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            std::fs::set_permissions(&path, Permissions::from_mode(0o700))?;
            for file in std::fs::read_dir(path)? {
                let file = file?;
                if file.file_type()?.is_file() {
                    std::fs::set_permissions(file.path(), Permissions::from_mode(0o600))?;
                }
            }
        }
    }
    Ok(())
}

/// Write `content` to `path` atomically (tmp file + rename).
pub fn write_atomic(path: &Path, content: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .mode(0o600)
        .open(&tmp)?;
    file.set_permissions(Permissions::from_mode(0o600))?;
    file.write_all(content)?;
    drop(file);
    std::fs::rename(&tmp, path)
}

// ---------------------------------------------------------------------------
// meta.json — treated as a contract. Do not rename fields casually.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordingStatus {
    Recording,
    Done,
    Failed,
    /// The core exited while this was recording. Files are still usable (headers are
    /// flushed every 5 s); only the tail may be missing.
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceSwitch {
    /// Seconds since recording start.
    pub at_sec: u64,
    pub device: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MetaDevices {
    pub mic: String,
    pub system: String,
    #[serde(default)]
    pub switches: Vec<DeviceSwitch>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MetaTracks {
    pub mic: String,
    pub system: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub status: RecordingStatus,
    pub started_at: chrono::DateTime<chrono::Local>,
    pub duration_sec: u64,
    pub sample_rate: u32,
    pub tracks: MetaTracks,
    pub devices: MetaDevices,
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl Meta {
    pub fn path_in(folder: &Path) -> PathBuf {
        folder.join("meta.json")
    }

    pub fn load(folder: &Path) -> Option<Meta> {
        let s = std::fs::read_to_string(Self::path_in(folder)).ok()?;
        serde_json::from_str(&s).ok()
    }

    pub fn save(&self, folder: &Path) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        write_atomic(&Self::path_in(folder), &json)
    }
}

// ---------------------------------------------------------------------------
// state.json
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StateMic {
    pub device: String,
    /// `false` while the mic has produced nothing but silence.
    pub level_ok: bool,
    /// Peak amplitude 0..1 of the latest 250 ms window (0 when idle).
    #[serde(default)]
    pub level: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct StateSystem {
    pub device: String,
    #[serde(default)]
    pub level: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppState {
    /// "idle" | "recording"
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub folder: Option<PathBuf>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub started_at: Option<chrono::DateTime<chrono::Local>>,
    pub elapsed_sec: u64,
    pub mic: StateMic,
    pub system: StateSystem,
    /// Why the last start attempt failed, if it did. Cleared on the next successful start.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub updated_at: chrono::DateTime<chrono::Local>,
}

impl AppState {
    pub fn idle(mic: &str, system: &str) -> Self {
        Self {
            status: "idle".into(),
            folder: None,
            started_at: None,
            elapsed_sec: 0,
            mic: StateMic {
                device: mic.into(),
                level_ok: true,
                level: 0.0,
            },
            system: StateSystem {
                device: system.into(),
                level: 0.0,
            },
            last_error: None,
            updated_at: chrono::Local::now(),
        }
    }

    pub fn load() -> Option<AppState> {
        let s = std::fs::read_to_string(state_file()).ok()?;
        serde_json::from_str(&s).ok()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        write_atomic(&state_file(), &json)
    }
}

// ---------------------------------------------------------------------------
// settings.json
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Settings {
    /// `None` = follow the system default input (skipping virtual devices).
    pub mic_override: Option<String>,
}

impl Settings {
    pub fn load() -> Settings {
        std::fs::read_to_string(settings_file())
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> std::io::Result<()> {
        let json = serde_json::to_vec_pretty(self)?;
        write_atomic(&settings_file(), &json)
    }
}

// ---------------------------------------------------------------------------
// IPC protocol (newline-delimited JSON over the unix socket)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    Start {
        #[serde(default)]
        mic: Option<String>,
    },
    Stop,
    Status,
    List {
        #[serde(default = "default_limit")]
        limit: usize,
    },
    Devices,
    /// `mic: None` = automatic (follow system default).
    SetMic {
        #[serde(default)]
        mic: Option<String>,
    },
    /// Stop any recording and exit the core process.
    Quit,
}

fn default_limit() -> usize {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl Response {
    pub fn ok(data: impl Serialize) -> Self {
        Self {
            ok: true,
            data: Some(serde_json::to_value(data).unwrap_or(serde_json::Value::Null)),
            error: None,
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            data: None,
            error: Some(msg.into()),
        }
    }
}

// ---------------------------------------------------------------------------
// Recordings on disk
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
pub struct RecordingEntry {
    pub folder: PathBuf,
    pub started_at: chrono::DateTime<chrono::Local>,
    pub duration_sec: u64,
    pub status: RecordingStatus,
}

impl RecordingEntry {
    /// "Sep 2, 14:30 · 42 min"
    pub fn label(&self) -> String {
        format!(
            "{} · {}",
            self.started_at.format("%b %-d, %H:%M"),
            format_minutes(self.duration_sec)
        )
    }
}

pub fn format_minutes(sec: u64) -> String {
    if sec < 60 {
        format!("{sec} sec")
    } else {
        format!("{} min", sec / 60)
    }
}

/// "12:34" or "1:02:03"
pub fn format_elapsed(sec: u64) -> String {
    let h = sec / 3600;
    let m = (sec % 3600) / 60;
    let s = sec % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Newest first. Skips folders without a readable meta.json.
pub fn list_recordings(limit: usize) -> Vec<RecordingEntry> {
    let mut out = Vec::new();
    let Ok(rd) = std::fs::read_dir(recordings_dir()) else {
        return out;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() || path.file_name().is_some_and(|n| n == "latest") {
            continue;
        }
        if let Some(meta) = Meta::load(&path) {
            out.push(RecordingEntry {
                folder: path,
                started_at: meta.started_at,
                duration_sec: meta.duration_sec,
                status: meta.status,
            });
        }
    }
    out.sort_by(|a, b| b.started_at.cmp(&a.started_at));
    out.truncate(limit);
    out
}

/// At core startup nothing can be recording. Any meta.json still saying `recording`
/// belongs to a session that died — mark it `interrupted` so the UI stops lying.
pub fn mark_stale_recordings() -> usize {
    let mut n = 0;
    let Ok(rd) = std::fs::read_dir(recordings_dir()) else {
        return 0;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if let Some(mut meta) = Meta::load(&path)
            && meta.status == RecordingStatus::Recording
        {
            meta.status = RecordingStatus::Interrupted;
            meta.warnings
                .push("interrupted: core exited during recording".into());
            if meta.duration_sec == 0 {
                // best effort: derive from the mic file length (16-bit mono)
                if let Ok(m) = std::fs::metadata(path.join(&meta.tracks.mic))
                    && meta.sample_rate > 0
                    && m.len() > 44
                {
                    meta.duration_sec = (m.len() - 44) / 2 / meta.sample_rate as u64;
                }
            }
            if meta.save(&path).is_ok() {
                n += 1;
            }
        }
    }
    n
}

/// Create `~/Sori/recordings/YYYY-MM-DD-HHMM[-n]`.
pub fn new_recording_folder(now: chrono::DateTime<chrono::Local>) -> std::io::Result<PathBuf> {
    ensure_dirs()?;
    let base = now.format("%Y-%m-%d-%H%M").to_string();
    let mut candidate = recordings_dir().join(&base);
    let mut n = 2;
    while candidate.exists() {
        candidate = recordings_dir().join(format!("{base}-{n}"));
        n += 1;
    }
    create_private_dir(&candidate)?;
    Ok(candidate)
}

/// Point `recordings/latest` at `folder`.
pub fn update_latest_link(folder: &Path) -> std::io::Result<()> {
    let link = latest_link();
    let _ = std::fs::remove_file(&link);
    let target = folder
        .file_name()
        .map(PathBuf::from)
        .unwrap_or(folder.to_path_buf());
    std::os::unix::fs::symlink(target, link)
}

#[cfg(test)]
mod permission_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn private_directory_and_atomic_file_modes_ignore_umask_defaults() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root =
            std::env::temp_dir().join(format!("sori-permissions-{}-{nonce}", std::process::id()));
        create_private_dir(&root).unwrap();
        let file = root.join("state.json");
        write_atomic(&file, b"{}").unwrap();

        assert_eq!(
            std::fs::metadata(&root).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );

        std::fs::remove_file(file).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
}
