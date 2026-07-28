//! KMS/DRM + GBM + EGL: the real device.
//!
//! **Unverified against hardware.** This compiles and the logic follows
//! DESIGN §6.1–6.3, but there is no DRM node in the development container, so
//! nothing below has run against a real display: not the modeset, not a
//! single page flip. Treat first boot on the Pi as the actual test; the
//! shared GL path underneath it *is* covered by the headless golden images,
//! and the pure fragments here (mode parsing, framebuffer caching, the master
//! backoff) are covered by the unit tests at the bottom.
//!
//! Atomic modesetting throughout. `vc4` is an atomic driver and the legacy
//! ioctls are emulated over `drm_atomic_helper`, so legacy would be a shim
//! over the same path with less control — no `TEST_ONLY` validation, no
//! single-commit modeset, and a less reliable route to a genuinely dark
//! panel.
//!
//! `DRM_MODE_PAGE_FLIP_ASYNC` is never set. It is broken on the Pi 5's vc4
//! stack (raspberrypi/linux#5828), and vsync-synchronised flips are what the
//! render-on-demand loop wants anyway.

use std::collections::HashMap;
use std::ffi::c_void;
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use drm::control::atomic::AtomicModeReq;
use drm::control::{
    connector, crtc, framebuffer, plane, property, AtomicCommitFlags, Device as ControlDevice,
    FbCmd2Flags, Mode, ModeTypeFlags, PlaneType, ResourceHandle,
};
use drm::{ClientCapability, Device as BasicDevice};
use gbm::{AsRaw, BufferObjectFlags};
use glow::HasContext;
use lpframe_config::Config;

use crate::gl::Renderer;

type Egl = khronos_egl::Instance<khronos_egl::Static>;

/// `EGL_PLATFORM_GBM_KHR`.
const PLATFORM_GBM: khronos_egl::Enum = 0x31D7;

/// How long to wait for a page-flip event before giving up on the commit.
///
/// How long to wait for a page-flip event before giving up on it.
///
/// One second is far longer than any real refresh interval, including a slow
/// first flip after a modeset. It exists so a wedged pipeline cannot hang the
/// loop forever, not to police frame timing — a missed flip is recovered
/// from, never fatal, because aborting would leave a wall display dark with
/// no console to read the error on.
const FLIP_TIMEOUT_MS: u64 = 1000;

/// How often the connector is re-probed for hotplug.
pub const HOTPLUG_POLL: Duration = Duration::from_secs(2);

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
            let (w, h, refresh) = parse_mode_spec(explicit)
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

/// Split `WIDTHxHEIGHT[@HZ]` into its parts.
///
/// Strict on purpose: a typo in `display.mode` must be reported, not rounded
/// to something that happens to parse. A missing refresh means "any".
fn parse_mode_spec(s: &str) -> Option<(u16, u16, Option<u32>)> {
    let (dims, refresh) = match s.split_once('@') {
        Some((d, r)) => (d, Some(r.trim().parse::<u32>().ok()?)),
        None => (s, None),
    };
    let (a, b) = dims.split_once('x')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?, refresh))
}

