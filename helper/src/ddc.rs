//! DDC/CI brightness access through the Windows physical monitor API (dxva2.dll).

use std::cell::Cell;
use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows::Win32::Devices::Display::{
    DestroyPhysicalMonitor, DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes,
    GetNumberOfPhysicalMonitorsFromHMONITOR, GetPhysicalMonitorsFromHMONITOR,
    GetVCPFeatureAndVCPFeatureReply, QueryDisplayConfig, SetVCPFeature,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO, DISPLAYCONFIG_SOURCE_DEVICE_NAME,
    DISPLAYCONFIG_TARGET_DEVICE_NAME, PHYSICAL_MONITOR, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{BOOL, ERROR_SUCCESS, HANDLE, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};

/// VCP code for luminance (brightness).
const VCP_BRIGHTNESS: u8 = 0x10;
/// Some monitors drop a DDC/CI command now and then; retry a few times.
const RETRIES: u32 = 3;
const RETRY_DELAY_MS: u64 = 60;
/// DDC/CI wants roughly 50 ms between two commands to the same monitor. Stricter monitors
/// silently drop a command that follows another one too closely.
const COMMAND_GAP: Duration = Duration::from_millis(50);
/// How many times a brightness write is sent again when reading back shows it did not land.
const VERIFY_RESENDS: u32 = 2;
/// `MONITORINFOF_PRIMARY`.
const PRIMARY_FLAG: u32 = 1;

/// The native VCP value for `percent` of a monitor whose brightness range ends at `max`.
///
/// The value is rounded to the nearest one the monitor can show, and `percent` is clamped to
/// 100, so a caller that asks for 120 gets the top of the range instead of an overflow. A
/// monitor that reports no range at all has no value to scale to, and the answer is 0.
fn scale_percent(percent: u32, max: u32) -> u32 {
    (percent.min(100) * max + 50) / 100
}

/// The percentage a raw brightness value corresponds to, rounded to the nearest percent.
///
/// The answer never exceeds 100, so a monitor that reports more than its own maximum reads as
/// full brightness. A monitor that reports no range at all has no percentage to report, and the
/// answer is 0 rather than a division by zero.
fn percent(current: u32, max: u32) -> u32 {
    if max == 0 {
        return 0;
    }
    ((current * 100 + max / 2) / max).min(100)
}

#[derive(Debug, Clone, Copy)]
pub struct Brightness {
    pub current: u32,
    pub max: u32,
}

impl Brightness {
    /// The brightness as a percentage of the monitor's own range.
    pub fn percent(&self) -> u32 {
        percent(self.current, self.max)
    }
}

/// One physical monitor. The handle is released on drop.
pub struct Monitor {
    handle: HANDLE,
    /// The description dxva2 reports; often just "Generic PnP Monitor".
    pub description: String,
    /// Best human-readable name: `description` or the EDID name, whichever is more complete.
    pub name: String,
    /// GDI device of the desktop area this monitor shows, e.g. `\\.\DISPLAY1`.
    pub device: String,
    /// Position among the physical monitors of that device (non-zero only when mirrored).
    pub sub_index: usize,
    pub width: i32,
    pub height: i32,
    pub primary: bool,
    /// When the last DDC/CI command to this monitor finished, to keep [`COMMAND_GAP`].
    last_command: Cell<Option<Instant>>,
    /// The monitor's brightness range, remembered so a write does not need a read first.
    max: Cell<Option<u32>>,
}

impl Monitor {
    /// Waits until the monitor is ready for its next command.
    fn pace(&self) {
        if let Some(last) = self.last_command.get() {
            let since = last.elapsed();
            if since < COMMAND_GAP {
                std::thread::sleep(COMMAND_GAP - since);
            }
        }
    }

    fn mark(&self) {
        self.last_command.set(Some(Instant::now()));
    }

    /// Identifies this monitor across enumerations as long as the display layout is unchanged.
    pub fn key(&self) -> String {
        format!("{}#{}|{}", self.device, self.sub_index, self.name)
    }

    pub fn brightness(&self) -> Option<Brightness> {
        for attempt in 0..RETRIES {
            let mut current = 0u32;
            let mut max = 0u32;
            self.pace();
            let ok = unsafe {
                GetVCPFeatureAndVCPFeatureReply(
                    self.handle,
                    VCP_BRIGHTNESS,
                    None,
                    &mut current,
                    Some(&mut max),
                )
            };
            self.mark();
            if ok != 0 && max > 0 {
                self.max.set(Some(max));
                return Some(Brightness { current, max });
            }
            if attempt + 1 < RETRIES {
                std::thread::sleep(std::time::Duration::from_millis(RETRY_DELAY_MS));
            }
        }
        None
    }

    /// Sends one brightness write. True when the command went out.
    fn write_raw(&self, value: u32) -> bool {
        for attempt in 0..RETRIES {
            self.pace();
            let ok = unsafe { SetVCPFeature(self.handle, VCP_BRIGHTNESS, value) } != 0;
            self.mark();
            if ok {
                return true;
            }
            if attempt + 1 < RETRIES {
                std::thread::sleep(Duration::from_millis(RETRY_DELAY_MS));
            }
        }
        false
    }

    /// The raw VCP value for `percent` on this monitor, or `None` when the range is not known
    /// yet and the monitor does not answer.
    ///
    /// Verification compares raw values, not percentages, so the caller needs the same scaling
    /// the write used.
    pub fn scale_percent(&self, percent: u32) -> Option<u32> {
        let max = match self.max.get() {
            Some(max) => max,
            None => self.brightness()?.max,
        };
        Some(scale_percent(percent, max))
    }

    /// Sends one brightness write without reading it back. True when the command went out.
    ///
    /// A slider drag calls this once per throttle window; the read-back that confirms the write
    /// belongs to the end of the drag, not to every step, because every write is an EEPROM write
    /// on the monitor.
    pub fn write_brightness_percent(&self, percent: u32) -> bool {
        match self.scale_percent(percent) {
            Some(value) => self.write_raw(value),
            None => false,
        }
    }

    /// Reads the brightness back and reports whether it matches what
    /// [`Self::write_brightness_percent`] would have written for `percent`.
    ///
    /// A monitor that does not answer counts as matched: there is nothing to compare with, and
    /// asking again will not make it answer. That includes a monitor whose brightness range is
    /// still unknown, because [`Self::scale_percent`] only fails when the read did not answer.
    /// False means the monitor answered with another value, so the caller may resend once.
    pub fn verify_brightness(&self, percent: u32) -> bool {
        let value = match self.scale_percent(percent) {
            Some(value) => value,
            None => return true,
        };
        match self.brightness() {
            Some(b) => b.current == value,
            None => true,
        }
    }

    /// Sets brightness as a percentage (0-100), scaled to the monitor's own range.
    ///
    /// `SetVCPFeature` succeeding only means the command was sent, so the value is read back
    /// and sent again if the monitor did not take it. Returns false when the command could not
    /// be sent at all (a stale handle); a monitor that keeps ignoring the write still counts as
    /// reached, since asking again will not change its mind.
    ///
    /// This is the CLI's single deliberate write. The server writes without verifying during a
    /// drag and verifies once at the end instead.
    pub fn set_brightness_percent(&self, percent: u32) -> bool {
        for _ in 0..=VERIFY_RESENDS {
            if !self.write_brightness_percent(percent) {
                return false;
            }
            if self.verify_brightness(percent) {
                break;
            }
        }
        true
    }
}

impl Drop for Monitor {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyPhysicalMonitor(self.handle);
        }
    }
}

