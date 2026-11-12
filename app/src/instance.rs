//! Single instance: a second launch asks the running one to show its window, then exits.

use windows::core::w;
use windows::Win32::Foundation::{CloseHandle, GetLastError, ERROR_ALREADY_EXISTS, HANDLE};
use windows::Win32::System::Threading::{
    CreateEventW, CreateMutexW, OpenEventW, SetEvent, WaitForSingleObject, EVENT_MODIFY_STATE,
    INFINITE,
};

/// Held for the lifetime of the primary instance.
pub struct Primary {
    _mutex: HANDLE,
}

/// `Some` in the first instance. In a later one the first instance has been asked to show
/// itself and `None` is returned.
pub fn acquire() -> Option<Primary> {
    unsafe {
        let mutex = CreateMutexW(None, true, w!(r"Local\BrightTray.App")).ok()?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let _ = CloseHandle(mutex);
            if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, w!(r"Local\BrightTray.Show")) {
                let _ = SetEvent(event);
                let _ = CloseHandle(event);
            }
            return None;
        }
        Some(Primary { _mutex: mutex })
    }
}

/// Calls `on_show` every time another launch asks this instance to come forward.
pub fn listen(on_show: impl Fn() + Send + 'static) {
    let Ok(event) = (unsafe { CreateEventW(None, false, false, w!(r"Local\BrightTray.Show")) })
    else {
        return;
    };
    let raw = event.0 as isize;
    std::thread::spawn(move || loop {
        let event = HANDLE(raw as _);
        if unsafe { WaitForSingleObject(event, INFINITE) }.0 != 0 {
            return;
        }
        on_show();
    });
}
