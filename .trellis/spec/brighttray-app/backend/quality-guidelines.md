# Quality Guidelines

## Threading

The tao event loop runs on the main thread. Everything slow happens
elsewhere and comes back as a `UserEvent`:

```rust
/// Enumerates monitors and probes DDC/CI off the UI thread (each probe can take ~200 ms).
fn scan(&mut self) {
    if self.scanning { return; }
    self.scanning = true;
    self.push_state();
    let proxy = self.proxy.clone();
    std::thread::spawn(move || {
        let views = ddc::enumerate().iter().map(|m| MonitorView { … }).collect();
        let _ = proxy.send_event(UserEvent::Scanned(views));
    });
}
```

Rules:

- **Never block the event loop.** A DDC/CI probe, a file write or a registry
  write goes to a worker thread that reports back with a `UserEvent`.
- **Every `UserEvent` arm in the event loop touches `app`.** The `App` struct
  is not `Send`; it stays on the loop thread.
- A worker thread never touches the `Window` or the `WebView` — it only sends
  an event.

## Window behaviour

- The window is created with `.with_visible(false)` and is revealed only
  after the page reported its size (`reveal_pending`).
- Closing the window **hides** it; the app stays in the tray. Only the tray
  menu's "退出" / "Quit" exits.

  ```rust
  /// Closing the window always keeps BrightTray running in the tray; "退出" in the tray menu
  /// is the way out.
  fn hide(&self) { self.window.set_visible(false); }
  ```

- The window is frameless (`.with_decorations(false)`) with a shadow and
  rounded corners (`round_corners` uses `DwmSetWindowAttribute`).
- The page is **opaque on purpose**:

  ```rust
  // Opaque on purpose: a transparent WebView2 does not clear what it painted before, so
  // changed text (the brightness percentage) left a ghost of the old value behind.
  ```

  Do not make it transparent to "fix" a colour issue.

## The app restarts ShellHost

The app asks the watcher to restart a long-running ShellHost:

```rust
// Restart a ShellHost that has been up for a while: its Quick Settings
// panel may already have cached "no brightness".
inject::watch_and_inject(dll, true, move |event| { … });
```

The `true` is `restart_stale`: a ShellHost that has been alive for more than
10 s is terminated (and restarted by `sihost`) so that the hook is in place
before Quick Settings builds its panel. The user sees the Start menu and
Quick Settings disappear briefly.

The CLI does **not** do this — `brighttray-helper serve` passes `false`. The
difference is deliberate in the code, and the flag is the whole difference.

## Single instance

```rust
let Some(_primary) = instance::acquire() else {
    return;
};
```

This is the **first statement of `main`**. A second launch asks the running
one to show its window and exits. The mutex handle is held for the lifetime of
the process (`Primary`), so the instance stays unique.

## Autostart

The Run entry is `"<path to exe>" --autostart`. A logon start behaves
differently from a manual start:

- the window is never shown (`reveal_pending: false`);
- the saved display is resumed automatically (`resume_pending`), with up to
  `RESUME_ATTEMPTS = 10` rescans 3 s apart, because a DisplayPort monitor can
  take a few seconds to appear after wake;
- if the saved display never comes back, the user is asked to pick one again
  instead of the app idling invisibly.

`autostart::refresh_path()` runs at every startup: if the exe moved, the Run
entry is rewritten.

## The embedded hook DLL

`hookdll::extract()` writes the embedded DLL to
`%APPDATA%\BrightTray\brighttray_hook.dll`. It sits next to the settings
rather than in `%LOCALAPPDATA%` on purpose — ShellHost refused to load the
same file from `%LOCALAPPDATA%\BrightTray`, where it inherits an app-container
ACE from the parent folder.

If the file on disk differs from the embedded one and ShellHost still has the
old one loaded, the old file is renamed to `brighttray_hook.old-<pid>.dll`
(a loaded DLL can be renamed) and the new one is written. Stale copies are
deleted on the next run.

## The page

- The page is plain HTML/CSS/JS in one file, no build step, no framework.
- **Text that comes from Rust must be escaped.** The page escapes monitor
  names with `esc()`; Rust builds the state with `serde_json::json!`, which
  escapes for JSON. Never interpolate a monitor name into `innerHTML`
  unescaped.
- The page keeps its own `selected` key and Rust re-validates it on apply.
- The palette lives in CSS custom properties with a light and a dark variant;
  the layout follows the grid documented in the comment above `:root`.

## Forbidden patterns

| Forbidden | Why |
|-----------|-----|
| Calling DDC/CI on the event loop thread | A probe takes ~200 ms and freezes the window |
| Starting a monitor handle in the app | The helper's `Server` owns the handles |
| A second `Server` in the app process | The pipe name is claimed once; `AddrInUse` is the symptom |
| `println!` / `eprintln!` in the app | There is no console in a `windows_subsystem` binary |
| A user-visible string without `tr()` | Half-translated UI |
| Making the WebView transparent | It leaves ghost text behind (see the comment in `main.rs`) |
| Showing the window before the page has sized it | The window jumps while it appears |
| Duplicating helper logic in the app | Two implementations of DDC/CI, one pipe protocol, two bugs |

## Testing

There is **no automated test suite** in this repository. Verification is
manual.

## Verification steps

```powershell
cargo build --release
cargo clippy --workspace --all-targets
cargo fmt --check
```

Then run `target\release\BrightTray.exe` and check:

1. The tray icon appears; clicking it opens the window.
2. The display list shows the connected monitors with their brightness.
3. **Apply** starts the server and the hook; the status line goes from
   "未启用" to "运行中" (or "正在挂接快速设置…" first).
4. <kbd>Win</kbd> + <kbd>A</kbd> shows the brightness slider; dragging it
   changes the monitor.
5. Closing the window keeps the app in the tray; "退出" in the tray menu ends
   it.
6. The autostart toggle adds and removes the Run entry.
7. A logon start (`target\release\BrightTray.exe --autostart`) does not show
   the window and resumes the saved display.

## Review checklist

- [ ] Nothing blocking happens on the event loop thread.
- [ ] Every new user-visible string exists in both `tr()` and the page's
      `STRINGS`.
- [ ] The state JSON and the page's `statusOf()` agree on the `hook.kind`
      values.
- [ ] The app still uses `brighttray::` for DDC/CI, injection and the server.
- [ ] `cargo clippy` and `cargo fmt --check` are clean.
