//! The settings half of the interface: reading, writing and reverting
//! configuration (DESIGN §7.3).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use lpframe_config::{Config, ConfigError, Loaded};

use crate::ipc::Command;

/// Settings the API neither reads out nor accepts.
///
/// The password hash is a setting as far as the schema is concerned. Handing
/// it to a browser would turn one authenticated session into an offline
/// cracking target that outlives the password change, and accepting one from
/// a browser would be a way to install a password nobody typed.
pub const SECRET_KEYS: [&str; 1] = ["web.password_hash"];

/// Changes that can leave a headless device showing nothing (DESIGN §7.3).
pub const CONFIRM_KEYS: [&str; 2] = ["display.mode", "display.rotation"];

/// How long a display change waits to be confirmed before it is undone.
pub const CONFIRM_WINDOW: Duration = Duration::from_secs(15);

pub const RENDERER_UNIT: &str = "lpframe-lprender";
pub const DAEMON_UNIT: &str = "lpframe-artd";

/// What a setting needs before it takes effect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tier {
    /// `artd` re-reads it on every evaluation; the change is already live.
    Live,
    /// `lprender` reads its own copy of the config at startup.
    Renderer,
    /// `artd` opened something at startup that it cannot reopen in place.
    Daemon,
}

impl Tier {
    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Live => "live",
            Tier::Renderer => "renderer",
            Tier::Daemon => "daemon",
        }
    }
}

/// Which restart, if any, a key needs.
///
/// This is what the code actually does, which is not quite what DESIGN §7.3
/// hoped for. The design lists the render settings — ambient, fit,
/// background, crossfade — as live. They are not: `lprender` loads the
/// configuration once at startup and nothing in the snapshot protocol carries
/// render policy, so the only honest label for them is "needs the renderer
/// restarted", which the page offers as one click. Enrichment is the same
/// story for a different reason: the pipeline owns an open cache and a
/// sweeper task, and rebuilding it under a running daemon would leave the old
/// one sweeping the same database.
pub fn tier(key: &str) -> Tier {
    match key {
        // Read out of `Machine::config` every time a timer is evaluated.
        k if k.starts_with("timeouts.") => Tier::Live,
        k if k.starts_with("power.display.") => Tier::Live,
        // ...except the three that describe the line itself, which is
        // requested once when the daemon starts.
        "power.amp.gpio_chip" | "power.amp.gpio_line" | "power.amp.active_low" => Tier::Daemon,
        k if k.starts_with("power.amp.") => Tier::Live,

        k if k.starts_with("display.") || k.starts_with("render.") => Tier::Renderer,

        _ => Tier::Daemon,
    }
}

/// A display change waiting to be confirmed.
struct Pending {
    id: u64,
    /// The local override entries to put back. `None` means the key was not
    /// overridden before and the reversal is a deletion.
    undo: BTreeMap<String, Option<toml::Value>>,
}

/// Everything the settings endpoints need.
pub struct Settings {
    pub base: PathBuf,
    pub local: PathBuf,
    /// Where an applied change goes so the running daemon picks it up.
    pub commands: tokio::sync::mpsc::Sender<Command>,
    /// Shortened by the tests; [`CONFIRM_WINDOW`] on a device.
    pub confirm_window: Duration,
    pending: Mutex<Option<Pending>>,
    next_id: AtomicU64,
}

impl Settings {
    pub fn new(
        base: PathBuf,
        local: PathBuf,
        commands: tokio::sync::mpsc::Sender<Command>,
    ) -> Settings {
        Settings {
            base,
            local,
            commands,
            confirm_window: CONFIRM_WINDOW,
            pending: Mutex::new(None),
            next_id: AtomicU64::new(1),
        }
    }

    /// Load the merged configuration as it currently reads on disk.
    pub fn load(&self) -> Result<Loaded, ConfigError> {
        Config::load(&self.base, &self.local)
    }

    /// Apply a sparse update, returning the configuration it produced.
    ///
    /// Nothing is written unless the merged result parses and validates, and
    /// the running daemon is only told about a change that reached the disk —
    /// so a device whose configuration file and running state disagree is not
    /// a state this can produce.
    pub fn apply(&self, updates: &BTreeMap<String, toml::Value>) -> Result<Loaded, ConfigError> {
        let loaded = lpframe_config::set_overrides(&self.base, &self.local, updates)?;
        self.publish(&loaded.config);
        Ok(loaded)
    }

    pub fn reset(&self, keys: &[String]) -> Result<Loaded, ConfigError> {
        let loaded = lpframe_config::reset_overrides(&self.base, &self.local, keys)?;
        self.publish(&loaded.config);
        Ok(loaded)
    }

    fn publish(&self, config: &Config) {
        // `try_send` rather than an await: the channel is sized for the
        // daemon's own traffic and a settings change that cannot be delivered
        // is still on disk, so the next restart picks it up regardless.
        if let Err(e) = self
            .commands
            .try_send(Command::Reload(Box::new(config.clone())))
        {
            tracing::warn!("configuration written but not applied in place: {e}");
        }
    }

