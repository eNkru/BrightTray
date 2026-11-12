//! Safe, read-only validation of the `DisplayBrightness` assumptions.
//!
//! Loads SettingsHandlers_PCDisplay.dll in this process, calls its exported `GetSetting`, and
//! prints what the setting object answers. It never patches anything, so it can be run before
//! trying the injected hook.

use std::ffi::c_void;

use windows::core::{s, w, IInspectable, Interface, HRESULT, HSTRING};
use windows::Foundation::{IPropertyValue, PropertyType};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::ProcessStatus::{GetModuleInformation, MODULEINFO};
use windows::Win32::System::Threading::GetCurrentProcess;
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

type GetSettingFn = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
type GetBoolFn = unsafe extern "system" fn(*mut c_void, *mut u8) -> HRESULT;
type GetIdFn = unsafe extern "system" fn(*mut c_void, *mut *mut c_void) -> HRESULT;
type GetValueFn = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut *mut c_void) -> HRESULT;

const SETTING_ID: &str = "SystemSettings_Display_Brightness";
const SLOT_GET_ID: usize = 6;
const SLOT_IS_ENABLED: usize = 9;
const SLOT_IS_APPLICABLE: usize = 10;
const SLOT_GET_VALUE: usize = 13;

unsafe fn hstring(raw: *mut c_void) -> String {
    if raw.is_null() {
        return "<null>".to_string();
    }
    // `raw` is the HSTRING handle itself, not the address of a handle variable.
    let borrowed: std::mem::ManuallyDrop<HSTRING> = std::mem::transmute_copy(&raw);
    borrowed.to_string_lossy()
}

unsafe fn module_range(module: windows::Win32::Foundation::HMODULE) -> (usize, usize) {
    let mut info = MODULEINFO::default();
    let _ = GetModuleInformation(
        GetCurrentProcess(),
        module,
        &mut info,
        std::mem::size_of::<MODULEINFO>() as u32,
    );
    (info.lpBaseOfDll as usize, info.SizeOfImage as usize)
}

