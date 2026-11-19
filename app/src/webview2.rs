//! Is the WebView2 Evergreen Runtime installed?
//!
//! The app is a GUI binary with no console, and WebView2 is an implicit hard dependency: the
//! window is built and then the webview, and a missing runtime used to be nothing but an
//! `expect()` that nobody could see. This module answers the question once, before the window
//! exists, so the app can say what is wrong instead of failing silently.
//!
//! Detection is the EdgeUpdate client registration Microsoft documents for WebView2 — the
//! `pv` value under `...\Microsoft\EdgeUpdate\Clients\{F3017226-…}`. Both hives are read,
//! because a per-user install writes only `HKCU`; checking `HKLM` alone would report a working
//! installation as missing.
//!
//! The check is deliberately **cautious**: only "both hives clearly say not installed" is
//! reported as missing. An unreadable key (permissions, a policy-locked hive, a future WebView2
//! that registers somewhere else) counts as installed and lets `wry` try, because a wrong
//! "missing" dialog would block a user whose runtime works, while a wrong "installed" leaves
//! exactly today's behaviour.
//!
//! It is not part of the `brighttray` helper library on purpose: only the GUI embeds a
//! webview, so the check would be a GUI-only concern in the CLI's surface for zero reuse.

use std::sync::OnceLock;

use windows::core::{w, HSTRING, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PATH_NOT_FOUND, ERROR_SUCCESS, HWND};
use windows::Win32::System::Registry::{
    RegCloseKey, RegOpenKeyExW, RegQueryValueExW, HKEY, HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE,
    KEY_QUERY_VALUE,
};
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK, MB_SETFOREGROUND};

use crate::i18n::tr;

/// The EdgeUpdate client that registers the WebView2 Runtime.
const MACHINE_KEY: PCWSTR =
    w!(r"SOFTWARE\WOW6432Node\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}");
/// Where a per-user install registers instead; `HKLM` alone would miss it.
const USER_KEY: PCWSTR =
    w!(r"Software\Microsoft\EdgeUpdate\Clients\{F3017226-FE2A-4295-8BDF-00C3A9A7E4C5}");
/// The version value; `pv` is what Microsoft's own detection reads.
const VERSION_VALUE: PCWSTR = w!("pv");
/// Where the user can get the runtime; the READMEs carry the same address.
const DOWNLOAD_URL: &str = "https://go.microsoft.com/fwlink/p/?LinkId=2124703";

/// What one hive says about the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Presence {
    Installed,
    /// The key or value is missing, or `pv` is the "uninstalled but key left behind" literal.
    Absent,
    /// The read failed for a reason that is not "not found": access denied, a redirected hive.
    Unknown,
}

/// Whether a `pv` value from the EdgeUpdate registration means "installed".
///
/// `None` is a missing value; the empty string and `"0.0.0.0"` are what the uninstaller leaves
/// behind, and neither means a usable runtime. Anything else is a version string.
fn version_present(version: Option<&str>) -> bool {
    match version.map(str::trim) {
        Some(version) => !version.is_empty() && version != "0.0.0.0",
        None => false,
    }
}

/// The decision over the two hives: installed if either says so, missing only if both clearly
/// say not installed. `Unknown` wins over `Absent` on purpose — see the module comment.
fn decide(machine: Presence, user: Presence) -> bool {
    match (machine, user) {
        (Presence::Unknown, _) | (_, Presence::Unknown) => true,
        (Presence::Installed, _) | (_, Presence::Installed) => true,
        (Presence::Absent, Presence::Absent) => false,
    }
}

/// The `BRIGHTTRAY_FAKE_WEBVIEW2` override: `0` reports missing, `1` reports installed.
///
/// Same idiom as `BRIGHTTRAY_FAKE_BUILD` in `osver.rs` — no test-only compilation, no separate
/// build — and inert unless set. It is what makes the dialog reachable on a machine that has
/// the runtime.
fn parse_override(value: Option<&str>) -> Option<bool> {
    match value?.trim() {
        "0" => Some(false),
        "1" => Some(true),
        _ => None,
    }
}

/// Reads `pv` from one `EdgeUpdate` clients key.
///
/// This is a small read-only helper of its own rather than `autostart`'s wrapper: that one is
/// read-write and tied to the Run value, and this one exists only to answer a yes/no question
/// once at startup. Sharing it would widen `autostart`'s job for one caller.
fn read_pv(root: HKEY, path: PCWSTR) -> Presence {
    let mut key = HKEY::default();
    let status = unsafe { RegOpenKeyExW(root, path, 0, KEY_QUERY_VALUE, &mut key) };
    if status == ERROR_FILE_NOT_FOUND || status == ERROR_PATH_NOT_FOUND {
        return Presence::Absent;
    }
    if status != ERROR_SUCCESS {
        return Presence::Unknown;
    }
    let key = Key(key);

    let mut size = 0u32;
    let status =
        unsafe { RegQueryValueExW(key.0, VERSION_VALUE, None, None, None, Some(&mut size)) };
    if status == ERROR_FILE_NOT_FOUND {
        return Presence::Absent;
    }
    if status != ERROR_SUCCESS {
        return Presence::Unknown;
    }
    if size == 0 {
        // The value exists but is empty: the uninstaller's leftover.
        return Presence::Absent;
    }

    let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            VERSION_VALUE,
            None,
            None,
            Some(buffer.as_mut_ptr() as *mut u8),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return Presence::Unknown;
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    if version_present(Some(&String::from_utf16_lossy(&buffer[..len]))) {
        Presence::Installed
    } else {
        Presence::Absent
    }
}

