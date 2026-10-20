//! Diagnostics setup: `tracing` events go to stderr, filtered by `UAPP_LOG`
//! (an `EnvFilter` directive such as `debug` or `uapp::server=trace`) or else
//! by the `-v`/`-q` count. The native window has no console on Windows, so
//! that build can also append to a size-capped file in the per-user data dir
//! (`<data_local>/uapp/logs/uapp.log`) and record panics there.
//!
//! Plain user-facing CLI output ("uapp: serving at ...", the final error
//! before exit) is printed directly and doesn't go through here.

use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tracing_subscriber::{fmt, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter, Layer};

/// Rotate the log file once it passes this size.
const MAX_BYTES: u64 = 4 * 1024 * 1024;
/// Rotated files kept beside the live one (`uapp.log.1`, `uapp.log.2`).
const KEEP: usize = 2;

/// Where the window build writes its log.
pub fn log_path() -> PathBuf {
    crate::registry::local_root().join("logs").join("uapp.log")
}

/// Install the global subscriber. `verbosity` is the number of `-v` minus the
/// number of `-q` (0 = info). With `to_file`, events (and panics) are also
/// written to [`log_path`]. Safe to call more than once; later calls are
/// ignored.
pub fn init(verbosity: i8, to_file: bool) {
    let level = match verbosity {
        i8::MIN..=-2 => "error",
        -1 => "warn",
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    // Other crates stay at warn unless UAPP_LOG asks for more: at debug the
    // HTTP stack alone drowns out everything else.
    let filter = || {
        std::env::var("UAPP_LOG")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .and_then(|v| EnvFilter::try_new(v).ok())
            .unwrap_or_else(|| EnvFilter::new(format!("warn,uapp={level}")))
    };
    let stderr = fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(std::io::stderr().is_terminal())
        .with_target(false)
        .with_filter(filter());
    let file = if to_file {
        RotatingFile::open(log_path()).map(|f| {
            let f = Arc::new(Mutex::new(f));
            install_panic_hook(f.clone());
            fmt::layer()
                .with_writer(FileWriter(f))
                .with_ansi(false)
                .with_filter(filter())
        })
    } else {
        None
    };
    let _ = tracing_subscriber::registry()
        .with(stderr)
        .with(file)
        .try_init();
}

/// A log file that starts over (keeping [`KEEP`] old ones) when it grows past
/// [`MAX_BYTES`].
struct RotatingFile {
    path: PathBuf,
    file: std::fs::File,
    size: u64,
    max: u64,
}

impl RotatingFile {
    fn open(path: PathBuf) -> Option<RotatingFile> {
        std::fs::create_dir_all(path.parent()?).ok()?;
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)
            .ok()?;
        let size = file.metadata().map(|m| m.len()).unwrap_or(0);
        Some(RotatingFile {
            path,
            file,
            size,
            max: MAX_BYTES,
        })
    }

    fn rotate(&mut self) {
        let numbered = |n: usize| -> PathBuf {
            let mut s = self.path.clone().into_os_string();
            s.push(format!(".{n}"));
            PathBuf::from(s)
        };
        for n in (1..KEEP).rev() {
            let _ = std::fs::rename(numbered(n), numbered(n + 1));
        }
        let _ = std::fs::rename(&self.path, numbered(1));
        if let Ok(f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            self.file = f;
            self.size = 0;
        }
    }

    fn write_all(&mut self, buf: &[u8]) {
        if self.size + buf.len() as u64 > self.max && self.size > 0 {
            self.rotate();
        }
        if self.file.write_all(buf).is_ok() {
            self.size += buf.len() as u64;
        }
    }
}

/// `MakeWriter` for the shared log file: each event is formatted into one
/// buffer and written with a single call when the writer drops, so lines from
/// different threads never interleave.
#[derive(Clone)]
struct FileWriter(Arc<Mutex<RotatingFile>>);

struct EventBuf {
    file: Arc<Mutex<RotatingFile>>,
    buf: Vec<u8>,
}

impl Write for EventBuf {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.buf.extend_from_slice(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Drop for EventBuf {
    fn drop(&mut self) {
        if !self.buf.is_empty() {
            let mut f = self.file.lock().unwrap_or_else(|e| e.into_inner());
            f.write_all(&self.buf);
        }
    }
}

impl<'a> fmt::MakeWriter<'a> for FileWriter {
    type Writer = EventBuf;
    fn make_writer(&'a self) -> EventBuf {
        EventBuf {
            file: self.0.clone(),
            buf: Vec::new(),
        }
    }
}

/// Write panics to the log file too (then run the default hook, which prints
/// to stderr). A Windows window build has no stderr to print to, so without
/// this a crash leaves nothing behind.
fn install_panic_hook(file: Arc<Mutex<RotatingFile>>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let backtrace = std::backtrace::Backtrace::force_capture();
        let line = format!(
            "{} PANIC thread '{}' {info}\n{backtrace}\n",
            now_utc(),
            thread.name().unwrap_or("<unnamed>"),
        );
        // try_lock: a panic while the file lock is held must not deadlock.
        if let Ok(mut f) = file.try_lock() {
            f.write_all(line.as_bytes());
        }
        previous(info);
    }));
}

/// Seconds since the epoch, for the panic line (the event lines carry the
/// formatter's own timestamp).
fn now_utc() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("[unix {secs}]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotates_and_keeps_two() {
        let dir = std::env::temp_dir().join(format!("uapp-log-{}", fastrand::u32(..)));
        let path = dir.join("uapp.log");
        let mut f = RotatingFile::open(path.clone()).unwrap();
        f.max = 10;
        for line in ["aaaaaaaa\n", "bbbbbbbb\n", "cccccccc\n", "dddddddd\n"] {
            f.write_all(line.as_bytes());
        }
        let read = |name: &str| std::fs::read_to_string(dir.join(name)).unwrap_or_default();
        assert_eq!(read("uapp.log"), "dddddddd\n");
        assert_eq!(read("uapp.log.1"), "cccccccc\n");
        assert_eq!(read("uapp.log.2"), "bbbbbbbb\n");
        assert!(!dir.join("uapp.log.3").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
