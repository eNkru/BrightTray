# Error Handling

The hook has no user interface and no error channel except its log file. Its
job is to **never make ShellHost worse**.

## HRESULT conventions

| Value | When |
|-------|------|
| `S_OK` | the hook handled the call |
| `E_POINTER` | the `out` parameter was null |
| `E_UNEXPECTED` | the state is not installed yet (the shell called a hook before `install()` finished) |
| `E_FAIL` | a panic was caught, or the hook cannot serve the request |

The original return value of the wrapped function is passed through unchanged
when the hook does not handle the call.

## Every entry point catches panics

A panic unwinding into ShellHost's vtable call would take the shell down.
Each hooked entry point wraps its body:

```rust
match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
    get_value_impl(this, name, out)
})) {
    Ok(hr) => hr,
    Err(_) => {
        log("panic in GetValue");
        E_FAIL
    }
}
```

`AssertUnwindSafe` is required because the closures capture raw pointers.
The `log(...)` call names the function that panicked — "panic in GetValue",
"panic in SetValue", "panic in QuickAction GetValue", "panic in
add_SettingChanged", … Keep that naming when you add a hook.

## Fall back to the original implementation

A hook that does not handle a call must call the original:

```rust
match name_text.as_deref() {
    Some("Value") => box_i32(...),
    ...
    // "Values" and anything added in a future build keep the original behaviour.
    _ => (state.original_get_value)(this, name, out),
}
```

The same applies to the `IQuickAction` hooks for every instance that is not
the brightness one. This is what keeps the other Quick Settings tiles working.

## Install failures are logged, not fatal

`try_install()` and `try_install_quick_actions()` return `Result<(), String>`
with a message that names what failed:

- `"GetSetting -> 0x{:08X}"`
- `"quick action slot {slot} = 0x{pointer:X}, outside module 0x{base:X}..0x{:X}"`
- `"unexpected quick action id {id:?}"`

The install loop logs those messages and retries; it never propagates them
anywhere, because there is nowhere to propagate them to.

## Installing nothing is a valid outcome

If every check fails — a Windows update changed the layout — the DLL simply
does nothing. The shell behaves as it did before. **This is the designed
outcome, not a bug.** Do not add a fallback that patches "anyway".

## The pipe never fails loudly

`pipe.rs` has no error type: `get_all()` returns `Option<u32>` and `set_all()`
returns `()`. A missing helper is the normal case when BrightTray is closed.

## The `HSTRING` ownership trap

`get_Id` returns a duplicate string that the caller owns
(`WindowsDuplicateString`), so it must be released with `free_hstring`. The
`ISettingItem` install path deliberately does **not** free the id it reads
back — it runs once and the ownership model cannot be verified from the ABI
alone. Do not "fix" that asymmetry without a reason; it is a comment in the
code.
