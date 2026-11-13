# Design — P0-01

## Current behaviour

Three call sites reach a `TerminateProcess`:

| Site | Trigger | Scope |
|------|---------|-------|
| `inject.rs` — `inject_early()` | `brighttray-helper inject --early` | every running ShellHost |
| `inject.rs` — `restart_shellhost(pid)` | called by the watcher | one ShellHost |
| `inject.rs` — `watch_and_inject(..., restart_stale = true, …)` | the app | one ShellHost, if it is older than 10 s and the 60 s cooldown has passed |

`inject_early()` then starts a **suspended** ShellHost, injects the hook,
waits for the `Local\BrightTrayHookReady` event (set by the hook once its
vtable is patched) and only then resumes it. That part is deliberate and
stays.

The stale-restart exists for a different reason: Quick Settings caches which
tiles are applicable when it builds its panel, so a hook that arrives late may
never take effect. Restarting the shell makes it ask again.

The reason the restart is unnecessary in practice is recorded in the code
itself (`helper/src/inject.rs`):

> Fallback only. In testing, a normal injection into a running ShellHost is
> enough because the `IQuickAction` hook answers every later query; this path
> exists for builds where Quick Settings caches the brightness capability
> before a late hook could take effect.

and in `docs/quick-settings-hook-research.md` §7:

> 在本机实测中最终工作路径是**普通注入 + QuickSetting vtable patch**，不依赖
> early 注入。

So the normal path is the one that works; the restart is a fallback that is
being paid for as a default.

## Decision 1 — Remove the restart branch from the watcher

`watch_and_inject` loses its `restart_stale: bool` parameter and the whole
stale branch:

```rust
// removed
let stale = restart_stale
    && process_age(pid).is_some_and(|age| age > FRESH_SHELLHOST)
    && last_restart.is_none_or(|t| t.elapsed() > RESTART_COOLDOWN);
if stale { … restart_shellhost(pid) … }
```

The parameter goes away rather than defaulting to `false`, because a boolean
parameter that silently decides whether the user's shell dies is exactly the
kind of thing that gets flipped back by accident. The signature becomes:

```rust
pub fn watch_and_inject(
    dll: PathBuf,
    report: impl Fn(&WatchEvent) + Send + 'static,
)
```

Consequences: `FRESH_SHELLHOST`, `RESTART_COOLDOWN`, `process_age()` and the
`last_restart` local all become dead and are removed. `restart_shellhost()`
stays, because the explicit user action needs it.

## Decision 2 — The explicit restart is a public, clearly named function

`helper/src/inject.rs` gains:

```rust
/// Terminates every running ShellHost. `sihost` starts a new one within about a second.
///
/// This is destructive and user-visible: the Start menu and Quick Settings
/// disappear until the shell is back. Only call it after the user asked for it.
pub fn restart_shellhost() -> Result<(), String>
```

It finds the pids with `shellhost_pids()` and terminates each one. The watcher
then sees a new pid (the old one is gone, `hooked.retain(...)` drops it) and
injects into the new process — no new code needed for the recovery.

## Decision 3 — The app's tray menu is the consent

The tray menu gains one item between "Open" and "Quit":

```rust
let restart_item = MenuItem::new(
    tr(
        "重启 ShellHost（开始菜单会短暂消失）",
        "Restart ShellHost (the Start menu disappears briefly)",
    ),
    true,
    None,
);
```

The label carries the consequence, because the tray menu has no confirmation
dialog and adding one is a larger UI change than this task should make.

Clicking it:

1. If the watcher is not running yet (the user has not applied a display, or
   the hook DLL could not be extracted), the app shows the notice "请先应用一个显示器" /
   "Apply a display first" and does nothing.
2. Otherwise it calls `inject::restart_shellhost()`.
3. On success the status line follows the watcher on its own: it reports the
   shell as gone and then as hooked again. The app does not set the hook state
   itself, because that would race with the watcher's report.
4. On failure the error goes into `App::error` like any other failure.

The app's event loop gains a `UserEvent` for the result, because
`TerminateProcess` on several processes is fast but not free, and the UI
thread should not block on it. A short `std::thread::spawn` that sends back
the result keeps the rule "the event loop thread only does UI work".

**A notice that only lives in the page would be invisible.** The app usually
lives in the tray, so both error paths bring the window forward first
(`App::show_notice()`), the same way `App::show()` does.

**`watcher_started` is only set once the watcher thread exists** — that is,
after `hookdll::extract()` succeeded. Otherwise the tray item would offer a
restart that nothing re-injects into, and a failed extraction would
permanently disable the watcher.

## Decision 4 — The watcher identifies a process, not a pid

A restarted ShellHost may get the pid of the old one. The watcher remembers
which processes have the hook, and a bare pid is not enough to tell "the same
process" from "a new process that reused the number". So the remembered
identity is the pid **and** the process creation time:

```rust
// pid -> creation time of the process that has the hook.
let mut hooked: std::collections::HashMap<u32, Option<u64>> = …
```

The creation time costs one `OpenProcess` + `GetProcessTimes` per ShellHost
per tick, next to the process snapshot the watcher already takes — negligible
compared to that snapshot, which walks every process in the system.

The same identity check also settles the transient error: a ShellHost that is
on its way out cannot be injected into, and failing to inject into a process
that is already exiting is not an error worth showing. `process_gone(pid)`
asks `GetExitCodeProcess` whether the process is still `STILL_ACTIVE`.

## Decision 5 — The CLI needs a second flag

`inject --early` is a destructive operation. The CLI becomes:

```
brighttray-helper inject [--early --force-restart] [dll]
```

* `--early` alone → print the consequence and the hint, exit with code 2
  (`usage()`), terminate nothing.
* `--early --force-restart` → the current behaviour.

The guard lives in the CLI, because that is where the user's intent is
expressed. The library function keeps its name and gains a doc comment that
states the consequence, so a future caller sees it.

The alternative — dropping `--early` altogether — was rejected: the early
path is the only recovery for a shell that caches the capability too early,
and the audit explicitly asks for it to stay behind an explicit flag.

## Decision 6 — Remove the dead `WatchEvent::Restarting`

The variant, its `Display` arm, the `push_state()` arm and the page's
`statusOf()` arm and its `restarting` string all go. The page's `STRINGS`
table keeps both languages in sync — `zh.restarting` and `en.restarting` are
removed together.

`WatchEvent` keeps: `Hooked`, `NoShellHost`, `DllMissing`, `Error`.

## Data flow after the change

```
apply
  └─ inject::watch_and_inject(dll, report)
        ├─ every 500 ms: snapshot the ShellHost pids
        ├─ a pid without the hook → inject into it        (no kill, ever)
        └─ a pid that disappears → drop it from `hooked`

tray menu: "Restart ShellHost"
  └─ inject::restart_shellhost()
        └─ TerminateProcess each ShellHost → sihost starts a new one
              └─ the watcher sees the new pid → injects into it
```

## Compatibility

- The hook is unaffected: it never knew about the restart.
- The pipe protocol is unaffected.
- The CLI keeps every command; `inject` keeps working without any flag
  (a normal injection into a running ShellHost).

## Risks

| Risk | Mitigation |
|------|------------|
| On a build where Quick Settings caches the capability early, the slider no longer appears | The explicit tray action and `inject --early --force-restart` remain; the FAQ must say so |
| The tray item is easy to click by accident | It is labelled with its consequence, and it is not the default path |
| A ShellHost that keeps dying (crash loop) would previously be restarted by the watcher | The watcher only injects; it never resurrects. A crash loop is a different problem and is not this task's business |
