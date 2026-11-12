//! DDC/CI brightness access through the Windows physical monitor API (dxva2.dll).

use std::collections::HashMap;

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
}

impl Monitor {
    /// Identifies this monitor across enumerations as long as the display layout is unchanged.
    pub fn key(&self) -> String {
        format!("{}#{}|{}", self.device, self.sub_index, self.name)
    }

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
