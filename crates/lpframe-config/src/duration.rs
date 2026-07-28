//! Human-readable durations in TOML: `"600ms"`, `"15s"`, `"10m"`, `"2h"`.
//!
//! Also accepts `"off"` / `"never"` for settings that can be disabled, which
//! is why this is hand-rolled rather than a dependency: the disabled case is
//! part of the type, not a sentinel value a user has to guess at.

use std::fmt;
use std::time::Duration;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// A duration that may be switched off entirely.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MaybeDuration(pub Option<Duration>);

impl MaybeDuration {
    pub const OFF: MaybeDuration = MaybeDuration(None);

    pub fn secs(n: u64) -> Self {
        MaybeDuration(Some(Duration::from_secs(n)))
    }

    pub fn millis(n: u64) -> Self {
        MaybeDuration(Some(Duration::from_millis(n)))
    }

    pub fn as_millis(self) -> Option<u64> {
        self.0.map(|d| d.as_millis() as u64)
    }

    pub fn is_off(self) -> bool {
        self.0.is_none()
    }
}

impl fmt::Display for MaybeDuration {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.0 {
            None => f.write_str("off"),
            Some(d) => f.write_str(&render(d)),
        }
    }
}

/// A duration that must be present.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dur(pub Duration);

impl Dur {
    pub const fn from_millis(n: u64) -> Self {
        Dur(Duration::from_millis(n))
    }
    pub const fn from_secs(n: u64) -> Self {
        Dur(Duration::from_secs(n))
    }
    pub fn as_millis(self) -> u64 {
        self.0.as_millis() as u64
    }
}

impl fmt::Display for Dur {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render(self.0))
    }
}

fn render(d: Duration) -> String {
    let ms = d.as_millis() as u64;
    if ms == 0 {
        return "0s".into();
    }
    if ms % 3_600_000 == 0 {
        format!("{}h", ms / 3_600_000)
    } else if ms % 60_000 == 0 {
        format!("{}m", ms / 60_000)
    } else if ms % 1000 == 0 {
        format!("{}s", ms / 1000)
    } else {
        format!("{ms}ms")
    }
}

/// Parse `"250ms"`, `"15s"`, `"10m"`, `"2h"`. A bare number is rejected:
/// silently guessing seconds versus milliseconds is exactly the kind of
/// ambiguity that produces a screen that blanks 1000× too early.
pub fn parse(s: &str) -> Result<Duration, String> {
    let t = s.trim();
    if t.is_empty() {
        return Err("empty duration".into());
    }
    let split = t
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .ok_or_else(|| format!("{t:?} has no unit; use ms, s, m or h"))?;
    let (num, unit) = t.split_at(split);
    let value: f64 = num
        .parse()
        .map_err(|_| format!("{num:?} is not a number in {t:?}"))?;
    if value < 0.0 {
        return Err(format!("{t:?} is negative"));
    }
    let millis = match unit.trim() {
        "ms" => value,
        "s" => value * 1000.0,
        "m" => value * 60_000.0,
        "h" => value * 3_600_000.0,
        other => {
            return Err(format!(
                "unknown unit {other:?} in {t:?}; use ms, s, m or h"
            ))
        }
    };
    Ok(Duration::from_millis(millis.round() as u64))
}

struct DurVisitor;

impl Visitor<'_> for DurVisitor {
    type Value = Dur;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a duration such as \"600ms\", \"15s\", \"10m\" or \"2h\"")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<Dur, E> {
        parse(v).map(Dur).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for Dur {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_str(DurVisitor)
    }
}

impl Serialize for Dur {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

struct MaybeVisitor;

impl Visitor<'_> for MaybeVisitor {
    type Value = MaybeDuration;
    fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
        f.write_str("a duration such as \"30s\", or \"off\"")
    }
    fn visit_str<E: de::Error>(self, v: &str) -> Result<MaybeDuration, E> {
        let t = v.trim();
        if t.eq_ignore_ascii_case("off") || t.eq_ignore_ascii_case("never") {
            return Ok(MaybeDuration(None));
        }
        parse(t).map(|d| MaybeDuration(Some(d))).map_err(E::custom)
    }
}

impl<'de> Deserialize<'de> for MaybeDuration {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        d.deserialize_str(MaybeVisitor)
    }
}

impl Serialize for MaybeDuration {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_unit() {
        assert_eq!(parse("250ms").unwrap(), Duration::from_millis(250));
        assert_eq!(parse("15s").unwrap(), Duration::from_secs(15));
        assert_eq!(parse("10m").unwrap(), Duration::from_secs(600));
        assert_eq!(parse("2h").unwrap(), Duration::from_secs(7200));
        assert_eq!(parse(" 1.5s ").unwrap(), Duration::from_millis(1500));
    }

    #[test]
    fn rejects_a_bare_number() {
        // Guessing the unit here would be a silent 1000x error.
        assert!(parse("30").is_err());
        assert!(parse("").is_err());
        assert!(parse("30 fortnights").is_err());
        assert!(parse("-5s").is_err());
    }

    #[test]
    fn off_is_a_value_not_a_sentinel() {
        let d: MaybeDuration = toml::from_str::<toml::Value>("x = \"off\"")
            .unwrap()
            .get("x")
            .unwrap()
            .clone()
            .try_into()
            .unwrap();
        assert!(d.is_off());
    }

    #[test]
    fn renders_back_to_the_shortest_exact_form() {
        assert_eq!(Dur::from_secs(600).to_string(), "10m");
        assert_eq!(Dur::from_secs(15).to_string(), "15s");
        assert_eq!(Dur::from_millis(600).to_string(), "600ms");
        assert_eq!(Dur::from_secs(7200).to_string(), "2h");
        assert_eq!(MaybeDuration::OFF.to_string(), "off");
    }

    #[test]
    fn round_trips_through_serde() {
        for s in ["250ms", "15s", "10m", "2h"] {
            let d: Dur = toml::from_str::<toml::Value>(&format!("x = {s:?}"))
                .unwrap()
                .get("x")
                .unwrap()
                .clone()
                .try_into()
                .unwrap();
            assert_eq!(d.to_string(), s);
        }
    }
}
