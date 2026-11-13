# Directory Structure

## Workspace layout

```
Cargo.toml            # workspace: members = ["helper", "hook", "app"]
helper/               # core library + CLI  (package brighttray-helper)
hook/                 # cdylib injected into ShellHost.exe (package brighttray-hook)
app/                  # desktop GUI (package brighttray-app)
docs/                 # research notes and screenshots
assets/               # icons, banners
```

The workspace release profile is tuned for a single-file executable
(`opt-level = "s"`, `lto = true`, `strip = true` in the root `Cargo.toml`) —
keep it that way, `BrightTray.exe` is meant to ship as one file.

## The `helper` crate

```
helper/
├── Cargo.toml
└── src/
    ├── lib.rs        # module list + crate-level docs
    ├── main.rs       # CLI: argument parsing and exit codes
    ├── ddc.rs        # monitor enumeration and DDC/CI brightness
    ├── server.rs     # the \\.\pipe\BrightTray server
    ├── inject.rs     # ShellHost injection and the hook watcher
    ├── slider.rs     # Windows brightness slider subscription
    └── probe.rs      # read-only diagnostic of the shell's brightness setting
```

The library is named `brighttray`, **not** `brighttray_helper`
(`[lib] name = "brighttray"` in `helper/Cargo.toml`). Consumers write
`use brighttray::ddc;` — `app/src/main.rs` does exactly that. Do not rename it.

The binary is named `brighttray-helper` and lives at `helper/src/main.rs`. It
holds almost no logic of its own: it parses arguments, calls into the library
and prints. Each command is a flat `if args.first() == Some("...")` block that
returns an `ExitCode`; the last two (`list` and `set`) share one `match` on
the first argument at the end of `main`.

## Module responsibilities

| Module | Owns | Must not |
|--------|------|----------|
| `ddc` | The only place that calls the dxva2 API. Monitor enumeration, brightness read/write, monitor identity | Know about the pipe, the app or the hook |
| `server` | The pipe protocol, the worker thread that owns the monitor handles, the `Target` that selects which monitors `getall`/`setall` act on | Talk to the UI or to the registry |
| `inject` | Finding ShellHost, `CreateRemoteThread(LoadLibraryW)` injection, the early-inject path, the watcher | Patch memory itself — that is the hook's job |
| `slider` | Subscribing to `GUID_VIDEO_CURRENT_MONITOR_BRIGHTNESS` | Assume the value is a monitor brightness it can set |
| `probe` | A read-only diagnostic that prints what the shell's setting object answers | Change anything; it must stay safe to run on a live machine |

`server` depends on `ddc` and `slider`; `inject` and `probe` are independent of
both. `ddc` depends on nothing in the crate.

Two hard-coded system paths live in `inject.rs`:
`C:\Windows\System32\ShellHost.exe` (the early-inject path starts this
executable) and the DLL name `brighttray_hook.dll`.

## Where new code goes

- A new DDC/CI capability (a new VCP code, a new monitor property) belongs in
  `helper/src/ddc.rs`, next to the existing pacing and retry helpers.
- A new pipe command belongs in `helper/src/server.rs`: add a `Cmd` variant,
  handle it in `worker()`, and add the parsing arm in `handle_client()`.
- A new CLI command belongs in `helper/src/main.rs` — add the `if` block and a
  line in `usage()`. There is no argument-parsing crate in this project; keep
  the hand-written style.
- A new diagnostic belongs in `helper/src/probe.rs`, and must stay read-only.

## Library / binary split

`main.rs` uses `use brighttray::{ddc, inject, probe, server, slider};` — the
binary consumes the library exactly like any other crate would. Anything the
app might need belongs in the library (`lib.rs` module list), not in `main.rs`.
