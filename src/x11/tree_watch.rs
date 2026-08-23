use std::collections::{HashMap, HashSet};
use std::error::Error;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{
    ChangeWindowAttributesAux, ConnectionExt, EventMask, MapState, Window,
};
use x11rb::protocol::Event;

use super::capture::{is_bad_window_error, WindowGeometry};
use super::connection::X11Connection;
use super::tree::{BindingStatus, HierarchySnapshot};

#[derive(Clone, Debug, Eq, PartialEq)]
struct BindingFingerprint {
    semantic_client: BindingStatus,
    map_state: Option<MapState>,
    geometry: Option<WindowGeometry>,
    stale: bool,
}

#[derive(Debug, Eq, PartialEq)]
enum RegistryChange {
    RootChildAdded(Window),
    RootChildRemoved(Window),
    StackingChanged,
    SemanticBindingChanged {
        window: Window,
        previous: BindingStatus,
        current: BindingStatus,
    },
    MapStateChanged {
        window: Window,
        previous: Option<MapState>,
        current: Option<MapState>,
    },
    GeometryChanged { window: Window },
    StaleChanged {
        window: Window,
        previous: bool,
        current: bool,
    },
}

struct HierarchyRegistry {
    snapshot: HierarchySnapshot,
    revision: u64,
    watched_windows: HashSet<Window>,
    root: Window,
    root_selected: bool,
}

impl HierarchyRegistry {
    fn new(snapshot: HierarchySnapshot, root: Window) -> Self {
        Self {
            snapshot,
            revision: 0,
            watched_windows: HashSet::new(),
            root,
            root_selected: false,
        }
    }

    fn replace_snapshot(&mut self, snapshot: HierarchySnapshot) -> Vec<RegistryChange> {
        let changes = snapshot_delta(&self.snapshot, &snapshot);
        self.snapshot = snapshot;
        self.revision += 1;
        changes
    }

