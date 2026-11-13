# BrightTray Helper — Backend Guidelines

`brighttray-helper` is the core of the workspace. It ships twice:

- as the **library** `brighttray` (`helper/src/lib.rs`), consumed by `brighttray-app`
  through `brighttray-helper = { path = "../helper" }` in `app/Cargo.toml`
- as the **CLI binary** `brighttray-helper` (`helper/src/main.rs`)

It owns everything that touches hardware or Windows internals: DDC/CI over
dxva2, the `\\.\pipe\BrightTray` server, the ShellHost hook injector, the
power-manager brightness subscription and the read-only `probe` diagnostic.

## Guidelines Index

| Guide | What it covers |
|-------|----------------|
| [Directory Structure](./directory-structure.md) | Module map, library vs binary split, where new code goes |
| [DDC/CI Access](./ddc-ci.md) | Pacing, retries, write verification, percentage scaling, stale handles |
| [Pipe Protocol](./pipe-protocol.md) | The `\\.\pipe\BrightTray` wire contract, threading, coalescing |
| [Windows FFI Safety](./windows-ffi.md) | `unsafe` blocks, handles, UTF-16, HSTRING, ABI calls |
| [Error Handling](./error-handling.md) | `Result`/`String` conventions, exit codes, message style |
| [Logging Guidelines](./logging-guidelines.md) | `eprintln!` diagnostics, no logging framework |
| [Quality Guidelines](./quality-guidelines.md) | Required and forbidden patterns, how to verify |

## Non-negotiables

1. **The helper is the only process that talks DDC/CI.** The hook DLL and the
   GUI must go through `server::Server` or the pipe — never open a monitor
   handle of their own (`hook/src/pipe.rs`, `app/src/main.rs`).
2. **DDC/CI is slow and lossy.** Every command is paced, retried and verified.
   See [DDC/CI Access](./ddc-ci.md).
3. **Nothing in the library may panic** on a Windows API failure. See
   [Error Handling](./error-handling.md).
4. **The code is Windows-only.** `windows` crate 0.58, no cross-platform
   fallback. Build and verify on Windows: `cargo build --release`.
