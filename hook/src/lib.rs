//! `brighttray_hook.dll` - injected into `ShellHost.exe`.
//!
//! Windows only exposes the Quick Settings brightness tile when it finds a display whose
//! brightness it can control itself. On a desktop with external monitors that check fails, so
//! `DisplayBrightness::get_IsApplicable` returns FALSE and the tile is hidden.
//!
//! This DLL patches the shared `ISettingItem` vtable of the `DisplayBrightness` setting once:
//!
//! * `get_IsEnabled` / `get_IsApplicable` report TRUE.
//! * `GetValue("Value" / "MinValue" / "MaxValue" / "QuickActionStatus")` answer from a cached
//!   DDC/CI brightness, read and written through the BrightTray helper pipe.
//! * `SetValue("Value", Int32)` forwards the percentage to the helper.
//! * `add_SettingChanged` / `remove_SettingChanged` remember the UI handlers, so changes made
//!   from outside (the tray app) can refresh an open slider.
//!
//! ControlCenter does not bind directly to the SettingsHandlers object, though. Its Quick
//! Settings list is made of `QuickSetting` objects from `QuickActionsDataModel.dll`, which cache
//! the setting's IsApplicable/IsEnabled. A second, equally dynamic hook patches that shared
//! `IQuickAction` vtable: the object is obtained from the exported `GetQuickActionsProvider`
//! and identified by FriendlyName, so no RVA or PDB is needed here either.
//!
//! If any check fails the DLL does nothing, which is safe for ShellHost.

mod pipe;

use std::collections::HashSet;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use windows::core::{w, IInspectable, Interface, HRESULT, HSTRING, PCWSTR};
use windows::Foundation::{EventRegistrationToken, IPropertyValue, PropertyValue};
use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, E_POINTER, E_UNEXPECTED, FALSE, HINSTANCE, HMODULE, S_OK, TRUE,
};
use windows::Win32::System::LibraryLoader::{
    GetModuleHandleExW, GetModuleHandleW, GetProcAddress, LoadLibraryW,
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_PIN,
};
use windows::Win32::System::Memory::{VirtualProtect, PAGE_PROTECTION_FLAGS, PAGE_READWRITE};
use windows::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
use windows::Win32::System::SystemServices::DLL_PROCESS_ATTACH;
use windows::Win32::System::Threading::{
    GetCurrentProcess, OpenEventW, SetEvent, EVENT_MODIFY_STATE,
};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

/// `GetSetting(HSTRING id, ISettingItem** out)` from SettingsHandlers_PCDisplay.dll.
type GetSettingFn = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
type GetIdFn = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
type GetBoolFn = unsafe extern "system" fn(*mut c_void, *mut u8) -> HRESULT;
type GetValueFn = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;
type SetValueFn = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT;
type AddHandlerFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *mut EventRegistrationToken) -> HRESULT;
type RemoveHandlerFn = unsafe extern "system" fn(*mut c_void, EventRegistrationToken) -> HRESULT;
/// `GetQuickActionsProvider(IInspectable** out)` from QuickActionsDataModel.dll.
type GetProviderFn = unsafe extern "system" fn(*mut *mut c_void) -> HRESULT;
/// `IQuickActionsProvider::GetQuickActionByFriendlyName(HSTRING, IQuickAction**)`.
type GetActionByNameFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;

// ISettingItem vtable slots (IInspectable occupies 0..=5).
const SLOT_GET_ID: usize = 6;
const SLOT_IS_ENABLED: usize = 9;
const SLOT_IS_APPLICABLE: usize = 10;
const SLOT_GET_VALUE: usize = 13;
const SLOT_SET_VALUE: usize = 14;
const SLOT_ADD_HANDLER: usize = 18;
const SLOT_REMOVE_HANDLER: usize = 19;

// IQuickAction vtable slots (IInspectable occupies 0..=5). Confirmed against the public PDB
// symbols of QuickActionsDataModel.dll: get_Id=7, get_FriendlyName=8, get_IsApplicable=9,
// get_IsEnabled=10, get_QuickActionStatus=12, get_QuickActionIsActive=14,
// add/remove_QuickActionPropertyChanged=19/20, GetValue=21, SetValue=22.
const QA_SLOT_GET_ID: usize = 7;
const QA_SLOT_GET_FRIENDLY_NAME: usize = 8;
const QA_SLOT_IS_APPLICABLE: usize = 9;
const QA_SLOT_IS_ENABLED: usize = 10;
const QA_SLOT_STATUS: usize = 12;
const QA_SLOT_IS_ACTIVE: usize = 14;
const QA_SLOT_ADD_HANDLER: usize = 19;
const QA_SLOT_REMOVE_HANDLER: usize = 20;
const QA_SLOT_GET_VALUE: usize = 21;
const QA_SLOT_SET_VALUE: usize = 22;

const SETTING_ID: &str = "SystemSettings_Display_Brightness";
const QUICK_ACTION_NAME: &str = "Microsoft.QuickAction.Brightness";
const PIPE_FALLBACK_PERCENT: u32 = 100;

