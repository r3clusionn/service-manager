//! The supervisor: one thread that owns every service's state and reacts to events.
//!
//! Other threads only send events: one per service output stream (each line), one per process
//! (its exit), one per readiness probe, the control server, and the Ctrl+C handler. Every event
//! carries the *generation* of the process it is about (a counter bumped at every start), so an
//! event from a process that has since been replaced is ignored.
//!
//! The rules, in one place:
//!
//! * A service starts when it is wanted and every service it depends on is ready.
//! * It is ready when its readiness check passes (immediately, if it has none). A service that is
//!   not ready within its timeout is stopped and counts as failed.
//! * When it exits on its own, its restart policy decides. Restarts wait `backoff(n)`, doubling
//!   each time; a service that ran for longer than its restart window starts again from the first
//!   delay. More than `max_restarts` restarts within the window is a crash loop: it is marked
//!   failed and left alone until started by hand.
//! * A service that should stop is asked to (`SIGTERM` or `CTRL_BREAK`) only once nothing that
//!   depends on it is still running, and killed with everything it started if it has not exited
//!   within its stop timeout. Shutting down is asking every service to stop, so it happens in
//!   reverse dependency order.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::net::{SocketAddr, TcpStream};
use std::path::PathBuf;
use std::process::Child;
use std::sync::mpsc::{channel, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::config::{Config, Restart, Service};
use crate::logs::LogFile;
use crate::process::{self, Proc};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Wanted, but waiting for its dependencies to be ready.
    Waiting,
    /// Started; its readiness check has not passed yet.
    Starting,
    /// Started and ready.
    Running,
    /// Asked to stop; waiting for it to exit.
    Stopping,
    /// Not running, and not wanted.
    Stopped,
    /// Exited; waiting to be restarted.
    Backoff,
    /// Exited and will not be restarted (by its policy).
    Exited,
    /// Restarted too often too quickly; left alone until started by hand.
    Failed,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum StopReason {
    /// Stopped by command or shutdown: do not restart.
    Wanted,
    /// Being restarted by command.
    Restart,
    /// Did not become ready in time: counts as a failure.
    NotReady,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum TimerKind {
    Restart,
    ReadyDelay,
    ReadyTimeout,
    Kill,
}

#[derive(Debug)]
enum Event {
    Exited { name: String, gen: u64, desc: String, success: bool },
    Line { name: String, gen: u64, err: bool, text: String },
    Probe { name: String, gen: u64 },
    Control(Request, Sender<Response>),
    Shutdown,
}

/// A command for the running supervisor.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    Status,
    Start { name: String },
    Stop { name: String },
    Restart { name: String },
    Shutdown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ServiceStatus {
    pub name: String,
    pub state: State,
    pub pid: Option<u32>,
    pub restarts: u32,
    pub uptime_s: Option<u64>,
    pub last_exit: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "result", rename_all = "lowercase")]
pub enum Response {
    Ok { message: String },
    Status { services: Vec<ServiceStatus> },
    Error { message: String },
}

/// When, a sequence number (so equal times keep their order), what, which service, which process.
type TimerEntry = (Instant, u64, TimerKind, String, u64);

struct Svc {
    spec: Service,
    state: State,
    wanted: bool,
    gen: u64,
    proc: Option<Proc>,
    started: Option<Instant>,
    stop_reason: Option<StopReason>,
    restarts: u32,
    recent: VecDeque<Instant>,
    backoff_n: u32,
    last_exit: Option<String>,
    log: Option<LogFile>,
}

/// Output for the person watching: supervisor messages and, unless quiet, service output.
pub trait Console: Send {
    fn event(&mut self, service: &str, text: &str);
    fn output(&mut self, service: &str, err: bool, text: &str);
}

/// Prints to standard output.
pub struct Stdout {
    pub echo: bool,
    pub width: usize,
}

impl Console for Stdout {
    fn event(&mut self, service: &str, text: &str) {
        println!("{:>w$} * {text}", service, w = self.width);
    }

    fn output(&mut self, service: &str, err: bool, text: &str) {
        if self.echo {
            println!("{:>w$} {} {text}", service, if err { "!" } else { "|" }, w = self.width);
        }
    }
}

pub struct Supervisor {
    cfg: Config,
    svcs: HashMap<String, Svc>,
    tx: Sender<Event>,
    rx: Receiver<Event>,
    timers: BinaryHeap<Reverse<TimerEntry>>,
    timer_seq: u64,
    shutting_down: bool,
    own_log: LogFile,
    console: Box<dyn Console>,
}

/// Lets other threads talk to a running supervisor.
#[derive(Clone)]
pub struct Handle {
    tx: Sender<Event>,
}

impl Handle {
    /// Begins an orderly shutdown (what Ctrl+C does). A second call kills everything.
    pub fn shutdown(&self) {
        let _ = self.tx.send(Event::Shutdown);
    }

    /// Sends a request and waits for the answer.
    pub fn request(&self, r: Request) -> Response {
        let (tx, rx) = channel();
        if self.tx.send(Event::Control(r, tx)).is_err() {
            return Response::Error { message: "the supervisor has stopped".into() };
        }
        rx.recv_timeout(Duration::from_secs(10)).unwrap_or(Response::Error { message: "no answer from the supervisor".into() })
    }
}

fn describe(status: std::process::ExitStatus) -> (String, bool) {
    if let Some(c) = status.code() {
        return (format!("exit code {c}"), c == 0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(s) = status.signal() {
            return (format!("killed by signal {s}"), false);
        }
    }
    ("exited".into(), false)
}

/// Forwards a stream line by line. Long lines are cut at 64 KiB so one runaway line cannot take
/// unbounded memory; invalid UTF-8 is replaced.
fn pump(name: String, gen: u64, err: bool, stream: impl Read, tx: Sender<Event>) {
    let mut r = BufReader::new(stream);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        let mut chunk = (&mut r).take(64 * 1024);
        match chunk.read_until(b'\n', &mut buf) {
            Ok(0) | Err(_) => return,
            Ok(_) => {
                while buf.last().is_some_and(|&b| b == b'\n' || b == b'\r') {
                    buf.pop();
                }
                let text = String::from_utf8_lossy(&buf).into_owned();
                if tx.send(Event::Line { name: name.clone(), gen, err, text }).is_err() {
                    return;
                }
            }
        }
    }
}

impl Supervisor {
    pub fn new(cfg: Config, console: Box<dyn Console>) -> std::io::Result<Supervisor> {
        let (tx, rx) = channel();
        let own_log = LogFile::open(&cfg.log_dir.join("tend.log"), cfg.log_max_bytes, cfg.log_keep)?;
        let mut svcs = HashMap::new();
        for (name, spec) in &cfg.services {
            let log = LogFile::open(&cfg.log_dir.join(format!("{name}.log")), cfg.log_max_bytes, cfg.log_keep)?;
            svcs.insert(
                name.clone(),
                Svc {
                    spec: spec.clone(),
                    state: State::Stopped,
                    wanted: false,
                    gen: 0,
                    proc: None,
                    started: None,
                    stop_reason: None,
                    restarts: 0,
                    recent: VecDeque::new(),
                    backoff_n: 0,
                    last_exit: None,
                    log: Some(log),
                },
            );
        }
        Ok(Supervisor { cfg, svcs, tx, rx, timers: BinaryHeap::new(), timer_seq: 0, shutting_down: false, own_log, console })
    }

    pub fn handle(&self) -> Handle {
        Handle { tx: self.tx.clone() }
    }

    pub fn log_dir(&self) -> PathBuf {
        self.cfg.log_dir.clone()
    }

    fn note(&mut self, name: &str, text: &str) {
        let _ = self.own_log.line(name, text);
        self.console.event(name, text);
    }

    fn timer(&mut self, after: Duration, kind: TimerKind, name: &str, gen: u64) {
        self.timer_seq += 1;
        self.timers.push(Reverse((Instant::now() + after, self.timer_seq, kind, name.to_string(), gen)));
    }

    fn alive(&self, name: &str) -> bool {
        self.svcs[name].proc.is_some()
    }

    /// Starts everything, then runs until shut down. Returns when every service has exited.
    pub fn run(mut self) {
        let names: Vec<String> = self.cfg.order.clone();
        for n in &names {
            let s = self.svcs.get_mut(n).unwrap();
            s.wanted = true;
            s.state = State::Waiting;
        }
        self.note("tend", &format!("starting {} service(s): {}", names.len(), names.join(", ")));
        self.start_ready();
        loop {
            if self.shutting_down && self.svcs.keys().all(|n| self.svcs[n].proc.is_none()) {
                break;
            }
            let now = Instant::now();
            let wait =
                self.timers.peek().map(|Reverse(t)| t.0.saturating_duration_since(now)).unwrap_or(Duration::from_secs(3600));
            match self.rx.recv_timeout(wait) {
                Ok(ev) => self.handle_event(ev),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => break,
            }
            while let Some(Reverse(t)) = self.timers.peek() {
                if t.0 > Instant::now() {
                    break;
                }
                let Reverse((_, _, kind, name, gen)) = self.timers.pop().unwrap();
                self.handle_timer(kind, &name, gen);
            }
            self.pump_stops();
            self.start_ready();
        }
        self.note("tend", "all services stopped");
    }

    /// Starts every wanted, waiting service whose dependencies are all ready.
    fn start_ready(&mut self) {
        if self.shutting_down {
            return;
        }
        for name in self.cfg.order.clone() {
            let s = &self.svcs[&name];
            if !(s.wanted && s.state == State::Waiting) {
                continue;
            }
            if s.spec.depends_on.iter().all(|d| self.svcs[d].state == State::Running) {
                self.spawn(&name);
            }
        }
    }

    fn spawn(&mut self, name: &str) {
        let s = self.svcs.get_mut(name).unwrap();
        s.gen += 1;
        let gen = s.gen;
        let restarts = s.restarts;
        match process::spawn(name, &s.spec, restarts) {
            Ok((proc, child)) => {
                let pid = proc.pid;
                s.proc = Some(proc);
                s.state = State::Starting;
                s.started = Some(Instant::now());
                s.stop_reason = None;
                let ready = s.spec.ready.clone();
                let timeout = s.spec.ready_timeout();
                self.watch(name, gen, child);
                self.note(name, &format!("started (pid {pid})"));
                if let Some(ms) = ready.delay_ms {
                    self.timer(Duration::from_millis(ms), TimerKind::ReadyDelay, name, gen);
                } else if let Some(addr) = ready.tcp {
                    self.timer(timeout, TimerKind::ReadyTimeout, name, gen);
                    let addr: SocketAddr = addr.parse().expect("validated with the configuration");
                    let tx = self.tx.clone();
                    let n = name.to_string();
                    thread::spawn(move || {
                        let end = Instant::now() + timeout;
                        while Instant::now() < end {
                            if TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_ok() {
                                let _ = tx.send(Event::Probe { name: n, gen });
                                return;
                            }
                            thread::sleep(Duration::from_millis(50));
                        }
                    });
                } else if ready.log.is_some() {
                    self.timer(timeout, TimerKind::ReadyTimeout, name, gen);
                } else {
                    self.mark_ready(name, gen);
                }
            }
            Err(e) => {
                let program = s.spec.command[0].clone();
                self.note(name, &format!("could not start {program:?}: {e}"));
                self.after_exit(name, format!("could not start: {e}"), false);
            }
        }
    }

    /// Starts the threads that forward the child's output and report its exit.
    fn watch(&self, name: &str, gen: u64, mut child: Child) {
        let out = child.stdout.take().unwrap();
        let err = child.stderr.take().unwrap();
        for (stream, is_err) in [(Box::new(out) as Box<dyn Read + Send>, false), (Box::new(err), true)] {
            let (n, tx) = (name.to_string(), self.tx.clone());
            thread::spawn(move || pump(n, gen, is_err, stream, tx));
        }
        let (n, tx) = (name.to_string(), self.tx.clone());
        thread::spawn(move || {
            let (desc, success) = match child.wait() {
                Ok(st) => describe(st),
                Err(e) => (format!("could not be waited for: {e}"), false),
            };
            let _ = tx.send(Event::Exited { name: n, gen, desc, success });
        });
    }

    fn mark_ready(&mut self, name: &str, gen: u64) {
        let s = self.svcs.get_mut(name).unwrap();
        if s.gen != gen || s.state != State::Starting {
            return;
        }
        s.state = State::Running;
        let took = s.started.map(|t| t.elapsed().as_millis()).unwrap_or(0);
        self.note(name, &format!("ready after {took} ms"));
    }

    fn handle_event(&mut self, ev: Event) {
        match ev {
            Event::Line { name, gen, err, text } => {
                let s = self.svcs.get_mut(&name).unwrap();
                if let Some(l) = s.log.as_mut() {
                    let _ = l.line(if err { "err" } else { "out" }, &text);
                }
                self.console.output(&name, err, &text);
                let s = &self.svcs[&name];
                if s.gen == gen && s.state == State::Starting {
                    if let Some(want) = &s.spec.ready.log {
                        if text.contains(want.as_str()) {
                            self.mark_ready(&name, gen);
                        }
                    }
                }
            }
            Event::Probe { name, gen } => self.mark_ready(&name, gen),
            Event::Exited { name, gen, desc, success } => {
                if self.svcs[&name].gen != gen {
                    return;
                }
                self.svcs.get_mut(&name).unwrap().proc = None;
                self.after_exit(&name, desc, success);
            }
            Event::Control(req, reply) => {
                let r = self.control(req);
                let _ = reply.send(r);
            }
            Event::Shutdown => {
                if self.shutting_down {
                    self.note("tend", "second interrupt: killing every service now");
                    for s in self.svcs.values() {
                        if let Some(p) = &s.proc {
                            p.kill();
                        }
                    }
                } else {
                    self.shutdown();
                }
            }
        }
    }

    fn shutdown(&mut self) {
        self.shutting_down = true;
        self.note("tend", "shutting down: stopping services in reverse dependency order");
        for s in self.svcs.values_mut() {
            s.wanted = false;
            if matches!(s.state, State::Waiting | State::Backoff) {
                s.state = State::Stopped;
            }
        }
    }

    fn after_exit(&mut self, name: &str, desc: String, success: bool) {
        let s = self.svcs.get_mut(name).unwrap();
        let ran = s.started.map(|t| t.elapsed()).unwrap_or_default();
        s.started = None;
        s.last_exit = Some(desc.clone());
        let reason = s.stop_reason.take();
        let _ = s.log.as_mut().map(|l| l.line("tend", &format!("{desc} after {:.1} s", ran.as_secs_f64())));
        match reason {
            Some(StopReason::Wanted) => {
                s.state = State::Stopped;
                self.note(name, &format!("stopped ({desc})"));
                return;
            }
            Some(StopReason::Restart) => {
                s.state = State::Waiting;
                s.restarts += 1;
                self.note(name, &format!("stopped for restart ({desc})"));
                return;
            }
            Some(StopReason::NotReady) | None => {}
        }
        let failed = !success || reason == Some(StopReason::NotReady);
        let again = s.wanted
            && !self.shutting_down
            && match s.spec.restart {
                Restart::Always => true,
                Restart::OnFailure => failed,
                Restart::Never => false,
            };
        if !again {
            s.state = State::Exited;
            self.note(name, &format!("exited ({desc}); not restarting"));
            return;
        }
        // A long healthy run forgives earlier failures.
        let window = Duration::from_secs(s.spec.restart_window_s);
        if ran >= window {
            s.backoff_n = 0;
            s.recent.clear();
        }
        let now = Instant::now();
        while s.recent.front().is_some_and(|t| now.duration_since(*t) > window) {
            s.recent.pop_front();
        }
        if s.recent.len() as u32 >= s.spec.max_restarts {
            s.state = State::Failed;
            let (n, w) = (s.recent.len(), s.spec.restart_window_s);
            self.note(
                name,
                &format!("exited ({desc}); restarted {n} times in {w} s: giving up (`tend start {name}` to try again)"),
            );
            return;
        }
        s.recent.push_back(now);
        let delay = s.spec.backoff(s.backoff_n);
        s.backoff_n += 1;
        s.restarts += 1;
        s.state = State::Backoff;
        let gen = s.gen;
        self.note(name, &format!("exited ({desc}); restarting in {} ms", delay.as_millis()));
        self.timer(delay, TimerKind::Restart, name, gen);
    }

    fn handle_timer(&mut self, kind: TimerKind, name: &str, gen: u64) {
        let s = &self.svcs[name];
        if s.gen != gen {
            return;
        }
        match kind {
            TimerKind::Restart => {
                if s.state == State::Backoff && s.wanted {
                    self.svcs.get_mut(name).unwrap().state = State::Waiting;
                }
            }
            TimerKind::ReadyDelay => self.mark_ready(name, gen),
            TimerKind::ReadyTimeout => {
                if s.state == State::Starting {
                    let secs = s.spec.ready_timeout().as_secs_f64();
                    self.note(name, &format!("not ready after {secs:.1} s: stopping it"));
                    self.begin_stop(name, StopReason::NotReady);
                }
            }
            TimerKind::Kill => {
                if s.state == State::Stopping {
                    if let Some(p) = &s.proc {
                        p.kill();
                    }
                    let secs = s.spec.stop_timeout().as_secs_f64();
                    self.note(name, &format!("did not stop within {secs:.1} s: killed"));
                }
            }
        }
    }

    fn begin_stop(&mut self, name: &str, reason: StopReason) {
        let s = self.svcs.get_mut(name).unwrap();
        if s.proc.is_none() || s.state == State::Stopping {
            return;
        }
        s.state = State::Stopping;
        s.stop_reason = Some(reason);
        let delivered = s.proc.as_ref().unwrap().request_stop();
        let gen = s.gen;
        let timeout = s.spec.stop_timeout();
        if delivered {
            self.timer(timeout, TimerKind::Kill, name, gen);
        } else {
            // No way to ask politely (no shared console, for example): kill now.
            self.svcs[name].proc.as_ref().unwrap().kill();
            self.note(name, "could not ask it to stop: killed");
        }
    }

    /// Asks unwanted running services to stop, but only once nothing that depends on them is
    /// still running.
    fn pump_stops(&mut self) {
        let names: Vec<String> = self.cfg.order.iter().rev().cloned().collect();
        for name in names {
            let s = &self.svcs[&name];
            if s.wanted || s.proc.is_none() || s.state == State::Stopping {
                continue;
            }
            if self.cfg.dependents(&name).iter().all(|d| !self.alive(d)) {
                self.note(&name, "stopping");
                self.begin_stop(&name, StopReason::Wanted);
            }
        }
    }

    /// `name` and everything it needs, transitively.
    fn with_dependencies(&self, name: &str) -> Vec<String> {
        let mut out = vec![name.to_string()];
        let mut i = 0;
        while i < out.len() {
            for d in &self.svcs[&out[i]].spec.depends_on {
                if !out.contains(d) {
                    out.push(d.clone());
                }
            }
            i += 1;
        }
        out
    }

    /// `name` and everything that needs it, transitively.
    fn with_dependents(&self, name: &str) -> Vec<String> {
        let mut out = vec![name.to_string()];
        let mut i = 0;
        while i < out.len() {
            for d in self.cfg.dependents(&out[i]) {
                if !out.iter().any(|x| x == d) {
                    out.push(d.to_string());
                }
            }
            i += 1;
        }
        out
    }

    fn control(&mut self, req: Request) -> Response {
        let known = |n: &str| self.svcs.contains_key(n);
        match req {
            Request::Status => {
                let services = self
                    .cfg
                    .order
                    .iter()
                    .map(|n| {
                        let s = &self.svcs[n];
                        ServiceStatus {
                            name: n.clone(),
                            state: s.state,
                            pid: s.proc.as_ref().map(|p| p.pid),
                            restarts: s.restarts,
                            uptime_s: s.started.map(|t| t.elapsed().as_secs()),
                            last_exit: s.last_exit.clone(),
                        }
                    })
                    .collect();
                Response::Status { services }
            }
            Request::Start { name } | Request::Stop { name } | Request::Restart { name } if !known(&name) => {
                Response::Error { message: format!("no service named {name}") }
            }
            Request::Start { name } => {
                if self.shutting_down {
                    return Response::Error { message: "shutting down".into() };
                }
                for n in self.with_dependencies(&name) {
                    let s = self.svcs.get_mut(&n).unwrap();
                    s.wanted = true;
                    if matches!(s.state, State::Stopped | State::Exited | State::Failed) {
                        s.state = State::Waiting;
                        s.recent.clear();
                        s.backoff_n = 0;
                    }
                }
                self.note(&name, "start requested");
                Response::Ok { message: format!("starting {name}") }
            }
            Request::Stop { name } => {
                let all = self.with_dependents(&name);
                for n in &all {
                    let s = self.svcs.get_mut(n).unwrap();
                    s.wanted = false;
                    if matches!(s.state, State::Waiting | State::Backoff) {
                        s.state = State::Stopped;
                    }
                }
                self.note(&name, "stop requested");
                let also: Vec<&String> = all.iter().skip(1).collect();
                let extra = if also.is_empty() {
                    String::new()
                } else {
                    format!(" (and {}, which depend on it)", also.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "))
                };
                Response::Ok { message: format!("stopping {name}{extra}") }
            }
            Request::Restart { name } => {
                if self.svcs[&name].proc.is_none() {
                    return self.control(Request::Start { name });
                }
                self.svcs.get_mut(&name).unwrap().wanted = true;
                self.note(&name, "restart requested");
                self.begin_stop(&name, StopReason::Restart);
                Response::Ok { message: format!("restarting {name}") }
            }
            Request::Shutdown => {
                self.shutdown();
                Response::Ok { message: "shutting down".into() }
            }
        }
    }
}
