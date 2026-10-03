//! The configuration file, and the dependency graph it describes.
//!
//! ```toml
//! log_dir = "logs"               # relative to this file
//! control = "127.0.0.1:7340"     # where `tend status` and friends connect
//!
//! [service.db]
//! command = ["postgres", "-D", "data"]
//! ready = { tcp = "127.0.0.1:5432" }
//!
//! [service.web]
//! command = ["python", "-m", "http.server", "8000"]
//! depends_on = ["db"]
//! restart = "always"
//! ready = { log = "Serving HTTP" }
//! ```

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

/// When a service that has exited is started again.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Restart {
    /// Whatever the exit status.
    Always,
    /// Only after a non-zero exit status, a signal, or failing to become ready.
    #[default]
    OnFailure,
    Never,
}

/// How the supervisor decides that a started service is ready, so its dependents may start.
#[derive(Clone, Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Ready {
    /// Ready this long after starting.
    pub delay_ms: Option<u64>,
    /// Ready once this address accepts a TCP connection.
    pub tcp: Option<String>,
    /// Ready once a line of its output contains this text.
    pub log: Option<String>,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Service {
    pub command: Vec<String>,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub depends_on: Vec<String>,
    #[serde(default)]
    pub restart: Restart,
    #[serde(default)]
    pub ready: Ready,
    #[serde(default = "d_ready_timeout")]
    pub ready_timeout_ms: u64,
    #[serde(default = "d_backoff_initial")]
    pub backoff_initial_ms: u64,
    #[serde(default = "d_backoff_max")]
    pub backoff_max_ms: u64,
    /// Give up after this many restarts within `restart_window_s` (a crash loop).
    #[serde(default = "d_max_restarts")]
    pub max_restarts: u32,
    #[serde(default = "d_window")]
    pub restart_window_s: u64,
    /// After asking a service to stop, how long to wait before killing it.
    #[serde(default = "d_stop_timeout")]
    pub stop_timeout_ms: u64,
}