struct HandlerReg {
    sender: *mut c_void,
    handler: *mut c_void,
    token: i64,
}

// The pointers are opaque COM references owned by the event registration, not pointers into
// this module's memory. They are only dereferenced while the corresponding reference is held.
unsafe impl Send for HandlerReg {}

struct State {
    original_get_value: GetValueFn,
    original_set_value: SetValueFn,
    original_add_handler: AddHandlerFn,
    original_remove_handler: RemoveHandlerFn,
    handlers: Mutex<Vec<HandlerReg>>,
}

struct QuickState {
    original_get_id: GetIdFn,
    original_is_applicable: GetBoolFn,
    original_is_enabled: GetBoolFn,
    original_status: GetIdFn,
    original_is_active: GetBoolFn,
    original_get_value: GetValueFn,
    original_set_value: SetValueFn,
    original_add_handler: AddHandlerFn,
    original_remove_handler: RemoveHandlerFn,
    handlers: Mutex<Vec<HandlerReg>>,
}

static STATE: OnceLock<State> = OnceLock::new();
static QUICK_STATE: OnceLock<QuickState> = OnceLock::new();
/// QuickSetting instances that have been confirmed to be the brightness setting. The vtable is
/// shared by WiFi/Bluetooth/etc. as well, so every hook must forward those instances untouched.
static BRIGHTNESS_INSTANCES: OnceLock<Mutex<HashSet<usize>>> = OnceLock::new();
/// Last brightness in percent. GetValue must be cheap and non-blocking; the helper is queried
/// asynchronously and the result is published here.
static CACHED_PERCENT: AtomicU32 = AtomicU32::new(PIPE_FALLBACK_PERCENT);
/// Monotonic time of the last UI-originated SetValue. The watcher ignores DDC reads for a short
/// window afterwards so a slow monitor cannot pull the slider back to an in-flight old value.
static LAST_SET_MS: AtomicU64 = AtomicU64::new(0);
static WATCH_STARTED: AtomicBool = AtomicBool::new(false);
static SEEN_PROPERTIES: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();

fn monotonic_ms() -> u64 {
    static EPOCH: OnceLock<Instant> = OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// Appends a line to `%TEMP%\brighttray-hook.log`. Best effort only: logging must never be
/// able to take down ShellHost.
fn log(message: impl AsRef<str>) {
    use std::io::Write;

    let path = std::env::temp_dir().join("brighttray-hook.log");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let _ = writeln!(
        file,
        "[{secs} pid={}] {}",
        std::process::id(),
        message.as_ref()
    );
}

fn log_property(operation: &str, name: &str) {
    let seen = SEEN_PROPERTIES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = seen.lock().unwrap_or_else(|e| e.into_inner());
    if guard.insert(format!("{operation}:{name}")) {
        log(format!("{operation}(\"{name}\")"));
    }
}

unsafe fn hstring_to_string(raw: *mut c_void) -> Option<String> {
    if raw.is_null() {
        return None;
    }
    // `raw` is an HSTRING handle (a pointer to its header), already passed by value. Do not
    // dereference it as `*const HSTRING`; wrap the handle without taking ownership of it.
    let borrowed: std::mem::ManuallyDrop<HSTRING> = std::mem::transmute_copy(&raw);
    Some(borrowed.to_string_lossy())
}

/// Frees an HSTRING that an interface returned in an out-parameter. `get_Id` is implemented as
/// `WindowsDuplicateString`, so the caller owns the result.
unsafe fn free_hstring(raw: *mut c_void) {
    if raw.is_null() {
        return;
    }
    let borrowed: std::mem::ManuallyDrop<HSTRING> = std::mem::transmute_copy(&raw);
    let _ = windows::Win32::System::WinRT::WindowsDeleteString(&borrowed);
}

/// True only for the brightness QuickSetting instance. The QuickSetting vtable is shared by
/// every tile, so the other tiles must be allowed to run their real methods.
unsafe fn is_brightness_instance(this: *mut c_void) -> bool {
    if this.is_null() {
        return false;
    }
    let cache = BRIGHTNESS_INSTANCES.get_or_init(|| Mutex::new(HashSet::new()));
    if cache
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains(&(this as usize))
    {
        return true;
    }
    let Some(state) = QUICK_STATE.get() else {
        return false;
    };
    let mut id: *mut c_void = std::ptr::null_mut();
    let hr = (state.original_get_id)(this, &mut id);
    if hr.is_err() || id.is_null() {
        return false;
    }
    let id_text = hstring_to_string(id).unwrap_or_default();
    free_hstring(id);
    let matches = id_text.eq_ignore_ascii_case(SETTING_ID);
    if matches {
        cache
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(this as usize);
        log(format!(
            "identified brightness QuickSetting instance 0x{:X}",
            this as usize
        ));
    }
    matches
}

unsafe fn call_iunknown(object: *mut c_void, slot: usize) {
    if object.is_null() {
        return;
    }
    let vtable = *(object as *mut *mut usize);
    if vtable.is_null() {
        return;
    }
    let f: unsafe extern "system" fn(*mut c_void) -> u32 = std::mem::transmute(*vtable.add(slot));
    f(object);
}

unsafe fn add_ref(object: *mut c_void) {
    call_iunknown(object, 1);
}

unsafe fn release(object: *mut c_void) {
    call_iunknown(object, 2);
}

unsafe fn box_i32(value: i32, out: *mut *mut c_void) -> HRESULT {
    match PropertyValue::CreateInt32(value) {
        Ok(value) => {
            *out = value.into_raw();
            S_OK
        }
        Err(e) => e.code(),
    }
}

unsafe fn box_string(value: &str, out: *mut *mut c_void) -> HRESULT {
    let text = HSTRING::from(value);
    match PropertyValue::CreateString(&text) {
        Ok(value) => {
            *out = value.into_raw();
            S_OK
        }
        Err(e) => e.code(),
    }
}

unsafe fn box_bool(value: bool, out: *mut *mut c_void) -> HRESULT {
    match PropertyValue::CreateBoolean(value) {
        Ok(value) => {
            *out = value.into_raw();
            S_OK
        }
        Err(e) => e.code(),
    }
}

unsafe fn read_percent(value: *mut c_void) -> Option<i32> {
    if value.is_null() {
        return None;
    }
    let inspectable = IInspectable::from_raw_borrowed(&value)?;
    let property: IPropertyValue = inspectable.cast().ok()?;
    if let Ok(v) = property.GetInt32() {
        return Some(v);
    }
    if let Ok(v) = property.GetUInt32() {
        return Some(v as i32);
    }
    property.GetSingle().ok().map(|v| v.round() as i32)
}

unsafe extern "system" fn hooked_is_enabled(_this: *mut c_void, out: *mut u8) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = 1;
    S_OK
}

