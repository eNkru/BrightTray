//! Windows build detection for the ShellHost Quick Settings hook.
//!
//! `RtlGetVersion` is used instead of `GetVersionEx`: the latter can report a
//! compatibility version when the process has no matching manifest. Quick
//! Settings moved to ShellHost.exe in Windows 11 24H2 (build 26100), so older
//! builds must not be sent through the hook path. If the build cannot be read,
//! the hook's own slot and identity validation remains the safety net.

use std::sync::OnceLock;

/// First build whose Quick Settings panel is hosted by `ShellHost.exe`.
pub const MIN_QUICK_SETTINGS_BUILD: u32 = 26100;

#[repr(C)]
struct OsVersionInfoW {
    size: u32,
    major: u32,
    minor: u32,
    build: u32,
    platform_id: u32,
    csd_version: [u16; 128],
}

#[link(name = "ntdll")]
unsafe extern "system" {
    /// Unlike `GetVersionEx`, this is not subject to manifest-based version lies.
    fn RtlGetVersion(info: *mut OsVersionInfoW) -> i32;
}

fn detect() -> u32 {
    if let Some(fake) = std::env::var("BRIGHTTRAY_FAKE_BUILD")
        .ok()
        .and_then(|value| value.trim().parse().ok())
    {
        return fake;
    }

    let mut info = OsVersionInfoW {
        size: std::mem::size_of::<OsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform_id: 0,
        csd_version: [0; 128],
    };
    if unsafe { RtlGetVersion(&mut info) } == 0 {
        info.build
    } else {
        0
    }
}

/// The Windows build number, or `0` when it could not be read.
pub fn build() -> u32 {
    static BUILD: OnceLock<u32> = OnceLock::new();
    *BUILD.get_or_init(detect)
}

/// Whether the Quick Settings hook can work on a build number.
fn supported(build: u32) -> bool {
    build == 0 || build >= MIN_QUICK_SETTINGS_BUILD
}

/// Whether Quick Settings is expected to be hosted by `ShellHost.exe`.
pub fn quick_settings_supported() -> bool {
    supported(build())
}

#[cfg(test)]
mod tests {
    use super::{supported, MIN_QUICK_SETTINGS_BUILD};

    #[test]
    fn unreadable_build_is_allowed_for_hook_validation() {
        assert!(supported(0));
    }

    #[test]
    fn older_build_is_not_supported() {
        assert!(!supported(MIN_QUICK_SETTINGS_BUILD - 1));
    }

    #[test]
    fn minimum_build_is_supported() {
        assert!(supported(MIN_QUICK_SETTINGS_BUILD));
    }

    #[test]
    fn newer_build_is_supported() {
        assert!(supported(MIN_QUICK_SETTINGS_BUILD + 1));
    }
}
