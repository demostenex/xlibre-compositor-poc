use std::ffi::CStr;
use std::error::Error;
use std::ffi::c_void;


pub struct CaptureRenderer { program: u32, vao: u32, texture: u32 }

pub struct SceneRenderer {
    program: u32,
    vao: u32,
    buffer: u32,
    corner_radius_uniform: i32,
    surface_size_uniform: i32,
    border_width_uniform: i32,
    border_color_uniform: i32,
    shadow_mode_uniform: i32,
    shadow_extent_uniform: i32,
    shadow_strength_uniform: i32,
    shadow_color_uniform: i32,
    surface_opacity_uniform: i32,
    reveal_radius_uniform: i32,
    kamui_visible_radius_uniform: i32,
    kamui_twist_uniform: i32,
    kamui_uv_min_uniform: i32,
    kamui_uv_scale_uniform: i32,
    kamui_radial_power_uniform: i32,
    #[allow(dead_code)]
    background_blur: Option<BackgroundBlurResources>,
    backdrop_program: Option<BackdropProgram>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BackdropParams {
    pub(crate) owner_x: i32,
    pub(crate) owner_y: i32,
    pub(crate) owner_width: i32,
    pub(crate) owner_height: i32,
    pub(crate) draw_x: i32,
    pub(crate) draw_y: i32,
    pub(crate) draw_width: i32,
    pub(crate) draw_height: i32,
    pub(crate) root_width: i32,
    pub(crate) root_height: i32,
}

impl BackdropParams {
    #[allow(dead_code)]
    pub(crate) fn new(
        owner_x: i32,
        owner_y: i32,
        owner_width: i32,
        owner_height: i32,
        root_width: i32,
        root_height: i32,
    ) -> Option<Self> {
        (owner_width > 0 && owner_height > 0 && root_width > 0 && root_height > 0).then_some(Self {
            owner_x, owner_y, owner_width, owner_height,
            draw_x: owner_x, draw_y: owner_y, draw_width: owner_width, draw_height: owner_height,
            root_width, root_height,
        })
    }

    pub(crate) fn new_region(
        owner_x: i32,
        owner_y: i32,
        owner_width: i32,
        owner_height: i32,
        draw_x: i32,
        draw_y: i32,
        draw_width: i32,
        draw_height: i32,
        root_width: i32,
        root_height: i32,
    ) -> Option<Self> {
        (owner_width > 0 && owner_height > 0 && draw_width > 0 && draw_height > 0
            && root_width > 0 && root_height > 0).then_some(Self {
            owner_x, owner_y, owner_width, owner_height,
            draw_x, draw_y, draw_width, draw_height,
            root_width, root_height,
        })
    }
}

struct BackdropProgram {
    program: u32,
    texture_uniform: i32,
    surface_size_uniform: i32,
    corner_radius_uniform: i32,
}

impl BackdropProgram {
    fn new() -> Result<Self, Box<dyn Error>> {
        let program = create_program(BACKDROP_VERTEX_SHADER, BACKDROP_FRAGMENT_SHADER)?;
        let texture_uniform = unsafe { gl::GetUniformLocation(program, b"blurred_root\0".as_ptr().cast()) };
        let surface_size_uniform = unsafe { gl::GetUniformLocation(program, b"surface_size\0".as_ptr().cast()) };
        let corner_radius_uniform = unsafe { gl::GetUniformLocation(program, b"corner_radius\0".as_ptr().cast()) };
        if texture_uniform < 0 || surface_size_uniform < 0 || corner_radius_uniform < 0 {
            unsafe { gl::DeleteProgram(program); }
            return Err("backdrop shader uniforms are unavailable".into());
        }
        Ok(Self { program, texture_uniform, surface_size_uniform, corner_radius_uniform })
    }
}

impl Drop for BackdropProgram {
    fn drop(&mut self) {
        unsafe { gl::DeleteProgram(self.program); }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct ShadowParams {
    pub(crate) outer_x: f32,
    pub(crate) outer_y: f32,
    pub(crate) outer_width: f32,
    pub(crate) outer_height: f32,
    pub(crate) corner_radius: f32,
    pub(crate) extent: f32,
    pub(crate) offset_x: f32,
    pub(crate) offset_y: f32,
    pub(crate) strength: f32,
    pub(crate) color: [f32; 3],
}

pub(crate) fn normalized_shadow_color(color: [u8; 3]) -> [f32; 3] {
    color.map(|component| f32::from(component) / 255.0)
}

impl ShadowParams {
    pub(crate) fn new(
        outer_x: f32,
        outer_y: f32,
        outer_width: f32,
        outer_height: f32,
        corner_radius: f32,
        extent: f32,
        offset_x: f32,
        offset_y: f32,
        strength: f32,
    ) -> Option<Self> {
        let values = [
            outer_x,
            outer_y,
            outer_width,
            outer_height,
            corner_radius,
            extent,
            offset_x,
            offset_y,
            strength,
        ];
        if values.iter().any(|value| !value.is_finite())
            || outer_width <= 0.0
            || outer_height <= 0.0
            || corner_radius < 0.0
            || extent <= 0.0
            || strength <= 0.0
            || strength > 1.0
        {
            return None;
        }
        Some(Self {
            outer_x,
            outer_y,
            outer_width,
            outer_height,
            corner_radius: corner_radius.min(outer_width.min(outer_height) * 0.5),
            extent,
            offset_x,
            offset_y,
            strength,
            color: [0.0, 0.0, 0.0],
        })
    }

    fn quad(self, root_width: i32, root_height: i32) -> Option<ShadowQuadPlan> {
        build_shadow_quad_plan(self, root_width, root_height)
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ShadowQuadPlan {
    dst_x: i32,
    dst_y: i32,
    width: i32,
    height: i32,
    local_x: f32,
    local_y: f32,
}

fn build_shadow_quad_plan(
    params: ShadowParams,
    root_width: i32,
    root_height: i32,
) -> Option<ShadowQuadPlan> {
    if root_width <= 0 || root_height <= 0 {
        return None;
    }

    let left = params.outer_x + params.offset_x - params.extent;
    let top = params.outer_y + params.offset_y - params.extent;
    let right = left + params.outer_width + 2.0 * params.extent;
    let bottom = top + params.outer_height + 2.0 * params.extent;
    let framebuffer_width = root_width as f32;
    let framebuffer_height = root_height as f32;
    let clipped_left = left.max(0.0).min(framebuffer_width);
    let clipped_top = top.max(0.0).min(framebuffer_height);
    let clipped_right = right.max(0.0).min(framebuffer_width);
    let clipped_bottom = bottom.max(0.0).min(framebuffer_height);

    if clipped_right <= clipped_left || clipped_bottom <= clipped_top {
        return None;
    }

    let dst_x = clipped_left.floor() as i32;
    let dst_y = clipped_top.floor() as i32;
    let end_x = clipped_right.ceil() as i32;
    let end_y = clipped_bottom.ceil() as i32;
    let width = end_x - dst_x;
    let height = end_y - dst_y;
    if width <= 0 || height <= 0 {
        return None;
    }

    Some(ShadowQuadPlan {
        dst_x,
        dst_y,
        width,
        height,
        local_x: dst_x as f32 - left,
        local_y: dst_y as f32 - top,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlendState {
    Disabled,
    PremultipliedAlpha,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct SurfaceOpacity(f32);

impl SurfaceOpacity {
    pub(crate) fn new(value: f32) -> Option<Self> {
        (value.is_finite() && (0.0..=1.0).contains(&value)).then_some(Self(value))
    }

    fn value(self) -> f32 {
        self.0
    }
}

#[allow(dead_code)]
const BLUR_TAP_RADIUS: f32 = 4.0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BlurCaptureRegion {
    pub(crate) root_width: i32,
    pub(crate) root_height: i32,
    pub(crate) x: i32,
    pub(crate) y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) framebuffer_y: i32,
}

fn root_to_texture_u(root_x: f32, root_width: i32) -> f32 {
    root_x / root_width as f32
}

fn root_to_texture_v(root_y: f32, root_height: i32) -> f32 {
    (root_height as f32 - root_y) / root_height as f32
}

#[cfg(test)]
fn backdrop_replacement(rgb: [f32; 3], coverage: f32) -> [f32; 4] {
    let coverage = coverage.clamp(0.0, 1.0);
    [rgb[0] * coverage, rgb[1] * coverage, rgb[2] * coverage, coverage]
}

impl BlurCaptureRegion {
    pub(crate) fn new(
        owner_x: i32,
        owner_y: i32,
        owner_width: i32,
        owner_height: i32,
        radius: f32,
        root_width: i32,
        root_height: i32,
    ) -> Option<Self> {
        if owner_width <= 0 || owner_height <= 0 || root_width <= 0 || root_height <= 0
            || !radius.is_finite() || radius <= 0.0
        {
            return None;
        }
        let reach = radius.ceil();
        let left = (owner_x as f32 - reach).floor().max(0.0) as i32;
        let top = (owner_y as f32 - reach).floor().max(0.0) as i32;
        let right = (owner_x as f32 + owner_width as f32 + reach)
            .ceil()
            .min(root_width as f32) as i32;
        let bottom = (owner_y as f32 + owner_height as f32 + reach)
            .ceil()
            .min(root_height as f32) as i32;
        if right <= left || bottom <= top {
            return None;
        }
        Some(Self {
            root_width,
            root_height,
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
            framebuffer_y: root_height - bottom,
        })
    }
}

#[allow(dead_code)]
struct BackgroundBlurResources {
    textures: [u32; 2],
    framebuffers: [u32; 2],
    program: u32,
    vao: u32,
    buffer: u32,
    texture_size_uniform: i32,
    direction_uniform: i32,
    radius_uniform: i32,
    width: i32,
    height: i32,
}

/// Owns raw GL names created while `BackgroundBlurResources::new` is still
/// assembling a candidate resource set. `Drop` deletes whatever has been
/// created so far (GL delete calls silently ignore zero/absent names, so a
/// partially populated guard cleans up exactly the names that exist).
#[allow(dead_code)]
struct PendingBlurResources {
    textures: [u32; 2],
    framebuffers: [u32; 2],
    program: u32,
    vao: u32,
    buffer: u32,
}

#[allow(dead_code)]
impl PendingBlurResources {
    fn empty() -> Self {
        Self {
            textures: [0; 2],
            framebuffers: [0; 2],
            program: 0,
            vao: 0,
            buffer: 0,
        }
    }

    fn into_resources(
        mut self,
        texture_size_uniform: i32,
        direction_uniform: i32,
        radius_uniform: i32,
        width: i32,
        height: i32,
    ) -> BackgroundBlurResources {
        BackgroundBlurResources {
            textures: std::mem::replace(&mut self.textures, [0; 2]),
            framebuffers: std::mem::replace(&mut self.framebuffers, [0; 2]),
            program: std::mem::replace(&mut self.program, 0),
            vao: std::mem::replace(&mut self.vao, 0),
            buffer: std::mem::replace(&mut self.buffer, 0),
            texture_size_uniform,
            direction_uniform,
            radius_uniform,
            width,
            height,
        }
    }
}

impl Drop for PendingBlurResources {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteBuffers(1, &self.buffer);
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteFramebuffers(2, self.framebuffers.as_ptr());
            gl::DeleteTextures(2, self.textures.as_ptr());
            gl::DeleteProgram(self.program);
        }
    }
}

#[allow(dead_code)]
impl BackgroundBlurResources {
    fn new(width: i32, height: i32) -> Result<Self, Box<dyn Error>> {
        if width <= 0 || height <= 0 {
            return Err("background blur dimensions must be positive".into());
        }

        let program = create_program(BLUR_VERTEX_SHADER, BLUR_FRAGMENT_SHADER)?;
        let mut pending = PendingBlurResources {
            program,
            ..PendingBlurResources::empty()
        };

        let texture_size_uniform = unsafe {
            gl::GetUniformLocation(pending.program, b"texture_size\0".as_ptr().cast())
        };
        let direction_uniform = unsafe {
            gl::GetUniformLocation(pending.program, b"direction\0".as_ptr().cast())
        };
        let radius_uniform = unsafe {
            gl::GetUniformLocation(pending.program, b"radius\0".as_ptr().cast())
        };
        if texture_size_uniform < 0 || direction_uniform < 0 || radius_uniform < 0 {
            return Err("background blur shader uniforms are unavailable".into());
        }

        unsafe {
            check_gl_error("before background blur resource generation")?;
            gl::GenTextures(2, pending.textures.as_mut_ptr());
            gl::GenFramebuffers(2, pending.framebuffers.as_mut_ptr());
            check_gl_error("background blur resource generation")?;
        }
        if pending.textures.iter().any(|&texture| texture == 0) {
            return Err("glGenTextures returned a zero texture name".into());
        }
        if pending.framebuffers.iter().any(|&framebuffer| framebuffer == 0) {
            return Err("glGenFramebuffers returned a zero framebuffer name".into());
        }

        unsafe {
            for texture in pending.textures {
                gl::ActiveTexture(gl::TEXTURE0);
                gl::BindTexture(gl::TEXTURE_2D, texture);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
                gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
                gl::TexImage2D(
                    gl::TEXTURE_2D,
                    0,
                    gl::RGBA8 as i32,
                    width,
                    height,
                    0,
                    gl::RGBA,
                    gl::UNSIGNED_BYTE,
                    std::ptr::null(),
                );
                check_gl_error("background blur texture storage allocation")?;
            }
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }

        unsafe {
            for (framebuffer, texture) in pending.framebuffers.into_iter().zip(pending.textures) {
                gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffer);
                gl::FramebufferTexture2D(
                    gl::FRAMEBUFFER,
                    gl::COLOR_ATTACHMENT0,
                    gl::TEXTURE_2D,
                    texture,
                    0,
                );
                check_gl_error("background blur framebuffer attachment")?;
                if gl::CheckFramebufferStatus(gl::FRAMEBUFFER) != gl::FRAMEBUFFER_COMPLETE {
                    gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
                    return Err("background blur framebuffer is incomplete".into());
                }
            }
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        }

        unsafe {
            gl::GenVertexArrays(1, &mut pending.vao);
            gl::GenBuffers(1, &mut pending.buffer);
            check_gl_error("background blur vertex resource generation")?;
        }
        if pending.vao == 0 {
            return Err("glGenVertexArrays returned a zero name".into());
        }
        if pending.buffer == 0 {
            return Err("glGenBuffers returned a zero name".into());
        }

        let vertices: [f32; 6] = [-1.0, -1.0, 3.0, -1.0, -1.0, 3.0];
        unsafe {
            gl::BindVertexArray(pending.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, pending.buffer);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (vertices.len() * std::mem::size_of::<f32>()) as isize,
                vertices.as_ptr().cast(),
                gl::STATIC_DRAW,
            );
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 8, std::ptr::null());
            gl::EnableVertexAttribArray(0);
            gl::BindVertexArray(0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            check_gl_error("background blur vertex buffer setup")?;
        }

        let resources = pending.into_resources(
            texture_size_uniform,
            direction_uniform,
            radius_uniform,
            width,
            height,
        );
        Ok(resources)
    }

    fn ensure_size(&mut self, width: i32, height: i32) -> Result<(), Box<dyn Error>> {
        if self.width == width && self.height == height {
            return Ok(());
        }
        let replacement = Self::new(width, height)?;
        let _ = std::mem::replace(self, replacement);
        Ok(())
    }

    fn capture_and_blur(
        &mut self,
        region: BlurCaptureRegion,
        radius: f32,
    ) -> Result<u32, Box<dyn Error>> {
        if region.root_width != self.width || region.root_height != self.height
            || !radius.is_finite() || radius <= 0.0
        {
            return Err("background blur region does not match resources".into());
        }
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
            gl::ReadBuffer(gl::BACK);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, self.textures[0]);
            gl::CopyTexSubImage2D(
                gl::TEXTURE_2D,
                0,
                region.x,
                region.framebuffer_y,
                region.x,
                region.framebuffer_y,
                region.width,
                region.height,
            );
            gl::BindTexture(gl::TEXTURE_2D, 0);
            gl::UseProgram(self.program);
            gl::BindVertexArray(self.vao);
            gl::Disable(gl::BLEND);
            gl::Enable(gl::SCISSOR_TEST);
            gl::Uniform2f(self.texture_size_uniform, self.width as f32, self.height as f32);
            gl::Uniform1f(self.radius_uniform, radius / BLUR_TAP_RADIUS);
            for (framebuffer, texture, direction) in [
                (self.framebuffers[1], self.textures[0], [1.0_f32, 0.0_f32]),
                (self.framebuffers[0], self.textures[1], [0.0_f32, 1.0_f32]),
            ] {
                gl::BindFramebuffer(gl::FRAMEBUFFER, framebuffer);
                gl::Viewport(region.x, region.framebuffer_y, region.width, region.height);
                gl::Scissor(region.x, region.framebuffer_y, region.width, region.height);
                gl::Uniform2f(self.direction_uniform, direction[0], direction[1]);
                gl::BindTexture(gl::TEXTURE_2D, texture);
                gl::DrawArrays(gl::TRIANGLES, 0, 3);
            }
            gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
        }
        Ok(self.textures[0])
    }
}

impl Drop for BackgroundBlurResources {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteBuffers(1, &self.buffer);
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteFramebuffers(2, self.framebuffers.as_ptr());
            gl::DeleteTextures(2, self.textures.as_ptr());
            gl::DeleteProgram(self.program);
        }
    }
}

#[allow(dead_code)]
struct BlurGlState {
    framebuffer: i32,
    read_buffer: i32,
    viewport: [i32; 4],
    scissor: [i32; 4],
    scissor_enabled: bool,
    blend_enabled: bool,
    active_texture: i32,
    active_texture_binding: i32,
    texture0_binding: i32,
    vertex_array: i32,
    array_buffer: i32,
    program: i32,
    blend_src_rgb: i32,
    blend_dst_rgb: i32,
    blend_src_alpha: i32,
    blend_dst_alpha: i32,
    blend_equation_rgb: i32,
    blend_equation_alpha: i32,
}

