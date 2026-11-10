//! `freemicro-rs` subcommands. Hand-rolled argument parsing (no clap).
//!
//! ```text
//! freemicro-rs run [--dry-run] [--interval S] [--seconds S] [--config PATH] [--take-pad] [-v]
//! freemicro-rs daemon run | install [--on-connect] | uninstall | status [--on-connect]
//! freemicro-rs hook                   (Claude Code hook; JSON event on stdin)
//! freemicro-rs keymap pull [OUT] | push [FILE] [--test]
//! freemicro-rs keys --list [--config PATH]   (resolved bindings; never opens the pad)
//! freemicro-rs status [--json]        (agent state only; never opens the pad)
//! freemicro-rs version
//! ```

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use crate::clock::wall;
use crate::config::{config_home, ENV_HOME};
use crate::runtime::daemon::{self, SystemLaunchctl, LABEL, ONCONNECT_LABEL};
use crate::runtime::keymap;
use crate::runtime::run::{run, RunOptions};
use crate::state::StateStore;

pub const USAGE: &str = "usage:
  freemicro-rs run [--dry-run] [--interval S] [--seconds S] [--config PATH] [--take-pad] [-v]
  freemicro-rs daemon run | install [--on-connect] | uninstall | status [--on-connect]
  freemicro-rs hook
  freemicro-rs keymap pull [OUT] | push [FILE] [--test]
  freemicro-rs keys --list [--config PATH]
  freemicro-rs status [--json]
  freemicro-rs version";

#[derive(Debug, Clone, PartialEq)]
pub enum DaemonCommand {
    Run,
    Install { on_connect: bool },
    Uninstall,
    Status { on_connect: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Run {
        dry_run: bool,
        interval: f64,
        seconds: f64,
        config: Option<PathBuf>,
        take_pad: bool,
        verbose: bool,
    },
    Daemon(DaemonCommand),
    Hook,
    KeymapPull {
        out: PathBuf,
    },
    KeymapPush {
        source: Option<PathBuf>,
        test: bool,
    },
    KeysList {
        config: Option<PathBuf>,
    },
    Status {
        json: bool,
    },
    Version,
    Help,
}

fn number(flag: &str, value: Option<&String>) -> Result<f64, String> {
    value
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| format!("{flag} needs a number"))
}

/// Split `rest` into positionals and checked flags.
fn split<'a>(rest: &'a [String], flags: &[&str]) -> Result<(Vec<&'a str>, Vec<&'a str>), String> {
    let mut positional = Vec::new();
    let mut seen = Vec::new();
    for arg in rest {
        if arg.starts_with('-') {
            if !flags.contains(&arg.as_str()) {
                return Err(format!("unknown option {arg}"));
            }
            seen.push(arg.as_str());
        } else {
            positional.push(arg.as_str());
        }
    }
    Ok((positional, seen))
}

/// Parse argv (without the program name).
pub fn parse_args(args: &[String]) -> Result<Command, String> {
    let Some(name) = args.first() else {
        return Ok(Command::Help);
    };
    let rest = &args[1..];
    match name.as_str() {
        "help" | "-h" | "--help" => Ok(Command::Help),
        "version" | "--version" => Ok(Command::Version),
        "hook" => Ok(Command::Hook),
        "run" => {
            let (mut dry_run, mut take_pad, mut verbose) = (false, false, false);
            let (mut interval, mut seconds, mut config) = (1.0, 0.0, None);
            let mut it = rest.iter();
            while let Some(arg) = it.next() {
                match arg.as_str() {
                    "--dry-run" => dry_run = true,
                    "--take-pad" => take_pad = true,
                    "-v" | "--verbose" => verbose = true,
                    "--no-restart" => {}
                    "--interval" => interval = number(arg, it.next())?,
                    "--seconds" => seconds = number(arg, it.next())?,
                    "--config" => {
                        config = Some(PathBuf::from(it.next().ok_or("--config needs a path")?))
                    }
                    other => return Err(format!("unknown option {other}")),
                }
            }
            if interval <= 0.0 {
                return Err("--interval must be more than 0".into());
            }
            Ok(Command::Run {
                dry_run,
                interval,
                seconds,
                config,
                take_pad,
                verbose,
            })
        }
        "daemon" => {
            let (positional, flags) = split(rest, &["--on-connect", "--no-restart"])?;
            let on_connect = flags.contains(&"--on-connect");
            let command = match positional.as_slice() {
                ["run"] => DaemonCommand::Run,
                ["install"] => DaemonCommand::Install { on_connect },
                ["uninstall"] => DaemonCommand::Uninstall,
                [] | ["status"] => DaemonCommand::Status { on_connect },
                other => return Err(format!("unknown daemon action {}", other.join(" "))),
            };
            Ok(Command::Daemon(command))
        }
        "keymap" => {
            let (positional, flags) = split(rest, &["--test"])?;
            match positional.as_slice() {
                ["pull"] => Ok(Command::KeymapPull {
                    out: PathBuf::from("keymap.json"),
                }),
                ["pull", out] => Ok(Command::KeymapPull {
                    out: PathBuf::from(out),
                }),
                ["push", rest @ ..] if rest.len() <= 1 => Ok(Command::KeymapPush {
                    source: rest.first().map(PathBuf::from),
                    test: flags.contains(&"--test"),
                }),
                _ => Err("keymap needs: pull [OUT] | push [FILE] [--test]".into()),
            }
        }
        "keys" => match rest {
            [list] if list == "--list" => Ok(Command::KeysList { config: None }),
            [list, flag, path] | [flag, path, list] if list == "--list" && flag == "--config" => {
                Ok(Command::KeysList {
                    config: Some(PathBuf::from(path)),
                })
            }
            _ => Err("keys needs: --list [--config PATH] (use `run` to bridge keys)".into()),
        },
        "status" => {
            let (positional, flags) = split(rest, &["--json"])?;
            if !positional.is_empty() {
                return Err(format!("unexpected argument {}", positional[0]));
            }
            Ok(Command::Status {
                json: flags.contains(&"--json"),
            })
        }
        other => Err(format!("unknown command {other:?}\n{USAGE}")),
    }
}

