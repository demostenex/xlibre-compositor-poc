use std::cell::RefCell;
use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::damage::{self, ConnectionExt as DamageConnectionExt};
use x11rb::protocol::xproto::ConnectionExt;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ClientMessageEvent, Colormap, CreateWindowAux, EventMask, MapState,
    Window, WindowClass,
};
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::xcb_ffi::XCBConnection;

use crate::graphics::egl::{CaptureState, EglContext};
use crate::x11::capture::CaptureInfo;
use crate::x11::compositor::{
    selection_clear_matches, CompositorOwnership, RedirectedWindow,
};

pub struct X11Connection {
    pub(crate) inner: XCBConnection,
    screen_num: usize,
    pub wm_protocols: Atom,
    pub wm_delete_window: Atom,
    pub(crate) capture_info: RefCell<Option<CaptureInfo>>,
}
enum EventLoopAction {
    Continue,
    Break,
}

fn log_damage_event(event: &damage::NotifyEvent) {
    println!("DamageNotify:");
    println!("damage={}", event.damage);
    println!("drawable={}", event.drawable);
    println!(
        "area={}x{}+{}+{}",
        event.area.width, event.area.height, event.area.x, event.area.y
    );
    println!(
        "geometry={}x{}+{}+{}",
        event.geometry.width,
        event.geometry.height,
        event.geometry.x,
        event.geometry.y
    );
}

impl X11Connection {
    pub fn connect() -> Result<Self, Box<dyn Error>> {
        let (inner, screen_num) = XCBConnection::connect(None)?;
        let wm_protocols = inner.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
        let wm_delete_window = inner.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;

        Ok(Self {
            inner,
            screen_num,
            wm_protocols,
            wm_delete_window,
            capture_info: RefCell::new(None),
        })
    }

    pub fn screen_num(&self) -> usize {
        self.screen_num
    }

    pub fn create_damage(&self, drawable: Window) -> Result<damage::Damage, Box<dyn Error>> {
        let version = self.inner.damage_query_version(1, 1)?.reply()?;
        println!(
            "XDamage version: {}.{}",
            version.major_version, version.minor_version
        );
        if version.major_version < 1 {
            return Err("XDamage 1.0 or newer is required".into());
        }

        let damage_id = self.inner.generate_id()?;
        self.inner
            .damage_create(damage_id, drawable, damage::ReportLevel::NON_EMPTY)?
            .check()?;
        Ok(damage_id)
    }

    pub fn destroy_damage(&self, damage_id: damage::Damage) -> Result<(), Box<dyn Error>> {
        self.inner.damage_destroy(damage_id)?.check()?;
        Ok(())
    }

    pub fn subtract_damage(&self, damage_id: damage::Damage) -> Result<(), Box<dyn Error>> {
        self.inner
            .damage_subtract(damage_id, x11rb::NONE, x11rb::NONE)?
            .check()?;
        Ok(())
    }

    pub fn visual_depth(&self, visual_id: u32) -> Option<u8> {
        self.inner.setup().roots[self.screen_num]
            .allowed_depths
            .iter()
            .find(|depth| {
                depth
                    .visuals
                    .iter()
                    .any(|visual| visual.visual_id == visual_id)
            })
            .map(|depth| depth.depth)
    }

    pub fn create_colormap(&self, visual: u32) -> Result<Colormap, Box<dyn Error>> {
        let screen = &self.inner.setup().roots[self.screen_num];
        let colormap = self.inner.generate_id()?;
        self.inner
            .create_colormap(xproto::ColormapAlloc::NONE, colormap, screen.root, visual)?
            .check()?;
        Ok(colormap)
    }