unsafe extern "system" fn hooked_is_applicable(_this: *mut c_void, out: *mut u8) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = 1;
    S_OK
}

unsafe fn get_value_impl(this: *mut c_void, name: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();

    let Some(state) = STATE.get() else {
        return E_UNEXPECTED;
    };
    let name_text = hstring_to_string(name);
    if let Some(name_text) = &name_text {
        log_property("GetValue", name_text);
    }

    match name_text.as_deref() {
        Some("Value") => box_i32(CACHED_PERCENT.load(Ordering::Relaxed) as i32, out),
        Some("MinValue") => box_i32(0, out),
        Some("MaxValue") => box_i32(100, out),
        Some("QuickActionStatus") => {
            let percent = CACHED_PERCENT.load(Ordering::Relaxed);
            box_string(&format!("{percent}%"), out)
        }
        // The tile is not a toggle, but the shell asks every quick action for this flag. A
        // desktop brightness setting that only exists because we injected it is "active".
        Some("QuickActionIsActive") => box_bool(true, out),
        // "Values" and anything added in a future build keep the original behaviour.
        _ => (state.original_get_value)(this, name, out),
    }
}

unsafe extern "system" fn hooked_get_value(
    this: *mut c_void,
    name: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        get_value_impl(this, name, out)
    })) {
        Ok(hr) => hr,
        Err(_) => {
            log("panic in GetValue");
            E_FAIL
        }
    }
}

unsafe fn set_value_impl(this: *mut c_void, name: *mut c_void, value: *mut c_void) -> HRESULT {
    let Some(state) = STATE.get() else {
        return E_UNEXPECTED;
    };
    let name_text = hstring_to_string(name);
    if let Some(name_text) = &name_text {
        log_property("SetValue", name_text);
    }

    if name_text.as_deref() == Some("Value") {
        if let Some(percent) = read_percent(value) {
            let percent = percent.clamp(0, 100) as u32;
            CACHED_PERCENT.store(percent, Ordering::Relaxed);
            LAST_SET_MS.store(monotonic_ms(), Ordering::Relaxed);
            pipe::set_all(percent);
            return S_OK;
        }
    }
    (state.original_set_value)(this, name, value)
}

unsafe extern "system" fn hooked_set_value(
    this: *mut c_void,
    name: *mut c_void,
    value: *mut c_void,
) -> HRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        set_value_impl(this, name, value)
    })) {
        Ok(hr) => hr,
        Err(_) => {
            log("panic in SetValue");
            E_FAIL
        }
    }
}

unsafe extern "system" fn hooked_add_handler(
    this: *mut c_void,
    handler: *mut c_void,
    token: *mut EventRegistrationToken,
) -> HRESULT {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(state) = STATE.get() else {
            return E_UNEXPECTED;
        };
        let hr = (state.original_add_handler)(this, handler, token);
        if hr.is_ok() && !this.is_null() && !handler.is_null() && !token.is_null() {
            add_ref(this);
            add_ref(handler);
            let token_value = (*token).Value;
            let mut handlers = state.handlers.lock().unwrap_or_else(|e| e.into_inner());
            handlers.push(HandlerReg {
                sender: this,
                handler,
                token: token_value,
            });
            log(format!(
                "add_SettingChanged token={token_value} handlers={}",
                handlers.len()
            ));
        }
        hr
    }));
    result.unwrap_or_else(|_| {
        log("panic in add_SettingChanged");
        E_FAIL
    })
}

