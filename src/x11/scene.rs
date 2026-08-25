use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::errors::ReplyError;
use x11rb::protocol::composite::ConnectionExt as CompositeConnectionExt;
use x11rb::protocol::damage::{self, ConnectionExt as DamageConnectionExt};
use x11rb::protocol::render::{self, ConnectionExt as RenderConnectionExt};
use x11rb::protocol::ErrorKind;
use x11rb::protocol::xproto::{
    self, ChangeWindowAttributesAux, ConnectionExt as XprotoConnectionExt,
    EventMask, Window, WindowClass,
};
use x11rb::protocol::Event;

use crate::graphics::egl::{EglImportedSurface, EglSceneRenderer};
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
    backend: BackendCompatibility,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackendCompatibility {
    Renderable,
    BackendUnsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EglPixelSemantics {
    Opaque,
    PremultipliedAlpha,
    Unsupported,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VisualFormatInfo {
    pub(crate) visual: u32,
    pub(crate) depth: u8,
    pub(crate) pict_format: render::Pictformat,
    pub(crate) pict_type: render::PictType,
    pub(crate) red_shift: u16,
    pub(crate) red_mask: u16,
    pub(crate) green_shift: u16,
    pub(crate) green_mask: u16,
    pub(crate) blue_shift: u16,
    pub(crate) blue_mask: u16,
    pub(crate) alpha_shift: u16,
    pub(crate) alpha_mask: u16,
}

#[derive(Clone, Debug, Default)]
struct VisualFormatCache {
    by_visual: HashMap<u32, VisualFormatInfo>,
}

impl VisualFormatCache {
    fn acquire(connection: &X11Connection) -> Result<Self, Box<dyn Error>> {
        let version = connection
            .inner
            .render_query_version(RENDER_CLIENT_MAJOR, RENDER_CLIENT_MINOR)?
            .reply()?;
        if !render_version_compatible(version.major_version, version.minor_version) {
            return Err(format!(
                "Render version {}.{} is incompatible",
                version.major_version, version.minor_version
            ).into());
        }
        let reply = connection.inner.render_query_pict_formats()?.reply()?;
        Self::from_reply(&reply)
    }

    fn from_reply(reply: &render::QueryPictFormatsReply) -> Result<Self, Box<dyn Error>> {
        let formats = build_pict_format_index(&reply.formats)?;
        let mut by_visual = HashMap::new();
        for depth in reply.screens.iter().flat_map(|screen| screen.depths.iter()) {
            for visual in &depth.visuals {
                let format = formats.get(&visual.format).ok_or_else(|| {
                    format!(
                        "Render Visual 0x{:08x} references unknown PictFormat 0x{:08x}",
                        visual.visual, visual.format
                    )
                })?;
                if depth.depth != format.depth {
                    return Err(format!(
                        "Render Visual 0x{:08x} has Pictdepth {} but PictFormat {}",
                        visual.visual, depth.depth, format.depth
                    ).into());
                }
                let info = VisualFormatInfo {
                    visual: visual.visual,
                    depth: depth.depth,
                    pict_format: format.id,
                    pict_type: format.type_,
                    red_shift: format.direct.red_shift,
                    red_mask: format.direct.red_mask,
                    green_shift: format.direct.green_shift,
                    green_mask: format.direct.green_mask,
                    blue_shift: format.direct.blue_shift,
                    blue_mask: format.direct.blue_mask,
                    alpha_shift: format.direct.alpha_shift,
                    alpha_mask: format.direct.alpha_mask,
                };
                insert_visual_format(&mut by_visual, info)?;
            }
        }
        Ok(Self { by_visual })
    }

    fn semantics(&self, visual: u32, depth: u8) -> EglPixelSemantics {
        self.by_visual
            .get(&visual)
            .map_or(EglPixelSemantics::Unsupported, |info| {
                classify_scene_visual_format(info, depth)
            })
    }
}

const MAX_EVENTS_PER_BATCH: usize = 64;
const MAX_CANDIDATE_RETRIES: usize = 1;
const RENDER_CLIENT_MAJOR: u32 = 0;
const RENDER_CLIENT_MINOR: u32 = 11;

#[derive(Clone, Debug, Eq, PartialEq)]
struct SceneSnapshot {
    root: Window,
    root_geometry: RootGeometry,
    entries: Vec<SurfaceEntry>,
}

#[derive(Debug)]
enum CandidateBuildError {
    Stale(SceneInvalidation),
}

impl fmt::Display for CandidateBuildError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Stale(invalidation) => write!(formatter, "candidate stale: {invalidation:?}"),
        }
    }
}

impl Error for CandidateBuildError {}

