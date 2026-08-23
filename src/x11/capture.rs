use std::error::Error;
use std::fmt;

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::composite::ConnectionExt as CompositeConnectionExt;
use x11rb::protocol::xproto::{
    AtomEnum, ChangeWindowAttributesAux, ConnectionExt, EventMask, GetPropertyReply, MapState,
    Window, WindowClass,
};
use x11rb::protocol::ErrorKind;

use super::connection::X11Connection;
use super::map_state_name;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowHierarchy {
    pub source: Window,
    pub parent: Option<Window>,
    pub top_level: Window,
    pub root: Window,
}
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WindowRole {
    Client,
    TopLevelOrWmFrame,
    OverrideRedirect,
    Root,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowRoleFacts {
    pub is_root: bool,
    pub is_top_level: bool,
    pub override_redirect: Option<bool>,
    pub has_wm_state: bool,
}

pub fn classify_window_role(facts: WindowRoleFacts) -> WindowRole {
    if facts.is_root {
        WindowRole::Root
    } else if facts.override_redirect == Some(true) {
        WindowRole::OverrideRedirect
    } else if facts.has_wm_state {
        WindowRole::Client
    } else if facts.is_top_level {
        WindowRole::TopLevelOrWmFrame
    } else {
        WindowRole::Unknown
    }
}

#[derive(Debug)]
pub struct WindowMetadata {
    pub window: Window,
    pub geometry: WindowGeometry,
    pub depth: u8,
    pub visual: u32,
    pub class: WindowClass,
    pub override_redirect: bool,
    pub has_wm_state: bool,
    pub map_state: MapState,
    pub wm_class: Option<String>,
    pub window_type: Option<String>,
    pub role: WindowRole,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WindowGeometry {
    pub x: i16,
    pub y: i16,
    pub width: u16,
    pub height: u16,
    pub border_width: u16,
}

#[derive(Debug)]
pub struct CaptureInfo {
    pub requested_window: Window,
    pub capture_window: Window,
    pub lifecycle_window: Window,
    pub hierarchy: WindowHierarchy,
    pub source: WindowMetadata,
    pub top_level: WindowMetadata,
}

#[derive(Debug)]
pub struct CaptureNotCapturable {
    pub requested: Window,
    pub top_level: Window,
    pub map_state: MapState,
    pub role: WindowRole,
}

impl fmt::Display for CaptureNotCapturable {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "capture target 0x{:08x} is not currently capturable through CompositeNameWindowPixmap\n\
             requested/source: 0x{:08x}\n\
             top-level:        0x{:08x}\n\
             map state:        {}\n\
             role:             {:?}\n\
             composite target: rejected (BadMatch)",
            self.requested,
            self.requested,
            self.top_level,
            map_state_name(self.map_state),
            self.role
        )
    }
}
impl Error for CaptureNotCapturable {}

pub struct CapturedPixmap {
    pub(crate) window: Window,
    pub(crate) pixmap: u32,
    pub(crate) width: u16,
    pub(crate) height: u16,
}

pub(crate) struct HierarchyProbeInfo {
    pub(crate) client: Window,
    pub(crate) direct_root_child: Window,
    pub(crate) root: Window,
    pub(crate) path: Vec<Window>,
    pub(crate) client_metadata: WindowMetadata,
    pub(crate) direct_root_child_metadata: WindowMetadata,
}

pub fn parse_window_id(value: &str) -> Result<Window, Box<dyn Error>> {
    if let Some(value) = value.strip_prefix("0x") {
        return Ok(u32::from_str_radix(value, 16)?);
    }
    Ok(value.parse::<u32>()?)
}

impl X11Connection {
    pub fn require_composite(&self) -> Result<(u32, u32), Box<dyn Error>> {
        let version = self.inner.composite_query_version(0, 4)?.reply()?;
        println!("\nComposite:");
        println!(
            "version: {}.{}",
            version.major_version, version.minor_version
        );
        if (version.major_version, version.minor_version) < (0, 2) {
            return Err("Composite 0.2 or newer is required".into());
        }
        Ok((version.major_version as u32, version.minor_version as u32))
    }

