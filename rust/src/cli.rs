//! CLI parsing. Ported from `src/main.cpp` argv handling: identical exit codes
//! (2 for option validation, 0 for --help/--version) and stderr messages.

use crate::browser::Options;
use std::time::Duration;

const HELP: &str = "\
firepanda [--lightpanda /path/to/lightpanda | --socket /absolute/cdp.sock]
          [--idle-timeout SECONDS]
Managed browser idle timeout: 60 seconds by default; 0 disables it.
MCP over stdio; starts native Lightpanda lazily unless --socket is given.
";

/// Strict idle-timeout parse: ASCII digits only, 0..=86400. Mirrors C++
/// `std::from_chars` into `unsigned` — rejects "+1", "-1", "1.5", " 1",
/// "1 ", "nan", "86401", and anything that overflows.
fn parse_idle_timeout(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds: u64 = value.parse().ok()?;
    (seconds <= 86400).then_some(seconds)
}

pub fn parse() -> Options {
    let mut options = Options::default();
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" => {
                print!("{HELP}");
                std::process::exit(0);
            }
            "--version" => {
                println!("firepanda {}", env!("CARGO_PKG_VERSION"));
                std::process::exit(0);
            }
            "--idle-timeout" => {
                let Some(value) = args.next() else {
                    eprintln!("Unknown or incomplete option: {arg}");
                    std::process::exit(2);
                };
                match parse_idle_timeout(&value) {
                    Some(seconds) => options.idle_timeout = Duration::from_secs(seconds),
                    None => {
                        eprintln!("--idle-timeout must be an integer from 0 to 86400 seconds");
                        std::process::exit(2);
                    }
                }
            }
            "--lightpanda" | "--socket" => {
                let Some(value) = args.next() else {
                    eprintln!("Unknown or incomplete option: {arg}");
                    std::process::exit(2);
                };
                if arg == "--lightpanda" {
                    options.lightpanda = value;
                } else {
                    options.socket = value;
                }
            }
            other => {
                eprintln!("Unknown or incomplete option: {other}");
                std::process::exit(2);
            }
        }
    }
    if !options.socket.is_empty()
        && (!options.socket.starts_with('/') || options.socket.len() > 100)
    {
        eprintln!("--socket must be an absolute Unix socket path of at most 100 bytes");
        std::process::exit(2);
    }
    options
}

#[cfg(test)]
mod tests {
    use super::parse_idle_timeout;

    #[test]
    fn idle_timeout_bounds() {
        for bad in [
            "",
            "+1",
            "-1",
            "1.5",
            " 1",
            "1 ",
            "nan",
            "86401",
            "18446744073709551616",
        ] {
            assert!(parse_idle_timeout(bad).is_none(), "{bad}");
        }
        for good in ["0", "1", "60", "86400"] {
            assert!(parse_idle_timeout(good).is_some(), "{good}");
        }
    }
}
