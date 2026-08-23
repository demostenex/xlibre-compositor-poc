use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as CompositeConnectionExt;
use x11rb::protocol::randr::ConnectionExt as RandrConnectionExt;
use x11rb::protocol::shape::ConnectionExt as ShapeConnectionExt;
use x11rb::protocol::xfixes::ConnectionExt as XfixesConnectionExt;
use x11rb::protocol::xproto::ConnectionExt;

use super::compositor::selection_name;
use super::connection::X11Connection;
use super::shutdown::SignalWake;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct Version {
    major: u32,
    minor: u32,
}

impl Version {
    fn at_least(self, required: Version) -> bool {
        (self.major, self.minor) >= (required.major, required.minor)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct TakeoverGates {
    compositor_available: bool,
    composite_ok: bool,
    xfixes_ok: bool,
    shape_ok: bool,
    egl_config_ok: bool,
    root_stable: bool,
    signal_ready: bool,
}

impl TakeoverGates {
    fn ready(self) -> bool {
        self.compositor_available
            && self.composite_ok
            && self.xfixes_ok
            && self.shape_ok
            && self.egl_config_ok
            && self.root_stable
            && self.signal_ready
    }
}

pub(crate) fn run(connection: &X11Connection) -> Result<(), Box<dyn Error>> {
    let _signal = match SignalWake::install() {
        Ok(signal) => {
            println!("shutdown wake: PASS");
            Some(signal)
        }
        Err(error) => {
            println!("shutdown wake: FAIL");
            println!("  reason: {error}");
            None
        }
    };
    let signal_ready = _signal.is_some();
    let baseline_geometry = read_root_geometry(connection)?;
    let compositor_available = print_selection(connection)?;
    let composite_ok = print_composite(connection)?;
    let xfixes_ok = print_xfixes(connection)?;
    let shape_ok = print_shape(connection)?;
    let screen = &connection.inner.setup().roots[connection.screen_num()];
    let egl_config_ok = match crate::graphics::egl::EglContext::config_preflight(connection) {
        Ok(report) => {
            let visual_match = report.visual == screen.root_visual;
            let depth_match = report.depth == screen.root_depth;
            let compatible = visual_match && depth_match;
            println!("EGL config: {}", if compatible { "PASS" } else { "FAIL" });
            println!("  native visual: 0x{:08x}", report.visual);
            println!("  native depth: {}", report.depth);
            println!("  root visual match: {}", if visual_match { "yes" } else { "no" });
            println!("  root depth match: {}", if depth_match { "yes" } else { "no" });
            println!("  required EGL/GL extensions: PASS");
            compatible
        }
        Err(error) => {
            println!("EGL config: FAIL");
            println!("  reason: {error}");
            false
        }
    };
    let final_geometry = read_root_geometry(connection)?;
    let root_stable = baseline_geometry == final_geometry;
    print_root(connection, &baseline_geometry, &final_geometry, root_stable)?;

    let gates = TakeoverGates {
        compositor_available,
        composite_ok,
        xfixes_ok,
        shape_ok,
        egl_config_ok,
        root_stable,
        signal_ready,
    };
    println!();
    println!(
        "takeover ready: {}",
        if gates.ready() { "YES" } else { "NO" }
    );
    if !gates.compositor_available {
        println!("reason: compositor selection already owned");
    }
    if !gates.composite_ok {
        println!("reason: Composite < 0.3 or unavailable");
    }
    if !gates.xfixes_ok {
        println!("reason: XFixes < 2.0 or unavailable");
    }
    if !gates.shape_ok {
        println!("reason: Shape < 1.1 or unavailable");
    }
    if !gates.egl_config_ok {
        println!("reason: EGL config compatibility failed");
    }
    if !gates.root_stable {
        println!("reason: unstable root geometry");
    }
    if !gates.signal_ready {
        println!("reason: graceful shutdown infrastructure unavailable");
    }
    Ok(())
}

fn print_selection(connection: &X11Connection) -> Result<bool, Box<dyn Error>> {
    let name = selection_name(connection.screen_num());
    let atom = connection.inner.intern_atom(true, name.as_bytes())?.reply()?.atom;
    let owner = if atom == x11rb::NONE {
        x11rb::NONE
    } else {
        connection.inner.get_selection_owner(atom)?.reply()?.owner
    };
    println!("compositor selection:");
    println!("  name: {name}");
    if owner == x11rb::NONE {
        println!("  owner: NONE");
        println!("  status: AVAILABLE");
        Ok(true)
    } else {
        println!("  owner: 0x{owner:08x}");
        println!("  status: OCCUPIED");
        Ok(false)
    }
}

fn print_composite(connection: &X11Connection) -> Result<bool, Box<dyn Error>> {
    let version = match connection.inner.composite_query_version(0, 4) {
        Ok(cookie) => match cookie.reply() {
            Ok(version) => Version {
                major: version.major_version as u32,
                minor: version.minor_version as u32,
            },
            Err(error) => {
                println!("Composite: FAIL ({error})");
                return Ok(false);
            }
        },
        Err(error) => {
            println!("Composite: FAIL ({error})");
            return Ok(false);
        }
    };
    let required = Version { major: 0, minor: 3 };
    let pass = version.at_least(required);
    println!(
        "Composite: {} {}.{} required >= {}.{}",
        if pass { "PASS" } else { "FAIL" },
        version.major,
        version.minor,
        required.major,
        required.minor
    );
    Ok(pass)
}

fn print_xfixes(connection: &X11Connection) -> Result<bool, Box<dyn Error>> {
    let version = match connection.inner.xfixes_query_version(6, 1) {
        Ok(cookie) => match cookie.reply() {
            Ok(version) => Version {
                major: version.major_version,
                minor: version.minor_version,
            },
            Err(error) => {
                println!("XFixes: FAIL ({error})");
                return Ok(false);
            }
        },
        Err(error) => {
            println!("XFixes: FAIL ({error})");
            return Ok(false);
        }
    };
    let required = Version { major: 2, minor: 0 };
    let pass = version.at_least(required);
    println!(
        "XFixes: {} {}.{} required >= {}.{}",
        if pass { "PASS" } else { "FAIL" },
        version.major,
        version.minor,
        required.major,
        required.minor
    );
    Ok(pass)
}

fn print_shape(connection: &X11Connection) -> Result<bool, Box<dyn Error>> {
    let version = match connection.inner.shape_query_version() {
        Ok(cookie) => match cookie.reply() {
            Ok(version) => Version {
                major: version.major_version as u32,
                minor: version.minor_version as u32,
            },
            Err(error) => {
                println!("Shape: FAIL ({error})");
                return Ok(false);
            }
        },
        Err(error) => {
            println!("Shape: FAIL ({error})");
            return Ok(false);
        }
    };
    let required = Version { major: 1, minor: 1 };
    let pass = version.at_least(required);
    println!(
        "Shape: {} {}.{} required >= {}.{}",
        if pass { "PASS" } else { "FAIL" },
        version.major,
        version.minor,
        required.major,
        required.minor
    );
    Ok(pass)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RootGeometry {
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
}

fn read_root_geometry(connection: &X11Connection) -> Result<RootGeometry, Box<dyn Error>> {
    let root = connection.inner.setup().roots[connection.screen_num()].root;
    let geometry = connection.inner.get_geometry(root)?.reply()?;
    Ok(RootGeometry {
        x: geometry.x,
        y: geometry.y,
        width: geometry.width,
        height: geometry.height,
        border_width: geometry.border_width,
    })
}

fn print_root(
    connection: &X11Connection,
    baseline: &RootGeometry,
    final_geometry: &RootGeometry,
    stable: bool,
) -> Result<(), Box<dyn Error>> {
    let screen = &connection.inner.setup().roots[connection.screen_num()];
    let root = screen.root;
    let outputs = match connection.inner.randr_get_screen_resources_current(root) {
        Ok(cookie) => cookie.reply().ok().map(|resources| resources.outputs.len()),
        Err(_) => None,
    };

    println!("root/screen:");
    println!("  screen index: {}", connection.screen_num());
    println!("  root: 0x{root:08x}");
    println!(
        "  geometry: {}x{}+{}+{}",
        final_geometry.width, final_geometry.height, final_geometry.x, final_geometry.y
    );
    println!(
        "  baseline geometry: {}x{}+{}+{}",
        baseline.width, baseline.height, baseline.x, baseline.y
    );
    println!("  border width: {}", final_geometry.border_width);
    println!("  root visual: 0x{:08x}", screen.root_visual);
    println!("  root depth: {}", screen.root_depth);
    match outputs {
        Some(count) => println!("  RandR outputs on this X screen: {count}"),
        None => println!("  RandR outputs: unavailable"),
    }
    println!("root stability: {}", if stable { "PASS" } else { "FAIL" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TakeoverGates, Version};

    #[test]
    fn versions_accept_equal_or_newer() {
        assert!(Version { major: 1, minor: 1 }.at_least(Version { major: 1, minor: 1 }));
        assert!(Version { major: 2, minor: 0 }.at_least(Version { major: 1, minor: 1 }));
        assert!(!Version { major: 0, minor: 4 }.at_least(Version { major: 1, minor: 0 }));
    }

    #[test]
    fn required_extension_versions_are_enforced() {
        assert!(!Version { major: 0, minor: 2 }.at_least(Version { major: 0, minor: 3 }));
        assert!(!Version { major: 1, minor: 9 }.at_least(Version { major: 2, minor: 0 }));
        assert!(!Version { major: 1, minor: 0 }.at_least(Version { major: 1, minor: 1 }));
    }

    fn all_ready() -> TakeoverGates {
        TakeoverGates {
            compositor_available: true,
            composite_ok: true,
            xfixes_ok: true,
            shape_ok: true,
            egl_config_ok: true,
            root_stable: true,
            signal_ready: true,
        }
    }

    #[test]
    fn all_gates_make_preflight_ready() {
        assert!(all_ready().ready());
    }

    #[test]
    fn occupied_selection_blocks_readiness() {
        let mut gates = all_ready();
        gates.compositor_available = false;
        assert!(!gates.ready());
    }

    #[test]
    fn each_capability_gate_blocks_readiness() {
        for mutate in [
            |gates: &mut TakeoverGates| gates.composite_ok = false,
            |gates: &mut TakeoverGates| gates.xfixes_ok = false,
            |gates: &mut TakeoverGates| gates.shape_ok = false,
            |gates: &mut TakeoverGates| gates.egl_config_ok = false,
            |gates: &mut TakeoverGates| gates.root_stable = false,
            |gates: &mut TakeoverGates| gates.signal_ready = false,
        ] {
            let mut gates = all_ready();
            mutate(&mut gates);
            assert!(!gates.ready());
        }
    }
}
