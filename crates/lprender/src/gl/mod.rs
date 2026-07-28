//! The GLES 3.0 renderer, shared by every backend.
//!
//! Backends differ only in where the default framebuffer comes from: a GBM
//! surface on the Pi, an SDL2 window on a laptop, an FBO in tests. The
//! drawing below is identical in all three, which is what makes the headless
//! golden images meaningful.

pub mod shaders;

use anyhow::{bail, Result};
use glow::HasContext;
use lpframe_config::Background;

use crate::color::Palette;
use crate::geometry::Layout;
use crate::scene::Frame;

/// An image resident on the GPU.
#[derive(Debug)]
pub struct Texture {
    pub handle: glow::Texture,
    pub width: u32,
    pub height: u32,
}

pub struct Renderer {
    gl: glow::Context,
    quad_vao: glow::VertexArray,
    quad_vbo: glow::Buffer,

    artwork: Program,
    background: Program,
    kawase: Program,
    blit: Program,

    /// Ping-pong targets for the blur chain, rebuilt when the panel resizes.
    blur: Option<BlurChain>,
    blur_source: Option<glow::Texture>,

    /// A 1×1 black texture, bound wherever a sampler must be valid but is
    /// unused. Sampling an unbound texture unit is undefined behaviour in
    /// GLES, and on some drivers it renders as noise rather than nothing.
    dummy: glow::Texture,

    /// Where `draw` sends its output. The blur chain binds its own targets,
    /// so it must restore *this* rather than framebuffer zero — on a
    /// surfaceless context, framebuffer zero is nowhere and the frame is
    /// silently discarded.
    target: Option<glow::Framebuffer>,

    panel: (u32, u32),
}

struct Program {
    handle: glow::Program,
}

struct BlurChain {
    /// Half-resolution ping-pong pair.
    fbos: [(glow::Framebuffer, glow::Texture); 2],
    size: (u32, u32),
}

/// Everything needed to draw one frame.
pub struct DrawCall<'a> {
    pub layout: Layout,
    pub frame: Frame,
    pub current: Option<&'a Texture>,
    pub previous: Option<&'a Texture>,
    pub background: Background,
    pub background_dim: f32,
    pub palette: Palette,
    /// Extra dimming for ambient mode, multiplied into the global fade.
    pub ambient_dim: Option<f32>,
}

impl Renderer {
    /// # Safety contract
    ///
    /// `gl` must be a current GLES 3.0 context, and must remain current for
    /// the lifetime of this renderer.
    pub fn new(gl: glow::Context, panel: (u32, u32)) -> Result<Renderer> {
        let artwork = Program::new(&gl, shaders::VERTEX, shaders::ARTWORK_FRAGMENT)?;
        let background = Program::new(&gl, shaders::VERTEX, shaders::BACKGROUND_FRAGMENT)?;
        let kawase = Program::new(&gl, shaders::VERTEX, shaders::KAWASE_FRAGMENT)?;
        let blit = Program::new(&gl, shaders::VERTEX, shaders::BLIT_FRAGMENT)?;
        let (quad_vao, quad_vbo) = make_quad(&gl)?;
        let dummy = make_dummy_texture(&gl)?;

        Ok(Renderer {
            gl,
            quad_vao,
            quad_vbo,
            artwork,
            background,
            kawase,
            blit,
            blur: None,
            blur_source: None,
            dummy,
            target: None,
            panel,
        })
    }

    pub fn gl(&self) -> &glow::Context {
        &self.gl
    }

    pub fn panel(&self) -> (u32, u32) {
        self.panel
    }

    /// Set the framebuffer `draw` renders into. `None` is the default one.
    pub fn set_target(&mut self, target: Option<glow::Framebuffer>) {
        self.target = target;
    }

    pub fn set_panel(&mut self, panel: (u32, u32)) {
        if self.panel != panel {
            self.panel = panel;
            self.drop_blur();
        }
    }

