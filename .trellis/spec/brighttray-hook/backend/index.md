# BrightTray Hook — Backend Guidelines

`brighttray-hook` builds `brighttray_hook.dll`, a `cdylib` that is injected
into `ShellHost.exe` (the Windows 11 24H2+ Quick Settings host). It patches
two shared COM vtables so that the native brightness slider appears on a
desktop that has only external monitors, and forwards the slider value to the
helper over a named pipe.

Everything here runs **inside another process**. A bug does not show up as a
crash of BrightTray — it shows up as a broken ShellHost.

## Guidelines Index

| Guide | What it covers |
|-------|----------------|
| [Directory Structure](./directory-structure.md) | The two files, how the DLL is built and shipped |
| [Hooking Contract](./hooking-contract.md) | The two vtables, slot numbers, validation, fail-safe rules |
| [Pipe Client](./pipe-client.md) | Talking to the helper from inside ShellHost |
| [Error Handling](./error-handling.md) | HRESULT conventions, `catch_unwind`, falling back to the original |
| [Logging Guidelines](./logging-guidelines.md) | `%TEMP%\brighttray-hook.log`, format, de-duplication |
| [Quality Guidelines](./quality-guidelines.md) | Required and forbidden patterns, how to verify |

## Non-negotiables

1. **The DLL never panics.** A panic must not unwind into ShellHost. Every
   hooked entry point catches it and returns `E_FAIL`.
2. **Every patch is validated before it is written.** If a slot address is not
   inside the expected module, or the object does not identify itself as the
   brightness setting, nothing is patched. Failing safe is the whole design.
3. **The DLL never talks DDC/CI.** The helper is the only process that talks
   to the monitor; the DLL only asks the helper for a value and hands it a new
   one.
4. **Nothing blocking runs on ShellHost's UI thread.** The vtable hooks are
   called from the shell's own threads.