    fn cleanup(&mut self, connection: &X11Connection) -> Result<(), Box<dyn Error>> {
        let mut cleanup_error = None;
        let watched = self.watched_windows.iter().copied().collect::<Vec<_>>();
        for window in watched {
            match clear_structure_notify(connection, window) {
                Ok(()) => {
                    self.watched_windows.remove(&window);
                }
                Err(error) if is_bad_window_error(error.as_ref()) => {
                    self.watched_windows.remove(&window);
                }
                Err(error) => {
                    if cleanup_error.is_none() {
                        cleanup_error = Some(error);
                    }
                }
            }
        }
        if self.root_selected {
            if let Err(error) = clear_structure_notify(connection, self.root) {
                if !is_bad_window_error(error.as_ref()) && cleanup_error.is_none() {
                    cleanup_error = Some(error);
                }
            }
            self.root_selected = false;
        }
        if let Err(error) = connection.inner.flush() {
            if cleanup_error.is_none() {
                cleanup_error = Some(error.into());
            }
        }
        match cleanup_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

pub(crate) fn run(connection: &X11Connection) -> Result<(), Box<dyn Error>> {
    let root = connection.inner.setup().roots[connection.screen_num()].root;
    let mut registry = HierarchyRegistry::new(
        HierarchySnapshot {
            root,
            children: Vec::new(),
        },
        root,
    );
    let result = run_loop(connection, &mut registry);
    if let Err(error) = result {
        if let Err(cleanup_error) = registry.cleanup(connection) {
            eprintln!("tree watch cleanup failed: {cleanup_error}");
        }
        return Err(error);
    }
    registry.cleanup(connection)
}

fn run_loop(
    connection: &X11Connection,
    registry: &mut HierarchyRegistry,
) -> Result<(), Box<dyn Error>> {
    select_substructure_notify(connection, registry.root)?;
    registry.root_selected = true;

    let first_snapshot = connection.snapshot_hierarchy()?;
    reconcile_watch_set(connection, registry, &first_snapshot)?;

    let initial_snapshot = connection.snapshot_hierarchy()?;
    reconcile_watch_set(connection, registry, &initial_snapshot)?;
    registry.snapshot = initial_snapshot;

    println!("X11 hierarchy watch");
    println!("root: 0x{:08x}", registry.root);
    println!("initial revision: {}", registry.revision);
    println!("root children: {}", registry.snapshot.children.len());
    println!("waiting for structural events...");

    loop {
        let first_event = connection.inner.wait_for_event()?;
        let mut reasons = Vec::new();
        if let Some(reason) = structural_event_reason(&first_event) {
            reasons.push(reason);
        }

        while let Some(event) = connection.inner.poll_for_event()? {
            if let Some(reason) = structural_event_reason(&event) {
                reasons.push(reason);
            }
        }

        if reasons.is_empty() {
            continue;
        }
        println!("\nevents:");
        for reason in reasons {
            println!("  {reason}");
        }

        let snapshot = connection.snapshot_hierarchy()?;
        reconcile_watch_set(connection, registry, &snapshot)?;
        let changes = registry.replace_snapshot(snapshot);
        println!("\nrevision: {}", registry.revision);
        if changes.is_empty() {
            println!("changed: none");
        } else {
            println!("changed:");
            for change in changes {
                print_change(&change);
            }
        }
    }
}

fn reconcile_watch_set(
    connection: &X11Connection,
    registry: &mut HierarchyRegistry,
    snapshot: &HierarchySnapshot,
) -> Result<(), Box<dyn Error>> {
    let desired = snapshot_window_ids(snapshot);
    let additions = desired
        .difference(&registry.watched_windows)
        .copied()
        .collect::<Vec<_>>();
    for window in additions {
        match select_substructure_notify(connection, window) {
            Ok(()) => {
                registry.watched_windows.insert(window);
            }
            Err(error) if is_bad_window_error(error.as_ref()) => {
                eprintln!("watch skipped for vanished window 0x{window:08x}");
            }
            Err(error) => return Err(error),
        }
    }

    let removals = registry
        .watched_windows
        .difference(&desired)
        .copied()
        .collect::<Vec<_>>();
    for window in removals {
        match clear_structure_notify(connection, window) {
            Ok(()) => {
                registry.watched_windows.remove(&window);
            }
            Err(error) if is_bad_window_error(error.as_ref()) => {
                registry.watched_windows.remove(&window);
            }
            Err(error) => return Err(error),
        }
    }
    connection.inner.flush()?;
    Ok(())
}

fn select_substructure_notify(
    connection: &X11Connection,
    window: Window,
) -> Result<(), Box<dyn Error>> {
    connection
        .inner
        .change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::SUBSTRUCTURE_NOTIFY),
        )?
        .check()?;
    Ok(())
}

fn clear_structure_notify(
    connection: &X11Connection,
    window: Window,
) -> Result<(), Box<dyn Error>> {
    connection
        .inner
        .change_window_attributes(
            window,
            &ChangeWindowAttributesAux::new().event_mask(EventMask::NO_EVENT),
        )?
        .check()?;
    Ok(())
}

fn snapshot_window_ids(snapshot: &HierarchySnapshot) -> HashSet<Window> {
    let mut windows = HashSet::new();
    for binding in &snapshot.children {
        windows.insert(binding.root_child_xid);
        for metadata in &binding.descendants {
            windows.insert(metadata.window);
        }
    }
    windows
}

fn structural_event_reason(event: &Event) -> Option<String> {
    let (name, window) = match event {
        Event::CreateNotify(event) => ("CreateNotify", event.window),
        Event::MapNotify(event) => ("MapNotify", event.window),
        Event::UnmapNotify(event) => ("UnmapNotify", event.window),
        Event::DestroyNotify(event) => ("DestroyNotify", event.window),
        Event::ReparentNotify(event) => ("ReparentNotify", event.window),
        Event::ConfigureNotify(event) => ("ConfigureNotify", event.window),
        Event::CirculateNotify(event) => ("CirculateNotify", event.window),
        _ => return None,
    };
    Some(format!("{name} window=0x{window:08x}"))
}

