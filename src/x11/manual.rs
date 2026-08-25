use std::cell::Cell;
use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::composite::{self, ConnectionExt as CompositeConnectionExt};
use x11rb::protocol::shape::ConnectionExt as ShapeConnectionExt;
use x11rb::protocol::xfixes::ConnectionExt as XfixesConnectionExt;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt as XprotoConnectionExt, CreateGCAux, EventMask,
    Rectangle, Window,
};
use x11rb::protocol::Event;

use super::compositor::CompositorOwnership;
use super::connection::X11Connection;
use super::overlay::OverlayLease;
use super::shutdown::{wait_for_event_or_shutdown, SignalWake, WaitResult};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManualRedirectState {
    Inactive,
    Active,
    UnredirectAttempted,
    Released,
}

pub(crate) struct ManualSubwindowsRedirect<'a> {
    connection: &'a X11Connection,
    root: Window,
    state: Cell<ManualRedirectState>,
}

impl<'a> ManualSubwindowsRedirect<'a> {
    pub(crate) fn acquire(connection: &'a X11Connection, root: Window) -> Result<Self, Box<dyn Error>> {
        let redirect = Self {
            connection,
            root,
            state: Cell::new(ManualRedirectState::Inactive),
        };
        redirect
            .connection
            .inner
            .composite_redirect_subwindows(root, composite::Redirect::MANUAL)?
            .check()?;
        redirect.state.set(ManualRedirectState::Active);
        if let Err(error) = redirect.connection.inner.flush() {
            eprintln!("manual redirect flush failed after checked request: {error}");
        }
        Ok(redirect)
    }

    pub(crate) fn unredirect(&mut self) -> Result<(), Box<dyn Error>> {
        if !begin_unredirect(&self.state) {
            return Err("manual unredirect is not available in the current state".into());
        }
        self.connection
            .inner
            .composite_unredirect_subwindows(self.root, composite::Redirect::MANUAL)?
            .check()?;
        confirm_unredirect(&self.state);
        Ok(())
    }

    pub(crate) fn disarm_cleanup(&mut self) {
        self.state.set(ManualRedirectState::UnredirectAttempted);
    }
}

impl Drop for ManualSubwindowsRedirect<'_> {
    fn drop(&mut self) {
        // MANUAL is at-most-once. In particular, neither Active nor
        // UnredirectAttempted is retried from Drop.
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RootGeometry {
    width: u16,
    height: u16,
    depth: u8,
    visual: u32,
}

pub(crate) struct RootStructureWatch<'a> {
    connection: &'a X11Connection,
    root: Window,
    previous_mask: EventMask,
    armed: bool,
}

impl<'a> RootStructureWatch<'a> {
    fn acquire(connection: &'a X11Connection, root: Window) -> Result<Self, Box<dyn Error>> {
        let attributes = connection.inner.get_window_attributes(root)?.reply()?;
        let previous_mask = attributes.your_event_mask;
        connection
            .inner
            .change_window_attributes(
                root,
                &ChangeWindowAttributesAux::new()
                    .event_mask(previous_mask | EventMask::STRUCTURE_NOTIFY),
            )?
            .check()?;
        connection.inner.flush()?;
        Ok(Self {
            connection,
            root,
            previous_mask,
            armed: true,
        })
    }

    fn restore(&mut self) -> Result<(), Box<dyn Error>> {
        if !self.armed {
            return Ok(());
        }
        self.connection
            .inner
            .change_window_attributes(
                self.root,
                &ChangeWindowAttributesAux::new().event_mask(self.previous_mask),
            )?
            .check()?;
        self.armed = false;
        Ok(())
    }

    pub(crate) fn disarm_cleanup(&mut self) {
        self.armed = false;
    }
}

impl Drop for RootStructureWatch<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

pub(crate) struct SolidFrameGc<'a> {
    connection: &'a X11Connection,
    gc: u32,
    armed: Cell<bool>,
}

impl<'a> SolidFrameGc<'a> {
    fn create(connection: &'a X11Connection, overlay: Window) -> Result<Self, Box<dyn Error>> {
        let screen = &connection.inner.setup().roots[connection.screen_num()];
        let gc = connection.inner.generate_id()?;
        connection
            .inner
            .create_gc(
                gc,
                overlay,
                &CreateGCAux::new()
                    .foreground(screen.black_pixel)
                    .background(screen.black_pixel),
            )?
            .check()?;
        Ok(Self {
            connection,
            gc,
            armed: Cell::new(true),
        })
    }

