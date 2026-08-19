//! The mpv-to-Slint render bridge.
//!
//! Slint hands out `GraphicsAPI::NativeOpenGL { get_proc_address }`; libmpv's
//! render API takes an `OpenGLInitParams { get_proc_address }`. They plug
//! directly into each other.
//!
//! mpv renders into an FBO-backed texture that we wrap with
//! [`slint::BorrowedOpenGLTextureBuilder`] and hand to Slint as an ordinary
//! `image` property — so Slint does layout, clipping and overlays natively.

use std::cell::Cell;
use std::ffi::{CStr, CString, c_void};
use std::ptr;

use anyhow::{Result, anyhow};
use glow::HasContext;
use libmpv2::Mpv;
use libmpv2::render::{OpenGLInitParams, RenderContext, RenderParam, RenderParamApiType};
use slint::{BorrowedOpenGLTextureBuilder, BorrowedOpenGLTextureOrigin, GraphicsAPI};

type GlLoader = dyn Fn(&CStr) -> *const c_void;

/// Context passed to mpv for resolving GL function pointers.
///
/// Slint only lends its loader for the duration of one rendering-notifier
/// invocation, but `OpenGLInitParams` demands a `'static` context. We therefore
/// hold a raw pointer that is published at the top of every invocation and
/// cleared at the bottom, so it is non-null only while the borrow is live.
struct ProcAddrCtx {
    current: Cell<Option<*const GlLoader>>,
}

thread_local! {
    /// One per render thread; mpv only ever calls back from there.
    static PROC_CTX: &'static ProcAddrCtx = Box::leak(Box::new(ProcAddrCtx {
        current: Cell::new(None),
    }));
}

fn resolve_gl(ctx: &&'static ProcAddrCtx, name: &str) -> *mut c_void {
    let Some(loader) = ctx.current.get() else {
        return ptr::null_mut();
    };
    let Ok(cname) = CString::new(name) else {
        return ptr::null_mut();
    };
    // SAFETY: `current` is Some only between the top and bottom of a single
    // rendering-notifier invocation, during which the pointee is alive.
    unsafe { (*loader)(&cname) as *mut c_void }
}

/// Publishes Slint's loader for the duration of a scope.
///
/// # Safety
///
/// The caller must not let the guard outlive the notifier invocation that
/// produced `loader`.
struct LoaderGuard {
    ctx: &'static ProcAddrCtx,
}

impl LoaderGuard {
    unsafe fn new(loader: &dyn Fn(&CStr) -> *const c_void) -> Self {
        let ctx = PROC_CTX.with(|c| *c);
        // SAFETY: erasing the borrow's lifetime; cleared in Drop.
        let raw: *const GlLoader = unsafe {
            std::mem::transmute::<*const (dyn Fn(&CStr) -> *const c_void + '_), *const GlLoader>(
                loader,
            )
        };
        ctx.current.set(Some(raw));
        Self { ctx }
    }
}

impl Drop for LoaderGuard {
    fn drop(&mut self) {
        self.ctx.current.set(None);
    }
}

/// Snapshot of the OpenGL state that mpv disturbs.
///
/// `render_gl.h` is explicit about this in *both* directions:
///
/// > All the mpv functions mentioned above expect that the OpenGL state is
/// > reasonably set to OpenGL standard defaults. Likewise, mpv will attempt to
/// > leave the OpenGL context with standard defaults. The following state is
/// > excluded from this: the glViewport state, the glScissor state, ...
/// > glBlendFuncSeparate() state, glClearColor() state ...
///
/// We call `render()` from inside Slint's frame, where femtovg has its own
/// program, VAO and blend state bound — so we must reset to defaults on the way
/// in (or mpv renders incorrectly) and restore femtovg's state on the way out
/// (or the UI renders incorrectly).
struct GlState {
    framebuffer: i32,
    viewport: [i32; 4],
    scissor_box: [i32; 4],
    scissor_test: bool,
    blend: bool,
    blend_src_rgb: i32,
    blend_dst_rgb: i32,
    blend_src_alpha: i32,
    blend_dst_alpha: i32,
    blend_eq_rgb: i32,
    blend_eq_alpha: i32,
    clear_color: [f32; 4],
    program: i32,
    vertex_array: i32,
    array_buffer: i32,
    element_array_buffer: i32,
    active_texture: i32,
    texture_2d: i32,
    depth_test: bool,
    cull_face: bool,
    stencil_test: bool,
}

