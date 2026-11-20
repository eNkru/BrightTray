//! Injects `brighttray_hook.dll` into `ShellHost.exe` with `CreateRemoteThread(LoadLibraryW)`.
//!
//! ShellHost runs at medium integrity as the current user and does not block dynamic code or
//! remote image loads (see docs/quick-settings-hook-research.md), so no elevation is needed.
//!
//! This module never ends a ShellHost process automatically. Ending the shell is visible to the
//! user — the Start menu and Quick Settings disappear until Windows starts it again — so the
//! only way to do it is [`restart_shellhost`], which exists for an explicit user request, and
//! [`inject_early`], which the CLI gates behind `--force-restart`.

use std::ffi::c_void;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use windows::core::{s, w, PCWSTR, PWSTR};
use windows::Win32::Foundation::FILETIME;
use windows::Win32::Foundation::{CloseHandle, FALSE, HANDLE, TRUE, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, Process32FirstW, Process32NextW,
    MODULEENTRY32W, PROCESSENTRY32W, TH32CS_SNAPMODULE, TH32CS_SNAPMODULE32, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::SystemInformation::GetSystemDirectoryW;
use windows::Win32::System::Threading::{
    CreateEventW, CreateProcessW, CreateRemoteThread, GetExitCodeProcess, GetExitCodeThread,
    GetProcessTimes, OpenProcess, ResetEvent, ResumeThread, TerminateProcess, WaitForSingleObject,
    CREATE_SUSPENDED, PROCESS_CREATE_THREAD, PROCESS_INFORMATION, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_TERMINATE, PROCESS_VM_OPERATION, PROCESS_VM_READ,
    PROCESS_VM_WRITE, STARTUPINFOW,
};

pub const SHELLHOST: &str = "ShellHost.exe";
pub const HOOK_DLL: &str = "brighttray_hook.dll";

/// `brighttray_hook.dll` is built next to `brighttray-helper.exe` by the workspace build.
pub fn default_dll_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe
        .parent()
        .ok_or_else(|| "helper executable has no parent directory".to_string())?;
    Ok(dir.join(HOOK_DLL))
}

/// The directory the loader considers the system directory, asked of Windows itself.
///
/// The system drive is not necessarily `C:`, so a literal `C:\Windows\System32` fails on a
/// Windows installed elsewhere. The returned length excludes the terminating NUL; a value larger
/// than the buffer is the required size, not a path.
fn system_directory() -> Result<PathBuf, String> {
    // A system directory path is far shorter than this; the check below keeps a longer one from
    // being silently truncated.
    let mut buffer = [0u16; 260];
    let length = unsafe { GetSystemDirectoryW(Some(&mut buffer)) } as usize;
    if length == 0 {
        return Err(format!(
            "GetSystemDirectoryW: {}",
            std::io::Error::last_os_error()
        ));
    }
    if length > buffer.len() {
        return Err(format!(
            "GetSystemDirectoryW: the system directory needs {length} UTF-16 units, buffer has {}",
            buffer.len()
        ));
    }
    Ok(PathBuf::from(String::from_utf16_lossy(&buffer[..length])))
}

fn wide_to_string(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}

/// PIDs of every running ShellHost.exe.
pub fn shellhost_pids() -> Vec<u32> {
    let mut pids = Vec::new();
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return pids;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if wide_to_string(&entry.szExeFile).eq_ignore_ascii_case(SHELLHOST) {
                    pids.push(entry.th32ProcessID);
                }
                if Process32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
    }
    pids
}

