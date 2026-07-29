//! The amplifier trigger: one GPIO line through an optocoupler (DESIGN §5.5).
//!
//! **The line driving is unverified against hardware.** There is no
//! `/dev/gpiochip*` in the development container and no `gpio-sim` to make
//! one, so nothing here has actually toggled a pin. What *is* tested is
//! everything around it — chip selection, the pulse and hold policy, the
//! minimum interval between transitions, and the guarantee that the line ends
//! up inactive on shutdown — because those are pure and they are where the
//! behaviour that matters lives.
//!
//! ## The hardware requirement this code cannot enforce
//!
//! When the process exits the kernel releases the line request and the pin
//! reverts to its power-on state, which is an input. Nothing in software runs
//! at that moment. **The optocoupler input must therefore have an external
//! pull-down** (or pull-up, if `active_low`) holding it in the amplifier-off
//! state. Without it, a crash can leave an amplifier powered indefinitely.
//! `Drop` and the SIGTERM path below cover the orderly cases; the resistor
//! covers the rest, and it is the only thing that covers a power cut on the
//! Pi alone.

use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use lpframe_config::Amp;

use crate::runtime::AmpBackend;

/// Chip labels that carry the 40-pin header, most recent first.
///
/// Selection is by label and never by index: the Pi 5 moved the header to the
/// RP1 pin controller, and `gpiochipN` numbering has shifted between kernel
/// releases on both boards. A config that says `gpiochip0` is a config that
/// breaks on an update.
const HEADER_LABELS: &[&str] = &["pinctrl-rp1", "pinctrl-bcm2835", "pinctrl-bcm2711"];

/// One candidate GPIO chip, as read from the kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChipCandidate {
    pub path: PathBuf,
    pub label: String,
    pub num_lines: u32,
}

/// Choose which chip to drive.
///
/// `want` is either `"auto"`, a chip label, or a device path. Pure, so the
/// selection rules are testable without a kernel that has any GPIO at all.
pub fn choose_chip(candidates: &[ChipCandidate], want: &str, line: u32) -> Result<PathBuf> {
    if candidates.is_empty() {
        bail!(
            "no GPIO chips found. On a Pi this means the kernel has no pin \
             controller bound, which is not a configuration lpframe can fix; \
             set power.amp.enabled = false to run without an amplifier trigger"
        );
    }

    let describe = || {
        candidates
            .iter()
            .map(|c| format!("{} ({}, {} lines)", c.path.display(), c.label, c.num_lines))
            .collect::<Vec<_>>()
            .join(", ")
    };

    let chosen = if want.starts_with('/') {
        candidates
            .iter()
            .find(|c| c.path == Path::new(want))
            .ok_or_else(|| anyhow::anyhow!("no GPIO chip at {want}. Found: {}", describe()))?
    } else if want != "auto" {
        candidates.iter().find(|c| c.label == want).ok_or_else(|| {
            anyhow::anyhow!("no GPIO chip labelled {want:?}. Found: {}", describe())
        })?
    } else {
        HEADER_LABELS
            .iter()
            .find_map(|want| candidates.iter().find(|c| c.label == *want))
            // Nothing recognised: fall back to the first chip with enough
            // lines rather than refusing to start. A board we have not heard
            // of is more likely than a broken one.
            .or_else(|| candidates.iter().find(|c| c.num_lines > line))
            .ok_or_else(|| {
                anyhow::anyhow!("no GPIO chip exposes line {line}. Found: {}", describe())
            })?
    };

    if chosen.num_lines <= line {
        bail!(
            "{} ({}) has {} lines, so line {line} does not exist",
            chosen.path.display(),
            chosen.label,
            chosen.num_lines
        );
    }
    Ok(chosen.path.clone())
}

