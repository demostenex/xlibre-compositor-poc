use std::error::Error;
use std::ffi::c_void;
use std::sync::Arc;

use khronos_egl as egl;
use libloading::Library;

use crate::graphics::renderer;
use crate::x11::connection::X11Connection;

// EGL_EXT_platform_xcb, EGL/eglext.h:
// EGL_PLATFORM_XCB_EXT = 0x31DC
// EGL_PLATFORM_XCB_SCREEN_EXT = 0x31DE
const EGL_PLATFORM_XCB_EXT: egl::Enum = 0x31DC;
const EGL_PLATFORM_XCB_SCREEN_EXT: egl::Attrib = 0x31DE;

pub struct EglContext {
    instance: Arc<egl::DynamicInstance<egl::EGL1_5>>,
    display: egl::Display,
    context: egl::Context,
    surface: Option<egl::Surface>,
    window: Option<u32>,
    colormap: Option<u32>,
    connection: Option<X11Connection>,
    _native_window: Option<Box<u32>>,
    width: i32,
    height: i32,
}

impl EglContext {
    pub fn diagnostics(connection: &X11Connection) -> Result<Self, Box<dyn Error>> {
        let (instance, display, config) = Self::base(connection)?;
        let context_attributes = [egl::CONTEXT_MAJOR_VERSION, 3, egl::CONTEXT_MINOR_VERSION, 3, egl::CONTEXT_OPENGL_PROFILE_MASK, egl::CONTEXT_OPENGL_CORE_PROFILE_BIT, egl::NONE];
        let context = instance.create_context(display, config, None, &context_attributes)?;
        instance.make_current(display, None, None, Some(context))?;
        renderer::load(|name| instance.get_proc_address(name).map_or(std::ptr::null(), |pointer| pointer as *const c_void));

        Ok(Self { instance, display, context, surface: None, window: None, colormap: None, connection: None, _native_window: None, width: 1, height: 1 })
    }

    pub fn create(mut connection: X11Connection) -> Result<Self, Box<dyn Error>> {
        let (instance, display, config) = Self::base(&connection)?;
        let visual_id = instance.get_config_attrib(display, config, egl::NATIVE_VISUAL_ID)? as u32;
        let depth = connection.visual_depth(visual_id).ok_or("EGL visual is not present in X11 setup")?;
        let colormap = connection.create_colormap(visual_id)?;
        let window = connection.create_window(visual_id, depth, colormap)?;
        let mut native_window = Box::new(window);

        let surface = unsafe {
            instance.create_platform_window_surface(display, config, (&mut *native_window as *mut u32).cast::<c_void>(), &[egl::ATTRIB_NONE])?
        };

        let context_attributes = [egl::CONTEXT_MAJOR_VERSION, 3, egl::CONTEXT_MINOR_VERSION, 3, egl::CONTEXT_OPENGL_PROFILE_MASK, egl::CONTEXT_OPENGL_CORE_PROFILE_BIT, egl::NONE];
        let context = instance.create_context(display, config, None, &context_attributes)?;
        instance.make_current(display, Some(surface), Some(surface), Some(context))?;
        instance.swap_interval(display, 1)?;
        renderer::load(|name| instance.get_proc_address(name).map_or(std::ptr::null(), |pointer| pointer as *const c_void));
        renderer::resize(640, 360);

        Ok(Self { instance, display, context, surface: Some(surface), window: Some(window), colormap: Some(colormap), connection: Some(connection), _native_window: Some(native_window), width: 640, height: 360 })
    }

    fn base(connection: &X11Connection) -> Result<(Arc<egl::DynamicInstance<egl::EGL1_5>>, egl::Display, egl::Config), Box<dyn Error>> {
        let library = unsafe { Library::new("libEGL.so.1")? };
        let instance = Arc::new(unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required_from(library)? });
        let display_attributes = [EGL_PLATFORM_XCB_SCREEN_EXT, connection.screen_num() as egl::Attrib, egl::ATTRIB_NONE];
        let display = unsafe { instance.get_platform_display(EGL_PLATFORM_XCB_EXT, connection.inner.get_raw_xcb_connection().cast::<c_void>(), &display_attributes)? };
        instance.initialize(display)?;
        instance.bind_api(egl::OPENGL_API)?;

        let config_attributes = [egl::SURFACE_TYPE, egl::WINDOW_BIT, egl::RENDERABLE_TYPE, egl::OPENGL_BIT, egl::RED_SIZE, 8, egl::GREEN_SIZE, 8, egl::BLUE_SIZE, 8, egl::ALPHA_SIZE, 8, egl::NONE];
        let config = instance.choose_first_config(display, &config_attributes)?.ok_or("no suitable EGL config")?;
        Ok((instance, display, config))
    }

    pub fn print(&self) {
        println!("\nEGL:\nversion: {}\nvendor: {}\n\nOpenGL:\nrenderer: {}\nversion: {}\nGLSL: {}", self.query(egl::VERSION), self.query(egl::VENDOR), renderer::string(gl::RENDERER), renderer::string(gl::VERSION), renderer::string(gl::SHADING_LANGUAGE_VERSION));
    }

    fn query(&self, attribute: egl::Int) -> String {
        self.instance.query_string(Some(self.display), attribute).ok().map(|value| value.to_string_lossy().into_owned()).unwrap_or_else(|| "unavailable".to_owned())
    }

    pub fn render(&self) { renderer::render(); }

    pub fn swap_buffers(&self) -> Result<(), Box<dyn Error>> {
        if let Some(surface) = self.surface { self.instance.swap_buffers(self.display, surface)?; }
        Ok(())
    }

    pub fn resize(&mut self, width: i32, height: i32) {
        self.width = width.max(1);
        self.height = height.max(1);
        renderer::resize(self.width, self.height);
    }

    pub fn window(&self) -> Option<u32> { self.window }

    pub fn run_event_loop(&mut self) -> Result<(), Box<dyn Error>> {
        let mut connection = self.connection.take().ok_or("X11 connection is not available")?;
        let result = connection.run_event_loop(self);
        self.connection = Some(connection);
        result
    }
}

impl Drop for EglContext {
    fn drop(&mut self) {
        let _ = self.instance.make_current(self.display, None, None, None);

        if let Some(surface) = self.surface { let _ = self.instance.destroy_surface(self.display, surface); }
        let _ = self.instance.destroy_context(self.display, self.context);

        if let Some(connection) = self.connection.as_ref() {
            if let Some(window) = self.window { let _ = connection.destroy_window(window); }
            if let Some(colormap) = self.colormap { let _ = connection.free_colormap(colormap); }
        }

        let _ = self.instance.terminate(self.display);
    }
}