unsafe extern "system" fn hooked_remove_handler(
    this: *mut c_void,
    token: EventRegistrationToken,
) -> HRESULT {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(state) = STATE.get() else {
            return E_UNEXPECTED;
        };
        let hr = (state.original_remove_handler)(this, token);
        let removed = {
            let mut handlers = state.handlers.lock().unwrap_or_else(|e| e.into_inner());
            let position = handlers
                .iter()
                .position(|h| h.sender == this && h.token == token.Value);
            position.map(|i| handlers.remove(i))
        };
        if let Some(handler) = removed {
            release(handler.sender);
            release(handler.handler);
            log(format!("remove_SettingChanged token={}", token.Value));
        }
        hr
    }));
    result.unwrap_or_else(|_| {
        log("panic in remove_SettingChanged");
        E_FAIL
    })
}

unsafe extern "system" fn quick_is_applicable(this: *mut c_void, out: *mut u8) -> HRESULT {
    let Some(state) = QUICK_STATE.get() else {
        return E_UNEXPECTED;
    };
    if !is_brightness_instance(this) {
        return (state.original_is_applicable)(this, out);
    }
    log_property("QuickIsApplicable", &format!("{this:p}"));
    if out.is_null() {
        return E_POINTER;
    }
    *out = 1;
    S_OK
}

unsafe extern "system" fn quick_is_enabled(this: *mut c_void, out: *mut u8) -> HRESULT {
    let Some(state) = QUICK_STATE.get() else {
        return E_UNEXPECTED;
    };
    if !is_brightness_instance(this) {
        return (state.original_is_enabled)(this, out);
    }
    log_property("QuickIsEnabled", &format!("{this:p}"));
    if out.is_null() {
        return E_POINTER;
    }
    *out = 1;
    S_OK
}

unsafe extern "system" fn quick_is_active(this: *mut c_void, out: *mut u8) -> HRESULT {
    let Some(state) = QUICK_STATE.get() else {
        return E_UNEXPECTED;
    };
    if !is_brightness_instance(this) {
        return (state.original_is_active)(this, out);
    }
    if out.is_null() {
        return E_POINTER;
    }
    *out = 1;
    S_OK
}

unsafe extern "system" fn quick_status(this: *mut c_void, out: *mut *mut c_void) -> HRESULT {
    let Some(state) = QUICK_STATE.get() else {
        return E_UNEXPECTED;
    };
    if !is_brightness_instance(this) {
        return (state.original_status)(this, out);
    }
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();
    let percent = CACHED_PERCENT.load(Ordering::Relaxed);
    box_string(&format!("{percent}%"), out)
}

unsafe fn quick_get_value_impl(
    this: *mut c_void,
    name: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    *out = std::ptr::null_mut();

    let Some(state) = QUICK_STATE.get() else {
        return E_UNEXPECTED;
    };
    if !is_brightness_instance(this) {
        return (state.original_get_value)(this, name, out);
    }
    let name_text = hstring_to_string(name);
    if let Some(name_text) = &name_text {
        log_property("QuickGetValue", name_text);
    }

    match name_text.as_deref() {
        Some("Value") => box_i32(CACHED_PERCENT.load(Ordering::Relaxed) as i32, out),
        Some("MinValue") => box_i32(0, out),
        Some("MaxValue") => box_i32(100, out),
        Some("QuickActionStatus") => {
            let percent = CACHED_PERCENT.load(Ordering::Relaxed);
            box_string(&format!("{percent}%"), out)
        }
        Some("QuickActionIsActive") => box_bool(true, out),
        _ => (state.original_get_value)(this, name, out),
    }
}

unsafe extern "system" fn quick_get_value(
    this: *mut c_void,
    name: *mut c_void,
    out: *mut *mut c_void,
) -> HRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        quick_get_value_impl(this, name, out)
    })) {
        Ok(hr) => hr,
        Err(_) => {
            log("panic in QuickAction GetValue");
            E_FAIL
        }
    }
}

unsafe fn quick_set_value_impl(
    this: *mut c_void,
    name: *mut c_void,
    value: *mut c_void,
) -> HRESULT {
    let Some(state) = QUICK_STATE.get() else {
        return E_UNEXPECTED;
    };
    if !is_brightness_instance(this) {
        return (state.original_set_value)(this, name, value);
    }
    let name_text = hstring_to_string(name);
    if let Some(name_text) = &name_text {
        log_property("QuickSetValue", name_text);
    }

    if name_text.as_deref() == Some("Value") {
        if let Some(percent) = read_percent(value) {
            let percent = percent.clamp(0, 100) as u32;
            CACHED_PERCENT.store(percent, Ordering::Relaxed);
            LAST_SET_MS.store(monotonic_ms(), Ordering::Relaxed);
            pipe::set_all(percent);
            return S_OK;
        }
    }
    (state.original_set_value)(this, name, value)
}

unsafe extern "system" fn quick_set_value(
    this: *mut c_void,
    name: *mut c_void,
    value: *mut c_void,
) -> HRESULT {
    match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        quick_set_value_impl(this, name, value)
    })) {
        Ok(hr) => hr,
        Err(_) => {
            log("panic in QuickAction SetValue");
            E_FAIL
        }
    }
}