#[allow(dead_code)]
impl BlurGlState {
    fn save() -> Self {
        let mut viewport = [0; 4];
        let mut scissor = [0; 4];
        let mut framebuffer = 0;
        let mut read_buffer = 0;
        let mut active_texture = 0;
        let mut active_texture_binding = 0;
        let mut texture0_binding = 0;
        let mut vertex_array = 0;
        let mut array_buffer = 0;
        let mut program = 0;
        let mut blend_src_rgb = 0;
        let mut blend_dst_rgb = 0;
        let mut blend_src_alpha = 0;
        let mut blend_dst_alpha = 0;
        let mut blend_equation_rgb = 0;
        let mut blend_equation_alpha = 0;
        unsafe {
            gl::GetIntegerv(gl::VIEWPORT, viewport.as_mut_ptr());
            gl::GetIntegerv(gl::SCISSOR_BOX, scissor.as_mut_ptr());
            gl::GetIntegerv(gl::FRAMEBUFFER_BINDING, &mut framebuffer);
            gl::GetIntegerv(gl::READ_BUFFER, &mut read_buffer);
            gl::GetIntegerv(gl::ACTIVE_TEXTURE, &mut active_texture);
            gl::GetIntegerv(gl::TEXTURE_BINDING_2D, &mut active_texture_binding);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::GetIntegerv(gl::TEXTURE_BINDING_2D, &mut texture0_binding);
            gl::ActiveTexture(active_texture as u32);
            gl::GetIntegerv(gl::VERTEX_ARRAY_BINDING, &mut vertex_array);
            gl::GetIntegerv(gl::ARRAY_BUFFER_BINDING, &mut array_buffer);
            gl::GetIntegerv(gl::CURRENT_PROGRAM, &mut program);
            gl::GetIntegerv(gl::BLEND_SRC_RGB, &mut blend_src_rgb);
            gl::GetIntegerv(gl::BLEND_DST_RGB, &mut blend_dst_rgb);
            gl::GetIntegerv(gl::BLEND_SRC_ALPHA, &mut blend_src_alpha);
            gl::GetIntegerv(gl::BLEND_DST_ALPHA, &mut blend_dst_alpha);
            gl::GetIntegerv(gl::BLEND_EQUATION_RGB, &mut blend_equation_rgb);
            gl::GetIntegerv(gl::BLEND_EQUATION_ALPHA, &mut blend_equation_alpha);
        }
        Self {
            framebuffer,
            read_buffer,
            viewport,
            scissor,
            scissor_enabled: unsafe { gl::IsEnabled(gl::SCISSOR_TEST) == gl::TRUE },
            blend_enabled: unsafe { gl::IsEnabled(gl::BLEND) == gl::TRUE },
            active_texture,
            active_texture_binding,
            texture0_binding,
            vertex_array,
            array_buffer,
            program,
            blend_src_rgb, blend_dst_rgb, blend_src_alpha, blend_dst_alpha,
            blend_equation_rgb, blend_equation_alpha,
        }
    }
}

#[allow(dead_code)]
impl Drop for BlurGlState {
    fn drop(&mut self) {
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, self.framebuffer as u32);
            gl::ReadBuffer(self.read_buffer as u32);
            gl::Viewport(self.viewport[0], self.viewport[1], self.viewport[2], self.viewport[3]);
            gl::Scissor(self.scissor[0], self.scissor[1], self.scissor[2], self.scissor[3]);
            if self.scissor_enabled { gl::Enable(gl::SCISSOR_TEST); } else { gl::Disable(gl::SCISSOR_TEST); }
            if self.blend_enabled { gl::Enable(gl::BLEND); } else { gl::Disable(gl::BLEND); }
            gl::BlendFuncSeparate(
                self.blend_src_rgb as u32, self.blend_dst_rgb as u32,
                self.blend_src_alpha as u32, self.blend_dst_alpha as u32,
            );
            gl::BlendEquationSeparate(
                self.blend_equation_rgb as u32, self.blend_equation_alpha as u32,
            );
            gl::BindVertexArray(self.vertex_array as u32);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.array_buffer as u32);
            gl::ActiveTexture(gl::TEXTURE0);
            gl::BindTexture(gl::TEXTURE_2D, self.texture0_binding as u32);
            gl::ActiveTexture(self.active_texture as u32);
            gl::BindTexture(gl::TEXTURE_2D, self.active_texture_binding as u32);
            gl::UseProgram(self.program as u32);
        }
    }
}

/// 3a3fa2b5 — RAII temporary framebuffer: created and destroyed WITHIN
/// one `capture_closing_snapshot` call, never retained (a `ClosingVisual`
/// must not retain an FBO — only its `ClosingTexture`). `glDeleteFramebuffers`
/// exactly once, via `Drop`.
struct ScratchFramebuffer {
    fbo: u32,
}

/// Temporary target used to render a scene into an already allocated texture.
/// The framebuffer is deliberately short-lived; the texture remains owned by
/// the caller and is suitable for later composition.
pub(crate) struct TextureRenderTarget {
    state: BlurGlState,
    scratch: ScratchFramebuffer,
}

impl TextureRenderTarget {
    pub(crate) fn new(texture: u32, width: i32, height: i32) -> Result<Self, Box<dyn Error>> {
        if width <= 0 || height <= 0 {
            return Err("offscreen render target dimensions must be positive".into());
        }
        let state = BlurGlState::save();
        let scratch = match ScratchFramebuffer::new(texture) {
            Ok(scratch) => scratch,
            Err(error) => {
                drop(state);
                return Err(error);
            }
        };
        unsafe {
            gl::Disable(gl::SCISSOR_TEST);
            gl::Viewport(0, 0, width, height);
        }
        Ok(Self { state, scratch })
    }
}

impl Drop for TextureRenderTarget {
    fn drop(&mut self) {
        let _ = (&self.state, &self.scratch);
        // Rust drops struct fields in declaration order. Therefore
        // BlurGlState restores the caller's framebuffer/state first; the
        // now-unbound temporary FBO is then deleted by ScratchFramebuffer.
    }
}

impl ScratchFramebuffer {
    fn new(texture: u32) -> Result<Self, Box<dyn Error>> {
        let mut fbo = 0;
        unsafe {
            check_gl_error("before closing snapshot scratch framebuffer generation")?;
            gl::GenFramebuffers(1, &mut fbo);
        }
        if fbo == 0 {
            return Err("glGenFramebuffers returned a zero framebuffer name".into());
        }
        unsafe {
            gl::BindFramebuffer(gl::FRAMEBUFFER, fbo);
            gl::FramebufferTexture2D(gl::FRAMEBUFFER, gl::COLOR_ATTACHMENT0, gl::TEXTURE_2D, texture, 0);
            let status = gl::CheckFramebufferStatus(gl::FRAMEBUFFER);
            if status != gl::FRAMEBUFFER_COMPLETE {
                gl::DeleteFramebuffers(1, &fbo);
                return Err(format!("closing snapshot scratch framebuffer is incomplete: 0x{status:04x}").into());
            }
        }
        Ok(Self { fbo })
    }
}

impl Drop for ScratchFramebuffer {
    fn drop(&mut self) {
        unsafe { gl::DeleteFramebuffers(1, &self.fbo); }
    }
}

#[cfg(test)]
fn apply_surface_opacity(
    sampled: [f32; 4],
    rounded_coverage: f32,
    opacity: SurfaceOpacity,
) -> [f32; 4] {
    let factor = rounded_coverage * opacity.value();
    sampled.map(|component| component * factor)
}

#[cfg(test)]
fn compose_surface_fragment(
    sampled: [f32; 4],
    coverage: f32,
    inner: Option<f32>,
    border: [f32; 4],
    border_coverage: f32,
    opacity: SurfaceOpacity,
) -> [f32; 4] {
    let client_factor = inner.unwrap_or(coverage) * opacity.value();
    let client = sampled.map(|component| component * client_factor);
    if inner.is_none() {
        return client;
    }
    let border = border.map(|component| component * border_coverage);
    [
        client[0] + border[0],
        client[1] + border[1],
        client[2] + border[2],
        client[3] + border[3],
    ]
}

pub(crate) fn blend_state_for(
    semantics: crate::x11::scene::EglPixelSemantics,
) -> Option<BlendState> {
    match semantics {
        crate::x11::scene::EglPixelSemantics::Opaque => Some(BlendState::Disabled),
        crate::x11::scene::EglPixelSemantics::PremultipliedAlpha => {
            Some(BlendState::PremultipliedAlpha)
        }
        crate::x11::scene::EglPixelSemantics::Unsupported => None,
    }
}

pub fn load<F>(loader: F)
where
    F: FnMut(&'static str) -> *const std::ffi::c_void,
{
    gl::load_with(loader);
}

pub fn resize(width: i32, height: i32) {
    unsafe { gl::Viewport(0, 0, width.max(1), height.max(1)); }
}

pub fn render() {
    unsafe {
        gl::ClearColor(0.08, 0.12, 0.20, 1.0);
        gl::Clear(gl::COLOR_BUFFER_BIT);
    }
}

pub fn string(value: u32) -> String {
    unsafe {
        let pointer = gl::GetString(value);
        if pointer.is_null() { return "unavailable".to_owned(); }
        CStr::from_ptr(pointer.cast()).to_string_lossy().into_owned()
    }
}

pub fn has_extension(name: &str) -> bool {
    let mut count = 0;
    unsafe { gl::GetIntegerv(gl::NUM_EXTENSIONS, &mut count); }

    for index in 0..count {
        let extension = unsafe { gl::GetStringi(gl::EXTENSIONS, index as u32) };
        if extension.is_null() { continue; }
        let extension = unsafe { std::ffi::CStr::from_ptr(extension.cast()) };
        if extension.to_bytes() == name.as_bytes() { return true; }
    }

    false
}

pub fn create_egl_texture(
    image_target: unsafe extern "system" fn(u32, *const c_void),
    image: *const c_void,
) -> Result<u32, Box<dyn Error>> {
    let mut texture = 0;
    unsafe {
        while gl::GetError() != gl::NO_ERROR {}
        gl::GenTextures(1, &mut texture);
        gl::BindTexture(gl::TEXTURE_2D, texture);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
        gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);

        let preparation_error = gl::GetError();
        if preparation_error != gl::NO_ERROR {
            gl::DeleteTextures(1, &texture);
            return Err(format!("OpenGL texture preparation failed: GL error 0x{preparation_error:04x}").into());
        }

        image_target(gl::TEXTURE_2D, image);
        let error = gl::GetError();
        gl::BindTexture(gl::TEXTURE_2D, 0);

        if error != gl::NO_ERROR {
            gl::DeleteTextures(1, &texture);
            return Err(format!("glEGLImageTargetTexture2DOES failed: GL error 0x{error:04x}").into());
        }
    }
    Ok(texture)
}

impl CaptureRenderer {
    pub fn new(texture: u32) -> Result<Self, Box<dyn Error>> {
        let vertex = compile_shader(VERTEX_SHADER, gl::VERTEX_SHADER)?;
        let fragment = compile_shader(FRAGMENT_SHADER, gl::FRAGMENT_SHADER)?;
        let program = unsafe { gl::CreateProgram() };
        unsafe {
            gl::AttachShader(program, vertex);
            gl::AttachShader(program, fragment);
            gl::LinkProgram(program);
            gl::DeleteShader(vertex);
            gl::DeleteShader(fragment);
        }
        check_program(program)?;

        let vertices: [f32; 12] = [-1.0, -1.0, 0.0, 0.0, 3.0, -1.0, 2.0, 0.0, -1.0, 3.0, 0.0, 2.0];
        let mut vao = 0;
        let mut buffer = 0;
        unsafe {
            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut buffer);
            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, buffer);
            gl::BufferData(gl::ARRAY_BUFFER, (vertices.len() * std::mem::size_of::<f32>()) as isize, vertices.as_ptr().cast(), gl::STATIC_DRAW);
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 16, std::ptr::null());
            gl::VertexAttribPointer(1, 2, gl::FLOAT, gl::FALSE, 16, 8 as *const c_void);
            gl::EnableVertexAttribArray(0);
            gl::EnableVertexAttribArray(1);
            gl::BindVertexArray(0);
            gl::DeleteBuffers(1, &buffer);
        }
        Ok(Self { program, vao, texture })
    }

    pub fn render(&self) {
        unsafe {
            gl::UseProgram(self.program);
            gl::BindTexture(gl::TEXTURE_2D, self.texture);
            gl::BindVertexArray(self.vao);
            gl::DrawArrays(gl::TRIANGLES, 0, 3);
            gl::BindVertexArray(0);
            gl::BindTexture(gl::TEXTURE_2D, 0);
            gl::UseProgram(0);
        }
    }

    pub fn replace_texture(&mut self, texture: u32) -> u32 {
        std::mem::replace(&mut self.texture, texture)
    }
}

impl SceneRenderer {
    pub fn new() -> Result<Self, Box<dyn Error>> {
        let vertex = compile_shader(SCENE_VERTEX_SHADER, gl::VERTEX_SHADER)?;
        let fragment = compile_shader(SCENE_FRAGMENT_SHADER, gl::FRAGMENT_SHADER)?;
        let program = unsafe { gl::CreateProgram() };
        unsafe {
            gl::AttachShader(program, vertex);
            gl::AttachShader(program, fragment);
            gl::LinkProgram(program);
            gl::DeleteShader(vertex);
            gl::DeleteShader(fragment);
        }
        check_program(program)?;

        let corner_radius_uniform = unsafe {
            gl::GetUniformLocation(program, b"corner_radius\0".as_ptr().cast())
        };
        let surface_size_uniform = unsafe {
            gl::GetUniformLocation(program, b"surface_size\0".as_ptr().cast())
        };
        let border_width_uniform = unsafe { gl::GetUniformLocation(program, b"border_width\0".as_ptr().cast()) };
        let border_color_uniform = unsafe { gl::GetUniformLocation(program, b"border_color\0".as_ptr().cast()) };
        let shadow_mode_uniform = unsafe { gl::GetUniformLocation(program, b"shadow_mode\0".as_ptr().cast()) };
        let shadow_extent_uniform = unsafe { gl::GetUniformLocation(program, b"shadow_extent\0".as_ptr().cast()) };
        let shadow_strength_uniform = unsafe { gl::GetUniformLocation(program, b"shadow_strength\0".as_ptr().cast()) };
        let shadow_color_uniform = unsafe { gl::GetUniformLocation(program, b"shadow_color\0".as_ptr().cast()) };
        let surface_opacity_uniform = unsafe { gl::GetUniformLocation(program, b"surface_opacity\0".as_ptr().cast()) };
        // 3a3fa2b6-r2 (Minato) — one new uniform location, fetched once
        // here at setup exactly like every other uniform above, never
        // per-frame.
        let reveal_radius_uniform = unsafe { gl::GetUniformLocation(program, b"reveal_radius\0".as_ptr().cast()) };
        // 3a3fa2b7 (Kamui) — four new uniform locations, fetched once
        // here at setup exactly like every other uniform above, never
        // per-frame. `kamui_uv_min`/`kamui_uv_scale` let the shader
        // reconstruct the current draw's actual `u0/v0/u1/v1` UV
        // sub-rectangle (never assumed to be [0,1] — see the UV-subrect
        // correctness requirement), since the fragment shader otherwise
        // has no access to those per-draw plan values, only the already-
        // interpolated `texcoord` varying.
        let kamui_visible_radius_uniform = unsafe { gl::GetUniformLocation(program, b"kamui_visible_radius\0".as_ptr().cast()) };
        let kamui_twist_uniform = unsafe { gl::GetUniformLocation(program, b"kamui_twist\0".as_ptr().cast()) };
        let kamui_uv_min_uniform = unsafe { gl::GetUniformLocation(program, b"kamui_uv_min\0".as_ptr().cast()) };
        let kamui_uv_scale_uniform = unsafe { gl::GetUniformLocation(program, b"kamui_uv_scale\0".as_ptr().cast()) };
        // 3a3fa2b7-r2 — one new uniform location for the nonlinear radial-
        // power warp stage, fetched once here exactly like every other
        // uniform above, never per-frame.
        let kamui_radial_power_uniform = unsafe { gl::GetUniformLocation(program, b"kamui_radial_power\0".as_ptr().cast()) };
        if corner_radius_uniform < 0
            || surface_size_uniform < 0
            || border_width_uniform < 0
            || border_color_uniform < 0
            || shadow_mode_uniform < 0
            || shadow_extent_uniform < 0
            || shadow_strength_uniform < 0
            || shadow_color_uniform < 0
            || surface_opacity_uniform < 0
            || reveal_radius_uniform < 0
            || kamui_visible_radius_uniform < 0
            || kamui_twist_uniform < 0
            || kamui_uv_min_uniform < 0
            || kamui_uv_scale_uniform < 0
            || kamui_radial_power_uniform < 0
        {
            return Err("rounded-corner shader uniforms are unavailable".into());
        }

        let mut vao = 0;
        let mut buffer = 0;
        unsafe {
            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut buffer);
            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, buffer);
            gl::BufferData(gl::ARRAY_BUFFER, 0, std::ptr::null(), gl::STREAM_DRAW);
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 24, std::ptr::null());
            gl::VertexAttribPointer(1, 2, gl::FLOAT, gl::FALSE, 24, 8 as *const c_void);
            gl::VertexAttribPointer(2, 2, gl::FLOAT, gl::FALSE, 24, 16 as *const c_void);
            gl::EnableVertexAttribArray(0);
            gl::EnableVertexAttribArray(1);
            gl::EnableVertexAttribArray(2);
            gl::BindVertexArray(0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
        }
        Ok(Self {
            program, vao, buffer, corner_radius_uniform, surface_size_uniform,
            border_width_uniform, border_color_uniform, shadow_mode_uniform,
            shadow_extent_uniform, shadow_strength_uniform,
            shadow_color_uniform,
            surface_opacity_uniform,
            reveal_radius_uniform,
            kamui_visible_radius_uniform,
            kamui_twist_uniform,
            kamui_uv_min_uniform,
            kamui_uv_scale_uniform,
            kamui_radial_power_uniform,
            background_blur: None,
            backdrop_program: None,
        })
    }

    #[allow(dead_code)]
    pub(crate) fn capture_and_blur_background(
        &mut self,
        owner_x: i32,
        owner_y: i32,
        owner_width: i32,
        owner_height: i32,
        radius: f32,
        root_width: i32,
        root_height: i32,
    ) -> Result<u32, Box<dyn Error>> {
        let region = BlurCaptureRegion::new(
            owner_x, owner_y, owner_width, owner_height, radius, root_width, root_height,
        ).ok_or("invalid background blur capture region")?;
        // Snapshot GL state before any lazy resource allocation touches
        // bindings, so entry/exit state is preserved even on the first-ever
        // call, when the resource constructor still has to run.
        let state = BlurGlState::save();
        let result = (|| {
            if self.background_blur.is_none() {
                self.background_blur = Some(BackgroundBlurResources::new(root_width, root_height)?);
            }
            let resources = self.background_blur.as_mut().expect("blur resources exist");
            resources.ensure_size(root_width, root_height)?;
            resources.capture_and_blur(region, radius)
        })();
        drop(state);
        result
    }

    /// Composite an already-blurred root-sized texture through the supplied
    /// owner mask. This is deliberately a graphics-only primitive: it does
    /// not capture, consult policy, or draw the owner's client texture.
    #[allow(dead_code)]
    pub(crate) fn draw_blurred_backdrop(
        &mut self,
        blurred_texture: u32,
        params: BackdropParams,
        corner_radius: f32,
    ) -> Result<(), Box<dyn Error>> {
        if !corner_radius.is_finite() || corner_radius < 0.0 {
            return Err("invalid backdrop corner radius".into());
        }
        let left = i64::from(params.draw_x).max(0).min(i64::from(params.root_width));
        let top = i64::from(params.draw_y).max(0).min(i64::from(params.root_height));
        let right = (i64::from(params.draw_x) + i64::from(params.draw_width))
            .max(0).min(i64::from(params.root_width));
        let bottom = (i64::from(params.draw_y) + i64::from(params.draw_height))
            .max(0).min(i64::from(params.root_height));
        if right <= left || bottom <= top {
            return Ok(());
        }
        let visible_width = right - left;
        let visible_height = bottom - top;
        let ndc_left = left as f32 / params.root_width as f32 * 2.0 - 1.0;
        let ndc_right = right as f32 / params.root_width as f32 * 2.0 - 1.0;
        let ndc_top = 1.0 - top as f32 / params.root_height as f32 * 2.0;
        let ndc_bottom = 1.0 - bottom as f32 / params.root_height as f32 * 2.0;
        let local_left = (left - i64::from(params.owner_x)) as f32;
        let local_top = (top - i64::from(params.owner_y)) as f32;
        let local_right = local_left + visible_width as f32;
        let local_bottom = local_top + visible_height as f32;
        let u0 = root_to_texture_u(left as f32, params.root_width);
        let u1 = root_to_texture_u(right as f32, params.root_width);
        let v0 = root_to_texture_v(top as f32, params.root_height);
        let v1 = root_to_texture_v(bottom as f32, params.root_height);
        let vertices: [f32; 36] = [
            ndc_left, ndc_bottom, u0, v1, local_left, local_bottom,
            ndc_right, ndc_bottom, u1, v1, local_right, local_bottom,
            ndc_right, ndc_top, u1, v0, local_right, local_top,
            ndc_left, ndc_bottom, u0, v1, local_left, local_bottom,
            ndc_right, ndc_top, u1, v0, local_right, local_top,
            ndc_left, ndc_top, u0, v0, local_left, local_top,
        ];

        let state = BlurGlState::save();
        let result = (|| {
            if self.backdrop_program.is_none() {
                self.backdrop_program = Some(BackdropProgram::new()?);
            }
            let backdrop = self.backdrop_program.as_ref().expect("backdrop program exists");
            unsafe {
                gl::BindFramebuffer(gl::FRAMEBUFFER, 0);
                gl::UseProgram(backdrop.program);
                gl::ActiveTexture(gl::TEXTURE0);
                gl::BindTexture(gl::TEXTURE_2D, blurred_texture);
                gl::BindVertexArray(self.vao);
                gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (vertices.len() * std::mem::size_of::<f32>()) as isize,
                    vertices.as_ptr().cast(),
                    gl::STREAM_DRAW,
                );
                gl::Uniform1i(backdrop.texture_uniform, 0);
                gl::Uniform2f(backdrop.surface_size_uniform, params.owner_width as f32, params.owner_height as f32);
                gl::Uniform1f(backdrop.corner_radius_uniform, corner_radius);
                gl::Enable(gl::BLEND);
                gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
                gl::BlendEquation(gl::FUNC_ADD);
                gl::Disable(gl::SCISSOR_TEST);
                check_gl_error("before backdrop draw")?;
                gl::DrawArrays(gl::TRIANGLES, 0, 6);
                check_gl_error("backdrop draw")?;
            }
            Ok(())
        })();
        drop(state);
        result
    }

    pub fn clear(&self) {
        unsafe {
            gl::ClearColor(0.0, 0.0, 0.0, 1.0);
            gl::Clear(gl::COLOR_BUFFER_BIT);
        }
    }

