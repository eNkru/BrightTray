# Directory Structure

## The crate

```
hook/
├── Cargo.toml
└── src/
    ├── lib.rs        # everything: vtable patching, the watchers, the handlers
    └── pipe.rs       # the client side of the BrightTray helper pipe
```

`hook/Cargo.toml` is minimal on purpose:

```toml
[lib]
name = "brighttray_hook"
crate-type = ["cdylib"]
```

There is no `[[bin]]`, no `main`, and no dependency on the helper crate — the
DLL is loaded into ShellHost and must not drag the helper's code in with it.

## Responsibilities

| File | Owns |
|------|------|
| `lib.rs` | The `ISettingItem` and `IQuickAction` vtable patches, the install loop, the DDC read-back watcher, the UI handler registry, the file logger |
| `pipe.rs` | The connection to `\\.\pipe\BrightTray`: `get_all()` for the current percent, `set_all()` to push a new one |

The module doc block at the top of `lib.rs` is the architecture document of
this package: it explains what Quick Settings asks, which vtables are patched
and why. **Keep it accurate** — it is the first thing a reader sees and the
only place the whole design is written down.

## How the DLL is built and shipped

The DLL is produced twice over:

1. `app/build.rs` runs a nested `cargo build -p brighttray-hook` into
   `OUT_DIR` and copies `brighttray_hook.dll` next to the embedded copy in
   `app/src/hookdll.rs`. `BrightTray.exe` therefore ships as a single file.
2. `cargo build --release` at the workspace root also produces
   `target/release/brighttray_hook.dll`, which the CLI expects next to
   `brighttray-helper.exe` (`inject::default_dll_path`).

The nested build in `app/build.rs` uses its own target directory under
`OUT_DIR`, because the outer cargo holds a lock on the workspace target
directory while the build script runs. Do not "simplify" that to a plain
`cargo build` — it deadlocks.

## The helper pipe lives in the hook, not in the helper

The pipe *server* is in the helper (`helper/src/server.rs`). The pipe *client*
is here (`pipe.rs`), because only this process needs to talk to the shell
internals. The protocol itself is documented in the helper package's
[Pipe Protocol](../../brighttray-helper/backend/pipe-protocol.md).
