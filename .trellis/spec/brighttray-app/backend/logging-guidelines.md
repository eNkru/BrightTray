# Logging Guidelines

## The app has no log file

There is no `println!`, no `eprintln!` and no logging crate in
`brighttray-app`. It is a `#![windows_subsystem = "windows"]` binary: it has
no console to print to, and a stray `println!` would go nowhere.

The app's diagnostics are its **UI**:

| Surface | What it shows |
|---------|---------------|
| the notice area (red bar) | `App::error` — a user-facing failure |
| the status line (bottom) | the hook state (`hook.kind`) |
| the display list | the live brightness percentage of each display |

That is usually enough, because the app's job is short: pick a display, watch
the hook state, see the percentage.

## Where the file logs live

| File | Written by |
|------|-------------|
| `%TEMP%\brighttray-hook.log` | the hook DLL (`hook/src/lib.rs`) — see the hook's [Logging Guidelines](../../brighttray-hook/backend/logging-guidelines.md) |
| `%APPDATA%\BrightTray\config.ini` | the app's settings (not a log) |
| `%APPDATA%\BrightTray\brighttray_hook.dll` | the extracted hook (not a log) |

The app never writes the hook log. When a user reports "the slider does not
appear", the README FAQ points them at `%TEMP%\brighttray-hook.log` — the app
does not need a second log for the same problem.

## Debugging instead of logging

Prefer making state visible over adding a log:

- add a field to the state JSON in `push_state()` and show it in the page;
- add a `UserEvent` variant and handle it in the event loop;
- use the helper's CLI, which *does* print to a console:
  `brighttray-helper list`, `probe`, `watch`.

The `watch` command prints every Windows brightness slider change, which is
the quickest way to see whether the shell side works at all.

## The devtools are available in debug builds

```rust
.with_devtools(cfg!(debug_assertions))
```

In a debug build the WebView2 devtools are enabled, so the page's console and
the state JSON are inspectable. Release builds have no devtools and no
console.