    pub fn create_window(
        &self,
        visual: u32,
        depth: u8,
        colormap: Colormap,
    ) -> Result<Window, Box<dyn Error>> {
        let screen = &self.inner.setup().roots[self.screen_num];
        let window = self.inner.generate_id()?;

        self.inner
            .create_window(
                depth,
                window,
                screen.root,
                100,
                100,
                640,
                360,
                0,
                WindowClass::INPUT_OUTPUT,
                visual,
                &CreateWindowAux::new()
                    .colormap(colormap)
                    .event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY),
            )?
            .check()?;

        self.inner
            .change_property32(
                xproto::PropMode::REPLACE,
                window,
                self.wm_protocols,
                AtomEnum::ATOM,
                &[self.wm_delete_window],
            )?
            .check()?;
        self.inner.map_window(window)?.check()?;
        self.inner.flush()?;
        Ok(window)
    }

    pub fn run_event_loop(
        &self,
        graphics: &mut EglContext<'_>,
        ownership: Option<&CompositorOwnership>,
        redirected: Option<&RedirectedWindow>,
    ) -> Result<(), Box<dyn Error>> {
        loop {
            let action = self.dispatch_event(
                self.inner.wait_for_event()?,
                graphics,
                ownership,
                redirected,
            )?;
            if matches!(action, EventLoopAction::Break) {
                break;
            }
        }
        Ok(())
    }

    fn dispatch_event(
        &self,
        event: Event,
        graphics: &mut EglContext<'_>,
        ownership: Option<&CompositorOwnership>,
        redirected: Option<&RedirectedWindow>,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        match event {
            Event::ClientMessage(event) => {
                self.handle_client_message(event, graphics, ownership, redirected)
            }
            Event::SelectionClear(event) => {
                self.handle_selection_clear(event, graphics, ownership, redirected)
            }
            Event::Expose(event) => self.handle_expose(event.window, graphics),
            Event::ConfigureNotify(event) => self.handle_configure_notify(event, graphics),
            Event::UnmapNotify(event) => self.handle_unmap_notify(event, graphics),
            Event::MapNotify(event) => self.handle_map_notify(event, graphics),
            Event::VisibilityNotify(event) => self.handle_visibility_notify(event, graphics),
            Event::ReparentNotify(event) => self.handle_reparent_notify(event),
            Event::DestroyNotify(event) => {
                self.handle_destroy_notify(event.window, graphics, ownership)
            }
            Event::DamageNotify(event) => self.handle_damage_notify(event, graphics),
            _ => Ok(EventLoopAction::Continue),
        }
    }

    fn handle_client_message(
        &self,
        event: ClientMessageEvent,
        graphics: &mut EglContext<'_>,
        ownership: Option<&CompositorOwnership>,
        redirected: Option<&RedirectedWindow>,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        if graphics.window() == Some(event.window)
            && event.type_ == self.wm_protocols
            && event.data.as_data32()[0] == self.wm_delete_window
        {
            if redirected.is_some() {
                self.cleanup_compositor_capture(graphics, ownership, redirected, true)?;
            }
            return Ok(EventLoopAction::Break);
        }
        Ok(EventLoopAction::Continue)
    }

    fn handle_selection_clear(
        &self,
        event: x11rb::protocol::xproto::SelectionClearEvent,
        graphics: &mut EglContext<'_>,
        ownership: Option<&CompositorOwnership>,
        redirected: Option<&RedirectedWindow>,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        if ownership.is_some_and(|ownership| selection_clear_matches(&event, ownership)) {
            println!("compositor ownership lost");
            self.cleanup_compositor_capture(graphics, ownership, redirected, true)?;
            return Ok(EventLoopAction::Break);
        }
        Ok(EventLoopAction::Continue)
    }

    fn handle_expose(
        &self,
        window: Window,
        graphics: &mut EglContext<'_>,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        if graphics.window() == Some(window) {
            graphics.render();
            graphics.swap_buffers()?;
        }
        Ok(EventLoopAction::Continue)
    }

    fn handle_configure_notify(
        &self,
        event: xproto::ConfigureNotifyEvent,
        graphics: &mut EglContext<'_>,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        if graphics.window() == Some(event.window) {
            graphics.resize(event.width as i32, event.height as i32);
            graphics.render();
            graphics.swap_buffers()?;
        } else if self.source_window() == Some(event.window) {
            let old_size = graphics
                .source_size()
                .unwrap_or((event.width, event.height));
            let size_changed = old_size != (event.width, event.height);
            println!("Source ConfigureNotify:");
            println!("old size: {}x{}", old_size.0, old_size.1);
            println!("new size: {}x{}", event.width, event.height);
            println!("size_changed: {}", if size_changed { "yes" } else { "no" });
            println!("x={}", event.x);
            println!("y={}", event.y);
            println!("width={}", event.width);
            println!("height={}", event.height);
            println!("border_width={}", event.border_width);
            if size_changed && graphics.capture_state() == Some(CaptureState::Active) {
                if let Err(error) = graphics.resize_capture(event.width, event.height) {
                    eprintln!("capture resize failed; keeping previous resources: {error}");
                } else {
                    graphics.render();
                    graphics.swap_buffers()?;
                }
            }
        }
        Ok(EventLoopAction::Continue)
    }

    fn handle_unmap_notify(
        &self,
        event: xproto::UnmapNotifyEvent,
        graphics: &mut EglContext<'_>,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        if self.source_root_is_top_level(event.event, event.window) {
            if let Some(state) = self.source_map_state(graphics) {
                if matches!(state, MapState::UNVIEWABLE | MapState::UNMAPPED) {
                    println!("source top-level unmapped");
                    println!("source map_state: {}", crate::x11::map_state_name(state));
                    graphics.suspend_capture();
                    println!("capture suspended");
                }
            }
        }
        Ok(EventLoopAction::Continue)
    }

    fn handle_map_notify(
        &self,
        event: xproto::MapNotifyEvent,
        graphics: &mut EglContext<'_>,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        if self.source_root_is_top_level(event.event, event.window) {
            println!("source top-level mapped");
            if let Some(state) = self.source_map_state(graphics) {
                println!("source map_state: {}", crate::x11::map_state_name(state));
                if state == MapState::VIEWABLE
                    && graphics.capture_state() == Some(CaptureState::Suspended)
                {
                    println!("recreating capture after remap");
                    self.try_resume_capture(graphics)?;
                } else if state != MapState::VIEWABLE {
                    println!("source mapped but not viewable yet");
                }
            }
        }
        Ok(EventLoopAction::Continue)
    }

    fn handle_visibility_notify(
        &self,
        event: xproto::VisibilityNotifyEvent,
        graphics: &mut EglContext<'_>,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        println!(
            "VisibilityNotify: state={}",
            Self::visibility_state_name(event.state)
        );
        if graphics.source_window() == Some(event.window)
            && graphics.capture_state() == Some(CaptureState::Suspended)
        {
            self.try_resume_capture(graphics)?;
        }
        Ok(EventLoopAction::Continue)
    }

    fn handle_reparent_notify(
        &self,
        event: xproto::ReparentNotifyEvent,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        if self.source_window() == Some(event.window) {
            if let Err(error) = self.refresh_capture_hierarchy() {
                eprintln!("source hierarchy refresh failed: {error}");
            }
        }
        Ok(EventLoopAction::Continue)
    }

    fn handle_destroy_notify(
        &self,
        window: Window,
        graphics: &mut EglContext<'_>,
        ownership: Option<&CompositorOwnership>,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        if self.source_window() == Some(window) {
            graphics.destroy_capture();
            if let Some(ownership) = ownership {
                if let Err(error) = ownership.release(self) {
                    eprintln!("compositor ownership cleanup failed: {error}");
                }
            }
            return Ok(EventLoopAction::Break);
        }
        Ok(EventLoopAction::Continue)
    }

    fn handle_damage_notify(
        &self,
        event: damage::NotifyEvent,
        graphics: &mut EglContext<'_>,
    ) -> Result<EventLoopAction, Box<dyn Error>> {
        if graphics.damage() != Some(event.damage) {
            return Ok(EventLoopAction::Continue);
        }
        self.subtract_damage(event.damage)?;
        if graphics.capture_state() == Some(CaptureState::Active) {
            log_damage_event(&event);
            graphics.render();
            graphics.swap_buffers()?;
        }
        Ok(EventLoopAction::Continue)
    }

    fn cleanup_compositor_capture(
        &self,
        graphics: &mut EglContext<'_>,
        ownership: Option<&CompositorOwnership>,
        redirected: Option<&RedirectedWindow>,
        unredirect: bool,
    ) -> Result<(), Box<dyn Error>> {
        graphics.destroy_capture();
        let mut cleanup_error = None;
        if unredirect {
            if let Some(redirected) = redirected {
                if let Err(error) = redirected.unredirect(self) {
                    cleanup_error = Some(error);
                }
            }
        }
        if let Some(ownership) = ownership {
            if let Err(error) = ownership.release(self) {
                if cleanup_error.is_none() {
                    cleanup_error = Some(error);
                }
            }
        }
        if let Some(error) = cleanup_error {
            return Err(error);
        }
        Ok(())
    }

    pub fn destroy_window(&self, window: Window) -> Result<(), Box<dyn Error>> {
        self.inner.destroy_window(window)?.check()?;
        Ok(())
    }

    pub fn free_colormap(&self, colormap: Colormap) -> Result<(), Box<dyn Error>> {
        self.inner.free_colormap(colormap)?.check()?;
        Ok(())
    }

    pub fn free_pixmap(&self, pixmap: u32) -> Result<(), Box<dyn Error>> {
        self.inner.free_pixmap(pixmap)?.check()?;
        Ok(())
    }

    fn source_root_is_top_level(&self, event_window: Window, affected_window: Window) -> bool {
        self.root() == Some(event_window) && self.top_level() == Some(affected_window)
    }

    fn source_window(&self) -> Option<Window> {
        self.capture_info
            .borrow()
            .as_ref()
            .map(|info| info.capture_window)
    }
    fn top_level(&self) -> Option<Window> {
        self.capture_info
            .borrow()
            .as_ref()
            .map(|info| info.lifecycle_window)
    }
    fn root(&self) -> Option<Window> {
        self.capture_info
            .borrow()
            .as_ref()
            .map(|info| info.hierarchy.root)
    }

    fn source_map_state(&self, graphics: &EglContext<'_>) -> Option<MapState> {
        let source_window = graphics.source_window()?;
        match self.inner.get_window_attributes(source_window) {
            Ok(cookie) => match cookie.reply() {
                Ok(attributes) => Some(attributes.map_state),
                Err(error) => {
                    eprintln!("source get_window_attributes reply failed: {error}");
                    None
                }
            },
            Err(error) => {
                eprintln!("source get_window_attributes request failed: {error}");
                None
            }
        }
    }

    fn try_resume_capture(&self, graphics: &mut EglContext<'_>) -> Result<(), Box<dyn Error>> {
        match graphics.resume_capture() {
            Ok(true) => {
                println!("capture resumed");
                graphics.render();
                graphics.swap_buffers()?;
            }
            Ok(false) if graphics.capture_state() == Some(CaptureState::Suspended) => {
                println!("source mapped but not viewable yet");
            }
            Ok(false) => {}
            Err(error) => {
                eprintln!("capture resume failed; keeping capture suspended: {error}");
            }
        }
        Ok(())
    }

    fn visibility_state_name(state: xproto::Visibility) -> &'static str {
        match state {
            xproto::Visibility::UNOBSCURED => "Unobscured",
            xproto::Visibility::PARTIALLY_OBSCURED => "PartiallyObscured",
            xproto::Visibility::FULLY_OBSCURED => "FullyObscured",
            _ => "Unknown",
        }
    }
}