/// Two modes are the same as far as the present path is concerned.
///
/// Compares what the pipeline is actually built around rather than the whole
/// timing struct: a re-probe can hand back a bit-different `drm_mode_modeinfo`
/// for the same picture, and tearing the world down for that would turn a
/// harmless EDID re-read into a visible glitch every two seconds.
fn same_mode(a: &Mode, b: &Mode) -> bool {
    a.size() == b.size() && a.vrefresh() == b.vrefresh()
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

/// Pick the primary plane wired to this CRTC.
///
/// Needs `DRM_CLIENT_CAP_UNIVERSAL_PLANES`, without which the kernel hides
/// primary planes entirely and this returns nothing useful.
fn choose_primary_plane(card: &Card, crtc: crtc::Handle) -> Result<plane::Handle> {
    let resources = card.resource_handles().context("reading DRM resources")?;
    let planes = card
        .plane_handles()
        .context("listing planes; is DRM_CLIENT_CAP_UNIVERSAL_PLANES set?")?;

    let mut seen = 0usize;
    for handle in planes {
        let Ok(info) = card.get_plane(handle) else {
            continue;
        };
        if !resources
            .filter_crtcs(info.possible_crtcs())
            .contains(&crtc)
        {
            continue;
        }
        seen += 1;
        let Ok(values) = card.get_properties(handle) else {
            continue;
        };
        for (id, value) in values.iter() {
            let Ok(info) = card.get_property(*id) else {
                continue;
            };
            if info.name().to_bytes() == b"type" {
                if *value == PlaneType::Primary as u32 as u64 {
                    return Ok(handle);
                }
                break;
            }
        }
    }
    bail!(
        "no primary plane among the {seen} plane(s) attached to crtc {crtc:?}. \
         This normally means DRM_CLIENT_CAP_UNIVERSAL_PLANES was refused — \
         check the kernel is at least 3.15 and that nothing else holds DRM master."
    )
}

/// Property handles by name, for one DRM object.
fn property_ids<T: ResourceHandle>(
    card: &Card,
    handle: T,
) -> Result<HashMap<String, property::Handle>> {
    let set = card
        .get_properties(handle)
        .context("reading object properties")?;
    let mut map = HashMap::new();
    for (id, _) in set.iter() {
        if let Ok(info) = card.get_property(*id) {
            map.insert(info.name().to_string_lossy().into_owned(), *id);
        }
    }
    Ok(map)
}

fn need_prop(
    map: &HashMap<String, property::Handle>,
    object: &str,
    name: &str,
) -> Result<property::Handle> {
    map.get(name).copied().ok_or_else(|| {
        let mut names: Vec<&str> = map.keys().map(String::as_str).collect();
        names.sort_unstable();
        anyhow::anyhow!(
            "the {object} has no {name:?} property, so atomic modesetting is not \
             available on this driver. Found: {}. Check the kernel exposes an \
             atomic driver (vc4 does) and that DRM_CLIENT_CAP_ATOMIC was accepted.",
            names.join(", ")
        )
    })
}

/// The atomic properties the present path sets.
///
/// Resolved once per modeset: a lookup by name is two ioctls per property,
/// and the flip path runs at vsync.
struct Props {
    connector_crtc_id: property::Handle,
    crtc_mode_id: property::Handle,
    crtc_active: property::Handle,
    plane_fb_id: property::Handle,
    plane_crtc_id: property::Handle,
    plane_src_x: property::Handle,
    plane_src_y: property::Handle,
    plane_src_w: property::Handle,
    plane_src_h: property::Handle,
    plane_crtc_x: property::Handle,
    plane_crtc_y: property::Handle,
    plane_crtc_w: property::Handle,
    plane_crtc_h: property::Handle,
}

impl Props {
    fn resolve(
        card: &Card,
        connector: connector::Handle,
        crtc: crtc::Handle,
        plane: plane::Handle,
    ) -> Result<Props> {
        let c = property_ids(card, connector)?;
        let r = property_ids(card, crtc)?;
        let p = property_ids(card, plane)?;
        Ok(Props {
            connector_crtc_id: need_prop(&c, "connector", "CRTC_ID")?,
            crtc_mode_id: need_prop(&r, "CRTC", "MODE_ID")?,
            crtc_active: need_prop(&r, "CRTC", "ACTIVE")?,
            plane_fb_id: need_prop(&p, "primary plane", "FB_ID")?,
            plane_crtc_id: need_prop(&p, "primary plane", "CRTC_ID")?,
            plane_src_x: need_prop(&p, "primary plane", "SRC_X")?,
            plane_src_y: need_prop(&p, "primary plane", "SRC_Y")?,
            plane_src_w: need_prop(&p, "primary plane", "SRC_W")?,
            plane_src_h: need_prop(&p, "primary plane", "SRC_H")?,
            plane_crtc_x: need_prop(&p, "primary plane", "CRTC_X")?,
            plane_crtc_y: need_prop(&p, "primary plane", "CRTC_Y")?,
            plane_crtc_w: need_prop(&p, "primary plane", "CRTC_W")?,
            plane_crtc_h: need_prop(&p, "primary plane", "CRTC_H")?,
        })
    }
}

/// DRM framebuffers, one per GBM buffer object.
///
/// A GBM surface cycles between a small fixed set of buffer objects, so
/// calling `drmModeAddFB2` every frame would allocate and free a kernel
/// object at vsync for no reason. Keyed on the buffer object's address,
/// which is stable for as long as the surface owns it; the whole cache dies
/// with the surface, so there is no stale-key window.
#[derive(Default)]
struct FbCache {
    map: HashMap<usize, framebuffer::Handle>,
}

impl FbCache {
    /// The framebuffer for `key`, creating one on first sight.
    fn get_or_create<E>(
        &mut self,
        key: usize,
        make: impl FnOnce() -> std::result::Result<framebuffer::Handle, E>,
    ) -> std::result::Result<framebuffer::Handle, E> {
        match self.map.get(&key) {
            Some(fb) => Ok(*fb),
            None => {
                let fb = make()?;
                self.map.insert(key, fb);
                Ok(fb)
            }
        }
    }

    fn len(&self) -> usize {
        self.map.len()
    }

    fn take_all(&mut self) -> Vec<framebuffer::Handle> {
        self.map.drain().map(|(_, fb)| fb).collect()
    }
}

/// Delay before the nth `drmSetMaster` retry, in milliseconds.
///
/// Doubling from 50ms to a 1s ceiling. Nothing should be holding master on a
/// device booted with `getty@tty1` masked, but plymouth or a leftover session
/// can hold it for a moment; the schedule spends about four and a half
/// seconds in total before declaring a genuine permission problem, which is
/// long enough to outlast a handover and short enough not to look like a hang.
fn master_backoff_ms(attempt: u32) -> u64 {
    (50u64 << attempt.min(4)).min(1000)
}

/// Number of `drmSetMaster` attempts before giving up.
const MASTER_ATTEMPTS: u32 = 10;

/// Become DRM master, waiting out anything that still holds it.
pub fn acquire_master(card: &Card) -> Result<()> {
    let mut last = None;
    for attempt in 0..MASTER_ATTEMPTS {
        match card.acquire_master_lock() {
            Ok(()) => {
                if attempt > 0 {
                    tracing::info!("became DRM master after {attempt} retries");
                }
                return Ok(());
            }
            // EACCES and EBUSY mean somebody else is master right now, which
            // is a race worth waiting out. Anything else is structural —
            // wrong device, no permission on the node — and waiting only
            // delays the report.
            Err(e) if !matches!(e.raw_os_error(), Some(libc::EACCES) | Some(libc::EBUSY)) => {
                return Err(e)
                    .with_context(|| format!("drmSetMaster on {}", card.path().display()));
            }
            Err(e) => {
                last = Some(e);
                if attempt + 1 < MASTER_ATTEMPTS {
                    let delay = master_backoff_ms(attempt);
                    tracing::debug!("drmSetMaster refused; retrying in {delay}ms");
                    std::thread::sleep(Duration::from_millis(delay));
                }
            }
        }
    }
    let e = last.expect("MASTER_ATTEMPTS is non-zero");
    bail!(
        "could not become DRM master on {} after {MASTER_ATTEMPTS} attempts ({e}). \
         Something else owns the display: check `systemctl status plymouth-quit` \
         and that getty@tty1 is masked, and that no compositor or second \
         lprender is running (`fuser -v {}`).",
        card.path().display(),
        card.path().display()
    )
}

/// What a hotplug poll found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hotplug {
    /// Same panel, same mode. Keep going.
    Unchanged,
    /// Nothing is attached any more.
    Disconnected,
    /// Still attached, but the mode we should be driving has changed.
    ModeChanged,
}