    pub fn capture_window(&self, value: &str) -> Result<CapturedPixmap, Box<dyn Error>> {
        let (window, width, height, _) = self.prepare_capture_window(value)?;
        self.capture_pixmap(window, width, height)
    }

    pub fn prepare_capture_window(
        &self,
        value: &str,
    ) -> Result<(Window, u16, u16, WindowRole), Box<dyn Error>> {
        let window = parse_window_id(value)?;
        self.require_composite()?;
        let hierarchy = self.query_source_hierarchy(window)?;
        let source = self.window_metadata(window, hierarchy)?;
        let top_level = self.window_metadata(hierarchy.top_level, hierarchy)?;
        let width = source.geometry.width;
        let height = source.geometry.height;
        println!("Capture target:");
        print_metadata("requested/source", &source);
        print_metadata("top-level", &top_level);
        println!(
            "source/top-level differ: {}",
            if window != hierarchy.top_level {
                "yes"
            } else {
                "no"
            }
        );
        if source.class != WindowClass::INPUT_OUTPUT {
            return Err("capture target is not an InputOutput window".into());
        }
        if source.map_state != MapState::VIEWABLE {
            return Err("capture target is not viewable".into());
        }
        let role = source.role;
        self.inner
            .change_window_attributes(
                window,
                &ChangeWindowAttributesAux::new()
                    .event_mask(EventMask::STRUCTURE_NOTIFY | EventMask::VISIBILITY_CHANGE),
            )?
            .check()?;
        println!("source event mask: STRUCTURE_NOTIFY=yes, VISIBILITY_CHANGE=yes");
        self.capture_info.replace(Some(CaptureInfo {
            requested_window: window,
            capture_window: window,
            lifecycle_window: hierarchy.top_level,
            hierarchy,
            source,
            top_level,
        }));
        Ok((window, width, height, role))
    }

    pub fn capture_pixmap(
        &self,
        window: Window,
        width: u16,
        height: u16,
    ) -> Result<CapturedPixmap, Box<dyn Error>> {
        let pixmap = self.inner.generate_id()?;
        if let Err(error) = self.name_window_pixmap(window, pixmap) {
            return Err(error);
        }
        println!("\nNameWindowPixmap: OK");
        println!("pixmap: 0x{pixmap:08x}");
        Ok(CapturedPixmap {
            window,
            pixmap,
            width,
            height,
        })
    }

    pub(crate) fn inspect_hierarchy_probe(
        &self,
        value: &str,
    ) -> Result<HierarchyProbeInfo, Box<dyn Error>> {
        let client = parse_window_id(value)?;
        let (hierarchy, path, direct_root_child) = self.resolve_hierarchy_path(client)?;
        let client_metadata = self.window_metadata(client, hierarchy)?;
        let direct_root_child_metadata =
            self.window_metadata(direct_root_child, hierarchy)?;
        Ok(HierarchyProbeInfo {
            client,
            direct_root_child,
            root: hierarchy.root,
            path,
            client_metadata,
            direct_root_child_metadata,
        })
    }

    pub(crate) fn print_hierarchy_probe(info: &HierarchyProbeInfo) {
        println!("\nHierarchy probe:");
        println!("requested client: 0x{:08x}", info.client);
        println!(
            "direct root child: 0x{:08x}",
            info.direct_root_child
        );
        println!("root: 0x{:08x}", info.root);
        println!("hierarchy:");
        for (index, window) in info.path.iter().enumerate() {
            if index == 0 {
                println!("0x{window:08x}");
            } else {
                println!("  -> 0x{window:08x}");
            }
        }
        print_metadata("requested client", &info.client_metadata);
        print_metadata("direct root child", &info.direct_root_child_metadata);
    }

