//! An SDL2 window: the development backend.
//!
//! Same GL code, same shaders, same scene state as the device — only the
//! source of the default framebuffer differs. This is what makes "everything
//! runs on a laptop" (DESIGN §1) true for the renderer.

use anyhow::{Context, Result};

use crate::gl::Renderer;

pub struct Window {
    // Field order is the drop order, and the GL context must outlive the
    // renderer's handles.
    renderer: Renderer,
    _gl_context: sdl2::video::GLContext,
    window: sdl2::video::Window,
    events: sdl2::EventPump,
    size: (u32, u32),
}

impl Window {
    pub fn new(width: u32, height: u32, title: &str) -> Result<Window> {
        let sdl = sdl2::init().map_err(|e| anyhow::anyhow!("SDL init: {e}"))?;
        let video = sdl.video().map_err(|e| anyhow::anyhow!("SDL video: {e}"))?;

        let attr = video.gl_attr();
        attr.set_context_profile(sdl2::video::GLProfile::GLES);
        attr.set_context_version(3, 0);
        attr.set_red_size(8);
        attr.set_green_size(8);
        attr.set_blue_size(8);
        attr.set_alpha_size(8);

        let window = video
            .window(title, width, height)
            .opengl()
            .position_centered()
            .build()
            .context("creating the SDL window")?;

        let gl_context = window
            .gl_create_context()
            .map_err(|e| anyhow::anyhow!("creating a GLES3 context: {e}"))?;
        window
            .gl_make_current(&gl_context)
            .map_err(|e| anyhow::anyhow!("making the context current: {e}"))?;
        // Match the device: vsync-paced, never a free-running loop.
        let _ = video.gl_set_swap_interval(sdl2::video::SwapInterval::VSync);

        #[allow(unsafe_code)]
        let gl = unsafe {
            // SAFETY: the loader returns valid pointers for the context just
            // made current, which stays current for this window's lifetime.
            glow::Context::from_loader_function(|name| video.gl_get_proc_address(name) as *const _)
        };
        let renderer = Renderer::new(gl, (width, height))?;
        let events = sdl.event_pump().map_err(|e| anyhow::anyhow!(e))?;

        Ok(Window {
            renderer,
            _gl_context: gl_context,
            window,
            events,
            size: (width, height),
        })
    }

    pub fn renderer(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Drain events. Returns false when the window should close.
    ///
    /// Resizing is handled the same way a DRM hotplug will be: re-read the
    /// size and tell the renderer, rather than assuming it never changes.
    pub fn pump(&mut self) -> bool {
        use sdl2::event::{Event, WindowEvent};
        use sdl2::keyboard::Keycode;
        for event in self.events.poll_iter() {
            match event {
                Event::Quit { .. }
                | Event::KeyDown {
                    keycode: Some(Keycode::Escape | Keycode::Q),
                    ..
                } => return false,
                Event::Window {
                    win_event: WindowEvent::Resized(w, h) | WindowEvent::SizeChanged(w, h),
                    ..
                } => {
                    let size = (w.max(1) as u32, h.max(1) as u32);
                    self.size = size;
                    self.renderer.set_panel(size);
                }
                _ => {}
            }
        }
        true
    }

    pub fn present(&self) {
        self.window.gl_swap_window();
    }
}
