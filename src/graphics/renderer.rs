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
        })
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
    let shader = unsafe { gl::CreateShader(kind) };
    let source = std::ffi::CString::new(source)?;
    unsafe { gl::ShaderSource(shader, 1, &source.as_ptr(), std::ptr::null()); gl::CompileShader(shader); }
    let mut status = 0;
    unsafe { gl::GetShaderiv(shader, gl::COMPILE_STATUS, &mut status); }
    if status == 0 { return Err(shader_log(shader).into()); }
    Ok(shader)
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
        build_shadow_quad_plan, BlendState, ShadowParams, SurfaceOpacity,
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
}

const VERTEX_SHADER: &str = "#version 330 core\nlayout(location=0) in vec2 position;\nlayout(location=1) in vec2 uv;\nout vec2 texcoord;\nvoid main(){ gl_Position=vec4(position,0.0,1.0); texcoord=uv; }";
const FRAGMENT_SHADER: &str = "#version 330 core\nin vec2 texcoord;\nout vec4 color;\nuniform sampler2D captured;\nvoid main(){ color=texture(captured,texcoord); }";
const SCENE_VERTEX_SHADER: &str = "#version 330 core\nlayout(location=0) in vec2 position;\nlayout(location=1) in vec2 uv;\nlayout(location=2) in vec2 local_position_in;\nout vec2 texcoord;\nout vec2 local_position;\nvoid main(){ gl_Position=vec4(position,0.0,1.0); texcoord=uv; local_position=local_position_in; }";
const SCENE_FRAGMENT_SHADER: &str = "#version 330 core\nin vec2 texcoord;\nin vec2 local_position;\nout vec4 color;\nuniform sampler2D captured;\nuniform int shadow_mode;\nuniform float shadow_extent;\nuniform float shadow_strength;\nuniform vec3 shadow_color;\nuniform float surface_opacity;\nuniform float corner_radius;\nuniform vec2 surface_size;\nuniform float border_width;\nuniform vec4 border_color;\nfloat rounded_distance(vec2 point, vec2 size, float radius){ vec2 q=abs(point-size*0.5)-(size*0.5-vec2(radius)); return length(max(q,vec2(0.0)))+min(max(q.x,q.y),0.0)-radius; }\nfloat coverage(float distance){ float aa=max(fwidth(distance),0.0001); return 1.0-smoothstep(-aa,aa,distance); }\nvoid main(){ float outer_radius=min(corner_radius,min(surface_size.x,surface_size.y)*0.5); if(shadow_mode!=0){ vec2 shadow_point=local_position-vec2(shadow_extent); float shadow_distance=rounded_distance(shadow_point,surface_size,outer_radius); float edge=coverage(-shadow_distance); float falloff=1.0-smoothstep(0.0,max(shadow_extent,0.0001),max(shadow_distance,0.0)); float alpha=shadow_strength*edge*falloff; color=vec4(shadow_color*alpha,alpha); return; } vec4 sampled=texture(captured,texcoord); if(border_width<=0.0){ if(corner_radius<=0.0){ color=sampled*surface_opacity; return; } color=sampled*coverage(rounded_distance(local_position,surface_size,outer_radius))*surface_opacity; return; } float width=min(border_width,min(surface_size.x,surface_size.y)*0.5); float outer=coverage(rounded_distance(local_position,surface_size,outer_radius)); vec2 inner_size=max(surface_size-vec2(2.0*width),vec2(0.0)); float inner_radius=max(outer_radius-width,0.0); float inner=inner_size.x>0.0 && inner_size.y>0.0 ? coverage(rounded_distance(local_position-vec2(width),inner_size,inner_radius)) : 0.0; float border=clamp(outer-inner,0.0,1.0); vec4 premultiplied_border=vec4(border_color.rgb*border_color.a,border_color.a)*border; color=sampled*inner*surface_opacity+premultiplied_border; }";
