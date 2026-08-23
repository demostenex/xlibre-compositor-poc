use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as CompositeConnectionExt;
use x11rb::protocol::shape::ConnectionExt as ShapeConnectionExt;
use x11rb::protocol::xfixes::ConnectionExt as XfixesConnectionExt;
use x11rb::protocol::xproto::{ConnectionExt as XprotoConnectionExt, MapState, WindowClass};

use super::compositor::selection_name;
use super::connection::X11Connection;
use super::shutdown::{wait_for_event_or_shutdown, SignalWake, WaitResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Version {
    major: u32,
    minor: u32,
}

impl Version {
    fn at_least(self, required: Self) -> bool {
        (self.major, self.minor) >= (required.major, required.minor)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OverlayGates {
    compositor_available: bool,
    composite_ok: bool,
    xfixes_ok: bool,
    shape_ok: bool,
}

impl OverlayGates {
    fn ready(self) -> bool {
        self.compositor_available && self.composite_ok && self.xfixes_ok && self.shape_ok
    }
}

pub(crate) fn run(
    connection: &X11Connection,
    expected_root_value: &str,
) -> Result<(), Box<dyn Error>> {
    let expected_root = parse_window_id(expected_root_value)?;
    let screen = &connection.inner.setup().roots[connection.screen_num()];
    let root = screen.root;

    println!("Composite overlay probe");
    println!("screen: {}", connection.screen_num());
    println!("root: 0x{root:08x}");
    println!("expected root: 0x{expected_root:08x}");
    if !root_matches(expected_root, root) {
        return Err(format!(
            "overlay probe refused: expected root 0x{expected_root:08x}, actual root 0x{root:08x}"
        )
        .into());
    }
    println!("root validation: MATCH");

    let compositor_available = print_selection(connection)?;
    let composite_ok = print_composite(connection)?;
    let xfixes_ok = print_xfixes(connection)?;
    let shape_ok = print_shape(connection)?;
    let gates = OverlayGates {
        compositor_available,
        composite_ok,
        xfixes_ok,
        shape_ok,
    };
    if !gates.ready() {
        return Err("overlay probe refused: local X11 capability gates failed".into());
    }

    let mut signal = SignalWake::install()?;
    let mut overlay = OverlayLease::acquire(connection, root)?;
    overlay.print_metadata()?;
    overlay.configure_input_passthrough()?;
    connection.inner.flush()?;
    println!("input region: EMPTY");
    println!("waiting for shutdown...");

    loop {
        match wait_for_event_or_shutdown(connection, &mut signal)? {
            WaitResult::Event(_) => {}
            WaitResult::Shutdown => {
                println!("shutdown requested");
                break;
            }
        }
    }

    println!("releasing overlay...");
    overlay.release()?;
    println!("overlay released");
    println!("cleanup complete");
    Ok(())
}

fn root_matches(expected: u32, actual: u32) -> bool {
    expected == actual
}

fn parse_window_id(value: &str) -> Result<u32, Box<dyn Error>> {
    let trimmed = value.trim();
    let (digits, radix) = trimmed
        .strip_prefix("0x")
        .map_or((trimmed, 10), |digits| (digits, 16));
    if digits.is_empty() {
        return Err("expected root XID is empty".into());
    }
    Ok(u32::from_str_radix(digits, radix)?)
}

fn print_selection(connection: &X11Connection) -> Result<bool, Box<dyn Error>> {
    let name = selection_name(connection.screen_num());
    let atom = connection.inner.intern_atom(true, name.as_bytes())?.reply()?.atom;
    let owner = if atom == x11rb::NONE {
        x11rb::NONE
    } else {
        connection.inner.get_selection_owner(atom)?.reply()?.owner
    };
    println!("compositor owner: {}", if owner == x11rb::NONE {
        "NONE".to_owned()
    } else {
        format!("0x{owner:08x}")
    });
    if owner == x11rb::NONE {
        Ok(true)
    } else {
        println!("overlay probe refused: compositor selection is occupied");
        Ok(false)
    }
}

fn print_composite(connection: &X11Connection) -> Result<bool, Box<dyn Error>> {
    let required = Version { major: 0, minor: 3 };
    let version = match connection.inner.composite_query_version(0, 4)?.reply() {
        Ok(reply) => Some(Version {
            major: reply.major_version as u32,
            minor: reply.minor_version as u32,
        }),
        Err(_) => None,
    };
    let pass = version.map_or(false, |version| version.at_least(required));
    match version {
        Some(version) => println!(
            "Composite: {} {}.{} required >= {}.{}",
            if pass { "PASS" } else { "FAIL" },
            version.major,
            version.minor,
            required.major,
            required.minor
        ),
        None => println!("Composite: FAIL unavailable"),
    }
    Ok(pass)
}

fn print_xfixes(connection: &X11Connection) -> Result<bool, Box<dyn Error>> {
    let required = Version { major: 2, minor: 0 };
    let version = match connection.inner.xfixes_query_version(6, 1)?.reply() {
        Ok(reply) => Some(Version {
            major: reply.major_version,
            minor: reply.minor_version,
        }),
        Err(_) => None,
    };
    let pass = version.map_or(false, |version| version.at_least(required));
    match version {
        Some(version) => println!(
            "XFixes: {} {}.{} required >= {}.{}",
            if pass { "PASS" } else { "FAIL" },
            version.major,
            version.minor,
            required.major,
            required.minor
        ),
        None => println!("XFixes: FAIL unavailable"),
    }
    Ok(pass)
}

fn print_shape(connection: &X11Connection) -> Result<bool, Box<dyn Error>> {
    let required = Version { major: 1, minor: 1 };
    let version = match connection.inner.shape_query_version()?.reply() {
        Ok(reply) => Some(Version {
            major: reply.major_version as u32,
            minor: reply.minor_version as u32,
        }),
        Err(_) => None,
    };
    let pass = version.map_or(false, |version| version.at_least(required));
    match version {
        Some(version) => println!(
            "Shape: {} {}.{} required >= {}.{}",
            if pass { "PASS" } else { "FAIL" },
            version.major,
            version.minor,
            required.major,
            required.minor
        ),
        None => println!("Shape: FAIL unavailable"),
    }
    Ok(pass)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OverlayMetadata {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    depth: u8,
    visual: u32,
    class: WindowClass,
    override_redirect: bool,
    map_state: MapState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct OverlayExpectations {
    root_width: u16,
    root_height: u16,
    root_depth: u8,
    root_visual: u32,
}

fn validate_overlay_metadata(
    metadata: OverlayMetadata,
    expected: OverlayExpectations,
) -> Result<(), &'static str> {
    if metadata.x != 0 || metadata.y != 0 {
        return Err("overlay origin is not 0,0");
    }
    if metadata.width != expected.root_width || metadata.height != expected.root_height {
        return Err("overlay size does not match root");
    }
    if metadata.border_width != 0 {
        return Err("overlay border width is not zero");
    }
    if metadata.depth != expected.root_depth {
        return Err("overlay depth does not match root");
    }
    if metadata.visual != expected.root_visual {
        return Err("overlay visual does not match root");
    }
    if metadata.class != WindowClass::INPUT_OUTPUT {
        return Err("overlay class is not InputOutput");
    }
    if !metadata.override_redirect {
        return Err("overlay is not override-redirect");
    }
    if metadata.map_state != MapState::VIEWABLE {
        return Err("overlay is not Viewable");
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReleaseState {
    Active,
    Attempted,
    Released,
}

fn mark_release_attempted(state: &mut ReleaseState) -> bool {
    if *state != ReleaseState::Active {
        return false;
    }
    *state = ReleaseState::Attempted;
    true
}

fn mark_release_confirmed(state: &mut ReleaseState) {
    if *state == ReleaseState::Attempted {
        *state = ReleaseState::Released;
    }
}

struct OverlayLease<'a> {
    connection: &'a X11Connection,
    root: u32,
    overlay: u32,
    input_passthrough_applied: bool,
    release_state: ReleaseState,
}

impl<'a> OverlayLease<'a> {
    fn acquire(connection: &'a X11Connection, root: u32) -> Result<Self, Box<dyn Error>> {
        let overlay = connection
            .inner
            .composite_get_overlay_window(root)?
            .reply()?
            .overlay_win;
        println!("overlay acquired: 0x{overlay:08x}");
        Ok(Self {
            connection,
            root,
            overlay,
            input_passthrough_applied: false,
            release_state: ReleaseState::Active,
        })
    }

    fn print_metadata(&self) -> Result<(), Box<dyn Error>> {
        let screen = &self.connection.inner.setup().roots[self.connection.screen_num()];
        let root_geometry = self.connection.inner.get_geometry(screen.root)?.reply()?;
        let overlay_geometry = self.connection.inner.get_geometry(self.overlay)?.reply()?;
        let attributes = self
            .connection
            .inner
            .get_window_attributes(self.overlay)?
            .reply()?;
        let metadata = OverlayMetadata {
            x: overlay_geometry.x,
            y: overlay_geometry.y,
            width: overlay_geometry.width,
            height: overlay_geometry.height,
            border_width: overlay_geometry.border_width,
            depth: overlay_geometry.depth,
            visual: attributes.visual,
            class: attributes.class,
            override_redirect: attributes.override_redirect,
            map_state: attributes.map_state,
        };
        let expected = OverlayExpectations {
            root_width: root_geometry.width,
            root_height: root_geometry.height,
            root_depth: screen.root_depth,
            root_visual: screen.root_visual,
        };
        let geometry_ok = metadata.x == 0
            && metadata.y == 0
            && metadata.width == expected.root_width
            && metadata.height == expected.root_height;
        let border_ok = metadata.border_width == 0;
        let depth_ok = metadata.depth == expected.root_depth;
        let visual_ok = metadata.visual == expected.root_visual;
        let class_ok = metadata.class == WindowClass::INPUT_OUTPUT;
        let override_ok = metadata.override_redirect;
        let map_ok = metadata.map_state == MapState::VIEWABLE;
        println!(
            "geometry: {} {}x{}+{}+{}",
            if geometry_ok { "PASS" } else { "FAIL" },
            metadata.width,
            metadata.height,
            metadata.x,
            metadata.y
        );
        println!(
            "border: {} {}",
            if border_ok { "PASS" } else { "FAIL" },
            metadata.border_width
        );
        println!(
            "depth: {} {}",
            if depth_ok { "PASS" } else { "FAIL" },
            metadata.depth
        );
        println!(
            "visual: {} 0x{:08x}",
            if visual_ok { "PASS" } else { "FAIL" },
            metadata.visual
        );
        println!(
            "class: {} {:?}",
            if class_ok { "PASS" } else { "FAIL" },
            metadata.class
        );
        println!(
            "override redirect: {}",
            if override_ok { "PASS" } else { "FAIL" }
        );
        println!(
            "map state: {} {:?}",
            if map_ok { "PASS" } else { "FAIL" },
            metadata.map_state
        );
        validate_overlay_metadata(metadata, expected)
            .map_err(|error| format!("overlay metadata validation failed: {error}" ).into())
    }

    fn configure_input_passthrough(&mut self) -> Result<(), Box<dyn Error>> {
        let region = self.connection.inner.generate_id()?;
        self.connection
            .inner
            .xfixes_create_region(region, &[])?
            .check()?;
        let set_result = (|| -> Result<(), Box<dyn Error>> {
            self.connection
                .inner
                .xfixes_set_window_shape_region(
                    self.overlay,
                    x11rb::protocol::shape::SK::INPUT,
                    0,
                    0,
                    region,
                )?
                .check()?;
            self.input_passthrough_applied = true;
            Ok(())
        })();
        let destroy_result = (|| -> Result<(), Box<dyn Error>> {
            self.connection
                .inner
                .xfixes_destroy_region(region)?
                .check()?;
            Ok(())
        })();
        set_result?;
        destroy_result?;
        Ok(())
    }

    fn restore_input_shape(&mut self) -> Result<(), Box<dyn Error>> {
        if !self.input_passthrough_applied {
            return Ok(());
        }
        self.connection
            .inner
            .xfixes_set_window_shape_region(
                self.overlay,
                x11rb::protocol::shape::SK::INPUT,
                0,
                0,
                x11rb::NONE,
            )?
            .check()?;
        self.input_passthrough_applied = false;
        Ok(())
    }

    fn release(&mut self) -> Result<(), Box<dyn Error>> {
        self.restore_input_shape()?;
        self.release_overlay()
    }

    fn release_overlay(&mut self) -> Result<(), Box<dyn Error>> {
        if self.release_state != ReleaseState::Active {
            return Ok(());
        }
        if !mark_release_attempted(&mut self.release_state) {
            return Ok(());
        }
        self.connection
            .inner
            .composite_release_overlay_window(self.root)?
            .check()?;
        self.connection.inner.flush()?;
        mark_release_confirmed(&mut self.release_state);
        Ok(())
    }
}

impl Drop for OverlayLease<'_> {
    fn drop(&mut self) {
        let _ = self.restore_input_shape();
        if self.release_state != ReleaseState::Active {
            return;
        }
        if !mark_release_attempted(&mut self.release_state) {
            return;
        }
        if let Ok(cookie) = self
            .connection
            .inner
            .composite_release_overlay_window(self.root)
        {
            if cookie.check().is_ok() && self.connection.inner.flush().is_ok() {
                mark_release_confirmed(&mut self.release_state);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        mark_release_attempted, mark_release_confirmed, validate_overlay_metadata, MapState,
        OverlayExpectations, OverlayGates, OverlayMetadata, ReleaseState, Version, WindowClass,
    };

    fn metadata() -> OverlayMetadata {
        OverlayMetadata {
            x: 0,
            y: 0,
            width: 1280,
            height: 720,
            border_width: 0,
            depth: 24,
            visual: 0x21,
            class: WindowClass::INPUT_OUTPUT,
            override_redirect: true,
            map_state: MapState::VIEWABLE,
        }
    }

    fn expectations() -> OverlayExpectations {
        OverlayExpectations {
            root_width: 1280,
            root_height: 720,
            root_depth: 24,
            root_visual: 0x21,
        }
    }

    fn assert_metadata_fails(mutated: impl FnOnce(&mut OverlayMetadata)) {
        let mut value = metadata();
        mutated(&mut value);
        assert!(validate_overlay_metadata(value, expectations()).is_err());
    }

    #[test]
    fn root_match_is_required() {
        assert!(super::root_matches(0x381, 0x381));
        assert!(!super::root_matches(0x381, 0x380));
    }

    #[test]
    fn occupied_selection_blocks_probe() {
        let gates = OverlayGates {
            compositor_available: false,
            composite_ok: true,
            xfixes_ok: true,
            shape_ok: true,
        };
        assert!(!gates.ready());
    }

    #[test]
    fn required_versions_block_probe() {
        assert!(!Version { major: 0, minor: 2 }.at_least(Version { major: 0, minor: 3 }));
        assert!(!Version { major: 1, minor: 9 }.at_least(Version { major: 2, minor: 0 }));
        assert!(!Version { major: 1, minor: 0 }.at_least(Version { major: 1, minor: 1 }));
    }

    #[test]
    fn all_overlay_gates_are_required() {
        let gates = OverlayGates {
            compositor_available: true,
            composite_ok: true,
            xfixes_ok: true,
            shape_ok: true,
        };
        assert!(gates.ready());
    }

    #[test]
    fn correct_metadata_passes() {
        assert!(validate_overlay_metadata(metadata(), expectations()).is_ok());
    }

    #[test]
    fn incorrect_origin_fails() {
        assert_metadata_fails(|metadata| metadata.x = 1);
    }

    #[test]
    fn incorrect_size_fails() {
        assert_metadata_fails(|metadata| metadata.width = 1279);
    }

    #[test]
    fn incorrect_border_fails() {
        assert_metadata_fails(|metadata| metadata.border_width = 1);
    }

    #[test]
    fn incorrect_depth_fails() {
        assert_metadata_fails(|metadata| metadata.depth = 32);
    }

    #[test]
    fn incorrect_visual_fails() {
        assert_metadata_fails(|metadata| metadata.visual = 0x22);
    }

    #[test]
    fn incorrect_override_redirect_fails() {
        assert_metadata_fails(|metadata| metadata.override_redirect = false);
    }

    #[test]
    fn incorrect_class_fails() {
        assert_metadata_fails(|metadata| metadata.class = WindowClass::INPUT_ONLY);
    }

    #[test]
    fn incorrect_map_state_fails() {
        assert_metadata_fails(|metadata| metadata.map_state = MapState::UNMAPPED);
    }

    #[test]
    fn release_state_requires_confirmation_and_does_not_retry() {
        let mut state = ReleaseState::Active;
        assert!(mark_release_attempted(&mut state));
        assert_eq!(state, ReleaseState::Attempted);
        assert!(!mark_release_attempted(&mut state));
        mark_release_confirmed(&mut state);
        assert_eq!(state, ReleaseState::Released);
        assert!(!mark_release_attempted(&mut state));
    }

    #[test]
    fn each_overlay_gate_blocks_probe() {
        for mutate in [
            |gates: &mut OverlayGates| gates.compositor_available = false,
            |gates: &mut OverlayGates| gates.composite_ok = false,
            |gates: &mut OverlayGates| gates.xfixes_ok = false,
            |gates: &mut OverlayGates| gates.shape_ok = false,
        ] {
            let mut gates = OverlayGates {
                compositor_available: true,
                composite_ok: true,
                xfixes_ok: true,
                shape_ok: true,
            };
            mutate(&mut gates);
            assert!(!gates.ready());
        }
    }
}