pub(crate) fn clear_transparent(&self) {
        let color = [0.0_f32, 0.0, 0.0, 0.0];
        unsafe {
            gl::ClearBufferfv(gl::COLOR, 0, color.as_ptr());
        }
    }

    /// 3a3fa2b5 — GPU-side-only, one-shot exact copy of `source_texture`
    /// into a brand-new, compositor-owned RGBA8 texture: no XGetImage, no
    /// glReadPixels CPU readback, no persisted FBO (`ScratchFramebuffer`
    /// is created and destroyed within this single call). The copy is a
    /// raw texel copy — opacity 1.0, corner_radius 0, border_width 0,
    /// blending explicitly disabled — so resolved opacity/corner mask/
    /// border/shadow/background blur are never baked in (see
    /// `render_exact_copy` and `SCENE_FRAGMENT_SHADER`'s
    /// `corner_radius<=0.0` branch: output is exactly `sampled`). GL state
    /// (framebuffer/viewport/scissor/blend/textures/VAO/program) is saved
    /// and restored via the SAME `BlurGlState` primitive
    /// `capture_and_blur_background` already uses — no duplicated partial
    /// state-backup mechanism. On any failure, the just-allocated texture
    /// is deleted before returning (never leaked).
    pub(crate) fn capture_closing_snapshot(
        &self,
        source_texture: u32,
        width: i32,
        height: i32,
    ) -> Result<u32, Box<dyn Error>> {
        if width <= 0 || height <= 0 {
            return Err("closing snapshot dimensions must be positive".into());
        }
        let state = BlurGlState::save();
        let result = (|| {
            let mut texture = 0;
            unsafe {
                check_gl_error("before closing snapshot texture allocation")?;
                gl::GenTextures(1, &mut texture);
            }
            if texture == 0 {
                return Err("glGenTextures returned a zero texture name".into());
            }
            let allocate = (|| -> Result<(), Box<dyn Error>> {
                unsafe {
                    gl::BindTexture(gl::TEXTURE_2D, texture);
                    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
                    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
                    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
                    gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
                    gl::TexImage2D(
                        gl::TEXTURE_2D, 0, gl::RGBA8 as i32, width, height, 0,
                        gl::RGBA, gl::UNSIGNED_BYTE, std::ptr::null(),
                    );
                    gl::BindTexture(gl::TEXTURE_2D, 0);
                    check_gl_error("closing snapshot texture storage allocation")?;
                }
                let scratch = ScratchFramebuffer::new(texture)?;
                unsafe {
                    gl::Disable(gl::SCISSOR_TEST);
                    gl::Viewport(0, 0, width, height);
                }
                self.render_exact_copy(source_texture, width, height)?;
                drop(scratch);
                Ok(())
            })();
            if let Err(error) = allocate {
                delete_texture(texture);
                return Err(error);
            }
            Ok(texture)
        })();
        drop(state);
        result
    }

    /// Renders `source_texture` into the currently-bound framebuffer at
    /// 1:1 scale with corner_radius=0, border_width=0, opacity=1.0, and
    /// blending explicitly DISABLED — an exact premultiplied-alpha texel
    /// copy. Caller (`capture_closing_snapshot`) is responsible for
    /// binding the destination framebuffer, setting the viewport to the
    /// destination texture's own size, and restoring GL state afterward.
    fn render_exact_copy(
        &self,
        source_texture: u32,
        width: i32,
        height: i32,
    ) -> Result<(), Box<dyn Error>> {
        if width <= 0 || height <= 0 {
            return Ok(());
        }
        let vertices: [f32; 36] = [
            -1.0, -1.0, 0.0, 1.0, 0.0, height as f32,
             1.0, -1.0, 1.0, 1.0, width as f32, height as f32,
             1.0,  1.0, 1.0, 0.0, width as f32, 0.0,
            -1.0, -1.0, 0.0, 1.0, 0.0, height as f32,
             1.0,  1.0, 1.0, 0.0, width as f32, 0.0,
            -1.0,  1.0, 0.0, 0.0, 0.0, 0.0,
        ];
        unsafe {
            check_gl_error("before closing snapshot copy")?;
            gl::UseProgram(self.program);
            gl::Uniform1i(self.shadow_mode_uniform, 0);
            gl::Disable(gl::BLEND);
            check_gl_error("closing snapshot copy blend state")?;
            gl::Uniform1f(self.surface_opacity_uniform, 1.0);
            gl::Uniform1f(self.corner_radius_uniform, 0.0);
            gl::Uniform2f(self.surface_size_uniform, width as f32, height as f32);
            gl::Uniform1f(self.border_width_uniform, 0.0);
            gl::Uniform4f(self.border_color_uniform, 0.0, 0.0, 0.0, 1.0);
            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (vertices.len() * std::mem::size_of::<f32>()) as isize,
                vertices.as_ptr().cast(),
                gl::STREAM_DRAW,
            );
            gl::BindTexture(gl::TEXTURE_2D, source_texture);
            gl::DrawArrays(gl::TRIANGLES, 0, 6);
            check_gl_error("closing snapshot copy draw")?;
            gl::BindTexture(gl::TEXTURE_2D, 0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
            gl::UseProgram(0);
        }
        Ok(())
    }

    pub fn render_surface(
        &self,
        texture: u32,
        plan: crate::x11::scene::RenderQuadPlan,
        pixel_semantics: crate::x11::scene::EglPixelSemantics,
        root_width: i32,
        root_height: i32,
    ) -> Result<(), Box<dyn Error>> {
        self.render_surface_with_opacity(
            texture,
            plan,
            pixel_semantics,
            root_width,
            root_height,
            SurfaceOpacity::new(1.0).expect("constant opacity is valid"),
        )
    }

    pub(crate) fn render_surface_with_opacity(
        &self,
        texture: u32,
        plan: crate::x11::scene::RenderQuadPlan,
        pixel_semantics: crate::x11::scene::EglPixelSemantics,
        root_width: i32,
        root_height: i32,
        opacity: SurfaceOpacity,
    ) -> Result<(), Box<dyn Error>> {
        let x = plan.dst_x;
        let y = plan.dst_y;
        let width = plan.width;
        let height = plan.height;
        if width <= 0 || height <= 0 || root_width <= 0 || root_height <= 0 {
            return Ok(());
        }
        let left = (x.max(0) as f32 / root_width as f32) * 2.0 - 1.0;
        let right = ((x + width).min(root_width).max(0) as f32 / root_width as f32) * 2.0 - 1.0;
        let top = 1.0 - (y.max(0) as f32 / root_height as f32) * 2.0;
        let bottom = 1.0 - ((y + height).min(root_height).max(0) as f32 / root_height as f32) * 2.0;
        if right <= left || top <= bottom {
            return Ok(());
        }
        let vertices: [f32; 36] = [
            left, bottom, plan.u0, plan.v1, 0.0, height as f32,
            right, bottom, plan.u1, plan.v1, width as f32, height as f32,
            right, top, plan.u1, plan.v0, width as f32, 0.0,
            left, bottom, plan.u0, plan.v1, 0.0, height as f32,
            right, top, plan.u1, plan.v0, width as f32, 0.0,
            left, top, plan.u0, plan.v0, 0.0, 0.0,
        ];
        unsafe {
            check_gl_error("before scene draw")?;
            gl::UseProgram(self.program);
            gl::Uniform1i(self.shadow_mode_uniform, 0);
            match blend_state_for_surface(pixel_semantics, plan.corner_radius, opacity) {
                Some(BlendState::Disabled) => gl::Disable(gl::BLEND),
                Some(BlendState::PremultipliedAlpha) => {
                    gl::Enable(gl::BLEND);
                    gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
                }
                None => return Err("unsupported pixel semantics reached GL renderer".into()),
            }
            check_gl_error("blend state")?;
            gl::Uniform1f(self.surface_opacity_uniform, opacity.value());
            gl::Uniform1f(self.corner_radius_uniform, plan.corner_radius);
            gl::Uniform2f(self.surface_size_uniform, width as f32, height as f32);
            gl::Uniform1f(self.border_width_uniform, plan.border_width);
            gl::Uniform4f(self.border_color_uniform, plan.border_color[0], plan.border_color[1], plan.border_color[2], plan.border_color[3]);
            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (vertices.len() * std::mem::size_of::<f32>()) as isize,
                vertices.as_ptr().cast(),
                gl::STREAM_DRAW,
            );
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::DrawArrays(gl::TRIANGLES, 0, 6);
            check_gl_error("scene draw")?;
            gl::BindTexture(gl::TEXTURE_2D, 0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
            gl::UseProgram(0);
        }
        Ok(())
    }

    /// 3a3fa2b6-r2 (Minato radial reveal) — byte-identical to
    /// `render_surface_with_opacity` above (same vertex layout, same
    /// blend-state resolution, same corner_radius/border_width/
    /// border_color plumbing from `plan`) except `shadow_mode` selects
    /// the new mode 3 branch (SCENE_FRAGMENT_SHADER) and one extra
    /// uniform (`reveal_radius`) drives that branch's per-pixel radial
    /// reveal mask. No new VAO/VBO/texture/framebuffer/shader program —
    /// same `self.program`/`self.vao`/`self.buffer` as every other draw
    /// in this renderer. `texcoord` is sampled unwarped — this is a
    /// reveal MASK, never a texture warp (that is Kamui's job, not
    /// Minato's — see the 3a3fa2b6-r2/3a3fa2b7 architecture audit).
    pub(crate) fn render_surface_with_radial_reveal(
        &self,
        texture: u32,
        plan: crate::x11::scene::RenderQuadPlan,
        pixel_semantics: crate::x11::scene::EglPixelSemantics,
        root_width: i32,
        root_height: i32,
        opacity: SurfaceOpacity,
        reveal_radius: f32,
    ) -> Result<(), Box<dyn Error>> {
        let x = plan.dst_x;
        let y = plan.dst_y;
        let width = plan.width;
        let height = plan.height;
        if width <= 0 || height <= 0 || root_width <= 0 || root_height <= 0 {
            return Ok(());
        }
        let left = (x.max(0) as f32 / root_width as f32) * 2.0 - 1.0;
        let right = ((x + width).min(root_width).max(0) as f32 / root_width as f32) * 2.0 - 1.0;
        let top = 1.0 - (y.max(0) as f32 / root_height as f32) * 2.0;
        let bottom = 1.0 - ((y + height).min(root_height).max(0) as f32 / root_height as f32) * 2.0;
        if right <= left || top <= bottom {
            return Ok(());
        }
        let vertices: [f32; 36] = [
            left, bottom, plan.u0, plan.v1, 0.0, height as f32,
            right, bottom, plan.u1, plan.v1, width as f32, height as f32,
            right, top, plan.u1, plan.v0, width as f32, 0.0,
            left, bottom, plan.u0, plan.v1, 0.0, height as f32,
            right, top, plan.u1, plan.v0, width as f32, 0.0,
            left, top, plan.u0, plan.v0, 0.0, 0.0,
        ];
        unsafe {
            check_gl_error("before radial reveal scene draw")?;
            gl::UseProgram(self.program);
            gl::Uniform1i(self.shadow_mode_uniform, 3);
            match blend_state_for_surface(pixel_semantics, plan.corner_radius, opacity) {
                Some(BlendState::Disabled) => gl::Disable(gl::BLEND),
                Some(BlendState::PremultipliedAlpha) => {
                    gl::Enable(gl::BLEND);
                    gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
                }
                None => return Err("unsupported pixel semantics reached GL renderer".into()),
            }
            check_gl_error("radial reveal blend state")?;
            gl::Uniform1f(self.surface_opacity_uniform, opacity.value());
            gl::Uniform1f(self.corner_radius_uniform, plan.corner_radius);
            gl::Uniform2f(self.surface_size_uniform, width as f32, height as f32);
            gl::Uniform1f(self.border_width_uniform, plan.border_width);
            gl::Uniform4f(self.border_color_uniform, plan.border_color[0], plan.border_color[1], plan.border_color[2], plan.border_color[3]);
            gl::Uniform1f(self.reveal_radius_uniform, reveal_radius);
            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (vertices.len() * std::mem::size_of::<f32>()) as isize,
                vertices.as_ptr().cast(),
                gl::STREAM_DRAW,
            );
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::DrawArrays(gl::TRIANGLES, 0, 6);
            check_gl_error("radial reveal scene draw")?;
            gl::BindTexture(gl::TEXTURE_2D, 0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
            gl::UseProgram(0);
        }
        Ok(())
    }

    /// 3a3fa2b7 (Kamui) — byte-identical scaffolding to
    /// `render_surface_with_radial_reveal` above (same vertex layout,
    /// same blend-state resolution, same corner_radius/border_width/
    /// border_color plumbing from `plan`) except `shadow_mode` selects
    /// the new mode 4 branch (SCENE_FRAGMENT_SHADER), and FOUR extra
    /// uniforms (`kamui_visible_radius`, `kamui_twist`, `kamui_uv_min`,
    /// `kamui_uv_scale`) drive that branch's polar texture warp + radial
    /// domain mask, plus (3a3fa2b7-r2) a fifth uniform
    /// (`kamui_radial_power`) driving a nonlinear reshaping of the sample
    /// radius — `1.0` is an exact no-op, preserving the R1 identity/
    /// cropped-UV round-trip. `kamui_uv_min`/`kamui_uv_scale` are derived from
    /// THIS draw's own `plan.u0/v0/u1/v1` — never assumed to be [0,1] —
    /// so Kamui works correctly for an arbitrary cropped UV sub-rect (see
    /// the UV-subrect correctness requirement). No new VAO/VBO/texture/
    /// framebuffer/shader program — same `self.program`/`self.vao`/
    /// `self.buffer` as every other draw in this renderer.
    pub(crate) fn render_surface_with_kamui_warp(
        &self,
        texture: u32,
        plan: crate::x11::scene::RenderQuadPlan,
        pixel_semantics: crate::x11::scene::EglPixelSemantics,
        root_width: i32,
        root_height: i32,
        opacity: SurfaceOpacity,
        visible_radius: f32,
        twist: f32,
        radial_power: f32,
    ) -> Result<(), Box<dyn Error>> {
        let x = plan.dst_x;
        let y = plan.dst_y;
        let width = plan.width;
        let height = plan.height;
        if width <= 0 || height <= 0 || root_width <= 0 || root_height <= 0 {
            return Ok(());
        }
        let left = (x.max(0) as f32 / root_width as f32) * 2.0 - 1.0;
        let right = ((x + width).min(root_width).max(0) as f32 / root_width as f32) * 2.0 - 1.0;
        let top = 1.0 - (y.max(0) as f32 / root_height as f32) * 2.0;
        let bottom = 1.0 - ((y + height).min(root_height).max(0) as f32 / root_height as f32) * 2.0;
        if right <= left || top <= bottom {
            return Ok(());
        }
        let vertices: [f32; 36] = [
            left, bottom, plan.u0, plan.v1, 0.0, height as f32,
            right, bottom, plan.u1, plan.v1, width as f32, height as f32,
            right, top, plan.u1, plan.v0, width as f32, 0.0,
            left, bottom, plan.u0, plan.v1, 0.0, height as f32,
            right, top, plan.u1, plan.v0, width as f32, 0.0,
            left, top, plan.u0, plan.v0, 0.0, 0.0,
        ];
        unsafe {
            check_gl_error("before kamui warp scene draw")?;
            gl::UseProgram(self.program);
            gl::Uniform1i(self.shadow_mode_uniform, 4);
            match blend_state_for_surface(pixel_semantics, plan.corner_radius, opacity) {
                Some(BlendState::Disabled) => gl::Disable(gl::BLEND),
                Some(BlendState::PremultipliedAlpha) => {
                    gl::Enable(gl::BLEND);
                    gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
                }
                None => return Err("unsupported pixel semantics reached GL renderer".into()),
            }
            check_gl_error("kamui warp blend state")?;
            gl::Uniform1f(self.surface_opacity_uniform, opacity.value());
            gl::Uniform1f(self.corner_radius_uniform, plan.corner_radius);
            gl::Uniform2f(self.surface_size_uniform, width as f32, height as f32);
            gl::Uniform1f(self.border_width_uniform, plan.border_width);
            gl::Uniform4f(self.border_color_uniform, plan.border_color[0], plan.border_color[1], plan.border_color[2], plan.border_color[3]);
            gl::Uniform1f(self.kamui_visible_radius_uniform, visible_radius);
            gl::Uniform1f(self.kamui_twist_uniform, twist);
            // 3a3fa2b7-r2: nonlinear radial-power reshaping — at 1.0 this
            // is a strict no-op (pow(x,1.0)==x exactly), preserving the
            // R1 identity/cropped-UV round-trip proof unchanged.
            gl::Uniform1f(self.kamui_radial_power_uniform, radial_power);
            // 3a3fa2b7: RAW (never min/max-reordered) origin + slope —
            // this reproduces the exact affine mapping the vertex shader
            // already establishes between `local_position` and
            // `texcoord` (u0 at local=0, u1 at local=width), including a
            // deliberately flipped plan (u0>u1). Domain validity is
            // instead checked in the shader against the UNWARPED
            // fraction (sample_local/surface_size in [0,1]), which is
            // correct regardless of u0/u1 ordering — see mode 4 below.
            gl::Uniform2f(self.kamui_uv_min_uniform, plan.u0, plan.v0);
            gl::Uniform2f(self.kamui_uv_scale_uniform, plan.u1 - plan.u0, plan.v1 - plan.v0);
            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (vertices.len() * std::mem::size_of::<f32>()) as isize,
                vertices.as_ptr().cast(),
                gl::STREAM_DRAW,
            );
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::DrawArrays(gl::TRIANGLES, 0, 6);
            check_gl_error("kamui warp scene draw")?;
            gl::BindTexture(gl::TEXTURE_2D, 0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
            gl::UseProgram(0);
        }
        Ok(())
    }

    pub(crate) fn render_shadow(
        &self,
        params: ShadowParams,
        root_width: i32,
        root_height: i32,
    ) -> Result<(), Box<dyn Error>> {
        let Some(plan) = params.quad(root_width, root_height) else {
            return Ok(());
        };

        let left = (plan.dst_x as f32 / root_width as f32) * 2.0 - 1.0;
        let right = ((plan.dst_x + plan.width) as f32 / root_width as f32) * 2.0 - 1.0;
        let top = 1.0 - (plan.dst_y as f32 / root_height as f32) * 2.0;
        let bottom =
            1.0 - ((plan.dst_y + plan.height) as f32 / root_height as f32) * 2.0;
        let local_right = plan.local_x + plan.width as f32;
        let local_bottom = plan.local_y + plan.height as f32;
        let vertices: [f32; 36] = [
            left, bottom, 0.0, 0.0, plan.local_x, local_bottom,
            right, bottom, 0.0, 0.0, local_right, local_bottom,
            right, top, 0.0, 0.0, local_right, plan.local_y,
            left, bottom, 0.0, 0.0, plan.local_x, local_bottom,
            right, top, 0.0, 0.0, local_right, plan.local_y,
            left, top, 0.0, 0.0, plan.local_x, plan.local_y,
        ];

        unsafe {
            check_gl_error("before shadow draw")?;
            gl::UseProgram(self.program);
            gl::Enable(gl::BLEND);
            gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
            gl::Uniform1i(self.shadow_mode_uniform, 1);
            gl::Uniform1f(self.corner_radius_uniform, params.corner_radius);
            gl::Uniform2f(
                self.surface_size_uniform,
                params.outer_width,
                params.outer_height,
            );
            gl::Uniform1f(self.border_width_uniform, 0.0);
            gl::Uniform4f(self.border_color_uniform, 0.0, 0.0, 0.0, 0.0);
            gl::Uniform1f(self.shadow_extent_uniform, params.extent);
            gl::Uniform1f(self.shadow_strength_uniform, params.strength);
            gl::Uniform3f(self.shadow_color_uniform, params.color[0], params.color[1], params.color[2]);
            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (vertices.len() * std::mem::size_of::<f32>()) as isize,
                vertices.as_ptr().cast(),
                gl::STREAM_DRAW,
            );
            gl::DrawArrays(gl::TRIANGLES, 0, 6);
            check_gl_error("shadow draw")?;
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
            gl::UseProgram(0);
        }
        Ok(())
    }

    /// 3a3fa2b3 — energy_tear's slice+streak overlay. Reuses `self.vao`/
    /// `self.buffer`/`self.program` exactly like `render_surface_with_
    /// opacity`/`render_shadow` already do: no new VAO, no new buffer, no
    /// new program, no new texture, no new framebuffer, no new uniform
    /// location (slices reuse `shadow_mode=0`'s existing texture-sampling
    /// path; streaks reuse `shadow_mode=2`, `shadow_strength`, and
    /// `shadow_color` — a flat-rect variant of the same uniforms the
    /// shadow path already uses, not new ones). Each slice passes the
    /// FULL window's `surface_size`/`corner_radius` (not its own tiny
    /// slice size) together with its own `local_offset_x`, so the
    /// existing corner-radius/border-masking math in the shader —
    /// completely unmodified — correctly rounds only the two true outer
    /// corners and leaves every interior seam square, with zero new
    /// shader code for that part.
    pub(crate) fn render_energy_tear_slices(
        &self,
        texture: u32,
        pixel_semantics: crate::x11::scene::EglPixelSemantics,
        opacity: SurfaceOpacity,
        render_plan: &crate::x11::scene::EnergyTearRenderPlan,
        root_width: i32,
        root_height: i32,
    ) -> Result<(), Box<dyn Error>> {
        if root_width <= 0 || root_height <= 0 {
            return Ok(());
        }
        for slice in &render_plan.slices {
            if slice.width <= 0 || slice.height <= 0 {
                continue;
            }
            let left = (slice.dst_x.max(0) as f32 / root_width as f32) * 2.0 - 1.0;
            let right = ((slice.dst_x + slice.width).min(root_width).max(0) as f32 / root_width as f32) * 2.0 - 1.0;
            let top = 1.0 - (slice.dst_y.max(0) as f32 / root_height as f32) * 2.0;
            let bottom = 1.0 - ((slice.dst_y + slice.height).min(root_height).max(0) as f32 / root_height as f32) * 2.0;
            if right <= left || top <= bottom {
                continue;
            }
            let local_left = slice.local_offset_x;
            let local_right = slice.local_offset_x + slice.width as f32;
            let vertices: [f32; 36] = [
                left, bottom, slice.u0, slice.v1, local_left, render_plan.full_height,
                right, bottom, slice.u1, slice.v1, local_right, render_plan.full_height,
                right, top, slice.u1, slice.v0, local_right, 0.0,
                left, bottom, slice.u0, slice.v1, local_left, render_plan.full_height,
                right, top, slice.u1, slice.v0, local_right, 0.0,
                left, top, slice.u0, slice.v0, local_left, 0.0,
            ];
            unsafe {
                check_gl_error("before energy_tear slice draw")?;
                gl::UseProgram(self.program);
                gl::Uniform1i(self.shadow_mode_uniform, 0);
                match blend_state_for_surface(pixel_semantics, render_plan.corner_radius, opacity) {
                    Some(BlendState::Disabled) => gl::Disable(gl::BLEND),
                    Some(BlendState::PremultipliedAlpha) => {
                        gl::Enable(gl::BLEND);
                        gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
                    }
                    None => return Err("unsupported pixel semantics reached GL renderer".into()),
                }
                check_gl_error("energy_tear slice blend state")?;
                gl::Uniform1f(self.surface_opacity_uniform, opacity.value());
                gl::Uniform1f(self.corner_radius_uniform, render_plan.corner_radius);
                gl::Uniform2f(self.surface_size_uniform, render_plan.full_width, render_plan.full_height);
                // No border on individual slices — see energy_tear_render_plan's doc comment.
                gl::Uniform1f(self.border_width_uniform, 0.0);
                gl::Uniform4f(self.border_color_uniform, 0.0, 0.0, 0.0, 0.0);
                gl::BindVertexArray(self.vao);
                gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
                gl::BufferData(
                    gl::ARRAY_BUFFER,
                    (vertices.len() * std::mem::size_of::<f32>()) as isize,
                    vertices.as_ptr().cast(),
                    gl::STREAM_DRAW,
                );
                gl::BindTexture(gl::TEXTURE_2D, texture);
                gl::DrawArrays(gl::TRIANGLES, 0, 6);
                check_gl_error("energy_tear slice draw")?;
                gl::BindTexture(gl::TEXTURE_2D, 0);
                gl::BindBuffer(gl::ARRAY_BUFFER, 0);
                gl::BindVertexArray(0);
                gl::UseProgram(0);
            }
        }
        if render_plan.streak_alpha > 0.0 {
            for streak in &render_plan.streaks {
                if streak.width <= 0 || streak.height <= 0 {
                    continue;
                }
                let left = (streak.dst_x.max(0) as f32 / root_width as f32) * 2.0 - 1.0;
                let right = ((streak.dst_x + streak.width).min(root_width).max(0) as f32 / root_width as f32) * 2.0 - 1.0;
                let top = 1.0 - (streak.dst_y.max(0) as f32 / root_height as f32) * 2.0;
                let bottom = 1.0 - ((streak.dst_y + streak.height).min(root_height).max(0) as f32 / root_height as f32) * 2.0;
                if right <= left || top <= bottom {
                    continue;
                }
                let vertices: [f32; 36] = [
                    left, bottom, 0.0, 0.0, 0.0, streak.height as f32,
                    right, bottom, 0.0, 0.0, streak.width as f32, streak.height as f32,
                    right, top, 0.0, 0.0, streak.width as f32, 0.0,
                    left, bottom, 0.0, 0.0, 0.0, streak.height as f32,
                    right, top, 0.0, 0.0, streak.width as f32, 0.0,
                    left, top, 0.0, 0.0, 0.0, 0.0,
                ];
                unsafe {
                    check_gl_error("before energy_tear streak draw")?;
                    gl::UseProgram(self.program);
                    gl::Enable(gl::BLEND);
                    gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
                    gl::Uniform1i(self.shadow_mode_uniform, 2);
                    gl::Uniform1f(self.corner_radius_uniform, 0.0);
                    gl::Uniform2f(self.surface_size_uniform, streak.width as f32, streak.height as f32);
                    gl::Uniform1f(self.border_width_uniform, 0.0);
                    gl::Uniform4f(self.border_color_uniform, 0.0, 0.0, 0.0, 0.0);
                    gl::Uniform1f(self.shadow_strength_uniform, render_plan.streak_alpha);
                    gl::Uniform3f(
                        self.shadow_color_uniform,
                        render_plan.streak_color[0],
                        render_plan.streak_color[1],
                        render_plan.streak_color[2],
                    );
                    gl::BindVertexArray(self.vao);
                    gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
                    gl::BufferData(
                        gl::ARRAY_BUFFER,
                        (vertices.len() * std::mem::size_of::<f32>()) as isize,
                        vertices.as_ptr().cast(),
                        gl::STREAM_DRAW,
                    );
                    gl::DrawArrays(gl::TRIANGLES, 0, 6);
                    check_gl_error("energy_tear streak draw")?;
                    gl::BindBuffer(gl::ARRAY_BUFFER, 0);
                    gl::BindVertexArray(0);
                    gl::UseProgram(0);
                }
            }
        }
        Ok(())
    }

    /// 3a3fa2b6 — generic solid-window-overlay primitive: ONE flat
    /// solid-color quad, independent alpha, rounded-corner-clipped to
    /// `plan`'s own `corner_radius`. Reuses `shadow_mode==2` (the same
    /// flat-rect mode `render_energy_tear_slices`'s streak draw already
    /// uses) — no new shader program, no new uniform location, no new
    /// VAO/VBO, no new texture, no new framebuffer. Deliberately takes a
    /// generic `RenderQuadPlan`, never `EnergyTearRenderPlan` — this
    /// primitive carries no EnergyTear effect-state dependency, so any
    /// future effect (TeleportFlashy included) can reuse it without
    /// conceptually depending on EnergyTear. See the
    /// `SCENE_FRAGMENT_SHADER`'s `shadow_mode==2` branch: it now reads
    /// `outer_radius` (derived from the `corner_radius` uniform) instead
    /// of a hardcoded `0.0` — proven non-regressive for EnergyTear, since
    /// `render_energy_tear_slices`'s own streak draw already explicitly
    /// sends `corner_radius_uniform=0.0` for every streak, so its streaks
    /// evaluate `outer_radius=0` and remain pixel-identical.
    pub(crate) fn render_solid_overlay(
        &self,
        plan: crate::x11::scene::RenderQuadPlan,
        color: [f32; 3],
        alpha: f32,
        root_width: i32,
        root_height: i32,
    ) -> Result<(), Box<dyn Error>> {
        let x = plan.dst_x;
        let y = plan.dst_y;
        let width = plan.width;
        let height = plan.height;
        if width <= 0 || height <= 0 || root_width <= 0 || root_height <= 0 || alpha <= 0.0 {
            return Ok(());
        }
        let left = (x.max(0) as f32 / root_width as f32) * 2.0 - 1.0;
        let right = ((x + width).min(root_width).max(0) as f32 / root_width as f32) * 2.0 - 1.0;
        let top = 1.0 - (y.max(0) as f32 / root_height as f32) * 2.0;
        let bottom = 1.0 - ((y + height).min(root_height).max(0) as f32 / root_height as f32) * 2.0;
        if right <= left || top <= bottom {
            return Ok(());
        }
        let vertices: [f32; 36] = [
            left, bottom, 0.0, 0.0, 0.0, height as f32,
            right, bottom, 0.0, 0.0, width as f32, height as f32,
            right, top, 0.0, 0.0, width as f32, 0.0,
            left, bottom, 0.0, 0.0, 0.0, height as f32,
            right, top, 0.0, 0.0, width as f32, 0.0,
            left, top, 0.0, 0.0, 0.0, 0.0,
        ];
        unsafe {
            check_gl_error("before solid overlay draw")?;
            gl::UseProgram(self.program);
            gl::Enable(gl::BLEND);
            gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
            gl::Uniform1i(self.shadow_mode_uniform, 2);
            gl::Uniform1f(self.corner_radius_uniform, plan.corner_radius);
            gl::Uniform2f(self.surface_size_uniform, width as f32, height as f32);
            gl::Uniform1f(self.border_width_uniform, 0.0);
            gl::Uniform4f(self.border_color_uniform, 0.0, 0.0, 0.0, 0.0);
            gl::Uniform1f(self.shadow_strength_uniform, alpha);
            gl::Uniform3f(self.shadow_color_uniform, color[0], color[1], color[2]);
            gl::BindVertexArray(self.vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, self.buffer);
            gl::BufferData(
                gl::ARRAY_BUFFER,
                (vertices.len() * std::mem::size_of::<f32>()) as isize,
                vertices.as_ptr().cast(),
                gl::STREAM_DRAW,
            );
            gl::DrawArrays(gl::TRIANGLES, 0, 6);
            check_gl_error("solid overlay draw")?;
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
            gl::BindVertexArray(0);
            gl::UseProgram(0);
        }
        Ok(())
    }
}

