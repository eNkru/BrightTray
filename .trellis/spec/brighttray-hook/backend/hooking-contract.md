# Hooking Contract

This is the riskiest code in the repository: it rewrites COM vtables inside
`ShellHost.exe`. The rules below exist because the alternative is a broken
Windows shell.

The slot numbers and the reasoning behind them come from
`docs/quick-settings-hook-research.md` (Chinese) — §2 for the `ISettingItem`
vtable, §7 for the `IQuickAction` vtable. That document, not this file, is
the source of truth when a slot number changes.

## Two independent hooks

| Hook | Module | Vtable | Purpose |
|------|--------|--------|---------|
| `ISettingItem` | `SettingsHandlers_PCDisplay.dll` | `SLOT_*` constants | Compatibility layer for the Settings page; **not** what makes the slider appear |
| `IQuickAction` | `QuickActionsDataModel.dll` | `QA_SLOT_*` constants | This is the object ControlCenter binds to; **this** hook makes the brightness tile appear |

Both are installed independently: neither waits for the other, each is retried
on its own, and the install loop returns when both succeed
(`install()` in `lib.rs`).

The `IQuickAction` vtable is **shared by every Quick Settings tile** (WiFi,
Bluetooth, night light …). The `ISettingItem` vtable is shared by every
setting item. So every hook must first check that the instance it was called
on is the brightness one.

The `ISettingItem` hook keeps the original `get_IsEnabled` and
`get_IsApplicable` pointers as well, so it can hand the call back when it has
no measured value.

## Identify the instance on every call

```rust
unsafe fn is_brightness_instance(this: *mut c_void) -> bool {
    ...
    let id_text = hstring_to_string(id).unwrap_or_default();
    free_hstring(id);
    let matches = id_text.eq_ignore_ascii_case(SETTING_ID);
    ...
}
```

The identity is asked **every time** and never cached by address:

> The identity is asked every time instead of being cached by address: a freed
> brightness object's address can be reused by another tile, which would then
> be hijacked.

`SETTING_ID` is `SystemSettings_Display_Brightness`, `QUICK_ACTION_NAME` is
`Microsoft.QuickAction.Brightness`. The comparison is
`eq_ignore_ascii_case`, matching the shell's own case-insensitive ids.

A hook that is not looking at the brightness instance must call the original
implementation:

```rust
if !is_brightness_instance(this) {
    return (state.original_is_applicable)(this, out);
}
```

This is why the original function pointers are saved in `State` /
`QuickState` before the vtable is patched.

## Validate every address before writing it

Before a slot is overwritten, its current pointer must lie inside the module
image:

```rust
fn in_module(pointer: usize, base: usize, size: usize) -> bool {
    pointer >= base && pointer < base + size
}
```

`try_install()` checks every slot it intends to patch, and
`try_install_quick_actions()` checks the provider vtable, the
`GetQuickActionByFriendlyName` slot (13) and all ten `QA_SLOT_*` slots. If any
of them is outside the module, the install **aborts with an error** and
nothing is patched. A Windows update that changes the layout therefore
disables the hook instead of crashing the shell.

## A range check is not enough: dedupe and probe

A table shifted by one slot keeps every pointer inside the module, so the
range check passes and the identity probe can even still answer plausibly.
Both installs therefore run two more checks, in order, after the range check
and before the first `VirtualProtect`:

1. **Dedupe the table** — `check_distinct()` refuses two semantic slots that
   resolve to the same function pointer. A shifted table maps several of our
   slots onto the same neighbour; the error names both slot numbers and the
   shared pointer.
2. **Probe the safe slots** — `probe_slot()` calls a slot once for real and
   refuses an `HRESULT` failure. `SetValue` and the `add`/`remove` handler
   pairs are never probed: a probe would change the value or register a
   handler. On the `IQuickAction` vtable **nothing is probed at all** — see
   the section below.

Everything that can fail happens before the first `VirtualProtect`, so a
refused install leaves the vtable untouched.

## Do not call the `QuickAction` state getters

`probe_slot()` used to probe `get_IsEnabled`, `get_QuickActionStatus`,
`get_QuickActionIsActive` and `GetValue("Value")` on the brightness
`QuickSetting` as part of the install. That call is a **crash**, measured on
Windows 11 26100.9818:

- Every one of those getters, asked of a `QuickSetting` that the install had
  just created, leaves the object with an unfinished async read. About a
  second later `QuickActionsDataModel.dll` dereferences the missing result and
  takes `ShellHost.exe` down with `0xc0000005` at
  `QuickActionsDataModel.dll+0x2FFDE` — the same offset every time.
- It is not a race and not a slow machine: with the probes 5 crashes landed in
  25 s; without them, zero, over the same install path and the same patched
  vtable. Bisecting one probe at a time: `get_IsApplicable` was the only one
  that survived, `get_IsEnabled`, `get_QuickActionStatus`,
  `get_QuickActionIsActive` and `GetValue("Value")` each crashed on their own.
- It is not the patch either: the crash reproduced with the `IQuickAction`
  vtable left completely unpatched, and did not reproduce with the
  `ISettingItem` hook alone (whose probes are still fine and still run).

What pins the table instead: the module-range check, `check_distinct`, and the
two identity calls (`get_Id` → `SystemSettings_Display_Brightness`,
`get_FriendlyName` → `Microsoft.QuickAction.Brightness`). Those read slots 7
and 8, which sit *before* the ten slots we patch, so a table shifted anywhere
in our range moves them too and the identity check fails. The probes never had
that power anyway: they only proved a slot answers, not that it is the right
one.

