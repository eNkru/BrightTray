//! Persistent settings in `%APPDATA%\BrightTray\config.ini` (plain `key=value` lines).

use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct Config {
    /// The monitor the user last applied, as [`brighttray::ddc::Monitor::key`] and name.
    pub monitor_key: Option<String>,
    pub monitor_name: Option<String>,
}

fn path() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("BrightTray").join("config.ini"))
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
        let _ = std::fs::write(path, text);
    }
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

    use super::parse;

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
}
