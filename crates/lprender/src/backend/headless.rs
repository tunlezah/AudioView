//! Surfaceless EGL, rendering into an FBO.
//!
//! Always compiled, on every platform. This is how the shaders are tested:
//! CI runs it against llvmpipe, and the same code runs on the Pi against v3d,
//! so a golden-image regression is caught without a panel attached
//! (DESIGN §6.4).

use anyhow::{bail, Context, Result};
use glow::HasContext;

use crate::gl::Renderer;

type Egl = khronos_egl::Instance<khronos_egl::Static>;

pub struct Headless {
    egl: Egl,
    display: khronos_egl::Display,
    context: khronos_egl::Context,
    renderer: Renderer,
    fbo: glow::Framebuffer,
    color: glow::Texture,
    size: (u32, u32),
}

impl Headless {
    pub fn new(width: u32, height: u32) -> Result<Headless> {
        let egl = Egl::new(khronos_egl::Static);

        // The surfaceless platform needs no DRM node and no window system,
        // which is exactly what makes this runnable in a container.
        let display = unsafe_get_display(&egl)?;
        let (major, minor) = egl
            .initialize(display)
            .context("eglInitialize on the surfaceless platform")?;
        tracing::debug!("EGL {major}.{minor} headless");

        egl.bind_api(khronos_egl::OPENGL_ES_API)
            .context("binding the OpenGL ES API")?;

        let attribs = [
            khronos_egl::SURFACE_TYPE,
            khronos_egl::PBUFFER_BIT,
            khronos_egl::RENDERABLE_TYPE,
            khronos_egl::OPENGL_ES3_BIT,
            khronos_egl::RED_SIZE,
            8,
            khronos_egl::GREEN_SIZE,
            8,
            khronos_egl::BLUE_SIZE,
            8,
            khronos_egl::ALPHA_SIZE,
            8,
            khronos_egl::NONE,
        ];
        let config = egl
            .choose_first_config(display, &attribs)
            .context("choosing an EGL config")?
            .ok_or_else(|| anyhow::anyhow!("no EGL config with GLES3 support"))?;

        let ctx_attribs = [khronos_egl::CONTEXT_MAJOR_VERSION, 3, khronos_egl::NONE];
        let context = egl
            .create_context(display, config, None, &ctx_attribs)
            .context("creating a GLES3 context")?;
        egl.make_current(display, None, None, Some(context))
            .context("eglMakeCurrent")?;

        let gl = load_gl(&egl);
        let mut renderer = Renderer::new(gl, (width, height))?;
        let (fbo, color) = make_target(renderer.gl(), width, height)?;
        renderer.set_target(Some(fbo));

        Ok(Headless {
            egl,
            display,
            context,
            renderer,
            fbo,
            color,
            size: (width, height),
        })
    }

    pub fn renderer(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    /// Bind the offscreen target, so subsequent draws land in it.
    pub fn bind(&self) {
        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: a current context; the FBO is owned by self.
            self.renderer
                .gl()
                .bind_framebuffer(glow::FRAMEBUFFER, Some(self.fbo));
        }
    }

    /// Read the rendered image back as RGBA8, top row first.
    pub fn read(&self) -> Vec<u8> {
        self.bind();
        self.renderer.read_pixels(self.size.0, self.size.1)
    }
}

impl Drop for Headless {
    fn drop(&mut self) {
        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: handles created in `new` on this context.
            self.renderer.gl().delete_framebuffer(self.fbo);
            self.renderer.gl().delete_texture(self.color);
        }
        let _ = self.egl.destroy_context(self.display, self.context);
        let _ = self.egl.terminate(self.display);
    }
}

/// `EGL_PLATFORM_SURFACELESS_MESA`.
const PLATFORM_SURFACELESS: khronos_egl::Enum = 0x31DD;

fn unsafe_get_display(egl: &Egl) -> Result<khronos_egl::Display> {
    // Prefer the explicit surfaceless platform; fall back to the default
    // display, which some drivers still accept.
    #[allow(unsafe_code)]
    let display = unsafe {
        // SAFETY: a null native display is what the surfaceless platform
        // expects, and the returned handle is checked before use.
        egl.get_platform_display(
            PLATFORM_SURFACELESS,
            khronos_egl::DEFAULT_DISPLAY,
            &[khronos_egl::ATTRIB_NONE],
        )
    };
    match display {
        Ok(d) => Ok(d),
        Err(e) => {
            #[allow(unsafe_code)]
            let fallback = unsafe {
                // SAFETY: as above.
                egl.get_display(khronos_egl::DEFAULT_DISPLAY)
            };
            fallback.ok_or_else(|| anyhow::anyhow!("no EGL display available ({e})"))
        }
    }
}

fn load_gl(egl: &Egl) -> glow::Context {
    #[allow(unsafe_code)]
    unsafe {
        // SAFETY: the loader returns valid function pointers for the current
        // context, which stays current for the renderer's lifetime.
        glow::Context::from_loader_function(|name| {
            egl.get_proc_address(name)
                .map_or(std::ptr::null(), |p| p as *const _)
        })
    }
}

fn make_target(
    gl: &glow::Context,
    width: u32,
    height: u32,
) -> Result<(glow::Framebuffer, glow::Texture)> {
    #[allow(unsafe_code)]
    unsafe {
        // SAFETY: a current context; completeness is checked before returning.
        let color = gl.create_texture().map_err(|e| anyhow::anyhow!(e))?;
        gl.bind_texture(glow::TEXTURE_2D, Some(color));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            width as i32,
            height as i32,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(None),
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MIN_FILTER,
            glow::LINEAR as i32,
        );
        gl.tex_parameter_i32(
            glow::TEXTURE_2D,
            glow::TEXTURE_MAG_FILTER,
            glow::LINEAR as i32,
        );

        let fbo = gl.create_framebuffer().map_err(|e| anyhow::anyhow!(e))?;
        gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
        gl.framebuffer_texture_2d(
            glow::FRAMEBUFFER,
            glow::COLOR_ATTACHMENT0,
            glow::TEXTURE_2D,
            Some(color),
            0,
        );
        let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
        if status != glow::FRAMEBUFFER_COMPLETE {
            bail!("offscreen framebuffer incomplete: 0x{status:x}");
        }
        Ok((fbo, color))
    }
}
