use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, Atom, AtomEnum, ClientMessageEvent, Colormap, ConfigureNotifyEvent, CreateWindowAux, EventMask, Window, WindowClass};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::ConnectionExt;
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;
use x11rb::xcb_ffi::XCBConnection;

use crate::graphics::egl::EglContext;

pub struct X11Connection {
    pub(crate) inner: XCBConnection,
    screen_num: usize,
    pub wm_protocols: Atom,
    pub wm_delete_window: Atom,
}

impl X11Connection {
    pub fn connect() -> Result<Self, Box<dyn Error>> {
        let (inner, screen_num) = XCBConnection::connect(None)?;
        let wm_protocols = inner.intern_atom(false, b"WM_PROTOCOLS")?.reply()?.atom;
        let wm_delete_window = inner.intern_atom(false, b"WM_DELETE_WINDOW")?.reply()?.atom;

        Ok(Self { inner, screen_num, wm_protocols, wm_delete_window })
    }

    pub fn screen_num(&self) -> usize { self.screen_num }

    pub fn visual_depth(&self, visual_id: u32) -> Option<u8> {
        self.inner.setup().roots[self.screen_num].allowed_depths.iter()
            .find(|depth| depth.visuals.iter().any(|visual| visual.visual_id == visual_id))
            .map(|depth| depth.depth)
    }

    pub fn create_colormap(&self, visual: u32) -> Result<Colormap, Box<dyn Error>> {
        let screen = &self.inner.setup().roots[self.screen_num];
        let colormap = self.inner.generate_id()?;
        self.inner.create_colormap(xproto::ColormapAlloc::NONE, colormap, screen.root, visual)?.check()?;
        Ok(colormap)
    }

    pub fn create_window(&mut self, visual: u32, depth: u8, colormap: Colormap) -> Result<Window, Box<dyn Error>> {
        let screen = &self.inner.setup().roots[self.screen_num];
        let window = self.inner.generate_id()?;

        self.inner.create_window(
            depth, window, screen.root, 100, 100, 640, 360, 0,
            WindowClass::INPUT_OUTPUT, visual,
            &CreateWindowAux::new().colormap(colormap).event_mask(EventMask::EXPOSURE | EventMask::STRUCTURE_NOTIFY | EventMask::KEY_PRESS),
        )?.check()?;

        self.inner.change_property32(xproto::PropMode::REPLACE, window, self.wm_protocols, AtomEnum::ATOM, &[self.wm_delete_window])?.check()?;
        self.inner.map_window(window)?.check()?;
        self.inner.flush()?;
        Ok(window)
    }

    pub fn run_event_loop(&mut self, graphics: &mut EglContext) -> Result<(), Box<dyn Error>> {
        loop {
            match self.inner.wait_for_event()? {
                Event::ClientMessage(ClientMessageEvent { window, type_, data, .. }) if graphics.window() == Some(window) && type_ == self.wm_protocols && data.as_data32()[0] == self.wm_delete_window => break,
                Event::Expose(event) if graphics.window() == Some(event.window) => {
                    graphics.render();
                    graphics.swap_buffers()?;
                }
                Event::ConfigureNotify(ConfigureNotifyEvent { window, width, height, .. }) if graphics.window() == Some(window) => {
                    graphics.resize(width as i32, height as i32);
                    graphics.render();
                    graphics.swap_buffers()?;
                }
                Event::KeyPress(_) => break,
                _ => {}
            }
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
}
