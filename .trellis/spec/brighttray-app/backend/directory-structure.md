# Directory Structure

## The crate

```
app/
├── Cargo.toml
├── build.rs            # embeds the icon resource and builds the hook DLL
├── app.rc              # Windows resource script (icon + version info)
├── src/
│   ├── main.rs         # the whole app: App state, UserEvent, event loop, IPC
│   ├── config.rs       # %APPDATA%\BrightTray\config.ini
│   ├── i18n.rs         # tr(zh, en) and the language detection
│   ├── instance.rs     # single-instance mutex + show event
│   ├── autostart.rs    # the HKCU Run key
│   └── hookdll.rs      # writes the embedded hook DLL to %APPDATA%\BrightTray
└── ui/
    └── index.html      # the whole UI: HTML + CSS + JS, no build step
```

The binary is named `BrightTray` (capital B) while the package is
`brighttray-app` — `[[bin]] name = "BrightTray"` in `app/Cargo.toml`.

## Module responsibilities

| Module | Owns |
|--------|------|
| `main.rs` | The `App` struct, the `UserEvent` enum, the tao event loop, the tray menu, the IPC dispatch, `push_state()` |
| `config.rs` | The two saved settings (`monitor_key`, `monitor_name`) in `%APPDATA%\BrightTray\config.ini` |
| `i18n.rs` | Language detection and `tr(zh, en)` |
| `instance.rs` | One app at a time: a mutex decides, an event asks the first one to show itself |
| `autostart.rs` | The `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` entry |
| `hookdll.rs` | Extracts the embedded `brighttray_hook.dll` to disk so ShellHost can `LoadLibraryW` it |

`main.rs` is large (~530 lines) but it is one file on purpose: the app has one
event loop, one state struct and one page. Split it only when a module has a
clear boundary of its own, the way `config` / `i18n` / `instance` /
`autostart` / `hookdll` do.

## The page is a single file with no build step

`app/ui/index.html` holds the markup, the CSS and the JavaScript, and it is
embedded with `include_str!("../ui/index.html")`. There is no bundler, no
framework and no npm.

Two placeholders are substituted at load time:

```rust
.with_html(
    UI.replace("{{ICON}}", ICON_SVG)
        .replace("{{LANG}}", i18n::lang().code()),
)
```

`{{ICON}}` is `assets/icon.svg` (`include_str!`), `{{LANG}}` is `zh` or `en`
and selects the page's string table.

The CSS uses custom properties for the palette and a documented alignment
grid; the comment block above `:root` explains the grid. Keep that comment
accurate when the layout changes.

## What the app uses from the helper library

```rust
use brighttray::ddc;
use brighttray::inject::{self, WatchEvent};
use brighttray::server::{Server, Target};
```

- `ddc::enumerate()` — to list the displays.
- `server::Server` — the pipe server inside the app process, so the hook can
  reach it.
- `server::Target` — which display `getall`/`setall` act on.
- `inject::watch_and_inject` — keeps the hook present across ShellHost
  restarts.

## `build.rs` does two things

1. `embed_resource::compile("app.rc", embed_resource::NONE)` — the icon and
   version resource.
2. A nested `cargo build -p brighttray-hook` into `OUT_DIR`, whose output is
   embedded by `app/src/hookdll.rs` with `include_bytes!`.

The nested build needs its own target directory under `OUT_DIR` because the
outer cargo holds the lock on the workspace target directory. Do not simplify
that.

## Where new code goes

- A new persistent setting → `config.rs` (add the field, add the `key=value`
  arm in `load()`, add it to the `save()` format).
- A new user-visible string → `tr()` in the Rust code **and** the `STRINGS`
  table in `ui/index.html`.
- A new page → host command → `App::on_ipc` arm; see
  [UI Contract](./ui-contract.md).
- A new startup behaviour → the `UserEvent` enum plus the event loop arms.
