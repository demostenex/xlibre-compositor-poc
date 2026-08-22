use std::error::Error;
use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as CompositeConnectionExt;
use x11rb::protocol::xproto::{ChangeWindowAttributesAux, ConnectionExt, EventMask, MapState, Window, WindowClass};
use super::connection::X11Connection;

pub struct CapturedPixmap {
    pub(crate) window: Window,
    pub(crate) pixmap: u32,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

pub fn parse_window_id(value: &str) -> Result<Window, Box<dyn Error>> {
    if let Some(value) = value.strip_prefix("0x") {
        return Ok(u32::from_str_radix(value, 16)?);
    }
    Ok(value.parse::<u32>()?)
}

impl X11Connection {
    pub fn capture_window(&self, value: &str) -> Result<CapturedPixmap, Box<dyn Error>> {
        let window = parse_window_id(value)?;
        let attributes = self.inner.get_window_attributes(window)?.reply()?;
        let geometry = self.inner.get_geometry(window)?.reply()?;

        println!("Capture target:");
        println!("window: 0x{window:08x}");
        println!("geometry: {}x{}", geometry.width, geometry.height);
        println!("depth: {}", geometry.depth);
        println!("visual: 0x{:08x}", attributes.visual);
        println!("mapped: {}", attributes.map_state == MapState::VIEWABLE);

        if attributes.class != WindowClass::INPUT_OUTPUT {
            return Err("capture target is not an InputOutput window".into());
        }
        if attributes.map_state != MapState::VIEWABLE {
            return Err("capture target is not viewable".into());
        }

        self.inner.change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY),
        )?.check()?;

        let version = self.inner.composite_query_version(0, 4)?.reply()?;
        println!("\nComposite:");
        println!("version: {}.{}", version.major_version, version.minor_version);
        if (version.major_version, version.minor_version) < (0, 2) {
            return Err("Composite 0.2 or newer is required".into());
        }

        let pixmap = self.inner.generate_id()?;
        self.inner.composite_name_window_pixmap(window, pixmap)?.check()?;
        println!("\nNameWindowPixmap: OK");
        println!("pixmap: 0x{pixmap:08x}");
        Ok(CapturedPixmap { window, pixmap, width: geometry.width, height: geometry.height })
    }
}
