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

#[derive(Debug, Clone, Copy)]
pub struct Brightness {
    pub current: u32,
    pub max: u32,
}

impl Brightness {
    pub fn percent(&self) -> u32 {
        ((self.current * 100 + self.max / 2) / self.max).min(100)
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
        Some((percent.min(100) * max + 50) / 100)
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