impl SceneSnapshot {
    fn from_hierarchy(
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
                    return Err(Box::new(CandidateBuildError::Stale(SceneInvalidation::Hierarchy)))
                }
            };
            if metadata.window != surface_xid {
                return Err(Box::new(CandidateBuildError::Stale(SceneInvalidation::Hierarchy)));
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
        println!(
            "SceneSnapshot: root=0x{:08x} children={} eligible={}",
            hierarchy.root,
            hierarchy.children.len(),
            entries.len()
        );
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
    let backend = if metadata.depth == root_geometry.depth
        && metadata.visual == root_geometry.visual
    {
        BackendCompatibility::Renderable
    } else {
        println!(
            "scene surface backend unsupported 0x{surface_xid:08x}: depth={} visual=0x{:08x} root_depth={} root_visual=0x{:08x}",
            metadata.depth,
            metadata.visual,
            root_geometry.depth,
            root_geometry.visual
        );
        BackendCompatibility::BackendUnsupported
    };
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
        backend,
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DamageState {
    Active,
    DestroyAttempted,
    Released,
    AlreadyGone,
    Disarmed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DamageReleaseOutcome {
    Released,
    AlreadyGone,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DamageDestroyClassification {
    BadDamage,
    OtherError,
}

struct DamageLease<'a> {
    connection: &'a X11Connection,
    surface_xid: Window,
    damage_xid: damage::Damage,
    state: Cell<DamageState>,
}

impl<'a> DamageLease<'a> {
    fn acquire(
        connection: &'a X11Connection,
        surface_xid: Window,
    ) -> Result<Self, Box<dyn Error>> {
        let damage_xid = connection.inner.generate_id()?;
        connection
            .inner
            .damage_create(damage_xid, surface_xid, damage::ReportLevel::NON_EMPTY)?
            .check()?;
        println!(
            "DamageLease: damage=0x{:08x} surface=0x{:08x}",
            damage_xid, surface_xid
        );
        Ok(Self {
            connection,
            surface_xid,
            damage_xid,
            state: Cell::new(DamageState::Active),
        })
    }

    fn subtract(&self) -> Result<(), Box<dyn Error>> {
        if self.state.get() != DamageState::Active {
            return Ok(());
        }
        self.connection
            .inner
            .damage_subtract(self.damage_xid, x11rb::NONE, x11rb::NONE)?
            .check()?;
        Ok(())
    }

    fn destroy(&self) -> Result<DamageReleaseOutcome, Box<dyn Error>> {
        if self.state.get() != DamageState::Active {
            return Ok(DamageReleaseOutcome::Released);
        }
        self.state.set(DamageState::DestroyAttempted);
        self.connection
            .inner
            .damage_destroy(self.damage_xid)?
            .check()?;
        self.state.set(DamageState::Released);
        println!(
            "DamageLease released: damage=0x{:08x} surface=0x{:08x}",
            self.damage_xid, self.surface_xid
        );
        Ok(DamageReleaseOutcome::Released)
    }

    fn mark_already_gone(&self) {
        self.state.set(DamageState::AlreadyGone);
    }

    fn disarm_cleanup(&self) {
        self.state.set(DamageState::Disarmed);
    }
}

impl Drop for DamageLease<'_> {
    fn drop(&mut self) {
        if self.state.get() != DamageState::Active {
            return;
        }
        self.state.set(DamageState::DestroyAttempted);
        if let Ok(cookie) = self.connection.inner.damage_destroy(self.damage_xid) {
            if cookie.check().is_ok() {
                self.state.set(DamageState::Released);
            }
        }
    }
}

fn classify_damage_destroy_error(error: &(dyn Error + 'static)) -> DamageDestroyClassification {
    match error.downcast_ref::<ReplyError>() {
        Some(ReplyError::X11Error(error))
            if error.error_kind == ErrorKind::DamageBadDamage =>
        {
            DamageDestroyClassification::BadDamage
        }
        Some(_) | None => DamageDestroyClassification::OtherError,
    }
}

fn classify_retired_damage_destroy(
    surface_removed: bool,
    result: Result<(), DamageDestroyClassification>,
) -> Result<DamageReleaseOutcome, DamageDestroyClassification> {
    match result {
        Ok(()) => Ok(DamageReleaseOutcome::Released),
        Err(DamageDestroyClassification::BadDamage) if surface_removed => {
            Ok(DamageReleaseOutcome::AlreadyGone)
        }
        Err(error) => Err(error),
    }
}

fn retire_damage_lease(
    damage: &DamageLease<'_>,
    surface_removed: bool,
) -> Result<(), Box<dyn Error>> {
    match damage.destroy() {
        Ok(_) => Ok(()),
        Err(error) => {
            let classification = classify_damage_destroy_error(&*error);
            match classify_retired_damage_destroy(surface_removed, Err(classification)) {
                Ok(DamageReleaseOutcome::AlreadyGone) => {
                    damage.mark_already_gone();
                    println!(
                        "retired Damage already gone: surface=0x{:08x} damage=0x{:08x}",
                        damage.surface_xid, damage.damage_xid
                    );
                    Ok(())
                }
                Ok(DamageReleaseOutcome::Released) | Err(_) => Err(error),
            }
        }
    }
}

#[allow(dead_code)]
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
        _root: RootGeometry,
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
            || geometry.depth != entry.depth
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
                entry.depth,
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

    #[allow(dead_code)]
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

#[allow(dead_code)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CopyPlan {
    src_x: i16,
    src_y: i16,
    dst_x: i16,
    dst_y: i16,
    width: u16,
    height: u16,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct RenderQuadPlan {
    pub(crate) dst_x: i32,
    pub(crate) dst_y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) src_x: i32,
    pub(crate) src_y: i32,
    pub(crate) src_width: i32,
    pub(crate) src_height: i32,
    pub(crate) u0: f32,
    pub(crate) v0: f32,
    pub(crate) u1: f32,
    pub(crate) v1: f32,
}

fn build_render_quad_plan(
    window: WindowGeometry,
    pixmap: PixmapGeometry,
    root: RootGeometry,
) -> Option<RenderQuadPlan> {
    if pixmap.root == x11rb::NONE || pixmap.width == 0 || pixmap.height == 0 {
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
    if width <= 0 || height <= 0 || src_x + width > i32::from(pixmap.width)
        || src_y + height > i32::from(pixmap.height)
    {
        return None;
    }
    Some(RenderQuadPlan {
        dst_x,
        dst_y,
        width,
        height,
        src_x,
        src_y,
        src_width: width,
        src_height: height,
        u0: src_x as f32 / f32::from(pixmap.width),
        v0: src_y as f32 / f32::from(pixmap.height),
        u1: (src_x + width) as f32 / f32::from(pixmap.width),
        v1: (src_y + height) as f32 / f32::from(pixmap.height),
    })
}

#[allow(dead_code)]
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
                    root_live_event_mask(previous_mask),
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
    ScenePresented,
    RunningLivePixel,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownReason {
    RootConfigure,
    SelectionLost,
    OwnershipLost,
    Signal,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SceneInvalidation {
    Ignore,
    PixelDamage(damage::Damage),
    Geometry(Window),
    Hierarchy,
    Shutdown(ShutdownReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GateDecision {
    Accept,
    Retry(SceneInvalidation),
    Shutdown(ShutdownReason),
}

#[derive(Default)]
struct InvalidationBatch {
    hierarchy: bool,
    geometry: Option<Window>,
    shutdown: Option<ShutdownReason>,
    pixel_damage: HashSet<damage::Damage>,
}

#[derive(Debug)]
struct MetricsWindow {
    started: Instant,
    damage_events: u64,
    pixel_batches: u64,
    damaged_surface_total: u64,
    damaged_surface_max: u64,
    renders: u64,
    swaps: u64,
    total_swap_us: u128,
    max_swap_us: u128,
    damage_to_present_count: u64,
    total_damage_to_present_us: u128,
    max_damage_to_present_us: u128,
    total_render_to_swap_us: u128,
    max_render_to_swap_us: u128,
    structural_rebuilds: u64,
    egl_imports: u64,
    stale_retry: u64,
    stale_deferred: u64,
    first_damage_at: Option<Instant>,
}

impl MetricsWindow {
    fn new() -> Self {
        Self::new_at(Instant::now())
    }

    fn new_at(started: Instant) -> Self {
        Self {
            started,
            damage_events: 0,
            pixel_batches: 0,
            damaged_surface_total: 0,
            damaged_surface_max: 0,
            renders: 0,
            swaps: 0,
            total_swap_us: 0,
            max_swap_us: 0,
            damage_to_present_count: 0,
            total_damage_to_present_us: 0,
            max_damage_to_present_us: 0,
            total_render_to_swap_us: 0,
            max_render_to_swap_us: 0,
            structural_rebuilds: 0,
            egl_imports: 0,
            stale_retry: 0,
            stale_deferred: 0,
            first_damage_at: None,
        }
    }

    fn record_damage_event(&mut self) {
        self.damage_events += 1;
    }

    fn record_first_damage(&mut self, at: Instant) {
        if self.first_damage_at.is_none() {
            self.first_damage_at = Some(at);
        }
    }

    fn record_pixel_batch(&mut self, damaged_surfaces: usize) {
        self.pixel_batches += 1;
        self.damaged_surface_total += damaged_surfaces as u64;
        self.damaged_surface_max = self.damaged_surface_max.max(damaged_surfaces as u64);
    }

    fn record_render(&mut self) {
        self.renders += 1;
    }

    fn record_import(&mut self) {
        self.egl_imports += 1;
    }

    fn record_structural_rebuild(&mut self) {
        self.structural_rebuilds += 1;
    }

    fn record_stale_retry(&mut self) {
        self.stale_retry += 1;
    }

    fn record_stale_deferred(&mut self) {
        self.stale_deferred += 1;
    }

    fn clear_pending_damage(&mut self) {
        self.first_damage_at = None;
    }

    fn record_swap(
        &mut self,
        swap_duration: Duration,
        render_to_swap: Duration,
        presented_at: Instant,
        pixel_frame: bool,
        succeeded: bool,
    ) {
        let swap_us = swap_duration.as_micros();
        let render_to_swap_us = render_to_swap.as_micros();
        self.swaps += 1;
        self.total_swap_us += swap_us;
        self.max_swap_us = self.max_swap_us.max(swap_us);
        self.total_render_to_swap_us += render_to_swap_us;
        self.max_render_to_swap_us = self.max_render_to_swap_us.max(render_to_swap_us);
        if pixel_frame && succeeded {
            if let Some(first_damage_at) = self.first_damage_at.take() {
                let latency_us = presented_at
                    .saturating_duration_since(first_damage_at)
                    .as_micros();
                self.damage_to_present_count += 1;
                self.total_damage_to_present_us += latency_us;
                self.max_damage_to_present_us = self.max_damage_to_present_us.max(latency_us);
            }
        }
    }

    fn report_if_due(&mut self, now: Instant) -> Option<String> {
        let interval = now.saturating_duration_since(self.started);
        if interval < Duration::from_secs(1) {
            return None;
        }
        let interval_ms = interval.as_millis();
        let damaged_surface_avg = if self.pixel_batches == 0 {
            0.0
        } else {
            self.damaged_surface_total as f64 / self.pixel_batches as f64
        };
        let swap_avg = average(self.total_swap_us, self.swaps);
        let damage_to_present_avg =
            average(self.total_damage_to_present_us, self.damage_to_present_count);
        let render_to_swap_avg = average(self.total_render_to_swap_us, self.swaps);
        let line = format!(
            "XOMPOSITE_METRICS interval_ms={interval_ms} damage_events={} pixel_batches={} damaged_surfaces_avg={damaged_surface_avg:.2} damaged_surfaces_max={} renders={} swaps={} swap_total_us={} swap_avg_us={swap_avg} swap_max_us={} damage_to_present_avg_us={damage_to_present_avg} damage_to_present_max_us={} render_to_swap_avg_us={render_to_swap_avg} render_to_swap_max_us={} structural_rebuilds={} egl_imports={} stale_retry={} stale_deferred={}",
            self.damage_events,
            self.pixel_batches,
            self.damaged_surface_max,
            self.renders,
            self.swaps,
            self.total_swap_us,
            self.max_swap_us,
            self.max_damage_to_present_us,
            self.max_render_to_swap_us,
            self.structural_rebuilds,
            self.egl_imports,
            self.stale_retry,
            self.stale_deferred,
        );
        *self = Self::new_at(now);
        Some(line)
    }
}

fn average(total: u128, count: u64) -> u128 {
    if count == 0 {
        0
    } else {
        total / u128::from(count)
    }
}

impl InvalidationBatch {
    fn push(&mut self, invalidation: SceneInvalidation) {
        match invalidation {
            SceneInvalidation::Ignore => {}
            SceneInvalidation::PixelDamage(damage_id) => {
                self.pixel_damage.insert(damage_id);
            }
            SceneInvalidation::Geometry(window) if !self.hierarchy => {
                self.geometry = Some(window);
            }
            SceneInvalidation::Geometry(_) => {}
            SceneInvalidation::Hierarchy => {
                self.hierarchy = true;
                self.geometry = None;
            }
            SceneInvalidation::Shutdown(reason) => {
                self.shutdown = Some(reason);
            }
        }
    }

    fn decision(&self) -> SceneInvalidation {
        if let Some(reason) = self.shutdown {
            SceneInvalidation::Shutdown(reason)
        } else if self.hierarchy {
            SceneInvalidation::Hierarchy
        } else if let Some(window) = self.geometry {
            SceneInvalidation::Geometry(window)
        } else if let Some(damage_id) = self.pixel_damage.iter().next().copied() {
            SceneInvalidation::PixelDamage(damage_id)
        } else {
            SceneInvalidation::Ignore
        }
    }

    fn pixel_damage(&self) -> &HashSet<damage::Damage> {
        &self.pixel_damage
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
    structure_watches: SceneStructureWatches<'a>,
    visual_formats: VisualFormatCache,
    manual: Option<ManualSubwindowsRedirect<'a>>,
    egl: Option<EglSceneRenderer>,
    pixmaps: Vec<NamedSurfacePixmap<'a>>,
    damage_leases: Vec<DamageLease<'a>>,
    damage_registry: HashMap<damage::Damage, Window>,
    pending_damage: HashSet<damage::Damage>,
    structural_generation: u64,
    attempted_structural_generation: u64,
    snapshot: Option<SceneSnapshot>,
    egl_surfaces: HashMap<Window, EglImportedSurface>,
    signal: SignalWake,
    state: SceneState,
    metrics: MetricsWindow,
}

struct SceneCandidate<'a> {
    snapshot: SceneSnapshot,
    generation: u64,
    render_started: Instant,
    // Declaration order is cleanup order: imported EGL resources must drop
    // before their source pixmaps, with Damage leases released in between.
    egl_surfaces: HashMap<Window, EglImportedSurface>,
    damage_leases: Vec<DamageLease<'a>>,
    pixmaps: Vec<NamedSurfacePixmap<'a>>,
    damage_registry: HashMap<damage::Damage, Window>,
    watch_ids: HashSet<Window>,
    watch_additions: Vec<Window>,
}

struct SceneStructureWatches<'a> {
    connection: &'a X11Connection,
    previous_masks: HashMap<Window, EventMask>,
    disarmed: bool,
}

impl<'a> SceneStructureWatches<'a> {
    fn new(connection: &'a X11Connection) -> Self {
        Self {
            connection,
            previous_masks: HashMap::new(),
            disarmed: false,
        }
    }

    fn ensure_candidate(
        &mut self,
        windows: &HashSet<Window>,
    ) -> Result<Vec<Window>, Box<dyn Error>> {
        let mut additions = Vec::new();
        let existing = self.previous_masks.keys().copied().collect::<HashSet<_>>();
        let (candidate_additions, _) = watch_plan(&existing, windows);
        for window in candidate_additions {
            let attributes = match self.connection.inner.get_window_attributes(window) {
                Ok(cookie) => match cookie.reply() {
                    Ok(attributes) => attributes,
                    Err(error) if super::capture::is_bad_window_error(&error) => continue,
                    Err(error) => {
                        self.rollback(&additions)?;
                        return Err(error.into());
                    }
                },
                Err(error) => {
                    self.rollback(&additions)?;
                    return Err(error.into());
                }
            };
            let previous = attributes.your_event_mask;
            let cookie = match self.connection
                .inner
                .change_window_attributes(
                    window,
                    &ChangeWindowAttributesAux::new()
                        .event_mask(canonical_live_event_mask(previous)),
                ) {
                Ok(cookie) => cookie,
                Err(error) => {
                    self.rollback(&additions)?;
                    return Err(error.into());
                }
            };
            let result = cookie.check();
            if let Err(error) = result {
                self.rollback(&additions)?;
                return Err(error.into());
            }
            self.previous_masks.insert(window, previous);
            additions.push(window);
        }
        if let Err(error) = self.connection.inner.flush() {
            self.rollback(&additions)?;
            return Err(error.into());
        }
        Ok(additions)
    }

    fn rollback(&mut self, additions: &[Window]) -> Result<(), Box<dyn Error>> {
        let mut first_error = None;
        for window in additions.iter().rev() {
            let Some(previous) = self.previous_masks.remove(window) else {
                continue;
            };
            match self
                .connection
                .inner
                .change_window_attributes(
                    *window,
                    &ChangeWindowAttributesAux::new().event_mask(previous),
                ) {
                Ok(cookie) => {
                    if let Err(error) = cookie.check() {
                        if !super::capture::is_bad_window_error(&error)
                            && first_error.is_none()
                        {
                            first_error = Some(error.into());
                        }
                    }
                }
                Err(error) => {
                    if !super::capture::is_bad_window_error(&error)
                        && first_error.is_none()
                    {
                        first_error = Some(error.into());
                    }
                }
            }
        }
        if let Err(error) = self.connection.inner.flush() {
            if first_error.is_none() {
                first_error = Some(error.into());
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    fn reconcile(&mut self, desired: &HashSet<Window>) -> Result<(), Box<dyn Error>> {
        let existing = self.previous_masks.keys().copied().collect::<HashSet<_>>();
        let (_, obsolete_set) = watch_plan(&existing, desired);
        let obsolete = obsolete_set.into_iter().collect::<Vec<_>>();
        for window in obsolete {
            if let Some(previous) = self.previous_masks.remove(&window) {
                let result = self
                    .connection
                    .inner
                    .change_window_attributes(
                        window,
                        &ChangeWindowAttributesAux::new().event_mask(previous),
                    )?
                    .check();
                if let Err(error) = result {
                    if !super::capture::is_bad_window_error(&error) {
                        return Err(error.into());
                    }
                }
            }
        }
        self.connection.inner.flush()?;
        Ok(())
    }

    fn cleanup(&mut self) -> Result<(), Box<dyn Error>> {
        if self.disarmed {
            return Ok(());
        }
        self.reconcile(&HashSet::new())
    }

    fn disarm_cleanup(&mut self) {
        self.previous_masks.clear();
        self.disarmed = true;
    }
}

impl<'a> SceneSession<'a> {
    fn acquire(connection: &'a X11Connection, expected_root: Window) -> Result<Self, Box<dyn Error>> {
        let root = connection.inner.setup().roots[connection.screen_num()].root;
        root_guard(expected_root, root)?;
        check_capabilities(connection)?;
        check_selection_available(connection)?;
        ensure_damage_version(connection)?;
        let visual_formats = VisualFormatCache::acquire(connection)?;
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
        let egl = match EglSceneRenderer::create(
            connection,
            overlay.overlay,
            screen.root_visual,
            screen.root_depth,
            root_geometry.width,
            root_geometry.height,
        ) {
            Ok(egl) => egl,
            Err(error) => {
                if let Err(cleanup_error) = overlay.restore_input_shape() {
                    eprintln!("EGL preflight input cleanup failed: {cleanup_error}");
                }
                if let Err(cleanup_error) = overlay.release_overlay() {
                    eprintln!("EGL preflight overlay cleanup failed: {cleanup_error}");
                }
                if let Err(cleanup_error) = ownership.release(connection) {
                    eprintln!("EGL preflight ownership cleanup failed: {cleanup_error}");
                }
                return Err(error);
            }
        };
        println!("state: PlaceholderReady");
        let manual = ManualSubwindowsRedirect::acquire(connection, root)?;
        println!("state: ManualActive");
        let mut session = Self {
            connection,
            root,
            ownership: Some(ownership),
            overlay: Some(overlay),
            root_watch: Some(root_watch),
            structure_watches: SceneStructureWatches::new(connection),
            visual_formats,
            manual: Some(manual),
            egl: Some(egl),
            pixmaps: Vec::new(),
            damage_leases: Vec::new(),
            damage_registry: HashMap::new(),
            pending_damage: HashSet::new(),
            // Generation 1 is the initial scene build.  A generation is
            // ready exactly when it is newer than the last attempted one.
            structural_generation: 1,
            attempted_structural_generation: 0,
            snapshot: None,
            egl_surfaces: HashMap::new(),
            signal,
            state: SceneState::PlaceholderReady,
            metrics: MetricsWindow::new(),
        };
        session.state = SceneState::ManualActive;
        Ok(session)
    }

    fn run(connection: &'a X11Connection, expected_root: Window) -> Result<(), Box<dyn Error>> {
        let mut session = Self::acquire(connection, expected_root)?;
        let operation = session
            .prepare_scene()
            .and_then(|_| session.wait_live_pixel());
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
        self.rebuild_and_present()
    }

    fn build_candidate(&mut self) -> Result<SceneCandidate<'a>, Box<dyn Error>> {
        let generation = self.structural_generation;
        let root_geometry = read_root_geometry(self.connection, self.root)?;
        let hierarchy = self.connection.snapshot_hierarchy()?;
        let watch_ids = snapshot_watch_ids(&hierarchy);
        let overlay = self.overlay.as_ref().ok_or("overlay is unavailable")?.overlay;
        let owner = self
            .ownership
            .as_ref()
            .ok_or("ownership is unavailable")?
            .owner_window;
        let snapshot = SceneSnapshot::from_hierarchy(
            hierarchy,
            root_geometry,
            overlay,
            owner,
        )?;
        self.state = SceneState::SceneSnapshotReady;
        let mut pixmaps = Vec::new();
        let mut damage_leases = Vec::new();
        let mut damage_registry = HashMap::new();
        let mut egl_surfaces = HashMap::new();
        for entry in &snapshot.entries {
            let semantics = self.visual_formats.semantics(entry.visual, entry.depth);
            let importable = semantics != EglPixelSemantics::Unsupported;
            if importable {
                let damage = DamageLease::acquire(self.connection, entry.surface_xid)?;
                damage.subtract()?;
                damage_registry.insert(damage.damage_xid, entry.surface_xid);
                damage_leases.push(damage);
            }
            let pixmap = NamedSurfacePixmap::acquire(
                self.connection,
                entry,
                self.root,
                root_geometry,
            )?;
            if !importable {
                println!(
                    "EGL import unsupported by capability policy: canonical surface=0x{:08x} depth={} visual=0x{:08x}",
                    entry.surface_xid, entry.depth, entry.visual
                );
                pixmaps.push(pixmap);
                continue;
            }
            let egl_surface = self
                .egl
                .as_ref()
                .ok_or("EGL scene renderer is unavailable")?
                .import_pixmap(pixmap.pixmap_xid, semantics)?;
            self.metrics.record_import();
            egl_surfaces.insert(entry.surface_xid, egl_surface);
            pixmaps.push(pixmap);
        }
        self.connection.inner.get_input_focus()?.reply()?;
        for entry in &snapshot.entries {
            let semantics = self.visual_formats.semantics(entry.visual, entry.depth);
            let damage_active = damage_registry.values().any(|surface| *surface == entry.surface_xid);
            if !candidate_render_allowed(semantics, damage_active) {
                return Err(format!("candidate DamageLease is not active before EGL render for surface 0x{:08x}", entry.surface_xid).into());
            }
        }
        if !egl_scene_is_renderable(snapshot.entries.len(), egl_surfaces.len()) {
            return Err("scene has canonical surfaces but no EGL-renderable surfaces".into());
        }
        let render_started = Instant::now();
        self.metrics.record_render();
        self.render_egl_scene(&snapshot, &egl_surfaces, &pixmaps)?;
        self.state = SceneState::NamedPixmapsReady;
        println!("state: EGLImported surfaces={}", egl_surfaces.len());
        let watch_additions = self.structure_watches.ensure_candidate(&watch_ids)?;
        Ok(SceneCandidate {
            snapshot,
            generation,
            render_started,
            pixmaps,
            damage_leases,
            damage_registry,
            egl_surfaces,
            watch_ids,
            watch_additions,
        })
    }

    fn rebuild_and_present(&mut self) -> Result<(), Box<dyn Error>> {
        self.metrics.record_structural_rebuild();
        self.metrics.clear_pending_damage();
        for attempt in 0..=MAX_CANDIDATE_RETRIES {
            let generation = self.structural_generation;
            self.attempted_structural_generation = generation;
            let candidate = match self.build_candidate() {
                Ok(candidate) => candidate,
                Err(error) => {
                    let stale = error
                        .downcast_ref::<CandidateBuildError>()
                        .map(|stale| match stale { CandidateBuildError::Stale(invalidation) => *invalidation });
                    let Some(invalidation) = stale else {
                        return Err(error);
                    };
                    if retry_allowed(attempt) {
                        self.metrics.record_stale_retry();
                        println!("candidate stale; bounded retry: {invalidation:?}");
                        continue;
                    } else {
                        self.metrics.record_stale_deferred();
                        println!("candidate stale; deferred rebuild: {invalidation:?}");
                        return Ok(());
                    }
                }
            };
            debug_assert_eq!(candidate.generation, generation);
            let (gate, deferred_damage) = match self.pre_commit_gate(&candidate) {
                Ok(gate) => gate,
                Err(error) => {
                    self.structure_watches.rollback(&candidate.watch_additions)?;
                    return Err(error);
                }
            };
            match gate {
                GateDecision::Accept => {
                    self.commit_candidate(candidate)?;
                    self.merge_deferred_damage(deferred_damage);
                    return Ok(());
                }
                GateDecision::Shutdown(reason) => {
                    self.structure_watches.rollback(&candidate.watch_additions)?;
                    return Err(format!("candidate aborted by shutdown: {reason:?}").into());
                }
                GateDecision::Retry(invalidation) if retry_allowed(attempt) => {
                    self.metrics.record_stale_retry();
                    self.merge_deferred_damage(deferred_damage);
                    self.structure_watches.rollback(&candidate.watch_additions)?;
                    println!("candidate stale; bounded retry: {invalidation:?}");
                }
                GateDecision::Retry(invalidation) => {
                    self.merge_deferred_damage(deferred_damage);
                    self.structure_watches.rollback(&candidate.watch_additions)?;
                    if retry_allowed(attempt) {
                        println!("candidate stale; bounded retry: {invalidation:?}");
                        continue;
                    } else {
                        self.metrics.record_stale_deferred();
                        drop(candidate);
                        println!("candidate stale; deferred rebuild: {invalidation:?}");
                        return Ok(());
                    }
                }
            }
        }
        unreachable!("bounded candidate retry must return");
    }

    fn pre_commit_gate(
        &mut self,
        candidate: &SceneCandidate<'_>,
    ) -> Result<(GateDecision, HashSet<damage::Damage>), Box<dyn Error>> {
        self.connection.inner.get_input_focus()?.reply()?;
        let mut batch = InvalidationBatch::default();
        let mut drained = 0;
        for _ in 0..MAX_EVENTS_PER_BATCH {
            let Some(event) = self.connection.inner.poll_for_event()? else {
                break;
            };
            drained += 1;
            self.observe_event(&event);
            let invalidation = classify_event_with_registries(
                event,
                self.root,
                &candidate.snapshot,
                self.ownership.as_ref(),
                &self.damage_registry,
                &candidate.damage_registry,
            );
            self.observe_invalidation(invalidation);
            batch.push(invalidation);
        }
        let batch_decision = batch.decision();
        let deferred_damage = batch.pixel_damage().clone();
        let ownership_verified = self.verify_ownership().is_ok();
        if !ownership_verified {
            return Ok((gate_decision_after_batch(
                batch_decision,
                bounded_batch_requires_retry(drained),
                false,
                false,
            ), deferred_damage));
        }
        let signal_pending = self.signal.poll_shutdown_pending()?;
        let decision = candidate_gate_decision(
            batch_decision,
            bounded_batch_requires_retry(drained),
            true,
            signal_pending,
        );
        Ok((decision, deferred_damage))
    }

    fn commit_candidate(&mut self, candidate: SceneCandidate<'a>) -> Result<(), Box<dyn Error>> {
        let additions = candidate.watch_additions.clone();
        let result = self.commit_candidate_inner(candidate);
        if result.is_err() {
            if let Err(error) = self.structure_watches.rollback(&additions) {
                eprintln!("candidate watch rollback failed: {error}");
            }
        }
        result
    }

    fn commit_candidate_inner(
        &mut self,
        candidate: SceneCandidate<'a>,
    ) -> Result<(), Box<dyn Error>> {
        let render_started = candidate.render_started;
        self.metrics.clear_pending_damage();
        self.present_frame(render_started, false)?;
        self.state = SceneState::ScenePresented;
        let old_surfaces = self
            .snapshot
            .as_ref()
            .map(|snapshot| {
                snapshot
                    .entries
                    .iter()
                    .map(|entry| entry.surface_xid)
                    .collect::<HashSet<_>>()
            })
            .unwrap_or_default();
        let new_surfaces = candidate
            .snapshot
            .entries
            .iter()
            .map(|entry| entry.surface_xid)
            .collect::<HashSet<_>>();
        let removed_surfaces = old_surfaces
            .difference(&new_surfaces)
            .copied()
            .collect::<HashSet<_>>();
        let snapshot = candidate.snapshot;
        let old_pixmaps = std::mem::replace(&mut self.pixmaps, candidate.pixmaps);
        let old_damage_leases = std::mem::replace(&mut self.damage_leases, candidate.damage_leases);
        let mut old_egl_surfaces = std::mem::replace(&mut self.egl_surfaces, candidate.egl_surfaces);
        self.damage_registry = candidate.damage_registry;
        self.snapshot = Some(snapshot);
        self.structure_watches.reconcile(&candidate.watch_ids)?;
        for damage in &old_damage_leases {
            retire_damage_lease(damage, removed_surfaces.contains(&damage.surface_xid))?;
        }
        let egl = self.egl.as_ref().ok_or("EGL scene renderer is unavailable")?;
        egl.make_current()?;
        for surface in old_egl_surfaces.values_mut() {
            egl.destroy_import(surface)?;
        }
        drop(old_pixmaps);
        self.retain_current_pending();
        self.state = SceneState::RunningLivePixel;
        println!("state: ScenePresented (MANUAL active, EGL scene renderer)");
        println!("state: RunningLivePixel");
        Ok(())
    }

    fn retain_current_pending(&mut self) {
        retain_pending_for_registry(&mut self.pending_damage, &self.damage_registry);
    }

    fn merge_deferred_damage(&mut self, deferred: HashSet<damage::Damage>) {
        merge_deferred_damage_for_registry(
            &mut self.pending_damage,
            deferred,
            &self.damage_registry,
        );
    }

    fn observe_event(&mut self, event: &Event) {
        if matches!(event, Event::DamageNotify(_)) {
            self.metrics.record_damage_event();
        }
    }

    fn observe_invalidation(&mut self, invalidation: SceneInvalidation) {
        if matches!(invalidation, SceneInvalidation::PixelDamage(_)) {
            self.metrics.record_first_damage(Instant::now());
        }
        observe_structural_generation(&mut self.structural_generation, invalidation);
    }

    fn wait_live_pixel(&mut self) -> Result<(), Box<dyn Error>> {
        loop {
            if let Some(line) = self.metrics.report_if_due(Instant::now()) {
                println!("{line}");
            }
            let mut batch = InvalidationBatch::default();
            let pending = std::mem::take(&mut self.pending_damage);
            let had_pending_work = pending_work_requires_iteration(&pending);
            if matches!(
                structural_generation_state(
                    self.structural_generation,
                    self.attempted_structural_generation,
                ),
                StructuralGenerationState::Ready(_)
            ) {
                batch.push(SceneInvalidation::Hierarchy);
            }
            for damage_id in pending {
                if self.damage_registry.contains_key(&damage_id) {
                    batch.push(SceneInvalidation::PixelDamage(damage_id));
                }
            }
            if batch.decision() == SceneInvalidation::Ignore && !had_pending_work {
                let first = match wait_for_event_or_shutdown(self.connection, &mut self.signal)? {
                    WaitResult::Event(event) => event,
                    WaitResult::Shutdown => {
                        println!("scene shutdown: Signal");
                        return Ok(());
                    }
                };
                self.observe_event(&first);
                let invalidation = classify_event_with_registries(
                    first,
                    self.root,
                    self.current_snapshot(),
                    self.ownership.as_ref(),
                    &self.damage_registry,
                    &self.damage_registry,
                );
                self.observe_invalidation(invalidation);
                batch.push(invalidation);
                for _ in 1..MAX_EVENTS_PER_BATCH {
                    let Some(event) = self.connection.inner.poll_for_event()? else {
                        break;
                    };
                    self.observe_event(&event);
                    let invalidation = classify_event_with_registries(
                        event,
                        self.root,
                        self.current_snapshot(),
                        self.ownership.as_ref(),
                        &self.damage_registry,
                        &self.damage_registry,
                    );
                    self.observe_invalidation(invalidation);
                    batch.push(invalidation);
                }
            }
            if self.signal.poll_shutdown_pending()? {
                println!("scene shutdown: Signal");
                return Ok(());
            }
            match batch.decision() {
                SceneInvalidation::Ignore => {}
                SceneInvalidation::Shutdown(reason) => {
                    println!("scene shutdown: {reason:?}");
                    return Ok(());
                }
                SceneInvalidation::Geometry(_) | SceneInvalidation::Hierarchy => {
                    self.pending_damage.clear();
                    self.rebuild_and_present()?;
                }
                SceneInvalidation::PixelDamage(_) => {
                    self.recompose_current_scene(batch.pixel_damage().clone())?;
                }
            }
        }
    }

    fn recompose_current_scene(
        &mut self,
        touched_damage: HashSet<damage::Damage>,
    ) -> Result<(), Box<dyn Error>> {
        let touched_damage = touched_damage
            .into_iter()
            .filter(|damage_id| self.damage_registry.contains_key(damage_id))
            .collect::<HashSet<_>>();
        if touched_damage.is_empty() {
            return Ok(());
        }
        for damage_id in subtract_plan(&touched_damage) {
            self.damage_lease(damage_id)?.subtract()?;
        }
        self.connection.inner.get_input_focus()?.reply()?;
        let post_subtract = self.drain_current_events()?;
        self.pending_damage.extend(post_subtract.pixel_damage().iter().copied());
        match post_subtract.decision() {
            SceneInvalidation::Shutdown(reason) => {
                println!("scene shutdown: {reason:?}");
                return Ok(());
            }
            SceneInvalidation::Hierarchy | SceneInvalidation::Geometry(_) => {
                self.pending_damage.clear();
                return self.rebuild_and_present();
            }
            SceneInvalidation::Ignore | SceneInvalidation::PixelDamage(_) => {}
        }
        let render_started = Instant::now();
        self.metrics.record_pixel_batch(
            touched_damage
                .iter()
                .filter_map(|damage_id| self.damage_registry.get(damage_id))
                .collect::<HashSet<_>>()
                .len(),
        );
        self.metrics.record_render();
        self.full_recompose_current()?;
        self.connection.inner.get_input_focus()?.reply()?;
        let final_gate = self.drain_current_events()?;
        self.pending_damage.extend(final_gate.pixel_damage().iter().copied());
        let ownership_ok = self.verify_ownership().is_ok();
        if !ownership_ok {
            println!("scene shutdown: OwnershipLost");
            return Ok(());
        }
        if self.signal.poll_shutdown_pending()? {
            println!("scene shutdown: Signal");
            return Ok(());
        }
        if pixel_gate_allows_presentation(final_gate.decision(), ownership_ok, false) {
            return self.present_frame(render_started, true);
        }
        match final_gate.decision() {
            SceneInvalidation::Shutdown(reason) => {
                println!("scene shutdown: {reason:?}");
                Ok(())
            }
            SceneInvalidation::Hierarchy | SceneInvalidation::Geometry(_) => {
                self.pending_damage.clear();
                self.rebuild_and_present()
            }
            SceneInvalidation::Ignore | SceneInvalidation::PixelDamage(_) => Ok(()),
        }
    }

    fn drain_current_events(&mut self) -> Result<InvalidationBatch, Box<dyn Error>> {
        let mut batch = InvalidationBatch::default();
        for _ in 0..MAX_EVENTS_PER_BATCH {
            let Some(event) = self.connection.inner.poll_for_event()? else {
                break;
            };
            self.observe_event(&event);
            let invalidation = classify_event_with_registries(
                event,
                self.root,
                self.current_snapshot(),
                self.ownership.as_ref(),
                &self.damage_registry,
                &self.damage_registry,
            );
            self.observe_invalidation(invalidation);
            batch.push(invalidation);
        }
        Ok(batch)
    }

    fn damage_lease(
        &self,
        damage_id: damage::Damage,
    ) -> Result<&DamageLease<'a>, Box<dyn Error>> {
        self.damage_leases
            .iter()
            .find(|lease| lease.damage_xid == damage_id)
            .ok_or_else(|| format!("current DamageLease is unavailable: 0x{damage_id:08x}").into())
    }

    fn full_recompose_current(&self) -> Result<(), Box<dyn Error>> {
        self.render_egl_scene(self.current_snapshot(), &self.egl_surfaces, &self.pixmaps)
    }

    fn render_egl_scene(
        &self,
        snapshot: &SceneSnapshot,
        surfaces: &HashMap<Window, EglImportedSurface>,
        pixmaps: &[NamedSurfacePixmap<'a>],
    ) -> Result<(), Box<dyn Error>> {
        let egl = self.egl.as_ref().ok_or("EGL scene renderer is unavailable")?;
        egl.clear()?;
        for entry in &snapshot.entries {
            let Some(surface) = surfaces.get(&entry.surface_xid) else {
                continue;
            };
            let pixmap = pixmaps
                .iter()
                .find(|pixmap| pixmap.surface_xid == entry.surface_xid)
                .ok_or_else(|| format!("missing pixmap for EGL surface 0x{:08x}", entry.surface_xid))?;
            let plan = build_render_quad_plan(entry.geometry, pixmap.geometry, snapshot.root_geometry)
                .ok_or_else(|| format!("surface 0x{:08x} has no visible render quad", entry.surface_xid))?;
            egl.render_surface(
                surface.texture,
                plan,
                surface.pixel_semantics,
            )?;
        }
        Ok(())
    }

    fn present_frame(
        &mut self,
        render_started: Instant,
        pixel_frame: bool,
    ) -> Result<(), Box<dyn Error>> {
        let swap_started = Instant::now();
        let result = self
            .egl
            .as_ref()
            .ok_or("EGL scene renderer is unavailable")?
            .swap();
        let presented_at = Instant::now();
        self.metrics.record_swap(
            presented_at.saturating_duration_since(swap_started),
            presented_at.saturating_duration_since(render_started),
            presented_at,
            pixel_frame,
            result.is_ok(),
        );
        if let Some(line) = self.metrics.report_if_due(presented_at) {
            println!("{line}");
        }
        result
    }

    fn current_snapshot(&self) -> &SceneSnapshot {
        self.snapshot
            .as_ref()
            .expect("published scene snapshot must exist while live")
    }

    #[allow(dead_code)]
    fn overlay_depth(&self) -> Result<u8, Box<dyn Error>> {
        let overlay = self.overlay.as_ref().ok_or("overlay is unavailable")?.overlay;
        Ok(self.connection.inner.get_geometry(overlay)?.reply()?.depth)
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
        for damage in &self.damage_leases {
            if let Err(error) = damage.destroy() {
                first_error.get_or_insert(error);
            }
        }
        self.damage_leases.clear();
        self.damage_registry.clear();
        self.pending_damage.clear();
        let egl_current = match self.egl.as_ref() {
            Some(egl) => match egl.make_current() {
                Ok(()) => true,
                Err(error) => {
                    first_error.get_or_insert(error);
                    false
                }
            },
            None => false,
        };
        if egl_current {
            let egl = self.egl.as_ref().expect("EGL renderer exists when current");
            for surface in self.egl_surfaces.values_mut() {
                if let Err(error) = egl.destroy_import(surface) {
                    first_error.get_or_insert(error);
                }
            }
        } else {
            for surface in self.egl_surfaces.values_mut() {
                surface.disarm();
            }
        }
        self.egl_surfaces.clear();
        for pixmap in &self.pixmaps {
            if let Err(error) = pixmap.free() {
                first_error.get_or_insert(error);
            }
        }
        self.pixmaps.clear();
        if let Some(mut egl) = self.egl.take() {
            if egl_current {
                if let Err(error) = egl.destroy() {
                    first_error.get_or_insert(error);
                }
            } else {
                egl.disarm();
            }
        }
        if let Err(error) = self.structure_watches.cleanup() {
            first_error.get_or_insert(error);
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
        for pixmap in &self.pixmaps {
            pixmap.disarm_cleanup();
        }
        self.pixmaps.clear();
        for damage in &self.damage_leases {
            damage.disarm_cleanup();
        }
        self.damage_leases.clear();
        self.damage_registry.clear();
        self.pending_damage.clear();
        for surface in self.egl_surfaces.values_mut() {
            surface.disarm();
        }
        self.egl_surfaces.clear();
        if let Some(mut egl) = self.egl.take() {
            egl.disarm();
        }
        self.structure_watches.disarm_cleanup();
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

fn egl_scene_is_renderable(entry_count: usize, egl_surface_count: usize) -> bool {
    entry_count == 0 || egl_surface_count > 0
}

fn retain_pending_for_registry(
    pending: &mut HashSet<damage::Damage>,
    registry: &HashMap<damage::Damage, Window>,
) {
    pending.retain(|damage_id| registry.contains_key(damage_id));
}

fn merge_deferred_damage_for_registry(
    pending: &mut HashSet<damage::Damage>,
    deferred: HashSet<damage::Damage>,
    registry: &HashMap<damage::Damage, Window>,
) {
    pending.extend(deferred);
    retain_pending_for_registry(pending, registry);
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

fn ensure_damage_version(connection: &X11Connection) -> Result<(), Box<dyn Error>> {
    let version = connection.inner.damage_query_version(1, 1)?.reply()?;
    println!(
        "XDamage version: {}.{}",
        version.major_version, version.minor_version
    );
    if !damage_version_compatible(version.major_version, version.minor_version) {
        return Err("XDamage 1.0 or newer is required for live pixel damage".into());
    }
    Ok(())
}

fn render_version_compatible(major: u32, minor: u32) -> bool {
    let _ = minor;
    major == 0
}

fn pict_format_semantically_equal(
    left: &render::Pictforminfo,
    right: &render::Pictforminfo,
) -> bool {
    left.type_ == right.type_
        && left.depth == right.depth
        && left.direct.red_shift == right.direct.red_shift
        && left.direct.red_mask == right.direct.red_mask
        && left.direct.green_shift == right.direct.green_shift
        && left.direct.green_mask == right.direct.green_mask
        && left.direct.blue_shift == right.direct.blue_shift
        && left.direct.blue_mask == right.direct.blue_mask
        && left.direct.alpha_shift == right.direct.alpha_shift
        && left.direct.alpha_mask == right.direct.alpha_mask
        && left.colormap == right.colormap
}

fn insert_pict_format(
    by_id: &mut HashMap<render::Pictformat, render::Pictforminfo>,
    info: render::Pictforminfo,
) -> Result<(), Box<dyn Error>> {
    if let Some(previous) = by_id.get(&info.id) {
        if !pict_format_semantically_equal(previous, &info) {
            return Err(format!(
                "Render PictFormat 0x{:08x} has conflicting definitions",
                info.id
            ).into());
        }
    } else {
        by_id.insert(info.id, info);
    }
    Ok(())
}

fn build_pict_format_index(
    formats: &[render::Pictforminfo],
) -> Result<HashMap<render::Pictformat, render::Pictforminfo>, Box<dyn Error>> {
    let mut by_id = HashMap::new();
    for info in formats {
        insert_pict_format(&mut by_id, *info)?;
    }
    Ok(by_id)
}

fn insert_visual_format(
    by_visual: &mut HashMap<u32, VisualFormatInfo>,
    info: VisualFormatInfo,
) -> Result<(), Box<dyn Error>> {
    if let Some(previous) = by_visual.get(&info.visual) {
        if previous != &info {
            return Err(format!(
                "Render Visual 0x{:08x} maps to conflicting PictFormats",
                info.visual
            ).into());
        }
    } else {
        by_visual.insert(info.visual, info);
    }
    Ok(())
}

fn classify_visual_format(info: &VisualFormatInfo) -> EglPixelSemantics {
    if info.pict_type != render::PictType::DIRECT {
        return EglPixelSemantics::Unsupported;
    }
    let rgb888 = info.depth == 24
        && info.red_shift == 16 && info.red_mask == 0xff
        && info.green_shift == 8 && info.green_mask == 0xff
        && info.blue_shift == 0 && info.blue_mask == 0xff
        && info.alpha_mask == 0;
    if rgb888 {
        return EglPixelSemantics::Opaque;
    }
    let argb8888 = info.depth == 32
        && info.red_shift == 16 && info.red_mask == 0xff
        && info.green_shift == 8 && info.green_mask == 0xff
        && info.blue_shift == 0 && info.blue_mask == 0xff
        && info.alpha_shift == 24 && info.alpha_mask == 0xff;
    if argb8888 {
        return EglPixelSemantics::PremultipliedAlpha;
    }
    EglPixelSemantics::Unsupported
}

fn classify_scene_visual_format(
    info: &VisualFormatInfo,
    scene_depth: u8,
) -> EglPixelSemantics {
    if info.depth != scene_depth {
        return EglPixelSemantics::Unsupported;
    }
    classify_visual_format(info)
}

fn damage_version_compatible(major: u32, _minor: u32) -> bool {
    major >= 1
}

fn candidate_render_allowed(semantics: EglPixelSemantics, damage_active: bool) -> bool {
    semantics == EglPixelSemantics::Unsupported || damage_active
}

#[allow(dead_code)]
fn damage_monitoring_enabled(entry: &SurfaceEntry) -> bool {
    entry.backend == BackendCompatibility::Renderable
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

fn snapshot_watch_ids(snapshot: &HierarchySnapshot) -> HashSet<Window> {
    // 3A3c5b1 watches canonical root-child topology only; descendants are
    // snapshot metadata and are not recursively watched.
    snapshot
        .children
        .iter()
        .map(|binding| binding.root_child_xid)
        .collect()
}

fn root_live_event_mask(previous: EventMask) -> EventMask {
    previous | EventMask::STRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_NOTIFY
}

fn canonical_live_event_mask(previous: EventMask) -> EventMask {
    previous | EventMask::STRUCTURE_NOTIFY
}

#[cfg(test)]
fn classify_event(
    event: Event,
    root: Window,
    snapshot: &SceneSnapshot,
    ownership: Option<&CompositorOwnership>,
) -> SceneInvalidation {
    classify_event_with_registries(
        event,
        root,
        snapshot,
        ownership,
        &HashMap::new(),
        &HashMap::new(),
    )
}

fn classify_event_with_registries(
    event: Event,
    root: Window,
    snapshot: &SceneSnapshot,
    ownership: Option<&CompositorOwnership>,
    current_registry: &HashMap<damage::Damage, Window>,
    candidate_registry: &HashMap<damage::Damage, Window>,
) -> SceneInvalidation {
    match event {
        Event::SelectionClear(event)
            if ownership.is_some_and(|ownership| selection_clear_matches(&event, ownership)) =>
        {
            SceneInvalidation::Shutdown(ShutdownReason::SelectionLost)
        }
        Event::ConfigureNotify(event) if event.window == root => {
            SceneInvalidation::Shutdown(ShutdownReason::RootConfigure)
        }
        Event::ConfigureNotify(event)
            if snapshot
                .entries
                .iter()
                .any(|entry| entry.surface_xid == event.window) =>
        {
            SceneInvalidation::Geometry(event.window)
        }
        Event::ConfigureNotify(_) => SceneInvalidation::Hierarchy,
        Event::DamageNotify(event)
            if current_registry.contains_key(&event.damage)
                || candidate_registry.contains_key(&event.damage) =>
        {
            SceneInvalidation::PixelDamage(event.damage)
        }
        Event::CreateNotify(_)
        | Event::MapNotify(_)
        | Event::UnmapNotify(_)
        | Event::DestroyNotify(_)
        | Event::ReparentNotify(_)
        | Event::CirculateNotify(_) => SceneInvalidation::Hierarchy,
        Event::SelectionClear(_) => SceneInvalidation::Ignore,
        _ => SceneInvalidation::Ignore,
    }
}

fn observe_structural_generation(generation: &mut u64, invalidation: SceneInvalidation) {
    if matches!(invalidation, SceneInvalidation::Geometry(_) | SceneInvalidation::Hierarchy) {
        *generation = generation.wrapping_add(1);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum StructuralGenerationState {
    Ready(u64),
    AwaitExternalChange(u64),
}

fn structural_generation_state(
    generation: u64,
    attempted_generation: u64,
) -> StructuralGenerationState {
    if generation > attempted_generation {
        StructuralGenerationState::Ready(generation)
    } else {
        StructuralGenerationState::AwaitExternalChange(generation)
    }
}

fn bounded_batch_requires_retry(drained: usize) -> bool {
    drained == MAX_EVENTS_PER_BATCH
}

fn retry_allowed(attempt: usize) -> bool {
    attempt < MAX_CANDIDATE_RETRIES
}

fn guards_allow_retry(ownership_verified: bool, signal_pending: bool) -> bool {
    ownership_verified && !signal_pending
}

fn pixel_gate_allows_presentation(
    invalidation: SceneInvalidation,
    ownership_verified: bool,
    signal_pending: bool,
) -> bool {
    ownership_verified
        && !signal_pending
        && matches!(invalidation, SceneInvalidation::Ignore | SceneInvalidation::PixelDamage(_))
}

fn candidate_gate_decision(
    batch: SceneInvalidation,
    overflow: bool,
    ownership_verified: bool,
    signal_pending: bool,
) -> GateDecision {
    if matches!(batch, SceneInvalidation::PixelDamage(_)) && !overflow {
        if !guards_allow_retry(ownership_verified, signal_pending) {
            if !ownership_verified {
                return GateDecision::Shutdown(ShutdownReason::OwnershipLost);
            }
            return GateDecision::Shutdown(ShutdownReason::Signal);
        }
        return GateDecision::Accept;
    }
    gate_decision_after_batch(
        batch,
        overflow,
        ownership_verified,
        signal_pending,
    )
}

fn pending_work_requires_iteration(pending: &HashSet<damage::Damage>) -> bool {
    !pending.is_empty()
}

fn subtract_plan(touched: &HashSet<damage::Damage>) -> Vec<damage::Damage> {
    touched.iter().copied().collect()
}

fn gate_decision_after_batch(
    batch: SceneInvalidation,
    overflow: bool,
    ownership_verified: bool,
    signal_pending: bool,
) -> GateDecision {
    if let SceneInvalidation::Shutdown(reason) = batch {
        return GateDecision::Shutdown(reason);
    }
    if !ownership_verified {
        return GateDecision::Shutdown(ShutdownReason::OwnershipLost);
    }
    if signal_pending {
        return GateDecision::Shutdown(ShutdownReason::Signal);
    }
    if batch != SceneInvalidation::Ignore || overflow {
        return GateDecision::Retry(if batch == SceneInvalidation::Ignore {
            SceneInvalidation::Hierarchy
        } else {
            batch
        });
    }
    GateDecision::Accept
}

fn watch_plan(
    existing: &HashSet<Window>,
    desired: &HashSet<Window>,
) -> (HashSet<Window>, HashSet<Window>) {
    (
        desired.difference(existing).copied().collect(),
        existing.difference(desired).copied().collect(),
    )
}

pub(crate) fn run(
    connection: &X11Connection,
    expected_root_value: &str,
) -> Result<(), Box<dyn Error>> {
    SceneSession::run(connection, parse_root(expected_root_value)?)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    use super::{
        build_copy_plan,
        classify_event, coordinator_requires_cleanup, eligible_surface,
        is_internal_xid, root_guard, BackendCompatibility, CandidateBuildError, CopyPlan,
        PixmapGeometry, RootGeometry,
        bounded_batch_requires_retry, candidate_gate_decision, candidate_render_allowed,
        damage_monitoring_enabled, damage_version_compatible, classify_visual_format, insert_visual_format,
        render_version_compatible, gate_decision_after_batch, guards_allow_retry, GateDecision,
        pending_work_requires_iteration, pixel_gate_allows_presentation,
        retry_allowed, subtract_plan, watch_plan, build_render_quad_plan,
        egl_scene_is_renderable, merge_deferred_damage_for_registry, EglPixelSemantics,
        VisualFormatCache, VisualFormatInfo, InvalidationBatch, SceneInvalidation, SceneSnapshot,
        build_pict_format_index, classify_scene_visual_format,
        RENDER_CLIENT_MAJOR, RENDER_CLIENT_MINOR,
        root_live_event_mask, canonical_live_event_mask, snapshot_watch_ids, SceneState,
        ShutdownReason, SurfaceEntry, MAX_CANDIDATE_RETRIES, MAX_EVENTS_PER_BATCH,
        observe_structural_generation,
        structural_generation_state, StructuralGenerationState,
        classify_retired_damage_destroy, DamageDestroyClassification, DamageReleaseOutcome,
        DamageState, MetricsWindow,
    };
    use crate::x11::capture::WindowGeometry;
    use super::super::tree::{BindingStatus, HierarchyBinding, HierarchySnapshot};
    use x11rb::protocol::damage::ReportLevel;
    use x11rb::protocol::render;
    use x11rb::protocol::xproto::{EventMask, MapState, Rectangle, WindowClass};
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

    fn assert_close(actual: f32, expected: f32) {
        assert!((actual - expected).abs() < 0.0001, "{actual} != {expected}");
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

    fn visual_info(
        visual: u32,
        depth: u8,
        pict_type: render::PictType,
        red_shift: u16,
        red_mask: u16,
        green_shift: u16,
        green_mask: u16,
        blue_shift: u16,
        blue_mask: u16,
        alpha_shift: u16,
        alpha_mask: u16,
    ) -> VisualFormatInfo {
        VisualFormatInfo {
            visual,
            depth,
            pict_format: 42_u32.into(),
            pict_type,
            red_shift,
            red_mask,
            green_shift,
            green_mask,
            blue_shift,
            blue_mask,
            alpha_shift,
            alpha_mask,
        }
    }

    fn rgb888_info() -> VisualFormatInfo {
        visual_info(0x21, 24, render::PictType::DIRECT, 16, 0xff, 8, 0xff, 0, 0xff, 0, 0)
    }

    fn argb8888_info() -> VisualFormatInfo {
        visual_info(0x42, 32, render::PictType::DIRECT, 16, 0xff, 8, 0xff, 0, 0xff, 24, 0xff)
    }

    fn pict_format_info(
        id: u32,
        depth: u8,
        pict_type: render::PictType,
        direct: render::Directformat,
    ) -> render::Pictforminfo {
        render::Pictforminfo {
            id: id.into(),
            type_: pict_type,
            depth,
            direct,
            colormap: 0,
        }
    }

    fn pict_reply(
        format: render::Pictforminfo,
        pict_depth: u8,
        visual: u32,
    ) -> render::QueryPictFormatsReply {
        render::QueryPictFormatsReply {
            formats: vec![format],
            screens: vec![render::Pictscreen {
                fallback: 0,
                depths: vec![render::Pictdepth {
                    depth: pict_depth,
                    visuals: vec![render::Pictvisual {
                        visual,
                        format: 42,
                    }],
                }],
            }],
            ..Default::default()
        }
    }

    fn damage_event(damage: u32) -> Event {
        Event::DamageNotify(x11rb::protocol::damage::NotifyEvent {
            response_type: 0,
            level: ReportLevel::NON_EMPTY,
            sequence: 0,
            drawable: 10,
            damage,
            timestamp: 0,
            area: Rectangle { x: 0, y: 0, width: 1, height: 1 },
            geometry: Rectangle { x: 0, y: 0, width: 20, height: 20 },
        })
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
        let unsupported = eligible_surface(&unsupported_depth, None, root(), 10, 0).unwrap();
        assert_eq!(unsupported.backend, BackendCompatibility::BackendUnsupported);
    }

    #[test]
    fn empty_scene_snapshot_is_valid() {
        let snapshot = SceneSnapshot {
            root: 1,
            root_geometry: root(),
            entries: Vec::new(),
        };
        assert!(snapshot.entries.is_empty());
    }

    #[test]
    fn backend_unsupported_only_scene_remains_canonical() {
        let mut unsupported = metadata();
        unsupported.depth = 32;
        let entry = eligible_surface(&unsupported, None, root(), 10, 0).unwrap();
        let snapshot = SceneSnapshot {
            root: 1,
            root_geometry: root(),
            entries: vec![entry],
        };
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].surface_xid, 10);
        assert_eq!(snapshot.entries[0].backend, BackendCompatibility::BackendUnsupported);
    }

    #[test]
    fn empty_scene_has_no_source_copy_operations() {
        let snapshot = SceneSnapshot {
            root: 1,
            root_geometry: root(),
            entries: Vec::new(),
        };
        let source_copy_count = snapshot
            .entries
            .iter()
            .filter(|entry| entry.backend == BackendCompatibility::Renderable)
            .count();
        assert_eq!(source_copy_count, 0);
    }

    #[test]
    fn empty_scene_keeps_structural_guards_in_force() {
        assert!(guards_allow_retry(true, false));
        assert!(!guards_allow_retry(false, false));
        assert!(!guards_allow_retry(true, true));
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
        let snapshot = SceneSnapshot {
            root: 1,
            root_geometry: root(),
            entries: Vec::new(),
        };
        assert_eq!(
            classify_event(Event::ConfigureNotify(configure), 1, &snapshot, None),
            SceneInvalidation::Shutdown(ShutdownReason::RootConfigure)
        );
        assert_eq!(
            classify_event(Event::ConfigureNotify(configure), 2, &snapshot, None),
            SceneInvalidation::Hierarchy
        );

    }

    #[test]
    fn structural_events_are_hierarchy_invalidations() {
        let snapshot = SceneSnapshot {
            root: 1,
            root_geometry: root(),
            entries: Vec::new(),
        };
        let events = [
            Event::CreateNotify(x11rb::protocol::xproto::CreateNotifyEvent {
                response_type: 0, sequence: 0, parent: 1, window: 2,
                x: 0, y: 0, width: 1, height: 1, border_width: 0,
                override_redirect: false,
            }),
        ];
        for event in events {
            assert_eq!(
                classify_event(event, 1, &snapshot, None),
                SceneInvalidation::Hierarchy
            );
        }
    }

    #[test]
    fn shutdown_dominates_batch_and_hierarchy_dominates_geometry() {
        let mut batch = InvalidationBatch::default();
        batch.push(SceneInvalidation::Geometry(10));
        batch.push(SceneInvalidation::Hierarchy);
        assert_eq!(batch.decision(), SceneInvalidation::Hierarchy);
        batch.push(SceneInvalidation::Shutdown(ShutdownReason::RootConfigure));
        assert_eq!(
            batch.decision(),
            SceneInvalidation::Shutdown(ShutdownReason::RootConfigure)
        );
    }

    #[test]
    fn current_damage_notify_resolves_to_pixel_damage() {
        let snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: Vec::new() };
        let registry = HashMap::from([(42_u32, 10_u32)]);
        assert_eq!(
            super::classify_event_with_registries(
                damage_event(42), 1, &snapshot, None, &registry, &HashMap::new()
            ),
            SceneInvalidation::PixelDamage(42)
        );
    }

    #[test]
    fn stale_and_unknown_damage_notify_are_ignored() {
        let snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: Vec::new() };
        let registry = HashMap::from([(42_u32, 10_u32)]);
        assert_eq!(
            super::classify_event_with_registries(
                damage_event(41), 1, &snapshot, None, &registry, &HashMap::new()
            ),
            SceneInvalidation::Ignore
        );
    }

    #[test]
    fn damage_id_resolution_never_uses_semantic_client() {
        let snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: Vec::new() };
        let registry = HashMap::from([(42_u32, 10_u32)]);
        assert_eq!(
            super::classify_event_with_registries(
                damage_event(20), 1, &snapshot, None, &registry, &HashMap::new()
            ),
            SceneInvalidation::Ignore
        );
    }

    #[test]
    fn damage_batch_deduplicates_touched_leases() {
        let mut batch = InvalidationBatch::default();
        batch.push(SceneInvalidation::PixelDamage(42));
        batch.push(SceneInvalidation::PixelDamage(42));
        assert_eq!(batch.pixel_damage().len(), 1);
    }

    #[test]
    fn subtract_plan_has_one_operation_per_current_damage_id() {
        let touched = HashSet::from([41_u32, 42_u32, 42_u32]);
        let plan = subtract_plan(&touched);
        assert_eq!(plan.len(), 2);
        assert!(plan.contains(&41));
        assert!(plan.contains(&42));
    }

    #[test]
    fn structural_dominance_hides_pixel_damage_without_dropping_batch_data() {
        let mut batch = InvalidationBatch::default();
        batch.push(SceneInvalidation::PixelDamage(42));
        batch.push(SceneInvalidation::Geometry(10));
        assert_eq!(batch.decision(), SceneInvalidation::Geometry(10));
        assert!(batch.pixel_damage().contains(&42));
        batch.push(SceneInvalidation::Hierarchy);
        assert_eq!(batch.decision(), SceneInvalidation::Hierarchy);
        batch.push(SceneInvalidation::Shutdown(ShutdownReason::Signal));
        assert_eq!(batch.decision(), SceneInvalidation::Shutdown(ShutdownReason::Signal));
    }

    #[test]
    fn candidate_pixel_gate_accepts_without_retry() {
        assert_eq!(
            candidate_gate_decision(SceneInvalidation::PixelDamage(42), false, true, false),
            GateDecision::Accept
        );
        assert_eq!(
            candidate_gate_decision(SceneInvalidation::PixelDamage(42), false, true, true),
            GateDecision::Shutdown(ShutdownReason::Signal)
        );
    }

    #[test]
    fn pixel_damage_gate_does_not_block_presentation() {
        assert!(pixel_gate_allows_presentation(
            SceneInvalidation::PixelDamage(42), true, false
        ));
        assert!(!pixel_gate_allows_presentation(
            SceneInvalidation::Hierarchy, true, false
        ));
        assert!(!pixel_gate_allows_presentation(
            SceneInvalidation::PixelDamage(42), true, true
        ));
    }

    #[test]
    fn pending_damage_requires_immediate_iteration() {
        assert!(pending_work_requires_iteration(&HashSet::from([42_u32])));
        assert!(!pending_work_requires_iteration(&HashSet::new()));
    }

    #[test]
    fn damage_version_policy_accepts_one_zero_and_newer() {
        assert!(damage_version_compatible(1, 0));
        assert!(damage_version_compatible(1, 1));
        assert!(damage_version_compatible(2, 0));
        assert!(!damage_version_compatible(0, 9));
    }

    #[test]
    fn active_retired_damage_destroy_success_is_released() {
        assert_eq!(
            classify_retired_damage_destroy(
                true,
                Ok(()),
            ),
            Ok(DamageReleaseOutcome::Released)
        );
    }

    #[test]
    fn removed_retired_damage_bad_damage_is_already_gone() {
        assert_eq!(
            classify_retired_damage_destroy(
                true,
                Err(DamageDestroyClassification::BadDamage),
            ),
            Ok(DamageReleaseOutcome::AlreadyGone)
        );
    }

    #[test]
    fn survivor_retired_damage_bad_damage_is_fatal() {
        assert_eq!(
            classify_retired_damage_destroy(
                false,
                Err(DamageDestroyClassification::BadDamage),
            ),
            Err(DamageDestroyClassification::BadDamage)
        );
    }

    #[test]
    fn removed_retired_damage_other_error_is_fatal() {
        assert_eq!(
            classify_retired_damage_destroy(
                true,
                Err(DamageDestroyClassification::OtherError),
            ),
            Err(DamageDestroyClassification::OtherError)
        );
    }

    #[test]
    fn already_gone_damage_state_is_terminal_for_drop() {
        assert_ne!(DamageState::AlreadyGone, DamageState::Active);
        assert_ne!(DamageState::Released, DamageState::Active);
        assert_ne!(DamageState::Disarmed, DamageState::Active);
    }

    #[test]
    fn render_query_version_requests_011_and_accepts_base_and_newer_minor() {
        assert_eq!((RENDER_CLIENT_MAJOR, RENDER_CLIENT_MINOR), (0, 11));
        assert!(render_version_compatible(0, 0));
        assert!(render_version_compatible(0, 11));
        assert!(render_version_compatible(0, 12));
        assert!(!render_version_compatible(1, 0));
    }

    #[test]
    fn identical_visual_mapping_is_deduplicated_but_conflict_is_rejected() {
        let info = rgb888_info();
        let mut cache = HashMap::new();
        insert_visual_format(&mut cache, info).unwrap();
        insert_visual_format(&mut cache, info).unwrap();
        let mut conflict = info;
        conflict.pict_format = 43_u32.into();
        assert!(insert_visual_format(&mut cache, conflict).is_err());
    }

    #[test]
    fn exact_rgb888_is_opaque_and_exact_argb8888_is_premultiplied() {
        assert_eq!(classify_visual_format(&rgb888_info()), EglPixelSemantics::Opaque);
        assert_eq!(classify_visual_format(&argb8888_info()), EglPixelSemantics::PremultipliedAlpha);
    }

    #[test]
    fn depth_alone_does_not_imply_argb() {
        let mut info = argb8888_info();
        info.alpha_mask = 0;
        assert_eq!(classify_visual_format(&info), EglPixelSemantics::Unsupported);
    }

    #[test]
    fn unsupported_depth32_layouts_are_rejected() {
        let mut abgr = argb8888_info();
        abgr.red_shift = 0;
        abgr.blue_shift = 16;
        assert_eq!(classify_visual_format(&abgr), EglPixelSemantics::Unsupported);

        let mut ten_bit = argb8888_info();
        ten_bit.red_mask = 0x3ff;
        assert_eq!(classify_visual_format(&ten_bit), EglPixelSemantics::Unsupported);

        let mut indexed = argb8888_info();
        indexed.pict_type = render::PictType::INDEXED;
        assert_eq!(classify_visual_format(&indexed), EglPixelSemantics::Unsupported);
    }

    #[test]
    fn source_visual_and_output_visual_are_independent() {
        let info = argb8888_info();
        assert_ne!(info.visual, root().visual);
        assert_eq!(classify_visual_format(&info), EglPixelSemantics::PremultipliedAlpha);
    }

    #[test]
    fn pict_format_index_deduplicates_identical_and_rejects_conflicting_ids() {
        let direct = render::Directformat {
            red_shift: 16, red_mask: 0xff,
            green_shift: 8, green_mask: 0xff,
            blue_shift: 0, blue_mask: 0xff,
            alpha_shift: 0, alpha_mask: 0,
        };
        let format = pict_format_info(42, 24, render::PictType::DIRECT, direct);
        assert_eq!(build_pict_format_index(&[format, format]).unwrap().len(), 1);
        let conflicting = pict_format_info(42, 32, render::PictType::DIRECT, direct);
        assert!(build_pict_format_index(&[format, conflicting]).is_err());
    }

    #[test]
    fn pict_visual_format_resolution_validates_missing_id_and_depth() {
        let direct = render::Directformat {
            red_shift: 16, red_mask: 0xff,
            green_shift: 8, green_mask: 0xff,
            blue_shift: 0, blue_mask: 0xff,
            alpha_shift: 0, alpha_mask: 0,
        };
        let format = pict_format_info(42, 24, render::PictType::DIRECT, direct);
        assert!(VisualFormatCache::from_reply(&pict_reply(format, 24, 7)).is_ok());
        assert!(VisualFormatCache::from_reply(&pict_reply(format, 32, 7)).is_err());

        let mut missing = pict_reply(format, 24, 7);
        missing.screens[0].depths[0].visuals[0].format = 99;
        assert!(VisualFormatCache::from_reply(&missing).is_err());
    }

    #[test]
    fn scene_entry_depth_mismatch_is_unsupported_before_import() {
        let info = argb8888_info();
        assert_eq!(
            classify_scene_visual_format(&info, 24),
            EglPixelSemantics::Unsupported
        );
        assert_eq!(
            classify_scene_visual_format(&info, 32),
            EglPixelSemantics::PremultipliedAlpha
        );
    }

    #[test]
    fn egl_import_policy_is_decided_from_source_format_before_import() {
        assert_eq!(classify_visual_format(&rgb888_info()), EglPixelSemantics::Opaque);
        assert_eq!(classify_visual_format(&argb8888_info()), EglPixelSemantics::PremultipliedAlpha);
        assert_eq!(classify_visual_format(&visual_info(
            0x21, 24, render::PictType::DIRECT, 0, 0, 0, 0, 0, 0, 0, 0
        )), EglPixelSemantics::Unsupported);
    }

    #[test]
    fn egl_capability_does_not_depend_on_copyarea_backend_classification() {
        let info = rgb888_info();
        assert_eq!(classify_visual_format(&info), EglPixelSemantics::Opaque);
        let mut entry = eligible_surface(&metadata(), None, root(), 10, 0).unwrap();
        entry.backend = BackendCompatibility::BackendUnsupported;
        assert_eq!(entry.backend, BackendCompatibility::BackendUnsupported);
        assert_eq!(classify_visual_format(&info), EglPixelSemantics::Opaque);
    }

    #[test]
    fn backend_unsupported_has_no_pixel_monitoring_subscription() {
        let mut unsupported = metadata();
        unsupported.depth = 32;
        let entry = eligible_surface(&unsupported, None, root(), 10, 0).unwrap();
        assert!(!damage_monitoring_enabled(&entry));
        let renderable = eligible_surface(&metadata(), None, root(), 10, 0).unwrap();
        assert!(damage_monitoring_enabled(&renderable));
    }

    #[test]
    fn candidate_damage_is_active_before_first_render() {
        assert!(candidate_render_allowed(
            EglPixelSemantics::PremultipliedAlpha,
            true
        ));
        assert!(!candidate_render_allowed(
            EglPixelSemantics::PremultipliedAlpha,
            false
        ));
        assert!(candidate_render_allowed(
            EglPixelSemantics::Unsupported,
            false
        ));
    }

    #[test]
    fn current_scene_pixel_path_does_not_change_identity_policy() {
        let mut entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        entry.semantic_client_xid = Some(99);
        assert_eq!(entry.surface_xid, 10);
        assert_eq!(entry.lifecycle_xid, 10);
    }

    #[test]
    fn ready_generation_is_processed_before_blocking() {
        assert!(matches!(
            structural_generation_state(10, 9),
            StructuralGenerationState::Ready(10)
        ));
    }

    #[test]
    fn same_generation_enters_await_without_immediate_spin() {
        assert!(matches!(
            structural_generation_state(10, 10),
            StructuralGenerationState::AwaitExternalChange(10)
        ));
    }

    #[test]
    fn newer_generation_becomes_ready_after_structural_event() {
        assert!(matches!(
            structural_generation_state(11, 10),
            StructuralGenerationState::Ready(11)
        ));
    }

    #[test]
    fn structural_event_advances_generation() {
        let mut generation = 10;
        observe_structural_generation(&mut generation, SceneInvalidation::Hierarchy);
        assert_eq!(generation, 11);
    }

    #[test]
    fn geometry_event_advances_generation() {
        let mut generation = 10;
        observe_structural_generation(&mut generation, SceneInvalidation::Geometry(7));
        assert_eq!(generation, 11);
    }

    #[test]
    fn pixel_event_does_not_create_structural_work() {
        let mut generation = 10;
        observe_structural_generation(&mut generation, SceneInvalidation::PixelDamage(7));
        assert_eq!(generation, 10);
    }

    #[test]
    fn deferred_attempt_is_not_ready_again_without_new_generation() {
        let generation = 10;
        let attempted = generation;
        assert!(matches!(
            structural_generation_state(generation, attempted),
            StructuralGenerationState::AwaitExternalChange(10)
        ));
        assert!(matches!(
            structural_generation_state(generation + 1, attempted),
            StructuralGenerationState::Ready(11)
        ));
    }

    #[test]
    fn two_structural_generations_provide_two_bounded_opportunities() {
        let mut generation = 10;
        let mut attempted = 9;
        assert!(matches!(
            structural_generation_state(generation, attempted),
            StructuralGenerationState::Ready(10)
        ));
        attempted = generation;
        assert!(matches!(
            structural_generation_state(generation, attempted),
            StructuralGenerationState::AwaitExternalChange(10)
        ));
        observe_structural_generation(&mut generation, SceneInvalidation::Hierarchy);
        assert!(matches!(
            structural_generation_state(generation, attempted),
            StructuralGenerationState::Ready(11)
        ));
    }

    #[test]
    fn generation_state_models_ready_and_await_transitions() {
        assert_eq!(
            structural_generation_state(10, 9),
            StructuralGenerationState::Ready(10)
        );
        assert_eq!(
            structural_generation_state(10, 10),
            StructuralGenerationState::AwaitExternalChange(10)
        );
        assert_eq!(
            structural_generation_state(11, 10),
            StructuralGenerationState::Ready(11)
        );
    }

    #[test]
    fn stale_root_child_is_typed_transient() {
        let binding = HierarchyBinding {
            root_child_xid: 10,
            semantic_client_xids: Vec::new(),
            semantic_client: BindingStatus::NoClient,
            lifecycle_candidate_xid: 10,
            surface_candidate: None,
            descendants: Vec::new(),
            stale: true,
        };
        let hierarchy = HierarchySnapshot { root: 1, children: vec![binding] };
        let error = SceneSnapshot::from_hierarchy(
            hierarchy,
            root(),
            99,
            100,
        )
        .expect_err("missing surface metadata must be stale");
        assert!(matches!(
            error.downcast_ref::<CandidateBuildError>(),
            Some(CandidateBuildError::Stale(SceneInvalidation::Hierarchy))
        ));
    }

    #[test]
    fn bounded_batch_marks_retry_without_consuming_overflow() {
        assert!(!bounded_batch_requires_retry(MAX_EVENTS_PER_BATCH - 1));
        assert!(bounded_batch_requires_retry(MAX_EVENTS_PER_BATCH));
    }

    #[test]
    fn gate_shutdown_and_guards_prevent_retry() {
        assert_eq!(
            gate_decision_after_batch(
                SceneInvalidation::Shutdown(ShutdownReason::SelectionLost),
                false,
                true,
                false,
            ),
            GateDecision::Shutdown(ShutdownReason::SelectionLost)
        );
        assert_eq!(
            gate_decision_after_batch(SceneInvalidation::Hierarchy, false, true, true),
            GateDecision::Shutdown(ShutdownReason::Signal)
        );
        assert_eq!(
            gate_decision_after_batch(SceneInvalidation::Geometry(10), false, false, false),
            GateDecision::Shutdown(ShutdownReason::OwnershipLost)
        );
    }

    #[test]
    fn retry_policy_is_bounded_and_stale_never_accepts() {
        assert_eq!(MAX_CANDIDATE_RETRIES, 1);
        assert!(retry_allowed(0));
        assert!(!retry_allowed(1));
        assert_ne!(
            gate_decision_after_batch(SceneInvalidation::Hierarchy, false, true, false),
            GateDecision::Accept
        );
    }

    #[test]
    fn candidate_watch_plan_has_additions_and_obsolete_sets() {
        let existing = HashSet::from([1, 2]);
        let desired = HashSet::from([2, 3]);
        let (additions, obsolete) = watch_plan(&existing, &desired);
        assert_eq!(additions, HashSet::from([3]));
        assert_eq!(obsolete, HashSet::from([1]));
    }

    #[test]
    fn guards_require_ownership_and_no_pending_signal() {
        assert!(guards_allow_retry(true, false));
        assert!(!guards_allow_retry(false, false));
        assert!(!guards_allow_retry(true, true));
    }

    #[test]
    fn live_masks_cover_root_and_canonical_surface_only() {
        let root_mask = root_live_event_mask(EventMask::NO_EVENT);
        assert!(root_mask.contains(EventMask::STRUCTURE_NOTIFY));
        assert!(root_mask.contains(EventMask::SUBSTRUCTURE_NOTIFY));
        let canonical_mask = canonical_live_event_mask(EventMask::NO_EVENT);
        assert!(canonical_mask.contains(EventMask::STRUCTURE_NOTIFY));
        assert!(!canonical_mask.contains(EventMask::SUBSTRUCTURE_NOTIFY));
    }

    #[test]
    fn live_watch_plan_excludes_snapshot_descendants() {
        let binding = HierarchyBinding {
            root_child_xid: 10,
            semantic_client_xids: vec![30],
            semantic_client: BindingStatus::SingleClient(30),
            lifecycle_candidate_xid: 10,
            surface_candidate: Some(metadata()),
            descendants: vec![metadata()],
            stale: false,
        };
        let snapshot = HierarchySnapshot {
            root: 1,
            children: vec![binding],
        };
        assert_eq!(snapshot_watch_ids(&snapshot), HashSet::from([10]));
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

    #[test]
    fn render_plan_inside_border_zero_maps_full_texture() {
        let plan = build_render_quad_plan(window(10, 12, 20, 15, 0), pixmap(20, 15), root()).unwrap();
        assert_eq!((plan.dst_x, plan.dst_y, plan.width, plan.height), (10, 12, 20, 15));
        assert_eq!((plan.src_x, plan.src_y, plan.src_width, plan.src_height), (0, 0, 20, 15));
        assert_close(plan.u0, 0.0);
        assert_close(plan.v0, 0.0);
        assert_close(plan.u1, 1.0);
        assert_close(plan.v1, 1.0);
    }

    #[test]
    fn render_plan_border_maps_named_pixmap_without_stretching() {
        let mut large_root = root();
        large_root.width = 1000;
        large_root.height = 800;
        let plan = build_render_quad_plan(
            window(300, 250, 500, 350, 1),
            pixmap(502, 352),
            large_root,
        ).unwrap();
        assert_eq!((plan.dst_x, plan.dst_y, plan.width, plan.height), (299, 249, 502, 352));
        assert_eq!((plan.src_x, plan.src_y, plan.src_width, plan.src_height), (0, 0, 502, 352));
        assert_close(plan.u1, 1.0);
        assert_close(plan.v1, 1.0);
    }

    #[test]
    fn render_plan_clips_left_and_adjusts_uv() {
        let plan = build_render_quad_plan(window(-20, 10, 50, 20, 0), pixmap(50, 20), root()).unwrap();
        assert_eq!((plan.dst_x, plan.width, plan.src_x, plan.src_width), (0, 30, 20, 30));
        assert_close(plan.u0, 0.4);
        assert_close(plan.u1, 1.0);
    }

    #[test]
    fn render_plan_clips_top_right_bottom_and_corner() {
        let top = build_render_quad_plan(window(10, -5, 20, 15, 0), pixmap(20, 15), root()).unwrap();
        assert_eq!((top.dst_y, top.height, top.src_y, top.src_height), (0, 10, 5, 10));
        assert_close(top.v0, 5.0 / 15.0);

        let right = build_render_quad_plan(window(90, 10, 20, 15, 0), pixmap(20, 15), root()).unwrap();
        assert_eq!((right.dst_x, right.width, right.src_width), (90, 10, 10));
        assert_close(right.u1, 0.5);

        let bottom = build_render_quad_plan(window(10, 70, 20, 15, 0), pixmap(20, 15), root()).unwrap();
        assert_eq!((bottom.dst_y, bottom.height, bottom.src_height), (70, 10, 10));
        assert_close(bottom.v1, 10.0 / 15.0);

        let corner = build_render_quad_plan(window(-5, -5, 20, 15, 0), pixmap(20, 15), root()).unwrap();
        assert_eq!((corner.src_x, corner.src_y, corner.width, corner.height), (5, 5, 15, 10));
        assert_close(corner.u0, 0.25);
        assert_close(corner.v0, 5.0 / 15.0);
    }

    #[test]
    fn render_plan_fully_outside_has_no_draw() {
        assert!(build_render_quad_plan(window(-30, 0, 10, 10, 0), pixmap(10, 10), root()).is_none());
    }

    #[test]
    fn render_plan_keeps_single_y_flip_policy() {
        let plan = build_render_quad_plan(window(10, 10, 20, 15, 0), pixmap(20, 15), root()).unwrap();
        assert_close(plan.v0, 0.0);
        assert_close(plan.v1, 1.0);
    }

    #[test]
    fn empty_scene_is_valid_but_nonempty_without_egl_surfaces_is_not() {
        assert!(egl_scene_is_renderable(0, 0));
        assert!(!egl_scene_is_renderable(2, 0));
        assert!(egl_scene_is_renderable(3, 2));
    }

    #[test]
    fn candidate_pending_damage_transfers_only_on_commit() {
        let mut pending = HashSet::new();
        let candidate_registry = HashMap::from([(77_u32, 10_u32)]);
        merge_deferred_damage_for_registry(&mut pending, HashSet::from([77]), &candidate_registry);
        assert!(pending.contains(&77));

        let old_registry = HashMap::new();
        merge_deferred_damage_for_registry(&mut pending, HashSet::from([77]), &old_registry);
        assert!(!pending.contains(&77));
    }

    #[test]
    fn gate_decisions_allow_swap_only_for_clean_or_pixel_batches() {
        assert!(pixel_gate_allows_presentation(SceneInvalidation::Ignore, true, false));
        assert!(pixel_gate_allows_presentation(SceneInvalidation::PixelDamage(1), true, false));
        assert!(!pixel_gate_allows_presentation(SceneInvalidation::Hierarchy, true, false));
        assert!(!pixel_gate_allows_presentation(SceneInvalidation::Geometry(1), true, false));
        assert!(!pixel_gate_allows_presentation(
            SceneInvalidation::Shutdown(ShutdownReason::Signal), true, false
        ));
        assert!(!pixel_gate_allows_presentation(SceneInvalidation::Ignore, false, false));
        assert!(!pixel_gate_allows_presentation(SceneInvalidation::Ignore, true, true));
    }

    #[test]
    fn metrics_aggregate_counts() {
        let start = Instant::now();
        let mut metrics = MetricsWindow::new_at(start);
        metrics.record_damage_event();
        metrics.record_first_damage(start);
        metrics.record_damage_event();
        metrics.record_first_damage(start + Duration::from_millis(1));
        metrics.record_pixel_batch(2);
        metrics.record_render();
        metrics.record_import();
        metrics.record_structural_rebuild();
        metrics.record_stale_retry();
        metrics.record_stale_deferred();
        metrics.record_swap(
            Duration::from_micros(10),
            Duration::from_micros(20),
            start + Duration::from_millis(2),
            true,
            true,
        );
        let line = metrics
            .report_if_due(start + Duration::from_secs(1))
            .unwrap();
        assert!(line.contains("damage_events=2"));
        assert!(line.contains("pixel_batches=1"));
        assert!(line.contains("damaged_surfaces_max=2"));
        assert!(line.contains("renders=1"));
        assert!(line.contains("swaps=1"));
        assert!(line.contains("structural_rebuilds=1"));
        assert!(line.contains("egl_imports=1"));
        assert!(line.contains("stale_retry=1"));
        assert!(line.contains("stale_deferred=1"));
    }

    #[test]
    fn metrics_report_resets_window() {
        let start = Instant::now();
        let mut metrics = MetricsWindow::new_at(start);
        metrics.record_render();
        assert!(metrics.report_if_due(start + Duration::from_millis(999)).is_none());
        assert!(metrics.report_if_due(start + Duration::from_secs(1)).is_some());
        let next = start + Duration::from_secs(2);
        let line = metrics.report_if_due(next).unwrap();
        assert!(line.contains("renders=0"));
        assert!(line.contains("swaps=0"));
    }

    #[test]
    fn metrics_compute_average_and_max_swap_duration() {
        let start = Instant::now();
        let mut metrics = MetricsWindow::new_at(start);
        metrics.record_swap(
            Duration::from_micros(10),
            Duration::from_micros(30),
            start,
            false,
            true,
        );
        metrics.record_swap(
            Duration::from_micros(20),
            Duration::from_micros(50),
            start,
            false,
            true,
        );
        let line = metrics
            .report_if_due(start + Duration::from_secs(1))
            .unwrap();
        assert!(line.contains("swap_avg_us=15"));
        assert!(line.contains("swap_max_us=20"));
        assert!(line.contains("render_to_swap_avg_us=40"));
        assert!(line.contains("render_to_swap_max_us=50"));
    }

    #[test]
    fn metrics_preserve_first_damage_timestamp_across_coalescing() {
        let start = Instant::now();
        let mut metrics = MetricsWindow::new_at(start);
        metrics.record_damage_event();
        metrics.record_first_damage(start);
        metrics.record_damage_event();
        metrics.record_first_damage(start + Duration::from_micros(10));
        metrics.record_pixel_batch(1);
        metrics.record_swap(
            Duration::from_micros(1),
            Duration::from_micros(2),
            start + Duration::from_micros(20),
            true,
            true,
        );
        let line = metrics
            .report_if_due(start + Duration::from_secs(1))
            .unwrap();
        assert!(line.contains("damage_to_present_avg_us=20"));
        assert!(line.contains("damage_to_present_max_us=20"));
    }

    #[test]
    fn metrics_do_not_divide_by_zero() {
        let start = Instant::now();
        let mut metrics = MetricsWindow::new_at(start);
        let line = metrics
            .report_if_due(start + Duration::from_secs(1))
            .unwrap();
        assert!(line.contains("damaged_surfaces_avg=0"));
        assert!(line.contains("swap_avg_us=0"));
        assert!(line.contains("damage_to_present_avg_us=0"));
        assert!(!line.contains("NaN"));
    }

    #[test]
    fn structural_only_swap_has_no_damage_latency() {
        let start = Instant::now();
        let mut metrics = MetricsWindow::new_at(start);
        metrics.record_swap(
            Duration::from_micros(5),
            Duration::from_micros(15),
            start + Duration::from_micros(15),
            false,
            true,
        );
        let line = metrics
            .report_if_due(start + Duration::from_secs(1))
            .unwrap();
        assert!(line.contains("damage_to_present_avg_us=0"));
        assert!(line.contains("damage_to_present_max_us=0"));
    }

    #[test]
    fn metrics_reset_does_not_leak_counters_between_windows() {
        let start = Instant::now();
        let mut metrics = MetricsWindow::new_at(start);
        metrics.record_damage_event();
        metrics.record_first_damage(start);
        metrics.record_pixel_batch(3);
        let _ = metrics.report_if_due(start + Duration::from_secs(1));
        metrics.record_pixel_batch(1);
        let line = metrics
            .report_if_due(start + Duration::from_secs(2))
            .unwrap();
        assert!(line.contains("damage_events=0"));
        assert!(line.contains("pixel_batches=1"));
        assert!(line.contains("damaged_surfaces_avg=1"));
        assert!(line.contains("damaged_surfaces_max=1"));
    }
}
