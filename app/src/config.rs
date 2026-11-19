//! Persistent settings in `%APPDATA%\BrightTray\config.ini` (plain `key=value` lines).

use std::io::Write;
use std::path::{Path, PathBuf};

/// The settings file's name inside `%APPDATA%\BrightTray`; the uninstall removes it by name.
pub(crate) const FILE: &str = "config.ini";

#[derive(Debug, Clone, Default)]
pub struct Config {
    /// The monitor the user last applied, as [`brighttray::ddc::Monitor::key`] and name.
    pub monitor_key: Option<String>,
    pub monitor_name: Option<String>,
}

fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("BrightTray").join(FILE))
}

impl Config {
    pub fn load() -> Config {
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return Config::default();
        };
        parse(&text)
    }

    pub fn save(&self) {
        let Some(path) = path() else {
            return;
        };
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let text = format!(
            "monitor_key={}\nmonitor_name={}\n",
            self.monitor_key.as_deref().unwrap_or(""),
            self.monitor_name.as_deref().unwrap_or(""),
        );
        // Best effort, exactly as before: nothing the user can act on, and the setting is
        // re-entered on demand.
        let _ = write_atomically(&path, text.as_bytes());
    }
}

/// Writes `text` to `path` so a crash can never leave a partial file behind: the bytes go to
/// a temporary file next to it, are flushed, and only then replace the old file with a
/// rename — one operation that either swaps in the new content or keeps the old one.
fn write_atomically(path: &Path, text: &[u8]) -> std::io::Result<()> {
    let tmp = path.with_extension("ini.tmp");
    let result = (|| {
        let mut file = std::fs::File::create(&tmp)?;
        file.write_all(text)?;
        file.sync_all()?;
        // Windows refuses to rename a file that is still open (ERROR_ACCESS_DENIED), so the
        // handle has to be closed before the swap.
        drop(file);
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        // A half-written temporary file is not a settings file; leaving it behind would only
        // accumulate. Removing it is best effort too — it may be a directory a test put there.
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// Parses the settings file: one `key=value` per line.
///
/// A line without an `=` is not a setting and is skipped, an unknown key is not ours and is
/// skipped too. The value is trimmed, and a key with an empty value stays unset — that is how
/// [`Config::save`] writes "unset". A value may contain an `=` itself: only the first one
/// separates the key from the value.
fn parse(text: &str) -> Config {
    let mut config = Config::default();
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim();
        match key.trim() {
            "monitor_key" if !value.is_empty() => config.monitor_key = Some(value.to_string()),
            "monitor_name" if !value.is_empty() => config.monitor_name = Some(value.to_string()),
            _ => {}
        }
    }
    config
}

#[cfg(test)]
mod tests {
    //! The settings file format: one `key=value` per line, and an empty value leaves the key
    //! unset. The file is written by `save` and read by `load`, so the two have to agree.

    use super::{parse, write_atomically};
    use std::path::{Path, PathBuf};

    #[test]
    fn a_key_value_pair_is_parsed() {
        let config = parse("monitor_key=abc\nmonitor_name=Monitor\n");
        assert_eq!(config.monitor_key.as_deref(), Some("abc"));
        assert_eq!(config.monitor_name.as_deref(), Some("Monitor"));
    }

    #[test]
    fn an_empty_value_leaves_the_key_unset() {
        let config = parse("monitor_key=\nmonitor_name=\n");
        assert!(config.monitor_key.is_none());
        assert!(config.monitor_name.is_none());
    }

    #[test]
    fn an_unknown_key_is_ignored() {
        let config = parse("monitor_key=abc\nsomething_else=1\n");
        assert_eq!(config.monitor_key.as_deref(), Some("abc"));
        assert!(config.monitor_name.is_none());
    }

    #[test]
    fn a_line_without_an_equals_sign_is_ignored() {
        let config = parse("monitor_key\nmonitor_name=Monitor\n");
        assert!(config.monitor_key.is_none());
        assert_eq!(config.monitor_name.as_deref(), Some("Monitor"));
    }

    #[test]
    fn the_whitespace_around_the_key_and_the_value_is_trimmed() {
        let config = parse("  monitor_key  =  abc  \n");
        assert_eq!(config.monitor_key.as_deref(), Some("abc"));
    }

    #[test]
    fn a_value_may_contain_an_equals_sign() {
        // Only the first `=` separates the key from the value.
        let config = parse("monitor_key=a=b\n");
        assert_eq!(config.monitor_key.as_deref(), Some("a=b"));
    }

    #[test]
    fn the_saved_format_parses_back() {
        // The format `save` writes, read back by `parse`.
        let config = parse("monitor_key=abc\nmonitor_name=\n");
        assert_eq!(config.monitor_key.as_deref(), Some("abc"));
        assert!(config.monitor_name.is_none());
    }

    #[test]
    fn an_empty_file_leaves_everything_unset() {
        let config = parse("");
        assert!(config.monitor_key.is_none());
        assert!(config.monitor_name.is_none());
    }

    /// A directory of its own under `%TEMP%`, named with the process id and the test name so
    /// parallel tests never share one.
    fn temp_dir(test: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("brighttray-config-{}-{test}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_write_leaves_the_whole_content_and_no_tmp_file() {
        let dir = temp_dir("round-trip");
        let path = dir.join("config.ini");

        write_atomically(&path, b"monitor_key=abc\nmonitor_name=Monitor\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "monitor_key=abc\nmonitor_name=Monitor\n"
        );
        assert!(!dir.join("config.ini.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_second_write_replaces_the_file() {
        let dir = temp_dir("replace");
        let path = dir.join("config.ini");
        write_atomically(&path, b"monitor_key=old\nmonitor_name=Old\n").unwrap();

        write_atomically(&path, b"monitor_key=new\nmonitor_name=New\n").unwrap();

        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "monitor_key=new\nmonitor_name=New\n"
        );
        assert!(!dir.join("config.ini.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_write_keeps_the_previous_file() {
        let dir = temp_dir("failed-write");
        let path = dir.join("config.ini");
        std::fs::write(&path, b"monitor_key=old\n").unwrap();
        // A directory at the temporary path makes `File::create` fail on every platform, which
        // is how a save that cannot even start looks from the outside.
        std::fs::create_dir(dir.join("config.ini.tmp")).unwrap();

        assert!(write_atomically(&path, b"monitor_key=new\n").is_err());

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "monitor_key=old\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_failed_rename_removes_the_temporary_file() {
        let dir = temp_dir("failed-rename");
        let path = dir.join("config.ini");
        // A non-empty directory at the final path fails the rename rather than the write, which
        // is how a destination locked by another process looks. The content is complete but
        // unused, so it must not be left behind for the next `load()` to wonder about.
        std::fs::create_dir(&path).unwrap();
        std::fs::write(path.join("occupied"), b"junk").unwrap();

        assert!(write_atomically(&path, b"monitor_key=new\n").is_err());

        assert!(path.is_dir());
        assert!(!dir.join("config.ini.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_leftover_tmp_from_a_crashed_run_is_overwritten() {
        let dir = temp_dir("leftover-tmp");
        let path = dir.join("config.ini");
        // What a run killed between the write and the rename leaves behind.
        std::fs::write(dir.join("config.ini.tmp"), b"junk").unwrap();

        write_atomically(&path, b"monitor_key=new\n").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "monitor_key=new\n");
        assert!(!dir.join("config.ini.tmp").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_tmp_file_is_config_ini_tmp_next_to_the_settings() {
        // The checkpoint sits in `%APPDATA%\BrightTray` while a save is running and
        // `a_failed_write_keeps_the_previous_file` depends on the name, so pin it here instead
        // of leaving it implicit in the helper.
        let path = Path::new("BrightTray").join("config.ini");
        assert_eq!(
            path.with_extension("ini.tmp"),
            path.with_file_name("config.ini.tmp")
        );
    }
}
