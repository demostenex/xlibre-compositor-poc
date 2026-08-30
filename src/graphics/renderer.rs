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
        if corner_radius_uniform < 0
            || surface_size_uniform < 0
            || border_width_uniform < 0
            || border_color_uniform < 0
            || shadow_mode_uniform < 0
            || shadow_extent_uniform < 0
            || shadow_strength_uniform < 0
            || shadow_color_uniform < 0
            || surface_opacity_uniform < 0
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
        let shadow_branch = source.find("if(shadow_mode!=0)").unwrap();
        let texture_sample = source.find("vec4 sampled=texture(captured,texcoord)").unwrap();
        assert!(shadow_branch < texture_sample);
        assert!(source.contains("shadow_distance"));
        assert!(source.contains("shadow_strength"));
        assert!(source.contains("shadow_color"));
        assert!(source.contains("color=vec4(shadow_color*alpha,alpha)"));
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
const SCENE_FRAGMENT_SHADER: &str = "#version 330 core\nin vec2 texcoord;\nin vec2 local_position;\nout vec4 color;\nuniform sampler2D captured;\nuniform int shadow_mode;\nuniform float shadow_extent;\nuniform float shadow_strength;\nuniform vec3 shadow_color;\nuniform float surface_opacity;\nuniform float corner_radius;\nuniform vec2 surface_size;\nuniform float border_width;\nuniform vec4 border_color;\nfloat rounded_distance(vec2 point, vec2 size, float radius){ vec2 q=abs(point-size*0.5)-(size*0.5-vec2(radius)); return length(max(q,vec2(0.0)))+min(max(q.x,q.y),0.0)-radius; }\nfloat coverage(float distance){ float aa=max(fwidth(distance),0.0001); return 1.0-smoothstep(-aa,aa,distance); }\nvoid main(){ float outer_radius=min(corner_radius,min(surface_size.x,surface_size.y)*0.5); if(shadow_mode!=0){ vec2 shadow_point=local_position-vec2(shadow_extent); float shadow_distance=rounded_distance(shadow_point,surface_size,outer_radius); float edge=coverage(-shadow_distance); float falloff=1.0-smoothstep(0.0,max(shadow_extent,0.0001),max(shadow_distance,0.0)); float alpha=shadow_strength*edge*falloff; color=vec4(shadow_color*alpha,alpha); return; } vec4 sampled=texture(captured,texcoord); if(border_width<=0.0){ if(corner_radius<=0.0){ color=sampled*surface_opacity; return; } color=sampled*coverage(rounded_distance(local_position,surface_size,outer_radius))*surface_opacity; return; } float width=min(border_width,min(surface_size.x,surface_size.y)*0.5); float outer=coverage(rounded_distance(local_position,surface_size,outer_radius)); vec2 inner_size=max(surface_size-vec2(2.0*width),vec2(0.0)); float inner_radius=max(outer_radius-width,0.0); float inner=inner_size.x>0.0 && inner_size.y>0.0 ? coverage(rounded_distance(local_position-vec2(width),inner_size,inner_radius)) : 0.0; float border=clamp(outer-inner,0.0,1.0); vec4 premultiplied_border=vec4(border_color.rgb*border_color.a,border_color.a)*border; color=sampled*inner*surface_opacity+premultiplied_border; }";
#[allow(dead_code)]
const BLUR_VERTEX_SHADER: &str = "#version 330 core\nlayout(location=0) in vec2 position;\nvoid main(){ gl_Position=vec4(position,0.0,1.0); }";
#[allow(dead_code)]
const BLUR_FRAGMENT_SHADER: &str = "#version 330 core\nout vec4 color;\nuniform sampler2D source;\nuniform vec2 texture_size;\nuniform vec2 direction;\nuniform float radius;\nvoid main(){ vec2 texcoord=gl_FragCoord.xy/texture_size; vec2 step_uv=direction*radius/texture_size; vec4 result=texture(source,texcoord)*0.22702703; result+=(texture(source,texcoord+step_uv)+texture(source,texcoord-step_uv))*0.19459459; result+=(texture(source,texcoord+2.0*step_uv)+texture(source,texcoord-2.0*step_uv))*0.12162162; result+=(texture(source,texcoord+3.0*step_uv)+texture(source,texcoord-3.0*step_uv))*0.05405405; result+=(texture(source,texcoord+4.0*step_uv)+texture(source,texcoord-4.0*step_uv))*0.01621622; color=result; }";
const BACKDROP_VERTEX_SHADER: &str = "#version 330 core\nlayout(location=0) in vec2 position;\nlayout(location=1) in vec2 uv;\nlayout(location=2) in vec2 local_position_in;\nout vec2 texcoord;\nout vec2 local_position;\nvoid main(){ gl_Position=vec4(position,0.0,1.0); texcoord=uv; local_position=local_position_in; }";
const BACKDROP_FRAGMENT_SHADER: &str = "#version 330 core\nin vec2 texcoord;\nin vec2 local_position;\nout vec4 color;\nuniform sampler2D blurred_root;\nuniform vec2 surface_size;\nuniform float corner_radius;\nfloat rounded_distance(vec2 point, vec2 size, float radius){ vec2 q=abs(point-size*0.5)-(size*0.5-vec2(radius)); return length(max(q,vec2(0.0)))+min(max(q.x,q.y),0.0)-radius; }\nfloat coverage(float distance){ float aa=max(fwidth(distance),0.0001); return 1.0-smoothstep(-aa,aa,distance); }\nvoid main(){ float radius=min(corner_radius,min(surface_size.x,surface_size.y)*0.5); float c=coverage(rounded_distance(local_position,surface_size,radius)); vec3 blurred=texture(blurred_root,texcoord).rgb; color=vec4(blurred*c,c); }";