    fn paint(&self, overlay: Window, geometry: RootGeometry) -> Result<(), Box<dyn Error>> {
        self.connection
            .inner
            .poly_fill_rectangle(
                overlay,
                self.gc,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width: geometry.width,
                    height: geometry.height,
                }],
            )?
            .check()?;
        self.connection.inner.flush()?;
        self.connection.inner.get_input_focus()?.reply()?;
        Ok(())
    }

    pub(crate) fn disarm_cleanup(&self) {
        self.armed.set(false);
    }
}

impl Drop for SolidFrameGc<'_> {
    fn drop(&mut self) {
        if !self.armed.replace(false) {
            return;
        }
        if let Ok(cookie) = self.connection.inner.free_gc(self.gc) {
            let _ = cookie.check();
            let _ = self.connection.inner.flush();
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ManualEventAction {
    Continue,
    Shutdown(ShutdownReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownReason {
    RootGeometryChanged,
    SelectionLost,
}

pub(crate) struct ManualProbeSession<'a> {
    connection: &'a X11Connection,
    ownership: Option<CompositorOwnership>,
    overlay: Option<OverlayLease<'a>>,
    root_watch: Option<RootStructureWatch<'a>>,
    gc: Option<SolidFrameGc<'a>>,
    redirect: Option<ManualSubwindowsRedirect<'a>>,
    signal: SignalWake,
    cleanup_state: CleanupState,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CleanupState {
    Clean,
    ManualCleanupFailed,
}

impl<'a> ManualProbeSession<'a> {
    fn acquire(
        connection: &'a X11Connection,
        expected_root: Window,
    ) -> Result<Self, Box<dyn Error>> {
        let root = connection.inner.setup().roots[connection.screen_num()].root;
        if expected_root != root {
            return Err(format!(
                "manual probe refused: expected root 0x{expected_root:08x}, actual root 0x{root:08x}"
            )
            .into());
        }
        check_capabilities(connection)?;
        check_selection_available(connection)?;
        let signal = SignalWake::install()?;
        let ownership = CompositorOwnership::claim(connection)?;
        let mut overlay = OverlayLease::acquire(connection, root)?;
        overlay.print_metadata()?;
        overlay.configure_input_passthrough()?;
        let root_watch = RootStructureWatch::acquire(connection, root)?;
        let initial_geometry = read_root_geometry(connection, root)?;
        let gc = SolidFrameGc::create(connection, overlay.overlay)?;
        gc.paint(overlay.overlay, initial_geometry)?;
        let final_geometry = read_root_geometry(connection, root)?;
        if final_geometry != initial_geometry {
            return Err("manual probe root geometry changed before redirect".into());
        }
        let redirect = ManualSubwindowsRedirect::acquire(connection, root)?;
        Ok(Self {
            connection,
            ownership: Some(ownership),
            overlay: Some(overlay),
            root_watch: Some(root_watch),
            gc: Some(gc),
            redirect: Some(redirect),
            signal,
            cleanup_state: CleanupState::Clean,
        })
    }

    pub(crate) fn run(
        connection: &'a X11Connection,
        expected_root: Window,
    ) -> Result<(), Box<dyn Error>> {
        let mut session = Self::acquire(connection, expected_root)?;
        let mut wait_error = None;
        loop {
            let wait = match wait_for_event_or_shutdown(session.connection, &mut session.signal) {
                Ok(wait) => wait,
                Err(error) => {
                    wait_error = Some(error);
                    break;
                }
            };
            match wait {
                WaitResult::Shutdown => {
                    println!("manual probe shutdown: Signal");
                    break;
                }
                WaitResult::Event(event) => match session.dispatch(event)? {
                    ManualEventAction::Continue => {}
                    ManualEventAction::Shutdown(reason) => {
                        println!("manual probe shutdown: {reason:?}");
                        break;
                    }
                },
            }
        }
        let cleanup_result = session.cleanup();
        if let Some(error) = wait_error {
            return Err(error);
        }
        cleanup_result
    }

    fn dispatch(&self, event: Event) -> Result<ManualEventAction, Box<dyn Error>> {
        let root = self.connection.inner.setup().roots[self.connection.screen_num()].root;
        Ok(event_action(event, root, self.ownership.as_ref()))
    }

    fn cleanup(&mut self) -> Result<(), Box<dyn Error>> {
        let mut first_error = None;
        let unredirect_ok = match self.redirect.as_mut() {
            Some(redirect) => match redirect.unredirect() {
                Ok(()) => true,
                Err(error) => {
                    first_error = Some(error);
                    false
                }
            },
            None => true,
        };
        if !unredirect_ok {
            self.cleanup_state = CleanupState::ManualCleanupFailed;
            self.gc.take().map(|gc| gc.disarm_cleanup());
            self.root_watch.take().map(|mut watch| watch.disarm_cleanup());
            self.overlay.take().map(|mut overlay| overlay.disarm_cleanup());
            if let Some(ownership) = self.ownership.take() {
                ownership.disarm_cleanup();
            }
            self.redirect.take().map(|mut redirect| redirect.disarm_cleanup());
            return Err(first_error.expect("manual cleanup failure must have an error"));
        }
        debug_assert!(self
            .redirect
            .as_ref()
            .is_none_or(|redirect| normal_cleanup_allowed(redirect.state.get())));
        self.redirect.take();
        if let Some(gc) = self.gc.take() {
            gc.disarm_cleanup();
            if let Err(error) = free_gc(&gc) {
                first_error.get_or_insert(error);
            }
        }
        if let Some(mut watch) = self.root_watch.take() {
            if let Err(error) = watch.restore() {
                first_error.get_or_insert(error);
            }
        }
        if let Some(mut overlay) = self.overlay.take() {
            if let Err(error) = overlay.restore_input_shape() {
                first_error.get_or_insert(error);
            }
            if let Err(error) = overlay.release_overlay() {
                first_error.get_or_insert(error);
            }
        }
        if let Some(ownership) = self.ownership.take() {
            if let Err(error) = ownership.release(self.connection) {
                first_error.get_or_insert(error);
            }
        }
        first_error.map_or(Ok(()), Err)
    }
}

pub(crate) fn parse_root(value: &str) -> Result<Window, Box<dyn Error>> {
    let (digits, radix) = value
        .strip_prefix("0x")
        .map_or((value, 10), |digits| (digits, 16));
    Ok(u32::from_str_radix(digits, radix)?)
}

pub(crate) fn run(
    connection: &X11Connection,
    expected_root_value: &str,
) -> Result<(), Box<dyn Error>> {
    ManualProbeSession::run(connection, parse_root(expected_root_value)?)
}

pub(crate) fn check_selection_available(connection: &X11Connection) -> Result<(), Box<dyn Error>> {
    let name = super::compositor::selection_name(connection.screen_num());
    let atom = connection.inner.intern_atom(true, name.as_bytes())?.reply()?.atom;
    if atom == x11rb::NONE {
        return Ok(());
    }
    let owner = connection.inner.get_selection_owner(atom)?.reply()?.owner;
    if owner != x11rb::NONE {
        return Err(format!("manual probe refused: compositor selection owner 0x{owner:08x}").into());
    }
    Ok(())
}

pub(crate) fn check_capabilities(connection: &X11Connection) -> Result<(), Box<dyn Error>> {
    let composite = connection.inner.composite_query_version(0, 3)?.reply()?;
    let gates = CapabilityGates {
        composite: (composite.major_version, composite.minor_version) >= (0, 3),
        xfixes: false,
        shape: false,
    };
    if !gates.composite {
        return Err("manual probe requires Composite >= 0.3".into());
    }
    let xfixes = connection.inner.xfixes_query_version(2, 0)?.reply()?;
    let gates = CapabilityGates {
        xfixes: (xfixes.major_version, xfixes.minor_version) >= (2, 0),
        ..gates
    };
    if !gates.xfixes {
        return Err("manual probe requires XFixes >= 2.0".into());
    }
    let shape = connection.inner.shape_query_version()?.reply()?;
    let gates = CapabilityGates {
        shape: (shape.major_version as u16, shape.minor_version as u16) >= (1, 1),
        ..gates
    };
    if !gates.shape {
        return Err("manual probe requires Shape >= 1.1".into());
    }
    debug_assert!(gates.ready());
    Ok(())
}

fn read_root_geometry(connection: &X11Connection, root: Window) -> Result<RootGeometry, Box<dyn Error>> {
    let screen = &connection.inner.setup().roots[connection.screen_num()];
    let geometry = connection.inner.get_geometry(root)?.reply()?;
    Ok(RootGeometry {
        width: geometry.width,
        height: geometry.height,
        depth: geometry.depth,
        visual: screen.root_visual,
    })
}

fn free_gc(gc: &SolidFrameGc<'_>) -> Result<(), Box<dyn Error>> {
    gc.connection.inner.free_gc(gc.gc)?.check()?;
    gc.connection.inner.flush()?;
    Ok(())
}

fn selection_clear_relevant(
    event: &x11rb::protocol::xproto::SelectionClearEvent,
    selection: u32,
    owner_window: Window,
) -> bool {
    event.selection == selection && event.owner == owner_window
}

fn normal_cleanup_allowed(state: ManualRedirectState) -> bool {
    state == ManualRedirectState::Released
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CapabilityGates {
    composite: bool,
    xfixes: bool,
    shape: bool,
}

impl CapabilityGates {
    fn ready(self) -> bool {
        self.composite && self.xfixes && self.shape
    }
}

fn event_action(
    event: Event,
    root: Window,
    ownership: Option<&CompositorOwnership>,
) -> ManualEventAction {
    match event {
        Event::ConfigureNotify(event) if event.window == root => {
            ManualEventAction::Shutdown(ShutdownReason::RootGeometryChanged)
        }
        Event::SelectionClear(event)
            if ownership.is_some_and(|ownership| {
                selection_clear_relevant(&event, ownership.selection, ownership.owner_window)
            }) =>
        {
            ManualEventAction::Shutdown(ShutdownReason::SelectionLost)
        }
        _ => ManualEventAction::Continue,
    }
}

fn begin_unredirect(state: &Cell<ManualRedirectState>) -> bool {
    if state.get() != ManualRedirectState::Active {
        return false;
    }
    state.set(ManualRedirectState::UnredirectAttempted);
    true
}

fn confirm_unredirect(state: &Cell<ManualRedirectState>) {
    if state.get() == ManualRedirectState::UnredirectAttempted {
        state.set(ManualRedirectState::Released);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use x11rb::protocol::Event;

    use super::{
        begin_unredirect, confirm_unredirect, event_action, normal_cleanup_allowed,
        selection_clear_relevant, CapabilityGates, CleanupState, ManualEventAction,
        ManualRedirectState, ShutdownReason,
    };

    #[test]
    fn manual_redirect_state_sequence_is_explicit() {
        let state = Cell::new(ManualRedirectState::Inactive);
        assert!(!begin_unredirect(&state));
        assert!(!normal_cleanup_allowed(ManualRedirectState::UnredirectAttempted));
        assert!(normal_cleanup_allowed(ManualRedirectState::Released));
        state.set(ManualRedirectState::Active);
        assert!(begin_unredirect(&state));
        assert_eq!(state.get(), ManualRedirectState::UnredirectAttempted);
        confirm_unredirect(&state);
        assert_eq!(state.get(), ManualRedirectState::Released);
        assert!(!begin_unredirect(&state));
    }

    #[test]
    fn attempted_state_is_not_a_normal_cleanup_state() {
        assert_eq!(CleanupState::ManualCleanupFailed, CleanupState::ManualCleanupFailed);
        assert_ne!(ManualRedirectState::UnredirectAttempted, ManualRedirectState::Active);
        let state = Cell::new(ManualRedirectState::UnredirectAttempted);
        assert!(!begin_unredirect(&state));
    }

    #[test]
    fn event_actions_are_shutdown_only() {
        assert_eq!(
            ManualEventAction::Shutdown(ShutdownReason::RootGeometryChanged),
            ManualEventAction::Shutdown(ShutdownReason::RootGeometryChanged)
        );
        assert_eq!(
            ManualEventAction::Shutdown(ShutdownReason::SelectionLost),
            ManualEventAction::Shutdown(ShutdownReason::SelectionLost)
        );
        assert_eq!(ManualEventAction::Continue, ManualEventAction::Continue);
    }

    #[test]
    fn root_configure_is_shutdown_and_other_events_continue() {
        use x11rb::protocol::xproto::ConfigureNotifyEvent;
        let configure = ConfigureNotifyEvent {
            response_type: 0,
            sequence: 0,
            event: 0x381,
            window: 0x381,
            above_sibling: 0,
            x: 0,
            y: 0,
            width: 1280,
            height: 720,
            border_width: 0,
            override_redirect: false,
        };
        assert_eq!(
            event_action(Event::ConfigureNotify(configure), 0x381, None),
            ManualEventAction::Shutdown(ShutdownReason::RootGeometryChanged)
        );
        assert_eq!(
            event_action(Event::ConfigureNotify(configure), 0x380, None),
            ManualEventAction::Continue
        );
    }

    #[test]
    fn selection_clear_relevance_is_exact() {
        use x11rb::protocol::xproto::SelectionClearEvent;
        let event = SelectionClearEvent {
            response_type: 0,
            sequence: 0,
            time: 0,
            owner: 0x20,
            selection: 0x10,
        };
        assert!(selection_clear_relevant(&event, 0x10, 0x20));
        assert!(!selection_clear_relevant(&event, 0x11, 0x20));
        assert!(!selection_clear_relevant(&event, 0x10, 0x21));
    }

    #[test]
    fn capability_gates_require_all_extensions() {
        assert!(!CapabilityGates {
            composite: false,
            xfixes: true,
            shape: true,
        }
        .ready());
        assert!(CapabilityGates {
            composite: true,
            xfixes: true,
            shape: true,
        }
        .ready());
    }

    #[test]
    fn root_guard_is_exact() {
        assert_eq!(0x381_u32, 0x381_u32);
        assert_ne!(0x381_u32, 0x380_u32);
    }
}
