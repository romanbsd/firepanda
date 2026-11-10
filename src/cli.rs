//! CLI parsing: exit codes (2 for option validation, 0 for --help/--version)
//! and stderr messages are part of the CLI contract.

use crate::browser::{Backend, Options};
use std::time::Duration;

const HELP: &str = "\
firepanda [--backend lightpanda|obscura|auto]
          [--browser /path/to/browser]
          [--lightpanda /path/to/lightpanda | --socket /absolute/cdp.sock]
          [--cdp-url ws://host:port/devtools/browser]
          [--idle-timeout SECONDS]
          [--stealth] [--obey-robots] [--no-private-network]
Managed browser idle timeout: 60 seconds by default; 0 disables it.
MCP over stdio; starts the selected browser lazily unless --socket/--cdp-url is given.
";

/// Strict idle-timeout parse: ASCII digits only, 0..=86400. Rejects "+1", "-1", "1.5", " 1",
/// "1 ", "nan", "86401", and anything that overflows.
fn parse_idle_timeout(value: &str) -> Option<u64> {
    if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let seconds: u64 = value.parse().ok()?;
    (seconds <= 86400).then_some(seconds)
}

pub fn parse() -> Options {
    match parse_args(std::env::args().skip(1).collect()) {
        Ok(Command::Run(options)) => options,
        Ok(Command::Help) => {
            print!("{HELP}");
            std::process::exit(0);
        }
        Ok(Command::Version) => {
            println!("firepanda {}", env!("CARGO_PKG_VERSION"));
            std::process::exit(0);
        }
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
}

enum Command {
    Run(Options),
    Help,
    Version,
}

fn take_value(args: &mut impl Iterator<Item = String>, arg: &str) -> Result<String, String> {
    args.next()
        .ok_or_else(|| format!("Unknown or incomplete option: {arg}"))
}

fn parse_args(args: Vec<String>) -> Result<Command, String> {
    let mut options = Options::default();
    let mut backend: Option<String> = None;
    let mut program_given = false;
    let mut lightpanda_given = false;

    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--help" => return Ok(Command::Help),
            "--version" => return Ok(Command::Version),
            "--backend" => {
                let value = take_value(&mut args, &arg)?;
                if Backend::parse(&value).is_none() && value != "auto" {
                    return Err("--backend must be lightpanda, obscura, or auto".to_string());
                }
                backend = Some(value);
            }
            "--idle-timeout" => {
                let value = take_value(&mut args, &arg)?;
                let Some(seconds) = parse_idle_timeout(&value) else {
                    return Err(
                        "--idle-timeout must be an integer from 0 to 86400 seconds".to_string()
                    );
                };
                options.idle_timeout = Duration::from_secs(seconds);
            }
            "--browser" | "--lightpanda" | "--socket" | "--cdp-url" => {
                let value = take_value(&mut args, &arg)?;
                match arg.as_str() {
                    "--browser" => {
                        options.program.clone_from(&value);
                        program_given = true;
                    }
                    "--lightpanda" => {
                        options.program = value;
                        lightpanda_given = true;
                    }
                    "--socket" => options.socket = value,
                    _ => options.cdp_url = value,
                }
            }
            "--stealth" => options.stealth = true,
            "--obey-robots" => options.obey_robots = true,
            "--no-private-network" => options.allow_private_network = false,
            other => return Err(format!("Unknown or incomplete option: {other}")),
        }
    }

    finish(options, backend.as_deref(), program_given, lightpanda_given).map(Command::Run)
}

fn finish(
    mut options: Options,
    backend: Option<&str>,
    program_given: bool,
    lightpanda_given: bool,
) -> Result<Options, String> {
    if program_given && lightpanda_given {
        return Err("--browser and --lightpanda are mutually exclusive".to_string());
    }
    if !options.socket.is_empty() && !options.cdp_url.is_empty() {
        return Err("--socket and --cdp-url are mutually exclusive".to_string());
    }
    if !options.socket.is_empty()
        && (!options.socket.starts_with('/') || options.socket.len() > 100)
    {
        return Err(
            "--socket must be an absolute Unix socket path of at most 100 bytes".to_string(),
        );
    }
    if !options.cdp_url.is_empty() {
        if program_given || lightpanda_given {
            return Err("--cdp-url cannot be combined with --browser or --lightpanda".to_string());
        }
        if !options.cdp_url.starts_with("ws://") || options.cdp_url.len() > 2048 {
            return Err(
                "--cdp-url must be a ws:// WebSocket URL of at most 2048 bytes".to_string(),
            );
        }
    }

    let selected = backend.unwrap_or("lightpanda");
    options.backend = if selected == "auto" {
        resolve_auto(&options, program_given || lightpanda_given)
    } else {
        Backend::parse(selected).expect("validated above")
    };
    if lightpanda_given && options.backend != Backend::Lightpanda {
        return Err("--lightpanda selects the Lightpanda backend".to_string());
    }
    if !program_given && !lightpanda_given {
        options.program = options.backend.default_program().to_string();
    }

    let obscura = options.backend == Backend::Obscura;
    if !options.cdp_url.is_empty()
        && (options.stealth || options.obey_robots || !options.allow_private_network)
    {
        return Err("Obscura launch flags cannot be combined with --cdp-url".to_string());
    }
    if options.stealth && !obscura {
        return Err("--stealth requires --backend obscura".to_string());
    }
    if options.obey_robots && !obscura {
        return Err("--obey-robots requires --backend obscura".to_string());
    }
    if !options.allow_private_network && !obscura {
        return Err("--no-private-network requires --backend obscura".to_string());
    }
    Ok(options)
}

