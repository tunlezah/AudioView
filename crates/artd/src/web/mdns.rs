//! Advertising the web interface over mDNS (DESIGN §7.3).
//!
//! **What `web.mdns` actually does, plainly: it adds an `_http._tcp` service
//! record and nothing else.** Avahi is already on the device for AirPlay and
//! already publishes the hostname, so `http://lpframe.local:8730` resolves
//! whether this is on or off. What the service record adds is discovery —
//! the device turning up in a browser's or a phone's list of local services
//! rather than having to be typed.
//!
//! Implemented by spawning `avahi-publish`, which is the tool the daemon we
//! are already depending on ships with. A resident mDNS responder in-process
//! would be a second one on the same port for a feature this small, and the
//! Rust ones large enough to be correct are larger than everything else the
//! web interface adds put together.
//!
//! Absent, unusable or dying is tolerated silently past one warning: a device
//! nobody can discover still works when its address is typed, and this must
//! never be a reason for the daemon not to start.

use std::net::SocketAddr;
use std::process::Stdio;

/// A running `avahi-publish`, killed when this is dropped.
pub struct Advertisement {
    child: tokio::process::Child,
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        // `start_kill` rather than `kill().await`: Drop is not async, and the
        // child is a grandchild of pid 1 the moment we exit anyway.
        let _ = self.child.start_kill();
    }
}

/// Advertise the interface. `None` if Avahi is not available.
pub fn advertise(name: &str, addr: SocketAddr) -> Option<Advertisement> {
    let service = format!("{name} web");
    let child = tokio::process::Command::new("avahi-publish")
        .arg("--service")
        .arg(&service)
        .arg("_http._tcp")
        .arg(addr.port().to_string())
        .arg("path=/")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        // Otherwise a daemon restart leaves the responder behind holding the
        // old record, and the network sees two.
        .kill_on_drop(true)
        .spawn();

    match child {
        Ok(child) => {
            tracing::info!(
                "advertising {service:?} as _http._tcp on port {}",
                addr.port()
            );
            Some(Advertisement { child })
        }
        Err(e) => {
            tracing::warn!(
                "not advertising over mDNS ({e}); the page is still reachable by \
                 hostname if Avahi is publishing one, and always by address"
            );
            None
        }
    }
}
