//! The `abacus` binary: parse flags, install signal handlers, run the daemon loop.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};

use abacus_daemon::daemon::{daemon_run_with, DaemonConfig};
use abacus_daemon::registry::DEFAULT_MAX_INTERLOCKS;

const DEFAULT_SOCKET_PATH: &str = "/run/abacus-rts/abacus.sock";

static STOP: AtomicBool = AtomicBool::new(false);

extern "C" fn on_stop_signal(_signal: libc::c_int) {
    STOP.store(true, Ordering::Release);
}

fn main() {
    let config = match parse_args(std::env::args().skip(1)) {
        Ok(Some(config)) => config,
        Ok(None) => return,
        Err(message) => {
            eprintln!("abacus: {message}");
            eprintln!("{}", usage());
            std::process::exit(2);
        }
    };

    if let Err(errno) = install_stop_handlers() {
        eprintln!("abacus: fatal: sigaction failed, errno={errno}");
        std::process::exit(1);
    }

    // Lock all current and future pages to prevent cold-page faults in the hot loop.
    // Non-fatal: the daemon runs without it, just with possible jitter from page faults.
    // SAFETY: mlockall is a process-wide memory-locking syscall with no pointer arguments.
    let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if rc != 0 {
        let errno = std::io::Error::last_os_error().raw_os_error().unwrap_or(0);
        eprintln!(
            "abacus: warning: mlockall failed, errno={errno} (page faults possible in hot loop)"
        );
    }

    if let Err(e) = daemon_run_with(&config, &STOP) {
        eprintln!("abacus: fatal: {e}");
        std::process::exit(1);
    }
    eprintln!("abacus: stopped");
}

fn install_stop_handlers() -> Result<(), i32> {
    for signal in [libc::SIGTERM, libc::SIGINT] {
        let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
        action.sa_sigaction = on_stop_signal as extern "C" fn(libc::c_int) as usize;
        unsafe { libc::sigemptyset(&mut action.sa_mask) };
        // No SA_RESTART: ppoll must return EINTR so the loop sees the flag at once.
        action.sa_flags = 0;
        let rc = unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) };
        if rc != 0 {
            return Err(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
    }
    Ok(())
}

fn usage() -> String {
    format!(
        "usage: abacus [--socket-path PATH] [--socket-mode OCTAL] [--socket-group NAME]\n\
         \x20             [--max-interlocks N] [--help] [--version]\n\
         \n\
         \x20 --socket-path PATH     UDS path to listen on (default {DEFAULT_SOCKET_PATH})\n\
         \x20 --socket-mode OCTAL    permission bits for the socket file (default 0660)\n\
         \x20 --socket-group NAME    group to chown the socket file to (default: unchanged)\n\
         \x20 --max-interlocks N     registry cap, excluding the clock (default {DEFAULT_MAX_INTERLOCKS})\n\
         \n\
         Flags take either `--flag value` or `--flag=value`."
    )
}

/// Parse the command line. `Ok(None)` when `--help` or `--version` handled the run.
fn parse_args<I: Iterator<Item = String>>(args: I) -> Result<Option<DaemonConfig>, String> {
    let mut config = DaemonConfig::new(PathBuf::from(DEFAULT_SOCKET_PATH));
    let mut iter = args.peekable();
    while let Some(arg) = iter.next() {
        let (flag, inline_value) = match arg.split_once('=') {
            Some((f, v)) => (f.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        match flag.as_str() {
            "--help" | "-h" => {
                println!("{}", usage());
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("abacus {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--socket-path" | "--socket-mode" | "--socket-group" | "--max-interlocks" => {
                let value = match inline_value {
                    Some(v) => v,
                    None => iter
                        .next()
                        .ok_or_else(|| format!("{flag} requires a value"))?,
                };
                match flag.as_str() {
                    "--socket-path" => config.socket_path = PathBuf::from(value),
                    "--socket-mode" => {
                        config.socket_mode = u32::from_str_radix(value.trim_start_matches("0o"), 8)
                            .map_err(|_| {
                                format!("--socket-mode expects octal bits, got {value}")
                            })?;
                    }
                    "--socket-group" => config.socket_group = Some(value),
                    "--max-interlocks" => {
                        config.max_interlocks = value.parse::<usize>().map_err(|_| {
                            format!("--max-interlocks expects an integer, got {value}")
                        })?;
                    }
                    _ => unreachable!(),
                }
            }
            other => return Err(format!("unknown argument: {other}")),
        }
    }
    Ok(Some(config))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Option<DaemonConfig>, String> {
        parse_args(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults() {
        let c = parse(&[]).unwrap().unwrap();
        assert_eq!(c.socket_path, PathBuf::from(DEFAULT_SOCKET_PATH));
        assert_eq!(c.socket_mode, 0o660);
        assert_eq!(c.socket_group, None);
        assert_eq!(c.max_interlocks, DEFAULT_MAX_INTERLOCKS);
    }

    #[test]
    fn both_flag_forms() {
        let a = parse(&["--socket-path=/tmp/a.sock", "--max-interlocks=12"])
            .unwrap()
            .unwrap();
        let b = parse(&["--socket-path", "/tmp/a.sock", "--max-interlocks", "12"])
            .unwrap()
            .unwrap();
        assert_eq!(a.socket_path, b.socket_path);
        assert_eq!(a.max_interlocks, 12);
        assert_eq!(b.max_interlocks, 12);
    }

    #[test]
    fn socket_mode_is_octal() {
        let c = parse(&["--socket-mode", "0600"]).unwrap().unwrap();
        assert_eq!(c.socket_mode, 0o600);
        assert!(parse(&["--socket-mode", "9"]).is_err());
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(parse(&["--help"]).unwrap().is_none());
        assert!(parse(&["--version"]).unwrap().is_none());
    }

    #[test]
    fn unknown_and_missing_values_are_errors() {
        assert!(parse(&["--bogus"]).is_err());
        assert!(parse(&["--socket-path"]).is_err());
    }
}