impl GlState {
    fn save(gl: &glow::Context) -> Self {
        unsafe {
            let mut viewport = [0i32; 4];
            gl.get_parameter_i32_slice(glow::VIEWPORT, &mut viewport);
            let mut scissor_box = [0i32; 4];
            gl.get_parameter_i32_slice(glow::SCISSOR_BOX, &mut scissor_box);
            let mut clear_color = [0f32; 4];
            gl.get_parameter_f32_slice(glow::COLOR_CLEAR_VALUE, &mut clear_color);

            Self {
                framebuffer: gl.get_parameter_i32(glow::DRAW_FRAMEBUFFER_BINDING),
                viewport,
                scissor_box,
                scissor_test: gl.is_enabled(glow::SCISSOR_TEST),
                blend: gl.is_enabled(glow::BLEND),
                blend_src_rgb: gl.get_parameter_i32(glow::BLEND_SRC_RGB),
                blend_dst_rgb: gl.get_parameter_i32(glow::BLEND_DST_RGB),
                blend_src_alpha: gl.get_parameter_i32(glow::BLEND_SRC_ALPHA),
                blend_dst_alpha: gl.get_parameter_i32(glow::BLEND_DST_ALPHA),
                blend_eq_rgb: gl.get_parameter_i32(glow::BLEND_EQUATION_RGB),
                blend_eq_alpha: gl.get_parameter_i32(glow::BLEND_EQUATION_ALPHA),
                clear_color,
                program: gl.get_parameter_i32(glow::CURRENT_PROGRAM),
                vertex_array: gl.get_parameter_i32(glow::VERTEX_ARRAY_BINDING),
                array_buffer: gl.get_parameter_i32(glow::ARRAY_BUFFER_BINDING),
                element_array_buffer: gl.get_parameter_i32(glow::ELEMENT_ARRAY_BUFFER_BINDING),
                active_texture: gl.get_parameter_i32(glow::ACTIVE_TEXTURE),
                texture_2d: gl.get_parameter_i32(glow::TEXTURE_BINDING_2D),
                depth_test: gl.is_enabled(glow::DEPTH_TEST),
                cull_face: gl.is_enabled(glow::CULL_FACE),
                stencil_test: gl.is_enabled(glow::STENCIL_TEST),
            }
        }
    }

    /// Put the context into the standard defaults mpv documents that it expects.
    fn reset_to_defaults(gl: &glow::Context) {
        unsafe {
            gl.bind_vertex_array(None);
            gl.use_program(None);
            gl.bind_buffer(glow::ARRAY_BUFFER, None);
            gl.bind_buffer(glow::ELEMENT_ARRAY_BUFFER, None);
            gl.active_texture(glow::TEXTURE0);
            gl.bind_texture(glow::TEXTURE_2D, None);
            gl.disable(glow::SCISSOR_TEST);
            gl.disable(glow::BLEND);
            gl.disable(glow::DEPTH_TEST);
            gl.disable(glow::CULL_FACE);
            gl.disable(glow::STENCIL_TEST);
            gl.color_mask(true, true, true, true);
            gl.depth_mask(true);
        }
    }

