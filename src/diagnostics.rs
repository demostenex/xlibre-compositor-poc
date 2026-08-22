use std::error::Error;

use x11rb::connection::{Connection, RequestConnection};
use x11rb::protocol::{composite, damage, dri3, present, randr, xfixes};

use crate::x11::connection::X11Connection;

pub fn print_x11(connection: &X11Connection) -> Result<(), Box<dyn Error>> {
    let setup = connection.inner.setup();

    println!("XLibre connection: OK");
    println!("X server vendor: {}", String::from_utf8_lossy(&setup.vendor));
    println!("X server version: {}.{}", setup.protocol_major_version, setup.protocol_minor_version);
    println!("X server release: {}\n", setup.release_number);
    println!("X extensions:");

    for (name, extension) in [
        ("Composite", composite::X11_EXTENSION_NAME),
        ("Damage", damage::X11_EXTENSION_NAME),
        ("DRI3", dri3::X11_EXTENSION_NAME),
        ("Present", present::X11_EXTENSION_NAME),
        ("RandR", randr::X11_EXTENSION_NAME),
        ("XFixes", xfixes::X11_EXTENSION_NAME),
    ] {
        let available = connection.inner.extension_information(extension)?.is_some();
        println!("{name}: {}", if available { "yes" } else { "no" });
    }

    Ok(())
}