fn d_ready_timeout() -> u64 {
    30_000
}
fn d_backoff_initial() -> u64 {
    500
}
fn d_backoff_max() -> u64 {
    30_000
}
fn d_max_restarts() -> u32 {
    5
}
fn d_window() -> u64 {
    60
}
fn d_stop_timeout() -> u64 {
    5_000
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Raw {
    #[serde(default)]
    log_dir: Option<PathBuf>,
    #[serde(default)]
    control: Option<String>,
    #[serde(default)]
    log_max_bytes: Option<u64>,
    #[serde(default)]
    log_keep: Option<usize>,
    #[serde(default)]
    service: BTreeMap<String, Service>,
}

/// A validated configuration.
#[derive(Clone, Debug)]
pub struct Config {
    pub log_dir: PathBuf,
    pub control: String,
    pub log_max_bytes: u64,
    pub log_keep: usize,
    pub services: BTreeMap<String, Service>,
    /// Service names in an order where every service comes after its dependencies.
    pub order: Vec<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum ConfigError {
    Read(String),
    Parse(String),
    Invalid(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Read(s) | ConfigError::Parse(s) | ConfigError::Invalid(s) => f.write_str(s),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Service {
    pub fn ready_timeout(&self) -> Duration {
        Duration::from_millis(self.ready_timeout_ms)
    }

    pub fn stop_timeout(&self) -> Duration {
        Duration::from_millis(self.stop_timeout_ms)
    }

    /// The wait before restart number `n` (counting from 0): doubling from the initial delay,
    /// capped at the maximum.
    pub fn backoff(&self, n: u32) -> Duration {
        let ms = self.backoff_initial_ms.saturating_mul(1u64 << n.min(32)).min(self.backoff_max_ms);
        Duration::from_millis(ms)
    }
}

impl Config {
    pub fn load(path: &Path) -> Result<Config, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Read(format!("{}: {e}", path.display())))?;
        let base = path.parent().unwrap_or(Path::new("."));
        Config::parse(&text, base)
    }

    /// Parses and validates; relative paths are taken relative to `base`.
    pub fn parse(text: &str, base: &Path) -> Result<Config, ConfigError> {
        let raw: Raw = toml::from_str(text).map_err(|e| ConfigError::Parse(e.to_string()))?;
        let bad = |s: String| Err(ConfigError::Invalid(s));
        if raw.service.is_empty() {
            return bad("no services: add a [service.NAME] table".into());
        }
        let mut services = raw.service;
        for (name, s) in services.iter_mut() {
            if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
                return bad(format!("service name {name:?}: use letters, digits, '-' and '_'"));
            }
            if s.command.is_empty() || s.command[0].is_empty() {
                return bad(format!("service {name}: command is empty"));
            }
            let r = &s.ready;
            if [r.delay_ms.is_some(), r.tcp.is_some(), r.log.is_some()].iter().filter(|x| **x).count() > 1 {
                return bad(format!("service {name}: give at most one of ready.delay_ms, ready.tcp, ready.log"));
            }
            if let Some(a) = &r.tcp {
                if a.parse::<std::net::SocketAddr>().is_err() {
                    return bad(format!("service {name}: ready.tcp {a:?} is not an address such as 127.0.0.1:8080"));
                }
            }
            if s.backoff_max_ms < s.backoff_initial_ms {
                return bad(format!("service {name}: backoff_max_ms is less than backoff_initial_ms"));
            }
            for d in &s.depends_on {
                if d == name {
                    return bad(format!("service {name} depends on itself"));
                }
            }
            if let Some(cwd) = &s.cwd {
                if cwd.is_relative() {
                    s.cwd = Some(base.join(cwd));
                }
            }
        }
        for (name, s) in &services {
            for d in &s.depends_on {
                if !services.contains_key(d) {
                    return bad(format!("service {name} depends on {d}, which is not defined"));
                }
            }
        }
        let order = topo_order(&services)?;
        let log_dir = raw.log_dir.unwrap_or_else(|| PathBuf::from("logs"));
        let log_dir = if log_dir.is_relative() { base.join(log_dir) } else { log_dir };
        let control = raw.control.unwrap_or_else(|| "127.0.0.1:7340".into());
        if control.parse::<std::net::SocketAddr>().is_err() {
            return bad(format!("control {control:?} is not an address such as 127.0.0.1:7340"));
        }
        Ok(Config {
            log_dir,
            control,
            log_max_bytes: raw.log_max_bytes.unwrap_or(10 * 1024 * 1024),
            log_keep: raw.log_keep.unwrap_or(5),
            services,
            order,
        })
    }

    /// The services that depend on `name` directly.
    pub fn dependents(&self, name: &str) -> Vec<&str> {
        self.services.iter().filter(|(_, s)| s.depends_on.iter().any(|d| d == name)).map(|(n, _)| n.as_str()).collect()
    }
}

/// Kahn's algorithm, taking ready services alphabetically so the order is stable. A cycle is
/// reported with its members in order.
fn topo_order(services: &BTreeMap<String, Service>) -> Result<Vec<String>, ConfigError> {
    let mut missing: HashMap<&str, usize> = services.iter().map(|(n, s)| (n.as_str(), s.depends_on.len())).collect();
    let mut order = Vec::with_capacity(services.len());
    loop {
        let mut ready: Vec<&str> = missing.iter().filter(|(_, &c)| c == 0).map(|(n, _)| *n).collect();
        if ready.is_empty() {
            break;
        }
        ready.sort();
        for n in ready {
            missing.remove(n);
            order.push(n.to_string());
            for (m, s) in services {
                if s.depends_on.iter().any(|d| d == n) {
                    if let Some(c) = missing.get_mut(m.as_str()) {
                        *c -= 1;
                    }
                }
            }
        }
    }
    if missing.is_empty() {
        return Ok(order);
    }
    // Walk dependencies from any remaining service until a name repeats: that is a cycle.
    let mut path: Vec<&str> = Vec::new();
    let mut cur = *missing.keys().min().unwrap();
    while !path.contains(&cur) {
        path.push(cur);
        cur = services[cur].depends_on.iter().map(String::as_str).find(|d| missing.contains_key(d)).unwrap();
    }
    let start = path.iter().position(|n| *n == cur).unwrap();
    let mut cycle: Vec<&str> = path[start..].to_vec();
    cycle.push(cur);
    Err(ConfigError::Invalid(format!("dependency cycle: {}", cycle.join(" -> "))))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(t: &str) -> Result<Config, ConfigError> {
        Config::parse(t, Path::new("/base"))
    }

    #[test]
    fn a_full_example_with_defaults() {
        let c = parse(
            r#"
            log_dir = "var/log"
            [service.db]
            command = ["db", "--fast"]
            ready = { tcp = "127.0.0.1:5432" }
            [service.web]
            command = ["web"]
            depends_on = ["db", "cache"]
            restart = "always"
            cwd = "site"
            env = { PORT = "8000" }
            [service.cache]
            command = ["cache"]
            ready = { log = "accepting connections" }
            "#,
        )
        .unwrap();
        assert_eq!(c.order, ["cache", "db", "web"]);
        assert_eq!(c.log_dir, Path::new("/base").join("var/log"));
        let web = &c.services["web"];
        assert_eq!(web.restart, Restart::Always);
        assert_eq!(web.cwd.as_deref(), Some(Path::new("/base/site")));
        assert_eq!(web.env["PORT"], "8000");
        assert_eq!(c.services["db"].restart, Restart::OnFailure);
        assert_eq!(c.services["db"].stop_timeout(), Duration::from_secs(5));
        assert_eq!(c.control, "127.0.0.1:7340");
        let mut d = c.dependents("db");
        d.sort();
        assert_eq!(d, ["web"]);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let c = parse("[service.a]\ncommand=[\"a\"]\nbackoff_initial_ms = 100\nbackoff_max_ms = 1000").unwrap();
        let s = &c.services["a"];
        let ms: Vec<u128> = (0..6).map(|n| s.backoff(n).as_millis()).collect();
        assert_eq!(ms, [100, 200, 400, 800, 1000, 1000]);
        assert_eq!(s.backoff(200).as_millis(), 1000, "no overflow for large counts");
    }

    #[test]
    fn mistakes_are_explained() {
        let e = |t: &str| parse(t).unwrap_err().to_string();
        assert!(e("").contains("no services"));
        assert!(e("[service.a]\ncommand = []").contains("command is empty"));
        assert!(e("[service.a]\ncommand = [\"x\"]\ndepends_on = [\"b\"]").contains("depends on b, which is not defined"));
        assert!(e("[service.a]\ncommand = [\"x\"]\ndepends_on = [\"a\"]").contains("depends on itself"));
        assert!(e("[service.\"a b\"]\ncommand = [\"x\"]").contains("use letters"));
        assert!(e("[service.a]\ncommand = [\"x\"]\nready = { tcp = \"nowhere\" }").contains("not an address"));
        assert!(e("[service.a]\ncommand = [\"x\"]\nready = { tcp = \"127.0.0.1:1\", log = \"x\" }").contains("at most one"));
        assert!(e("[service.a]\ncommand = [\"x\"]\nrestrat = \"always\"").contains("unknown field"));
        assert!(e("[service.a]\ncommand = [\"x\"]\nrestart = \"sometimes\"").contains("unknown variant"));
        assert!(e("[service.a]\ncommand=[\"x\"]\nbackoff_initial_ms = 10\nbackoff_max_ms = 1").contains("backoff_max_ms"));
        assert!(e("control = \"x\"\n[service.a]\ncommand=[\"x\"]").contains("control"));
    }

    #[test]
    fn cycles_are_named() {
        let t = r#"
            [service.a]
            command = ["a"]
            depends_on = ["b"]
            [service.b]
            command = ["b"]
            depends_on = ["c"]
            [service.c]
            command = ["c"]
            depends_on = ["a"]
            [service.d]
            command = ["d"]
        "#;
        let e = parse(t).unwrap_err().to_string();
        assert_eq!(e, "dependency cycle: a -> b -> c -> a");
    }

    #[test]
    fn the_order_puts_every_dependency_first() {
        // A diamond and a chain.
        let c = parse(
            r#"
            [service.top]
            command = ["x"]
            depends_on = ["left", "right"]
            [service.left]
            command = ["x"]
            depends_on = ["base"]
            [service.right]
            command = ["x"]
            depends_on = ["base"]
            [service.base]
            command = ["x"]
            [service.z1]
            command = ["x"]
            depends_on = ["z2"]
            [service.z2]
            command = ["x"]
            "#,
        )
        .unwrap();
        let pos = |n: &str| c.order.iter().position(|x| x == n).unwrap();
        for (name, s) in &c.services {
            for d in &s.depends_on {
                assert!(pos(d) < pos(name), "{d} must come before {name}: {:?}", c.order);
            }
        }
        assert_eq!(c.order.len(), 6);
    }
}