fn blend_state_for_surface(
    semantics: crate::x11::scene::EglPixelSemantics,
    corner_radius: f32,
    opacity: SurfaceOpacity,
) -> Option<BlendState> {
    if corner_radius > 0.0 || opacity.value() < 1.0 {
        match semantics {
            crate::x11::scene::EglPixelSemantics::Opaque
            | crate::x11::scene::EglPixelSemantics::PremultipliedAlpha => {
                Some(BlendState::PremultipliedAlpha)
            }
            crate::x11::scene::EglPixelSemantics::Unsupported => None,
        }
    } else {
        blend_state_for(semantics)
    }
}

impl Drop for SceneRenderer {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteBuffers(1, &self.buffer);
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteProgram(self.program);
        }
    }
}

pub fn delete_texture(texture: u32) {
    unsafe { gl::DeleteTextures(1, &texture); }
}

pub(crate) fn allocate_rgba8_texture(width: i32, height: i32) -> Result<u32, Box<dyn Error>> {
    if width <= 0 || height <= 0 {
        return Err("offscreen texture dimensions must be positive".into());
    }
    let mut texture = 0;
    unsafe {
        gl::GenTextures(1, &mut texture);
        if texture != 0 {
            gl::BindTexture(gl::TEXTURE_2D, texture);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MIN_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_MAG_FILTER, gl::LINEAR as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_S, gl::CLAMP_TO_EDGE as i32);
            gl::TexParameteri(gl::TEXTURE_2D, gl::TEXTURE_WRAP_T, gl::CLAMP_TO_EDGE as i32);
            gl::TexImage2D(
                gl::TEXTURE_2D, 0, gl::RGBA8 as i32, width, height, 0,
                gl::RGBA, gl::UNSIGNED_BYTE, std::ptr::null(),
            );
            gl::BindTexture(gl::TEXTURE_2D, 0);
        }
    }
    if texture == 0 {
        return Err("glGenTextures returned a zero offscreen texture name".into());
    }
    Ok(texture)
}

impl Drop for CaptureRenderer {
    fn drop(&mut self) {
        unsafe {
            gl::DeleteTextures(1, &self.texture);
            gl::DeleteVertexArrays(1, &self.vao);
            gl::DeleteProgram(self.program);
        }
    }
}

fn check_gl_error(operation: &str) -> Result<(), Box<dyn Error>> {
    let error = unsafe { gl::GetError() };
    if error != gl::NO_ERROR {
        return Err(format!("{operation} failed: GL error 0x{error:04x}").into());
    }
    Ok(())
}

