//! The web interface against a real listener, over real HTTP.
//!
//! Everything here runs a genuine `axum` server on an ephemeral port and
//! talks to it with an HTTP client, because the properties being asserted are
//! properties of headers, status codes and cookies — the things a handler
//! test written against the router directly would quietly stop covering the
//! moment a layer moved.
//!
//! The security cases are the point of the file (DESIGN §7.3). If one of them
//! starts failing, the answer is not to adjust the test.

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

use artd::hub::Hub;
use artd::ipc::Command;
use artd::web::settings::Settings;
use artd::web::App;
use lpframe_config::Config;
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::StatusCode;
use serde_json::{json, Value};

const CSRF: &str = "X-LPFrame-Request";

/// Short enough to wait out in a test, long enough that a slow machine does
/// not revert a change the test is still confirming.
const CONFIRM_WINDOW: Duration = Duration::from_millis(400);

/// Every endpoint that changes something, with a body that would otherwise
/// be accepted. The unauthenticated and CSRF tests walk this list, so a new
/// mutating route is covered by adding it here and nowhere else.
fn mutating_endpoints() -> Vec<(&'static str, &'static str, Value)> {
    vec![
        ("PATCH", "/api/config", json!({ "render.ambient": true })),
        (
            "POST",
            "/api/config/reset",
            json!({ "keys": ["render.ambient"] }),
        ),
        ("POST", "/api/config/confirm", json!({ "id": 1 })),
        ("POST", "/api/actions/amp_on", json!({})),
        ("POST", "/api/logout", json!({})),
    ]
}

/// Every endpoint that reads something the owner would not want published.
fn private_reads() -> Vec<&'static str> {
    vec![
        "/api/state",
        "/api/diagnostics",
        "/api/config",
        "/api/artwork/current",
    ]
}

struct Harness {
    base: String,
    client: reqwest::Client,
    cookie: Option<String>,
    dir: PathBuf,
    config: PathBuf,
    local: PathBuf,
    password: String,
    hash: String,
    commands: tokio::sync::mpsc::Receiver<Command>,
}