fn wide_to_string(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// Maps each GDI device name (`\\.\DISPLAY1`) to the EDID names of the monitors showing it.
fn friendly_names() -> HashMap<String, Vec<String>> {
    let mut names: HashMap<String, Vec<String>> = HashMap::new();
    unsafe {
        let mut path_count = 0u32;
        let mut mode_count = 0u32;
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut path_count, &mut mode_count)
            != ERROR_SUCCESS
        {
            return names;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); path_count as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); mode_count as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut path_count,
            paths.as_mut_ptr(),
            &mut mode_count,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return names;
        }
        paths.truncate(path_count as usize);

        for path in &paths {
            let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
            source.header.size = std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
            source.header.adapterId = path.sourceInfo.adapterId;
            source.header.id = path.sourceInfo.id;
            if DisplayConfigGetDeviceInfo(&mut source.header) != 0 {
                continue;
            }

            let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
            target.header.size = std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
            target.header.adapterId = path.targetInfo.adapterId;
            target.header.id = path.targetInfo.id;
            if DisplayConfigGetDeviceInfo(&mut target.header) != 0 {
                continue;
            }

            let friendly = wide_to_string(&target.monitorFriendlyDeviceName);
            if !friendly.is_empty() {
                names
                    .entry(wide_to_string(&source.viewGdiDeviceName))
                    .or_default()
                    .push(friendly);
            }
        }
    }
    names
}

struct Collect {
    monitors: Vec<Monitor>,
    names: HashMap<String, Vec<String>>,
}