/// True when `pid` already has a module with this file name loaded.
pub fn module_loaded(pid: u32, name: &str) -> bool {
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE | TH32CS_SNAPMODULE32, pid)
        else {
            return false;
        };
        let mut entry = MODULEENTRY32W {
            dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = false;
        if Module32FirstW(snapshot, &mut entry).is_ok() {
            loop {
                if wide_to_string(&entry.szModule).eq_ignore_ascii_case(name) {
                    found = true;
                    break;
                }
                if Module32NextW(snapshot, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

fn inject_pid(pid: u32, dll: &Path) -> Result<(), String> {
    unsafe {
        let process = OpenProcess(
            PROCESS_CREATE_THREAD
                | PROCESS_QUERY_INFORMATION
                | PROCESS_VM_OPERATION
                | PROCESS_VM_WRITE
                | PROCESS_VM_READ,
            FALSE,
            pid,
        )
        .map_err(|e| format!("OpenProcess({pid}): {e}"))?;

        let result = inject_inner(process, pid, dll);
        let _ = CloseHandle(process);
        result
    }
}

unsafe fn inject_inner(process: HANDLE, pid: u32, dll: &Path) -> Result<(), String> {
    let path: Vec<u16> = dll
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let bytes = path.len() * std::mem::size_of::<u16>();

    let mut remote: *mut c_void = std::ptr::null_mut();
    let mut thread: HANDLE = HANDLE::default();
    let mut exit_code = 0u32;

    let result = (|| -> Result<(), String> {
        remote = VirtualAllocEx(
            process,
            None,
            bytes,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if remote.is_null() {
            return Err(format!(
                "VirtualAllocEx({pid}): {}",
                std::io::Error::last_os_error()
            ));
        }

        WriteProcessMemory(process, remote, path.as_ptr() as *const c_void, bytes, None)
            .map_err(|e| format!("WriteProcessMemory({pid}): {e}"))?;

        let kernel32 = GetModuleHandleW(w!("kernel32.dll"))
            .map_err(|e| format!("GetModuleHandle(kernel32): {e}"))?;
        let load_library = GetProcAddress(kernel32, s!("LoadLibraryW"))
            .ok_or_else(|| "LoadLibraryW not found".to_string())?;
        // LPTHREAD_START_ROUTINE returns u32; only zero/non-zero matters here.
        let start: unsafe extern "system" fn(*mut c_void) -> u32 =
            std::mem::transmute(load_library);

        thread = CreateRemoteThread(
            process,
            None,
            0,
            Some(start),
            Some(remote as *const c_void),
            0,
            None,
        )
        .map_err(|e| format!("CreateRemoteThread({pid}): {e}"))?;

        if WaitForSingleObject(thread, 15_000) != WAIT_OBJECT_0 {
            return Err(format!("timed out waiting for LoadLibraryW in pid {pid}"));
        }
        GetExitCodeThread(thread, &mut exit_code)
            .map_err(|e| format!("GetExitCodeThread({pid}): {e}"))?;
        // The exit code is only the low 32 bits of the HMODULE, which can legitimately be zero
        // on x64, so a zero is confirmed against the module list before calling it a failure.
        // The loader's own error code lives in the remote thread and cannot be read from here.
        let name = dll
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        if exit_code == 0 && !module_loaded(pid, &name) {
            return Err(format!(
                "LoadLibraryW returned NULL in pid {pid}: {} could not be loaded                  (wrong architecture, missing dependency, or blocked by security software)",
                dll.display()
            ));
        }
        Ok(())
    })();

    if !remote.is_null() {
        let _ = VirtualFreeEx(process, remote, 0, MEM_RELEASE);
    }
    if !thread.is_invalid() {
        let _ = CloseHandle(thread);
    }
    result
}

/// Injects into every ShellHost.exe that does not already have the hook. Returns how many
/// processes were injected into this time.
pub fn inject_all(dll: &Path) -> Result<usize, String> {
    if !dll.is_file() {
        return Err(format!("hook DLL not found: {}", dll.display()));
    }
    let pids = shellhost_pids();
    if pids.is_empty() {
        return Err(format!("{SHELLHOST} is not running"));
    }

    let mut injected = 0;
    let mut errors = Vec::new();
    for pid in pids {
        if module_loaded(pid, HOOK_DLL) {
            continue;
        }
        match inject_pid(pid, dll) {
            Ok(()) => injected += 1,
            Err(e) => errors.push(e),
        }
    }

    if injected == 0 && !errors.is_empty() {
        return Err(errors.join("; "));
    }
    Ok(injected)
}

/// Starts ShellHost.exe suspended, loads the hook while its main thread has not run a single
/// instruction, waits for the hook to finish patching, and only then resumes the process.
///
/// Fallback only. In testing, a normal injection into a running ShellHost is enough because the
/// `IQuickAction` hook answers every later query; this path exists for builds where Quick
/// Settings caches the brightness capability before a late hook could take effect.
///
/// The shell is started from the system directory as Windows reports it, not from a literal
/// `C:\Windows\System32`: the system drive is not always `C:`.
///
/// **This ends every running ShellHost process first.** The Start menu and Quick Settings
/// disappear until the shell is back. The CLI makes the user confirm with `--force-restart`;
/// do not reach this from an automatic path.
pub fn inject_early(dll: &Path) -> Result<u32, String> {
    if !dll.is_file() {
        return Err(format!("hook DLL not found: {}", dll.display()));
    }

    for pid in shellhost_pids() {
        let _ = terminate_shellhost(pid);
    }
    // Do not wait for the old process to disappear: sihost restarts it within a second. Creating
    // the suspended instance immediately usually wins the race and becomes the singleton.
    std::thread::sleep(Duration::from_millis(150));

    let event = unsafe { CreateEventW(None, TRUE, FALSE, w!(r"Local\BrightTrayHookReady")) }
        .map_err(|e| format!("CreateEventW: {e}"))?;
    let _ = unsafe { ResetEvent(event) };

    let shellhost = system_directory()?.join(SHELLHOST);
    let shellhost_wide: Vec<u16> = shellhost
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();

    let startup = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut process_info = PROCESS_INFORMATION::default();
    let created = unsafe {
        CreateProcessW(
            PCWSTR(shellhost_wide.as_ptr()),
            PWSTR::null(),
            None,
            None,
            FALSE,
            CREATE_SUSPENDED,
            None,
            PCWSTR::null(),
            &startup,
            &mut process_info,
        )
    };
    if let Err(e) = created {
        let _ = unsafe { CloseHandle(event) };
        return Err(format!("CreateProcessW({}): {e}", shellhost.display()));
    }

    let injected = unsafe { inject_inner(process_info.hProcess, process_info.dwProcessId, dll) };
    if let Err(e) = injected {
        let _ = unsafe { ResumeThread(process_info.hThread) };
        let _ = unsafe { CloseHandle(process_info.hThread) };
        let _ = unsafe { CloseHandle(process_info.hProcess) };
        let _ = unsafe { CloseHandle(event) };
        return Err(e);
    }

    // The DLL signals this after its worker has patched the vtable. Resuming earlier would let
    // the shell cache IsApplicable=false before the hook exists.
    let waited = unsafe { WaitForSingleObject(event, 15_000) };
    let _ = unsafe { ResumeThread(process_info.hThread) };
    let pid = process_info.dwProcessId;
    let _ = unsafe { CloseHandle(process_info.hThread) };
    let _ = unsafe { CloseHandle(process_info.hProcess) };
    let _ = unsafe { CloseHandle(event) };

    if waited != WAIT_OBJECT_0 {
        eprintln!(
            "warning: ShellHost pid {pid} resumed before the hook signalled ready; see %TEMP%\\brighttray-hook.log"
        );
    }
    Ok(pid)
}

/// `GetExitCodeProcess` reports this while the process is still running.
const STILL_ACTIVE: u32 = 259;

/// The time the process was created, in FILETIME ticks.
///
/// Two processes that share a pid are different processes, so this is what identifies one.
/// A restarted ShellHost may get the pid of the old one, and then the pid alone is not
/// enough to tell whether the hook is already in it.
fn process_creation(pid: u32) -> Option<u64> {
    unsafe {
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid).ok()?;
        let mut creation = FILETIME::default();
        let mut exit = FILETIME::default();
        let mut kernel = FILETIME::default();
        let mut user = FILETIME::default();
        let ok = GetProcessTimes(process, &mut creation, &mut exit, &mut kernel, &mut user);
        let _ = CloseHandle(process);
        ok.ok()?;
        Some((u64::from(creation.dwHighDateTime) << 32) | u64::from(creation.dwLowDateTime))
    }
}

/// True when the process is gone: it has exited, or it cannot be opened at all.
///
/// `GetExitCodeProcess` still reports `STILL_ACTIVE` while a process is
/// terminating, so a shell that is on its way out is only seen once it is gone.
fn process_gone(pid: u32) -> bool {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid) else {
            // It cannot even be opened: it is gone.
            return true;
        };
        let mut code = 0u32;
        let exited = GetExitCodeProcess(process, &mut code);
        let _ = CloseHandle(process);
        match exited {
            Ok(()) => code != STILL_ACTIVE,
            // The state is unknown: assume the process is still there, so a real failure
            // is still reported.
            Err(_) => false,
        }
    }
}

/// Ends one ShellHost process; sihost starts a new one within about a second.
fn terminate_shellhost(pid: u32) -> Result<(), String> {
    unsafe {
        let process = OpenProcess(PROCESS_TERMINATE, FALSE, pid)
            .map_err(|e| format!("OpenProcess({pid}): {e}"))?;
        let result =
            TerminateProcess(process, 0).map_err(|e| format!("TerminateProcess({pid}): {e}"));
        let _ = CloseHandle(process);
        result
    }
}

/// Ends every running ShellHost process. sihost starts a new one within about a second, and the
/// hook watcher injects into it, so this is how a shell that never asked for the brightness
/// capability gets a fresh start.
///
/// Destructive and visible to the user: the Start menu and Quick Settings disappear until the
/// shell is back. Only call this after the user asked for it — nothing in the normal flow
/// reaches it.
///
/// Succeeds when at least one ShellHost was ended; the errors of the ones that refused are
/// reported only when nothing was ended at all.
pub fn restart_shellhost() -> Result<(), String> {
    let pids = shellhost_pids();
    if pids.is_empty() {
        return Err(format!("{SHELLHOST} is not running"));
    }
    let mut errors = Vec::new();
    let mut ended = 0;
    for pid in pids {
        match terminate_shellhost(pid) {
            Ok(()) => ended += 1,
            Err(e) => errors.push(e),
        }
    }
    if ended == 0 {
        Err(errors.join("; "))
    } else {
        Ok(())
    }
}

/// How many new `ShellHost.exe` processes inside [`CHURN_WINDOW_MS`] mean "the shell is
/// restarting in a loop", not "the user restarted the shell".
///
/// The hook is the only third party inside that process, so a churn loop is assumed to be the
/// hook's fault: after this many starts the watcher stops injecting, which turns a broken hook
/// into a missing brightness tile instead of a shell that keeps killing itself.
const CHURN_LIMIT: usize = 5;

/// The window the churn limit is measured over.
const CHURN_WINDOW_MS: u64 = 120_000;

/// Milliseconds since this process started. Monotonic, so a clock change cannot make the
/// watcher believe the shell restarted.
fn monotonic_ms() -> u64 {
    static EPOCH: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    EPOCH.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// True when `starts` (monotonic milliseconds of the `ShellHost.exe` processes seen, oldest
/// first) already contains [`CHURN_LIMIT`] starts inside [`CHURN_WINDOW_MS`] before `now`.
///
/// Pure on purpose: the watcher's decision must be testable without a shell to watch.
fn churn_tripped(starts: &[u64], now: u64) -> bool {
    starts
        .iter()
        .filter(|&&start| now.saturating_sub(start) < CHURN_WINDOW_MS)
        .count()
        >= CHURN_LIMIT
}

/// What the ShellHost watcher observed. Reported only when it changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WatchEvent {
    /// The hook is loaded in every running ShellHost.
    Hooked,
    /// No ShellHost is running right now.
    NoShellHost,
    /// ShellHost restarted too often to be the user's doing, so nothing is injected any more.
    ShellChurn {
        restarts: usize,
        window_secs: u64,
    },
    DllMissing(PathBuf),
    Error(String),
}

impl std::fmt::Display for WatchEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WatchEvent::Hooked => write!(f, "hook loaded in ShellHost"),
            WatchEvent::NoShellHost => write!(f, "{SHELLHOST} is not running"),
            WatchEvent::ShellChurn {
                restarts,
                window_secs,
            } => write!(
                f,
                "{SHELLHOST} started {restarts} times in {window_secs} s; not injecting the hook \
                 (a restarted shell should not happen; see the warning in the README)"
            ),
            WatchEvent::DllMissing(path) => write!(f, "{} not found", path.display()),
            WatchEvent::Error(message) => f.write_str(message),
        }
    }
}

