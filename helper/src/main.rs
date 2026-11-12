mod ddc;
mod inject;
mod probe;
mod server;
mod slider;

use std::process::ExitCode;

fn usage() -> ExitCode {
    eprintln!("usage:");
    eprintln!("  brighttray-helper list");
    eprintln!("  brighttray-helper set <index> <percent 0-100>");
    eprintln!("  brighttray-helper serve          (pipe server, also keeps the shell hook loaded)");
    eprintln!("  brighttray-helper inject [--early] [dll]");
    eprintln!("                                   (patch ShellHost's brightness setting)");
    eprintln!(
        "    --early                        restart ShellHost suspended, inject before it boots"
    );
    eprintln!("  brighttray-helper probe          (query the brightness setting, no patching)");
    eprintln!("  brighttray-helper watch          (print Windows brightness slider changes)");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();

    if args.first().map(String::as_str) == Some("inject") {
        let mut early = false;
        let mut dll_argument = None;
        for argument in &args[1..] {
            if argument == "--early" {
                early = true;
            } else {
                dll_argument = Some(argument.clone());
            }
        }
        let dll = match dll_argument {
            Some(path) => std::path::PathBuf::from(path),
            None => match inject::default_dll_path() {
                Ok(path) => path,
                Err(e) => {
                    eprintln!("{e}");
                    return ExitCode::FAILURE;
                }
            },
        };
        if early {
            return match inject::inject_early(&dll) {
                Ok(pid) => {
                    println!("started and hooked ShellHost pid {pid}");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("early inject failed: {e}");
                    ExitCode::FAILURE
                }
            };
        }
        return match inject::inject_all(&dll) {
            Ok(0) => {
                println!("brighttray hook is already loaded in ShellHost");
                ExitCode::SUCCESS
            }
            Ok(n) => {
                println!("injected into {n} ShellHost process(es)");
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("inject failed: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if args.first().map(String::as_str) == Some("probe") {
        return match probe::probe() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("probe failed: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if args.first().map(String::as_str) == Some("serve") {
        // The helper is the long-lived process, so it is also what keeps the hook present
        // across ShellHost restarts. Set BRIGHTTRAY_NO_HOOK=1 to disable that.
        if std::env::var_os("BRIGHTTRAY_NO_HOOK").is_none() {
            match inject::default_dll_path() {
                Ok(dll) => {
                    inject::watch_and_inject(dll, |event| eprintln!("hook watcher: {event}"))
                }
                Err(e) => eprintln!("hook watcher: {e}"),
            }
        }
        return match server::serve() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("pipe server failed: {e}");
                ExitCode::FAILURE
            }
        };
    }

    if args.first().map(String::as_str) == Some("watch") {
        let (tx, rx) = std::sync::mpsc::channel();
        let tx = std::sync::Mutex::new(tx);
        let _sub = match slider::subscribe(move |p| {
            let _ = tx.lock().unwrap().send(p);
        }) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("cannot subscribe to the brightness slider: {e}");
                return ExitCode::FAILURE;
            }
        };
        for v in rx {
            println!("slider -> {v}%");
        }
        return ExitCode::SUCCESS;
    }

    let monitors = ddc::enumerate();

    match args.first().map(String::as_str) {
        Some("list") | None => {
            if monitors.is_empty() {
                println!("no physical monitors found");
            }
            for (i, m) in monitors.iter().enumerate() {
                match m.brightness() {
                    Some(b) => println!("[{i}] {}: {}/{}", m.description, b.current, b.max),
                    None => println!("[{i}] {}: DDC/CI brightness not available", m.description),
                }
            }
            ExitCode::SUCCESS
        }
        Some("set") => {
            let (Some(index), Some(percent)) = (
                args.get(1).and_then(|s| s.parse::<usize>().ok()),
                args.get(2).and_then(|s| s.parse::<u32>().ok()),
            ) else {
                return usage();
            };
            let Some(m) = monitors.get(index) else {
                eprintln!("no monitor with index {index}");
                return ExitCode::FAILURE;
            };
            if m.set_brightness_percent(percent) {
                println!("[{index}] {} -> {}%", m.description, percent.min(100));
                ExitCode::SUCCESS
            } else {
                eprintln!("[{index}] {}: failed to set brightness", m.description);
                ExitCode::FAILURE
            }
        }
        _ => usage(),
    }
}