    /// Upload an image. RGBA8, tightly packed.
    pub fn upload(&self, width: u32, height: u32, rgba: &[u8]) -> Result<Texture> {
        let expected = width as usize * height as usize * 4;
        if rgba.len() != expected {
            bail!(
                "expected {expected} bytes for {width}x{height}, got {}",
                rgba.len()
            );
        }
        let gl = &self.gl;
        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: a current context; all handles are created here and the
            // pixel slice is verified to match the declared dimensions above.
            let handle = gl.create_texture().map_err(|e| anyhow::anyhow!(e))?;
            gl.bind_texture(glow::TEXTURE_2D, Some(handle));
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA8 as i32,
                width as i32,
                height as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(rgba)),
            );
            // Mipmaps matter: minifying 3000² down to 1920² without them
            // aliases visibly on fine label text.
            gl.generate_mipmap(glow::TEXTURE_2D);
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MIN_FILTER,
                glow::LINEAR_MIPMAP_LINEAR as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_MAG_FILTER,
                glow::LINEAR as i32,
            );
            // Clamp, so a Ken Burns zoom or a rounding error at the edge
            // samples the edge pixel rather than wrapping to the far side.
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_S,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.tex_parameter_i32(
                glow::TEXTURE_2D,
                glow::TEXTURE_WRAP_T,
                glow::CLAMP_TO_EDGE as i32,
            );
            gl.bind_texture(glow::TEXTURE_2D, None);
            Ok(Texture {
                handle,
                width,
                height,
            })
        }
    }

    pub fn delete_texture(&self, t: Texture) {
        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: the handle came from `upload` on this context.
            self.gl.delete_texture(t.handle);
        }
    }

    /// Draw a frame into the currently bound framebuffer.
    pub fn draw(&mut self, call: &DrawCall<'_>) {
        let (pw, ph) = self.panel;
        let fade = call.frame.global_fade * call.ambient_dim.unwrap_or(1.0);

        // The blurred background is expensive and only depends on the
        // artwork, so it is rebuilt on image change rather than per frame —
        // which is what keeps a static image at zero ongoing cost.
        if call.background == Background::Blur && call.layout.background_visible {
            if let Some(tex) = call.current {
                self.ensure_blur(tex);
            }
        }

        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: a current context; every handle below is owned by self.
            let gl = &self.gl;
            // Bind after the blur chain, which uses framebuffers of its own.
            gl.bind_framebuffer(glow::FRAMEBUFFER, self.target);
            gl.viewport(0, 0, pw as i32, ph as i32);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::BLEND);
            gl.clear_color(0.0, 0.0, 0.0, 1.0);
            gl.clear(glow::COLOR_BUFFER_BIT);

            if call.layout.background_visible {
                self.draw_background(call, fade);
            }
            if call.current.is_some() || call.previous.is_some() {
                self.draw_artwork(call, fade);
            }
        }
    }

    #[allow(unsafe_code)]
    unsafe fn draw_background(&self, call: &DrawCall<'_>, fade: f32) {
        // SAFETY: called from `draw` with a current context.
        let gl = &self.gl;
        let p = &self.background;
        gl.use_program(Some(p.handle));

        let mode = match call.background {
            Background::Black => 0,
            Background::Blur => {
                if self.blur.is_some() {
                    1
                } else {
                    // No blur built yet (no artwork): black beats garbage.
                    0
                }
            }
            Background::Dominant => 2,
            Background::Gradient => 3,
        };

        p.set_rect(gl, &crate::geometry::Rect::FULL);
        p.set_mat2(gl, "uRotation", &call.layout.rotation_matrix());
        p.set_vec2(gl, "uUvScale", [1.0, 1.0]);
        p.set_vec2(gl, "uUvOffset", [0.0, 0.0]);
        p.set_f32(gl, "uZoom", 1.0);
        p.set_f32(gl, "uFlipV", 0.0);
        p.set_i32(gl, "uMode", mode);
        p.set_vec3(gl, "uColorA", call.palette.primary);
        p.set_vec3(gl, "uColorB", call.palette.secondary);
        p.set_f32(gl, "uDim", call.background_dim.clamp(0.0, 1.0));
        p.set_f32(gl, "uGlobalFade", fade);

        gl.active_texture(glow::TEXTURE0);
        let blur_tex = self
            .blur
            .as_ref()
            .map(|b| b.fbos[0].1)
            .unwrap_or(self.dummy);
        gl.bind_texture(glow::TEXTURE_2D, Some(blur_tex));
        p.set_i32(gl, "uBlur", 0);

        self.draw_quad();
    }

    #[allow(unsafe_code)]
    unsafe fn draw_artwork(&self, call: &DrawCall<'_>, fade: f32) {
        // SAFETY: called from `draw` with a current context.
        let gl = &self.gl;
        let p = &self.artwork;
        gl.use_program(Some(p.handle));

        // With no incoming image (artwork cleared) the roles invert: fade the
        // outgoing one out rather than drawing nothing abruptly.
        let (next, prev, mix) = match (call.current, call.previous) {
            (Some(c), Some(pv)) => (c, Some(pv), call.frame.mix),
            (Some(c), None) => (c, None, 1.0),
            (None, Some(pv)) => (pv, None, 1.0),
            (None, None) => return,
        };

        p.set_rect(gl, &call.layout.art);
        p.set_mat2(gl, "uRotation", &call.layout.rotation_matrix());
        p.set_vec2(gl, "uUvScale", call.layout.uv_scale);
        p.set_vec2(gl, "uUvOffset", call.layout.uv_offset);
        p.set_f32(gl, "uZoom", call.frame.zoom);
        p.set_f32(gl, "uFlipV", 1.0);
        p.set_f32(gl, "uMix", mix);
        p.set_f32(
            gl,
            "uGlobalFade",
            if call.current.is_none() {
                // Cleared artwork fades out with the crossfade ramp.
                fade * (1.0 - call.frame.mix)
            } else {
                fade
            },
        );
        p.set_f32(gl, "uHasPrev", if prev.is_some() { 1.0 } else { 0.0 });

        gl.active_texture(glow::TEXTURE0);
        gl.bind_texture(
            glow::TEXTURE_2D,
            Some(prev.map(|t| t.handle).unwrap_or(self.dummy)),
        );
        p.set_i32(gl, "uPrev", 0);
        gl.active_texture(glow::TEXTURE1);
        gl.bind_texture(glow::TEXTURE_2D, Some(next.handle));
        p.set_i32(gl, "uNext", 1);

        self.draw_quad();
    }

    #[allow(unsafe_code)]
    unsafe fn draw_quad(&self) {
        // SAFETY: the VAO is created in `new` and owned by self.
        let gl = &self.gl;
        gl.bind_vertex_array(Some(self.quad_vao));
        gl.draw_arrays(glow::TRIANGLE_STRIP, 0, 4);
        gl.bind_vertex_array(None);
    }

    /// Build or refresh the blurred background for `tex`.
    fn ensure_blur(&mut self, tex: &Texture) {
        if self.blur_source == Some(tex.handle) && self.blur.is_some() {
            return;
        }
        // Quarter resolution is plenty for something this heavily blurred,
        // and keeps the chain cheap on a VideoCore.
        let size = ((self.panel.0 / 4).max(16), (self.panel.1 / 4).max(16));
        if self.blur.as_ref().map(|b| b.size) != Some(size) {
            self.drop_blur();
            match self.make_blur_chain(size) {
                Ok(chain) => self.blur = Some(chain),
                Err(e) => {
                    tracing::warn!("blur chain unavailable, falling back to black: {e}");
                    return;
                }
            }
        }
        self.render_blur(tex);
        self.blur_source = Some(tex.handle);
    }

    fn make_blur_chain(&self, size: (u32, u32)) -> Result<BlurChain> {
        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: a current context; handles are checked before use.
            let gl = &self.gl;
            let mut fbos = Vec::with_capacity(2);
            for _ in 0..2 {
                let tex = gl.create_texture().map_err(|e| anyhow::anyhow!(e))?;
                gl.bind_texture(glow::TEXTURE_2D, Some(tex));
                gl.tex_image_2d(
                    glow::TEXTURE_2D,
                    0,
                    glow::RGBA8 as i32,
                    size.0 as i32,
                    size.1 as i32,
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
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_S,
                    glow::CLAMP_TO_EDGE as i32,
                );
                gl.tex_parameter_i32(
                    glow::TEXTURE_2D,
                    glow::TEXTURE_WRAP_T,
                    glow::CLAMP_TO_EDGE as i32,
                );

                let fbo = gl.create_framebuffer().map_err(|e| anyhow::anyhow!(e))?;
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
                gl.framebuffer_texture_2d(
                    glow::FRAMEBUFFER,
                    glow::COLOR_ATTACHMENT0,
                    glow::TEXTURE_2D,
                    Some(tex),
                    0,
                );
                let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
                if status != glow::FRAMEBUFFER_COMPLETE {
                    bail!("blur framebuffer incomplete: 0x{status:x}");
                }
                fbos.push((fbo, tex));
            }
            gl.bind_framebuffer(glow::FRAMEBUFFER, self.target);
            Ok(BlurChain {
                fbos: [fbos[0], fbos[1]],
                size,
            })
        }
    }

    fn render_blur(&self, tex: &Texture) {
        let Some(chain) = &self.blur else { return };
        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: a current context; chain handles are owned by self.
            let gl = &self.gl;
            gl.viewport(0, 0, chain.size.0 as i32, chain.size.1 as i32);

            // Downsample the artwork into the first target...
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(chain.fbos[0].0));
            gl.use_program(Some(self.blit.handle));
            self.blit.set_rect(gl, &crate::geometry::Rect::FULL);
            self.blit.set_mat2(gl, "uRotation", &[1.0, 0.0, 0.0, 1.0]);
            self.blit.set_vec2(gl, "uUvScale", [1.0, 1.0]);
            self.blit.set_vec2(gl, "uUvOffset", [0.0, 0.0]);
            self.blit.set_f32(gl, "uZoom", 1.0);
            self.blit.set_f32(gl, "uFlipV", 1.0);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, Some(tex.handle));
            self.blit.set_i32(gl, "uSrc", 0);
            self.draw_quad();

            // ...then ping-pong the Kawase step, widening each pass.
            gl.use_program(Some(self.kawase.handle));
            self.kawase.set_rect(gl, &crate::geometry::Rect::FULL);
            self.kawase.set_mat2(gl, "uRotation", &[1.0, 0.0, 0.0, 1.0]);
            self.kawase.set_vec2(gl, "uUvScale", [1.0, 1.0]);
            self.kawase.set_vec2(gl, "uUvOffset", [0.0, 0.0]);
            self.kawase.set_f32(gl, "uZoom", 1.0);
            self.kawase.set_f32(gl, "uFlipV", 0.0);
            self.kawase.set_vec2(
                gl,
                "uTexel",
                [1.0 / chain.size.0 as f32, 1.0 / chain.size.1 as f32],
            );

            let mut src = 0usize;
            for pass in 0..4 {
                let dst = 1 - src;
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(chain.fbos[dst].0));
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(chain.fbos[src].1));
                self.kawase.set_i32(gl, "uSrc", 0);
                self.kawase.set_f32(gl, "uRadius", 1.0 + pass as f32 * 2.0);
                self.draw_quad();
                src = dst;
            }

            // Leave the result in slot 0, which is what the background pass
            // samples.
            if src != 0 {
                gl.bind_framebuffer(glow::FRAMEBUFFER, Some(chain.fbos[0].0));
                gl.use_program(Some(self.blit.handle));
                self.blit.set_f32(gl, "uFlipV", 0.0);
                gl.active_texture(glow::TEXTURE0);
                gl.bind_texture(glow::TEXTURE_2D, Some(chain.fbos[1].1));
                self.blit.set_i32(gl, "uSrc", 0);
                self.draw_quad();
            }

            gl.bind_framebuffer(glow::FRAMEBUFFER, self.target);
            gl.viewport(0, 0, self.panel.0 as i32, self.panel.1 as i32);
        }
    }

    fn drop_blur(&mut self) {
        if let Some(chain) = self.blur.take() {
            #[allow(unsafe_code)]
            unsafe {
                // SAFETY: handles created by `make_blur_chain` on this context.
                for (fbo, tex) in chain.fbos {
                    self.gl.delete_framebuffer(fbo);
                    self.gl.delete_texture(tex);
                }
            }
        }
        self.blur_source = None;
    }

    /// Read the framebuffer back as RGBA8, top row first.
    pub fn read_pixels(&self, width: u32, height: u32) -> Vec<u8> {
        let mut buf = vec![0u8; width as usize * height as usize * 4];
        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: the buffer is sized exactly for the requested rectangle.
            self.gl.read_pixels(
                0,
                0,
                width as i32,
                height as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
        }
        // GL returns bottom-up; flip so callers get a normal image.
        let stride = width as usize * 4;
        let mut out = vec![0u8; buf.len()];
        for y in 0..height as usize {
            let src = (height as usize - 1 - y) * stride;
            out[y * stride..(y + 1) * stride].copy_from_slice(&buf[src..src + stride]);
        }
        out
    }
}

