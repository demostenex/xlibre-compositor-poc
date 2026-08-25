use std::error::Error;
use std::ffi::c_void;
use std::sync::Arc;

use khronos_egl as egl;
use libloading::Library;
use x11rb::connection::Connection;
use x11rb::protocol::damage;
use x11rb::protocol::xproto::ConnectionExt as XprotoConnectionExt;

use crate::graphics::renderer;
use crate::x11::capture::CapturedPixmap;
use crate::x11::connection::X11Connection;

// EGL_EXT_platform_xcb, EGL/eglext.h:
// EGL_PLATFORM_XCB_EXT = 0x31DC
// EGL_PLATFORM_XCB_SCREEN_EXT = 0x31DE
const EGL_PLATFORM_XCB_EXT: egl::Enum = 0x31DC;
const EGL_PLATFORM_XCB_SCREEN_EXT: egl::Attrib = 0x31DE;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CaptureState {
    Active,
    Suspended,
    Destroyed,
}

pub struct EglContext<'a> {
    instance: Arc<egl::DynamicInstance<egl::EGL1_5>>,
    display: egl::Display,
    context: egl::Context,
    surface: Option<egl::Surface>,
    window: Option<u32>,
    colormap: Option<u32>,
    connection: &'a X11Connection,
    _native_window: Option<Box<u32>>,
    captured_image: Option<egl::Image>,
    captured_pixmap: Option<u32>,
    capture_renderer: Option<renderer::CaptureRenderer>,
    damage: Option<damage::Damage>,
    source_window: Option<u32>,
    source_size: Option<(u16, u16)>,
    capture_state: Option<CaptureState>,
    width: i32,
    height: i32,
}

