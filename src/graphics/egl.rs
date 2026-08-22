use std::error::Error;
use std::ffi::c_void;
use std::sync::Arc;

use khronos_egl as egl;
use libloading::Library;
use x11rb::protocol::damage;

use crate::graphics::renderer;
use crate::x11::capture::CapturedPixmap;
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
    captured_image: Option<egl::Image>,
    captured_pixmap: Option<u32>,
    capture_renderer: Option<renderer::CaptureRenderer>,
    damage: Option<damage::Damage>,
    source_window: Option<u32>,
    source_size: Option<(u16, u16)>,
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

        Ok(Self { instance, display, context, surface: None, window: None, colormap: None, connection: None, _native_window: None, captured_image: None, captured_pixmap: None, capture_renderer: None, damage: None, source_window: None, source_size: None, width: 1, height: 1 })
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

        Ok(Self { instance, display, context, surface: Some(surface), window: Some(window), colormap: Some(colormap), connection: Some(connection), _native_window: Some(native_window), captured_image: None, captured_pixmap: None, capture_renderer: None, damage: None, source_window: None, source_size: None, width: 640, height: 360 })
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

    pub fn render(&self) {
        renderer::render();
        if let Some(capture) = self.capture_renderer.as_ref() { capture.render(); }
    }

    pub fn import_pixmap(&mut self, capture: CapturedPixmap) -> Result<(), Box<dyn Error>> {
        const EGL_NATIVE_PIXMAP_KHR: egl::Enum = 0x30B0;
        let connection = self.connection.as_ref().ok_or("X11 connection is not available")?;
        let damage = connection.create_damage(capture.window)?;
        self.damage = Some(damage);
        self.source_window = Some(capture.window);
        self.source_size = Some((capture.width, capture.height));
        println!("Damage: created (report level: NonEmpty)");
        let client_buffer = unsafe { egl::ClientBuffer::from_ptr(capture.pixmap as usize as *mut c_void) };
        self.check_import_capabilities()?;
        let image_attributes: [egl::Int; 3] = [
            egl::IMAGE_PRESERVED,
            egl::TRUE as egl::Int,
            egl::NONE,
        ];
        let image = unsafe {
            create_native_pixmap_image(
                &self.instance,
                self.display,
                EGL_NATIVE_PIXMAP_KHR,
                client_buffer.as_ptr(),
                &image_attributes,
            )?
        };
        let proc = self.instance.get_proc_address("glEGLImageTargetTexture2DOES").ok_or("glEGLImageTargetTexture2DOES is unavailable")?;
        let image_target: unsafe extern "system" fn(u32, *const c_void) = unsafe { std::mem::transmute(proc) };
        let texture = renderer::create_egl_texture(image_target, image.as_ptr().cast())?;
        let capture_renderer = renderer::CaptureRenderer::new(texture)?;
        println!("Import:");
        println!("backend: EGL_KHR_image_pixmap");
        println!("texture: OK");
        self.captured_image = Some(image);
        self.captured_pixmap = Some(capture.pixmap);
        self.capture_renderer = Some(capture_renderer);
        Ok(())
    }

    fn check_import_capabilities(&self) -> Result<(), Box<dyn Error>> {
        for extension in [
            "EGL_KHR_image",
            "EGL_KHR_image_base",
            "EGL_KHR_image_pixmap",
        ] {
            let supported = self.instance.query_string(Some(self.display), egl::EXTENSIONS)
                .ok()
                .map(|value| value.to_string_lossy().split_whitespace().any(|item| item == extension))
                .unwrap_or(false);
            println!("{extension}: {}", if supported { "yes" } else { "no" });
            if !supported { return Err(format!("required EGL extension is unavailable: {extension}").into()); }
        }

        let supported = renderer::has_extension("GL_OES_EGL_image");
        println!("GL_OES_EGL_image: {}", if supported { "yes" } else { "no" });
        if !supported { return Err("required OpenGL extension is unavailable: GL_OES_EGL_image".into()); }
        Ok(())
    }

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

    pub fn damage(&self) -> Option<damage::Damage> { self.damage }

    pub fn source_window(&self) -> Option<u32> { self.source_window }

    pub fn source_size(&self) -> Option<(u16, u16)> { self.source_size }

    pub fn run_event_loop(&mut self) -> Result<(), Box<dyn Error>> {
        let mut connection = self.connection.take().ok_or("X11 connection is not available")?;
        let result = connection.run_event_loop(self);
        self.connection = Some(connection);
        result
    }
}

unsafe fn create_native_pixmap_image(
    instance: &egl::DynamicInstance<egl::EGL1_5>,
    display: egl::Display,
    target: egl::Enum,
    buffer: egl::EGLClientBuffer,
    attributes: &[egl::Int],
) -> Result<egl::Image, Box<dyn Error>> {
    type CreateImageKhr = unsafe extern "system" fn(
        egl::EGLDisplay,
        egl::EGLContext,
        egl::Enum,
        egl::EGLClientBuffer,
        *const egl::Int,
    ) -> egl::EGLImage;

    let symbol = instance
        .get_proc_address("eglCreateImageKHR")
        .ok_or("eglCreateImageKHR is unavailable")?;
    let create_image: CreateImageKhr = unsafe { std::mem::transmute(symbol) };
    let image = unsafe {
        create_image(
            display.as_ptr(),
            egl::NO_CONTEXT,
            target,
            buffer,
            attributes.as_ptr(),
        )
    };

    if image == egl::NO_IMAGE {
        return Err(format!("eglCreateImageKHR failed: {:?}", instance.get_error()).into());
    }

    Ok(unsafe { egl::Image::from_ptr(image) })
}

impl Drop for EglContext {
    fn drop(&mut self) {
        self.capture_renderer.take();
        let _ = self.instance.make_current(self.display, None, None, None);

        if let Some(surface) = self.surface { let _ = self.instance.destroy_surface(self.display, surface); }
        let _ = self.instance.destroy_context(self.display, self.context);

        if let Some(image) = self.captured_image {
            let _ = self.instance.destroy_image(self.display, image);
        }

        if let Some(connection) = self.connection.as_ref() {
            if let Some(damage) = self.damage { let _ = connection.destroy_damage(damage); }
            if let Some(window) = self.window { let _ = connection.destroy_window(window); }
            if let Some(colormap) = self.colormap { let _ = connection.free_colormap(colormap); }
            if let Some(pixmap) = self.captured_pixmap { let _ = connection.free_pixmap(pixmap); }
        }

        let _ = self.instance.terminate(self.display);
    }
}