/// Closes the key on every return path of [`read_pv`].
struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn detect() -> bool {
    if let Some(forced) = parse_override(std::env::var("BRIGHTTRAY_FAKE_WEBVIEW2").ok().as_deref())
    {
        return forced;
    }
    decide(
        read_pv(HKEY_LOCAL_MACHINE, MACHINE_KEY),
        read_pv(HKEY_CURRENT_USER, USER_KEY),
    )
}

/// Whether the WebView2 Evergreen Runtime is installed, or could not be ruled out.
pub fn installed() -> bool {
    static INSTALLED: OnceLock<bool> = OnceLock::new();
    *INSTALLED.get_or_init(detect)
}

/// Tells the user why the app cannot start, then lets the caller exit.
///
/// The webview is the thing that is missing, so the message cannot be a webview page; there is
/// no console for a `println!`. Nothing opens the browser on its own — the address is short
/// enough to select by hand.
pub fn show_missing_dialog() {
    let text = HSTRING::from(missing_message());
    let caption = HSTRING::from("BrightTray");
    // MB_SETFOREGROUND: the dialog is the only thing this run shows, so it must not end up
    // behind whatever the user was doing.
    unsafe {
        MessageBoxW(
            HWND::default(),
            &text,
            &caption,
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

/// The dialog's text in the UI language, with the download address on its own line.
///
/// Separate from [`show_missing_dialog`] so the URL it has to carry can be asserted in a test
/// instead of a `debug_assert` that only fires in dev builds.
fn missing_message() -> String {
    format!(
        "{}\n{DOWNLOAD_URL}",
        tr(
            "BrightTray 需要 Microsoft Edge WebView2 运行时才能启动，但这台电脑上没有安装它。\n\n安装 WebView2 Evergreen Runtime 后再启动 BrightTray。\n下载地址：",
            "BrightTray needs the Microsoft Edge WebView2 Runtime to start, and this PC doesn't have it.\n\nInstall the WebView2 Evergreen Runtime, then start BrightTray again.\nDownload address:",
        )
    )
}

#[cfg(test)]
mod tests {
    use super::{decide, missing_message, parse_override, version_present, Presence, DOWNLOAD_URL};

    #[test]
    fn the_dialog_text_carries_the_download_address() {
        assert!(missing_message().contains(DOWNLOAD_URL));
    }

    #[test]
    fn missing_value_is_not_installed() {
        assert!(!version_present(None));
    }

    #[test]
    fn empty_value_is_not_installed() {
        assert!(!version_present(Some("")));
    }

    #[test]
    fn whitespace_only_value_is_not_installed() {
        assert!(!version_present(Some("  ")));
    }

    #[test]
    fn uninstaller_leftover_is_not_installed() {
        assert!(!version_present(Some("0.0.0.0")));
    }

    #[test]
    fn version_is_installed() {
        assert!(version_present(Some("130.0.2849.68")));
    }

    #[test]
    fn version_with_whitespace_is_installed() {
        assert!(version_present(Some(" 130.0.2849.68 ")));
    }

    #[test]
    fn either_hive_reporting_installed_is_installed() {
        for other in [Presence::Installed, Presence::Absent, Presence::Unknown] {
            assert!(decide(Presence::Installed, other));
            assert!(decide(other, Presence::Installed));
        }
    }

    #[test]
    fn only_both_hives_absent_is_missing() {
        assert!(!decide(Presence::Absent, Presence::Absent));
    }

    #[test]
    fn an_unreadable_hive_is_treated_as_installed() {
        // A wrong "missing" dialog would block a working install; let `wry` try instead.
        for other in [Presence::Absent, Presence::Installed] {
            assert!(decide(Presence::Unknown, other));
            assert!(decide(other, Presence::Unknown));
        }
    }

    #[test]
    fn fake_override_parses_zero_and_one() {
        assert_eq!(parse_override(Some("0")), Some(false));
        assert_eq!(parse_override(Some("1")), Some(true));
        assert_eq!(parse_override(Some(" 0 ")), Some(false));
    }

    #[test]
    fn fake_override_ignores_anything_else() {
        assert_eq!(parse_override(None), None);
        assert_eq!(parse_override(Some("")), None);
        assert_eq!(parse_override(Some("yes")), None);
        assert_eq!(parse_override(Some("2")), None);
    }
}