unsafe extern "system" fn quick_add_handler(
    this: *mut c_void,
    handler: *mut c_void,
    token: *mut EventRegistrationToken,
) -> HRESULT {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(state) = QUICK_STATE.get() else {
            return E_UNEXPECTED;
        };
        let hr = (state.original_add_handler)(this, handler, token);
        if hr.is_ok()
            && !this.is_null()
            && !handler.is_null()
            && !token.is_null()
            && is_brightness_instance(this)
        {
            add_ref(this);
            add_ref(handler);
            let token_value = (*token).Value;
            let mut handlers = state.handlers.lock().unwrap_or_else(|e| e.into_inner());
            handlers.push(HandlerReg {
                sender: this,
                handler,
                token: token_value,
            });
            log(format!(
                "add_QuickActionPropertyChanged token={token_value} handlers={}",
                handlers.len()
            ));
        }
        hr
    }));
    result.unwrap_or_else(|_| {
        log("panic in add_QuickActionPropertyChanged");
        E_FAIL
    })
}

unsafe extern "system" fn quick_remove_handler(
    this: *mut c_void,
    token: EventRegistrationToken,
) -> HRESULT {
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let Some(state) = QUICK_STATE.get() else {
            return E_UNEXPECTED;
        };
        let hr = (state.original_remove_handler)(this, token);
        let removed = {
            let mut handlers = state.handlers.lock().unwrap_or_else(|e| e.into_inner());
            let position = handlers
                .iter()
                .position(|h| h.sender == this && h.token == token.Value);
            position.map(|i| handlers.remove(i))
        };
        if let Some(handler) = removed {
            release(handler.sender);
            release(handler.handler);
            log(format!(
                "remove_QuickActionPropertyChanged token={}",
                token.Value
            ));
        }
        hr
    }));
    result.unwrap_or_else(|_| {
        log("panic in remove_QuickActionPropertyChanged");
        E_FAIL
    })
}

unsafe fn invoke_handler(sender: *mut c_void, handler: *mut c_void) {
    let vtable = *(handler as *mut *mut usize);
    if vtable.is_null() {
        return;
    }
    // ITypedEventHandler<IInspectable*, HSTRING>: IUnknown (0..=2), Invoke at slot 3.
    let invoke: unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> HRESULT =
        std::mem::transmute(*vtable.add(3));
    let args = HSTRING::from("Value");
    let raw_args: *mut c_void = std::mem::transmute_copy(&args);
    let _ = invoke(handler, sender, raw_args);
}

fn collect_handler_targets(handlers: &Mutex<Vec<HandlerReg>>) -> Vec<(*mut c_void, *mut c_void)> {
    let handlers = handlers.lock().unwrap_or_else(|e| e.into_inner());
    handlers
        .iter()
        .map(|h| {
            unsafe {
                add_ref(h.sender);
                add_ref(h.handler);
            }
            (h.sender, h.handler)
        })
        .collect()
}

/// Tells every registered UI handler that `Value` changed. Handlers are AddRef'd while copied
/// out of the lock so a concurrent remove cannot free them under us.
fn notify_handlers() {
    let mut targets = Vec::new();
    if let Some(state) = STATE.get() {
        targets.extend(collect_handler_targets(&state.handlers));
    }
    if let Some(state) = QUICK_STATE.get() {
        targets.extend(collect_handler_targets(&state.handlers));
    }
    for (sender, handler) in targets {
        unsafe {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                invoke_handler(sender, handler)
            }));
            release(sender);
            release(handler);
        }
    }
}

fn has_handlers() -> bool {
    let setting_handlers = STATE.get().is_some_and(|state| {
        !state
            .handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    });
    let quick_handlers = QUICK_STATE.get().is_some_and(|state| {
        !state
            .handlers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_empty()
    });
    setting_handlers || quick_handlers
}

fn start_watcher() {
    if WATCH_STARTED.swap(true, Ordering::SeqCst) {
        return;
    }
    std::thread::spawn(|| loop {
        std::thread::sleep(std::time::Duration::from_millis(1500));
        if !has_handlers() {
            continue;
        }
        // A SetValue was just sent to the monitor. DDC reads take tens of milliseconds and can
        // still return the previous value while the write is in flight; ignoring them for a
        // moment keeps the slider under the user's finger instead of fighting it.
        if monotonic_ms().saturating_sub(LAST_SET_MS.load(Ordering::Relaxed)) < 1200 {
            continue;
        }
        if let Some(percent) = pipe::get_all() {
            let previous = CACHED_PERCENT.swap(percent, Ordering::Relaxed);
            if previous != percent {
                log(format!(
                    "external brightness change {previous}% -> {percent}%"
                ));
                notify_handlers();
            }
        }
    });
}