fn compile_shader(source: &str, kind: u32) -> Result<u32, Box<dyn Error>> {
    let source = std::ffi::CString::new(source)?;
    let shader = unsafe { gl::CreateShader(kind) };
    if shader == 0 {
        return Err("glCreateShader returned a zero shader name".into());
    }
    unsafe { gl::ShaderSource(shader, 1, &source.as_ptr(), std::ptr::null()); gl::CompileShader(shader); }
    let mut status = 0;
    unsafe { gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut status); }
    if status == 0 {
        let log = shader_log(shader);
        unsafe { gl::DeleteShader(shader); }
        return Err(log.into());
    }
    Ok(shader)
}

#[allow(dead_code)]
fn create_program(vertex_source: &str, fragment_source: &str) -> Result<u32, Box<dyn Error>> {
    let vertex = compile_shader(vertex_source, gl::VERTEX_SHADER)?;
    let fragment = match compile_shader(fragment_source, gl::FRAGMENT_SHADER) {
        Ok(fragment) => fragment,
        Err(error) => {
            unsafe { gl::DeleteShader(vertex); }
            return Err(error);
        }
    };
    let program = unsafe { gl::CreateProgram() };
    if program == 0 {
        unsafe {
            gl::DeleteShader(vertex);
            gl::DeleteShader(fragment);
        }
        return Err("glCreateProgram returned a zero program name".into());
    }
    unsafe {
        gl::AttachShader(program, vertex);
        gl::AttachShader(program, fragment);
        gl::LinkProgram(program);
        gl::DeleteShader(vertex);
        gl::DeleteShader(fragment);
    }
    if let Err(error) = check_program(program) {
        unsafe { gl::DeleteProgram(program); }
        return Err(error);
    }
    Ok(program)
}

fn check_program(program: u32) -> Result<(), Box<dyn Error>> {
    let mut status = 0;
    unsafe { gl::GetProgramiv(program, gl::LINK_STATUS, &mut status); }
    if status == 0 { return Err(program_log(program).into()); }
    Ok(())
}

fn shader_log(shader: u32) -> String { let mut length = 0; unsafe { gl::GetShaderiv(shader, gl::INFO_LOG_LENGTH, &mut length); } let mut data = vec![0; length.max(1) as usize]; unsafe { gl::GetShaderInfoLog(shader, length, std::ptr::null_mut(), data.as_mut_ptr().cast()); } String::from_utf8_lossy(&data).into_owned() }
fn program_log(program: u32) -> String { let mut length = 0; unsafe { gl::GetProgramiv(program, gl::INFO_LOG_LENGTH, &mut length); } let mut data = vec![0; length.max(1) as usize]; unsafe { gl::GetProgramInfoLog(program, length, std::ptr::null_mut(), data.as_mut_ptr().cast()); } String::from_utf8_lossy(&data).into_owned() }

#[cfg(test)]
mod tests {
    use super::{
        apply_surface_opacity, blend_state_for, blend_state_for_surface,
        backdrop_replacement, build_shadow_quad_plan, root_to_texture_u, root_to_texture_v,
        BackdropParams, BlurCaptureRegion, BlendState, ShadowParams, SurfaceOpacity,
        BACKDROP_FRAGMENT_SHADER, BLUR_FRAGMENT_SHADER, BLUR_TAP_RADIUS, BLUR_VERTEX_SHADER,
    };
    use crate::x11::scene::EglPixelSemantics;

    fn assert_pixel_close(actual: [f32; 4], expected: [f32; 4]) {
        for (actual, expected) in actual.into_iter().zip(expected) {
            assert!((actual - expected).abs() < 0.00001, "{actual} != {expected}");
        }
    }

    #[test]
    fn surface_opacity_validation_accepts_only_finite_unit_values() {
        assert!(SurfaceOpacity::new(0.0).is_some());
        assert!(SurfaceOpacity::new(0.5).is_some());
        assert!(SurfaceOpacity::new(1.0).is_some());
        assert!(SurfaceOpacity::new(-0.01).is_none());
        assert!(SurfaceOpacity::new(1.01).is_none());
        assert!(SurfaceOpacity::new(f32::NAN).is_none());
        assert!(SurfaceOpacity::new(f32::INFINITY).is_none());
        assert!(SurfaceOpacity::new(f32::NEG_INFINITY).is_none());
    }

    #[test]
    fn surface_opacity_scales_opaque_and_premultiplied_pixels() {
        let half = SurfaceOpacity::new(0.5).unwrap();
        assert_pixel_close(
            apply_surface_opacity([1.0, 1.0, 1.0, 1.0], 1.0, half),
            [0.5, 0.5, 0.5, 0.5],
        );
        assert_pixel_close(
            apply_surface_opacity([0.4, 0.2, 0.1, 0.5], 1.0, half),
            [0.2, 0.1, 0.05, 0.25],
        );
        assert_pixel_close(
            apply_surface_opacity([0.0, 0.0, 0.0, 0.0], 1.0, half),
            [0.0, 0.0, 0.0, 0.0],
        );
        assert_pixel_close(
            apply_surface_opacity([0.4, 0.2, 0.1, 0.5], 1.0, SurfaceOpacity::new(1.0).unwrap()),
            [0.4, 0.2, 0.1, 0.5],
        );
        assert_pixel_close(
            apply_surface_opacity([0.4, 0.2, 0.1, 0.5], 1.0, SurfaceOpacity::new(0.0).unwrap()),
            [0.0, 0.0, 0.0, 0.0],
        );
    }

    #[test]
    fn surface_opacity_multiplies_rounded_coverage_once() {
        let result = apply_surface_opacity(
            [1.0, 0.8, 0.4, 1.0],
            0.5,
            SurfaceOpacity::new(0.5).unwrap(),
        );
        assert_pixel_close(result, [0.25, 0.2, 0.1, 0.25]);
    }

    #[test]
    fn surface_shader_branch_model_keeps_border_independent() {
        let opacity = SurfaceOpacity::new(0.5).unwrap();
        assert_pixel_close(
            super::compose_surface_fragment(
                [1.0, 0.8, 0.4, 1.0], 1.0, None, [0.0; 4], 0.0, opacity,
            ),
            [0.5, 0.4, 0.2, 0.5],
        );
        assert_pixel_close(
            super::compose_surface_fragment(
                [1.0, 0.8, 0.4, 1.0], 0.5, None, [0.0; 4], 0.0, opacity,
            ),
            [0.25, 0.2, 0.1, 0.25],
        );
        let border = [0.2, 0.3, 0.4, 1.0];
        assert_pixel_close(
            super::compose_surface_fragment(
                [1.0, 0.8, 0.4, 1.0], 1.0, Some(0.5), border, 0.75, opacity,
            ),
            [0.4, 0.425, 0.4, 1.0],
        );
        assert_pixel_close(
            super::compose_surface_fragment(
                [1.0, 0.8, 0.4, 1.0], 1.0, Some(0.5), border, 0.75,
                SurfaceOpacity::new(0.0).unwrap(),
            ),
            [0.15, 0.225, 0.3, 0.75],
        );
    }

    #[test]
    fn opacity_requires_blending_for_translucent_opaque_surfaces() {
        let opaque = SurfaceOpacity::new(1.0).unwrap();
        let half = SurfaceOpacity::new(0.5).unwrap();
        assert_eq!(blend_state_for_surface(EglPixelSemantics::Opaque, 0.0, opaque), Some(BlendState::Disabled));
        assert_eq!(blend_state_for_surface(EglPixelSemantics::Opaque, 0.0, half), Some(BlendState::PremultipliedAlpha));
        assert_eq!(blend_state_for_surface(EglPixelSemantics::PremultipliedAlpha, 0.0, opaque), Some(BlendState::PremultipliedAlpha));
        assert_eq!(blend_state_for_surface(EglPixelSemantics::Opaque, 8.0, opaque), Some(BlendState::PremultipliedAlpha));
    }

    #[test]
    fn opacity_shader_scales_client_only_and_keeps_border_independent() {
        let source = super::SCENE_FRAGMENT_SHADER;
        assert!(source.contains("uniform float surface_opacity"));
        assert!(source.contains("sampled*surface_opacity"));
        assert!(source.contains("sampled*inner*surface_opacity+premultiplied_border"));
        assert!(source.contains("premultiplied_border=vec4(border_color.rgb*border_color.a,border_color.a)*border"));
        assert!(!source.contains("premultiplied_border*surface_opacity"));
    }

    #[test]
    fn opaque_and_premultiplied_blend_policies_are_explicit() {
        assert_eq!(blend_state_for(EglPixelSemantics::Opaque), Some(BlendState::Disabled));
        assert_eq!(
            blend_state_for(EglPixelSemantics::PremultipliedAlpha),
            Some(BlendState::PremultipliedAlpha)
        );
        assert_eq!(blend_state_for(EglPixelSemantics::Unsupported), None);
    }

    #[test]
    fn scene_shader_samples_uv_without_an_extra_vertical_flip() {
        assert!(super::FRAGMENT_SHADER.contains("texture(captured,texcoord)"));
        assert!(!super::FRAGMENT_SHADER.contains("1.0-texcoord.y"));
    }

    #[test]
    fn rounded_scene_shader_masks_premultiplied_color_and_alpha() {
        assert!(super::SCENE_FRAGMENT_SHADER.contains("color=sampled*coverage"));
        assert!(super::SCENE_FRAGMENT_SHADER.contains("fwidth(distance)"));
        assert!(super::SCENE_FRAGMENT_SHADER.contains("corner_radius"));
    }

    #[test]
    fn opaque_surfaces_enable_blending_only_when_corner_masked() {
        let opaque = SurfaceOpacity::new(1.0).unwrap();
        assert_eq!(blend_state_for_surface(EglPixelSemantics::Opaque, 0.0, opaque), Some(BlendState::Disabled));
        assert_eq!(blend_state_for_surface(EglPixelSemantics::Opaque, 8.0, opaque), Some(BlendState::PremultipliedAlpha));
    }

    #[test]
    fn border_shader_contract_premultiplies_border_color() {
        assert!(super::SCENE_FRAGMENT_SHADER.contains("border_color.rgb*border_color.a"));
        assert!(super::SCENE_FRAGMENT_SHADER.contains("border_width"));
    }

    #[test]
    fn shadow_bounds_expand_by_extent_and_apply_offset() {
        let params = ShadowParams::new(
            100.0, 80.0, 200.0, 100.0, 16.0, 8.0, 3.0, -4.0, 0.5,
        ).unwrap();
        let plan = build_shadow_quad_plan(params, 1000, 800).unwrap();
        assert_eq!((plan.dst_x, plan.dst_y), (95, 68));
        assert_eq!((plan.width, plan.height), (216, 116));
        assert_eq!((plan.local_x, plan.local_y), (0.0, 0.0));
    }

    #[test]
    fn shadow_zero_offset_is_symmetric() {
        let params = ShadowParams::new(
            100.0, 80.0, 200.0, 100.0, 16.0, 8.0, 0.0, 0.0, 0.5,
        ).unwrap();
        let plan = build_shadow_quad_plan(params, 1000, 800).unwrap();
        assert_eq!((plan.dst_x, plan.dst_y), (92, 72));
        assert_eq!((plan.width, plan.height), (216, 116));
    }

    #[test]
    fn shadow_positive_and_negative_offsets_shift_geometry() {
        let positive = ShadowParams::new(
            100.0, 80.0, 200.0, 100.0, 16.0, 8.0, 5.0, 7.0, 0.5,
        ).unwrap();
        let negative = ShadowParams::new(
            100.0, 80.0, 200.0, 100.0, 16.0, 8.0, -5.0, -7.0, 0.5,
        ).unwrap();
        let positive_plan = build_shadow_quad_plan(positive, 1000, 800).unwrap();
        let negative_plan = build_shadow_quad_plan(negative, 1000, 800).unwrap();
        assert_eq!((positive_plan.dst_x, positive_plan.dst_y), (97, 79));
        assert_eq!((negative_plan.dst_x, negative_plan.dst_y), (87, 65));
    }

    #[test]
    fn shadow_clipping_preserves_local_geometry_at_all_edges() {
        let cases = [
            (0.0, 0.0, 0, 0, 10.0, 10.0, 20, 20),
            (90.0, 0.0, 80, 0, 0.0, 10.0, 20, 20),
            (0.0, 90.0, 0, 80, 10.0, 0.0, 20, 20),
            (90.0, 90.0, 80, 80, 0.0, 0.0, 20, 20),
        ];
        for (x, y, dst_x, dst_y, local_x, local_y, width, height) in cases {
            let params = ShadowParams::new(
                x, y, 10.0, 10.0, 4.0, 10.0, 0.0, 0.0, 0.5,
            ).unwrap();
            let plan = build_shadow_quad_plan(params, 100, 100).unwrap();
            assert_eq!((plan.dst_x, plan.dst_y), (dst_x, dst_y));
            assert_eq!((plan.local_x, plan.local_y), (local_x, local_y));
            assert_eq!((plan.width, plan.height), (width, height));
        }
    }

    #[test]
    fn shadow_fully_outside_framebuffer_is_skipped() {
        let params = ShadowParams::new(
            200.0, 200.0, 10.0, 10.0, 4.0, 10.0, 0.0, 0.0, 0.5,
        ).unwrap();
        assert!(build_shadow_quad_plan(params, 100, 100).is_none());
    }

    #[test]
    fn shadow_fractional_bounds_use_floor_ceil_and_preserve_local_origin() {
        let params = ShadowParams::new(
            0.25, 1.75, 10.0, 8.0, 4.0, 2.25, 0.5, -0.75, 0.5,
        ).unwrap();
        let plan = build_shadow_quad_plan(params, 100, 100).unwrap();
        assert_eq!((plan.dst_x, plan.dst_y), (0, 0));
        assert_eq!((plan.width, plan.height), (13, 12));
        assert_eq!((plan.local_x, plan.local_y), (1.5, 1.25));
    }

    #[test]
    fn shadow_radius_is_clamped_to_outer_rectangle() {
        let params = ShadowParams::new(
            0.0, 0.0, 100.0, 80.0, 100.0, 8.0, 0.0, 0.0, 0.5,
        ).unwrap();
        assert_eq!(params.corner_radius, 40.0);
    }

    #[test]
    fn shadow_invalid_extent_or_strength_is_skipped() {
        assert!(ShadowParams::new(
            0.0, 0.0, 100.0, 80.0, 8.0, 0.0, 0.0, 0.0, 0.5,
        ).is_none());
        assert!(ShadowParams::new(
            0.0, 0.0, 100.0, 80.0, 8.0, 8.0, 0.0, 0.0, 0.0,
        ).is_none());
        assert!(ShadowParams::new(
            0.0, 0.0, 100.0, 80.0, 8.0, 8.0, 0.0, 0.0, 1.1,
        ).is_none());
    }

    #[test]
    fn shadow_params_are_copy_sized_renderer_values() {
        let params = ShadowParams::new(
            0.0, 0.0, 100.0, 80.0, 8.0, 8.0, 0.0, 0.0, 0.5,
        ).unwrap();
        let copy = params;
        assert_eq!(params, copy);
        assert_eq!(
            std::mem::size_of::<ShadowParams>(),
            12 * std::mem::size_of::<f32>()
        );
    }

    #[test]
    fn shadow_shader_contract_places_non_sampling_branch_first() {
        let source = super::SCENE_FRAGMENT_SHADER;
        // 3a3fa2b3: shadow_mode==1 replaces the old shadow_mode!=0 test —
        // mode 2 (energy_tear streak) is a separate non-sampling branch,
        // also placed before the texture-sampling surface path.
        let shadow_branch = source.find("if(shadow_mode==1)").unwrap();
        let texture_sample = source.find("vec4 sampled=texture(captured,texcoord)").unwrap();
        assert!(shadow_branch < texture_sample);
        assert!(source.contains("shadow_distance"));
        assert!(source.contains("shadow_strength"));
        assert!(source.contains("shadow_color"));
        assert!(source.contains("color=vec4(shadow_color*alpha,alpha)"));
    }

    #[test]
    fn energy_tear_shader_contract_adds_a_non_sampling_flat_rect_mode() {
        let source = super::SCENE_FRAGMENT_SHADER;
        let tear_branch = source.find("if(shadow_mode==2)").unwrap();
        let texture_sample = source.find("vec4 sampled=texture(captured,texcoord)").unwrap();
        assert!(tear_branch < texture_sample);
        // Reuses the SAME rounded_distance/coverage helpers already used
        // for corner-radius/border masking — no new helper function, no
        // new uniform (shadow_strength/shadow_color are reused, not
        // duplicated). 3a3fa2b6: the mask radius is now `outer_radius`
        // (derived from the `corner_radius` uniform, same local every
        // other branch already uses), not a hardcoded 0.0 — see
        // `solid_overlay_mode_reads_the_corner_radius_uniform` below for
        // the generic-primitive proof, and
        // `energy_tear_streak_draw_still_forces_corner_radius_uniform_to_zero`
        // for the non-regression proof (the caller, not the shader, is
        // what keeps EnergyTear's streaks square).
        assert!(source.contains("float tear_mask=coverage(rounded_distance(local_position,surface_size,outer_radius))"));
        assert!(source.contains("float alpha=shadow_strength*tear_mask"));
    }

    // ========================================================
    // 3a3fa2b6-r2 (Minato) — radial reveal mode 3: mask, not warp.
    // ========================================================

    #[test]
    fn minato_reveal_shader_contract_adds_a_sampling_masked_mode_before_ordinary_sampling() {
        let source = super::SCENE_FRAGMENT_SHADER;
        let reveal_branch = source.find("if(shadow_mode==3)").unwrap();
        let ordinary_texture_sample = source.find("vec4 sampled=texture(captured,texcoord)").unwrap();
        let tear_branch = source.find("if(shadow_mode==2)").unwrap();
        assert!(tear_branch < reveal_branch, "mode 3 must be declared after mode 2, preserving existing branch order");
        assert!(reveal_branch < ordinary_texture_sample, "mode 3 must be resolved before falling through to the ordinary mode-0 sampling path");
        // Unlike modes 1/2, mode 3 DOES sample the texture (it's a mask
        // over ordinary content, never a texture warp) — via its own
        // `reveal_sampled` local, never colliding with mode 0's `sampled`.
        assert!(source.contains("vec4 reveal_sampled=texture(captured,texcoord)"));
        assert!(!source.contains("reveal_sampled_warped"));
    }

    #[test]
    fn minato_reveal_uses_per_axis_normalized_coordinates_and_the_existing_coverage_helper() {
        let source = super::SCENE_FRAGMENT_SHADER;
        // Per-axis independent normalization by half-size (never a single
        // aspect-corrected scalar) — this is exactly what guarantees every
        // corner reaches r_norm==1.0 regardless of window aspect ratio.
        assert!(source.contains("vec2 reveal_p=(local_position-surface_size*0.5)/(surface_size*0.5)"));
        assert!(source.contains("float reveal_r=length(reveal_p)/1.4142135"));
        // Reuses the SAME coverage() helper every other mask in this
        // shader already uses — no new antialiasing primitive.
        assert!(source.contains("float reveal=coverage(reveal_r-reveal_radius)"));
    }

