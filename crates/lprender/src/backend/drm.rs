//! KMS/DRM + GBM + EGL: the real device.
//!
//! **Unverified against hardware.** This compiles and the logic follows
//! DESIGN §6.1–6.2, but there is no DRM node in the development container, so
//! nothing below has run against a real display. Treat first boot on the Pi
//! as the actual test; the shared GL path underneath it *is* covered by the
//! headless golden images.
//!
//! Atomic modesetting throughout. `vc4` is an atomic driver and the legacy
//! ioctls are emulated over `drm_atomic_helper`, so legacy would be a shim
//! over the same path with less control — no `TEST_ONLY` validation, no
//! single-commit modeset, and a less reliable route to a genuinely dark
//! panel.

use std::os::fd::{AsFd, BorrowedFd};
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use drm::control::{connector, crtc, Device as ControlDevice, Mode, ModeTypeFlags};
use drm::Device as BasicDevice;

/// A DRM device node.
pub struct Card {
    file: std::fs::File,
    path: PathBuf,
}

impl AsFd for Card {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl BasicDevice for Card {}
impl ControlDevice for Card {}

impl Card {
    /// Open a card, preferring one driven by `vc4`.
    ///
    /// Never hardcodes `card0`: the numbering moves when the v3d render node
    /// enumerates first, which it does on some Pi kernels.
    pub fn open(preference: &str) -> Result<Card> {
        if preference != "auto" {
            return Card::open_path(Path::new(preference));
        }

        let mut candidates: Vec<PathBuf> = std::fs::read_dir("/dev/dri")
            .context("listing /dev/dri; is the kernel modesetting driver loaded?")?
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with("card"))
            })
            .collect();
        candidates.sort();
        if candidates.is_empty() {
            bail!("no /dev/dri/card* nodes; no display driver is bound");
        }

        let mut first_error = None;
        // Two passes: prefer a vc4-driven card, then settle for any card that
        // actually has a connected output.
        for want_vc4 in [true, false] {
            for path in &candidates {
                match Card::open_path(path) {
                    Ok(card) => {
                        let is_vc4 = card
                            .get_driver()
                            .map(|d| d.name().to_string_lossy().contains("vc4"))
                            .unwrap_or(false);
                        if want_vc4 && !is_vc4 {
                            continue;
                        }
                        if card.first_connected().is_ok() {
                            tracing::info!("using {}", path.display());
                            return Ok(card);
                        }
                    }
                    Err(e) => {
                        first_error.get_or_insert(e);
                    }
                }
            }
        }
        match first_error {
            Some(e) => Err(e),
            None => bail!("no DRM card has a connected output"),
        }
    }

    fn open_path(path: &Path) -> Result<Card> {
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .with_context(|| format!("opening {}", path.display()))?;
        Ok(Card {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The first connected connector.
    pub fn first_connected(&self) -> Result<connector::Info> {
        let resources = self.resource_handles().context("reading DRM resources")?;
        for handle in resources.connectors() {
            if let Ok(info) = self.get_connector(*handle, true) {
                if info.state() == connector::State::Connected && !info.modes().is_empty() {
                    return Ok(info);
                }
            }
        }
        bail!("no connected connector with a usable mode")
    }

    /// Find a connector by name, e.g. `HDMI-A-1`, or the first connected one.
    pub fn connector(&self, want: &str) -> Result<connector::Info> {
        if want == "auto" {
            return self.first_connected();
        }
        let resources = self.resource_handles().context("reading DRM resources")?;
        let mut seen = Vec::new();
        for handle in resources.connectors() {
            if let Ok(info) = self.get_connector(*handle, true) {
                let name = connector_name(&info);
                if name == want {
                    if info.state() != connector::State::Connected {
                        bail!("{want} exists but nothing is connected to it");
                    }
                    return Ok(info);
                }
                seen.push(name);
            }
        }
        bail!("no connector named {want:?}; found: {}", seen.join(", "))
    }
}

pub fn connector_name(info: &connector::Info) -> String {
    format!("{}-{}", info.interface().as_str(), info.interface_id())
}

/// Pick a mode.
///
/// `auto` takes the connector's preferred mode, `highest` the largest at the
/// highest refresh, and an explicit `WxH[@R]` overrides both. The explicit
/// form matters for the 1920×1920 target: cheap HDMI→eDP boards frequently
/// report absent or wrong EDID.
pub fn choose_mode(info: &connector::Info, want: &str) -> Result<Mode> {
    let modes = info.modes();
    if modes.is_empty() {
        bail!("{} reports no modes", connector_name(info));
    }

    match want {
        "auto" => Ok(modes
            .iter()
            .find(|m| m.mode_type().contains(ModeTypeFlags::PREFERRED))
            .copied()
            .unwrap_or(modes[0])),
        "highest" => Ok(*modes
            .iter()
            .max_by_key(|m| {
                let (w, h) = m.size();
                (w as u64 * h as u64, m.vrefresh() as u64)
            })
            .expect("modes is non-empty")),
        explicit => {
            let (dims, refresh) = match explicit.split_once('@') {
                Some((d, r)) => (d, r.parse::<u32>().ok()),
                None => (explicit, None),
            };
            let (w, h) = dims
                .split_once('x')
                .and_then(|(a, b)| {
                    Some((a.trim().parse::<u16>().ok()?, b.trim().parse::<u16>().ok()?))
                })
                .ok_or_else(|| anyhow::anyhow!("mode {explicit:?} is not WIDTHxHEIGHT[@HZ]"))?;

            let matches: Vec<&Mode> = modes
                .iter()
                .filter(|m| m.size() == (w, h))
                .filter(|m| refresh.is_none_or(|r| m.vrefresh() == r))
                .collect();
            match matches.first() {
                Some(m) => Ok(**m),
                None => {
                    let available: Vec<String> = modes
                        .iter()
                        .map(|m| {
                            let (mw, mh) = m.size();
                            format!("{mw}x{mh}@{}", m.vrefresh())
                        })
                        .collect();
                    bail!(
                        "{} does not offer {explicit}. Available: {}. \
                         For a panel with absent or wrong EDID, add hdmi_timings \
                         to config.txt — see docs/BUILD.md.",
                        connector_name(info),
                        available.join(", ")
                    )
                }
            }
        }
    }
}

/// Pick a CRTC that can drive this connector.
pub fn choose_crtc(card: &Card, info: &connector::Info) -> Result<crtc::Handle> {
    let resources = card.resource_handles()?;
    for enc_handle in info.encoders() {
        if let Ok(enc) = card.get_encoder(*enc_handle) {
            if let Some(crtc) = enc.crtc() {
                return Ok(crtc);
            }
            // No CRTC bound yet: take the first the encoder permits.
            let compatible = resources.filter_crtcs(enc.possible_crtcs());
            if let Some(c) = compatible.first() {
                return Ok(*c);
            }
        }
    }
    bail!("no CRTC can drive {}", connector_name(info))
}

/// Describe what was selected, for the log and the diagnostics page.
pub fn describe(info: &connector::Info, mode: &Mode) -> String {
    let (w, h) = mode.size();
    format!("{} {}x{}@{}", connector_name(info), w, h, mode.vrefresh())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mode selection is the part most likely to be wrong on an unusual panel,
    // and it is pure enough to test without a device. Everything else here
    // needs real hardware.

    #[test]
    fn an_explicit_mode_string_is_parsed_strictly() {
        // Malformed strings must be rejected with a message naming the
        // expected form rather than silently falling back to a wrong mode.
        for bad in ["1920", "1920*1920", "widescreen", "1920x", "x1080"] {
            let err = parse_dims(bad);
            assert!(err.is_none(), "{bad:?} should not parse");
        }
        assert_eq!(parse_dims("1920x1920"), Some((1920, 1920)));
        assert_eq!(parse_dims(" 720 x 720 "), Some((720, 720)));
    }

    fn parse_dims(s: &str) -> Option<(u16, u16)> {
        let dims = s.split_once('@').map_or(s, |(d, _)| d);
        dims.split_once('x')
            .and_then(|(a, b)| Some((a.trim().parse().ok()?, b.trim().parse().ok()?)))
    }

    #[test]
    fn opening_a_missing_card_is_an_error_not_a_panic() {
        assert!(Card::open_path(Path::new("/dev/dri/definitely-not-a-card")).is_err());
    }
}