/// Describe what was selected, for the log and the diagnostics page.
pub fn describe(info: &connector::Info, mode: &Mode) -> String {
    let (w, h) = mode.size();
    format!("{} {}x{}@{}", connector_name(info), w, h, mode.vrefresh())
}

/// A configured scanout pipeline: connector, CRTC, primary plane, and the GBM
/// surface the GLES context renders into.
///
/// One of these exists per modeset. Hotplug drops it and builds another
/// rather than mutating it in place, so there is no half-reconfigured state
/// to reason about.
pub struct Display {
    // Teardown order matters and is enforced by `Drop` below: GL handles,
    // then the EGL context that owns them, then the GBM surface underneath
    // it, then the device. Getting this wrong leaks a context per hotplug
    // cycle, which on a flaky HDMI cable is not a theoretical amount.
    // `Option` only so `Drop` can release the renderer before the context.
    renderer: Option<Renderer>,
    egl: Egl,
    egl_display: khronos_egl::Display,
    egl_context: khronos_egl::Context,
    egl_surface: khronos_egl::Surface,
    surface: gbm::Surface<()>,
    _gbm: gbm::Device<Arc<Card>>,
    card: Arc<Card>,

    fbs: FbCache,
    props: Props,
    plane: plane::Handle,
    crtc: crtc::Handle,
    connector: connector::Handle,
    mode: Mode,
    mode_blob: u64,
    description: String,

    /// The buffer object currently being scanned out. Held so it is not
    /// handed back to GBM while the display is still reading from it.
    front: Option<gbm::BufferObject<()>>,
    front_fb: Option<framebuffer::Handle>,
    /// Whether the CRTC is currently `ACTIVE`.
    active: bool,
}

