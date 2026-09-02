//! Optional post-recording hook.

use std::path::Path;
use std::process::Command;

/// Run `~/Sori/on-finish <folder>` if it exists and is executable.
pub fn run_finish_hook(folder: &Path) -> bool {
    let hook = crate::hook_path();
    if !hook.is_file() {
        return false;
    }

    match Command::new(&hook)
        .arg(folder)
        .current_dir(folder)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(_) => {
            tracing::info!(hook = %hook.display(), folder = %folder.display(), "on_finish_hook_started");
            true
        }
        Err(error) => {
            tracing::warn!(%error, hook = %hook.display(), "on_finish_hook_failed");
            false
        }
    }
}
