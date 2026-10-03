//! `tend run` with real child processes (the `testsvc` example), controlled through the command line.

use std::io::Read;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

fn tend_exe() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_tend"))
}

/// The test service, built as an example next to the test binaries.
fn testsvc() -> String {
    let deps = std::env::current_exe().unwrap().parent().unwrap().to_path_buf();
    let exe = deps.parent().unwrap().join("examples").join(if cfg!(windows) { "testsvc.exe" } else { "testsvc" });
    assert!(exe.exists(), "{} is missing: run `cargo test` (which builds examples)", exe.display());
    exe.to_string_lossy().replace('\\', "/")
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

struct Run {
    dir: tempfile::TempDir,
    child: Option<Child>,
}

impl Run {
    /// Writes the configuration (`SVC` stands for the test service, `CONTROL` for a free port) and
    /// starts `tend run --quiet`.
    fn start(body: &str) -> Run {
        let dir = tempfile::tempdir().unwrap();
        let control = format!("127.0.0.1:{}", free_port());
        let text = format!("control = \"{control}\"\n{}", body.replace("SVC", &testsvc()));
        std::fs::write(dir.path().join("tend.toml"), text).unwrap();
        let child = Command::new(tend_exe())
            .args(["--config", "tend.toml", "run"])
            .current_dir(dir.path())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        let mut r = Run { dir, child: Some(child) };
        r.wait_for(|r| r.cli(&["status"]).0, Duration::from_secs(10), "the supervisor to answer");
        r
    }

    fn path(&self, p: &str) -> PathBuf {
        self.dir.path().join(p)
    }

    /// Runs `tend ARGS` against this supervisor: (success, stdout + stderr).
    fn cli(&self, args: &[&str]) -> (bool, String) {
        let o = Command::new(tend_exe()).arg("--config").arg(self.path("tend.toml")).args(args).output().unwrap();
        let mut s = String::from_utf8_lossy(&o.stdout).into_owned();
        s.push_str(&String::from_utf8_lossy(&o.stderr));
        (o.status.success(), s)
    }

    /// (state, restarts) of a service.
    fn state(&self, name: &str) -> (String, u32) {
        let (_, out) = self.cli(&["status"]);
        let line = out.lines().find(|l| l.split_whitespace().next() == Some(name)).unwrap_or_else(|| panic!("{out}"));
        let f: Vec<&str> = line.split_whitespace().collect();
        (f[1].to_string(), f[3].parse().unwrap())
    }

    fn wait_for(&mut self, mut f: impl FnMut(&Run) -> bool, limit: Duration, what: &str) {
        let t = Instant::now();
        while !f(self) {
            if t.elapsed() > limit {
                let out = self.stop();
                panic!("timed out waiting for {what}; supervisor output:\n{out}");
            }
            std::thread::sleep(Duration::from_millis(30));
        }
    }

    fn wait_state(&mut self, name: &str, state: &str) {
        let n = name.to_string();
        let s = state.to_string();
        self.wait_for(move |r| r.state(&n).0 == s, Duration::from_secs(15), &format!("{name} to be {state}"));
    }

    /// Shuts the supervisor down and returns everything it printed.
    fn stop(&mut self) -> String {
        let Some(mut child) = self.child.take() else { return String::new() };
        let _ = self.cli(&["shutdown"]);
        let t = Instant::now();
        while child.try_wait().unwrap().is_none() {
            if t.elapsed() > Duration::from_secs(20) {
                let _ = child.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(30));
        }
        let mut out = String::new();
        child.stdout.take().unwrap().read_to_string(&mut out).unwrap();
        child.stderr.take().unwrap().read_to_string(&mut out).unwrap();
        out
    }

    /// Lines of the marker file as (microseconds, event, service).
    fn marks(&self) -> Vec<(u128, String, String)> {
        std::fs::read_to_string(self.path("marks.txt"))
            .unwrap_or_default()
            .lines()
            .map(|l| {
                let f: Vec<&str> = l.split_whitespace().collect();
                (f[0].parse().unwrap(), f[1].to_string(), f.get(2).unwrap_or(&"").to_string())
            })
            .collect()
    }

    fn mark_time(&self, event: &str, name: &str) -> u128 {
        self.marks()
            .iter()
            .find(|m| m.1 == event && m.2 == name)
            .unwrap_or_else(|| panic!("no {event} mark for {name}: {:?}", self.marks()))
            .0
    }
}

impl Drop for Run {
    fn drop(&mut self) {
        if let Some(mut c) = self.child.take() {
            let _ = c.kill();
        }
    }
}

#[test]
fn dependencies_start_in_order_after_readiness_and_stop_in_reverse() {
    let port = free_port();
    let mut r = Run::start(&format!(
        r#"
        [service.db]
        command = ["SVC", "--ready-after", "300", "--listen", "127.0.0.1:{port}", "--marker", "marks.txt"]
        ready = {{ tcp = "127.0.0.1:{port}" }}
        [service.api]
        command = ["SVC", "--ready-after", "200", "--marker", "marks.txt"]
        depends_on = ["db"]
        ready = {{ log = "ready" }}
        [service.web]
        command = ["SVC", "--marker", "marks.txt"]
        depends_on = ["api"]
        ready = {{ delay_ms = 100 }}
        [service.solo]
        command = ["SVC", "--marker", "marks.txt"]
        "#
    ));
    r.wait_state("web", "running");
    // Each dependent started only after its dependency was ready, which took 300 and 200 ms.
    let (db, api, web) = (r.mark_time("start", "db"), r.mark_time("start", "api"), r.mark_time("start", "web"));
    assert!(api >= db + 300_000, "api started {} us after db", api - db);
    assert!(web >= api + 200_000, "web started {} us after api", web - api);
    let out = r.stop();
    assert!(out.contains("all services stopped"), "{out}");
    let (w, a, d) = (r.mark_time("stop", "web"), r.mark_time("stop", "api"), r.mark_time("stop", "db"));
    assert!(w < a && a < d, "stop order: web {w}, api {a}, db {d}");
    // Logs: each service's output, timestamped.
    let log = std::fs::read_to_string(r.path("logs/api.log")).unwrap();
    assert!(log.lines().any(|l| l.ends_with(" out  ready")), "{log}");
    assert!(std::fs::read_to_string(r.path("logs/tend.log")).unwrap().contains("ready after"));
    assert!(!r.path("logs/tend.token").exists(), "the token file is removed at exit");
}

#[test]
fn failures_are_restarted_with_growing_delays() {
    let mut r = Run::start(
        r#"
        [service.flaky]
        command = ["SVC", "--fail-until-restarts", "3"]
        backoff_initial_ms = 50
        "#,
    );
    r.wait_for(|r| r.state("flaky") == ("running".into(), 3), Duration::from_secs(10), "flaky to run after 3 restarts");
    let out = r.stop();
    for ms in [50, 100, 200] {
        assert!(out.contains(&format!("restarting in {ms} ms")), "{ms}: {out}");
    }
}

#[test]
fn a_crash_loop_is_given_up_and_can_be_started_by_hand() {
    let mut r = Run::start(
        r#"
        [service.bad]
        command = ["SVC", "--exit-after", "20", "--code", "1"]
        backoff_initial_ms = 10
        max_restarts = 3
        restart_window_s = 60
        "#,
    );
    r.wait_state("bad", "failed");
    assert_eq!(r.state("bad").1, 3);
    let (ok, msg) = r.cli(&["start", "bad"]);
    assert!(ok && msg.contains("starting bad"), "{msg}");
    // It fails again, three more times, and is given up again.
    r.wait_for(|r| r.state("bad") == ("failed".into(), 6), Duration::from_secs(10), "the second give-up");
    let out = r.stop();
    assert!(out.contains("restarted 3 times in 60 s: giving up"), "{out}");
}

#[test]
fn restart_policies() {
    let mut r = Run::start(
        r#"
        [service.never]
        command = ["SVC", "--exit-after", "20", "--code", "1"]
        restart = "never"
        [service.clean]
        command = ["SVC", "--exit-after", "20", "--code", "0"]
        [service.always]
        command = ["SVC", "--exit-after", "50", "--code", "0"]
        restart = "always"
        backoff_initial_ms = 10
        max_restarts = 100
        "#,
    );
    r.wait_state("never", "exited");
    r.wait_state("clean", "exited");
    r.wait_for(|r| r.state("always").1 >= 3, Duration::from_secs(10), "always to restart after clean exits");
    assert_eq!(r.state("never").1, 0);
    let out = r.stop();
    assert!(out.contains("never * exited (exit code 1); not restarting"), "{out}");
}

#[test]
fn a_service_that_ignores_the_stop_request_is_killed_with_its_children() {
    let mut r = Run::start(
        r#"
        [service.stubborn]
        command = ["SVC", "--ignore-stop", "--grandchild", "grandchild.pid"]
        stop_timeout_ms = 300
        "#,
    );
    r.wait_state("stubborn", "running");
    r.wait_for(|r| r.path("grandchild.pid").exists(), Duration::from_secs(5), "the grandchild");
    let gc: u32 = std::fs::read_to_string(r.path("grandchild.pid")).unwrap().trim().parse().unwrap();
    assert!(running(gc), "the grandchild runs");
    let (ok, msg) = r.cli(&["stop", "stubborn"]);
    assert!(ok, "{msg}");
    r.wait_state("stubborn", "stopped");
    let t = Instant::now();
    while running(gc) && t.elapsed() < Duration::from_secs(5) {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!running(gc), "the grandchild {gc} outlived its service");
    let out = r.stop();
    assert!(out.contains("did not stop within 0.3 s: killed"), "{out}");
}

#[test]
fn not_becoming_ready_counts_as_a_failure() {
    let mut r = Run::start(
        r#"
        [service.slow]
        command = ["SVC", "--say", "starting up"]
        ready = { log = "this never appears" }
        ready_timeout_ms = 300
        backoff_initial_ms = 10
        max_restarts = 2
        [service.after]
        command = ["SVC"]
        depends_on = ["slow"]
        "#,
    );
    r.wait_state("slow", "failed");
    // Its dependent never started.
    assert_eq!(r.state("after").0, "waiting");
    let out = r.stop();
    assert!(out.contains("not ready after 0.3 s: stopping it"), "{out}");
}

#[test]
fn stop_and_start_follow_dependencies() {
    let mut r = Run::start(
        r#"
        [service.base]
        command = ["SVC"]
        [service.mid]
        command = ["SVC"]
        depends_on = ["base"]
        [service.top]
        command = ["SVC"]
        depends_on = ["mid"]
        [service.other]
        command = ["SVC"]
        "#,
    );
    r.wait_state("top", "running");
    let (_, msg) = r.cli(&["stop", "base"]);
    assert!(msg.contains("stopping base (and mid, top, which depend on it)"), "{msg}");
    r.wait_state("base", "stopped");
    for n in ["mid", "top"] {
        assert_eq!(r.state(n).0, "stopped");
    }
    assert_eq!(r.state("other").0, "running");
    r.cli(&["start", "top"]);
    r.wait_state("top", "running");
    assert_eq!(r.state("base").0, "running");
    // Restart: a new process, counted.
    let pid_before = status_pid(&r, "mid");
    r.cli(&["restart", "mid"]);
    r.wait_for(|r| r.state("mid") == ("running".into(), 1), Duration::from_secs(10), "mid to come back");
    assert_ne!(status_pid(&r, "mid"), pid_before);
    let (ok, msg) = r.cli(&["stop", "nosuch"]);
    assert!(!ok && msg.contains("no service named nosuch"), "{msg}");
    r.stop();
}

#[test]
fn logs_command_and_rotation() {
    let mut r = Run::start(
        r#"
        log_max_bytes = 2000
        log_keep = 2
        [service.talky]
        command = ["SVC", "--chatter", "1"]
        "#,
    );
    r.wait_for(|r| r.path("logs/talky.log.2").exists(), Duration::from_secs(15), "two rotations");
    let (ok, tail) = r.cli(&["logs", "talky", "-n", "3"]);
    assert!(ok && tail.lines().count() == 3 && tail.contains("out  tick"), "{tail}");
    r.stop();
    assert!(!r.path("logs/talky.log.3").exists(), "only two old files are kept");
    assert!(std::fs::metadata(r.path("logs/talky.log.1")).unwrap().len() <= 2000);
}

#[test]
fn the_control_port_requires_the_token() {
    use std::io::{BufRead, BufReader, Write};
    let mut r = Run::start("[service.a]\ncommand = [\"SVC\"]\n");
    let cfg = std::fs::read_to_string(r.path("tend.toml")).unwrap();
    let addr = cfg.lines().next().unwrap().split('"').nth(1).unwrap().to_string();
    let mut c = std::net::TcpStream::connect(&addr).unwrap();
    c.write_all(b"{\"token\":\"guess\",\"cmd\":\"shutdown\"}\n").unwrap();
    let mut line = String::new();
    BufReader::new(c).read_line(&mut line).unwrap();
    assert!(line.contains("wrong token"), "{line}");
    assert_eq!(r.state("a").0, "running", "nothing happened");
    r.stop();
}

#[test]
fn clear_errors() {
    let d = tempfile::tempdir().unwrap();
    let cfg = d.path().join("tend.toml");
    std::fs::write(
        &cfg,
        "[service.a]\ncommand = [\"x\"]\ndepends_on = [\"a2\"]\n[service.a2]\ncommand = [\"y\"]\ndepends_on = [\"a\"]\n",
    )
    .unwrap();
    let o = Command::new(tend_exe()).arg("--config").arg(&cfg).arg("check").output().unwrap();
    assert_eq!(o.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&o.stderr).contains("dependency cycle: a -> a2 -> a"));
    std::fs::write(&cfg, format!("control = \"127.0.0.1:{}\"\n[service.a]\ncommand = [\"x\"]\n", free_port())).unwrap();
    let o = Command::new(tend_exe()).arg("--config").arg(&cfg).arg("status").output().unwrap();
    assert!(String::from_utf8_lossy(&o.stderr).contains("no supervisor seems to be running"));
    // A program that does not exist: reported, and handled like a failure.
    let mut r =
        Run::start("[service.ghost]\ncommand = [\"no-such-program-tend-test\"]\nmax_restarts = 1\nbackoff_initial_ms = 10\n");
    r.wait_state("ghost", "failed");
    let out = r.stop();
    assert!(out.contains("could not start \"no-such-program-tend-test\""), "{out}");
}

fn status_pid(r: &Run, name: &str) -> String {
    let (_, out) = r.cli(&["status"]);
    out.lines().find(|l| l.split_whitespace().next() == Some(name)).unwrap().split_whitespace().nth(2).unwrap().to_string()
}

#[cfg(windows)]
fn running(pid: u32) -> bool {
    let o = Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().unwrap();
    String::from_utf8_lossy(&o.stdout).contains(&pid.to_string())
}

#[cfg(unix)]
fn running(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}