impl Display {
    /// Bring up the pipeline described by `cfg` and leave the panel showing
    /// black.
    ///
    /// The caller must already be DRM master; see [`acquire_master`].
    pub fn open(card: Arc<Card>, cfg: &Config) -> Result<Display> {
        // Both caps must be set before anything is enumerated: without them
        // the kernel hides primary planes and the atomic properties, and the
        // failure looks like missing hardware rather than a missing opt-in.
        card.set_client_capability(ClientCapability::UniversalPlanes, true)
            .context(
                "DRM_CLIENT_CAP_UNIVERSAL_PLANES was refused; this kernel cannot \
                 do universal planes and lprender cannot drive it",
            )?;
        card.set_client_capability(ClientCapability::Atomic, true)
            .context(
                "DRM_CLIENT_CAP_ATOMIC was refused; this is not an atomic driver. \
                 On a Pi that means vc4 is not bound — check dtoverlay=vc4-kms-v3d \
                 in config.txt",
            )?;

        let info = card.connector(&cfg.display.connector)?;
        let mode = choose_mode(&info, &cfg.display.mode)?;
        let crtc = choose_crtc(&card, &info)?;
        let plane = choose_primary_plane(&card, crtc)?;
        let props = Props::resolve(&card, info.handle(), crtc, plane)?;
        let description = describe(&info, &mode);
        let (width, height) = mode.size();
        let (width, height) = (width as u32, height as u32);

        let gbm = gbm::Device::new(Arc::clone(&card)).with_context(|| {
            format!(
                "gbm_create_device on {}; is Mesa's GBM backend installed \
                 (libgbm1) and does the driver support rendering?",
                card.path().display()
            )
        })?;
        let surface = gbm
            .create_surface::<()>(
                width,
                height,
                gbm::Format::Xrgb8888,
                BufferObjectFlags::SCANOUT | BufferObjectFlags::RENDERING,
            )
            .with_context(|| {
                format!(
                    "creating a {width}x{height} XRGB8888 scanout surface. \
                     On a Pi 4 this is usually CMA exhaustion — raise cma= in \
                     cmdline.txt"
                )
            })?;

        let (egl, egl_display, egl_context, egl_surface) = init_egl(&gbm, &surface)?;
        let gl = load_gl(&egl);
        let renderer = Renderer::new(gl, (width, height))?;

        // The blob outlives the commit that references it, so it is owned by
        // the Display and destroyed in Drop.
        let mode_blob = card
            .create_property_blob(&mode)
            .context("creating the mode property blob")?
            .as_blob()
            .ok_or_else(|| anyhow::anyhow!("the kernel returned a non-blob mode property"))?;

        let mut display = Display {
            renderer: Some(renderer),
            egl,
            egl_display,
            egl_context,
            egl_surface,
            surface,
            _gbm: gbm,
            card,
            fbs: FbCache::default(),
            props,
            plane,
            crtc,
            connector: info.handle(),
            mode,
            mode_blob,
            description,
            front: None,
            front_fb: None,
            active: false,
        };

        // A framebuffer has to exist before the modeset can be validated, so
        // the first frame is drawn before anything is committed.
        display.clear_to_black();
        let (bo, fb) = display.swap_and_lock()?;

        let req = display.modeset_request(fb);
        display
            .card
            .atomic_commit(
                AtomicCommitFlags::TEST_ONLY | AtomicCommitFlags::ALLOW_MODESET,
                req,
            )
            .with_context(|| {
                format!(
                    "the kernel rejected {} before it was applied. The mode is \
                     advertised but not achievable — check the pixel clock \
                     against the connector's limit, try display.mode = \"auto\", \
                     and run --probe to see what else is on offer",
                    display.description
                )
            })?;

        let req = display.modeset_request(fb);
        display
            .card
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            .with_context(|| format!("setting {}", display.description))?;

        display.active = true;
        display.front = Some(bo);
        display.front_fb = Some(fb);
        Ok(display)
    }

    pub fn renderer(&mut self) -> &mut Renderer {
        self.renderer
            .as_mut()
            .expect("the renderer is only taken during Drop")
    }

    /// The framebuffer size, which is the mode size — rotation is applied in
    /// the shaders, never by the plane's `rotation` property, which is not
    /// guaranteed to exist (DESIGN §6.3).
    pub fn size(&self) -> (u32, u32) {
        let (w, h) = self.mode.size();
        (w as u32, h as u32)
    }

    /// Connector, resolution and refresh, for the log.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// How many distinct buffer objects the surface has handed out so far.
    /// Two or three is normal; a number that keeps climbing means the fb
    /// cache is not doing its job.
    pub fn framebuffer_count(&self) -> usize {
        self.fbs.len()
    }

    /// Publish whatever the renderer just drew, and wait for it to reach the
    /// panel.
    ///
    /// Blocking on the flip is deliberate: it paces rendering to vsync
    /// without a free-running loop, and it is the only point at which the
    /// previously scanned-out buffer can safely be released.
    pub fn present(&mut self) -> Result<()> {
        if !self.active {
            bail!("present() with the CRTC inactive; unblank() first");
        }
        let (bo, fb) = self.swap_and_lock()?;

        let mut req = AtomicModeReq::new();
        req.add_property(
            self.plane,
            self.props.plane_fb_id,
            property::Value::Framebuffer(Some(fb)),
        );
        self.card
            .atomic_commit(
                AtomicCommitFlags::NONBLOCK | AtomicCommitFlags::PAGE_FLIP_EVENT,
                req,
            )
            .context("queueing a page flip")?;

        self.wait_for_flip()?;
        // Assigning here drops the outgoing buffer object, which returns it
        // to the surface. Doing it before the flip completed would hand the
        // renderer a buffer the display is still scanning out.
        self.front = Some(bo);
        self.front_fb = Some(fb);
        Ok(())
    }

