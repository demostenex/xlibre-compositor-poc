use std::ffi::CStr;
use std::error::Error;
use std::ffi::c_void;

pub struct CaptureRenderer { program: u32, vao: u32, texture: u32 }

pub struct SceneRenderer {
    program: u32,
    vao: u32,
    buffer: u32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BlendState {
    Disabled,
    PremultipliedAlpha,
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

        let mut vao = 0;
        let mut buffer = 0;
        unsafe {
            gl::GenVertexArrays(1, &mut vao);
            gl::GenBuffers(1, &mut buffer);
            gl::BindVertexArray(vao);
            gl::BindBuffer(gl::ARRAY_BUFFER, buffer);
            gl::BufferData(gl::ARRAY_BUFFER, 0, std::ptr::null(), gl::STREAM_DRAW);
            gl::VertexAttribPointer(0, 2, gl::FLOAT, gl::FALSE, 16, std::ptr::null());
            gl::VertexAttribPointer(1, 2, gl::FLOAT, gl::FALSE, 16, 8 as *const c_void);
            gl::EnableVertexAttribArray(0);
            gl::EnableVertexAttribArray(1);
            gl::BindVertexArray(0);
            gl::BindBuffer(gl::ARRAY_BUFFER, 0);
        }
        Ok(Self { program, vao, buffer })
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
        let vertices: [f32; 24] = [
            left, bottom, plan.u0, plan.v1,
            right, bottom, plan.u1, plan.v1,
            right, top, plan.u1, plan.v0,
            left, bottom, plan.u0, plan.v1,
            right, top, plan.u1, plan.v0,
            left, top, plan.u0, plan.v0,
        ];
        unsafe {
            check_gl_error("before scene draw")?;
            gl::UseProgram(self.program);
            match blend_state_for(pixel_semantics) {
                Some(BlendState::Disabled) => gl::Disable(gl::BLEND),
                Some(BlendState::PremultipliedAlpha) => {
                    gl::Enable(gl::BLEND);
                    gl::BlendFunc(gl::ONE, gl::ONE_MINUS_SRC_ALPHA);
                }
                None => return Err("unsupported pixel semantics reached GL renderer".into()),
            }
            check_gl_error("blend state")?;
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
    use super::{blend_state_for, BlendState};
    use crate::x11::scene::EglPixelSemantics;

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
}

const VERTEX_SHADER: &str = "#version 330 core\nlayout(location=0) in vec2 position;\nlayout(location=1) in vec2 uv;\nout vec2 texcoord;\nvoid main(){ gl_Position=vec4(position,0.0,1.0); texcoord=uv; }";
const FRAGMENT_SHADER: &str = "#version 330 core\nin vec2 texcoord;\nout vec4 color;\nuniform sampler2D captured;\nvoid main(){ color=texture(captured,texcoord); }";
