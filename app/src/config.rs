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
        let mut config = Config::default();
        let Some(text) = path().and_then(|p| std::fs::read_to_string(p).ok()) else {
            return config;
        };
        for line in text.lines() {
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "monitor_key" if !value.is_empty() => config.monitor_key = Some(value.to_string()),
                "monitor_name" if !value.is_empty() => {
                    config.monitor_name = Some(value.to_string())
                }
                _ => {}
            }
        }
        config
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