unsafe fn module_range(module: HMODULE) -> Result<(usize, usize), String> {
    let mut info = MODULEINFO::default();
    GetModuleInformation(
        GetCurrentProcess(),
        module,
        &mut info,
        std::mem::size_of::<MODULEINFO>() as u32,
    )
    .map_err(|e| format!("GetModuleInformation: {e}"))?;
    Ok((info.lpBaseOfDll as usize, info.SizeOfImage as usize))
}

unsafe fn pin_self() {
    let mut module = HMODULE::default();
    let _ = GetModuleHandleExW(
        GET_MODULE_HANDLE_EX_FLAG_PIN | GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        PCWSTR(pin_self as *const u16),
        &mut module,
    );
}

/// Re-protects every page the selected vtable slots span, writes the hook pointers, then
/// restores the original protection page by page.
unsafe fn patch_slots(vtable: *mut usize, slots: &[(usize, usize)]) -> Result<(), String> {
    const PAGE_SIZE: usize = 0x1000;

    if slots.is_empty() {
        return Ok(());
    }
    let first = slots.iter().map(|(slot, _)| *slot).min().unwrap_or(0);
    let last = slots.iter().map(|(slot, _)| *slot).max().unwrap_or(0);
    let start = vtable as usize + first * std::mem::size_of::<usize>();
    let end = vtable as usize + (last + 1) * std::mem::size_of::<usize>();
    let mut saved: Vec<(usize, usize, PAGE_PROTECTION_FLAGS)> = Vec::new();

    let mut address = start & !(PAGE_SIZE - 1);
    while address < end {
        let length = PAGE_SIZE.min(end - address);
        let mut old = PAGE_PROTECTION_FLAGS(0);
        if let Err(e) = VirtualProtect(address as *const c_void, length, PAGE_READWRITE, &mut old) {
            for (a, l, o) in saved.iter().rev() {
                let mut ignored = PAGE_PROTECTION_FLAGS(0);
                let _ = VirtualProtect(*a as *const c_void, *l, *o, &mut ignored);
            }
            return Err(format!("VirtualProtect(RW) at 0x{address:X}: {e}"));
        }
        saved.push((address, length, old));
        address += PAGE_SIZE;
    }

    for (slot, function) in slots {
        std::ptr::write_volatile(vtable.add(*slot), *function);
    }

    for (a, l, o) in saved.iter().rev() {
        let mut ignored = PAGE_PROTECTION_FLAGS(0);
        let _ = VirtualProtect(*a as *const c_void, *l, *o, &mut ignored);
    }
    Ok(())
}

unsafe fn patch_vtable(vtable: *mut usize) -> Result<(), String> {
    patch_slots(
        vtable,
        &[
            (SLOT_IS_ENABLED, hooked_is_enabled as *const () as usize),
            (
                SLOT_IS_APPLICABLE,
                hooked_is_applicable as *const () as usize,
            ),
            (SLOT_GET_VALUE, hooked_get_value as *const () as usize),
            (SLOT_SET_VALUE, hooked_set_value as *const () as usize),
            (SLOT_ADD_HANDLER, hooked_add_handler as *const () as usize),
            (
                SLOT_REMOVE_HANDLER,
                hooked_remove_handler as *const () as usize,
            ),
        ],
    )
}

fn in_module(pointer: usize, base: usize, size: usize) -> bool {
    pointer >= base && pointer < base + size
}

