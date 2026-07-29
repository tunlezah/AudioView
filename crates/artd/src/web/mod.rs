//! The web interface (DESIGN §7.3).
//!
//! With no buttons, no screen controls and no keyboard, this is the only way
//! to configure a running device short of SSH — which is why it is
//! authenticated, why it is LAN-bound by default, and why every write goes
//! through the same validated, atomic path the daemon reads back at boot.
//!
//! The SSE stream carries exactly the snapshot the renderer receives over the
//! Unix socket, from the same broadcaster with the same `seq`.
//!
//! The threat model this authenticates against, and the several things it
//! deliberately does not, are written out in [`auth`]. Read that before
//! changing anything in here.

pub mod auth;
pub mod mdns;
pub mod settings;

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path as FsPath;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use axum::extract::{ConnectInfo, Path, Request, State as AxumState};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::sse::{Event, Sse};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use lpframe_config::Config;
use serde_json::json;
use tokio_stream::wrappers::BroadcastStream;
use tokio_stream::{Stream, StreamExt};

use crate::hub::Hub;
use crate::ipc::Command;
use auth::Auth;
use settings::{Settings, Tier};

const INDEX_HTML: &str = include_str!("index.html");

const COOKIE_NAME: &str = "lpframe_session";

/// The header every mutating request must carry.
///
/// `SameSite=Strict` on the cookie already stops a cross-site form arriving
/// with credentials attached; this is the half that does not depend on the
/// browser getting `SameSite` right. A form post from another origin cannot
/// set a custom header at all, and anything that can — `fetch`, XHR — becomes
/// a preflighted cross-origin request that we never answer with the CORS
/// headers it would need (DESIGN §7.3).
const CSRF_HEADER: &str = "x-lpframe-request";

/// Paths served without a session.
///
/// `/` is the application shell: it contains no device data and shows a login
/// form when [`api_session`] says one is needed. Everything else, including
/// the event stream and the artwork, is behind the session — the listening
/// history is the thing being protected.
const PUBLIC_PATHS: [&str; 3] = ["/", "/api/session", "/api/login"];

/// A permitted listener, and the reason to be uneasy about it if there is one.
#[derive(Debug)]
pub struct Bind {
    pub addr: SocketAddr,
    /// Present only when the bind is allowed because `web.insecure_no_auth`
    /// is set. Logged prominently at every startup.
    pub warning: Option<String>,
}

/// Decide whether `web.bind` may be bound, given how auth is configured.
///
/// Call this **after** [`prepare_auth`], so that "authentication is on" and
/// "there is a hash to check against" are the same statement. Failing here
/// fails the daemon: binding a surface the user did not intend is worse than
/// not starting, because not starting is visible and gets fixed.
pub fn check_bind(web: &lpframe_config::Web) -> Result<Bind> {
    let addr: SocketAddr = web
        .bind
        .parse()
        .with_context(|| format!("web.bind {:?} is not a valid address:port", web.bind))?;

    if addr.ip().is_loopback() {
        return Ok(Bind {
            addr,
            warning: None,
        });
    }

    if web.auth {
        if web.password_hash.is_empty() {
            bail!(
                "web.bind is {addr} and web.auth is on, but there is no password hash \
                 and one could not be generated. Refusing to serve the settings and \
                 the listening history to the network with nothing checking. Make \
                 /var/lib/lpframe writable, or set web.bind = \"127.0.0.1:{}\".",
                addr.port()
            );
        }
        return Ok(Bind {
            addr,
            warning: None,
        });
    }

    if web.insecure_no_auth {
        return Ok(Bind {
            addr,
            warning: Some(format!(
                "SERVING THE WEB INTERFACE ON {addr} WITH NO AUTHENTICATION. \
                 web.insecure_no_auth is set, so anything that can reach this port can \
                 read what you have been listening to and change every setting on the \
                 device. This is only sane behind something else that authenticates. \
                 Unset web.insecure_no_auth to undo it."
            )),
        });
    }

    bail!(
        "web.bind is {addr} with web.auth = false. Refusing to expose the settings and \
         the listening history to the network unauthenticated — set web.auth = true, or \
         web.bind = \"127.0.0.1:{}\" and use an SSH tunnel, or web.enabled = false. If \
         something in front of this authenticates for you, web.insecure_no_auth = true \
         says so deliberately.",
        addr.port()
    );
}

