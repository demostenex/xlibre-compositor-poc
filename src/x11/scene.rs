use std::cell::Cell;
use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::composite::ConnectionExt as CompositeConnectionExt;
use x11rb::protocol::xproto::{
    self, ChangeWindowAttributesAux, ConnectionExt as XprotoConnectionExt, CreateGCAux,
    EventMask, Rectangle, Window, WindowClass,
};
use x11rb::protocol::Event;

use super::capture::{WindowGeometry, WindowMetadata};
use super::compositor::{selection_clear_matches, CompositorOwnership};
use super::connection::X11Connection;
use super::manual::{
    check_capabilities, check_selection_available, parse_root, ManualSubwindowsRedirect,
};
use super::overlay::OverlayLease;
use super::shutdown::{wait_for_event_or_shutdown, SignalWake, WaitResult};
use super::tree::{BindingStatus, HierarchySnapshot};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RootGeometry {
    width: u16,
    height: u16,
    depth: u8,
    visual: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SurfaceEntry {
    surface_xid: Window,
    semantic_client_xid: Option<Window>,
    lifecycle_xid: Window,
    geometry: WindowGeometry,
    depth: u8,
    visual: u32,
    class: WindowClass,
    map_state: xproto::MapState,
    override_redirect: bool,
    stacking_index: usize,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SceneSnapshot {
    root: Window,
    root_geometry: RootGeometry,
    entries: Vec<SurfaceEntry>,
}

impl SceneSnapshot {
    fn from_hierarchy(
        connection: &X11Connection,
        hierarchy: HierarchySnapshot,
        root_geometry: RootGeometry,
        overlay: Window,
        owner_window: Window,
    ) -> Result<Self, Box<dyn Error>> {
        let mut entries = Vec::new();
        for (stacking_index, binding) in hierarchy.children.iter().enumerate() {
            let surface_xid = binding.root_child_xid;
            if is_internal_xid(surface_xid, overlay, owner_window) {
                println!(
                    "scene surface skip: internal XID 0x{surface_xid:08x}"
                );
                continue;
            }
            let metadata = match binding.surface_candidate.as_ref() {
                Some(metadata) => metadata,
                None => {
                    return Err(format!(
                        "scene snapshot stale root child 0x{surface_xid:08x}"
                    )
                    .into())
                }
            };
            if metadata.window != surface_xid {
                return Err(format!(
                    "scene snapshot surface mismatch: root child 0x{surface_xid:08x}, metadata 0x{:08x}",
                    metadata.window
                )
                .into());
            }
            let semantic_client_xid = match &binding.semantic_client {
                BindingStatus::SingleClient(client) => Some(*client),
                BindingStatus::NoClient | BindingStatus::Ambiguous(_) => None,
            };
            if let Some(entry) = eligible_surface(
                metadata,
                semantic_client_xid,
                root_geometry,
                surface_xid,
                stacking_index,
            ) {
                println_surface(&entry);
                entries.push(entry);
            }
        }
        if entries.is_empty() {
            return Err("scene snapshot contains no eligible root-child surfaces".into());
        }
        println!(
            "SceneSnapshot: root=0x{:08x} children={} eligible={}",
            hierarchy.root,
            hierarchy.children.len(),
            entries.len()
        );
        let _ = connection;
        Ok(Self {
            root: hierarchy.root,
            root_geometry,
            entries,
        })
    }
}

fn eligible_surface(
    metadata: &WindowMetadata,
    semantic_client_xid: Option<Window>,
    root_geometry: RootGeometry,
    surface_xid: Window,
    stacking_index: usize,
) -> Option<SurfaceEntry> {
    if metadata.class != WindowClass::INPUT_OUTPUT {
        println!("scene surface skip 0x{surface_xid:08x}: InputOnly/non-InputOutput");
        return None;
    }
    if metadata.map_state != xproto::MapState::VIEWABLE {
        println!(
            "scene surface skip 0x{surface_xid:08x}: map_state {:?}",
            metadata.map_state
        );
        return None;
    }
    if metadata.geometry.width == 0 || metadata.geometry.height == 0 {
        println!("scene surface skip 0x{surface_xid:08x}: zero-sized");
        return None;
    }
    if metadata.depth != root_geometry.depth {
        println!(
            "scene surface skip 0x{surface_xid:08x}: depth {} != root depth {}",
            metadata.depth, root_geometry.depth
        );
        return None;
    }
    if metadata.visual != root_geometry.visual {
        println!(
            "scene surface skip 0x{surface_xid:08x}: visual 0x{:08x} != root visual 0x{:08x}",
            metadata.visual, root_geometry.visual
        );
        return None;
    }
    Some(SurfaceEntry {
        surface_xid,
        semantic_client_xid,
        lifecycle_xid: surface_xid,
        geometry: metadata.geometry,
        depth: metadata.depth,
        visual: metadata.visual,
        class: metadata.class,
        map_state: metadata.map_state,
        override_redirect: metadata.override_redirect,
        stacking_index,
    })
}

fn println_surface(entry: &SurfaceEntry) {
    println!(
        "scene surface: xid=0x{:08x} semantic_client={} stack={} geometry={}x{}+{}+{} border={} depth={} visual=0x{:08x} override_redirect={}",
        entry.surface_xid,
        entry
            .semantic_client_xid
            .map_or_else(|| "NONE".to_owned(), |xid| format!("0x{xid:08x}")),
        entry.stacking_index,
        entry.geometry.width,
        entry.geometry.height,
        entry.geometry.x,
        entry.geometry.y,
        entry.geometry.border_width,
        entry.depth,
        entry.visual,
        entry.override_redirect,
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PixmapGeometry {
    root: Window,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    depth: u8,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PixmapState {
    Inactive,
    Active,
    FreeAttempted,
    Released,
}

struct NamedSurfacePixmap<'a> {
    connection: &'a X11Connection,
    surface_xid: Window,
    pixmap_xid: u32,
    window_geometry: WindowGeometry,
    geometry: PixmapGeometry,
    state: Cell<PixmapState>,
}

impl<'a> NamedSurfacePixmap<'a> {
    fn acquire(
        connection: &'a X11Connection,
        entry: &SurfaceEntry,
        root_window: Window,
        root: RootGeometry,
    ) -> Result<Self, Box<dyn Error>> {
        let pixmap_xid = connection.inner.generate_id()?;
        connection
            .inner
            .composite_name_window_pixmap(entry.surface_xid, pixmap_xid)?
            .check()
            .map_err(|error| {
                format!(
                    "NameWindowPixmap failed for selected surface 0x{:08x}: {error}",
                    entry.surface_xid
                )
            })?;
        let geometry = connection.inner.get_geometry(pixmap_xid)?.reply()?;
        let expected_width = u32::from(entry.geometry.width)
            .checked_add(u32::from(entry.geometry.border_width) * 2)
            .ok_or("expected named pixmap width overflow")?;
        let expected_height = u32::from(entry.geometry.height)
            .checked_add(u32::from(entry.geometry.border_width) * 2)
            .ok_or("expected named pixmap height overflow")?;
        if geometry.root != root_window
            || geometry.depth != root.depth
            || geometry.width == 0
            || geometry.height == 0
            || u32::from(geometry.width) != expected_width
            || u32::from(geometry.height) != expected_height
        {
            return Err(format!(
                "named pixmap geometry mismatch surface=0x{:08x} window={}x{}+{}+{} border={} pixmap=0x{:08x} root=0x{:08x} geometry={}x{}+{}+{} border={} depth={} expected={}x{} root=0x{:08x} depth={}",
                entry.surface_xid,
                entry.geometry.width,
                entry.geometry.height,
                entry.geometry.x,
                entry.geometry.y,
                entry.geometry.border_width,
                pixmap_xid,
                geometry.root,
                geometry.width,
                geometry.height,
                geometry.x,
                geometry.y,
                geometry.border_width,
                geometry.depth,
                expected_width,
                expected_height,
                root_window,
                root.depth,
            )
            .into());
        }
        println!(
            "NamedSurfacePixmap: surface=0x{:08x} pixmap=0x{:08x} geometry={}x{}+{}+{} depth={} root=0x{:08x}",
            entry.surface_xid,
            pixmap_xid,
            geometry.width,
            geometry.height,
            geometry.x,
            geometry.y,
            geometry.depth,
            geometry.root
        );
        let state = Cell::new(PixmapState::Inactive);
        state.set(PixmapState::Active);
        Ok(Self {
            connection,
            surface_xid: entry.surface_xid,
            pixmap_xid,
            window_geometry: entry.geometry,
            geometry: PixmapGeometry {
                root: geometry.root,
                x: geometry.x,
                y: geometry.y,
                width: geometry.width,
                height: geometry.height,
                border_width: geometry.border_width,
                depth: geometry.depth,
            },
            state,
        })
    }

    fn free(&self) -> Result<(), Box<dyn Error>> {
        if self.state.get() != PixmapState::Active {
            return Ok(());
        }
        self.state.set(PixmapState::FreeAttempted);
        self.connection.inner.free_pixmap(self.pixmap_xid)?.check()?;
        self.state.set(PixmapState::Released);
        println!(
            "NamedSurfacePixmap released: surface=0x{:08x} pixmap=0x{:08x}",
            self.surface_xid, self.pixmap_xid
        );
        Ok(())
    }

    fn disarm_cleanup(&self) {
        self.state.set(PixmapState::FreeAttempted);
    }

    fn copy_plan(&self, root: RootGeometry) -> Option<CopyPlan> {
        build_copy_plan(self.window_geometry, self.geometry, root)
    }
}

impl Drop for NamedSurfacePixmap<'_> {
    fn drop(&mut self) {
        if self.state.get() != PixmapState::Active {
            return;
        }
        self.state.set(PixmapState::FreeAttempted);
        if let Ok(cookie) = self.connection.inner.free_pixmap(self.pixmap_xid) {
            if cookie.check().is_ok() {
                self.state.set(PixmapState::Released);
            }
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CopyPlan {
    src_x: i16,
    src_y: i16,
    dst_x: i16,
    dst_y: i16,
    width: u16,
    height: u16,
}

fn build_copy_plan(
    window: WindowGeometry,
    pixmap: PixmapGeometry,
    root: RootGeometry,
) -> Option<CopyPlan> {
    if pixmap.root == x11rb::NONE || pixmap.depth != root.depth {
        return None;
    }
    let border = i32::from(window.border_width);
    let mut dst_x = i32::from(window.x) - border;
    let mut dst_y = i32::from(window.y) - border;
    let mut src_x = 0_i32;
    let mut src_y = 0_i32;
    let mut width = i32::from(pixmap.width);
    let mut height = i32::from(pixmap.height);
    if dst_x < 0 {
        let clipped = -dst_x;
        src_x += clipped;
        width -= clipped;
        dst_x = 0;
    }
    if dst_y < 0 {
        let clipped = -dst_y;
        src_y += clipped;
        height -= clipped;
        dst_y = 0;
    }
    width = width.min(i32::from(root.width) - dst_x);
    height = height.min(i32::from(root.height) - dst_y);
    if width <= 0 || height <= 0 || src_x < 0 || src_y < 0 {
        return None;
    }
    if src_x + width > i32::from(pixmap.width)
        || src_y + height > i32::from(pixmap.height)
        || dst_x > i32::from(i16::MAX)
        || dst_y > i32::from(i16::MAX)
        || src_x > i32::from(i16::MAX)
        || src_y > i32::from(i16::MAX)
    {
        return None;
    }
    Some(CopyPlan {
        src_x: src_x as i16,
        src_y: src_y as i16,
        dst_x: dst_x as i16,
        dst_y: dst_y as i16,
        width: width as u16,
        height: height as u16,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResourceState {
    Inactive,
    Active,
    FreeAttempted,
    Released,
}

struct SceneScratchPixmap<'a> {
    connection: &'a X11Connection,
    root: Window,
    pixmap: u32,
    width: u16,
    height: u16,
    depth: u8,
    state: Cell<ResourceState>,
}

impl<'a> SceneScratchPixmap<'a> {
    fn create(
        connection: &'a X11Connection,
        root: Window,
        geometry: RootGeometry,
    ) -> Result<Self, Box<dyn Error>> {
        let pixmap = connection.inner.generate_id()?;
        connection
            .inner
            .create_pixmap(geometry.depth, pixmap, root, geometry.width, geometry.height)?
            .check()?;
        let state = Cell::new(ResourceState::Inactive);
        state.set(ResourceState::Active);
        Ok(Self {
            connection,
            root,
            pixmap,
            width: geometry.width,
            height: geometry.height,
            depth: geometry.depth,
            state,
        })
    }

    fn clear(&self, gc: u32) -> Result<(), Box<dyn Error>> {
        self.connection
            .inner
            .poly_fill_rectangle(
                self.pixmap,
                gc,
                &[Rectangle {
                    x: 0,
                    y: 0,
                    width: self.width,
                    height: self.height,
                }],
            )?
            .check()?;
        Ok(())
    }

    fn free(&self) -> Result<(), Box<dyn Error>> {
        if self.state.get() != ResourceState::Active {
            return Ok(());
        }
        self.state.set(ResourceState::FreeAttempted);
        self.connection.inner.free_pixmap(self.pixmap)?.check()?;
        self.state.set(ResourceState::Released);
        Ok(())
    }

    fn disarm_cleanup(&self) {
        self.state.set(ResourceState::FreeAttempted);
    }
}

impl Drop for SceneScratchPixmap<'_> {
    fn drop(&mut self) {
        if self.state.get() != ResourceState::Active {
            return;
        }
        self.state.set(ResourceState::FreeAttempted);
        if let Ok(cookie) = self.connection.inner.free_pixmap(self.pixmap) {
            if cookie.check().is_ok() {
                self.state.set(ResourceState::Released);
            }
        }
    }
}

struct SceneGc<'a> {
    connection: &'a X11Connection,
    gc: u32,
    state: Cell<ResourceState>,
}

impl<'a> SceneGc<'a> {
    fn create(connection: &'a X11Connection, root: Window, black: u32) -> Result<Self, Box<dyn Error>> {
        let gc = connection.inner.generate_id()?;
        connection
            .inner
            .create_gc(
                gc,
                root,
                &CreateGCAux::new()
                    .foreground(black)
                    .background(black)
                    .function(xproto::GX::COPY)
                    .graphics_exposures(0_u32),
            )?
            .check()?;
        let state = Cell::new(ResourceState::Inactive);
        state.set(ResourceState::Active);
        Ok(Self {
            connection,
            gc,
            state,
        })
    }

    fn clear_overlay(&self, overlay: Window, geometry: RootGeometry) -> Result<(), Box<dyn Error>> {
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

    fn copy(
        &self,
        source: u32,
        destination: Window,
        plan: CopyPlan,
    ) -> Result<(), Box<dyn Error>> {
        self.connection
            .inner
            .copy_area(
                source,
                destination,
                self.gc,
                plan.src_x,
                plan.src_y,
                plan.dst_x,
                plan.dst_y,
                plan.width,
                plan.height,
            )?
            .check()?;
        Ok(())
    }

    fn free(&self) -> Result<(), Box<dyn Error>> {
        if self.state.get() != ResourceState::Active {
            return Ok(());
        }
        self.state.set(ResourceState::FreeAttempted);
        self.connection.inner.free_gc(self.gc)?.check()?;
        self.state.set(ResourceState::Released);
        Ok(())
    }

    fn disarm_cleanup(&self) {
        self.state.set(ResourceState::FreeAttempted);
    }
}

impl Drop for SceneGc<'_> {
    fn drop(&mut self) {
        if self.state.get() != ResourceState::Active {
            return;
        }
        self.state.set(ResourceState::FreeAttempted);
        if let Ok(cookie) = self.connection.inner.free_gc(self.gc) {
            if cookie.check().is_ok() {
                self.state.set(ResourceState::Released);
            }
        }
    }
}

struct SceneRootWatch<'a> {
    connection: &'a X11Connection,
    root: Window,
    previous_mask: EventMask,
    armed: bool,
}

impl<'a> SceneRootWatch<'a> {
    fn acquire(connection: &'a X11Connection, root: Window) -> Result<Self, Box<dyn Error>> {
        let attributes = connection.inner.get_window_attributes(root)?.reply()?;
        let previous_mask = attributes.your_event_mask;
        connection
            .inner
            .change_window_attributes(
                root,
                &ChangeWindowAttributesAux::new().event_mask(
                    previous_mask | EventMask::STRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_NOTIFY,
                ),
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

    fn disarm_cleanup(&mut self) {
        self.armed = false;
    }
}

impl Drop for SceneRootWatch<'_> {
    fn drop(&mut self) {
        let _ = self.restore();
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SceneState {
    PlaceholderReady,
    ManualActive,
    SceneSnapshotReady,
    NamedPixmapsReady,
    ScratchSceneReady,
    ScenePresented,
    RunningStatic,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SceneEventAction {
    Continue,
    Shutdown(ShutdownReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownReason {
    RootConfigure,
    Structural,
    SelectionLost,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PresentationDecision {
    Present,
    Shutdown,
}

fn presentation_decision(shutdown_pending: bool) -> PresentationDecision {
    if shutdown_pending {
        PresentationDecision::Shutdown
    } else {
        PresentationDecision::Present
    }
}

fn coordinator_requires_cleanup(state: SceneState) -> bool {
    state != SceneState::PlaceholderReady
}

struct SceneSession<'a> {
    connection: &'a X11Connection,
    root: Window,
    ownership: Option<CompositorOwnership>,
    overlay: Option<OverlayLease<'a>>,
    root_watch: Option<SceneRootWatch<'a>>,
    manual: Option<ManualSubwindowsRedirect<'a>>,
    gc: Option<SceneGc<'a>>,
    scratch: Option<SceneScratchPixmap<'a>>,
    pixmaps: Vec<NamedSurfacePixmap<'a>>,
    signal: SignalWake,
    state: SceneState,
}

impl<'a> SceneSession<'a> {
    fn acquire(connection: &'a X11Connection, expected_root: Window) -> Result<Self, Box<dyn Error>> {
        let root = connection.inner.setup().roots[connection.screen_num()].root;
        root_guard(expected_root, root)?;
        check_capabilities(connection)?;
        check_selection_available(connection)?;
        let signal = SignalWake::install()?;
        let ownership = CompositorOwnership::claim(connection)?;
        let mut overlay = OverlayLease::acquire(connection, root)?;
        overlay.print_metadata()?;
        overlay.configure_input_passthrough()?;
        let root_watch = SceneRootWatch::acquire(connection, root)?;
        let root_geometry = read_root_geometry(connection, root)?;
        let screen = &connection.inner.setup().roots[connection.screen_num()];
        if root_geometry.depth != screen.root_depth || root_geometry.visual != screen.root_visual {
            return Err("scene root geometry does not match screen metadata".into());
        }
        let gc = SceneGc::create(connection, root, screen.black_pixel)?;
        gc.clear_overlay(overlay.overlay, root_geometry)?;
        println!("state: PlaceholderReady");
        let manual = ManualSubwindowsRedirect::acquire(connection, root)?;
        println!("state: ManualActive");
        let mut session = Self {
            connection,
            root,
            ownership: Some(ownership),
            overlay: Some(overlay),
            root_watch: Some(root_watch),
            manual: Some(manual),
            gc: Some(gc),
            scratch: None,
            pixmaps: Vec::new(),
            signal,
            state: SceneState::PlaceholderReady,
        };
        session.state = SceneState::ManualActive;
        Ok(session)
    }

    fn run(connection: &'a X11Connection, expected_root: Window) -> Result<(), Box<dyn Error>> {
        let mut session = Self::acquire(connection, expected_root)?;
        let operation = session.prepare_scene().and_then(|_| session.wait_static());
        debug_assert!(coordinator_requires_cleanup(session.state));
        let cleanup = session.cleanup();
        match (operation, cleanup) {
            (Err(operation), Err(cleanup)) => {
                eprintln!("scene cleanup also failed: {cleanup}");
                Err(operation)
            }
            (Err(operation), Ok(())) => Err(operation),
            (Ok(()), Err(cleanup)) => Err(cleanup),
            (Ok(()), Ok(())) => Ok(()),
        }
    }

    fn prepare_scene(&mut self) -> Result<(), Box<dyn Error>> {
        let root_geometry = read_root_geometry(self.connection, self.root)?;
        let hierarchy = self.connection.snapshot_hierarchy()?;
        let overlay = self.overlay.as_ref().ok_or("overlay is unavailable")?.overlay;
        let owner = self
            .ownership
            .as_ref()
            .ok_or("ownership is unavailable")?
            .owner_window;
        let snapshot = SceneSnapshot::from_hierarchy(
            self.connection,
            hierarchy,
            root_geometry,
            overlay,
            owner,
        )?;
        self.state = SceneState::SceneSnapshotReady;
        println!("state: SceneSnapshotReady");
        let scratch = SceneScratchPixmap::create(self.connection, self.root, root_geometry)?;
        if scratch.root != self.root || scratch.depth != root_geometry.depth {
            return Err("scratch pixmap root/depth is incompatible with scene".into());
        }
        let gc = self.gc.as_ref().ok_or("scene GC is unavailable")?;
        scratch.clear(gc.gc)?;
        self.scratch = Some(scratch);
        for entry in &snapshot.entries {
            let pixmap = NamedSurfacePixmap::acquire(self.connection, entry, self.root, root_geometry)?;
            if pixmap.geometry.root != self.root || pixmap.geometry.depth != root_geometry.depth {
                return Err(format!(
                    "CopyArea compatibility failed for surface 0x{:08x}",
                    entry.surface_xid
                )
                .into());
            }
            let plan = pixmap.copy_plan(root_geometry).ok_or_else(|| {
                format!(
                    "surface 0x{:08x} has no visible copy intersection or valid coordinates",
                    entry.surface_xid
                )
            })?;
            println!(
                "copy plan surface=0x{:08x} src=({}, {}) dst=({}, {}) size={}x{}",
                entry.surface_xid,
                plan.src_x,
                plan.src_y,
                plan.dst_x,
                plan.dst_y,
                plan.width,
                plan.height
            );
            let scratch_drawable = self.scratch.as_ref().ok_or("scratch is unavailable")?.pixmap;
            gc.copy(pixmap.pixmap_xid, scratch_drawable, plan)?;
            self.pixmaps.push(pixmap);
        }
        if self.pixmaps.is_empty() {
            return Err("no named surface pixmaps were acquired".into());
        }
        self.state = SceneState::NamedPixmapsReady;
        println!("state: NamedPixmapsReady surfaces={}", self.pixmaps.len());
        let scratch_drawable = self.scratch.as_ref().ok_or("scratch is unavailable")?.pixmap;
        let final_plan = CopyPlan {
            src_x: 0,
            src_y: 0,
            dst_x: 0,
            dst_y: 0,
            width: root_geometry.width,
            height: root_geometry.height,
        };
        if root_geometry.depth != self.overlay_depth()? {
            return Err("scratch and overlay depths differ".into());
        }
        gc.copy(scratch_drawable, overlay, final_plan)?;
        self.connection.inner.flush()?;
        self.connection.inner.get_input_focus()?.reply()?;
        self.state = SceneState::ScratchSceneReady;
        println!("state: ScratchSceneReady");
        self.reject_pending_invalidations()?;
        self.verify_ownership()?;
        if presentation_decision(self.signal.poll_shutdown_pending()?)
            == PresentationDecision::Shutdown
        {
            return Err("scene invalidated before presentation: Signal".into());
        }
        self.state = SceneState::ScenePresented;
        println!("state: ScenePresented (MANUAL active, X11 CopyArea instrument)");
        self.state = SceneState::RunningStatic;
        println!("state: RunningStatic");
        println!("static scene: pixel updates are intentionally not tracked");
        Ok(())
    }

    fn overlay_depth(&self) -> Result<u8, Box<dyn Error>> {
        let overlay = self.overlay.as_ref().ok_or("overlay is unavailable")?.overlay;
        Ok(self.connection.inner.get_geometry(overlay)?.reply()?.depth)
    }

    fn reject_pending_invalidations(&self) -> Result<(), Box<dyn Error>> {
        while let Some(event) = self.connection.inner.poll_for_event()? {
            if let SceneEventAction::Shutdown(reason) = self.event_action(event) {
                return Err(format!("scene invalidated before presentation: {reason:?}").into());
            }
        }
        Ok(())
    }

    fn verify_ownership(&self) -> Result<(), Box<dyn Error>> {
        let ownership = self.ownership.as_ref().ok_or("ownership is unavailable")?;
        let name = super::compositor::selection_name(self.connection.screen_num());
        let atom = self
            .connection
            .inner
            .intern_atom(true, name.as_bytes())?
            .reply()?
            .atom;
        let owner = self.connection.inner.get_selection_owner(atom)?.reply()?.owner;
        if owner != ownership.owner_window {
            return Err(format!(
                "compositor ownership changed before ScenePresented: expected 0x{:08x}, got 0x{owner:08x}",
                ownership.owner_window
            )
            .into());
        }
        Ok(())
    }

    fn wait_static(&mut self) -> Result<(), Box<dyn Error>> {
        loop {
            match wait_for_event_or_shutdown(self.connection, &mut self.signal)? {
                WaitResult::Shutdown => {
                    println!("scene shutdown: Signal");
                    return Ok(());
                }
                WaitResult::Event(event) => match self.event_action(event) {
                    SceneEventAction::Continue => {}
                    SceneEventAction::Shutdown(reason) => {
                        println!("scene shutdown: {reason:?}");
                        return Ok(());
                    }
                },
            }
        }
    }

    fn event_action(&self, event: Event) -> SceneEventAction {
        scene_event_action(
            event,
            self.root,
            self.ownership.as_ref(),
        )
    }

    fn cleanup(&mut self) -> Result<(), Box<dyn Error>> {
        let mut first_error = None;
        let manual_ok = match self.manual.as_mut() {
            Some(manual) => match manual.unredirect() {
                Ok(()) => true,
                Err(error) => {
                    first_error = Some(error);
                    false
                }
            },
            None => true,
        };
        if !manual_ok {
            self.disarm_degraded();
            return Err(first_error.expect("manual cleanup failure must have an error"));
        }
        self.manual.take();
        if let Some(scratch) = self.scratch.take() {
            if let Err(error) = scratch.free() {
                first_error.get_or_insert(error);
            }
        }
        for pixmap in &self.pixmaps {
            if let Err(error) = pixmap.free() {
                first_error.get_or_insert(error);
            }
        }
        self.pixmaps.clear();
        if let Some(gc) = self.gc.take() {
            if let Err(error) = gc.free() {
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

    fn disarm_degraded(&mut self) {
        if let Some(manual) = self.manual.take() {
            let mut manual = manual;
            manual.disarm_cleanup();
        }
        if let Some(scratch) = self.scratch.take() {
            scratch.disarm_cleanup();
        }
        for pixmap in &self.pixmaps {
            pixmap.disarm_cleanup();
        }
        self.pixmaps.clear();
        if let Some(gc) = self.gc.take() {
            gc.disarm_cleanup();
        }
        if let Some(mut watch) = self.root_watch.take() {
            watch.disarm_cleanup();
        }
        if let Some(mut overlay) = self.overlay.take() {
            overlay.disarm_cleanup();
        }
        if let Some(ownership) = self.ownership.take() {
            ownership.disarm_cleanup();
        }
    }
}

fn root_guard(expected: Window, actual: Window) -> Result<(), Box<dyn Error>> {
    if expected != actual {
        return Err(format!(
            "scene X11 probe refused: expected root 0x{expected:08x}, actual root 0x{actual:08x}"
        )
        .into());
    }
    Ok(())
}

fn is_internal_xid(xid: Window, overlay: Window, owner_window: Window) -> bool {
    xid == overlay || xid == owner_window
}

fn read_root_geometry(
    connection: &X11Connection,
    root: Window,
) -> Result<RootGeometry, Box<dyn Error>> {
    let screen = &connection.inner.setup().roots[connection.screen_num()];
    let geometry = connection.inner.get_geometry(root)?.reply()?;
    Ok(RootGeometry {
        width: geometry.width,
        height: geometry.height,
        depth: geometry.depth,
        visual: screen.root_visual,
    })
}

fn scene_event_action(
    event: Event,
    root: Window,
    ownership: Option<&CompositorOwnership>,
) -> SceneEventAction {
    match event {
        Event::SelectionClear(event)
            if ownership.is_some_and(|ownership| selection_clear_matches(&event, ownership)) =>
        {
            SceneEventAction::Shutdown(ShutdownReason::SelectionLost)
        }
        Event::ConfigureNotify(event) if event.window == root => {
            SceneEventAction::Shutdown(ShutdownReason::RootConfigure)
        }
        Event::CreateNotify(event) if event.parent == root => {
            SceneEventAction::Shutdown(ShutdownReason::Structural)
        }
        Event::MapNotify(event) if event.event == root => {
            SceneEventAction::Shutdown(ShutdownReason::Structural)
        }
        Event::UnmapNotify(event) if event.event == root => {
            SceneEventAction::Shutdown(ShutdownReason::Structural)
        }
        Event::DestroyNotify(event) if event.event == root => {
            SceneEventAction::Shutdown(ShutdownReason::Structural)
        }
        Event::ReparentNotify(event) if event.parent == root || event.event == root => {
            SceneEventAction::Shutdown(ShutdownReason::Structural)
        }
        Event::ConfigureNotify(event) if event.event == root => {
            SceneEventAction::Shutdown(ShutdownReason::Structural)
        }
        Event::CirculateNotify(event) if event.event == root => {
            SceneEventAction::Shutdown(ShutdownReason::Structural)
        }
        _ => SceneEventAction::Continue,
    }
}

pub(crate) fn run(
    connection: &X11Connection,
    expected_root_value: &str,
) -> Result<(), Box<dyn Error>> {
    SceneSession::run(connection, parse_root(expected_root_value)?)
}

#[cfg(test)]
mod tests {
    use super::{
        build_copy_plan, coordinator_requires_cleanup, eligible_surface, is_internal_xid,
        presentation_decision, root_guard, scene_event_action, CopyPlan, PixmapGeometry,
        PresentationDecision, RootGeometry, SceneEventAction, SceneState,
        ShutdownReason, SurfaceEntry,
    };
    use crate::x11::capture::WindowGeometry;
    use x11rb::protocol::xproto::{MapState, WindowClass};
    use x11rb::protocol::Event;

    fn root() -> RootGeometry {
        RootGeometry {
            width: 100,
            height: 80,
            depth: 24,
            visual: 0x21,
        }
    }

    fn pixmap(width: u16, height: u16) -> PixmapGeometry {
        PixmapGeometry {
            root: 1,
            x: 0,
            y: 0,
            width,
            height,
            border_width: 0,
            depth: 24,
        }
    }

    fn window(x: i16, y: i16, width: u16, height: u16, border_width: u16) -> WindowGeometry {
        WindowGeometry {
            x,
            y,
            width,
            height,
            border_width,
        }
    }

    fn metadata() -> crate::x11::capture::WindowMetadata {
        crate::x11::capture::WindowMetadata {
            window: 10,
            geometry: window(0, 0, 20, 20, 0),
            depth: 24,
            visual: 0x21,
            class: WindowClass::INPUT_OUTPUT,
            override_redirect: false,
            has_wm_state: false,
            map_state: MapState::VIEWABLE,
            wm_class: None,
            window_type: None,
            role: crate::x11::capture::WindowRole::Unknown,
        }
    }

    #[test]
    fn copy_plan_is_fully_visible() {
        assert_eq!(
            build_copy_plan(window(10, 12, 20, 15, 0), pixmap(20, 15), root()),
            Some(CopyPlan {
                src_x: 0,
                src_y: 0,
                dst_x: 10,
                dst_y: 12,
                width: 20,
                height: 15,
            })
        );
    }

    #[test]
    fn copy_plan_clips_each_edge() {
        assert_eq!(build_copy_plan(window(-5, 10, 20, 15, 0), pixmap(20, 15), root()).unwrap().src_x, 5);
        assert_eq!(build_copy_plan(window(10, -5, 20, 15, 0), pixmap(20, 15), root()).unwrap().src_y, 5);
        assert_eq!(build_copy_plan(window(90, 10, 20, 15, 0), pixmap(20, 15), root()).unwrap().width, 10);
        assert_eq!(build_copy_plan(window(10, 70, 20, 15, 0), pixmap(20, 15), root()).unwrap().height, 10);
    }

    #[test]
    fn copy_plan_handles_border_and_offscreen() {
        let plan = build_copy_plan(window(10, 12, 20, 15, 2), pixmap(24, 19), root()).unwrap();
        assert_eq!(plan.dst_x, 8);
        assert_eq!(plan.dst_y, 10);
        assert_eq!(plan.width, 24);
        assert_eq!(plan.height, 19);
        assert_eq!(build_copy_plan(window(-30, 0, 10, 10, 0), pixmap(10, 10), root()), None);
    }

    #[test]
    fn unsupported_depth_has_no_copy_plan() {
        let mut source = pixmap(20, 20);
        source.depth = 32;
        assert_eq!(build_copy_plan(window(0, 0, 20, 20, 0), source, root()), None);
    }

    #[test]
    fn semantic_client_does_not_change_surface_selection() {
        let metadata = crate::x11::capture::WindowMetadata {
            window: 10,
            geometry: window(0, 0, 20, 20, 0),
            depth: 24,
            visual: 0x21,
            class: WindowClass::INPUT_OUTPUT,
            override_redirect: false,
            has_wm_state: true,
            map_state: MapState::VIEWABLE,
            wm_class: None,
            window_type: None,
            role: crate::x11::capture::WindowRole::Client,
        };
        let no_client = eligible_surface(&metadata, None, root(), 10, 0).unwrap();
        let client = eligible_surface(&metadata, Some(20), root(), 10, 0).unwrap();
        assert_eq!(no_client.surface_xid, client.surface_xid);
        assert_eq!(client.semantic_client_xid, Some(20));
    }

    #[test]
    fn exact_root_guard_rejects_mismatch() {
        assert!(root_guard(1, 1).is_ok());
        let error = root_guard(1, 2).unwrap_err().to_string();
        assert!(error.contains("expected root 0x00000001"));
        assert!(error.contains("actual root 0x00000002"));
    }

    #[test]
    fn internal_xids_are_excluded_by_identity_only() {
        assert!(is_internal_xid(10, 10, 20));
        assert!(is_internal_xid(20, 10, 20));
        assert!(!is_internal_xid(30, 10, 20));
    }

    #[test]
    fn surface_eligibility_skips_expected_non_renderable_children() {
        let base = metadata();
        assert!(eligible_surface(&base, None, root(), 10, 0).is_some());

        let mut input_only = metadata();
        input_only.class = WindowClass::INPUT_ONLY;
        assert!(eligible_surface(&input_only, None, root(), 10, 0).is_none());

        let mut unviewable = metadata();
        unviewable.map_state = MapState::UNVIEWABLE;
        assert!(eligible_surface(&unviewable, None, root(), 10, 0).is_none());

        let mut zero_sized = metadata();
        zero_sized.geometry.width = 0;
        assert!(eligible_surface(&zero_sized, None, root(), 10, 0).is_none());

        let mut unsupported_depth = metadata();
        unsupported_depth.depth = 32;
        assert!(eligible_surface(&unsupported_depth, None, root(), 10, 0).is_none());
    }

    #[test]
    fn surface_order_is_the_query_tree_order() {
        let metadata = metadata();
        let bottom = eligible_surface(&metadata, None, root(), 10, 0).unwrap();
        let top = eligible_surface(&metadata, None, root(), 10, 1).unwrap();
        assert!(bottom.stacking_index < top.stacking_index);
    }

    #[test]
    fn structural_and_selection_events_shutdown() {
        let configure = x11rb::protocol::xproto::ConfigureNotifyEvent {
            response_type: 0,
            sequence: 0,
            event: 1,
            window: 1,
            above_sibling: 0,
            x: 0,
            y: 0,
            width: 100,
            height: 80,
            border_width: 0,
            override_redirect: false,
        };
        assert_eq!(
            scene_event_action(Event::ConfigureNotify(configure), 1, None),
            SceneEventAction::Shutdown(ShutdownReason::RootConfigure)
        );
        assert_eq!(
            scene_event_action(Event::ConfigureNotify(configure), 2, None),
            SceneEventAction::Continue
        );

    }

    #[test]
    fn shutdown_pending_before_scene_presented_blocks_presentation() {
        assert_eq!(
            presentation_decision(true),
            PresentationDecision::Shutdown
        );
        assert_eq!(
            presentation_decision(false),
            PresentationDecision::Present
        );
    }

    #[test]
    fn acquisition_error_after_manual_active_requires_coordinated_cleanup() {
        assert!(!coordinator_requires_cleanup(SceneState::PlaceholderReady));
        assert!(coordinator_requires_cleanup(SceneState::ManualActive));
        assert!(coordinator_requires_cleanup(SceneState::SceneSnapshotReady));
    }

    #[test]
    fn copy_plan_has_zero_intersection() {
        assert_eq!(build_copy_plan(window(100, 80, 10, 10, 0), pixmap(10, 10), root()), None);
    }

    #[test]
    fn scene_entry_shape_is_stable() {
        let _ = std::mem::size_of::<SurfaceEntry>();
    }
}