/// Gets one `QuickSetting` instance from QuickActionsDataModel, verifies its identity, and
/// patches the shared IQuickAction vtable. ControlCenter binds to this object, so this is the
/// hook that actually makes the brightness slider appear.
unsafe fn try_install_quick_actions() -> Result<(), String> {
    let module = match GetModuleHandleW(w!("QuickActionsDataModel.dll")) {
        Ok(module) if !module.0.is_null() => module,
        _ => LoadLibraryW(w!("QuickActionsDataModel.dll"))
            .map_err(|e| format!("LoadLibrary(QuickActionsDataModel): {e}"))?,
    };
    let (base, size) = module_range(module)?;

    let procedure = GetProcAddress(module, windows::core::s!("GetQuickActionsProvider"))
        .ok_or_else(|| "GetQuickActionsProvider export not found".to_string())?;
    let get_provider: GetProviderFn = std::mem::transmute(procedure);
    let mut provider: *mut c_void = std::ptr::null_mut();
    let hr = get_provider(&mut provider);
    if hr.is_err() || provider.is_null() {
        return Err(format!("GetQuickActionsProvider -> 0x{:08X}", hr.0 as u32));
    }

    let provider_vtable = *(provider as *mut *mut usize);
    if !in_module(provider_vtable as usize, base, size) {
        return Err(format!(
            "provider vtable 0x{:X} is outside QuickActionsDataModel",
            provider_vtable as usize
        ));
    }
    // slot 13 = GetQuickActionByFriendlyName
    let get_by_name: GetActionByNameFn = std::mem::transmute(*provider_vtable.add(13));
    if !in_module(get_by_name as usize, base, size) {
        return Err("GetQuickActionByFriendlyName slot is outside the module".to_string());
    }

    let friendly = HSTRING::from(QUICK_ACTION_NAME);
    let raw_friendly: *mut c_void = std::mem::transmute_copy(&friendly);
    let mut action: *mut c_void = std::ptr::null_mut();
    let hr = get_by_name(provider, raw_friendly, &mut action);
    if hr.is_err() || action.is_null() {
        return Err(format!(
            "GetQuickActionByFriendlyName({QUICK_ACTION_NAME}) -> 0x{:08X}",
            hr.0 as u32
        ));
    }

    let vtable = *(action as *mut *mut usize);
    if !in_module(vtable as usize, base, size) {
        return Err(format!(
            "quick action vtable 0x{:X} is outside QuickActionsDataModel",
            vtable as usize
        ));
    }
    for slot in [
        QA_SLOT_GET_ID,
        QA_SLOT_GET_FRIENDLY_NAME,
        QA_SLOT_IS_APPLICABLE,
        QA_SLOT_IS_ENABLED,
        QA_SLOT_STATUS,
        QA_SLOT_IS_ACTIVE,
        QA_SLOT_ADD_HANDLER,
        QA_SLOT_REMOVE_HANDLER,
        QA_SLOT_GET_VALUE,
        QA_SLOT_SET_VALUE,
    ] {
        let pointer = *vtable.add(slot);
        if !in_module(pointer, base, size) {
            return Err(format!(
                "quick action slot {slot} = 0x{pointer:X}, outside module 0x{base:X}..0x{:X}",
                base + size
            ));
        }
    }

    let get_id: GetIdFn = std::mem::transmute(*vtable.add(QA_SLOT_GET_ID));
    let mut returned_id: *mut c_void = std::ptr::null_mut();
    let hr = get_id(action, &mut returned_id);
    if hr.is_err() || returned_id.is_null() {
        return Err(format!("quick action get_Id -> 0x{:08X}", hr.0 as u32));
    }
    let returned_id = hstring_to_string(returned_id).unwrap_or_default();
    if !returned_id.eq_ignore_ascii_case(SETTING_ID) {
        return Err(format!("unexpected quick action id {returned_id:?}"));
    }

    let get_friendly: GetIdFn = std::mem::transmute(*vtable.add(QA_SLOT_GET_FRIENDLY_NAME));
    let mut returned_friendly: *mut c_void = std::ptr::null_mut();
    let hr = get_friendly(action, &mut returned_friendly);
    if hr.is_ok() && !returned_friendly.is_null() {
        let returned_friendly = hstring_to_string(returned_friendly).unwrap_or_default();
        if !returned_friendly.eq_ignore_ascii_case(QUICK_ACTION_NAME) {
            return Err(format!(
                "unexpected quick action friendly name {returned_friendly:?}"
            ));
        }
    }

    if QUICK_STATE.get().is_none() {
        let state = QuickState {
            original_get_id: std::mem::transmute(*vtable.add(QA_SLOT_GET_ID)),
            original_is_applicable: std::mem::transmute(*vtable.add(QA_SLOT_IS_APPLICABLE)),
            original_is_enabled: std::mem::transmute(*vtable.add(QA_SLOT_IS_ENABLED)),
            original_status: std::mem::transmute(*vtable.add(QA_SLOT_STATUS)),
            original_is_active: std::mem::transmute(*vtable.add(QA_SLOT_IS_ACTIVE)),
            original_get_value: std::mem::transmute(*vtable.add(QA_SLOT_GET_VALUE)),
            original_set_value: std::mem::transmute(*vtable.add(QA_SLOT_SET_VALUE)),
            original_add_handler: std::mem::transmute(*vtable.add(QA_SLOT_ADD_HANDLER)),
            original_remove_handler: std::mem::transmute(*vtable.add(QA_SLOT_REMOVE_HANDLER)),
            handlers: Mutex::new(Vec::new()),
        };
        let _ = QUICK_STATE.set(state);
    }

    patch_slots(
        vtable,
        &[
            (
                QA_SLOT_IS_APPLICABLE,
                quick_is_applicable as *const () as usize,
            ),
            (QA_SLOT_IS_ENABLED, quick_is_enabled as *const () as usize),
            (QA_SLOT_STATUS, quick_status as *const () as usize),
            (QA_SLOT_IS_ACTIVE, quick_is_active as *const () as usize),
            (QA_SLOT_ADD_HANDLER, quick_add_handler as *const () as usize),
            (
                QA_SLOT_REMOVE_HANDLER,
                quick_remove_handler as *const () as usize,
            ),
            (QA_SLOT_GET_VALUE, quick_get_value as *const () as usize),
            (QA_SLOT_SET_VALUE, quick_set_value as *const () as usize),
        ],
    )?;
    log(format!(
        "IQuickAction hook installed: vtable=0x{:X} action=0x{:X}",
        vtable as usize, action as usize
    ));
    Ok(())
}

