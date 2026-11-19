//! "Uninstall BrightTray…" in the tray menu: one confirmation, the three things a running
//! process can remove, and an honest report about what survived.
//!
//! A running image cannot delete itself, so `BrightTray.exe` is not among the removals: the
//! report names the folder it is in and the user deletes it. The extracted hook DLL cannot be
//! deleted while `ShellHost` has it mapped either — Windows denies deleting a loaded image —
//! and that is reported with the remedy (sign out or restart) instead of being retried.
//!
//! The removal list is [`targets`], a pure function of the `%APPDATA%\BrightTray` root, so what
//! an uninstall would delete is testable without a modal dialog or a registry write.

use std::path::{Path, PathBuf};

use brighttray::inject::HOOK_DLL;
use windows::core::HSTRING;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    MessageBoxW, IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_OK, MB_SETFOREGROUND, MB_YESNO,
    MESSAGEBOX_RESULT, MESSAGEBOX_STYLE,
};

use crate::autostart;
use crate::config::FILE as CONFIG_FILE;
use crate::hookdll::STALE_PREFIX;
use crate::i18n::tr;

const CONFIRM_ZH: &str = "\
要卸载 BrightTray 吗？

将删除：
· 开机自启动项（注册表 Run 下的 BrightTray）
· 设置文件 config.ini（保存的显示器选择）
· 释放出的 hook DLL（brighttray_hook.dll 及 brighttray_hook.old-*.dll）

BrightTray.exe 是正在运行的程序自身，无法自动删除，卸载完成后请手动删除它。
选择“否”不做任何修改，BrightTray 继续运行。";

const CONFIRM_EN: &str = "\
Uninstall BrightTray?

This deletes:
- the Start with Windows entry (BrightTray under the registry Run key)
- the settings file config.ini (the saved display)
- the extracted hook DLLs (brighttray_hook.dll and brighttray_hook.old-*.dll)

BrightTray.exe cannot delete itself while it runs; delete it by hand afterwards.
Choose No to change nothing and keep BrightTray running.";

const SUCCESS_ZH: &str =
    "BrightTray 已卸载：开机自启动项、config.ini 和释放出的 hook DLL 都已删除。";
const SUCCESS_EN: &str =
    "BrightTray has been uninstalled: the Start with Windows entry, config.ini and the extracted hook DLLs are gone.";

const FAILED_ZH: &str = "卸载没有完全完成，以下内容仍然存在：";
const FAILED_EN: &str = "The uninstall did not finish; these are still there:";

const LOCKED_ZH: &str = "\
如果上面列出的文件里有 hook DLL，它通常仍被 ShellHost 加载，而 Windows 不允许删除正在使用的\
文件。请注销或重启 Windows 卸载它，然后删除 BrightTray.exe 所在的文件夹：";
const LOCKED_EN: &str = "\
If a hook DLL is among them, it is usually still loaded by ShellHost, and Windows won't delete \
a loaded file. Sign out or restart Windows to unload it, then delete the folder that holds \
BrightTray.exe:";

const EXE_ZH: &str = "\
BrightTray.exe 仍在上面这个文件夹里，请手动删除它。hook 已不再加载，无需注销。";
const EXE_EN: &str = "\
BrightTray.exe is still in that folder; delete it by hand. No sign-out is needed: no hook is left loaded.";

/// What a confirmed uninstall removed, or that the user backed out.
pub enum Outcome {
    /// The confirmation was declined or dismissed: nothing was touched and the app keeps
    /// running.
    Cancelled,
    /// The removals ran. `failures` is empty when every one of them succeeded.
    Removed {
        /// One line per file or registry value that survived, `path: reason`.
        failures: Vec<String>,
        /// The folder that still holds `BrightTray.exe`, named in the report.
        folder: PathBuf,
    },
}

/// Everything an uninstall removes under `%APPDATA%\BrightTray`; the folder itself is not one.
///
/// Pure apart from reading the directory for the `.old-` siblings, so a test can pin the list
/// without writing anything. A root that does not exist yields the fixed candidates; removing
/// a file that is not there counts as success, because an uninstall is idempotent.
fn targets(root: &Path) -> Vec<PathBuf> {
    let mut paths = vec![root.join(CONFIG_FILE), root.join(HOOK_DLL)];
    if let Ok(entries) = std::fs::read_dir(root) {
        for entry in entries.flatten() {
            // The same prefix `hookdll::remove_stale` uses, so whatever an upgrade left behind
            // is exactly what an uninstall takes away.
            if entry
                .file_name()
                .to_string_lossy()
                .starts_with(STALE_PREFIX)
                && entry.file_type().is_ok_and(|kind| kind.is_file())
            {
                paths.push(entry.path());
            }
        }
    }
    // Sorted so the removal order and a test's expectations never depend on directory order.
    paths.sort();
    paths
}

/// `%APPDATA%\BrightTray`, the folder `config.rs` and `hookdll.rs` also write to.
fn root() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("BrightTray"))
}

/// The folder `BrightTray.exe` sits in; the report names it so the file can be deleted by hand.
fn install_folder() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .or_else(root)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// One message box with the product name as its caption.
fn dialog(text: &str, style: MESSAGEBOX_STYLE) -> MESSAGEBOX_RESULT {
    let text = HSTRING::from(text);
    let caption = HSTRING::from("BrightTray");
    // MB_SETFOREGROUND is passed by both callers: the app lives in the tray, so this runs while
    // the window may be hidden and the dialog must not open behind whatever the user is doing.
    unsafe { MessageBoxW(HWND::default(), &text, &caption, style) }
}