/// Resolve authentication, generating a password on first run.
///
/// Mutates `cfg.web.password_hash` so the caller's copy — and therefore
/// [`check_bind`] — sees the hash that is actually in force.
///
/// Failing to *persist* the hash or the password file is loud but not fatal:
/// the daemon's job is playing music and showing artwork, the password works
/// for this boot, and a device that will not start is a worse answer to a
/// read-only `/var/lib` than one that complains every time it starts.
pub fn prepare_auth(cfg: &mut Config, base: &FsPath, local: &FsPath) -> Result<Option<Arc<Auth>>> {
    if !cfg.web.auth {
        return Ok(None);
    }
    if !cfg.web.password_hash.is_empty() {
        return Ok(Some(Arc::new(Auth::new(cfg.web.password_hash.clone()))));
    }

    let passphrase = auth::generate_passphrase();
    let hash = auth::hash_password(&passphrase).context("generating the web password")?;

    // The plaintext file first. If the hash were committed and the file then
    // failed, the password would exist and be unlearnable; this way round the
    // worst case is a password regenerated at the next start.
    let dir = local.parent().unwrap_or(FsPath::new("."));
    let file = match auth::write_password_file(dir, &passphrase) {
        Ok(path) => Some(path),
        Err(e) => {
            tracing::error!("could not write the web password file: {e:#}");
            None
        }
    };

    let mut updates = BTreeMap::new();
    updates.insert(
        "web.password_hash".to_string(),
        toml::Value::String(hash.clone()),
    );
    if let Err(e) = lpframe_config::set_overrides(base, local, &updates) {
        tracing::error!(
            "could not store the web password hash ({e}); the password below works until \
             the daemon restarts, at which point a new one is generated"
        );
    }
    cfg.web.password_hash = hash.clone();

    // The one place the plaintext is logged, and the reason it is `warn`: on a
    // device with `logging.level = "info"` this line is the install
    // instructions, and it must not be the one that scrolled past.
    match &file {
        Some(path) => tracing::warn!(
            "web interface password: {passphrase}  (also in {}, mode 0600; reprint it \
             with `lpctl web-password`). Only the Argon2id hash is stored.",
            path.display()
        ),
        None => tracing::warn!(
            "web interface password: {passphrase}  (it could not be written to disk, so \
             this log line is the only copy). Only the Argon2id hash is stored."
        ),
    }

    Ok(Some(Arc::new(Auth::new(hash))))
}

/// Everything the handlers share.
#[derive(Clone)]
pub struct App {
    hub: Hub,
    /// `None` when `web.auth = false`, which [`check_bind`] has already
    /// refused to combine with a non-loopback listener.
    auth: Option<Arc<Auth>>,
    settings: Arc<Settings>,
}

impl App {
    pub fn new(hub: Hub, auth: Option<Arc<Auth>>, settings: Settings) -> App {
        App {
            hub,
            auth,
            settings: Arc::new(settings),
        }
    }

    /// Shorten the confirm-or-revert countdown. Tests only — fifteen real
    /// seconds per case is not a suite anyone runs.
    pub fn with_confirm_window(mut self, window: Duration) -> App {
        let settings = Arc::get_mut(&mut self.settings).expect("no handlers running yet");
        settings.confirm_window = window;
        self
    }
}

