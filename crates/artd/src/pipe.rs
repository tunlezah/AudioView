//! Reading the shairport-sync metadata FIFO.
//!
//! Runs on a blocking thread and forwards decoded inputs to the async core.
//!
//! The one design decision worth restating (DESIGN §5.1): we do **not** hold
//! a spare write descriptor open to suppress EOF. EOF is the single most
//! reliable signal that shairport-sync has gone away, and trading it for a
//! tidier read loop would leave the amp powered and the screen lit after a
//! crash. We take the reopen churn instead.

use std::ffi::CString;
use std::fs::{File, OpenOptions};
use std::io::Read;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::path::Path;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use spmeta::Decoder;
use tokio::sync::mpsc;

use crate::machine::Input;

const BUF: usize = 64 * 1024;

/// Create the FIFO if it is missing.
///
/// `artd` is ordered before shairport-sync precisely so this exists and is
/// being read before any metadata can be written — shairport-sync discards
/// metadata written while no reader is attached, which would otherwise lose
/// the first bundle of every boot.
#[allow(unsafe_code)]
pub fn ensure_fifo(path: &Path) -> Result<()> {
    match std::fs::metadata(path) {
        Ok(m) if m.file_type().is_fifo() => return Ok(()),
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
    // SAFETY: `c` is a valid NUL-terminated string living across the call.
    if unsafe { libc::mkfifo(c.as_ptr(), 0o660) } != 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| format!("mkfifo {}", path.display()));
    }
    Ok(())
}

fn open_nonblocking(path: &Path) -> Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))
}

#[allow(unsafe_code)]
fn wait_readable(f: &File, timeout: Duration) -> bool {
    let mut pfd = libc::pollfd {
        fd: f.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one valid pollfd whose fd is owned by `f` for the call.
    unsafe { libc::poll(&mut pfd, 1, timeout.as_millis() as i32) > 0 }
}

/// Read the pipe forever, sending decoded inputs downstream.
///
/// Blocking; run under `spawn_blocking`. Returns only if the channel closes.
pub fn run(path: &Path, tx: mpsc::Sender<Input>) -> Result<()> {
    ensure_fifo(path)?;
    let mut file = open_nonblocking(path)?;
    let mut decoder = Decoder::new();
    let mut buf = vec![0u8; BUF];
    let mut attached = false;

    loop {
        if !wait_readable(&file, Duration::from_millis(250)) {
            if tx.is_closed() {
                return Ok(());
            }
            continue;
        }

        match file.read(&mut buf) {
            Ok(0) => {
                if attached {
                    attached = false;
                    tracing::info!("metadata pipe writer closed");
                    if tx.blocking_send(Input::PipeEof).is_err() {
                        return Ok(());
                    }
                    // A new writer starts a new stream; do not let a partial
                    // item from the old one corrupt the first item of the new.
                    decoder = Decoder::new();
                }
                file = open_nonblocking(path)?;
                // With no writer attached a non-blocking read returns EOF
                // immediately, so back off rather than spinning.
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(n) => {
                if !attached {
                    attached = true;
                    tracing::info!("metadata pipe writer attached");
                }
                decoder.feed(&buf[..n]);
                while let Some(result) = decoder.next_event() {
                    let input = match result {
                        Ok(ev) => Input::Meta(ev),
                        Err(e) => {
                            tracing::warn!("metadata parse error: {e}");
                            Input::ParseError
                        }
                    };
                    if tx.blocking_send(input).is_err() {
                        return Ok(());
                    }
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e).context("reading the metadata pipe"),
        }
    }
}