fn snapshot_delta(
    previous: &HierarchySnapshot,
    current: &HierarchySnapshot,
) -> Vec<RegistryChange> {
    let previous_entries = snapshot_fingerprints(previous);
    let current_entries = snapshot_fingerprints(current);
    let previous_order = previous_entries.iter().map(|(window, _)| *window).collect::<Vec<_>>();
    let current_order = current_entries.iter().map(|(window, _)| *window).collect::<Vec<_>>();
    let previous_map = previous_entries.into_iter().collect::<HashMap<_, _>>();
    let current_map = current_entries.into_iter().collect::<HashMap<_, _>>();
    let mut changes = Vec::new();

    for window in current_map.keys() {
        if !previous_map.contains_key(window) {
            changes.push(RegistryChange::RootChildAdded(*window));
        }
    }
    for window in previous_map.keys() {
        if !current_map.contains_key(window) {
            changes.push(RegistryChange::RootChildRemoved(*window));
        }
    }
    if previous_order != current_order {
        changes.push(RegistryChange::StackingChanged);
    }

    for (window, previous_fingerprint) in &previous_map {
        let Some(current_fingerprint) = current_map.get(window) else {
            continue;
        };
        if previous_fingerprint.semantic_client != current_fingerprint.semantic_client {
            changes.push(RegistryChange::SemanticBindingChanged {
                window: *window,
                previous: previous_fingerprint.semantic_client.clone(),
                current: current_fingerprint.semantic_client.clone(),
            });
        }
        if previous_fingerprint.map_state != current_fingerprint.map_state {
            changes.push(RegistryChange::MapStateChanged {
                window: *window,
                previous: previous_fingerprint.map_state,
                current: current_fingerprint.map_state,
            });
        }
        if previous_fingerprint.geometry != current_fingerprint.geometry {
            changes.push(RegistryChange::GeometryChanged { window: *window });
        }
        if previous_fingerprint.stale != current_fingerprint.stale {
            changes.push(RegistryChange::StaleChanged {
                window: *window,
                previous: previous_fingerprint.stale,
                current: current_fingerprint.stale,
            });
        }
    }
    changes
}

fn snapshot_fingerprints(
    snapshot: &HierarchySnapshot,
) -> Vec<(Window, BindingFingerprint)> {
    snapshot
        .children
        .iter()
        .map(|binding| {
            (
                binding.root_child_xid,
                BindingFingerprint {
                    semantic_client: binding.semantic_client.clone(),
                    map_state: binding.surface_candidate.as_ref().map(|metadata| metadata.map_state),
                    geometry: binding.surface_candidate.as_ref().map(|metadata| metadata.geometry),
                    stale: binding.stale,
                },
            )
        })
        .collect()
}

