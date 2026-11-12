//! "Start with Windows" through `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegOpenKeyExW, RegQueryValueExW, RegSetValueExW, HKEY,
    HKEY_CURRENT_USER, KEY_QUERY_VALUE, KEY_SET_VALUE, REG_SZ,
};

const RUN_KEY: PCWSTR = w!(r"Software\Microsoft\Windows\CurrentVersion\Run");
const VALUE: PCWSTR = w!("BrightTray");
/// Passed by the Run entry so a logon start goes straight to the tray.
pub const FLAG: &str = "--autostart";

struct Key(HKEY);

impl Drop for Key {
    fn drop(&mut self) {
        unsafe {
            let _ = RegCloseKey(self.0);
        }
    }
}

fn open(write: bool) -> Option<Key> {
    let access = if write {
        KEY_SET_VALUE
    } else {
        KEY_QUERY_VALUE
    };
    let mut key = HKEY::default();
    let status = unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, RUN_KEY, 0, access, &mut key) };
    (status == ERROR_SUCCESS).then_some(Key(key))
}

fn command() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    Some(format!("\"{}\" {FLAG}", exe.display()))
}

/// The command currently registered, if any.
fn registered() -> Option<String> {
    let key = open(false)?;
    let mut size = 0u32;
    let status = unsafe { RegQueryValueExW(key.0, VALUE, None, None, None, Some(&mut size)) };
    if status != ERROR_SUCCESS || size == 0 {
        return None;
    }
    let mut buffer = vec![0u16; (size as usize).div_ceil(2)];
    let status = unsafe {
        RegQueryValueExW(
            key.0,
            VALUE,
            None,
            None,
            Some(buffer.as_mut_ptr() as *mut u8),
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    Some(String::from_utf16_lossy(&buffer[..len]))
}

pub fn is_enabled() -> bool {
    registered().is_some()
}

pub fn set_enabled(enabled: bool) -> Result<(), String> {
    let key = open(true).ok_or("无法打开注册表启动项")?;
    let status = if enabled {
        let command = command().ok_or("无法获取程序路径")?;
        let data: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes =
            unsafe { std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * 2) };
        unsafe { RegSetValueExW(key.0, VALUE, 0, REG_SZ, Some(bytes)) }
    } else {
        let status = unsafe { RegDeleteValueW(key.0, VALUE) };
        if status == windows::Win32::Foundation::ERROR_FILE_NOT_FOUND {
            ERROR_SUCCESS
        } else {
            status
        }
    };
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(format!("写入注册表失败（错误 {}）", status.0))
    }
}

/// If the exe moved since autostart was enabled, point the Run entry at the current copy.
pub fn refresh_path() {
    if let (Some(current), Some(wanted)) = (registered(), command()) {
        if current != wanted {
            let _ = set_enabled(true);
        }
    }
}