    pub fn name_window_pixmap(&self, window: Window, pixmap: u32) -> Result<(), Box<dyn Error>> {
        match self
            .inner
            .composite_name_window_pixmap(window, pixmap)?
            .check()
        {
            Ok(()) => Ok(()),
            Err(error) if is_bad_match(&error) => {
                let info = self.capture_info.borrow();
                let capture = info.as_ref().ok_or("capture metadata is unavailable")?;
                Err(Box::new(CaptureNotCapturable {
                    requested: capture.requested_window,
                    top_level: capture.lifecycle_window,
                    map_state: capture.source.map_state,
                    role: capture.source.role,
                }))
            }
            Err(error) => Err(Box::new(error)),
        }
    }

    pub(crate) fn probe_name_window_pixmap(
        &self,
        label: &str,
        window: Window,
    ) -> Result<(), Box<dyn Error>> {
        println!("\n{label} NameWindowPixmap:");
        println!("window: 0x{window:08x}");
        let pixmap = self.inner.generate_id()?;
        match self
            .inner
            .composite_name_window_pixmap(window, pixmap)?
            .check()
        {
            Ok(()) => {
                println!("result: OK");
                println!("pixmap: 0x{pixmap:08x}");
                self.inner.free_pixmap(pixmap)?.check()?;
                self.inner.flush()?;
                Ok(())
            }
            Err(error) if is_name_window_pixmap_observation_error(&error) => {
                println!("result: {error}");
                Ok(())
            }
            Err(error) => Err(Box::new(error)),
        }
    }

    pub(crate) fn query_source_hierarchy(
        &self,
        source: Window,
    ) -> Result<WindowHierarchy, Box<dyn Error>> {
        let mut current = source;
        let mut parent = None;
        let mut top_level = source;
        let mut root;
        println!("Source hierarchy:");
        println!("0x{source:08x}");
        loop {
            let tree = self.inner.query_tree(current)?.reply()?;
            root = tree.root;
            if current == tree.root {
                parent = None;
                top_level = tree.root;
                println!("  -> root 0x{:08x}", tree.root);
                self.select_root_for_hierarchy(tree.root)?;
                break;
            }
            if tree.parent == tree.root {
                if parent.is_none() {
                    parent = Some(tree.parent);
                }
                println!("  -> root 0x{:08x}", tree.root);
                self.select_root_for_hierarchy(tree.root)?;
                break;
            }
            if parent.is_none() {
                parent = Some(tree.parent);
            }
            println!("  -> parent 0x{:08x}", tree.parent);
            top_level = tree.parent;
            current = tree.parent;
        }
        Ok(WindowHierarchy {
            source,
            parent,
            top_level,
            root,
        })
    }

    fn resolve_hierarchy_path(
        &self,
        source: Window,
    ) -> Result<(WindowHierarchy, Vec<Window>, Window), Box<dyn Error>> {
        let mut current = source;
        let mut path = vec![source];
        let mut visited = Vec::new();
        let mut root = None;

        loop {
            if visited.contains(&current) {
                return Err("window hierarchy contains a loop".into());
            }
            visited.push(current);

            let tree = self.inner.query_tree(current)?.reply()?;
            if let Some(expected_root) = root {
                if tree.root != expected_root {
                    return Err("window hierarchy changed roots during inspection".into());
                }
            } else {
                root = Some(tree.root);
            }

            if current == tree.root {
                return Err("requested window is the root and has no direct root child".into());
            }
            if tree.parent == x11rb::NONE || tree.parent == current {
                return Err("window hierarchy has an invalid parent".into());
            }
            if tree.parent == tree.root {
                path.push(tree.root);
                let hierarchy = WindowHierarchy {
                    source,
                    parent: path.get(1).copied(),
                    top_level: current,
                    root: tree.root,
                };
                return Ok((hierarchy, path, current));
            }

            current = tree.parent;
            path.push(current);
        }
    }