unsafe fn try_install() -> Result<(), String> {
    let module = match GetModuleHandleW(w!("SettingsHandlers_PCDisplay.dll")) {
        Ok(module) if !module.0.is_null() => module,
        _ => LoadLibraryW(w!("SettingsHandlers_PCDisplay.dll"))
            .map_err(|e| format!("LoadLibrary(SettingsHandlers_PCDisplay): {e}"))?,
    };

    let procedure = GetProcAddress(module, windows::core::s!("GetSetting"))
        .ok_or_else(|| "GetSetting export not found".to_string())?;
    let get_setting: GetSettingFn = std::mem::transmute(procedure);

    let id = HSTRING::from(SETTING_ID);
    let raw_id: *mut c_void = std::mem::transmute_copy(&id);
    let mut instance: *mut c_void = std::ptr::null_mut();
    let hr = get_setting(raw_id, &mut instance);
    if hr.is_err() || instance.is_null() {
        return Err(format!("GetSetting -> 0x{:08X}", hr.0 as u32));
    }

    let vtable = *(instance as *mut *mut usize);
    if vtable.is_null() {
        return Err("GetSetting returned an object without a vtable".to_string());
    }
    let (base, size) = module_range(module)?;
    for slot in [
        SLOT_GET_ID,
        SLOT_IS_ENABLED,
        SLOT_IS_APPLICABLE,
        SLOT_GET_VALUE,
        SLOT_SET_VALUE,
        SLOT_ADD_HANDLER,
        SLOT_REMOVE_HANDLER,
    ] {
        let pointer = *vtable.add(slot);
        if pointer < base || pointer >= base + size {
            return Err(format!(
                "vtable slot {slot} = 0x{pointer:X}, outside module 0x{base:X}..0x{:X}",
                base + size
            ));
        }
    }

    // Refuse to patch a vtable that does not identify itself as the brightness setting.
    let get_id: GetIdFn = std::mem::transmute(*vtable.add(SLOT_GET_ID));
    let mut returned_id: *mut c_void = std::ptr::null_mut();
    let hr = get_id(instance, &mut returned_id);
    if hr.is_err() || returned_id.is_null() {
        return Err(format!("get_Id -> 0x{:08X}", hr.0 as u32));
    }
    // Deliberately not freed: this runs once and the HSTRING's ownership model is the one
    // thing we cannot verify from the ABI alone.
    let returned_id_text = hstring_to_string(returned_id).unwrap_or_default();
    if !returned_id_text.eq_ignore_ascii_case(SETTING_ID) {
        return Err(format!("unexpected setting id {returned_id_text:?}"));
    }

    let initial_percent = pipe::get_all().unwrap_or(PIPE_FALLBACK_PERCENT);
    CACHED_PERCENT.store(initial_percent, Ordering::Relaxed);
    log(format!(
        "validated {SETTING_ID}: module=0x{base:X} vtable=0x{:X} initial={initial_percent}%",
        vtable as usize
    ));

    if STATE.get().is_none() {
        let state = State {
            original_get_value: std::mem::transmute(*vtable.add(SLOT_GET_VALUE)),
            original_set_value: std::mem::transmute(*vtable.add(SLOT_SET_VALUE)),
            original_add_handler: std::mem::transmute(*vtable.add(SLOT_ADD_HANDLER)),
            original_remove_handler: std::mem::transmute(*vtable.add(SLOT_REMOVE_HANDLER)),
            handlers: Mutex::new(Vec::new()),
        };
        let _ = STATE.set(state);
    }

    pin_self();
    patch_vtable(vtable)?;
    log("ISettingItem hook installed: IsEnabled/IsApplicable/GetValue/SetValue/SettingChanged");
    match try_install_quick_actions() {
        Ok(()) => {}
        Err(e) => log(format!("QuickActions hook not installed: {e}")),
    }
    start_watcher();
    signal_ready();
    Ok(())
}

/// Wakes the early injector. It starts ShellHost suspended and does not resume it until this
/// event is set, so the vtable is patched before the shell can cache the "no brightness"
/// capability. Late injection simply has no event to open.
unsafe fn signal_ready() {
    if let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, FALSE, w!(r"Local\BrightTrayHookReady")) {
        let _ = SetEvent(event);
        let _ = CloseHandle(event);
    }
}

fn install() {
    unsafe {
        let init = RoInitialize(RO_INIT_MULTITHREADED);
        if let Err(e) = &init {
            log(format!("RoInitialize(MTA) -> {e} (continuing)"));
        }
        for attempt in 1..=30 {
            match try_install() {
                Ok(()) => return,
                Err(e) => {
                    if attempt == 1 || attempt % 5 == 0 {
                        log(format!("install attempt {attempt}/30 failed: {e}"));
                    }
                    std::thread::sleep(std::time::Duration::from_secs(1));
                }
            }
        }
        log("giving up after 30 install attempts");
        signal_ready();
    }
}

#[no_mangle]
#[allow(non_snake_case)]
pub extern "system" fn DllMain(
    _instance: HINSTANCE,
    reason: u32,
    _reserved: *mut c_void,
) -> windows::Win32::Foundation::BOOL {
    if reason == DLL_PROCESS_ATTACH {
        // DllMain must stay tiny: do all resolution and patching on a worker thread after the
        // loader lock has been released.
        std::thread::spawn(|| {
            std::thread::sleep(std::time::Duration::from_millis(300));
            log("brighttray_hook attached");
            install();
        });
    }
    TRUE
}
