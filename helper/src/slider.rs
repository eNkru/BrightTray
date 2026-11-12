//! Follows the Windows brightness slider (Quick Settings / Settings > Display / brightness keys).
//!
//! Every brightness change made through the shell ends up in the power manager, which publishes
//! it as the `GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS` power setting (a 0-100 DWORD). Subscribing
//! to that setting with a callback needs no window, no injection and no elevation, and the power
//! manager also delivers the current value right after registration.
//!
//! Note: Windows only publishes this (and only shows the slider) when it owns a display it can
//! dim itself, e.g. a laptop panel. On a desktop with only external monitors nothing arrives.

use std::ffi::c_void;

use windows::Win32::Foundation::{HANDLE, WIN32_ERROR};
use windows::Win32::System::Power::{
    PowerSettingRegisterNotification, PowerSettingUnregisterNotification,
    DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY, POWERBROADCAST_SETTING,
};
use windows::Win32::System::SystemServices::GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS;
use windows::Win32::UI::WindowsAndMessaging::{DEVICE_NOTIFY_CALLBACK, PBT_POWERSETTINGCHANGE};

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