fn user_home() -> PathBuf {
    PathBuf::from(std::env::var_os("HOME").unwrap_or_default())
}

/// `~/x` -> `$HOME/x`, as Python's `expanduser`.
fn expand(path: &Path) -> PathBuf {
    match path.strip_prefix("~") {
        Ok(rest) => user_home().join(rest),
        Err(_) => path.to_path_buf(),
    }
}

fn run_options(command: &Command, home: PathBuf) -> RunOptions {
    match command {
        Command::Run {
            dry_run,
            interval,
            seconds,
            config,
            take_pad,
            verbose,
        } => RunOptions {
            home,
            dry_run: *dry_run,
            interval: *interval,
            seconds: *seconds,
            config: config.as_deref().map(expand),
            take_pad: *take_pad,
            verbose: *verbose,
            headless: false,
        },
        _ => RunOptions {
            home,
            dry_run: false,
            interval: 1.0,
            seconds: 0.0,
            config: None,
            take_pad: false,
            verbose: false,
            headless: true,
        },
    }
}

fn exit_of(result: anyhow::Result<i32>) -> i32 {
    result.unwrap_or_else(|err| {
        eprintln!("freemicro-rs: {err}");
        1
    })
}

fn daemon_install(home: &Path, on_connect: bool) -> i32 {
    let user = user_home();
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(err) => {
            eprintln!("Not installing: cannot find this binary ({err})");
            return 1;
        }
    };
    let argv = daemon::daemon_argv(&exe);
    let env_home = std::env::var(ENV_HOME).ok();
    let (label, plist) = if on_connect {
        let plist = daemon::build_onconnect_plist(&argv, home, &user, env_home.as_deref());
        (ONCONNECT_LABEL, plist)
    } else {
        let plist = daemon::build_plist(&argv, home, &user, env_home.as_deref());
        (LABEL, plist)
    };
    if let Some(dir) = daemon::log_path(home).parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let reinstall = daemon::is_installed(&user, label);
    if let Err(err) = daemon::install(&mut SystemLaunchctl, &user, label, &plist) {
        eprintln!("Not installing: {err}");
        return 1;
    }
    let what = if on_connect {
        "launch-on-connect agent"
    } else {
        "LaunchAgent"
    };
    let verb = if reinstall {
        "Reinstalled"
    } else {
        "Installed"
    };
    println!("{verb} the FreeMicro {what}.");
    println!("  plist:   {}", daemon::plist_path(&user, label).display());
    println!("  command: {}", argv.join(" "));
    println!("  log:     {}", daemon::log_path(home).display());
    if on_connect {
        println!(
            "  matches: VID 0x303A / PID 0x8360, 0x8297, 0x8298 (IOHIDDevice and IOUSBHostDevice)"
        );
    }
    0
}

