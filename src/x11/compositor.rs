use std::error::Error;
use std::cell::Cell;

use x11rb::connection::Connection;
use x11rb::protocol::composite::{self, ConnectionExt as CompositeConnectionExt};
use x11rb::protocol::Event;
use x11rb::protocol::xproto::ConnectionExt;
use x11rb::protocol::xproto::{
    self, Atom, AtomEnum, ClientMessageData, ClientMessageEvent, EventMask, Window,
    WindowClass,
};
use x11rb::wrapper::ConnectionExt as WrapperConnectionExt;

use super::connection::X11Connection;
use super::capture::WindowRole;

const MANAGER_ATOM_NAME: &[u8] = b"MANAGER";
const TIMESTAMP_PROPERTY_NAME: &[u8] = b"_XCOMPOSITE_COMPOSITOR_TIMESTAMP";

pub struct CompositorOwnership {
    pub selection: Atom,
    pub owner_window: Window,
    pub timestamp: xproto::Timestamp,
    pub root: Window,
    active: Cell<bool>,
}
pub struct RedirectedWindow {
    pub window: Window,
    pub mode: composite::Redirect,
    active: Cell<bool>,
}

pub fn selection_name(screen_num: usize) -> String {
    format!("_NET_WM_CM_S{screen_num}")
}

pub fn should_refuse_takeover(owner: Option<Window>) -> bool {
    owner.is_some()
}

pub fn can_redirect_role(role: WindowRole) -> bool {
    role == WindowRole::Client
}

pub fn redirect_target(requested: Window, role: WindowRole) -> Option<Window> {
    can_redirect_role(role).then_some(requested)
}

pub fn owned_redirect_mode() -> composite::Redirect {
    composite::Redirect::AUTOMATIC
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

fn compositor_selection_atom(
    connection: &X11Connection,
    screen_num: usize,
) -> Result<Atom, Box<dyn Error>> {
    let name = selection_name(screen_num);
    Ok(connection.inner.intern_atom(false, name.as_bytes())?.reply()?.atom)
}

fn current_selection_owner(
    connection: &X11Connection,
    selection: Atom,
) -> Result<Window, Box<dyn Error>> {
    Ok(connection.inner.get_selection_owner(selection)?.reply()?.owner)
}

fn create_owner_window(connection: &X11Connection) -> Result<Window, Box<dyn Error>> {
    let screen = &connection.inner.setup().roots[connection.screen_num()];
    let owner_window = connection.inner.generate_id()?;
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
    Ok(owner_window)
}

fn acquire_selection(
    connection: &X11Connection,
    selection: Atom,
    owner_window: Window,
    timestamp: xproto::Timestamp,
) -> Result<(), Box<dyn Error>> {
    connection
        .inner
        .set_selection_owner(owner_window, selection, timestamp)?
        .check()?;
    connection.inner.flush()?;
    Ok(())
}

fn announce_manager(
    connection: &X11Connection,
    root: Window,
    manager_atom: Atom,
    timestamp: xproto::Timestamp,
    selection: Atom,
    owner_window: Window,
) -> Result<(), Box<dyn Error>> {
    let event = ClientMessageEvent {
        response_type: x11rb::protocol::xproto::CLIENT_MESSAGE_EVENT,
        format: 32,
        sequence: 0,
        window: root,
        type_: manager_atom,
        data: ClientMessageData::from([timestamp, selection, owner_window, 0, 0]),
    };
    connection
        .inner
        .send_event(false, root, EventMask::STRUCTURE_NOTIFY, event)?
        .check()?;
    connection.inner.flush()?;
    Ok(())
}

impl CompositorOwnership {
    pub fn claim(connection: &X11Connection) -> Result<Self, Box<dyn Error>> {
        let selection_name = selection_name(connection.screen_num());
        let selection = compositor_selection_atom(connection, connection.screen_num())?;
        let current_owner = current_selection_owner(connection, selection)?;
        if should_refuse_takeover((current_owner != x11rb::NONE).then_some(current_owner)) {
            return Err(format!(
                "compositor already active\nselection: {selection_name}\ncurrent owner: 0x{current_owner:08x}"
            )
            .into());
        }

        let timestamp_property = connection
            .inner
            .intern_atom(false, TIMESTAMP_PROPERTY_NAME)?
            .reply()?
            .atom;
        let manager_atom = connection.inner.intern_atom(false, MANAGER_ATOM_NAME)?.reply()?.atom;
        let owner_window = create_owner_window(connection)?;
        let timestamp = Self::server_timestamp(connection, owner_window, timestamp_property)?;
        acquire_selection(connection, selection, owner_window, timestamp)?;

        let owner = current_selection_owner(connection, selection)?;
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

        let root = connection.inner.setup().roots[connection.screen_num()].root;
        announce_manager(
            connection,
            root,
            manager_atom,
            timestamp,
            selection,
            owner_window,
        )?;

        println!("compositor ownership acquired");
        println!("selection: {selection_name}");
        println!("owner window: 0x{owner_window:08x}");
        println!("timestamp: {timestamp}");
        println!("MANAGER announced");

        Ok(Self {
            selection,
            owner_window,
            timestamp,
            root,
            active: Cell::new(true),
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
                    self.release(connection)?;
                    return Ok(());
                }
            }
        }
    }

    pub fn release(&self, connection: &X11Connection) -> Result<(), Box<dyn Error>> {
        if !self.active.get() {
            return Ok(());
        }
        connection.inner.destroy_window(self.owner_window)?.check()?;
        connection.inner.flush()?;
        self.active.set(false);
        Ok(())
    }
}

