//! The `artd` ↔ client protocol: newline-delimited JSON over a Unix socket.
//!
//! Shared by both ends so the wire format cannot drift between them.
//!
//! Two rules make this protocol forgiving, and both matter for a device that
//! restarts its own components (DESIGN §5.3):
//!
//! * **Every change publishes a full snapshot**, never a delta. A client that
//!   connects late, reconnects, or misses a message is immediately correct.
//! * **Unknown message types and unknown fields are ignored**, so a newer
//!   `artd` and an older renderer keep working.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Envelope version. Bumped only for an incompatible reshaping.
pub const ENVELOPE_VERSION: u32 = 1;

/// Highest client protocol this build speaks.
pub const PROTOCOL_VERSION: u32 = 1;

/// Default socket path.
pub const DEFAULT_SOCKET: &str = "/run/lpframe/artd.sock";

// --- playback -------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Playback {
    /// No session. Nothing is connected.
    #[default]
    Idle,
    /// A sender is connected but no audio is flowing — including the gaps
    /// between tracks, which is why display blanking keys off `Idle` and not
    /// off `PlayEnd` (DESIGN §5.1.1).
    Active,
    /// A play stream has begun but audio has not been confirmed yet.
    Starting,
    Playing,
    Paused,
}

impl Playback {
    /// Whether a sender is attached at all.
    pub fn is_session(self) -> bool {
        !matches!(self, Playback::Idle)
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Playback::Idle => "idle",
            Playback::Active => "active",
            Playback::Starting => "starting",
            Playback::Playing => "playing",
            Playback::Paused => "paused",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum DisplayPower {
    On,
    /// Blurred, dimmed last artwork.
    Ambient,
    #[default]
    Off,
}

impl DisplayPower {
    pub fn as_str(self) -> &'static str {
        match self {
            DisplayPower::On => "on",
            DisplayPower::Ambient => "ambient",
            DisplayPower::Off => "off",
        }
    }
}

/// Where an artwork image came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtworkSource {
    /// Delivered over AirPlay. Always shown immediately.
    Airplay,
    Itunes,
    CoverArtArchive,
    Cache,
    Placeholder,
}