fn daemon_uninstall() -> i32 {
    let user = user_home();
    let mut any = false;
    let mut failed = false;
    for (name, label) in [
        ("login daemon", LABEL),
        ("on-connect agent", ONCONNECT_LABEL),
    ] {
        match daemon::uninstall(&mut SystemLaunchctl, &user, label) {
            Ok(existed) => {
                if existed {
                    any = true;
                    println!(
                        "Stopped and removed {}",
                        daemon::plist_path(&user, label).display()
                    );
                }
            }
            Err(err) => {
                failed = true;
                eprintln!("Uninstall of the {name} failed: {err}");
                eprintln!("Try:  launchctl bootout {}", daemon::service_target(label));
            }
        }
    }
    if failed {
        return 1;
    }
    if any {
        println!("The pad is free for `freemicro-rs run` again.");
    } else {
        println!("Neither launcher was installed. Nothing to do.");
    }
    0
}

fn daemon_status(home: &Path, on_connect: bool) -> i32 {
    let user = user_home();
    let label = if on_connect { ONCONNECT_LABEL } else { LABEL };
    let state = daemon::status(&mut SystemLaunchctl, &user, label);
    let holder = daemon::lock_holder(&daemon::lock_path(home));
    println!("Label:     {label}");
    let installed = state["installed"] == json!(true);
    println!(
        "Plist:     {}{}",
        state["plist"].as_str().unwrap_or(""),
        if installed { "" } else { "   (not installed)" }
    );
    let pid = state["pid"].as_i64();
    match (pid, state["loaded"] == json!(true)) {
        (Some(pid), _) => println!("Running:   yes, pid {pid}"),
        (None, true) if on_connect => {
            println!("Running:   no - launchd is watching for the pad (the normal resting state)")
        }
        (None, true) => println!("Running:   no - registered with launchd but not up"),
        (None, false) => println!("Running:   no - not registered with launchd"),
    }
    let log = daemon::log_path(home);
    let size = std::fs::metadata(&log).map(|m| m.len()).unwrap_or(0);
    println!("Log:       {}  ({size} bytes)", log.display());
    match holder {
        Some(holder) => println!("Pad held by: {}", daemon::describe_holder(&holder, wall())),
        None => println!("Pad held by: nobody"),
    }
    let healthy = if on_connect {
        installed && state["loaded"] == json!(true)
    } else {
        installed && pid.is_some()
    };
    i32::from(!healthy)
}

fn state_status(home: &Path, json_out: bool) -> i32 {
    let mut store = StateStore::for_home(home);
    let now = wall();
    let (resolved, sessions) = store.resolved_and_sessions(now);
    if json_out {
        let winner = sessions
            .iter()
            .find(|s| s.state == resolved)
            .map_or(Value::Null, |s| s.to_json());
        let report = json!({
            "resolved": resolved.as_str(),
            "sessions": sessions.iter().map(|s| s.to_json()).collect::<Vec<_>>(),
            "winner": winner,
        });
        println!("{report}");
        return 0;
    }
    println!("Resolved state: {}", resolved.as_str());
    if sessions.is_empty() {
        println!("No live sessions.");
    } else {
        println!("Live sessions:");
        for s in &sessions {
            println!(
                "  {:8} {}  ({}s ago)",
                s.state.as_str(),
                s.session_id,
                s.age(now) as i64
            );
        }
    }
    0
}

/// Print the resolved bindings, as Python `freemicro keys --list` does.
fn keys_list(home: &Path, config: Option<&Path>) -> i32 {
    use crate::input::config as padconfig;

    let pad = match padconfig::resolve_path(home, config).and_then(|p| padconfig::load(&p)) {
        Ok(pad) => pad,
        Err(err) => {
            eprintln!("Config error: {}", err.0);
            return 2;
        }
    };
    println!("Pad config: {}\n", pad.origin());
    println!("Bindings");
    let known: Vec<&str> = padconfig::known_inputs().collect();
    let ordered = known
        .iter()
        .copied()
        .filter(|id| pad.bindings.contains_key(*id))
        .chain(
            pad.bindings
                .keys()
                .map(String::as_str)
                .filter(|id| !known.contains(id)),
        );
    for id in ordered {
        let action = &pad.bindings[id];
        println!("  {id:9} {:16} {}", action.label, action.describe());
    }
    let unbound: Vec<&str> = known
        .into_iter()
        .filter(|id| !pad.bindings.contains_key(*id))
        .collect();
    if !unbound.is_empty() {
        println!("\n  unbound: {}", unbound.join(", "));
    }
    for warning in &pad.warnings {
        println!("\n  warning: {warning}");
    }
    0
}

