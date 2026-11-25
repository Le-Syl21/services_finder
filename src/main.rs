//! Services Finder: a small, native Windows services manager with instant
//! search across every column and clear marks for services that need
//! administrator rights.
#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod i18n;
#[cfg(windows)]
mod scm;
#[cfg(windows)]
mod ui;
#[cfg(windows)]
mod util;

#[cfg(windows)]
fn main() {
    let args: Vec<String> = std::env::args().collect();
    // Elevated helper: `services_finder --do <action> <service>` performs one
    // action and returns the Win32 error code as the exit code.
    if args.get(1).map(String::as_str) == Some("--do") {
        let code = match (
            args.get(2).and_then(|a| scm::Action::from_arg(a)),
            args.get(3),
        ) {
            (Some(action), Some(name)) => scm::perform(name, action).err().unwrap_or(0),
            _ => 87, // ERROR_INVALID_PARAMETER
        };
        std::process::exit(code as i32);
    }
    // `services_finder --list`: tab-separated dump of every service with the
    // access the current token has (for scripts and tests).
    if args.get(1).map(String::as_str) == Some("--list") {
        std::process::exit(list());
    }
    std::process::exit(ui::run());
}

#[cfg(windows)]
fn list() -> i32 {
    use std::io::Write;
    use windows_sys::Win32::System::Console::{ATTACH_PARENT_PROCESS, AttachConsole};
    // A GUI-subsystem program has no console; borrow the caller's if any.
    // SAFETY: plain call; failure just means output goes to a redirect.
    unsafe { AttachConsole(ATTACH_PARENT_PROCESS) };
    let services = match scm::enumerate() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("{}", util::error_text(e));
            return e as i32;
        }
    };
    let elevated = util::is_elevated();
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "# elevated={elevated}");
    let _ = writeln!(
        out,
        "name\tdisplay\tstate\tstart\tcan_control\tcan_config\tprotected\taccount\tpath"
    );
    for s in services {
        let _ = writeln!(
            out,
            "{}\t{}\t{}\t{:?}\t{}\t{}\t{}\t{}\t{}",
            s.name,
            s.display,
            s.state,
            s.start,
            s.can_control,
            s.can_config,
            s.protected,
            s.account,
            s.path
        );
    }
    0
}

#[cfg(not(windows))]
fn main() {
    eprintln!("services_finder only runs on Windows.");
    std::process::exit(1);
}