pub fn router(app: App) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/session", get(api_session))
        .route("/api/login", post(api_login))
        .route("/api/logout", post(api_logout))
        .route("/api/state", get(api_state))
        .route("/api/events", get(api_events))
        .route("/api/diagnostics", get(api_diagnostics))
        .route("/api/artwork/current", get(api_artwork))
        .route("/api/config", get(api_config).patch(api_config_patch))
        .route("/api/config/reset", post(api_config_reset))
        .route("/api/config/confirm", post(api_config_confirm))
        .route("/api/actions/:action", post(api_action))
        // Applied to the whole router rather than to individual handlers, so
        // a route added later is protected by default rather than by somebody
        // remembering to protect it.
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        .with_state(app)
}

pub async fn serve(app: App, addr: SocketAddr) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("binding {addr}"))?;
    tracing::info!("web interface on http://{addr}/");
    serve_on(app, listener).await
}

/// Serve on an already-bound listener, which is how the tests get a port.
pub async fn serve_on(app: App, listener: tokio::net::TcpListener) -> Result<()> {
    // `ConnectInfo` is what the login rate limiter counts against, so the
    // connect-info service is not optional here.
    axum::serve(
        listener,
        router(app).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await
    .context("web server")?;
    Ok(())
}

// --- the gate -------------------------------------------------------------

/// The one failure response.
///
/// A wrong password and a session that does not exist produce this same
/// status, body and header set. To a client entitled to be here both mean the
/// same thing — log in again — and to one that is not, the absence of a
/// difference is the point: probing cannot tell a guessed cookie from a
/// guessed password, or either from a session that simply expired.
fn unauthenticated() -> Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(json!({ "error": "authentication required" })),
    )
        .into_response()
}

fn refused(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "error": message }))).into_response()
}

fn session_token(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::COOKIE)?
        .to_str()
        .ok()?
        .split(';')
        .filter_map(|part| part.trim().split_once('='))
        .find(|(name, _)| *name == COOKIE_NAME)
        .map(|(_, value)| value)
}

/// The session cookie.
///
/// **`Secure` is deliberately absent, and must stay absent.** The device
/// serves plain HTTP on a LAN; there is no TLS for the flag to gate on, and a
/// `Secure` cookie is dropped by the browser on every plain-HTTP request,
/// which presents as "logging in appears to work and then nothing does".
/// Anyone terminating TLS in front of this wants their proxy to add the flag.
/// [`auth`] sets out what plain HTTP does and does not protect.
fn set_cookie(token: &str) -> String {
    format!(
        "{COOKIE_NAME}={token}; Path=/; HttpOnly; SameSite=Strict; Max-Age={}",
        auth::SESSION_LIFETIME.as_secs()
    )
}

fn clear_cookie() -> String {
    format!("{COOKIE_NAME}=; Path=/; HttpOnly; SameSite=Strict; Max-Age=0")
}

async fn guard(AxumState(app): AxumState<App>, req: Request, next: Next) -> Response {
    let mutating = !matches!(*req.method(), Method::GET | Method::HEAD);

    if mutating && !req.headers().contains_key(CSRF_HEADER) {
        return refused(
            StatusCode::BAD_REQUEST,
            "this request needs the X-LPFrame-Request header",
        );
    }

    if let Some(auth) = &app.auth {
        if !PUBLIC_PATHS.contains(&req.uri().path()) {
            let ok = session_token(req.headers())
                .is_some_and(|token| auth.validate(token, Instant::now()));
            if !ok {
                return unauthenticated();
            }
        }
    }

    next.run(req).await
}

// --- session --------------------------------------------------------------

