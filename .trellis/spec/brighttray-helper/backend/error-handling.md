# Error Handling

## Two error styles, chosen by what the caller can do with it

| Situation | Type | Examples |
|-----------|------|----------|
| The caller can branch on the OS error kind | `std::io::Result` | `server::Server::start`, `server::serve`, `config` I/O |
| The failure is only worth describing | `Result<_, String>` | `inject::*`, `probe::probe`, `autostart::set_enabled` |
| The monitor simply did not answer | `bool` / `Option` | `ddc::Monitor::brightness`, `set_brightness_percent` |

The third row matters: **a monitor that does not support DDC/CI is not an
error**. It is a capability, reported as `None` / `false`, and the callers
decide whether to warn the user (`app/src/main.rs`) or skip it
(`server.rs::probe`).

`Server::start` returns `io::Result` precisely so the app can test
`e.kind() == std::io::ErrorKind::AddrInUse` and show a dedicated message.

## The CLI

`helper/src/main.rs` returns `ExitCode` from `main`, never `()`:

| Exit code | Meaning |
|-----------|---------|
| `0` | success (`ExitCode::SUCCESS`) |
| `1` | the command failed (`ExitCode::FAILURE`) |
| `2` | the arguments were wrong (`usage()`) |

`usage()` also covers a flag that is only valid in combination with another one.
`inject --early` ends every running ShellHost, so it needs `--force-restart` as
well; without it the CLI prints the consequence and exits with code 2 without
touching any process. `--force-restart` on its own is a usage error too — it
means nothing without `--early`.

Every failure prints to **stderr** with `eprintln!` and returns
`ExitCode::FAILURE`. Successful output goes to stdout with `println!` — the
CLI is meant to be piped, so keep machine-readable results on stdout and
complaints on stderr.

```rust
Err(e) => {
    eprintln!("inject failed: {e}");
    ExitCode::FAILURE
}
```

The message prefix names the command (`inject failed:`, `probe failed:`,
`pipe server failed:`) so the user can tell which step failed.

## Message style

- Lowercase API name, then the target in parentheses, then the cause:
  `OpenProcess(1234): Access is denied.`
- `inject.rs` adds the likely cause for the classic loader failure:
  `LoadLibraryW returned NULL in pid 1234: ... could not be loaded (wrong
  architecture, missing dependency, or blocked by security software)`.
- Errors are sentences: start with a capital letter only when they start a
  line, end with a period when they are a full sentence.
- Never swallow an error silently in the CLI. In the library, an error may be
  dropped only when there is genuinely nothing to do about it, and then with a
  comment saying why.

## No panics

The library must not panic. Concretely:

- `unwrap()` and `expect()` are not allowed in library code. The crate has
  exactly one `unwrap()` today, on a mutex lock in the CLI's `watch` command
  (`helper/src/main.rs`). Do not add more.
- A poisoned mutex is recovered, not propagated:

  ```rust
  let mut guard = seen.lock().unwrap_or_else(|e| e.into_inner());
  ```

  (`server.rs` uses this in several places.)
- Slicing and indexing that could panic are avoided in favour of `get()` and
  `is_some_and()`. `helper/src/main.rs` parses an index with
  `args.get(1).and_then(|s| s.parse::<usize>().ok())` and then looks the
  monitor up with `monitors.get(index)`.
- `unwrap_or` / `unwrap_or_else` / `unwrap_or_default` are fine for defaults.

## Errors from the watcher

`inject::watch_and_inject` takes a `report` callback instead of returning an
error, because it runs forever. It reports a `WatchEvent`, and it reports only
when the event *changes*:

```rust
if last.as_ref() != Some(&event) {
    report(&event);
    last = Some(event);
}
```

So a missing DLL or a locked-down shell produces one line, not one line per
poll. The CLI prints them as `hook watcher: {event}`.
