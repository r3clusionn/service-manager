//! Log files: one per service plus one for the supervisor, each line timestamped, rotated by size.
//!
//! ```text
//! 2026-10-03T14:05:09.117Z out  listening on 127.0.0.1:8000
//! 2026-10-03T14:05:12.480Z err  warning: cache is cold
//! ```
//!
//! When a file would grow past the size limit it is renamed to `NAME.log.1` (older ones move up to
//! `.2`, `.3`, ...; the oldest beyond the limit is deleted) and a new file is started.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// UTC time as `YYYY-MM-DDTHH:MM:SS.mmmZ`.
pub fn timestamp(t: SystemTime) -> String {
    let d = t.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = (secs / 86_400) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}T{h:02}:{m:02}:{s:02}.{:03}Z", d.subsec_millis())
}

pub struct LogFile {
    path: PathBuf,
    file: File,
    size: u64,
    max: u64,
    keep: usize,
}

impl LogFile {
    pub fn open(path: &Path, max: u64, keep: usize) -> io::Result<LogFile> {
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir)?;
        }
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        let size = file.metadata()?.len();
        Ok(LogFile { path: path.to_path_buf(), file, size, max: max.max(1), keep })
    }

    fn numbered(&self, n: usize) -> PathBuf {
        let mut s = self.path.as_os_str().to_owned();
        s.push(format!(".{n}"));
        PathBuf::from(s)
    }

    fn rotate(&mut self) -> io::Result<()> {
        if self.keep == 0 {
            // No old files kept: start the file again.
            self.file = File::create(&self.path)?;
            self.size = 0;
            return Ok(());
        }
        let _ = fs::remove_file(self.numbered(self.keep));
        for n in (1..self.keep).rev() {
            let from = self.numbered(n);
            if from.exists() {
                fs::rename(&from, self.numbered(n + 1))?;
            }
        }
        // Close before renaming (Windows cannot rename an open file).
        self.file = File::create(self.numbered(0).with_extension("swap"))?;
        fs::rename(&self.path, self.numbered(1))?;
        let _ = fs::remove_file(self.numbered(0).with_extension("swap"));
        self.file = OpenOptions::new().create(true).append(true).open(&self.path)?;
        self.size = 0;
        Ok(())
    }

    /// Writes one timestamped line; `tag` says where it came from (`out`, `err`, or for the
    /// supervisor's own log, the service name).
    pub fn line(&mut self, tag: &str, text: &str) -> io::Result<()> {
        let line = format!("{} {tag:<4} {text}\n", timestamp(SystemTime::now()));
        if self.size > 0 && self.size + line.len() as u64 > self.max {
            self.rotate()?;
        }
        self.file.write_all(line.as_bytes())?;
        self.size += line.len() as u64;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn timestamps() {
        assert_eq!(timestamp(UNIX_EPOCH), "1970-01-01T00:00:00.000Z");
        assert_eq!(timestamp(UNIX_EPOCH + Duration::from_millis(951_782_400_123)), "2000-02-29T00:00:00.123Z");
        assert_eq!(
            timestamp(UNIX_EPOCH + Duration::from_secs(1_790_899_200 + 13 * 3600 + 5 * 60 + 9)),
            "2026-10-02T13:05:09.000Z"
        );
    }

    #[test]
    fn lines_are_timestamped_and_tagged() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("svc.log");
        let mut l = LogFile::open(&p, 1 << 20, 3).unwrap();
        l.line("out", "hello").unwrap();
        l.line("err", "oops").unwrap();
        let text = fs::read_to_string(&p).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("Z out  hello") && lines[1].ends_with("Z err  oops"), "{text}");
        // Reopening appends.
        let mut l = LogFile::open(&p, 1 << 20, 3).unwrap();
        l.line("out", "again").unwrap();
        assert_eq!(fs::read_to_string(&p).unwrap().lines().count(), 3);
    }

    #[test]
    fn rotation_keeps_the_newest_files() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("svc.log");
        // Each line is 24 (timestamp) + 1 + 4 + 1 + 4 + 1 = 35 bytes; the limit holds two.
        let mut l = LogFile::open(&p, 75, 2).unwrap();
        for i in 0..9 {
            l.line("out", &format!("n{i:03}")).unwrap();
        }
        let read = |q: &Path| fs::read_to_string(q).unwrap().lines().map(|x| x[x.len() - 4..].to_string()).collect::<Vec<_>>();
        assert_eq!(read(&p), ["n008"]);
        assert_eq!(read(&d.path().join("svc.log.1")), ["n006", "n007"]);
        assert_eq!(read(&d.path().join("svc.log.2")), ["n004", "n005"]);
        assert!(!d.path().join("svc.log.3").exists());
        assert!(fs::read_dir(d.path()).unwrap().count() == 3, "no temporary files left");
    }

    #[test]
    fn a_line_longer_than_the_limit_is_still_written() {
        let d = tempfile::tempdir().unwrap();
        let p = d.path().join("svc.log");
        let mut l = LogFile::open(&p, 10, 1).unwrap();
        l.line("out", &"x".repeat(100)).unwrap();
        l.line("out", "short").unwrap();
        assert!(fs::read_to_string(&p).unwrap().contains("short"));
        assert!(fs::read_to_string(d.path().join("svc.log.1")).unwrap().contains(&"x".repeat(100)));
    }
}
