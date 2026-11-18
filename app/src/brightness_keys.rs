//! The keyboard's brightness keys, and the third brightness path Windows keeps to itself.
//!
//! Three things can change brightness on a desktop, and only two of them reach BrightTray:
//!
//! 1. The Quick Settings tile → the `IQuickAction` / `ISettingItem` hook in `ShellHost` → pipe.
//! 2. A brightness change made through the power manager → [`crate::slider`]-style
//!    subscription in the helper → pipe. Only fires on a display Windows can dim itself.
//! 3. The keyboard's brightness keys → the Windows OSD → **nothing**. Measured on the
//!    reporting machine: no setting-item call, no power-setting write, no DDC write. The OSD
//!    is cosmetic there, and the external monitor never moves.
//!
//! This module closes path 3 without touching the shell: a raw-input sink on the Consumer
//! usage page sees the HID reports the keys produce (`0x006F` increment, `0x0070`
//! decrement), and the caller turns each press into the same `setall` the slider sends.
//! Raw input is delivered to every registered sink regardless of who else consumed the key,
//! so this works while Windows also shows its OSD — the OSD keeps its own number, which is
//! documented in the README.
//!
//! Nothing here opens a monitor handle, reads a registry value or requires elevation, and a
//! failure to register is reported once and then ignored: the slider must keep working.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::sync::mpsc::{channel, Sender};
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::{
    GetRawInputData, RegisterRawInputDevices, RAWINPUTDEVICE, RAWINPUTHEADER, RIDEV_INPUTSINK,
    RID_INPUT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
    PostThreadMessageW, SetWindowLongPtrW, TranslateMessage, GWLP_WNDPROC, MSG, WINDOW_EX_STYLE,
    WINDOW_STYLE, WM_INPUT, WM_QUIT, WNDPROC,
};

/// The Consumer-page usages the keys send (HID Usage Tables 0x0C/0x006F and 0x0C/0x0070).
const CONSUMER_BRIGHTNESS_INCREMENT: u16 = 0x006F;
const CONSUMER_BRIGHTNESS_DECREMENT: u16 = 0x0070;

/// One key press moves the monitor by ten points, the granularity the shell's own slider uses.
pub const STEP: u32 = 10;

/// The HID usage page and usage of a Consumer Control collection.
const USAGE_PAGE_CONSUMER: u16 = 0x0C;
const USAGE_CONSUMER_CONTROL: u16 = 0x01;
/// A generic keyboard, registered so a device that sends the keys as normal keys is not missed.
const USAGE_PAGE_KEYBOARD: u16 = 0x01;
const USAGE_KEYBOARD: u16 = 0x06;

/// Which way the user asked the brightness to move.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Up,
    Down,
}

/// The caller's callback, boxed so it can live in a thread local.
type Sink = Box<dyn Fn(Direction)>;

thread_local! {
    /// The caller's sink; set once on the watcher thread before the message loop starts.
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
    /// Which direction is currently held, so a key that repeats its usage steps once.
    static HELD: Cell<Option<Direction>> = const { Cell::new(None) };
    /// `STATIC`'s own window procedure, replaced by [`window_proc`] and called through.
    static PREVIOUS_PROC: Cell<isize> = const { Cell::new(0) };
}

/// A running raw-input watcher. Dropping it stops the thread.
pub struct KeyWatcher {
    thread: u32,
}

impl Drop for KeyWatcher {
    fn drop(&mut self) {
        unsafe {
            let _ = PostThreadMessageW(self.thread, WM_QUIT, WPARAM(0), LPARAM(0));
        }
    }
}

/// Starts watching for the brightness keys. `sink` runs on the watcher thread, once per press.
///
/// The error is returned only when the watcher could not be set up at all; the caller is
/// expected to log it and carry on, because the Quick Settings path does not depend on this.
pub fn start(sink: impl Fn(Direction) + Send + 'static) -> Result<KeyWatcher, String> {
    let (ready_tx, ready_rx) = channel::<Result<u32, String>>();
    std::thread::spawn(move || unsafe { run(sink, ready_tx) });
    // A thread that dies before reporting shows up as a timeout; either way start() answers.
    match ready_rx.recv_timeout(Duration::from_secs(2)) {
        Ok(Ok(thread)) => Ok(KeyWatcher { thread }),
        Ok(Err(e)) => Err(e),
        Err(e) => Err(format!("raw input watcher did not start: {e}")),
    }
}