impl Harness {
    async fn start(name: &str, auth: bool) -> Harness {
        let dir = std::env::temp_dir().join(format!("artd-web-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        let config = dir.join("config.toml");
        let local = dir.join("config.local.toml");
        std::fs::write(
            &config,
            "[render]\nambient = false\ncrossfade = \"600ms\"\n\n[display]\nrotation = 0\n",
        )
        .unwrap();

        let mut cfg = Config::load(&config, &local).unwrap().config;
        cfg.web.auth = auth;
        let resolved = artd::web::prepare_auth(&mut cfg, &config, &local).unwrap();

        let password = std::fs::read_to_string(dir.join("web-password.txt"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let hash = cfg.web.password_hash.clone();
        if auth {
            assert!(!password.is_empty(), "no password was generated");
            assert!(hash.starts_with("$argon2id$"), "{hash}");
        }

        let (cmd_tx, commands) = tokio::sync::mpsc::channel(16);
        let app = App::new(
            Hub::new(32),
            resolved,
            Settings::new(config.clone(), local.clone(), cmd_tx),
        )
        .with_confirm_window(CONFIRM_WINDOW);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr: SocketAddr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = artd::web::serve_on(app, listener).await;
        });

        Harness {
            base: format!("http://{addr}"),
            client: reqwest::Client::builder().build().unwrap(),
            cookie: None,
            dir,
            config,
            local,
            password,
            hash,
            commands,
        }
    }

    fn headers(&self, csrf: bool) -> HeaderMap {
        let mut headers = HeaderMap::new();
        if csrf {
            headers.insert(CSRF, HeaderValue::from_static("1"));
        }
        if let Some(cookie) = &self.cookie {
            headers.insert(
                reqwest::header::COOKIE,
                HeaderValue::from_str(&format!("lpframe_session={cookie}")).unwrap(),
            );
        }
        headers
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .headers(self.headers(false))
            .send()
            .await
            .unwrap()
    }

    async fn send(&self, method: &str, path: &str, body: &Value, csrf: bool) -> reqwest::Response {
        let method = reqwest::Method::from_bytes(method.as_bytes()).unwrap();
        self.client
            .request(method, format!("{}{path}", self.base))
            .headers(self.headers(csrf))
            .json(body)
            .send()
            .await
            .unwrap()
    }

    async fn patch(&self, body: Value) -> (StatusCode, Value) {
        let r = self.send("PATCH", "/api/config", &body, true).await;
        let status = r.status();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    async fn post(&self, path: &str, body: Value) -> (StatusCode, Value) {
        let r = self.send("POST", path, &body, true).await;
        let status = r.status();
        (status, r.json().await.unwrap_or(Value::Null))
    }

    /// Log in with the generated password, keeping the cookie.
    async fn login(&mut self) -> reqwest::Response {
        let password = self.password.clone();
        self.login_with(&password).await
    }

    async fn login_with(&mut self, password: &str) -> reqwest::Response {
        let response = self
            .send("POST", "/api/login", &json!({ "password": password }), true)
            .await;
        if let Some(cookie) = response
            .headers()
            .get(reqwest::header::SET_COOKIE)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.strip_prefix("lpframe_session="))
            .and_then(|v| v.split(';').next())
        {
            if !cookie.is_empty() {
                self.cookie = Some(cookie.to_string());
            }
        }
        response
    }

    fn reload(&self) -> Config {
        Config::load(&self.config, &self.local).unwrap().config
    }

    fn overrides(&self) -> BTreeSet<String> {
        Config::load(&self.config, &self.local).unwrap().overridden
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// --- authentication -------------------------------------------------------

#[tokio::test]
async fn an_unauthenticated_request_to_every_private_endpoint_is_refused() {
    let h = Harness::start("unauth", true).await;

    for path in private_reads() {
        let r = h.get(path).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED, "GET {path} was open");
    }
    // The event stream is the one that would leak listening history
    // continuously, so it gets its own line rather than hiding in a loop.
    assert_eq!(
        h.get("/api/events").await.status(),
        StatusCode::UNAUTHORIZED
    );

    for (method, path, body) in mutating_endpoints() {
        // With the CSRF header present, so the only thing left to refuse it
        // is the missing session.
        let r = h.send(method, path, &body, true).await;
        assert_eq!(
            r.status(),
            StatusCode::UNAUTHORIZED,
            "{method} {path} was open"
        );
    }

    // The shell itself is public, because it has to render the login form.
    assert_eq!(h.get("/").await.status(), StatusCode::OK);
    assert!(!h.get("/").await.text().await.unwrap().contains("$argon2"));
}

#[tokio::test]
async fn a_wrong_password_is_indistinguishable_from_an_unknown_session() {
    let mut h = Harness::start("indistinguishable", true).await;

    let wrong = h.login_with("not the password").await;
    let wrong_status = wrong.status();
    let wrong_type = wrong.headers().get(reqwest::header::CONTENT_TYPE).cloned();
    let wrong_names: BTreeSet<String> = wrong
        .headers()
        .keys()
        .map(|k| k.as_str().to_string())
        .collect();
    let wrong_body = wrong.text().await.unwrap();

    h.cookie = Some("f".repeat(64));
    let unknown = h.get("/api/config").await;
    let unknown_status = unknown.status();
    let unknown_type = unknown
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .cloned();
    let unknown_names: BTreeSet<String> = unknown
        .headers()
        .keys()
        .map(|k| k.as_str().to_string())
        .collect();
    let unknown_body = unknown.text().await.unwrap();

    assert_eq!(wrong_status, StatusCode::UNAUTHORIZED);
    assert_eq!(wrong_status, unknown_status);
    assert_eq!(wrong_type, unknown_type);
    assert_eq!(wrong_body, unknown_body, "the bodies differ");
    // `date` is the only header either response can differ by, and it is not
    // one an attacker learns anything from.
    let ignore = |s: &BTreeSet<String>| -> BTreeSet<String> {
        s.iter().filter(|k| *k != "date").cloned().collect()
    };
    assert_eq!(ignore(&wrong_names), ignore(&unknown_names));
}

#[tokio::test]
async fn the_login_rate_limit_engages_after_five_attempts() {
    let mut h = Harness::start("ratelimit", true).await;

    for attempt in 1..=5 {
        let r = h.login_with("wrong").await;
        assert_eq!(
            r.status(),
            StatusCode::UNAUTHORIZED,
            "attempt {attempt} was throttled early"
        );
    }

    let sixth = h.login_with("wrong").await;
    assert_eq!(sixth.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        sixth.headers().get(reqwest::header::RETRY_AFTER),
        Some(&HeaderValue::from_static("60"))
    );

    // And the right password does not get through either, so the limit
    // cannot be walked past by finally guessing correctly.
    let correct = h.login().await;
    assert_eq!(correct.status(), StatusCode::TOO_MANY_REQUESTS);
    assert!(h.cookie.is_none());
}

#[tokio::test]
async fn a_successful_login_admits_and_a_logout_ends_it() {
    let mut h = Harness::start("session", true).await;

    let response = h.login().await;
    assert_eq!(response.status(), StatusCode::OK);
    assert!(h.cookie.is_some());
    assert_eq!(h.get("/api/config").await.status(), StatusCode::OK);

    let (status, _) = h.post("/api/logout", json!({})).await;
    assert_eq!(status, StatusCode::OK);
    // The cookie the browser was told to keep is now worthless server-side,
    // which is the property that matters — a client that ignores the
    // expiring Set-Cookie still cannot use it.
    assert_eq!(
        h.get("/api/config").await.status(),
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn the_session_cookie_is_httponly_samesite_strict_and_not_secure() {
    let mut h = Harness::start("cookie", true).await;
    let response = h.login().await;
    let cookie = response
        .headers()
        .get(reqwest::header::SET_COOKIE)
        .unwrap()
        .to_str()
        .unwrap()
        .to_string();

    assert!(cookie.contains("HttpOnly"), "{cookie}");
    assert!(cookie.contains("SameSite=Strict"), "{cookie}");
    assert!(cookie.contains("Path=/"), "{cookie}");
    assert!(cookie.contains("Max-Age=2592000"), "thirty days: {cookie}");
    // The device serves plain HTTP on a LAN. `Secure` would make the browser
    // drop this cookie on every request, and logging in would silently do
    // nothing. See the comment on `set_cookie`.
    assert!(!cookie.contains("Secure"), "{cookie}");

    let token = cookie
        .strip_prefix("lpframe_session=")
        .and_then(|c| c.split(';').next())
        .unwrap();
    assert_eq!(token.len(), 64, "32 bytes of hex");
    assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
}

// --- CSRF -----------------------------------------------------------------

#[tokio::test]
async fn a_mutation_without_the_csrf_header_is_rejected() {
    let mut h = Harness::start("csrf", true).await;
    h.login().await;

    for (method, path, body) in mutating_endpoints() {
        let refused = h.send(method, path, &body, false).await;
        assert_eq!(
            refused.status(),
            StatusCode::BAD_REQUEST,
            "{method} {path} accepted a request a cross-origin form could make"
        );
        let error: Value = refused.json().await.unwrap();
        assert!(
            error["error"]
                .as_str()
                .unwrap_or_default()
                .contains("X-LPFrame-Request"),
            "{error}"
        );
    }

    // Nothing was written by any of them. The generated hash is the only
    // thing the override file is allowed to hold at this point.
    assert_eq!(
        h.overrides(),
        BTreeSet::from(["web.password_hash".to_string()])
    );
}

#[tokio::test]
async fn even_logging_in_needs_the_csrf_header() {
    // Login CSRF is the mildest of the family, but the header costs the real
    // client nothing and a uniform rule is one fewer exception to audit.
    let h = Harness::start("csrf-login", true).await;
    let password = h.password.clone();
    let refused = h
        .send(
            "POST",
            "/api/login",
            &json!({ "password": password }),
            false,
        )
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);
    assert!(refused.headers().get(reqwest::header::SET_COOKIE).is_none());
}

// --- the hash never leaves -------------------------------------------------

#[tokio::test]
async fn the_password_hash_never_appears_in_a_response_body() {
    let mut h = Harness::start("secret", true).await;
    h.login().await;
    let hash = h.hash.clone();
    assert!(!hash.is_empty());

    for path in private_reads()
        .into_iter()
        .chain(["/", "/api/session"])
        .collect::<Vec<_>>()
    {
        let body = h.get(path).await.text().await.unwrap();
        assert!(!body.contains(&hash), "{path} returned the hash");
        assert!(!body.contains("password_hash"), "{path} named the key");
        assert!(!body.contains(&h.password), "{path} returned the password");
    }

    // ...and it is not listed as a setting at all, so no client can even ask
    // for it by name.
    let config: Value = h.get("/api/config").await.json().await.unwrap();
    let keys: Vec<&str> = config["settings"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["key"].as_str().unwrap())
        .collect();
    assert!(!keys.contains(&"web.password_hash"), "{keys:?}");
    assert!(keys.contains(&"render.ambient"));

    // A write reports which keys are now overridden, and the hash — which is
    // always overridden once authentication is on — must not be among them.
    let (_, body) = h.patch(json!({ "render.ambient": true })).await;
    assert!(
        !body.to_string().contains("password_hash"),
        "the patch response named it: {body}"
    );
}

#[tokio::test]
async fn the_password_hash_cannot_be_written_through_the_api() {
    let mut h = Harness::start("secret-write", true).await;
    h.login().await;
    let before = h.reload().web.password_hash;

    let (status, _) = h
        .patch(json!({ "web.password_hash": "$argon2id$v=19$m=8,t=1,p=1$YQ$YQ" }))
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    let (status, _) = h
        .post(
            "/api/config/reset",
            json!({ "keys": ["web.password_hash"] }),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    assert_eq!(h.reload().web.password_hash, before, "the hash moved");
    assert!(!before.is_empty());
}

#[tokio::test]
async fn the_generated_password_file_is_readable_only_by_its_owner() {
    use std::os::unix::fs::PermissionsExt;
    let h = Harness::start("passwd-mode", true).await;
    let path = h.dir.join("web-password.txt");
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "{}", path.display());
    assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), h.password);
}

// --- authentication disabled -----------------------------------------------

#[tokio::test]
async fn with_auth_disabled_the_pages_open_but_mutations_still_need_the_header() {
    // This is the `web.insecure_no_auth` shape: a loopback or proxied device
    // where the operator has decided something else is doing the checking.
    // Losing authentication must not also lose the CSRF defence, because a
    // page on the wider internet can still point a form at a LAN address.
    let h = Harness::start("no-auth", false).await;
    assert!(h.password.is_empty(), "no password should be generated");

    for path in private_reads() {
        // Not `OK`: with no session playing, the artwork endpoint has
        // nothing to serve. What matters is that nothing is being withheld.
        assert_ne!(
            h.get(path).await.status(),
            StatusCode::UNAUTHORIZED,
            "{path}"
        );
    }

    let refused = h
        .send(
            "PATCH",
            "/api/config",
            &json!({ "render.ambient": true }),
            false,
        )
        .await;
    assert_eq!(refused.status(), StatusCode::BAD_REQUEST);

    let (status, _) = h.patch(json!({ "render.ambient": true })).await;
    assert_eq!(status, StatusCode::OK);
}

// --- settings --------------------------------------------------------------

#[tokio::test]
async fn a_setting_written_through_the_api_reaches_the_file_and_the_daemon() {
    let mut h = Harness::start("write", true).await;
    h.login().await;

    let (status, body) = h.patch(json!({ "render.ambient": true })).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    assert!(h.reload().render.ambient, "not on disk");
    assert!(h.overrides().contains("render.ambient"));

    // Only the changed key is written, which is what makes reset a deletion.
    let text = std::fs::read_to_string(&h.local).unwrap();
    assert!(!text.contains("crossfade"), "{text}");

    // ...and the running daemon was told, rather than waiting for a restart.
    let command = h.commands.try_recv().expect("no reload reached the core");
    match command {
        Command::Reload(cfg) => assert!(cfg.render.ambient),
        other => panic!("unexpected command {other:?}"),
    }
}

#[tokio::test]
async fn an_invalid_setting_is_refused_and_the_file_is_untouched() {
    let mut h = Harness::start("invalid", true).await;
    h.login().await;
    h.patch(json!({ "timeouts.stall": "10s" })).await;
    let before = std::fs::read(&h.local).unwrap();
    while h.commands.try_recv().is_ok() {}

    // stall must stay shorter than session.
    let (status, body) = h.patch(json!({ "timeouts.stall": "90s" })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("must be shorter"),
        "{body}"
    );

    assert_eq!(std::fs::read(&h.local).unwrap(), before);
    assert!(
        h.commands.try_recv().is_err(),
        "a refused change was still handed to the daemon"
    );

    // An unknown key is named rather than silently ignored.
    let (status, body) = h.patch(json!({ "render.ambiant": true })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        body["error"].as_str().unwrap().contains("ambiant"),
        "{body}"
    );
}

#[tokio::test]
async fn the_settings_page_cannot_lock_the_owner_out_of_the_device() {
    // Turning authentication off while bound to the network is a change whose
    // consequence only shows up at the next reboot, by which point nobody can
    // reach the page to undo it. `Config::validate` refuses the combination,
    // which is why the check lives there and not only in `check_bind`.
    let mut h = Harness::start("lockout", true).await;
    h.login().await;
    assert_eq!(h.reload().web.bind, "0.0.0.0:8730", "the default bind");

    let (status, body) = h.patch(json!({ "web.auth": false })).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(h.reload().web.auth);

    // Deliberately saying so is still allowed, in that order.
    let (status, _) = h.patch(json!({ "web.insecure_no_auth": true })).await;
    assert_eq!(status, StatusCode::OK);
    let (status, _) = h.patch(json!({ "web.auth": false })).await;
    assert_eq!(status, StatusCode::OK);
}

#[tokio::test]
async fn resetting_a_key_returns_it_to_the_base_value() {
    let mut h = Harness::start("reset", true).await;
    h.login().await;

    h.patch(json!({ "render.ambient": true })).await;
    assert!(h.reload().render.ambient);

    let (status, body) = h
        .post("/api/config/reset", json!({ "keys": ["render.ambient"] }))
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!h.reload().render.ambient, "the base value did not return");
    assert!(!h.overrides().contains("render.ambient"));
}

#[tokio::test]
async fn every_setting_is_labelled_with_the_restart_it_needs() {
    let mut h = Harness::start("tiers", true).await;
    h.login().await;
    let config: Value = h.get("/api/config").await.json().await.unwrap();

    let tier = |key: &str| -> String {
        config["settings"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| s["key"] == key)
            .unwrap_or_else(|| panic!("{key} missing"))["tier"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert_eq!(tier("timeouts.stall"), "live");
    assert_eq!(tier("power.amp.off_delay"), "live");
    assert_eq!(tier("display.rotation"), "renderer");
    assert_eq!(tier("web.bind"), "daemon");

    // The page needs to know whether offering a restart button is honest.
    assert!(config["systemd"].is_boolean());
    assert_eq!(config["units"]["renderer"], "lpframe-lprender");
}

// --- confirm or revert -----------------------------------------------------

#[tokio::test]
async fn an_unconfirmed_display_change_puts_itself_back() {
    let mut h = Harness::start("revert", true).await;
    h.login().await;

    let (status, body) = h.patch(json!({ "display.rotation": 90 })).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let id = body["confirm"]["id"].as_u64().expect("no countdown armed");
    assert_eq!(h.reload().display.rotation, 90);
    assert!(id > 0);

    tokio::time::sleep(CONFIRM_WINDOW * 3).await;

    assert_eq!(
        h.reload().display.rotation,
        0,
        "an unconfirmed rotation stayed applied"
    );
    assert!(
        !h.overrides().contains("display.rotation"),
        "the reverted key is still overridden"
    );
}

#[tokio::test]
async fn a_confirmed_display_change_stays() {
    let mut h = Harness::start("confirm", true).await;
    h.login().await;

    let (_, body) = h.patch(json!({ "display.rotation": 180 })).await;
    let id = body["confirm"]["id"].as_u64().unwrap();

    let (status, body) = h.post("/api/config/confirm", json!({ "id": id })).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["confirmed"], true);

    tokio::time::sleep(CONFIRM_WINDOW * 3).await;
    assert_eq!(h.reload().display.rotation, 180);

    // A second confirmation of the same countdown is honest about there
    // being nothing left to confirm.
    let (_, body) = h.post("/api/config/confirm", json!({ "id": id })).await;
    assert_eq!(body["confirmed"], false);
}

#[tokio::test]
async fn a_revert_restores_a_previous_override_rather_than_the_default() {
    let mut h = Harness::start("revert-override", true).await;
    h.login().await;

    // Establish 180 as a confirmed override, then fail to confirm 90.
    let (_, body) = h.patch(json!({ "display.rotation": 180 })).await;
    h.post(
        "/api/config/confirm",
        json!({ "id": body["confirm"]["id"] }),
    )
    .await;

    h.patch(json!({ "display.rotation": 90 })).await;
    tokio::time::sleep(CONFIRM_WINDOW * 3).await;

    assert_eq!(
        h.reload().display.rotation,
        180,
        "the revert dropped to the base value instead of the last good one"
    );
    assert!(h.overrides().contains("display.rotation"));
}

#[tokio::test]
async fn a_change_that_needs_no_confirmation_arms_no_countdown() {
    let mut h = Harness::start("no-countdown", true).await;
    h.login().await;
    let (_, body) = h.patch(json!({ "render.ambient": true })).await;
    assert!(body.get("confirm").is_none(), "{body}");

    tokio::time::sleep(CONFIRM_WINDOW * 3).await;
    assert!(h.reload().render.ambient, "something reverted it");
}

// --- actions ---------------------------------------------------------------

#[tokio::test]
async fn actions_reach_the_daemon_and_unknown_ones_do_not() {
    let mut h = Harness::start("actions", true).await;
    h.login().await;

    for (action, expected) in [
        ("amp_on", Command::SetAmp(true)),
        ("amp_off", Command::SetAmp(false)),
        (
            "display_wake",
            Command::SetDisplay(lpframe_proto::DisplayPower::On),
        ),
    ] {
        let (status, _) = h.post(&format!("/api/actions/{action}"), json!({})).await;
        assert_eq!(status, StatusCode::OK, "{action}");
        assert_eq!(h.commands.try_recv().unwrap(), expected);
    }

    let (status, body) = h.post("/api/actions/rm_rf", json!({})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(body["error"].as_str().unwrap().contains("rm_rf"), "{body}");
    assert!(h.commands.try_recv().is_err());

    // Declared in DESIGN §7.3 and deliberately not implemented; the page
    // needs an answer it can show rather than a button that lies.
    let (status, _) = h.post("/api/actions/cache_clear", json!({})).await;
    assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
}