    pub(crate) fn refresh_capture_hierarchy(&self) -> Result<(), Box<dyn Error>> {
        let source = self
            .capture_info
            .borrow()
            .as_ref()
            .map(|info| info.requested_window)
            .ok_or("capture metadata is unavailable")?;
        let hierarchy = self.query_source_hierarchy(source)?;
        let source_metadata = self.window_metadata(source, hierarchy)?;
        let top_level_metadata = self.window_metadata(hierarchy.top_level, hierarchy)?;
        let mut info = self.capture_info.borrow_mut();
        if let Some(info) = info.as_mut() {
            info.lifecycle_window = hierarchy.top_level;
            info.hierarchy = hierarchy;
            info.source = source_metadata;
            info.top_level = top_level_metadata;
        }
        Ok(())
    }

    fn select_root_for_hierarchy(&self, root: Window) -> Result<(), Box<dyn Error>> {
        self.inner
            .change_window_attributes(
                root,
                &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
            )?
            .check()?;
        println!("root event mask: SUBSTRUCTURE_NOTIFY=yes, SUBSTRUCTURE_REDIRECT=no");
        Ok(())
    }

    fn window_metadata(
        &self,
        window: Window,
        hierarchy: WindowHierarchy,
    ) -> Result<WindowMetadata, Box<dyn Error>> {
        let attributes = self.inner.get_window_attributes(window)?.reply()?;
        let geometry = self.inner.get_geometry(window)?.reply()?;
        let has_wm_state = self.has_wm_state(window);
        let role = classify_window_role(WindowRoleFacts {
            is_root: window == hierarchy.root,
            is_top_level: window == hierarchy.top_level,
            override_redirect: Some(attributes.override_redirect),
            has_wm_state,
        });
        Ok(WindowMetadata {
            window,
            geometry: WindowGeometry {
                x: geometry.x,
                y: geometry.y,
                width: geometry.width,
                height: geometry.height,
                border_width: geometry.border_width,
            },
            depth: geometry.depth,
            visual: attributes.visual,
            class: attributes.class,
            override_redirect: attributes.override_redirect,
            has_wm_state,
            map_state: attributes.map_state,
            wm_class: self.read_wm_class(window)?,
            window_type: self.read_window_type(window)?,
            role,
        })
    }

    fn read_wm_class(&self, window: Window) -> Result<Option<String>, Box<dyn Error>> {
        let atom = self.inner.intern_atom(false, b"WM_CLASS")?.reply()?.atom;
        let property = self
            .inner
            .get_property(false, window, atom, AtomEnum::STRING, 0, u32::MAX)?
            .reply()?;
        Ok(parse_wm_class(&property))
    }

    fn has_wm_state(&self, window: Window) -> bool {
        let atom = match self.inner.intern_atom(true, b"WM_STATE") {
            Ok(cookie) => match cookie.reply() {
                Ok(reply) => reply.atom,
                Err(_) => return false,
            },
            Err(_) => return false,
        };
        if atom == x11rb::NONE {
            return false;
        }
        let cookie = match self
            .inner
            .get_property(false, window, atom, AtomEnum::ANY, 0, 2)
        {
            Ok(cookie) => cookie,
            Err(_) => return false,
        };
        cookie
            .reply()
            .map(|property| !property.value.is_empty())
            .unwrap_or(false)
    }

    fn read_window_type(&self, window: Window) -> Result<Option<String>, Box<dyn Error>> {
        let property_atom = self
            .inner
            .intern_atom(false, b"_NET_WM_WINDOW_TYPE")?
            .reply()?
            .atom;
        let property = self
            .inner
            .get_property(false, window, property_atom, AtomEnum::ATOM, 0, u32::MAX)?
            .reply()?;
        let atoms = property
            .value32()
            .map(|values| values.collect::<Vec<_>>())
            .unwrap_or_default();
        if atoms.is_empty() {
            return Ok(None);
        }
        let mut names = Vec::with_capacity(atoms.len());
        for atom in atoms {
            let name = self.inner.get_atom_name(atom)?.reply()?.name;
            names.push(String::from_utf8_lossy(&name).into_owned());
        }
        Ok(Some(names.join(",")))
    }
}