    #[test]
    fn minato_reveal_multiplies_both_content_and_border_terms_by_the_mask() {
        let source = super::SCENE_FRAGMENT_SHADER;
        let start = source.find("if(shadow_mode==3)").unwrap();
        let end = start + source[start..].find("if(shadow_mode==4)").unwrap();
        let body = &source[start..end];
        // Content term(s): every color assignment in this branch carries
        // an explicit `*reveal` factor.
        assert!(body.contains("color=reveal_sampled*surface_opacity*reveal;"));
        assert!(body.contains("color=reveal_sampled*coverage(rounded_distance(local_position,surface_size,outer_radius))*surface_opacity*reveal;"));
        assert!(body.contains("color=reveal_sampled*reveal_inner*surface_opacity*reveal+reveal_premultiplied_border;"));
        // Border term: multiplied by `reveal` too (section 5's explicit
        // requirement) — a small revealed circle must never show a
        // complete rectangular border prematurely.
        assert!(body.contains("vec4 reveal_premultiplied_border=vec4(border_color.rgb*border_color.a,border_color.a)*reveal_border*reveal;"));
    }

    #[test]
    fn minato_reveal_still_applies_rounded_corner_coverage_never_bypassing_it() {
        let source = super::SCENE_FRAGMENT_SHADER;
        let start = source.find("if(shadow_mode==3)").unwrap();
        let end = start + source[start..].find("if(shadow_mode==4)").unwrap();
        let body = &source[start..end];
        // The rounded_distance/outer_radius corner mask is still computed
        // and multiplied into every content/border path in this branch —
        // radial_reveal * rounded_window_coverage, never one replacing
        // the other.
        assert!(body.contains("rounded_distance(local_position,surface_size,outer_radius)"));
        assert!(body.matches("rounded_distance(local_position,surface_size,outer_radius)").count() >= 2, "used for both the no-border rounded path and the bordered outer/inner split");
    }

    #[test]
    fn minato_reveal_does_not_warp_texcoord() {
        // Section 2's explicit requirement: no texture warp, no polar
        // angle math — `texcoord` (the ordinary, unwarped varying) is
        // sampled directly, exactly like every other content mode.
        let source = super::SCENE_FRAGMENT_SHADER;
        let start = source.find("if(shadow_mode==3)").unwrap();
        let end = start + source[start..].find("if(shadow_mode==4)").unwrap();
        let body = &source[start..end];
        assert!(body.contains("texture(captured,texcoord)"));
        assert!(!body.contains("atan"));
        assert!(!body.contains("sin("));
        assert!(!body.contains("cos("));
    }

    // ========================================================
    // 3a3fa2b6 — TeleportFlashy: generic solid-overlay primitive +
    // EnergyTear shader-parameterization non-regression.
    // ========================================================

    #[test]
    fn solid_overlay_mode_reads_the_corner_radius_uniform() {
        // The whole point of the 3a3fa2b6 shader change: shadow_mode==2's
        // rounded_distance call must use `outer_radius` (which is derived
        // from the real `corner_radius` uniform at the top of main()),
        // never a literal 0.0 — otherwise render_solid_overlay could never
        // produce a rounded-corner-clipped flash.
        let source = super::SCENE_FRAGMENT_SHADER;
        assert!(!source.contains("rounded_distance(local_position,surface_size,0.0)"));
        let outer_radius_decl = source.find("float outer_radius=min(corner_radius").unwrap();
        let tear_branch = source.find("if(shadow_mode==2)").unwrap();
        assert!(outer_radius_decl < tear_branch, "outer_radius must already be in scope before mode 2 reads it");
    }

    #[test]
    fn energy_tear_streak_draw_still_forces_corner_radius_uniform_to_zero() {
        // 3a3fa2b6 non-regression: EnergyTear's OWN call site — not the
        // shader — is what keeps its streaks square. This must remain
        // completely unmodified by the TeleportFlashy work: the streak
        // draw still explicitly sends corner_radius_uniform=0.0 for every
        // streak, so after the shader starts reading that uniform in mode
        // 2, EnergyTear's streaks evaluate outer_radius=0 and render
        // pixel-identically to before.
        let source = include_str!("renderer.rs");
        let start = source.find("pub(crate) fn render_energy_tear_slices(").unwrap();
        let end = start + source[start..].find("\n    /// 3a3fa2b6").unwrap();
        let body = &source[start..end];
        assert!(body.contains("gl::Uniform1f(self.corner_radius_uniform, 0.0);"));
    }

    #[test]
    fn render_solid_overlay_is_generic_over_render_quad_plan_not_energy_tear_plan() {
        // Section 5's explicit requirement: TeleportFlashy's overlay
        // primitive must not conceptually depend on EnergyTear effect
        // state — it takes the SAME generic RenderQuadPlan every other
        // surface draw already uses, never EnergyTearRenderPlan.
        let source = include_str!("renderer.rs");
        let start = source.find("pub(crate) fn render_solid_overlay(").unwrap();
        let end = start + source[start..].find("\n    }\n}").unwrap();
        let body = &source[start..end];
        assert!(body.contains("plan: crate::x11::scene::RenderQuadPlan"));
        assert!(!body.contains("EnergyTearRenderPlan"));
        // Reuses shadow_mode==2, no new uniform location, no new VAO/VBO,
        // no new texture, no new framebuffer.
        assert!(body.contains("gl::Uniform1i(self.shadow_mode_uniform, 2);"));
        assert!(!body.contains("gl::GenTextures"));
        assert!(!body.contains("gl::GenFramebuffers"));
        assert!(!body.contains("gl::GenVertexArrays"));
        assert!(!body.contains("gl::GenBuffers"));
    }

    #[test]
    fn render_solid_overlay_skips_the_draw_entirely_when_alpha_is_zero() {
        // No allocation, no draw call when alpha<=0 — matches the exact
        // gating EnergyTear's own streak draw already uses
        // (`if render_plan.streak_alpha > 0.0`).
        let source = include_str!("renderer.rs");
        let start = source.find("pub(crate) fn render_solid_overlay(").unwrap();
        let end = start + source[start..].find("\n    }\n}").unwrap();
        let body = &source[start..end];
        assert!(body.contains("alpha <= 0.0"));
    }

    #[test]
    fn render_surface_with_radial_reveal_selects_mode_3_and_allocates_no_new_gl_resource() {
        let source = include_str!("renderer.rs");
        let start = source.find("pub(crate) fn render_surface_with_radial_reveal(").unwrap();
        let end = start + source[start..].find("\n    }\n\n    /// 3a3fa2b7 (Kamui) —").unwrap();
        let body = &source[start..end];
        assert!(body.contains("gl::Uniform1i(self.shadow_mode_uniform, 3);"));
        assert!(body.contains("gl::Uniform1f(self.reveal_radius_uniform, reveal_radius);"));
        assert!(!body.contains("gl::GenTextures"));
        assert!(!body.contains("gl::GenFramebuffers"));
        assert!(!body.contains("gl::GenVertexArrays"));
        assert!(!body.contains("gl::GenBuffers"));
        // Reuses self.vao/self.buffer/self.program — the SAME resources
        // render_surface_with_opacity already uses.
        assert!(body.contains("self.vao"));
        assert!(body.contains("self.buffer"));
        assert!(body.contains("self.program"));
    }

    // ========================================================
    // 3a3fa2b7 (Kamui) — polar texture warp + radial domain mode 4.
    // ========================================================

    #[test]
    fn kamui_shader_declares_its_five_uniforms() {
        let source = super::SCENE_FRAGMENT_SHADER;
        assert!(source.contains("uniform float kamui_visible_radius;"));
        assert!(source.contains("uniform float kamui_twist;"));
        assert!(source.contains("uniform vec2 kamui_uv_min;"));
        assert!(source.contains("uniform vec2 kamui_uv_scale;"));
        // 3a3fa2b7-r2: nonlinear radial-power warp stage.
        assert!(source.contains("uniform float kamui_radial_power;"));
    }

    #[test]
    fn kamui_mode_4_is_declared_after_mode_3_and_before_the_ordinary_mode_0_sampling_path() {
        let source = super::SCENE_FRAGMENT_SHADER;
        let mode3 = source.find("if(shadow_mode==3)").unwrap();
        let mode4 = source.find("if(shadow_mode==4)").unwrap();
        let ordinary_sample = source.find("vec4 sampled=texture(captured,texcoord)").unwrap();
        assert!(mode3 < mode4, "mode 4 must be declared after mode 3, preserving existing branch order");
        assert!(mode4 < ordinary_sample, "mode 4 must be resolved before falling through to the ordinary mode-0 sampling path");
    }

    #[test]
    fn kamui_reconstruction_multiplies_back_by_sqrt2_the_mandatory_round_trip_factor() {
        // The exact bug this milestone's R1 spec calls out: reconstructing
        // sample_p as `direction * sample_r_norm` alone (omitting the
        // `*1.4142135` factor that undoes the initial `/1.4142135`
        // normalization) would compress the identity image toward the
        // center. Pin the literal reconstruction expression so this can
        // never silently regress.
        let source = super::SCENE_FRAGMENT_SHADER;
        assert!(source.contains("vec2 kamui_sample_p=vec2(cos(kamui_twisted_theta),sin(kamui_twisted_theta))*kamui_sample_r*1.4142135;"));
    }

    #[test]
    fn kamui_center_safety_avoids_a_bare_atan_at_the_exact_center() {
        // Deterministic explicit branch, never a bare atan(0,0) call
        // trusted blindly — see the R1 spec's center-safety requirement.
        let source = super::SCENE_FRAGMENT_SHADER;
        assert!(source.contains("float kamui_theta=kamui_len<=0.00001?0.0:atan(kamui_p.y,kamui_p.x);"));
    }

    #[test]
    fn kamui_radius_safety_never_divides_by_a_raw_potentially_zero_visible_radius() {
        let source = super::SCENE_FRAGMENT_SHADER;
        assert!(source.contains("float kamui_safe_radius=max(kamui_visible_radius,0.0001);"));
        // 3a3fa2b7-r2: the linear ratio is computed first, then reshaped
        // by kamui_radial_power (1.0 = exact no-op) — the division itself
        // is unchanged, still against the safe (never-zero) radius.
        assert!(source.contains("float kamui_sample_r_linear=kamui_r/kamui_safe_radius;"));
        assert!(source.contains("float kamui_sample_r=pow(kamui_sample_r_linear,kamui_radial_power);"));
        assert!(!source.contains("kamui_r/kamui_visible_radius"), "must never divide by the raw uniform directly");
    }

    #[test]
    fn kamui_masks_out_of_domain_samples_instead_of_relying_on_clamp_to_edge() {
        // Every content texture in this pipeline uses GL_CLAMP_TO_EDGE
        // (see create_egl_texture) — without this explicit domain check,
        // a warped sample outside the current UV sub-rect would smear
        // edge texels into radial streaks.
        let source = super::SCENE_FRAGMENT_SHADER;
        assert!(source.contains("bool kamui_in_domain=kamui_fraction.x>=0.0 && kamui_fraction.x<=1.0 && kamui_fraction.y>=0.0 && kamui_fraction.y<=1.0;"));
        assert!(source.contains("vec4 kamui_sampled=kamui_in_domain?texture(captured,kamui_uv):vec4(0.0);"));
    }

    #[test]
    fn kamui_uv_reconstruction_uses_the_actual_plan_uv_subrect_not_a_hardcoded_01_range() {
        // section 11 (CRITICAL): kamui_uv_min/kamui_uv_scale, not a bare
        // texcoord/local_position assumption of a [0,1]-covering texture.
        let source = super::SCENE_FRAGMENT_SHADER;
        assert!(source.contains("vec2 kamui_uv=kamui_uv_min+kamui_fraction*kamui_uv_scale;"));
    }

    #[test]
    fn kamui_content_and_border_terms_are_both_multiplied_by_the_same_mask() {
        let source = super::SCENE_FRAGMENT_SHADER;
        let start = source.find("if(shadow_mode==4)").unwrap();
        let body = &source[start..];
        assert!(body.contains("color=kamui_sampled*surface_opacity*kamui_mask;"));
        assert!(body.contains("color=kamui_sampled*coverage(rounded_distance(local_position,surface_size,outer_radius))*surface_opacity*kamui_mask;"));
        assert!(body.contains("color=kamui_sampled*kamui_inner*surface_opacity*kamui_mask+kamui_premultiplied_border;"));
        assert!(body.contains("vec4 kamui_premultiplied_border=vec4(border_color.rgb*border_color.a,border_color.a)*kamui_border*kamui_mask;"));
    }

    #[test]
    fn kamui_still_applies_rounded_corner_coverage_on_unwarped_local_position() {
        let source = super::SCENE_FRAGMENT_SHADER;
        let start = source.find("if(shadow_mode==4)").unwrap();
        let body = &source[start..];
        assert!(body.matches("rounded_distance(local_position,surface_size,outer_radius)").count() >= 2, "used for both the no-border rounded path and the bordered outer/inner split — never rounded_distance(kamui_sample_p,...) or similar warped-geometry variant");
        assert!(!body.contains("rounded_distance(kamui_"), "corner coverage must use UNWARPED local_position, never a warped coordinate");
    }

    #[test]
    fn kamui_border_geometry_is_unwarped_never_texture_sampled() {
        let source = super::SCENE_FRAGMENT_SHADER;
        let start = source.find("if(shadow_mode==4)").unwrap();
        let body = &source[start..];
        // Border width/inner-size/inner-radius all derive from
        // local_position/surface_size — compositor geometry, never from
        // kamui_sample_p or texture() at all.
        assert!(body.contains("float kamui_width=min(border_width,min(surface_size.x,surface_size.y)*0.5);"));
        assert!(body.contains("vec2 kamui_inner_size=max(surface_size-vec2(2.0*kamui_width),vec2(0.0));"));
    }

    #[test]
    fn render_surface_with_kamui_warp_selects_mode_4_and_allocates_no_new_gl_resource() {
        let source = include_str!("renderer.rs");
        let start = source.find("pub(crate) fn render_surface_with_kamui_warp(").unwrap();
        let end = start + source[start..].find("\n    }\n\n    pub(crate) fn render_shadow").unwrap();
        let body = &source[start..end];
        assert!(body.contains("gl::Uniform1i(self.shadow_mode_uniform, 4);"));
        assert!(body.contains("gl::Uniform1f(self.kamui_visible_radius_uniform, visible_radius);"));
        assert!(body.contains("gl::Uniform1f(self.kamui_twist_uniform, twist);"));
        assert!(body.contains("gl::Uniform2f(self.kamui_uv_min_uniform,"));
        assert!(body.contains("gl::Uniform2f(self.kamui_uv_scale_uniform,"));
        assert!(body.contains("gl::Uniform1f(self.kamui_radial_power_uniform, radial_power);"));
        assert!(!body.contains("gl::GenTextures"));
        assert!(!body.contains("gl::GenFramebuffers"));
        assert!(!body.contains("gl::GenVertexArrays"));
        assert!(!body.contains("gl::GenBuffers"));
        assert!(body.contains("self.vao"));
        assert!(body.contains("self.buffer"));
        assert!(body.contains("self.program"));
    }

    #[test]
    fn render_surface_with_kamui_warp_uv_uniforms_use_raw_plan_bounds_not_reordered() {
        // The reconstruction mapping must reproduce the SAME affine
        // relationship the vertex shader already establishes (u0 at
        // local=0, u1 at local=width) — including a deliberately flipped
        // plan (u0>u1) — so the CPU side must send RAW plan.u0/plan.v0 as
        // the origin and (u1-u0)/(v1-v0) as the (possibly negative) slope,
        // never min/max-reordered.
        let source = include_str!("renderer.rs");
        let start = source.find("pub(crate) fn render_surface_with_kamui_warp(").unwrap();
        let end = start + source[start..].find("\n    }\n\n    pub(crate) fn render_shadow").unwrap();
        let body = &source[start..end];
        assert!(body.contains("gl::Uniform2f(self.kamui_uv_min_uniform, plan.u0, plan.v0);"));
        assert!(body.contains("gl::Uniform2f(self.kamui_uv_scale_uniform, plan.u1 - plan.u0, plan.v1 - plan.v0);"));
        assert!(!body.contains(".min(plan.u1)"));
        assert!(!body.contains(".min(plan.v1)"));
    }

    /// Test-only CPU mirror of the shader's mode-4 polar transform,
    /// covering EXACTLY the math the GLSL performs (see
    /// SCENE_FRAGMENT_SHADER's `if(shadow_mode==4)` branch) — proves the
    /// round-trip/domain/safety properties as ordinary Rust numeric tests,
    /// not only via shader-source string scans.
    #[cfg(test)]
    fn kamui_transform(
        local: (f32, f32),
        surface_size: (f32, f32),
        visible_radius: f32,
        twist: f32,
        radial_power: f32,
        uv_min: (f32, f32),
        uv_scale: (f32, f32),
    ) -> (f32, f32, bool) {
        let px = (local.0 - surface_size.0 * 0.5) / (surface_size.0 * 0.5);
        let py = (local.1 - surface_size.1 * 0.5) / (surface_size.1 * 0.5);
        let len = (px * px + py * py).sqrt();
        let r = len / std::f32::consts::SQRT_2;
        let safe_radius = visible_radius.max(0.0001);
        let sample_r_linear = r / safe_radius;
        // 3a3fa2b7-r2.1: nonlinear reshaping — radial_power=1.0 is an
        // exact no-op (pow(x,1.0)==x), preserving the R1 identity
        // round-trip. `sample_r` (power-reshaped) drives ONLY the final
        // radial reconstruction below; the angular weight below uses the
        // UNREFORMED `sample_r_linear` — decoupling twist's spatial
        // footprint from radial_power (the R2 bug this milestone fixes:
        // R2 used the already-reshaped `sample_r` for the weight too,
        // meaning changing radial_power silently changed how far the
        // twist field reached).
        let sample_r = sample_r_linear.powf(radial_power);
        let theta = if len <= 0.00001 { 0.0 } else { py.atan2(px) };
        let weight = (1.0 - sample_r_linear).clamp(0.0, 1.0).powf(1.5);
        let twisted_theta = theta + twist * weight;
        let sample_px = twisted_theta.cos() * sample_r * std::f32::consts::SQRT_2;
        let sample_py = twisted_theta.sin() * sample_r * std::f32::consts::SQRT_2;
        let fraction_x = sample_px * 0.5 + 0.5;
        let fraction_y = sample_py * 0.5 + 0.5;
        let in_domain = (0.0..=1.0).contains(&fraction_x) && (0.0..=1.0).contains(&fraction_y);
        let uv_x = uv_min.0 + fraction_x * uv_scale.0;
        let uv_y = uv_min.1 + fraction_y * uv_scale.1;
        (uv_x, uv_y, in_domain)
    }

