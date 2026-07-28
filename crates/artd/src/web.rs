//! Read-only Now Playing and Diagnostics pages.
//!
//! The settings half of the interface, and authentication, land with
//! milestone 7 (DESIGN §7.3). Until then this **refuses to bind anywhere but
//! loopback**: an unauthenticated page on the LAN would expose listening
//! history to every device on the network, and shipping that with
//! `web.auth = true` sitting unimplemented in the config would be worse —
//! it would look protected.
//!
//! The SSE stream carries exactly the snapshot the renderer receives over the
//! Unix socket, from the same broadcaster with the same `seq`.

use std::net::SocketAddr;

use anyhow::{bail, Context, Result};
use axum::extract::State as AxumState;
use axum::http::{header, StatusCode};
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::hub::Hub;

const INDEX_HTML: &str = include_str!("web/index.html");

pub fn check_bind(bind: &str, auth_implemented: bool) -> Result<SocketAddr> {
    let addr: SocketAddr = bind
        .parse()
        .with_context(|| format!("web.bind {bind:?} is not a valid address:port"))?;
    if !auth_implemented && !addr.ip().is_loopback() {
        bail!(
            "web.bind is {addr}, but authentication is not implemented yet \
             (arrives with milestone 7). Refusing to expose an unauthenticated \
             page beyond loopback — set web.bind = \"127.0.0.1:{}\" and use an \
             SSH tunnel, or set web.enabled = false.",
            addr.port()
        );
    }
    Ok(addr)
}

pub async fn serve(hub: Hub, addr: SocketAddr) -> Result<()> {
    let app = Router::new()
        .route("/", get(index))
        .route("/api/state", get(api_state))
        .route("/api/events", get(api_events))
        .route("/api/diagnostics", get(api_diagnostics))
        .route("/api/artwork/current", get(api_artwork))
        .with_state(hub);

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("web interface on http://{addr}/");
    axum::serve(listener, app).await.context("web server")?;
    Ok(())
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn api_state(AxumState(hub): AxumState<Hub>) -> Response {
    let (seq, state) = hub.snapshot();
    axum::Json(serde_json::json!({ "seq": seq, "state": state })).into_response()
}

async fn api_diagnostics(AxumState(hub): AxumState<Hub>) -> Response {
    let c = hub.counters();
    axum::Json(serde_json::json!({
        "counters": {
            "sessions": c.sessions,
            "tracks": c.tracks,
            "artwork_updates": c.artwork_updates,
            "artwork_duplicates": c.artwork_duplicates,
            "parse_errors": c.parse_errors,
            "pipe_eofs": c.pipe_eofs,
            "stall_timeouts": c.stall_timeouts,
            "session_timeouts": c.session_timeouts,
        },
        // Every enrichment decision reaches the page twice: as a tally here,
        // and as a note carrying the scores behind it (DESIGN §7.3).
        "enrichment": {
            "attempted": c.enrichment.attempted,
            "cache_hits": c.enrichment.cache_hits,
            "negative_cache_hits": c.enrichment.negative_cache_hits,
            "text_rejections": c.enrichment.text_rejections,
            "size_rejections": c.enrichment.size_rejections,
            "perceptual_rejections": c.enrichment.perceptual_rejections,
            "upgrades": c.enrichment.upgrades,
            "network_errors": c.enrichment.network_errors,
            "rate_limited": c.enrichment.rate_limited,
        },
        "notes": hub.notes(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

async fn api_events(
    AxumState(hub): AxumState<Hub>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let initial = hub.current_message();
    let stream = BroadcastStream::new(hub.subscribe()).filter_map(|msg| {
        // A lagged receiver yields an error; skip it, because the next
        // snapshot is complete and makes the client correct again.
        msg.ok()
            .and_then(|m| serde_json::to_string(&*m).ok())
            .map(|json| Ok(Event::default().data(json)))
    });

    let first = tokio_stream::once(Ok(
        Event::default().data(serde_json::to_string(&initial).unwrap_or_else(|_| "{}".into()))
    ));

    Sse::new(first.chain(stream)).keep_alive(axum::response::sse::KeepAlive::default())
}

/// The current artwork, as bytes.
///
/// Served from the published path rather than an arbitrary one: this handler
/// must never become a way to read any file on the device.
async fn api_artwork(AxumState(hub): AxumState<Hub>) -> Response {
    let (_, state) = hub.snapshot();
    let Some(art) = state.artwork else {
        return StatusCode::NOT_FOUND.into_response();
    };
    match tokio::fs::read(&art.path).await {
        Ok(bytes) => {
            let mime = crate::artwork::ImageKind::sniff(&bytes).mime();
            (
                [
                    (header::CONTENT_TYPE, mime),
                    // Content-addressed by revision, so caching is safe and
                    // the page never shows the previous track's art.
                    (header::CACHE_CONTROL, "no-cache"),
                    (header::ETAG, ""),
                ],
                bytes,
            )
                .into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_binds_are_accepted() {
        assert!(check_bind("127.0.0.1:8730", false).is_ok());
        assert!(check_bind("[::1]:8730", false).is_ok());
    }

    #[test]
    fn non_loopback_binds_are_refused_until_auth_exists() {
        let err = check_bind("0.0.0.0:8730", false).unwrap_err().to_string();
        assert!(err.contains("authentication is not implemented"), "{err}");
        assert!(err.contains("127.0.0.1:8730"), "{err}");

        // Once auth lands, the same bind is fine.
        assert!(check_bind("0.0.0.0:8730", true).is_ok());
    }

    #[test]
    fn a_malformed_bind_is_rejected() {
        assert!(check_bind("lpframe.local:8730", true).is_err());
    }
}