async fn api_session(AxumState(app): AxumState<App>, headers: HeaderMap) -> Response {
    let authenticated = match &app.auth {
        None => true,
        Some(auth) => session_token(&headers).is_some_and(|t| auth.validate(t, Instant::now())),
    };
    Json(json!({
        "auth_required": app.auth.is_some(),
        "authenticated": authenticated,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct LoginBody {
    password: String,
}

async fn api_login(
    AxumState(app): AxumState<App>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(body): Json<LoginBody>,
) -> Response {
    let Some(auth) = app.auth.clone() else {
        return refused(StatusCode::BAD_REQUEST, "authentication is disabled");
    };
    let from = peer.ip();

    if !auth.note_attempt(from, Instant::now()) {
        tracing::warn!("web login rate limit engaged for {from}");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, "60")],
            Json(json!({ "error": "too many attempts; wait a minute" })),
        )
            .into_response();
    }

    // Argon2id at these parameters costs tens of milliseconds and 19 MiB,
    // which is the point of it; it does not belong on the async runtime.
    let verifier = auth.clone();
    let password = body.password;
    let ok = tokio::task::spawn_blocking(move || verifier.verify(&password))
        .await
        .unwrap_or(false);

    if !ok {
        tracing::warn!("failed web login from {from}");
        return unauthenticated();
    }

    auth.clear_attempts(from);
    let token = auth.issue(Instant::now());
    tracing::info!("web login from {from}");
    (
        StatusCode::OK,
        [(header::SET_COOKIE, set_cookie(&token))],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

async fn api_logout(AxumState(app): AxumState<App>, headers: HeaderMap) -> Response {
    if let (Some(auth), Some(token)) = (&app.auth, session_token(&headers)) {
        auth.revoke(token);
    }
    (
        StatusCode::OK,
        [(header::SET_COOKIE, clear_cookie())],
        Json(json!({ "ok": true })),
    )
        .into_response()
}

// --- read-only ------------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn api_state(AxumState(app): AxumState<App>) -> Response {
    let (seq, state) = app.hub.snapshot();
    Json(json!({ "seq": seq, "state": state })).into_response()
}

async fn api_diagnostics(AxumState(app): AxumState<App>) -> Response {
    let c = app.hub.counters();
    Json(json!({
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
        "notes": app.hub.notes(),
        "version": env!("CARGO_PKG_VERSION"),
    }))
    .into_response()
}

/// The snapshot stream.
///
/// The session is checked when the stream is opened and not again while it
/// runs, so a stream opened before a logout keeps delivering until the client
/// drops it or the daemon restarts. It carries only what the Now Playing page
/// already shows, and closing the tab ends it; re-checking on every snapshot
/// would be a lock per event on the daemon's hot path for that.
async fn api_events(
    AxumState(app): AxumState<App>,
) -> Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>> {
    let initial = app.hub.current_message();
    let stream = BroadcastStream::new(app.hub.subscribe()).filter_map(|msg| {
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
async fn api_artwork(AxumState(app): AxumState<App>) -> Response {
    let (_, state) = app.hub.snapshot();
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

// --- settings -------------------------------------------------------------

async fn api_config(AxumState(app): AxumState<App>) -> Response {
    let loaded = match app.settings.load() {
        Ok(l) => l,
        Err(e) => return refused(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    };
    let current = lpframe_config::flatten(&loaded.config);
    let defaults = lpframe_config::defaults();

    let items: Vec<serde_json::Value> = current
        .iter()
        // `web.password_hash` never leaves the daemon.
        .filter(|(key, _)| !settings::is_secret(key))
        .map(|(key, value)| {
            let mut item = json!({
                "key": key,
                "value": settings::toml_to_json(value),
                "default": defaults.get(key).map(settings::toml_to_json),
                "overridden": loaded.overridden.contains(key),
                "tier": settings::tier(key).as_str(),
            });
            let object = item.as_object_mut().expect("just constructed");
            if let Some(choices) = settings::choices(key) {
                object.insert("choices".into(), json!(choices));
            }
            if let Some(hint) = settings::hint(key) {
                object.insert("hint".into(), json!(hint));
            }
            item
        })
        .collect();

    Json(json!({
        "settings": items,
        "confirm_keys": settings::CONFIRM_KEYS,
        "confirm_seconds": app.settings.confirm_window.as_secs(),
        "units": { "renderer": settings::RENDERER_UNIT, "daemon": settings::DAEMON_UNIT },
        "systemd": settings::systemd_available(),
        "paths": {
            "base": app.settings.base.display().to_string(),
            "local": app.settings.local.display().to_string(),
        },
    }))
    .into_response()
}

async fn api_config_patch(
    AxumState(app): AxumState<App>,
    Json(body): Json<serde_json::Map<String, serde_json::Value>>,
) -> Response {
    if body.is_empty() {
        return refused(StatusCode::BAD_REQUEST, "no settings in the request");
    }

    let mut updates = BTreeMap::new();
    for (key, value) in &body {
        if settings::is_secret(key) {
            return refused(
                StatusCode::FORBIDDEN,
                "the password hash is not settable through this interface",
            );
        }
        let Some(value) = settings::json_to_toml(value) else {
            return refused(
                StatusCode::BAD_REQUEST,
                &format!("{key:?} was sent a value this setting cannot hold"),
            );
        };
        updates.insert(key.clone(), value);
    }

    // Recorded before the write, because afterwards the previous state is
    // gone and there is nothing exact left to put back.
    let touched: Vec<String> = settings::CONFIRM_KEYS
        .iter()
        .filter(|k| updates.contains_key(**k))
        .map(|k| k.to_string())
        .collect();
    let undo = if touched.is_empty() {
        BTreeMap::new()
    } else {
        match app.settings.snapshot_keys(&touched) {
            Ok(u) => u,
            Err(e) => return refused(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
        }
    };

    let loaded = match app.settings.apply(&updates) {
        Ok(l) => l,
        Err(e) => return config_error(e),
    };

    let mut response = json!({
        "applied": updates.keys().collect::<Vec<_>>(),
        "tiers": tiers_touched(updates.keys()),
        "overridden": visible_overrides(&loaded),
    });
    if !touched.is_empty() {
        let id = app.settings.arm(undo);
        let seconds = app.settings.confirm_window.as_secs();
        spawn_countdown(app.clone(), id);
        response["confirm"] = json!({ "id": id, "seconds": seconds, "keys": touched });
    }
    Json(response).into_response()
}

#[derive(serde::Deserialize)]
struct ResetBody {
    keys: Vec<String>,
}

async fn api_config_reset(AxumState(app): AxumState<App>, Json(body): Json<ResetBody>) -> Response {
    if body.keys.is_empty() {
        return refused(StatusCode::BAD_REQUEST, "no keys in the request");
    }
    if body.keys.iter().any(|k| settings::is_secret(k)) {
        return refused(
            StatusCode::FORBIDDEN,
            "the password hash is not resettable through this interface",
        );
    }
    match app.settings.reset(&body.keys) {
        Ok(loaded) => Json(json!({
            "reset": body.keys,
            "tiers": tiers_touched(body.keys.iter()),
            "overridden": visible_overrides(&loaded),
        }))
        .into_response(),
        Err(e) => config_error(e),
    }
}

#[derive(serde::Deserialize)]
struct ConfirmBody {
    id: u64,
}

async fn api_config_confirm(
    AxumState(app): AxumState<App>,
    Json(body): Json<ConfirmBody>,
) -> Response {
    let confirmed = app.settings.confirm(body.id);
    Json(json!({ "confirmed": confirmed })).into_response()
}

/// Undo a display change nobody confirmed.
///
/// The revert itself is a configuration write, so it works with or without
/// systemd; only the renderer restart that makes it visible needs one, and
/// when there is none the log says so rather than pretending.
fn spawn_countdown(app: App, id: u64) {
    let window = app.settings.confirm_window;
    tokio::spawn(async move {
        tokio::time::sleep(window).await;
        let Some(result) = app.settings.revert(id) else {
            return;
        };
        match result {
            Ok(keys) => {
                tracing::warn!(
                    "display change was not confirmed within {}s; put {} back",
                    window.as_secs(),
                    keys.join(", ")
                );
                let restart =
                    tokio::task::spawn_blocking(|| settings::restart_unit(settings::RENDERER_UNIT))
                        .await;
                if let Ok(Err(e)) = restart {
                    tracing::warn!("the revert is on disk but the renderer was not restarted: {e}");
                }
            }
            Err(e) => tracing::error!("could not revert an unconfirmed display change: {e}"),
        }
    });
}

/// Which keys are overridden, minus the ones the API does not admit to.
///
/// `web.password_hash` is always overridden on a device with authentication
/// on, and naming it here would be the one place the API mentions a setting
/// it otherwise refuses to read, write or list.
fn visible_overrides(loaded: &lpframe_config::Loaded) -> Vec<&String> {
    loaded
        .overridden
        .iter()
        .filter(|k| !settings::is_secret(k))
        .collect()
}

fn tiers_touched<'a>(keys: impl Iterator<Item = &'a String>) -> Vec<&'static str> {
    let mut out: Vec<&'static str> = keys
        .map(|k| settings::tier(k))
        .filter(|t| *t != Tier::Live)
        .map(Tier::as_str)
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

fn config_error(e: lpframe_config::ConfigError) -> Response {
    use lpframe_config::ConfigError as E;
    let status = match e {
        E::Invalid(_) | E::Parse { .. } | E::BadKey(_) => StatusCode::BAD_REQUEST,
        E::Read { .. } | E::Write { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    };
    refused(status, &e.to_string())
}

// --- actions --------------------------------------------------------------

async fn api_action(AxumState(app): AxumState<App>, Path(action): Path<String>) -> Response {
    let command = match action.as_str() {
        "amp_on" => Some(Command::SetAmp(true)),
        "amp_off" => Some(Command::SetAmp(false)),
        "display_wake" => Some(Command::SetDisplay(lpframe_proto::DisplayPower::On)),
        "display_sleep" => Some(Command::SetDisplay(lpframe_proto::DisplayPower::Off)),
        _ => None,
    };
    if let Some(command) = command {
        return match app.settings.commands.try_send(command) {
            Ok(()) => Json(json!({ "ok": true, "action": action })).into_response(),
            Err(e) => refused(
                StatusCode::SERVICE_UNAVAILABLE,
                &format!("the daemon did not accept the command: {e}"),
            ),
        };
    }

    match action.as_str() {
        // The renderer probes the card, connector and modes at startup, so
        // restarting it *is* the reprobe. Two names because §7.3 uses one and
        // the settings page's restart button uses the other.
        "restart_renderer" | "reprobe_display" => restart(settings::RENDERER_UNIT).await,
        "restart_daemon" => {
            if !settings::systemd_available() {
                return manual_restart(settings::DAEMON_UNIT);
            }
            // Answer first: restarting synchronously kills the connection the
            // response is going out on.
            tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let _ =
                    tokio::task::spawn_blocking(|| settings::restart_unit(settings::DAEMON_UNIT))
                        .await;
            });
            Json(json!({ "ok": true, "restarting": settings::DAEMON_UNIT })).into_response()
        }
        // Not implemented, and saying so beats a button that lies. Clearing
        // the cache means emptying a SQLite index and the files it points at
        // while the enrichment pipeline holds both open, and doing that from
        // out here would leave the two disagreeing. The sweeper already
        // enforces `cache.max_bytes`.
        "cache_clear" => refused(
            StatusCode::NOT_IMPLEMENTED,
            "cache_clear is not implemented; stop artd and remove cache.dir instead",
        ),
        other => refused(
            StatusCode::BAD_REQUEST,
            &format!("unknown action {other:?}"),
        ),
    }
}

async fn restart(unit: &'static str) -> Response {
    if !settings::systemd_available() {
        return manual_restart(unit);
    }
    match tokio::task::spawn_blocking(move || settings::restart_unit(unit)).await {
        Ok(Ok(())) => Json(json!({ "ok": true, "restarted": unit })).into_response(),
        Ok(Err(e)) => refused(StatusCode::INTERNAL_SERVER_ERROR, &e),
        Err(e) => refused(StatusCode::INTERNAL_SERVER_ERROR, &e.to_string()),
    }
}

fn manual_restart(unit: &'static str) -> Response {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({
            "error": format!(
                "systemd is not managing this device, so {unit} has to be restarted the \
                 way it was started. The change is written and takes effect then."
            ),
            "unit": unit,
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn web(bind: &str, auth: bool, hash: &str, escape: bool) -> lpframe_config::Web {
        lpframe_config::Web {
            enabled: true,
            bind: bind.into(),
            auth,
            password_hash: hash.into(),
            mdns: false,
            insecure_no_auth: escape,
        }
    }

    #[test]
    fn loopback_binds_are_accepted_however_auth_is_configured() {
        // Reaching loopback already means being on the device.
        for cfg in [
            web("127.0.0.1:8730", true, "$argon2id$v=19$x", false),
            web("127.0.0.1:8730", false, "", false),
            web("[::1]:8730", false, "", false),
        ] {
            assert!(check_bind(&cfg).unwrap().warning.is_none());
        }
    }

    #[test]
    fn a_non_loopback_bind_needs_authentication_and_a_hash() {
        let ok = check_bind(&web("0.0.0.0:8730", true, "$argon2id$v=19$x", false)).unwrap();
        assert_eq!(ok.addr.port(), 8730);
        assert!(ok.warning.is_none());

        // Auth on with nothing to check against is the state a failed
        // first-run generation leaves behind, and it must not serve.
        let err = check_bind(&web("0.0.0.0:8730", true, "", false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no password hash"), "{err}");
    }

    #[test]
    fn a_non_loopback_bind_with_auth_off_refuses_to_start() {
        let err = check_bind(&web("192.168.1.10:8730", false, "", false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("web.auth = false"), "{err}");
        assert!(err.contains("127.0.0.1:8730"), "{err}");
        assert!(err.contains("insecure_no_auth"), "{err}");
    }

    #[test]
    fn the_escape_hatch_allows_the_bind_and_warns_every_time() {
        let bind = check_bind(&web("0.0.0.0:8730", false, "", true)).unwrap();
        let warning = bind.warning.expect("the escape hatch must warn");
        assert!(warning.contains("NO AUTHENTICATION"), "{warning}");
        assert!(warning.contains("insecure_no_auth"), "{warning}");
    }

    #[test]
    fn a_malformed_bind_is_rejected() {
        assert!(check_bind(&web("lpframe.local:8730", true, "x", false)).is_err());
    }

    #[test]
    fn the_session_cookie_carries_exactly_the_flags_it_should() {
        let cookie = set_cookie("abc");
        assert!(cookie.starts_with("lpframe_session=abc;"));
        assert!(cookie.contains("Path=/"));
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(cookie.contains("Max-Age=2592000"), "thirty days");
        // Not an oversight: the device serves plain HTTP, and `Secure` would
        // make the browser drop the cookie on every request.
        assert!(!cookie.contains("Secure"), "{cookie}");
    }

    #[test]
    fn a_cookie_header_with_other_cookies_still_yields_the_session() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            "theme=dark; lpframe_session=deadbeef; other=1"
                .parse()
                .unwrap(),
        );
        assert_eq!(session_token(&headers), Some("deadbeef"));

        let mut none = HeaderMap::new();
        none.insert(header::COOKIE, "theme=dark".parse().unwrap());
        assert_eq!(session_token(&none), None);
        assert_eq!(session_token(&HeaderMap::new()), None);
    }
}