    fn restore(&self, gl: &glow::Context) {
        unsafe {
            match std::num::NonZeroU32::new(self.framebuffer as u32) {
                Some(id) => {
                    gl.bind_framebuffer(glow::FRAMEBUFFER, Some(glow::NativeFramebuffer(id)))
                }
                None => gl.bind_framebuffer(glow::FRAMEBUFFER, None),
            }
            gl.viewport(
                self.viewport[0],
                self.viewport[1],
                self.viewport[2],
                self.viewport[3],
            );
            gl.scissor(
                self.scissor_box[0],
                self.scissor_box[1],
                self.scissor_box[2],
                self.scissor_box[3],
            );
            set_enabled(gl, glow::SCISSOR_TEST, self.scissor_test);
            set_enabled(gl, glow::BLEND, self.blend);
            gl.blend_func_separate(
                self.blend_src_rgb as u32,
                self.blend_dst_rgb as u32,
                self.blend_src_alpha as u32,
                self.blend_dst_alpha as u32,
            );
            gl.blend_equation_separate(self.blend_eq_rgb as u32, self.blend_eq_alpha as u32);
            gl.clear_color(
                self.clear_color[0],
                self.clear_color[1],
                self.clear_color[2],
                self.clear_color[3],
            );

            gl.use_program(std::num::NonZeroU32::new(self.program as u32).map(glow::NativeProgram));
            gl.bind_vertex_array(
                std::num::NonZeroU32::new(self.vertex_array as u32).map(glow::NativeVertexArray),
            );
            gl.bind_buffer(
                glow::ARRAY_BUFFER,
                std::num::NonZeroU32::new(self.array_buffer as u32).map(glow::NativeBuffer),
            );
            gl.bind_buffer(
                glow::ELEMENT_ARRAY_BUFFER,
                std::num::NonZeroU32::new(self.element_array_buffer as u32).map(glow::NativeBuffer),
            );
            gl.active_texture(self.active_texture as u32);
            gl.bind_texture(
                glow::TEXTURE_2D,
                std::num::NonZeroU32::new(self.texture_2d as u32).map(glow::NativeTexture),
            );
            set_enabled(gl, glow::DEPTH_TEST, self.depth_test);
            set_enabled(gl, glow::CULL_FACE, self.cull_face);
            set_enabled(gl, glow::STENCIL_TEST, self.stencil_test);
        }
    }
}

fn set_enabled(gl: &glow::Context, cap: u32, enabled: bool) {
    unsafe {
        if enabled {
            gl.enable(cap);
        } else {
            gl.disable(cap);
        }
    }
}

struct Target {
    fbo: glow::NativeFramebuffer,
    tex: glow::NativeTexture,
    w: u32,
    h: u32,
}

pub struct VideoBridge {
    gl: glow::Context,
    render: RenderContext<'static>,
    target: Option<Target>,
}

impl VideoBridge {
    /// Build the bridge. Call from `RenderingState::RenderingSetup` only.
    pub fn new(mpv: &'static Mpv, api: &GraphicsAPI<'_>) -> Result<Self> {
        let GraphicsAPI::NativeOpenGL { get_proc_address } = api else {
            return Err(anyhow!("expected an OpenGL backend"));
        };

        let gl = unsafe { glow::Context::from_loader_function_cstr(|s| (*get_proc_address)(s)) };

        // mpv resolves entry points during context creation.
        let _guard = unsafe { LoaderGuard::new(*get_proc_address) };
        let render = mpv
            .create_render_context(vec![
                RenderParam::ApiType(RenderParamApiType::OpenGl),
                RenderParam::InitParams(OpenGLInitParams {
                    get_proc_address: resolve_gl,
                    ctx: PROC_CTX.with(|c| *c),
                }),
            ])
            .map_err(|e| anyhow!("could not create mpv render context: {e:?}"))?;

        Ok(Self {
            gl,
            render,
            target: None,
        })
    }