    /// Turn the panel genuinely off: `CRTC.ACTIVE = 0`, not legacy DPMS.
    ///
    /// Idempotent, so the caller can call it on every idle iteration.
    pub fn blank(&mut self) -> Result<()> {
        if !self.active {
            return Ok(());
        }
        let mut req = AtomicModeReq::new();
        req.add_property(
            self.connector,
            self.props.connector_crtc_id,
            property::Value::CRTC(None),
        );
        req.add_property(self.crtc, self.props.crtc_mode_id, property::Value::Blob(0));
        req.add_property(
            self.crtc,
            self.props.crtc_active,
            property::Value::Boolean(false),
        );
        // The plane has to be detached in the same commit: a driver that
        // still has a framebuffer bound to an inactive CRTC will reject the
        // state as inconsistent.
        req.add_property(
            self.plane,
            self.props.plane_fb_id,
            property::Value::Framebuffer(None),
        );
        req.add_property(
            self.plane,
            self.props.plane_crtc_id,
            property::Value::CRTC(None),
        );
        self.card
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            .context("switching the CRTC off")?;
        self.active = false;
        tracing::debug!("panel off ({})", self.description);
        Ok(())
    }

    /// Undo [`Display::blank`], re-showing the last framebuffer. Idempotent.
    pub fn unblank(&mut self) -> Result<()> {
        if self.active {
            return Ok(());
        }
        let fb = self
            .front_fb
            .ok_or_else(|| anyhow::anyhow!("nothing has been rendered yet; cannot unblank"))?;
        let req = self.modeset_request(fb);
        self.card
            .atomic_commit(AtomicCommitFlags::ALLOW_MODESET, req)
            .context("switching the CRTC back on")?;
        self.active = true;
        tracing::debug!("panel on ({})", self.description);
        Ok(())
    }