pub fn probe() -> Result<(), String> {
    unsafe {
        let _ = RoInitialize(RO_INIT_MULTITHREADED);
        let module = match GetModuleHandleW(w!("SettingsHandlers_PCDisplay.dll")) {
            Ok(module) => module,
            Err(_) => LoadLibraryW(w!("SettingsHandlers_PCDisplay.dll"))
                .map_err(|e| format!("LoadLibrary(SettingsHandlers_PCDisplay): {e}"))?,
        };
        let (base, size) = module_range(module);
        println!("SettingsHandlers_PCDisplay: base=0x{base:X} size=0x{size:X}");

        let procedure = GetProcAddress(module, s!("GetSetting"))
            .ok_or_else(|| "GetSetting export not found".to_string())?;
        let get_setting: GetSettingFn = std::mem::transmute(procedure);

        let id = HSTRING::from(SETTING_ID);
        let raw_id: *mut c_void = std::mem::transmute_copy(&id);
        let mut instance: *mut c_void = std::ptr::null_mut();
        let hr = get_setting(raw_id, &mut instance);
        println!("GetSetting -> 0x{:08X}, instance={instance:p}", hr.0 as u32);
        if hr.is_err() || instance.is_null() {
            return Err("GetSetting failed".to_string());
        }

        let vtable = *(instance as *mut *mut usize);
        println!("vtable = 0x{:X}", vtable as usize);
        let slots = [
            ("get_Id", SLOT_GET_ID),
            ("get_IsEnabled", SLOT_IS_ENABLED),
            ("get_IsApplicable", SLOT_IS_APPLICABLE),
            ("GetValue", SLOT_GET_VALUE),
        ];
        for (name, slot) in slots {
            let pointer = *vtable.add(slot);
            let inside = pointer >= base && pointer < base + size;
            println!(
                "  slot {slot:2} {name:18} = 0x{pointer:X} ({})",
                if inside {
                    "in module"
                } else {
                    "OUTSIDE MODULE"
                }
            );
        }

        for (name, slot) in [
            ("get_IsEnabled", SLOT_IS_ENABLED),
            ("get_IsApplicable", SLOT_IS_APPLICABLE),
        ] {
            let get_bool: GetBoolFn = std::mem::transmute(*vtable.add(slot));
            let mut value = 0u8;
            let hr = get_bool(instance, &mut value);
            println!("{name} -> 0x{:08X}, {value}", hr.0 as u32);
        }

        let get_id: GetIdFn = std::mem::transmute(*vtable.add(SLOT_GET_ID));
        let mut returned_id: *mut c_void = std::ptr::null_mut();
        let hr = get_id(instance, &mut returned_id);
        if hr.is_ok() && !returned_id.is_null() {
            println!(
                "get_Id -> 0x{:08X}, {:?}",
                hr.0 as u32,
                hstring(returned_id)
            );
        } else {
            println!("get_Id -> 0x{:08X}, <null>", hr.0 as u32);
        }

        let get_value: GetValueFn = std::mem::transmute(*vtable.add(SLOT_GET_VALUE));
        for property in [
            "Value",
            "MinValue",
            "MaxValue",
            "QuickActionStatus",
            "QuickActionIsActive",
            "Values",
        ] {
            let name = HSTRING::from(property);
            let raw_name: *mut c_void = std::mem::transmute_copy(&name);
            let mut value: *mut c_void = std::ptr::null_mut();
            let hr = get_value(instance, raw_name, &mut value);
            let mut detail = String::new();
            if hr.is_ok() && !value.is_null() {
                let inspectable = IInspectable::from_raw(value);
                if let Ok(property_value) = inspectable.cast::<IPropertyValue>() {
                    let kind: PropertyType = property_value.Type().unwrap_or(PropertyType(0));
                    detail.push_str(&format!("type={} ", kind.0));
                    match kind.0 {
                        1 => detail.push_str(&format!("{:?}", property_value.GetUInt8())),
                        2 => detail.push_str(&format!("{:?}", property_value.GetInt16())),
                        3 => detail.push_str(&format!("{:?}", property_value.GetUInt16())),
                        4 => detail.push_str(&format!("{:?}", property_value.GetInt32())),
                        5 => detail.push_str(&format!("{:?}", property_value.GetUInt32())),
                        6 => detail.push_str(&format!("{:?}", property_value.GetInt64())),
                        7 => detail.push_str(&format!("{:?}", property_value.GetUInt64())),
                        8 => detail.push_str(&format!("{:?}", property_value.GetSingle())),
                        9 => detail.push_str(&format!("{:?}", property_value.GetDouble())),
                        10 => detail.push_str(&format!("{:?}", property_value.GetChar16())),
                        11 => detail.push_str(&format!("{:?}", property_value.GetBoolean())),
                        12 => detail.push_str(&format!(
                            "{:?}",
                            property_value.GetString().map(|s| s.to_string_lossy())
                        )),
                        other => detail.push_str(&format!("<type {other}>")),
                    }
                }
                // `inspectable` owns the returned reference and drops it here.
            }
            println!("GetValue({property:?}) -> 0x{:08X} {detail}", hr.0 as u32);
        }

        // Second half: inspect the QuickActionsDataModel wrapper that actually feeds the
        // Quick Settings slider. This is the object ControlCenter binds to.
        match LoadLibraryW(w!("QuickActionsDataModel.dll")) {
            Ok(qa_module) => {
                println!(
                    "QuickActionsDataModel loaded at 0x{:X}",
                    qa_module.0 as usize
                );
                if let Some(proc) = GetProcAddress(qa_module, s!("GetQuickActionsProvider")) {
                    let get_provider: unsafe extern "system" fn(*mut *mut c_void) -> HRESULT =
                        std::mem::transmute(proc);
                    let mut provider: *mut c_void = std::ptr::null_mut();
                    let hr = get_provider(&mut provider);
                    println!(
                        "GetQuickActionsProvider -> 0x{:08X}, provider={provider:p}",
                        hr.0 as u32
                    );
                    if hr.is_ok() && !provider.is_null() {
                        let provider_vtable = *(provider as *mut *mut usize);
                        println!("provider vtable = 0x{:X}", provider_vtable as usize);
                        let get_by_name: unsafe extern "system" fn(
                            *mut c_void,
                            *mut c_void,
                            *mut *mut c_void,
                        )
                            -> HRESULT = std::mem::transmute(*provider_vtable.add(13));
                        let friendly = HSTRING::from("Microsoft.QuickAction.Brightness");
                        let raw_friendly: *mut c_void = std::mem::transmute_copy(&friendly);
                        let mut action: *mut c_void = std::ptr::null_mut();
                        let hr = get_by_name(provider, raw_friendly, &mut action);
                        println!(
                            "GetQuickActionByFriendlyName(Brightness) -> 0x{:08X}, action={action:p}",
                            hr.0 as u32
                        );
                        if hr.is_ok() && !action.is_null() {
                            let action_vtable = *(action as *mut *mut usize);
                            println!("Brightness action vtable = 0x{:X}", action_vtable as usize);
                            let get_applicable: unsafe extern "system" fn(
                                *mut c_void,
                                *mut u8,
                            )
                                -> HRESULT = std::mem::transmute(*action_vtable.add(9));
                            let get_enabled: unsafe extern "system" fn(
                                *mut c_void,
                                *mut u8,
                            )
                                -> HRESULT = std::mem::transmute(*action_vtable.add(10));
                            let mut value = 0u8;
                            let hr = get_applicable(action, &mut value);
                            println!(
                                "  QuickAction::get_IsApplicable -> 0x{:08X}, {value}",
                                hr.0 as u32
                            );
                            value = 0;
                            let hr = get_enabled(action, &mut value);
                            println!(
                                "  QuickAction::get_IsEnabled -> 0x{:08X}, {value}",
                                hr.0 as u32
                            );
                        }
                    }
                } else {
                    println!("GetQuickActionsProvider export not found");
                }
            }
            Err(e) => println!("LoadLibrary(QuickActionsDataModel): {e}"),
        }

        // SettingsHandlers_PCDisplay.dll hangs during DLL_PROCESS_DETACH in a console process,
        // so don't let the CRT run that teardown. This is a read-only diagnostic.
        use windows::Win32::System::Threading::{GetCurrentProcess, TerminateProcess};
        let _ = TerminateProcess(GetCurrentProcess(), 0);
        Ok(())
    }
}
