//! A service with configurable behaviour, for the integration tests and the demo.
//!
//! ```text
//! testsvc [--say TEXT] [--ready-after MS] [--listen ADDR] [--exit-after MS] [--code N]
//!         [--fail-until-restarts N] [--ignore-stop] [--marker FILE] [--grandchild FILE]
//!         [--chatter MS]
//! ```

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

static STOP: AtomicBool = AtomicBool::new(false);

fn now_us() -> u128 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_micros()
}

fn mark(file: &Option<String>, what: &str) {
    if let Some(f) = file {
        let name = std::env::var("TEND_SERVICE").unwrap_or_default();
        let mut h = std::fs::OpenOptions::new().create(true).append(true).open(f).unwrap();
        // One write per line: several services append to the same file at once, and separate
        // writes from different processes could interleave.
        h.write_all(format!("{} {what} {name}\n", now_us()).as_bytes()).unwrap();
    }
}

#[cfg(windows)]
fn install_stop_handler(ignore: bool) {
    use windows_sys::Win32::System::Console::SetConsoleCtrlHandler;
    unsafe extern "system" fn handler(_ctrl: u32) -> i32 {
        STOP.store(true, Ordering::SeqCst);
        1
    }
    unsafe extern "system" fn ignore_all(_ctrl: u32) -> i32 {
        1
    }
    unsafe {
        SetConsoleCtrlHandler(Some(if ignore { ignore_all } else { handler }), 1);
    }
}

#[cfg(unix)]
fn install_stop_handler(ignore: bool) {
    extern "C" fn handler(_: libc::c_int) {
        STOP.store(true, Ordering::SeqCst);
    }
    unsafe {
        if ignore {
            libc::signal(libc::SIGTERM, libc::SIG_IGN);
        } else {
            libc::signal(libc::SIGTERM, handler as *const () as libc::sighandler_t);
        }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let get = |k: &str| args.iter().position(|a| a == k).and_then(|i| args.get(i + 1)).cloned();
    let has = |k: &str| args.iter().any(|a| a == k);
    let marker = get("--marker");
    install_stop_handler(has("--ignore-stop"));
    mark(&marker, "start");
    let restarts: u32 = std::env::var("TEND_RESTARTS").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
    if let Some(n) = get("--fail-until-restarts").and_then(|v| v.parse::<u32>().ok()) {
        if restarts < n {
            eprintln!("failing on purpose (restart {restarts} of {n})");
            std::process::exit(3);
        }
    }
    if let Some(t) = get("--say") {
        println!("{t}");
    }
    if let Some(f) = get("--grandchild") {
        // A long-lived process of our own, to check that stopping the service reaches it.
        let me = std::env::current_exe().unwrap();
        // Deliberately never waited for: the test checks that the supervisor kills it.
        #[allow(clippy::zombie_processes)]
        let c = std::process::Command::new(me).args(["--exit-after", "60000"]).spawn().unwrap();
        std::fs::write(f, c.id().to_string()).unwrap();
    }
    if let Some(ms) = get("--ready-after").and_then(|v| v.parse::<u64>().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
        println!("ready");
    }
    let _listener = get("--listen").map(|a| std::net::TcpListener::bind(a).unwrap());
    let start = Instant::now();
    let exit_after = get("--exit-after").and_then(|v| v.parse::<u64>().ok()).map(Duration::from_millis);
    let chatter = get("--chatter").and_then(|v| v.parse::<u64>().ok()).map(Duration::from_millis);
    let mut last_chat = Instant::now();
    let mut n = 0u64;
    loop {
        if STOP.load(Ordering::SeqCst) {
            mark(&marker, "stop");
            println!("stopping on request");
            std::process::exit(0);
        }
        if exit_after.is_some_and(|d| start.elapsed() >= d) {
            let code: i32 = get("--code").and_then(|v| v.parse().ok()).unwrap_or(0);
            mark(&marker, "exit");
            std::process::exit(code);
        }
        if let Some(c) = chatter {
            if last_chat.elapsed() >= c {
                n += 1;
                println!("tick {n}");
                last_chat = Instant::now();
            }
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
