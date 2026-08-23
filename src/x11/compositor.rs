use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::ConnectionExt;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ClientMessageData, ClientMessageEvent, EventMask, Window,
    WindowClass,
};
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;

use super::connection::X11Connection;

const MANAGER_ATOM_NAME: &[u8] = b"MANAGER";
const TIMESTAMP_PROPERTY_NAME: &[u8] = b"_XCOMPOSITE_COMPOSITOR_TIMESTAMP";

pub struct CompositorOwnership {
    pub selection: Atom,
    pub owner_window: Window,
    pub timestamp: xproto::Timestamp,
    pub root: Window,
}

pub fn selection_name(screen_num: usize) -> String {
    format!("_NET_WM_CM_S{screen_num}")
}

pub fn should_refuse_takeover(owner: Option<Window>) -> bool {
    owner.is_some()
}

pub fn selection_clear_matches(
    event: &xproto::SelectionClearEvent,
    ownership: &CompositorOwnership,
) -> bool {
    event.selection == ownership.selection && event.owner == ownership.owner_window
}

pub fn probe(connection: &X11Connection) -> Result<(), Box<dyn Error>> {
    let selection_name = selection_name(connection.screen_num());
    let selection = connection.inner.intern_atom(true, selection_name.as_bytes())?.reply()?.atom;

    if selection == x11rb::NONE {
        println!("Compositor selection: {selection_name}");
        println!("owner: NONE");
        return Ok(());
    }

    let owner = connection.inner.get_selection_owner(selection)?.reply()?.owner;

    println!("Compositor selection: {selection_name}");
    match owner {
        x11rb::NONE => println!("owner: NONE"),
        owner => println!("owner: 0x{owner:08x}"),
    }
    Ok(())
}

impl CompositorOwnership {
    pub fn claim(connection: &X11Connection) -> Result<Self, Box<dyn Error>> {
        let selection_name = selection_name(connection.screen_num());
        let selection = connection.inner.intern_atom(false, selection_name.as_bytes())?.reply()?.atom;
        let current_owner = connection.inner.get_selection_owner(selection)?.reply()?.owner;
        if should_refuse_takeover((current_owner != x11rb::NONE).then_some(current_owner)) {
            return Err(format!(
                "compositor already active\nselection: {selection_name}\ncurrent owner: 0x{current_owner:08x}"
            )
            .into());
        }

        let screen = &connection.inner.setup().roots[connection.screen_num()];
        let owner_window = connection.inner.generate_id()?;
        let timestamp_property = connection
            .inner
            .intern_atom(false, TIMESTAMP_PROPERTY_NAME)?
            .reply()?
            .atom;
        let manager_atom = connection.inner.intern_atom(false, MANAGER_ATOM_NAME)?.reply()?.atom;

        connection
            .inner
            .create_window(
                x11rb::COPY_FROM_PARENT as u8,
                owner_window,
                screen.root,
                0,
                0,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &xproto::CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
            )?
            .check()?;
        connection.inner.flush()?;

        let timestamp = Self::server_timestamp(connection, owner_window, timestamp_property)?;
        connection
            .inner
            .set_selection_owner(owner_window, selection, timestamp)?
            .check()?;
        connection.inner.flush()?;

        let owner = connection.inner.get_selection_owner(selection)?.reply()?.owner;
        if owner != owner_window {
            let _ = connection.inner.destroy_window(owner_window);
            let _ = connection.inner.flush();
            return Err(format!(
                "failed to acquire compositor selection: expected 0x{owner_window:08x}, got {}",
                if owner == x11rb::NONE {
                    "NONE".to_owned()
                } else {
                    format!("0x{owner:08x}")
                }
            )
            .into());
        }

        let event = ClientMessageEvent {
            response_type: x11rb::protocol::xproto::CLIENT_MESSAGE_EVENT,
            format: 32,
            sequence: 0,
            window: screen.root,
            type_: manager_atom,
            data: ClientMessageData::from([timestamp, selection, owner_window, 0, 0]),
        };
        connection
            .inner
            .send_event(false, screen.root, EventMask::STRUCTURE_NOTIFY, event)?
            .check()?;
        connection.inner.flush()?;

        println!("compositor ownership acquired");
        println!("selection: {selection_name}");
        println!("owner window: 0x{owner_window:08x}");
        println!("timestamp: {timestamp}");
        println!("MANAGER announced");

        Ok(Self {
            selection,
            owner_window,
            timestamp,
            root: screen.root,
        })
    }

    fn server_timestamp(
        connection: &X11Connection,
        owner_window: Window,
        property: Atom,
    ) -> Result<xproto::Timestamp, Box<dyn Error>> {
        connection
            .inner
            .change_property8(
                xproto::PropMode::APPEND,
                owner_window,
                property,
                AtomEnum::STRING,
                &[],
            )?
            .check()?;
        connection.inner.flush()?;

        loop {
            match connection.inner.wait_for_event()? {
                Event::PropertyNotify(event)
                    if event.window == owner_window && event.atom == property =>
                {
                    return Ok(event.time);
                }
                _ => {}
            }
        }
    }

    pub fn run_event_loop(&self, connection: &X11Connection) -> Result<(), Box<dyn Error>> {
        let _ = (self.timestamp, self.root);
        loop {
            if let Event::SelectionClear(event) = connection.inner.wait_for_event()? {
                if selection_clear_matches(&event, self) {
                    println!("compositor ownership lost");
                    connection.inner.destroy_window(self.owner_window)?.check()?;
                    connection.inner.flush()?;
                    return Ok(());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        selection_name, selection_clear_matches, should_refuse_takeover, CompositorOwnership,
    };
    use x11rb::protocol::xproto::SelectionClearEvent;

    fn selection_clear(selection: u32, owner: u32) -> SelectionClearEvent {
        SelectionClearEvent {
            response_type: 0,
            sequence: 0,
            time: 0,
            owner,
            selection,
        }
    }

    fn ownership(selection: u32, owner_window: u32) -> CompositorOwnership {
        CompositorOwnership {
            selection,
            owner_window,
            timestamp: 1,
            root: 2,
        }
    }

    #[test]
    fn selection_name_for_screen_zero() {
        assert_eq!(selection_name(0), "_NET_WM_CM_S0");
    }

    #[test]
    fn selection_name_for_nonzero_screen() {
        assert_eq!(selection_name(1), "_NET_WM_CM_S1");
    }

    #[test]
    fn empty_selection_can_be_claimed() {
        assert!(!should_refuse_takeover(None));
    }

    #[test]
    fn existing_selection_must_be_refused() {
        assert!(should_refuse_takeover(Some(42)));
    }

    #[test]
    fn matching_selection_clear_is_recognized() {
        let ownership = ownership(10, 20);
        assert!(selection_clear_matches(&selection_clear(10, 20), &ownership));
    }

    #[test]
    fn different_selection_clear_is_ignored() {
        let ownership = ownership(10, 20);
        assert!(!selection_clear_matches(&selection_clear(11, 20), &ownership));
    }

    #[test]
    fn different_owner_selection_clear_is_ignored() {
        let ownership = ownership(10, 20);
        assert!(!selection_clear_matches(&selection_clear(10, 21), &ownership));
    }
}