If you ever want stronger validation, do it on a shell-owned instance *after*
the shell has initialized it — never on the object the install just created. Do
not re-add the probes.

## Get the vtable without a hard-coded address

Both hooks resolve the object through a **stable exported function**:

- `GetSetting(HSTRING id, ISettingItem** out)` from
  `SettingsHandlers_PCDisplay.dll`
- `GetQuickActionsProvider` from `QuickActionsDataModel.dll`, then
  `IQuickActionsProvider::GetQuickActionByFriendlyName` (provider vtable slot
  13)

No RVA, no PDB dependency, no pattern scanning. Do not introduce one.

## Patching

`patch_slots()` does the memory work:

1. Compute the byte range the slots span.
2. `VirtualProtect` every 4 KiB page in that range to `PAGE_READWRITE`,
   remembering the old protection.
3. Write the function pointers with `std::ptr::write_volatile`.
4. Restore the old protection, page by page, in reverse order.

If `VirtualProtect` fails part-way, the pages already unlocked are restored
before returning the error — the vtable is never left half-writable.

## Pin the module before patching

`pin_self()` uses `GetModuleHandleExW(GET_MODULE_HANDLE_EX_FLAG_PIN |
GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, …)` before any vtable can point into
this module. Without the pin, Windows could unload the DLL while a patched
vtable still references its code.

## `DllMain` stays tiny

```rust
if reason == DLL_PROCESS_ATTACH {
    // DllMain must stay tiny: do all resolution and patching on a worker thread after the
    // loader lock has been released.
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(300));
        log("brighttray_hook attached");
        install();
    });
}
```

All module resolution, COM calls and vtable writes happen on the worker
thread, after the 300 ms delay that lets ShellHost finish starting up. Never
call `LoadLibrary`, COM activation or `VirtualProtect` from `DllMain`.

## The install loop

`INSTALL_ATTEMPTS = 30`, one second apart. Each hook is retried until it
succeeds; failures are logged on attempt 1 and every fifth attempt only:

```rust
let report = attempt == 1 || attempt % 5 == 0;
```

The watcher thread starts as soon as one of the two hooks is in, and
`signal_ready()` fires once the `IQuickAction` hook is installed — the early
injector waits for that event before it resumes ShellHost.

## The value the shell sees

`GetValue` answers from `CACHED_PERCENT`, an `AtomicU32`:

| Property | Answer |
|----------|--------|
| `Value` | `CACHED_PERCENT` as `Int32` |
| `MinValue` | `0` |
| `MaxValue` | `100` |
| `QuickActionStatus` | `"{percent}%"` as a string |
| `QuickActionIsActive` | `true` |
| anything else | the original implementation |

**A value is reported only when it was measured.** The cache starts out
*unknown*, not at some default:

```rust
/// The cached brightness has never been measured. A percent is 0..=100, so `u32::MAX` cannot
/// collide with one.
const PERCENT_UNKNOWN: u32 = u32::MAX;
static CACHED_PERCENT: AtomicU32 = AtomicU32::new(PERCENT_UNKNOWN);

fn cached_percent() -> Option<u32> {
    match CACHED_PERCENT.load(Ordering::Relaxed) {
        PERCENT_UNKNOWN => None,
        percent => Some(percent),
    }
}
```

While the value is unknown, **every** override falls back to the original
implementation — `get_IsApplicable`, `get_IsEnabled`, `GetValue`,
`get_QuickActionStatus` and `get_QuickActionIsActive`. The hook then behaves
exactly like the un-hooked system: on a desktop the original `IsApplicable` is
FALSE, so the brightness tile is simply not shown. A missing tile is honest;
an invented "100 %" is not.

The value becomes known when:

- `install()` reads it from the helper for the first time;
- the watcher reads it from the helper;
- the user moves the slider — `SetValue` stores what the user asked for.

The first measurement is logged as `brightness known: N%`; every later change
as `external brightness change X% -> Y%`.

Once a value has been measured, it stays: the last known reading is shown
while the helper is unreachable.

The watcher keeps polling the helper until a value has been measured, even
while no UI handler is registered: an unknown value keeps the tile hidden, and
a hidden tile registers no handler — waiting for one would leave the value
unknown for good.

`SetValue("Value", Int32)` clamps to 0..100, stores it in `CACHED_PERCENT`,
records the time in `LAST_SET_MS` and forwards it to the helper.

## Do not fight the user's finger

`LAST_SET_MS` is the timestamp of the last UI-originated write. The watcher
ignores DDC read-backs for 1.2 s afterwards:

```rust
if monotonic_ms().saturating_sub(LAST_SET_MS.load(Ordering::Relaxed)) < 1200 {
    continue;
}
```

A DDC read takes tens of milliseconds and can still return the *previous*
value while the write is in flight. Without this window, a slow monitor drags
the slider back to the old value.

## UI handler registry

`add_/remove_SettingChanged` (and the `QuickActionPropertyChanged` pair) call
the original implementation first and then record the handler, with an
`AddRef` on both the sender and the handler. `notify_handlers()` copies the
list out of the lock and `AddRef`s again before invoking, so a concurrent
remove cannot free a handler under the caller's feet.

Only the brightness instance's handlers are registered — the `IQuickAction`
variant checks `is_brightness_instance(this)` before recording anything.

## Slot numbers change only with the PDB

If a Windows update changes the layout, the address validation fails and the
hook disables itself. To support the new build, update the `QA_SLOT_*` /
`SLOT_*` constants **and** the tables in
`docs/quick-settings-hook-research.md`, and re-verify with `probe`.

Do not "fix" a failed validation by removing the check.