    pub fn on_new_frame<F: Fn() + Send + 'static>(&mut self, callback: F) {
        self.render.set_update_callback(callback);
    }

    /// Render the current frame at `w`x`h` physical pixels.
    ///
    /// Call from `RenderingState::BeforeRendering`, where the GL context is current.
    pub fn render(&mut self, api: &GraphicsAPI<'_>, w: u32, h: u32) -> Option<slint::Image> {
        let GraphicsAPI::NativeOpenGL { get_proc_address } = api else {
            return None;
        };
        let _guard = unsafe { LoaderGuard::new(*get_proc_address) };

        let w = w.max(16);
        let h = h.max(16);
        self.ensure_target(w, h);
        let target = self.target.as_ref()?;

        // We are inside femtovg's frame, so the context is nowhere near the
        // defaults mpv requires. Snapshot, reset, render, restore.
        let saved = GlState::save(&self.gl);
        GlState::reset_to_defaults(&self.gl);

        // Orientation is the product of two independent inversions: mpv's `flip`
        // and the origin we declare to Slint. Each inverts once, so they must
        // agree — `flip=false` leaves the image top-down in memory, which is
        // exactly what `BorrowedOpenGLTextureOrigin::TopLeft` means. Verified
        // against a video with labelled corners; getting this wrong is invisible
        // on symmetric test patterns.
        let result = self.render.render::<&'static ProcAddrCtx>(
            target.fbo.0.get() as i32,
            target.w as i32,
            target.h as i32,
            false,
        );
        if let Err(e) = result {
            eprintln!("warning: mpv render failed: {e:?}");
        }

        saved.restore(&self.gl);

        Some(
            unsafe {
                BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                    target.tex.0,
                    [target.w, target.h].into(),
                )
            }
            .origin(BorrowedOpenGLTextureOrigin::TopLeft)
            .build(),
        )
    }

    /// Read the *window's* framebuffer back as raw RGBA, bottom-up.
    ///
    /// Unlike [`Self::read_pixels`], which sees only mpv's output, this captures
    /// what the user actually looks at: video composited with the Slint UI.
    /// Call from `RenderingState::AfterRendering`.
    ///
    /// Debug builds only — it exists for the headless self-checks.
    #[cfg(debug_assertions)]
    pub fn read_window(&self, w: u32, h: u32) -> Vec<u8> {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.read_pixels(
                0,
                0,
                w as i32,
                h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
        }
        buf
    }

    /// Read mpv's own render target back as raw RGBA, bottom-up.
    ///
    /// Debug builds only — it exists for the headless self-checks.
    #[cfg(debug_assertions)]
    pub fn read_pixels(&self) -> Option<(u32, u32, Vec<u8>)> {
        let t = self.target.as_ref()?;
        let mut buf = vec![0u8; (t.w * t.h * 4) as usize];
        unsafe {
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(t.fbo));
            self.gl.read_pixels(
                0,
                0,
                t.w as i32,
                t.h as i32,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
        }
        Some((t.w, t.h, buf))
    }

    fn ensure_target(&mut self, w: u32, h: u32) {
        if let Some(t) = &self.target
            && t.w == w
            && t.h == h
        {
            return;
        }
        self.drop_target();

        unsafe {
            let tex = self.gl.create_texture().expect("create texture");
            self.gl.bind_texture(glow::TEXTURE_2D, Some(tex));
            self.gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::RGBA as i32,
                w as i32,
                h as i32,
                0,
                glow::RGBA,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(None),
            );
            for (name, value) in [
                (glow::TEXTURE_MIN_FILTER, glow::LINEAR),
                (glow::TEXTURE_MAG_FILTER, glow::LINEAR),
                (glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE),
                (glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE),
            ] {
                self.gl
                    .tex_parameter_i32(glow::TEXTURE_2D, name, value as i32);
            }

            let fbo = self.gl.create_framebuffer().expect("create framebuffer");
            self.gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            self.gl.framebuffer_texture_2d(
                glow::FRAMEBUFFER,
                glow::COLOR_ATTACHMENT0,
                glow::TEXTURE_2D,
                Some(tex),
                0,
            );

            let status = self.gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                eprintln!("warning: framebuffer incomplete: 0x{status:x}");
            }

            self.gl.bind_framebuffer(glow::FRAMEBUFFER, None);
            self.gl.bind_texture(glow::TEXTURE_2D, None);
            self.target = Some(Target { fbo, tex, w, h });
        }
    }

    fn drop_target(&mut self) {
        if let Some(t) = self.target.take() {
            unsafe {
                self.gl.delete_framebuffer(t.fbo);
                self.gl.delete_texture(t.tex);
            }
        }
    }
}

impl Drop for VideoBridge {
    fn drop(&mut self) {
        // Must run while the GL context is still current, i.e. from
        // RenderingState::RenderingTeardown.
        self.drop_target();
    }
}