    /// Sleep until the DRM fd has something to say or `timeout_ms` elapses.
    ///
    /// This is where an idle device spends all its time. Nothing is drawn and
    /// nothing is flipped, so a static image costs exactly one `poll` wakeup
    /// per timeout (DESIGN §6.3).
    /// Block until the DRM fd or `extra` becomes readable, or the timeout
    /// expires.
    ///
    /// `extra` is the source's wakeup descriptor (see `Source::wakeup_fd`).
    /// Carrying it here is what lets the loop sit indefinitely on a static
    /// image and still react to a new track the moment it is published,
    /// rather than waking on a timer to check.
    pub fn wait_idle(&self, timeout_ms: u64, extra: Option<RawFd>) -> Result<()> {
        let ms = timeout_ms.min(i32::MAX as u64) as i32;
        let mut fds = vec![libc::pollfd {
            fd: self.card.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        if let Some(fd) = extra {
            fds.push(libc::pollfd {
                fd,
                events: libc::POLLIN,
                revents: 0,
            });
        }

        #[allow(unsafe_code)]
        // SAFETY: `fds` is a live slice of the stated length; the descriptors
        // outlive the call.
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), fds.len() as libc::nfds_t, ms) };
        if rc < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() != std::io::ErrorKind::Interrupted {
                return Err(e).context("poll on the DRM fd");
            }
            return Ok(());
        }
        if fds[0].revents & libc::POLLIN != 0 {
            // Stale events from a flip we already accounted for. Drain them
            // so poll does not spin. The source drains its own wakeup.
            let _ = self.card.receive_events();
        }
        Ok(())
    }

    /// Re-probe the connector.
    ///
    /// A deviation from DESIGN §6.2, which specifies a libudev monitor in the
    /// epoll set: this polls `get_connector` every [`HOTPLUG_POLL`] instead,
    /// which avoids a libudev dependency for what amounts to two seconds of
    /// extra latency on a wall display. The probe is unforced — the kernel
    /// already re-detects and updates connector state from its HPD handler,
    /// which is the same event udev would have reported — so this does not
    /// put an EDID read on the wire every two seconds.
    pub fn poll_hotplug(&self, cfg: &Config) -> Hotplug {
        let Ok(info) = self.card.get_connector(self.connector, false) else {
            return Hotplug::Disconnected;
        };
        if info.state() != connector::State::Connected || info.modes().is_empty() {
            return Hotplug::Disconnected;
        }
        match choose_mode(&info, &cfg.display.mode) {
            Ok(m) if same_mode(&m, &self.mode) => Hotplug::Unchanged,
            // A mode we can no longer select is not a reason to keep driving
            // the old one; re-init and let the error be reported properly.
            _ => Hotplug::ModeChanged,
        }
    }

    /// Every property needed to light the pipeline up from cold.
    fn modeset_request(&self, fb: framebuffer::Handle) -> AtomicModeReq {
        let (w, h) = self.mode.size();
        let mut req = AtomicModeReq::new();
        req.add_property(
            self.connector,
            self.props.connector_crtc_id,
            property::Value::CRTC(Some(self.crtc)),
        );
        req.add_property(
            self.crtc,
            self.props.crtc_mode_id,
            property::Value::Blob(self.mode_blob),
        );
        req.add_property(
            self.crtc,
            self.props.crtc_active,
            property::Value::Boolean(true),
        );
        req.add_property(
            self.plane,
            self.props.plane_fb_id,
            property::Value::Framebuffer(Some(fb)),
        );
        req.add_property(
            self.plane,
            self.props.plane_crtc_id,
            property::Value::CRTC(Some(self.crtc)),
        );
        // Source rectangle is 16.16 fixed point; destination is plain pixels.
        req.add_property(
            self.plane,
            self.props.plane_src_x,
            property::Value::UnsignedRange(0),
        );
        req.add_property(
            self.plane,
            self.props.plane_src_y,
            property::Value::UnsignedRange(0),
        );
        req.add_property(
            self.plane,
            self.props.plane_src_w,
            property::Value::UnsignedRange((w as u64) << 16),
        );
        req.add_property(
            self.plane,
            self.props.plane_src_h,
            property::Value::UnsignedRange((h as u64) << 16),
        );
        req.add_property(
            self.plane,
            self.props.plane_crtc_x,
            property::Value::SignedRange(0),
        );
        req.add_property(
            self.plane,
            self.props.plane_crtc_y,
            property::Value::SignedRange(0),
        );
        req.add_property(
            self.plane,
            self.props.plane_crtc_w,
            property::Value::UnsignedRange(w as u64),
        );
        req.add_property(
            self.plane,
            self.props.plane_crtc_h,
            property::Value::UnsignedRange(h as u64),
        );
        req
    }

    /// Post the GLES frame to the GBM surface and get a scanout-ready
    /// framebuffer for it.
    fn swap_and_lock(&mut self) -> Result<(gbm::BufferObject<()>, framebuffer::Handle)> {
        self.egl
            .swap_buffers(self.egl_display, self.egl_surface)
            .context("eglSwapBuffers on the GBM surface")?;

        #[allow(unsafe_code)]
        let bo = unsafe {
            // SAFETY: called exactly once after each eglSwapBuffers, which is
            // the contract gbm_surface_lock_front_buffer documents. This is
            // the only call site, and the swap immediately precedes it.
            self.surface.lock_front_buffer()
        }
        .map_err(|e| anyhow::anyhow!("gbm_surface_lock_front_buffer: {e}"))?;

        let card = Arc::clone(&self.card);
        let key = bo.as_raw() as usize;
        let fb = self.fbs.get_or_create(key, || create_fb(&card, &bo))?;
        Ok((bo, fb))
    }

    fn wait_for_flip(&self) -> Result<()> {
        let deadline = Instant::now() + Duration::from_millis(FLIP_TIMEOUT_MS);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                // Recovered, not fatal: drop the pending flip and carry on.
                // The next frame re-submits. A device on a wall with no
                // console must not exit because one flip was late.
                tracing::warn!(
                    "no page-flip event within {FLIP_TIMEOUT_MS}ms on {}; \
                     continuing. If this repeats, check dmesg for vc4 or v3d \
                     errors",
                    self.description
                );
                return Ok(());
            }
            if !poll_readable(self.card.as_fd(), left.as_millis() as i32)? {
                continue;
            }
            for event in self.card.receive_events().context("reading DRM events")? {
                if matches!(event, drm::control::Event::PageFlip(_)) {
                    return Ok(());
                }
            }
        }
    }

    #[allow(unsafe_code)]
    fn clear_to_black(&mut self) {
        unsafe {
            // SAFETY: the context created in `open` is current and stays
            // current for this Display's lifetime; framebuffer zero is the
            // GBM surface's back buffer.
            let gl = self.renderer().gl();
            gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);
        }
    }
}

impl Drop for Display {
    fn drop(&mut self) {
        // Release the scanned-out buffer before its framebuffer, and the
        // framebuffers before the surface that owns their buffer objects.
        self.front = None;
        self.front_fb = None;
        for fb in self.fbs.take_all() {
            let _ = self.card.destroy_framebuffer(fb);
        }
        let _ = self.card.destroy_property_blob(self.mode_blob);

        // Textures and programs go while their context is still current, and
        // the context is released before the surface it draws into is
        // destroyed. A hotplug cycle runs all of this and then builds it
        // again, so an ordering mistake here compounds.
        self.renderer = None;
        let _ = self.egl.make_current(self.egl_display, None, None, None);
        let _ = self.egl.destroy_surface(self.egl_display, self.egl_surface);
        let _ = self.egl.destroy_context(self.egl_display, self.egl_context);
        let _ = self.egl.terminate(self.egl_display);
    }
}