fn print_change(change: &RegistryChange) {
    match change {
        RegistryChange::RootChildAdded(window) => {
            println!("  root child added: 0x{window:08x}");
        }
        RegistryChange::RootChildRemoved(window) => {
            println!("  root child removed: 0x{window:08x}");
        }
        RegistryChange::StackingChanged => println!("  stacking order changed"),
        RegistryChange::SemanticBindingChanged {
            window,
            previous,
            current,
        } => println!(
            "  semantic binding changed for 0x{window:08x}: {previous:?} -> {current:?}"
        ),
        RegistryChange::MapStateChanged {
            window,
            previous,
            current,
        } => println!(
            "  map state changed for 0x{window:08x}: {:?} -> {:?}",
            previous.map(crate::x11::map_state_name),
            current.map(crate::x11::map_state_name)
        ),
        RegistryChange::GeometryChanged { window } => {
            println!("  geometry changed: 0x{window:08x}");
        }
        RegistryChange::StaleChanged {
            window,
            previous,
            current,
        } => println!(
            "  stale status changed for 0x{window:08x}: {previous} -> {current}"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::{snapshot_delta, RegistryChange};
    use crate::x11::capture::{WindowGeometry, WindowMetadata, WindowRole};
    use crate::x11::tree::{BindingStatus, HierarchyBinding, HierarchySnapshot};
    use x11rb::protocol::xproto::{MapState, WindowClass};

    fn binding(window: u32, semantic_client: BindingStatus) -> HierarchyBinding {
        binding_with_state(window, semantic_client, None, None, false)
    }

    fn binding_with_state(
        window: u32,
        semantic_client: BindingStatus,
        map_state: Option<MapState>,
        geometry: Option<WindowGeometry>,
        stale: bool,
    ) -> HierarchyBinding {
        HierarchyBinding {
            root_child_xid: window,
            semantic_client_xids: Vec::new(),
            semantic_client,
            lifecycle_candidate_xid: window,
            surface_candidate: map_state.zip(geometry).map(|(map_state, geometry)| {
                WindowMetadata {
                    window,
                    geometry,
                    depth: 24,
                    visual: 0,
                    class: WindowClass::INPUT_OUTPUT,
                    override_redirect: false,
                    has_wm_state: false,
                    map_state,
                    wm_class: None,
                    window_type: None,
                    role: WindowRole::Unknown,
                }
            }),
            descendants: Vec::new(),
            stale,
        }
    }

    fn snapshot(bindings: Vec<HierarchyBinding>) -> HierarchySnapshot {
        HierarchySnapshot {
            root: 1,
            children: bindings,
        }
    }

    #[test]
    fn registry_delta_detects_added_root_child() {
        let changes = snapshot_delta(&snapshot(Vec::new()), &snapshot(vec![binding(10, BindingStatus::NoClient)]));
        assert!(changes.contains(&RegistryChange::RootChildAdded(10)));
    }

    #[test]
    fn registry_delta_detects_removed_root_child() {
        let changes = snapshot_delta(&snapshot(vec![binding(10, BindingStatus::NoClient)]), &snapshot(Vec::new()));
        assert!(changes.contains(&RegistryChange::RootChildRemoved(10)));
    }

    #[test]
    fn registry_delta_detects_stacking_change() {
        let previous = snapshot(vec![
            binding(10, BindingStatus::NoClient),
            binding(20, BindingStatus::NoClient),
        ]);
        let current = snapshot(vec![
            binding(20, BindingStatus::NoClient),
            binding(10, BindingStatus::NoClient),
        ]);
        assert!(snapshot_delta(&previous, &current).contains(&RegistryChange::StackingChanged));
    }

    #[test]
    fn registry_delta_detects_semantic_binding_change() {
        let previous = snapshot(vec![binding(10, BindingStatus::SingleClient(100))]);
        let current = snapshot(vec![binding(10, BindingStatus::SingleClient(200))]);
        assert!(snapshot_delta(&previous, &current).iter().any(|change| {
            matches!(change, RegistryChange::SemanticBindingChanged { window: 10, .. })
        }));
    }

    #[test]
    fn registry_delta_detects_map_state_change() {
        let geometry = WindowGeometry {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
            border_width: 0,
        };
        let previous = snapshot(vec![binding_with_state(
            10,
            BindingStatus::NoClient,
            Some(MapState::UNMAPPED),
            Some(geometry),
            false,
        )]);
        let current = snapshot(vec![binding_with_state(
            10,
            BindingStatus::NoClient,
            Some(MapState::VIEWABLE),
            Some(geometry),
            false,
        )]);
        assert!(snapshot_delta(&previous, &current).iter().any(|change| {
            matches!(change, RegistryChange::MapStateChanged { window: 10, .. })
        }));
    }

    #[test]
    fn registry_delta_detects_geometry_change() {
        let previous = snapshot(vec![binding_with_state(
            10,
            BindingStatus::NoClient,
            Some(MapState::VIEWABLE),
            Some(WindowGeometry {
                x: 0,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
            }),
            false,
        )]);
        let current = snapshot(vec![binding_with_state(
            10,
            BindingStatus::NoClient,
            Some(MapState::VIEWABLE),
            Some(WindowGeometry {
                x: 10,
                y: 0,
                width: 100,
                height: 100,
                border_width: 0,
            }),
            false,
        )]);
        assert!(snapshot_delta(&previous, &current).contains(&RegistryChange::GeometryChanged {
            window: 10,
        }));
    }

    #[test]
    fn registry_delta_detects_stale_change() {
        let previous = snapshot(vec![binding_with_state(
            10,
            BindingStatus::NoClient,
            None,
            None,
            false,
        )]);
        let current = snapshot(vec![binding_with_state(
            10,
            BindingStatus::NoClient,
            None,
            None,
            true,
        )]);
        assert!(snapshot_delta(&previous, &current).iter().any(|change| {
            matches!(change, RegistryChange::StaleChanged { window: 10, .. })
        }));
    }
}