impl<'a> EglContext<'a> {
    pub fn diagnostics(connection: &'a X11Connection) -> Result<EglContext<'a>, Box<dyn Error>> {
        let (instance, display, config) = Self::base(connection)?;
        let context_attributes = [
            egl::CONTEXT_MAJOR_VERSION,
            3,
            egl::CONTEXT_MINOR_VERSION,
            3,
            egl::CONTEXT_OPENGL_PROFILE_MASK,
            egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
            egl::NONE,
        ];
        let context = instance.create_context(display, config, None, &context_attributes)?;
        instance.make_current(display, None, None, Some(context))?;
        renderer::load(|name| {
            instance
                .get_proc_address(name)
                .map_or(std::ptr::null(), |pointer| pointer as *const c_void)
        });

        Ok(Self {
            instance,
            display,
            context,
            surface: None,
            window: None,
            colormap: None,
            connection,
            _native_window: None,
            captured_image: None,
            captured_pixmap: None,
            capture_renderer: None,
            damage: None,
            source_window: None,
            source_size: None,
            capture_state: None,
            width: 1,
            height: 1,
        })
    }

    pub fn create(connection: &'a X11Connection) -> Result<EglContext<'a>, Box<dyn Error>> {
        let (instance, display, config) = Self::base(connection)?;
        let visual_id = instance.get_config_attrib(display, config, egl::NATIVE_VISUAL_ID)? as u32;
        let depth = connection
            .visual_depth(visual_id)
            .ok_or("EGL visual is not present in X11 setup")?;
        let colormap = connection.create_colormap(visual_id)?;
        let window = connection.create_window(visual_id, depth, colormap)?;
        let mut native_window = Box::new(window);

        let surface = unsafe {
            instance.create_platform_window_surface(
                display,
                config,
                (&mut *native_window as *mut u32).cast::<c_void>(),
                &[egl::ATTRIB_NONE],
            )?
        };

        let context_attributes = [
            egl::CONTEXT_MAJOR_VERSION,
            3,
            egl::CONTEXT_MINOR_VERSION,
            3,
            egl::CONTEXT_OPENGL_PROFILE_MASK,
            egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
            egl::NONE,
        ];
        let context = instance.create_context(display, config, None, &context_attributes)?;
        instance.make_current(display, Some(surface), Some(surface), Some(context))?;
        if let Err(error) = instance.swap_interval(display, 1) {
            let _ = instance.make_current(display, None, None, None);
            let _ = instance.destroy_context(display, context);
            let _ = instance.destroy_surface(display, surface);
            let _ = instance.terminate(display);
            return Err(error.into());
        }
        renderer::load(|name| {
            instance
                .get_proc_address(name)
                .map_or(std::ptr::null(), |pointer| pointer as *const c_void)
        });
        renderer::resize(640, 360);

        Ok(Self {
            instance,
            display,
            context,
            surface: Some(surface),
            window: Some(window),
            colormap: Some(colormap),
            connection,
            _native_window: Some(native_window),
            captured_image: None,
            captured_pixmap: None,
            capture_renderer: None,
            damage: None,
            source_window: None,
            source_size: None,
            capture_state: None,
            width: 640,
            height: 360,
        })
    }

    pub(crate) fn base_display(
        connection: &X11Connection,
    ) -> Result<(Arc<egl::DynamicInstance<egl::EGL1_5>>, egl::Display), Box<dyn Error>> {
        let library = unsafe { Library::new("libEGL.so.1")? };
        let instance =
            Arc::new(unsafe { egl::DynamicInstance::<egl::EGL1_5>::load_required_from(library)? });
        let display_attributes = [
            EGL_PLATFORM_XCB_SCREEN_EXT,
            connection.screen_num() as egl::Attrib,
            egl::ATTRIB_NONE,
        ];
        let display = unsafe {
            instance.get_platform_display(
                EGL_PLATFORM_XCB_EXT,
                connection.inner.get_raw_xcb_connection().cast::<c_void>(),
                &display_attributes,
            )?
        };
        instance.initialize(display)?;
        instance.bind_api(egl::OPENGL_API)?;
        Ok((instance, display))
    }

    fn base(
        connection: &X11Connection,
    ) -> Result<
        (
            Arc<egl::DynamicInstance<egl::EGL1_5>>,
            egl::Display,
            egl::Config,
        ),
        Box<dyn Error>,
    > {
        let (instance, display) = EglContext::base_display(connection)?;
        let extensions = instance
            .query_string(Some(display), egl::EXTENSIONS)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        for required in [
            "EGL_KHR_image",
            "EGL_KHR_image_base",
            "EGL_KHR_image_pixmap",
        ] {
            if !extensions.split_whitespace().any(|item| item == required) {
                let _ = instance.terminate(display);
                return Err(format!("required EGL extension is unavailable: {required}").into());
            }
        }
        if instance.get_proc_address("eglCreateImageKHR").is_none() {
            let _ = instance.terminate(display);
            return Err("eglCreateImageKHR is unavailable".into());
        }

        let config_attributes = [
            egl::SURFACE_TYPE,
            egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE,
            egl::OPENGL_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
            egl::NONE,
        ];
        let config = instance
            .choose_first_config(display, &config_attributes)?
            .ok_or("no suitable EGL config")?;
        Ok((instance, display, config))
    }

    pub(crate) fn config_preflight(
        connection: &X11Connection,
    ) -> Result<EglConfigReport, Box<dyn Error>> {
        let (instance, display) = Self::base_display(connection)?;
        let result = (|| -> Result<EglConfigReport, Box<dyn Error>> {
            let screen = &connection.inner.setup().roots[connection.screen_num()];
            let config_attributes = [
                egl::SURFACE_TYPE,
                egl::WINDOW_BIT,
                egl::RENDERABLE_TYPE,
                egl::OPENGL_BIT,
                egl::RED_SIZE,
                8,
                egl::GREEN_SIZE,
                8,
                egl::BLUE_SIZE,
                8,
                egl::ALPHA_SIZE,
                8,
                egl::NONE,
            ];
            let count = instance.matching_config_count(display, &config_attributes)?;
            let mut configs = Vec::with_capacity(count);
            instance.choose_config(display, &config_attributes, &mut configs)?;
            let config = configs.into_iter().find(|config| {
                let visual = instance
                    .get_config_attrib(display, *config, egl::NATIVE_VISUAL_ID)
                    .ok()
                    .map(|value| value as u32);
                visual == Some(screen.root_visual)
                    && connection.visual_depth(screen.root_visual) == Some(screen.root_depth)
            }).ok_or("no EGL config matches the root visual and depth")?;
            let visual = instance.get_config_attrib(display, config, egl::NATIVE_VISUAL_ID)? as u32;
            let depth = connection
                .visual_depth(visual)
                .ok_or("EGL native visual is not present in X11 setup")?;
            let required_extensions = [
                "EGL_KHR_image",
                "EGL_KHR_image_base",
                "EGL_KHR_image_pixmap",
            ];
            let egl_extensions = instance
                .query_string(Some(display), egl::EXTENSIONS)
                .ok()
                .map(|value| value.to_string_lossy().into_owned())
                .unwrap_or_default();
            for extension in required_extensions {
                if !egl_extensions.split_whitespace().any(|item| item == extension) {
                    return Err(format!("required EGL extension is unavailable: {extension}").into());
                }
            }

            let context_attributes = [
                egl::CONTEXT_MAJOR_VERSION,
                3,
                egl::CONTEXT_MINOR_VERSION,
                3,
                egl::CONTEXT_OPENGL_PROFILE_MASK,
                egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
                egl::NONE,
            ];
            let context = instance.create_context(display, config, None, &context_attributes)?;
            let context_result = (|| -> Result<(), Box<dyn Error>> {
                instance.make_current(display, None, None, Some(context))?;
                renderer::load(|name| {
                    instance
                        .get_proc_address(name)
                        .map_or(std::ptr::null(), |pointer| pointer as *const c_void)
                });
                if !renderer::has_extension("GL_OES_EGL_image") {
                    return Err("required OpenGL extension is unavailable: GL_OES_EGL_image".into());
                }
                Ok(())
            })();
            let _ = instance.make_current(display, None, None, None);
            let _ = instance.destroy_context(display, context);
            context_result?;
            Ok(EglConfigReport { visual, depth })
        })();
        let _ = instance.terminate(display);
        result
    }

    pub fn print(&self) {
        println!(
            "\nEGL:\nversion: {}\nvendor: {}\n\nOpenGL:\nrenderer: {}\nversion: {}\nGLSL: {}",
            self.query(egl::VERSION),
            self.query(egl::VENDOR),
            renderer::string(gl::RENDERER),
            renderer::string(gl::VERSION),
            renderer::string(gl::SHADING_LANGUAGE_VERSION)
        );
    }

    fn query(&self, attribute: egl::Int) -> String {
        self.instance
            .query_string(Some(self.display), attribute)
            .ok()
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_else(|| "unavailable".to_owned())
    }

    pub fn render(&self) {
        renderer::render();
        if let Some(capture) = self.capture_renderer.as_ref() {
            capture.render();
        }
    }

    pub fn import_pixmap(&mut self, capture: CapturedPixmap) -> Result<(), Box<dyn Error>> {
        let damage = self.connection.create_damage(capture.window)?;
        self.damage = Some(damage);
        self.source_window = Some(capture.window);
        self.source_size = Some((capture.width, capture.height));
        println!("Damage: created (report level: NonEmpty)");
        self.check_import_capabilities()?;
        let (image, texture) = self.create_capture_resources(capture.pixmap)?;
        let capture_renderer = match renderer::CaptureRenderer::new(texture) {
            Ok(renderer) => renderer,
            Err(error) => {
                renderer::delete_texture(texture);
                let _ = self.instance.destroy_image(self.display, image);
                return Err(error);
            }
        };
        println!("Import:");
        println!("backend: EGL_KHR_image_pixmap");
        println!("texture: OK");
        self.captured_image = Some(image);
        self.captured_pixmap = Some(capture.pixmap);
        self.capture_renderer = Some(capture_renderer);
        self.capture_state = Some(CaptureState::Active);
        Ok(())
    }

    fn create_capture_resources(&self, pixmap: u32) -> Result<(egl::Image, u32), Box<dyn Error>> {
        const EGL_NATIVE_PIXMAP_KHR: egl::Enum = 0x30B0;
        let client_buffer = unsafe { egl::ClientBuffer::from_ptr(pixmap as usize as *mut c_void) };
        let image_attributes: [egl::Int; 3] =
            [egl::IMAGE_PRESERVED, egl::TRUE as egl::Int, egl::NONE];
        let image = unsafe {
            create_native_pixmap_image(
                &self.instance,
                self.display,
                EGL_NATIVE_PIXMAP_KHR,
                client_buffer.as_ptr(),
                &image_attributes,
            )?
        };
        let proc = self
            .instance
            .get_proc_address("glEGLImageTargetTexture2DOES")
            .ok_or("glEGLImageTargetTexture2DOES is unavailable")?;
        let image_target: unsafe extern "system" fn(u32, *const c_void) =
            unsafe { std::mem::transmute(proc) };
        let texture = match renderer::create_egl_texture(image_target, image.as_ptr().cast()) {
            Ok(texture) => texture,
            Err(error) => {
                let _ = self.instance.destroy_image(self.display, image);
                return Err(error);
            }
        };
        Ok((image, texture))
    }

    pub fn resize_capture(&mut self, width: u16, height: u16) -> Result<(), Box<dyn Error>> {
        if self.capture_state != Some(CaptureState::Active) {
            return Ok(());
        }
        self.recreate_capture(width, height)
    }

    pub fn suspend_capture(&mut self) {
        if self.capture_state == Some(CaptureState::Active) {
            self.capture_state = Some(CaptureState::Suspended);
        }
    }

    pub fn resume_capture(&mut self) -> Result<bool, Box<dyn Error>> {
        if self.capture_state != Some(CaptureState::Suspended) {
            return Ok(false);
        }
        let window = self
            .source_window
            .ok_or("capture source window is not available")?;
        let attributes = self
            .connection
            .inner
            .get_window_attributes(window)?
            .reply()?;
        if attributes.map_state != x11rb::protocol::xproto::MapState::VIEWABLE {
            return Ok(false);
        }
        let geometry = self.connection.inner.get_geometry(window)?.reply()?;
        self.recreate_capture(geometry.width, geometry.height)?;
        self.clear_damage()?;
        self.capture_state = Some(CaptureState::Active);
        Ok(true)
    }

    pub fn destroy_capture(&mut self) {
        self.capture_state = Some(CaptureState::Destroyed);
        self.capture_renderer.take();
        if let Some(image) = self.captured_image.take() {
            let _ = self.instance.destroy_image(self.display, image);
        }
        if let Some(damage) = self.damage.take() {
            let _ = self.connection.destroy_damage(damage);
        }
        if let Some(pixmap) = self.captured_pixmap.take() {
            let _ = self.connection.free_pixmap(pixmap);
        }
        self.source_window = None;
        self.source_size = None;
    }

    fn clear_damage(&self) -> Result<(), Box<dyn Error>> {
        if let Some(damage) = self.damage {
            self.connection.subtract_damage(damage)?;
        }
        Ok(())
    }

    fn recreate_capture(&mut self, width: u16, height: u16) -> Result<(), Box<dyn Error>> {
        let window = self
            .source_window
            .ok_or("capture source window is not available")?;
        let new_pixmap = self.connection.inner.generate_id()?;
        if let Err(error) = self.connection.name_window_pixmap(window, new_pixmap) {
            return Err(error);
        }
        println!("Recreating capture:");
        println!("new pixmap: 0x{new_pixmap:08x}");

        let (new_image, new_texture) = match self.create_capture_resources(new_pixmap) {
            Ok(resources) => resources,
            Err(error) => {
                let _ = self.connection.free_pixmap(new_pixmap);
                return Err(error);
            }
        };
        println!("EGLImage: OK");
        println!("texture: OK");

        let old_texture = match self.capture_renderer.as_mut() {
            Some(capture_renderer) => capture_renderer.replace_texture(new_texture),
            None => {
                renderer::delete_texture(new_texture);
                let _ = self.instance.destroy_image(self.display, new_image);
                let _ = self.connection.free_pixmap(new_pixmap);
                return Err("capture renderer is not available".into());
            }
        };
        let old_image = self.captured_image.replace(new_image);
        let old_pixmap = self.captured_pixmap.replace(new_pixmap);
        self.source_size = Some((width, height));

        renderer::delete_texture(old_texture);
        if let Some(image) = old_image {
            let _ = self.instance.destroy_image(self.display, image);
        }
        if let Some(pixmap) = old_pixmap {
            let _ = self.connection.free_pixmap(pixmap);
        }
        println!("capture resources replaced");
        Ok(())
    }

    fn check_import_capabilities(&self) -> Result<(), Box<dyn Error>> {
        for extension in [
            "EGL_KHR_image",
            "EGL_KHR_image_base",
            "EGL_KHR_image_pixmap",
        ] {
            let supported = self
                .instance
                .query_string(Some(self.display), egl::EXTENSIONS)
                .ok()
                .map(|value| {
                    value
                        .to_string_lossy()
                        .split_whitespace()
                        .any(|item| item == extension)
                })
                .unwrap_or(false);
            println!("{extension}: {}", if supported { "yes" } else { "no" });
            if !supported {
                return Err(format!("required EGL extension is unavailable: {extension}").into());
            }
        }

        let supported = renderer::has_extension("GL_OES_EGL_image");
        println!("GL_OES_EGL_image: {}", if supported { "yes" } else { "no" });
        if !supported {
            return Err("required OpenGL extension is unavailable: GL_OES_EGL_image".into());
        }
        Ok(())
    }

    pub fn swap_buffers(&self) -> Result<(), Box<dyn Error>> {
        if let Some(surface) = self.surface {
            self.instance.swap_buffers(self.display, surface)?;
        }
        Ok(())
    }

    pub fn resize(&mut self, width: i32, height: i32) {
        self.width = width.max(1);
        self.height = height.max(1);
        renderer::resize(self.width, self.height);
    }

    pub fn window(&self) -> Option<u32> {
        self.window
    }

    pub fn damage(&self) -> Option<damage::Damage> {
        self.damage
    }

    pub fn source_window(&self) -> Option<u32> {
        self.source_window
    }

    pub fn source_size(&self) -> Option<(u16, u16)> {
        self.source_size
    }

    pub fn capture_state(&self) -> Option<CaptureState> {
        self.capture_state
    }
}

pub(crate) struct EglConfigReport {
    pub(crate) visual: u32,
    pub(crate) depth: u8,
}

pub struct EglImportedSurface {
    instance: Arc<egl::DynamicInstance<egl::EGL1_5>>,
    display: egl::Display,
    image: egl::Image,
    pub texture: u32,
    pub(crate) pixel_semantics: crate::x11::scene::EglPixelSemantics,
    released: bool,
}

fn claim_imported_surface_release(released: &mut bool) -> bool {
    if *released {
        false
    } else {
        *released = true;
        true
    }
}

impl EglImportedSurface {
    pub fn destroy(
        &mut self,
        instance: &egl::DynamicInstance<egl::EGL1_5>,
        display: egl::Display,
    ) -> Result<(), Box<dyn Error>> {
        if !claim_imported_surface_release(&mut self.released) {
            return Ok(());
        }
        renderer::delete_texture(self.texture);
        instance.destroy_image(display, self.image)?;
        Ok(())
    }

    pub fn disarm(&mut self) {
        self.released = true;
    }
}

impl Drop for EglImportedSurface {
    fn drop(&mut self) {
        if claim_imported_surface_release(&mut self.released) {
            renderer::delete_texture(self.texture);
            let _ = self.instance.destroy_image(self.display, self.image);
        }
    }
}

pub struct EglSceneRenderer {
    instance: Arc<egl::DynamicInstance<egl::EGL1_5>>,
    display: egl::Display,
    context: egl::Context,
    surface: egl::Surface,
    _native_window: Box<u32>,
    scene_renderer: Option<renderer::SceneRenderer>,
    width: u16,
    height: u16,
    disarmed: bool,
}

impl EglSceneRenderer {
    pub fn create(
        connection: &X11Connection,
        overlay: u32,
        visual: u32,
        depth: u8,
        width: u16,
        height: u16,
    ) -> Result<Self, Box<dyn Error>> {
        let (instance, display) = EglContext::base_display(connection)?;
        let egl_extensions = instance
            .query_string(Some(display), egl::EXTENSIONS)
            .map(|value| value.to_string_lossy().into_owned())
            .unwrap_or_default();
        for required in [
            "EGL_KHR_image",
            "EGL_KHR_image_base",
            "EGL_KHR_image_pixmap",
        ] {
            if !egl_extensions.split_whitespace().any(|item| item == required) {
                let _ = instance.terminate(display);
                return Err(format!("required EGL extension is unavailable: {required}").into());
            }
        }
        if instance.get_proc_address("eglCreateImageKHR").is_none() {
            let _ = instance.terminate(display);
            return Err("eglCreateImageKHR is unavailable".into());
        }
        let config_attributes = [
            egl::SURFACE_TYPE, egl::WINDOW_BIT,
            egl::RENDERABLE_TYPE, egl::OPENGL_BIT,
            egl::RED_SIZE, 8, egl::GREEN_SIZE, 8,
            egl::BLUE_SIZE, 8, egl::ALPHA_SIZE, 8, egl::NONE,
        ];
        let count = instance.matching_config_count(display, &config_attributes)?;
        let mut configs = Vec::with_capacity(count);
        instance.choose_config(display, &config_attributes, &mut configs)?;
        let config = configs.into_iter().find(|config| {
            instance.get_config_attrib(display, *config, egl::NATIVE_VISUAL_ID)
                .ok().map(|value| value as u32) == Some(visual)
                && connection.visual_depth(visual) == Some(depth)
        }).ok_or("no EGL scene config matches overlay visual/depth")?;
        let mut native_window = Box::new(overlay);
        let surface = unsafe {
            instance.create_platform_window_surface(
                display,
                config,
                (&mut *native_window as *mut u32).cast::<c_void>(),
                &[
                    egl::RENDER_BUFFER as usize,
                    egl::BACK_BUFFER as usize,
                    egl::ATTRIB_NONE,
                ],
            )?
        };
        let context_attributes = [
            egl::CONTEXT_MAJOR_VERSION, 3,
            egl::CONTEXT_MINOR_VERSION, 3,
            egl::CONTEXT_OPENGL_PROFILE_MASK,
            egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
            egl::NONE,
        ];
        let context = match instance.create_context(display, config, None, &context_attributes) {
            Ok(context) => context,
            Err(error) => {
                let _ = instance.destroy_surface(display, surface);
                return Err(error.into());
            }
        };
        if let Err(error) = instance.make_current(display, Some(surface), Some(surface), Some(context)) {
            let _ = instance.destroy_context(display, context);
            let _ = instance.destroy_surface(display, surface);
            return Err(error.into());
        }
        let render_buffer = match instance.query_context(display, context, egl::RENDER_BUFFER) {
            Ok(value) => value,
            Err(error) => {
                let _ = instance.make_current(display, None, None, None);
                let _ = instance.destroy_context(display, context);
                let _ = instance.destroy_surface(display, surface);
                let _ = instance.terminate(display);
                return Err(error.into());
            }
        };
        if !render_buffer_is_back_buffer(render_buffer) {
            let _ = instance.make_current(display, None, None, None);
            let _ = instance.destroy_context(display, context);
            let _ = instance.destroy_surface(display, surface);
            let _ = instance.terminate(display);
            return Err(format!("EGL window surface is not double-buffered: 0x{render_buffer:04x}").into());
        }
        if let Err(error) = instance.swap_interval(display, 1) {
            let _ = instance.make_current(display, None, None, None);
            let _ = instance.destroy_context(display, context);
            let _ = instance.destroy_surface(display, surface);
            let _ = instance.terminate(display);
            return Err(error.into());
        }
        renderer::load(|name| {
            instance.get_proc_address(name)
                .map_or(std::ptr::null(), |pointer| pointer as *const c_void)
        });
        if !renderer::has_extension("GL_OES_EGL_image") {
            let _ = instance.make_current(display, None, None, None);
            let _ = instance.destroy_context(display, context);
            let _ = instance.destroy_surface(display, surface);
            let _ = instance.terminate(display);
            return Err("required OpenGL extension is unavailable: GL_OES_EGL_image".into());
        }
        if instance.get_proc_address("glEGLImageTargetTexture2DOES").is_none() {
            let _ = instance.make_current(display, None, None, None);
            let _ = instance.destroy_context(display, context);
            let _ = instance.destroy_surface(display, surface);
            let _ = instance.terminate(display);
            return Err("glEGLImageTargetTexture2DOES is unavailable".into());
        }
        let scene_renderer = match renderer::SceneRenderer::new() {
            Ok(renderer) => renderer,
            Err(error) => {
                let _ = instance.make_current(display, None, None, None);
                let _ = instance.destroy_context(display, context);
                let _ = instance.destroy_surface(display, surface);
                let _ = instance.terminate(display);
                return Err(error);
            }
        };
        Ok(Self {
            instance,
            display,
            context,
            surface,
            _native_window: native_window,
            scene_renderer: Some(scene_renderer),
            width,
            height,
            disarmed: false,
        })
    }

    pub fn import_pixmap(
        &self,
        pixmap: u32,
        pixel_semantics: crate::x11::scene::EglPixelSemantics,
    ) -> Result<EglImportedSurface, Box<dyn Error>> {
        let image = unsafe {
            let client_buffer = egl::ClientBuffer::from_ptr(pixmap as usize as *mut c_void);
            create_native_pixmap_image(
                &self.instance,
                self.display,
                0x30B0,
                client_buffer.as_ptr(),
                &[egl::IMAGE_PRESERVED, egl::TRUE as egl::Int, egl::NONE],
            )?
        };
        let proc = self.instance.get_proc_address("glEGLImageTargetTexture2DOES")
            .ok_or("glEGLImageTargetTexture2DOES is unavailable")?;
        let target: unsafe extern "system" fn(u32, *const c_void) = unsafe { std::mem::transmute(proc) };
        let texture = match renderer::create_egl_texture(target, image.as_ptr().cast()) {
            Ok(texture) => texture,
            Err(error) => {
                let _ = self.instance.destroy_image(self.display, image);
                return Err(error);
            }
        };
        Ok(EglImportedSurface {
            instance: Arc::clone(&self.instance),
            display: self.display,
            image,
            texture,
            pixel_semantics,
            released: false,
        })
    }

    pub fn clear(&self) -> Result<(), Box<dyn Error>> {
        self.scene_renderer.as_ref().ok_or("EGL scene renderer is unavailable")?.clear();
        Ok(())
    }

    pub fn render_surface(
        &self,
        texture: u32,
        plan: crate::x11::scene::RenderQuadPlan,
        pixel_semantics: crate::x11::scene::EglPixelSemantics,
    ) -> Result<(), Box<dyn Error>> {
        self.scene_renderer
            .as_ref()
            .ok_or("EGL scene renderer is unavailable")?
            .render_surface(texture, plan, pixel_semantics, self.width as i32, self.height as i32)?;
        Ok(())
    }

    pub fn swap(&self) -> Result<(), Box<dyn Error>> {
        self.instance.swap_buffers(self.display, self.surface)?;
        Ok(())
    }

    pub fn make_current(&self) -> Result<(), Box<dyn Error>> {
        self.instance.make_current(
            self.display,
            Some(self.surface),
            Some(self.surface),
            Some(self.context),
        )?;
        Ok(())
    }

    pub fn destroy_import(&self, surface: &mut EglImportedSurface) -> Result<(), Box<dyn Error>> {
        surface.destroy(&self.instance, self.display)
    }

    pub fn destroy(&mut self) -> Result<(), Box<dyn Error>> {
        if self.disarmed { return Ok(()); }
        let _ = self.instance.make_current(self.display, Some(self.surface), Some(self.surface), Some(self.context))?;
        self.scene_renderer.take();
        let _ = self.instance.make_current(self.display, None, None, None)?;
        self.instance.destroy_surface(self.display, self.surface)?;
        self.instance.destroy_context(self.display, self.context)?;
        self.instance.terminate(self.display)?;
        self.disarmed = true;
        Ok(())
    }

    pub fn disarm(&mut self) {
        self.disarmed = true;
        if let Some(renderer) = self.scene_renderer.take() {
            std::mem::forget(renderer);
        }
    }
}

impl Drop for EglSceneRenderer {
    fn drop(&mut self) {
        if !self.disarmed {
            self.disarm();
        }
    }
}

pub(crate) fn render_buffer_is_back_buffer(value: egl::Int) -> bool {
    value == egl::BACK_BUFFER
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

impl Drop for EglContext<'_> {
    fn drop(&mut self) {
        self.capture_renderer.take();
        let _ = self.instance.make_current(self.display, None, None, None);

        if let Some(surface) = self.surface {
            let _ = self.instance.destroy_surface(self.display, surface);
        }
        let _ = self.instance.destroy_context(self.display, self.context);

        if let Some(image) = self.captured_image {
            let _ = self.instance.destroy_image(self.display, image);
        }

        if let Some(damage) = self.damage {
            let _ = self.connection.destroy_damage(damage);
        }
        if let Some(window) = self.window {
            let _ = self.connection.destroy_window(window);
        }
        if let Some(colormap) = self.colormap {
            let _ = self.connection.free_colormap(colormap);
        }
        if let Some(pixmap) = self.captured_pixmap {
            let _ = self.connection.free_pixmap(pixmap);
        }

        let _ = self.instance.terminate(self.display);
    }
}

#[cfg(test)]
mod tests {
    use super::{claim_imported_surface_release, render_buffer_is_back_buffer};
    use khronos_egl as egl;

    #[test]
    fn only_egl_back_buffer_is_accepted() {
        assert!(render_buffer_is_back_buffer(egl::BACK_BUFFER));
        assert!(!render_buffer_is_back_buffer(egl::SINGLE_BUFFER));
        assert!(!render_buffer_is_back_buffer(0));
        assert!(!render_buffer_is_back_buffer(egl::UNKNOWN));
    }

    #[test]
    fn imported_surface_release_is_at_most_once() {
        let mut released = false;
        assert!(claim_imported_surface_release(&mut released));
        assert!(!claim_imported_surface_release(&mut released));
    }
}