/// Wrap a GBM buffer object in a DRM framebuffer.
fn create_fb(card: &Card, bo: &gbm::BufferObject<()>) -> Result<framebuffer::Handle> {
    let modifier = bo.modifier();
    // `add_planar_framebuffer` asserts that the MODIFIERS flag and the
    // presence of a modifier agree, so the flag follows the buffer rather
    // than being fixed.
    let flags = if modifier == gbm::Modifier::Invalid {
        FbCmd2Flags::empty()
    } else {
        FbCmd2Flags::MODIFIERS
    };
    card.add_planar_framebuffer(bo, flags).with_context(|| {
        format!(
            "drmModeAddFB2 for a {}x{} {:?} buffer with modifier {modifier:?}. \
             A kernel without addfb2-with-modifiers support cannot scan out a \
             tiled buffer; check dmesg",
            bo.width(),
            bo.height(),
            bo.format(),
        )
    })
}

/// Wait for the fd to become readable. `Ok(false)` means the timeout expired.
#[allow(unsafe_code)]
fn poll_readable(fd: BorrowedFd<'_>, timeout_ms: i32) -> Result<bool> {
    loop {
        let mut pfd = libc::pollfd {
            fd: fd.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pfd` is a single initialised pollfd owned by this frame,
        // and the length passed matches. The fd is borrowed for the call.
        let n = unsafe { libc::poll(&mut pfd, 1, timeout_ms) };
        if n < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e).context("poll on the DRM fd");
        }
        return Ok(n > 0);
    }
}

/// EGL on the GBM platform: display, GLES 3.0 context, window surface.
fn init_egl(
    gbm: &gbm::Device<Arc<Card>>,
    surface: &gbm::Surface<()>,
) -> Result<(
    Egl,
    khronos_egl::Display,
    khronos_egl::Context,
    khronos_egl::Surface,
)> {
    let egl = Egl::new(khronos_egl::Static);

    #[allow(unsafe_code)]
    let display = unsafe {
        // SAFETY: EGL_PLATFORM_GBM_KHR requires a `struct gbm_device *`,
        // which is exactly what `as_raw` yields, and the device outlives the
        // display because both are owned by the same `Display`.
        egl.get_platform_display(
            PLATFORM_GBM,
            gbm.as_raw() as *mut c_void,
            &[khronos_egl::ATTRIB_NONE],
        )
    }
    .context("eglGetPlatformDisplay(EGL_PLATFORM_GBM_KHR); is Mesa's EGL installed?")?;

    let (major, minor) = egl
        .initialize(display)
        .context("eglInitialize on the GBM platform")?;
    tracing::debug!("EGL {major}.{minor} on GBM");
    egl.bind_api(khronos_egl::OPENGL_ES_API)
        .context("binding the OpenGL ES API")?;

    let attribs = [
        khronos_egl::SURFACE_TYPE,
        khronos_egl::WINDOW_BIT,
        khronos_egl::RENDERABLE_TYPE,
        khronos_egl::OPENGL_ES3_BIT,
        khronos_egl::RED_SIZE,
        8,
        khronos_egl::GREEN_SIZE,
        8,
        khronos_egl::BLUE_SIZE,
        8,
        khronos_egl::NONE,
    ];
    let mut configs = Vec::with_capacity(32);
    egl.choose_config(display, &attribs, &mut configs)
        .context("choosing an EGL config")?;

    // The config's native visual must be the surface's format or the driver
    // silently renders into something the display cannot scan out. Matching
    // it explicitly is the difference between a picture and a black panel.
    // XRGB8888 is what the GBM surface was created with, so prefer it. Fall
    // back to ARGB8888: the alpha channel is ignored on scanout, and a
    // working picture beats refusing to start because a driver only
    // advertises the alpha variant.
    let preferred = [
        (gbm::Format::Xrgb8888 as u32, "XRGB8888"),
        (gbm::Format::Argb8888 as u32, "ARGB8888"),
    ];
    let mut chosen = None;
    for (want, name) in preferred {
        if let Some(c) = configs.iter().copied().find(|c| {
            egl.get_config_attrib(display, *c, khronos_egl::NATIVE_VISUAL_ID)
                .is_ok_and(|v| v as u32 == want)
        }) {
            if name != "XRGB8888" {
                tracing::warn!("no XRGB8888 EGL config; falling back to {name}");
            }
            chosen = Some(c);
            break;
        }
    }
    let config = chosen.ok_or_else(|| {
        anyhow::anyhow!(
            "no GLES3 EGL config with an XRGB8888 or ARGB8888 native visual \
             among {} candidates. The GBM backend and the EGL driver disagree \
             about formats — check that libgbm and libEGL come from the same \
             Mesa",
            configs.len()
        )
    })?;

    let ctx_attribs = [khronos_egl::CONTEXT_MAJOR_VERSION, 3, khronos_egl::NONE];
    let context = egl
        .create_context(display, config, None, &ctx_attribs)
        .context("creating a GLES3 context")?;

    #[allow(unsafe_code)]
    let egl_surface = unsafe {
        // SAFETY: `surface` is a `struct gbm_surface *` from the same device
        // the display was created from, which is what this platform's native
        // window type is, and it outlives the EGL surface.
        // eglCreateWindowSurface rather than the EGL 1.5 platform entry point
        // because Mesa has accepted the former for GBM since long before 1.5.
        egl.create_window_surface(display, config, surface.as_raw() as *mut c_void, None)
    }
    .context("creating an EGL window surface on the GBM surface")?;

    egl.make_current(display, Some(egl_surface), Some(egl_surface), Some(context))
        .context("eglMakeCurrent")?;
    // Vsync. Combined with waiting for each page-flip event this keeps the
    // renderer exactly one frame ahead and never spinning.
    egl.swap_interval(display, 1)
        .context("eglSwapInterval(1)")?;

    Ok((egl, display, context, egl_surface))
}

fn load_gl(egl: &Egl) -> glow::Context {
    #[allow(unsafe_code)]
    unsafe {
        // SAFETY: the loader returns valid function pointers for the context
        // just made current, which stays current for the Display's lifetime.
        glow::Context::from_loader_function(|name| {
            egl.get_proc_address(name)
                .map_or(std::ptr::null(), |p| p as *const _)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mode selection, framebuffer caching and the master backoff are pure
    // enough to test without a device. Everything else here needs real
    // hardware, and is deliberately not faked: a test that pretends to page
    // flip would only prove the fake works.

    #[test]
    fn an_explicit_mode_string_is_parsed_strictly() {
        // Malformed strings must be rejected with a message naming the
        // expected form rather than silently falling back to a wrong mode.
        for bad in [
            "1920",
            "1920*1920",
            "widescreen",
            "1920x",
            "x1080",
            "1920x1080@",
            "1920x1080@60Hz",
        ] {
            assert!(parse_mode_spec(bad).is_none(), "{bad:?} should not parse");
        }
        assert_eq!(parse_mode_spec("1920x1920"), Some((1920, 1920, None)));
        assert_eq!(parse_mode_spec(" 720 x 720 "), Some((720, 720, None)));
        assert_eq!(
            parse_mode_spec("1920x1080@60"),
            Some((1920, 1080, Some(60)))
        );
    }

    #[test]
    fn a_framebuffer_is_created_once_per_buffer_object() {
        // The point of the cache: a GBM surface cycles between a couple of
        // buffer objects forever, and drmModeAddFB2 must not run at vsync.
        let mut cache = FbCache::default();
        let mut created = 0;
        let mut fb_for = |cache: &mut FbCache, key: usize| {
            cache
                .get_or_create(key, || {
                    created += 1;
                    Ok::<_, std::convert::Infallible>(
                        drm::control::from_u32::<framebuffer::Handle>(key as u32 + 1).unwrap(),
                    )
                })
                .unwrap()
        };

        let a = fb_for(&mut cache, 10);
        let b = fb_for(&mut cache, 20);
        for _ in 0..100 {
            assert_eq!(fb_for(&mut cache, 10), a);
            assert_eq!(fb_for(&mut cache, 20), b);
        }
        assert_ne!(a, b);
        assert_eq!(created, 2);
        assert_eq!(cache.len(), 2);

        // Teardown must hand every framebuffer back exactly once, or the
        // handles leak across a hotplug re-init.
        let mut taken = cache.take_all();
        taken.sort_by_key(|h| u32::from(*h));
        assert_eq!(taken, vec![a, b]);
        assert_eq!(cache.len(), 0);
        assert!(cache.take_all().is_empty());
    }

    #[test]
    fn a_failed_framebuffer_is_not_cached() {
        let mut cache = FbCache::default();
        let err = cache.get_or_create(1, || Err::<framebuffer::Handle, _>("nope"));
        assert!(err.is_err());
        assert_eq!(cache.len(), 0, "a failed creation must not be remembered");
    }

    #[test]
    fn the_master_backoff_doubles_to_a_ceiling() {
        let schedule: Vec<u64> = (0..MASTER_ATTEMPTS).map(master_backoff_ms).collect();
        assert_eq!(&schedule[..5], &[50, 100, 200, 400, 800]);
        assert!(schedule.iter().all(|d| *d <= 1000));
        // Long enough to outlast a plymouth handover, short enough that a
        // real permission failure is reported rather than looking like a hang.
        let total: u64 = schedule.iter().sum();
        assert!((3000..8000).contains(&total), "total wait {total}ms");
    }

    #[test]
    fn opening_a_missing_card_is_an_error_not_a_panic() {
        assert!(Card::open_path(Path::new("/dev/dri/definitely-not-a-card")).is_err());
    }
}