/// Run a parsed command; exit code.
pub fn dispatch(command: Command) -> i32 {
    let home = config_home();
    let mut say = |line: &str| println!("{line}");
    match &command {
        Command::Help => {
            println!("{USAGE}");
            0
        }
        Command::Version => {
            println!("freemicro-rs {}", env!("CARGO_PKG_VERSION"));
            0
        }
        Command::Hook => crate::state::hooks::run_hook_stdin(&home),
        Command::Run { .. } | Command::Daemon(DaemonCommand::Run) => {
            exit_of(run(run_options(&command, home)))
        }
        Command::Daemon(DaemonCommand::Install { on_connect }) => {
            daemon_install(&home, *on_connect)
        }
        Command::Daemon(DaemonCommand::Uninstall) => daemon_uninstall(),
        Command::Daemon(DaemonCommand::Status { on_connect }) => daemon_status(&home, *on_connect),
        Command::KeymapPull { out } => keymap::pull(&home, &user_home(), &expand(out), &mut say),
        Command::KeymapPush { source, test } => {
            let source = source.as_deref().map(expand);
            keymap::push(&home, &user_home(), source.as_deref(), *test, &mut say)
        }
        Command::KeysList { config } => keys_list(&home, config.as_deref()),
        Command::Status { json } => state_status(&home, *json),
    }
}

pub fn main(args: &[String]) -> i32 {
    match parse_args(args) {
        Ok(command) => dispatch(command),
        Err(message) => {
            eprintln!("freemicro-rs: {message}");
            2
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(line: &str) -> Result<Command, String> {
        let args: Vec<String> = line.split_whitespace().map(String::from).collect();
        parse_args(&args)
    }

    #[test]
    fn parse_args_reads_every_command() {
        let cases = [
            ("", Command::Help),
            ("version", Command::Version),
            ("hook", Command::Hook),
            (
                "run",
                Command::Run {
                    dry_run: false,
                    interval: 1.0,
                    seconds: 0.0,
                    config: None,
                    take_pad: false,
                    verbose: false,
                },
            ),
            (
                "run --dry-run --interval 0.25 --seconds 5 --config k.json --take-pad -v",
                Command::Run {
                    dry_run: true,
                    interval: 0.25,
                    seconds: 5.0,
                    config: Some("k.json".into()),
                    take_pad: true,
                    verbose: true,
                },
            ),
            (
                "daemon",
                Command::Daemon(DaemonCommand::Status { on_connect: false }),
            ),
            ("daemon run", Command::Daemon(DaemonCommand::Run)),
            (
                "daemon install --on-connect",
                Command::Daemon(DaemonCommand::Install { on_connect: true }),
            ),
            (
                "daemon status --on-connect",
                Command::Daemon(DaemonCommand::Status { on_connect: true }),
            ),
            (
                "daemon uninstall",
                Command::Daemon(DaemonCommand::Uninstall),
            ),
            (
                "keymap pull",
                Command::KeymapPull {
                    out: "keymap.json".into(),
                },
            ),
            (
                "keymap pull out.json",
                Command::KeymapPull {
                    out: "out.json".into(),
                },
            ),
            (
                "keymap push --test",
                Command::KeymapPush {
                    source: None,
                    test: true,
                },
            ),
            (
                "keymap push edited.json",
                Command::KeymapPush {
                    source: Some("edited.json".into()),
                    test: false,
                },
            ),
            ("status --json", Command::Status { json: true }),
            ("keys --list", Command::KeysList { config: None }),
            (
                "keys --config k.json --list",
                Command::KeysList {
                    config: Some("k.json".into()),
                },
            ),
        ];
        for (line, expected) in cases {
            assert_eq!(parse(line), Ok(expected), "{line:?}");
        }
    }

    #[test]
    fn parse_args_rejects_bad_input() {
        for line in [
            "frobnicate",
            "run --interval",
            "run --interval x",
            "run --interval 0",
            "run --bogus",
            "daemon explode",
            "keymap",
            "keymap push a b",
            "status extra",
            "keys",
            "keys --init",
        ] {
            assert!(parse(line).is_err(), "{line:?}");
        }
    }

    #[test]
    fn daemon_run_is_headless_with_python_defaults() {
        let opts = run_options(&Command::Daemon(DaemonCommand::Run), PathBuf::from("/h"));
        assert!(opts.headless);
        assert_eq!(opts.interval, 1.0);
        assert!(!run_options(&parse("run").unwrap(), PathBuf::from("/h")).headless);
    }
}
