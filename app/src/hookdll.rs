//! The hook DLL, embedded in BrightTray.exe so the app ships as a single file.
//!
//! ShellHost loads the hook with `LoadLibraryW`, which needs a file on disk, so the embedded
//! copy is written to `%APPDATA%\BrightTray\brighttray_hook.dll` before injecting.
//!
//! It sits next to the settings rather than in `%LOCALAPPDATA%`: on the development machine
//! ShellHost refused to load the very same file from `%LOCALAPPDATA%\BrightTray`, where it
//! inherits an app-container ACE from the parent folder, and loaded it from here.

use std::path::{Path, PathBuf};

use brighttray::inject::HOOK_DLL;

const BYTES: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/brighttray_hook.dll"));
/// A previous version's DLL that was still loaded in ShellHost when it had to be replaced.
/// The uninstall removes these by the same prefix.
pub(crate) const STALE_PREFIX: &str = "brighttray_hook.old-";

fn dir() -> Result<PathBuf, String> {
    std::env::var_os("APPDATA")
        .map(|dir| PathBuf::from(dir).join("BrightTray"))
        .ok_or_else(|| "APPDATA is not set".to_string())
}

/// Writes the embedded hook DLL to disk if it is not there already and returns its path.
pub fn extract() -> Result<PathBuf, String> {
    let dir = dir()?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    remove_stale(&dir);

    let path = dir.join(HOOK_DLL);
    if std::fs::read(&path).is_ok_and(|existing| existing == BYTES) {
        return Ok(path);
    }
    if std::fs::write(&path, BYTES).is_ok() {
        return Ok(path);
    }

    // A different version is on disk and ShellHost still has it loaded, so it cannot be
    // overwritten. A loaded DLL can be renamed, though: move it aside and write the new one.
    // The running ShellHost keeps the old hook until it restarts.
    let stale = dir.join(format!("{STALE_PREFIX}{}.dll", std::process::id()));
    std::fs::rename(&path, &stale).map_err(|e| format!("replace {}: {e}", path.display()))?;
    std::fs::write(&path, BYTES).map_err(|e| format!("write {}: {e}", path.display()))?;
    Ok(path)
}

/// Deletes DLLs moved aside by earlier runs; the ones still loaded somewhere stay for now.
fn remove_stale(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(STALE_PREFIX)
        {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}
