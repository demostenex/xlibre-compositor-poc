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
    pub(crate) hierarchy: Vec<Window>,
    pub(crate) root: Window,
    pub(crate) top_level: Window,
}

pub fn parse_window_id(value: &str) -> Result<Window, Box<dyn Error>> {
    if let Some(value) = value.strip_prefix("0x") {
        return Ok(u32::from_str_radix(value, 16)?);
    }
    Ok(value.parse::<u32>()?)
}

impl X11Connection {
    pub fn capture_window(&mut self, value: &str) -> Result<CapturedPixmap, Box<dyn Error>> {
        let window = parse_window_id(value)?;
        let (hierarchy, root, top_level) = self.query_source_hierarchy(window)?;
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
            &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::VISIBILITY_CHANGE),
        )?.check()?;
        println!("source event mask: STRUCTURE_NOTIFY=yes, VISIBILITY_CHANGE=yes");

        let version = self.inner.composite_query_version(0, 4)?.reply()?;
        println!("\nComposite:");
        println!("version: {}.{}", version.major_version, version.minor_version);
        if (version.major_version, version.minor_version) < (0, 2) {
            return Err("Composite 0.2 or newer is required".into());
        }

        let pixmap = self.inner.generate_id()?;
        self.name_window_pixmap(window, pixmap)?;
        println!("\nNameWindowPixmap: OK");
        println!("pixmap: 0x{pixmap:08x}");
        Ok(CapturedPixmap { window, pixmap, width: geometry.width, height: geometry.height, hierarchy, root, top_level })
    }

    pub fn name_window_pixmap(&self, window: Window, pixmap: u32) -> Result<(), Box<dyn Error>> {
        self.inner.composite_name_window_pixmap(window, pixmap)?.check()?;
        Ok(())
    }

    pub(crate) fn query_source_hierarchy(&self, source: Window) -> Result<(Vec<Window>, Window, Window), Box<dyn Error>> {
        let mut hierarchy = vec![source];
        let mut current = source;
        let mut top_level = source;
        println!("Source hierarchy:");
        println!("0x{source:08x}");

        loop {
            let tree = self.inner.query_tree(current)?.reply()?;
            if tree.parent == tree.root {
                println!("  -> root 0x{:08x}", tree.root);
                hierarchy.push(tree.root);
                self.select_root_for_hierarchy(tree.root)?;
                break;
            }
            println!("  -> parent 0x{:08x}", tree.parent);
            hierarchy.push(tree.parent);
            top_level = tree.parent;
            current = tree.parent;
        }

        let root = *hierarchy.last().ok_or("source hierarchy is empty")?;
        Ok((hierarchy, root, top_level))
    }

    pub(crate) fn select_root_for_hierarchy(&self, root: Window) -> Result<(), Box<dyn Error>> {
        self.inner.change_window_attributes(
            root,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?.check()?;
        println!("root event mask: SUBSTRUCTURE_NOTIFY=yes, SUBSTRUCTURE_REDIRECT=no");
        Ok(())
    }
}
