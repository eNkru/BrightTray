//! Chinese / English UI text. The language follows the Windows display language (Chinese for
//! any Chinese locale, English otherwise) and can be forced with `BRIGHTTRAY_LANG=zh|en`.

use std::sync::OnceLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Lang {
    Zh,
    En,
}

impl Lang {
    /// The code the page expects (`<html lang>` and its string table).
    pub fn code(self) -> &'static str {
        match self {
            Lang::Zh => "zh",
            Lang::En => "en",
        }
    }
}

fn detect() -> Lang {
    if let Ok(forced) = std::env::var("BRIGHTTRAY_LANG") {
        let forced = forced.to_ascii_lowercase();
        if forced.starts_with("zh") {
            return Lang::Zh;
        }
        if forced.starts_with("en") {
            return Lang::En;
        }
    }
    // LANG_CHINESE is primary language 0x04 for every Chinese locale.
    const LANG_CHINESE: u16 = 0x04;
    let id = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    if id & 0x3ff == LANG_CHINESE {
        Lang::Zh
    } else {
        Lang::En
    }
}

pub fn lang() -> Lang {
    static LANG: OnceLock<Lang> = OnceLock::new();
    *LANG.get_or_init(detect)
}

/// Picks the Chinese or English text for the current language.
pub fn tr(zh: &'static str, en: &'static str) -> &'static str {
    match lang() {
        Lang::Zh => zh,
        Lang::En => en,
    }
}