/// The brightness `direction` should move `current` to, clamped to 0-100.
///
/// `current` is what the monitor reports, not a cached number, so the keys stay correct after
/// the slider, the OSD or the monitor's own knob changed something.
pub(crate) fn next_brightness(current: u32, direction: Direction) -> u32 {
    let current = current.min(100);
    match direction {
        Direction::Up => (current + STEP).min(100),
        Direction::Down => current.saturating_sub(STEP),
    }
}

/// The direction a Consumer Control report asks for, or `None`.
///
/// A HID input report is a report ID (when the descriptor declares one) followed by 16-bit
/// usages. Both alignments are tried, so the report ID does not have to be assumed — the
/// reporting keyboard sends `02 6F 00`, i.e. ID 2 and usage 0x006F.
pub(crate) fn parse_consumer_report(report: &[u8]) -> Option<Direction> {
    for offset in [0usize, 1] {
        let Some(pairs) = report.get(offset..) else {
            continue;
        };
        for pair in pairs.as_chunks::<2>().0 {
            match u16::from_le_bytes([pair[0], pair[1]]) {
                CONSUMER_BRIGHTNESS_INCREMENT => return Some(Direction::Up),
                CONSUMER_BRIGHTNESS_DECREMENT => return Some(Direction::Down),
                _ => {}
            }
        }
    }
    None
}

/// True when the report carries no usage at all: the key was released.
fn is_release(report: &[u8]) -> bool {
    // The first byte may be a report ID, which stays non-zero on release.
    report.iter().skip(1).all(|byte| *byte == 0) || report.len() <= 2
}

unsafe extern "system" fn window_proc(
    window: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if message == WM_INPUT {
        handle_raw_input(lparam);
    }
    // The window procedure is never uninstalled, so a zero here cannot happen in practice;
    // falling through to `DefWindowProcW` is still the safe answer if it somehow does.
    let previous = std::mem::transmute::<isize, WNDPROC>(PREVIOUS_PROC.with(|stored| stored.get()));
    match previous {
        Some(pointer) => CallWindowProcW(Some(pointer), window, message, wparam, lparam),
        None => DefWindowProcW(window, message, wparam, lparam),
    }
}

/// Reads one `WM_INPUT` message and turns a brightness usage into a sink call.
unsafe fn handle_raw_input(lparam: LPARAM) {
    let header_size = std::mem::size_of::<RAWINPUTHEADER>() as u32;
    let mut size: u32 = 0;
    if GetRawInputData(
        windows::Win32::UI::Input::HRAWINPUT(lparam.0 as *mut c_void),
        RID_INPUT,
        None,
        &mut size,
        header_size,
    ) != 0
        || size == 0
    {
        return;
    }

    let mut buffer = vec![0u8; size as usize];
    let copied = GetRawInputData(
        windows::Win32::UI::Input::HRAWINPUT(lparam.0 as *mut c_void),
        RID_INPUT,
        Some(buffer.as_mut_ptr() as *mut c_void),
        &mut size,
        header_size,
    );
    if copied != size {
        return;
    }

    let header: RAWINPUTHEADER = std::ptr::read_unaligned(buffer.as_ptr() as *const RAWINPUTHEADER);
    // Only HID reports carry a Consumer usage; a keyboard event is 0x6F only by accident.
    if header.dwType != 2 {
        return;
    }

    let counts = buffer.as_ptr().add(header_size as usize) as *const u32;
    let size_hid = std::ptr::read_unaligned(counts) as usize;
    let count = std::ptr::read_unaligned(counts.add(1)) as usize;
    let available = buffer.len().saturating_sub(header_size as usize + 8);
    let length = (size_hid * count).min(available);
    if length == 0 {
        return;
    }
    let report = std::slice::from_raw_parts(counts.add(2) as *const u8, length);

    match parse_consumer_report(report) {
        Some(direction) => {
            // The same usage repeating means the key is still held down: step once per press.
            if HELD.with(|held| held.get()) == Some(direction) {
                return;
            }
            HELD.with(|held| held.set(Some(direction)));
            SINK.with(|sink| {
                if let Some(sink) = sink.borrow().as_ref() {
                    sink(direction);
                }
            });
        }
        None => {
            if is_release(report) {
                HELD.with(|held| held.set(None));
            }
        }
    }
}