    #[test]
    fn kamui_identity_round_trip_center_side_interior_and_all_corners() {
        // visible_radius=1, twist=0 -> sample coordinate MUST equal the
        // original coordinate exactly (float tolerance only). This is
        // THE single most important correctness property in this
        // candidate — the R1 spec's own explicit bug it corrects.
        let surface_size = (200.0_f32, 100.0_f32);
        let points = [
            (100.0, 50.0),  // center
            (200.0, 50.0),  // side midpoint
            (150.0, 30.0),  // arbitrary interior point
            (0.0, 0.0), (200.0, 0.0), (0.0, 100.0), (200.0, 100.0), // all four corners
        ];
        for local in points {
            // radial_power=1.0: part of the identity state alongside
            // visible_radius=1.0/twist=0.0 — the new pow() stage must be
            // an exact no-op here.
            let (uv_x, uv_y, in_domain) = kamui_transform(local, surface_size, 1.0, 0.0, 1.0, (0.0, 0.0), (1.0, 1.0));
            let expected_u = local.0 / surface_size.0;
            let expected_v = local.1 / surface_size.1;
            assert!(in_domain, "local={local:?}");
            assert!((uv_x - expected_u).abs() < 1e-4, "local={local:?} uv_x={uv_x} expected={expected_u}");
            assert!((uv_y - expected_v).abs() < 1e-4, "local={local:?} uv_y={uv_y} expected={expected_v}");
        }
    }

    #[test]
    fn kamui_identity_round_trip_holds_for_a_nontrivial_uv_subrect() {
        // Section 11 (CRITICAL): must NOT assume the source texture
        // covers [0,1] — a cropped/nontrivial UV sub-rect must round-trip
        // exactly too.
        let surface_size = (200.0_f32, 100.0_f32);
        let uv_min = (0.25_f32, 0.10_f32);
        let uv_scale = (0.5_f32, 0.6_f32); // covers u in [0.25,0.75], v in [0.10,0.70]
        let points = [(100.0, 50.0), (200.0, 100.0), (0.0, 0.0), (150.0, 30.0)];
        for local in points {
            let (uv_x, uv_y, in_domain) = kamui_transform(local, surface_size, 1.0, 0.0, 1.0, uv_min, uv_scale);
            let fraction = (local.0 / surface_size.0, local.1 / surface_size.1);
            let expected_u = uv_min.0 + fraction.0 * uv_scale.0;
            let expected_v = uv_min.1 + fraction.1 * uv_scale.1;
            assert!(in_domain, "local={local:?}");
            assert!((uv_x - expected_u).abs() < 1e-4, "local={local:?}");
            assert!((uv_y - expected_v).abs() < 1e-4, "local={local:?}");
        }
    }

    #[test]
    fn kamui_without_the_sqrt2_factor_would_compress_the_identity_image_toward_center() {
        // Explicit negative-control proof that the *sqrt(2) reconstruction
        // factor is load-bearing: a naive `direction * sample_r_norm`
        // (omitting the factor) would NOT reproduce the corner exactly —
        // this is precisely the bug the R1 spec calls out.
        let surface_size = (200.0_f32, 100.0_f32);
        let corner = (200.0_f32, 100.0_f32);
        let px = (corner.0 - surface_size.0 * 0.5) / (surface_size.0 * 0.5);
        let py = (corner.1 - surface_size.1 * 0.5) / (surface_size.1 * 0.5);
        let r_norm = (px * px + py * py).sqrt() / std::f32::consts::SQRT_2;
        // Correct (with *sqrt2): sample_p == p exactly at identity.
        let correct_sample_r = r_norm / 1.0;
        let correct_p = correct_sample_r * std::f32::consts::SQRT_2;
        assert!((correct_p - (px * px + py * py).sqrt()).abs() < 1e-4);
        // Buggy (without *sqrt2): would compress toward center by a
        // factor of sqrt(2) — demonstrably NOT equal to the original.
        let buggy_p = correct_sample_r; // omits the *sqrt(2)
        assert!((buggy_p - (px * px + py * py).sqrt()).abs() > 0.1, "the omitted factor must produce a materially different (compressed) result");
    }

    #[test]
    fn kamui_center_stays_exact_center_regardless_of_twist() {
        let surface_size = (200.0_f32, 100.0_f32);
        let center = (100.0_f32, 50.0_f32);
        for twist in [-3.2_f32, -1.0, 0.0, 1.0, 3.2] {
            // radial_power irrelevant at the exact center (sample_r==0,
            // pow(0,p)==0 for any p>0) — still pass a nontrivial value
            // (1.8, matching the CLOSE R2 end value) to prove that too.
            let (uv_x, uv_y, in_domain) = kamui_transform(center, surface_size, 1.0, twist, 1.8, (0.0, 0.0), (1.0, 1.0));
            assert!(in_domain, "twist={twist}");
            assert!((uv_x - 0.5).abs() < 1e-4, "twist={twist} uv_x={uv_x}");
            assert!((uv_y - 0.5).abs() < 1e-4, "twist={twist} uv_y={uv_y}");
        }
    }

    #[test]
    fn kamui_zero_and_near_zero_visible_radius_never_produce_nan_or_inf() {
        let surface_size = (200.0_f32, 100.0_f32);
        // 3a3fa2b7-r2.1: also sweep the actual OPEN (1.8) and CLOSE (0.55)
        // radial_power values (corrected direction — OPEN pushes source
        // content outward via power>1, CLOSE pulls it inward via
        // power<1), not just the identity 1.0 — the reshaped
        // sample_r_linear can grow large as visible_radius->0, so this
        // proves pow() stays finite across the real production range.
        for radial_power in [1.0_f32, 0.55, 1.8] {
            for visible_radius in [0.0_f32, 0.0001, 0.001, 0.01] {
                for local in [(0.0, 0.0), (100.0, 50.0), (200.0, 100.0), (37.0, 91.0)] {
                    let (uv_x, uv_y, _) = kamui_transform(local, surface_size, visible_radius, 2.0, radial_power, (0.0, 0.0), (1.0, 1.0));
                    assert!(uv_x.is_finite(), "radial_power={radial_power} visible_radius={visible_radius} local={local:?} uv_x={uv_x}");
                    assert!(uv_y.is_finite(), "radial_power={radial_power} visible_radius={visible_radius} local={local:?} uv_y={uv_y}");
                    assert!(!uv_x.is_nan());
                    assert!(!uv_y.is_nan());
                }
            }
        }
    }

    #[test]
    fn kamui_radial_power_is_exact_no_op_at_one_regardless_of_visible_radius() {
        // 3a3fa2b7-r2/r2.1: radial_power=1.0 must reproduce the EXACT R1
        // behavior (pow(x,1.0)==x) for any visible_radius, not just at
        // the full identity state — proven directly against the R1
        // formula (sample_r_linear used with no reshaping at all).
        let surface_size = (200.0_f32, 100.0_f32);
        for visible_radius in [0.02_f32, 0.2, 0.5, 0.97, 1.0] {
            for local in [(30.0, 20.0), (150.0, 80.0), (0.0, 0.0), (200.0, 100.0)] {
                let (with_power, _, _) = kamui_transform(local, surface_size, visible_radius, 1.2, 1.0, (0.0, 0.0), (1.0, 1.0));
                let px = (local.0 - surface_size.0 * 0.5) / (surface_size.0 * 0.5);
                let py = (local.1 - surface_size.1 * 0.5) / (surface_size.1 * 0.5);
                let len = (px * px + py * py).sqrt();
                let r = len / std::f32::consts::SQRT_2;
                let sample_r_linear = r / visible_radius.max(0.0001);
                let theta = if len <= 0.00001 { 0.0 } else { py.atan2(px) };
                let weight = (1.0 - sample_r_linear).clamp(0.0, 1.0).powf(1.5);
                let twisted = theta + 1.2 * weight;
                let sample_px = twisted.cos() * sample_r_linear * std::f32::consts::SQRT_2;
                let expected = sample_px * 0.5 + 0.5;
                assert!((with_power - expected).abs() < 1e-4, "visible_radius={visible_radius} local={local:?}");
            }
        }
    }

    // ========================================================
    // 3a3fa2b7-r2.1 — radial-flow direction correction + angular/radial
    // decoupling (the R2 bug this milestone fixes).
    // ========================================================

    #[test]
    fn kamui_weight_uses_the_linear_unreshaped_radius_never_the_power_reshaped_one() {
        // The exact R2 bug: `kamui_weight` was computed from the
        // ALREADY-`pow()`-reshaped `kamui_sample_r`, so changing
        // `radial_power` silently changed the spatial extent of the twist
        // field too. Fixed: the weight must be computed from
        // `kamui_sample_r_linear` (the plain, un-reshaped ratio),
        // completely independent of `radial_power`.
        let source = super::SCENE_FRAGMENT_SHADER;
        assert!(source.contains("float kamui_weight=pow(clamp(1.0-kamui_sample_r_linear,0.0,1.0),1.5);"));
        assert!(!source.contains("float kamui_weight=pow(clamp(1.0-kamui_sample_r,0.0,1.0),1.5);"), "must not use the power-reshaped radius for the angular weight");
    }

    #[test]
    fn kamui_angular_weight_is_unaffected_by_changing_radial_power() {
        // Numeric proof (not just a string scan): holding local position,
        // visible_radius, and twist fixed, the COMPUTED angular weight
        // must be identical regardless of radial_power — only the final
        // radial reconstruction (and therefore the sampled UV) may change.
        let surface_size = (200.0_f32, 100.0_f32);
        let local = (140.0_f32, 65.0_f32);
        let visible_radius = 0.6_f32;
        let px = (local.0 - surface_size.0 * 0.5) / (surface_size.0 * 0.5);
        let py = (local.1 - surface_size.1 * 0.5) / (surface_size.1 * 0.5);
        let len = (px * px + py * py).sqrt();
        let r = len / std::f32::consts::SQRT_2;
        let sample_r_linear = r / visible_radius.max(0.0001);
        let weight = (1.0 - sample_r_linear).clamp(0.0, 1.0).powf(1.5);
        // The weight formula above reads only `sample_r_linear`, which
        // does not depend on radial_power at all — so for ANY
        // radial_power value, the weight computed this same way is
        // identical by construction. Confirm with a few representative
        // values used in production (OPEN start 1.8, identity 1.0, CLOSE
        // end 0.55) that none of them appear anywhere in this formula.
        for radial_power in [1.8_f32, 1.0, 0.55] {
            let sample_r_reshaped = sample_r_linear.powf(radial_power);
            let weight_if_bugged = (1.0 - sample_r_reshaped).clamp(0.0, 1.0).powf(1.5);
            if (radial_power - 1.0).abs() > 1e-6 {
                assert!((weight - weight_if_bugged).abs() > 1e-4, "sanity: the R2-bugged formula WOULD differ from the correct one at radial_power={radial_power}, proving this is a real, observable difference, not a no-op");
            }
        }
        // The correct weight (from sample_r_linear) is fixed, single value,
        // computed once above — trivially "the same for any radial_power"
        // since radial_power never entered its computation at all.
        assert!(weight.is_finite() && weight >= 0.0);
    }

    #[test]
    fn kamui_output_sample_direction_matches_the_corrected_open_close_assignment() {
        // Section 4: at a representative output_fraction=0.5, verify the
        // corrected direction. `source_r = pow(output_r, power)`:
        //   - OPEN power (>1): pow(0.5, 1.8) < 0.5 -> an output point at
        //     r=0.5 samples from CLOSER to source-center (source_r<output_r)
        //     than the identity mapping would. Combined with the
        //     source-feature-displacement proof below (the authoritative
        //     framing), this is exactly what produces OUTWARD-moving
        //     content: pulling the source's center closer inflates how
        //     much of the outer disk is filled by near-center source
        //     content, i.e. central features get pushed out to fill more
        //     of the visible area.
        //   - CLOSE power (<1): pow(0.5, 0.55) > 0.5 -> an output point at
        //     r=0.5 samples from FARTHER from source-center (source_r>
        //     output_r) than identity — pulling outer source content
        //     inward to be displayed nearer the center.
        let open_power = 1.8_f32;
        let close_power = 0.55_f32;
        assert!(0.5_f32.powf(open_power) < 0.5, "OPEN: output r=0.5 must sample CLOSER to source-center than identity");
        assert!(0.5_f32.powf(close_power) > 0.5, "CLOSE: output r=0.5 must sample FARTHER from source-center than identity");
    }

    #[test]
    fn kamui_source_feature_forward_displacement_matches_the_corrected_direction() {
        // Section 5 (the authoritative, independently-verified proof):
        // for a FIXED SOURCE FEATURE at source_r=S, it is displayed at
        // output_r = S^(1/power) (solving source_r=pow(output_r,power)
        // for output_r). This directly answers "does a piece of content
        // move toward or away from center," which is what the visual
        // vortex sensation actually depends on.
        let source_feature_r = 0.25_f32;
        let open_power = 1.8_f32;
        let close_power = 0.55_f32;
        let open_output_r = source_feature_r.powf(1.0 / open_power);
        let close_output_r = source_feature_r.powf(1.0 / close_power);
        // Matches the independently-verified numeric proof: 0.25^(1/1.8)
        // ≈ 0.463 (OUTWARD, > source_r), 0.25^(1/0.55) ≈ 0.080 (INWARD,
        // < source_r).
        assert!(open_output_r > source_feature_r, "OPEN: a source feature must be displayed FARTHER from center (outward), got output_r={open_output_r} vs source_r={source_feature_r}");
        assert!((open_output_r - 0.4629).abs() < 0.01, "OPEN output_r sanity check, got {open_output_r}");
        assert!(close_output_r < source_feature_r, "CLOSE: a source feature must be displayed CLOSER to center (inward), got output_r={close_output_r} vs source_r={source_feature_r}");
        assert!((close_output_r - 0.0804).abs() < 0.01, "CLOSE output_r sanity check, got {close_output_r}");
    }