impl Drop for Renderer {
    fn drop(&mut self) {
        self.drop_blur();
        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: every handle was created by this renderer.
            self.gl.delete_vertex_array(self.quad_vao);
            self.gl.delete_buffer(self.quad_vbo);
            self.gl.delete_texture(self.dummy);
            for p in [&self.artwork, &self.background, &self.kawase, &self.blit] {
                self.gl.delete_program(p.handle);
            }
        }
    }
}

impl Program {
    fn new(gl: &glow::Context, vertex: &str, fragment: &str) -> Result<Program> {
        #[allow(unsafe_code)]
        unsafe {
            // SAFETY: a current context; every object is deleted on failure.
            let program = gl.create_program().map_err(|e| anyhow::anyhow!(e))?;
            let mut shaders = Vec::new();
            for (kind, src) in [
                (glow::VERTEX_SHADER, vertex),
                (glow::FRAGMENT_SHADER, fragment),
            ] {
                let s = gl.create_shader(kind).map_err(|e| anyhow::anyhow!(e))?;
                gl.shader_source(s, src);
                gl.compile_shader(s);
                if !gl.get_shader_compile_status(s) {
                    let log = gl.get_shader_info_log(s);
                    gl.delete_shader(s);
                    gl.delete_program(program);
                    bail!("shader compile failed: {log}");
                }
                gl.attach_shader(program, s);
                shaders.push(s);
            }
            gl.link_program(program);
            for s in shaders {
                gl.detach_shader(program, s);
                gl.delete_shader(s);
            }
            if !gl.get_program_link_status(program) {
                let log = gl.get_program_info_log(program);
                gl.delete_program(program);
                bail!("program link failed: {log}");
            }
            Ok(Program { handle: program })
        }
    }