fn is_bad_match(error: &ReplyError) -> bool {
    matches!(error, ReplyError::X11Error(error) if error.error_kind == ErrorKind::Match)
}

fn is_name_window_pixmap_observation_error(error: &ReplyError) -> bool {
    matches!(
        error,
        ReplyError::X11Error(error)
            if error.error_kind == ErrorKind::Match || error.error_kind == ErrorKind::Window
    )
}

fn parse_wm_class(property: &GetPropertyReply) -> Option<String> {
    if property.value.is_empty() {
        return None;
    }
    let values = property
        .value
        .split(|byte| *byte == 0)
        .filter(|value| !value.is_empty())
        .map(|value| String::from_utf8_lossy(value).into_owned())
        .collect::<Vec<_>>();
    (!values.is_empty()).then(|| values.join("/"))
}

fn print_metadata(label: &str, metadata: &WindowMetadata) {
    println!("{label}:");
    println!("window: 0x{:08x}", metadata.window);
    println!(
        "geometry: {}x{}+{}+{}",
        metadata.geometry.width, metadata.geometry.height, metadata.geometry.x, metadata.geometry.y
    );
    println!("depth: {}", metadata.depth);
    println!("visual: 0x{:08x}", metadata.visual);
    println!(
        "override_redirect: {}",
        if metadata.override_redirect {
            "yes"
        } else {
            "no"
        }
    );
    println!("map_state: {}", map_state_name(metadata.map_state));
    println!(
        "WM_STATE: {}",
        if metadata.has_wm_state {
            "present"
        } else {
            "absent"
        }
    );
    println!(
        "WM_CLASS: {}",
        metadata.wm_class.as_deref().unwrap_or("<absent>")
    );
    println!(
        "_NET_WM_WINDOW_TYPE: {}",
        metadata.window_type.as_deref().unwrap_or("<absent>")
    );
    println!("role: {:?}", metadata.role);
}

#[cfg(test)]
mod tests {
    use super::{classify_window_role, WindowRole, WindowRoleFacts};
    fn facts(
        is_root: bool,
        is_top_level: bool,
        override_redirect: Option<bool>,
        has_wm_state: bool,
    ) -> WindowRoleFacts {
        WindowRoleFacts {
            is_root,
            is_top_level,
            override_redirect,
            has_wm_state,
        }
    }

    #[test]
    fn root_takes_precedence() {
        assert_eq!(
            classify_window_role(facts(true, true, Some(false), false)),
            WindowRole::Root
        );
    }
    #[test]
    fn override_redirect_is_explicit() {
        assert_eq!(
            classify_window_role(facts(false, false, Some(true), false)),
            WindowRole::OverrideRedirect
        );
    }
    #[test]
    fn descendant_with_wm_state_is_client() {
        assert_eq!(
            classify_window_role(facts(false, false, Some(false), true)),
            WindowRole::Client
        );
    }
    #[test]
    fn top_level_with_wm_state_is_client() {
        assert_eq!(
            classify_window_role(facts(false, true, Some(false), true)),
            WindowRole::Client
        );
    }
    #[test]
    fn top_level_without_wm_state_is_frame() {
        assert_eq!(
            classify_window_role(facts(false, true, Some(false), false)),
            WindowRole::TopLevelOrWmFrame
        );
    }
    #[test]
    fn descendant_without_wm_state_is_unknown() {
        assert_eq!(
            classify_window_role(facts(false, false, Some(false), false)),
            WindowRole::Unknown
        );
    }
}