/// Keeps the hook present for the lifetime of the caller: after a ShellHost restart the new
/// process gets the DLL again. `report` is called whenever the observed state changes, so a
/// missing DLL or a locked-down shell does not flood the output.
///
/// The processes that already have the hook are remembered, so the steady state costs one
/// process snapshot per tick instead of a module snapshot of ShellHost. The remembered identity
/// is the pid *and* the creation time, because a restarted ShellHost may reuse the pid of the
/// old one.
///
/// This never ends a ShellHost process. Injecting into a running ShellHost is enough because
/// the `IQuickAction` hook answers every later query; the builds that cache the brightness
/// capability early are handled by [`inject_early`] or by an explicit [`restart_shellhost`].
pub fn watch_and_inject(dll: PathBuf, report: impl Fn(&WatchEvent) + Send + 'static) {
    std::thread::spawn(move || {
        let name = dll
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| HOOK_DLL.to_string());
        // pid -> creation time of the process that has the hook.
        let mut hooked: std::collections::HashMap<u32, Option<u64>> =
            std::collections::HashMap::new();
        // Monotonic milliseconds of every `ShellHost.exe` start this watcher has seen, oldest
        // first. Trimmed to the churn window on every tick.
        let mut starts: Vec<u64> = Vec::new();
        // Identity (pid, creation time) of the last process that counted as a start, so a
        // single long-lived shell does not look like a restart on every tick.
        let mut counted: Option<(u32, Option<u64>)> = None;
        let mut last: Option<WatchEvent> = None;

        loop {
            let pids = shellhost_pids();
            hooked.retain(|pid, _| pids.contains(pid));

            let event = if !dll.is_file() {
                WatchEvent::DllMissing(dll.clone())
            } else if pids.is_empty() {
                WatchEvent::NoShellHost
            } else {
                let now = monotonic_ms();
                starts.retain(|start| now.saturating_sub(*start) < CHURN_WINDOW_MS);

                let mut error = None;
                for &pid in &pids {
                    // A pid alone does not identify a process: a restarted ShellHost may get
                    // the pid of the old one, and then it is a different process that does
                    // not have the hook.
                    let created = process_creation(pid);
                    if counted != Some((pid, created)) {
                        counted = Some((pid, created));
                        starts.push(now);
                    }
                    if hooked.get(&pid) == Some(&created) {
                        continue;
                    }
                    // The shell is restarting in a loop. The hook is the only third party in
                    // that process, so stop feeding it: no brightness tile beats a shell that
                    // keeps taking itself down. Deliberately *not* remembered as hooked, so
                    // the watcher resumes on its own once the churn window has passed.
                    if churn_tripped(&starts, now) {
                        continue;
                    }
                    if module_loaded(pid, &name) {
                        hooked.insert(pid, created);
                        continue;
                    }
                    match inject_pid(pid, &dll) {
                        Ok(()) => {
                            hooked.insert(pid, created);
                        }
                        Err(e) => {
                            // The shell may be gone — a restart the user asked for,
                            // or a crash. A process that is already gone is not an
                            // error.
                            if !process_gone(pid) {
                                error = Some(e);
                            }
                        }
                    }
                }
                if error.is_none() && churn_tripped(&starts, now) {
                    WatchEvent::ShellChurn {
                        restarts: starts.len(),
                        window_secs: CHURN_WINDOW_MS / 1000,
                    }
                } else {
                    error.map_or(WatchEvent::Hooked, WatchEvent::Error)
                }
            };

            if last.as_ref() != Some(&event) {
                report(&event);
                last = Some(event);
            }
            // Poll quickly while a ShellHost is coming back, so the hook lands within moments
            // of its start.
            let fast = matches!(last, Some(WatchEvent::NoShellHost));
            std::thread::sleep(Duration::from_millis(if fast { 100 } else { 500 }));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The directory must come from Windows, not from a literal: the test checks that it exists
    /// and that the shell is looked up inside it. `ShellHost.exe` itself is not asserted on —
    /// the Windows CI runner is a Server image, where the shell host may be absent.
    #[test]
    fn four_restarts_in_the_window_are_tolerated() {
        let starts = [1_000, 2_000, 3_000, 4_000];
        assert!(!churn_tripped(&starts, 5_000));
    }

    #[test]
    fn five_restarts_in_the_window_trip_the_watcher() {
        let starts = [1_000, 2_000, 3_000, 4_000, 5_000];
        assert!(churn_tripped(&starts, 5_500));
    }

    #[test]
    fn restarts_older_than_the_window_do_not_count() {
        // Three long ago plus two now: a shell that restarted once yesterday and twice today is
        // not churning.
        let starts = [
            0,
            1_000,
            2_000,
            CHURN_WINDOW_MS + 1_000,
            CHURN_WINDOW_MS + 2_000,
        ];
        assert!(!churn_tripped(&starts, CHURN_WINDOW_MS + 3_000));
    }

    #[test]
    fn no_starts_is_not_churn() {
        assert!(!churn_tripped(&[], 0));
        assert!(!churn_tripped(&[], u64::MAX));
    }

    #[test]
    fn a_window_edge_is_inside_the_window() {
        // `now - start == CHURN_WINDOW_MS` is still inside: the window is half-open.
        let starts = [0, 1, 2, 3, 4];
        assert!(churn_tripped(&starts, CHURN_WINDOW_MS - 1));
        assert!(!churn_tripped(&starts, CHURN_WINDOW_MS));
    }

    #[test]
    fn system_directory_is_the_loader_directory() {
        let dir = system_directory().expect("GetSystemDirectoryW");
        assert!(dir.is_absolute(), "{} is not absolute", dir.display());
        assert!(dir.is_dir(), "{} is not a directory", dir.display());
        // `GetSystemDirectoryW` keeps whatever casing the volume recorded; the CI runner
        // answers `system32`, a desktop answers `System32`. Compare without case.
        let name = dir.file_name().and_then(|name| name.to_str());
        assert!(
            name.is_some_and(|name| name.eq_ignore_ascii_case("System32")),
            "{} is not the system directory",
            dir.display()
        );
        assert!(dir.join(SHELLHOST).ends_with(SHELLHOST));
    }
}
