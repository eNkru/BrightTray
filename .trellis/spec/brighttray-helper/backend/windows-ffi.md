# Windows FFI Safety

The crate talks to Windows through the `windows` crate 0.58. Every module that
touches Win32 has `unsafe` blocks; the crate does **not** forbid unsafe code
and must not.

## Enabling an API

Each crate lists the `windows` crate features it needs in its own
`Cargo.toml` (`helper/Cargo.toml` lists `Win32_Devices_Display`,
`Win32_System_Pipes`, `Win32_System_Power`, …). When you need a new API
family, add the feature to the crate's own `windows` dependency — do not add a
second `windows` dependency with a different version, and do not add a
different FFI crate.

The `windows` crate only builds for Windows targets. There is no Linux/macOS
fallback anywhere in this workspace, and no `cfg(target_os)` branches.

## Literal strings

Windows wants UTF-16. Use the crate macros:

```rust
CreateNamedPipeW(w!(r"\\.\pipe\BrightTray"), ...)
GetProcAddress(kernel32, s!("LoadLibraryW"))
```

`s!` is for ANSI (`GetProcAddress` takes an ANSI name), `w!` for UTF-16.

## UTF-16 → `String`

Each module that needs it has its own tiny helper — `ddc.rs`, `inject.rs` and
`autostart.rs` all have a `wide_to_string`:

```rust
fn wide_to_string(buffer: &[u16]) -> String {
    let len = buffer.iter().position(|&c| c == 0).unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..len])
}
```

Copying the helper into a new module is the established pattern; it is three
lines and it avoids a dependency on a string crate the workspace does not have.

## `HSTRING` is a handle, not a pointer to a handle

An `HSTRING` is passed by value as a pointer to its own header. Dereferencing
it as `*const HSTRING` is wrong. The established pattern (in `probe.rs` and
`hook/src/lib.rs`) is to borrow it without taking ownership:

```rust
// `raw` is the HSTRING handle itself, not the address of a handle variable.
let borrowed: std::mem::ManuallyDrop<HSTRING> = std::mem::transmute_copy(&raw);
borrowed.to_string_lossy()
```

When an interface returns an `HSTRING` in an out-parameter, the caller owns it
and must release it with `WindowsDeleteString` (see `free_hstring` in
`hook/src/lib.rs`).

## Handles

- Prefer the RAII wrappers of the `windows` crate (`File::from_raw_handle` for
  pipes, `CloseHandle` for raw handles).
- A raw `HANDLE` that is a pointer must be moved across threads as an integer:

  ```rust
  // HANDLE is a raw pointer; move it across as an integer.
  let raw = first.0 as isize;
  ```

- Every handle that is opened by hand is closed on **all** paths, including
  the error path. `inject.rs::inject_inner` frees the remote allocation and
  closes the thread handle after the closure returns, whether it succeeded or
  not.
- `ddc::Monitor` releases the `PHYSICAL_MONITOR` handle in `Drop`.

## Return codes

Two families, checked differently:

| API style | Check |
|-----------|-------|
| `Get*` with a separate status out-param | compare with `ERROR_SUCCESS` or `0` |
| `BOOL` / `int` return | compare with `TRUE` / `FALSE` / `!= 0` |
| `HRESULT` | `.is_ok()` / `.is_err()`, or `hr.0 as u32` when printing |
| `Result`-returning `windows` API | `.map_err(...)` / `.ok()` |

`GetVCPFeatureAndVCPFeatureReply` returns an `int`, so `ddc.rs` compares it
with `!= 0`. `ConnectNamedPipe` returns a `Result`, so `server.rs` matches on
`Err(e) if e.code() == ERROR_PIPE_CONNECTED.to_hresult()`.

## Error messages name the API and the target

```rust
.map_err(|e| format!("OpenProcess({pid}): {e}"))?
.map_err(|e| format!("CreateRemoteThread({pid}): {e}"))?
```

Always include the API name and the PID / path / index that failed. A bare
"failed" is not debuggable in a log that has no stack trace.

## `transmute` for vtable calls

The hook crate casts a function pointer out of a COM vtable with
`std::mem::transmute`. The helper crate does not do vtable work at all — that
is deliberate. When you see a `transmute` of an address into a function
pointer, the target type must be declared next to the slot constants, with the
slot number in a comment (`hook/src/lib.rs`).
