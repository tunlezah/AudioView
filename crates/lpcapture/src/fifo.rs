//! FIFO plumbing for live capture and replay.
//!
//! The awkward parts of a named pipe are all here: it may not exist yet, the
//! writer may come and go, and forwarding to a downstream FIFO must not block
//! forever when nothing is reading it.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};

const BUF: usize = 64 * 1024;

/// Create a FIFO if nothing is there yet. Leaves existing FIFOs alone.
pub fn ensure_fifo(path: &Path) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(m) if m.file_type().is_fifo() => return Ok(()),
        // Never clobber a regular file that happens to sit at this path —
        // pointing this at the wrong thing should fail, not delete it.
        Ok(_) => bail!("{} exists and is not a FIFO", path.display()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e).with_context(|| format!("stat {}", path.display())),
    }
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            std::fs::create_dir_all(parent)?;
        }
    }
    let c =
        CString::new(path.as_os_str().as_encoded_bytes()).context("path contains a NUL byte")?;
    // SAFETY: `c` is a valid NUL-terminated string that outlives the call.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o660) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("mkfifo {}", path.display()));
    }
    Ok(())
}

fn open_read_nonblocking(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

/// Wait for readability. Returns false on timeout.
fn wait_readable(f: &File, timeout: Duration) -> Result<bool> {
    let mut pfd = libc::pollfd {
        fd: f.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    let ms = timeout.as_millis().min(i32::MAX as u128) as i32;
    // SAFETY: single valid pollfd, count matches, fd owned by `f`.
    let rc = unsafe { libc::poll(&mut pfd, 1, ms) };
    if rc < 0 {
        let e = std::io::Error::last_os_error();
        if e.kind() == std::io::ErrorKind::Interrupted {
            return Ok(false);
        }
        return Err(e).context("poll");
    }
    Ok(rc > 0)
}

/// Read the pipe until interrupted, calling `on_chunk` for every read.
///
/// A writer that exits closes the pipe, which surfaces as EOF. That is
/// meaningful — for `artd` it means shairport-sync is gone (DESIGN §5.1) —
/// so we surface the reopen count rather than papering over it by holding a
/// spare write descriptor open.
///
/// Returns the number of distinct writer sessions observed.
pub fn read_loop(
    path: &Path,
    idle_timeout_secs: Option<u64>,
    mut on_chunk: impl FnMut(&[u8]) -> Result<()>,
) -> Result<u32> {
    ensure_fifo(path)?;
    let idle_timeout = idle_timeout_secs.map(Duration::from_secs);

    let mut file = open_read_nonblocking(path)?;
    let mut buf = vec![0u8; BUF];
    let mut sessions = 0u32;
    let mut had_data = false;
    let mut last_data = Instant::now();

    loop {
        if let Some(limit) = idle_timeout {
            if last_data.elapsed() >= limit {
                return Ok(sessions);
            }
        }

        if !wait_readable(&file, Duration::from_millis(250))? {
            continue;
        }

        match file.read(&mut buf) {
            Ok(0) => {
                // Writer gone. Reopen so the next session is picked up, and
                // pause briefly: with no writer attached, a non-blocking read
                // returns EOF immediately and would spin.
                if had_data {
                    had_data = false;
                }
                file = open_read_nonblocking(path)?;
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(n) => {
                if !had_data {
                    sessions += 1;
                    had_data = true;
                }
                last_data = Instant::now();
                on_chunk(&buf[..n])?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("reading the metadata pipe"),
        }
    }
}

/// Open a path for writing, waiting for a reader if it is a FIFO.
pub fn open_for_write(path: &Path) -> Result<File> {
    if std::fs::metadata(path).is_ok_and(|m| m.file_type().is_fifo()) {
        eprintln!("lpcapture: waiting for a reader on {}", path.display());
    }
    OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {} for write", path.display()))
}

/// The capture file, plus its optional timing sidecar.
pub struct CaptureSink {
    file: File,
    timing: Option<File>,
    started: Instant,
    bytes: u64,
}

impl CaptureSink {
    pub fn create(path: &Path, timing: bool) -> Result<Self> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;
        let timing = if timing {
            Some(File::create(path.with_extension("timing"))?)
        } else {
            None
        };
        Ok(CaptureSink {
            file,
            timing,
            started: Instant::now(),
            bytes: 0,
        })
    }

    pub fn write(&mut self, chunk: &[u8]) -> Result<()> {
        self.file.write_all(chunk)?;
        // Flushed per chunk: capture sessions are ended with ctrl-c, and a
        // buffered tail lost to SIGINT would be a capture that silently
        // differs from what actually came down the pipe.
        self.file.flush()?;
        if let Some(t) = self.timing.as_mut() {
            let line = format!(
                "{{\"at_ms\":{},\"len\":{}}}\n",
                self.started.elapsed().as_millis(),
                chunk.len()
            );
            t.write_all(line.as_bytes())?;
            t.flush()?;
        }
        self.bytes += chunk.len() as u64;
        Ok(())
    }

    pub fn finish(self) -> Result<u64> {
        Ok(self.bytes)
    }
}

/// A downstream FIFO that receives a copy of everything captured.
pub struct Fanout {
    path: PathBuf,
    file: Option<File>,
    warned: bool,
}

impl Fanout {
    pub fn create(path: &Path) -> Result<Self> {
        ensure_fifo(path)?;
        Ok(Fanout {
            path: path.to_path_buf(),
            file: None,
            warned: false,
        })
    }

    /// Forward a chunk, tolerating there being no reader.
    ///
    /// Opening non-blocking fails with ENXIO while nothing is reading, and a
    /// reader that goes away gives EPIPE. Neither is fatal: the capture is
    /// the deliverable, and the fanout is a convenience.
    pub fn write(&mut self, chunk: &[u8]) -> Result<()> {
        if self.file.is_none() {
            match OpenOptions::new()
                .write(true)
                .custom_flags(libc::O_NONBLOCK)
                .open(&self.path)
            {
                Ok(f) => {
                    self.file = Some(f);
                    self.warned = false;
                }
                Err(_) => {
                    if !self.warned {
                        eprintln!(
                            "lpcapture: nothing reading {}; buffering nothing, continuing",
                            self.path.display()
                        );
                        self.warned = true;
                    }
                    return Ok(());
                }
            }
        }

        if let Some(f) = self.file.as_mut() {
            if let Err(e) = f.write_all(chunk) {
                eprintln!("lpcapture: fanout write failed ({e}); will retry on next chunk");
                self.file = None;
            }
        }
        Ok(())
    }
}

/// Recorded inter-read timing, so a replay can reproduce real pacing.
pub struct Timing {
    entries: Vec<(u64, usize)>,
}

impl Timing {
    pub fn load(path: &Path) -> Result<Option<Timing>> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Ok(None);
        };
        let mut entries = Vec::new();
        for line in text.lines().filter(|l| !l.trim().is_empty()) {
            let v: serde_json::Value = serde_json::from_str(line)
                .with_context(|| format!("parsing timing line: {line}"))?;
            let at = v.get("at_ms").and_then(|x| x.as_u64()).unwrap_or(0);
            let len = v.get("len").and_then(|x| x.as_u64()).unwrap_or(0) as usize;
            entries.push((at, len));
        }
        Ok(Some(Timing { entries }))
    }

    /// `(start, end, delay_before_ms)` spans covering `total` bytes.
    ///
    /// A capture that was packed is shorter than what was recorded, so the
    /// spans are clipped and any remainder is emitted as a final chunk.
    pub fn chunks(&self, total: usize) -> Vec<(usize, usize, u64)> {
        let mut out = Vec::with_capacity(self.entries.len());
        let mut off = 0usize;
        let mut prev_at = 0u64;
        for (at, len) in &self.entries {
            if off >= total {
                break;
            }
            let end = (off + len).min(total);
            out.push((off, end, at.saturating_sub(prev_at)));
            prev_at = *at;
            off = end;
        }
        if off < total {
            out.push((off, total, 0));
        }
        out
    }
}