/// The watcher thread: a hidden window, a raw-input registration and a message loop.
unsafe fn run(sink: impl Fn(Direction) + 'static, ready: Sender<Result<u32, String>>) {
    SINK.with(|slot| *slot.borrow_mut() = Some(Box::new(sink)));

    // A `STATIC` window that is never shown: it exists only to own the raw-input sink. Its own
    // procedure ignores `WM_INPUT`, so it is subclassed before the registration.
    let window = match CreateWindowExW(
        WINDOW_EX_STYLE(0),
        w!("STATIC"),
        PCWSTR::null(),
        WINDOW_STYLE(0),
        -2000,
        -2000,
        1,
        1,
        None,
        None,
        None,
        None,
    ) {
        Ok(window) => window,
        Err(e) => {
            let _ = ready.send(Err(format!("CreateWindowExW(STATIC): {e}")));
            return;
        }
    };

    let previous = SetWindowLongPtrW(window, GWLP_WNDPROC, window_proc as *const () as isize);
    if previous == 0 {
        let error = std::io::Error::last_os_error();
        let _ = ready.send(Err(format!("SetWindowLongPtrW(GWLP_WNDPROC): {error}")));
        return;
    }
    PREVIOUS_PROC.with(|stored| stored.set(previous));

    let devices = [
        RAWINPUTDEVICE {
            usUsagePage: USAGE_PAGE_KEYBOARD,
            usUsage: USAGE_KEYBOARD,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: window,
        },
        RAWINPUTDEVICE {
            usUsagePage: USAGE_PAGE_CONSUMER,
            usUsage: USAGE_CONSUMER_CONTROL,
            dwFlags: RIDEV_INPUTSINK,
            hwndTarget: window,
        },
    ];
    if let Err(e) = RegisterRawInputDevices(&devices, std::mem::size_of::<RAWINPUTDEVICE>() as u32)
    {
        let _ = ready.send(Err(format!("RegisterRawInputDevices: {e}")));
        return;
    }

    let _ = ready.send(Ok(GetCurrentThreadId()));

    let mut message = MSG::default();
    // 0 is WM_QUIT (posted by Drop), -1 is an error: both end the loop.
    while GetMessageW(&mut message, None, 0, 0).0 > 0 {
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The report the reporting keyboard sends for its brightness-up key.
    #[test]
    fn a_brightness_up_report_is_recognised() {
        assert_eq!(
            parse_consumer_report(&[0x02, 0x6F, 0x00]),
            Some(Direction::Up)
        );
    }

    #[test]
    fn a_brightness_down_report_is_recognised() {
        assert_eq!(
            parse_consumer_report(&[0x02, 0x70, 0x00]),
            Some(Direction::Down)
        );
    }

    /// The release report carries usage 0, which is not a brightness request.
    #[test]
    fn a_release_report_is_not_a_press() {
        assert_eq!(parse_consumer_report(&[0x02, 0x00, 0x00]), None);
        assert!(is_release(&[0x02, 0x00, 0x00]));
        assert!(!is_release(&[0x02, 0x6F, 0x00]));
    }

    /// A descriptor without a report ID puts the usage first.
    #[test]
    fn a_report_without_a_report_id_is_recognised() {
        assert_eq!(parse_consumer_report(&[0x6F, 0x00]), Some(Direction::Up));
        assert_eq!(parse_consumer_report(&[0x70, 0x00]), Some(Direction::Down));
    }

    /// Volume and play/pause must not move the brightness.
    #[test]
    fn other_consumer_usages_are_ignored() {
        assert_eq!(parse_consumer_report(&[0x02, 0xE9, 0x00]), None);
        assert_eq!(parse_consumer_report(&[0x02, 0xCD, 0x00]), None);
        assert_eq!(parse_consumer_report(&[]), None);
    }

    #[test]
    fn a_step_moves_by_ten_and_stops_at_the_ends() {
        assert_eq!(next_brightness(60, Direction::Up), 70);
        assert_eq!(next_brightness(60, Direction::Down), 50);
        assert_eq!(next_brightness(95, Direction::Up), 100);
        assert_eq!(next_brightness(100, Direction::Up), 100);
        assert_eq!(next_brightness(5, Direction::Down), 0);
        assert_eq!(next_brightness(0, Direction::Down), 0);
    }

    /// A monitor that reports something out of range must not produce an out-of-range write.
    #[test]
    fn an_impossible_reading_is_clamped() {
        assert_eq!(next_brightness(120, Direction::Up), 100);
        assert_eq!(next_brightness(120, Direction::Down), 90);
    }
}