// --- state ----------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Session {
    pub active: bool,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub user_agent: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub client_ip: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Track {
    /// Stable identity for this track: the sender's persistent ID when it
    /// gives one, otherwise a hash of artist/album/title. Used to tell a
    /// genuinely new track from a resent identical bundle.
    pub id: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub album_artist: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub genre: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub composer: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub duration_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub track_number: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Artwork {
    /// Monotonic. **The renderer's sole trigger for a crossfade.** Identical
    /// artwork resent by the sender does not bump this.
    pub revision: u64,
    /// File on tmpfs. Image bytes never traverse the socket.
    pub path: PathBuf,
    pub sha256: String,
    pub bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub height: Option<u32>,
    pub source: ArtworkSource,
    /// True when this replaces the same album's art with a higher-resolution
    /// copy, so the renderer can use a shorter crossfade.
    #[serde(default)]
    pub is_upgrade: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Progress {
    pub position_ms: u64,
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    pub airplay_db: f32,
    pub muted: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct Power {
    pub amp: bool,
    pub display: DisplayPower,
}

/// The complete published state. Sent in full on every change.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct State {
    pub playback: Playback,
    pub session: Session,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub track: Option<Track>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub artwork: Option<Artwork>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub progress: Option<Progress>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub volume: Option<Volume>,
    pub power: Power,
}

// --- messages -------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ServerMessage {
    Hello {
        v: u32,
        server: String,
        version: String,
        /// Highest protocol this server speaks.
        proto: u32,
    },
    State {
        v: u32,
        seq: u64,
        ts: String,
        state: Box<State>,
    },
    Pong {
        v: u32,
    },
    /// Surfaced to the diagnostics page; not an error channel for the socket.
    Log {
        v: u32,
        level: String,
        msg: String,
    },
}

impl ServerMessage {
    pub fn hello() -> Self {
        ServerMessage::Hello {
            v: ENVELOPE_VERSION,
            server: "artd".into(),
            version: env!("CARGO_PKG_VERSION").into(),
            proto: PROTOCOL_VERSION,
        }
    }

    pub fn state(seq: u64, ts: String, state: State) -> Self {
        ServerMessage::State {
            v: ENVELOPE_VERSION,
            seq,
            ts,
            state: Box::new(state),
        }
    }

    /// Serialise as one NDJSON line, terminator included.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("ServerMessage is always serialisable");
        s.push('\n');
        s
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ClientMessage {
    Hello {
        #[serde(default)]
        client: String,
        #[serde(default)]
        proto: u32,
    },
    GetState,
    Ping,
    SetDisplay {
        value: DisplayPower,
    },
    /// Override the amplifier trigger.
    ///
    /// An override, not a mode: the next session or idle transition moves the
    /// line again. It exists so the trigger can be tested against a
    /// multimeter without waiting for a listening session, and so the
    /// Diagnostics page's buttons do something.
    SetAmp {
        value: bool,
    },
    /// Debug builds only; rejected otherwise.
    InjectArtwork {
        path: PathBuf,
    },
    /// Anything this build does not recognise. Ignored, never fatal — this is
    /// what lets a newer client talk to an older daemon.
    #[serde(other)]
    Unknown,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_message_matches_the_documented_shape() {
        let msg = ServerMessage::state(42, "2026-07-27T10:14:02.113Z".into(), State::default());
        let v: serde_json::Value = serde_json::from_str(&msg.to_line()).unwrap();
        assert_eq!(v["type"], "state");
        assert_eq!(v["v"], 1);
        assert_eq!(v["seq"], 42);
        assert_eq!(v["state"]["playback"], "idle");
        assert_eq!(v["state"]["power"]["display"], "off");
        // Absent optionals must be omitted, not null.
        assert!(v["state"].get("track").is_none());
    }

    #[test]
    fn lines_are_newline_terminated_and_contain_no_embedded_newlines() {
        let line = ServerMessage::hello().to_line();
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
    }

    #[test]
    fn unknown_client_messages_decode_rather_than_failing() {
        let m: ClientMessage = serde_json::from_str(r#"{"type":"teleport","x":1}"#).unwrap();
        assert_eq!(m, ClientMessage::Unknown);
    }

    #[test]
    fn unknown_fields_are_ignored() {
        let m: ClientMessage =
            serde_json::from_str(r#"{"type":"ping","future_field":true}"#).unwrap();
        assert_eq!(m, ClientMessage::Ping);
    }

    #[test]
    fn client_messages_round_trip() {
        for m in [
            ClientMessage::Hello {
                client: "lprender".into(),
                proto: 1,
            },
            ClientMessage::GetState,
            ClientMessage::Ping,
            ClientMessage::SetDisplay {
                value: DisplayPower::Ambient,
            },
        ] {
            let s = serde_json::to_string(&m).unwrap();
            assert_eq!(serde_json::from_str::<ClientMessage>(&s).unwrap(), m);
        }
    }

    #[test]
    fn a_full_state_round_trips() {
        let state = State {
            playback: Playback::Playing,
            session: Session {
                active: true,
                client_name: Some("Ben's iPhone".into()),
                user_agent: Some("AirPlay/845.5.1".into()),
                client_ip: None,
            },
            track: Some(Track {
                id: "abc".into(),
                title: Some("Teardrop".into()),
                artist: Some("Massive Attack".into()),
                album: Some("Mezzanine".into()),
                ..Default::default()
            }),
            artwork: Some(Artwork {
                revision: 7,
                path: "/run/lpframe/art/0007-4f3a9c1e.jpg".into(),
                sha256: "4f3a9c1e".into(),
                bytes: 1234,
                width: Some(3000),
                height: Some(3000),
                source: ArtworkSource::Itunes,
                is_upgrade: true,
            }),
            progress: Some(Progress {
                position_ms: 12480,
                duration_ms: 330_000,
            }),
            volume: Some(Volume {
                airplay_db: -14.5,
                muted: false,
            }),
            power: Power {
                amp: true,
                display: DisplayPower::On,
            },
        };
        let json = serde_json::to_string(&state).unwrap();
        assert_eq!(serde_json::from_str::<State>(&json).unwrap(), state);
    }
}

#[cfg(test)]
mod amp_wire {
    use super::*;

    #[test]
    fn set_amp_round_trips_and_is_not_swallowed_as_unknown() {
        // `#[serde(other)]` means a variant the daemon does not know about
        // decodes to Unknown and is ignored rather than being an error — so
        // a message that fails to match its own name fails silently, and the
        // symptom is a command that does nothing.
        for value in [true, false] {
            let sent = ClientMessage::SetAmp { value };
            let json = serde_json::to_string(&sent).unwrap();
            assert_eq!(json, format!(r#"{{"type":"set_amp","value":{value}}}"#));
            assert_eq!(serde_json::from_str::<ClientMessage>(&json).unwrap(), sent);
        }
    }
}