    #[allow(unsafe_code)]
    unsafe fn loc(&self, gl: &glow::Context, name: &str) -> Option<glow::UniformLocation> {
        // SAFETY: `self.handle` is a linked program on the current context.
        gl.get_uniform_location(self.handle, name)
    }

    #[allow(unsafe_code)]
    unsafe fn set_f32(&self, gl: &glow::Context, name: &str, v: f32) {
        // SAFETY: as `loc`; a missing uniform yields None and is skipped.
        if let Some(l) = self.loc(gl, name) {
            gl.uniform_1_f32(Some(&l), v);
        }
    }

    #[allow(unsafe_code)]
    unsafe fn set_i32(&self, gl: &glow::Context, name: &str, v: i32) {
        // SAFETY: as `loc`.
        if let Some(l) = self.loc(gl, name) {
            gl.uniform_1_i32(Some(&l), v);
        }
    }

    #[allow(unsafe_code)]
    unsafe fn set_vec2(&self, gl: &glow::Context, name: &str, v: [f32; 2]) {
        // SAFETY: as `loc`.
        if let Some(l) = self.loc(gl, name) {
            gl.uniform_2_f32(Some(&l), v[0], v[1]);
        }
    }

    #[allow(unsafe_code)]
    unsafe fn set_vec3(&self, gl: &glow::Context, name: &str, v: [f32; 3]) {
        // SAFETY: as `loc`.
        if let Some(l) = self.loc(gl, name) {
            gl.uniform_3_f32(Some(&l), v[0], v[1], v[2]);
        }
    }