/// Asks before anything is touched. `MB_DEFBUTTON2` makes No the default, so Enter after an
/// accidental click changes nothing.
fn confirm() -> bool {
    dialog(
        tr(CONFIRM_ZH, CONFIRM_EN),
        MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND,
    ) == IDYES
}

/// Confirms, removes, reports. Returns what happened so the caller can quit on success.
pub fn offer() -> Outcome {
    if !confirm() {
        return Outcome::Cancelled;
    }

    let mut failures = Vec::new();
    // The one correct way to clear the Run value: it knows the value name and treats a missing
    // value as success. Removing it here by hand would duplicate that contract; naming the
    // value in the report is this module's job.
    if let Err(e) = autostart::set_enabled(false) {
        failures.push(format!(
            "{}: {e}",
            tr(
                r"开机自启动项 HKCU\Software\Microsoft\Windows\CurrentVersion\Run\BrightTray",
                r"the Start with Windows value HKCU\Software\Microsoft\Windows\CurrentVersion\Run\BrightTray",
            )
        ));
    }
    match root() {
        Some(root) => {
            for path in targets(&root) {
                match std::fs::remove_file(&path) {
                    // Already gone: exactly what an uninstall wants (a second run, a fresh
                    // install that never extracted the DLL).
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Ok(()) => {}
                    // Anything else — the usual case is a DLL ShellHost still has mapped — is
                    // collected with its path so the report can name it.
                    Err(e) => failures.push(format!("{}: {e}", path.display())),
                }
            }
        }
        None => failures.push(
            tr(
                "无法确定 %APPDATA% 路径，设置和 hook DLL 未删除。",
                "Couldn't determine %APPDATA%, so the settings and hook DLL were not removed.",
            )
            .to_string(),
        ),
    }

    let folder = install_folder();
    let outcome = Outcome::Removed { failures, folder };
    // Reported from the outcome itself, so the dialog and the value the caller gets cannot
    // disagree about what was removed.
    if let Outcome::Removed { failures, folder } = &outcome {
        report(failures, folder);
    }
    outcome
}

/// The result dialog: what survived and what to do about it, or a short confirmation plus the
/// path of the executable that is still there.
fn report(failures: &[String], folder: &Path) {
    let folder = folder.display();
    let text = if failures.is_empty() {
        format!(
            "{}\n\n{}\n\n{folder}",
            tr(SUCCESS_ZH, SUCCESS_EN),
            tr(EXE_ZH, EXE_EN)
        )
    } else {
        format!(
            "{}\n\n{}\n\n{} {folder}",
            tr(FAILED_ZH, FAILED_EN),
            failures.join("\n"),
            tr(LOCKED_ZH, LOCKED_EN)
        )
    };
    let style = if failures.is_empty() {
        MB_OK | MB_SETFOREGROUND
    } else {
        MB_OK | MB_ICONWARNING | MB_SETFOREGROUND
    };
    dialog(&text, style);
}

#[cfg(test)]
mod tests {
    use super::targets;
    use std::path::{Path, PathBuf};

    /// A directory of its own under `%TEMP%`, named with the process id and the test name so
    /// parallel tests never share one.
    fn temp_dir(test: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "brighttray-uninstall-{}-{test}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// File names in the order `targets` returns them, so a test reads as the removal list.
    fn names(paths: &[PathBuf]) -> Vec<String> {
        paths
            .iter()
            .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
            .collect()
    }

    #[test]
    fn a_full_folder_yields_config_hook_and_the_stale_siblings() {
        let dir = temp_dir("full");
        for name in [
            "config.ini",
            "brighttray_hook.dll",
            "brighttray_hook.old-11.dll",
            "brighttray_hook.old-7.dll",
            // Not ours: an uninstall must not take a file BrightTray never wrote.
            "notes.txt",
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }

        let paths = targets(&dir);

        assert_eq!(
            names(&paths),
            vec![
                "brighttray_hook.dll",
                "brighttray_hook.old-11.dll",
                "brighttray_hook.old-7.dll",
                "config.ini",
            ]
        );
        // Repeated calls are identical: the list never depends on directory order.
        assert_eq!(paths, targets(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_without_the_stale_siblings_yields_config_and_hook() {
        let dir = temp_dir("no-stale");
        std::fs::write(dir.join("config.ini"), b"x").unwrap();

        assert_eq!(
            names(&targets(&dir)),
            vec!["brighttray_hook.dll", "config.ini"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_folder_with_only_the_hook_still_yields_the_config_candidate() {
        let dir = temp_dir("hook-only");
        std::fs::write(dir.join("brighttray_hook.dll"), b"x").unwrap();

        assert_eq!(
            names(&targets(&dir)),
            vec!["brighttray_hook.dll", "config.ini"]
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_folder_yields_the_same_candidates() {
        // A fresh install that never extracted the DLL: removal reports not-found as success.
        let dir = temp_dir("missing").join("BrightTray");

        assert_eq!(
            names(&targets(&dir)),
            vec!["brighttray_hook.dll", "config.ini"]
        );
        let _ = std::fs::remove_dir_all(dir.parent().unwrap());
    }

    #[test]
    fn every_target_stays_inside_the_root() {
        let dir = temp_dir("inside");
        std::fs::write(dir.join("brighttray_hook.old-1.dll"), b"x").unwrap();

        for path in targets(&dir) {
            assert!(
                path.starts_with(&dir),
                "{} escaped {}",
                path.display(),
                dir.display()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
        // The candidates of a root that does not exist are joined onto it as well.
        let missing = Path::new("nowhere").join("BrightTray");
        assert!(targets(&missing)
            .iter()
            .all(|path| path.starts_with(&missing)));
    }
}
