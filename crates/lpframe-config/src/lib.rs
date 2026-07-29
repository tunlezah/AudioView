//! Device configuration: schema, layered loading, validation.
//!
//! Two files are merged, the second winning per key (DESIGN §7.1):
//!
//! | File | Owner | Writable |
//! |---|---|---|
//! | `/etc/lpframe/config.toml` | the package | no, under overlayfs |
//! | `/var/lib/lpframe/config.local.toml` | the web interface | yes |
//!
//! The split exists because of the read-only root: a settings page that
//! edited `/etc` would appear to work and forget everything at reboot.

#![forbid(unsafe_code)]

pub mod duration;
pub mod write;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

pub use duration::{Dur, MaybeDuration};
pub use write::{edit_overrides, read_local, reset_overrides, set_overrides, write_atomic};

pub const DEFAULT_BASE_PATH: &str = "/etc/lpframe/config.toml";
pub const DEFAULT_LOCAL_PATH: &str = "/var/lib/lpframe/config.local.toml";

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{path} is not valid TOML: {source}")]
    Parse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },
    #[error("configuration is invalid: {0}")]
    Invalid(String),
    #[error("writing {path}: {source}")]
    Write {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{0}")]
    BadKey(String),
}

// --- schema ---------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub device: Device,
    pub audio: Audio,
    pub display: Display,
    pub render: Render,
    pub enrichment: Enrichment,
    pub cache: Cache,
    pub power: Power,
    pub timeouts: Timeouts,
    pub ipc: Ipc,
    pub web: Web,
    pub logging: Logging,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Device {
    /// Advertised AirPlay name.
    pub name: String,
    /// The shairport-sync metadata pipe to read.
    pub metadata_pipe: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Audio {
    pub alsa_device: String,
    pub mixer: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Display {
    pub connector: String,
    pub mode: String,
    pub rotation: u32,
    pub card: String,
    pub margin_percent: f32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Fit {
    Cover,
    Contain,
}

/// What fills the panel outside the square art area. Only visible on
/// non-1:1 panels (DESIGN §6.3.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Background {
    Black,
    Blur,
    Dominant,
    Gradient,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Render {
    /// False drops the 1:1 constraint and fills the panel.
    pub square: bool,
    pub fit: Fit,
    pub background: Background,
    pub background_dim: f32,
    pub crossfade: Dur,
    /// Shorter, for the same album getting sharper rather than a new record.
    pub enrichment_crossfade: Dur,
    pub ken_burns: bool,
    pub ken_burns_period: Dur,
    pub ambient: bool,
    pub ambient_dim: f32,
    pub placeholder: PathBuf,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Strictness {
    Off,
    TextOnly,
    TextAndVisual,
    Strict,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Enrichment {
    /// Sends artist and album to Apple and/or MusicBrainz.
    pub enabled: bool,
    pub strictness: Strictness,
    pub sources: Vec<String>,
    pub itunes_country: String,
    pub max_dimension: u32,
    /// Required by MusicBrainz; unused when only iTunes is enabled.
    pub contact: String,
    pub rate_limit_per_min: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Cache {
    pub dir: PathBuf,
    pub max_bytes: ByteSize,
    pub negative_ttl: Dur,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Power {
    pub amp: Amp,
    pub display: DisplayPower,
}

/// When the amplifier trigger fires.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AmpTrigger {
    /// `abeg` — one transition per listening session. The default.
    SessionBegin,
    /// `pbeg` — fires per track; only for genuinely momentary trigger inputs.
    PlayBegin,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Amp {
    pub enabled: bool,
    /// Resolve by label, never by index: Pi 5 moved the header to the RP1
    /// controller and `gpiochipN` numbering has shifted between kernels.
    pub gpio_chip: String,
    pub gpio_line: u32,
    pub active_low: bool,
    /// 0 holds the level while on; >0 emits a momentary pulse.
    pub pulse: Dur,
    pub on_event: AmpTrigger,
    pub off_delay: Dur,
    pub debounce: Dur,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DisplayPower {
    pub blank_after: Dur,
    pub ambient_after: MaybeDuration,
    pub fade_out: Dur,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Timeouts {
    /// No pipe activity for this long while playing → paused.
    pub stall: Dur,
    /// No pipe activity for this long in any session state → idle.
    pub session: Dur,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Ipc {
    pub socket: PathBuf,
    /// tmpfs directory for artwork the renderer reads by path.
    pub art_dir: PathBuf,
    /// Artwork revisions kept before unlinking, so a reader mid-decode never
    /// has the file pulled out from under it.
    pub art_retain: usize,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Web {
    pub enabled: bool,
    pub bind: String,
    pub auth: bool,
    /// Argon2id PHC string for the web password, generated on first run and
    /// written to the local override. The plaintext is never stored here, is
    /// never logged after the one line at generation, and never leaves the
    /// daemon in an API response.
    pub password_hash: String,
    /// Advertise `_http._tcp` over Avahi. Hostname resolution is Avahi's
    /// doing already, so this only adds the service record (DESIGN §7.3).
    pub mdns: bool,
    /// Bind beyond loopback with `auth = false`. Off, and staying off unless
    /// somebody types it: the setting it disables is the only thing standing
    /// between a stranger on the network and the listening history.
    ///
    /// The honest use is a reverse proxy that authenticates in front of us.
    pub insecure_no_auth: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    pub level: String,
    /// Events retained for the diagnostics page.
    pub event_buffer: usize,
}

/// A byte count written as `"2GiB"`, `"512MiB"` or a plain integer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ByteSize(pub u64);

impl Serialize for ByteSize {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        const UNITS: [(&str, u64); 3] = [("GiB", 1 << 30), ("MiB", 1 << 20), ("KiB", 1 << 10)];
        for (name, size) in UNITS {
            if self.0 >= size && self.0 % size == 0 {
                return s.serialize_str(&format!("{}{name}", self.0 / size));
            }
        }
        s.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for ByteSize {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(n) => Ok(ByteSize(n)),
            Raw::Str(s) => parse_bytes(&s).map(ByteSize).map_err(D::Error::custom),
        }
    }
}

fn parse_bytes(s: &str) -> Result<u64, String> {
    let t = s.trim();
    let split = t.find(|c: char| !c.is_ascii_digit()).unwrap_or(t.len());
    let (num, unit) = t.split_at(split);
    let n: u64 = num
        .parse()
        .map_err(|_| format!("{t:?} is not a byte size"))?;
    let mult = match unit.trim().to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "kib" | "k" => 1 << 10,
        "mib" | "m" => 1 << 20,
        "gib" | "g" => 1 << 30,
        other => return Err(format!("unknown unit {other:?} in {t:?}")),
    };
    n.checked_mul(mult)
        .ok_or_else(|| format!("{t:?} overflows"))
}

// --- defaults -------------------------------------------------------------

impl Default for Device {
    fn default() -> Self {
        Device {
            name: "LP Frame".into(),
            metadata_pipe: "/tmp/shairport-sync-metadata".into(),
        }
    }
}

impl Default for Audio {
    fn default() -> Self {
        Audio {
            alsa_device: "default".into(),
            mixer: String::new(),
        }
    }
}

impl Default for Display {
    fn default() -> Self {
        Display {
            connector: "auto".into(),
            mode: "auto".into(),
            rotation: 0,
            card: "auto".into(),
            margin_percent: 0.0,
        }
    }
}

impl Default for Render {
    fn default() -> Self {
        Render {
            square: true,
            fit: Fit::Cover,
            // Blurred fill by default so a non-1:1 panel looks deliberate
            // rather than broken; invisible on a square one.
            background: Background::Blur,
            background_dim: 0.35,
            crossfade: Dur::from_millis(600),
            enrichment_crossfade: Dur::from_millis(250),
            ken_burns: false,
            ken_burns_period: Dur::from_secs(180),
            ambient: false,
            ambient_dim: 0.12,
            placeholder: "/usr/share/lpframe/placeholder.png".into(),
        }
    }
}

impl Default for Enrichment {
    fn default() -> Self {
        Enrichment {
            enabled: true,
            strictness: Strictness::TextAndVisual,
            // MusicBrainz requires a contactable User-Agent, so it cannot
            // be on by default — enable it alongside `contact`.
            sources: vec!["itunes".into()],
            itunes_country: "GB".into(),
            max_dimension: 3000,
            contact: String::new(),
            rate_limit_per_min: 15,
        }
    }
}

impl Default for Cache {
    fn default() -> Self {
        Cache {
            dir: "/var/lib/lpframe/cache".into(),
            max_bytes: ByteSize(2 << 30),
            negative_ttl: Dur::from_secs(7 * 24 * 3600),
        }
    }
}

impl Default for Amp {
    fn default() -> Self {
        Amp {
            enabled: true,
            gpio_chip: "auto".into(),
            gpio_line: 17,
            active_low: false,
            pulse: Dur::from_millis(0),
            on_event: AmpTrigger::SessionBegin,
            off_delay: Dur::from_secs(600),
            debounce: Dur::from_secs(2),
        }
    }
}

impl Default for DisplayPower {
    fn default() -> Self {
        DisplayPower {
            blank_after: Dur::from_secs(300),
            ambient_after: MaybeDuration::OFF,
            fade_out: Dur::from_millis(1000),
        }
    }
}

impl Default for Timeouts {
    fn default() -> Self {
        Timeouts {
            stall: Dur::from_secs(15),
            session: Dur::from_secs(60),
        }
    }
}

impl Default for Ipc {
    fn default() -> Self {
        Ipc {
            socket: lpframe_proto::DEFAULT_SOCKET.into(),
            art_dir: "/run/lpframe/art".into(),
            art_retain: 4,
        }
    }
}

impl Default for Web {
    fn default() -> Self {
        Web {
            enabled: true,
            // LAN by default, because the interface is the only way to
            // configure a device with no buttons and no keyboard. Safe only
            // because `auth` below defaults on and `validate` refuses the
            // combination of a non-loopback bind and no authentication.
            bind: "0.0.0.0:8730".into(),
            auth: true,
            password_hash: String::new(),
            mdns: true,
            insecure_no_auth: false,
        }
    }
}

impl Default for Logging {
    fn default() -> Self {
        Logging {
            level: "info".into(),
            event_buffer: 200,
        }
    }
}

// --- loading --------------------------------------------------------------

/// A loaded configuration, plus which keys came from the local override.
#[derive(Debug, Clone)]
pub struct Loaded {
    pub config: Config,
    /// Dotted paths present in the local override, e.g. `render.ambient`.
    /// The settings page marks these and offers a reset.
    pub overridden: BTreeSet<String>,
}

impl Config {
    /// Parse from a TOML string. Unknown keys are rejected — a typo in a
    /// config file should be loud, not silently ignored.
    pub fn from_toml(text: &str, path: &Path) -> Result<Config, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Load base + local override, merged.
    ///
    /// A missing file is not an error at either layer: a fresh device has no
    /// local overrides, and an all-defaults device needs no base file.
    pub fn load(base: &Path, local: &Path) -> Result<Loaded, ConfigError> {
        let base_value = read_toml(base)?;
        let local_value = read_toml(local)?;

        let mut merged = base_value.unwrap_or_else(|| toml::Value::Table(Default::default()));
        let mut overridden = BTreeSet::new();
        if let Some(l) = &local_value {
            merge(&mut merged, l);
            collect_paths(l, String::new(), &mut overridden);
        }

        let config: Config = merged.try_into().map_err(|source| ConfigError::Parse {
            path: local_value.map_or_else(|| base.to_path_buf(), |_| local.to_path_buf()),
            source,
        })?;
        config.validate()?;
        Ok(Loaded { config, overridden })
    }

    /// Reject combinations that would produce confusing behaviour later.
    pub fn validate(&self) -> Result<(), ConfigError> {
        let bad = |m: String| Err(ConfigError::Invalid(m));

        if !matches!(self.display.rotation, 0 | 90 | 180 | 270) {
            return bad(format!(
                "display.rotation must be 0, 90, 180 or 270, not {}",
                self.display.rotation
            ));
        }
        if !(0.0..=45.0).contains(&self.display.margin_percent) {
            return bad("display.margin_percent must be between 0 and 45".into());
        }
        for (name, v) in [
            ("render.background_dim", self.render.background_dim),
            ("render.ambient_dim", self.render.ambient_dim),
        ] {
            if !(0.0..=1.0).contains(&v) {
                return bad(format!("{name} must be between 0 and 1, not {v}"));
            }
        }
        if self.timeouts.stall.0 >= self.timeouts.session.0 {
            // Otherwise the session timeout fires first and the paused state
            // is unreachable, which reads as "it just goes dark".
            return bad(format!(
                "timeouts.stall ({}) must be shorter than timeouts.session ({})",
                self.timeouts.stall, self.timeouts.session
            ));
        }
        if let Some(ambient) = self.power.display.ambient_after.0 {
            if ambient >= self.power.display.blank_after.0 {
                return bad(format!(
                    "power.display.ambient_after ({}) must be shorter than blank_after ({})",
                    self.power.display.ambient_after, self.power.display.blank_after
                ));
            }
        }
        if self.ipc.art_retain < 2 {
            return bad("ipc.art_retain must be at least 2".into());
        }
        if self.enrichment.enabled {
            let known = ["itunes", "musicbrainz"];
            for s in &self.enrichment.sources {
                if !known.contains(&s.as_str()) {
                    return bad(format!(
                        "enrichment.sources contains unknown source {s:?}; known: {known:?}"
                    ));
                }
            }
            if self.enrichment.sources.iter().any(|s| s == "musicbrainz")
                && self.enrichment.contact.trim().is_empty()
            {
                // MusicBrainz requires a contactable User-Agent; sending
                // requests without one gets the device blocked.
                return bad("enrichment.contact must be set when musicbrainz is enabled".into());
            }
        }
        if self.web.enabled {
            let Ok(addr) = self.web.bind.parse::<std::net::SocketAddr>() else {
                return bad(format!(
                    "web.bind {:?} is not a valid address:port",
                    self.web.bind
                ));
            };
            // Checked here rather than only at startup so the settings page
            // cannot write the combination either: turning auth off while
            // bound to the LAN is a change whose consequence only shows up at
            // the next reboot, by which point the device is unreachable.
            if !addr.ip().is_loopback() && !self.web.auth && !self.web.insecure_no_auth {
                return bad(format!(
                    "web.bind is {addr} with web.auth = false, which would serve the \
                     listening history and every setting to anything on the network. \
                     Set web.auth = true, or web.bind = \"127.0.0.1:{}\", or — if \
                     something in front of this authenticates for you — \
                     web.insecure_no_auth = true.",
                    addr.port()
                ));
            }
        }
        Ok(())
    }
}

/// Every setting flattened to its dotted path, e.g. `render.ambient`.
///
/// The settings page is generated from this rather than from a hand-written
/// list, so a new key in the schema appears in the interface without anyone
/// remembering to add it.
pub fn flatten(config: &Config) -> BTreeMap<String, toml::Value> {
    let value = toml::Value::try_from(config).expect("the config schema always serialises");
    let mut out = BTreeMap::new();
    collect_leaves(&value, String::new(), &mut out);
    out
}

/// The default value of every setting, keyed by dotted path.
pub fn defaults() -> BTreeMap<String, toml::Value> {
    flatten(&Config::default())
}

fn collect_leaves(v: &toml::Value, prefix: String, out: &mut BTreeMap<String, toml::Value>) {
    match v {
        toml::Value::Table(t) => {
            for (k, child) in t {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                collect_leaves(child, path, out);
            }
        }
        other => {
            if !prefix.is_empty() {
                out.insert(prefix, other.clone());
            }
        }
    }
}

fn read_toml(path: &Path) -> Result<Option<toml::Value>, ConfigError> {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            let v: toml::Value = toml::from_str(&text).map_err(|source| ConfigError::Parse {
                path: path.to_path_buf(),
                source,
            })?;
            Ok(Some(v))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(source) => Err(ConfigError::Read {
            path: path.to_path_buf(),
            source,
        }),
    }
}

/// Deep-merge `overlay` into `base`, per key. Tables recurse; every other
/// value replaces wholesale, including arrays — a half-overridden source list
/// is never what anyone means.
fn merge(base: &mut toml::Value, overlay: &toml::Value) {
    match (base, overlay) {
        (toml::Value::Table(b), toml::Value::Table(o)) => {
            for (k, v) in o {
                match b.get_mut(k) {
                    Some(existing) => merge(existing, v),
                    None => {
                        b.insert(k.clone(), v.clone());
                    }
                }
            }
        }
        (b, o) => *b = o.clone(),
    }
}

fn collect_paths(v: &toml::Value, prefix: String, out: &mut BTreeSet<String>) {
    match v {
        toml::Value::Table(t) => {
            for (k, child) in t {
                let path = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                collect_paths(child, path, out);
            }
        }
        _ => {
            if !prefix.is_empty() {
                out.insert(prefix);
            }
        }
    }
}

#[cfg(test)]
mod tests;