/// `auto` prefers an explicitly named executable, then a Lightpanda on PATH,
/// then an Obscura. The default stays Lightpanda when neither is installed so
/// the spawn error keeps naming the expected engine.
fn resolve_auto(options: &Options, program_given: bool) -> Backend {
    if program_given {
        let name = std::path::Path::new(&options.program)
            .file_name()
            .map(|name| name.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        if name.contains("obscura") {
            return Backend::Obscura;
        }
        return Backend::Lightpanda;
    }
    if command_exists("lightpanda") {
        Backend::Lightpanda
    } else if command_exists("obscura") {
        Backend::Obscura
    } else {
        Backend::Lightpanda
    }
}

fn command_exists(name: &str) -> bool {
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| {
            std::fs::metadata(directory.join(name)).is_ok_and(|metadata| metadata.is_file())
        })
    })
}

#[cfg(test)]
mod tests {
    use super::{Backend, Command, parse_args, parse_idle_timeout};

    fn run(args: &[&str]) -> Result<Command, String> {
        parse_args(args.iter().map(ToString::to_string).collect())
    }

    fn parse_ok(args: &[&str]) -> crate::browser::Options {
        match run(args).expect("valid CLI") {
            Command::Run(options) => options,
            _ => panic!("expected a run command"),
        }
    }

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

    #[test]
    fn lightpanda_remains_the_default() {
        let options = parse_ok(&[]);
        assert_eq!(options.backend, Backend::Lightpanda);
        assert_eq!(options.program, "lightpanda");
        assert!(options.allow_private_network);
    }

    #[test]
    fn backend_selects_its_default_program() {
        let options = parse_ok(&["--backend", "obscura"]);
        assert_eq!(options.backend, Backend::Obscura);
        assert_eq!(options.program, "obscura");
        let options = parse_ok(&["--backend", "obscura", "--browser", "/opt/obscura"]);
        assert_eq!(options.program, "/opt/obscura");
    }

    #[test]
    fn external_endpoints_conflict_with_managed_ones() {
        assert!(
            run(&[
                "--socket",
                "/tmp/cdp.sock",
                "--cdp-url",
                "ws://127.0.0.1:1/"
            ])
            .is_err()
        );
        assert!(run(&["--cdp-url", "ws://127.0.0.1:1/", "--browser", "x"]).is_err());
        assert!(run(&["--cdp-url", "http://127.0.0.1:1/"]).is_err());
        assert!(run(&["--socket", "relative.sock"]).is_err());
        let options = parse_ok(&["--socket", "/tmp/my.sock"]);
        assert_eq!(options.socket, "/tmp/my.sock");
    }

    #[test]
    fn obscura_only_flags_are_rejected_for_lightpanda() {
        for flag in ["--stealth", "--obey-robots", "--no-private-network"] {
            assert!(run(&["--backend", "lightpanda", flag]).is_err(), "{flag}");
            assert!(run(&["--backend", "obscura", flag]).is_ok(), "{flag}");
        }
        let options = parse_ok(&["--backend", "obscura", "--no-private-network"]);
        assert!(!options.allow_private_network);
    }

    #[test]
    fn lightpanda_alias_cannot_select_obscura() {
        assert!(run(&["--backend", "obscura", "--lightpanda", "/x/lightpanda"]).is_err());
        assert!(run(&["--browser", "/x", "--lightpanda", "/x"]).is_err());
        let options = parse_ok(&["--lightpanda", "/x/lightpanda"]);
        assert_eq!(options.backend, Backend::Lightpanda);
        assert_eq!(options.program, "/x/lightpanda");
    }

    #[test]
    fn auto_prefers_the_named_executable() {
        let options = parse_ok(&["--backend", "auto", "--browser", "/opt/obscura"]);
        assert_eq!(options.backend, Backend::Obscura);
        let options = parse_ok(&["--backend", "auto", "--browser", "/opt/lightpanda"]);
        assert_eq!(options.backend, Backend::Lightpanda);
    }

    #[test]
    fn help_and_version_short_circuit() {
        assert!(matches!(run(&["--help"]), Ok(Command::Help)));
        assert!(matches!(run(&["--version"]), Ok(Command::Version)));
        assert!(matches!(
            run(&["--backend", "obscura", "--help"]),
            Ok(Command::Help)
        ));
    }
}