impl RedirectedWindow {
    pub fn redirect(
        connection: &X11Connection,
        window: Window,
    ) -> Result<Self, Box<dyn Error>> {
        let mode = owned_redirect_mode();
        connection
            .inner
            .composite_redirect_window(window, mode)?
            .check()?;
        connection.inner.flush()?;
        println!("CompositeRedirectWindow: AUTOMATIC");
        Ok(Self {
            window,
            mode,
            active: Cell::new(true),
        })
    }

    pub fn unredirect(&self, connection: &X11Connection) -> Result<(), Box<dyn Error>> {
        if !self.active.get() {
            return Ok(());
        }
        connection
            .inner
            .composite_unredirect_window(self.window, self.mode)?
            .check()?;
        connection.inner.flush()?;
        self.active.set(false);
        println!("CompositeUnredirectWindow: AUTOMATIC");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        can_redirect_role, owned_redirect_mode, redirect_target, selection_name,
        selection_clear_matches, should_refuse_takeover, CompositorOwnership,
    };
    use super::RedirectedWindow;
    use crate::x11::capture::WindowRole;
    use x11rb::protocol::composite::Redirect;
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
            active: std::cell::Cell::new(true),
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

    #[test]
    fn only_client_windows_can_be_redirected() {
        assert!(can_redirect_role(WindowRole::Client));
        assert!(!can_redirect_role(WindowRole::Root));
        assert!(!can_redirect_role(WindowRole::OverrideRedirect));
        assert!(!can_redirect_role(WindowRole::TopLevelOrWmFrame));
        assert!(!can_redirect_role(WindowRole::Unknown));
    }

    #[test]
    fn owned_redirection_uses_automatic_mode() {
        assert_eq!(owned_redirect_mode(), Redirect::AUTOMATIC);
    }

    #[test]
    fn redirection_abstraction_has_no_alternate_target() {
        let _ = std::mem::size_of::<RedirectedWindow>();
        assert_eq!(redirect_target(42, WindowRole::Client), Some(42));
        assert_eq!(redirect_target(42, WindowRole::Root), None);
        assert_eq!(redirect_target(42, WindowRole::TopLevelOrWmFrame), None);
        assert_eq!(redirect_target(42, WindowRole::OverrideRedirect), None);
        assert_eq!(redirect_target(42, WindowRole::Unknown), None);
    }
}