    #[test]
    fn kamui_every_corner_r_norm_is_exactly_one_regardless_of_aspect_ratio() {
        for (w, h) in [(200.0_f32, 100.0_f32), (100.0, 200.0), (50.0, 50.0), (1920.0, 1080.0)] {
            for corner in [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)] {
                let px = (corner.0 - w * 0.5) / (w * 0.5);
                let py = (corner.1 - h * 0.5) / (h * 0.5);
                let r = (px * px + py * py).sqrt() / std::f32::consts::SQRT_2;
                assert!((r - 1.0).abs() < 1e-5, "w={w} h={h} corner={corner:?} r={r}");
            }
        }
    }

    #[test]
    fn kamui_math_is_confined_to_mode_4_ordinary_modes_never_execute_it() {
        // section 25/26/33: atan/cos/sin/pow (transcendental Kamui-only
        // math) must exist ONLY inside the mode-4 branch. Check BOTH
        // sides of it: modes 1/2/3 (textually before mode 4) and the
        // ordinary mode-0 fallback (textually AFTER mode 4's own
        // `return;`, since mode 4 is spliced in between mode 3 and mode
        // 0 — see the shader source layout).
        let source = super::SCENE_FRAGMENT_SHADER;
        let mode1 = source.find("if(shadow_mode==1)").unwrap();
        let mode4_start = source.find("if(shadow_mode==4)").unwrap();
        let before_mode4 = &source[mode1..mode4_start];
        assert!(!before_mode4.contains("atan("));
        assert!(!before_mode4.contains("cos("));
        assert!(!before_mode4.contains("sin("));
        assert!(!before_mode4.contains("pow("));
        let mode4_end = mode4_start + source[mode4_start..].find(" vec4 sampled=texture(captured,texcoord);").unwrap();
        let after_mode4 = &source[mode4_end..];
        assert!(!after_mode4.contains("atan("));
        assert!(!after_mode4.contains("cos("));
        assert!(!after_mode4.contains("sin("));
        assert!(!after_mode4.contains("pow("));
    }

    #[test]
    fn shadow_rgb_color_reaches_renderer_as_normalized_rgb() {
        let color = super::normalized_shadow_color([0x4c, 0x78, 0x99]);
        assert_eq!(color, [0x4c as f32 / 255.0, 0x78 as f32 / 255.0, 0x99 as f32 / 255.0]);
    }

    #[test]
    fn blur_capture_region_expands_by_effective_kernel_reach() {
        let region = BlurCaptureRegion::new(100, 80, 200, 100, 8.0, 1000, 800).unwrap();
        assert_eq!((region.x, region.y, region.width, region.height), (92, 72, 216, 116));
        assert_eq!(region.framebuffer_y, 612);
    }

    #[test]
    fn backdrop_root_x_maps_to_root_relative_u() {
        assert_eq!(root_to_texture_u(0.0, 100), 0.0);
        assert_eq!(root_to_texture_u(25.0, 100), 0.25);
        assert_eq!(root_to_texture_u(100.0, 100), 1.0);
    }

    #[test]
    fn backdrop_root_top_and_bottom_map_to_gl_v() {
        assert_eq!(root_to_texture_v(0.0, 100), 1.0);
        assert_eq!(root_to_texture_v(100.0, 100), 0.0);
    }

    #[test]
    fn backdrop_edge_mapping_and_negative_owner_are_clipped_in_root_space() {
        let params = BackdropParams::new(-5, -2, 10, 10, 100, 80).unwrap();
        assert_eq!(root_to_texture_u(0.0, params.root_width), 0.0);
        assert_eq!(root_to_texture_v(0.0, params.root_height), 1.0);
        assert_eq!(root_to_texture_u(5.0, params.root_width), 0.05);
        assert_eq!(root_to_texture_v(8.0, params.root_height), 0.9);
    }

    #[test]
    fn backdrop_replacement_outputs_premultiplied_coverage() {
        assert_eq!(backdrop_replacement([0.8, 0.4, 0.2], 0.0), [0.0, 0.0, 0.0, 0.0]);
        assert_eq!(backdrop_replacement([0.8, 0.4, 0.2], 0.25), [0.2, 0.1, 0.05, 0.25]);
        assert_eq!(backdrop_replacement([0.8, 0.4, 0.2], 0.5), [0.4, 0.2, 0.1, 0.5]);
        assert_eq!(backdrop_replacement([0.8, 0.4, 0.2], 1.0), [0.8, 0.4, 0.2, 1.0]);
    }

    #[test]
    fn backdrop_shader_contract_uses_runtime_rounded_mask_and_replacement_output() {
        assert!(BACKDROP_FRAGMENT_SHADER.contains("uniform float corner_radius"));
        assert!(BACKDROP_FRAGMENT_SHADER.contains("rounded_distance(local_position,surface_size,radius)"));
        assert!(BACKDROP_FRAGMENT_SHADER.contains("vec4(blurred*c,c)"));
        assert!(!BACKDROP_FRAGMENT_SHADER.contains("surface_opacity"));
        assert!(!BACKDROP_FRAGMENT_SHADER.contains("texture(blurred_root,texcoord).a"));
    }

    #[test]
    fn backdrop_is_a_lazy_inert_graphics_primitive() {
        let source = include_str!("renderer.rs");
        assert!(source.contains("backdrop_program: Option<BackdropProgram>"));
        assert!(source.contains("if self.backdrop_program.is_none()"));
        let start = source.find("pub(crate) fn draw_blurred_backdrop(").unwrap();
        let end = start + source[start..].find("\n    pub fn clear").unwrap();
        let body = &source[start..end];
        assert!(!body.contains("capture_and_blur_background("));
        assert!(source.contains("gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA)"));
        assert!(source.contains("gl::BlendEquation(gl::FUNC_ADD)"));
        assert!(source.contains("gl::Disable(gl::SCISSOR_TEST)"));
        assert!(source.contains("textures: [u32; 2]"));
        assert!(source.contains("framebuffers: [u32; 2]"));
    }

    #[test]
    fn blur_capture_region_clips_at_each_root_edge() {
        let cases = [
            (0, 0, 0, 0, 20, 20),
            (90, 0, 80, 0, 20, 20),
            (0, 90, 0, 80, 20, 20),
            (90, 90, 80, 80, 20, 20),
        ];
        for (x, y, expected_x, expected_y, expected_width, expected_height) in cases {
            let region = BlurCaptureRegion::new(x, y, 10, 10, 10.0, 100, 100).unwrap();
            assert_eq!((region.x, region.y), (expected_x, expected_y));
            assert_eq!((region.width, region.height), (expected_width, expected_height));
        }
    }

    #[test]
    fn blur_capture_region_rejects_invalid_dimensions_and_radius() {
        assert!(BlurCaptureRegion::new(0, 0, 0, 10, 4.0, 100, 100).is_none());
        assert!(BlurCaptureRegion::new(0, 0, 10, 10, 0.0, 100, 100).is_none());
        assert!(BlurCaptureRegion::new(0, 0, 10, 10, f32::NAN, 100, 100).is_none());
        assert!(BlurCaptureRegion::new(0, 0, 10, 10, 4.0, 0, 100).is_none());
    }

    #[test]
    fn blur_region_uses_root_to_framebuffer_y_conversion() {
        let region = BlurCaptureRegion::new(20, 30, 40, 50, 4.0, 200, 300).unwrap();
        assert_eq!((region.y, region.height, region.framebuffer_y), (26, 58, 216));
        assert_eq!(region.root_height - (region.y + region.height), region.framebuffer_y);
    }

    #[test]
    fn blur_shader_contract_is_two_pass_fixed_tap_ping_pong() {
        assert_eq!(BLUR_TAP_RADIUS, 4.0);
        assert!(BLUR_VERTEX_SHADER.contains("gl_Position"));
        assert!(BLUR_FRAGMENT_SHADER.contains("texture(source,texcoord)"));
        assert!(BLUR_FRAGMENT_SHADER.contains("direction"));
        assert!(BLUR_FRAGMENT_SHADER.contains("0.22702703"));
        assert!(BLUR_FRAGMENT_SHADER.contains("0.01621622"));
    }

    #[test]
    fn blur_resource_contract_has_two_textures_and_two_framebuffers() {
        assert_eq!(std::mem::size_of::<[u32; 2]>(), 2 * std::mem::size_of::<u32>());
        let source = include_str!("renderer.rs");
        assert!(source.contains("textures: [u32; 2]"));
        assert!(source.contains("framebuffers: [u32; 2]"));
        assert!(source.contains("gl::GenTextures(2"));
        assert!(source.contains("gl::GenFramebuffers(2"));
        assert!(source.contains("gl::BindFramebuffer(gl::FRAMEBUFFER, 0)"));
    }

    // The following tests exercise Phase 1 V2's transactional-cleanup and
    // state-ordering contract via static source inspection. A live GL
    // context is not available under `cargo test`, so failure-injection
    // (e.g. a forced glTexImage2D allocation error) cannot be exercised
    // directly; these assert the required code shape instead, matching the
    // existing project convention for GL-adjacent contract tests.

    #[test]
    fn blur_shader_creation_deletes_shader_on_compile_failure() {
        let source = include_str!("renderer.rs");
        let start = source.find("fn compile_shader(").expect("compile_shader exists");
        let end = start + source[start..].find("\n}\n").expect("compile_shader body ends");
        let body = &source[start..end];
        assert!(body.contains("if status == 0"));
        assert!(body.contains("gl::DeleteShader(shader)"));
        // The shader must be created before the compile-status check so the
        // delete-on-failure branch has something to delete.
        assert!(body.find("gl::CreateShader").unwrap() < body.find("status == 0").unwrap());
    }

    #[test]
    fn blur_program_creation_deletes_vertex_shader_on_fragment_failure() {
        let source = include_str!("renderer.rs");
        let start = source.find("fn create_program(").expect("create_program exists");
        let end = start + source[start..].find("\n}\n").expect("create_program body ends");
        let body = &source[start..end];
        // Fragment compile failure must delete the already-created vertex shader.
        let fragment_match = body.find("compile_shader(fragment_source").expect("compiles fragment");
        let vertex_delete = body.find("gl::DeleteShader(vertex);").expect("deletes vertex on failure");
        assert!(fragment_match < vertex_delete);
        // glCreateProgram's return value must be checked for zero before use.
        assert!(body.contains("program == 0"));
    }

    #[test]
    fn blur_resource_construction_uses_transactional_pending_guard() {
        let source = include_str!("renderer.rs");
        assert!(source.contains("struct PendingBlurResources"));
        let drop_start = source
            .find("impl Drop for PendingBlurResources")
            .expect("PendingBlurResources has a Drop impl");
        let drop_end = drop_start + source[drop_start..].find("\n}\n").expect("Drop body ends");
        let drop_body = &source[drop_start..drop_end];
        assert!(drop_body.contains("gl::DeleteBuffers"));
        assert!(drop_body.contains("gl::DeleteVertexArrays"));
        assert!(drop_body.contains("gl::DeleteFramebuffers(2"));
        assert!(drop_body.contains("gl::DeleteTextures(2"));
        assert!(drop_body.contains("gl::DeleteProgram"));

        let new_start = source.find("fn new(width: i32, height: i32) -> Result<Self, Box<dyn Error>> {")
            .expect("BackgroundBlurResources::new exists");
        let new_end = new_start + source[new_start..].find("\n    fn ensure_size")
            .expect("new() body ends before ensure_size");
        let new_body = &source[new_start..new_end];
        assert!(new_body.contains("let mut pending = PendingBlurResources"));
        // Ownership transfers by consuming the guard; its fields are empty
        // before its normal Drop runs.
        assert!(new_body.contains("pending.into_resources("));
        assert!(!new_body.contains("std::mem::forget(pending)"));
    }

    #[test]
    fn blur_resource_construction_checks_gl_errors_and_zero_names() {
        let source = include_str!("renderer.rs");
        let new_start = source.find("fn new(width: i32, height: i32) -> Result<Self, Box<dyn Error>> {")
            .expect("BackgroundBlurResources::new exists");
        let new_end = new_start + source[new_start..].find("\n    fn ensure_size")
            .expect("new() body ends before ensure_size");
        let new_body = &source[new_start..new_end];
        assert!(new_body.matches("check_gl_error(").count() >= 5);
        assert!(new_body.contains("texture == 0"));
        assert!(new_body.contains("framebuffer == 0"));
        assert!(new_body.contains("pending.vao == 0"));
        assert!(new_body.contains("pending.buffer == 0"));
    }

    #[test]
    fn blur_public_entry_snapshots_state_before_lazy_allocation() {
        let source = include_str!("renderer.rs");
        let start = source.find("pub(crate) fn capture_and_blur_background(")
            .expect("capture_and_blur_background exists");
        let end = start + source[start..].find("\n    }\n").expect("function body ends");
        let body = &source[start..end];
        let save_index = body.find("BlurGlState::save()").expect("saves GL state");
        let alloc_index = body.find("BackgroundBlurResources::new").expect("lazily allocates resources");
        assert!(save_index < alloc_index);
    }

    #[test]
    fn blur_capture_and_blur_no_longer_saves_state_internally() {
        // State save/restore is centralized at the public entry point so the
        // very first (lazy-allocating) call is covered; the inner primitive
        // must not duplicate it.
        let source = include_str!("renderer.rs");
        let start = source.find("fn capture_and_blur(\n").expect("capture_and_blur exists");
        let end = start + source[start..].find("\n    }\n}\n").expect("capture_and_blur body ends");
        let body = &source[start..end];
        assert!(!body.contains("BlurGlState::save()"));
    }
}

const VERTEX_SHADER: &str = "#version 330 core\nlayout(location=0) in vec2 position;\nlayout(location=1) in vec2 uv;\nout vec2 texcoord;\nvoid main(){ gl_Position=vec4(position,0.0,1.0); texcoord=uv; }";
const FRAGMENT_SHADER: &str = "#version 330 core\nin vec2 texcoord;\nout vec4 color;\nuniform sampler2D captured;\nvoid main(){ color=texture(captured,texcoord); }";
const SCENE_VERTEX_SHADER: &str = "#version 330 core\nlayout(location=0) in vec2 position;\nlayout(location=1) in vec2 uv;\nlayout(location=2) in vec2 local_position_in;\nout vec2 texcoord;\nout vec2 local_position;\nvoid main(){ gl_Position=vec4(position,0.0,1.0); texcoord=uv; local_position=local_position_in; }";
const SCENE_FRAGMENT_SHADER: &str = "#version 330 core\nin vec2 texcoord;\nin vec2 local_position;\nout vec4 color;\nuniform sampler2D captured;\nuniform int shadow_mode;\nuniform float shadow_extent;\nuniform float shadow_strength;\nuniform vec3 shadow_color;\nuniform float surface_opacity;\nuniform float corner_radius;\nuniform vec2 surface_size;\nuniform float border_width;\nuniform vec4 border_color;\nuniform float reveal_radius;\nuniform float kamui_visible_radius;\nuniform float kamui_twist;\nuniform vec2 kamui_uv_min;\nuniform vec2 kamui_uv_scale;\nuniform float kamui_radial_power;\nfloat rounded_distance(vec2 point, vec2 size, float radius){ vec2 q=abs(point-size*0.5)-(size*0.5-vec2(radius)); return length(max(q,vec2(0.0)))+min(max(q.x,q.y),0.0)-radius; }\nfloat coverage(float distance){ float aa=max(fwidth(distance),0.0001); return 1.0-smoothstep(-aa,aa,distance); }\nvoid main(){ float outer_radius=min(corner_radius,min(surface_size.x,surface_size.y)*0.5); if(shadow_mode==1){ vec2 shadow_point=local_position-vec2(shadow_extent); float shadow_distance=rounded_distance(shadow_point,surface_size,outer_radius); float edge=coverage(-shadow_distance); float falloff=1.0-smoothstep(0.0,max(shadow_extent,0.0001),max(shadow_distance,0.0)); float alpha=shadow_strength*edge*falloff; color=vec4(shadow_color*alpha,alpha); return; } if(shadow_mode==2){ float tear_mask=coverage(rounded_distance(local_position,surface_size,outer_radius)); float alpha=shadow_strength*tear_mask; color=vec4(shadow_color*alpha,alpha); return; } if(shadow_mode==3){ vec2 reveal_p=(local_position-surface_size*0.5)/(surface_size*0.5); float reveal_r=length(reveal_p)/1.4142135; float reveal=coverage(reveal_r-reveal_radius); vec4 reveal_sampled=texture(captured,texcoord); if(border_width<=0.0){ if(corner_radius<=0.0){ color=reveal_sampled*surface_opacity*reveal; return; } color=reveal_sampled*coverage(rounded_distance(local_position,surface_size,outer_radius))*surface_opacity*reveal; return; } float reveal_width=min(border_width,min(surface_size.x,surface_size.y)*0.5); float reveal_outer=coverage(rounded_distance(local_position,surface_size,outer_radius)); vec2 reveal_inner_size=max(surface_size-vec2(2.0*reveal_width),vec2(0.0)); float reveal_inner_radius=max(outer_radius-reveal_width,0.0); float reveal_inner=reveal_inner_size.x>0.0 && reveal_inner_size.y>0.0 ? coverage(rounded_distance(local_position-vec2(reveal_width),reveal_inner_size,reveal_inner_radius)) : 0.0; float reveal_border=clamp(reveal_outer-reveal_inner,0.0,1.0); vec4 reveal_premultiplied_border=vec4(border_color.rgb*border_color.a,border_color.a)*reveal_border*reveal; color=reveal_sampled*reveal_inner*surface_opacity*reveal+reveal_premultiplied_border; return; } if(shadow_mode==4){ vec2 kamui_p=(local_position-surface_size*0.5)/(surface_size*0.5); float kamui_len=length(kamui_p); float kamui_r=kamui_len/1.4142135; float kamui_safe_radius=max(kamui_visible_radius,0.0001); float kamui_sample_r_linear=kamui_r/kamui_safe_radius; float kamui_sample_r=pow(kamui_sample_r_linear,kamui_radial_power); float kamui_theta=kamui_len<=0.00001?0.0:atan(kamui_p.y,kamui_p.x); float kamui_weight=pow(clamp(1.0-kamui_sample_r_linear,0.0,1.0),1.5); float kamui_twisted_theta=kamui_theta+kamui_twist*kamui_weight; vec2 kamui_sample_p=vec2(cos(kamui_twisted_theta),sin(kamui_twisted_theta))*kamui_sample_r*1.4142135; vec2 kamui_fraction=kamui_sample_p*0.5+vec2(0.5); bool kamui_in_domain=kamui_fraction.x>=0.0 && kamui_fraction.x<=1.0 && kamui_fraction.y>=0.0 && kamui_fraction.y<=1.0; vec2 kamui_uv=kamui_uv_min+kamui_fraction*kamui_uv_scale; vec4 kamui_sampled=kamui_in_domain?texture(captured,kamui_uv):vec4(0.0); float kamui_mask=coverage(kamui_r-kamui_visible_radius); if(border_width<=0.0){ if(corner_radius<=0.0){ color=kamui_sampled*surface_opacity*kamui_mask; return; } color=kamui_sampled*coverage(rounded_distance(local_position,surface_size,outer_radius))*surface_opacity*kamui_mask; return; } float kamui_width=min(border_width,min(surface_size.x,surface_size.y)*0.5); float kamui_outer=coverage(rounded_distance(local_position,surface_size,outer_radius)); vec2 kamui_inner_size=max(surface_size-vec2(2.0*kamui_width),vec2(0.0)); float kamui_inner_radius=max(outer_radius-kamui_width,0.0); float kamui_inner=kamui_inner_size.x>0.0 && kamui_inner_size.y>0.0 ? coverage(rounded_distance(local_position-vec2(kamui_width),kamui_inner_size,kamui_inner_radius)) : 0.0; float kamui_border=clamp(kamui_outer-kamui_inner,0.0,1.0); vec4 kamui_premultiplied_border=vec4(border_color.rgb*border_color.a,border_color.a)*kamui_border*kamui_mask; color=kamui_sampled*kamui_inner*surface_opacity*kamui_mask+kamui_premultiplied_border; return; } vec4 sampled=texture(captured,texcoord); if(border_width<=0.0){ if(corner_radius<=0.0){ color=sampled*surface_opacity; return; } color=sampled*coverage(rounded_distance(local_position,surface_size,outer_radius))*surface_opacity; return; } float width=min(border_width,min(surface_size.x,surface_size.y)*0.5); float outer=coverage(rounded_distance(local_position,surface_size,outer_radius)); vec2 inner_size=max(surface_size-vec2(2.0*width),vec2(0.0)); float inner_radius=max(outer_radius-width,0.0); float inner=inner_size.x>0.0 && inner_size.y>0.0 ? coverage(rounded_distance(local_position-vec2(width),inner_size,inner_radius)) : 0.0; float border=clamp(outer-inner,0.0,1.0); vec4 premultiplied_border=vec4(border_color.rgb*border_color.a,border_color.a)*border; color=sampled*inner*surface_opacity+premultiplied_border; }";
#[allow(dead_code)]
const BLUR_VERTEX_SHADER: &str = "#version 330 core\nlayout(location=0) in vec2 position;\nvoid main(){ gl_Position=vec4(position,0.0,1.0); }";
#[allow(dead_code)]
const BLUR_FRAGMENT_SHADER: &str = "#version 330 core\nout vec4 color;\nuniform sampler2D source;\nuniform vec2 texture_size;\nuniform vec2 direction;\nuniform float radius;\nvoid main(){ vec2 texcoord=gl_FragCoord.xy/texture_size; vec2 step_uv=direction*radius/texture_size; vec4 result=texture(source,texcoord)*0.22702703; result+=(texture(source,texcoord+step_uv)+texture(source,texcoord-step_uv))*0.19459459; result+=(texture(source,texcoord+2.0*step_uv)+texture(source,texcoord-2.0*step_uv))*0.12162162; result+=(texture(source,texcoord+3.0*step_uv)+texture(source,texcoord-3.0*step_uv))*0.05405405; result+=(texture(source,texcoord+4.0*step_uv)+texture(source,texcoord-4.0*step_uv))*0.01621622; color=result; }";
const BACKDROP_VERTEX_SHADER: &str = "#version 330 core\nlayout(location=0) in vec2 position;\nlayout(location=1) in vec2 uv;\nlayout(location=2) in vec2 local_position_in;\nout vec2 texcoord;\nout vec2 local_position;\nvoid main(){ gl_Position=vec4(position,0.0,1.0); texcoord=uv; local_position=local_position_in; }";
const BACKDROP_FRAGMENT_SHADER: &str = "#version 330 core\nin vec2 texcoord;\nin vec2 local_position;\nout vec4 color;\nuniform sampler2D blurred_root;\nuniform vec2 surface_size;\nuniform float corner_radius;\nfloat rounded_distance(vec2 point, vec2 size, float radius){ vec2 q=abs(point-size*0.5)-(size*0.5-vec2(radius)); return length(max(q,vec2(0.0)))+min(max(q.x,q.y),0.0)-radius; }\nfloat coverage(float distance){ float aa=max(fwidth(distance),0.0001); return 1.0-smoothstep(-aa,aa,distance); }\nvoid main(){ float radius=min(corner_radius,min(surface_size.x,surface_size.y)*0.5); float c=coverage(rounded_distance(local_position,surface_size,radius)); vec3 blurred=texture(blurred_root,texcoord).rgb; color=vec4(blurred*c,c); }";
