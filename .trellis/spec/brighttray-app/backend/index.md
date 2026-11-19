# BrightTray App — Backend Guidelines

`brighttray-app` builds `BrightTray.exe`, the desktop app: a tray icon, a
small frameless window that lists the displays, and the logic that decides
which display the Quick Settings brightness slider controls.

The app is a **thin shell around the helper library**. It owns the window, the
page, the settings file and the registry entry — and nothing else. All DDC/CI,
the pipe server and the injection live in `brighttray-helper`
(`helper/src/`).

## Guidelines Index

| Guide | What it covers |
|-------|----------------|
| [Directory Structure](./directory-structure.md) | Module map, the embedded HTML page, the build script |
| [UI Contract](./ui-contract.md) | The IPC messages, the state JSON, who owns what |
| [i18n](./i18n.md) | `tr(zh, en)`, the page's string table, forcing a language |
| [Error Handling](./error-handling.md) | Bilingual user-facing errors, what is fatal |
| [Logging Guidelines](./logging-guidelines.md) | No log file; the UI is the diagnostic surface |
| [Quality Guidelines](./quality-guidelines.md) | Threading rules, the event loop, how to verify |

## Non-negotiables

1. **Do not re-implement the helper.** Use `brighttray::ddc`,
   `brighttray::inject` and `brighttray::server`. The app must not open a
   monitor handle itself and must not talk to the pipe.
2. **Rust is the source of truth.** The page renders a state object it is
   given; it never computes brightness or decides which monitor is applied.
3. **The event loop thread only does UI work.** DDC probes take ~200 ms and
   run on a worker thread that reports back through `UserEvent`.
4. **Every user-visible string goes through `tr()`.** See [i18n](./i18n.md).