    #[allow(unsafe_code)]
    unsafe fn set_mat2(&self, gl: &glow::Context, name: &str, v: &[f32; 4]) {
        // SAFETY: as `loc`; the slice is exactly one mat2.
        if let Some(l) = self.loc(gl, name) {
            gl.uniform_matrix_2_f32_slice(Some(&l), false, v);
        }
    }

    #[allow(unsafe_code)]
    unsafe fn set_rect(&self, gl: &glow::Context, r: &crate::geometry::Rect) {
        // SAFETY: as `loc`.
        if let Some(l) = self.loc(gl, "uRect") {
            gl.uniform_4_f32(Some(&l), r.cx, r.cy, r.half_w, r.half_h);
        }
    }
}

fn make_quad(gl: &glow::Context) -> Result<(glow::VertexArray, glow::Buffer)> {
    // Triangle strip covering -1..1.
    const VERTS: [f32; 8] = [-1.0, -1.0, 1.0, -1.0, -1.0, 1.0, 1.0, 1.0];
    #[allow(unsafe_code)]
    unsafe {
        // SAFETY: a current context; the byte view below matches VERTS.
        let vao = gl.create_vertex_array().map_err(|e| anyhow::anyhow!(e))?;
        let vbo = gl.create_buffer().map_err(|e| anyhow::anyhow!(e))?;
        gl.bind_vertex_array(Some(vao));
        gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
        let bytes: &[u8] =
            core::slice::from_raw_parts(VERTS.as_ptr() as *const u8, std::mem::size_of_val(&VERTS));
        gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STATIC_DRAW);
        gl.enable_vertex_attrib_array(0);
        gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, 8, 0);
        gl.bind_vertex_array(None);
        Ok((vao, vbo))
    }
}

fn make_dummy_texture(gl: &glow::Context) -> Result<glow::Texture> {
    #[allow(unsafe_code)]
    unsafe {
        // SAFETY: a current context; a single opaque black texel.
        let t = gl.create_texture().map_err(|e| anyhow::anyhow!(e))?;
        gl.bind_texture(glow::TEXTURE_2D, Some(t));
        gl.tex_image_2d(
            glow::TEXTURE_2D,
            0,
            glow::RGBA8 as i32,
            1,
            1,
            0,
            glow::RGBA,
            glow::UNSIGNED_BYTE,
            glow::PixelUnpackData::Slice(Some(&[0, 0, 0, 255])),
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
        gl.bind_texture(glow::TEXTURE_2D, None);
        Ok(t)
    }
}