    /// Record what the local override said about these keys, so the countdown
    /// has something exact to put back.
    pub fn snapshot_keys(
        &self,
        keys: &[String],
    ) -> Result<BTreeMap<String, Option<toml::Value>>, ConfigError> {
        let table = lpframe_config::read_local(&self.local)?;
        Ok(keys
            .iter()
            .map(|k| {
                (
                    k.clone(),
                    lpframe_config::write::get_path(&table, k).cloned(),
                )
            })
            .collect())
    }

    /// Arm a confirm-or-revert countdown, superseding any earlier one.
    ///
    /// When two display changes overlap, the older undo wins per key: the
    /// state worth getting back to is the one the user could still see, not
    /// the intermediate one they have already failed to confirm.
    pub fn arm(&self, mut undo: BTreeMap<String, Option<toml::Value>>) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let mut pending = self.pending.lock().expect("settings mutex poisoned");
        if let Some(previous) = pending.take() {
            for (key, value) in previous.undo {
                undo.insert(key, value);
            }
        }
        *pending = Some(Pending { id, undo });
        id
    }

    /// Cancel a countdown. Returns false if it had already fired or never
    /// existed, which the caller reports rather than pretending otherwise.
    pub fn confirm(&self, id: u64) -> bool {
        let mut pending = self.pending.lock().expect("settings mutex poisoned");
        match pending.as_ref() {
            Some(p) if p.id == id => {
                *pending = None;
                true
            }
            _ => false,
        }
    }

    /// Undo an unconfirmed change. Returns the keys put back, or `None` if
    /// the countdown was confirmed or superseded in the meantime.
    pub fn revert(&self, id: u64) -> Option<Result<Vec<String>, ConfigError>> {
        let undo = {
            let mut pending = self.pending.lock().expect("settings mutex poisoned");
            match pending.as_ref() {
                Some(p) if p.id == id => pending.take().map(|p| p.undo)?,
                _ => return None,
            }
        };
        let keys: Vec<String> = undo.keys().cloned().collect();
        Some(
            lpframe_config::edit_overrides(&self.base, &self.local, &undo).map(|loaded| {
                self.publish(&loaded.config);
                keys
            }),
        )
    }
}

/// Whether there is a systemd to ask.
///
/// `/run/systemd/system` is what `sd_booted(3)` looks for, and it is the only
/// check that distinguishes a device systemd is running from a container that
/// merely has `systemctl` installed — where the binary is present, answers
/// `--version` cheerfully, and then fails every command with "has not been
/// booted with systemd as init system". Getting this wrong means offering a
/// restart button that cannot work.
pub fn systemd_available() -> bool {
    std::path::Path::new("/run/systemd/system").is_dir()
}

/// Restart a unit, or explain what to run by hand.
///
/// Blocking; callers put it on a blocking thread. Systemd is how the device
/// ships, but the daemon runs perfectly well started from a shell — in which
/// case there is nothing to ask, and saying so is more use than a spinner
/// that never resolves.
pub fn restart_unit(unit: &str) -> Result<(), String> {
    let output = std::process::Command::new("systemctl")
        .arg("restart")
        .arg(unit)
        .output();
    match output {
        Ok(o) if o.status.success() => Ok(()),
        Ok(o) => Err(format!(
            "systemctl restart {unit} failed: {}",
            String::from_utf8_lossy(&o.stderr).trim()
        )),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(format!(
            "systemd is not available here, so {unit} has to be restarted the \
             way it was started — the change is written and will be picked up \
             then."
        )),
        Err(e) => Err(format!("could not run systemctl: {e}")),
    }
}

/// Convert a JSON value from the browser into the TOML the config speaks.
///
/// Objects and nulls are refused rather than flattened: the API takes dotted
/// paths, so a nested object would be a client sending something this does
/// not mean, and guessing at it is how a typo becomes a silent no-op.
pub fn json_to_toml(v: &serde_json::Value) -> Option<toml::Value> {
    Some(match v {
        serde_json::Value::Bool(b) => toml::Value::Boolean(*b),
        serde_json::Value::String(s) => toml::Value::String(s.clone()),
        serde_json::Value::Number(n) => match (n.as_i64(), n.as_f64()) {
            (Some(i), _) => toml::Value::Integer(i),
            (None, Some(f)) => toml::Value::Float(f),
            _ => return None,
        },
        serde_json::Value::Array(items) => {
            toml::Value::Array(items.iter().map(json_to_toml).collect::<Option<_>>()?)
        }
        serde_json::Value::Null | serde_json::Value::Object(_) => return None,
    })
}

/// Convert a TOML value into JSON for the settings page.
pub fn toml_to_json(v: &toml::Value) -> serde_json::Value {
    match v {
        toml::Value::String(s) => serde_json::Value::String(s.clone()),
        toml::Value::Integer(i) => serde_json::Value::from(*i),
        toml::Value::Float(f) => serde_json::Value::from(*f),
        toml::Value::Boolean(b) => serde_json::Value::Bool(*b),
        toml::Value::Datetime(d) => serde_json::Value::String(d.to_string()),
        toml::Value::Array(a) => serde_json::Value::Array(a.iter().map(toml_to_json).collect()),
        toml::Value::Table(t) => serde_json::Value::Object(
            t.iter()
                .map(|(k, v)| (k.clone(), toml_to_json(v)))
                .collect(),
        ),
    }
}

