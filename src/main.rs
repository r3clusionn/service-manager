//! `tend`: run services from a configuration file, and control them while they run.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tend::config::Config;
use tend::control;
use tend::supervisor::{Request, Response, Stdout, Supervisor};

#[derive(Parser)]
#[command(name = "tend", version, about = "Start services in dependency order, keep them running, stop them in reverse.")]
struct Cli {
    /// The configuration file.
    #[arg(long, short, global = true, default_value = "tend.toml")]
    config: PathBuf,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Start every service and supervise them until Ctrl+C or `tend shutdown`.
    Run {
        /// Do not echo service output to the console (it still goes to the log files).
        #[arg(long, short)]
        quiet: bool,
    },
    /// Check the configuration and print the start and stop order.
    Check,
    /// The state of every service.
    Status,
    /// Start a service (and what it depends on).
    Start { name: String },
    /// Stop a service (and what depends on it).
    Stop { name: String },
    /// Stop a service and start it again.
    Restart { name: String },
    /// Stop everything in reverse dependency order, then exit.
    Shutdown,
    /// The last lines of a service's log.
    Logs {
        name: String,
        #[arg(long, short = 'n', default_value_t = 20)]
        lines: usize,
    },
}

fn load(p: &Path) -> Result<Config, String> {
    Config::load(p).map_err(|e| format!("{}: {e}", p.display()))
}

fn ask(cfg: &Config, r: Request) -> Result<(), String> {
    match control::send(&cfg.control, &cfg.log_dir, r)? {
        Response::Ok { message } => {
            println!("{message}");
            Ok(())
        }
        Response::Error { message } => Err(message),
        Response::Status { services } => {
            println!("{:<16} {:<9} {:>7} {:>8} {:>9}  last exit", "service", "state", "pid", "restarts", "uptime");
            for s in services {
                let state = serde_json::to_string(&s.state).unwrap().trim_matches('"').to_string();
                println!(
                    "{:<16} {:<9} {:>7} {:>8} {:>9}  {}",
                    s.name,
                    state,
                    s.pid.map_or("-".into(), |p| p.to_string()),
                    s.restarts,
                    s.uptime_s.map_or("-".into(), |u| format!("{u}s")),
                    s.last_exit.unwrap_or_default()
                );
            }
            Ok(())
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let cfg = load(&cli.config)?;
    match cli.cmd {
        Cmd::Check => {
            println!("{}: {} service(s)", cli.config.display(), cfg.services.len());
            println!("start order: {}", cfg.order.join(" -> "));
            let rev: Vec<&str> = cfg.order.iter().rev().map(String::as_str).collect();
            println!("stop order:  {}", rev.join(" -> "));
            for name in &cfg.order {
                let s = &cfg.services[name];
                let ready = match (&s.ready.delay_ms, &s.ready.tcp, &s.ready.log) {
                    (Some(ms), _, _) => format!("after {ms} ms"),
                    (_, Some(a), _) => format!("when {a} accepts connections"),
                    (_, _, Some(t)) => format!("when its output contains {t:?}"),
                    _ => "as soon as it starts".into(),
                };
                let deps = if s.depends_on.is_empty() { String::new() } else { format!(", needs {}", s.depends_on.join(", ")) };
                println!("  {name}: {:?}, restart {:?}, ready {ready}{deps}", s.command.join(" "), s.restart);
            }
            Ok(())
        }
        Cmd::Run { quiet } => {
            std::fs::create_dir_all(&cfg.log_dir).map_err(|e| format!("{}: {e}", cfg.log_dir.display()))?;
            let width = cfg.services.keys().map(|n| n.len()).max().unwrap_or(4).max(4);
            let addr = cfg.control.clone();
            let sup = Supervisor::new(cfg, Box::new(Stdout { echo: !quiet, width })).map_err(|e| e.to_string())?;
            let log_dir = sup.log_dir();
            let token = control::new_token(&log_dir).map_err(|e| e.to_string())?;
            control::serve(&addr, token, sup.handle())
                .map_err(|e| format!("cannot listen on {addr}: {e} (is another tend running?)"))?;
            let h = sup.handle();
            ctrlc::set_handler(move || h.shutdown()).map_err(|e| e.to_string())?;
            sup.run();
            let _ = std::fs::remove_file(control::token_path(&log_dir));
            Ok(())
        }
        Cmd::Status => ask(&cfg, Request::Status),
        Cmd::Start { name } => ask(&cfg, Request::Start { name }),
        Cmd::Stop { name } => ask(&cfg, Request::Stop { name }),
        Cmd::Restart { name } => ask(&cfg, Request::Restart { name }),
        Cmd::Shutdown => ask(&cfg, Request::Shutdown),
        Cmd::Logs { name, lines } => {
            if !cfg.services.contains_key(&name) {
                return Err(format!("no service named {name}"));
            }
            let p = cfg.log_dir.join(format!("{name}.log"));
            let text = std::fs::read_to_string(&p).map_err(|e| format!("{}: {e}", p.display()))?;
            let all: Vec<&str> = text.lines().collect();
            for l in &all[all.len().saturating_sub(lines)..] {
                println!("{l}");
            }
            Ok(())
        }
    }
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tend: {e}");
            ExitCode::from(2)
        }
    }
}