/// Enumerate the chips the kernel is offering.
pub fn discover() -> Result<Vec<ChipCandidate>> {
    let mut out = Vec::new();
    for path in gpiocdev::chip::chips().context("listing GPIO chips")? {
        let Ok(chip) = gpiocdev::chip::Chip::from_path(&path) else {
            continue;
        };
        let Ok(info) = chip.info() else { continue };
        out.push(ChipCandidate {
            path,
            label: info.label,
            num_lines: info.num_lines,
        });
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

/// What the worker should do with the line for a given desired state.
///
/// Separated from the doing so the policy is testable: a level-hold amplifier
/// and a toggle-style one need genuinely different pin behaviour, and getting
/// it backwards means an amp that is on when the music stops.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Hold the line at this level until told otherwise.
    Hold(bool),
    /// Emit a momentary pulse; the line returns to inactive afterwards.
    Pulse(Duration),
}

pub fn action_for(cfg: &Amp, on: bool) -> Action {
    match cfg.pulse.as_millis() {
        0 => Action::Hold(on),
        // A toggle-style trigger has no notion of "on": both directions are
        // the same momentary contact closure, and the amplifier keeps its own
        // state. Sending a level here would latch it permanently.
        ms => Action::Pulse(Duration::from_millis(ms)),
    }
}

/// A message to the line worker.
enum Msg {
    Set(bool),
    /// Drive inactive and stop. Sent by `Drop`.
    Shutdown,
}

/// The amplifier trigger, driven from a worker thread.
///
/// A thread rather than inline calls because pulse and minimum-hold timing
/// both need to sleep, and `Core::handle` runs on the daemon's event loop —
/// blocking it for a second to stretch a relay pulse would stall metadata.
pub struct GpioAmp {
    tx: mpsc::Sender<Msg>,
    worker: Option<JoinHandle<()>>,
    chip: PathBuf,
    line: u32,
}

impl GpioAmp {
    pub fn open(cfg: &Amp) -> Result<GpioAmp> {
        let candidates = discover()?;
        let chip = choose_chip(&candidates, &cfg.gpio_chip, cfg.gpio_line)?;

        let mut builder = gpiocdev::Request::builder();
        builder
            .on_chip(&chip)
            .with_consumer("lpframe-amp")
            .with_line(cfg.gpio_line)
            // Requested inactive: taking the line must never be the thing
            // that switches an amplifier on.
            .as_output(gpiocdev::line::Value::Inactive);
        if cfg.active_low {
            builder.as_active_low();
        }
        let request = builder.request().with_context(|| {
            format!(
                "requesting line {} on {}. Another process may hold it — check \
                 `gpioinfo` — or lpframe may lack access to the gpio group",
                cfg.gpio_line,
                chip.display()
            )
        })?;

        tracing::info!(
            "amp trigger on {} line {}{}",
            chip.display(),
            cfg.gpio_line,
            if cfg.active_low { " (active low)" } else { "" }
        );

        let (tx, rx) = mpsc::channel();
        let cfg = cfg.clone();
        let line = cfg.gpio_line;
        let worker = std::thread::Builder::new()
            .name("lpframe-amp".into())
            .spawn(move || run(request, cfg, rx))
            .context("spawning the amp worker")?;

        Ok(GpioAmp {
            tx,
            worker: Some(worker),
            chip,
            line,
        })
    }

    pub fn describe(&self) -> String {
        format!("{} line {}", self.chip.display(), self.line)
    }
}

impl AmpBackend for GpioAmp {
    fn set(&mut self, on: bool) -> Result<()> {
        // A closed channel means the worker died; the line has already
        // reverted, so there is nothing useful left to do but say so.
        self.tx
            .send(Msg::Set(on))
            .map_err(|_| anyhow::anyhow!("the amp worker has stopped"))
    }
}

impl Drop for GpioAmp {
    fn drop(&mut self) {
        // Orderly shutdown drives the line inactive before releasing it. The
        // external pull-down is what covers the disorderly cases.
        let _ = self.tx.send(Msg::Shutdown);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn run(request: gpiocdev::Request, cfg: Amp, rx: mpsc::Receiver<Msg>) {
    use gpiocdev::line::Value;

    let min_interval = Duration::from_millis(cfg.debounce.as_millis());
    let mut last_change: Option<Instant> = None;
    let mut current = false;

    let apply = |value: Value| {
        if let Err(e) = request.set_lone_value(value) {
            tracing::error!("could not drive the amp line: {e}");
        }
    };

    // Ends on a Shutdown or on the sender being dropped; either way the line
    // is driven inactive below.
    while let Ok(msg) = rx.recv() {
        let on = match msg {
            Msg::Shutdown => break,
            Msg::Set(on) => on,
        };

        // For a level-hold trigger a repeat is a no-op. For a pulse trigger
        // it would emit a spurious contact closure and toggle the amplifier
        // to the wrong state, so it is suppressed there too.
        if on == current {
            continue;
        }

        // Hold the previous state for at least the configured interval, so a
        // rapid sequence cannot chatter a relay. Sleeping here is safe: this
        // is the worker, not the event loop.
        if let Some(last) = last_change {
            let elapsed = last.elapsed();
            if elapsed < min_interval {
                std::thread::sleep(min_interval - elapsed);
            }
        }

        match action_for(&cfg, on) {
            Action::Hold(level) => {
                apply(if level {
                    Value::Active
                } else {
                    Value::Inactive
                });
            }
            Action::Pulse(width) => {
                apply(Value::Active);
                std::thread::sleep(width);
                apply(Value::Inactive);
            }
        }
        current = on;
        last_change = Some(Instant::now());
        tracing::debug!("amp line -> {}", if on { "on" } else { "off" });
    }

    // Whatever brought us here, leave the amplifier off.
    apply(Value::Inactive);
    tracing::info!("amp line released inactive");
}

#[cfg(test)]
mod tests {
    use super::*;
    use lpframe_config::Dur;

    fn candidates() -> Vec<ChipCandidate> {
        vec![
            ChipCandidate {
                path: "/dev/gpiochip0".into(),
                label: "pinctrl-rp1".into(),
                num_lines: 54,
            },
            ChipCandidate {
                path: "/dev/gpiochip1".into(),
                label: "rp1-gpio-aon".into(),
                num_lines: 8,
            },
        ]
    }

    #[test]
    fn auto_prefers_the_header_controller_over_whatever_is_first() {
        // The Pi 5 exposes several chips; picking by index lands on the wrong
        // one as soon as enumeration order changes.
        let mut c = candidates();
        c.reverse();
        assert_eq!(
            choose_chip(&c, "auto", 17).unwrap(),
            Path::new("/dev/gpiochip0")
        );
    }

    #[test]
    fn auto_falls_back_to_any_chip_with_the_line_on_an_unknown_board() {
        let c = vec![ChipCandidate {
            path: "/dev/gpiochip0".into(),
            label: "some-future-soc".into(),
            num_lines: 32,
        }];
        assert_eq!(
            choose_chip(&c, "auto", 17).unwrap(),
            Path::new("/dev/gpiochip0")
        );
    }

    #[test]
    fn an_explicit_label_or_path_is_honoured() {
        let c = candidates();
        assert_eq!(
            choose_chip(&c, "rp1-gpio-aon", 4).unwrap(),
            Path::new("/dev/gpiochip1")
        );
        assert_eq!(
            choose_chip(&c, "/dev/gpiochip1", 4).unwrap(),
            Path::new("/dev/gpiochip1")
        );
    }

    #[test]
    fn a_missing_chip_names_what_was_actually_found() {
        // The user is reading this over SSH with no other diagnostics.
        let err = choose_chip(&candidates(), "pinctrl-bcm2835", 17)
            .unwrap_err()
            .to_string();
        assert!(err.contains("pinctrl-rp1"), "{err}");
        assert!(err.contains("gpiochip0"), "{err}");
    }

    #[test]
    fn a_line_beyond_the_chip_is_rejected_rather_than_silently_wrong() {
        let err = choose_chip(&candidates(), "rp1-gpio-aon", 40)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn no_chips_at_all_suggests_the_way_out() {
        let err = choose_chip(&[], "auto", 17).unwrap_err().to_string();
        assert!(err.contains("power.amp.enabled = false"), "{err}");
    }

    #[test]
    fn a_zero_pulse_holds_the_level_and_a_nonzero_one_pulses() {
        // Getting this backwards on a toggle-style amplifier means it ends up
        // in the opposite state to the one requested.
        let mut cfg = Amp::default();
        assert_eq!(action_for(&cfg, true), Action::Hold(true));
        assert_eq!(action_for(&cfg, false), Action::Hold(false));

        cfg.pulse = Dur::from_millis(120);
        assert_eq!(
            action_for(&cfg, true),
            Action::Pulse(Duration::from_millis(120))
        );
        // Both directions are the same momentary closure: the amplifier keeps
        // its own state, and sending a level would latch it.
        assert_eq!(
            action_for(&cfg, false),
            Action::Pulse(Duration::from_millis(120))
        );
    }
}
