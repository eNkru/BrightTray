//! DDC/CI brightness access through the Windows physical monitor API (dxva2.dll).

use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitor, GetNumberOfPhysicalMonitorsFromHMONITOR,
    GetPhysicalMonitorsFromHMONITOR, GetVCPFeatureAndVCPFeatureReply, SetVCPFeature,
    PHYSICAL_MONITOR,
};
use windows::Win32::Foundation::{BOOL, HANDLE, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{EnumDisplayMonitors, HDC, HMONITOR};

/// VCP code for luminance (brightness).
const VCP_BRIGHTNESS: u8 = 0x10;
/// Some monitors drop a DDC/CI command now and then; retry a few times.
const RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 60;

#[derive(Debug, Clone, Copy)]
pub struct Brightness {
    pub current: u32,
    pub max: u32,
}

/// One physical monitor. The handle is released on drop.
pub struct Monitor {
    handle: HANDLE,
    pub description: String,
}

impl Monitor {
    pub fn brightness(&self) -> Option<Brightness> {
        for attempt in 0..RETRIES {
            let mut current = 0u32;
            let mut max = 0u32;
            let ok = unsafe {
                GetVCPFeatureAndVCPFeatureReply(
                    self.handle,
                    VCP_BRIGHTNESS,
                    None,
                    &mut current,
                    Some(&mut max),
                )
            };
            if ok != 0 && max > 0 {
                return Some(Brightness { current, max });
            }
            if attempt + 1 < RETRIES {
                std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
            }
        }
        None
    }

    /// Sets brightness as a percentage (0-100), scaled to the monitor's own range.
    pub fn set_brightness_percent(&self, percent: u32) -> bool {
        let Some(b) = self.brightness() else {
            return false;
        };
        let value = (percent.min(100) * b.max + 50) / 100;
        for attempt in 0..RETRIES {
            if unsafe { SetVCPFeature(self.handle, VCP_BRIGHTNESS, value) } != 0 {
                return true;
            }
            if attempt + 1 < RETRIES {
                std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
            }
        }
        false
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyPhysicalMonitor(self.handle);
        }
    }
}

unsafe extern "system" fn collect(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let out = &mut *(data.0 as *mut Vec<Monitor>);

    let mut count = 0u32;
    if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut count).is_err() || count == 0 {
        return TRUE;
    }
    let mut buf = vec![PHYSICAL_MONITOR::default(); count as usize];
    if GetPhysicalMonitorsFromHMONITOR(hmon, &mut buf).is_err() {
        return TRUE;
    }
    for pm in buf {
        // PHYSICAL_MONITOR is packed: copy fields out before using them.
        let handle = pm.hPhysicalMonitor;
        let raw = pm.szPhysicalMonitorDescription;
        let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        out.push(Monitor {
            handle,
            description: String::from_utf16_lossy(&raw[..len]),
        });
    }
    TRUE
}

/// Enumerates every physical monitor attached to the desktop.
pub fn enumerate() -> Vec<Monitor> {
    let mut monitors: Vec<Monitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(collect),
            LPARAM(&mut monitors as *mut _ as isize),
        );
    }
    monitors
}
