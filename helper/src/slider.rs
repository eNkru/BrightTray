//! Follows the Windows brightness slider (Quick Settings / Settings > Display / brightness keys).
//!
//! Every brightness change made through the shell ends up in the power manager, which publishes
//! it as the `GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS` power setting (a 0-100 DWORD). Subscribing
//! to that setting with a callback needs no window, no injection and no elevation, and the power
//! manager also delivers the current value right after registration.
//!
//! Note: Windows only publishes this (and only shows the slider) when it owns a display it can
//! dim itself, e.g. a laptop panel. On a desktop with only external monitors nothing arrives.
//!
//! That asymmetry is why the mirroring can surprise a laptop user: the panel it dims and the
//! external monitors it does not are both on screen, and a drag keeps writing the targets -
//! coalesced by the worker to at most one write per `WRITE_INTERVAL`, and every one of those
//! writes is an EEPROM write on the monitor - for a change the user made to the panel only.
//! `BRIGHTTRAY_NO_MIRROR` turns the mirroring off; see [`mirroring_enabled`].

use std::ffi::c_void;

use windows::Win32::Foundation::{HANDLE, WIN32_ERROR};
use windows::Win32::System::Power::{
    PowerSettingRegisterNotification, PowerSettingUnregisterNotification,
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, POWERBROADCAST_SETTING,
};
use windows::Win32::System::SystemServices::GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS;
use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_POWERSETTINGCHANGE};

/// Set it to keep Windows brightness moves off the monitors. Any value that is not one of the
/// words [`mirroring_disabled`] accepts leaves the mirroring on.
const NO_MIRROR_VAR: &str = "BRIGHTTRAY_NO_MIRROR";

/// Whether a value of `BRIGHTTRAY_NO_MIRROR` means "do not mirror".
///
/// Only `1`, `true`, `yes` and `on` (trimmed, case-insensitive) turn the mirroring off. `None`
/// (the variable is unset or not valid Unicode), the empty string, an explicit off-word and any
/// unrecognised value all mean "mirror", i.e. today's behaviour: a typo must not silently
/// disable a feature the user did not ask to disable.
fn mirroring_disabled(value: Option<&str>) -> bool {
    matches!(
        value.map(str::trim).map(str::to_ascii_lowercase).as_deref(),
        Some("1" | "true" | "yes" | "on")
    )
}

/// Whether the server should follow the Windows brightness slider at all.
///
/// `BRIGHTTRAY_NO_MIRROR=1` (`true`, `yes`, `on`) skips the subscription entirely, so no
/// power-manager callback runs and no DDC/CI work is queued for a slider the system owns. With
/// the variable unset, empty or unrecognised, the mirroring is on.
pub fn mirroring_enabled() -> bool {
    !mirroring_disabled(std::env::var(NO_MIRROR_VAR).ok().as_deref())
}

type Sink = Box<dyn Fn(u32) + Send + Sync>;

/// Keeps the subscription alive; unregisters on drop.
pub struct Subscription {
    handle: *mut c_void,
    // Boxed so the pointers handed to the power manager stay valid until unregistration.
    _params: Box<DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS>,
    _sink: Box<Sink>,
}

impl Drop for Subscription {
    fn drop(&mut self) {
        unsafe {
            let _ = PowerSettingUnregisterNotification(HPOWERNOTIFY(self.handle as isize));
        }
    }
}

/// Runs on a power-manager thread pool thread.
unsafe extern "system" fn on_change(
    context: *const c_void,
    kind: u32,
    setting: *const c_void,
) -> u32 {
    if kind == PBT_POWERSETTINGCHANGE && !context.is_null() && !setting.is_null() {
        let s = &*(setting as *const POWERBROADCAST_SETTING);
        if s.PowerSetting == GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS && s.DataLength >= 4 {
            let value = std::ptr::read_unaligned(s.Data.as_ptr() as *const u32);
            let sink = &*(context as *const Sink);
            sink(value.min(100));
        }
    }
    0
}

/// Calls `sink` with every slider position (0-100), starting with the current one.
pub fn subscribe(
    sink: impl Fn(u32) + Send + Sync + 'static,
) -> windows::core::Result<Subscription> {
    let sink: Box<Sink> = Box::new(Box::new(sink));
    let mut params = Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_change),
        Context: &*sink as *const Sink as *mut c_void,
    });
    let mut handle: *mut c_void = std::ptr::null_mut();
    let err = unsafe {
        PowerSettingRegisterNotification(
            &GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS,
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(&mut *params as *mut DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS as *mut c_void),
            &mut handle,
        )
    };
    if err != WIN32_ERROR(0) {
        return Err(err.to_hresult().into());
    }
    Ok(Subscription {
        handle,
        _params: params,
        _sink: sink,
    })
}

#[cfg(test)]
mod tests {
    //! The `BRIGHTTRAY_NO_MIRROR` table: the pure decision, without an environment or a slider.

    use super::mirroring_disabled;

    #[test]
    fn explicit_on_words_disable_the_mirroring() {
        for value in ["1", "true", "yes", "on", "TRUE", "Yes", "ON"] {
            assert!(
                mirroring_disabled(Some(value)),
                "{value:?} should disable the mirroring"
            );
        }
    }

    #[test]
    fn surrounding_whitespace_is_ignored() {
        for value in [" 1 ", "\ttrue\n", "  on  ", "\r\nyes"] {
            assert!(
                mirroring_disabled(Some(value)),
                "{value:?} should disable the mirroring"
            );
        }
    }

    #[test]
    fn explicit_off_words_keep_the_mirroring() {
        for value in ["0", "false", "no", "off", "FALSE", "Off"] {
            assert!(
                !mirroring_disabled(Some(value)),
                "{value:?} should keep the mirroring"
            );
        }
    }

    #[test]
    fn unset_empty_and_junk_keep_the_mirroring() {
        // Today's behaviour is the default: only an explicit word turns it off.
        for value in [
            None,
            Some(""),
            Some("   "),
            Some("nope"),
            Some("2"),
            Some("disable"),
        ] {
            assert!(
                !mirroring_disabled(value),
                "{value:?} should keep the mirroring"
            );
        }
    }
}