/// The permitted values of a setting, where there is a fixed set.
///
/// Only a hint for the interface — the schema is what actually rejects a bad
/// value, on the way in and again on reload.
pub fn choices(key: &str) -> Option<&'static [&'static str]> {
    Some(match key {
        "display.rotation" => &["0", "90", "180", "270"],
        "enrichment.strictness" => &["off", "text_only", "text_and_visual", "strict"],
        "logging.level" => &["error", "warn", "info", "debug", "trace"],
        "power.amp.on_event" => &["session_begin", "play_begin"],
        "render.background" => &["black", "blur", "dominant", "gradient"],
        "render.fit" => &["cover", "contain"],
        _ => return None,
    })
}

/// A one-line explanation for the settings page, where the name is not
/// enough on its own.
pub fn hint(key: &str) -> Option<&'static str> {
    Some(match key {
        "display.margin_percent" => "Inset, for televisions that overscan.",
        "display.mode" => "auto, highest, or an exact mode such as 1920x1920@60.",
        "enrichment.contact" => "An email address. Required by MusicBrainz; unused otherwise.",
        "enrichment.enabled" => {
            "Sends the artist and album name to the configured catalogues. Off leaves \
             the device fully working, just softer-looking."
        }
        "enrichment.sources" => "Add musicbrainz only alongside a contact address.",
        "ipc.art_retain" => "Artwork revisions kept, so a reader mid-decode keeps its file.",
        "power.amp.on_event" => {
            "session_begin fires once per listening session; play_begin fires per track."
        }
        "power.amp.pulse" => "0ms holds the line; anything longer emits a momentary pulse.",
        "power.display.ambient_after" => "\"off\", or a delay before the blurred idle view.",
        "render.background" => "What fills a non-square panel outside the artwork.",
        "render.square" => "Off drops the 1:1 constraint and fills the panel.",
        "timeouts.stall" => "Must stay shorter than timeouts.session.",
        "web.bind" => "0.0.0.0:8730 for the network, 127.0.0.1:8730 for this device only.",
        "web.insecure_no_auth" => {
            "Allows a network bind with authentication off. Only for a device behind \
             something else that authenticates."
        }
        "web.mdns" => {
            "Advertises an _http._tcp record over Avahi. The hostname resolves \
                       either way."
        }
        _ => return None,
    })
}

/// Whether a path is one the API refuses to read out or write.
pub fn is_secret(key: &str) -> bool {
    SECRET_KEYS.contains(&key)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_setting_lands_in_exactly_one_tier() {
        // Not a tautology: the point is that the match has no gaps, so a key
        // added to the schema gets a label rather than disappearing.
        for key in lpframe_config::defaults().keys() {
            let t = tier(key);
            assert!(
                ["live", "renderer", "daemon"].contains(&t.as_str()),
                "{key}"
            );
        }
    }

    #[test]
    fn the_keys_artd_re_reads_are_the_ones_labelled_live() {
        assert_eq!(tier("timeouts.stall"), Tier::Live);
        assert_eq!(tier("power.amp.off_delay"), Tier::Live);
        assert_eq!(tier("power.display.blank_after"), Tier::Live);
        // The line itself is requested once, at startup.
        assert_eq!(tier("power.amp.gpio_line"), Tier::Daemon);
        assert_eq!(tier("display.rotation"), Tier::Renderer);
        assert_eq!(tier("render.crossfade"), Tier::Renderer);
        assert_eq!(tier("web.bind"), Tier::Daemon);
        assert_eq!(tier("ipc.socket"), Tier::Daemon);
    }

    #[test]
    fn json_values_convert_only_where_the_meaning_is_unambiguous() {
        use serde_json::json;
        assert_eq!(json_to_toml(&json!(true)), Some(toml::Value::Boolean(true)));
        assert_eq!(json_to_toml(&json!(17)), Some(toml::Value::Integer(17)));
        assert_eq!(json_to_toml(&json!(0.35)), Some(toml::Value::Float(0.35)));
        assert_eq!(
            json_to_toml(&json!(["itunes"])),
            Some(toml::Value::Array(vec![toml::Value::String(
                "itunes".into()
            )]))
        );
        assert_eq!(json_to_toml(&json!(null)), None);
        assert_eq!(json_to_toml(&json!({"a": 1})), None);
    }

    #[test]
    fn restarting_something_that_cannot_be_restarted_explains_itself() {
        // No unit by this name exists anywhere, so whichever arm runs — no
        // systemctl, systemctl without systemd, or systemd without the unit —
        // the caller gets a sentence rather than a silent failure.
        let err = restart_unit("lpframe-nothing-of-the-sort").unwrap_err();
        assert!(!err.trim().is_empty(), "{err:?}");
    }
}
