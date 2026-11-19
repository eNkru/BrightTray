# Logging Guidelines

The hook has exactly one log: `%TEMP%\brighttray-hook.log`. It is the only
place where the user can see what the hook did, and the README FAQ points
people at it ("check `%TEMP%\brighttray-hook.log` for
`IQuickAction hook installed`").

## Format

One line per event:

```
[<unix seconds> pid=<pid>] <message>
```

Example:

```
[1762910400 pid=8124] brighttray_hook attached
[1762910400 pid=8124] IQuickAction hook installed: build=26100 vtable=0x7FF6A1B40000 action=0x7FF6A1D2E118 slots=7,8,9,10,12,14,19,20,21,22
[1762910402 pid=8124] QuickSetValue("Value")
```

## Logging must never be able to take down ShellHost

```rust
fn log(message: impl AsRef<str>) {
    let path = std::env::temp_dir().join("brighttray-hook.log");
    let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    else {
        return;
    };
    ...
}
```

If the file cannot be opened, `log()` returns silently. There is no error
path, no panic, no retry. The hook runs inside a system process; a logging
failure must not become a shell failure.

## De-duplicate the noisy events

The shell calls `GetValue` and `SetValue` constantly. `log_property()` writes
each `operation:name` pair only once:

```rust
fn log_property(operation: &str, name: &str) {
    let seen = SEEN_PROPERTIES.get_or_init(|| Mutex::new(HashSet::new()));
    let mut guard = seen.lock().unwrap_or_else(|e| e.into_inner());
    if guard.insert(format!("{operation}:{name}")) {
        log(format!("{operation}(\"{name}\")"));
    }
}
```

The pair includes the operation, so `GetValue("Value")` and
`QuickGetValue("Value")` are logged separately — they come from different
layers and matter for diagnosis.

## Rate-limit the install retries

The install loop runs 30 times, one second apart. Logging every failure would
bury the useful lines, so only attempt 1 and every fifth attempt are logged:

```rust
let report = attempt == 1 || attempt % 5 == 0;
```

Keep that rule when you touch the loop.

## The identifier dump is intentional

```rust
log(format!(
    "IQuickAction hook installed: build={} vtable=0x{:X} action=0x{:X} slots={}",
    windows_build(), vtable as usize, action as usize, slot_list(&patched)
));
```

The vtable and object addresses are what makes a crash report analysable
against the module image. They are process-local addresses, not user data.
The build number and the slot list are the other half: a Windows update that
shifts a table shows up as a changed `build=` / `slots=` line, before any
failure, so the log alone says whether the constants still match.

## No size limit and no rotation

The log is opened in append mode and never truncated or rotated. ShellHost
restarts often (and the watcher re-injects on every restart), so the file
grows for as long as the machine keeps running. There is no cleanup code
anywhere.

## What is not logged

- No monitor names, no serial numbers, no EDID content — the DLL does not
  know them.
- No pipe traffic: the helper owns that side of the story.
- No per-poll messages from the watcher. The watcher only logs an actual
  change:

  ```rust
  log(format!("external brightness change {previous}% -> {percent}%"));
  ```

  The very first measurement is not a change - there was no previous value -
  so it is logged once as `brightness known: N%`.
