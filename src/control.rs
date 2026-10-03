//! Talking to a running supervisor: one JSON line in, one JSON line out, over a TCP connection to
//! the configured address (127.0.0.1:7340 by default).
//!
//! Every request carries a token: 32 random bytes, hex-encoded, that the supervisor writes to
//! `tend.token` in its log directory when it starts. Anyone who can read that file can control the
//! supervisor; anyone who cannot, cannot, even though the port is open to every local user.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::supervisor::{Handle, Request, Response};

#[derive(Serialize, Deserialize)]
struct Envelope {
    token: String,
    #[serde(flatten)]
    request: Request,
}

pub fn token_path(log_dir: &Path) -> PathBuf {
    log_dir.join("tend.token")
}

/// Creates a fresh token and writes it next to the logs.
pub fn new_token(log_dir: &Path) -> std::io::Result<String> {
    let mut b = [0u8; 32];
    getrandom::fill(&mut b).map_err(|e| std::io::Error::other(e.to_string()))?;
    let t: String = b.iter().map(|x| format!("{x:02x}")).collect();
    std::fs::write(token_path(log_dir), &t)?;
    Ok(t)
}

/// Compares without stopping at the first difference, so timing reveals nothing about the token.
fn same(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Serves requests until the process exits. Binds before returning, so a port that is taken is
/// reported to the caller.
pub fn serve(addr: &str, token: String, handle: Handle) -> std::io::Result<()> {
    let listener = TcpListener::bind(addr)?;
    std::thread::spawn(move || {
        for conn in listener.incoming().flatten() {
            let (token, handle) = (token.clone(), handle.clone());
            std::thread::spawn(move || {
                let _ = answer(conn, &token, &handle);
            });
        }
    });
    Ok(())
}

fn answer(conn: TcpStream, token: &str, handle: &Handle) -> std::io::Result<()> {
    conn.set_read_timeout(Some(Duration::from_secs(5)))?;
    let mut line = String::new();
    BufReader::new((&conn).take(64 * 1024)).read_line(&mut line)?;
    let response = match serde_json::from_str::<Envelope>(&line) {
        Ok(env) if same(&env.token, token) => handle.request(env.request),
        Ok(_) => Response::Error { message: "wrong token".into() },
        Err(e) => Response::Error { message: format!("bad request: {e}") },
    };
    let mut out = serde_json::to_string(&response).unwrap();
    out.push('\n');
    (&conn).write_all(out.as_bytes())
}

/// Sends one request to the supervisor described by `addr` and `log_dir`.
pub fn send(addr: &str, log_dir: &Path, request: Request) -> Result<Response, String> {
    let token = std::fs::read_to_string(token_path(log_dir))
        .map_err(|_| format!("no supervisor seems to be running ({} is missing)", token_path(log_dir).display()))?;
    let mut conn = TcpStream::connect(addr).map_err(|e| format!("cannot reach the supervisor at {addr}: {e}"))?;
    conn.set_read_timeout(Some(Duration::from_secs(15))).ok();
    let mut msg = serde_json::to_string(&Envelope { token: token.trim().to_string(), request }).unwrap();
    msg.push('\n');
    conn.write_all(msg.as_bytes()).map_err(|e| e.to_string())?;
    let mut line = String::new();
    BufReader::new(conn).read_line(&mut line).map_err(|e| e.to_string())?;
    serde_json::from_str(&line).map_err(|e| format!("unreadable answer from the supervisor: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_serialise_as_documented() {
        let e = Envelope { token: "t".into(), request: Request::Restart { name: "web".into() } };
        assert_eq!(serde_json::to_string(&e).unwrap(), r#"{"token":"t","cmd":"restart","name":"web"}"#);
        let s: Envelope = serde_json::from_str(r#"{"token":"x","cmd":"status"}"#).unwrap();
        assert_eq!(s.request, Request::Status);
    }

    #[test]
    fn token_comparison() {
        assert!(same("abc", "abc"));
        assert!(!same("abc", "abd"));
        assert!(!same("abc", "ab"));
        assert!(!same("", "a"));
    }

    #[test]
    fn tokens_are_fresh_and_written() {
        let d = tempfile::tempdir().unwrap();
        let a = new_token(d.path()).unwrap();
        let b = new_token(d.path()).unwrap();
        assert_eq!(a.len(), 64);
        assert_ne!(a, b);
        assert_eq!(std::fs::read_to_string(token_path(d.path())).unwrap(), b);
    }
}