unsafe extern "system" fn collect(
    hmon: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let state = &mut *(data.0 as *mut Collect);

    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let have_info = GetMonitorInfoW(hmon, &mut info as *mut _ as *mut MONITORINFO).as_bool();
    let device = if have_info {
        wide_to_string(&info.szDevice)
    } else {
        String::new()
    };
    let area = info.monitorInfo.rcMonitor;
    let primary = have_info && info.monitorInfo.dwFlags & PRIMARY_FLAG != 0;

    let mut count = 0u32;
    if GetNumberOfPhysicalMonitorsFromHMONITOR(hmon, &mut count).is_err() || count == 0 {
        return TRUE;
    }
    let mut buf = vec![PHYSICAL_MONITOR::default(); count as usize];
    if GetPhysicalMonitorsFromHMONITOR(hmon, &mut buf).is_err() {
        return TRUE;
    }
    let friendly = state.names.get(&device);
    for (sub_index, pm) in buf.into_iter().enumerate() {
        // PHYSICAL_MONITOR is packed: copy fields out before using them.
        let handle = pm.hPhysicalMonitor;
        let raw = pm.szPhysicalMonitorDescription;
        let description = wide_to_string(&raw);
        // dxva2 sometimes gives the full name ("Lenovo Y27q-30") where the EDID only has the
        // model ("Y27q-30"), and sometimes only "Generic PnP Monitor". Prefer the description
        // when it contains the EDID name, the EDID name otherwise.
        let name = match friendly.and_then(|names| names.get(sub_index)) {
            Some(edid) if !description.contains(edid.as_str()) => edid.clone(),
            _ => description.clone(),
        };
        state.monitors.push(Monitor {
            handle,
            description,
            name,
            device: device.clone(),
            sub_index,
            width: area.right - area.left,
            height: area.bottom - area.top,
            primary,
            last_command: Cell::new(None),
            max: Cell::new(None),
        });
    }
    TRUE
}

/// Enumerates every physical monitor attached to the desktop.
pub fn enumerate() -> Vec<Monitor> {
    let mut state = Collect {
        monitors: Vec::new(),
        names: friendly_names(),
    };
    unsafe {
        let _ = EnumDisplayMonitors(
            HDC::default(),
            None,
            Some(collect),
            LPARAM(&mut state as *mut _ as isize),
        );
    }
    state.monitors
}

#[cfg(test)]
mod tests {
    //! The scaling arithmetic: what a percentage means in the monitor's own units. Every
    //! brightness write and every read-back comparison goes through these two numbers, and
    //! neither of them needs a monitor — only the range it works on.

    use super::{percent, scale_percent};

    #[test]
    fn a_percentage_scales_to_the_monitors_own_range() {
        // A range of 100 is the percentage itself.
        assert_eq!(scale_percent(50, 100), 50);
        assert_eq!(scale_percent(0, 100), 0);
        assert_eq!(scale_percent(100, 100), 100);
        // A range of 255 (the usual VCP range) is scaled, not taken as is.
        assert_eq!(scale_percent(100, 255), 255);
        assert_eq!(scale_percent(0, 255), 0);
    }

    #[test]
    fn the_scaled_value_is_rounded_to_the_nearest_one() {
        // 50 % of 255 is 127.5, which rounds up.
        assert_eq!(scale_percent(50, 255), 128);
        // 1 % of 255 is 2.55, which rounds up.
        assert_eq!(scale_percent(1, 255), 3);
        // 10 % of 255 is 25.5, which rounds up.
        assert_eq!(scale_percent(10, 255), 26);
    }

    #[test]
    fn a_scaled_percentage_above_one_hundred_is_clamped() {
        // A caller that asks for 120 gets the top of the range, not an overflow.
        assert_eq!(scale_percent(120, 255), 255);
        assert_eq!(scale_percent(120, 100), 100);
    }

    #[test]
    fn a_monitor_without_a_range_scales_to_nothing() {
        // No range, no value to scale to — and no division by zero.
        assert_eq!(scale_percent(50, 0), 0);
    }

    #[test]
    fn a_raw_value_scales_back_to_a_percentage() {
        assert_eq!(percent(50, 100), 50);
        assert_eq!(percent(0, 100), 0);
        assert_eq!(percent(100, 100), 100);
        // The value a write of 50 % produces on a 255 range reads back as 50 %.
        assert_eq!(percent(scale_percent(50, 255), 255), 50);
    }

    #[test]
    fn the_percentage_is_rounded_to_the_nearest_one() {
        // 2 of 255 is 0.78 %, which rounds up to 1.
        assert_eq!(percent(2, 255), 1);
        // 1 of 255 is 0.39 %, which rounds down to 0.
        assert_eq!(percent(1, 255), 0);
        // 128 of 255 is 50.2 %, which rounds down to 50.
        assert_eq!(percent(128, 255), 50);
        // 129 of 255 is 50.6 %, which rounds up to 51.
        assert_eq!(percent(129, 255), 51);
    }

    #[test]
    fn a_raw_value_above_one_hundred_is_clamped() {
        // A monitor that reports more than its own maximum reads as full brightness.
        assert_eq!(percent(150, 100), 100);
        assert_eq!(percent(255, 255), 100);
    }

    #[test]
    fn a_monitor_without_a_range_has_no_percentage() {
        // No range, no percentage — and no division by zero.
        assert_eq!(percent(50, 0), 0);
    }
}
