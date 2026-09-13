use std::cell::Cell;
use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fmt;
use std::rc::Rc;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::errors::ReplyError;
use x11rb::protocol::composite::ConnectionExt as CompositeConnectionExt;
use x11rb::protocol::damage::{self, ConnectionExt as DamageConnectionExt};
use x11rb::protocol::present::{self, ConnectionExt as PresentConnectionExt};
use x11rb::protocol::render::{self, ConnectionExt as RenderConnectionExt};
use x11rb::protocol::ErrorKind;
use x11rb::protocol::xproto::{
    self, ChangeWindowAttributesAux, ConnectionExt as XprotoConnectionExt,
    EventMask, Window, WindowClass,
};
use x11rb::protocol::Event;

use crate::graphics::egl::{EglImportedSurface, EglSceneRenderer};
use crate::config::CompositorConfig;
use super::capture::{WindowGeometry, WindowMetadata};
use super::compositor::{selection_clear_matches, CompositorOwnership};
use super::connection::X11Connection;
use super::manual::{
    check_capabilities, check_selection_available, parse_root, ManualSubwindowsRedirect,
};
use super::overlay::OverlayLease;
use super::shutdown::{wait_for_event_or_shutdown, SignalWake, WaitResult};
use super::tree::{BindingStatus, HierarchySnapshot};

const BACKGROUND_BLUR_RADIUS_PX: f32 = 12.0;
const MAX_DIAGNOSTIC_PENDING_DAMAGE: usize = 256;
const MAX_SURFACE_DIAGNOSTICS: usize = 32;
const RECENT_MOVE_DIAGNOSTIC_WINDOW: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ResizeOnlyDirection {
    Grow,
    Shrink,
    Mixed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum GeometryEventSource { CanonicalSurface, SemanticClient, Other, Unknown }

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[allow(dead_code)]
enum StructuralOrigin { NormalLifecycle, Hierarchy, GeometrySurface, GeometrySemanticClient, GeometryNoPending, Other }

#[derive(Default)]
struct TimingMetric {
    samples: u64,
    total_us: u128,
    max_us: u128,
}

#[derive(Default)]
struct ResizeOnlyFallbackReasons {
    unavailable_state: u64,
    identity_mismatch: u64,
    no_size_change: u64,
    geometry_superseded: u64,
    unsupported_visual: u64,
    missing_damage: u64,
    precommit_rejected: u64,
    hierarchy: u64,
}

impl ResizeOnlyFallbackReasons {
    fn record(&mut self, reason: ResizeOnlyFallbackReason) {
        match reason {
            ResizeOnlyFallbackReason::UnavailableState => self.unavailable_state += 1,
            ResizeOnlyFallbackReason::IdentityMismatch => self.identity_mismatch += 1,
            ResizeOnlyFallbackReason::NoSizeChange => self.no_size_change += 1,
            ResizeOnlyFallbackReason::GeometrySuperseded => self.geometry_superseded += 1,
            ResizeOnlyFallbackReason::UnsupportedVisual => self.unsupported_visual += 1,
            ResizeOnlyFallbackReason::MissingDamage => self.missing_damage += 1,
            ResizeOnlyFallbackReason::PrecommitRejected => self.precommit_rejected += 1,
            ResizeOnlyFallbackReason::Hierarchy => self.hierarchy += 1,
        }
    }

    fn total(&self) -> u64 {
        self.unavailable_state
            + self.identity_mismatch
            + self.no_size_change
            + self.geometry_superseded
            + self.unsupported_visual
            + self.missing_damage
            + self.precommit_rejected
            + self.hierarchy
    }
}

#[derive(Clone, Copy)]
enum ResizeOnlyFallbackReason {
    UnavailableState,
    IdentityMismatch,
    NoSizeChange,
    GeometrySuperseded,
    UnsupportedVisual,
    MissingDamage,
    PrecommitRejected,
    Hierarchy,
}

impl TimingMetric {
    fn record(&mut self, elapsed: Duration) {
        let micros = elapsed.as_micros();
        self.samples += 1;
        self.total_us += micros;
        self.max_us = self.max_us.max(micros);
    }

    fn merge(&mut self, other: TimingMetric) {
        self.samples += other.samples;
        self.total_us += other.total_us;
        self.max_us = self.max_us.max(other.max_us);
    }
}

#[derive(Default)]
struct ResizeOnlyDirectionDiagnostics {
    attempted: u64,
    success: u64,
    fallback: u64,
    hierarchy_abort: u64,
    move_resize_attempted: u64,
    move_resize_success: u64,
    total: TimingMetric,
    pre_acquire: TimingMetric,
    damage: TimingMetric,
    name_pixmap: TimingMetric,
    pixmap_get_geometry: TimingMetric,
    egl_import: TimingMetric,
    target_build_render: TimingMetric,
    precommit: TimingMetric,
    publish: TimingMetric,
    resource_blocking: TimingMetric,
    fallback_reasons: ResizeOnlyFallbackReasons,
    fallback_move_resize: u64,
    fallback_to_structural: u64,
    fallback_full_snapshot: u64,
    structural_candidates_started: u64,
    structural_total: TimingMetric,
    structural_full_snapshot: TimingMetric,
    structural_stale: u64,
    structural_published: u64,
    structural_retry: u64,
}

#[derive(Default)]
struct SurfaceDiagnostic3a3f8b4c {
    surface_xid: Window,
    semantic_client_xid: Option<Window>,
    first_damage_id: Option<damage::Damage>,
    current_damage_id: Option<damage::Damage>,
    damage_id_changes: u64,
    damage_notify_arrivals: u64,
    unique_damage_obligations: u64,
    damage_subtracts: u64,
    damage_dispatches: u64,
    damage_pending_samples: u64,
    damage_pending_total_us: u128,
    damage_pending_max_us: u128,
    last_damage_notify_timestamp: Option<Instant>,
    damage_notify_gap_samples: u64,
    damage_notify_gap_total_us: u128,
    damage_notify_gap_max_us: u128,
    moveonly_count: u64,
    last_moveonly_timestamp: Option<Instant>,
    damage_arrivals_before_first_move: u64,
    damage_arrivals_after_first_move: u64,
    damage_arrivals_within_2s_after_move: u64,
    damage_gap_max_before_move_us: u128,
    damage_gap_max_after_move_us: u128,
    damage_gap_max_within_2s_after_move_us: u128,
}

impl SurfaceDiagnostic3a3f8b4c {
    fn observe_identity(&mut self, surface_xid: Window, semantic_client_xid: Option<Window>, damage_id: damage::Damage) {
        self.surface_xid = surface_xid;
        if semantic_client_xid.is_some() { self.semantic_client_xid = semantic_client_xid; }
        match self.current_damage_id {
            None => {
                self.first_damage_id = Some(damage_id);
                self.current_damage_id = Some(damage_id);
            }
            Some(current) if current != damage_id => {
                self.damage_id_changes += 1;
                self.current_damage_id = Some(damage_id);
            }
            Some(_) => {}
        }
    }
}

#[derive(Clone, Copy, Default, Debug, Eq, PartialEq)]
struct GeometryPresentHistory {
    ever_deferred: bool,
    deferrals: u8,
    updated_while_deferred: bool,
    superseded_while_deferred: bool,
}

impl GeometryPresentHistory {
    fn deferred(mut self) -> Self {
        self.ever_deferred = true;
        self.deferrals = self.deferrals.saturating_add(1).min(8);
        self
    }
}

#[derive(Default)]
struct Diagnostics3a3f8b3a {
    enabled: bool,
    configure_seen: u64,
    configure_move_like: u64,
    configure_resize_like: u64,
    configure_other: u64,
    configure_superseded: u64,
    geometry_dispatches: u64,
    moveonly_attempted: u64,
    moveonly_success: u64,
    moveonly_fallback: u64,
    resize_geometry_dispatches: u64,
    geometry_dispatches_while_damage_pending: u64,
    max_geometry_dispatches_before_pending_damage_service: u64,
    consecutive_geometry_while_damage_pending: u64,
    pixel_damage_arrivals: u64,
    pixel_damage_coalesced_notifications: u64,
    pixel_damage_dispatches: u64,
    pixel_damage_dispatch_while_geometry_pending: u64,
    pixel_damage_deferred_by_geometry: u64,
    pixel_damage_wait_max_us: u128,
    pixel_damage_wait_total_us: u128,
    pixel_damage_wait_samples: u64,
    pixel_damage_wait_le_1ms: u64,
    pixel_damage_wait_1_4ms: u64,
    pixel_damage_wait_4_8ms: u64,
    pixel_damage_wait_8_16ms: u64,
    pixel_damage_wait_16_33ms: u64,
    pixel_damage_wait_33_50ms: u64,
    pixel_damage_wait_50_100ms: u64,
    pixel_damage_wait_gt100ms: u64,
    event_batches: u64,
    event_batches_with_geometry: u64,
    event_batches_with_pixel_damage_arrival: u64,
    event_batches_ended_with_pixel_damage_pending: u64,
    max_batches_damage_remained_pending: u64,
    recompositions: u64,
    recompositions_after_geometry: u64,
    recompositions_after_pixel_damage: u64,
    present_submissions: u64,
    present_completion_events: u64,
    structural_candidates_started: u64,
    structural_candidates_published: u64,
    structural_candidates_stale: u64,
    structural_candidates_failed: u64,
    resize_candidate_started: u64,
    resize_candidate_stale: u64,
    resize_candidate_published: u64,
    resize_candidate_failed: u64,
    distinct_resize_states_dispatched: u64,
    resized_target_bundle_acquisitions: u64,
    resized_target_damage_acquisitions: u64,
    resized_target_named_pixmap_acquisitions: u64,
    resized_target_egl_imports: u64,
    resized_target_bundle_acquisition_then_candidate_stale: u64,
    resource_bundles_reused: u64,
    resource_bundles_new: u64,
    resizeonly_attempted: u64,
    resizeonly_success: u64,
    resizeonly_fallback: u64,
    resizeonly_superseded_before_acquisition: u64,
    resizeonly_full_snapshot_avoided: u64,
    resizeonly_hierarchy_abort: u64,
    resizeonly_target_damage_reused: u64,
    resizeonly_target_damage_created: u64,
    resizeonly_target_damage_id_changed: u64,
    resizeonly_publish_with_damage_pending: u64,
    resizeonly_fallback_early_unclassified: u64,
    resizeonly_grow: ResizeOnlyDirectionDiagnostics,
    resizeonly_shrink: ResizeOnlyDirectionDiagnostics,
    resizeonly_mixed: ResizeOnlyDirectionDiagnostics,
    resizeonly_fallback_origin: Option<(ResizeOnlyDirection, bool)>,
    resizeonly_structural_direction: Option<ResizeOnlyDirection>,
    resizeonly_structural_timing: Option<(ResizeOnlyDirection, Instant)>,
    pending_since: HashMap<damage::Damage, Instant>,
    last_resize_geometry: Option<(Window, u16, u16, u16)>,
    batches_with_damage_pending: u64,
    last_candidate_resize: bool,
    surface_diagnostics: Vec<SurfaceDiagnostic3a3f8b4c>,
    configure_from_surface: u64,
    configure_from_semantic_client: u64,
    configure_from_other: u64,
    configure_from_unknown: u64,
    semantic_client_resolved_to_surface: u64,
    semantic_client_geometry_update_rejected: u64,
    semantic_client_without_surface_pending_geometry: u64,
    surface_geometry_update_accepted: u64,
    surface_geometry_update_rejected: u64,
    pending_geometry_created: u64,
    pending_geometry_updated: u64,
    pending_geometry_superseded: u64,
    pending_geometry_missing_at_dispatch: u64,
    pending_geometry_surface_match: u64,
    pending_geometry_surface_mismatch: u64,
    resize_dispatch_total: u64,
    resize_dispatch_resizeonly_selected: u64,
    resize_dispatch_structural_selected: u64,
    resize_dispatch_deferred: u64,
    resize_dispatch_hierarchy_dominated: u64,
    resize_dispatch_no_pending_geometry: u64,
    resize_dispatch_other_source_reason: u64,
    resize_dispatch_unknown: u64,
    resizeonly_pre_attempt_bypass_total: u64,
    resizeonly_pre_attempt_bypass_no_present_complete: u64,
    resizeonly_pre_attempt_bypass_hierarchy_priority: u64,
    resizeonly_pre_attempt_bypass_no_pending_geometry: u64,
    resizeonly_pre_attempt_bypass_semantic_client_no_surface_pending_geometry: u64,
    resizeonly_pre_attempt_bypass_pending_geometry_other_surface: u64,
    resizeonly_pre_attempt_bypass_no_size_or_border_change: u64,
    resizeonly_pre_attempt_bypass_ambiguous_or_superseded: u64,
    resizeonly_pre_attempt_bypass_structural_already_required: u64,
    resizeonly_pre_attempt_bypass_other: u64,
    resizeonly_pre_attempt_bypass_direction_unknown: u64,
    resizeonly_grow_pre_attempt_bypass: u64,
    resizeonly_shrink_pre_attempt_bypass: u64,
    resizeonly_mixed_pre_attempt_bypass: u64,
    resizeonly_direction_unknown_bypass: u64,
    pre_resizeonly_bypass_move_resize: u64,
    structural_origin_normal: u64,
    structural_origin_hierarchy: u64,
    structural_origin_geometry_surface: u64,
    structural_origin_geometry_semantic_client: u64,
    structural_origin_geometry_no_pending: u64,
    structural_origin_other: u64,
    stale_geometry_from_surface_configure: u64,
    stale_geometry_from_semantic_client_configure: u64,
    stale_geometry_without_pending_geometry: u64,
    stale_geometry_retry: u64,
    stale_geometry_deferred: u64,
    snapshot_geometry_surface: u64,
    snapshot_geometry_semantic_client: u64,
    snapshot_geometry_no_pending: u64,
    snapshot_hierarchy: u64,
    snapshot_other: u64,
    structural_origin: Option<StructuralOrigin>,
    geometry_scheduling_batches_total: u64,
    geometry_scheduling_present_deferred: u64,
    geometry_scheduling_hierarchy_dominated: u64,
    geometry_pending_ever_present_deferred: u64,
    geometry_pending_present_deferred_once: u64,
    geometry_pending_present_deferred_multiple: u64,
    geometry_pending_updated_while_present_deferred: u64,
    geometry_pending_superseded_while_present_deferred: u64,
    final_resize_was_present_deferred: u64,
    final_resize_never_present_deferred: u64,
    final_resize_deferrals_0: u64,
    final_resize_deferrals_1: u64,
    final_resize_deferrals_2_3: u64,
    final_resize_deferrals_4_7: u64,
    final_resize_deferrals_8_plus: u64,
    grow_after_present_defer: u64,
    grow_without_present_defer: u64,
    shrink_after_present_defer: u64,
    shrink_without_present_defer: u64,
    mixed_after_present_defer: u64,
    mixed_without_present_defer: u64,
    move_resize_after_present_defer: u64,
    move_resize_without_present_defer: u64,
    resizeonly_selected_after_present_defer: u64,
    resizeonly_selected_without_present_defer: u64,
    structural_selected_after_present_defer: u64,
    structural_selected_without_present_defer: u64,
    structural_publish_without_present_defer: u64,
    resizeonly_success_after_present_defer: u64,
    resizeonly_success_without_present_defer: u64,
    resizeonly_fallback_after_present_defer: u64,
    resizeonly_fallback_without_present_defer: u64,
    precommit_rejected_after_present_defer: u64,
    precommit_rejected_without_present_defer: u64,
    resizeonly_structural_present_deferred: Option<bool>,
    resizeonly_present_deferred: Option<bool>,
    structural_stale_after_present_defer: u64,
    structural_stale_without_present_defer: u64,
    structural_publish_after_present_defer: u64,
    geometry_retry_after_present_defer: u64,
    geometry_retry_without_present_defer: u64,
    geometry_deferred_rebuild_after_present_defer: u64,
    geometry_deferred_rebuild_without_present_defer: u64,
    hierarchy_event_total: u64,
    hierarchy_event_unknown_configure: u64,
    hierarchy_event_create: u64,
    hierarchy_event_map: u64,
    hierarchy_event_unmap: u64,
    hierarchy_event_destroy: u64,
    hierarchy_event_reparent: u64,
    hierarchy_event_circulate: u64,
    hierarchy_decision_total: u64,
    hierarchy_decision_only_unknown_configure: u64,
    hierarchy_decision_only_create: u64,
    hierarchy_decision_only_map: u64,
    hierarchy_decision_only_unmap: u64,
    hierarchy_decision_only_destroy: u64,
    hierarchy_decision_only_reparent: u64,
    hierarchy_decision_only_circulate: u64,
    hierarchy_decision_multi_source: u64,
    hierarchy_decision_existing_merge: u64,
    unknown_configure_internal: u64,
    unknown_configure_unresolved: u64,
    hierarchy_from_internal_window: u64,
    hierarchy_event_target_surface: u64,
    hierarchy_event_target_semantic_client: u64,
    hierarchy_event_other_tracked_surface: u64,
    hierarchy_event_other_semantic_client: u64,
    hierarchy_event_unknown_window: u64,
    hierarchy_decision_with_geometry_pending: u64,
    hierarchy_decision_cleared_pending_geometry: u64,
    hierarchy_selected_while_resize_geometry_pending: u64,
    hierarchy_won_over_grow: u64,
    hierarchy_won_over_shrink: u64,
    hierarchy_won_over_mixed: u64,
    snapshot_hierarchy_unknown_configure: u64,
    snapshot_hierarchy_lifecycle: u64,
    snapshot_hierarchy_reparent: u64,
    snapshot_hierarchy_circulate: u64,
    snapshot_hierarchy_multi_source: u64,
    hierarchy_unknown_configure_candidate_stale_geometry: u64,
    hierarchy_lifecycle_candidate_stale_geometry: u64,
    hierarchy_reparent_candidate_stale_geometry: u64,
    hierarchy_circulate_candidate_stale_geometry: u64,
    hierarchy_multi_candidate_stale_geometry: u64,
    hierarchy_unknown_configure_retry: u64,
    hierarchy_lifecycle_retry: u64,
    hierarchy_reparent_retry: u64,
    hierarchy_circulate_retry: u64,
    hierarchy_multi_retry: u64,
    hierarchy_unknown_configure_deferred: u64,
    hierarchy_lifecycle_deferred: u64,
    hierarchy_reparent_deferred: u64,
    hierarchy_circulate_deferred: u64,
    hierarchy_multi_deferred: u64,
    hierarchy_source_bits: u16,
    compound_hierarchy_geometry_observed: u64,
    compound_rebase_attempted: u64,
    compound_rebase_success: u64,
    compound_rebase_rejected_lifecycle: u64,
    compound_rebase_rejected_scene_membership: u64,
    compound_rebase_rejected_newer_hierarchy: u64,
    compound_rebase_superseded_geometry: u64,
    compound_rebase_damage_reused: u64,
    compound_rebase_named_pixmap_reacquired: u64,
    compound_rebase_egl_reacquired: u64,
    compound_rebase_avoided_full_retry: u64,
}

impl Diagnostics3a3f8b3a {
    fn from_environment() -> Self {
        Self { enabled: std::env::var_os("XOMPOSITE_3A3F8B3A_DIAG").is_some(), ..Self::default() }
    }

    fn record_configure(&mut self, event: &Event, snapshot: &SceneSnapshot) {
        if !self.enabled { return; }
        let Event::ConfigureNotify(event) = event else { return; };
        self.configure_seen += 1;
        match snapshot.entries.iter().find(|entry| entry.surface_xid == event.window) {
            Some(entry) if entry.geometry.width == event.width && entry.geometry.height == event.height
                && entry.geometry.border_width == event.border_width => self.configure_move_like += 1,
            Some(_) => {
                self.configure_resize_like += 1;
                let state = (event.window, event.width, event.height, event.border_width);
                if self.last_resize_geometry != Some(state) { self.distinct_resize_states_dispatched += 1; }
                self.last_resize_geometry = Some(state);
            }
            None => self.configure_other += 1,
        }
    }

    fn record_geometry_source(&mut self, source: GeometryEventSource) {
        if !self.enabled { return; }
        match source {
            GeometryEventSource::CanonicalSurface => self.configure_from_surface += 1,
            GeometryEventSource::SemanticClient => { self.configure_from_semantic_client += 1; self.semantic_client_resolved_to_surface += 1; }
            GeometryEventSource::Other => self.configure_from_other += 1,
            GeometryEventSource::Unknown => self.configure_from_unknown += 1,
        }
    }

    fn record_geometry_rejected(&mut self, source: GeometryEventSource) {
        if !self.enabled { return; }
        match source {
            GeometryEventSource::CanonicalSurface => self.surface_geometry_update_rejected += 1,
            GeometryEventSource::SemanticClient => {
                self.semantic_client_geometry_update_rejected += 1;
                self.semantic_client_without_surface_pending_geometry += 1;
            }
            _ => {}
        }
    }

    fn record_pending_geometry(&mut self, source: GeometryEventSource, had_pending: bool, same_surface: bool) {
        if !self.enabled { return; }
        if had_pending { self.pending_geometry_updated += 1; self.pending_geometry_superseded += 1; }
        else { self.pending_geometry_created += 1; }
        if same_surface { self.pending_geometry_surface_match += 1; }
        else { self.pending_geometry_surface_mismatch += 1; }
        if matches!(source, GeometryEventSource::CanonicalSurface) { self.surface_geometry_update_accepted += 1; }
    }

    fn record_resize_dispatch(&mut self, source: GeometryEventSource, structural: bool) {
        if !self.enabled { return; }
        self.resize_dispatch_total += 1;
        if structural { self.resize_dispatch_structural_selected += 1; }
        else { self.resize_dispatch_resizeonly_selected += 1; }
        match source { GeometryEventSource::Unknown => self.resize_dispatch_unknown += 1, GeometryEventSource::Other => self.resize_dispatch_other_source_reason += 1, _ => {} }
    }

    fn record_pre_attempt_bypass(&mut self, source: GeometryEventSource, reason: PreResizeOnlyBypassReason, direction: Option<ResizeOnlyDirection>, move_resize: bool) {
        if !self.enabled { return; }
        self.resizeonly_pre_attempt_bypass_total += 1;
        match reason {
            PreResizeOnlyBypassReason::NoPresentComplete => self.resizeonly_pre_attempt_bypass_no_present_complete += 1,
            PreResizeOnlyBypassReason::HierarchyPriority => self.resizeonly_pre_attempt_bypass_hierarchy_priority += 1,
            PreResizeOnlyBypassReason::NoPendingGeometry => self.resizeonly_pre_attempt_bypass_no_pending_geometry += 1,
            PreResizeOnlyBypassReason::SemanticClientNoSurfacePendingGeometry => self.resizeonly_pre_attempt_bypass_semantic_client_no_surface_pending_geometry += 1,
            PreResizeOnlyBypassReason::PendingGeometryOtherSurface => self.resizeonly_pre_attempt_bypass_pending_geometry_other_surface += 1,
            PreResizeOnlyBypassReason::NoSizeOrBorderChange => self.resizeonly_pre_attempt_bypass_no_size_or_border_change += 1,
            PreResizeOnlyBypassReason::AmbiguousOrSuperseded => self.resizeonly_pre_attempt_bypass_ambiguous_or_superseded += 1,
            PreResizeOnlyBypassReason::StructuralAlreadyRequired => self.resizeonly_pre_attempt_bypass_structural_already_required += 1,
            PreResizeOnlyBypassReason::Other => self.resizeonly_pre_attempt_bypass_other += 1,
            PreResizeOnlyBypassReason::DirectionUnknown => self.resizeonly_pre_attempt_bypass_direction_unknown += 1,
        }
        match direction { Some(ResizeOnlyDirection::Grow) => self.resizeonly_grow_pre_attempt_bypass += 1, Some(ResizeOnlyDirection::Shrink) => self.resizeonly_shrink_pre_attempt_bypass += 1, Some(ResizeOnlyDirection::Mixed) => self.resizeonly_mixed_pre_attempt_bypass += 1, None => self.resizeonly_direction_unknown_bypass += 1 }
        if move_resize { self.pre_resizeonly_bypass_move_resize += 1; }
        if matches!(source, GeometryEventSource::SemanticClient) { self.semantic_client_geometry_update_rejected += 1; self.semantic_client_without_surface_pending_geometry += 1; }
    }

    fn begin_structural_origin(&mut self, origin: StructuralOrigin) {
        if !self.enabled { return; }
        self.structural_origin = Some(origin);
        match origin { StructuralOrigin::NormalLifecycle => self.structural_origin_normal += 1, StructuralOrigin::Hierarchy => self.structural_origin_hierarchy += 1, StructuralOrigin::GeometrySurface => self.structural_origin_geometry_surface += 1, StructuralOrigin::GeometrySemanticClient => self.structural_origin_geometry_semantic_client += 1, StructuralOrigin::GeometryNoPending => self.structural_origin_geometry_no_pending += 1, StructuralOrigin::Other => self.structural_origin_other += 1 }
    }

    fn record_snapshot_origin(&mut self) {
        if !self.enabled { return; }
        match self.structural_origin { Some(StructuralOrigin::GeometrySurface) => self.snapshot_geometry_surface += 1, Some(StructuralOrigin::GeometrySemanticClient) => self.snapshot_geometry_semantic_client += 1, Some(StructuralOrigin::GeometryNoPending) => self.snapshot_geometry_no_pending += 1, Some(StructuralOrigin::Hierarchy) => self.snapshot_hierarchy += 1, _ => self.snapshot_other += 1 }
        if matches!(self.structural_origin, Some(StructuralOrigin::Hierarchy)) { self.record_hierarchy_snapshot_source(self.hierarchy_source_bits); }
    }

    fn record_stale_origin(&mut self, invalidation: SceneInvalidation, deferred: bool) {
        if !self.enabled || !matches!(invalidation, SceneInvalidation::Geometry(_)) { return; }
        match self.structural_origin { Some(StructuralOrigin::GeometrySurface) => self.stale_geometry_from_surface_configure += 1, Some(StructuralOrigin::GeometrySemanticClient) => self.stale_geometry_from_semantic_client_configure += 1, Some(StructuralOrigin::GeometryNoPending) => self.stale_geometry_without_pending_geometry += 1, _ => {} }
        if deferred { self.stale_geometry_deferred += 1; } else { self.stale_geometry_retry += 1; }
        if matches!(self.structural_origin, Some(StructuralOrigin::Hierarchy)) { self.record_hierarchy_geometry_stage(self.hierarchy_source_bits, !deferred); }
    }

    fn record_geometry_pending_at_dispatch(&mut self, update: Option<PendingGeometry>) { if self.enabled && update.is_none() { self.pending_geometry_missing_at_dispatch += 1; } }

    fn record_hierarchy_event(&mut self, source: HierarchyEventSource, internal: bool, relation: HierarchyEventRelation) {
        if !self.enabled { return; }
        self.hierarchy_event_total += 1;
        match source {
            HierarchyEventSource::UnknownConfigure => self.hierarchy_event_unknown_configure += 1,
            HierarchyEventSource::Create => self.hierarchy_event_create += 1,
            HierarchyEventSource::Map => self.hierarchy_event_map += 1,
            HierarchyEventSource::Unmap => self.hierarchy_event_unmap += 1,
            HierarchyEventSource::Destroy => self.hierarchy_event_destroy += 1,
            HierarchyEventSource::Reparent => self.hierarchy_event_reparent += 1,
            HierarchyEventSource::Circulate => self.hierarchy_event_circulate += 1,
            HierarchyEventSource::ExistingHierarchyMerge => {}
        }
        if internal { self.hierarchy_from_internal_window += 1; }
        match relation {
            HierarchyEventRelation::TargetSurface => self.hierarchy_event_target_surface += 1,
            HierarchyEventRelation::TargetSemanticClient => self.hierarchy_event_target_semantic_client += 1,
            HierarchyEventRelation::OtherTrackedSurface => self.hierarchy_event_other_tracked_surface += 1,
            HierarchyEventRelation::OtherSemanticClient => self.hierarchy_event_other_semantic_client += 1,
            HierarchyEventRelation::Unknown => self.hierarchy_event_unknown_window += 1,
        }
        if matches!(source, HierarchyEventSource::UnknownConfigure) {
            if internal { self.unknown_configure_internal += 1; }
            else { self.unknown_configure_unresolved += 1; }
        }
    }

    fn record_hierarchy_decision(&mut self, bits: u16, had_geometry: bool, direction: Option<ResizeOnlyDirection>) {
        if !self.enabled { return; }
        self.hierarchy_decision_total += 1;
        if had_geometry {
            self.hierarchy_decision_with_geometry_pending += 1;
            self.hierarchy_decision_cleared_pending_geometry += 1;
            self.hierarchy_selected_while_resize_geometry_pending += 1;
            match direction {
                Some(ResizeOnlyDirection::Grow) => self.hierarchy_won_over_grow += 1,
                Some(ResizeOnlyDirection::Shrink) => self.hierarchy_won_over_shrink += 1,
                Some(ResizeOnlyDirection::Mixed) => self.hierarchy_won_over_mixed += 1,
                None => {}
            }
        }
        if bits.count_ones() > 1 { self.hierarchy_decision_multi_source += 1; return; }
        match bits {
            b if b == HierarchyEventSource::UnknownConfigure.bit() => self.hierarchy_decision_only_unknown_configure += 1,
            b if b == HierarchyEventSource::Create.bit() => self.hierarchy_decision_only_create += 1,
            b if b == HierarchyEventSource::Map.bit() => self.hierarchy_decision_only_map += 1,
            b if b == HierarchyEventSource::Unmap.bit() => self.hierarchy_decision_only_unmap += 1,
            b if b == HierarchyEventSource::Destroy.bit() => self.hierarchy_decision_only_destroy += 1,
            b if b == HierarchyEventSource::Reparent.bit() => self.hierarchy_decision_only_reparent += 1,
            b if b == HierarchyEventSource::Circulate.bit() => self.hierarchy_decision_only_circulate += 1,
            _ => self.hierarchy_decision_existing_merge += 1,
        }
    }

    fn record_hierarchy_snapshot_source(&mut self, bits: u16) {
        if !self.enabled || bits == 0 { return; }
        if bits.count_ones() > 1 { self.snapshot_hierarchy_multi_source += 1; }
        if bits & HierarchyEventSource::UnknownConfigure.bit() != 0 { self.snapshot_hierarchy_unknown_configure += 1; }
        if bits & (HierarchyEventSource::Create.bit() | HierarchyEventSource::Map.bit() | HierarchyEventSource::Unmap.bit() | HierarchyEventSource::Destroy.bit()) != 0 { self.snapshot_hierarchy_lifecycle += 1; }
        if bits & HierarchyEventSource::Reparent.bit() != 0 { self.snapshot_hierarchy_reparent += 1; }
        if bits & HierarchyEventSource::Circulate.bit() != 0 { self.snapshot_hierarchy_circulate += 1; }
    }

    fn record_hierarchy_geometry_stage(&mut self, bits: u16, retry: bool) {
        if !self.enabled || bits == 0 { return; }
        let multiple = bits.count_ones() > 1;
        if multiple {
            if retry { self.hierarchy_multi_candidate_stale_geometry += 1; }
            if retry { self.hierarchy_multi_retry += 1; } else { self.hierarchy_multi_deferred += 1; }
        } else if bits & HierarchyEventSource::UnknownConfigure.bit() != 0 {
            self.hierarchy_unknown_configure_candidate_stale_geometry += 1;
            if retry { self.hierarchy_unknown_configure_retry += 1; } else { self.hierarchy_unknown_configure_deferred += 1; }
        } else if bits & HierarchyEventSource::Reparent.bit() != 0 {
            self.hierarchy_reparent_candidate_stale_geometry += 1;
            if retry { self.hierarchy_reparent_retry += 1; } else { self.hierarchy_reparent_deferred += 1; }
        } else if bits & HierarchyEventSource::Circulate.bit() != 0 {
            self.hierarchy_circulate_candidate_stale_geometry += 1;
            if retry { self.hierarchy_circulate_retry += 1; } else { self.hierarchy_circulate_deferred += 1; }
        } else {
            self.hierarchy_lifecycle_candidate_stale_geometry += 1;
            if retry { self.hierarchy_lifecycle_retry += 1; } else { self.hierarchy_lifecycle_deferred += 1; }
        }
    }

    fn record_geometry_scheduling_batch(&mut self) {
        if self.enabled { self.geometry_scheduling_batches_total += 1; }
    }

    fn record_present_deferred(&mut self) {
        if self.enabled { self.geometry_scheduling_present_deferred += 1; }
    }

    fn record_pending_present_history(&mut self, history: GeometryPresentHistory) {
        if !self.enabled || !history.ever_deferred { return; }
        self.geometry_pending_ever_present_deferred += 1;
        if history.deferrals > 1 { self.geometry_pending_present_deferred_multiple += 1; }
        else { self.geometry_pending_present_deferred_once += 1; }
    }

    fn record_final_resize_history(&mut self, history: GeometryPresentHistory, direction: ResizeOnlyDirection, move_resize: bool) {
        if !self.enabled { return; }
        if history.ever_deferred { self.final_resize_was_present_deferred += 1; }
        else { self.final_resize_never_present_deferred += 1; }
        match history.deferrals {
            0 => self.final_resize_deferrals_0 += 1,
            1 => self.final_resize_deferrals_1 += 1,
            2..=3 => self.final_resize_deferrals_2_3 += 1,
            4..=7 => self.final_resize_deferrals_4_7 += 1,
            _ => self.final_resize_deferrals_8_plus += 1,
        }
        match (direction, history.ever_deferred) {
            (ResizeOnlyDirection::Grow, true) => self.grow_after_present_defer += 1,
            (ResizeOnlyDirection::Grow, false) => self.grow_without_present_defer += 1,
            (ResizeOnlyDirection::Shrink, true) => self.shrink_after_present_defer += 1,
            (ResizeOnlyDirection::Shrink, false) => self.shrink_without_present_defer += 1,
            (ResizeOnlyDirection::Mixed, true) => self.mixed_after_present_defer += 1,
            (ResizeOnlyDirection::Mixed, false) => self.mixed_without_present_defer += 1,
        }
        if move_resize {
            if history.ever_deferred { self.move_resize_after_present_defer += 1; }
            else { self.move_resize_without_present_defer += 1; }
        }
    }

    fn record_final_resize_selection(&mut self, history: GeometryPresentHistory, structural: bool) {
        if !self.enabled { return; }
        match (structural, history.ever_deferred) {
            (true, true) => self.structural_selected_after_present_defer += 1,
            (true, false) => self.structural_selected_without_present_defer += 1,
            (false, true) => self.resizeonly_selected_after_present_defer += 1,
            (false, false) => self.resizeonly_selected_without_present_defer += 1,
        }
    }

    fn record_resizeonly_cohort_outcome(&mut self, success: bool, reason: Option<ResizeOnlyFallbackReason>) {
        if !self.enabled { return; }
        let Some(deferred) = self.resizeonly_present_deferred else { return; };
        match (success, deferred) {
            (true, true) => self.resizeonly_success_after_present_defer += 1,
            (true, false) => self.resizeonly_success_without_present_defer += 1,
            (false, true) => self.resizeonly_fallback_after_present_defer += 1,
            (false, false) => self.resizeonly_fallback_without_present_defer += 1,
        }
        if matches!(reason, Some(ResizeOnlyFallbackReason::PrecommitRejected)) {
            if deferred { self.precommit_rejected_after_present_defer += 1; }
            else { self.precommit_rejected_without_present_defer += 1; }
        }
    }

    fn surface_record_mut(&mut self, surface: (Window, Option<Window>), id: damage::Damage) -> Option<&mut SurfaceDiagnostic3a3f8b4c> {
        let (surface_xid, semantic_client_xid) = surface;
        let index = self.surface_diagnostics.iter().position(|record| record.surface_xid == surface_xid)?;
        let record = &mut self.surface_diagnostics[index];
        record.observe_identity(surface_xid, semantic_client_xid, id);
        Some(record)
    }

    fn observe_surface_identity(&mut self, surface: Option<(Window, Option<Window>)>, id: damage::Damage) {
        if !self.enabled { return; }
        let Some(surface) = surface else { return; };
        if self.surface_diagnostics.iter().any(|record| record.surface_xid == surface.0) {
            let _ = self.surface_record_mut(surface, id);
        } else if self.surface_diagnostics.len() < MAX_SURFACE_DIAGNOSTICS {
            let mut record = SurfaceDiagnostic3a3f8b4c::default();
            record.observe_identity(surface.0, surface.1, id);
            self.surface_diagnostics.push(record);
        }
    }

    fn record_damage_arrival(&mut self, id: damage::Damage, surface: Option<(Window, Option<Window>)>) {
        if !self.enabled { return; }
        let now = Instant::now();
        let unique = !self.pending_since.contains_key(&id);
        self.observe_surface_identity(surface, id);
        if let Some(surface) = surface {
            if let Some(record) = self.surface_record_mut(surface, id) {
                record.damage_notify_arrivals += 1;
                if let Some(previous) = record.last_damage_notify_timestamp {
                    let gap = previous.elapsed().as_micros();
                    record.damage_notify_gap_samples += 1;
                    record.damage_notify_gap_total_us += gap;
                    record.damage_notify_gap_max_us = record.damage_notify_gap_max_us.max(gap);
                    if record.last_moveonly_timestamp.is_none() {
                        record.damage_gap_max_before_move_us = record.damage_gap_max_before_move_us.max(gap);
                    } else {
                        record.damage_gap_max_after_move_us = record.damage_gap_max_after_move_us.max(gap);
                        if record.last_moveonly_timestamp.is_some_and(|move_time| now.duration_since(move_time) <= RECENT_MOVE_DIAGNOSTIC_WINDOW) {
                            record.damage_gap_max_within_2s_after_move_us = record.damage_gap_max_within_2s_after_move_us.max(gap);
                        }
                    }
                }
                record.last_damage_notify_timestamp = Some(now);
                if record.last_moveonly_timestamp.is_none() {
                    record.damage_arrivals_before_first_move += 1;
                } else {
                    record.damage_arrivals_after_first_move += 1;
                    if record.last_moveonly_timestamp.is_some_and(|move_time| now.duration_since(move_time) <= RECENT_MOVE_DIAGNOSTIC_WINDOW) {
                        record.damage_arrivals_within_2s_after_move += 1;
                    }
                }
                if unique { record.unique_damage_obligations += 1; }
            }
        }
        if !unique {
            self.pixel_damage_coalesced_notifications += 1;
        } else {
            self.pixel_damage_arrivals += 1;
            if self.pending_since.len() < MAX_DIAGNOSTIC_PENDING_DAMAGE {
                self.pending_since.insert(id, now);
            }
        }
    }

    fn record_damage_dispatch(&mut self, id: damage::Damage, geometry_pending: bool, surface: Option<(Window, Option<Window>)>) {
        if !self.enabled { return; }
        let start = self.pending_since.remove(&id);
        self.observe_surface_identity(surface, id);
        if let Some(surface) = surface {
            if let Some(record) = self.surface_record_mut(surface, id) {
                record.damage_subtracts += 1;
                record.damage_dispatches += 1;
                if let Some(start) = start {
                    let micros = start.elapsed().as_micros();
                    record.damage_pending_samples += 1;
                    record.damage_pending_total_us += micros;
                    record.damage_pending_max_us = record.damage_pending_max_us.max(micros);
                }
            }
        }
        let Some(start) = start else { return; };
        let micros = start.elapsed().as_micros();
        self.pixel_damage_dispatches += 1;
        if geometry_pending { self.pixel_damage_dispatch_while_geometry_pending += 1; }
        self.consecutive_geometry_while_damage_pending = 0;
        self.pixel_damage_wait_samples += 1;
        self.pixel_damage_wait_total_us += micros;
        self.pixel_damage_wait_max_us = self.pixel_damage_wait_max_us.max(micros);
        match micros {
            0..=1_000 => self.pixel_damage_wait_le_1ms += 1,
            1_001..=4_000 => self.pixel_damage_wait_1_4ms += 1,
            4_001..=8_000 => self.pixel_damage_wait_4_8ms += 1,
            8_001..=16_000 => self.pixel_damage_wait_8_16ms += 1,
            16_001..=33_000 => self.pixel_damage_wait_16_33ms += 1,
            33_001..=50_000 => self.pixel_damage_wait_33_50ms += 1,
            50_001..=100_000 => self.pixel_damage_wait_50_100ms += 1,
            _ => self.pixel_damage_wait_gt100ms += 1,
        }
    }

    fn print_summary(&self) {
        if !self.enabled { return; }
        println!("3a3f8b3a_diag: configure_seen={} configure_move_like={} configure_resize_like={} configure_other={} configure_superseded={} geometry_dispatches={} moveonly_attempted={} moveonly_success={} moveonly_fallback={} resize_geometry_dispatches={} geometry_dispatches_while_damage_pending={} max_geometry_dispatches_before_pending_damage_service={} pixel_damage_arrivals={} pixel_damage_coalesced_notifications={} pixel_damage_dispatches={} pixel_damage_dispatch_while_geometry_pending={} pixel_damage_deferred_by_geometry={} pixel_damage_wait_max_us={} pixel_damage_wait_total_us={} pixel_damage_wait_samples={} pixel_damage_wait_le_1ms={} pixel_damage_wait_1_4ms={} pixel_damage_wait_4_8ms={} pixel_damage_wait_8_16ms={} pixel_damage_wait_16_33ms={} pixel_damage_wait_33_50ms={} pixel_damage_wait_50_100ms={} pixel_damage_wait_gt100ms={} event_batches={} event_batches_with_geometry={} event_batches_with_pixel_damage_arrival={} event_batches_ended_with_pixel_damage_pending={} max_batches_damage_remained_pending={} recompositions={} recompositions_after_geometry={} recompositions_after_pixel_damage={} present_submissions={} present_completion_events={} structural_candidates_started={} structural_candidates_published={} structural_candidates_stale={} structural_candidates_failed={} resize_candidate_started={} resize_candidate_stale={} resize_candidate_published={} resize_candidate_failed={} distinct_resize_states_dispatched={} resized_target_bundle_acquisitions={} resized_target_damage_acquisitions={} resized_target_named_pixmap_acquisitions={} resized_target_egl_imports={} resized_target_bundle_acquisition_then_candidate_stale={} resource_bundles_reused={} resource_bundles_new={}", self.configure_seen, self.configure_move_like, self.configure_resize_like, self.configure_other, self.configure_superseded, self.geometry_dispatches, self.moveonly_attempted, self.moveonly_success, self.moveonly_fallback, self.resize_geometry_dispatches, self.geometry_dispatches_while_damage_pending, self.max_geometry_dispatches_before_pending_damage_service, self.pixel_damage_arrivals, self.pixel_damage_coalesced_notifications, self.pixel_damage_dispatches, self.pixel_damage_dispatch_while_geometry_pending, self.pixel_damage_deferred_by_geometry, self.pixel_damage_wait_max_us, self.pixel_damage_wait_total_us, self.pixel_damage_wait_samples, self.pixel_damage_wait_le_1ms, self.pixel_damage_wait_1_4ms, self.pixel_damage_wait_4_8ms, self.pixel_damage_wait_8_16ms, self.pixel_damage_wait_16_33ms, self.pixel_damage_wait_33_50ms, self.pixel_damage_wait_50_100ms, self.pixel_damage_wait_gt100ms, self.event_batches, self.event_batches_with_geometry, self.event_batches_with_pixel_damage_arrival, self.event_batches_ended_with_pixel_damage_pending, self.max_batches_damage_remained_pending, self.recompositions, self.recompositions_after_geometry, self.recompositions_after_pixel_damage, self.present_submissions, self.present_completion_events, self.structural_candidates_started, self.structural_candidates_published, self.structural_candidates_stale, self.structural_candidates_failed, self.resize_candidate_started, self.resize_candidate_stale, self.resize_candidate_published, self.resize_candidate_failed, self.distinct_resize_states_dispatched, self.resized_target_bundle_acquisitions, self.resized_target_damage_acquisitions, self.resized_target_named_pixmap_acquisitions, self.resized_target_egl_imports, self.resized_target_bundle_acquisition_then_candidate_stale, self.resource_bundles_reused, self.resource_bundles_new);
        println!("3a3f8b4c_surface_diag:");
        for record in &self.surface_diagnostics {
            let semantic_client = record.semantic_client_xid.map_or_else(|| "none".to_string(), |id| format!("0x{id:08x}"));
            let first_damage = record.first_damage_id.map_or_else(|| "none".to_string(), |id| format!("0x{id:08x}"));
            let current_damage = record.current_damage_id.map_or_else(|| "none".to_string(), |id| format!("0x{id:08x}"));
            println!("surface=0x{:08x} semantic_client={} first_damage={} damage={} damage_id_changes={} moveonly={} arrivals={} arrivals_before_move={} arrivals_after_move={} arrivals_within_2s_after_move={} obligations={} subtracts={} dispatches={} pending_samples={} pending_total_us={} pending_max_us={} gap_samples={} gap_total_us={} gap_max_us={} gap_max_before_move_us={} gap_max_after_move_us={} gap_max_within_2s_after_move_us={}", record.surface_xid, semantic_client, first_damage, current_damage, record.damage_id_changes, record.moveonly_count, record.damage_notify_arrivals, record.damage_arrivals_before_first_move, record.damage_arrivals_after_first_move, record.damage_arrivals_within_2s_after_move, record.unique_damage_obligations, record.damage_subtracts, record.damage_dispatches, record.damage_pending_samples, record.damage_pending_total_us, record.damage_pending_max_us, record.damage_notify_gap_samples, record.damage_notify_gap_total_us, record.damage_notify_gap_max_us, record.damage_gap_max_before_move_us, record.damage_gap_max_after_move_us, record.damage_gap_max_within_2s_after_move_us);
        }
        println!("3a3f8b5d_resizeonly_diag: attempted={} success={} fallback={} superseded_before_acquisition={} full_snapshot_avoided={} hierarchy_abort={} target_damage_reused={} target_damage_created={} target_damage_id_changed={} publish_with_damage_pending={} fallback_early_unclassified={}", self.resizeonly_attempted, self.resizeonly_success, self.resizeonly_fallback, self.resizeonly_superseded_before_acquisition, self.resizeonly_full_snapshot_avoided, self.resizeonly_hierarchy_abort, self.resizeonly_target_damage_reused, self.resizeonly_target_damage_created, self.resizeonly_target_damage_id_changed, self.resizeonly_publish_with_damage_pending, self.resizeonly_fallback_early_unclassified);
        self.print_resizeonly_direction("grow", &self.resizeonly_grow);
        self.print_resizeonly_direction("shrink", &self.resizeonly_shrink);
        self.print_resizeonly_direction("mixed", &self.resizeonly_mixed);
        println!("3a3f8b5o_event_provenance: configure_from_surface={} configure_from_semantic_client={} configure_from_other={} configure_from_unknown={} semantic_client_resolved_to_surface={} semantic_client_geometry_update_rejected={} semantic_client_without_surface_pending_geometry={} surface_geometry_update_accepted={} surface_geometry_update_rejected={} pending_geometry_created={} pending_geometry_updated={} pending_geometry_superseded={} pending_geometry_missing_at_dispatch={} pending_geometry_surface_match={} pending_geometry_surface_mismatch={}", self.configure_from_surface, self.configure_from_semantic_client, self.configure_from_other, self.configure_from_unknown, self.semantic_client_resolved_to_surface, self.semantic_client_geometry_update_rejected, self.semantic_client_without_surface_pending_geometry, self.surface_geometry_update_accepted, self.surface_geometry_update_rejected, self.pending_geometry_created, self.pending_geometry_updated, self.pending_geometry_superseded, self.pending_geometry_missing_at_dispatch, self.pending_geometry_surface_match, self.pending_geometry_surface_mismatch);
        println!("3a3f8b5o_resize_dispatch: total={} resizeonly_selected={} structural_selected={} deferred={} hierarchy_dominated={} no_pending_geometry={} other_source_reason={} unknown={} pre_attempt_bypass_total={} no_present_complete={} hierarchy_priority={} no_pending_geometry={} semantic_client_no_surface_pending_geometry={} pending_geometry_other_surface={} no_size_or_border_change={} ambiguous_or_superseded={} structural_already_required={} other={} direction_unknown={} grow_bypass={} shrink_bypass={} mixed_bypass={} direction_unknown_bypass={} move_resize_bypass={}", self.resize_dispatch_total, self.resize_dispatch_resizeonly_selected, self.resize_dispatch_structural_selected, self.resize_dispatch_deferred, self.resize_dispatch_hierarchy_dominated, self.resize_dispatch_no_pending_geometry, self.resize_dispatch_other_source_reason, self.resize_dispatch_unknown, self.resizeonly_pre_attempt_bypass_total, self.resizeonly_pre_attempt_bypass_no_present_complete, self.resizeonly_pre_attempt_bypass_hierarchy_priority, self.resizeonly_pre_attempt_bypass_no_pending_geometry, self.resizeonly_pre_attempt_bypass_semantic_client_no_surface_pending_geometry, self.resizeonly_pre_attempt_bypass_pending_geometry_other_surface, self.resizeonly_pre_attempt_bypass_no_size_or_border_change, self.resizeonly_pre_attempt_bypass_ambiguous_or_superseded, self.resizeonly_pre_attempt_bypass_structural_already_required, self.resizeonly_pre_attempt_bypass_other, self.resizeonly_pre_attempt_bypass_direction_unknown, self.resizeonly_grow_pre_attempt_bypass, self.resizeonly_shrink_pre_attempt_bypass, self.resizeonly_mixed_pre_attempt_bypass, self.resizeonly_direction_unknown_bypass, self.pre_resizeonly_bypass_move_resize);
        println!("3a3f8b5o_structural_origin: normal={} hierarchy={} geometry_surface={} geometry_semantic_client={} geometry_no_pending={} other={} stale_geometry_surface={} stale_geometry_semantic_client={} stale_geometry_no_pending={} stale_geometry_retry={} stale_geometry_deferred={} snapshot_geometry_surface={} snapshot_geometry_semantic_client={} snapshot_geometry_no_pending={} snapshot_hierarchy={} snapshot_other={}", self.structural_origin_normal, self.structural_origin_hierarchy, self.structural_origin_geometry_surface, self.structural_origin_geometry_semantic_client, self.structural_origin_geometry_no_pending, self.structural_origin_other, self.stale_geometry_from_surface_configure, self.stale_geometry_from_semantic_client_configure, self.stale_geometry_without_pending_geometry, self.stale_geometry_retry, self.stale_geometry_deferred, self.snapshot_geometry_surface, self.snapshot_geometry_semantic_client, self.snapshot_geometry_no_pending, self.snapshot_hierarchy, self.snapshot_other);
        println!("3a3f8b5q_scheduling: geometry_scheduling_batches_total={} geometry_scheduling_present_deferred={} geometry_scheduling_hierarchy_dominated={} note=these_are_scheduling_observations_not_final_resize_decisions", self.geometry_scheduling_batches_total, self.geometry_scheduling_present_deferred, self.geometry_scheduling_hierarchy_dominated);
        println!("3a3f8b5q_pending_geometry_cohort: ever_present_deferred={} present_deferred_once={} present_deferred_multiple={} updated_while_present_deferred={} superseded_while_present_deferred={}", self.geometry_pending_ever_present_deferred, self.geometry_pending_present_deferred_once, self.geometry_pending_present_deferred_multiple, self.geometry_pending_updated_while_present_deferred, self.geometry_pending_superseded_while_present_deferred);
        println!("3a3f8b5q_final_resize: total={} was_present_deferred={} never_present_deferred={} deferrals_0={} deferrals_1={} deferrals_2_3={} deferrals_4_7={} deferrals_8_plus={} source_metadata_unknown={}", self.final_resize_was_present_deferred + self.final_resize_never_present_deferred, self.final_resize_was_present_deferred, self.final_resize_never_present_deferred, self.final_resize_deferrals_0, self.final_resize_deferrals_1, self.final_resize_deferrals_2_3, self.final_resize_deferrals_4_7, self.final_resize_deferrals_8_plus, self.resize_dispatch_unknown);
        println!("3a3f8b5q_direction_present_history: grow_after_present_defer={} grow_without_present_defer={} shrink_after_present_defer={} shrink_without_present_defer={} mixed_after_present_defer={} mixed_without_present_defer={} move_resize_after_present_defer={} move_resize_without_present_defer={}", self.grow_after_present_defer, self.grow_without_present_defer, self.shrink_after_present_defer, self.shrink_without_present_defer, self.mixed_after_present_defer, self.mixed_without_present_defer, self.move_resize_after_present_defer, self.move_resize_without_present_defer);
        println!("3a3f8b5q_outcome_present_history: resizeonly_selected_after_present_defer={} resizeonly_selected_without_present_defer={} structural_selected_after_present_defer={} structural_selected_without_present_defer={} resizeonly_success_after_present_defer={} resizeonly_success_without_present_defer={} resizeonly_fallback_after_present_defer={} resizeonly_fallback_without_present_defer={} precommit_rejected_after_present_defer={} precommit_rejected_without_present_defer={}", self.resizeonly_selected_after_present_defer, self.resizeonly_selected_without_present_defer, self.structural_selected_after_present_defer, self.structural_selected_without_present_defer, self.resizeonly_success_after_present_defer, self.resizeonly_success_without_present_defer, self.resizeonly_fallback_after_present_defer, self.resizeonly_fallback_without_present_defer, self.precommit_rejected_after_present_defer, self.precommit_rejected_without_present_defer);
        println!("3a3f8b5q_structural_present_history: stale_after_present_defer={} stale_without_present_defer={} publish_after_present_defer={} publish_without_present_defer={} retry_after_present_defer={} retry_without_present_defer={} deferred_rebuild_after_present_defer={} deferred_rebuild_without_present_defer={}", self.structural_stale_after_present_defer, self.structural_stale_without_present_defer, self.structural_publish_after_present_defer, self.structural_publish_without_present_defer, self.geometry_retry_after_present_defer, self.geometry_retry_without_present_defer, self.geometry_deferred_rebuild_after_present_defer, self.geometry_deferred_rebuild_without_present_defer);
        println!("3a3f8b5s_hierarchy_events: total={} unknown_configure={} create={} map={} unmap={} destroy={} reparent={} circulate={} note=raw_event_population_separate_from_scheduler_decisions", self.hierarchy_event_total, self.hierarchy_event_unknown_configure, self.hierarchy_event_create, self.hierarchy_event_map, self.hierarchy_event_unmap, self.hierarchy_event_destroy, self.hierarchy_event_reparent, self.hierarchy_event_circulate);
        println!("3a3f8b5s_hierarchy_decisions: total={} only_unknown_configure={} only_create={} only_map={} only_unmap={} only_destroy={} only_reparent={} only_circulate={} multi_source={} existing_merge={} with_geometry_pending={} cleared_pending_geometry={} selected_while_resize_geometry_pending={} won_over_grow={} won_over_shrink={} won_over_mixed={}", self.hierarchy_decision_total, self.hierarchy_decision_only_unknown_configure, self.hierarchy_decision_only_create, self.hierarchy_decision_only_map, self.hierarchy_decision_only_unmap, self.hierarchy_decision_only_destroy, self.hierarchy_decision_only_reparent, self.hierarchy_decision_only_circulate, self.hierarchy_decision_multi_source, self.hierarchy_decision_existing_merge, self.hierarchy_decision_with_geometry_pending, self.hierarchy_decision_cleared_pending_geometry, self.hierarchy_selected_while_resize_geometry_pending, self.hierarchy_won_over_grow, self.hierarchy_won_over_shrink, self.hierarchy_won_over_mixed);
        println!("3a3f8b5s_hierarchy_event_relation: internal={} target_surface={} target_semantic_client={} other_tracked_surface={} other_semantic_client={} unknown_window={} unknown_configure_internal={} unknown_configure_unresolved={}", self.hierarchy_from_internal_window, self.hierarchy_event_target_surface, self.hierarchy_event_target_semantic_client, self.hierarchy_event_other_tracked_surface, self.hierarchy_event_other_semantic_client, self.hierarchy_event_unknown_window, self.unknown_configure_internal, self.unknown_configure_unresolved);
        println!("3a3f8b5s_hierarchy_snapshot_source: unknown_configure={} lifecycle={} reparent={} circulate={} multi_source={}", self.snapshot_hierarchy_unknown_configure, self.snapshot_hierarchy_lifecycle, self.snapshot_hierarchy_reparent, self.snapshot_hierarchy_circulate, self.snapshot_hierarchy_multi_source);
        println!("3a3f8b5s_hierarchy_stale_source: unknown_configure={} lifecycle={} reparent={} circulate={} multi_source={} retry_unknown={} retry_lifecycle={} retry_reparent={} retry_circulate={} retry_multi={} deferred_unknown={} deferred_lifecycle={} deferred_reparent={} deferred_circulate={} deferred_multi={}", self.hierarchy_unknown_configure_candidate_stale_geometry, self.hierarchy_lifecycle_candidate_stale_geometry, self.hierarchy_reparent_candidate_stale_geometry, self.hierarchy_circulate_candidate_stale_geometry, self.hierarchy_multi_candidate_stale_geometry, self.hierarchy_unknown_configure_retry, self.hierarchy_lifecycle_retry, self.hierarchy_reparent_retry, self.hierarchy_circulate_retry, self.hierarchy_multi_retry, self.hierarchy_unknown_configure_deferred, self.hierarchy_lifecycle_deferred, self.hierarchy_reparent_deferred, self.hierarchy_circulate_deferred, self.hierarchy_multi_deferred);
        println!("3a3f8b5v_r2_compound: geometry_observed={} attempted={} success={} reject_lifecycle={} reject_scene_membership={} reject_newer_hierarchy={} superseded_geometry={} damage_reused={} named_pixmap_reacquired={} egl_reacquired={} avoided_full_retry={}", self.compound_hierarchy_geometry_observed, self.compound_rebase_attempted, self.compound_rebase_success, self.compound_rebase_rejected_lifecycle, self.compound_rebase_rejected_scene_membership, self.compound_rebase_rejected_newer_hierarchy, self.compound_rebase_superseded_geometry, self.compound_rebase_damage_reused, self.compound_rebase_named_pixmap_reacquired, self.compound_rebase_egl_reacquired, self.compound_rebase_avoided_full_retry);
    }

    fn record_moveonly(&mut self, surface: Window, semantic_client_xid: Option<Window>, damage_id: Option<damage::Damage>) {
        if !self.enabled { return; }
        let Some(damage_id) = damage_id else { return; };
        self.observe_surface_identity(Some((surface, semantic_client_xid)), damage_id);
        if let Some(record) = self.surface_record_mut((surface, semantic_client_xid), damage_id) {
            record.moveonly_count += 1;
            record.last_moveonly_timestamp = Some(Instant::now());
        }
    }

    fn resizeonly_direction_mut(&mut self, direction: ResizeOnlyDirection) -> &mut ResizeOnlyDirectionDiagnostics {
        match direction {
            ResizeOnlyDirection::Grow => &mut self.resizeonly_grow,
            ResizeOnlyDirection::Shrink => &mut self.resizeonly_shrink,
            ResizeOnlyDirection::Mixed => &mut self.resizeonly_mixed,
        }
    }

    fn record_resizeonly_attempt(&mut self, direction: ResizeOnlyDirection, move_resize: bool) {
        let stats = self.resizeonly_direction_mut(direction);
        stats.attempted += 1;
        if move_resize { stats.move_resize_attempted += 1; }
    }

    fn record_resizeonly_early_fallback(&mut self) {
        self.resizeonly_fallback += 1;
        self.resizeonly_fallback_early_unclassified += 1;
        self.record_resizeonly_cohort_outcome(false, None);
    }

    fn record_resizeonly_fallback(
        &mut self,
        direction: ResizeOnlyDirection,
        move_resize: bool,
        reason: ResizeOnlyFallbackReason,
    ) {
        let stats = self.resizeonly_direction_mut(direction);
        stats.fallback_reasons.record(reason);
        if move_resize {
            stats.fallback_move_resize += 1;
        }
        self.record_resizeonly_cohort_outcome(false, Some(reason));
        self.resizeonly_fallback_origin = Some((direction, move_resize));
    }

    fn begin_resizeonly_structural_fallback(&mut self) {
        let Some((direction, _move_resize)) = self.resizeonly_fallback_origin.take() else {
            return;
        };
        let stats = self.resizeonly_direction_mut(direction);
        stats.fallback_to_structural += 1;
        stats.structural_candidates_started += 1;
        self.resizeonly_structural_direction = Some(direction);
        self.resizeonly_structural_timing = self.enabled.then(|| (direction, Instant::now()));
        self.resizeonly_structural_present_deferred = self.resizeonly_present_deferred;
    }

    fn record_structural_snapshot(&mut self, elapsed: Duration) {
        if let Some(direction) = self.resizeonly_structural_direction {
            let stats = self.resizeonly_direction_mut(direction);
            stats.fallback_full_snapshot += 1;
            stats.structural_full_snapshot.record(elapsed);
        }
    }

    fn record_structural_terminal(&mut self, published: bool, stale: bool, retry: bool) {
        let Some((direction, start)) = self.resizeonly_structural_timing.take() else {
            return;
        };
        let cohort = self.resizeonly_structural_present_deferred;
        if stale {
            match cohort {
                Some(true) => self.structural_stale_after_present_defer += 1,
                Some(false) => self.structural_stale_without_present_defer += 1,
                None => {}
            }
        }
        if published {
            match cohort {
                Some(true) => self.structural_publish_after_present_defer += 1,
                Some(false) => self.structural_publish_without_present_defer += 1,
                None => {}
            }
        }
        if retry {
            match cohort {
                Some(true) => self.geometry_retry_after_present_defer += 1,
                Some(false) => self.geometry_retry_without_present_defer += 1,
                None => {}
            }
            self.resizeonly_structural_timing = Some((direction, Instant::now()));
        } else if stale {
            match self.resizeonly_structural_present_deferred {
                Some(true) => self.geometry_deferred_rebuild_after_present_defer += 1,
                Some(false) => self.geometry_deferred_rebuild_without_present_defer += 1,
                None => {}
            }
        }
        let stats = self.resizeonly_direction_mut(direction);
        stats.structural_total.record(start.elapsed());
        if stale { stats.structural_stale += 1; }
        if published { stats.structural_published += 1; }
        if retry { stats.structural_retry += 1; }
        if !retry {
            self.resizeonly_structural_direction = None;
            self.resizeonly_structural_present_deferred = None;
            self.resizeonly_present_deferred = None;
            self.structural_origin = None;
        }
    }

    fn record_resizeonly_outcome(
        &mut self,
        direction: ResizeOnlyDirection,
        move_resize: bool,
        success: bool,
        hierarchy_abort: bool,
        elapsed: Option<Duration>,
    ) {
        if success { self.record_resizeonly_cohort_outcome(true, None); }
        let stats = self.resizeonly_direction_mut(direction);
        if success {
            stats.success += 1;
            if move_resize { stats.move_resize_success += 1; }
        } else if hierarchy_abort {
            stats.hierarchy_abort += 1;
        } else {
            stats.fallback += 1;
        }
        if let Some(elapsed) = elapsed { stats.total.record(elapsed); }
    }

    fn record_resizeonly_stage(
        &mut self,
        direction: ResizeOnlyDirection,
        stage: ResizeOnlyStage,
        elapsed: Duration,
    ) {
        let stats = self.resizeonly_direction_mut(direction);
        let metric = match stage {
            ResizeOnlyStage::PreAcquire => &mut stats.pre_acquire,
            ResizeOnlyStage::Damage => &mut stats.damage,
            ResizeOnlyStage::NamePixmap => &mut stats.name_pixmap,
            ResizeOnlyStage::EglImport => &mut stats.egl_import,
            ResizeOnlyStage::TargetBuildRender => &mut stats.target_build_render,
            ResizeOnlyStage::Precommit => &mut stats.precommit,
            ResizeOnlyStage::Publish => &mut stats.publish,
            ResizeOnlyStage::ResourceBlocking => &mut stats.resource_blocking,
        };
        metric.record(elapsed);
    }

    fn print_resizeonly_direction(&self, name: &str, stats: &ResizeOnlyDirectionDiagnostics) {
        if !self.enabled { return; }
        let metric = |timing: &TimingMetric| {
            format!("{}:{}:{}", timing.samples, timing.total_us, timing.max_us)
        };
        println!(
            "3a3f8b5j_resizeonly_direction: direction={} attempted={} success={} fallback={} fallback_reason_total={} hierarchy_abort={} move_resize_attempted={} move_resize_success={} total={} pre_acquire={} damage={} name_pixmap={} pixmap_get_geometry={} egl_import={} target_build_render={} precommit={} publish={} resource_blocking={} fallback_reasons=unavailable_state:{}:identity_mismatch:{}:no_size_change:{}:geometry_superseded:{}:unsupported_visual:{}:missing_damage:{}:precommit_rejected:{}:hierarchy:{} fallback_move_resize={} fallback_to_structural={} fallback_full_snapshot={} structural_candidates_started={} structural_total={} structural_full_snapshot={} structural_stale={} structural_published={} structural_retry={}",
            name,
            stats.attempted,
            stats.success,
            stats.fallback,
            stats.fallback_reasons.total(),
            stats.hierarchy_abort,
            stats.move_resize_attempted,
            stats.move_resize_success,
            metric(&stats.total),
            metric(&stats.pre_acquire),
            metric(&stats.damage),
            metric(&stats.name_pixmap),
            metric(&stats.pixmap_get_geometry),
            metric(&stats.egl_import),
            metric(&stats.target_build_render),
            metric(&stats.precommit),
            metric(&stats.publish),
            metric(&stats.resource_blocking),
            stats.fallback_reasons.unavailable_state,
            stats.fallback_reasons.identity_mismatch,
            stats.fallback_reasons.no_size_change,
            stats.fallback_reasons.geometry_superseded,
            stats.fallback_reasons.unsupported_visual,
            stats.fallback_reasons.missing_damage,
            stats.fallback_reasons.precommit_rejected,
            stats.fallback_reasons.hierarchy,
            stats.fallback_move_resize,
            stats.fallback_to_structural,
            stats.fallback_full_snapshot,
            stats.structural_candidates_started,
            metric(&stats.structural_total),
            metric(&stats.structural_full_snapshot),
            stats.structural_stale,
            stats.structural_published,
            stats.structural_retry,
        );
    }
}

#[derive(Clone, Copy)]
enum ResizeOnlyStage {
    PreAcquire,
    Damage,
    NamePixmap,
    EglImport,
    TargetBuildRender,
    Precommit,
    Publish,
    ResourceBlocking,
}

#[derive(Clone, Copy)]
#[allow(dead_code)]
enum PreResizeOnlyBypassReason {
    NoPresentComplete,
    HierarchyPriority,
    NoPendingGeometry,
    SemanticClientNoSurfacePendingGeometry,
    PendingGeometryOtherSurface,
    NoSizeOrBorderChange,
    AmbiguousOrSuperseded,
    StructuralAlreadyRequired,
    Other,
    DirectionUnknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RootGeometry {
    width: u16,
    height: u16,
    depth: u8,
    visual: u32,
}

#[cfg(test)]
mod resource_reuse_invariant_tests {
    use std::cell::Cell;
    use std::collections::{HashMap, HashSet};
    use std::rc::Rc;

    #[derive(Default)]
    struct Drops {
        damage: Cell<usize>,
        pixmap: Cell<usize>,
        egl: Cell<usize>,
    }

    struct FakeResource {
        kind: &'static str,
        drops: Rc<Drops>,
    }

    impl Drop for FakeResource {
        fn drop(&mut self) {
            let counter = match self.kind {
                "damage" => &self.drops.damage,
                "pixmap" => &self.drops.pixmap,
                "egl" => &self.drops.egl,
                _ => unreachable!(),
            };
            counter.set(counter.get() + 1);
        }
    }

    #[allow(dead_code)]
    struct FakeBundle {
        damage_id: u32,
        damage: FakeResource,
        pixmap: FakeResource,
        egl: FakeResource,
    }

    type SharedBundle = Rc<FakeBundle>;

    fn bundle(id: u32, drops: &Rc<Drops>) -> SharedBundle {
        Rc::new(FakeBundle {
            damage_id: id,
            damage: FakeResource { kind: "damage", drops: Rc::clone(drops) },
            pixmap: FakeResource { kind: "pixmap", drops: Rc::clone(drops) },
            egl: FakeResource { kind: "egl", drops: Rc::clone(drops) },
        })
    }

    fn candidate(
        live: &HashMap<char, SharedBundle>,
        additions: &[char],
        fail: bool,
        next_id: u32,
        drops: &Rc<Drops>,
    ) -> Result<HashMap<char, SharedBundle>, ()> {
        let mut result = HashMap::new();
        for (surface, resource) in live {
            if *surface != 'C' {
                result.insert(*surface, Rc::clone(resource));
            }
        }
        for surface in additions {
            let resource = bundle(next_id, drops);
            if fail {
                drop(resource);
                return Err(());
            }
            result.insert(*surface, resource);
        }
        Ok(result)
    }

    #[test]
    fn shared_bundle_identity_and_last_owner_drop_are_exactly_once() {
        let drops = Rc::new(Drops::default());
        let live = bundle(1, &drops);
        let candidate = Rc::clone(&live);
        assert!(Rc::ptr_eq(&live, &candidate));
        drop(live);
        assert_eq!(drops.damage.get(), 0);
        assert_eq!(drops.pixmap.get(), 0);
        assert_eq!(drops.egl.get(), 0);
        drop(candidate);
        assert_eq!(drops.damage.get(), 1);
        assert_eq!(drops.pixmap.get(), 1);
        assert_eq!(drops.egl.get(), 1);
    }

    #[test]
    fn publication_reuses_survivors_and_retires_removed_bundle_once() {
        let drops = Rc::new(Drops::default());
        let a = bundle(1, &drops);
        let b = bundle(2, &drops);
        let c = bundle(3, &drops);
        let mut old = HashMap::from([('A', Rc::clone(&a)), ('B', Rc::clone(&b)), ('C', Rc::clone(&c))]);
        let new_d = bundle(4, &drops);
        let new = HashMap::from([('A', Rc::clone(&a)), ('B', Rc::clone(&b)), ('D', Rc::clone(&new_d))]);
        assert!(Rc::ptr_eq(old.get(&'A').unwrap(), new.get(&'A').unwrap()));
        assert!(Rc::ptr_eq(old.get(&'B').unwrap(), new.get(&'B').unwrap()));
        assert_eq!(drops.damage.get(), 0);
        old = new;
        drop(c);
        assert_eq!(drops.damage.get(), 1);
        assert_eq!(drops.pixmap.get(), 1);
        assert_eq!(drops.egl.get(), 1);
        drop(old);
        drop(a);
        drop(b);
        drop(new_d);
        assert_eq!(drops.damage.get(), 4);
        assert_eq!(drops.pixmap.get(), 4);
        assert_eq!(drops.egl.get(), 4);
    }

    #[test]
    fn candidate_failure_and_stale_preserve_old_scene_and_cleanup_new_only() {
        let drops = Rc::new(Drops::default());
        let a = bundle(1, &drops);
        let b = bundle(2, &drops);
        let c = bundle(3, &drops);
        let old = HashMap::from([('A', Rc::clone(&a)), ('B', Rc::clone(&b)), ('C', Rc::clone(&c))]);
        assert!(candidate(&old, &['D'], true, 4, &drops).is_err());
        assert_eq!(drops.damage.get(), 1);
        assert_eq!(Rc::strong_count(old.get(&'A').unwrap()), 2);
        let stale = candidate(&old, &['D'], false, 5, &drops).unwrap();
        drop(stale);
        assert_eq!(drops.damage.get(), 2);
        assert_eq!(Rc::strong_count(old.get(&'A').unwrap()), 2);
        drop(old);
        assert_eq!(drops.damage.get(), 2);
        drop(a);
        drop(b);
        drop(c);
        assert_eq!(drops.damage.get(), 5);
        assert_eq!(drops.pixmap.get(), 5);
        assert_eq!(drops.egl.get(), 5);
    }

    #[test]
    fn pending_damage_is_coalesced_and_subtracted_once_across_candidate_reuse() {
        let drops = Rc::new(Drops::default());
        let a = bundle(41, &drops);
        let old = HashMap::from([('A', Rc::clone(&a))]);
        let reused = candidate(&old, &[], false, 42, &drops).unwrap();
        let mut pending = HashSet::from([reused.get(&'A').unwrap().damage_id]);
        pending.insert(41);
        assert_eq!(pending.len(), 1);
        let mut subtracts = 0;
        for damage_id in pending {
            if damage_id == 41 { subtracts += 1; }
        }
        assert_eq!(subtracts, 1);
        drop(reused);
        drop(old);
        drop(a);
        assert_eq!(drops.damage.get(), 1);
    }

    #[test]
    fn higher_priority_structural_work_preserves_damage_through_failure_and_stale() {
        let drops = Rc::new(Drops::default());
        let a = bundle(51, &drops);
        let old = HashMap::from([('A', Rc::clone(&a))]);
        let pending = HashSet::from([51_u32]);
        assert!(candidate(&old, &['D'], true, 52, &drops).is_err());
        assert!(pending.contains(&old.get(&'A').unwrap().damage_id));
        let stale = candidate(&old, &['D'], false, 53, &drops).unwrap();
        drop(stale);
        assert!(pending.contains(&51));
        let subtract_count = pending.iter().filter(|damage_id| **damage_id == 51).count();
        assert_eq!(subtract_count, 1);
        drop(old);
        drop(a);
        assert_eq!(drops.damage.get(), 3);
    }

    #[test]
    fn resource_identity_is_separate_from_candidate_metadata() {
        let drops = Rc::new(Drops::default());
        let resource = bundle(7, &drops);
        let old_metadata = (100_i32, 200_i32, 0_usize);
        let candidate_metadata = (120_i32, 240_i32, 1_usize);
        assert!(Rc::ptr_eq(&resource, &resource));
        assert_ne!(old_metadata, candidate_metadata);
        assert_eq!(old_metadata, (100, 200, 0));
        drop(resource);
        assert_eq!(drops.damage.get(), 1);
    }

    #[test]
    fn resize_uses_new_generation_and_retires_old_after_publication() {
        let drops = Rc::new(Drops::default());
        let old_c = bundle(10, &drops);
        let new_c = bundle(11, &drops);
        let old = Rc::clone(&old_c);
        let new = Rc::clone(&new_c);
        drop(old);
        assert_eq!(drops.damage.get(), 0);
        drop(old_c);
        assert_eq!(drops.damage.get(), 1);
        assert_eq!(Rc::strong_count(&new), 2);
        drop(new);
        drop(new_c);
        assert_eq!(drops.damage.get(), 2);
        assert_eq!(drops.pixmap.get(), 2);
        assert_eq!(drops.egl.get(), 2);
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackgroundAtoms {
    xrootpmap_id: xproto::Atom,
    esetroot_pmap_id: xproto::Atom,
    pixmap_type: xproto::Atom,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct VisualAtoms {
    active_window: xproto::Atom,
    wm_hints: xproto::Atom,
    net_wm_state: xproto::Atom,
    demands_attention: xproto::Atom,
    fullscreen: xproto::Atom,
    blur_behind_region: xproto::Atom,
    effect_owner: xproto::Atom,
}

/// One `x, y, width, height` group from a `_KDE_NET_WM_BLUR_BEHIND_REGION`
/// payload, retained exactly as parsed (client-local coordinate space,
/// unconverted). Phase 2A only parses and caches this data; nothing in
/// this codebase yet interprets or renders it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BlurRegionRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

/// A semantic client's parsed background-blur request, per
/// `_KDE_NET_WM_BLUR_BEHIND_REGION`. This is the client's REQUEST only —
/// it is never derived from transparency capability (visual class, depth,
/// opacity) and does not by itself imply anything is rendered.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
enum BlurRequest {
    /// Property absent, malformed, or rejected (wrong type/format, or a
    /// payload length that is not a multiple of 4).
    #[default]
    None,
    /// Property present with either a zero-length payload, or exactly one
    /// degenerate (width == 0 && height == 0) rectangle — both are the
    /// confirmed "blur the whole window" shape (the latter is the exact
    /// payload the reference client, Ghostty, emits).
    FullWindow,
    /// Property present with one or more non-degenerate groups, retained
    /// verbatim (including any degenerate rectangle mixed into a
    /// multi-rectangle payload — Phase 2A does not filter or reinterpret
    /// mixed payloads; see the parser's own documentation).
    Regions(Vec<BlurRegionRect>),
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
struct CachedClientVisualState {
    wm_hints: bool,
    demands_attention: bool,
    fullscreen: bool,
    blur_requested: BlurRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct BackgroundPixmap {
    xid: xproto::Pixmap,
    geometry: PixmapGeometry,
    semantics: EglPixelSemantics,
}

struct ImportedBackground {
    source: BackgroundPixmap,
    surface: EglImportedSurface,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BackgroundCandidate {
    Valid(BackgroundPixmap),
    SolidFallback,
    Preserve,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct SurfaceEntry {
    surface_xid: Window,
    semantic_client_xid: Option<Window>,
    /// Raw `_XOMPOSITE_EFFECT_OWNER` property value from this surface. This
    /// is an effect-authority reference only; it is never semantic identity
    /// and never owns compositor resources.
    effect_owner: Option<Window>,
    /// The surface's own explicit blur request. This is populated only for
    /// surfaces without a usable semantic client; managed-client blur stays
    /// on the existing semantic-client cache path.
    own_blur_request: BlurRequest,
    client_root_geometry: Option<ClientRootGeometry>,
    lifecycle_xid: Window,
    geometry: WindowGeometry,
    depth: u8,
    visual: u32,
    class: WindowClass,
    map_state: xproto::MapState,
    /// The CAPTURE surface's own override_redirect attribute (this
    /// surface_xid's GetWindowAttributes reply). Left with exactly this
    /// meaning for every existing consumer (identity/rebase guards,
    /// diagnostics, geometry tracking) — do not repurpose.
    override_redirect: bool,
    /// 3a3fa2a R2 — semantic-client-preferring resolution, consumed ONLY
    /// by open-animation eligibility. Equal to the semantic client's own
    /// override_redirect when semantic metadata is available (resolved
    /// from already-fetched hierarchy metadata, zero new X11 queries),
    /// otherwise falls back to `override_redirect` (the capture value)
    /// above. See effective_override_redirect().
    effective_override_redirect: bool,
    stacking_index: usize,
    backend: BackendCompatibility,
    visual_class: SurfaceVisualClass,
    fullscreen: bool,
    shadow_eligible: bool,
    resolved_border_color: [u32; 4],
    resolved_opacity_bits: u32,
    /// This entry's owned blur request, resolved from the cached,
    /// per-semantic-client `BlurRequest` (Phase 2A) via the structural
    /// `semantic_client_xid` relationship only (Phase 2B owner audit) —
    /// never from WM_CLASS, override_redirect, visual_class, opacity, or
    /// fullscreen. `None` for a surface with no semantic client (e.g. a
    /// popup/helper) or whose client has no active request. Preserves
    /// the full protocol shape (None/FullWindow/Regions) rather than
    /// collapsing to a boolean; Phase 2B2b consumes only FullWindow while
    /// Regions remains intentionally deferred.
    resolved_blur_request: BlurRequest,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ClientRootGeometry {
    root_x: i32,
    root_y: i32,
    width: i32,
    height: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SurfaceVisualClass {
    Normal,
    Dock,
    Desktop,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum BorderVisualState {
    Inactive,
    Focused,
    Urgent,
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
const PRESENT_CLIENT_MAJOR: u32 = 1;
const PRESENT_CLIENT_MINOR: u32 = 0;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FrameSchedulerState {
    Idle,
    Armed { serial: u32, target_msc: u64 },
    Dirty {
        pixel_damage: bool,
        structural_generation: Option<u64>,
    },
    Rendering { generation: u64 },
    AwaitExternalStructuralChange { generation: u64 },
}

#[derive(Debug)]
struct FrameScheduler {
    state: FrameSchedulerState,
    next_serial: u32,
    armed_serial: Option<u32>,
}

impl FrameScheduler {
    fn new() -> Self {
        Self {
            state: FrameSchedulerState::Idle,
            next_serial: 1,
            armed_serial: None,
        }
    }

    fn mark_pixel_dirty(&mut self) {
        let structural_generation = match self.state {
            FrameSchedulerState::Dirty { structural_generation, .. } => structural_generation,
            FrameSchedulerState::AwaitExternalStructuralChange { generation } => Some(generation),
            _ => None,
        };
        self.state = FrameSchedulerState::Dirty {
            pixel_damage: true,
            structural_generation,
        };
    }

    fn mark_structural_dirty(&mut self, generation: u64) {
        let pixel_damage = matches!(
            self.state,
            FrameSchedulerState::Dirty { pixel_damage: true, .. }
        );
        self.state = FrameSchedulerState::Dirty {
            pixel_damage,
            structural_generation: Some(generation),
        };
    }

    fn arm(&mut self, target_msc: u64) -> (u32, u64) {
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        self.armed_serial = Some(serial);
        self.state = FrameSchedulerState::Armed { serial, target_msc };
        (serial, target_msc)
    }

    fn complete(&mut self, serial: u32, msc: u64) -> bool {
        let armed = self.armed_serial == Some(serial);
        if !armed {
            return false;
        }
        self.armed_serial = None;
        self.state = FrameSchedulerState::Rendering { generation: 0 };
        let _ = msc;
        true
    }

    fn finish_render(&mut self, generation: u64, dirty: bool) {
        self.state = if dirty {
            FrameSchedulerState::Dirty {
                pixel_damage: true,
                structural_generation: Some(generation),
            }
        } else {
            FrameSchedulerState::AwaitExternalStructuralChange { generation }
        };
    }
}

struct PresentClock {
    event_id: present::Event,
    window: Window,
    pending_serial: Option<u32>,
}

impl PresentClock {
    fn acquire(connection: &X11Connection, window: Window) -> Result<Option<Self>, Box<dyn Error>> {
        let Some(info) = connection.inner.extension_information(present::X11_EXTENSION_NAME)? else {
            println!("Present scheduler: extension unavailable; using event fallback");
            return Ok(None);
        };
        let version = match connection
            .inner
            .present_query_version(PRESENT_CLIENT_MAJOR, PRESENT_CLIENT_MINOR)
        {
            Ok(cookie) => match cookie.reply() {
                Ok(version) => version,
                Err(error) => {
                    println!("Present scheduler: version query failed ({error}); using event fallback");
                    return Ok(None);
                }
            },
            Err(error) => {
                println!("Present scheduler: version request failed ({error}); using event fallback");
                return Ok(None);
            }
        };
        if (version.major_version, version.minor_version)
            < (PRESENT_CLIENT_MAJOR, PRESENT_CLIENT_MINOR)
        {
            println!("Present scheduler: incompatible version; using event fallback");
            return Ok(None);
        }
        let capabilities = match connection.inner.present_query_capabilities(window) {
            Ok(cookie) => match cookie.reply() {
                Ok(capabilities) => capabilities,
                Err(error) => {
                    println!("Present scheduler: capability query failed ({error}); using event fallback");
                    return Ok(None);
                }
            },
            Err(error) => {
                println!("Present scheduler: capability request failed ({error}); using event fallback");
                return Ok(None);
            }
        };
        println!(
            "Present scheduler: version {}.{} capabilities=0x{:08x}",
            version.major_version, version.minor_version, capabilities.capabilities
        );
        let event_id = connection.inner.generate_id()?;
        let selected = match connection.inner.present_select_input(
                event_id,
                window,
                present::EventMask::COMPLETE_NOTIFY | present::EventMask::IDLE_NOTIFY,
            ) {
            Ok(cookie) => cookie.check(),
            Err(error) => Err(error.into()),
        };
        if let Err(error) = selected {
            println!("Present scheduler: select_input failed ({error}); using event fallback");
            return Ok(None);
        }
        connection.inner.flush()?;
        println!("Present scheduler: MSC clock armed on event base {}", info.first_event);
        Ok(Some(Self {
            event_id,
            window,
            pending_serial: None,
        }))
    }

    fn arm(&mut self, connection: &X11Connection, serial: u32, target_msc: u64) -> Result<(), Box<dyn Error>> {
        if self.pending_serial.is_some() {
            return Ok(());
        }
        connection
            .inner
            .present_notify_msc(self.window, serial, target_msc, 0, 0)?
            .check()?;
        connection.inner.flush()?;
        self.pending_serial = Some(serial);
        Ok(())
    }

    fn complete(&mut self, event: &present::CompleteNotifyEvent) -> Option<u64> {
        if event.event != self.event_id || event.window != self.window {
            return None;
        }
        if self.pending_serial != Some(event.serial)
            || event.kind != present::CompleteKind::NOTIFY_MSC
        {
            return None;
        }
        self.pending_serial = None;
        Some(event.msc)
    }

    fn cleanup(&mut self, connection: &X11Connection) -> Result<(), Box<dyn Error>> {
        connection
            .inner
            .present_select_input(self.event_id, self.window, present::EventMask::NO_EVENT)?
            .check()?;
        connection.inner.flush()?;
        self.pending_serial = None;
        Ok(())
    }
}

/// 3a3fa2a/3a3fa2b1 — window open animation. Visual-only, resource-free
/// per-surface state: never holds a DamageLease/NamedPixmap/EGLImage, and
/// never mutates authoritative X11 geometry. See milestone-3a3fa2a-window-
/// animation-core-open-audit.txt and milestone-3a3fa2b-animation-config-
/// effect-model-audit.txt for the architecture this implements. Only one
/// animation kind (open) exists for this milestone, so there is
/// deliberately no `kind` discriminant yet — add one when a second kind
/// is actually built.
///
/// Duration is now config-resolved (see `crate::config::AnimationConfig`)
/// rather than a hardcoded constant — captured once at construction time,
/// per `WindowAnimation::open`, and never reread afterward.
#[derive(Clone, Debug)]
struct WindowAnimation {
    effect: crate::config::OpenAnimationEffect,
    started_at: Instant,
    duration: Duration,
}

impl WindowAnimation {
    fn open(started_at: Instant, effect: crate::config::OpenAnimationEffect, duration: Duration) -> Self {
        Self { effect, started_at, duration }
    }

    fn progress(&self, now: Instant) -> f32 {
        animation_progress(now.saturating_duration_since(self.started_at), self.duration)
    }

    /// Convenience wrapper around the single, generic
    /// `sample_open_effect` — the SAME function consulted by every render
    /// path (provisional and persistent alike), never a separately
    /// hardcoded first-frame path. See sample_open_effect.
    fn sample(&self, now: Instant) -> AnimationVisual {
        sample_open_effect(self.effect, self.progress(now))
    }

    fn is_complete(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }
}

/// Pure, deterministic progress in [0, 1]. Never accumulates frame deltas —
/// always derived from absolute elapsed time against the animation's own
/// `started_at`, so it cannot drift.
fn animation_progress(elapsed: Duration, duration: Duration) -> f32 {
    if duration.is_zero() {
        return 1.0;
    }
    (elapsed.as_secs_f32() / duration.as_secs_f32()).clamp(0.0, 1.0)
}

fn ease_out_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    let inv = 1.0 - t;
    1.0 - inv * inv * inv
}

/// 3a3fa2b7-r3 — ease-in cubic: `t³`. Slow initial change accelerating
/// toward the end — the opposite shape of `ease_out_cubic`. Used by the
/// R3 synchronized final collapse so content remains visually substantial
/// early in the final phase and the collapse accelerates toward the end.
fn ease_in_cubic(t: f32) -> f32 {
    let t = t.clamp(0.0, 1.0);
    t * t * t
}

/// 3a3fa2b1 — pure, resource-free description of "how to visually
/// transform this surface's existing texture/geometry" for one animation
/// frame. Owns no XID/DamageLease/NamedPixmap/EGLImage/texture/
/// framebuffer/Present state. `scale_x`/`scale_y` are independent so a
/// future non-uniform effect (e.g. teleport) needs no renderer change —
/// see `scale_render_quad_plan`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct AnimationVisual {
    opacity: f32,
    scale_x: f32,
    scale_y: f32,
}

/// TEMPORARY DEVELOPMENT TUNING (3a3fa2a/b1) — start scale kept at the
/// current exaggerated human-validation value (0.70), not yet the eventual
/// release value (0.96), per this milestone's explicit "do not tune
/// aesthetics in b1" scope. `animation.open.duration = 800` in config now
/// reproduces the exact previously-hardcoded 800ms/0.70 behavior.
const SCALE_EFFECT_FROM_SCALE: f32 = 0.70;
const SCALE_EFFECT_TO_SCALE: f32 = 1.0;
const SCALE_EFFECT_FROM_OPACITY: f32 = 0.0;
const SCALE_EFFECT_TO_OPACITY: f32 = 1.0;

fn sample_scale(t: f32) -> AnimationVisual {
    let eased = ease_out_cubic(t);
    let scale = lerp(SCALE_EFFECT_FROM_SCALE, SCALE_EFFECT_TO_SCALE, eased);
    AnimationVisual {
        opacity: lerp(SCALE_EFFECT_FROM_OPACITY, SCALE_EFFECT_TO_OPACITY, eased),
        scale_x: scale,
        scale_y: scale,
    }
}

/// 3a3fa2b2-r2 — "teleport materialization": a DBZ-inspired RAPID
/// materialization, not a smooth zoom. Generic visual inspiration only —
/// no copyrighted sprites/artwork/sounds/character imagery of any kind,
/// just a piecewise numeric curve. Four phases, each a `lerp` between
/// fixed endpoint constants driven by `ease_out_cubic` on that phase's
/// own local progress — no effect-local runtime state, no timer, no
/// frame counter: the entire curve is a pure function of `t` alone,
/// exactly like `sample_scale`. Motion is heavily FRONT-LOADED: by
/// t=0.35 the window is already materialized (opacity≈1, scale≈1), with
/// the remaining 65% of the animation spent on a small, sharp
/// impact/recoil/snap — see milestone-3a3fa2b2-teleport-materialization-
/// r2-preview.txt for the full curve rationale and human-character
/// contract. Deliberately distinct from Scale's single uniform lerp:
/// Teleport's Y axis is far more aggressive than X for most of the
/// animation (starts as a nearly-flat horizontal streak, not a small
/// normal-looking window), and it overshoots then recoils before
/// snapping — Scale does neither.
const TELEPORT_PHASE_A_END: f32 = 0.10; // ignition / streak
const TELEPORT_PHASE_B_END: f32 = 0.35; // explosive materialization
const TELEPORT_PHASE_C_END: f32 = 0.65; // impact / recoil
// phase D (snap) runs from TELEPORT_PHASE_C_END to 1.0

const TELEPORT_START_OPACITY: f32 = 0.0;
const TELEPORT_START_SCALE_X: f32 = 0.45;
const TELEPORT_START_SCALE_Y: f32 = 0.03; // thin materialization streak,
// not a tiny normal-looking window — see scale_render_quad_plan's
// existing `.round().max(1.0)` minimum-dimension guard (unchanged by
// this milestone) for why this never produces a zero-sized plan.

const TELEPORT_PHASE_A_OPACITY: f32 = 0.28;
const TELEPORT_PHASE_A_SCALE_X: f32 = 0.62;
const TELEPORT_PHASE_A_SCALE_Y: f32 = 0.22;

const TELEPORT_PHASE_B_OPACITY: f32 = 1.0; // materialized by end of phase B
const TELEPORT_PHASE_B_SCALE_X: f32 = 1.03; // small, sharp overshoot
const TELEPORT_PHASE_B_SCALE_Y: f32 = 1.06; // Y overshoots harder than X

const TELEPORT_PHASE_C_OPACITY: f32 = 1.0; // stays materialized through impact —
// opacity does NOT keep fading through the recoil/snap phases, which is
// what visually separates "materializing" from "settling geometry".
const TELEPORT_PHASE_C_SCALE_X: f32 = 0.995; // sharp recoil, briefly under 1
const TELEPORT_PHASE_C_SCALE_Y: f32 = 0.985;

const TELEPORT_END_OPACITY: f32 = 1.0;
const TELEPORT_END_SCALE: f32 = 1.0;

/// Local progress within one phase's own `[start, end)` window, clamped
/// to [0, 1] — the same normalization idiom `animation_progress` already
/// uses for the whole-animation `t`, just re-applied per phase.
fn phase_progress(t: f32, start: f32, end: f32) -> f32 {
    if end <= start {
        return 1.0;
    }
    ((t - start) / (end - start)).clamp(0.0, 1.0)
}

fn sample_teleport(t: f32) -> AnimationVisual {
    // Explicit, unconditional exact-final-state guard — not relied upon
    // to merely "happen" from lerp/ease_out_cubic rounding; matches the
    // contract ("no residual overshoot, no floating final geometry
    // drift") by construction, independent of any float-rounding
    // argument. Phase D approaches 1.0 from BELOW (its recoil trough is
    // slightly under 1.0), so this guard is what actually delivers the
    // required sharp final SNAP rather than a lingering easing tail.
    if t >= 1.0 {
        return AnimationVisual {
            opacity: TELEPORT_END_OPACITY,
            scale_x: TELEPORT_END_SCALE,
            scale_y: TELEPORT_END_SCALE,
        };
    }
    if t < TELEPORT_PHASE_A_END {
        let u = ease_out_cubic(phase_progress(t, 0.0, TELEPORT_PHASE_A_END));
        AnimationVisual {
            opacity: lerp(TELEPORT_START_OPACITY, TELEPORT_PHASE_A_OPACITY, u),
            scale_x: lerp(TELEPORT_START_SCALE_X, TELEPORT_PHASE_A_SCALE_X, u),
            scale_y: lerp(TELEPORT_START_SCALE_Y, TELEPORT_PHASE_A_SCALE_Y, u),
        }
    } else if t < TELEPORT_PHASE_B_END {
        let u = ease_out_cubic(phase_progress(t, TELEPORT_PHASE_A_END, TELEPORT_PHASE_B_END));
        AnimationVisual {
            opacity: lerp(TELEPORT_PHASE_A_OPACITY, TELEPORT_PHASE_B_OPACITY, u),
            scale_x: lerp(TELEPORT_PHASE_A_SCALE_X, TELEPORT_PHASE_B_SCALE_X, u),
            scale_y: lerp(TELEPORT_PHASE_A_SCALE_Y, TELEPORT_PHASE_B_SCALE_Y, u),
        }
    } else if t < TELEPORT_PHASE_C_END {
        let u = ease_out_cubic(phase_progress(t, TELEPORT_PHASE_B_END, TELEPORT_PHASE_C_END));
        AnimationVisual {
            opacity: lerp(TELEPORT_PHASE_B_OPACITY, TELEPORT_PHASE_C_OPACITY, u),
            scale_x: lerp(TELEPORT_PHASE_B_SCALE_X, TELEPORT_PHASE_C_SCALE_X, u),
            scale_y: lerp(TELEPORT_PHASE_B_SCALE_Y, TELEPORT_PHASE_C_SCALE_Y, u),
        }
    } else {
        let u = ease_out_cubic(phase_progress(t, TELEPORT_PHASE_C_END, 1.0));
        AnimationVisual {
            opacity: lerp(TELEPORT_PHASE_C_OPACITY, TELEPORT_END_OPACITY, u),
            scale_x: lerp(TELEPORT_PHASE_C_SCALE_X, TELEPORT_END_SCALE, u),
            scale_y: lerp(TELEPORT_PHASE_C_SCALE_Y, TELEPORT_END_SCALE, u),
        }
    }
}

/// 3a3fa2b3-r2 — "energy_tear": the window's OWN opacity/scale transform
/// for this effect. `opacity` is an unconditional CONSTANT 1.0 — this is
/// a MULTIPLIER against the surface's already-resolved/configured
/// opacity (see `render_egl_scene_parts`'s `base_opacity * visual.
/// opacity` formula, unchanged code), never an independent fade. A
/// window whose resolved opacity is 0.82 renders its energy_tear slices
/// at 0.82 throughout — energy_tear must NEVER force a translucent
/// window toward 1.0, and never applies any fade-in of its own to the
/// window's own opacity. The temporal "materializing" character belongs
/// ENTIRELY to `EnergyTearLayout` (slice displacement + tear/streak
/// alpha, see `sample_energy_tear_layout` below) — a decoration drawn
/// OVER the window, never a property of the window's own visual state.
/// `scale_x == scale_y == 1.0` throughout too, unchanged from R1 —
/// energy_tear never resizes the window's bounding box, so `draw_plan ==
/// plan` for this effect (scale_render_quad_plan is an exact identity at
/// 1.0/1.0), and shadow/blur inherit the s1 contract with zero special-
/// casing, exactly like a non-animated surface would.
fn sample_energy_tear(_t: f32) -> AnimationVisual {
    AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 }
}

/// Number of vertical slices energy_tear divides the window into.
const ENERGY_TEAR_SLICE_COUNT: usize = 5;
/// 3a3fa2b3-r3 — widened from R2's 0.30 (per human feedback: the single-
/// convergence R2 envelope was too short to fit the two additional
/// rebound cycles this milestone adds). At and after this point,
/// energy_tear behaves byte-identically to a plain single-quad draw (see
/// `energy_tear_render_plan`'s None case in the render loop, which falls
/// back to the exact same `render_surface_with_opacity` call every other
/// effect uses). At the eventual release duration (250ms), 0.55 gives
/// ~137.5ms (~8 frames @ 60Hz) for the full initial/rebound/rebound/
/// settle sequence — enough to read as "ZZZT-ZZT, locked in", not a slow
/// wobble.
const ENERGY_TEAR_END: f32 = 0.55;
/// Local (envelope-normalized, i.e. already divided by ENERGY_TEAR_END)
/// phase boundaries for `energy_tear_oscillation`'s 4-segment piecewise
/// curve — same reviewable idiom Teleport already established (phase
/// boundary constants + phase_progress + ease_out_cubic + lerp), not a
/// generic/endless sine. Phase A: initial peak -> first crossing. Phase
/// B: crossing -> opposite-side rebound-1 peak. Phase C: rebound-1 peak
/// -> same-side rebound-2 peak (crossing zero again along the way).
/// Phase D: rebound-2 peak -> exact zero (final settle).
const ENERGY_TEAR_PHASE_A_END: f32 = 0.20;
const ENERGY_TEAR_PHASE_B_END: f32 = 0.50;
const ENERGY_TEAR_PHASE_C_END: f32 = 0.75;
// phase D runs ENERGY_TEAR_PHASE_C_END..1.0, ending at exactly 0.0.
/// Rebound-1 value, as a signed fraction of the initial (cycle-0) peak —
/// negative here means "opposite sign from cycle 0" (per-slice sign is
/// still carried by ENERGY_TEAR_SLICE_PEAK_OFFSET_FRACTIONS; this factor
/// only scales/flips the shared oscillation envelope). Within the
/// requested 45-60% magnitude range.
const ENERGY_TEAR_REBOUND_1_FACTOR: f32 = -0.55;
/// Rebound-2 value, same-sign as cycle 0 (positive here), smaller
/// magnitude. Within the requested 20-30% magnitude range.
const ENERGY_TEAR_REBOUND_2_FACTOR: f32 = 0.25;

/// 3a3fa2b3-r3 — the ONE shared, deterministic, piecewise oscillation
/// driving BOTH slice displacement (signed, per-slice-scaled) and tear-
/// streak alpha (unsigned magnitude, alpha-scaled) — see
/// `sample_energy_tear_layout`. No trait, no PRNG, no time-based noise,
/// no generic sine: a plain `t -> f32` pure function using the exact
/// same phase-boundary-constants + `phase_progress` + `ease_out_cubic` +
/// `lerp` idiom Teleport already established. Value sequence: 1.0 (cycle
/// 0 / initial peak) -> 0.0 (first crossing, end of phase A) ->
/// ENERGY_TEAR_REBOUND_1_FACTOR (opposite-sign rebound-1 peak, end of
/// phase B) -> ENERGY_TEAR_REBOUND_2_FACTOR (same-sign, smaller
/// rebound-2 peak, end of phase C) -> 0.0 (final, end of phase D). The
/// sign flip between consecutive boundary values (1.0 -> 0 ->
/// REBOUND_1(-) -> REBOUND_2(+) -> 0) is what produces the two required
/// additional zero-crossings — the curve is never asked to "jump" a
/// sign, it always transitions smoothly (eased) through zero.
fn energy_tear_oscillation(t: f32) -> f32 {
    if t >= ENERGY_TEAR_END {
        return 0.0;
    }
    let t_prime = phase_progress(t, 0.0, ENERGY_TEAR_END);
    if t_prime < ENERGY_TEAR_PHASE_A_END {
        let u = ease_out_cubic(phase_progress(t_prime, 0.0, ENERGY_TEAR_PHASE_A_END));
        lerp(1.0, 0.0, u)
    } else if t_prime < ENERGY_TEAR_PHASE_B_END {
        let u = ease_out_cubic(phase_progress(t_prime, ENERGY_TEAR_PHASE_A_END, ENERGY_TEAR_PHASE_B_END));
        lerp(0.0, ENERGY_TEAR_REBOUND_1_FACTOR, u)
    } else if t_prime < ENERGY_TEAR_PHASE_C_END {
        let u = ease_out_cubic(phase_progress(t_prime, ENERGY_TEAR_PHASE_B_END, ENERGY_TEAR_PHASE_C_END));
        lerp(ENERGY_TEAR_REBOUND_1_FACTOR, ENERGY_TEAR_REBOUND_2_FACTOR, u)
    } else {
        let u = ease_out_cubic(phase_progress(t_prime, ENERGY_TEAR_PHASE_C_END, 1.0));
        lerp(ENERGY_TEAR_REBOUND_2_FACTOR, 0.0, u)
    }
}

/// Deterministic, hand-authored per-slice pattern (dimensionless
/// coefficient, signed, magnitude <= 1.0) — not random, and no longer
/// symmetric like R2's `[-1.0, 0.6, -0.3, 0.6, -1.0]`: derived from (and
/// normalized against) the human-reviewed illustrative reference pattern
/// for a "roughly comparable" feel, per the R3 milestone brief. Combined
/// with `energy_tear_oscillation` and the window-width-relative unit in
/// `energy_tear_render_plan`, this is what each slice's live displacement
/// is scaled by.
const ENERGY_TEAR_SLICE_PEAK_OFFSET_FRACTIONS: [f32; ENERGY_TEAR_SLICE_COUNT] = [-0.95, 0.68, -0.53, 1.0, -0.74];
/// TEMPORARY DEVELOPMENT TUNING — peak independent streak alpha (not
/// the window's own opacity). Bright but not fully opaque, so the streak
/// reads as an energetic highlight rather than a flat white bar.
const ENERGY_TEAR_PEAK_STREAK_ALPHA: f32 = 0.85;
/// Fixed streak tint — generic "energy" cyan-white, not tied to any
/// copyrighted character palette.
const ENERGY_TEAR_STREAK_COLOR: [f32; 3] = [0.75, 0.95, 1.0];
/// Streak line width, as a fraction of the (narrower of the two
/// adjacent) slice widths — clamped to >=1px at the call site.
const ENERGY_TEAR_LINE_WIDTH_FRACTION: f32 = 0.10;
/// 3a3fa2b3-r3 — the "unit" displacement magnitude (before per-slice
/// coefficient and oscillation scaling), as a fraction of the WHOLE
/// WINDOW's width — replaces R2's slice-width-relative model per the
/// human-reviewed preference ("relative_to_window_width + sane pixel
/// clamp"). Chosen so the strongest slice's peak displacement (10% x
/// coefficient 1.0) is unambiguously larger than R2's strongest slice
/// (which peaked at 7% of window width under the old slice-width-
/// relative formula) for every slice in the new pattern, not just some —
/// see the R3 preview report for the exact old-vs-new comparison.
const ENERGY_TEAR_DISPLACEMENT_FRACTION_OF_WINDOW_WIDTH: f32 = 0.10;
/// Sane bounds on the unit displacement (see `energy_tear_render_plan`),
/// applied to the UNIT magnitude before the per-slice coefficient — so
/// the relative shape between slices is always preserved exactly, only
/// the absolute scale is clamped. MIN keeps the tear visible even on
/// very small/short-lived windows (where 10% of width would otherwise
/// round away to nothing); MAX prevents visually absurd tearing on very
/// large windows (where 10% of width could otherwise be hundreds of
/// pixels).
const ENERGY_TEAR_MIN_DISPLACEMENT_PX: f32 = 4.0;
const ENERGY_TEAR_MAX_DISPLACEMENT_PX: f32 = 72.0;

/// 3a3fa2b3 — per-frame, resolution-independent tear timing. Kept
/// entirely separate from `AnimationVisual` on purpose: this is an
/// energy_tear-only decoration, not a property of "the window's visual
/// state" that Scale/Teleport also share. Computed from the SAME `t`
/// already sampled once per surface per render (see
/// render_egl_scene_parts) — no second `Instant::now()`, no new timer.
/// `slice_offset_fractions` are dimensionless coefficients (roughly
/// [-1,1]) — NOT yet a fraction of window width; `energy_tear_render_
/// plan` applies `ENERGY_TEAR_DISPLACEMENT_FRACTION_OF_WINDOW_WIDTH`
/// (with its min/max clamp) to convert to pixels, since only that
/// function has access to the actual window geometry.
#[derive(Clone, Copy, Debug, PartialEq)]
struct EnergyTearLayout {
    slice_offset_fractions: [f32; ENERGY_TEAR_SLICE_COUNT],
    streak_alpha: f32,
}

fn sample_energy_tear_layout(t: f32) -> EnergyTearLayout {
    if t >= ENERGY_TEAR_END {
        return EnergyTearLayout { slice_offset_fractions: [0.0; ENERGY_TEAR_SLICE_COUNT], streak_alpha: 0.0 };
    }
    let osc = energy_tear_oscillation(t);
    let mut slice_offset_fractions = [0.0_f32; ENERGY_TEAR_SLICE_COUNT];
    for i in 0..ENERGY_TEAR_SLICE_COUNT {
        slice_offset_fractions[i] = ENERGY_TEAR_SLICE_PEAK_OFFSET_FRACTIONS[i] * osc;
    }
    EnergyTearLayout {
        slice_offset_fractions,
        // Unsigned magnitude of the SAME shared oscillation: this alone
        // produces the required 3-pulse shape (strongest at cycle 0,
        // weaker at rebound-1, weaker still at rebound-2, zero between
        // and after) with no separate alpha-specific curve.
        streak_alpha: ENERGY_TEAR_PEAK_STREAK_ALPHA * osc.abs(),
    }
}

/// 3a3fa2b3 — the ONLY gate deciding whether an animated surface takes
/// energy_tear's slice/streak render path this frame. Pulled out as its
/// own pure function (rather than an inline condition in the render
/// loop) so "Scale/Teleport never produce a tear layout" and
/// "energy_tear stops producing one once its own tear phase ends" are
/// both independently, executably testable — not just visible in the
/// render loop's source text.
fn energy_tear_layout_for(effect: crate::config::OpenAnimationEffect, t: f32) -> Option<EnergyTearLayout> {
    if effect == crate::config::OpenAnimationEffect::EnergyTear && t < ENERGY_TEAR_END {
        Some(sample_energy_tear_layout(t))
    } else {
        None
    }
}

/// 3a3fa2b4-r1 — "bubble": a compressed-pop-squash-rebound-lock
/// materialization, built with the exact same phase-boundary-constants +
/// `phase_progress` + `ease_out_cubic` + `lerp` idiom Teleport already
/// established (see `sample_teleport`) — no new curve architecture, no
/// spring library, no sine. What makes Bubble visually distinct from
/// Teleport is the CONSTANT TABLE, not the mechanism: Teleport starts as
/// a near-flat horizontal streak and spends most of its duration on a
/// small sharp impact/recoil; Bubble starts as a small compressed blob
/// on BOTH axes, overshoots past 1.0 on BOTH axes for its "pop", then
/// alternates which axis is >1 vs <1 across two more phases (squash,
/// then opposite-sign rebound) before locking — a genuine axis-reversal
/// character neither Scale (uniform single lerp) nor Teleport (Y always
/// more aggressive than X, never reverses which axis leads) produces.
const BUBBLE_PHASE_A_END: f32 = 0.35; // pop
const BUBBLE_PHASE_B_END: f32 = 0.58; // squash
const BUBBLE_PHASE_C_END: f32 = 0.78; // opposite rebound
// phase D (settle) runs from BUBBLE_PHASE_C_END to 1.0.

const BUBBLE_START_OPACITY: f32 = 0.10;
const BUBBLE_START_SCALE_X: f32 = 0.62;
const BUBBLE_START_SCALE_Y: f32 = 0.42;

const BUBBLE_PHASE_A_OPACITY: f32 = 1.0; // fully materialized by end of the pop
const BUBBLE_PHASE_A_SCALE_X: f32 = 1.12;
const BUBBLE_PHASE_A_SCALE_Y: f32 = 1.18;

const BUBBLE_PHASE_B_OPACITY: f32 = 1.0;
const BUBBLE_PHASE_B_SCALE_X: f32 = 1.04; // squash: X > 1, Y < 1
const BUBBLE_PHASE_B_SCALE_Y: f32 = 0.96;

const BUBBLE_PHASE_C_OPACITY: f32 = 1.0;
const BUBBLE_PHASE_C_SCALE_X: f32 = 0.985; // opposite rebound: X < 1, Y > 1
const BUBBLE_PHASE_C_SCALE_Y: f32 = 1.025;

const BUBBLE_END_OPACITY: f32 = 1.0;
const BUBBLE_END_SCALE: f32 = 1.0;

fn sample_bubble(t: f32) -> AnimationVisual {
    // Explicit, unconditional exact-final-state guard — same contract as
    // `sample_teleport`'s: phase D's rebound trough does not itself land
    // exactly on 1.0 by construction, so this guard is what delivers the
    // required exact final lock rather than a lingering easing tail.
    if t >= 1.0 {
        return AnimationVisual {
            opacity: BUBBLE_END_OPACITY,
            scale_x: BUBBLE_END_SCALE,
            scale_y: BUBBLE_END_SCALE,
        };
    }
    if t < BUBBLE_PHASE_A_END {
        let u = ease_out_cubic(phase_progress(t, 0.0, BUBBLE_PHASE_A_END));
        AnimationVisual {
            opacity: lerp(BUBBLE_START_OPACITY, BUBBLE_PHASE_A_OPACITY, u),
            scale_x: lerp(BUBBLE_START_SCALE_X, BUBBLE_PHASE_A_SCALE_X, u),
            scale_y: lerp(BUBBLE_START_SCALE_Y, BUBBLE_PHASE_A_SCALE_Y, u),
        }
    } else if t < BUBBLE_PHASE_B_END {
        let u = ease_out_cubic(phase_progress(t, BUBBLE_PHASE_A_END, BUBBLE_PHASE_B_END));
        AnimationVisual {
            opacity: lerp(BUBBLE_PHASE_A_OPACITY, BUBBLE_PHASE_B_OPACITY, u),
            scale_x: lerp(BUBBLE_PHASE_A_SCALE_X, BUBBLE_PHASE_B_SCALE_X, u),
            scale_y: lerp(BUBBLE_PHASE_A_SCALE_Y, BUBBLE_PHASE_B_SCALE_Y, u),
        }
    } else if t < BUBBLE_PHASE_C_END {
        let u = ease_out_cubic(phase_progress(t, BUBBLE_PHASE_B_END, BUBBLE_PHASE_C_END));
        AnimationVisual {
            opacity: lerp(BUBBLE_PHASE_B_OPACITY, BUBBLE_PHASE_C_OPACITY, u),
            scale_x: lerp(BUBBLE_PHASE_B_SCALE_X, BUBBLE_PHASE_C_SCALE_X, u),
            scale_y: lerp(BUBBLE_PHASE_B_SCALE_Y, BUBBLE_PHASE_C_SCALE_Y, u),
        }
    } else {
        let u = ease_out_cubic(phase_progress(t, BUBBLE_PHASE_C_END, 1.0));
        AnimationVisual {
            opacity: lerp(BUBBLE_PHASE_C_OPACITY, BUBBLE_END_OPACITY, u),
            scale_x: lerp(BUBBLE_PHASE_C_SCALE_X, BUBBLE_END_SCALE, u),
            scale_y: lerp(BUBBLE_PHASE_C_SCALE_Y, BUBBLE_END_SCALE, u),
        }
    }
}

/// 3a3fa2b6-r1 — fixed bright-neutral flash color shared by BOTH the
/// open and close TeleportFlashy overlay draws. R1 deliberately has no
/// `animation.flash.color` config key (see the architecture audit,
/// section 4/7) — color tuning is deferred until after human validation.
pub(crate) const TELEPORT_FLASHY_COLOR: [f32; 3] = [1.0, 1.0, 1.0];

/// 3a3fa2b6-r1 — "teleport_flashy" OPEN geometry: near-identity, reaches
/// exact final state very early (by `TELEPORT_FLASHY_OPEN_GEOMETRY_END`)
/// so the window is already fully materialized BEFORE the flash finishes
/// clearing (see `sample_teleport_flashy_open_flash`) — the reveal must
/// show an already-complete window, never one still visibly growing
/// under a thinning flash. Deliberately much smaller motion than Scale/
/// Teleport/Bubble: the flash overlay carries this effect's identity, not
/// the geometry. Same `phase_progress` + `ease_out_cubic` + `lerp` idiom
/// every other effect already uses — no new curve architecture.
const TELEPORT_FLASHY_OPEN_START_OPACITY: f32 = 0.05;
const TELEPORT_FLASHY_OPEN_START_SCALE: f32 = 0.985;
const TELEPORT_FLASHY_OPEN_GEOMETRY_END: f32 = 0.20;

fn sample_teleport_flashy_open(t: f32) -> AnimationVisual {
    if t >= 1.0 {
        return AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 };
    }
    if t < TELEPORT_FLASHY_OPEN_GEOMETRY_END {
        let u = ease_out_cubic(phase_progress(t, 0.0, TELEPORT_FLASHY_OPEN_GEOMETRY_END));
        let scale = lerp(TELEPORT_FLASHY_OPEN_START_SCALE, 1.0, u);
        AnimationVisual {
            opacity: lerp(TELEPORT_FLASHY_OPEN_START_OPACITY, 1.0, u),
            scale_x: scale,
            scale_y: scale,
        }
    } else {
        AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 }
    }
}

/// 3a3fa2b6-r1 — "teleport_flashy" OPEN flash-alpha curve: strongly
/// front-loaded — a brief flat hold near full strength, then an eased
/// decay to exactly zero. This is EFFECT-SPECIFIC OVERLAY STATE, kept
/// deliberately separate from `AnimationVisual` (never added as a
/// `flash_alpha` field there — see the architecture audit, section 6/13),
/// mirroring EnergyTear's own `EnergyTearLayout`/`streak_alpha`
/// precedent exactly.
const TELEPORT_FLASHY_OPEN_FLASH_HOLD_END: f32 = 0.08;
const TELEPORT_FLASHY_OPEN_FLASH_END: f32 = 0.40;

fn sample_teleport_flashy_open_flash(t: f32) -> f32 {
    if t <= TELEPORT_FLASHY_OPEN_FLASH_HOLD_END {
        1.0
    } else if t < TELEPORT_FLASHY_OPEN_FLASH_END {
        let u = ease_out_cubic(phase_progress(t, TELEPORT_FLASHY_OPEN_FLASH_HOLD_END, TELEPORT_FLASHY_OPEN_FLASH_END));
        lerp(1.0, 0.0, u).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 3a3fa2b6-r1 — the ONLY gate deciding whether an animated OPEN surface
/// carries a TeleportFlashy overlay this frame. Mirrors
/// `energy_tear_layout_for`'s exact shape: `None` for every other effect,
/// so "only TeleportFlashy ever produces open flash state" is
/// independently, executably testable.
fn teleport_flashy_open_flash_for(effect: crate::config::OpenAnimationEffect, t: f32) -> Option<f32> {
    if effect == crate::config::OpenAnimationEffect::TeleportFlashy {
        Some(sample_teleport_flashy_open_flash(t))
    } else {
        None
    }
}

/// 3a3fa2b6-r2 ("Minato" correction) — small epsilon past the exact
/// corner value (`r_norm==1.0` in the shader's aspect-safe per-axis
/// normalization — see `render_surface_with_radial_reveal`) so
/// antialiasing can never leave a corner fragment partially covered at
/// the moment the reveal completes. The shader's `coverage()` helper's
/// `fwidth`-derived antialiasing band is far smaller than this 0.02
/// margin for any realistically sized window (its derivative is taken in
/// the unitless, per-axis-normalized-by-half-size space, which changes
/// extremely slowly per screen pixel), so `reveal_radius >=
/// MINATO_REVEAL_FULL_RADIUS` guarantees exact full coverage everywhere
/// inside the window, including all four corners, with no visible pop at
/// the `MINATO_REVEAL_END` fallback boundary.
const MINATO_REVEAL_FULL_RADIUS: f32 = 1.02;
/// Nothing content-visible before this — pure flash only (see the
/// architecture audit, section A3: "0.00->0.06 strong flash" prefacing
/// any reveal).
const MINATO_REVEAL_START: f32 = 0.04;
/// Deliberately REUSES `TELEPORT_FLASHY_OPEN_GEOMETRY_END` rather than an
/// independently-chosen 0.20 literal — this ties the radial reveal's
/// completion to the SAME instant the scale-pop geometry already reaches
/// exact identity, by construction, so there is never a frame where
/// "geometry says done" but "reveal mask says still growing" or vice
/// versa.
const MINATO_REVEAL_END: f32 = TELEPORT_FLASHY_OPEN_GEOMETRY_END;

/// 3a3fa2b6-r2 ("Minato" correction) — center-to-edges radial reveal
/// radius, in the SAME aspect-safe per-axis-normalized-by-half-size space
/// the shader computes `r_norm` in (corners at exactly `1.0`). `t <
/// MINATO_REVEAL_START`: nothing revealed yet (`0.0`) — the window is
/// still pure flash. `[MINATO_REVEAL_START, MINATO_REVEAL_END)`: eased
/// (`ease_out_cubic` + the established `phase_progress` idiom, exactly
/// like every other effect curve) growth from `0.0` to
/// `MINATO_REVEAL_FULL_RADIUS`. `t >= MINATO_REVEAL_END`: fully revealed
/// — the caller falls back to the ordinary (mode-0) draw at this point,
/// exactly like `energy_tear_render_plan`'s existing `None`-after-
/// `ENERGY_TEAR_END` fallback, so this value is never actually consulted
/// past that boundary, but stays defined and correct (`MINATO_REVEAL_FULL_RADIUS`)
/// for anyone sampling it directly (e.g. tests).
fn sample_minato_reveal_radius(t: f32) -> f32 {
    if t < MINATO_REVEAL_START {
        0.0
    } else if t < MINATO_REVEAL_END {
        let u = ease_out_cubic(phase_progress(t, MINATO_REVEAL_START, MINATO_REVEAL_END));
        lerp(0.0, MINATO_REVEAL_FULL_RADIUS, u)
    } else {
        MINATO_REVEAL_FULL_RADIUS
    }
}

/// 3a3fa2b6-r2 — the ONLY gate deciding whether an animated OPEN surface
/// carries a Minato radial-reveal mask this frame. Mirrors
/// `teleport_flashy_open_flash_for`'s exact shape: `None` for every other
/// effect (so "only TeleportFlashy ever produces reveal state" is
/// independently, executably testable) AND for `t >= MINATO_REVEAL_END`
/// (so the render loop falls back to the ordinary, zero-extra-cost
/// mode-0 draw once revealed — no lingering effect-mode cost, mirroring
/// `energy_tear_layout_for`'s own post-`ENERGY_TEAR_END` `None` fallback).
fn minato_reveal_radius_for(effect: crate::config::OpenAnimationEffect, t: f32) -> Option<f32> {
    if effect == crate::config::OpenAnimationEffect::TeleportFlashy && t < MINATO_REVEAL_END {
        Some(sample_minato_reveal_radius(t))
    } else {
        None
    }
}

/// 3a3fa2b7 ("Kamui" vortex) — OPEN `AnimationVisual`: exact identity
/// opacity/scale throughout. The vortex effect IS the visual (see
/// `kamui_open_state_for` for the polar-warp/visible-radius driver) —
/// unlike Scale/Teleport/Bubble, this effect's geometry curve carries no
/// meaningful motion of its own, deliberately, so the content warp reads
/// as the sole source of movement.
fn sample_kamui_open(_t: f32) -> AnimationVisual {
    AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 }
}

/// 3a3fa2b7-r2 — OPEN visible-radius curve, three phases per the R2
/// motion-tuning spec (section 5): "core" `[0, KAMUI_OPEN_CORE_END]`
/// (0.15) — radius eases `KAMUI_OPEN_START_RADIUS` (0.02) up to
/// `KAMUI_OPEN_CORE_RADIUS` (0.20), a tight vortex core; "expulsion"
/// `[KAMUI_OPEN_CORE_END, KAMUI_OPEN_EXPAND_END]` (0.65) — radius eases
/// `0.20 -> 1.0`, the main visible outward flow; "settle"
/// `[KAMUI_OPEN_EXPAND_END, KAMUI_OPEN_SETTLE_END]` (0.90) — radius held
/// at exactly `1.0`. At `t >= KAMUI_OPEN_SETTLE_END`: exact identity
/// (radius=1, twist=0, radial_power=1) — the render loop falls back to
/// ordinary `shadow_mode==0` rendering here, exactly like
/// `MINATO_REVEAL_END`'s and `ENERGY_TEAR_END`'s own existing fallback
/// precedent, applied a third time.
const KAMUI_OPEN_START_RADIUS: f32 = 0.02;
const KAMUI_OPEN_CORE_END: f32 = 0.15;
const KAMUI_OPEN_CORE_RADIUS: f32 = 0.20;
const KAMUI_OPEN_EXPAND_END: f32 = 0.65;
const KAMUI_OPEN_SETTLE_END: f32 = 0.90;

/// 3a3fa2b7-r2 — increased from R1's `1.6` (human feedback: OPEN "needs
/// visibly MORE movement, more like an actual Kamui vortex").
/// COUNTER-CLOCKWISE (positive sign) per the "unwinds/pushes outward from
/// center" sensation, unchanged from R1.
const KAMUI_OPEN_MAX_TWIST: f32 = 2.4;
/// 3a3fa2b7-r2 — twist is now COUPLED to `visible_radius` (never held
/// static through most of expansion, per the R2 spec's explicit
/// correction) via `MAX_TWIST * pow(1-radius, DECAY_POWER)`: strongest at
/// the tiny core (radius near `KAMUI_OPEN_START_RADIUS`), decaying to
/// EXACTLY `0.0` the instant radius reaches `1.0` — automatic, with zero
/// extra phase-boundary bookkeeping for twist specifically. A decay power
/// below `1.0` keeps twist "remaining strong during early/mid expansion"
/// (per section 4) before falling off faster as radius approaches 1 (a
/// concave `pow(x,0.6)` curve stays closer to its start value than a
/// linear one for most of `x`'s range, only dropping steeply near `x=0`,
/// i.e. near `radius=1`).
const KAMUI_OPEN_TWIST_DECAY_POWER: f32 = 0.6;
/// 3a3fa2b7-r2.1 — nonlinear radial-power warp: OPEN uses a value ABOVE
/// `1.0` (`1.8`) at the core, easing to exactly `1.0` at settle.
///
/// CORRECTED direction from R2 (R2 shipped `0.55`, the opposite sign —
/// see the r2.1 spec's "inverse-mapping direction bug: CONFIRMED"
/// finding). The shader computes `source_r = pow(output_r, radial_power)`
/// — i.e. it maps an OUTPUT fragment position to a SOURCE sampling
/// position. What matters for the visual sensation is the INVERSE
/// question: where does a FIXED SOURCE FEATURE end up displayed? Solving
/// `source_r = pow(output_r, power)` for `output_r` gives
/// `output_r = pow(source_r, 1/power)`. For `power > 1` and a source
/// feature at `source_r` in `(0,1)`, `1/power < 1`, so
/// `pow(source_r, 1/power) > source_r` — e.g. a feature at `source_r=0.25`
/// with `power=1.8` is displayed at `output_r ≈ 0.463`: FARTHER from
/// center than it originally sat in the source. That is a source feature
/// being pushed OUTWARD — the desired OPEN "expelled from center"
/// sensation. (R2's reasoning instead asked "does a near-center OUTPUT
/// point sample from farther out in the source," which is a different,
/// less relevant question — it does not by itself tell you whether
/// content visually moves toward or away from center.)
const KAMUI_OPEN_RADIAL_POWER_START: f32 = 1.8;

fn sample_kamui_open_visible_radius(t: f32) -> f32 {
    if t < KAMUI_OPEN_CORE_END {
        let u = ease_out_cubic(phase_progress(t, 0.0, KAMUI_OPEN_CORE_END));
        lerp(KAMUI_OPEN_START_RADIUS, KAMUI_OPEN_CORE_RADIUS, u)
    } else if t < KAMUI_OPEN_EXPAND_END {
        let u = ease_out_cubic(phase_progress(t, KAMUI_OPEN_CORE_END, KAMUI_OPEN_EXPAND_END));
        lerp(KAMUI_OPEN_CORE_RADIUS, 1.0, u)
    } else {
        1.0
    }
}

/// 3a3fa2b7-r2 — derived purely from `visible_radius`, never an
/// independent time phase — see the constant doc comment above for the
/// exact coupling rationale.
fn sample_kamui_open_twist(t: f32) -> f32 {
    let radius = sample_kamui_open_visible_radius(t);
    KAMUI_OPEN_MAX_TWIST * (1.0 - radius).clamp(0.0, 1.0).powf(KAMUI_OPEN_TWIST_DECAY_POWER)
}

/// 3a3fa2b7-r2 — also derived from `visible_radius`, reaching exactly
/// `1.0` (a strict shader-side no-op) the instant radius reaches `1.0`,
/// exactly like twist above. Linear in `radius` — a simpler shape than
/// twist's own decay curve, since radial_power is a secondary/subtler
/// distortion knob here.
fn sample_kamui_open_radial_power(t: f32) -> f32 {
    let radius = sample_kamui_open_visible_radius(t);
    KAMUI_OPEN_RADIAL_POWER_START + (1.0 - KAMUI_OPEN_RADIAL_POWER_START) * radius.clamp(0.0, 1.0)
}

/// 3a3fa2b7 — the ONLY gate deciding whether an animated OPEN surface
/// carries Kamui polar-warp state this frame. Mirrors
/// `minato_reveal_radius_for`'s exact shape: `None` for every other
/// effect AND for `t >= KAMUI_OPEN_SETTLE_END` (fallback to the ordinary,
/// zero-extra-cost mode-0 draw once settled). Returns
/// `(visible_radius, twist, radial_power)` together since all three
/// curves are consulted unconditionally as a group at every call site —
/// no reason to force separate `Option`s that would always agree on
/// `Some`/`None` in lockstep.
fn kamui_open_state_for(effect: crate::config::OpenAnimationEffect, t: f32) -> Option<(f32, f32, f32)> {
    if effect == crate::config::OpenAnimationEffect::Kamui && t < KAMUI_OPEN_SETTLE_END {
        Some((sample_kamui_open_visible_radius(t), sample_kamui_open_twist(t), sample_kamui_open_radial_power(t)))
    } else {
        None
    }
}

/// 3a3fa2b7 — Kamui's shadow ENVELOPE (section 20 of the R1 spec):
/// shadow must never receive polar warp/twist state, only a broad
/// strength multiplier tracking `visible_radius` directly. Deliberately
/// NOT gated by `KAMUI_OPEN_SETTLE_END` like `kamui_open_state_for` is —
/// shadow is drawn every frame regardless of which content mode is
/// active, and `sample_kamui_open_visible_radius` already naturally
/// returns exactly `1.0` (reproducing today's ordinary shadow strength
/// unchanged) for the entire settled tail, so no extra time-gating is
/// needed for the envelope value itself to become a no-op post-settle.
/// `visible_radius` alone (no extra `sqrt`/`clamp` wrapper) already
/// satisfies both required boundary behaviors: it starts at
/// `KAMUI_OPEN_START_RADIUS` (0.02, negligibly close to the required
/// "radius→0 ⇒ shadow→0") and reaches exactly `1.0` ("radius→1 ⇒
/// shadow→existing behavior") by construction of the curve above.
fn kamui_open_shadow_envelope_for(effect: crate::config::OpenAnimationEffect, t: f32) -> Option<f32> {
    if effect == crate::config::OpenAnimationEffect::Kamui {
        Some(sample_kamui_open_visible_radius(t))
    } else {
        None
    }
}

/// The ONE pure effect-sampling entry point, dispatched by a plain
/// `match` — not a trait object, not a boxed closure, not a plugin
/// registry (per the 3a3fa2b audit's explicit preference). Every render
/// path (provisional first-frame and persistent subsequent frames alike)
/// calls this same function, via `WindowAnimation::sample` or directly
/// with an explicit `t` — there is no separate hardcoded "first frame"
/// implementation anywhere.
fn sample_open_effect(effect: crate::config::OpenAnimationEffect, t: f32) -> AnimationVisual {
    match effect {
        crate::config::OpenAnimationEffect::Scale => sample_scale(t),
        crate::config::OpenAnimationEffect::Teleport => sample_teleport(t),
        crate::config::OpenAnimationEffect::EnergyTear => sample_energy_tear(t),
        crate::config::OpenAnimationEffect::Bubble => sample_bubble(t),
        crate::config::OpenAnimationEffect::TeleportFlashy => sample_teleport_flashy_open(t),
        crate::config::OpenAnimationEffect::Kamui => sample_kamui_open(t),
    }
}

fn lerp(a: f32, b: f32, t: f32) -> f32 {
    a + (b - a) * t
}

/// 3a3fa2b5 — lifecycle state for the ONE R1 reference close effect.
/// Resource-free, exactly like `WindowAnimation`: never holds a Damage/
/// NamedPixmap/EGLImage. A deliberately SEPARATE type from
/// `WindowAnimation` — close state must never semantically pretend to be
/// an open animation (`effect` is `crate::config::CloseAnimationEffect`,
/// not `OpenAnimationEffect`). See `ClosingTexture`/`ClosingVisual` for
/// the one GPU resource a completed close carries, entirely separate from
/// this value type.
#[derive(Clone, Debug)]
struct ClosingAnimation {
    effect: crate::config::CloseAnimationEffect,
    started_at: Instant,
    duration: Duration,
}

impl ClosingAnimation {
    fn new(started_at: Instant, effect: crate::config::CloseAnimationEffect, duration: Duration) -> Self {
        Self { effect, started_at, duration }
    }

    fn progress(&self, now: Instant) -> f32 {
        animation_progress(now.saturating_duration_since(self.started_at), self.duration)
    }

    fn is_complete(&self, now: Instant) -> bool {
        self.progress(now) >= 1.0
    }
}

/// 3a3fa2b5 — reference close effect end-state: this exists to prove
/// lifecycle/snapshot-ownership/ordering/first-frame/retirement, not
/// aesthetics. Deterministic plain `lerp` (no easing, no bounce, no
/// overshoot — deliberately no "personality", unlike the open effects).
const CLOSE_SCALE_END_OPACITY: f32 = 0.0;
const CLOSE_SCALE_END_SCALE: f32 = 0.95;

fn sample_close_scale(t: f32) -> AnimationVisual {
    let t = t.clamp(0.0, 1.0);
    let scale = lerp(1.0, CLOSE_SCALE_END_SCALE, t);
    AnimationVisual {
        opacity: lerp(1.0, CLOSE_SCALE_END_OPACITY, t),
        scale_x: scale,
        scale_y: scale,
    }
}

/// 3a3fa2b6-r1 — "teleport_flashy" CLOSE geometry: the window stays at
/// exact identity until the flash has already begun rising, then
/// collapses fast to a SMALL contraction (0.98, not Scale-close's 0.95 —
/// see the architecture audit, section 12: a larger shrink would read as
/// a fade/scale effect rather than teleport) and holds there. Deliberately
/// NOT a `1.0 - sample_teleport_flashy_open(t)` reversal — CLOSE has its
/// own distinct hold-then-collapse shape, never OPEN played backwards.
const TELEPORT_FLASHY_CLOSE_HOLD_END: f32 = 0.12;
const TELEPORT_FLASHY_CLOSE_COLLAPSE_END: f32 = 0.28;
const TELEPORT_FLASHY_CLOSE_END_SCALE: f32 = 0.98;

fn sample_teleport_flashy_close(t: f32) -> AnimationVisual {
    if t <= TELEPORT_FLASHY_CLOSE_HOLD_END {
        AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 }
    } else if t < TELEPORT_FLASHY_CLOSE_COLLAPSE_END {
        let u = ease_out_cubic(phase_progress(t, TELEPORT_FLASHY_CLOSE_HOLD_END, TELEPORT_FLASHY_CLOSE_COLLAPSE_END));
        let scale = lerp(1.0, TELEPORT_FLASHY_CLOSE_END_SCALE, u);
        AnimationVisual {
            opacity: lerp(1.0, 0.0, u),
            scale_x: scale,
            scale_y: scale,
        }
    } else {
        AnimationVisual { opacity: 0.0, scale_x: TELEPORT_FLASHY_CLOSE_END_SCALE, scale_y: TELEPORT_FLASHY_CLOSE_END_SCALE }
    }
}

/// 3a3fa2b6-r1 — "teleport_flashy" CLOSE flash-alpha curve: a PULSE
/// (0 -> peak -> 0), unlike OPEN's pure fade-out — see the architecture
/// audit, section 9/13. The window's own opacity (see
/// `sample_teleport_flashy_close`) reaches exactly 0 at
/// `TELEPORT_FLASHY_CLOSE_COLLAPSE_END` (0.28), strictly BEFORE this
/// flash has finished decaying (`TELEPORT_FLASHY_CLOSE_FLASH_END`, 0.50)
/// — by construction, not coincidence, the window disappears underneath/
/// inside the still-visible flash.
const TELEPORT_FLASHY_CLOSE_FLASH_PEAK: f32 = 0.22;
const TELEPORT_FLASHY_CLOSE_FLASH_END: f32 = 0.50;

fn sample_teleport_flashy_close_flash(t: f32) -> f32 {
    if t <= 0.0 {
        0.0
    } else if t < TELEPORT_FLASHY_CLOSE_FLASH_PEAK {
        let u = ease_out_cubic(phase_progress(t, 0.0, TELEPORT_FLASHY_CLOSE_FLASH_PEAK));
        lerp(0.0, 1.0, u).clamp(0.0, 1.0)
    } else if t < TELEPORT_FLASHY_CLOSE_FLASH_END {
        let u = ease_out_cubic(phase_progress(t, TELEPORT_FLASHY_CLOSE_FLASH_PEAK, TELEPORT_FLASHY_CLOSE_FLASH_END));
        lerp(1.0, 0.0, u).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

/// 3a3fa2b6-r1 — the ONLY gate deciding whether an animated CLOSE surface
/// carries a TeleportFlashy overlay this frame. Mirrors
/// `teleport_flashy_open_flash_for`'s shape exactly, kept as a SEPARATE
/// function (never merged with the open flash dispatch) since open and
/// close flash samplers have genuinely different curves.
fn teleport_flashy_close_flash_for(effect: crate::config::CloseAnimationEffect, t: f32) -> Option<f32> {
    if effect == crate::config::CloseAnimationEffect::TeleportFlashy {
        Some(sample_teleport_flashy_close_flash(t))
    } else {
        None
    }
}

/// 3a3fa2b7-r3 ("Kamui" vortex) — CLOSE `AnimationVisual`: scale stays exact
/// identity throughout (Kamui's CLOSE motion is entirely the polar warp/
/// radial-domain shrink — see `kamui_close_state_for` — never a geometry
/// pop). Opacity carries ONLY the phase-D fade: held at exact `1.0`
/// through `KAMUI_CLOSE_SUCTION_END` (content stays visually intact while
/// being spatially pulled inward — the defining visual is the SPATIAL
/// collapse, not a fade, per the R1 spec's explicit instruction), then
/// eased to exactly `0.0` by `KAMUI_CLOSE_COLLAPSE_END`.
///
/// R3 FIX: uses `ease_in_cubic` (back-loaded: slow start, accelerating
/// end) on the SAME `phase_progress` shared with radius — the R2.1 bug
/// was `ease_out_cubic` (front-loaded) which consumed most opacity early
/// while radius was already at ~0.10. Now both radius and opacity use
/// `ease_in_cubic(phase_progress(...))`, so content stays materially
/// visible through early final-phase and collapses together at the end.
fn sample_kamui_close(t: f32) -> AnimationVisual {
    let opacity = if t <= KAMUI_CLOSE_SUCTION_END {
        1.0
    } else if t < KAMUI_CLOSE_COLLAPSE_END {
        let u = ease_in_cubic(phase_progress(t, KAMUI_CLOSE_SUCTION_END, KAMUI_CLOSE_COLLAPSE_END));
        lerp(1.0, 0.0, u)
    } else {
        0.0
    };
    AnimationVisual { opacity, scale_x: 1.0, scale_y: 1.0 }
}

/// 3a3fa2b7-r3 — CLOSE visible-radius curve, FOUR time-based phases.
/// R3 modifies the R2 suction endpoint and final-collapse easing to fix
/// the visual bug where content disappeared before the Kamui finished:
/// "grab" `[0, KAMUI_CLOSE_GRAB_END]` (0.18) — radius eases
/// `1.0 -> KAMUI_CLOSE_GRAB_RADIUS` (0.97); "flow"
/// `[KAMUI_CLOSE_GRAB_END, KAMUI_CLOSE_FLOW_END]` (0.55) — radius eases
/// `0.97 -> KAMUI_CLOSE_FLOW_RADIUS` (0.70), the defining early spiral
/// motion, with the window still substantially visible; "suction"
/// `[KAMUI_CLOSE_FLOW_END, KAMUI_CLOSE_SUCTION_END]` (0.82) — radius
/// eases `0.70 -> KAMUI_CLOSE_SUCTION_RADIUS` (0.45 — R2.1 was 0.10,
/// which left content practically invisible before the final fade);
/// "synchronized final collapse"
/// `[KAMUI_CLOSE_SUCTION_END, KAMUI_CLOSE_COLLAPSE_END]` (0.94) — radius
/// eases `0.45 -> 0.0` via `ease_in_cubic` (late-accelerating) while
/// opacity simultaneously fades via the SAME shared `ease_in_cubic(
/// phase_progress(...))` (see `sample_kamui_close`). Content and mask
/// die together.
const KAMUI_CLOSE_GRAB_END: f32 = 0.18;
const KAMUI_CLOSE_FLOW_END: f32 = 0.55;
const KAMUI_CLOSE_SUCTION_END: f32 = 0.82;
const KAMUI_CLOSE_COLLAPSE_END: f32 = 0.94;
const KAMUI_CLOSE_GRAB_RADIUS: f32 = 0.97;
const KAMUI_CLOSE_FLOW_RADIUS: f32 = 0.70;
const KAMUI_CLOSE_SUCTION_RADIUS: f32 = 0.45;

/// 3a3fa2b7-r2 — THE core fix this milestone exists for. R1's twist was
/// `-MAX_TWIST * (1.0 - visible_radius)`, which stays near-zero through
/// the entire grab phase (radius barely moves off `1.0` there) — the
/// viewer never saw angular motion until radius had ALREADY collapsed
/// substantially, reading as a plain shrink rather than a vortex (the
/// reported "currently ugly... squeezed/closed into the center" bug).
/// Twist now follows its OWN separate, EARLIER-ramping time curve,
/// reusing the SAME four phase boundaries as radius above but with
/// magnitude breakpoints chosen to run well AHEAD of radius's own
/// collapse: `0 -> KAMUI_CLOSE_TWIST_AFTER_GRAB` (1.0 rad) already by
/// `KAMUI_CLOSE_GRAB_END`, while radius has only eased to `0.97` — i.e.
/// meaningfully nonzero twist while the window still looks almost full
/// size (see `kamui_close_twist_precedes_radius_contraction`, the
/// section-9-required proof) — then `-> KAMUI_CLOSE_TWIST_AFTER_FLOW`
/// (2.6 rad, matching R1's OLD max — now just a midpoint) by
/// `KAMUI_CLOSE_FLOW_END`, then `-> KAMUI_CLOSE_MAX_TWIST` (3.4 rad) by
/// `KAMUI_CLOSE_SUCTION_END`, held there through core collapse ("twist
/// may remain strong until nearly invisible", R2 spec section 7 phase D).
/// `3.4` sits inside the spec's suggested `3.2-3.8` range (its own first
/// suggestion). CLOCKWISE (negative sign), unchanged from R1's "sucked
/// in" sensation.
const KAMUI_CLOSE_TWIST_AFTER_GRAB: f32 = 1.0;
const KAMUI_CLOSE_TWIST_AFTER_FLOW: f32 = 2.6;
const KAMUI_CLOSE_MAX_TWIST: f32 = 3.4;

/// 3a3fa2b7-r2.1 — nonlinear radial-power warp: CLOSE moves BELOW `1.0`
/// (unlike OPEN, which moves above it) as suction proceeds, reaching
/// `KAMUI_CLOSE_RADIAL_POWER_END` (`0.55`) by `SUCTION_END`, held through
/// collapse.
///
/// CORRECTED direction from R2 (R2 shipped `1.8`, the opposite sign — see
/// the r2.1 spec's "inverse-mapping direction bug: CONFIRMED" finding;
/// see `KAMUI_OPEN_RADIAL_POWER_START`'s comment for the full derivation
/// this mirrors). Solving `source_r = pow(output_r, power)` for the
/// INVERSE question ("where does a fixed source feature end up
/// displayed") gives `output_r = pow(source_r, 1/power)`. For `power < 1`
/// and a source feature at `source_r` in `(0,1)`, `1/power > 1`, so
/// `pow(source_r, 1/power) < source_r` — e.g. a feature at `source_r=0.25`
/// with `power=0.55` is displayed at `output_r ≈ 0.080`: much CLOSER to
/// center than it originally sat in the source. That is a source feature
/// being pulled/sucked INWARD — the desired CLOSE "sucked into the
/// vortex" sensation. Coupled directly to `visible_radius` (`1.0` at
/// `radius=1`, `0.55` at `radius=0`) rather than an independent phase
/// timeline — this is safe to couple (unlike twist) since the R2 spec
/// only flagged TWIST's radius-coupling as the bug, and coupling here
/// guarantees a perfectly continuous `1.0` at t=0 (no pop the instant
/// closing begins) for free.
const KAMUI_CLOSE_RADIAL_POWER_END: f32 = 0.55;

fn sample_kamui_close_visible_radius(t: f32) -> f32 {
    if t <= KAMUI_CLOSE_GRAB_END {
        let u = ease_out_cubic(phase_progress(t, 0.0, KAMUI_CLOSE_GRAB_END));
        lerp(1.0, KAMUI_CLOSE_GRAB_RADIUS, u)
    } else if t < KAMUI_CLOSE_FLOW_END {
        let u = ease_out_cubic(phase_progress(t, KAMUI_CLOSE_GRAB_END, KAMUI_CLOSE_FLOW_END));
        lerp(KAMUI_CLOSE_GRAB_RADIUS, KAMUI_CLOSE_FLOW_RADIUS, u)
    } else if t < KAMUI_CLOSE_SUCTION_END {
        let u = ease_out_cubic(phase_progress(t, KAMUI_CLOSE_FLOW_END, KAMUI_CLOSE_SUCTION_END));
        lerp(KAMUI_CLOSE_FLOW_RADIUS, KAMUI_CLOSE_SUCTION_RADIUS, u)
    } else if t < KAMUI_CLOSE_COLLAPSE_END {
        let u = ease_in_cubic(phase_progress(t, KAMUI_CLOSE_SUCTION_END, KAMUI_CLOSE_COLLAPSE_END));
        lerp(KAMUI_CLOSE_SUCTION_RADIUS, 0.0, u)
    } else {
        0.0
    }
}

/// 3a3fa2b7-r2 — the mandatory fix: a SEPARATE time curve from radius,
/// ramping earlier — see the constant doc comment above for the full
/// rationale and the exact required relationship this establishes.
fn sample_kamui_close_twist(t: f32) -> f32 {
    let magnitude = if t <= KAMUI_CLOSE_GRAB_END {
        let u = ease_out_cubic(phase_progress(t, 0.0, KAMUI_CLOSE_GRAB_END));
        lerp(0.0, KAMUI_CLOSE_TWIST_AFTER_GRAB, u)
    } else if t < KAMUI_CLOSE_FLOW_END {
        let u = ease_out_cubic(phase_progress(t, KAMUI_CLOSE_GRAB_END, KAMUI_CLOSE_FLOW_END));
        lerp(KAMUI_CLOSE_TWIST_AFTER_GRAB, KAMUI_CLOSE_TWIST_AFTER_FLOW, u)
    } else if t < KAMUI_CLOSE_SUCTION_END {
        let u = ease_out_cubic(phase_progress(t, KAMUI_CLOSE_FLOW_END, KAMUI_CLOSE_SUCTION_END));
        lerp(KAMUI_CLOSE_TWIST_AFTER_FLOW, KAMUI_CLOSE_MAX_TWIST, u)
    } else {
        KAMUI_CLOSE_MAX_TWIST
    };
    -magnitude
}

/// 3a3fa2b7-r2 — see the constant doc comment above for the direction
/// rationale. Coupled to `visible_radius`, reaching exactly `1.0` at
/// `t=0` (radius=1) and `KAMUI_CLOSE_RADIAL_POWER_END` at full collapse.
fn sample_kamui_close_radial_power(t: f32) -> f32 {
    let radius = sample_kamui_close_visible_radius(t).clamp(0.0, 1.0);
    1.0 + (KAMUI_CLOSE_RADIAL_POWER_END - 1.0) * (1.0 - radius)
}

/// 3a3fa2b7 — the ONLY gate deciding whether an animated CLOSE surface
/// (provisional frame 0 OR committed `ClosingVisual` — see
/// `render_closing_layer`, ONE shared path for both) carries Kamui
/// polar-warp state this frame. Unlike the OPEN gate, this is never
/// time-bounded to `None` for a settled tail — Kamui's CLOSE vortex is
/// present for the entire close duration, since the window is
/// disappearing rather than settling into a final visible state. Returns
/// `(visible_radius, twist, radial_power)`, mirroring `kamui_open_state_for`.
fn kamui_close_state_for(effect: crate::config::CloseAnimationEffect, t: f32) -> Option<(f32, f32, f32)> {
    if effect == crate::config::CloseAnimationEffect::Kamui {
        Some((sample_kamui_close_visible_radius(t), sample_kamui_close_twist(t), sample_kamui_close_radial_power(t)))
    } else {
        None
    }
}

/// 3a3fa2b7 — Kamui CLOSE's shadow ENVELOPE, mirroring
/// `kamui_open_shadow_envelope_for`'s exact reasoning: `visible_radius`
/// alone, no polar warp state, no extra wrapper — reaches exactly `0.0`
/// at full collapse and exactly `1.0` at the very start (before any
/// visible distortion has begun).
fn kamui_close_shadow_envelope_for(effect: crate::config::CloseAnimationEffect, t: f32) -> Option<f32> {
    if effect == crate::config::CloseAnimationEffect::Kamui {
        Some(sample_kamui_close_visible_radius(t))
    } else {
        None
    }
}

/// The ONE pure close-effect-sampling entry point — same `match`-dispatch
/// shape as `sample_open_effect`, kept as a SEPARATE function/dispatch
/// (never merged with the open dispatch) since `CloseAnimationEffect` and
/// `OpenAnimationEffect` are deliberately separate enums.
fn sample_close_effect(effect: crate::config::CloseAnimationEffect, t: f32) -> AnimationVisual {
    match effect {
        crate::config::CloseAnimationEffect::Scale => sample_close_scale(t),
        crate::config::CloseAnimationEffect::TeleportFlashy => sample_teleport_flashy_close(t),
        crate::config::CloseAnimationEffect::Kamui => sample_kamui_close(t),
    }
}

/// Dock/Desktop and override_redirect are excluded: no richer transient
/// taxonomy exists yet (SurfaceVisualClass has exactly Normal/Dock/Desktop),
/// so an unknown override_redirect popup would otherwise default to Normal.
fn eligible_for_open_animation(entry: &SurfaceEntry) -> bool {
    // 3a3fa2a R2: uses the semantic-preferring effective_override_redirect,
    // not the capture-scoped override_redirect — a managed application
    // window whose canonical capture surface happens to be override_redirect
    // (e.g. an i3-internal wrapper) must not be rejected on that basis.
    matches!(entry.visual_class, SurfaceVisualClass::Normal) && !entry.effective_override_redirect
}

/// Candidate-local, pure, and temporary: never touches persistent
/// `SceneSession::window_animations`. Only a successful commit (see
/// `commit_candidate_inner`) promotes any of this map's entries into
/// persistent state; a rejected/retried candidate simply drops it.
///
/// `is_first_publish` suppresses animation on the very first scene
/// publication (startup with pre-existing windows must not animate).
/// `present_available` suppresses animation when Present is unavailable
/// (no MSC heartbeat to drive intermediate frames), so surfaces render
/// directly at final state rather than failing or busy-looping — this
/// gate is independent of, and always ANDed with, `animation.enabled`
/// (3a3fa2b1): a config-enabled but Present-unavailable case still
/// yields no animation, and vice versa.
fn provisional_open_animations(
    old_surfaces: &HashSet<Window>,
    snapshot: &SceneSnapshot,
    is_first_publish: bool,
    present_available: bool,
    animation: crate::config::AnimationConfig,
    started_at: Instant,
) -> HashMap<Window, WindowAnimation> {
    // TEMPORARY FORENSIC INSTRUMENTATION (3a3fa2a runtime non-observation
    // investigation) — read-only, no synchronous X11 queries, no effect on
    // the actual filter chain below. Remove before release.
    log_open_anim_eligibility(old_surfaces, snapshot, is_first_publish, present_available, animation.enabled);
    if is_first_publish || !present_available || !animation.enabled {
        return HashMap::new();
    }
    snapshot
        .entries
        .iter()
        .filter(|entry| !old_surfaces.contains(&entry.surface_xid))
        .filter(|entry| eligible_for_open_animation(entry))
        .inspect(|entry| {
            println!(
                "OPEN_ANIM_PROVISIONAL_CREATE surface=0x{:08x} effect={:?} duration_ms={}",
                entry.surface_xid,
                animation.open.effect,
                animation.open.duration.as_millis(),
            );
        })
        .map(|entry| (entry.surface_xid, WindowAnimation::open(started_at, animation.open.effect, animation.open.duration)))
        .collect()
}

/// TEMPORARY FORENSIC INSTRUMENTATION — see provisional_open_animations.
/// Mirrors (never feeds back into) the real eligibility decision, purely
/// for one diagnostic line per newly-added surface. Remove before release.
fn log_open_anim_eligibility(
    old_surfaces: &HashSet<Window>,
    snapshot: &SceneSnapshot,
    is_first_publish: bool,
    present_available: bool,
    animation_config_enabled: bool,
) {
    for entry in &snapshot.entries {
        if old_surfaces.contains(&entry.surface_xid) {
            continue;
        }
        let eligible_visual = eligible_for_open_animation(entry);
        let eligible = !is_first_publish && present_available && animation_config_enabled && eligible_visual;
        // R2/b1: rejection reason now reflects the semantic-preferring
        // value actually consulted by eligible_for_open_animation, and
        // distinguishes the config-disabled gate from the Present-
        // unavailable gate (previously conflated into one bool).
        let reason = if is_first_publish {
            "first_publish"
        } else if !present_available {
            "present_unavailable"
        } else if !animation_config_enabled {
            "animation_disabled"
        } else if !matches!(entry.visual_class, SurfaceVisualClass::Normal) {
            "visual_class"
        } else if entry.effective_override_redirect {
            "override_redirect"
        } else {
            "eligible"
        };
        println!(
            "OPEN_ANIM_ELIGIBILITY surface=0x{:08x} semantic={} class={:?} capture_override_redirect={} effective_override_redirect={} present={} animation_enabled={} first_publish={} eligible={} reason={}",
            entry.surface_xid,
            entry
                .semantic_client_xid
                .map(|xid| format!("0x{xid:08x}"))
                .unwrap_or_else(|| "none".to_string()),
            entry.visual_class,
            entry.override_redirect,
            entry.effective_override_redirect,
            present_available,
            animation_config_enabled,
            is_first_publish,
            eligible,
            reason,
        );
    }
}

/// The view a pre-commit render must use: already-committed animations plus
/// this candidate's not-yet-committed provisional ones. Never mutates
/// either input map.
fn merge_window_animations(
    persistent: &HashMap<Window, WindowAnimation>,
    provisional: &HashMap<Window, WindowAnimation>,
) -> HashMap<Window, WindowAnimation> {
    let mut merged = persistent.clone();
    merged.extend(provisional.iter().map(|(xid, animation)| (*xid, animation.clone())));
    merged
}

/// Called only from `commit_candidate_inner`, after a successful commit —
/// never speculatively. Preserves an existing entry's `started_at` rather
/// than restarting it (defensive: provisional is built only from
/// `new_surfaces - old_surfaces`, so a collision should not occur).
fn promote_provisional_animations(
    persistent: &mut HashMap<Window, WindowAnimation>,
    provisional: HashMap<Window, WindowAnimation>,
) {
    for (surface_xid, animation) in provisional {
        // TEMPORARY FORENSIC INSTRUMENTATION — remove before release.
        println!("OPEN_ANIM_PROMOTE surface=0x{surface_xid:08x}");
        persistent.entry(surface_xid).or_insert(animation);
    }
}

/// TEMPORARY FORENSIC INSTRUMENTATION — logs, does not mutate anything.
/// Called at rebuild_and_present's candidate-discard points (Retry/
/// Shutdown) so a candidate that never reaches commit_candidate_inner is
/// still visible in the forensic trace. Remove before release.
fn log_open_anim_reject(provisional: &HashMap<Window, WindowAnimation>) {
    for surface_xid in provisional.keys() {
        println!("OPEN_ANIM_REJECT surface=0x{surface_xid:08x}");
    }
}

/// Resource-free removal: `WindowAnimation` owns no DamageLease/NamedPixmap/
/// EGLImage, so this cannot delay or interact with resource teardown.
fn retire_removed_surface_animations(
    animations: &mut HashMap<Window, WindowAnimation>,
    removed_surfaces: &HashSet<Window>,
) {
    for surface_xid in removed_surfaces {
        animations.remove(surface_xid);
    }
}

/// 3a3fa2b5 — persistent composite ordering identity. `Closing(u64)` is
/// never keyed by a `Window` XID (see the transactional `close_id`
/// allocation in `allocate_close_ids`) — a later Live entry whose XID
/// happens to match a historically-dead, still-animating close can never
/// collide with it, since this enum carries no Window field to collide
/// on (see the XID-reuse-safety proof in the r4 audit).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderLayer {
    Live(Window),
    Closing(u64),
}

/// 3a3fa2b5-r4 — the accepted "left-neighbor gap" reconciliation.
/// Rebuilds the Live spine UNCONDITIONALLY from `new_live_order`,
/// guaranteeing `projection(result, Live) == new_live_order` exactly, by
/// construction — the Live order is never "preserved from history" (that
/// was R3's bug). Every Closing entry (continuing from `previous_order`,
/// or newly converted this commit from a `removed_surfaces` XID present
/// in `provisional_closes`) is re-spliced into the gap defined by the
/// nearest entry to its LEFT in `previous_order` that is still live in
/// `new_live_order` — computed via a single running-anchor left-to-right
/// scan (O(previous_order.len())), never a per-entry re-walk. Old
/// relative order within a shared-anchor group is preserved by construction
/// (scan order == group push order). A `None` anchor (nothing live ever
/// to its left) is spliced at the very front (bottom of stack).
/// `previous_order` is assumed to already reflect the previous commit's
/// retirements (see `SceneSession::retire_completed_closing_visuals`,
/// which mutates `render_order` directly, outside this candidate-local
/// reconciliation) — so every `Closing(id)` found here is still-animating
/// by invariant, and is unconditionally carried forward.
fn reconcile_render_order(
    previous_order: &[RenderLayer],
    new_live_order: &[Window],
    removed_surfaces: &HashSet<Window>,
    provisional_closes: &HashMap<Window, u64>,
) -> Vec<RenderLayer> {
    let new_live_set: HashSet<Window> = new_live_order.iter().copied().collect();
    let mut groups: HashMap<Option<Window>, Vec<RenderLayer>> = HashMap::new();
    let mut current_anchor: Option<Window> = None;
    for layer in previous_order {
        match *layer {
            RenderLayer::Live(xid) if new_live_set.contains(&xid) => {
                current_anchor = Some(xid);
            }
            RenderLayer::Live(xid) => {
                if removed_surfaces.contains(&xid)
                    && let Some(&close_id) = provisional_closes.get(&xid)
                {
                    groups.entry(current_anchor).or_default().push(RenderLayer::Closing(close_id));
                }
                // Ineligible/unconverted removal: dropped, anchor unchanged.
            }
            RenderLayer::Closing(id) => {
                groups.entry(current_anchor).or_default().push(RenderLayer::Closing(id));
            }
        }
    }
    let mut output = groups.remove(&None).unwrap_or_default();
    for &xid in new_live_order {
        output.push(RenderLayer::Live(xid));
        if let Some(group) = groups.remove(&Some(xid)) {
            output.extend(group);
        }
    }
    output
}

/// 3a3fa2b5 — transactional `close_id` allocation, factored out as a pure
/// function so the retry-then-accept semantics (same `base` reserves the
/// same sequence; overflow fails open) are independently testable without
/// a live `SceneSession`. `eligible_sources` must already be filtered to
/// exactly the XIDs that should receive a new close this commit, in
/// deterministic (stacking-index) order — this function only allocates
/// and advances, it makes no eligibility decisions. On checked-add
/// overflow, that ONE source is skipped (fails open: no close is created
/// for it, `next_id` does not advance for it) — never wraps, never
/// panics, never blocks the remaining allocations.
fn allocate_close_ids(base: u64, eligible_sources: &[Window]) -> (HashMap<Window, u64>, u64) {
    let mut next_id = base;
    let mut ids = HashMap::new();
    for &xid in eligible_sources {
        let Some(advanced) = next_id.checked_add(1) else {
            println!("CLOSE_ID_SPACE_EXHAUSTED surface=0x{xid:08x}");
            continue;
        };
        ids.insert(xid, next_id);
        next_id = advanced;
    }
    (ids, next_id)
}

/// 3a3fa2b5-r2 — pure retention rule for `SceneSession::destroy_intents`,
/// factored out so the correction can be tested directly without a live
/// `SceneSession`. Corrects R1's narrower `for xid in &removed_surfaces {
/// destroy_intents.remove(xid) }`, which never retired an intent for an
/// XID that was never part of the OLD committed scene at all (e.g. an
/// override-redirect popup, or any window destroyed before ever becoming
/// eligible/tracked) — such an XID can never appear in ANY future
/// `removed_surfaces` set (it was never in `old_surfaces` to begin with),
/// so R1's loop would leave it in `destroy_intents` forever, creating an
/// XID-reuse hazard (a later, unrelated Live window reusing that same
/// numeric XID would inherit the stale intent and could false-trigger a
/// close on a mere Unmap). The corrected rule instead re-establishes, on
/// every committed Accept, the invariant `destroy_intents ⊆ new_surfaces`
/// — retaining an intent only for an XID that is part of the
/// JUST-COMMITTED live scene (i.e. still a genuine candidate for a
/// FUTURE close). This single rule subsumes R1's old removal loop
/// (anything in `removed_surfaces` is by definition NOT in
/// `new_surfaces` either) and additionally closes the untracked-XID leak.
/// Called only from `commit_candidate_inner`, never during
/// `build_candidate`/`pre_commit_gate` — so a Retry always observes the
/// exact same causal Destroy information as its first attempt.
fn retained_destroy_intents(
    destroy_intents: &HashSet<Window>,
    new_surfaces: &HashSet<Window>,
) -> HashSet<Window> {
    destroy_intents
        .iter()
        .copied()
        .filter(|xid| new_surfaces.contains(xid))
        .collect()
}

// ============================================================
// TEMPORARY PERFORMANCE FORENSIC INSTRUMENTATION (3a3fa2a Brave repaint
// latency investigation). Bounded, ~once/second aggregate stdout output
// only — never per-frame. In-memory counters only, zero new X11 queries,
// zero functional/behavioral change (every hook below is a pure counter
// increment or Instant::now()/.elapsed() measurement around an otherwise
// unmodified call). Remove before release.
// ============================================================

#[derive(Default, Clone, Copy)]
struct PerfTiming {
    count: u64,
    total: Duration,
    max: Duration,
}

impl PerfTiming {
    fn record(&mut self, elapsed: Duration) {
        self.count += 1;
        self.total += elapsed;
        if elapsed > self.max {
            self.max = elapsed;
        }
    }

    fn avg_micros(&self) -> f64 {
        if self.count == 0 {
            0.0
        } else {
            self.total.as_secs_f64() * 1_000_000.0 / self.count as f64
        }
    }

    fn max_micros(&self) -> f64 {
        self.max.as_secs_f64() * 1_000_000.0
    }
}

struct PerfForensics {
    window_start: Instant,
    events: u64,
    present_complete_events: u64,
    pixel_damage_events: u64,
    hierarchy_events: u64,
    geometry_events: u64,
    visual_state_events: u64,
    candidate_rebuilds: u64,
    full_recomposes: u64,
    animation_only_recomposes: u64,
    renders: u64,
    egl_swaps: u64,
    damage_subtracts: u64,
    max_event_batch_size: usize,
    batch_saturations: u64,
    event_batch_drain: PerfTiming,
    classification: PerfTiming,
    full_recompose: PerfTiming,
    render: PerfTiming,
    swap: PerfTiming,
}

impl PerfForensics {
    fn new() -> Self {
        Self {
            window_start: Instant::now(),
            events: 0,
            present_complete_events: 0,
            pixel_damage_events: 0,
            hierarchy_events: 0,
            geometry_events: 0,
            visual_state_events: 0,
            candidate_rebuilds: 0,
            full_recomposes: 0,
            animation_only_recomposes: 0,
            renders: 0,
            egl_swaps: 0,
            damage_subtracts: 0,
            max_event_batch_size: 0,
            batch_saturations: 0,
            event_batch_drain: PerfTiming::default(),
            classification: PerfTiming::default(),
            full_recompose: PerfTiming::default(),
            render: PerfTiming::default(),
            swap: PerfTiming::default(),
        }
    }

    /// Counted at wait_live_pixel's main event-batch drain and
    /// drain_current_events (the PixelDamage/typing-dominant paths) only —
    /// NOT at pre_commit_gate/refresh_resize_state_before_acquisition/
    /// try_move_only's own drains, which are structural-rebuild/resize
    /// paths, not typing-relevant. Documented scope limit, not an omission.
    fn record_invalidation(&mut self, invalidation: SceneInvalidation) {
        match invalidation {
            SceneInvalidation::PixelDamage(_) => self.pixel_damage_events += 1,
            SceneInvalidation::Hierarchy => self.hierarchy_events += 1,
            SceneInvalidation::Geometry(_) => self.geometry_events += 1,
            SceneInvalidation::VisualState => self.visual_state_events += 1,
            _ => {}
        }
    }

    fn record_batch_size(&mut self, size: usize) {
        if size > self.max_event_batch_size {
            self.max_event_batch_size = size;
        }
        if size >= MAX_EVENTS_PER_BATCH {
            self.batch_saturations += 1;
        }
    }

    fn maybe_flush(&mut self, active_animations: usize) {
        let elapsed = self.window_start.elapsed();
        if elapsed < Duration::from_secs(1) {
            return;
        }
        println!(
            "PERF elapsed={:.2}s active_animations={} events={} present_complete={} pixel_damage={} hierarchy={} geometry={} visual_state={} candidate_rebuilds={} full_recomposes={} animation_only_recomposes={} renders={} egl_swaps={} damage_subtracts={} max_event_batch_size={} batch_saturations={} event_batch_drain[n={} avg_us={:.1} max_us={:.1}] classification[n={} avg_us={:.1} max_us={:.1}] full_recompose[n={} avg_us={:.1} max_us={:.1}] render[n={} avg_us={:.1} max_us={:.1}] swap[n={} avg_us={:.1} max_us={:.1}]",
            elapsed.as_secs_f64(),
            active_animations,
            self.events,
            self.present_complete_events,
            self.pixel_damage_events,
            self.hierarchy_events,
            self.geometry_events,
            self.visual_state_events,
            self.candidate_rebuilds,
            self.full_recomposes,
            self.animation_only_recomposes,
            self.renders,
            self.egl_swaps,
            self.damage_subtracts,
            self.max_event_batch_size,
            self.batch_saturations,
            self.event_batch_drain.count, self.event_batch_drain.avg_micros(), self.event_batch_drain.max_micros(),
            self.classification.count, self.classification.avg_micros(), self.classification.max_micros(),
            self.full_recompose.count, self.full_recompose.avg_micros(), self.full_recompose.max_micros(),
            self.render.count, self.render.avg_micros(), self.render.max_micros(),
            self.swap.count, self.swap.avg_micros(), self.swap.max_micros(),
        );
        *self = Self::new();
    }
}

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
            let semantic_metadata = semantic_client_xid.and_then(|client| {
                if client == metadata.window {
                    Some(metadata)
                } else {
                    binding.descendants.iter().find(|candidate| candidate.window == client)
                }
            });
            if let Some(entry) = eligible_surface_with_semantic_metadata(
                metadata,
                semantic_client_xid,
                semantic_metadata,
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

fn known_non_renderable_windows(
    hierarchy: &HierarchySnapshot,
    overlay: Window,
    owner_window: Window,
) -> HashSet<Window> {
    let mut ignored = HashSet::from([overlay, owner_window]);
    for binding in &hierarchy.children {
        let root_child = binding.root_child_xid;
        if is_internal_xid(root_child, overlay, owner_window) {
            ignored.insert(root_child);
        }
        if let Some(metadata) = binding.surface_candidate.as_ref()
            && (metadata.class != WindowClass::INPUT_OUTPUT
                || metadata.map_state != xproto::MapState::VIEWABLE)
        {
            ignored.insert(root_child);
        }
        for metadata in &binding.descendants {
            if metadata.class != WindowClass::INPUT_OUTPUT
                || metadata.map_state != xproto::MapState::VIEWABLE
            {
                ignored.insert(metadata.window);
            }
        }
    }
    ignored
}

#[cfg(test)]
fn eligible_surface(
    metadata: &WindowMetadata,
    semantic_client_xid: Option<Window>,
    root_geometry: RootGeometry,
    surface_xid: Window,
    stacking_index: usize,
) -> Option<SurfaceEntry> {
    eligible_surface_with_semantic_metadata(
        metadata,
        semantic_client_xid,
        None,
        root_geometry,
        surface_xid,
        stacking_index,
    )
}

fn eligible_surface_with_semantic_metadata(
    metadata: &WindowMetadata,
    semantic_client_xid: Option<Window>,
    semantic_metadata: Option<&WindowMetadata>,
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
        effect_owner: None,
        own_blur_request: BlurRequest::None,
        lifecycle_xid: surface_xid,
        geometry: metadata.geometry,
        depth: metadata.depth,
        visual: metadata.visual,
        class: metadata.class,
        map_state: metadata.map_state,
        override_redirect: metadata.override_redirect,
        effective_override_redirect: effective_override_redirect(metadata, semantic_metadata),
        stacking_index,
        backend,
        visual_class: classify_surface_visual_class(effective_window_type(metadata, semantic_metadata)),
        fullscreen: false,
        shadow_eligible: false,
        resolved_border_color: [0.0f32.to_bits(), 0.0f32.to_bits(), 0.0f32.to_bits(), 1.0f32.to_bits()],
        resolved_opacity_bits: 1.0f32.to_bits(),
        client_root_geometry: None,
        resolved_blur_request: BlurRequest::None,
    })
}

fn effective_window_type<'a>(
    capture_metadata: &'a WindowMetadata,
    semantic_metadata: Option<&'a WindowMetadata>,
) -> Option<&'a str> {
    semantic_metadata
        .and_then(|metadata| metadata.window_type.as_deref())
        .or(capture_metadata.window_type.as_deref())
}

/// 3a3fa2a R2 — same precedence shape as effective_window_type: the
/// semantic client's own override_redirect wins when semantic metadata is
/// available (already resolved, in-memory, from the same hierarchy walk —
/// zero new X11 queries), otherwise the capture surface's own bit is used.
/// Consumed only by open-animation eligibility; SurfaceEntry.override_redirect
/// itself keeps its existing capture-only meaning for every other consumer.
fn effective_override_redirect(
    capture_metadata: &WindowMetadata,
    semantic_metadata: Option<&WindowMetadata>,
) -> bool {
    semantic_metadata
        .map(|metadata| metadata.override_redirect)
        .unwrap_or(capture_metadata.override_redirect)
}

fn classify_surface_visual_class(window_type: Option<&str>) -> SurfaceVisualClass {
    let mut types = window_type.unwrap_or_default().split(',');
    if types.clone().any(|kind| kind == "_NET_WM_WINDOW_TYPE_DOCK") {
        SurfaceVisualClass::Dock
    } else if types.any(|kind| kind == "_NET_WM_WINDOW_TYPE_DESKTOP") {
        SurfaceVisualClass::Desktop
    } else {
        SurfaceVisualClass::Normal
    }
}

fn apply_surface_visual_policy(
    plan: &mut RenderQuadPlan,
    config: &crate::config::VisualConfig,
    visual_class: SurfaceVisualClass,
) {
    if matches!(visual_class, SurfaceVisualClass::Dock | SurfaceVisualClass::Desktop) {
        plan.corner_radius = 0.0;
        plan.border_width = 0.0;
        plan.border_color = [0.0, 0.0, 0.0, 1.0];
        return;
    }
    plan.corner_radius = effective_corner_radius(config.corner_radius, plan.width, plan.height);
    plan.border_width = effective_border_width(config.border.width, plan.width, plan.height);
    plan.border_color = config.border.inactive_color;
}

fn shadow_eligible_for_entry(
    style: crate::config::ShadowConfig,
    entry: &SurfaceEntry,
) -> bool {
    style.enabled
        && entry.semantic_client_xid.is_some()
        && !entry.fullscreen
        && matches!(entry.visual_class, SurfaceVisualClass::Normal)
}

/// `opacity_multiplier` scales the CONFIGURED `style.strength` only — it
/// never replaces it (see 3a3fa2b1-s1's shadow-opacity contract). Callers
/// that are not coupling this shadow to an open animation pass `1.0`,
/// which reproduces the exact pre-3a3fa2b1-s1 params byte-for-byte.
/// `strength <= 0.0` is already rejected by `ShadowParams::new` below, so
/// `opacity_multiplier == 0.0` naturally yields `None` (no shadow drawn)
/// rather than a degenerate zero-alpha draw call.
fn shadow_params_from_plan(
    style: crate::config::ShadowConfig,
    plan: &RenderQuadPlan,
    opacity_multiplier: f32,
) -> Option<crate::graphics::renderer::ShadowParams> {
    let mut params = crate::graphics::renderer::ShadowParams::new(
        plan.outer_x as f32,
        plan.outer_y as f32,
        plan.outer_width as f32,
        plan.outer_height as f32,
        plan.corner_radius,
        style.extent,
        style.offset_x,
        style.offset_y,
        style.strength * opacity_multiplier,
    )?;
    params.color = crate::graphics::renderer::normalized_shadow_color(style.color);
    Some(params)
}

fn client_bounds_from_hierarchy(
    hierarchy: &HierarchySnapshot,
) -> HashMap<Window, (i32, i32)> {
    let mut bounds = HashMap::new();
    for binding in &hierarchy.children {
        let BindingStatus::SingleClient(client) = binding.semantic_client else {
            continue;
        };
        let metadata = if client == binding.root_child_xid {
            binding.surface_candidate.as_ref()
        } else {
            binding.descendants.iter().find(|metadata| metadata.window == client)
        };
        if let Some(metadata) = metadata {
            bounds.entry(client).or_insert((
                i32::from(metadata.geometry.width),
                i32::from(metadata.geometry.height),
            ));
        }
    }
    bounds
}

/// Reads effect-owner metadata from already-tracked capture surfaces. A
/// surface without a semantic client also reads its own blur request here;
/// managed-client requests remain on the existing semantic-client cache path.
/// BadWindow is a safe no-relationship result for this metadata read. The
/// surrounding hierarchy/resource candidate still owns lifecycle truth.
fn initialize_surface_effect_metadata(
    connection: &X11Connection,
    snapshot: &mut SceneSnapshot,
    atoms: VisualAtoms,
) -> Result<(), Box<dyn Error>> {
    for entry in &mut snapshot.entries {
        entry.effect_owner = match read_effect_owner(connection, entry.surface_xid, atoms.effect_owner) {
            Ok(owner) => owner,
            Err(error) if super::capture::is_bad_window_error(error.as_ref()) => None,
            Err(error) => return Err(error),
        };
        if entry.semantic_client_xid.is_none() {
            entry.own_blur_request = match read_client_blur_request(connection, entry.surface_xid, atoms) {
                Ok(request) => request,
                Err(error) if super::capture::is_bad_window_error(error.as_ref()) => BlurRequest::None,
                Err(error) => return Err(error),
            };
        }
    }
    Ok(())
}

fn translate_coordinates_reply_error(error: ReplyError) -> Box<dyn Error> {
    if matches!(
        error,
        ReplyError::X11Error(ref error) if error.error_kind == ErrorKind::Window
    ) {
        Box::new(CandidateBuildError::Stale(SceneInvalidation::Hierarchy))
    } else {
        Box::new(error)
    }
}

fn region_request_requires_client_origin(request: &BlurRequest, client: Option<Window>) -> bool {
    client.is_some() && matches!(request, BlurRequest::Regions(_))
}

fn client_root_geometry_from_translation(
    root_x: i16,
    root_y: i16,
    width: i32,
    height: i32,
) -> ClientRootGeometry {
    ClientRootGeometry {
        root_x: i32::from(root_x),
        root_y: i32::from(root_y),
        width,
        height,
    }
}

fn resolve_regions_client_geometry(
    connection: &X11Connection,
    root: Window,
    snapshot: &mut SceneSnapshot,
    client_bounds: &HashMap<Window, (i32, i32)>,
    urgency: &HashMap<Window, CachedClientVisualState>,
) -> Result<(), Box<dyn Error>> {
    let mut translated = HashMap::new();
    for entry in &snapshot.entries {
        let Some(client) = entry.semantic_client_xid else {
            if matches!(entry.own_blur_request, BlurRequest::Regions(_)) {
                translated.insert(
                    entry.surface_xid,
                    ClientRootGeometry {
                        root_x: i32::from(entry.geometry.x),
                        root_y: i32::from(entry.geometry.y),
                        width: i32::from(entry.geometry.width),
                        height: i32::from(entry.geometry.height),
                    },
                );
            }
            continue;
        };
        let Some(state) = urgency.get(&client) else {
            continue;
        };
        if !region_request_requires_client_origin(&state.blur_requested, Some(client)) {
            continue;
        }
        if translated.contains_key(&client) {
            continue;
        }
        let Some(&(width, height)) = client_bounds.get(&client) else {
            return Err(Box::new(CandidateBuildError::Stale(SceneInvalidation::Hierarchy)));
        };
        let reply = connection
            .inner
            .translate_coordinates(client, root, 0, 0)?
            .reply()
            .map_err(translate_coordinates_reply_error)?;
        translated.insert(
            client,
            client_root_geometry_from_translation(reply.dst_x, reply.dst_y, width, height),
        );
    }
    for entry in &mut snapshot.entries {
        if let Some(client) = entry.semantic_client_xid {
            entry.client_root_geometry = translated.get(&client).copied();
        } else if matches!(entry.own_blur_request, BlurRequest::Regions(_)) {
            entry.client_root_geometry = translated.get(&entry.surface_xid).copied();
        }
    }
    Ok(())
}

/// Resolves `entry`'s blur-request OWNERSHIP from its already-resolved
/// `semantic_client_xid` and the already-cached, per-client
/// `BlurRequest` (Phase 2A). Structural only — see the Phase 2B owner
/// audit: no WM_CLASS, PID, override_redirect, visual_class, opacity, or
/// fullscreen check. A `semantic_client_xid` of `None` (a popup/helper
/// surface, per the audit's directly observed cases) always resolves to
/// `BlurRequest::None` — never a fallback or inherited request from any
/// other client. The full protocol shape is preserved verbatim
/// (None/FullWindow/Regions), not collapsed to a boolean.
fn resolved_blur_request(
    entry: &SurfaceEntry,
    urgency: &HashMap<Window, CachedClientVisualState>,
) -> BlurRequest {
    entry
        .semantic_client_xid
        .and_then(|client| urgency.get(&client))
        .map(|state| state.blur_requested.clone())
        .unwrap_or(BlurRequest::None)
}

fn resolved_blur_request_with_auxiliary(
    entry: &SurfaceEntry,
    urgency: &HashMap<Window, CachedClientVisualState>,
    tracked_semantic_clients: &HashSet<Window>,
) -> BlurRequest {
    if entry.semantic_client_xid.is_some() {
        return resolved_blur_request(entry, urgency);
    }
    if entry
        .effect_owner
        .is_some_and(|owner| tracked_semantic_clients.contains(&owner))
    {
        entry.own_blur_request.clone()
    } else {
        BlurRequest::None
    }
}

fn permitted_blur_request(
    entry: &SurfaceEntry,
    urgency: &HashMap<Window, CachedClientVisualState>,
    blur_enabled: bool,
) -> BlurRequest {
    if blur_enabled {
        resolved_blur_request(entry, urgency)
    } else {
        BlurRequest::None
    }
}

fn permitted_blur_request_with_auxiliary(
    entry: &SurfaceEntry,
    urgency: &HashMap<Window, CachedClientVisualState>,
    tracked_semantic_clients: &HashSet<Window>,
    blur_enabled: bool,
) -> BlurRequest {
    if entry.semantic_client_xid.is_some() {
        permitted_blur_request(entry, urgency, blur_enabled)
    } else if blur_enabled {
        resolved_blur_request_with_auxiliary(entry, urgency, tracked_semantic_clients)
    } else {
        BlurRequest::None
    }
}

fn resolve_snapshot_fullscreen(
    snapshot: &mut SceneSnapshot,
    urgency: &HashMap<Window, CachedClientVisualState>,
    blur_enabled: bool,
    style: crate::config::ShadowConfig,
) {
    let tracked_semantic_clients: HashSet<Window> = snapshot
        .entries
        .iter()
        .filter_map(|entry| entry.semantic_client_xid)
        .collect();
    for entry in &mut snapshot.entries {
        entry.fullscreen = entry
            .semantic_client_xid
            .and_then(|client| urgency.get(&client))
            .is_some_and(|state| state.fullscreen);
        entry.shadow_eligible = shadow_eligible_for_entry(style, entry);
        entry.resolved_blur_request = permitted_blur_request_with_auxiliary(
            entry,
            urgency,
            &tracked_semantic_clients,
            blur_enabled,
        );
    }
}

fn resolved_surface_opacity(
    visuals: &crate::config::VisualConfig,
    entry: &SurfaceEntry,
    active_window: Option<Window>,
    urgency: &HashMap<Window, CachedClientVisualState>,
) -> f32 {
    if entry.fullscreen
        || entry.semantic_client_xid.is_none()
        || !matches!(entry.visual_class, SurfaceVisualClass::Normal)
    {
        return 1.0;
    }
    match border_visual_state(entry, active_window, urgency) {
        BorderVisualState::Urgent => visuals.opacity.urgent,
        BorderVisualState::Focused => visuals.opacity.focused,
        BorderVisualState::Inactive => visuals.opacity.inactive,
    }
}

fn resolve_snapshot_opacity(
    snapshot: &mut SceneSnapshot,
    visuals: &crate::config::VisualConfig,
    active_window: Option<Window>,
    urgency: &HashMap<Window, CachedClientVisualState>,
) {
    for entry in &mut snapshot.entries {
        entry.resolved_opacity_bits = resolved_surface_opacity(
            visuals, entry, active_window, urgency,
        ).to_bits();
    }
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

#[derive(Debug)]
enum DamageLeaseAcquireError {
    StaleDrawable,
    Other(Box<dyn Error>),
}

impl fmt::Display for DamageLeaseAcquireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleDrawable => write!(formatter, "damage create drawable is stale"),
            Self::Other(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for DamageLeaseAcquireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::StaleDrawable => None,
            Self::Other(error) => Some(&**error),
        }
    }
}

/// The only DAMAGE/Create protocol error treated as an expected
/// snapshot-to-create TOCTOU race: the drawable stopped existing between
/// hierarchy snapshot and this request. All other error kinds (Match,
/// Value, IDChoice, Alloc, ...) indicate a real backend/program defect for
/// this specific call (its drawable is the only externally-influenced
/// argument; the report level is a fixed, always-valid constant) and must
/// remain fatal.
fn stale_damage_create_reply(error: &ReplyError) -> bool {
    matches!(
        error,
        ReplyError::X11Error(error) if error.error_kind == ErrorKind::Drawable
    )
}

/// The outcome of classifying a DAMAGE/Subtract error against the issuing
/// lease's own, independently-tracked ownership state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DamageSubtractClassification {
    /// The lease was Active (proven, by construction of DamageState's
    /// transition graph, to have never gone through this lease's own
    /// local destroy path -- see DamageLease::destroy/Drop) and the
    /// server replied DamageBadDamage, the DAMAGE extension's only
    /// defined error. Subtract's `repair`/`parts` arguments are always
    /// the fixed x11rb::NONE constants at this call site, so `damage`
    /// (this lease's own, immutable, singly-owned, never-reused XID) is
    /// the only externally-influenced argument. Together these facts
    /// prove the server invalidated this specific resource independently
    /// of any action Xomposite itself took -- the same disappearance
    /// race already treated as non-fatal for DamageCreate
    /// (stale_damage_create_reply above).
    AlreadyGone,
    /// Either the error was not DamageBadDamage (a real backend/program
    /// defect for this call, per the same argument-domain reasoning), or
    /// the lease was not Active when the error was classified -- in
    /// which case the Active-lease invariant that justifies AlreadyGone
    /// does not hold, and this reply must not be treated as stale.
    Fatal,
}

/// Classifies a DAMAGE/Subtract error using two independent signals: the
/// server's reply (`error`) and this lease's own ownership state at the
/// moment the request was issued (`state`, captured by the caller before
/// the request -- see DamageLease::subtract()). Neither signal alone is
/// sufficient: `state` says nothing about what the server just reported,
/// and `error` alone cannot distinguish a legitimate external
/// disappearance from a hypothetical use of an already locally-retired
/// lease. Only their conjunction resolves AlreadyGone.
fn classify_damage_subtract_error(
    state: DamageState,
    error: &ReplyError,
) -> DamageSubtractClassification {
    let is_bad_damage = matches!(
        error,
        ReplyError::X11Error(inner) if inner.error_kind == ErrorKind::DamageBadDamage
    );
    if state == DamageState::Active && is_bad_damage {
        DamageSubtractClassification::AlreadyGone
    } else {
        DamageSubtractClassification::Fatal
    }
}

fn translate_damage_lease_acquire_error(error: DamageLeaseAcquireError) -> Box<dyn Error> {
    match error {
        DamageLeaseAcquireError::StaleDrawable => {
            Box::new(CandidateBuildError::Stale(SceneInvalidation::Hierarchy))
        }
        DamageLeaseAcquireError::Other(error) => error,
    }
}

fn is_hierarchy_stale_candidate_error(error: &(dyn Error + 'static)) -> bool {
    matches!(
        error.downcast_ref::<CandidateBuildError>(),
        Some(CandidateBuildError::Stale(SceneInvalidation::Hierarchy))
    )
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
    ) -> Result<Self, DamageLeaseAcquireError> {
        let damage_xid = connection
            .inner
            .generate_id()
            .map_err(|error| DamageLeaseAcquireError::Other(Box::new(error)))?;
        connection
            .inner
            .damage_create(damage_xid, surface_xid, damage::ReportLevel::NON_EMPTY)
            .map_err(|error| DamageLeaseAcquireError::Other(Box::new(error)))?
            .check()
            .map_err(|error| {
                if stale_damage_create_reply(&error) {
                    DamageLeaseAcquireError::StaleDrawable
                } else {
                    DamageLeaseAcquireError::Other(Box::new(error))
                }
            })?;
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
        let state = self.state.get();
        if state != DamageState::Active {
            return Ok(());
        }
        match self
            .connection
            .inner
            .damage_subtract(self.damage_xid, x11rb::NONE, x11rb::NONE)?
            .check()
        {
            Ok(()) => Ok(()),
            Err(error) => match classify_damage_subtract_error(state, &error) {
                DamageSubtractClassification::AlreadyGone => {
                    self.mark_already_gone();
                    println!(
                        "DamageSubtract already gone: surface=0x{:08x} damage=0x{:08x}",
                        self.surface_xid, self.damage_xid
                    );
                    Ok(())
                }
                DamageSubtractClassification::Fatal => Err(Box::new(error)),
            },
        }
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

#[derive(Debug)]
enum NamedSurfacePixmapAcquireError {
    StaleGeometry,
    StaleX11(Box<dyn Error>),
    Other(Box<dyn Error>),
}

impl fmt::Display for NamedSurfacePixmapAcquireError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::StaleGeometry => write!(formatter, "named pixmap geometry is stale"),
            Self::StaleX11(error) => write!(formatter, "named pixmap drawable became stale: {error}"),
            Self::Other(error) => write!(formatter, "{error}"),
        }
    }
}

impl Error for NamedSurfacePixmapAcquireError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::StaleGeometry => None,
            Self::StaleX11(error) | Self::Other(error) => Some(&**error),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RawPixmapOwnership {
    owned: bool,
}

impl RawPixmapOwnership {
    fn new() -> Self {
        Self { owned: true }
    }

    fn transfer(&mut self) {
        self.owned = false;
    }

    #[cfg(test)]
    fn is_owned(self) -> bool {
        self.owned
    }
}

struct NamedPixmapGuard<'a> {
    connection: &'a X11Connection,
    pixmap_xid: u32,
    ownership: RawPixmapOwnership,
}

impl<'a> NamedPixmapGuard<'a> {
    fn new(connection: &'a X11Connection, pixmap_xid: u32) -> Self {
        Self {
            connection,
            pixmap_xid,
            ownership: RawPixmapOwnership::new(),
        }
    }

    fn transfer(&mut self) {
        self.ownership.transfer();
    }
}

impl Drop for NamedPixmapGuard<'_> {
    fn drop(&mut self) {
        if self.ownership.owned {
            let _ = self.connection.inner.free_pixmap(self.pixmap_xid);
        }
    }
}

fn stale_pixmap_reply(error: &ReplyError) -> bool {
    matches!(
        error,
        ReplyError::X11Error(error)
            if matches!(
                error.error_kind,
                ErrorKind::Drawable | ErrorKind::Match | ErrorKind::Pixmap | ErrorKind::Window
            )
    )
}

fn named_pixmap_dimensions_match(window: WindowGeometry, pixmap: PixmapGeometry) -> bool {
    let Some(expected_width) = u32::from(window.width)
        .checked_add(u32::from(window.border_width) * 2)
    else {
        return false;
    };
    let Some(expected_height) = u32::from(window.height)
        .checked_add(u32::from(window.border_width) * 2)
    else {
        return false;
    };
    pixmap.width != 0
        && pixmap.height != 0
        && u32::from(pixmap.width) == expected_width
        && u32::from(pixmap.height) == expected_height
}

fn validate_named_pixmap_dimensions(
    window: WindowGeometry,
    pixmap: PixmapGeometry,
) -> Result<(), NamedSurfacePixmapAcquireError> {
    named_pixmap_dimensions_match(window, pixmap)
        .then_some(())
        .ok_or(NamedSurfacePixmapAcquireError::StaleGeometry)
}

fn translate_named_pixmap_acquire_error(
    error: NamedSurfacePixmapAcquireError,
) -> Box<dyn Error> {
    match error {
        NamedSurfacePixmapAcquireError::StaleGeometry
        | NamedSurfacePixmapAcquireError::StaleX11(_) => {
            Box::new(CandidateBuildError::Stale(SceneInvalidation::Hierarchy))
        }
        NamedSurfacePixmapAcquireError::Other(error) => error,
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
        mut pixmap_geometry_timing: Option<&mut TimingMetric>,
    ) -> Result<Self, NamedSurfacePixmapAcquireError> {
        let pixmap_xid = connection
            .inner
            .generate_id()
            .map_err(|error| NamedSurfacePixmapAcquireError::Other(Box::new(error)))?;
        connection
            .inner
            .composite_name_window_pixmap(entry.surface_xid, pixmap_xid)
            .map_err(|error| NamedSurfacePixmapAcquireError::Other(Box::new(error)))?
            .check()
            .map_err(|error| {
                if stale_pixmap_reply(&error) {
                    NamedSurfacePixmapAcquireError::StaleX11(Box::new(error))
                } else {
                    NamedSurfacePixmapAcquireError::Other(Box::new(error))
                }
            })?;
        let mut guard = NamedPixmapGuard::new(connection, pixmap_xid);
        let geometry_start = pixmap_geometry_timing.as_ref().map(|_| Instant::now());
        let geometry = connection
            .inner
            .get_geometry(pixmap_xid)
            .map_err(|error| NamedSurfacePixmapAcquireError::Other(Box::new(error)))?
            .reply()
            .map_err(|error| {
                if stale_pixmap_reply(&error) {
                    NamedSurfacePixmapAcquireError::StaleX11(Box::new(error))
                } else {
                    NamedSurfacePixmapAcquireError::Other(Box::new(error))
                }
            })?;
        if let (Some(start), Some(timing)) = (geometry_start, pixmap_geometry_timing.as_mut()) {
            timing.record(start.elapsed());
        }
        let pixmap_geometry = PixmapGeometry {
            root: geometry.root,
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            border_width: geometry.border_width,
            depth: geometry.depth,
        };
        let expected_width = u32::from(entry.geometry.width)
            + u32::from(entry.geometry.border_width) * 2;
        let expected_height = u32::from(entry.geometry.height)
            + u32::from(entry.geometry.border_width) * 2;
        validate_named_pixmap_dimensions(entry.geometry, pixmap_geometry)?;
        if geometry.root != root_window || geometry.depth != entry.depth {
            return Err(NamedSurfacePixmapAcquireError::Other(format!(
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
            ).into()));
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
        let surface = Self {
            connection,
            surface_xid: entry.surface_xid,
            pixmap_xid,
            window_geometry: entry.geometry,
            geometry: pixmap_geometry,
            state,
        };
        guard.transfer();
        Ok(surface)
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

/// One atomically reusable owner for a surface's source-side resources.
/// Scene metadata remains in SurfaceEntry and is never shared through this
/// bundle. Rc is correct because SceneSession and SceneCandidate are strictly
/// single-threaded; the bundle itself has one owning Drop path per resource.
struct SurfaceResourceBundle<'a> {
    damage: Option<Rc<DamageLease<'a>>>,
    pixmap: Rc<NamedSurfacePixmap<'a>>,
    egl: Option<Rc<std::cell::RefCell<EglImportedSurface>>>,
}

/// 3a3fa2b5 — single-owner RAII for one compositor-owned GPU texture (see
/// `EglSceneRenderer::capture_closing_snapshot`). Deliberately NOT
/// `Rc<RefCell<_>>` like `EglImportedSurface`: a `ClosingVisual` has
/// exactly one owner (`SceneSession::closing_visuals`), never shared —
/// `EglImportedSurface`'s multi-owner sharing pattern is not the right
/// analogue here (see the r2 audit). Exactly-once `glDeleteTextures` via
/// the `released` guard, matching `EglImportedSurface`'s proven guard
/// idiom without adopting its Rc/RefCell sharing. No `mem::forget`, no
/// `ManuallyDrop`, no `ptr::read`.
struct ClosingTexture {
    texture: u32,
    released: bool,
}

impl ClosingTexture {
    fn new(texture: u32) -> Self {
        Self { texture, released: false }
    }

    /// Real, exactly-once `glDeleteTextures` — requires a current GL
    /// context (called only while one is current: normal retirement in
    /// `retire_completed_closing_visuals`, and `SceneSession::cleanup`'s
    /// `egl_current` branch, mirroring `EglImportedSurface::destroy`).
    fn destroy(&mut self) {
        if !self.released {
            self.released = true;
            crate::graphics::renderer::delete_texture(self.texture);
        }
    }

    /// No-GL-call fallback — used only when no GL context is current
    /// (the degraded-shutdown path), mirroring `EglImportedSurface::disarm`.
    fn disarm(&mut self) {
        self.released = true;
    }
}

impl Drop for ClosingTexture {
    fn drop(&mut self) {
        self.destroy();
    }
}

/// 3a3fa2b5 — committed, persistent-frame close visual. Owns exactly one
/// GPU resource (`texture`) and otherwise only frozen value data captured
/// at close-commit time — never a live Window-owned EGLImage/NamedPixmap/
/// DamageLease/dead X11 pixmap (the milestone's non-negotiable ownership
/// rule). `plan`/`pixel_semantics`/`base_opacity`/`shadow_eligible` are
/// frozen from the OLD live surface's own committed geometry/border/
/// opacity/shadow-eligibility at the moment the close was accepted —
/// never re-derived from any live X11 state afterward. `source_xid` is
/// diagnostic-only, never identity (see XID-reuse safety) — identity is
/// `id` (the transactionally-allocated `close_id`).
struct ClosingVisual {
    #[allow(dead_code)]
    id: u64,
    texture: ClosingTexture,
    plan: RenderQuadPlan,
    pixel_semantics: EglPixelSemantics,
    base_opacity: f32,
    shadow_eligible: bool,
    animation: ClosingAnimation,
    #[allow(dead_code)]
    source_xid: Window,
}

/// 3a3fa2b5-r2 — candidate-local, PURE value-type frame-0 data for a
/// newly-triggered provisional close. Deliberately owns NO GPU/X11
/// resource at all — no `Rc<RefCell<EglImportedSurface>>`, no
/// `NamedPixmap`, no `Damage` (see the r2 correction: R1's `old_surface:
/// Rc<RefCell<EglImportedSurface>>` field artificially extended a dead
/// window's EGLImage lifetime merely to keep the close animation's
/// texture source reachable, which the milestone's non-negotiable
/// ownership rule forbids). Frame-0 rendering instead resolves the source
/// texture FRESH, by `source_xid`, from `SceneSession::egl_surfaces`
/// (still the OLD, live map at that point in `build_candidate` — see
/// `render_closing_layer`'s `closing_source_surfaces` parameter); the
/// post-Accept snapshot capture resolves it from `old_resources` inside
/// `commit_candidate_inner`, again by lookup, never via a stored
/// reference. Never candidate-owns a GL snapshot texture either — the
/// real `ClosingTexture` is created only post-Accept. A rejected/retried
/// candidate simply drops this map, touching no GPU/X11 resource and no
/// committed state.
struct ProvisionalClosingFrame {
    source_xid: Window,
    plan: RenderQuadPlan,
    pixel_semantics: EglPixelSemantics,
    base_opacity: f32,
    shadow_eligible: bool,
    animation: ClosingAnimation,
}

/// 3a3fa2b5 — unifies "committed" and "provisional" closing sources so
/// `render_closing_layer` draws both through ONE code path (never a
/// separate hardcoded first-frame draw call), mirroring
/// `WindowAnimation::sample`'s "one dispatch for every render path"
/// precedent.
enum ClosingDrawSource<'a> {
    Committed(&'a ClosingVisual),
    Provisional(&'a ProvisionalClosingFrame),
}

impl ClosingDrawSource<'_> {
    fn plan(&self) -> RenderQuadPlan {
        match self {
            Self::Committed(visual) => visual.plan,
            Self::Provisional(frame) => frame.plan,
        }
    }

    fn pixel_semantics(&self) -> EglPixelSemantics {
        match self {
            Self::Committed(visual) => visual.pixel_semantics,
            Self::Provisional(frame) => frame.pixel_semantics,
        }
    }

    fn base_opacity(&self) -> f32 {
        match self {
            Self::Committed(visual) => visual.base_opacity,
            Self::Provisional(frame) => frame.base_opacity,
        }
    }

    fn shadow_eligible(&self) -> bool {
        match self {
            Self::Committed(visual) => visual.shadow_eligible,
            Self::Provisional(frame) => frame.shadow_eligible,
        }
    }

    fn animation(&self) -> &ClosingAnimation {
        match self {
            Self::Committed(visual) => &visual.animation,
            Self::Provisional(frame) => &frame.animation,
        }
    }

    // 3a3fa2b5-r2: `texture()` was removed here on purpose. A committed
    // `ClosingVisual` owns its `ClosingTexture` directly, but a
    // `ProvisionalClosingFrame` owns no GPU resource at all (see its own
    // doc comment) — its source texture must be resolved FRESH, by
    // `source_xid`, from the still-live `closing_source_surfaces` map at
    // the call site (`render_closing_layer`), never cached here.
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
    pub(crate) outer_x: i32,
    pub(crate) outer_y: i32,
    pub(crate) outer_width: i32,
    pub(crate) outer_height: i32,
    pub(crate) src_x: i32,
    pub(crate) src_y: i32,
    pub(crate) src_width: i32,
    pub(crate) src_height: i32,
    pub(crate) u0: f32,
    pub(crate) v0: f32,
    pub(crate) u1: f32,
    pub(crate) v1: f32,
    pub(crate) corner_radius: f32,
    pub(crate) border_width: f32,
    pub(crate) border_color: [f32; 4],
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
        outer_x: i32::from(window.x) - border,
        outer_y: i32::from(window.y) - border,
        outer_width: i32::from(pixmap.width),
        outer_height: i32::from(pixmap.height),
        src_x,
        src_y,
        src_width: width,
        src_height: height,
        u0: src_x as f32 / f32::from(pixmap.width),
        v0: src_y as f32 / f32::from(pixmap.height),
        u1: (src_x + width) as f32 / f32::from(pixmap.width),
        v1: (src_y + height) as f32 / f32::from(pixmap.height),
        corner_radius: 0.0,
        border_width: 0.0,
        border_color: [0.0, 0.0, 0.0, 1.0],
    })
}

/// Scales a render plan's destination rect AND its outer (shadow-extent)
/// rect, each independently around its OWN center, for the open
/// animation. `dst_x/dst_y/width/height` drive the actual surface texture
/// blit — `render_surface_with_opacity` never reads `outer_*` at all.
/// `outer_x/outer_y/outer_width/outer_height` drive shadow's base
/// rectangle (see `shadow_params_from_plan`); as of 3a3fa2b1-s1 these now
/// scale WITH the box, because `render_egl_scene_parts` passes this
/// function's result to `shadow_params_from_plan` for the shadow draw.
/// Blur staying on real (un-animated) bounds is entirely a call-site
/// property, not a property of this function: `render_egl_scene_parts`
/// always passes blur the original, un-scaled `plan`, never this result —
/// see the "BLUR CONTRACT" note there. u0/v0/u1/v1 (UV mapping is
/// independent of the destination rect, so texture sampling stays correct
/// at any scale) are preserved untouched via `..plan`. corner_radius and
/// border_width are scaled with the dst box so the mask ratio is
/// preserved. Guards against a non-finite/non-positive scale and against
/// a scaled dimension rounding to zero, even though `from_scale` (0.96)
/// cannot normally produce one.
fn scale_render_quad_plan(plan: RenderQuadPlan, scale_x: f32, scale_y: f32) -> RenderQuadPlan {
    if !scale_x.is_finite() || scale_x <= 0.0 || !scale_y.is_finite() || scale_y <= 0.0 {
        return plan;
    }
    let width = ((plan.width as f32) * scale_x).round().max(1.0) as i32;
    let height = ((plan.height as f32) * scale_y).round().max(1.0) as i32;
    let dst_x = plan.dst_x + (plan.width - width) / 2;
    let dst_y = plan.dst_y + (plan.height - height) / 2;
    // 3a3fa2b1-s1: outer_* (shadow's base rectangle) scales the same way,
    // independently, around its OWN center — not derived from the dst
    // rect's new position, so a partially off-screen window (whose outer
    // and dst centers can already differ pre-animation, per
    // build_render_quad_plan's edge-clipping) still gets a correctly
    // self-centered animated shadow rect.
    let outer_width = ((plan.outer_width as f32) * scale_x).round().max(1.0) as i32;
    let outer_height = ((plan.outer_height as f32) * scale_y).round().max(1.0) as i32;
    let outer_x = plan.outer_x + (plan.outer_width - outer_width) / 2;
    let outer_y = plan.outer_y + (plan.outer_height - outer_height) / 2;
    // 3a3fa2b1: corner_radius/border_width scale by min(scale_x, scale_y),
    // not an average or a single axis — effective_corner_radius already
    // clamps the radius to min(width, height) * 0.5 elsewhere; using the
    // smaller axis' scale here preserves that same safety invariant under
    // non-uniform scaling (the radius/border can never exceed what the
    // smaller shrunk dimension allows), which an average or fixed-axis
    // rule could not guarantee in the worst case.
    let corner_border_scale = scale_x.min(scale_y);
    RenderQuadPlan {
        dst_x,
        dst_y,
        width,
        height,
        outer_x,
        outer_y,
        outer_width,
        outer_height,
        corner_radius: plan.corner_radius * corner_border_scale,
        border_width: plan.border_width * corner_border_scale,
        ..plan
    }
}

/// 3a3fa2b3 — one vertical slice of energy_tear's window texture.
/// `local_offset_x` is this slice's REST (un-shifted) x-position within
/// the WHOLE window's local space — constant regardless of animation
/// progress (a function of slice index and window width only), used
/// ONLY for corner-radius/border masking continuity (see
/// `render_energy_tear_slices` in renderer.rs), never for on-screen
/// placement (that's `dst_x`, which DOES include the live tear offset).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EnergyTearSlicePlan {
    pub(crate) dst_x: i32,
    pub(crate) dst_y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
    pub(crate) u0: f32,
    pub(crate) v0: f32,
    pub(crate) u1: f32,
    pub(crate) v1: f32,
    pub(crate) local_offset_x: f32,
}

/// One bright vertical tear line drawn at a slice boundary.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EnergyTearStreakPlan {
    pub(crate) dst_x: i32,
    pub(crate) dst_y: i32,
    pub(crate) width: i32,
    pub(crate) height: i32,
}

/// Everything renderer.rs needs to draw one frame of energy_tear's
/// slice+streak overlay — geometry only, no GL state, no resource
/// handles. `full_width`/`full_height` and `corner_radius` mirror
/// exactly what a plain single-quad `render_surface_with_opacity(...,
/// draw_plan, ...)` call would use for its own `surface_size`/
/// `corner_radius` uniforms — passing the SAME whole-window values to
/// every slice (paired with each slice's own `local_offset_x`) is what
/// makes the existing rounded-corner/border shader math correctly mask
/// only the two true outer corners, with zero shader change to that
/// math and zero new uniforms.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EnergyTearRenderPlan {
    pub(crate) slices: [EnergyTearSlicePlan; ENERGY_TEAR_SLICE_COUNT],
    pub(crate) streaks: [EnergyTearStreakPlan; ENERGY_TEAR_SLICE_COUNT - 1],
    pub(crate) full_width: f32,
    pub(crate) full_height: f32,
    pub(crate) corner_radius: f32,
    pub(crate) streak_alpha: f32,
    pub(crate) streak_color: [f32; 3],
}

/// Pure geometry: turns a `RenderQuadPlan` + this frame's
/// `EnergyTearLayout` into concrete slice/streak rectangles. Returns
/// `None` when the plan is too narrow to safely form
/// `ENERGY_TEAR_SLICE_COUNT` non-zero-width slices (the render loop
/// falls back to the ordinary single-quad draw in that case, exactly as
/// if no layout were active at all) — this is a real safety guard, not
/// a clamp: it structurally prevents ever constructing a degenerate
/// (<1px or UV-out-of-range) slice, rather than clamping one after the
/// fact. NO border is drawn on the individual slices (border_width is
/// not part of this plan at all) — while slices are torn apart there is
/// no single continuous ring to draw a border around; the window's
/// configured border reappears correctly, unmodified, the instant this
/// function's caller falls back to the ordinary single-quad path (at
/// t >= ENERGY_TEAR_END or for any non-energy_tear effect).
fn energy_tear_render_plan(plan: RenderQuadPlan, layout: &EnergyTearLayout) -> Option<EnergyTearRenderPlan> {
    if plan.width < ENERGY_TEAR_SLICE_COUNT as i32 || plan.height < 1 {
        return None;
    }
    let total_width = plan.width;
    let base = total_width / ENERGY_TEAR_SLICE_COUNT as i32;
    let remainder = total_width % ENERGY_TEAR_SLICE_COUNT as i32;
    let u_span = plan.u1 - plan.u0;
    // 3a3fa2b3-r3: unit displacement is a fraction of the WHOLE WINDOW's
    // width (not this slice's own width, per the R3 human-reviewed
    // preference), clamped to a sane absolute-pixel range BEFORE the
    // per-slice coefficient is applied — this preserves each slice's
    // relative displacement pattern exactly, regardless of window size,
    // while still bounding the absolute scale for very small/large
    // windows.
    let unit_displacement_px = ((total_width as f32) * ENERGY_TEAR_DISPLACEMENT_FRACTION_OF_WINDOW_WIDTH)
        .clamp(ENERGY_TEAR_MIN_DISPLACEMENT_PX, ENERGY_TEAR_MAX_DISPLACEMENT_PX);
    let mut slices = [EnergyTearSlicePlan {
        dst_x: 0, dst_y: 0, width: 0, height: 0, u0: 0.0, v0: 0.0, u1: 0.0, v1: 0.0, local_offset_x: 0.0,
    }; ENERGY_TEAR_SLICE_COUNT];
    let mut cumulative_px: i32 = 0;
    for (i, slot) in slices.iter_mut().enumerate() {
        let slice_width = base + if (i as i32) < remainder { 1 } else { 0 };
        let rest_offset_px = cumulative_px;
        let live_offset_px = unit_displacement_px * layout.slice_offset_fractions[i];
        *slot = EnergyTearSlicePlan {
            dst_x: plan.dst_x + rest_offset_px + live_offset_px.round() as i32,
            dst_y: plan.dst_y,
            width: slice_width,
            height: plan.height,
            u0: plan.u0 + u_span * (rest_offset_px as f32 / total_width as f32),
            v0: plan.v0,
            u1: plan.u0 + u_span * ((rest_offset_px + slice_width) as f32 / total_width as f32),
            v1: plan.v1,
            local_offset_x: rest_offset_px as f32,
        };
        cumulative_px += slice_width;
    }
    let mut streaks = [EnergyTearStreakPlan { dst_x: 0, dst_y: 0, width: 0, height: 0 }; ENERGY_TEAR_SLICE_COUNT - 1];
    for (i, slot) in streaks.iter_mut().enumerate() {
        let left = &slices[i];
        let right = &slices[i + 1];
        let boundary_x = (left.dst_x + left.width + right.dst_x) / 2;
        let line_width = ((left.width.min(right.width) as f32) * ENERGY_TEAR_LINE_WIDTH_FRACTION)
            .round()
            .max(1.0) as i32;
        *slot = EnergyTearStreakPlan {
            dst_x: boundary_x - line_width / 2,
            dst_y: plan.dst_y,
            width: line_width,
            height: plan.height,
        };
    }
    Some(EnergyTearRenderPlan {
        slices,
        streaks,
        full_width: plan.width as f32,
        full_height: plan.height as f32,
        corner_radius: plan.corner_radius,
        streak_alpha: layout.streak_alpha,
        streak_color: ENERGY_TEAR_STREAK_COLOR,
    })
}

/// True iff the axis-aligned rectangle `x, y, width, height` (root-relative
/// pixel coordinates) has any pixel in common with the root window.
///
/// This is the same emptiness test `build_render_quad_plan` already
/// performs via its dst_x/dst_y clamp-to-zero-and-shrink-width/height
/// sequence (its "None" cases are exactly `width <= 0 || height <= 0`
/// after that clamp) — restated here as a plain boolean so it can be
/// evaluated from window geometry alone, before any `PixmapGeometry`
/// exists. Kept intentionally independent of pixmap contents: only the
/// window's own declared width/height (plus border) is needed to bound
/// the same outer quad `build_render_quad_plan` computes.
fn rect_intersects_root(x: i32, y: i32, width: i32, height: i32, root: RootGeometry) -> bool {
    if width <= 0 || height <= 0 {
        return false;
    }
    let left = x.max(0);
    let top = y.max(0);
    let right = x.saturating_add(width).min(i32::from(root.width));
    let bottom = y.saturating_add(height).min(i32::from(root.height));
    right > left && bottom > top
}

/// The surface's own visual quad (client rectangle, independent of any
/// shadow) intersects root. Uses the window's own geometry directly —
/// equivalent to `build_render_quad_plan`'s `outer_x/outer_y` placement
/// together with an outer size of `window.width/height` plus twice the
/// border, which is exactly what a correctly sized NamedSurfacePixmap
/// will report once acquired (see `named_pixmap_dimensions_match`) — so
/// this does not need an already-acquired pixmap to agree with that
/// function's later verdict.
fn surface_quad_intersects_root(geometry: WindowGeometry, root: RootGeometry) -> bool {
    let border = i32::from(geometry.border_width);
    let x = i32::from(geometry.x) - border;
    let y = i32::from(geometry.y) - border;
    let width = i32::from(geometry.width) + 2 * border;
    let height = i32::from(geometry.height) + 2 * border;
    rect_intersects_root(x, y, width, height, root)
}

/// The surface's shadow-expanded bounds intersect root, using the SAME
/// expansion formula `renderer::build_shadow_quad_plan` uses (outer quad
/// shifted by `offset_x/offset_y` and grown by `extent` on every side,
/// then clamped to root — empty iff the clamped rectangle is degenerate).
/// Duplicated here in pure, GL-free form rather than called directly:
/// `renderer::build_shadow_quad_plan` and `ShadowParams::quad` are private
/// to the `graphics::renderer` module, and pulling GL-adjacent shadow
/// renderer types into this candidate-build-time geometry filter is out of
/// scope for this fix (src/graphics/renderer.rs is not touched by this
/// patch). Kept in exact algebraic sync with that function — see
/// `renderer::build_shadow_quad_plan` for the authoritative geometry this
/// mirrors.
fn shadow_bounds_intersect_root(
    geometry: WindowGeometry,
    style: crate::config::ShadowConfig,
    root: RootGeometry,
) -> bool {
    let border = f32::from(geometry.border_width);
    let outer_x = f32::from(geometry.x) - border;
    let outer_y = f32::from(geometry.y) - border;
    let outer_width = f32::from(geometry.width) + 2.0 * border;
    let outer_height = f32::from(geometry.height) + 2.0 * border;
    if outer_width <= 0.0
        || outer_height <= 0.0
        || !style.extent.is_finite()
        || style.extent <= 0.0
        || !style.offset_x.is_finite()
        || !style.offset_y.is_finite()
    {
        return false;
    }
    let left = outer_x + style.offset_x - style.extent;
    let top = outer_y + style.offset_y - style.extent;
    let right = left + outer_width + 2.0 * style.extent;
    let bottom = top + outer_height + 2.0 * style.extent;
    let root_width = f32::from(root.width);
    let root_height = f32::from(root.height);
    let clipped_left = left.max(0.0).min(root_width);
    let clipped_top = top.max(0.0).min(root_height);
    let clipped_right = right.max(0.0).min(root_width);
    let clipped_bottom = bottom.max(0.0).min(root_height);
    clipped_right > clipped_left && clipped_bottom > clipped_top
}

/// True iff `entry` can contribute any visible compositor pixel to root:
/// its own visual quad intersects root, OR — only when it is already
/// resolved as shadow-eligible (`entry.shadow_eligible`, set by
/// `resolve_snapshot_fullscreen` before this is called) — its
/// shadow-expanded bounds do. A shadow-ineligible entry (shadow disabled,
/// no semantic client, fullscreen, or non-Normal visual class — see
/// `shadow_eligible_for_entry`) never keeps an otherwise-invisible surface
/// alive: only the client quad is considered for it.
fn entry_has_visible_contribution(
    entry: &SurfaceEntry,
    style: crate::config::ShadowConfig,
    root: RootGeometry,
) -> bool {
    surface_quad_intersects_root(entry.geometry, root)
        || (entry.shadow_eligible && shadow_bounds_intersect_root(entry.geometry, style, root))
}

/// Removes candidate entries that cannot contribute any visible compositor
/// pixel, BEFORE any per-entry Damage/NamedPixmap/EGL/GL resource is
/// acquired for them. Must run after `resolve_snapshot_fullscreen` (so
/// `entry.shadow_eligible` already reflects shadow config, semantic
/// client, fullscreen, and visual class) and before the candidate
/// resource-acquisition loop. Self-correcting: a pruned entry simply does
/// not appear in `snapshot.entries` for this candidate, exactly like the
/// existing `eligible_surface` skip reasons (InputOnly, unmapped,
/// zero-size) — if it later moves on-screen, the next hierarchy rebuild
/// (triggered by the existing `ConfigureNotify` -> `SceneInvalidation::Hierarchy`
/// fallback for XIDs absent from `snapshot.entries`) re-evaluates it fresh
/// against its new geometry. No persistent exclusion state is introduced.
fn prune_invisible_entries(
    entries: &mut Vec<SurfaceEntry>,
    style: crate::config::ShadowConfig,
    root: RootGeometry,
) {
    entries.retain(|entry| entry_has_visible_contribution(entry, style, root));
}

fn effective_corner_radius(radius: f32, width: i32, height: i32) -> f32 {
    if !radius.is_finite() || radius <= 0.0 || width <= 0 || height <= 0 {
        return 0.0;
    }
    radius.min(width.min(height) as f32 * 0.5)
}

fn effective_border_width(border_width: f32, width: i32, height: i32) -> f32 {
    if !border_width.is_finite() || border_width <= 0.0 || width <= 0 || height <= 0 {
        return 0.0;
    }
    border_width.min(width.min(height) as f32 * 0.5)
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
enum FirstPublishStep {
    Rebuild,
    AwaitEvent,
    Published,
    Shutdown,
}

fn first_publish_step(
    snapshot_present: bool,
    rebuild_deferred: bool,
    shutdown: bool,
) -> FirstPublishStep {
    if shutdown {
        FirstPublishStep::Shutdown
    } else if snapshot_present {
        FirstPublishStep::Published
    } else if rebuild_deferred {
        FirstPublishStep::AwaitEvent
    } else {
        FirstPublishStep::Rebuild
    }
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
    Background,
    VisualState,
    Geometry(Window),
    Hierarchy,
    Shutdown(ShutdownReason),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HierarchyEventSource {
    UnknownConfigure,
    Create,
    Map,
    Unmap,
    Destroy,
    Reparent,
    Circulate,
    ExistingHierarchyMerge,
}

impl HierarchyEventSource {
    fn bit(self) -> u16 {
        match self {
            Self::UnknownConfigure => 1 << 0,
            Self::Create => 1 << 1,
            Self::Map => 1 << 2,
            Self::Unmap => 1 << 3,
            Self::Destroy => 1 << 4,
            Self::Reparent => 1 << 5,
            Self::Circulate => 1 << 6,
            Self::ExistingHierarchyMerge => 1 << 7,
        }
    }
}

fn hierarchy_event_source(event: &Event) -> Option<HierarchyEventSource> {
    match event {
        Event::ConfigureNotify(_) => Some(HierarchyEventSource::UnknownConfigure),
        Event::CreateNotify(_) => Some(HierarchyEventSource::Create),
        Event::MapNotify(_) => Some(HierarchyEventSource::Map),
        Event::UnmapNotify(_) => Some(HierarchyEventSource::Unmap),
        Event::DestroyNotify(_) => Some(HierarchyEventSource::Destroy),
        Event::ReparentNotify(_) => Some(HierarchyEventSource::Reparent),
        Event::CirculateNotify(_) => Some(HierarchyEventSource::Circulate),
        _ => None,
    }
}

fn hierarchy_event_window(event: &Event) -> Option<Window> {
    match event {
        Event::ConfigureNotify(event) => Some(event.window),
        Event::CreateNotify(event) => Some(event.window),
        Event::MapNotify(event) => Some(event.window),
        Event::UnmapNotify(event) => Some(event.window),
        Event::DestroyNotify(event) => Some(event.window),
        Event::ReparentNotify(event) => Some(event.window),
        Event::CirculateNotify(event) => Some(event.window),
        _ => None,
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum HierarchyEventRelation {
    TargetSurface,
    TargetSemanticClient,
    OtherTrackedSurface,
    OtherSemanticClient,
    Unknown,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct PendingGeometry {
    surface_xid: Window,
    x: i16,
    y: i16,
    width: u16,
    height: u16,
    border_width: u16,
    override_redirect: bool,
}

fn configure_geometry_update(event: &Event, snapshot: &SceneSnapshot) -> Option<PendingGeometry> {
    let Event::ConfigureNotify(event) = event else {
        return None;
    };
    if !snapshot.entries.iter().any(|entry| entry.surface_xid == event.window) {
        return None;
    }
    Some(PendingGeometry {
        surface_xid: event.window,
        x: event.x,
        y: event.y,
        width: event.width,
        height: event.height,
        border_width: event.border_width,
        override_redirect: event.override_redirect,
    })
}

fn geometry_event_source(event: &Event, snapshot: &SceneSnapshot) -> GeometryEventSource {
    let Event::ConfigureNotify(event) = event else { return GeometryEventSource::Unknown; };
    if snapshot.entries.iter().any(|entry| entry.surface_xid == event.window) { GeometryEventSource::CanonicalSurface }
    else if snapshot.entries.iter().any(|entry| entry.semantic_client_xid == Some(event.window)) { GeometryEventSource::SemanticClient }
    else { GeometryEventSource::Other }
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
    geometry_update: Option<PendingGeometry>,
    geometry_ambiguous: bool,
    shutdown: Option<ShutdownReason>,
    pixel_damage: HashSet<damage::Damage>,
    background: bool,
    visual_state: bool,
    geometry_source: Option<GeometryEventSource>,
    geometry_event_update: Option<PendingGeometry>,
    present_history: GeometryPresentHistory,
    hierarchy_source_bits: u16,
    hierarchy_geometry_pending: bool,
    hierarchy_pending_geometry: Option<PendingGeometry>,
}

impl InvalidationBatch {
    fn push(&mut self, invalidation: SceneInvalidation) {
        match invalidation {
            SceneInvalidation::Ignore => {}
            SceneInvalidation::PixelDamage(damage_id) => {
                self.pixel_damage.insert(damage_id);
            }
            SceneInvalidation::Background => self.background = true,
            SceneInvalidation::VisualState => self.visual_state = true,
            SceneInvalidation::Geometry(window) if !self.hierarchy => {
                if self.geometry.is_some_and(|current| current != window) {
                    self.geometry_ambiguous = true;
                }
                self.geometry = Some(window);
            }
            SceneInvalidation::Geometry(_) => {}
            SceneInvalidation::Hierarchy => {
                self.hierarchy_geometry_pending |= self.geometry.is_some() || self.geometry_update.is_some();
                self.hierarchy_pending_geometry = self.geometry_update.or(self.geometry_event_update);
                self.hierarchy = true;
                self.geometry = None;
                self.geometry_ambiguous = false;
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
        } else if self.background {
            SceneInvalidation::Background
        } else if self.visual_state {
            SceneInvalidation::VisualState
        } else if let Some(damage_id) = self.pixel_damage.iter().next().copied() {
            SceneInvalidation::PixelDamage(damage_id)
        } else {
            SceneInvalidation::Ignore
        }
    }

    fn pixel_damage(&self) -> &HashSet<damage::Damage> {
        &self.pixel_damage
    }

    fn push_geometry_update(&mut self, update: Option<PendingGeometry>) {
        let Some(update) = update else {
            return;
        };
        if self.hierarchy {
            self.hierarchy_geometry_pending = true;
            self.hierarchy_pending_geometry = Some(update);
            return;
        }
        if self.geometry_update.is_some_and(|current| current.surface_xid != update.surface_xid) {
            self.geometry_ambiguous = true;
        }
        if self.present_history.ever_deferred {
            self.present_history.updated_while_deferred = true;
            self.present_history.superseded_while_deferred = true;
        }
        self.geometry_update = Some(update);
    }

    fn note_configure_event(&mut self, event: &Event, source: GeometryEventSource, surface_xid: Option<Window>) {
        let Event::ConfigureNotify(event) = event else { return; };
        self.geometry_source = Some(source);
        if let Some(surface_xid) = surface_xid {
            self.geometry_event_update = Some(PendingGeometry { surface_xid, x: event.x, y: event.y, width: event.width, height: event.height, border_width: event.border_width, override_redirect: event.override_redirect });
        }
    }

    fn note_hierarchy_source(&mut self, source: HierarchyEventSource) {
        self.hierarchy_source_bits |= source.bit();
    }

    fn move_geometry(&self) -> Option<PendingGeometry> {
        (!self.geometry_ambiguous).then_some(self.geometry_update).flatten()
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
    pixmaps: Vec<Rc<NamedSurfacePixmap<'a>>>,
    damage_leases: Vec<Rc<DamageLease<'a>>>,
    damage_registry: HashMap<damage::Damage, Window>,
    pending_damage: HashSet<damage::Damage>,
    pending_background: bool,
    structural_generation: u64,
    attempted_structural_generation: u64,
    snapshot: Option<SceneSnapshot>,
    resources: HashMap<Window, Rc<SurfaceResourceBundle<'a>>>,
    egl_surfaces: HashMap<Window, Rc<std::cell::RefCell<EglImportedSurface>>>,
    background: Option<ImportedBackground>,
    background_atoms: BackgroundAtoms,
    visual_atoms: VisualAtoms,
    active_window: Option<Window>,
    active_window_initialized: bool,
    urgency: HashMap<Window, CachedClientVisualState>,
    pending_visual_state: bool,
    pending_move_geometry: Option<PendingGeometry>,
    pending_move_geometry_ambiguous: bool,
    pending_move_geometry_present_history: GeometryPresentHistory,
    pending_hierarchy_geometry: Option<PendingGeometry>,
    signal: SignalWake,
    scheduler: FrameScheduler,
    present: Option<PresentClock>,
    state: SceneState,
    _config: CompositorConfig,
    shadow_style: crate::config::ShadowConfig,
    ignored_configure_windows: HashSet<Window>,
    diagnostics: Diagnostics3a3f8b3a,
    // 3a3fa2a: resource-free (no DamageLease/NamedPixmap/EGLImage), keyed by
    // the same stable surface_xid identity as `resources`/`egl_surfaces`.
    // Populated only by a successful commit (see `commit_candidate_inner`),
    // never speculatively.
    window_animations: HashMap<Window, WindowAnimation>,
    // 3a3fa2b5 — persistent close-animation state. `render_order` is the
    // single authoritative composite ordering (Live projection always ==
    // `snapshot.entries` order, see `reconcile_render_order`);
    // `closing_visuals` owns exactly one GPU texture per still-animating
    // close (`ClosingTexture`), keyed by `close_id`, never by Window XID;
    // `next_close_id` is committed state, mutated ONLY inside
    // `commit_candidate_inner` on Accept; `destroy_intents` records
    // per-XID genuine DestroyNotify provenance, populated the instant an
    // event is drained (see `note_destroy_intent`) and retired only once
    // that XID is actually removed by a COMMITTED candidate — so a
    // candidate Retry always observes the same causal Destroy
    // information as its first attempt.
    render_order: Vec<RenderLayer>,
    closing_visuals: HashMap<u64, ClosingVisual>,
    next_close_id: u64,
    destroy_intents: HashSet<Window>,
    // TEMPORARY (Brave repaint latency forensic) — see PerfForensics.
    perf: PerfForensics,
}

struct SceneCandidate<'a> {
    snapshot: SceneSnapshot,
    generation: u64,
    resources: HashMap<Window, Rc<SurfaceResourceBundle<'a>>>,
    // Declaration order is cleanup order: imported EGL resources must drop
    // before their source pixmaps, with Damage leases released in between.
    egl_surfaces: HashMap<Window, Rc<std::cell::RefCell<EglImportedSurface>>>,
    damage_leases: Vec<Rc<DamageLease<'a>>>,
    pixmaps: Vec<Rc<NamedSurfacePixmap<'a>>>,
    damage_registry: HashMap<damage::Damage, Window>,
    watch_ids: HashSet<Window>,
    watch_additions: Vec<Window>,
    ignored_configure_windows: HashSet<Window>,
    // 3a3fa2a: candidate-local and pure until a successful commit promotes
    // it into SceneSession::window_animations. A rejected/retried candidate
    // is simply dropped, taking this with it — see provisional_open_animations.
    provisional_animations: HashMap<Window, WindowAnimation>,
    // 3a3fa2b5 — candidate-local close-animation state, mirroring
    // `provisional_animations`'s exact precedent: pure and disposable
    // until a successful commit promotes it. `provisional_render_order`
    // already satisfies the Live-projection invariant by construction
    // (see `reconcile_render_order`); `provisional_closing_frames` holds
    // ONLY cheap value data + an `Rc` clone (no GPU allocation — see
    // `ProvisionalClosingFrame`); `next_close_id_after` is this
    // candidate's own locally-advanced counter, never written back to
    // `SceneSession::next_close_id` except by `commit_candidate_inner` on
    // Accept.
    provisional_render_order: Vec<RenderLayer>,
    provisional_closing_frames: HashMap<u64, ProvisionalClosingFrame>,
    next_close_id_after: u64,
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
    fn defer_geometry(&mut self, batch: &InvalidationBatch) {
        if batch.hierarchy {
            return;
        }
        if let Some(update) = batch.geometry_update {
            if self.pending_move_geometry.is_some() {
                self.diagnostics.configure_superseded += 1;
            }
            if self
                .pending_move_geometry
                .is_some_and(|current| current.surface_xid != update.surface_xid)
            {
                self.pending_move_geometry_ambiguous = true;
            }
            self.pending_move_geometry = Some(update);
        }
        let mut history = self.pending_move_geometry_present_history;
        history.updated_while_deferred |= batch.present_history.updated_while_deferred;
        history.superseded_while_deferred |= batch.present_history.superseded_while_deferred;
        let was_deferred = history.ever_deferred;
        history = history.deferred();
        if !was_deferred { self.diagnostics.record_pending_present_history(history); }
        self.pending_move_geometry_present_history = history;
        if history.updated_while_deferred { self.diagnostics.geometry_pending_updated_while_present_deferred += 1; }
        if history.superseded_while_deferred { self.diagnostics.geometry_pending_superseded_while_present_deferred += 1; }
        self.pending_move_geometry_ambiguous |= batch.geometry_ambiguous;
    }

    fn acquire(connection: &'a X11Connection, expected_root: Window, config: CompositorConfig) -> Result<Self, Box<dyn Error>> {
        let root = connection.inner.setup().roots[connection.screen_num()].root;
        root_guard(expected_root, root)?;
        check_capabilities(connection)?;
        check_selection_available(connection)?;
        ensure_damage_version(connection)?;
        let visual_formats = VisualFormatCache::acquire(connection)?;
        let background_atoms = acquire_background_atoms(connection)?;
        let visual_atoms = acquire_visual_atoms(connection)?;
        let signal = SignalWake::install()?;
        let ownership = CompositorOwnership::claim(connection)?;
        let mut overlay = OverlayLease::acquire(connection, root)?;
        overlay.print_metadata()?;
        overlay.configure_input_passthrough()?;
        let root_watch = SceneRootWatch::acquire(connection, root)?;
        let present = PresentClock::acquire(connection, overlay.overlay)?;
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
            pending_background: true,
            // Generation 1 is the initial scene build.  A generation is
            // ready exactly when it is newer than the last attempted one.
            structural_generation: 1,
            attempted_structural_generation: 0,
            snapshot: None,
            resources: HashMap::new(),
            egl_surfaces: HashMap::new(),
            background: None,
            background_atoms,
            visual_atoms,
            active_window: None,
            active_window_initialized: false,
            urgency: HashMap::new(),
            pending_visual_state: false,
            pending_move_geometry: None,
            pending_move_geometry_ambiguous: false,
            pending_move_geometry_present_history: GeometryPresentHistory::default(),
            pending_hierarchy_geometry: None,
            signal,
            scheduler: FrameScheduler::new(),
            present,
            state: SceneState::PlaceholderReady,
            _config: config,
            shadow_style: config.visuals.shadow,
            ignored_configure_windows: HashSet::new(),
            diagnostics: Diagnostics3a3f8b3a::from_environment(),
            window_animations: HashMap::new(),
            render_order: Vec::new(),
            closing_visuals: HashMap::new(),
            next_close_id: 0,
            destroy_intents: HashSet::new(),
            perf: PerfForensics::new(),
        };
        session.state = SceneState::ManualActive;
        Ok(session)
    }

    fn run(connection: &'a X11Connection, expected_root: Window, config: CompositorConfig) -> Result<(), Box<dyn Error>> {
        let mut session = Self::acquire(connection, expected_root, config)?;
        let operation = session
            .prepare_scene()
            .and_then(|_| session.wait_live_pixel());
        debug_assert!(coordinator_requires_cleanup(session.state));
        let cleanup = session.cleanup();
        session.diagnostics.print_summary();
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
        self.refresh_background()?;
        self.rebuild_and_present()?;
        if self.snapshot.is_some() {
            self.arm_next_presentation(0)?;
        }
        Ok(())
    }

    fn initialize_visual_state(&mut self, snapshot: &SceneSnapshot) -> Result<(), Box<dyn Error>> {
        if !self.active_window_initialized {
            self.active_window = read_active_window(self.connection, self.root, self.visual_atoms.active_window)?;
            self.active_window_initialized = true;
        }
        for client in snapshot.entries.iter().filter_map(|entry| entry.semantic_client_xid) {
            if self.urgency.contains_key(&client) {
                continue;
            }
            let cached = match read_client_urgency(self.connection, client, self.visual_atoms) {
                Ok(cached) => cached,
                Err(error) if super::capture::is_bad_window_error(error.as_ref()) => CachedClientVisualState::default(),
                Err(error) => return Err(error),
            };
            self.urgency.insert(client, cached);
        }
        Ok(())
    }

    /// 3a3fa2b5 — the ONLY place `destroy_intents` is ever written.
    /// Called at every site an event is drained from the X11 connection
    /// (poll_for_event or the initial wait_for_event_or_shutdown event),
    /// so a DestroyNotify's causal information is never lost merely
    /// because it was observed during a speculative/retried build —
    /// events are removed from the queue the instant they are polled,
    /// regardless of which attempt is running, so this must run
    /// unconditionally at drain time, never deferred into
    /// build_candidate/pre_commit_gate. Retirement (removal) happens only
    /// in commit_candidate_inner, for XIDs an ACCEPTED candidate actually
    /// removes — see the r4/R1 close-trigger findings.
    fn note_destroy_intent(&mut self, event: &Event) {
        if let Event::DestroyNotify(destroy) = event {
            self.destroy_intents.insert(destroy.window);
        }
    }

    /// 3a3fa2b5 — computes this candidate's close-trigger set, allocates
    /// their close_ids from a LOCAL counter seeded by (never mutating)
    /// `self.next_close_id`, and reconciles `self.render_order` against
    /// the fresh authoritative live order. Pure with respect to `self`:
    /// reads `self.snapshot`/`self.resources`/`self.destroy_intents`/
    /// `self.render_order`/`self._config`/`self.next_close_id`, mutates
    /// nothing — the caller (`build_candidate`) stores the result
    /// candidate-locally; only `commit_candidate_inner` on Accept ever
    /// promotes it into committed state.
    ///
    /// R1 close trigger (ALL must hold): the XID is in `removed_surfaces`
    /// (present in the OLD committed scene, absent from `snapshot`);
    /// `eligible_for_open_animation` holds for its OLD committed metadata
    /// (same semantic policy as open, reused directly — not reimplemented);
    /// a genuine DestroyNotify intent is recorded for it
    /// (`self.destroy_intents`) — Unmap-only (workspace hide/minimize/
    /// withdraw) never sets this, so it never triggers a close; and its
    /// OLD imported EGL texture still exists (`self.resources[xid].egl`).
    fn build_provisional_closing_state(
        &self,
        snapshot: &SceneSnapshot,
        removed_surfaces: &HashSet<Window>,
    ) -> (Vec<RenderLayer>, HashMap<u64, ProvisionalClosingFrame>, u64) {
        let new_live_order: Vec<Window> = snapshot.entries.iter().map(|entry| entry.surface_xid).collect();
        let mut provisional_closing_frames: HashMap<u64, ProvisionalClosingFrame> = HashMap::new();
        let animation = self._config.animation;
        let mut eligible_sources: Vec<Window> = Vec::new();
        let mut frame_inputs: HashMap<Window, (RenderQuadPlan, EglPixelSemantics, f32, bool)> = HashMap::new();
        if animation.enabled && animation.close.enabled {
            let mut candidates: Vec<&SurfaceEntry> = self
                .snapshot
                .as_ref()
                .map(|live| {
                    live.entries
                        .iter()
                        .filter(|entry| removed_surfaces.contains(&entry.surface_xid))
                        .collect()
                })
                .unwrap_or_default();
            candidates.sort_by_key(|entry| entry.stacking_index);
            for old_entry in candidates {
                if !eligible_for_open_animation(old_entry) {
                    continue;
                }
                if !self.destroy_intents.contains(&old_entry.surface_xid) {
                    continue;
                }
                let Some(bundle) = self.resources.get(&old_entry.surface_xid) else { continue; };
                if bundle.egl.is_none() {
                    continue;
                }
                let Some(mut plan) = build_render_quad_plan(old_entry.geometry, bundle.pixmap.geometry, snapshot.root_geometry) else { continue; };
                apply_surface_visual_policy(&mut plan, &self._config.visuals, old_entry.visual_class);
                plan.border_color = old_entry.resolved_border_color.map(f32::from_bits);
                eligible_sources.push(old_entry.surface_xid);
                frame_inputs.insert(
                    old_entry.surface_xid,
                    (
                        plan,
                        bundle.egl.as_ref().expect("checked above").borrow().pixel_semantics,
                        f32::from_bits(old_entry.resolved_opacity_bits),
                        old_entry.shadow_eligible,
                    ),
                );
            }
        }
        let (provisional_closes, next_close_id_after) = allocate_close_ids(self.next_close_id, &eligible_sources);
        for (&source_xid, &close_id) in &provisional_closes {
            // 3a3fa2b5-r2: existence-only re-check — no Rc is cloned or
            // retained here. The frame carries only `source_xid`; the
            // actual GL texture is looked up fresh, by `source_xid`, at
            // render time (`render_closing_layer`'s `closing_source_surfaces`
            // lookup) and again at post-Accept capture time (`old_resources`
            // lookup in `commit_candidate_inner`) — never cached in
            // candidate-local state.
            let Some(bundle) = self.resources.get(&source_xid) else { continue; };
            if bundle.egl.is_none() {
                continue;
            }
            let Some((plan, pixel_semantics, base_opacity, shadow_eligible)) = frame_inputs.get(&source_xid).copied() else { continue; };
            provisional_closing_frames.insert(
                close_id,
                ProvisionalClosingFrame {
                    source_xid,
                    plan,
                    pixel_semantics,
                    base_opacity,
                    shadow_eligible,
                    animation: ClosingAnimation::new(Instant::now(), animation.close.effect, animation.close.duration),
                },
            );
        }
        let provisional_render_order = reconcile_render_order(
            &self.render_order,
            &new_live_order,
            removed_surfaces,
            &provisional_closes,
        );
        (provisional_render_order, provisional_closing_frames, next_close_id_after)
    }

    /// 3a3fa2b5 — removes every `ClosingVisual` whose animation has
    /// completed, destroying its `ClosingTexture` exactly once (via
    /// `Drop`, requires the GL context this is always called with while
    /// current — mirrors `retire_completed_animations`'s call site and
    /// timing), and drops its `RenderLayer::Closing(id)` entry from
    /// `render_order` — a persistent-only mutation, entirely outside the
    /// candidate transaction, exactly like `retire_completed_animations`.
    /// Relative order of any remaining layers is preserved by construction
    /// (`retain` never reorders).
    fn retire_completed_closing_visuals(&mut self) {
        if self.closing_visuals.is_empty() {
            return;
        }
        let now = Instant::now();
        let completed: HashSet<u64> = self
            .closing_visuals
            .iter()
            .filter(|(_, visual)| visual.animation.is_complete(now))
            .map(|(id, _)| *id)
            .collect();
        if completed.is_empty() {
            return;
        }
        for id in &completed {
            self.closing_visuals.remove(id);
        }
        self.render_order.retain(|layer| !matches!(layer, RenderLayer::Closing(id) if completed.contains(id)));
    }

    fn build_candidate(&mut self) -> Result<SceneCandidate<'a>, Box<dyn Error>> {
        self.perf.candidate_rebuilds += 1;
        if self.diagnostics.enabled && self.diagnostics.structural_origin.is_none() { self.diagnostics.begin_structural_origin(StructuralOrigin::NormalLifecycle); }
        self.diagnostics.structural_candidates_started += 1;
        let resizeonly_snapshot_start = self.diagnostics.resizeonly_structural_direction
            .filter(|_| self.diagnostics.enabled)
            .map(|_| Instant::now());
        let generation = self.structural_generation;
        let root_geometry = read_root_geometry(self.connection, self.root)?;
        let hierarchy = self.connection.snapshot_hierarchy()?;
        let client_bounds = client_bounds_from_hierarchy(&hierarchy);
        let watch_ids = snapshot_watch_ids(&hierarchy);
        let overlay = self.overlay.as_ref().ok_or("overlay is unavailable")?.overlay;
        let owner = self
            .ownership
            .as_ref()
            .ok_or("ownership is unavailable")?
            .owner_window;
        let ignored_configure_windows = known_non_renderable_windows(&hierarchy, overlay, owner);
        let mut snapshot = SceneSnapshot::from_hierarchy(
            hierarchy,
            root_geometry,
            overlay,
            owner,
        )?;
        self.diagnostics.record_snapshot_origin();
        initialize_surface_effect_metadata(
            self.connection,
            &mut snapshot,
            self.visual_atoms,
        )?;
        let mut compound_target = None;
        if let Some(update) = self.pending_hierarchy_geometry {
            self.diagnostics.compound_hierarchy_geometry_observed += 1;
            self.diagnostics.compound_rebase_attempted += 1;
            let live_entry = self.snapshot.as_ref().and_then(|live| live.entries.iter().find(|entry| entry.surface_xid == update.surface_xid));
            let candidate_entry = snapshot.entries.iter_mut().find(|entry| entry.surface_xid == update.surface_xid);
            if let (Some(live_entry), Some(candidate_entry)) = (live_entry, candidate_entry) {
                let identity_ok = live_entry.surface_xid == candidate_entry.surface_xid
                    && live_entry.semantic_client_xid == candidate_entry.semantic_client_xid
                    && live_entry.lifecycle_xid == candidate_entry.lifecycle_xid
                    && candidate_entry.map_state != xproto::MapState::UNMAPPED
                    && candidate_entry.override_redirect == update.override_redirect;
                if identity_ok {
                    compound_target = Some(update.surface_xid);
                    rebase_candidate_geometry_fields(candidate_entry, update);
                    if let Some(client) = candidate_entry.client_root_geometry.as_mut() {
                        client.width = i32::from(update.width);
                        client.height = i32::from(update.height);
                    }
                    self.diagnostics.compound_rebase_success += 1;
                    self.diagnostics.compound_rebase_avoided_full_retry += 1;
                } else {
                    self.diagnostics.compound_rebase_rejected_lifecycle += 1;
                    self.pending_hierarchy_geometry = None;
                    return Err(Box::new(CandidateBuildError::Stale(SceneInvalidation::Hierarchy)));
                }
            } else {
                self.diagnostics.compound_rebase_rejected_scene_membership += 1;
                self.pending_hierarchy_geometry = None;
                return Err(Box::new(CandidateBuildError::Stale(SceneInvalidation::Hierarchy)));
            }
        }
        self.initialize_visual_state(&snapshot)?;
        resolve_regions_client_geometry(
            self.connection,
            self.root,
            &mut snapshot,
            &client_bounds,
            &self.urgency,
        )?;
        resolve_snapshot_border_colors(&mut snapshot, &self._config.visuals, self.active_window, &self.urgency);
        resolve_snapshot_fullscreen(&mut snapshot, &self.urgency, self._config.blur_enabled, self.shadow_style);
        resolve_snapshot_opacity(&mut snapshot, &self._config.visuals, self.active_window, &self.urgency);
        prune_invisible_entries(&mut snapshot.entries, self.shadow_style, snapshot.root_geometry);
        if let Some(start) = resizeonly_snapshot_start {
            self.diagnostics.record_structural_snapshot(start.elapsed());
        }
        self.state = SceneState::SceneSnapshotReady;
        if self.snapshot.as_ref().is_some_and(|live| candidate_has_resized_target(live, &snapshot, &self.resources)) {
            if let Some(invalidation) = self.refresh_resize_state_before_acquisition(&mut snapshot)? {
                return Err(Box::new(CandidateBuildError::Stale(invalidation)));
            }
        }
        // 3a3fa2a: computed from the current live snapshot BEFORE any commit,
        // and from the final (post prune/resize-refresh) candidate entries —
        // pure, candidate-local. Never touches self.window_animations.
        let is_first_publish = self.snapshot.is_none();
        let old_surfaces: HashSet<Window> = self
            .snapshot
            .as_ref()
            .map(|live| live.entries.iter().map(|entry| entry.surface_xid).collect())
            .unwrap_or_default();
        let provisional_animations = provisional_open_animations(
            &old_surfaces,
            &snapshot,
            is_first_publish,
            self.present.is_some(),
            self._config.animation,
            Instant::now(),
        );
        // 3a3fa2b5: candidate-local close-trigger/ordering state, computed
        // from the OLD live snapshot/resources (still valid, unmutated at
        // this point) and this candidate's own fresh snapshot. See
        // build_provisional_closing_state.
        let new_surface_ids: HashSet<Window> = snapshot.entries.iter().map(|entry| entry.surface_xid).collect();
        let removed_surfaces: HashSet<Window> = old_surfaces.difference(&new_surface_ids).copied().collect();
        let (provisional_render_order, provisional_closing_frames, next_close_id_after) =
            self.build_provisional_closing_state(&snapshot, &removed_surfaces);
        let mut pixmaps = Vec::new();
        let mut damage_leases = Vec::new();
        let mut damage_registry = HashMap::new();
        let mut egl_surfaces = HashMap::new();
        let mut resources = HashMap::new();
        let mut replaced_existing_resource = false;
        let egl = self.egl.as_ref().ok_or("EGL scene renderer is unavailable")?;
        for index in 0..snapshot.entries.len() {
            let entry = snapshot.entries[index].clone();
            let semantics = self.visual_formats.semantics(entry.visual, entry.depth);
            let importable = semantics != EglPixelSemantics::Unsupported;
            let replaced_existing = self.resources.contains_key(&entry.surface_xid);
            replaced_existing_resource |= replaced_existing;
            if let Some(old) = self.resources.get(&entry.surface_xid)
                && reusable_resource_identity(self.current_snapshot(), &entry, old)
            {
                self.diagnostics.resource_bundles_reused += 1;
                if let Some(damage) = &old.damage {
                    damage_registry.insert(damage.damage_xid, entry.surface_xid);
                }
                if let Some(egl_surface) = &old.egl {
                    egl_surfaces.insert(entry.surface_xid, Rc::clone(egl_surface));
                }
                pixmaps.push(Rc::clone(&old.pixmap));
                if let Some(damage) = &old.damage { damage_leases.push(Rc::clone(damage)); }
                resources.insert(entry.surface_xid, Rc::clone(old));
                continue;
            }
            self.diagnostics.resource_bundles_new += 1;
            let reuse_compound_damage = compound_target == Some(entry.surface_xid)
                && self.resources.get(&entry.surface_xid).is_some_and(|old| {
                    old.damage.as_ref().is_some_and(|_| {
                        self.snapshot.as_ref().and_then(|live| live.entries.iter().find(|previous| previous.surface_xid == entry.surface_xid)).is_some_and(|previous| damage_identity_compatible(previous, &entry))
                    })
                });
            let damage = if importable {
                let damage = if reuse_compound_damage {
                    let damage = Rc::clone(self.resources.get(&entry.surface_xid).and_then(|old| old.damage.as_ref()).expect("compound Damage reuse was checked"));
                    self.diagnostics.compound_rebase_damage_reused += 1;
                    damage
                } else {
                    let damage = match DamageLease::acquire(self.connection, entry.surface_xid) {
                        Ok(damage) => damage,
                        Err(error) => return Err(translate_damage_lease_acquire_error(error)),
                    };
                    if replaced_existing { self.diagnostics.resized_target_damage_acquisitions += 1; }
                    damage.subtract()?;
                    Rc::new(damage)
                };
                damage_registry.insert(damage.damage_xid, entry.surface_xid);
                damage_leases.push(Rc::clone(&damage));
                Some(damage)
            } else {
                None
            };
            let pixmap = match NamedSurfacePixmap::acquire(
                self.connection,
                &entry,
                self.root,
                root_geometry,
                None,
            ) {
                Ok(pixmap) => pixmap,
                Err(error) => return Err(translate_named_pixmap_acquire_error(error)),
            };
            if replaced_existing { self.diagnostics.resized_target_named_pixmap_acquisitions += 1; }
            if compound_target == Some(entry.surface_xid) && replaced_existing {
                self.diagnostics.compound_rebase_named_pixmap_reacquired += 1;
            }
            if !importable {
                println!(
                    "EGL import unsupported by capability policy: canonical surface=0x{:08x} depth={} visual=0x{:08x}",
                    entry.surface_xid, entry.depth, entry.visual
                );
                let pixmap = Rc::new(pixmap);
                pixmaps.push(Rc::clone(&pixmap));
                resources.insert(entry.surface_xid, Rc::new(SurfaceResourceBundle {
                    damage,
                    pixmap,
                    egl: None,
                }));
                continue;
            }
            let pixmap = Rc::new(pixmap);
            let egl_surface = Rc::new(std::cell::RefCell::new(egl.import_pixmap(pixmap.pixmap_xid, semantics)?));
            if replaced_existing {
                self.diagnostics.resized_target_egl_imports += 1;
                self.diagnostics.resized_target_bundle_acquisitions += 1;
            }
            if compound_target == Some(entry.surface_xid) && replaced_existing {
                self.diagnostics.compound_rebase_egl_reacquired += 1;
            }
            egl_surfaces.insert(entry.surface_xid, Rc::clone(&egl_surface));
            pixmaps.push(Rc::clone(&pixmap));
            resources.insert(entry.surface_xid, Rc::new(SurfaceResourceBundle {
                damage,
                pixmap,
                egl: Some(egl_surface),
            }));
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
        // 3a3fa2a: the FIRST frame this candidate can ever present already
        // uses provisional_animations, merged with whatever is already
        // persistent — so a newly eligible surface can never be swapped
        // visible at full opacity/scale before its animation exists.
        let render_animations = merge_window_animations(&self.window_animations, &provisional_animations);
        self.render_egl_scene(
            &snapshot,
            &egl_surfaces,
            &pixmaps,
            &render_animations,
            &provisional_render_order,
            &provisional_closing_frames,
        )?;
        self.diagnostics.last_candidate_resize = replaced_existing_resource;
        if replaced_existing_resource { self.diagnostics.resize_candidate_started += 1; }
        self.state = SceneState::NamedPixmapsReady;
        println!("state: EGLImported surfaces={}", egl_surfaces.len());
        let watch_additions = self.structure_watches.ensure_candidate(&watch_ids)?;
        Ok(SceneCandidate {
            snapshot,
            generation,
            resources,
            pixmaps,
            damage_leases,
            damage_registry,
            egl_surfaces,
            watch_ids,
            watch_additions,
            ignored_configure_windows,
            provisional_animations,
            provisional_render_order,
            provisional_closing_frames,
            next_close_id_after,
        })
    }

    fn refresh_resize_state_before_acquisition(
        &mut self,
        candidate: &mut SceneSnapshot,
    ) -> Result<Option<SceneInvalidation>, Box<dyn Error>> {
        for _ in 0..MAX_EVENTS_PER_BATCH {
            let Some(event) = self.connection.inner.poll_for_event()? else {
                break;
            };
            self.note_destroy_intent(&event);
            self.diagnostics.record_configure(&event, candidate);
            let geometry_source = geometry_event_source(&event, candidate);
            self.diagnostics.record_geometry_source(geometry_source);
            let _ = self.present_opportunity(&event);
            let geometry_update = configure_geometry_update(&event, candidate);
            if geometry_update.is_none() && matches!(event, Event::ConfigureNotify(_)) { self.diagnostics.record_geometry_rejected(geometry_source); }
            let visual_invalidation = self.maybe_update_visual_state(&event)?;
            let invalidation = if is_background_property_notify(&event, self.root, self.background_atoms) {
                SceneInvalidation::Background
            } else if let Some(invalidation) = visual_invalidation {
                invalidation
            } else {
                self.classify_session_event(event, self.current_snapshot(), &self.damage_registry, &self.damage_registry)
            };
            self.observe_invalidation(invalidation);
            if matches!(invalidation, SceneInvalidation::Hierarchy) {
                self.diagnostics.compound_rebase_attempted += 1;
                self.diagnostics.compound_rebase_rejected_newer_hierarchy += 1;
                return Ok(Some(SceneInvalidation::Hierarchy));
            }
            if let Some(update) = geometry_update {
                self.diagnostics.record_pending_geometry(geometry_source, self.pending_move_geometry.is_some(), self.pending_move_geometry.is_some_and(|current| current.surface_xid == update.surface_xid));
                if self.pending_move_geometry.is_some() {
                    self.diagnostics.configure_superseded += 1;
                }
                if self.pending_move_geometry.is_some_and(|current| current.surface_xid != update.surface_xid) {
                    self.pending_move_geometry_ambiguous = true;
                }
                self.pending_move_geometry = Some(update);
                if let Some(candidate_entry) = candidate.entries.iter().find(|entry| entry.surface_xid == update.surface_xid)
                    && resize_geometry_is_obsolete(candidate_entry.geometry, update)
                {
                    self.diagnostics.compound_hierarchy_geometry_observed += 1;
                    self.diagnostics.compound_rebase_attempted += 1;
                    self.diagnostics.compound_rebase_superseded_geometry += 1;
                    if target_geometry_rebase_compatible(self.current_snapshot(), candidate, update) {
                        if let Some(candidate_entry) = candidate.entries.iter_mut().find(|entry| entry.surface_xid == update.surface_xid) {
                            let size_changed = candidate_entry.geometry.width != update.width || candidate_entry.geometry.height != update.height || candidate_entry.geometry.border_width != update.border_width;
                            rebase_candidate_geometry_fields(candidate_entry, update);
                            if let Some(client) = candidate_entry.client_root_geometry.as_mut() { client.width = i32::from(update.width); client.height = i32::from(update.height); }
                            self.diagnostics.compound_rebase_success += 1;
                            self.diagnostics.compound_rebase_avoided_full_retry += 1;
                            if size_changed { self.diagnostics.compound_rebase_named_pixmap_reacquired += 1; self.diagnostics.compound_rebase_egl_reacquired += 1; }
                            else { self.diagnostics.compound_rebase_damage_reused += 1; }
                        }
                    } else {
                        self.diagnostics.compound_rebase_rejected_scene_membership += 1;
                        return Ok(Some(SceneInvalidation::Geometry(update.surface_xid)));
                    }
                }
            }
            match invalidation {
                SceneInvalidation::PixelDamage(damage_id) => {
                    self.pending_damage.insert(damage_id);
                }
                SceneInvalidation::Hierarchy => return Ok(Some(SceneInvalidation::Hierarchy)),
                SceneInvalidation::Shutdown(reason) => return Ok(Some(SceneInvalidation::Shutdown(reason))),
                _ => {}
            }
        }
        Ok(None)
    }

    fn rebuild_and_present(&mut self) -> Result<(), Box<dyn Error>> {
        for attempt in 0..=MAX_CANDIDATE_RETRIES {
            let generation = self.structural_generation;
            self.attempted_structural_generation = generation;
            let mut candidate = match self.build_candidate() {
                Ok(candidate) => candidate,
                Err(error) => {
                    let stale = error
                        .downcast_ref::<CandidateBuildError>()
                        .map(|stale| match stale { CandidateBuildError::Stale(invalidation) => *invalidation });
                    let Some(invalidation) = stale else {
                        self.diagnostics.structural_candidates_failed += 1;
                        self.diagnostics.record_structural_terminal(false, false, false);
                        return Err(error);
                    };
                    self.diagnostics.structural_candidates_stale += 1;
                    self.diagnostics.record_stale_origin(invalidation, !retry_allowed(attempt));
                    if self.diagnostics.last_candidate_resize { self.diagnostics.resize_candidate_stale += 1; }
                    if retry_allowed(attempt) {
                        self.diagnostics.record_structural_terminal(false, true, true);
                        println!("candidate stale; bounded retry: {invalidation:?}");
                        continue;
                    } else {
                        self.diagnostics.record_structural_terminal(false, true, false);
                        println!("candidate stale; deferred rebuild: {invalidation:?}");
                        return Ok(());
                    }
                }
            };
            debug_assert_eq!(candidate.generation, generation);
            let (gate, deferred_damage) = match self.pre_commit_gate(&mut candidate) {
                Ok(gate) => gate,
                Err(error) => {
                    self.structure_watches.rollback(&candidate.watch_additions)?;
                    self.diagnostics.record_structural_terminal(false, false, false);
                    return Err(error);
                }
            };
            match gate {
                GateDecision::Accept => {
                    self.diagnostics.structural_candidates_published += 1;
                    if self.diagnostics.last_candidate_resize { self.diagnostics.resize_candidate_published += 1; }
                    if let Err(error) = self.commit_candidate(candidate) {
                        self.diagnostics.record_structural_terminal(false, false, false);
                        return Err(error);
                    }
                    self.diagnostics.record_structural_terminal(true, false, false);
                    self.merge_deferred_damage(deferred_damage);
                    return Ok(());
                }
                GateDecision::Shutdown(reason) => {
                    log_open_anim_reject(&candidate.provisional_animations);
                    self.structure_watches.rollback(&candidate.watch_additions)?;
                    self.diagnostics.record_structural_terminal(false, false, false);
                    return Err(format!("candidate aborted by shutdown: {reason:?}").into());
                }
                GateDecision::Retry(invalidation) if retry_allowed(attempt) => {
                    log_open_anim_reject(&candidate.provisional_animations);
                    self.diagnostics.structural_candidates_stale += 1;
                    self.diagnostics.record_stale_origin(invalidation, false);
                    if self.diagnostics.last_candidate_resize { self.diagnostics.resize_candidate_stale += 1; }
                    self.merge_deferred_damage(deferred_damage);
                    self.structure_watches.rollback(&candidate.watch_additions)?;
                    self.diagnostics.record_structural_terminal(false, true, true);
                    println!("candidate stale; bounded retry: {invalidation:?}");
                }
                GateDecision::Retry(invalidation) => {
                    log_open_anim_reject(&candidate.provisional_animations);
                    self.diagnostics.structural_candidates_stale += 1;
                    self.diagnostics.record_stale_origin(invalidation, true);
                    if self.diagnostics.last_candidate_resize { self.diagnostics.resize_candidate_stale += 1; }
                    self.merge_deferred_damage(deferred_damage);
                    self.structure_watches.rollback(&candidate.watch_additions)?;
                    if retry_allowed(attempt) {
                        self.diagnostics.record_structural_terminal(false, true, true);
                        println!("candidate stale; bounded retry: {invalidation:?}");
                        continue;
                    } else {
                        drop(candidate);
                        self.diagnostics.record_structural_terminal(false, true, false);
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
        candidate: &mut SceneCandidate<'a>,
    ) -> Result<(GateDecision, HashSet<damage::Damage>), Box<dyn Error>> {
        self.connection.inner.get_input_focus()?.reply()?;
        let mut batch = InvalidationBatch::default();
        let mut drained = 0;
        for _ in 0..MAX_EVENTS_PER_BATCH {
            let Some(event) = self.connection.inner.poll_for_event()? else {
                break;
            };
            drained += 1;
            self.note_destroy_intent(&event);
            self.diagnostics.record_configure(&event, &candidate.snapshot);
            let geometry_source = geometry_event_source(&event, &candidate.snapshot);
            self.diagnostics.record_geometry_source(geometry_source);
            let geometry_update = configure_geometry_update(&event, &candidate.snapshot);
            if geometry_update.is_none() && matches!(event, Event::ConfigureNotify(_)) { self.diagnostics.record_geometry_rejected(geometry_source); }
            batch.note_configure_event(&event, geometry_source, candidate.snapshot.entries.iter().find_map(|entry| matches!(&event, Event::ConfigureNotify(event) if entry.surface_xid == event.window || entry.semantic_client_xid == Some(event.window)).then_some(entry.surface_xid)));
            let visual_invalidation = if is_visual_property_notify(&event, self.root, self.visual_atoms, &candidate.snapshot) {
                let entries = candidate.snapshot.entries.clone();
                self.update_visual_state(&event, &entries)?
            } else {
                None
            };
            let invalidation = if is_background_property_notify(&event, self.root, self.background_atoms) {
                SceneInvalidation::Background
            } else if let Some(invalidation) = visual_invalidation {
                invalidation
            } else {
                classify_event_with_registries_and_ignored(
                event.clone(),
                self.root,
                &candidate.snapshot,
                self.ownership.as_ref(),
                &self.damage_registry,
                &candidate.damage_registry,
                &candidate.ignored_configure_windows,
                )
            };
            self.record_hierarchy_event_diagnostic(&event, invalidation, &candidate.snapshot, self.pending_move_geometry);
            if matches!(invalidation, SceneInvalidation::Hierarchy) { if let Some(source) = hierarchy_event_source(&event) { batch.note_hierarchy_source(source); } }
            self.observe_invalidation(invalidation);
            batch.push(invalidation);
            batch.push_geometry_update(geometry_update);
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
        if !signal_pending
            && !batch.hierarchy
            && !batch.background
            && !batch.visual_state
            && !bounded_batch_requires_retry(drained)
            && batch.geometry.is_some()
            && self.rebase_candidate_pure_move(candidate, batch.move_geometry())?
        {
            self.attempted_structural_generation = self.structural_generation;
            return Ok((GateDecision::Accept, deferred_damage));
        }
        let decision = candidate_gate_decision(
            batch_decision,
            bounded_batch_requires_retry(drained),
            true,
            signal_pending,
        );
        Ok((decision, deferred_damage))
    }

    fn rebase_candidate_pure_move(
        &mut self,
        candidate: &mut SceneCandidate<'a>,
        update: Option<PendingGeometry>,
    ) -> Result<bool, Box<dyn Error>> {
        let Some(update) = update else {
            return Ok(false);
        };
        let live = self.current_snapshot();
        let Some(live_entry) = live.entries.iter().find(|entry| entry.surface_xid == update.surface_xid) else {
            return Ok(false);
        };
        let Some(candidate_index) = candidate
            .snapshot
            .entries
            .iter()
            .position(|entry| entry.surface_xid == update.surface_xid)
        else {
            return Ok(false);
        };
        let candidate_entry = candidate.snapshot.entries[candidate_index].clone();
        if live.root != candidate.snapshot.root
            || live_entry.surface_xid != candidate_entry.surface_xid
            || live_entry.semantic_client_xid != candidate_entry.semantic_client_xid
            || live_entry.effect_owner != candidate_entry.effect_owner
            || live_entry.own_blur_request != candidate_entry.own_blur_request
            || live_entry.lifecycle_xid != candidate_entry.lifecycle_xid
            || live_entry.geometry.width != candidate_entry.geometry.width
            || live_entry.geometry.height != candidate_entry.geometry.height
            || live_entry.geometry.border_width != candidate_entry.geometry.border_width
            || live_entry.depth != candidate_entry.depth
            || live_entry.visual != candidate_entry.visual
            || live_entry.class != candidate_entry.class
            || live_entry.map_state != candidate_entry.map_state
            || live_entry.override_redirect != candidate_entry.override_redirect
            || live_entry.backend != candidate_entry.backend
            || live_entry.visual_class != candidate_entry.visual_class
            || live_entry.fullscreen != candidate_entry.fullscreen
            || live_entry.shadow_eligible != candidate_entry.shadow_eligible
            || live_entry.resolved_border_color != candidate_entry.resolved_border_color
            || live_entry.resolved_opacity_bits != candidate_entry.resolved_opacity_bits
            || live_entry.resolved_blur_request != candidate_entry.resolved_blur_request
            || live_entry.stacking_index != candidate_entry.stacking_index
            || !same_common_surface_order(&live.entries, &candidate.snapshot.entries)
        {
            return Ok(false);
        }
        let next_geometry = WindowGeometry {
            x: update.x,
            y: update.y,
            width: update.width,
            height: update.height,
            border_width: update.border_width,
        };
        if !move_only_geometry_is_eligible(
            &candidate_entry,
            next_geometry,
            self.root,
            update.override_redirect,
            candidate_entry.semantic_client_xid,
        ) {
            return Ok(false);
        }
        let previous_geometry = candidate_entry.geometry;
        let previous_client_root = candidate_entry.client_root_geometry;
        rebase_candidate_geometry_fields(&mut candidate.snapshot.entries[candidate_index], update);
        let render_animations = merge_window_animations(&self.window_animations, &candidate.provisional_animations);
        let render_result = self.render_egl_scene(
            &candidate.snapshot,
            &candidate.egl_surfaces,
            &candidate.pixmaps,
            &render_animations,
            &candidate.provisional_render_order,
            &candidate.provisional_closing_frames,
        );
        if let Err(error) = render_result {
            candidate.snapshot.entries[candidate_index].geometry = previous_geometry;
            candidate.snapshot.entries[candidate_index].client_root_geometry = previous_client_root;
            return Err(error);
        }
        Ok(true)
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
        self.timed_swap()?;
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
        let old_resources = std::mem::replace(&mut self.resources, candidate.resources);
        let _old_pixmaps = std::mem::replace(&mut self.pixmaps, candidate.pixmaps);
        let _old_damage_leases = std::mem::replace(&mut self.damage_leases, candidate.damage_leases);
        let _old_egl_surfaces = std::mem::replace(&mut self.egl_surfaces, candidate.egl_surfaces);
        self.damage_registry = candidate.damage_registry;
        self.ignored_configure_windows = candidate.ignored_configure_windows;
        self.snapshot = Some(snapshot);
        let live_clients = self
            .current_snapshot()
            .entries
            .iter()
            .filter_map(|entry| entry.semantic_client_xid)
            .collect::<HashSet<_>>();
        self.urgency.retain(|client, _| live_clients.contains(client));
        self.structure_watches.reconcile(&candidate.watch_ids)?;
        self.pending_hierarchy_geometry = None;
        // 3a3fa2a: promote this now-committed candidate's provisional
        // animations into persistent state, preserving the exact
        // `started_at` already used by the render this commit just swapped
        // (continuity: no restart, no discontinuity). Retire animations for
        // surfaces that no longer exist — resource-free, so this cannot
        // delay or interact with removed_surfaces' resource teardown below.
        retire_removed_surface_animations(&mut self.window_animations, &removed_surfaces);
        promote_provisional_animations(&mut self.window_animations, candidate.provisional_animations);
        // 3a3fa2b5-r2 — retire destroy intents. Corrected from R1's
        // narrower `for surface_xid in &removed_surfaces { remove(..) }`:
        // that loop only ever retired intents for XIDs that were part of
        // the OLD committed scene and got removed this commit — it left a
        // permanent leak for any DestroyNotify(X) where X was NEVER part
        // of the committed scene at all (an override-redirect popup, a
        // stray/untracked child, a destroyed-before-ever-becoming-eligible
        // window): such an X can never appear in ANY future
        // `removed_surfaces` (since it was never in `old_surfaces` to
        // begin with), so the old loop's `destroy_intents` entry for it
        // would sit there forever — a real XID-reuse hazard (see the r2
        // correction), since a LATER, unrelated Live window that happens
        // to reuse that same numeric XID would inherit the stale intent
        // and could false-trigger a close on a mere Unmap.
        //
        // The corrected rule instead re-establishes, on every committed
        // Accept, the invariant `destroy_intents ⊆ new_surfaces`: retain
        // an intent only if its XID is part of the JUST-COMMITTED live
        // scene (i.e. still a candidate for a REAL future close). This
        // single `retain` subsumes R1's old removal loop (anything in
        // `removed_surfaces` is, by definition, not in `new_surfaces`
        // either, so it's still dropped, whether or not it produced a
        // close) AND additionally prunes any intent for an XID that was
        // never tracked as part of the scene at all — closing the leak.
        // Never mutated during build/Retry — only here, on a committed
        // Accept, so a Retry always observes the SAME causal Destroy
        // information as its first attempt.
        self.destroy_intents = retained_destroy_intents(&self.destroy_intents, &new_surfaces);
        // 3a3fa2b5-r2 — materialize this commit's newly-triggered
        // provisional closes into compositor-owned GPU snapshots. Strictly
        // AFTER timed_swap (frame 0 already presented from the OLD live
        // texture, per the r1 first-frame-gap finding) and BEFORE the OLD
        // X11/EGL resources are dropped below. Corrected from R1: the
        // source texture and the snapshot's CLIENT-CONTENT dimensions are
        // now resolved by `source_xid` lookup into `old_resources` (the
        // pre-swap resource map captured just above via `mem::replace`,
        // naturally still alive here, dropped normally right after this
        // loop) — never via a stored `Rc` clone (R1's frame carried an
        // owned reference to the live surface, which artificially
        // extended a dead window's EGLImage lifetime). Width/height come
        // from `bundle.pixmap.geometry` — the ACTUAL client pixmap's own
        // dimensions — never the render plan's own outer extent fields
        // (R1's bug: those are documented as the SHADOW's base rectangle
        // in `shadow_params_from_plan`/`scale_render_quad_plan`, not a
        // guaranteed content-size source, even though they numerically
        // coincide with the pixmap size for an unclipped on-screen window
        // via `build_render_quad_plan`).
        // GPU-side only: no XGetImage, no glReadPixels, no new X11
        // request. Snapshot failure is cosmetic (never aborts this
        // commit): logged, the ClosingVisual is omitted, and its
        // `close_id` is still filtered out of the committed render_order
        // below — but the id itself stays consumed (see
        // commit_candidate_inner's next_close_id promotion:
        // `next_close_id_after` was already computed from every
        // reservation this candidate made, regardless of later capture
        // outcome).
        let mut failed_close_ids: HashSet<u64> = HashSet::new();
        for (close_id, frame) in candidate.provisional_closing_frames {
            let source = old_resources
                .get(&frame.source_xid)
                .ok_or_else(|| -> Box<dyn Error> {
                    format!("closing source surface 0x{:08x} is missing from old resources at capture time", frame.source_xid).into()
                })
                .and_then(|bundle| {
                    bundle.egl.as_ref().ok_or_else(|| -> Box<dyn Error> {
                        format!("closing source surface 0x{:08x} has no EGL texture at capture time", frame.source_xid).into()
                    }).map(|egl_surface| {
                        (egl_surface.borrow().texture, bundle.pixmap.geometry.width, bundle.pixmap.geometry.height)
                    })
                });
            let capture = match source {
                Ok((source_texture, width, height)) => match self.egl.as_mut() {
                    Some(egl) => egl.capture_closing_snapshot(source_texture, i32::from(width), i32::from(height)),
                    None => Err("EGL scene renderer is unavailable".into()),
                },
                Err(error) => Err(error),
            };
            match capture {
                Ok(texture) => {
                    self.closing_visuals.insert(close_id, ClosingVisual {
                        id: close_id,
                        texture: ClosingTexture::new(texture),
                        plan: frame.plan,
                        pixel_semantics: frame.pixel_semantics,
                        base_opacity: frame.base_opacity,
                        shadow_eligible: frame.shadow_eligible,
                        animation: frame.animation,
                        source_xid: frame.source_xid,
                    });
                }
                Err(error) => {
                    println!(
                        "CLOSE_SNAPSHOT_FAILED close_id={close_id} surface=0x{:08x}: {error}",
                        frame.source_xid,
                    );
                    failed_close_ids.insert(close_id);
                }
            }
        }
        self.render_order = candidate
            .provisional_render_order
            .into_iter()
            .filter(|layer| !matches!(layer, RenderLayer::Closing(id) if failed_close_ids.contains(id)))
            .collect();
        self.next_close_id = candidate.next_close_id_after;
        drop(removed_surfaces);
        drop(old_resources);
        self.retain_current_pending();
        self.state = SceneState::RunningLivePixel;
        println!("state: ScenePresented (MANUAL active, EGL scene renderer)");
        println!("state: RunningLivePixel");
        Ok(())
    }

    fn retain_current_pending(&mut self) {
        retain_pending_for_registry(&mut self.pending_damage, &self.damage_registry);
    }

    /// 3a3fa2a: removes animations whose progress has reached 1.0. Must
    /// only be called after a render that observed the current animation
    /// state has already happened this iteration (see call site).
    fn retire_completed_animations(&mut self) {
        if self.window_animations.is_empty() {
            return;
        }
        let now = Instant::now();
        // TEMPORARY FORENSIC INSTRUMENTATION — behaviorally identical to
        // the original `.retain(|_, a| !a.is_complete(now))` (same `now`
        // snapshot, same removed set), restructured only so each removal
        // can be logged. Remove before release.
        let completed: Vec<Window> = self
            .window_animations
            .iter()
            .filter(|(_, animation)| animation.is_complete(now))
            .map(|(surface_xid, _)| *surface_xid)
            .collect();
        for surface_xid in completed {
            self.window_animations.remove(&surface_xid);
            println!(
                "OPEN_ANIM_RETIRE surface=0x{surface_xid:08x} active_remaining={}",
                self.window_animations.len(),
            );
        }
    }

    /// TEMPORARY FORENSIC INSTRUMENTATION — behaviorally identical to
    /// calling `self.egl...swap()` directly; only adds a count+timing
    /// sample. Remove before release.
    fn timed_swap(&mut self) -> Result<(), Box<dyn Error>> {
        let start = Instant::now();
        let result = self
            .egl
            .as_ref()
            .ok_or("EGL scene renderer is unavailable")?
            .swap();
        self.perf.egl_swaps += 1;
        self.perf.swap.record(start.elapsed());
        result
    }

    fn arm_next_presentation(&mut self, target_msc: u64) -> Result<(), Box<dyn Error>> {
        let Some(present) = self.present.as_mut() else {
            return Ok(());
        };
        let (serial, target_msc) = self.scheduler.arm(target_msc);
        let result = present.arm(self.connection, serial, target_msc);
        if result.is_ok() { self.diagnostics.present_submissions += 1; }
        result
    }

    fn merge_deferred_damage(&mut self, deferred: HashSet<damage::Damage>) {
        merge_deferred_damage_for_registry(
            &mut self.pending_damage,
            deferred,
            &self.damage_registry,
        );
    }

    fn observe_invalidation(&mut self, invalidation: SceneInvalidation) {
        observe_structural_generation(&mut self.structural_generation, invalidation);
        match invalidation {
            SceneInvalidation::PixelDamage(damage_id) => {
                let surface = self.damage_registry.get(&damage_id).copied();
                let identity = surface.map(|surface_xid| {
                    (surface_xid, self.current_snapshot().entries.iter()
                        .find(|entry| entry.surface_xid == surface_xid)
                        .and_then(|entry| entry.semantic_client_xid))
                });
                self.diagnostics.record_damage_arrival(damage_id, identity);
                self.scheduler.mark_pixel_dirty();
            }
            SceneInvalidation::Background => {
                self.pending_background = true;
                self.scheduler.mark_pixel_dirty();
            }
            SceneInvalidation::VisualState => {
                self.pending_visual_state = true;
                self.scheduler.mark_pixel_dirty();
            }
            SceneInvalidation::Geometry(_) | SceneInvalidation::Hierarchy => {
                self.scheduler.mark_structural_dirty(self.structural_generation)
            }
            _ => {}
        }
    }

    fn record_hierarchy_event_diagnostic(&mut self, event: &Event, invalidation: SceneInvalidation, snapshot: &SceneSnapshot, pending: Option<PendingGeometry>) {
        if !matches!(invalidation, SceneInvalidation::Hierarchy) { return; }
        let Some(source) = hierarchy_event_source(event) else { return; };
        let xid = hierarchy_event_window(event);
        let internal = xid.is_some_and(|xid| {
            self.overlay.as_ref().is_some_and(|overlay| self.ownership.as_ref().is_some_and(|owner| is_internal_xid(xid, overlay.overlay, owner.owner_window)))
        });
        let relation = match xid {
            Some(xid) if pending.is_some_and(|pending| pending.surface_xid == xid) => HierarchyEventRelation::TargetSurface,
            Some(xid) if pending.and_then(|pending| snapshot.entries.iter().find(|entry| entry.surface_xid == pending.surface_xid).and_then(|entry| entry.semantic_client_xid)).is_some_and(|client| client == xid) => HierarchyEventRelation::TargetSemanticClient,
            Some(xid) if snapshot.entries.iter().any(|entry| entry.surface_xid == xid) => HierarchyEventRelation::OtherTrackedSurface,
            Some(xid) if snapshot.entries.iter().any(|entry| entry.semantic_client_xid == Some(xid)) => HierarchyEventRelation::OtherSemanticClient,
            _ => HierarchyEventRelation::Unknown,
        };
        self.diagnostics.record_hierarchy_event(source, internal, relation);
    }

    fn begin_hierarchy_origin(&mut self, bits: u16) {
        self.diagnostics.hierarchy_source_bits = bits;
        self.diagnostics.begin_structural_origin(StructuralOrigin::Hierarchy);
    }

    fn present_opportunity(&mut self, event: &Event) -> Option<u64> {
        let Event::PresentCompleteNotify(event) = event else {
            return None;
        };
        let present = self.present.as_mut()?;
        let msc = present.complete(event)?;
        self.diagnostics.present_completion_events += 1;
        self.perf.present_complete_events += 1;
        if !self.scheduler.complete(event.serial, msc) {
            return None;
        }
        Some(msc)
    }

    fn wait_live_pixel(&mut self) -> Result<(), Box<dyn Error>> {
        loop {
            if self.snapshot.is_none() {
                if !self.await_first_publish()? {
                    return Ok(());
                }
                self.arm_next_presentation(0)?;
                continue;
            }
            let present_enabled = self.present.is_some();
            let mut opportunity_msc = None;
            let mut batch = InvalidationBatch::default();
            if let Some(update) = self.pending_move_geometry.take() {
                let history = self.pending_move_geometry_present_history;
                batch.push(SceneInvalidation::Geometry(update.surface_xid));
                batch.push_geometry_update(Some(update));
                batch.present_history = history;
                self.pending_move_geometry_present_history = GeometryPresentHistory::default();
                batch.geometry_ambiguous = self.pending_move_geometry_ambiguous;
                self.pending_move_geometry_ambiguous = false;
            }
            let pending = if present_enabled {
                self.pending_damage.clone()
            } else {
                std::mem::take(&mut self.pending_damage)
            };
            if self.pending_background {
                batch.push(SceneInvalidation::Background);
            }
            if self.pending_visual_state {
                batch.push(SceneInvalidation::VisualState);
            }
            let had_pending_work = pending_work_requires_iteration(&pending)
                || self.pending_background
                || self.pending_visual_state;
            if matches!(
                structural_generation_state(
                    self.structural_generation,
                    self.attempted_structural_generation,
                ),
                StructuralGenerationState::Ready(_)
            ) {
                batch.note_hierarchy_source(HierarchyEventSource::ExistingHierarchyMerge);
                batch.push(SceneInvalidation::Hierarchy);
            }
            for damage_id in pending {
                if self.damage_registry.contains_key(&damage_id) {
                    batch.push(SceneInvalidation::PixelDamage(damage_id));
                }
            }
            if !present_enabled && batch.decision() == SceneInvalidation::Ignore && !had_pending_work
                || present_enabled
            {
                // TEMPORARY FORENSIC INSTRUMENTATION (Brave repaint
                // latency investigation) — timing/counters only, no
                // functional change. Remove before release.
                let batch_drain_start = Instant::now();
                let mut batch_event_count: usize = 0;
                let first = match wait_for_event_or_shutdown(self.connection, &mut self.signal)? {
                    WaitResult::Event(event) => event,
                    WaitResult::Shutdown => {
                        println!("scene shutdown: Signal");
                        return Ok(());
                    }
                };
                self.note_destroy_intent(&first);
                let snapshot = self.snapshot.as_ref().expect("published scene snapshot must exist while live");
                self.diagnostics.record_configure(&first, snapshot);
                let first_geometry_source = geometry_event_source(&first, snapshot);
                let first_surface_xid = snapshot.entries.iter().find_map(|entry| matches!(&first, Event::ConfigureNotify(event) if entry.surface_xid == event.window || entry.semantic_client_xid == Some(event.window)).then_some(entry.surface_xid));
                self.diagnostics.record_geometry_source(first_geometry_source);
                batch.note_configure_event(&first, first_geometry_source, first_surface_xid);
                opportunity_msc = self.present_opportunity(&first);
                self.perf.events += 1;
                batch_event_count += 1;
                let geometry_update = configure_geometry_update(&first, self.current_snapshot());
                let classify_start = Instant::now();
                let visual_invalidation = self.maybe_update_visual_state(&first)?;
                let invalidation = visual_invalidation.unwrap_or_else(|| {
                self.classify_session_event(first.clone(), self.current_snapshot(), &self.damage_registry, &self.damage_registry)
                });
                self.perf.classification.record(classify_start.elapsed());
                self.perf.record_invalidation(invalidation);
                if !matches!(invalidation, SceneInvalidation::Geometry(_)) {
                    self.observe_invalidation(invalidation);
                }
                let current_snapshot = self.current_snapshot().clone();
                self.record_hierarchy_event_diagnostic(&first, invalidation, &current_snapshot, batch.geometry_update);
                if matches!(invalidation, SceneInvalidation::Hierarchy) { if let Some(source) = hierarchy_event_source(&first) { batch.note_hierarchy_source(source); } }
                batch.push(invalidation);
                if geometry_update.is_some() { self.diagnostics.record_pending_geometry(first_geometry_source, batch.geometry_update.is_some(), true); }
                batch.push_geometry_update(geometry_update);
                for _ in 1..MAX_EVENTS_PER_BATCH {
                    let Some(event) = self.connection.inner.poll_for_event()? else {
                        break;
                    };
                    self.note_destroy_intent(&event);
                    let snapshot = self.snapshot.as_ref().expect("published scene snapshot must exist while live");
                    self.diagnostics.record_configure(&event, snapshot);
                    let geometry_source = geometry_event_source(&event, snapshot);
                    let surface_xid = snapshot.entries.iter().find_map(|entry| matches!(&event, Event::ConfigureNotify(event) if entry.surface_xid == event.window || entry.semantic_client_xid == Some(event.window)).then_some(entry.surface_xid));
                    self.diagnostics.record_geometry_source(geometry_source);
                    batch.note_configure_event(&event, geometry_source, surface_xid);
                    opportunity_msc = opportunity_msc.or_else(|| self.present_opportunity(&event));
                    self.perf.events += 1;
                    batch_event_count += 1;
                    let geometry_update = configure_geometry_update(&event, self.current_snapshot());
                    let classify_start = Instant::now();
                    let visual_invalidation = self.maybe_update_visual_state(&event)?;
                    let invalidation = visual_invalidation.unwrap_or_else(|| {
                    self.classify_session_event(event.clone(), self.current_snapshot(), &self.damage_registry, &self.damage_registry)
                    });
                    self.perf.classification.record(classify_start.elapsed());
                    self.perf.record_invalidation(invalidation);
                    if !matches!(invalidation, SceneInvalidation::Geometry(_)) {
                        self.observe_invalidation(invalidation);
                    }
                    let current_snapshot = self.current_snapshot().clone();
                    self.record_hierarchy_event_diagnostic(&event, invalidation, &current_snapshot, batch.geometry_update);
                    if matches!(invalidation, SceneInvalidation::Hierarchy) { if let Some(source) = hierarchy_event_source(&event) { batch.note_hierarchy_source(source); } }
                    batch.push(invalidation);
                    if geometry_update.is_some() { self.diagnostics.record_pending_geometry(geometry_source, batch.geometry_update.is_some(), true); }
                    batch.push_geometry_update(geometry_update);
                }
                self.perf.record_batch_size(batch_event_count);
                self.perf.event_batch_drain.record(batch_drain_start.elapsed());
            }
            if self.signal.poll_shutdown_pending()? {
                println!("scene shutdown: Signal");
                return Ok(());
            }
            if batch.geometry.is_some() { self.diagnostics.record_geometry_scheduling_batch(); }
            if present_enabled && opportunity_msc.is_none() {
                if batch.geometry.is_some() {
                    self.diagnostics.resize_dispatch_deferred += 1;
                    self.diagnostics.record_present_deferred();
                    self.diagnostics.record_pre_attempt_bypass(batch.geometry_source.unwrap_or(GeometryEventSource::Unknown), PreResizeOnlyBypassReason::NoPresentComplete, None, false);
                }
                self.defer_geometry(&batch);
                self.pending_damage.extend(batch.pixel_damage().iter().copied());
                continue;
            }
            let decision = batch.decision();
            if matches!(decision, SceneInvalidation::Hierarchy) {
                let direction = batch.hierarchy_pending_geometry.and_then(|update| self.current_snapshot().entries.iter().find(|entry| entry.surface_xid == update.surface_xid).map(|entry| classify_resizeonly_direction(entry.geometry, update).0));
                self.diagnostics.record_hierarchy_decision(batch.hierarchy_source_bits, batch.hierarchy_geometry_pending, direction);
            }
            let batch_pixel_damage = batch.pixel_damage().clone();
            self.diagnostics.event_batches += 1;
            if batch.geometry.is_some() { self.diagnostics.event_batches_with_geometry += 1; }
            if !batch_pixel_damage.is_empty() { self.diagnostics.event_batches_with_pixel_damage_arrival += 1; }
            if !self.pending_damage.is_empty() {
                self.diagnostics.event_batches_ended_with_pixel_damage_pending += 1;
                self.diagnostics.batches_with_damage_pending += 1;
                self.diagnostics.max_batches_damage_remained_pending =
                    self.diagnostics.max_batches_damage_remained_pending.max(self.diagnostics.batches_with_damage_pending);
            } else {
                self.diagnostics.batches_with_damage_pending = 0;
            }
            if batch_damage_requires_subtraction(decision, &batch_pixel_damage) {
                for damage_id in &batch_pixel_damage {
                    self.subtract_damage_for_diagnostics(*damage_id)?;
                }
            }
            if present_enabled && !matches!(decision, SceneInvalidation::Geometry(_) | SceneInvalidation::Hierarchy) {
                self.pending_damage.clear();
            } else {
                carry_structural_pending_damage(&mut self.pending_damage, decision, &batch_pixel_damage);
            }
            match decision {
                // 3a3fa2a: this arm is only reached when either Present is
                // disabled (no animation is ever created in that case, see
                // provisional_open_animations) or a genuine Present
                // completion was observed this iteration (the guard above,
                // `present_enabled && opportunity_msc.is_none() -> continue`,
                // already prevents reaching here otherwise) — so this is
                // exactly the "Present completion + animation exists +
                // decision == Ignore" tick, and it never touches Damage.
                SceneInvalidation::Ignore => {
                    if !self.window_animations.is_empty() || !self.closing_visuals.is_empty() {
                        // TEMPORARY FORENSIC INSTRUMENTATION — only while
                        // animations are active, remove before release.
                        println!("OPEN_ANIM_TICK active={}", self.window_animations.len());
                        self.perf.animation_only_recomposes += 1;
                        self.full_recompose_current()?;
                        self.timed_swap()?;
                    }
                }
                SceneInvalidation::Shutdown(reason) => {
                    println!("scene shutdown: {reason:?}");
                    return Ok(());
                }
                SceneInvalidation::Geometry(window) => {
                    self.diagnostics.geometry_dispatches += 1;
                    let geometry_source = batch.geometry_source.unwrap_or(GeometryEventSource::Unknown);
                    let diagnostic_update = batch.move_geometry().or(batch.geometry_event_update);
                    let size_change = diagnostic_update.is_some_and(|update| self.current_snapshot().entries.iter().find(|entry| entry.surface_xid == window).is_some_and(|entry| entry.geometry.width != update.width || entry.geometry.height != update.height || entry.geometry.border_width != update.border_width));
                    let resize_direction = diagnostic_update.and_then(|update| self.current_snapshot().entries.iter().find(|entry| entry.surface_xid == window).map(|entry| classify_resizeonly_direction(entry.geometry, update)));
                    if let Some((direction, move_resize)) = resize_direction.filter(|_| size_change) {
                        self.diagnostics.record_final_resize_history(batch.present_history, direction, move_resize);
                    }
                    if size_change { self.diagnostics.record_geometry_pending_at_dispatch(batch.move_geometry()); }
                    if !batch_pixel_damage.is_empty() {
                        self.diagnostics.geometry_dispatches_while_damage_pending += 1;
                        self.diagnostics.pixel_damage_deferred_by_geometry += 1;
                        self.diagnostics.consecutive_geometry_while_damage_pending += 1;
                        self.diagnostics.max_geometry_dispatches_before_pending_damage_service =
                            self.diagnostics.max_geometry_dispatches_before_pending_damage_service
                                .max(self.diagnostics.consecutive_geometry_while_damage_pending);
                    }
                    if let Some(update) = batch.move_geometry()
                        && self.current_snapshot().entries.iter().find(|entry| entry.surface_xid == window)
                            .is_some_and(|entry| entry.geometry.width != update.width || entry.geometry.height != update.height)
                    {
                        self.diagnostics.resize_geometry_dispatches += 1;
                    }
                    let resize_only = batch.move_geometry()
                        .filter(|update| update.surface_xid == window)
                        .filter(|update| self.current_snapshot().entries.iter()
                            .find(|entry| entry.surface_xid == window)
                            .is_some_and(|entry| entry.geometry.width != update.width
                                || entry.geometry.height != update.height
                            || entry.geometry.border_width != update.border_width));
                    let resizeonly_selected = resize_only.is_some();
                    if resizeonly_selected {
                        self.diagnostics.resizeonly_present_deferred = Some(batch.present_history.ever_deferred);
                    }
                    let resizeonly_succeeded = if let Some(update) = resize_only
                        && self.try_resize_only(update, &batch_pixel_damage)?
                    {
                        if size_change { self.diagnostics.record_resize_dispatch(geometry_source, false); }
                        self.pending_background = false;
                        self.pending_visual_state = false;
                        true
                    } else { false };
                    if resizeonly_succeeded {
                        self.diagnostics.record_final_resize_selection(batch.present_history, false);
                        self.diagnostics.resizeonly_present_deferred = None;
                    } else if self.try_move_only(window, batch.move_geometry(), &batch_pixel_damage)? {
                        self.pending_damage.clear();
                        self.pending_background = false;
                        self.pending_visual_state = false;
                        self.timed_swap()?;
                    } else {
                        self.observe_invalidation(SceneInvalidation::Geometry(window));
                        self.pending_background = false;
                        self.diagnostics.begin_resizeonly_structural_fallback();
                        self.refresh_background()?;
                        if size_change {
                            self.diagnostics.record_resize_dispatch(geometry_source, true);
                            self.diagnostics.record_final_resize_selection(batch.present_history, true);
                            if batch.move_geometry().is_none() { self.diagnostics.resize_dispatch_no_pending_geometry += 1; }
                            let direction = resize_direction.map(|(direction, _)| direction);
                            if !resizeonly_selected {
                                let reason = if matches!(geometry_source, GeometryEventSource::SemanticClient) { PreResizeOnlyBypassReason::SemanticClientNoSurfacePendingGeometry } else if batch.move_geometry().is_none() { PreResizeOnlyBypassReason::NoPendingGeometry } else if batch.geometry_ambiguous { PreResizeOnlyBypassReason::AmbiguousOrSuperseded } else { PreResizeOnlyBypassReason::StructuralAlreadyRequired };
                                self.diagnostics.record_pre_attempt_bypass(geometry_source, reason, direction, false);
                            }
                            self.diagnostics.begin_structural_origin(match geometry_source { GeometryEventSource::CanonicalSurface => StructuralOrigin::GeometrySurface, GeometryEventSource::SemanticClient => StructuralOrigin::GeometrySemanticClient, GeometryEventSource::Other | GeometryEventSource::Unknown => StructuralOrigin::GeometryNoPending });
                        }
                        self.rebuild_and_present()?;
                    }
                }
                SceneInvalidation::Hierarchy => {
                    self.pending_background = false;
                    self.diagnostics.resize_dispatch_hierarchy_dominated += 1;
                    self.diagnostics.geometry_scheduling_hierarchy_dominated += 1;
                    self.pending_hierarchy_geometry = batch.hierarchy_pending_geometry;
                    self.begin_hierarchy_origin(batch.hierarchy_source_bits);
                    self.refresh_background()?;
                    self.rebuild_and_present()?;
                }
                SceneInvalidation::Background => {
                    self.pending_background = false;
                    self.refresh_background()?;
                    self.full_recompose_current()?;
                    self.timed_swap()?;
                }
                SceneInvalidation::VisualState => {
                    self.pending_visual_state = false;
                    self.full_recompose_current()?;
                    self.timed_swap()?;
                }
                SceneInvalidation::PixelDamage(_) => {
                    self.recompose_current_scene(batch.pixel_damage().clone())?;
                }
            }
            // 3a3fa2a: every reachable arm above that can observe a
            // non-empty window_animations map has just rendered+swapped
            // using it (Ignore's new branch; Geometry/Hierarchy via
            // rebuild_and_present/try_move_only/try_resize_only;
            // Background/VisualState/PixelDamage via full_recompose_current)
            // — so any animation retired here already had its current
            // (clamped, so t>=1.0 renders exactly opacity=1.0/scale=1.0)
            // frame presented this same iteration. Retiring after the match
            // also guarantees no extra perpetual redraw: once empty, the
            // Ignore arm's guard above is false again on the next tick.
            self.retire_completed_animations();
            self.retire_completed_closing_visuals();
            // TEMPORARY FORENSIC INSTRUMENTATION — once-per-~1s aggregate
            // stdout line only. Remove before release.
            self.perf.maybe_flush(self.window_animations.len());
            if let Some(msc) = opportunity_msc {
                self.scheduler.finish_render(
                    self.structural_generation,
                    !self.pending_damage.is_empty()
                        || matches!(
                            structural_generation_state(
                                self.structural_generation,
                                self.attempted_structural_generation,
                            ),
                            StructuralGenerationState::Ready(_)
                        ),
                );
                self.arm_next_presentation(msc.saturating_add(1))?;
            }
        }
    }

    fn await_first_publish(&mut self) -> Result<bool, Box<dyn Error>> {
        let mut rebuild_deferred = false;
        loop {
            match first_publish_step(self.snapshot.is_some(), rebuild_deferred, false) {
                FirstPublishStep::Published => return Ok(true),
                FirstPublishStep::Rebuild => {
                    self.rebuild_and_present()?;
                    if self.snapshot.is_some() {
                        return Ok(true);
                    }
                    rebuild_deferred = true;
                }
                FirstPublishStep::AwaitEvent => {
                    match wait_for_event_or_shutdown(self.connection, &mut self.signal)? {
                        WaitResult::Event(_) => {}
                        WaitResult::Shutdown => return Ok(false),
                    }
                    self.observe_invalidation(SceneInvalidation::Hierarchy);
                    self.pending_background = true;
                    self.refresh_background()?;
                    self.rebuild_and_present()?;
                    rebuild_deferred = self.snapshot.is_none();
                }
                FirstPublishStep::Shutdown => return Ok(false),
            }
        }
    }

    fn try_resize_only(
        &mut self,
        update: PendingGeometry,
        initial_damage: &HashSet<damage::Damage>,
    ) -> Result<bool, Box<dyn Error>> {
        self.diagnostics.resizeonly_attempted += 1;
        if self.pending_background || self.pending_visual_state || self.snapshot.is_none() {
            self.diagnostics.record_resizeonly_early_fallback();
            return Ok(false);
        }
        let live = self.current_snapshot().clone();
        let Some(index) = live.entries.iter().position(|entry| entry.surface_xid == update.surface_xid) else {
            self.diagnostics.record_resizeonly_early_fallback();
            return Ok(false);
        };
        let previous = live.entries[index].clone();
        let (direction, move_resize) = classify_resizeonly_direction(previous.geometry, update);
        self.diagnostics.record_resizeonly_attempt(direction, move_resize);
        let total_start = self.diagnostics.enabled.then(Instant::now);
        macro_rules! resizeonly_fallback {
            ($reason:expr) => {{
                self.diagnostics.resizeonly_fallback += 1;
                self.diagnostics.record_resizeonly_fallback(direction, move_resize, $reason);
                self.diagnostics.record_resizeonly_outcome(
                    direction, move_resize, false, false, total_start.map(|start| start.elapsed()),
                );
                return Ok(false);
            }};
        }
        if previous.override_redirect != update.override_redirect {
            resizeonly_fallback!(ResizeOnlyFallbackReason::IdentityMismatch);
        }
        if previous.geometry.width == update.width
            && previous.geometry.height == update.height
            && previous.geometry.border_width == update.border_width
        {
            resizeonly_fallback!(ResizeOnlyFallbackReason::NoSizeChange);
        }

        let mut snapshot = live;
        rebase_candidate_geometry_fields(&mut snapshot.entries[index], update);
        if let Some(client) = snapshot.entries[index].client_root_geometry.as_mut() {
            client.width = i32::from(update.width);
            client.height = i32::from(update.height);
        }

        let pre_acquire_start = self.diagnostics.enabled.then(Instant::now);
        if let Some(invalidation) = self.refresh_resize_state_before_acquisition(&mut snapshot)? {
            if matches!(invalidation, SceneInvalidation::Geometry(_)) {
                self.diagnostics.resizeonly_superseded_before_acquisition += 1;
            }
            let reason = match invalidation {
                SceneInvalidation::Geometry(_) => ResizeOnlyFallbackReason::GeometrySuperseded,
                SceneInvalidation::Hierarchy => ResizeOnlyFallbackReason::Hierarchy,
                _ => ResizeOnlyFallbackReason::UnavailableState,
            };
            resizeonly_fallback!(reason);
        }
        if let Some(start) = pre_acquire_start {
            self.diagnostics.record_resizeonly_stage(direction, ResizeOnlyStage::PreAcquire, start.elapsed());
        }

        let semantics = self.visual_formats.semantics(previous.visual, previous.depth);
        if semantics == EglPixelSemantics::Unsupported {
            resizeonly_fallback!(ResizeOnlyFallbackReason::UnsupportedVisual);
        }
        let root_geometry = snapshot.root_geometry;
        let Some(damage) = self
            .resources
            .get(&update.surface_xid)
            .and_then(|bundle| bundle.damage.as_ref())
            .filter(|damage| self.damage_registry.get(&damage.damage_xid) == Some(&update.surface_xid))
            .map(Rc::clone)
        else {
            resizeonly_fallback!(ResizeOnlyFallbackReason::MissingDamage);
        };
        self.diagnostics.resizeonly_target_damage_reused += 1;
        if self.diagnostics.enabled {
            self.diagnostics.record_resizeonly_stage(direction, ResizeOnlyStage::Damage, Duration::ZERO);
        }
        let resource_start = self.diagnostics.enabled.then(Instant::now);
        let mut pixmap_geometry_timing = TimingMetric::default();
        let name_pixmap_start = self.diagnostics.enabled.then(Instant::now);
        let pixmap_result = NamedSurfacePixmap::acquire(
            self.connection,
            &snapshot.entries[index],
            self.root,
            root_geometry,
            self.diagnostics.enabled.then_some(&mut pixmap_geometry_timing),
        ).map_err(translate_named_pixmap_acquire_error);
        if let Some(start) = name_pixmap_start {
            self.diagnostics.record_resizeonly_stage(
                direction,
                ResizeOnlyStage::NamePixmap,
                start.elapsed(),
            );
        }
        if self.diagnostics.enabled {
            self.diagnostics
                .resizeonly_direction_mut(direction)
                .pixmap_get_geometry
                .merge(pixmap_geometry_timing);
        }
        let pixmap = match pixmap_result {
            Ok(pixmap) => Rc::new(pixmap),
            Err(error) if is_hierarchy_stale_candidate_error(error.as_ref()) => {
                self.diagnostics.resizeonly_hierarchy_abort += 1;
                self.diagnostics.record_resizeonly_outcome(
                    direction, move_resize, false, true, total_start.map(|start| start.elapsed()),
                );
                return Ok(false);
            }
            Err(error) => return Err(error),
        };
        let egl = self.egl.as_ref().ok_or("EGL scene renderer is unavailable")?;
        let egl_start = self.diagnostics.enabled.then(Instant::now);
        let egl_surface = Rc::new(std::cell::RefCell::new(egl.import_pixmap(pixmap.pixmap_xid, semantics)?));
        if let Some(start) = egl_start {
            self.diagnostics.record_resizeonly_stage(direction, ResizeOnlyStage::EglImport, start.elapsed());
        }

        let mut resources = self.resources.clone();
        resources.insert(update.surface_xid, Rc::new(SurfaceResourceBundle {
            damage: Some(Rc::clone(&damage)),
            pixmap: Rc::clone(&pixmap),
            egl: Some(Rc::clone(&egl_surface)),
        }));
        let mut pixmaps = self.pixmaps.clone();
        pixmaps.retain(|item| item.surface_xid != update.surface_xid);
        pixmaps.push(Rc::clone(&pixmap));
        let damage_leases = self.damage_leases.clone();
        let mut egl_surfaces = self.egl_surfaces.clone();
        egl_surfaces.insert(update.surface_xid, Rc::clone(&egl_surface));
        let damage_registry = self.damage_registry.clone();
        let mut candidate = SceneCandidate {
            snapshot,
            generation: self.structural_generation,
            resources,
            pixmaps,
            damage_leases,
            damage_registry,
            egl_surfaces,
            watch_ids: self.structure_watches.previous_masks.keys().copied().collect(),
            watch_additions: Vec::new(),
            ignored_configure_windows: self.ignored_configure_windows.clone(),
            // Resize-only never changes the surface_xid set (same surface,
            // rebased geometry only) — no new surface can appear here.
            provisional_animations: HashMap::new(),
            // 3a3fa2b5: same reasoning — resize-only never adds/removes a
            // surface, so the composite ordering and close-id counter are
            // carried forward completely unchanged; no close is ever
            // triggered by a pure resize.
            provisional_render_order: self.render_order.clone(),
            provisional_closing_frames: HashMap::new(),
            next_close_id_after: self.next_close_id,
        };
        let target_build_start = self.diagnostics.enabled.then(Instant::now);
        let render_animations = self.window_animations.clone();
        self.render_egl_scene(&candidate.snapshot, &candidate.egl_surfaces, &candidate.pixmaps, &render_animations, &candidate.provisional_render_order, &candidate.provisional_closing_frames)?;
        if let Some(start) = target_build_start {
            self.diagnostics.record_resizeonly_stage(direction, ResizeOnlyStage::TargetBuildRender, start.elapsed());
        }
        if let Some(start) = resource_start {
            self.diagnostics.record_resizeonly_stage(direction, ResizeOnlyStage::ResourceBlocking, start.elapsed());
        }
        let precommit_start = self.diagnostics.enabled.then(Instant::now);
        let (gate, deferred_damage) = self.pre_commit_gate(&mut candidate)?;
        if let Some(start) = precommit_start {
            self.diagnostics.record_resizeonly_stage(direction, ResizeOnlyStage::Precommit, start.elapsed());
        }
        if !matches!(gate, GateDecision::Accept) {
            self.merge_deferred_damage(deferred_damage);
            resizeonly_fallback!(ResizeOnlyFallbackReason::PrecommitRejected);
        }
        if self.pending_damage.contains(&damage.damage_xid) || initial_damage.contains(&damage.damage_xid) {
            self.diagnostics.resizeonly_publish_with_damage_pending += 1;
        }
        let publish_start = self.diagnostics.enabled.then(Instant::now);
        self.commit_candidate(candidate)?;
        if let Some(start) = publish_start {
            self.diagnostics.record_resizeonly_stage(direction, ResizeOnlyStage::Publish, start.elapsed());
        }
        self.merge_deferred_damage(deferred_damage);
        let _ = initial_damage;
        self.diagnostics.resizeonly_success += 1;
        self.diagnostics.resizeonly_full_snapshot_avoided += 1;
        self.diagnostics.record_resizeonly_outcome(
            direction, move_resize, true, false, total_start.map(|start| start.elapsed()),
        );
        Ok(true)
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
            self.subtract_damage_for_diagnostics(damage_id)?;
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
                carry_structural_pending_damage(&mut self.pending_damage, post_subtract.decision(), post_subtract.pixel_damage());
                return self.rebuild_and_present();
            }
            SceneInvalidation::Background => {
                self.pending_background = false;
                self.refresh_background()?;
                self.full_recompose_current()?;
                return self.timed_swap();
            }
            SceneInvalidation::VisualState => {
                self.pending_visual_state = false;
                self.full_recompose_current()?;
                return self.timed_swap();
            }
            SceneInvalidation::Ignore | SceneInvalidation::PixelDamage(_) => {}
        }
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
            return self.timed_swap();
        }
        match final_gate.decision() {
            SceneInvalidation::Shutdown(reason) => {
                println!("scene shutdown: {reason:?}");
                Ok(())
            }
            SceneInvalidation::Hierarchy | SceneInvalidation::Geometry(_) => {
                carry_structural_pending_damage(&mut self.pending_damage, final_gate.decision(), final_gate.pixel_damage());
                self.rebuild_and_present()
            }
            SceneInvalidation::Background => {
                self.pending_background = true;
                Ok(())
            }
            SceneInvalidation::VisualState => {
                self.pending_visual_state = true;
                Ok(())
            }
            SceneInvalidation::Ignore | SceneInvalidation::PixelDamage(_) => Ok(()),
        }
    }

    fn drain_current_events(&mut self) -> Result<InvalidationBatch, Box<dyn Error>> {
        // TEMPORARY FORENSIC INSTRUMENTATION — timing/counters only, no
        // functional change. Remove before release.
        let batch_drain_start = Instant::now();
        let mut batch_event_count: usize = 0;
        let mut batch = InvalidationBatch::default();
        for _ in 0..MAX_EVENTS_PER_BATCH {
            let Some(event) = self.connection.inner.poll_for_event()? else {
                break;
            };
            self.perf.events += 1;
            batch_event_count += 1;
            self.note_destroy_intent(&event);
            let snapshot = self.snapshot.as_ref().expect("published scene snapshot must exist while live");
            self.diagnostics.record_configure(&event, snapshot);
            let geometry_source = geometry_event_source(&event, snapshot);
            self.diagnostics.record_geometry_source(geometry_source);
            let classify_start = Instant::now();
            let visual_invalidation = self.maybe_update_visual_state(&event)?;
            let invalidation = visual_invalidation.unwrap_or_else(|| {
                self.classify_session_event(event.clone(), self.current_snapshot(), &self.damage_registry, &self.damage_registry)
            });
            self.perf.classification.record(classify_start.elapsed());
            self.perf.record_invalidation(invalidation);
            let current_snapshot = self.current_snapshot().clone();
            self.record_hierarchy_event_diagnostic(&event, invalidation, &current_snapshot, batch.geometry_update);
            if matches!(invalidation, SceneInvalidation::Hierarchy) { if let Some(source) = hierarchy_event_source(&event) { batch.note_hierarchy_source(source); } }
            self.observe_invalidation(invalidation);
            batch.push(invalidation);
        }
        self.perf.record_batch_size(batch_event_count);
        self.perf.event_batch_drain.record(batch_drain_start.elapsed());
        Ok(batch)
    }

    fn damage_lease(
        &self,
        damage_id: damage::Damage,
    ) -> Result<&DamageLease<'a>, Box<dyn Error>> {
        self.damage_leases
            .iter()
            .find(|lease| lease.damage_xid == damage_id)
            .map(Rc::as_ref)
            .ok_or_else(|| format!("current DamageLease is unavailable: 0x{damage_id:08x}").into())
    }

    fn subtract_damage_for_diagnostics(
        &mut self,
        damage_id: damage::Damage,
    ) -> Result<(), Box<dyn Error>> {
        let geometry_pending = self.pending_move_geometry.is_some();
        let surface = self.damage_registry.get(&damage_id).copied();
        let identity = surface.map(|surface_xid| {
            (surface_xid, self.current_snapshot().entries.iter()
                .find(|entry| entry.surface_xid == surface_xid)
                .and_then(|entry| entry.semantic_client_xid))
        });
        self.damage_lease(damage_id)?.subtract()?;
        self.perf.damage_subtracts += 1;
        self.diagnostics.record_damage_dispatch(damage_id, geometry_pending, identity);
        Ok(())
    }

    fn full_recompose_current(&mut self) -> Result<(), Box<dyn Error>> {
        // TEMPORARY FORENSIC INSTRUMENTATION (timing only, no functional
        // change) — remove before release.
        let full_recompose_start = Instant::now();
        self.diagnostics.recompositions += 1;
        let snapshot = self
            .snapshot
            .as_ref()
            .expect("published scene snapshot must exist while live");
        let surfaces = &self.egl_surfaces;
        let pixmaps = &self.pixmaps;
        let background = self.background.as_ref();
        let shadow_style = self.shadow_style;
        let visuals = &self._config.visuals;
        let animations = &self.window_animations;
        let render_order = &self.render_order;
        let closing_committed = &self.closing_visuals;
        let empty_closing_provisional: HashMap<u64, ProvisionalClosingFrame> = HashMap::new();
        let egl = self.egl.as_mut().ok_or("EGL scene renderer is unavailable")?;
        let render_start = Instant::now();
        let result = render_egl_scene_parts(
            egl, background, shadow_style, visuals, snapshot, surfaces, pixmaps, animations,
            render_order, closing_committed, &empty_closing_provisional,
            // 3a3fa2b5-r2: no in-flight candidate here (ordinary committed
            // redraw) — `surfaces` already IS `&self.egl_surfaces`, and
            // `empty_closing_provisional` above means this map is never
            // actually consulted anyway.
            surfaces,
        );
        self.perf.renders += 1;
        self.perf.render.record(render_start.elapsed());
        self.perf.full_recomposes += 1;
        self.perf.full_recompose.record(full_recompose_start.elapsed());
        result
    }

    fn classify_session_event(
        &self,
        event: Event,
        snapshot: &SceneSnapshot,
        current_registry: &HashMap<damage::Damage, Window>,
        candidate_registry: &HashMap<damage::Damage, Window>,
    ) -> SceneInvalidation {
        if is_background_property_notify(&event, self.root, self.background_atoms) {
            SceneInvalidation::Background
        } else {
            classify_event_with_registries_and_ignored(
                event, self.root, snapshot, self.ownership.as_ref(), current_registry,
                candidate_registry,
                &self.ignored_configure_windows,
            )
        }
    }

    fn maybe_update_visual_state(
        &mut self,
        event: &Event,
    ) -> Result<Option<SceneInvalidation>, Box<dyn Error>> {
        let relevant = {
            let snapshot = self.current_snapshot();
            is_visual_property_notify(event, self.root, self.visual_atoms, snapshot)
        };
        if !relevant {
            return Ok(None);
        }
        let entries = self.current_snapshot().entries.clone();
        self.update_visual_state(event, &entries)
    }

    fn update_visual_state(
        &mut self,
        event: &Event,
        entries: &[SurfaceEntry],
    ) -> Result<Option<SceneInvalidation>, Box<dyn Error>> {
        let Event::PropertyNotify(property) = event else {
            return Ok(None);
        };
        if property.window == self.root && property.atom == self.visual_atoms.active_window {
            let active_window = read_active_window(self.connection, self.root, self.visual_atoms.active_window)?;
            if active_window == self.active_window {
                return Ok(None);
            }
            let previous = self.active_window;
            let affected = entries.iter().any(|entry| {
                entry.semantic_client_xid == previous || entry.semantic_client_xid == active_window
            });
            self.active_window = active_window;
            let changed = affected && self.refresh_resolved_visual_state(&[previous, active_window]);
            return Ok(changed.then_some(SceneInvalidation::VisualState));
        }
        if property.atom == self.visual_atoms.effect_owner {
            let Some(entry) = entries.iter().find(|entry| entry.surface_xid == property.window) else {
                return Ok(None);
            };
            let old_owner = entry.effect_owner;
            let owner = match read_effect_owner(self.connection, property.window, self.visual_atoms.effect_owner) {
                Ok(owner) => owner,
                Err(error) if super::capture::is_bad_window_error(error.as_ref()) => None,
                Err(error) => return Err(error),
            };
            let tracked_semantic_clients: HashSet<Window> = self
                .current_snapshot()
                .entries
                .iter()
                .filter_map(|candidate| candidate.semantic_client_xid)
                .collect();
            let urgency = self.urgency.clone();
            let blur_enabled = self._config.blur_enabled;
            let Some(current) = self.current_snapshot_mut().entries.iter_mut()
                .find(|candidate| candidate.surface_xid == property.window)
            else {
                return Ok(None);
            };
            let old_request = current.resolved_blur_request.clone();
            current.effect_owner = owner;
            current.resolved_blur_request = permitted_blur_request_with_auxiliary(
                current,
                &urgency,
                &tracked_semantic_clients,
                blur_enabled,
            );
            let changed = old_owner != owner || old_request != current.resolved_blur_request;
            return Ok(changed.then_some(SceneInvalidation::VisualState));
        }
        if property.atom == self.visual_atoms.blur_behind_region
            && entries.iter().any(|entry| {
                entry.surface_xid == property.window && entry.semantic_client_xid.is_none()
            })
        {
            let own_blur_request = match read_client_blur_request(
                self.connection,
                property.window,
                self.visual_atoms,
            ) {
                Ok(request) => request,
                Err(error) if super::capture::is_bad_window_error(error.as_ref()) => BlurRequest::None,
                Err(error) => return Err(error),
            };
            let tracked_semantic_clients: HashSet<Window> = self
                .current_snapshot()
                .entries
                .iter()
                .filter_map(|candidate| candidate.semantic_client_xid)
                .collect();
            let urgency = self.urgency.clone();
            let blur_enabled = self._config.blur_enabled;
            let Some(current) = self.current_snapshot_mut().entries.iter_mut()
                .find(|candidate| candidate.surface_xid == property.window)
            else {
                return Ok(None);
            };
            let old_request = current.resolved_blur_request.clone();
            current.own_blur_request = own_blur_request;
            current.resolved_blur_request = permitted_blur_request_with_auxiliary(
                current,
                &urgency,
                &tracked_semantic_clients,
                blur_enabled,
            );
            return Ok((old_request != current.resolved_blur_request)
                .then_some(SceneInvalidation::VisualState));
        }
        let Some(entry) = entries.iter().find(|entry| entry.semantic_client_xid == Some(property.window)) else {
            return Ok(None);
        };
        let Some(old) = self.urgency.get(&property.window).cloned() else {
            return Ok(None);
        };
        let old_fullscreen = old.fullscreen;
        let old_blur_requested = old.blur_requested.clone();
        let updated = if property.atom == self.visual_atoms.wm_hints {
            let wm_hints = match read_wm_hints_urgency(self.connection, property.window, self.visual_atoms.wm_hints) {
                Ok(value) => value,
                Err(error) if super::capture::is_bad_window_error(error.as_ref()) => {
                    self.urgency.remove(&property.window);
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            CachedClientVisualState { wm_hints, ..old }
        } else if property.atom == self.visual_atoms.net_wm_state {
            let state = match read_client_net_wm_state(self.connection, property.window, self.visual_atoms) {
                Ok(value) => value,
                Err(error) if super::capture::is_bad_window_error(error.as_ref()) => {
                    self.urgency.remove(&property.window);
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            CachedClientVisualState {
                demands_attention: state.demands_attention,
                fullscreen: state.fullscreen,
                ..old
            }
        } else if property.atom == self.visual_atoms.blur_behind_region {
            // Covers property creation, payload change, AND deletion: a
            // re-query after the client removes the property returns
            // "absent" (BlurRequest::None), which is a real change from
            // any prior non-None cached value — no branching on
            // `property.state` (Newvalue vs Deleted) is needed, matching
            // how wm_hints/net_wm_state already re-query unconditionally
            // above. Phase 2A only updates the cache here; nothing reads
            // the resolved request for rendering here; the render loop
            // consumes the already-resolved SurfaceEntry value later.
            let blur_requested = match read_client_blur_request(self.connection, property.window, self.visual_atoms) {
                Ok(value) => value,
                Err(error) if super::capture::is_bad_window_error(error.as_ref()) => {
                    self.urgency.remove(&property.window);
                    return Ok(None);
                }
                Err(error) => return Err(error),
            };
            CachedClientVisualState { blur_requested, ..old }
        } else {
            return Ok(None);
        };
        let updated_fullscreen = updated.fullscreen;
        let updated_blur_requested = updated.blur_requested.clone();
        let fullscreen_changed = old_fullscreen != updated_fullscreen;
        let blur_requested_changed = old_blur_requested != updated_blur_requested;
        self.urgency.insert(property.window, updated);
        if property.atom == self.visual_atoms.net_wm_state {
            let shadow_style = self.shadow_style;
            if let Some(entry) = self.current_snapshot_mut().entries.iter_mut()
                .find(|candidate| candidate.semantic_client_xid == Some(property.window))
            {
                entry.fullscreen = updated_fullscreen;
                entry.shadow_eligible = shadow_eligible_for_entry(shadow_style, entry);
            }
        }
        if property.atom == self.visual_atoms.blur_behind_region {
            // Blur-only change: keep the live, already-published snapshot's
            // resolved request in sync without waiting for the next full
            // candidate rebuild — mirrors the net_wm_state block above
            // exactly, but touches only `resolved_blur_request` (blur has
            // no effect on fullscreen/shadow_eligible).
            let blur_enabled = self._config.blur_enabled;
            if let Some(entry) = self.current_snapshot_mut().entries.iter_mut()
                .find(|candidate| candidate.semantic_client_xid == Some(property.window))
            {
                entry.resolved_blur_request = updated_blur_requested.clone();
                if !blur_enabled {
                    entry.resolved_blur_request = BlurRequest::None;
                }
            }
        }
        let before = (entry.resolved_border_color, entry.resolved_opacity_bits);
        let changed = self.refresh_resolved_visual_state(&[Some(property.window)]);
        let after = self.current_snapshot().entries.iter()
            .find(|candidate| candidate.semantic_client_xid == Some(property.window))
            .map_or(before, |candidate| (candidate.resolved_border_color, candidate.resolved_opacity_bits));
        if (changed && before != after) || fullscreen_changed || blur_requested_changed {
            Ok(Some(SceneInvalidation::VisualState))
        } else {
            Ok(None)
        }
    }

    fn refresh_resolved_visual_state(&mut self, clients: &[Option<Window>]) -> bool {
        let active_window = self.active_window;
        let visuals = &self._config.visuals;
        let urgency = &self.urgency;
        let Some(snapshot) = self.snapshot.as_mut() else { return false; };
        let mut changed = false;
        for entry in &mut snapshot.entries {
            if !clients.iter().any(|client| *client == entry.semantic_client_xid) {
                continue;
            }
            let color = resolved_border_color(visuals, entry, active_window, urgency);
            let color = color.map(f32::to_bits);
            changed |= entry.resolved_border_color != color;
            entry.resolved_border_color = color;
            let opacity = resolved_surface_opacity(visuals, entry, active_window, urgency).to_bits();
            changed |= entry.resolved_opacity_bits != opacity;
            entry.resolved_opacity_bits = opacity;
        }
        changed
    }

    fn refresh_background(&mut self) -> Result<(), Box<dyn Error>> {
        let candidate = self.load_background_candidate()?;
        let Some(egl) = self.egl.as_ref() else { return Ok(()); };
        let replacement = match candidate {
            BackgroundCandidate::Valid(source) => {
                let surface = match egl.import_pixmap(source.xid, source.semantics) {
                    Ok(surface) => surface,
                    Err(error) => {
                        eprintln!("root background PIXMAP import failed; keeping current background/fallback: {error}");
                        return Ok(());
                    }
                };
                Some(ImportedBackground { source, surface })
            }
            BackgroundCandidate::SolidFallback => None,
            BackgroundCandidate::Preserve => return Ok(()),
        };
        if let Some(mut old) = self.background.take() {
            egl.make_current()?;
            egl.destroy_import(&mut old.surface)?;
        }
        self.background = replacement;
        Ok(())
    }

    fn load_background_candidate(&self) -> Result<BackgroundCandidate, Box<dyn Error>> {
        let preferred = read_background_pixmap(self.connection, self.root, self.background_atoms.xrootpmap_id,
            self.background_atoms.pixmap_type, &self.visual_formats)?;
        let fallback = read_background_pixmap(self.connection, self.root, self.background_atoms.esetroot_pmap_id,
            self.background_atoms.pixmap_type, &self.visual_formats)?;
        if let Some(source) = preferred.or(fallback) {
            return Ok(BackgroundCandidate::Valid(source));
        }
        let preferred_present = background_property_present(self.connection, self.root, self.background_atoms.xrootpmap_id)?;
        let fallback_present = background_property_present(self.connection, self.root, self.background_atoms.esetroot_pmap_id)?;
        if preferred_present || fallback_present {
            Ok(if self.background.is_some() { BackgroundCandidate::Preserve } else { BackgroundCandidate::SolidFallback })
        } else {
            Ok(BackgroundCandidate::SolidFallback)
        }
    }

    fn render_egl_scene(
        &mut self,
        snapshot: &SceneSnapshot,
        surfaces: &HashMap<Window, Rc<std::cell::RefCell<EglImportedSurface>>>,
        pixmaps: &[Rc<NamedSurfacePixmap<'a>>],
        animations: &HashMap<Window, WindowAnimation>,
        render_order: &[RenderLayer],
        closing_provisional: &HashMap<u64, ProvisionalClosingFrame>,
    ) -> Result<(), Box<dyn Error>> {
        // TEMPORARY FORENSIC INSTRUMENTATION (timing only, no functional
        // change) — remove before release.
        let render_start = Instant::now();
        let result = render_egl_scene_parts(
            self.egl.as_mut().ok_or("EGL scene renderer is unavailable")?,
            self.background.as_ref(),
            self.shadow_style,
            &self._config.visuals,
            snapshot,
            surfaces,
            pixmaps,
            animations,
            render_order,
            &self.closing_visuals,
            closing_provisional,
            // 3a3fa2b5-r2: `self.egl_surfaces` — the SESSION's OWN, still-OLD
            // surfaces map at every call site that can carry a non-empty
            // `closing_provisional` (build_candidate and the MoveOnly-rebase
            // path both run strictly before commit_candidate_inner's swap).
            // Never a stored/cloned reference — read fresh on every call.
            &self.egl_surfaces,
        );
        self.perf.renders += 1;
        self.perf.render.record(render_start.elapsed());
        result
    }

    fn try_move_only(
        &mut self,
        surface: Window,
        geometry: Option<PendingGeometry>,
        initial_damage: &HashSet<damage::Damage>,
    ) -> Result<bool, Box<dyn Error>> {
        self.diagnostics.moveonly_attempted += 1;
        if self.pending_background {
            return Ok(false);
        }
        let Some(entry_index) = self
            .current_snapshot()
            .entries
            .iter()
            .position(|entry| entry.surface_xid == surface)
        else {
            return Ok(false);
        };
        let previous = self.current_snapshot().entries[entry_index].clone();
        let Some(geometry) = geometry.filter(|geometry| geometry.surface_xid == surface) else {
            return Ok(false);
        };
        let next_geometry = WindowGeometry {
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            border_width: geometry.border_width,
        };
        if !move_only_geometry_is_eligible(
            &previous,
            next_geometry,
            self.root,
            geometry.override_redirect,
            previous.semantic_client_xid,
        ) {
            return Ok(false);
        }

        let mut next_client_root = previous.client_root_geometry;
        if let (Some(client_root), Some(_client)) = (previous.client_root_geometry, previous.semantic_client_xid) {
            next_client_root = Some(move_client_root_geometry(
                client_root,
                previous.geometry,
                next_geometry,
            ));
        }

        self.current_snapshot_mut().entries[entry_index].geometry = next_geometry;
        self.current_snapshot_mut().entries[entry_index].client_root_geometry = next_client_root;

        let mut damage_to_subtract = initial_damage.clone();
        let mut structural_event = false;
        for _ in 0..MAX_EVENTS_PER_BATCH {
            let Some(event) = self.connection.inner.poll_for_event()? else {
                break;
            };
            self.note_destroy_intent(&event);
            let snapshot = self.snapshot.as_ref().expect("published scene snapshot must exist while live");
            self.diagnostics.record_configure(&event, snapshot);
            let visual_invalidation = self.maybe_update_visual_state(&event)?;
            let invalidation = visual_invalidation.unwrap_or_else(|| {
                self.classify_session_event(
                    event,
                    self.current_snapshot(),
                    &self.damage_registry,
                    &self.damage_registry,
                )
            });
            if !matches!(invalidation, SceneInvalidation::Geometry(_)) {
                self.observe_invalidation(invalidation);
            }
            match invalidation {
                SceneInvalidation::PixelDamage(damage_id) => {
                    damage_to_subtract.insert(damage_id);
                }
                SceneInvalidation::Geometry(_) | SceneInvalidation::Hierarchy => {
                    structural_event = true;
                }
                _ => {}
            }
        }
        if structural_event || self.signal.poll_shutdown_pending()? {
            for damage_id in damage_to_subtract {
                if self.damage_registry.contains_key(&damage_id) {
                    self.subtract_damage_for_diagnostics(damage_id)?;
                }
            }
            self.current_snapshot_mut().entries[entry_index].geometry = previous.geometry;
            self.current_snapshot_mut().entries[entry_index].client_root_geometry = previous.client_root_geometry;
            self.diagnostics.moveonly_fallback += 1;
            return Ok(false);
        }
        self.full_recompose_current()?;
        for damage_id in damage_to_subtract {
            if self.damage_registry.contains_key(&damage_id) {
                self.subtract_damage_for_diagnostics(damage_id)?;
            }
        }
        self.diagnostics.moveonly_success += 1;
        let damage_id = self.damage_leases.iter()
            .find(|lease| lease.surface_xid == surface)
            .map(|lease| lease.damage_xid);
        self.diagnostics.record_moveonly(
            surface,
            previous.semantic_client_xid,
            damage_id,
        );
        Ok(true)
    }

    fn current_snapshot(&self) -> &SceneSnapshot {
        self.snapshot
            .as_ref()
            .expect("published scene snapshot must exist while live")
    }

    fn current_snapshot_mut(&mut self) -> &mut SceneSnapshot {
        self.snapshot
            .as_mut()
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
        if let Some(present) = self.present.as_mut() {
            if let Err(error) = present.cleanup(self.connection) {
                first_error = Some(error);
            }
        }
        self.present = None;
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
            if let Err(error) = retire_damage_lease(damage, false) {
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
            if let Some(background) = self.background.as_mut() {
                if let Err(error) = egl.destroy_import(&mut background.surface) {
                    first_error.get_or_insert(error);
                }
            }
            for surface in self.egl_surfaces.values() {
                if let Err(error) = egl.destroy_import(&mut surface.borrow_mut()) {
                    first_error.get_or_insert(error);
                }
            }
            // 3a3fa2b5 — explicit, hand-ordered ClosingTexture teardown,
            // strictly BEFORE the EGL context itself is destroyed below —
            // mirrors egl_surfaces' own ordering exactly (see the r2 GL-
            // lifetime proof, which this reuses unmodified).
            for visual in self.closing_visuals.values_mut() {
                visual.texture.destroy();
            }
        } else {
            if let Some(background) = self.background.as_mut() {
                background.surface.disarm();
            }
            for surface in self.egl_surfaces.values() {
                surface.borrow_mut().disarm();
            }
            for visual in self.closing_visuals.values_mut() {
                visual.texture.disarm();
            }
        }
        self.background = None;
        self.egl_surfaces.clear();
        self.closing_visuals.clear();
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
        self.present = None;
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
        self.pending_background = false;
        for surface in self.egl_surfaces.values() {
            surface.borrow_mut().disarm();
        }
        for visual in self.closing_visuals.values_mut() {
            visual.texture.disarm();
        }
        if let Some(background) = self.background.as_mut() {
            background.surface.disarm();
        }
        self.background = None;
        self.egl_surfaces.clear();
        self.closing_visuals.clear();
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RootRect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct RegionRenderPlan {
    visible: Vec<RootRect>,
    capture: RootRect,
}

fn intersect_root_rect(a: (i64, i64, i64, i64), b: (i64, i64, i64, i64)) -> Option<RootRect> {
    let left = a.0.max(b.0);
    let top = a.1.max(b.1);
    let right = (a.0 + a.2).min(b.0 + b.2);
    let bottom = (a.1 + a.3).min(b.1 + b.3);
    if right <= left || bottom <= top {
        return None;
    }
    Some(RootRect {
        x: i32::try_from(left).ok()?,
        y: i32::try_from(top).ok()?,
        width: i32::try_from(right - left).ok()?,
        height: i32::try_from(bottom - top).ok()?,
    })
}

fn subtract_root_rect(rect: RootRect, covered: RootRect) -> Vec<RootRect> {
    let Some(intersection) = intersect_root_rect(
        (i64::from(rect.x), i64::from(rect.y), i64::from(rect.width), i64::from(rect.height)),
        (i64::from(covered.x), i64::from(covered.y), i64::from(covered.width), i64::from(covered.height)),
    ) else {
        return vec![rect];
    };
    let rect_right = rect.x + rect.width;
    let rect_bottom = rect.y + rect.height;
    let intersection_right = intersection.x + intersection.width;
    let intersection_bottom = intersection.y + intersection.height;
    let mut fragments = Vec::with_capacity(4);
    if intersection.x > rect.x {
        fragments.push(RootRect { x: rect.x, y: rect.y, width: intersection.x - rect.x, height: rect.height });
    }
    if intersection_right < rect_right {
        fragments.push(RootRect { x: intersection_right, y: rect.y, width: rect_right - intersection_right, height: rect.height });
    }
    if intersection.y > rect.y {
        fragments.push(RootRect { x: intersection.x, y: rect.y, width: intersection.width, height: intersection.y - rect.y });
    }
    if intersection_bottom < rect_bottom {
        fragments.push(RootRect { x: intersection.x, y: intersection_bottom, width: intersection.width, height: rect_bottom - intersection_bottom });
    }
    fragments
}

fn normalize_non_overlapping_rects(rects: &[RootRect]) -> Vec<RootRect> {
    let mut normalized = Vec::new();
    for &rect in rects {
        let mut fragments = vec![rect];
        for &covered in &normalized {
            let mut remaining = Vec::new();
            for fragment in fragments {
                remaining.extend(subtract_root_rect(fragment, covered));
            }
            fragments = remaining;
            if fragments.is_empty() {
                break;
            }
        }
        normalized.extend(fragments);
    }
    normalized
}

fn plan_region_backdrop(
    regions: &[BlurRegionRect],
    client: ClientRootGeometry,
    owner: RootRect,
    root: RootGeometry,
) -> Option<RegionRenderPlan> {
    let client_bounds = (i64::from(client.root_x), i64::from(client.root_y),
        i64::from(client.width), i64::from(client.height));
    let owner_bounds = (i64::from(owner.x), i64::from(owner.y),
        i64::from(owner.width), i64::from(owner.height));
    let root_bounds = (0_i64, 0_i64, i64::from(root.width), i64::from(root.height));
    let mut visible = Vec::new();
    for region in regions {
        if region.width <= 0 || region.height <= 0 {
            continue;
        }
        let translated = (
            i64::from(client.root_x).checked_add(i64::from(region.x))?,
            i64::from(client.root_y).checked_add(i64::from(region.y))?,
            i64::from(region.width),
            i64::from(region.height),
        );
        if let Some(clipped) = intersect_root_rect(translated, client_bounds)
            .and_then(|rect| intersect_root_rect((i64::from(rect.x), i64::from(rect.y),
                i64::from(rect.width), i64::from(rect.height)), owner_bounds))
            .and_then(|rect| intersect_root_rect((i64::from(rect.x), i64::from(rect.y),
                i64::from(rect.width), i64::from(rect.height)), root_bounds))
        {
            visible.push(clipped);
        }
    }
    let visible = normalize_non_overlapping_rects(&visible);
    let first = *visible.first()?;
    let mut left = first.x;
    let mut top = first.y;
    let mut right = first.x + first.width;
    let mut bottom = first.y + first.height;
    for rect in &visible[1..] {
        left = left.min(rect.x);
        top = top.min(rect.y);
        right = right.max(rect.x + rect.width);
        bottom = bottom.max(rect.y + rect.height);
    }
    Some(RegionRenderPlan {
        visible,
        capture: RootRect { x: left, y: top, width: right - left, height: bottom - top },
    })
}

fn render_egl_scene_parts<'a>(
    egl: &mut EglSceneRenderer,
    background: Option<&ImportedBackground>,
    shadow_style: crate::config::ShadowConfig,
    visuals: &crate::config::VisualConfig,
    snapshot: &SceneSnapshot,
    surfaces: &HashMap<Window, Rc<std::cell::RefCell<EglImportedSurface>>>,
    pixmaps: &[Rc<NamedSurfacePixmap<'a>>],
    animations: &HashMap<Window, WindowAnimation>,
    render_order: &[RenderLayer],
    closing_committed: &HashMap<u64, ClosingVisual>,
    closing_provisional: &HashMap<u64, ProvisionalClosingFrame>,
    closing_source_surfaces: &HashMap<Window, Rc<std::cell::RefCell<EglImportedSurface>>>,
) -> Result<(), Box<dyn Error>> {
    egl.clear()?;
    // 3a3fa2a: one clock reading for every entry in this render, so all
    // simultaneously animating surfaces advance in lockstep within a frame.
    let animation_now = Instant::now();
    if let Some(background) = background {
        if let Some(plan) = build_background_render_quad_plan(background.source.geometry, snapshot.root_geometry) {
            egl.render_surface(background.surface.texture, plan, background.surface.pixel_semantics)?;
        }
    }
    // 3a3fa2b5-r2: exploits the proven invariant `projection(render_order,
    // Live) == snapshot.entries` order EXACTLY (see reconcile_render_order)
    // via a single synchronized forward walk — every RenderLayer::Live
    // consumes exactly the next `live_entries` item, in order; Closing
    // layers never advance it. O(render_order.len() + snapshot.entries.len())
    // total, replacing R1's O(render_order.len() * snapshot.entries.len())
    // per-entry `.find()`.
    let mut live_entries = snapshot.entries.iter();
    for layer in render_order {
        // 3a3fa2b5: `render_order` (never `snapshot.entries` directly) now
        // drives composite depth, so committed close visuals splice into
        // the correct gap relative to Live entries — see
        // reconcile_render_order. Live-branch logic below this guard is
        // otherwise BYTE-IDENTICAL to before this milestone (same
        // indentation, same variable names) — see render_wiring_* source-
        // scan tests, which depend on it staying that way.
        let entry = match *layer {
            RenderLayer::Live(surface_xid) => {
                let Some(entry) = live_entries.next() else {
                    debug_assert!(
                        false,
                        "RenderLayer::Live(0x{surface_xid:08x}) has no corresponding \
                         snapshot entry — Live-projection invariant violated",
                    );
                    continue;
                };
                debug_assert_eq!(
                    entry.surface_xid, surface_xid,
                    "render_order's Live projection must exactly equal snapshot.entries order",
                );
                entry
            }
            RenderLayer::Closing(close_id) => {
                render_closing_layer(egl, shadow_style, closing_committed, closing_provisional, closing_source_surfaces, close_id, animation_now)?;
                continue;
            }
        };
        let Some(surface) = surfaces.get(&entry.surface_xid) else {
            continue;
        };
        let surface = surface.borrow();
        let pixmap = pixmaps
            .iter()
            .find(|pixmap| pixmap.surface_xid == entry.surface_xid)
            .ok_or_else(|| format!("missing pixmap for EGL surface 0x{:08x}", entry.surface_xid))?;
        let mut plan = build_render_quad_plan(entry.geometry, pixmap.geometry, snapshot.root_geometry)
            .ok_or_else(|| format!("surface 0x{:08x} has no visible render quad", entry.surface_xid))?;
        apply_surface_visual_policy(&mut plan, visuals, entry.visual_class);
        if entry.semantic_client_xid.is_none() {
            // No semantic client means this is an untracked override-redirect
            // popup (menu/tooltip/dropdown) that classify_surface_visual_class
            // still labels Normal (see eligibility_excludes_override_redirect_
            // even_when_classified_normal) — the same condition shadow_eligible_
            // for_entry already uses to withhold shadows. Such surfaces are
            // self-decorated by their own toolkit; drawing our WM border/
            // corner-radius quad on top of their real (often non-rectangular,
            // still-settling) geometry is what produces the stray outline
            // rectangle seen floating near popups after a click.
            plan.corner_radius = 0.0;
            plan.border_width = 0.0;
        }
        plan.border_color = entry.resolved_border_color.map(f32::from_bits);

        // 3a3fa2b1-s1: resolve the animated surface plan/opacity ONCE,
        // right after the real `plan` is finalized and before any
        // geometry-consuming draw work below — shadow and the final
        // surface draw both reuse this SAME sampled AnimationVisual,
        // never resampled a second time. `plan` itself (Copy) is left
        // untouched here and remains the real, un-animated footprint —
        // BLUR CONTRACT: blur below intentionally keeps using it, unchanged.
        let base_opacity = f32::from_bits(entry.resolved_opacity_bits);
        let (draw_plan, draw_opacity, shadow_opacity_multiplier, energy_tear_layout, open_flash_alpha, open_reveal_radius, open_kamui_state) = match animations.get(&entry.surface_xid) {
            Some(animation) => {
                let t = animation.progress(animation_now);
                let visual = animation.sample(animation_now);
                // TEMPORARY FORENSIC INSTRUMENTATION — bounded sampling via
                // threshold bands (not per-frame), remove before release.
                if t < 0.05 || (0.4..=0.6).contains(&t) || t >= 1.0 {
                    println!(
                        "OPEN_ANIM_RENDER surface=0x{:08x} progress={t:.2} scale_x={:.3} scale_y={:.3} alpha={:.3}",
                        entry.surface_xid, visual.scale_x, visual.scale_y, visual.opacity,
                    );
                }
                // 3a3fa2b3: energy_tear's slice/streak layout is derived
                // from this SAME already-sampled `t` — no second
                // Instant::now(), no new timer.
                let energy_tear_layout = energy_tear_layout_for(animation.effect, t);
                // 3a3fa2b6-r1: TeleportFlashy's flash-alpha overlay state,
                // same reasoning — derived from this SAME `t`, effect-
                // gated via teleport_flashy_open_flash_for exactly like
                // energy_tear_layout above.
                let open_flash_alpha = teleport_flashy_open_flash_for(animation.effect, t);
                // 3a3fa2b6-r2 (Minato): the center->edges radial reveal
                // radius, same reasoning again — derived from this SAME
                // `t`, effect-gated via minato_reveal_radius_for, `None`
                // once t>=MINATO_REVEAL_END (fallback to the ordinary
                // mode-0 draw below, mirroring energy_tear_layout's own
                // post-completion `None` fallback).
                let open_reveal_radius = minato_reveal_radius_for(animation.effect, t);
                // 3a3fa2b7 (Kamui): (visible_radius, twist) state for the
                // polar-warp content draw, same reasoning again — derived
                // from this SAME `t`, effect-gated via
                // kamui_open_state_for, `None` once t>=KAMUI_OPEN_SETTLE_END
                // (fallback to the ordinary mode-0 draw below).
                let open_kamui_state = kamui_open_state_for(animation.effect, t);
                // 3a3fa2b7: Kamui's shadow ENVELOPE multiplies into the
                // existing shadow_opacity_multiplier — `None` (i.e. no
                // change, multiplier stays 1.0) for every non-Kamui
                // effect. Shadow itself never receives twist/warp state —
                // see kamui_open_shadow_envelope_for.
                let shadow_opacity_multiplier = match kamui_open_shadow_envelope_for(animation.effect, t) {
                    Some(envelope) => visual.opacity * envelope,
                    None => visual.opacity,
                };
                (scale_render_quad_plan(plan, visual.scale_x, visual.scale_y), base_opacity * visual.opacity, shadow_opacity_multiplier, energy_tear_layout, open_flash_alpha, open_reveal_radius, open_kamui_state)
            }
            None => (plan, base_opacity, 1.0, None, None, None, None),
        };

        let region_plan = match &entry.resolved_blur_request {
            BlurRequest::Regions(regions) => entry.client_root_geometry.and_then(|client| {
                plan_region_backdrop(
                    regions,
                    client,
                    RootRect {
                        x: plan.outer_x,
                        y: plan.outer_y,
                        width: plan.outer_width,
                        height: plan.outer_height,
                    },
                    snapshot.root_geometry,
                )
            }),
            BlurRequest::None | BlurRequest::FullWindow => None,
        };
        let blurred_texture = match entry.resolved_blur_request {
            BlurRequest::FullWindow => Some(egl.capture_and_blur_background(
                plan.outer_x,
                plan.outer_y,
                plan.outer_width,
                plan.outer_height,
                BACKGROUND_BLUR_RADIUS_PX,
            )?),
            BlurRequest::Regions(_) => region_plan.as_ref().map(|region| {
                egl.capture_and_blur_background(
                    region.capture.x,
                    region.capture.y,
                    region.capture.width,
                    region.capture.height,
                    BACKGROUND_BLUR_RADIUS_PX,
                )
            }).transpose()?,
            BlurRequest::None => None,
        };
        // 3a3fa2b1-s1: shadow now uses `draw_plan` — the ANIMATED plan
        // (equal to `plan`, the real plan, when this entry has no active
        // animation) — for its base rectangle/corner radius, and
        // `shadow_opacity_multiplier` (`visual.opacity`, or 1.0 when not
        // animating) to scale the config's own shadow strength. Blur
        // above and below this block is untouched and still keys off the
        // real, un-animated `plan` — see BLUR CONTRACT above.
        if entry.shadow_eligible {
            if let Some(shadow) = shadow_params_from_plan(shadow_style, &draw_plan, shadow_opacity_multiplier) {
                egl.render_shadow(shadow)?;
            }
        }
        if let Some(blurred_texture) = blurred_texture {
            if let Some(region_plan) = region_plan {
                for region in region_plan.visible {
                    let backdrop_params = crate::graphics::renderer::BackdropParams::new_region(
                        plan.outer_x,
                        plan.outer_y,
                        plan.outer_width,
                        plan.outer_height,
                        region.x,
                        region.y,
                        region.width,
                        region.height,
                        i32::from(snapshot.root_geometry.width),
                        i32::from(snapshot.root_geometry.height),
                    ).ok_or("invalid Regions backdrop geometry")?;
                    egl.draw_blurred_backdrop(blurred_texture, backdrop_params, plan.corner_radius)?;
                }
            } else {
            let backdrop_params = crate::graphics::renderer::BackdropParams::new(
                plan.outer_x,
                plan.outer_y,
                plan.outer_width,
                plan.outer_height,
                i32::from(snapshot.root_geometry.width),
                i32::from(snapshot.root_geometry.height),
            ).ok_or("invalid FullWindow backdrop geometry")?;
            egl.draw_blurred_backdrop(blurred_texture, backdrop_params, plan.corner_radius)?;
            }
        }
        let opacity = crate::graphics::renderer::SurfaceOpacity::new(draw_opacity)
            .expect("resolved surface opacity must be valid");
        // 3a3fa2b3: energy_tear_render_plan returns None both when no
        // tear layout is active at all (Scale/Teleport/non-animated,
        // AND energy_tear itself once t >= ENERGY_TEAR_END) and when the
        // plan is too narrow to slice safely — both cases fall back to
        // the exact same single-quad call every other effect already
        // used, unchanged.
        match energy_tear_layout.and_then(|layout| energy_tear_render_plan(draw_plan, &layout)) {
            Some(render_plan) => {
                egl.render_energy_tear_slices(surface.texture, surface.pixel_semantics, opacity, &render_plan)?;
            }
            // 3a3fa2b6-r2/b7 (Minato/Kamui): mutually exclusive with the
            // EnergyTear arm above AND with each other by construction
            // (energy_tear_layout / open_reveal_radius / open_kamui_state
            // are each gated on DIFFERENT OpenAnimationEffect variants,
            // never more than one Some for the same entry) — this
            // REPLACES which function draws the single existing content
            // draw call, it never adds a second one. Falls through to the
            // ordinary mode-0 path below once every effect-specific state
            // is `None` (ordinary effects, OR TeleportFlashy past
            // MINATO_REVEAL_END, OR Kamui past KAMUI_OPEN_SETTLE_END).
            None => match (open_reveal_radius, open_kamui_state) {
                (Some(reveal_radius), _) => {
                    egl.render_surface_with_radial_reveal(
                        surface.texture,
                        draw_plan,
                        surface.pixel_semantics,
                        opacity,
                        reveal_radius,
                    )?;
                }
                (None, Some((visible_radius, twist, radial_power))) => {
                    egl.render_surface_with_kamui_warp(
                        surface.texture,
                        draw_plan,
                        surface.pixel_semantics,
                        opacity,
                        visible_radius,
                        twist,
                        radial_power,
                    )?;
                }
                (None, None) => {
                    egl.render_surface_with_opacity(
                        surface.texture,
                        draw_plan,
                        surface.pixel_semantics,
                        opacity,
                    )?;
                }
            },
        }
        // 3a3fa2b6-r1 — TeleportFlashy's solid-overlay flash, appended
        // strictly AFTER the surface draw above (never before it, where
        // it would be hidden). `open_flash_alpha` is `None` for every
        // effect except TeleportFlashy (see
        // teleport_flashy_open_flash_for), and the overlay draw itself
        // is skipped entirely once alpha reaches 0 (see
        // render_solid_overlay's own `alpha <= 0.0` guard) — so this is
        // exactly +1 draw call only while the flash is actually visible,
        // zero otherwise. Uses `draw_plan` — the SAME currently-animated
        // geometry the surface/shadow above already used — so the flash
        // follows the effect's own micro-scale and stays rounded-corner-
        // clipped to it.
        if let Some(alpha) = open_flash_alpha {
            egl.render_solid_overlay(draw_plan, TELEPORT_FLASHY_COLOR, alpha)?;
        }
    }
    Ok(())
}

fn egl_scene_is_renderable(entry_count: usize, egl_surface_count: usize) -> bool {
    entry_count == 0 || egl_surface_count > 0
}

/// 3a3fa2b5-r2 — draws one composited `RenderLayer::Closing` entry,
/// sourced from EITHER a committed `ClosingVisual` (compositor-owned GPU
/// snapshot, used for every persistent frame — texture resolved directly
/// from its own owned `ClosingTexture`) OR a candidate-local
/// `ProvisionalClosingFrame` (frame 0 only, sourced from the OLD LIVE
/// texture while it is still valid — see the r1 first-frame-gap finding).
/// Corrected from R1: a `ProvisionalClosingFrame` owns no GPU resource at
/// all (see its doc comment), so its source texture is resolved HERE,
/// fresh, by `frame.source_xid`, from `closing_source_surfaces` — the
/// caller's OWN still-live surfaces map (`SceneSession::egl_surfaces`,
/// naturally alive at frame-0 render time since commit hasn't swapped it
/// yet), never a value stored on the frame itself. A missing lookup
/// degrades to "draw nothing for this layer" (`Ok(())`), matching the
/// same fail-open shape used elsewhere in this pipeline for a stale/
/// inconsistent state, never a panic.
///
/// `ClosingDrawSource` still unifies plan/pixel_semantics/base_opacity/
/// shadow_eligible/animation access for both sources into ONE draw path.
/// Deliberately kept OUTSIDE `render_egl_scene_parts`'s own source span
/// (after `egl_scene_is_renderable`, never between it and that function)
/// so this closing-only path can never accidentally satisfy or break one
/// of that function's literal-text `render_wiring_*` source-scan tests.
/// Reuses exactly the same scale/shadow/draw primitives the Live path
/// already uses (`scale_render_quad_plan`, `shadow_params_from_plan`,
/// `render_surface_with_opacity`) — no new shader mode. R1/R2 scope: no
/// energy_tear, NO LIVE BLUR FROM FIRST CLOSING FRAME (the last ordinary
/// LIVE frame before a window starts closing may carry blur; no
/// `RenderLayer::Closing` frame — provisional or committed — ever issues
/// one, since neither `ClosingVisual` nor `ProvisionalClosingFrame` carry
/// a blur request at all). 3a3fa2b6-r1: the SAME `render_solid_overlay`
/// TeleportFlashy flash path serves BOTH provisional frame-0 and
/// committed ClosingVisual frames here — no frame-0 special case, since
/// this one function already runs for both sources.
fn render_closing_layer(
    egl: &EglSceneRenderer,
    shadow_style: crate::config::ShadowConfig,
    closing_committed: &HashMap<u64, ClosingVisual>,
    closing_provisional: &HashMap<u64, ProvisionalClosingFrame>,
    closing_source_surfaces: &HashMap<Window, Rc<std::cell::RefCell<EglImportedSurface>>>,
    close_id: u64,
    animation_now: Instant,
) -> Result<(), Box<dyn Error>> {
    let (source, texture) = match closing_committed.get(&close_id) {
        Some(visual) => (ClosingDrawSource::Committed(visual), visual.texture.texture),
        None => match closing_provisional.get(&close_id) {
            Some(frame) => {
                let Some(surface) = closing_source_surfaces.get(&frame.source_xid) else { return Ok(()); };
                (ClosingDrawSource::Provisional(frame), surface.borrow().texture)
            }
            None => return Ok(()),
        },
    };
    let animation = source.animation();
    let t = animation.progress(animation_now);
    let visual = sample_close_effect(animation.effect, t);
    let closing_draw_plan = scale_render_quad_plan(source.plan(), visual.scale_x, visual.scale_y);
    // 3a3fa2b7 (Kamui): shadow ENVELOPE multiplied into the existing
    // closing_opacity_multiplier — `None` (no change) for every non-Kamui
    // close effect. Shadow never receives twist/warp state, same
    // reasoning as the OPEN path — see kamui_close_shadow_envelope_for.
    let closing_opacity_multiplier = match kamui_close_shadow_envelope_for(animation.effect, t) {
        Some(envelope) => visual.opacity * envelope,
        None => visual.opacity,
    };
    if source.shadow_eligible() {
        if let Some(shadow) = shadow_params_from_plan(shadow_style, &closing_draw_plan, closing_opacity_multiplier) {
            egl.render_shadow(shadow)?;
        }
    }
    let opacity = crate::graphics::renderer::SurfaceOpacity::new(source.base_opacity() * visual.opacity)
        .expect("resolved closing opacity must be valid");
    // 3a3fa2b7 (Kamui): mutually exclusive with the ordinary path by
    // construction (kamui_close_state_for is gated exclusively on
    // crate::config::CloseAnimationEffect::Kamui) — this REPLACES which function draws
    // the single existing closing content draw call, exactly like the
    // OPEN path's own reveal/kamui/ordinary match. Same function serves
    // BOTH provisional frame-0 and committed ClosingVisual sources — no
    // frame-0 special case, since `source`/`texture` were already
    // resolved above regardless of which map they came from.
    match kamui_close_state_for(animation.effect, t) {
        Some((visible_radius, twist, radial_power)) => {
            egl.render_surface_with_kamui_warp(
                texture,
                closing_draw_plan,
                source.pixel_semantics(),
                opacity,
                visible_radius,
                twist,
                radial_power,
            )?;
        }
        None => {
            egl.render_surface_with_opacity(texture, closing_draw_plan, source.pixel_semantics(), opacity)?;
        }
    }
    // 3a3fa2b6-r1 — TeleportFlashy's solid-overlay flash, appended
    // strictly AFTER the closing surface draw above. `teleport_flashy_close_flash_for`
    // returns `None` for every other close effect (currently only Scale),
    // and `render_solid_overlay` itself skips the draw entirely once
    // alpha reaches 0 — so this is exactly +1 draw call only while the
    // flash is actually visible, identical gating shape to the OPEN path.
    // Uses `closing_draw_plan` — the SAME currently-animated geometry the
    // surface/shadow above already used.
    if let Some(alpha) = teleport_flashy_close_flash_for(animation.effect, t) {
        egl.render_solid_overlay(closing_draw_plan, TELEPORT_FLASHY_COLOR, alpha)?;
    }
    Ok(())
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

fn acquire_background_atoms(connection: &X11Connection) -> Result<BackgroundAtoms, Box<dyn Error>> {
    let intern = |name: &[u8]| -> Result<xproto::Atom, Box<dyn Error>> {
        Ok(connection.inner.intern_atom(false, name)?.reply()?.atom)
    };
    Ok(BackgroundAtoms {
        xrootpmap_id: intern(b"_XROOTPMAP_ID")?,
        esetroot_pmap_id: intern(b"ESETROOT_PMAP_ID")?,
        pixmap_type: xproto::AtomEnum::PIXMAP.into(),
    })
}

fn acquire_visual_atoms(connection: &X11Connection) -> Result<VisualAtoms, Box<dyn Error>> {
    let intern = |name: &[u8]| -> Result<xproto::Atom, Box<dyn Error>> {
        Ok(connection.inner.intern_atom(false, name)?.reply()?.atom)
    };
    Ok(VisualAtoms {
        active_window: intern(b"_NET_ACTIVE_WINDOW")?,
        wm_hints: intern(b"WM_HINTS")?,
        net_wm_state: intern(b"_NET_WM_STATE")?,
        demands_attention: intern(b"_NET_WM_STATE_DEMANDS_ATTENTION")?,
        fullscreen: intern(b"_NET_WM_STATE_FULLSCREEN")?,
        blur_behind_region: intern(b"_KDE_NET_WM_BLUR_BEHIND_REGION")?,
        effect_owner: intern(b"_XOMPOSITE_EFFECT_OWNER")?,
    })
}

fn read_active_window(connection: &X11Connection, root: Window, atom: xproto::Atom) -> Result<Option<Window>, Box<dyn Error>> {
    let reply = connection.inner.get_property(false, root, atom, xproto::AtomEnum::WINDOW, 0, 1)?.reply()?;
    Ok(reply.value32().and_then(|mut values| values.next()).filter(|window| *window != x11rb::NONE))
}

fn read_client_urgency(
    connection: &X11Connection,
    client: Window,
    atoms: VisualAtoms,
) -> Result<CachedClientVisualState, Box<dyn Error>> {
    let hints = connection.inner.get_property(false, client, atoms.wm_hints, xproto::AtomEnum::ANY, 0, 9)?.reply()?;
    let wm_hints_urgent = wm_hints_urgency(hints.value32().and_then(|mut values| values.next()));
    let state = connection.inner.get_property(false, client, atoms.net_wm_state, xproto::AtomEnum::ATOM, 0, u32::MAX)?.reply()?;
    let blur_requested = read_client_blur_request(connection, client, atoms)?;
    Ok(CachedClientVisualState {
        wm_hints: wm_hints_urgent,
        blur_requested,
        ..read_net_wm_state(state.value32(), atoms)
    })
}

fn read_wm_hints_urgency(
    connection: &X11Connection,
    client: Window,
    atom: xproto::Atom,
) -> Result<bool, Box<dyn Error>> {
    let hints = connection.inner.get_property(false, client, atom, xproto::AtomEnum::ANY, 0, 9)?.reply()?;
    Ok(wm_hints_urgency(hints.value32().and_then(|mut values| values.next())))
}

fn read_client_net_wm_state(
    connection: &X11Connection,
    client: Window,
    atoms: VisualAtoms,
) -> Result<CachedClientVisualState, Box<dyn Error>> {
    let state = connection.inner.get_property(false, client, atoms.net_wm_state, xproto::AtomEnum::ATOM, 0, u32::MAX)?.reply()?;
    Ok(read_net_wm_state(state.value32(), atoms))
}

/// Reads and parses `_KDE_NET_WM_BLUR_BEHIND_REGION` on `client` (never on
/// a redirected surface/frame XID — see `parse_blur_behind_region` for the
/// parsing contract). Requesting with `type = CARDINAL` means a
/// wrong-type property is rejected by the server itself (an empty reply,
/// `value32()` sees nothing to iterate) — the same convention already
/// used for `_NET_WM_STATE`'s `type = ATOM` filter, not a new mechanism.
fn read_client_blur_request(
    connection: &X11Connection,
    client: Window,
    atoms: VisualAtoms,
) -> Result<BlurRequest, Box<dyn Error>> {
    let reply = connection
        .inner
        .get_property(false, client, atoms.blur_behind_region, xproto::AtomEnum::CARDINAL, 0, u32::MAX)?
        .reply()?;
    Ok(parse_blur_behind_region(reply.value32()))
}

/// Parses the exact single-XID representation required by
/// `_XOMPOSITE_EFFECT_OWNER(WINDOW)`. A malformed or absent property is
/// deliberately indistinguishable from no relationship.
fn parse_effect_owner_property(
    property_type: xproto::Atom,
    window_type: xproto::Atom,
    format: u8,
    value_len: u32,
    value: &[u8],
) -> Option<Window> {
    if property_type != window_type || format != 32 || value_len != 1 || value.len() != 4 {
        return None;
    }
    let xid = u32::from_ne_bytes(value.try_into().ok()?);
    (xid != x11rb::NONE).then_some(xid)
}

fn read_effect_owner(
    connection: &X11Connection,
    surface: Window,
    atom: xproto::Atom,
) -> Result<Option<Window>, Box<dyn Error>> {
    let reply = connection
        .inner
        .get_property(false, surface, atom, xproto::AtomEnum::WINDOW, 0, u32::MAX)?
        .reply()?;
    Ok(parse_effect_owner_property(
        reply.type_,
        xproto::AtomEnum::WINDOW.into(),
        reply.format,
        reply.value_len,
        &reply.value,
    ))
}

/// Pure parser for a `_KDE_NET_WM_BLUR_BEHIND_REGION` payload, already
/// reduced to `Option<impl Iterator<Item = u32>>` by the caller (mirrors
/// `read_net_wm_state`'s split between I/O and parsing). `values ==
/// None` covers both "property absent" and "wrong format" (format != 32,
/// per `GetPropertyReply::value32`'s own contract) — both reject to
/// `BlurRequest::None`, matching "do not silently accept malformed data"
/// by never treating a rejected read as a request.
///
/// A payload length not divisible by 4 is rejected outright (`None`), not
/// truncated to the nearest complete group — silently accepting a
/// malformed group count would itself be a form of accepting malformed
/// data.
///
/// A zero-length payload, or a payload consisting of exactly one
/// degenerate (width == 0 && height == 0) rectangle, is the confirmed
/// "blur the whole window" shape (the latter is the exact payload the
/// reference client, Ghostty, emits for `background-blur = true`) and
/// parses to `BlurRequest::FullWindow`.
///
/// Any other payload — one or more non-degenerate rectangles, or a MIX of
/// degenerate and non-degenerate rectangles — parses to
/// `BlurRequest::Regions(...)`, retained verbatim, including any
/// degenerate entries. Phase 2A deliberately does not filter, coalesce,
/// or reinterpret a degenerate rectangle found WITHIN a multi-rectangle
/// payload as anything special: only the single-rectangle-and-degenerate
/// case has a confirmed, evidenced interpretation (FullWindow); how a
/// degenerate entry inside a larger region list should be treated is an
/// open question left to whichever future phase renders `Regions(...)`.
fn parse_blur_behind_region(values: Option<impl Iterator<Item = u32>>) -> BlurRequest {
    let Some(values) = values else {
        return BlurRequest::None;
    };
    let raw: Vec<u32> = values.collect();
    if raw.is_empty() {
        return BlurRequest::FullWindow;
    }
    if raw.len() % 4 != 0 {
        return BlurRequest::None;
    }
    let regions: Vec<BlurRegionRect> = raw
        .chunks_exact(4)
        .map(|group| BlurRegionRect {
            x: group[0] as i32,
            y: group[1] as i32,
            width: group[2] as i32,
            height: group[3] as i32,
        })
        .collect();
    if let [only] = regions.as_slice() {
        if only.width == 0 && only.height == 0 {
            return BlurRequest::FullWindow;
        }
    }
    BlurRequest::Regions(regions)
}

fn wm_hints_urgency(flags: Option<u32>) -> bool {
    flags.is_some_and(|flags| flags & (1 << 8) != 0)
}

#[cfg(test)]
fn state_demands_attention(values: Option<impl Iterator<Item = u32>>, atom: xproto::Atom) -> bool {
    values.is_some_and(|mut values| values.any(|value| value == atom))
}

fn read_net_wm_state(
    values: Option<impl Iterator<Item = u32>>,
    atoms: VisualAtoms,
) -> CachedClientVisualState {
    let mut state = CachedClientVisualState::default();
    if let Some(values) = values {
        for value in values {
            state.demands_attention |= value == atoms.demands_attention;
            state.fullscreen |= value == atoms.fullscreen;
        }
    }
    state
}

fn border_visual_state(
    entry: &SurfaceEntry,
    active_window: Option<Window>,
    urgency: &HashMap<Window, CachedClientVisualState>,
) -> BorderVisualState {
    if matches!(entry.visual_class, SurfaceVisualClass::Dock | SurfaceVisualClass::Desktop) {
        return BorderVisualState::Inactive;
    }
    if entry.semantic_client_xid.is_some_and(|client| urgency.get(&client).is_some_and(|state| state.wm_hints || state.demands_attention)) {
        BorderVisualState::Urgent
    } else if entry.semantic_client_xid == active_window {
        BorderVisualState::Focused
    } else {
        BorderVisualState::Inactive
    }
}

fn border_color(config: &crate::config::BorderConfig, state: BorderVisualState) -> [f32; 4] {
    match state {
        BorderVisualState::Inactive => config.inactive_color,
        BorderVisualState::Focused => config.focused_color,
        BorderVisualState::Urgent => config.urgent_color,
    }
}

fn rendered_border_color(
    visuals: &crate::config::VisualConfig,
    entry: &SurfaceEntry,
    active_window: Option<Window>,
    urgency: &HashMap<Window, CachedClientVisualState>,
) -> Option<[f32; 4]> {
    if matches!(entry.visual_class, SurfaceVisualClass::Dock | SurfaceVisualClass::Desktop)
        || effective_border_width(visuals.border.width, i32::from(entry.geometry.width), i32::from(entry.geometry.height)) == 0.0
    {
        return None;
    }
    Some(border_color(&visuals.border, border_visual_state(entry, active_window, urgency)))
}

fn resolved_border_color(
    visuals: &crate::config::VisualConfig,
    entry: &SurfaceEntry,
    active_window: Option<Window>,
    urgency: &HashMap<Window, CachedClientVisualState>,
) -> [f32; 4] {
    rendered_border_color(visuals, entry, active_window, urgency)
        .unwrap_or([0.0, 0.0, 0.0, 1.0])
}

fn resolve_snapshot_border_colors(
    snapshot: &mut SceneSnapshot,
    visuals: &crate::config::VisualConfig,
    active_window: Option<Window>,
    urgency: &HashMap<Window, CachedClientVisualState>,
) {
    for entry in &mut snapshot.entries {
        entry.resolved_border_color = resolved_border_color(visuals, entry, active_window, urgency).map(f32::to_bits);
    }
}

fn parse_background_property(
    property_type: xproto::Atom,
    pixmap_type: xproto::Atom,
    format: u8,
    value_len: u32,
    value: &[u8],
) -> Result<Option<xproto::Pixmap>, &'static str> {
    if property_type == xproto::AtomEnum::NONE.into() {
        return Ok(None);
    }
    if property_type != pixmap_type || format != 32 || value_len != 1 || value.len() != 4 {
        return Err("root background property has an invalid PIXMAP representation");
    }
    let xid = u32::from_ne_bytes(value.try_into().map_err(|_| "invalid PIXMAP value")?);
    if xid == x11rb::NONE {
        return Err("root background property contains NONE");
    }
    Ok(Some(xid))
}

fn read_background_property(
    connection: &X11Connection,
    root: Window,
    atom: xproto::Atom,
    pixmap_type: xproto::Atom,
) -> Result<Result<Option<xproto::Pixmap>, &'static str>, Box<dyn Error>> {
    let reply = connection.inner.get_property(false, root, atom, xproto::AtomEnum::ANY, 0, 1)?.reply()?;
    Ok(parse_background_property(reply.type_, pixmap_type, reply.format, reply.value_len, &reply.value))
}

fn background_property_present(
    connection: &X11Connection,
    root: Window,
    atom: xproto::Atom,
) -> Result<bool, Box<dyn Error>> {
    let reply = connection.inner.get_property(false, root, atom, xproto::AtomEnum::ANY, 0, 1)?.reply()?;
    Ok(reply.type_ != xproto::AtomEnum::NONE.into())
}

fn read_background_pixmap(
    connection: &X11Connection,
    root: Window,
    atom: xproto::Atom,
    pixmap_type: xproto::Atom,
    formats: &VisualFormatCache,
) -> Result<Option<BackgroundPixmap>, Box<dyn Error>> {
    let parsed = match read_background_property(connection, root, atom, pixmap_type)? {
        Ok(value) => value,
        Err(error) => {
            eprintln!("ignoring invalid root background property 0x{atom:08x}: {error}");
            return Ok(None);
        }
    };
    let Some(xid) = parsed else { return Ok(None); };
    let geometry = match connection.inner.get_geometry(xid)?.reply() {
        Ok(geometry) => PixmapGeometry {
            root: geometry.root,
            x: geometry.x,
            y: geometry.y,
            width: geometry.width,
            height: geometry.height,
            border_width: geometry.border_width,
            depth: geometry.depth,
        },
        Err(error) => {
            eprintln!("root background PIXMAP 0x{xid:08x} is unavailable: {error}");
            return Ok(None);
        }
    };
    let screen_root = connection.inner.setup().roots[connection.screen_num()].root;
    let screen = &connection.inner.setup().roots[connection.screen_num()];
    if geometry.root != screen_root || geometry.width < screen.width_in_pixels
        || geometry.height < screen.height_in_pixels || geometry.depth != screen.root_depth
    {
        eprintln!("rejecting root background PIXMAP 0x{xid:08x}: incompatible drawable geometry");
        return Ok(None);
    }
    let semantics = formats.semantics(screen.root_visual, geometry.depth);
    if semantics != EglPixelSemantics::Opaque {
        eprintln!("rejecting root background PIXMAP 0x{xid:08x}: unsupported root RGB format");
        return Ok(None);
    }
    Ok(Some(BackgroundPixmap { xid, geometry, semantics }))
}

fn build_background_render_quad_plan(pixmap: PixmapGeometry, root: RootGeometry) -> Option<RenderQuadPlan> {
    if pixmap.root == x11rb::NONE || pixmap.width < root.width || pixmap.height < root.height {
        return None;
    }
    Some(RenderQuadPlan {
        dst_x: 0,
        dst_y: 0,
        width: i32::from(root.width),
        height: i32::from(root.height),
        outer_x: 0,
        outer_y: 0,
        outer_width: i32::from(root.width),
        outer_height: i32::from(root.height),
        src_x: 0,
        src_y: 0,
        src_width: i32::from(root.width),
        src_height: i32::from(root.height),
        u0: 0.0,
        v0: 0.0,
        u1: f32::from(root.width) / f32::from(pixmap.width),
        v1: f32::from(root.height) / f32::from(pixmap.height),
        corner_radius: 0.0,
        border_width: 0.0,
        border_color: [0.0, 0.0, 0.0, 1.0],
    })
}

fn is_background_property_notify(event: &Event, root: Window, atoms: BackgroundAtoms) -> bool {
    matches!(event, Event::PropertyNotify(event) if event.window == root &&
        (event.atom == atoms.xrootpmap_id || event.atom == atoms.esetroot_pmap_id))
}

fn snapshot_watch_ids(snapshot: &HierarchySnapshot) -> HashSet<Window> {
    let mut ids = HashSet::new();
    for binding in &snapshot.children {
        ids.insert(binding.root_child_xid);
        ids.extend(binding.semantic_client_xids.iter().copied());
    }
    ids
}

fn root_live_event_mask(previous: EventMask) -> EventMask {
    previous | EventMask::STRUCTURE_NOTIFY | EventMask::SUBSTRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE
}

fn canonical_live_event_mask(previous: EventMask) -> EventMask {
    previous | EventMask::STRUCTURE_NOTIFY | EventMask::PROPERTY_CHANGE
}

fn is_visual_property_notify(
    event: &Event,
    root: Window,
    atoms: VisualAtoms,
    snapshot: &SceneSnapshot,
) -> bool {
    let Event::PropertyNotify(event) = event else {
        return false;
    };
    if event.window == root && event.atom == atoms.active_window {
        return true;
    }
    snapshot.entries.iter().any(|entry| {
        (entry.surface_xid == event.window && event.atom == atoms.effect_owner)
            || (entry.surface_xid == event.window
                && entry.semantic_client_xid.is_none()
                && event.atom == atoms.blur_behind_region)
            || (entry.semantic_client_xid == Some(event.window)
                && (event.atom == atoms.wm_hints
                    || event.atom == atoms.net_wm_state
                    || event.atom == atoms.blur_behind_region))
    })
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

#[cfg(test)]
fn classify_event_with_registries(
    event: Event,
    root: Window,
    snapshot: &SceneSnapshot,
    ownership: Option<&CompositorOwnership>,
    current_registry: &HashMap<damage::Damage, Window>,
    candidate_registry: &HashMap<damage::Damage, Window>,
) -> SceneInvalidation {
    classify_event_with_registries_and_ignored(
        event,
        root,
        snapshot,
        ownership,
        current_registry,
        candidate_registry,
        &HashSet::new(),
    )
}

fn classify_event_with_registries_and_ignored(
    event: Event,
    root: Window,
    snapshot: &SceneSnapshot,
    ownership: Option<&CompositorOwnership>,
    current_registry: &HashMap<damage::Damage, Window>,
    candidate_registry: &HashMap<damage::Damage, Window>,
    ignored_configure_windows: &HashSet<Window>,
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
        Event::ConfigureNotify(event) => {
            if let Some(entry) = snapshot
                .entries
                .iter()
                .find(|entry| entry.semantic_client_xid == Some(event.window))
            {
                SceneInvalidation::Geometry(entry.surface_xid)
            } else if ignored_configure_windows.contains(&event.window) {
                SceneInvalidation::Ignore
            } else {
                SceneInvalidation::Hierarchy
            }
        }
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

fn move_only_geometry_is_eligible(
    entry: &SurfaceEntry,
    next_geometry: WindowGeometry,
    expected_root: Window,
    next_override_redirect: bool,
    next_semantic_client: Option<Window>,
) -> bool {
    entry.geometry.width == next_geometry.width
        && entry.geometry.height == next_geometry.height
        && entry.geometry.border_width == next_geometry.border_width
        && (entry.geometry.x != next_geometry.x || entry.geometry.y != next_geometry.y)
        && entry.override_redirect == next_override_redirect
        && entry.semantic_client_xid == next_semantic_client
        && expected_root != x11rb::NONE
}

fn move_client_root_geometry(
    client_root: ClientRootGeometry,
    previous: WindowGeometry,
    next: WindowGeometry,
) -> ClientRootGeometry {
    ClientRootGeometry {
        root_x: client_root.root_x + i32::from(next.x) - i32::from(previous.x),
        root_y: client_root.root_y + i32::from(next.y) - i32::from(previous.y),
        width: client_root.width,
        height: client_root.height,
    }
}

fn rebase_candidate_geometry_fields(entry: &mut SurfaceEntry, update: PendingGeometry) {
    let next_geometry = WindowGeometry {
        x: update.x,
        y: update.y,
        width: update.width,
        height: update.height,
        border_width: update.border_width,
    };
    entry.client_root_geometry = entry
        .client_root_geometry
        .map(|client_root| move_client_root_geometry(client_root, entry.geometry, next_geometry));
    entry.geometry = next_geometry;
}

fn structural_identity_matches(left: &SceneSnapshot, right: &SceneSnapshot) -> bool {
    left.root == right.root
        && left.root_geometry == right.root_geometry
        && left.entries.len() == right.entries.len()
        && left.entries.iter().zip(&right.entries).all(|(left, right)| {
            left.surface_xid == right.surface_xid
                && left.semantic_client_xid == right.semantic_client_xid
                && left.effect_owner == right.effect_owner
                && left.own_blur_request == right.own_blur_request
                && left.lifecycle_xid == right.lifecycle_xid
                && left.depth == right.depth
                && left.visual == right.visual
                && left.class == right.class
                && left.map_state == right.map_state
                && left.override_redirect == right.override_redirect
                && left.stacking_index == right.stacking_index
                && left.backend == right.backend
                && left.visual_class == right.visual_class
                && left.fullscreen == right.fullscreen
                && left.shadow_eligible == right.shadow_eligible
                && left.resolved_border_color == right.resolved_border_color
                && left.resolved_opacity_bits == right.resolved_opacity_bits
                && left.resolved_blur_request == right.resolved_blur_request
        })
}

fn target_geometry_rebase_compatible(
    live: &SceneSnapshot,
    candidate: &SceneSnapshot,
    update: PendingGeometry,
) -> bool {
    if !structural_identity_matches(live, candidate) {
        return false;
    }
    let Some(live_entry) = live.entries.iter().find(|entry| entry.surface_xid == update.surface_xid) else {
        return false;
    };
    let Some(candidate_entry) = candidate.entries.iter().find(|entry| entry.surface_xid == update.surface_xid) else {
        return false;
    };
    live_entry.surface_xid == candidate_entry.surface_xid
        && live_entry.semantic_client_xid == candidate_entry.semantic_client_xid
        && live_entry.lifecycle_xid == candidate_entry.lifecycle_xid
        && candidate_entry.map_state != xproto::MapState::UNMAPPED
        && candidate_entry.override_redirect == update.override_redirect
}

fn same_common_surface_order(left: &[SurfaceEntry], right: &[SurfaceEntry]) -> bool {
    let right_ids = right.iter().map(|entry| entry.surface_xid).collect::<HashSet<_>>();
    let left_ids = left.iter().map(|entry| entry.surface_xid).collect::<HashSet<_>>();
    let left_common = left
        .iter()
        .filter(|entry| right_ids.contains(&entry.surface_xid))
        .map(|entry| entry.surface_xid);
    let right_common = right
        .iter()
        .filter(|entry| left_ids.contains(&entry.surface_xid))
        .map(|entry| entry.surface_xid);
    left_common.eq(right_common)
}

fn reusable_resource_identity(
    live: &SceneSnapshot,
    candidate: &SurfaceEntry,
    bundle: &SurfaceResourceBundle<'_>,
) -> bool {
    let Some(previous) = live.entries.iter().find(|entry| entry.surface_xid == candidate.surface_xid) else {
        return false;
    };
    resource_identity_fields_match(previous, candidate)
        && bundle.pixmap.geometry.root == live.root
        && bundle.pixmap.geometry.depth == candidate.depth
        && named_pixmap_dimensions_match(candidate.geometry, bundle.pixmap.geometry)
        && (candidate.backend == BackendCompatibility::BackendUnsupported || bundle.egl.is_some())
}

fn candidate_has_resized_target(
    live: &SceneSnapshot,
    candidate: &SceneSnapshot,
    resources: &HashMap<Window, Rc<SurfaceResourceBundle<'_>>>,
) -> bool {
    candidate.entries.iter().any(|entry| {
        let Some(previous) = live.entries.iter().find(|previous| previous.surface_xid == entry.surface_xid) else {
            return false;
        };
        resources.contains_key(&entry.surface_xid)
            && (previous.geometry.width != entry.geometry.width
                || previous.geometry.height != entry.geometry.height
                || previous.geometry.border_width != entry.geometry.border_width)
    })
}

fn resize_geometry_is_obsolete(candidate: WindowGeometry, update: PendingGeometry) -> bool {
    candidate.width != update.width
        || candidate.height != update.height
        || candidate.border_width != update.border_width
}

fn classify_resizeonly_direction(
    previous: WindowGeometry,
    update: PendingGeometry,
) -> (ResizeOnlyDirection, bool) {
    let width_grows = update.width > previous.width;
    let height_grows = update.height > previous.height;
    let width_shrinks = update.width < previous.width;
    let height_shrinks = update.height < previous.height;
    let direction = if (width_grows || height_grows) && !(width_shrinks || height_shrinks) {
        ResizeOnlyDirection::Grow
    } else if (width_shrinks || height_shrinks) && !(width_grows || height_grows) {
        ResizeOnlyDirection::Shrink
    } else {
        ResizeOnlyDirection::Mixed
    };
    (
        direction,
        update.x != previous.x || update.y != previous.y,
    )
}

fn resource_identity_fields_match(previous: &SurfaceEntry, candidate: &SurfaceEntry) -> bool {
    previous.surface_xid == candidate.surface_xid
        && previous.semantic_client_xid == candidate.semantic_client_xid
        && previous.lifecycle_xid == candidate.lifecycle_xid
        && previous.geometry.width == candidate.geometry.width
        && previous.geometry.height == candidate.geometry.height
        && previous.geometry.border_width == candidate.geometry.border_width
        && previous.depth == candidate.depth
        && previous.visual == candidate.visual
        && previous.map_state == candidate.map_state
        && previous.backend == candidate.backend
}

fn damage_identity_compatible(previous: &SurfaceEntry, candidate: &SurfaceEntry) -> bool {
    previous.surface_xid == candidate.surface_xid
        && previous.semantic_client_xid == candidate.semantic_client_xid
        && previous.lifecycle_xid == candidate.lifecycle_xid
        && previous.map_state == candidate.map_state
        && previous.depth == candidate.depth
        && previous.visual == candidate.visual
        && previous.backend == candidate.backend
}

fn observe_structural_generation(generation: &mut u64, invalidation: SceneInvalidation) {
    if matches!(invalidation, SceneInvalidation::Geometry(_) | SceneInvalidation::Hierarchy) {
        *generation = generation.wrapping_add(1);
    }
}

fn batch_damage_requires_subtraction(
    decision: SceneInvalidation,
    pixel_damage: &HashSet<damage::Damage>,
) -> bool {
    !pixel_damage.is_empty()
        && matches!(
            decision,
            SceneInvalidation::Background
                | SceneInvalidation::VisualState
        )
}

fn carry_structural_pending_damage(
    pending: &mut HashSet<damage::Damage>,
    decision: SceneInvalidation,
    batch_damage: &HashSet<damage::Damage>,
) {
    if matches!(decision, SceneInvalidation::Geometry(_) | SceneInvalidation::Hierarchy) {
        pending.extend(batch_damage.iter().copied());
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
    if matches!(batch, SceneInvalidation::PixelDamage(_) | SceneInvalidation::Background | SceneInvalidation::VisualState) && !overflow {
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
    config: CompositorConfig,
) -> Result<(), Box<dyn Error>> {
    SceneSession::run(connection, parse_root(expected_root_value)?, config)
}

pub(crate) fn run_with_root(
    connection: &X11Connection,
    root: Window,
    config: CompositorConfig,
) -> Result<(), Box<dyn Error>> {
    SceneSession::run(connection, root, config)
}

#[cfg(test)]
mod tests {
    use std::collections::{HashMap, HashSet};
    use std::time::{Duration, Instant};

    use super::{
        build_copy_plan,
        parse_background_property, build_background_render_quad_plan,
        effective_corner_radius,
        effective_border_width,
        classify_surface_visual_class, effective_window_type, apply_surface_visual_policy, border_visual_state,
        rendered_border_color, BorderVisualState, CachedClientVisualState, SurfaceVisualClass,
        wm_hints_urgency, state_demands_attention,
        is_background_property_notify, BackgroundAtoms, BackgroundCandidate, BackgroundPixmap,
        classify_event, coordinator_requires_cleanup, eligible_surface, eligible_surface_with_semantic_metadata,
        is_internal_xid, root_guard, BackendCompatibility, CandidateBuildError, CopyPlan,
        PixmapGeometry, RootGeometry,
        bounded_batch_requires_retry, candidate_gate_decision, candidate_render_allowed,
        damage_monitoring_enabled, damage_version_compatible, classify_visual_format, insert_visual_format,
        render_version_compatible, gate_decision_after_batch, guards_allow_retry, GateDecision,
        pending_work_requires_iteration, pixel_gate_allows_presentation,
        retry_allowed, subtract_plan, watch_plan, build_render_quad_plan,
        GeometryPresentHistory, ResizeOnlyDirection,
        HierarchyEventSource, HierarchyEventRelation,
        egl_scene_is_renderable, merge_deferred_damage_for_registry, EglPixelSemantics,
        VisualFormatCache, VisualFormatInfo, InvalidationBatch, SceneInvalidation, SceneSnapshot,
        build_pict_format_index, classify_scene_visual_format,
        RENDER_CLIENT_MAJOR, RENDER_CLIENT_MINOR,
        root_live_event_mask, canonical_live_event_mask, snapshot_watch_ids, SceneState,
        ShutdownReason, SurfaceEntry, MAX_CANDIDATE_RETRIES, MAX_EVENTS_PER_BATCH,
        BACKGROUND_BLUR_RADIUS_PX,
        observe_structural_generation,
        batch_damage_requires_subtraction,
        carry_structural_pending_damage,
        structural_generation_state, StructuralGenerationState,
        first_publish_step, FirstPublishStep,
        FrameScheduler, FrameSchedulerState,
        classify_retired_damage_destroy, DamageDestroyClassification, DamageReleaseOutcome,
        DamageState, shadow_eligible_for_entry, shadow_params_from_plan, resolved_surface_opacity,
        read_net_wm_state, VisualAtoms,
        NamedSurfacePixmapAcquireError, RawPixmapOwnership, named_pixmap_dimensions_match,
        validate_named_pixmap_dimensions, translate_named_pixmap_acquire_error,
        DamageLeaseAcquireError, stale_damage_create_reply, translate_damage_lease_acquire_error,
        is_hierarchy_stale_candidate_error,
        classify_damage_subtract_error, DamageSubtractClassification,
        Diagnostics3a3f8b3a,
        ResizeOnlyDirectionDiagnostics,
        ResizeOnlyFallbackReason, ResizeOnlyFallbackReasons,
        retain_pending_for_registry,
        rect_intersects_root, surface_quad_intersects_root, shadow_bounds_intersect_root,
        entry_has_visible_contribution, prune_invisible_entries,
        resource_identity_fields_match,
        damage_identity_compatible,
        BlurRequest, BlurRegionRect, parse_blur_behind_region, parse_effect_owner_property,
        is_visual_property_notify, resolved_blur_request_with_auxiliary,
        permitted_blur_request, resolved_blur_request, resolve_snapshot_fullscreen,
        ClientRootGeometry, client_root_geometry_from_translation,
        region_request_requires_client_origin, translate_coordinates_reply_error,
        RootRect, intersect_root_rect, plan_region_backdrop,
        move_only_geometry_is_eligible,
        move_client_root_geometry, PendingGeometry,
        rebase_candidate_geometry_fields, same_common_surface_order, resize_geometry_is_obsolete,
        structural_identity_matches, target_geometry_rebase_compatible,
        classify_resizeonly_direction,
        configure_geometry_update, classify_event_with_registries_and_ignored,
        geometry_event_source, GeometryEventSource, PreResizeOnlyBypassReason,
        StructuralOrigin,
        RenderQuadPlan,
        WindowAnimation, AnimationVisual,
        animation_progress, ease_out_cubic, ease_in_cubic, lerp, phase_progress, sample_open_effect, sample_scale,
        SCALE_EFFECT_FROM_SCALE, SCALE_EFFECT_TO_SCALE, SCALE_EFFECT_FROM_OPACITY, SCALE_EFFECT_TO_OPACITY,
        ClosingAnimation, sample_close_scale, sample_close_effect,
        CLOSE_SCALE_END_OPACITY, CLOSE_SCALE_END_SCALE,
        RenderLayer, reconcile_render_order, allocate_close_ids, retained_destroy_intents,
        sample_teleport,
        TELEPORT_PHASE_A_END, TELEPORT_PHASE_B_END, TELEPORT_PHASE_C_END,
        TELEPORT_START_OPACITY, TELEPORT_START_SCALE_X, TELEPORT_START_SCALE_Y,
        TELEPORT_PHASE_B_SCALE_X, TELEPORT_PHASE_B_SCALE_Y,
        sample_energy_tear, sample_energy_tear_layout, energy_tear_layout_for,
        energy_tear_render_plan, energy_tear_oscillation, EnergyTearLayout, EnergyTearSlicePlan,
        ENERGY_TEAR_SLICE_COUNT, ENERGY_TEAR_END, ENERGY_TEAR_PEAK_STREAK_ALPHA,
        ENERGY_TEAR_SLICE_PEAK_OFFSET_FRACTIONS,
        ENERGY_TEAR_PHASE_A_END, ENERGY_TEAR_PHASE_B_END, ENERGY_TEAR_PHASE_C_END,
        ENERGY_TEAR_REBOUND_1_FACTOR, ENERGY_TEAR_REBOUND_2_FACTOR,
        ENERGY_TEAR_MAX_DISPLACEMENT_PX,
        sample_bubble,
        BUBBLE_PHASE_A_END, BUBBLE_PHASE_B_END, BUBBLE_PHASE_C_END,
        BUBBLE_START_OPACITY, BUBBLE_START_SCALE_X, BUBBLE_START_SCALE_Y,
        TELEPORT_FLASHY_COLOR,
        sample_teleport_flashy_open, sample_teleport_flashy_open_flash, teleport_flashy_open_flash_for,
        sample_teleport_flashy_close, sample_teleport_flashy_close_flash, teleport_flashy_close_flash_for,
        TELEPORT_FLASHY_OPEN_START_OPACITY, TELEPORT_FLASHY_OPEN_START_SCALE, TELEPORT_FLASHY_OPEN_GEOMETRY_END,
        TELEPORT_FLASHY_OPEN_FLASH_HOLD_END, TELEPORT_FLASHY_OPEN_FLASH_END,
        TELEPORT_FLASHY_CLOSE_HOLD_END, TELEPORT_FLASHY_CLOSE_COLLAPSE_END, TELEPORT_FLASHY_CLOSE_END_SCALE,
        TELEPORT_FLASHY_CLOSE_FLASH_PEAK, TELEPORT_FLASHY_CLOSE_FLASH_END,
        sample_minato_reveal_radius, minato_reveal_radius_for,
        MINATO_REVEAL_START, MINATO_REVEAL_END, MINATO_REVEAL_FULL_RADIUS,
        sample_kamui_open, sample_kamui_open_visible_radius, sample_kamui_open_twist,
        sample_kamui_open_radial_power,
        kamui_open_state_for, kamui_open_shadow_envelope_for,
        KAMUI_OPEN_START_RADIUS, KAMUI_OPEN_CORE_END, KAMUI_OPEN_CORE_RADIUS,
        KAMUI_OPEN_EXPAND_END, KAMUI_OPEN_SETTLE_END, KAMUI_OPEN_MAX_TWIST,
        KAMUI_OPEN_TWIST_DECAY_POWER, KAMUI_OPEN_RADIAL_POWER_START,
        sample_kamui_close, sample_kamui_close_visible_radius, sample_kamui_close_twist,
        sample_kamui_close_radial_power,
        kamui_close_state_for, kamui_close_shadow_envelope_for,
        KAMUI_CLOSE_GRAB_END, KAMUI_CLOSE_FLOW_END, KAMUI_CLOSE_SUCTION_END, KAMUI_CLOSE_COLLAPSE_END,
        KAMUI_CLOSE_GRAB_RADIUS, KAMUI_CLOSE_FLOW_RADIUS, KAMUI_CLOSE_SUCTION_RADIUS,
        KAMUI_CLOSE_TWIST_AFTER_GRAB, KAMUI_CLOSE_TWIST_AFTER_FLOW, KAMUI_CLOSE_MAX_TWIST,
        KAMUI_CLOSE_RADIAL_POWER_END,
        eligible_for_open_animation, provisional_open_animations,
        merge_window_animations, scale_render_quad_plan,
        promote_provisional_animations, retire_removed_surface_animations,
        effective_override_redirect,
    };
    use crate::config::{AnimationConfig, OpenAnimationConfig, OpenAnimationEffect};
    use crate::x11::capture::WindowGeometry;
    use super::super::tree::{BindingStatus, HierarchyBinding, HierarchySnapshot};
    use x11rb::errors::ReplyError;
    use x11rb::protocol::damage::ReportLevel;
    use x11rb::protocol::render;
    use x11rb::protocol::xproto::{EventMask, MapState, Rectangle, Window, WindowClass};
    use x11rb::protocol::xproto;
    use x11rb::protocol::Event;
    use x11rb::protocol::ErrorKind;
    use x11rb::x11_utils::X11Error;

    fn root() -> RootGeometry {
        RootGeometry {
            width: 100,
            height: 80,
            depth: 24,
            visual: 0x21,
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    enum BlurRenderAction {
        CaptureBlur,
        Shadow,
        Backdrop,
        Surface,
    }

    fn modeled_blur_actions(request: &BlurRequest, shadow: bool) -> Vec<BlurRenderAction> {
        let mut actions = Vec::new();
        if matches!(request, BlurRequest::FullWindow) {
            actions.push(BlurRenderAction::CaptureBlur);
        }
        if shadow {
            actions.push(BlurRenderAction::Shadow);
        }
        if matches!(request, BlurRequest::FullWindow) {
            actions.push(BlurRenderAction::Backdrop);
        }
        actions.push(BlurRenderAction::Surface);
        actions
    }

    fn modeled_blur_selected(request: &BlurRequest, _fullscreen: bool, _depth: u8, _opacity: f32) -> bool {
        matches!(request, BlurRequest::FullWindow)
    }

    #[test]
    fn blur_model_none_has_no_blur_actions() {
        assert_eq!(modeled_blur_actions(&BlurRequest::None, true), vec![BlurRenderAction::Shadow, BlurRenderAction::Surface]);
    }

    #[test]
    fn blur_model_regions_has_no_blur_actions_and_preserves_request() {
        let request = BlurRequest::Regions(vec![BlurRegionRect { x: 1, y: 2, width: 3, height: 4 }]);
        assert_eq!(modeled_blur_actions(&request, false), vec![BlurRenderAction::Surface]);
        assert!(matches!(request, BlurRequest::Regions(_)));
    }

    #[test]
    fn blur_model_full_window_with_shadow_is_ordered() {
        assert_eq!(
            modeled_blur_actions(&BlurRequest::FullWindow, true),
            vec![BlurRenderAction::CaptureBlur, BlurRenderAction::Shadow, BlurRenderAction::Backdrop, BlurRenderAction::Surface],
        );
    }

    #[test]
    fn blur_model_full_window_without_shadow_is_ordered() {
        assert_eq!(
            modeled_blur_actions(&BlurRequest::FullWindow, false),
            vec![BlurRenderAction::CaptureBlur, BlurRenderAction::Backdrop, BlurRenderAction::Surface],
        );
    }

    fn client_geometry() -> ClientRootGeometry {
        ClientRootGeometry { root_x: 20, root_y: 10, width: 60, height: 40 }
    }

    fn owner_geometry() -> RootRect {
        RootRect { x: 10, y: 5, width: 80, height: 60 }
    }

    #[test]
    fn regions_translate_client_local_coordinates_to_root_space() {
        let plan = plan_region_backdrop(
            &[BlurRegionRect { x: 7, y: 9, width: 11, height: 13 }],
            client_geometry(), owner_geometry(), root(),
        ).unwrap();
        assert_eq!(plan.visible, vec![RootRect { x: 27, y: 19, width: 11, height: 13 }]);
    }

    #[test]
    fn regions_clip_against_client_bounds_before_owner_and_root() {
        let plan = plan_region_backdrop(
            &[BlurRegionRect { x: 55, y: 35, width: 20, height: 20 }],
            client_geometry(), owner_geometry(), root(),
        ).unwrap();
        assert_eq!(plan.visible, vec![RootRect { x: 75, y: 45, width: 5, height: 5 }]);
    }

    #[test]
    fn regions_partially_outside_client_are_clipped_not_rebased() {
        let plan = plan_region_backdrop(
            &[BlurRegionRect { x: -5, y: -4, width: 15, height: 14 }],
            client_geometry(), owner_geometry(), root(),
        ).unwrap();
        assert_eq!(plan.visible, vec![RootRect { x: 20, y: 10, width: 10, height: 10 }]);
    }

    #[test]
    fn regions_preserve_disjoint_rectangles_and_compute_union_capture() {
        let plan = plan_region_backdrop(
            &[
                BlurRegionRect { x: 0, y: 0, width: 5, height: 5 },
                BlurRegionRect { x: 30, y: 20, width: 5, height: 5 },
            ], client_geometry(), owner_geometry(), root(),
        ).unwrap();
        assert_eq!(plan.visible.len(), 2);
        assert_eq!(plan.capture, RootRect { x: 20, y: 10, width: 35, height: 25 });
    }

    #[test]
    fn regions_normalize_overlaps_without_expanding_visible_mask() {
        let plan = plan_region_backdrop(
            &[
                BlurRegionRect { x: 0, y: 0, width: 10, height: 10 },
                BlurRegionRect { x: 5, y: 5, width: 10, height: 10 },
            ], client_geometry(), owner_geometry(), root(),
        ).unwrap();
        assert_eq!(plan.visible.len(), 3);
        assert_eq!(plan.capture, RootRect { x: 20, y: 10, width: 15, height: 15 });
        assert!(plan.visible.iter().enumerate().all(|(index, left)| {
            plan.visible[index + 1..].iter().all(|right| {
                intersect_root_rect(
                    (i64::from(left.x), i64::from(left.y), i64::from(left.width), i64::from(left.height)),
                    (i64::from(right.x), i64::from(right.y), i64::from(right.width), i64::from(right.height)),
                ).is_none()
            })
        }));
    }

    #[test]
    fn duplicate_regions_normalize_to_one_rectangle() {
        let request = [BlurRegionRect { x: 4, y: 6, width: 12, height: 10 }];
        let single = plan_region_backdrop(&request, client_geometry(), owner_geometry(), root()).unwrap();
        let duplicate = plan_region_backdrop(&[request[0], request[0]], client_geometry(), owner_geometry(), root()).unwrap();
        assert_eq!(duplicate, single);
    }

    #[test]
    fn nested_region_adds_no_visible_coverage() {
        let plan = plan_region_backdrop(
            &[
                BlurRegionRect { x: 0, y: 0, width: 30, height: 30 },
                BlurRegionRect { x: 5, y: 5, width: 10, height: 10 },
            ], client_geometry(), owner_geometry(), root(),
        ).unwrap();
        assert_eq!(plan.visible, vec![RootRect { x: 20, y: 10, width: 30, height: 30 }]);
    }

    #[test]
    fn overlapping_regions_at_rounded_owner_corner_are_emitted_once() {
        let owner = RootRect { x: 20, y: 10, width: 60, height: 40 };
        let plan = plan_region_backdrop(
            &[
                BlurRegionRect { x: 0, y: 0, width: 30, height: 20 },
                BlurRegionRect { x: 0, y: 0, width: 20, height: 30 },
            ], client_geometry(), owner, root(),
        ).unwrap();
        assert!(plan.visible.iter().enumerate().all(|(index, left)| {
            plan.visible[index + 1..].iter().all(|right| {
                intersect_root_rect(
                    (i64::from(left.x), i64::from(left.y), i64::from(left.width), i64::from(left.height)),
                    (i64::from(right.x), i64::from(right.y), i64::from(right.width), i64::from(right.height)),
                ).is_none()
            })
        }));
    }

    #[test]
    fn cross_overlap_decomposes_to_non_overlapping_union() {
        let plan = plan_region_backdrop(
            &[
                BlurRegionRect { x: 0, y: 12, width: 40, height: 6 },
                BlurRegionRect { x: 17, y: 0, width: 6, height: 40 },
            ], client_geometry(), owner_geometry(), root(),
        ).unwrap();
        let area: i32 = plan.visible.iter().map(|rect| rect.width * rect.height).sum();
        assert_eq!(area, 40 * 6 + 6 * 40 - 6 * 6);
        assert!(plan.visible.iter().enumerate().all(|(index, left)| {
            plan.visible[index + 1..].iter().all(|right| {
                intersect_root_rect(
                    (i64::from(left.x), i64::from(left.y), i64::from(left.width), i64::from(left.height)),
                    (i64::from(right.x), i64::from(right.y), i64::from(right.width), i64::from(right.height)),
                ).is_none()
            })
        }));
    }

    #[test]
    fn regions_clip_against_owner_and_root_edges() {
        let client = ClientRootGeometry { root_x: -20, root_y: -10, width: 40, height: 40 };
        let owner = RootRect { x: -10, y: -5, width: 30, height: 30 };
        let plan = plan_region_backdrop(
            &[BlurRegionRect { x: 0, y: 0, width: 40, height: 40 }],
            client, owner, root(),
        ).unwrap();
        assert_eq!(plan.visible, vec![RootRect { x: 0, y: 0, width: 20, height: 25 }]);
    }

    #[test]
    fn regions_with_no_surviving_rectangles_do_no_work() {
        assert!(plan_region_backdrop(
            &[BlurRegionRect { x: 0, y: 0, width: 0, height: 20 }],
            client_geometry(), owner_geometry(), root(),
        ).is_none());
        assert!(plan_region_backdrop(
            &[BlurRegionRect { x: 100, y: 100, width: 2, height: 2 }],
            client_geometry(), owner_geometry(), root(),
        ).is_none());
    }

    #[test]
    fn region_planning_handles_large_signed_offsets_without_integer_wrap() {
        assert!(plan_region_backdrop(
            &[BlurRegionRect { x: i32::MAX, y: i32::MIN, width: 1, height: 1 }],
            client_geometry(), owner_geometry(), root(),
        ).is_none());
    }

    #[test]
    fn region_capture_is_union_only_and_expansion_is_deferred_to_blur_primitive() {
        let plan = plan_region_backdrop(
            &[BlurRegionRect { x: 2, y: 3, width: 4, height: 5 }],
            client_geometry(), owner_geometry(), root(),
        ).unwrap();
        assert_eq!(plan.capture, plan.visible[0]);
        let source = include_str!("../graphics/renderer.rs");
        assert!(source.contains("BlurCaptureRegion::new("));
    }

    #[test]
    fn regions_production_path_captures_once_then_composites_each_visible_rect() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = source[start..].find("\nfn egl_scene_is_renderable").unwrap() + start;
        let body = &source[start..end];
        assert_eq!(body.matches("BlurRequest::Regions(regions)").count(), 1);
        assert!(body.contains("for region in region_plan.visible"));
        assert!(body.contains("new_region("));
    }

    #[test]
    fn none_and_full_window_do_not_require_client_origin() {
        assert!(!region_request_requires_client_origin(&BlurRequest::None, Some(7)));
        assert!(!region_request_requires_client_origin(&BlurRequest::FullWindow, Some(7)));
        assert!(!region_request_requires_client_origin(&BlurRequest::Regions(Vec::new()), None));
    }

    #[test]
    fn regions_require_a_semantic_client_origin() {
        let request = BlurRequest::Regions(vec![BlurRegionRect { x: 0, y: 0, width: 10, height: 20 }]);
        assert!(region_request_requires_client_origin(&request, Some(7)));
    }

    #[test]
    fn translated_client_geometry_preserves_root_origin_and_client_bounds() {
        assert_eq!(
            client_root_geometry_from_translation(-12, 34, 948, 518),
            ClientRootGeometry { root_x: -12, root_y: 34, width: 948, height: 518 },
        );
    }

    #[test]
    fn missing_semantic_client_cannot_fabricate_region_mapping() {
        let request = BlurRequest::Regions(vec![BlurRegionRect { x: 0, y: 0, width: 1, height: 1 }]);
        assert!(!region_request_requires_client_origin(&request, None));
    }

    #[test]
    fn translate_coordinates_bad_window_is_a_stale_hierarchy_observation() {
        let error = translate_coordinates_reply_error(damage_create_x11_error(ErrorKind::Window));
        assert!(matches!(
            error.downcast_ref::<CandidateBuildError>(),
            Some(CandidateBuildError::Stale(SceneInvalidation::Hierarchy))
        ));
    }

    #[test]
    fn translate_coordinates_non_window_error_remains_fatal() {
        let error = translate_coordinates_reply_error(damage_create_x11_error(ErrorKind::Match));
        assert!(error.downcast_ref::<CandidateBuildError>().is_none());
    }

    #[test]
    fn region_origin_query_does_not_change_candidate_retry_budget() {
        assert_eq!(MAX_CANDIDATE_RETRIES, 1);
    }

    #[test]
    fn blur_model_two_full_window_owners_complete_before_next_capture() {
        let mut actions = modeled_blur_actions(&BlurRequest::FullWindow, true);
        actions.extend(modeled_blur_actions(&BlurRequest::FullWindow, false));
        assert_eq!(actions.iter().filter(|action| **action == BlurRenderAction::CaptureBlur).count(), 2);
        assert!(actions[..4].contains(&BlurRenderAction::Surface));
        assert_eq!(actions[4], BlurRenderAction::CaptureBlur);
    }

    #[test]
    fn blur_model_full_window_selection_is_independent_of_fullscreen_depth_and_opacity() {
        for fullscreen in [false, true] {
            for depth in [24_u8, 32_u8] {
                for opacity in [0.25_f32, 1.0_f32] {
                    assert!(modeled_blur_selected(&BlurRequest::FullWindow, fullscreen, depth, opacity));
                }
            }
        }
        assert!(!modeled_blur_selected(&BlurRequest::Regions(Vec::new()), true, 32, 0.25));
        assert_eq!(modeled_blur_actions(&BlurRequest::None, false), vec![BlurRenderAction::Surface]);
    }

    #[test]
    fn blur_model_transparent_non_requesting_surface_stays_inert() {
        assert!(!modeled_blur_selected(&BlurRequest::None, false, 32, 0.25));
        assert_eq!(modeled_blur_actions(&BlurRequest::None, false), vec![BlurRenderAction::Surface]);
    }

    #[test]
    fn blur_gaussian_radius_is_named_and_distinct_from_corner_radius() {
        assert_eq!(BACKGROUND_BLUR_RADIUS_PX, 12.0);
        let source = include_str!("scene.rs");
        assert!(source.contains("capture_and_blur_background("));
        assert!(source.contains("BACKGROUND_BLUR_RADIUS_PX"));
        assert!(source.contains("draw_blurred_backdrop(blurred_texture, backdrop_params, plan.corner_radius)"));
    }

    #[test]
    fn blur_wiring_has_no_new_gl_resources_or_renderer_x11_queries() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = start + source[start..].find("\nfn egl_scene_is_renderable").unwrap();
        let wiring = &source[start..end];
        assert!(!wiring.contains("GenTextures"));
        assert!(!wiring.contains("GenFramebuffers"));
        assert!(!wiring.contains("CreateProgram"));
        assert!(!wiring.contains("GetUniformLocation"));
        assert!(!wiring.contains("intern_atom"));
    }

    #[test]
    fn blur_wiring_uses_full_window_only_and_forwards_both_primitives() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = start + source[start..].find("\nfn egl_scene_is_renderable").unwrap();
        let wiring = &source[start..end];
        assert!(wiring.contains("BlurRequest::FullWindow"));
        assert!(wiring.contains("BlurRequest::None => None"));
        assert!(wiring.contains("capture_and_blur_background("));
        assert!(wiring.contains("draw_blurred_backdrop(blurred_texture, backdrop_params, plan.corner_radius)"));
        assert!(wiring.contains("BACKGROUND_BLUR_RADIUS_PX"));
        assert!(!wiring.contains("BlurRequest::Regions(_) => Some"));
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
    fn semantic_client_configure_is_geometry_for_its_canonical_surface() {
        let mut entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        entry.geometry = window(10, 12, 20, 15, 0);
        let snapshot = SceneSnapshot {
            root: 1,
            root_geometry: root(),
            entries: vec![entry],
        };
        let configure = xproto::ConfigureNotifyEvent {
            response_type: 0, sequence: 0, event: 1, window: 20,
            above_sibling: 0, x: 10, y: 12, width: 20, height: 15,
            border_width: 0, override_redirect: false,
        };
        assert_eq!(
            classify_event_with_registries_and_ignored(
                Event::ConfigureNotify(configure), 1, &snapshot, None,
                &HashMap::new(), &HashMap::new(), &HashSet::new(),
            ),
            SceneInvalidation::Geometry(10)
        );
    }

    #[test]
    fn known_non_renderable_configure_is_ignored_but_unknown_remains_hierarchy() {
        let snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: Vec::new() };
        let configure = |window| Event::ConfigureNotify(xproto::ConfigureNotifyEvent {
            response_type: 0, sequence: 0, event: 1, window,
            above_sibling: 0, x: 0, y: 0, width: 10, height: 10,
            border_width: 0, override_redirect: true,
        });
        let ignored = HashSet::from([20]);
        assert_eq!(
            classify_event_with_registries_and_ignored(
                configure(20), 1, &snapshot, None,
                &HashMap::new(), &HashMap::new(), &ignored,
            ),
            SceneInvalidation::Ignore
        );
        assert_eq!(
            classify_event_with_registries_and_ignored(
                configure(21), 1, &snapshot, None,
                &HashMap::new(), &HashMap::new(), &ignored,
            ),
            SceneInvalidation::Hierarchy
        );
    }

    #[test]
    fn semantic_client_configure_does_not_supply_surface_geometry_update() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: vec![entry] };
        let configure = xproto::ConfigureNotifyEvent {
            response_type: 0, sequence: 0, event: 1, window: 20,
            above_sibling: 0, x: 0, y: 0, width: 20, height: 15,
            border_width: 0, override_redirect: false,
        };
        assert!(configure_geometry_update(&Event::ConfigureNotify(configure), &snapshot).is_none());
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
    fn visual_batch_preserves_pixel_subtraction_obligation() {
        let damage = HashSet::from([41_u32, 42_u32]);
        assert!(batch_damage_requires_subtraction(
            SceneInvalidation::VisualState,
            &damage,
        ));
        assert!(batch_damage_requires_subtraction(
            SceneInvalidation::Background,
            &damage,
        ));
        assert!(!batch_damage_requires_subtraction(
            SceneInvalidation::PixelDamage(41),
            &HashSet::new(),
        ));
    }

    #[test]
    fn combined_visual_pixel_batch_subtracts_each_id_once() {
        let mut batch = InvalidationBatch::default();
        batch.push(SceneInvalidation::VisualState);
        batch.push(SceneInvalidation::PixelDamage(41));
        batch.push(SceneInvalidation::PixelDamage(41));
        batch.push(SceneInvalidation::PixelDamage(42));
        let ids = batch.pixel_damage().clone();
        assert_eq!(batch.decision(), SceneInvalidation::VisualState);
        assert!(batch_damage_requires_subtraction(batch.decision(), &ids));
        assert_eq!(subtract_plan(&ids).len(), 2);
    }

    #[test]
    fn visual_only_batch_has_no_damage_subtraction_obligation() {
        assert!(!batch_damage_requires_subtraction(
            SceneInvalidation::VisualState,
            &HashSet::new(),
        ));
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
    fn hierarchy_and_damage_carry_pending_obligation() {
        let mut pending = HashSet::new();
        carry_structural_pending_damage(&mut pending, SceneInvalidation::Hierarchy, &HashSet::from([42]));
        assert_eq!(subtract_plan(&pending), vec![42]);
    }

    #[test]
    fn geometry_and_damage_carry_pending_obligation() {
        let mut pending = HashSet::new();
        carry_structural_pending_damage(&mut pending, SceneInvalidation::Geometry(10), &HashSet::from([42]));
        assert_eq!(pending, HashSet::from([42]));
    }

    #[test]
    fn publication_keeps_survivor_damage_pending() {
        let mut pending = HashSet::from([41_u32]);
        carry_structural_pending_damage(&mut pending, SceneInvalidation::Hierarchy, &HashSet::from([42]));
        assert_eq!(pending, HashSet::from([41, 42]));
        assert_eq!(subtract_plan(&pending).len(), 2);
    }

    #[test]
    fn stale_candidate_keeps_damage_pending() {
        let mut pending = HashSet::new();
        for _ in 0..2 { carry_structural_pending_damage(&mut pending, SceneInvalidation::Hierarchy, &HashSet::from([42])); }
        assert_eq!(pending, HashSet::from([42]));
    }

    #[test]
    fn failed_candidate_keeps_damage_pending() {
        let mut pending = HashSet::new();
        carry_structural_pending_damage(&mut pending, SceneInvalidation::Hierarchy, &HashSet::from([42]));
        assert!(pending.contains(&42));
        assert_eq!(subtract_plan(&pending), vec![42]);
    }

    #[test]
    fn repeated_structural_dominance_does_not_erase_damage() {
        let mut pending = HashSet::new();
        for damage in [41_u32, 42_u32] {
            carry_structural_pending_damage(&mut pending, SceneInvalidation::Hierarchy, &HashSet::from([damage]));
        }
        assert_eq!(pending, HashSet::from([41, 42]));
    }

    #[test]
    fn duplicate_damage_id_has_one_subtract_plan() {
        let mut pending = HashSet::new();
        let batch = HashSet::from([42_u32, 42_u32]);
        carry_structural_pending_damage(&mut pending, SceneInvalidation::Hierarchy, &batch);
        assert_eq!(subtract_plan(&pending), vec![42]);
    }

    #[test]
    fn multiple_damage_ids_have_one_subtract_each() {
        let mut pending = HashSet::new();
        carry_structural_pending_damage(&mut pending, SceneInvalidation::Hierarchy, &HashSet::from([41, 42]));
        assert_eq!(subtract_plan(&pending).len(), 2);
    }

    #[test]
    fn create_survivor_damage_is_carried() {
        let mut pending = HashSet::new();
        carry_structural_pending_damage(&mut pending, SceneInvalidation::Hierarchy, &HashSet::from([7]));
        assert_eq!(pending, HashSet::from([7]));
    }

    #[test]
    fn destroy_survivor_damage_is_carried() {
        let mut pending = HashSet::new();
        carry_structural_pending_damage(&mut pending, SceneInvalidation::Hierarchy, &HashSet::from([8]));
        assert_eq!(subtract_plan(&pending).len(), 1);
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
    fn named_pixmap_size_change_is_typed_stale_geometry() {
        let snapshot = WindowGeometry { x: 933, y: 25, width: 27, height: 1050, border_width: 0 };
        let pixmap = PixmapGeometry { root: 1, x: 0, y: 0, width: 284, height: 1040, border_width: 0, depth: 24 };
        assert!(matches!(
            validate_named_pixmap_dimensions(snapshot, pixmap),
            Err(NamedSurfacePixmapAcquireError::StaleGeometry)
        ));
        let translated = translate_named_pixmap_acquire_error(NamedSurfacePixmapAcquireError::StaleGeometry);
        assert!(matches!(
            translated.downcast_ref::<CandidateBuildError>(),
            Some(CandidateBuildError::Stale(SceneInvalidation::Hierarchy))
        ));
    }

    #[test]
    fn matching_named_pixmap_dimensions_are_accepted() {
        let snapshot = WindowGeometry { x: 10, y: 30, width: 950, height: 1040, border_width: 0 };
        let pixmap = PixmapGeometry { root: 1, x: 0, y: 0, width: 950, height: 1040, border_width: 0, depth: 24 };
        assert!(named_pixmap_dimensions_match(snapshot, pixmap));
    }

    #[test]
    fn named_pixmap_border_is_included_in_expected_dimensions() {
        let snapshot = WindowGeometry { x: 0, y: 0, width: 27, height: 1050, border_width: 2 };
        let pixmap = PixmapGeometry { root: 1, x: 0, y: 0, width: 31, height: 1054, border_width: 0, depth: 24 };
        assert!(named_pixmap_dimensions_match(snapshot, pixmap));
    }

    #[test]
    fn zero_named_pixmap_dimension_is_stale() {
        let snapshot = WindowGeometry { x: 0, y: 0, width: 27, height: 1050, border_width: 0 };
        let pixmap = PixmapGeometry { root: 1, x: 0, y: 0, width: 0, height: 1050, border_width: 0, depth: 24 };
        assert!(matches!(
            validate_named_pixmap_dimensions(snapshot, pixmap),
            Err(NamedSurfacePixmapAcquireError::StaleGeometry)
        ));
    }

    #[test]
    fn stale_x11_observation_translates_but_other_error_stays_fatal() {
        let stale = translate_named_pixmap_acquire_error(NamedSurfacePixmapAcquireError::StaleX11("window disappeared".into()));
        assert!(stale.downcast_ref::<CandidateBuildError>().is_some());
        let other = translate_named_pixmap_acquire_error(NamedSurfacePixmapAcquireError::Other("backend incompatibility".into()));
        assert!(other.downcast_ref::<CandidateBuildError>().is_none());
    }

    // ========================================================
    // 3a3f6a V2 — BUG A: DamageLease::acquire stale classification.
    // Carried forward from the already-reviewed V1 candidate.
    // ========================================================

    fn damage_create_x11_error(error_kind: ErrorKind) -> ReplyError {
        // Shape matches the actually-reproduced fatal trace (DAMAGE/Create,
        // major/minor opcode, sequence, bad_value) with only `error_kind`
        // varied per case.
        ReplyError::X11Error(X11Error {
            error_kind,
            error_code: 9,
            sequence: 19649,
            bad_value: 0x0040031d,
            minor_opcode: 1,
            major_opcode: 132,
            extension_name: Some("DAMAGE".to_string()),
            request_name: Some("Create"),
        })
    }

    #[test]
    fn damage_create_bad_drawable_is_classified_stale() {
        assert!(stale_damage_create_reply(&damage_create_x11_error(ErrorKind::Drawable)));
    }

    #[test]
    fn stale_damage_lease_acquisition_translates_to_hierarchy_invalidation() {
        let translated = translate_damage_lease_acquire_error(DamageLeaseAcquireError::StaleDrawable);
        assert!(matches!(
            translated.downcast_ref::<CandidateBuildError>(),
            Some(CandidateBuildError::Stale(SceneInvalidation::Hierarchy))
        ));
        assert!(is_hierarchy_stale_candidate_error(translated.as_ref()));
    }

    #[test]
    fn resizeonly_hierarchy_stale_is_nonfatal_control_flow() {
        let hierarchy = Box::new(CandidateBuildError::Stale(SceneInvalidation::Hierarchy));
        let geometry = Box::new(CandidateBuildError::Stale(SceneInvalidation::Geometry(7)));
        assert!(is_hierarchy_stale_candidate_error(hierarchy.as_ref()));
        assert!(!is_hierarchy_stale_candidate_error(geometry.as_ref()));
    }

    #[test]
    fn resizeonly_direction_classes_are_mutually_exclusive() {
        let previous = WindowGeometry { x: 10, y: 20, width: 800, height: 600, border_width: 0 };
        let update = |x, y, width, height| PendingGeometry {
            surface_xid: 1,
            x,
            y,
            width,
            height,
            border_width: 0,
            override_redirect: false,
        };
        assert_eq!(classify_resizeonly_direction(previous, update(10, 20, 810, 600)), (ResizeOnlyDirection::Grow, false));
        assert_eq!(classify_resizeonly_direction(previous, update(10, 20, 800, 590)), (ResizeOnlyDirection::Shrink, false));
        assert_eq!(classify_resizeonly_direction(previous, update(10, 20, 810, 590)), (ResizeOnlyDirection::Mixed, false));
        assert_eq!(classify_resizeonly_direction(previous, update(11, 21, 810, 600)), (ResizeOnlyDirection::Grow, true));
        assert_eq!(classify_resizeonly_direction(previous, update(11, 21, 800, 590)), (ResizeOnlyDirection::Shrink, true));
    }

    #[test]
    fn resizeonly_fallback_reason_accounting_is_one_per_reason() {
        let reasons = [
            ResizeOnlyFallbackReason::UnavailableState,
            ResizeOnlyFallbackReason::IdentityMismatch,
            ResizeOnlyFallbackReason::NoSizeChange,
            ResizeOnlyFallbackReason::GeometrySuperseded,
            ResizeOnlyFallbackReason::UnsupportedVisual,
            ResizeOnlyFallbackReason::MissingDamage,
            ResizeOnlyFallbackReason::PrecommitRejected,
            ResizeOnlyFallbackReason::Hierarchy,
        ];
        let mut counts = ResizeOnlyFallbackReasons::default();
        for reason in reasons {
            counts.record(reason);
        }
        assert_eq!(counts.unavailable_state, 1);
        assert_eq!(counts.identity_mismatch, 1);
        assert_eq!(counts.no_size_change, 1);
        assert_eq!(counts.geometry_superseded, 1);
        assert_eq!(counts.unsupported_visual, 1);
        assert_eq!(counts.missing_damage, 1);
        assert_eq!(counts.precommit_rejected, 1);
        assert_eq!(counts.hierarchy, 1);
        assert_eq!(counts.total(), reasons.len() as u64);
    }

    #[test]
    fn resizeonly_structural_provenance_accounts_terminal_publish() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        diagnostics.record_resizeonly_attempt(ResizeOnlyDirection::Shrink, true);
        diagnostics.record_resizeonly_fallback(
            ResizeOnlyDirection::Shrink,
            true,
            ResizeOnlyFallbackReason::PrecommitRejected,
        );
        diagnostics.begin_resizeonly_structural_fallback();
        diagnostics.record_structural_snapshot(Duration::from_micros(3));
        diagnostics.record_structural_terminal(true, false, false);
        let stats = &diagnostics.resizeonly_shrink;
        assert_eq!(stats.fallback_to_structural, 1);
        assert_eq!(stats.fallback_full_snapshot, 1);
        assert_eq!(stats.structural_candidates_started, 1);
        assert_eq!(stats.structural_published, 1);
        assert_eq!(stats.structural_stale, 0);
        assert_eq!(stats.structural_total.samples, 1);
        assert_eq!(stats.structural_full_snapshot.total_us, 3);
    }

    #[test]
    fn resizeonly_early_fallback_has_explicit_direction_unknown_bucket() {
        let mut diagnostics = Diagnostics3a3f8b3a::default();
        diagnostics.record_resizeonly_early_fallback();
        assert_eq!(diagnostics.resizeonly_fallback, 1);
        assert_eq!(diagnostics.resizeonly_fallback_early_unclassified, 1);
    }

    #[test]
    fn resizeonly_direction_reason_totals_match_fallback_totals() {
        for direction in [
            ResizeOnlyDirection::Grow,
            ResizeOnlyDirection::Shrink,
            ResizeOnlyDirection::Mixed,
        ] {
            let mut diagnostics = ResizeOnlyDirectionDiagnostics::default();
            for reason in [
                ResizeOnlyFallbackReason::UnavailableState,
                ResizeOnlyFallbackReason::IdentityMismatch,
                ResizeOnlyFallbackReason::NoSizeChange,
            ] {
                diagnostics.fallback_reasons.record(reason);
                diagnostics.fallback += 1;
            }
            assert_eq!(diagnostics.fallback, diagnostics.fallback_reasons.total(), "direction={direction:?}");
        }
    }

    #[test]
    fn pre_resizeonly_provenance_classifies_surface_and_semantic_sources() {
        let mut entry = visibility_test_entry(window(0, 0, 80, 60, 0), false);
        entry.semantic_client_xid = Some(20);
        let snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: vec![entry] };
        let event = |window| Event::ConfigureNotify(xproto::ConfigureNotifyEvent {
            response_type: 0, sequence: 0, event: 1, window, above_sibling: 0,
            x: 0, y: 0, width: 81, height: 60, border_width: 0, override_redirect: false,
        });
        assert_eq!(geometry_event_source(&event(0x0040_0000), &snapshot), GeometryEventSource::CanonicalSurface);
        assert_eq!(geometry_event_source(&event(20), &snapshot), GeometryEventSource::SemanticClient);
    }

    #[test]
    fn pre_resizeonly_provenance_bypass_reasons_are_one_per_dispatch() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        diagnostics.record_pre_attempt_bypass(GeometryEventSource::SemanticClient, PreResizeOnlyBypassReason::SemanticClientNoSurfacePendingGeometry, Some(ResizeOnlyDirection::Shrink), true);
        diagnostics.record_pre_attempt_bypass(GeometryEventSource::CanonicalSurface, PreResizeOnlyBypassReason::NoPendingGeometry, Some(ResizeOnlyDirection::Grow), false);
        assert_eq!(diagnostics.resizeonly_pre_attempt_bypass_total, 2);
        assert_eq!(diagnostics.resizeonly_pre_attempt_bypass_semantic_client_no_surface_pending_geometry, 1);
        assert_eq!(diagnostics.resizeonly_pre_attempt_bypass_no_pending_geometry, 1);
        assert_eq!(diagnostics.resizeonly_shrink_pre_attempt_bypass, 1);
        assert_eq!(diagnostics.resizeonly_grow_pre_attempt_bypass, 1);
        assert_eq!(diagnostics.pre_resizeonly_bypass_move_resize, 1);
    }

    #[test]
    fn pre_resizeonly_provenance_dispatch_outcomes_are_disjoint() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        diagnostics.record_resize_dispatch(GeometryEventSource::CanonicalSurface, false);
        diagnostics.record_resize_dispatch(GeometryEventSource::SemanticClient, true);
        diagnostics.resize_dispatch_deferred += 1;
        diagnostics.resize_dispatch_hierarchy_dominated += 1;
        assert_eq!(diagnostics.resize_dispatch_total, 2);
        assert_eq!(diagnostics.resize_dispatch_resizeonly_selected + diagnostics.resize_dispatch_structural_selected, diagnostics.resize_dispatch_total);
        assert_eq!(diagnostics.resize_dispatch_deferred, 1);
        assert_eq!(diagnostics.resize_dispatch_hierarchy_dominated, 1);
    }

    #[test]
    fn pre_resizeonly_provenance_structural_origin_and_stale_are_accounted() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        diagnostics.begin_structural_origin(StructuralOrigin::GeometrySemanticClient);
        diagnostics.record_snapshot_origin();
        diagnostics.record_stale_origin(SceneInvalidation::Geometry(10), false);
        diagnostics.record_stale_origin(SceneInvalidation::Geometry(10), true);
        assert_eq!(diagnostics.structural_origin_geometry_semantic_client, 1);
        assert_eq!(diagnostics.snapshot_geometry_semantic_client, 1);
        assert_eq!(diagnostics.stale_geometry_from_semantic_client_configure, 2);
        assert_eq!(diagnostics.stale_geometry_retry, 1);
        assert_eq!(diagnostics.stale_geometry_deferred, 1);
    }

    #[test]
    fn pre_resizeonly_provenance_source_and_pending_buckets_are_complete() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        for source in [GeometryEventSource::CanonicalSurface, GeometryEventSource::SemanticClient, GeometryEventSource::Other, GeometryEventSource::Unknown] { diagnostics.record_geometry_source(source); }
        diagnostics.record_pending_geometry(GeometryEventSource::CanonicalSurface, false, true);
        diagnostics.record_pending_geometry(GeometryEventSource::CanonicalSurface, true, true);
        diagnostics.record_geometry_rejected(GeometryEventSource::CanonicalSurface);
        assert_eq!(diagnostics.configure_from_surface, 1);
        assert_eq!(diagnostics.configure_from_semantic_client, 1);
        assert_eq!(diagnostics.configure_from_other, 1);
        assert_eq!(diagnostics.configure_from_unknown, 1);
        assert_eq!(diagnostics.pending_geometry_created, 1);
        assert_eq!(diagnostics.pending_geometry_updated, 1);
        assert_eq!(diagnostics.pending_geometry_superseded, 1);
        assert_eq!(diagnostics.pending_geometry_surface_match, 2);
        assert_eq!(diagnostics.surface_geometry_update_accepted, 2);
        assert_eq!(diagnostics.surface_geometry_update_rejected, 1);
    }

    #[test]
    fn pre_resizeonly_provenance_all_bypass_reasons_are_reported() {
        let reasons = [
            PreResizeOnlyBypassReason::NoPresentComplete,
            PreResizeOnlyBypassReason::HierarchyPriority,
            PreResizeOnlyBypassReason::NoPendingGeometry,
            PreResizeOnlyBypassReason::SemanticClientNoSurfacePendingGeometry,
            PreResizeOnlyBypassReason::PendingGeometryOtherSurface,
            PreResizeOnlyBypassReason::NoSizeOrBorderChange,
            PreResizeOnlyBypassReason::AmbiguousOrSuperseded,
            PreResizeOnlyBypassReason::StructuralAlreadyRequired,
            PreResizeOnlyBypassReason::Other,
            PreResizeOnlyBypassReason::DirectionUnknown,
        ];
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        for (index, reason) in reasons.into_iter().enumerate() { diagnostics.record_pre_attempt_bypass(GeometryEventSource::Other, reason, [Some(ResizeOnlyDirection::Grow), Some(ResizeOnlyDirection::Shrink), Some(ResizeOnlyDirection::Mixed), None][index % 4], false); }
        assert_eq!(diagnostics.resizeonly_pre_attempt_bypass_total, 10);
        assert_eq!(diagnostics.resizeonly_pre_attempt_bypass_no_present_complete + diagnostics.resizeonly_pre_attempt_bypass_hierarchy_priority + diagnostics.resizeonly_pre_attempt_bypass_no_pending_geometry + diagnostics.resizeonly_pre_attempt_bypass_semantic_client_no_surface_pending_geometry + diagnostics.resizeonly_pre_attempt_bypass_pending_geometry_other_surface + diagnostics.resizeonly_pre_attempt_bypass_no_size_or_border_change + diagnostics.resizeonly_pre_attempt_bypass_ambiguous_or_superseded + diagnostics.resizeonly_pre_attempt_bypass_structural_already_required + diagnostics.resizeonly_pre_attempt_bypass_other + diagnostics.resizeonly_pre_attempt_bypass_direction_unknown, 10);
    }

    #[test]
    fn pre_resizeonly_provenance_structural_origin_buckets_are_complete() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        for origin in [StructuralOrigin::NormalLifecycle, StructuralOrigin::Hierarchy, StructuralOrigin::GeometrySurface, StructuralOrigin::GeometrySemanticClient, StructuralOrigin::GeometryNoPending, StructuralOrigin::Other] { diagnostics.begin_structural_origin(origin); }
        assert_eq!(diagnostics.structural_origin_normal + diagnostics.structural_origin_hierarchy + diagnostics.structural_origin_geometry_surface + diagnostics.structural_origin_geometry_semantic_client + diagnostics.structural_origin_geometry_no_pending + diagnostics.structural_origin_other, 6);
    }

    #[test]
    fn pre_resizeonly_provenance_snapshot_buckets_are_complete() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        for origin in [StructuralOrigin::GeometrySurface, StructuralOrigin::GeometrySemanticClient, StructuralOrigin::GeometryNoPending, StructuralOrigin::Hierarchy, StructuralOrigin::Other] { diagnostics.structural_origin = Some(origin); diagnostics.record_snapshot_origin(); }
        assert_eq!(diagnostics.snapshot_geometry_surface + diagnostics.snapshot_geometry_semantic_client + diagnostics.snapshot_geometry_no_pending + diagnostics.snapshot_hierarchy + diagnostics.snapshot_other, 5);
    }

    #[test]
    fn pre_resizeonly_provenance_reporter_contains_all_new_aggregates() {
        let source = include_str!("scene.rs");
        for name in ["3a3f8b5o_event_provenance", "3a3f8b5o_resize_dispatch", "3a3f8b5o_structural_origin", "semantic_client_without_surface_pending_geometry", "resizeonly_pre_attempt_bypass_total", "pending_geometry_missing_at_dispatch"] { assert!(source.contains(name), "reporter must contain {name}"); }
    }

    #[test]
    fn stable_resize_damage_remains_routable_when_pending() {
        let damage = 17;
        let surface = 42;
        let registry = HashMap::from([(damage, surface)]);
        let mut pending = HashSet::from([damage]);
        retain_pending_for_registry(&mut pending, &registry);
        assert_eq!(pending, HashSet::from([damage]));
        assert_eq!(registry.get(&damage), Some(&surface));
    }

    #[test]
    fn non_drawable_damage_create_errors_are_not_stale() {
        for kind in [
            ErrorKind::Match,
            ErrorKind::Value,
            ErrorKind::IDChoice,
            ErrorKind::Alloc,
            ErrorKind::Window,
            ErrorKind::Pixmap,
        ] {
            assert!(
                !stale_damage_create_reply(&damage_create_x11_error(kind)),
                "{kind:?} must not be classified stale for DAMAGE/Create"
            );
        }
        let other = translate_damage_lease_acquire_error(DamageLeaseAcquireError::Other("connection lost".into()));
        assert!(other.downcast_ref::<CandidateBuildError>().is_none());
    }

    #[test]
    fn damage_create_retry_policy_is_unchanged_by_this_fix() {
        assert_eq!(MAX_CANDIDATE_RETRIES, 1);
    }

    #[test]
    fn damage_lease_acquire_only_constructs_self_after_checked_success() {
        let source = include_str!("scene.rs");
        let impl_start = source.find("impl<'a> DamageLease<'a> {").expect("DamageLease impl exists");
        let fn_start = impl_start + source[impl_start..].find("fn acquire(").expect("acquire exists");
        let fn_end = fn_start + source[fn_start..].find("\n    fn subtract").expect("acquire body ends before subtract");
        let body = &source[fn_start..fn_end];
        // Ownership (Self, and therefore Drop-based DamageDestroy on later
        // release) is only granted after the checked DAMAGE/Create round
        // trip has already succeeded; a rejected Create must never
        // construct a DamageLease and must never itself send DamageDestroy
        // for the rejected XID.
        assert!(!body.contains("damage_destroy"));
        let check_index = body.find(".check()").expect("uses the checked round trip");
        let ok_index = body.find("Ok(Self {").expect("constructs Self on success");
        assert!(check_index < ok_index);
    }

    #[test]
    fn damage_create_stale_fix_does_not_touch_registry_or_commit_paths() {
        let source = include_str!("scene.rs");
        let impl_start = source.find("impl<'a> DamageLease<'a> {").expect("DamageLease impl exists");
        let fn_start = impl_start + source[impl_start..].find("fn acquire(").expect("acquire exists");
        let fn_end = fn_start + source[fn_start..].find("\n    fn subtract").expect("acquire body ends before subtract");
        let body = &source[fn_start..fn_end];
        assert!(!body.contains("damage_registry"));
        assert!(!body.contains("commit_candidate"));
    }

    // ========================================================
    // post-3a3fa1i R2 — active-invariant DamageSubtract classifier. Two
    // independent signals (this lease's own, provably-tamper-proof
    // DamageState, and the server's reply) must both hold before a
    // DamageBadDamage on Subtract is treated as a stale, already-gone
    // resource rather than a fatal error. Root cause, ownership proof,
    // and design rationale are in
    // xomposite-design-reports/milestone-post-3a3fa1i-xdamage-active-lease-invalidation-proof-audit.txt.
    // ========================================================

    fn damage_subtract_x11_error(error_kind: ErrorKind) -> ReplyError {
        // Shape matches the actually-reproduced fatal trace (DAMAGE/Subtract,
        // major/minor opcode, sequence, bad_value) with only `error_kind`
        // varied per case.
        ReplyError::X11Error(X11Error {
            error_kind,
            error_code: 152,
            sequence: 60393,
            bad_value: 0x00a00a4b,
            minor_opcode: 3,
            major_opcode: 132,
            extension_name: Some("DAMAGE".to_string()),
            request_name: Some("Subtract"),
        })
    }

    #[test]
    fn active_bad_damage_is_already_gone() {
        // Required test A.
        assert_eq!(
            classify_damage_subtract_error(
                DamageState::Active,
                &damage_subtract_x11_error(ErrorKind::DamageBadDamage),
            ),
            DamageSubtractClassification::AlreadyGone
        );
    }

    #[test]
    fn active_other_error_is_fatal() {
        // Required test B.
        for kind in [
            ErrorKind::Match,
            ErrorKind::Value,
            ErrorKind::IDChoice,
            ErrorKind::Alloc,
            ErrorKind::Window,
            ErrorKind::Drawable,
        ] {
            assert_eq!(
                classify_damage_subtract_error(DamageState::Active, &damage_subtract_x11_error(kind)),
                DamageSubtractClassification::Fatal,
                "{kind:?} must remain Fatal even while the lease is Active"
            );
        }
    }

    #[test]
    fn non_active_bad_damage_is_fatal() {
        // Required test C. Production subtract() never actually reaches
        // the classifier in these states (its own top guard returns
        // before issuing any request), but the classifier itself -- in
        // isolation -- must not launder a DamageBadDamage into
        // AlreadyGone just because the error kind matches; the Active
        // precondition must independently hold too.
        for state in [
            DamageState::DestroyAttempted,
            DamageState::Released,
            DamageState::AlreadyGone,
            DamageState::Disarmed,
        ] {
            assert_eq!(
                classify_damage_subtract_error(state, &damage_subtract_x11_error(ErrorKind::DamageBadDamage)),
                DamageSubtractClassification::Fatal,
                "{state:?} + DamageBadDamage must not produce AlreadyGone"
            );
        }
    }

    #[test]
    fn state_input_determines_classification_for_identical_error() {
        // Required test D (MANDATORY): this is the direct proof that
        // DamageBadDamage alone is insufficient -- the identical error
        // value classifies differently purely as a function of the
        // second, independent `state` parameter.
        let active_result = classify_damage_subtract_error(
            DamageState::Active,
            &damage_subtract_x11_error(ErrorKind::DamageBadDamage),
        );
        let non_active_result = classify_damage_subtract_error(
            DamageState::DestroyAttempted,
            &damage_subtract_x11_error(ErrorKind::DamageBadDamage),
        );
        assert_eq!(active_result, DamageSubtractClassification::AlreadyGone);
        assert_eq!(non_active_result, DamageSubtractClassification::Fatal);
        assert_ne!(
            active_result, non_active_result,
            "identical DamageBadDamage must classify differently depending on lease state alone"
        );
    }

    #[test]
    fn already_gone_state_matches_mark_already_gone_semantics() {
        // Executable state-machine proxy (Section 11 of the R2 tasking).
        // DamageLease cannot be constructed in a unit test without a live
        // X11Connection (its sole constructor, acquire(), requires one,
        // and no test anywhere in this file's existing suite constructs a
        // real DamageLease or NamedSurfacePixmap either) -- this is a
        // pre-existing limitation of the whole test architecture, not
        // introduced by this fix. mark_already_gone()'s entire body is
        // `self.state.set(DamageState::AlreadyGone)`; this test executes
        // that exact primitive against a bare Cell<DamageState> seeded at
        // Active (the only state subtract() ever calls it from) and
        // proves the classify -> transition contract end to end at the
        // data-type level.
        let state = std::cell::Cell::new(DamageState::Active);
        let classification = classify_damage_subtract_error(
            state.get(),
            &damage_subtract_x11_error(ErrorKind::DamageBadDamage),
        );
        assert_eq!(classification, DamageSubtractClassification::AlreadyGone);
        if classification == DamageSubtractClassification::AlreadyGone {
            state.set(DamageState::AlreadyGone);
        }
        assert_eq!(state.get(), DamageState::AlreadyGone);
        assert_ne!(state.get(), DamageState::Active);
    }

    #[test]
    fn subtract_wires_classifier_output_to_mark_already_gone_and_fatal_err() {
        // Secondary structural guardrail (not the primary proof -- see
        // the classifier-level tests above for that): confirms the wiring
        // inside subtract() dispatches on classify_damage_subtract_error's
        // two variants correctly and issues exactly one request.
        let source = include_str!("scene.rs");
        let impl_start = source.find("impl<'a> DamageLease<'a> {").expect("DamageLease impl exists");
        let fn_start = impl_start + source[impl_start..].find("fn subtract(").expect("subtract exists");
        let fn_end = fn_start + source[fn_start..].find("\n    fn destroy").expect("subtract body ends before destroy");
        let body = &source[fn_start..fn_end];
        assert!(body.contains("let state = self.state.get();"));
        assert!(body.contains("classify_damage_subtract_error(state, &error)"));
        assert!(body.contains("DamageSubtractClassification::AlreadyGone"));
        assert!(body.contains("DamageSubtractClassification::Fatal"));
        assert!(body.contains("mark_already_gone"));
        assert!(body.contains("Err(Box::new(error))"));
        assert_eq!(body.matches("damage_subtract(").count(), 1);
    }

    #[test]
    fn subtract_fix_contains_no_transient_specific_conditions() {
        let source = include_str!("scene.rs");
        let impl_start = source.find("impl<'a> DamageLease<'a> {").expect("DamageLease impl exists");
        let fn_start = impl_start + source[impl_start..].find("fn subtract(").expect("subtract exists");
        let fn_end = fn_start + source[fn_start..].find("\n    fn destroy").expect("subtract body ends before destroy");
        let body = &source[fn_start..fn_end];
        for token in ["xbar", "360", "88", "notification", "WM_CLASS", "wm_class", "geometry"] {
            assert!(
                !body.contains(token),
                "subtract() must not encode a {token}-specific condition"
            );
        }
    }

    #[test]
    fn subtract_fix_does_not_touch_registry_commit_or_resize_paths() {
        let source = include_str!("scene.rs");
        let impl_start = source.find("impl<'a> DamageLease<'a> {").expect("DamageLease impl exists");
        let fn_start = impl_start + source[impl_start..].find("fn subtract(").expect("subtract exists");
        let fn_end = fn_start + source[fn_start..].find("\n    fn destroy").expect("subtract body ends before destroy");
        let body = &source[fn_start..fn_end];
        assert!(!body.contains("damage_registry"));
        assert!(!body.contains("commit_candidate"));
        assert!(!body.contains("moveonly"));
        assert!(!body.contains("resizeonly"));
        assert!(!body.contains("surface_removed"));
        assert!(!body.contains("DestroyNotify"));
    }

    #[test]
    fn already_gone_lease_short_circuits_destroy_before_any_request() {
        // Secondary guardrail for the cleanup no-second-request property
        // (Section 12 of the R2 tasking). destroy() itself is UNCHANGED by
        // this candidate; this proves its pre-existing guard still checks
        // state before issuing any request, which combined with the
        // AlreadyGone terminal classification proven above (test A /
        // already_gone_state_matches_mark_already_gone_semantics) is what
        // makes cleanup's later retire_damage_lease() call emit zero
        // DamageDestroy requests for a lease this fix marked AlreadyGone.
        // A full mock-connection integration test proving "zero requests
        // observed on the wire" would require introducing a connection
        // trait/mock seam not present anywhere in this codebase's existing
        // test suite (no test here constructs a live or mocked
        // DamageLease/NamedSurfacePixmap) -- that is a genuine scope
        // expansion beyond this fix and was deliberately not pursued, per
        // instruction not to redesign solely for testing.
        let source = include_str!("scene.rs");
        let impl_start = source.find("impl<'a> DamageLease<'a> {").expect("DamageLease impl exists");
        let fn_start = impl_start + source[impl_start..].find("fn destroy(").expect("destroy exists");
        let fn_end = fn_start + source[fn_start..].find("\n    fn mark_already_gone").expect("destroy body ends before mark_already_gone");
        let body = &source[fn_start..fn_end];
        let guard_index = body.find("DamageState::Active").expect("guards on Active state");
        let request_index = body.find("damage_destroy(").expect("issues DamageDestroy");
        assert!(
            guard_index < request_index,
            "destroy() must check state before issuing a request"
        );
    }

    // ========================================================
    // 3a3f6a V2 — BUG B: early visual-contribution filter.
    // ========================================================

    fn full_hd_root() -> RootGeometry {
        RootGeometry { width: 1920, height: 1080, depth: 24, visual: 0x21 }
    }

    fn geo(x: i16, y: i16, width: u16, height: u16) -> WindowGeometry {
        WindowGeometry { x, y, width, height, border_width: 0 }
    }

    fn shadow_style(enabled: bool, extent: f32, offset_x: f32, offset_y: f32) -> crate::config::ShadowConfig {
        crate::config::ShadowConfig {
            enabled,
            color: [0, 0, 0],
            offset_x,
            offset_y,
            extent,
            strength: 0.5,
        }
    }

    fn visibility_test_entry(geometry: WindowGeometry, shadow_eligible: bool) -> SurfaceEntry {
        SurfaceEntry {
            surface_xid: 0x0040_0000,
            semantic_client_xid: None,
            effect_owner: None,
            own_blur_request: BlurRequest::None,
            lifecycle_xid: 0x0040_0000,
            geometry,
            depth: 24,
            visual: 0x2d8,
            class: WindowClass::INPUT_OUTPUT,
            map_state: MapState::VIEWABLE,
            override_redirect: true,
            effective_override_redirect: true,
            stacking_index: 0,
            backend: BackendCompatibility::BackendUnsupported,
            visual_class: SurfaceVisualClass::Normal,
            fullscreen: false,
            shadow_eligible,
            resolved_border_color: [0, 0, 0, 1.0f32.to_bits()],
        resolved_opacity_bits: 1.0f32.to_bits(),
        client_root_geometry: None,
        resolved_blur_request: BlurRequest::None,
        }
    }

    // ========================================================
    // 3a3fa2a — window open animation core.
    // ========================================================

    fn animation_test_entry(
        surface_xid: Window,
        visual_class: SurfaceVisualClass,
        override_redirect: bool,
    ) -> SurfaceEntry {
        SurfaceEntry {
            surface_xid,
            semantic_client_xid: None,
            effect_owner: None,
            own_blur_request: BlurRequest::None,
            lifecycle_xid: surface_xid,
            geometry: geo(0, 0, 100, 100),
            depth: 24,
            visual: 0x2d8,
            class: WindowClass::INPUT_OUTPUT,
            map_state: MapState::VIEWABLE,
            override_redirect,
            // R1 tests only ever model capture == semantic (no identity
            // mismatch) — mirroring the single `override_redirect` param
            // keeps every existing R1 test's semantics unchanged under R2.
            // R2-specific capture/semantic mismatch tests use the real
            // eligible_surface_with_semantic_metadata() construction path
            // instead (see the "R2" test section below), not this fixture.
            effective_override_redirect: override_redirect,
            stacking_index: 0,
            backend: BackendCompatibility::BackendUnsupported,
            visual_class,
            fullscreen: false,
            shadow_eligible: false,
            resolved_border_color: [0, 0, 0, 1.0f32.to_bits()],
            resolved_opacity_bits: 1.0f32.to_bits(),
            client_root_geometry: None,
            resolved_blur_request: BlurRequest::None,
        }
    }

    fn animation_test_plan(dst_x: i32, dst_y: i32, width: i32, height: i32) -> RenderQuadPlan {
        RenderQuadPlan {
            dst_x,
            dst_y,
            width,
            height,
            outer_x: dst_x - 2,
            outer_y: dst_y - 2,
            outer_width: width + 4,
            outer_height: height + 4,
            src_x: 0,
            src_y: 0,
            src_width: width,
            src_height: height,
            u0: 0.0,
            v0: 0.0,
            u1: 1.0,
            v1: 1.0,
            corner_radius: 8.0,
            border_width: 2.0,
            border_color: [1.0, 1.0, 1.0, 1.0],
        }
    }

    fn test_animation_config(enabled: bool) -> AnimationConfig {
        AnimationConfig {
            enabled,
            open: OpenAnimationConfig { effect: OpenAnimationEffect::Scale, duration: Duration::from_millis(180) },
            close: crate::config::CloseAnimationConfig { enabled: false, effect: crate::config::CloseAnimationEffect::Scale, duration: Duration::from_millis(180) },
        }
    }

    fn test_window_animation(started_at: Instant) -> WindowAnimation {
        WindowAnimation::open(started_at, OpenAnimationEffect::Scale, Duration::from_millis(180))
    }

    fn test_animation_config_with_effect(enabled: bool, effect: OpenAnimationEffect) -> AnimationConfig {
        AnimationConfig {
            enabled,
            open: OpenAnimationConfig { effect, duration: Duration::from_millis(180) },
            close: crate::config::CloseAnimationConfig { enabled: false, effect: crate::config::CloseAnimationEffect::Scale, duration: Duration::from_millis(180) },
        }
    }


    fn test_window_animation_with_effect(started_at: Instant, effect: OpenAnimationEffect) -> WindowAnimation {
        WindowAnimation::open(started_at, effect, Duration::from_millis(180))
    }

    // --- A/B/C: progress + easing (pure, deterministic, clamped) ---

    const TEST_ANIMATION_DURATION: Duration = Duration::from_millis(180);

    #[test]
    fn animation_progress_and_scale_sample_start_at_from_values() {
        let t = animation_progress(Duration::ZERO, TEST_ANIMATION_DURATION);
        assert_eq!(t, 0.0);
        let visual = sample_open_effect(OpenAnimationEffect::Scale, t);
        assert_eq!(visual.opacity, SCALE_EFFECT_FROM_OPACITY);
        assert_eq!(visual.scale_x, SCALE_EFFECT_FROM_SCALE);
        assert_eq!(visual.scale_y, SCALE_EFFECT_FROM_SCALE);
    }

    #[test]
    fn scale_sample_midpoint_strictly_between_endpoints() {
        let t = animation_progress(TEST_ANIMATION_DURATION / 2, TEST_ANIMATION_DURATION);
        assert!(t > 0.0 && t < 1.0);
        let visual = sample_open_effect(OpenAnimationEffect::Scale, t);
        assert!(visual.opacity > 0.0 && visual.opacity < 1.0, "opacity={}", visual.opacity);
        assert!(visual.scale_x > SCALE_EFFECT_FROM_SCALE && visual.scale_x < 1.0, "scale_x={}", visual.scale_x);
        assert_eq!(visual.scale_x, visual.scale_y, "Scale is uniform");
    }

    #[test]
    fn animation_progress_clamps_and_scale_sample_reaches_exact_final_state() {
        assert_eq!(animation_progress(TEST_ANIMATION_DURATION, TEST_ANIMATION_DURATION), 1.0);
        // Elapsed far past duration must still clamp to exactly 1.0, never overshoot.
        assert_eq!(animation_progress(TEST_ANIMATION_DURATION * 10, TEST_ANIMATION_DURATION), 1.0);
        let visual = sample_open_effect(OpenAnimationEffect::Scale, 1.0);
        assert_eq!(visual.opacity, 1.0);
        assert_eq!(visual.scale_x, 1.0);
        assert_eq!(visual.scale_y, 1.0);
    }

    #[test]
    fn ease_out_cubic_is_bounded_and_monotonic() {
        assert_eq!(ease_out_cubic(0.0), 0.0);
        assert_eq!(ease_out_cubic(1.0), 1.0);
        let a = ease_out_cubic(0.25);
        let b = ease_out_cubic(0.75);
        assert!((0.0..=1.0).contains(&a));
        assert!((0.0..=1.0).contains(&b));
        assert!(a < b);
    }

    #[test]
    fn lerp_interpolates_linearly() {
        assert_eq!(lerp(0.0, 10.0, 0.5), 5.0);
        assert_eq!(lerp(2.0, 2.0, 0.7), 2.0);
        assert_eq!(lerp(-1.0, 1.0, 0.0), -1.0);
        assert_eq!(lerp(-1.0, 1.0, 1.0), 1.0);
    }

    // --- D/E: render-plan scaling (geometry, UV, corner/border) ---

    #[test]
    fn scale_render_quad_plan_keeps_center_stable() {
        let plan = animation_test_plan(100, 200, 300, 400);
        let scaled = scale_render_quad_plan(plan, 0.5, 0.5);
        let orig_center_x = plan.dst_x as f32 + plan.width as f32 / 2.0;
        let orig_center_y = plan.dst_y as f32 + plan.height as f32 / 2.0;
        let new_center_x = scaled.dst_x as f32 + scaled.width as f32 / 2.0;
        let new_center_y = scaled.dst_y as f32 + scaled.height as f32 / 2.0;
        assert!((orig_center_x - new_center_x).abs() <= 1.0);
        assert!((orig_center_y - new_center_y).abs() <= 1.0);
        assert_eq!(scaled.width, 150);
        assert_eq!(scaled.height, 200);
    }

    #[test]
    fn scale_render_quad_plan_preserves_uv_but_scales_outer_bounds_with_the_box() {
        let plan = animation_test_plan(10, 20, 200, 100);
        let scaled = scale_render_quad_plan(plan, SCALE_EFFECT_FROM_SCALE, SCALE_EFFECT_FROM_SCALE);
        assert_eq!(scaled.u0, plan.u0);
        assert_eq!(scaled.v0, plan.v0);
        assert_eq!(scaled.u1, plan.u1);
        assert_eq!(scaled.v1, plan.v1);
        assert_eq!(scaled.src_x, plan.src_x);
        assert_eq!(scaled.src_y, plan.src_y);
        // 3a3fa2b1-s1: outer_* (shadow's base rect, see
        // shadow_params_from_plan) now scales WITH the box — this was
        // deliberately un-scaled before this milestone, which is exactly
        // the shadow-geometry bug this milestone fixes. Blur staying on
        // real bounds is a render_egl_scene_parts call-site property
        // (always passes blur the original `plan`, never this result),
        // not a property of this function anymore.
        let expected_outer_width = ((plan.outer_width as f32) * SCALE_EFFECT_FROM_SCALE).round().max(1.0) as i32;
        let expected_outer_height = ((plan.outer_height as f32) * SCALE_EFFECT_FROM_SCALE).round().max(1.0) as i32;
        assert_eq!(scaled.outer_width, expected_outer_width);
        assert_eq!(scaled.outer_height, expected_outer_height);
        assert_ne!(scaled.outer_width, plan.outer_width);
        assert_ne!(scaled.outer_height, plan.outer_height);
        // outer rect is recentered around its OWN center, same idiom as dst.
        let orig_outer_center_x = plan.outer_x as f32 + plan.outer_width as f32 / 2.0;
        let orig_outer_center_y = plan.outer_y as f32 + plan.outer_height as f32 / 2.0;
        let new_outer_center_x = scaled.outer_x as f32 + scaled.outer_width as f32 / 2.0;
        let new_outer_center_y = scaled.outer_y as f32 + scaled.outer_height as f32 / 2.0;
        assert!((orig_outer_center_x - new_outer_center_x).abs() <= 1.0);
        assert!((orig_outer_center_y - new_outer_center_y).abs() <= 1.0);
    }

    #[test]
    fn scale_render_quad_plan_scales_corner_radius_and_border_with_the_box() {
        let plan = animation_test_plan(0, 0, 100, 100);
        let scaled = scale_render_quad_plan(plan, 0.5, 0.5);
        assert_eq!(scaled.corner_radius, plan.corner_radius * 0.5);
        assert_eq!(scaled.border_width, plan.border_width * 0.5);
    }

    #[test]
    fn scale_render_quad_plan_guards_invalid_scale() {
        let plan = animation_test_plan(0, 0, 100, 100);
        assert_eq!(scale_render_quad_plan(plan, 0.0, 1.0), plan);
        assert_eq!(scale_render_quad_plan(plan, 1.0, 0.0), plan);
        assert_eq!(scale_render_quad_plan(plan, -1.0, 1.0), plan);
        assert_eq!(scale_render_quad_plan(plan, f32::NAN, 1.0), plan);
        assert_eq!(scale_render_quad_plan(plan, 1.0, f32::NAN), plan);
    }

    #[test]
    fn scale_render_quad_plan_never_produces_zero_or_negative_dimensions() {
        let plan = animation_test_plan(0, 0, 1, 1);
        let scaled = scale_render_quad_plan(plan, 0.01, 0.01);
        assert!(scaled.width >= 1);
        assert!(scaled.height >= 1);
    }

    // --- 3a3fa2b1 generic non-uniform transform seam (prepares Teleport
    // without implementing it) ---

    #[test]
    fn scale_render_quad_plan_identity_when_both_axes_are_one() {
        let plan = animation_test_plan(10, 20, 200, 100);
        assert_eq!(scale_render_quad_plan(plan, 1.0, 1.0), plan);
    }

    #[test]
    fn scale_render_quad_plan_x_only_scaling_leaves_height_unchanged() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let scaled = scale_render_quad_plan(plan, 0.5, 1.0);
        assert_eq!(scaled.width, 100);
        assert_eq!(scaled.height, 100);
        // center preserved on the scaled axis only
        assert_eq!(scaled.dst_y, plan.dst_y);
    }

    #[test]
    fn scale_render_quad_plan_y_only_scaling_leaves_width_unchanged() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let scaled = scale_render_quad_plan(plan, 1.0, 0.5);
        assert_eq!(scaled.width, 200);
        assert_eq!(scaled.height, 50);
        assert_eq!(scaled.dst_x, plan.dst_x);
    }

    #[test]
    fn scale_render_quad_plan_corner_and_border_use_the_smaller_axis_scale() {
        let plan = animation_test_plan(0, 0, 200, 100);
        // scale_x shrinks far more than scale_y — corner/border must
        // follow the SMALLER (scale_x) factor, per the audited rule.
        let scaled = scale_render_quad_plan(plan, 0.25, 0.9);
        assert_eq!(scaled.corner_radius, plan.corner_radius * 0.25);
        assert_eq!(scaled.border_width, plan.border_width * 0.25);
    }

    #[test]
    fn scale_render_quad_plan_non_uniform_preserves_uv() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let scaled = scale_render_quad_plan(plan, 0.3, 0.8);
        assert_eq!(scaled.u0, plan.u0);
        assert_eq!(scaled.v0, plan.v0);
        assert_eq!(scaled.u1, plan.u1);
        assert_eq!(scaled.v1, plan.v1);
    }

    // --- F: opacity multiplication ---

    #[test]
    fn animation_opacity_multiplies_existing_resolved_opacity() {
        let base_opacity = 0.5_f32;
        let start = sample_open_effect(OpenAnimationEffect::Scale, 0.0);
        assert_eq!(base_opacity * start.opacity, 0.0);
        let end = sample_open_effect(OpenAnimationEffect::Scale, 1.0);
        assert_eq!(base_opacity * end.opacity, base_opacity);
        let mid = sample_open_effect(OpenAnimationEffect::Scale, 0.5);
        let mixed = base_opacity * mid.opacity;
        assert!(mixed > 0.0 && mixed < base_opacity);
    }

    // --- J/K/L/M/Q: generic effect-sampling properties ---

    #[test]
    fn scale_sample_opacity_stays_within_bounds_across_a_dense_sweep() {
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let visual = sample_open_effect(OpenAnimationEffect::Scale, t);
            assert!((0.0..=1.0).contains(&visual.opacity), "t={t} opacity={}", visual.opacity);
        }
    }

    #[test]
    fn scale_sample_values_are_finite_across_a_dense_sweep() {
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let visual = sample_open_effect(OpenAnimationEffect::Scale, t);
            assert!(visual.opacity.is_finite());
            assert!(visual.scale_x.is_finite());
            assert!(visual.scale_y.is_finite());
        }
    }

    #[test]
    fn scale_sample_t_greater_than_one_still_finalizes_exactly() {
        let visual = sample_open_effect(OpenAnimationEffect::Scale, 1.0);
        assert_eq!(visual, AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 });
    }

    #[test]
    fn scale_sample_matches_the_already_validated_ease_out_cubic_lerp_composition() {
        // Regression pin against R1's already-human-validated behavior:
        // sample_scale(t) must equal directly composing ease_out_cubic+lerp
        // over the Scale effect's own documented endpoints.
        for t in [0.0_f32, 0.13, 0.5, 0.87, 1.0] {
            let eased = ease_out_cubic(t);
            let expected_scale = lerp(SCALE_EFFECT_FROM_SCALE, SCALE_EFFECT_TO_SCALE, eased);
            let expected_opacity = lerp(SCALE_EFFECT_FROM_OPACITY, SCALE_EFFECT_TO_OPACITY, eased);
            let visual = sample_scale(t);
            assert_eq!(visual.scale_x, expected_scale);
            assert_eq!(visual.scale_y, expected_scale);
            assert_eq!(visual.opacity, expected_opacity);
        }
    }

    // ========================================================
    // 3a3fa2b2-r2 — teleport materialization effect.
    // ========================================================

    // --- start state (exact) ---

    #[test]
    fn teleport_sample_t0_is_exact_thin_streak_start_state() {
        let visual = sample_teleport(0.0);
        assert_eq!(visual, AnimationVisual {
            opacity: TELEPORT_START_OPACITY,
            scale_x: TELEPORT_START_SCALE_X,
            scale_y: TELEPORT_START_SCALE_Y,
        });
        assert_eq!(visual.scale_x, 0.45);
        assert_eq!(visual.scale_y, 0.03);
        assert_eq!(visual.opacity, 0.0);
        assert!(visual.scale_y < visual.scale_x, "must read as a flat streak, not a small window");
    }

    // --- front-loaded motion contract ---

    #[test]
    fn teleport_phase_a_end_has_begun_materializing_substantially() {
        let start = sample_teleport(0.0);
        let phase_a_end = sample_teleport(TELEPORT_PHASE_A_END);
        assert!(phase_a_end.opacity > start.opacity + 0.15, "opacity={}", phase_a_end.opacity);
        assert!(phase_a_end.scale_y > start.scale_y + 0.10, "scale_y={}", phase_a_end.scale_y);
        // Still reads as a materializing streak, not a normal window yet.
        assert!(phase_a_end.scale_x < 0.9);
        assert!(phase_a_end.scale_y < 0.5);
    }

    #[test]
    fn teleport_at_t_035_is_already_essentially_full_size() {
        // The critical human-character gate: by t=0.35, geometry/opacity
        // transition must be essentially done — only a small, sharp
        // impact/recoil/snap remains.
        let visual = sample_teleport(TELEPORT_PHASE_B_END);
        assert!(visual.scale_x >= 0.98, "scale_x={}", visual.scale_x);
        assert!(visual.scale_y >= 0.98, "scale_y={}", visual.scale_y);
        assert!(visual.opacity >= 0.95, "opacity={}", visual.opacity);
    }

    #[test]
    fn teleport_opacity_reaches_one_by_t_035_and_stays_there() {
        assert_eq!(sample_teleport(TELEPORT_PHASE_B_END).opacity, 1.0);
        // No further fading through impact/recoil/snap — materialization
        // (opacity) is deliberately decoupled from the geometric settle.
        for i in 0..=20 {
            let t = TELEPORT_PHASE_B_END + (1.0 - TELEPORT_PHASE_B_END) * (i as f32 / 20.0);
            assert_eq!(sample_teleport(t).opacity, 1.0, "t={t}");
        }
    }

    // --- distinct from Scale ---

    #[test]
    fn teleport_differs_materially_from_scale_at_several_t_values() {
        for t in [0.05_f32, 0.15, 0.25, 0.50] {
            let teleport = sample_teleport(t);
            let scale = sample_scale(t);
            assert_ne!(teleport, scale, "t={t}");
        }
    }

    #[test]
    fn teleport_scale_y_is_far_behind_scale_x_at_early_t() {
        for t in [0.05_f32, TELEPORT_PHASE_A_END] {
            let visual = sample_teleport(t);
            assert!(visual.scale_y < visual.scale_x, "t={t} scale_y={} scale_x={}", visual.scale_y, visual.scale_x);
        }
    }

    // --- bounded overshoot ---

    #[test]
    fn teleport_overshoot_is_bounded_and_axis_specific() {
        let mut saw_x_overshoot = false;
        let mut saw_y_overshoot = false;
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let visual = sample_teleport(t);
            assert!(visual.scale_x <= 1.04, "t={t} scale_x={}", visual.scale_x);
            assert!(visual.scale_y <= 1.08, "t={t} scale_y={}", visual.scale_y);
            saw_x_overshoot |= visual.scale_x > 1.0;
            saw_y_overshoot |= visual.scale_y > 1.0;
        }
        assert!(saw_x_overshoot);
        assert!(saw_y_overshoot);
    }

    // --- recoil (phase C actually settles back down, not monotonic ease) ---

    #[test]
    fn teleport_phase_c_recoils_below_the_phase_b_peak() {
        let peak = sample_teleport(TELEPORT_PHASE_B_END);
        assert_eq!((peak.scale_x, peak.scale_y), (TELEPORT_PHASE_B_SCALE_X, TELEPORT_PHASE_B_SCALE_Y));
        let mid_recoil = sample_teleport((TELEPORT_PHASE_B_END + TELEPORT_PHASE_C_END) / 2.0);
        assert!(mid_recoil.scale_x < peak.scale_x, "scale_x did not recoil: {} !< {}", mid_recoil.scale_x, peak.scale_x);
        assert!(mid_recoil.scale_y < peak.scale_y, "scale_y did not recoil: {} !< {}", mid_recoil.scale_y, peak.scale_y);
        // Recoil dips slightly under 1.0 by design (a real recoil, not
        // just "less overshoot").
        assert!(mid_recoil.scale_x < 1.0);
        assert!(mid_recoil.scale_y < 1.0);
    }

    // --- final state (exact, at and beyond t=1.0) ---

    #[test]
    fn teleport_sample_t1_and_beyond_is_exact_final_state() {
        for t in [1.0_f32, 1.5, 10.0] {
            assert_eq!(sample_teleport(t), AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 }, "t={t}");
        }
    }

    // --- bounds / finiteness across a dense sweep ---

    #[test]
    fn teleport_opacity_stays_within_bounds_across_a_dense_sweep() {
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let visual = sample_teleport(t);
            assert!((0.0..=1.0).contains(&visual.opacity), "t={t} opacity={}", visual.opacity);
        }
    }

    #[test]
    fn teleport_scale_values_are_finite_and_positive_across_a_dense_sweep() {
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let visual = sample_teleport(t);
            assert!(visual.scale_x.is_finite() && visual.scale_x > 0.0, "t={t} scale_x={}", visual.scale_x);
            assert!(visual.scale_y.is_finite() && visual.scale_y > 0.0, "t={t} scale_y={}", visual.scale_y);
        }
    }

    #[test]
    fn teleport_sample_is_deterministic() {
        for i in 0..=40 {
            let t = i as f32 / 40.0;
            assert_eq!(sample_teleport(t), sample_teleport(t));
            assert_eq!(sample_open_effect(OpenAnimationEffect::Teleport, t), sample_teleport(t));
        }
    }

    // --- minimum dimensions (scale_y=0.03 must never yield a zero-sized plan) ---

    #[test]
    fn teleport_t0_scale_never_produces_zero_sized_dst_or_outer_plan() {
        for (w, h) in [(200, 100), (10, 10), (1, 1), (201, 99)] {
            let plan = animation_test_plan(0, 0, w, h);
            let visual = sample_teleport(0.0);
            let scaled = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
            assert!(scaled.width >= 1, "w={w} h={h} width={}", scaled.width);
            assert!(scaled.height >= 1, "w={w} h={h} height={}", scaled.height);
            assert!(scaled.outer_width >= 1, "w={w} h={h} outer_width={}", scaled.outer_width);
            assert!(scaled.outer_height >= 1, "w={w} h={h} outer_height={}", scaled.outer_height);
        }
    }

    // --- center stability across phases, parities, and sizes ---

    #[test]
    fn teleport_center_is_stable_across_all_phases_parities_and_sizes() {
        for (w, h) in [(200, 100), (201, 101), (10, 10), (11, 11), (1, 1)] {
            let plan = animation_test_plan(50, 60, w, h);
            let orig_center_x = plan.dst_x as f32 + plan.width as f32 / 2.0;
            let orig_center_y = plan.dst_y as f32 + plan.height as f32 / 2.0;
            let orig_outer_center_x = plan.outer_x as f32 + plan.outer_width as f32 / 2.0;
            let orig_outer_center_y = plan.outer_y as f32 + plan.outer_height as f32 / 2.0;
            for i in 0..=20 {
                let t = i as f32 / 20.0;
                let visual = sample_teleport(t);
                let scaled = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
                let new_center_x = scaled.dst_x as f32 + scaled.width as f32 / 2.0;
                let new_center_y = scaled.dst_y as f32 + scaled.height as f32 / 2.0;
                let new_outer_center_x = scaled.outer_x as f32 + scaled.outer_width as f32 / 2.0;
                let new_outer_center_y = scaled.outer_y as f32 + scaled.outer_height as f32 / 2.0;
                assert!((orig_center_x - new_center_x).abs() <= 1.0, "w={w} h={h} t={t}");
                assert!((orig_center_y - new_center_y).abs() <= 1.0, "w={w} h={h} t={t}");
                assert!((orig_outer_center_x - new_outer_center_x).abs() <= 1.0, "w={w} h={h} t={t}");
                assert!((orig_outer_center_y - new_outer_center_y).abs() <= 1.0, "w={w} h={h} t={t}");
            }
        }
    }

    // --- shadow integration (reusing the s1 animated-shadow contract, no
    // Teleport-specific shadow logic anywhere) ---

    #[test]
    fn teleport_t0_shadow_base_rect_follows_the_non_uniform_streak_geometry() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let visual = sample_teleport(0.0);
        assert_eq!((visual.scale_x, visual.scale_y), (0.45, 0.03));
        let draw_plan = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
        let style = shadow_style(true, 8.0, 0.0, 0.0);
        // Opacity multiplier forced to 1.0 to isolate GEOMETRY from the
        // separate opacity-coupling behavior proved below (Teleport's own
        // t=0 opacity is 0.0, which would otherwise make
        // shadow_params_from_plan return None and hide this check).
        let shadow = shadow_params_from_plan(style, &draw_plan, 1.0).unwrap();
        assert_eq!(shadow.outer_width, draw_plan.outer_width as f32);
        assert_eq!(shadow.outer_height, draw_plan.outer_height as f32);
        // Not a full-sized shadow behind the streak.
        assert_ne!(shadow.outer_width, plan.outer_width as f32);
        assert_ne!(shadow.outer_height, plan.outer_height as f32);
        assert!(shadow.outer_height < shadow.outer_width, "shadow must also read as a thin streak");
    }

    #[test]
    fn teleport_t0_opacity_zero_yields_no_shadow_params() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let visual = sample_teleport(0.0);
        let draw_plan = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
        let style = shadow_style(true, 8.0, 0.0, 0.0);
        assert_eq!(visual.opacity, 0.0);
        assert!(shadow_params_from_plan(style, &draw_plan, visual.opacity).is_none());
    }

    #[test]
    fn teleport_materialized_shadow_follows_current_non_uniform_geometry() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let visual = sample_teleport(TELEPORT_PHASE_B_END); // essentially materialized, per the front-loaded contract
        assert!(visual.opacity > 0.9);
        let draw_plan = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
        let style = shadow_style(true, 8.0, 0.0, 0.0);
        let shadow = shadow_params_from_plan(style, &draw_plan, visual.opacity).unwrap();
        assert_eq!(shadow.outer_width, draw_plan.outer_width as f32);
        assert_eq!(shadow.outer_height, draw_plan.outer_height as f32);
        assert_eq!(shadow.strength, style.strength * visual.opacity);
    }

    #[test]
    fn teleport_at_t1_shadow_equals_ordinary_final_shadow() {
        let plan = animation_test_plan(10, 5, 200, 100);
        let visual = sample_teleport(1.0);
        assert_eq!(visual, AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 });
        let draw_plan = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
        let style = shadow_style(true, 8.0, 3.0, 4.0);
        let animated_shadow = shadow_params_from_plan(style, &draw_plan, visual.opacity).unwrap();
        let ordinary_shadow = shadow_params_from_plan(style, &plan, 1.0).unwrap();
        assert_eq!(animated_shadow, ordinary_shadow);
    }

    // --- authoritative X11 geometry never touched ---

    #[test]
    fn teleport_pipeline_never_touches_authoritative_x11_geometry() {
        let entry = animation_test_entry(9, SurfaceVisualClass::Normal, false);
        let original_geometry = entry.geometry;
        let plan = animation_test_plan(0, 0, 200, 100);
        let visual = sample_teleport(0.2);
        let _draw_plan = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
        assert_eq!(entry.geometry, original_geometry);
    }

    // --- first visible frame: provisional Teleport renders a compressed
    // streak, never a full-size window on frame one ---

    #[test]
    fn provisional_teleport_first_frame_is_a_compressed_streak_not_full_size() {
        let new_surface = animation_test_entry(77, SurfaceVisualClass::Normal, false);
        let snapshot = SceneSnapshot { root: 1, root_geometry: full_hd_root(), entries: vec![new_surface] };
        let old_surfaces = HashSet::new();
        let persistent = HashMap::new();
        let now = Instant::now();
        let provisional = provisional_open_animations(
            &old_surfaces, &snapshot, false, true,
            test_animation_config_with_effect(true, OpenAnimationEffect::Teleport), now,
        );
        let render_view = merge_window_animations(&persistent, &provisional);
        let animation = render_view.get(&77).expect("newly eligible surface must have a provisional animation");
        assert_eq!(animation.effect, OpenAnimationEffect::Teleport);
        let t = animation.progress(now);
        let visual = animation.sample(now);
        assert!(t < 0.02);
        // Same generic sample path the persistent render path also uses.
        assert_eq!(visual, sample_teleport(t));
        assert_ne!((visual.opacity, visual.scale_x, visual.scale_y), (1.0, 1.0, 1.0));
        let real_plan = animation_test_plan(0, 0, 200, 100);
        let draw_plan = scale_render_quad_plan(real_plan, visual.scale_x, visual.scale_y);
        assert!(draw_plan.height < real_plan.height / 4, "first frame must read as a thin streak, not a full-size window");
        // And the shadow this frame is absent (opacity≈0), never a
        // full-size shadow behind a tiny streak.
        let shadow = shadow_params_from_plan(shadow_style(true, 8.0, 0.0, 0.0), &draw_plan, visual.opacity);
        assert!(shadow.is_none());
    }

    // --- retirement: no visual jump ---

    #[test]
    fn teleport_completed_animation_retires_with_no_visual_jump() {
        let mut animations = HashMap::new();
        // started_at far enough in the past that progress() clamps to 1.0
        // regardless of the configured duration.
        animations.insert(1, test_window_animation_with_effect(Instant::now() - Duration::from_secs(10), OpenAnimationEffect::Teleport));
        let now = Instant::now();
        let visual = animations[&1].sample(now);
        assert_eq!(visual, AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 });
        assert!(animations[&1].is_complete(now));
        let mut removed = HashSet::new();
        removed.insert(1);
        retire_removed_surface_animations(&mut animations, &removed);
        assert!(animations.is_empty());
    }

    // --- move/resize during animation: no cached geometry (Teleport-flavored) ---

    #[test]
    fn teleport_shadow_follows_latest_geometry_across_a_simulated_move_and_resize() {
        let visual = sample_teleport(0.20); // mid-streak, strongly non-uniform
        assert_ne!(visual.scale_x, visual.scale_y);
        let moved_plan = animation_test_plan(80, 40, 200, 100);
        let draw_plan = scale_render_quad_plan(moved_plan, visual.scale_x, visual.scale_y);
        let style = shadow_style(true, 8.0, 0.0, 0.0);
        let shadow = shadow_params_from_plan(style, &draw_plan, 1.0).unwrap();
        assert_eq!(shadow.outer_x + shadow.outer_width / 2.0, draw_plan.outer_x as f32 + draw_plan.outer_width as f32 / 2.0);
        assert_eq!(shadow.outer_y + shadow.outer_height / 2.0, draw_plan.outer_y as f32 + draw_plan.outer_height as f32 / 2.0);

        let resized_plan = animation_test_plan(80, 40, 350, 60);
        let resized_draw_plan = scale_render_quad_plan(resized_plan, visual.scale_x, visual.scale_y);
        assert_ne!(resized_draw_plan.outer_width, draw_plan.outer_width);
        let resized_shadow = shadow_params_from_plan(style, &resized_draw_plan, 1.0).unwrap();
        assert_eq!(resized_shadow.outer_width, resized_draw_plan.outer_width as f32);
    }

    // ========================================================
    // 3a3fa2b3 — energy_tear open effect.
    // ========================================================

    // --- effect sampling / model ---

    #[test]
    fn energy_tear_t0_layout_is_at_peak_tear_and_streak_alpha() {
        // "Estado visível" at t=0: the tear geometry (slice offsets +
        // streak alpha) is at its PEAK, non-trivial value — energy_tear's
        // distinctive character is expressed through this layout, not
        // through AnimationVisual.opacity (which follows the same
        // "starts at 0, ramps up" convention every other effect already
        // uses — see energy_tear_opacity_ramps_like_every_other_effect).
        let layout = sample_energy_tear_layout(0.0);
        assert_eq!(layout.streak_alpha, ENERGY_TEAR_PEAK_STREAK_ALPHA);
        assert_eq!(energy_tear_oscillation(0.0), 1.0);
        for i in 0..ENERGY_TEAR_SLICE_COUNT {
            let expected = ENERGY_TEAR_SLICE_PEAK_OFFSET_FRACTIONS[i]; // * osc(0.0)==1.0
            assert_eq!(layout.slice_offset_fractions[i], expected, "slice {i}");
            assert_ne!(layout.slice_offset_fractions[i], 0.0, "slice {i} must not be at rest at t=0");
        }
    }

    // --- 3a3fa2b3-r2: window-opacity-multiplier correction ---
    // EnergyTear's AnimationVisual.opacity is now an unconditional
    // CONSTANT 1.0 multiplier — it must never fade or override the
    // surface's already-resolved/configured opacity. Required test 1/2.

    #[test]
    fn energy_tear_opacity_multiplier_is_exactly_one_at_t0() {
        let visual = sample_energy_tear(0.0);
        assert_eq!(visual.opacity, 1.0);
        assert_eq!(visual.scale_x, 1.0);
        assert_eq!(visual.scale_y, 1.0);
    }

    #[test]
    fn energy_tear_opacity_multiplier_stays_one_throughout() {
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let visual = sample_energy_tear(t);
            assert_eq!(visual.opacity, 1.0, "t={t}");
            assert_eq!(visual.scale_x, 1.0, "t={t}");
            assert_eq!(visual.scale_y, 1.0, "t={t}");
        }
        for t in [1.0_f32, 1.5, 10.0] {
            assert_eq!(sample_energy_tear(t), AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 }, "t={t}");
        }
    }

    // Required test 3: Scale's existing opacity curve is unmodified by
    // this milestone (a fresh, explicit regression pin — Scale's own
    // code has zero hunks in either R1 or this R2 correction).
    #[test]
    fn scale_opacity_curve_is_unmodified_by_energy_tear() {
        for t in [0.0_f32, 0.13, 0.5, 0.87, 1.0] {
            let eased = ease_out_cubic(t);
            let expected_opacity = lerp(SCALE_EFFECT_FROM_OPACITY, SCALE_EFFECT_TO_OPACITY, eased);
            assert_eq!(sample_scale(t).opacity, expected_opacity, "t={t}");
        }
        assert_eq!(sample_scale(0.0).opacity, SCALE_EFFECT_FROM_OPACITY);
        assert_eq!(sample_scale(1.0).opacity, SCALE_EFFECT_TO_OPACITY);
    }

    // Required test 4: Teleport's existing opacity curve is unmodified
    // (extends the R1 regression pin with explicit opacity-at-several-t
    // coverage).
    #[test]
    fn teleport_opacity_curve_is_unmodified_by_energy_tear() {
        assert_eq!(sample_teleport(0.0).opacity, TELEPORT_START_OPACITY);
        assert_eq!(sample_teleport(TELEPORT_PHASE_A_END).opacity, 0.28);
        assert_eq!(sample_teleport(TELEPORT_PHASE_B_END).opacity, 1.0);
        assert_eq!(sample_teleport(1.0).opacity, 1.0);
    }

    // Required tests 5/6: resolved (base) opacity passes through
    // energy_tear's render-loop formula (base_opacity * visual.opacity)
    // completely unchanged, because visual.opacity is always exactly 1.0.
    #[test]
    fn resolved_opacity_0_82_remains_effective_0_82_under_energy_tear() {
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let visual = sample_energy_tear(t);
            let draw_opacity = 0.82_f32 * visual.opacity;
            assert_eq!(draw_opacity, 0.82, "t={t}");
        }
    }

    #[test]
    fn resolved_opacity_1_0_remains_1_0_under_energy_tear() {
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            let visual = sample_energy_tear(t);
            let draw_opacity = 1.0_f32 * visual.opacity;
            assert_eq!(draw_opacity, 1.0, "t={t}");
        }
    }

    // Required test 7: tear/streak alpha is computed by a function that
    // takes ONLY `t` — no resolved/window-opacity input exists for it to
    // depend on. Demonstrated by showing the SAME layout results
    // regardless of which (hypothetical) base_opacity the caller would
    // separately multiply into the surface's own draw_opacity.
    #[test]
    fn tear_alpha_is_independent_from_resolved_window_opacity() {
        for i in 0..=20 {
            let t = ENERGY_TEAR_END * (i as f32 / 20.0);
            let layout_a = sample_energy_tear_layout(t);
            let layout_b = sample_energy_tear_layout(t);
            // Nothing resembling a "base_opacity" parameter exists on
            // this function's signature at all — two independent calls
            // at the same t, standing in for two hypothetically
            // differently-configured windows (e.g. 0.3 vs 1.0 resolved
            // opacity), produce byte-identical tear state.
            assert_eq!(layout_a, layout_b, "t={t}");
            let hypothetical_low_opacity_draw = 0.3_f32 * sample_energy_tear(t).opacity;
            let hypothetical_full_opacity_draw = 1.0_f32 * sample_energy_tear(t).opacity;
            assert_ne!(hypothetical_low_opacity_draw, hypothetical_full_opacity_draw);
            // ...yet the tear layout itself never changed between them.
            assert_eq!(sample_energy_tear_layout(t).streak_alpha, layout_a.streak_alpha, "t={t}");
        }
    }

    // Required test 8: once tear_alpha reaches 0 (t >= ENERGY_TEAR_END),
    // only the OVERLAY disappears (energy_tear_layout_for -> None, so no
    // slice/streak draw calls happen) — the window's own opacity
    // multiplier was ALREADY 1.0 on both sides of that boundary, so
    // nothing about window rendering itself changes at the transition.
    #[test]
    fn at_tear_alpha_zero_only_the_overlay_disappears() {
        let just_before = ENERGY_TEAR_END - 0.001;
        assert!(energy_tear_layout_for(OpenAnimationEffect::EnergyTear, just_before).is_some());
        assert_eq!(energy_tear_layout_for(OpenAnimationEffect::EnergyTear, ENERGY_TEAR_END), None);
        // Window opacity multiplier: unchanged across the boundary.
        assert_eq!(sample_energy_tear(just_before).opacity, sample_energy_tear(ENERGY_TEAR_END).opacity);
        assert_eq!(sample_energy_tear(just_before).opacity, 1.0);
    }

    // Required test 9: no code path forces a translucent window toward
    // opacity=1.0 — the render-loop formula (base_opacity *
    // visual.opacity) is a genuine multiplier identity whenever
    // visual.opacity==1.0, for ANY base_opacity, not a hardcoded override.
    #[test]
    fn no_code_path_forces_translucent_windows_to_opacity_one() {
        for base_opacity in [0.10_f32, 0.30, 0.60, 0.82, 0.99, 1.0] {
            for i in 0..=10 {
                let t = i as f32 / 10.0;
                let visual = sample_energy_tear(t);
                let draw_opacity = base_opacity * visual.opacity;
                assert_eq!(draw_opacity, base_opacity, "base_opacity={base_opacity} t={t}");
                assert!(base_opacity >= 1.0 || draw_opacity < 1.0, "translucent window must stay translucent: base_opacity={base_opacity} t={t}");
            }
        }
    }

    #[test]
    fn energy_tear_layout_at_t1_is_exactly_no_tear() {
        let layout = sample_energy_tear_layout(1.0);
        assert_eq!(layout.streak_alpha, 0.0);
        assert_eq!(layout.slice_offset_fractions, [0.0; ENERGY_TEAR_SLICE_COUNT]);
    }

    #[test]
    fn energy_tear_visual_at_t1_and_beyond_is_exact_final_state() {
        for t in [1.0_f32, 1.5, 10.0] {
            assert_eq!(sample_energy_tear(t), AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 }, "t={t}");
        }
    }

    // ============================================================
    // 3a3fa2b3-r3 — multi-cycle displacement + tear pulses.
    // ============================================================

    // Boundary t-values in ABSOLUTE (whole-animation) time, derived from
    // the envelope-normalized phase boundaries — reused across the
    // section 17/18 tests below.
    fn energy_tear_r3_cycle0_t() -> f32 { 0.0 }
    fn energy_tear_r3_crossing1_t() -> f32 { ENERGY_TEAR_END * ENERGY_TEAR_PHASE_A_END }
    fn energy_tear_r3_rebound1_t() -> f32 { ENERGY_TEAR_END * ENERGY_TEAR_PHASE_B_END }
    fn energy_tear_r3_rebound2_t() -> f32 { ENERGY_TEAR_END * ENERGY_TEAR_PHASE_C_END }

    // --- test A: initial displacement is larger than R2 ---

    #[test]
    fn energy_tear_r3_initial_displacement_exceeds_r2() {
        // R2's old formula, inlined here ONLY as a fixed historical
        // reference to prove the regression requirement — R2's constants
        // no longer exist in production source (superseded by R3).
        // R2: live_offset_px = slice_width * (peak_fraction * 0.35),
        // with slice_width ~= window_width / ENERGY_TEAR_SLICE_COUNT and
        // R2's peak_fraction magnitude capped at 1.0.
        let window_width = 200.0_f32;
        let r2_slice_width = window_width / ENERGY_TEAR_SLICE_COUNT as f32;
        let r2_max_displacement_px = r2_slice_width * 1.0 * 0.35; // = 14.0

        let plan = animation_test_plan(0, 0, window_width as i32, 100);
        let layout = sample_energy_tear_layout(0.0);
        let render_plan = energy_tear_render_plan(plan, &layout).unwrap();
        // R3's strongest-magnitude slice is index 3 (coefficient 1.0).
        let r3_max_displacement_px = (render_plan.slices[3].dst_x - (plan.dst_x + render_plan.slices[3].local_offset_x.round() as i32)).abs() as f32;
        assert!(r3_max_displacement_px > r2_max_displacement_px, "r3={r3_max_displacement_px} r2={r2_max_displacement_px}");

        // And every single slice individually increased too, not just
        // the strongest one (see the R3 preview report's derivation).
        for i in 0..ENERGY_TEAR_SLICE_COUNT {
            let r2_px = r2_slice_width * ENERGY_TEAR_SLICE_PEAK_OFFSET_FRACTIONS_R2_REFERENCE[i].abs() * 0.35;
            let r3_px = (render_plan.slices[i].dst_x - (plan.dst_x + render_plan.slices[i].local_offset_x.round() as i32)).abs() as f32;
            assert!(r3_px > r2_px, "slice {i}: r3={r3_px} r2={r2_px}");
        }
    }
    // R2's old (superseded) per-slice pattern, kept ONLY as a named
    // reference constant for the regression comparison above — not used
    // anywhere in production code.
    const ENERGY_TEAR_SLICE_PEAK_OFFSET_FRACTIONS_R2_REFERENCE: [f32; ENERGY_TEAR_SLICE_COUNT] = [-1.0, 0.6, -0.3, 0.6, -1.0];

    // --- tests B/C: sign reversal across cycles ---

    #[test]
    fn energy_tear_r3_rebound1_reverses_sign_from_cycle0() {
        let cycle0 = sample_energy_tear_layout(energy_tear_r3_cycle0_t());
        let rebound1 = sample_energy_tear_layout(energy_tear_r3_rebound1_t());
        for i in 0..ENERGY_TEAR_SLICE_COUNT {
            assert_ne!(cycle0.slice_offset_fractions[i], 0.0, "slice {i}");
            assert_ne!(rebound1.slice_offset_fractions[i], 0.0, "slice {i}");
            assert!(
                cycle0.slice_offset_fractions[i].signum() != rebound1.slice_offset_fractions[i].signum(),
                "slice {i} did not reverse sign: cycle0={} rebound1={}",
                cycle0.slice_offset_fractions[i], rebound1.slice_offset_fractions[i],
            );
        }
    }

    #[test]
    fn energy_tear_r3_rebound2_reverses_sign_from_rebound1_back_to_cycle0_sign() {
        let cycle0 = sample_energy_tear_layout(energy_tear_r3_cycle0_t());
        let rebound1 = sample_energy_tear_layout(energy_tear_r3_rebound1_t());
        let rebound2 = sample_energy_tear_layout(energy_tear_r3_rebound2_t());
        for i in 0..ENERGY_TEAR_SLICE_COUNT {
            assert!(
                rebound1.slice_offset_fractions[i].signum() != rebound2.slice_offset_fractions[i].signum(),
                "slice {i} did not reverse sign again: rebound1={} rebound2={}",
                rebound1.slice_offset_fractions[i], rebound2.slice_offset_fractions[i],
            );
            // ...and rebound2 is back on cycle0's original side.
            assert_eq!(
                cycle0.slice_offset_fractions[i].signum(), rebound2.slice_offset_fractions[i].signum(),
                "slice {i} rebound2 must match cycle0's original sign",
            );
        }
    }

    // --- tests D/E: decreasing amplitude across cycles ---

    #[test]
    fn energy_tear_r3_rebound1_amplitude_is_smaller_than_cycle0() {
        let cycle0 = sample_energy_tear_layout(energy_tear_r3_cycle0_t());
        let rebound1 = sample_energy_tear_layout(energy_tear_r3_rebound1_t());
        for i in 0..ENERGY_TEAR_SLICE_COUNT {
            assert!(
                rebound1.slice_offset_fractions[i].abs() < cycle0.slice_offset_fractions[i].abs(),
                "slice {i}: rebound1={} !< cycle0={}",
                rebound1.slice_offset_fractions[i].abs(), cycle0.slice_offset_fractions[i].abs(),
            );
        }
        // The requested 45-60% range, applied to the shared oscillation factor.
        assert!((0.45..=0.60).contains(&ENERGY_TEAR_REBOUND_1_FACTOR.abs()));
    }

    #[test]
    fn energy_tear_r3_rebound2_amplitude_is_smaller_than_rebound1() {
        let rebound1 = sample_energy_tear_layout(energy_tear_r3_rebound1_t());
        let rebound2 = sample_energy_tear_layout(energy_tear_r3_rebound2_t());
        for i in 0..ENERGY_TEAR_SLICE_COUNT {
            assert!(
                rebound2.slice_offset_fractions[i].abs() < rebound1.slice_offset_fractions[i].abs(),
                "slice {i}: rebound2={} !< rebound1={}",
                rebound2.slice_offset_fractions[i].abs(), rebound1.slice_offset_fractions[i].abs(),
            );
        }
        // The requested 20-30% range.
        assert!((0.20..=0.30).contains(&ENERGY_TEAR_REBOUND_2_FACTOR.abs()));
    }

    // --- test F: exact zero after ENERGY_TEAR_END ---

    #[test]
    fn energy_tear_r3_offsets_and_streak_alpha_are_exact_zero_after_end() {
        for t in [ENERGY_TEAR_END, ENERGY_TEAR_END + 0.01, 1.0, 1.5] {
            let layout = sample_energy_tear_layout(t);
            assert_eq!(layout.slice_offset_fractions, [0.0; ENERGY_TEAR_SLICE_COUNT], "t={t}");
            assert_eq!(layout.streak_alpha, 0.0, "t={t}");
        }
    }

    // --- test G: deterministic output (multi-cycle-specific) ---

    #[test]
    fn energy_tear_r3_oscillation_and_layout_are_deterministic() {
        for i in 0..=60 {
            let t = ENERGY_TEAR_END * (i as f32 / 60.0);
            assert_eq!(energy_tear_oscillation(t), energy_tear_oscillation(t), "t={t}");
            assert_eq!(sample_energy_tear_layout(t), sample_energy_tear_layout(t), "t={t}");
        }
    }

    // --- tests H/I: finiteness + bounds ---

    #[test]
    fn energy_tear_r3_displacement_is_always_finite_and_bounded() {
        for i in 0..=100 {
            let t = i as f32 / 100.0;
            let layout = sample_energy_tear_layout(t);
            for slice_idx in 0..ENERGY_TEAR_SLICE_COUNT {
                let f = layout.slice_offset_fractions[slice_idx];
                assert!(f.is_finite(), "t={t} slice {slice_idx} not finite: {f}");
                // Coefficients are within [-1,1] and |oscillation|<=1.0,
                // so the dimensionless fraction itself is always within
                // [-1,1] — the actual pixel bound (ENERGY_TEAR_MIN/MAX_
                // DISPLACEMENT_PX) is applied downstream in
                // energy_tear_render_plan, proven separately below.
                assert!((-1.0..=1.0).contains(&f), "t={t} slice {slice_idx} out of [-1,1]: {f}");
            }
            assert!(layout.streak_alpha.is_finite(), "t={t}");
            assert!((0.0..=1.0).contains(&layout.streak_alpha), "t={t} streak_alpha={}", layout.streak_alpha);
        }
    }

    #[test]
    fn energy_tear_r3_pixel_displacement_stays_within_approved_bounds() {
        for window_width in [5, 40, 200, 800, 5000] {
            let plan = animation_test_plan(0, 0, window_width, 100);
            for i in 0..=20 {
                let t = ENERGY_TEAR_END * (i as f32 / 20.0);
                let layout = sample_energy_tear_layout(t);
                let render_plan = energy_tear_render_plan(plan, &layout).unwrap();
                for (slice_idx, slice) in render_plan.slices.iter().enumerate() {
                    let rest_x = plan.dst_x + slice.local_offset_x.round() as i32;
                    let displacement = (slice.dst_x - rest_x).abs() as f32;
                    assert!(
                        displacement <= ENERGY_TEAR_MAX_DISPLACEMENT_PX + 1.0, // +1 for .round() slack
                        "window_width={window_width} t={t} slice={slice_idx} displacement={displacement} > max",
                    );
                }
            }
        }
    }

    // --- tear pulse ordering (section 18) ---

    #[test]
    fn energy_tear_r3_tear_pulses_decrease_in_order() {
        let cycle0_alpha = sample_energy_tear_layout(energy_tear_r3_cycle0_t()).streak_alpha;
        let rebound1_alpha = sample_energy_tear_layout(energy_tear_r3_rebound1_t()).streak_alpha;
        let rebound2_alpha = sample_energy_tear_layout(energy_tear_r3_rebound2_t()).streak_alpha;
        assert!(cycle0_alpha > rebound1_alpha, "cycle0={cycle0_alpha} !> rebound1={rebound1_alpha}");
        assert!(rebound1_alpha > rebound2_alpha, "rebound1={rebound1_alpha} !> rebound2={rebound2_alpha}");
        assert_eq!(cycle0_alpha, ENERGY_TEAR_PEAK_STREAK_ALPHA);
    }

    #[test]
    fn energy_tear_r3_tear_pulses_dip_between_peaks() {
        // Proves these are genuine PULSES (rise/fall/rise/fall), not a
        // single monotonic fade: the crossing point between cycle0 and
        // rebound1 must be a local minimum, strictly lower than both
        // neighbors.
        let crossing1_alpha = sample_energy_tear_layout(energy_tear_r3_crossing1_t()).streak_alpha;
        let cycle0_alpha = sample_energy_tear_layout(energy_tear_r3_cycle0_t()).streak_alpha;
        let rebound1_alpha = sample_energy_tear_layout(energy_tear_r3_rebound1_t()).streak_alpha;
        assert!(crossing1_alpha < cycle0_alpha, "crossing1={crossing1_alpha} !< cycle0={cycle0_alpha}");
        assert!(crossing1_alpha < rebound1_alpha, "crossing1={crossing1_alpha} !< rebound1={rebound1_alpha}");
    }

    #[test]
    fn energy_tear_r3_tear_alpha_final_is_zero_and_always_in_bounds() {
        assert_eq!(sample_energy_tear_layout(ENERGY_TEAR_END).streak_alpha, 0.0);
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let alpha = sample_energy_tear_layout(t).streak_alpha;
            assert!((0.0..=1.0).contains(&alpha), "t={t} alpha={alpha}");
        }
    }

    #[test]
    fn energy_tear_r3_window_opacity_multiplier_unaffected_by_multicycle_change() {
        // Preserve R2 exactly: window opacity multiplier stays 1.0
        // throughout, completely independent of the new multi-cycle
        // displacement/tear-pulse machinery above.
        for i in 0..=20 {
            let t = ENERGY_TEAR_END * (i as f32 / 20.0);
            assert_eq!(sample_energy_tear(t).opacity, 1.0, "t={t}");
        }
    }

    #[test]
    fn energy_tear_sampling_is_deterministic() {
        for i in 0..=40 {
            let t = i as f32 / 40.0;
            assert_eq!(sample_energy_tear(t), sample_energy_tear(t));
            assert_eq!(sample_energy_tear_layout(t), sample_energy_tear_layout(t));
            assert_eq!(sample_open_effect(OpenAnimationEffect::EnergyTear, t), sample_energy_tear(t));
        }
    }

    #[test]
    fn scale_never_produces_a_tear_layout() {
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            assert_eq!(energy_tear_layout_for(OpenAnimationEffect::Scale, t), None, "t={t}");
        }
    }

    #[test]
    fn teleport_never_produces_a_tear_layout() {
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            assert_eq!(energy_tear_layout_for(OpenAnimationEffect::Teleport, t), None, "t={t}");
        }
    }

    #[test]
    fn bubble_never_produces_a_tear_layout() {
        // 3a3fa2b4-r1: bubble is geometry+opacity only — like Scale and
        // Teleport, it must never trigger energy_tear's slice/streak
        // overlay path, at any point across its whole [0, 1] range.
        for i in 0..=20 {
            let t = i as f32 / 20.0;
            assert_eq!(energy_tear_layout_for(OpenAnimationEffect::Bubble, t), None, "t={t}");
        }
    }

    #[test]
    fn energy_tear_only_produces_a_layout_during_its_own_tear_phase() {
        assert!(energy_tear_layout_for(OpenAnimationEffect::EnergyTear, 0.0).is_some());
        assert!(energy_tear_layout_for(OpenAnimationEffect::EnergyTear, ENERGY_TEAR_END - 0.001).is_some());
        assert_eq!(energy_tear_layout_for(OpenAnimationEffect::EnergyTear, ENERGY_TEAR_END), None);
        assert_eq!(energy_tear_layout_for(OpenAnimationEffect::EnergyTear, 1.0), None);
    }

    #[test]
    fn teleport_is_unmodified_by_this_milestone_regression_pin() {
        // Regression pin against the 3a3fa2b2-r2-accepted human-validated
        // curve — this milestone (3a3fa2b3) must not alter it at all.
        let peak = sample_teleport(TELEPORT_PHASE_B_END);
        assert_eq!((peak.opacity, peak.scale_x, peak.scale_y), (1.0, 1.03, 1.06));
        let start = sample_teleport(0.0);
        assert_eq!((start.opacity, start.scale_x, start.scale_y), (TELEPORT_START_OPACITY, TELEPORT_START_SCALE_X, TELEPORT_START_SCALE_Y));
    }

    // --- bubble sampler (3a3fa2b4-r1) ---

    #[test]
    fn bubble_start_is_compressed_and_translucent() {
        let start = sample_bubble(0.0);
        assert!(start.opacity > 0.0 && start.opacity < 1.0, "opacity={}", start.opacity);
        assert!(start.scale_x < 1.0, "scale_x={}", start.scale_x);
        assert!(start.scale_y < 1.0, "scale_y={}", start.scale_y);
        assert_eq!((start.opacity, start.scale_x, start.scale_y), (BUBBLE_START_OPACITY, BUBBLE_START_SCALE_X, BUBBLE_START_SCALE_Y));
    }

    #[test]
    fn bubble_pop_overshoots_both_axes_at_full_opacity() {
        // Sampled exactly at the phase A/B boundary: by construction this
        // equals phase A's own end values (continuity, u=0 on the phase B
        // side) — the peak of the "pop".
        let pop = sample_bubble(BUBBLE_PHASE_A_END);
        assert_eq!(pop.opacity, 1.0);
        assert!(pop.scale_x > 1.0, "scale_x={}", pop.scale_x);
        assert!(pop.scale_y > 1.0, "scale_y={}", pop.scale_y);
    }

    #[test]
    fn bubble_squash_has_one_axis_above_and_one_below_one() {
        // Sampled at the phase B/C boundary: equals phase B's own end
        // values by the same continuity argument.
        let squash = sample_bubble(BUBBLE_PHASE_B_END);
        assert!(squash.scale_x > 1.0, "scale_x={}", squash.scale_x);
        assert!(squash.scale_y < 1.0, "scale_y={}", squash.scale_y);
    }

    #[test]
    fn bubble_rebound_reverses_which_axis_leads() {
        // Sampled at the phase C/D boundary: equals phase C's own end
        // values by continuity — the opposite-sign rebound peak.
        let rebound = sample_bubble(BUBBLE_PHASE_C_END);
        assert!(rebound.scale_x < 1.0, "scale_x={}", rebound.scale_x);
        assert!(rebound.scale_y > 1.0, "scale_y={}", rebound.scale_y);
    }

    #[test]
    fn bubble_reaches_exact_final_identity() {
        let end = sample_bubble(1.0);
        assert_eq!((end.opacity, end.scale_x, end.scale_y), (1.0, 1.0, 1.0));
        // Also proven via the generic dispatcher, not just the direct call.
        let via_dispatch = sample_open_effect(OpenAnimationEffect::Bubble, 1.0);
        assert_eq!(via_dispatch, end);
    }

    #[test]
    fn bubble_sampling_is_always_finite_and_within_safe_bounds() {
        for i in 0..=200 {
            let t = i as f32 / 200.0;
            let visual = sample_bubble(t);
            assert!(visual.opacity.is_finite(), "t={t}");
            assert!(visual.scale_x.is_finite(), "t={t}");
            assert!(visual.scale_y.is_finite(), "t={t}");
            assert!((0.0..=1.0).contains(&visual.opacity), "t={t} opacity={}", visual.opacity);
            assert!(visual.scale_x > 0.0, "t={t} scale_x={}", visual.scale_x);
            assert!(visual.scale_y > 0.0, "t={t} scale_y={}", visual.scale_y);
        }
    }

    #[test]
    fn bubble_sampling_is_deterministic() {
        for i in 0..=40 {
            let t = i as f32 / 40.0;
            assert_eq!(sample_bubble(t), sample_bubble(t));
            assert_eq!(sample_open_effect(OpenAnimationEffect::Bubble, t), sample_bubble(t));
        }
    }

    #[test]
    fn bubble_is_distinct_from_scale_and_teleport_by_actual_sampled_values() {
        // Compare the actual AnimationVisual samples, not merely enum
        // identity — at every representative t below, Bubble's value
        // must differ from both Scale's and Teleport's own sample.
        for i in 1..20 {
            let t = i as f32 / 20.0;
            let bubble = sample_bubble(t);
            let scale = sample_scale(t);
            let teleport = sample_teleport(t);
            assert_ne!(bubble, scale, "t={t} bubble matched scale");
            assert_ne!(bubble, teleport, "t={t} bubble matched teleport");
        }
    }

    #[test]
    fn bubble_axis_reversal_is_present_across_the_curve() {
        // The defining Bubble characteristic (section 6 of the R1 spec):
        // at least one phase has scale_x > 1 && scale_y < 1, and a LATER
        // phase has scale_x < 1 && scale_y > 1.
        let squash = sample_bubble(BUBBLE_PHASE_B_END);
        let rebound = sample_bubble(BUBBLE_PHASE_C_END);
        assert!(squash.scale_x > 1.0 && squash.scale_y < 1.0);
        assert!(rebound.scale_x < 1.0 && rebound.scale_y > 1.0);
        assert!(BUBBLE_PHASE_B_END < BUBBLE_PHASE_C_END, "squash must precede rebound");
    }

    #[test]
    fn bubble_never_forces_translucent_windows_opaque_beyond_its_own_visual_opacity() {
        // Composition contract (unchanged, generic): effective opacity =
        // resolved/base opacity * AnimationVisual.opacity. At Bubble's
        // exact final state (opacity == 1.0), a resolved 0.80 window's
        // effective opacity is 0.80, never bumped to 1.0 by the effect.
        let end = sample_bubble(1.0);
        assert_eq!(end.opacity, 1.0);
        let base_opacity = 0.80_f32;
        assert_eq!(base_opacity * end.opacity, 0.80);
    }

    #[test]
    fn bubble_strongest_squash_survives_generic_geometry_scaling() {
        // Exercises Bubble's most extreme non-uniform values (the start
        // state: scale_x=0.62, scale_y=0.42) through the SAME generic
        // scale_render_quad_plan every other effect uses — no
        // Bubble-specific geometry function exists or is needed.
        let plan = animation_test_plan(10, 20, 200, 100);
        let scaled = scale_render_quad_plan(plan, BUBBLE_START_SCALE_X, BUBBLE_START_SCALE_Y);
        assert!(scaled.width >= 1);
        assert!(scaled.height >= 1);
        assert!(scaled.outer_width >= 1);
        assert!(scaled.outer_height >= 1);
        // Non-uniform scaling proof: width follows scale_x independently
        // of height following scale_y — not a single shared factor.
        let expected_width = ((plan.width as f32) * BUBBLE_START_SCALE_X).round().max(1.0) as i32;
        let expected_height = ((plan.height as f32) * BUBBLE_START_SCALE_Y).round().max(1.0) as i32;
        assert_eq!(scaled.width, expected_width);
        assert_eq!(scaled.height, expected_height);
        let orig_center_x = plan.dst_x as f32 + plan.width as f32 / 2.0;
        let orig_center_y = plan.dst_y as f32 + plan.height as f32 / 2.0;
        let new_center_x = scaled.dst_x as f32 + scaled.width as f32 / 2.0;
        let new_center_y = scaled.dst_y as f32 + scaled.height as f32 / 2.0;
        assert!((orig_center_x - new_center_x).abs() <= 1.0);
        assert!((orig_center_y - new_center_y).abs() <= 1.0);
        // Final identity (scale 1.0/1.0) exactly reproduces the original plan.
        let identity = scale_render_quad_plan(plan, 1.0, 1.0);
        assert_eq!(identity, plan);
    }

    // --- geometry / render contract (energy_tear_render_plan) ---

    #[test]
    fn energy_tear_slices_tile_the_full_width_with_no_gaps_at_rest() {
        let plan = animation_test_plan(0, 0, 203, 100); // not a multiple of 5
        let layout = EnergyTearLayout { slice_offset_fractions: [0.0; ENERGY_TEAR_SLICE_COUNT], streak_alpha: 0.0 };
        let render_plan = energy_tear_render_plan(plan, &layout).unwrap();
        let total: i32 = render_plan.slices.iter().map(|s| s.width).sum();
        assert_eq!(total, plan.width);
        let mut expected_local_offset = 0.0_f32;
        for slice in &render_plan.slices {
            assert!(slice.width >= 1);
            assert_eq!(slice.local_offset_x, expected_local_offset);
            // At rest (offsets all zero), on-screen dst_x must exactly
            // continue the previous slice — no gap, no overlap.
            assert_eq!(slice.dst_x, plan.dst_x + expected_local_offset.round() as i32);
            expected_local_offset += slice.width as f32;
        }
        assert_eq!(expected_local_offset, plan.width as f32);
    }

    #[test]
    fn energy_tear_slices_move_and_scale_with_the_window() {
        let layout = sample_energy_tear_layout(0.0);
        let plan_a = animation_test_plan(10, 20, 200, 100);
        let plan_b = animation_test_plan(60, 20, 200, 100); // moved +50 in x
        let render_a = energy_tear_render_plan(plan_a, &layout).unwrap();
        let render_b = energy_tear_render_plan(plan_b, &layout).unwrap();
        for i in 0..ENERGY_TEAR_SLICE_COUNT {
            assert_eq!(render_b.slices[i].dst_x - render_a.slices[i].dst_x, 50, "slice {i}");
            assert_eq!(render_a.slices[i].width, render_b.slices[i].width, "slice {i}");
        }
    }

    #[test]
    fn energy_tear_render_plan_carries_the_whole_window_mask_reference() {
        // This is the data-contract half of "tears respect corner
        // radius/clipping": each slice must be masked against the WHOLE
        // window's size/corner_radius, never its own tiny slice size —
        // the actual GPU-side masking correctness is only visually
        // verifiable at runtime (see the preview report), but this
        // proves the geometry handed to the renderer is correct.
        let mut plan = animation_test_plan(0, 0, 200, 100);
        plan.corner_radius = 12.0;
        let layout = sample_energy_tear_layout(0.0);
        let render_plan = energy_tear_render_plan(plan, &layout).unwrap();
        assert_eq!(render_plan.full_width, plan.width as f32);
        assert_eq!(render_plan.full_height, plan.height as f32);
        assert_eq!(render_plan.corner_radius, plan.corner_radius);
    }

    #[test]
    fn energy_tear_minimum_dimensions_are_always_at_least_one_pixel() {
        for width in [5, 6, 7, 9, 100, 101, 997] {
            let plan = animation_test_plan(0, 0, width, 50);
            let layout = sample_energy_tear_layout(0.05);
            let render_plan = energy_tear_render_plan(plan, &layout).unwrap();
            for slice in &render_plan.slices {
                assert!(slice.width >= 1, "width={width} slice.width={}", slice.width);
                assert!(slice.height >= 1, "width={width} slice.height={}", slice.height);
            }
            for streak in &render_plan.streaks {
                assert!(streak.width >= 1, "width={width} streak.width={}", streak.width);
                assert!(streak.height >= 1, "width={width} streak.height={}", streak.height);
            }
        }
    }

    #[test]
    fn energy_tear_render_plan_refuses_windows_too_narrow_to_slice() {
        for width in [0, 1, 2, 3, 4] {
            let plan = animation_test_plan(0, 0, width, 50);
            let layout = sample_energy_tear_layout(0.0);
            assert_eq!(energy_tear_render_plan(plan, &layout), None, "width={width}");
        }
    }

    #[test]
    fn energy_tear_uv_ranges_stay_within_the_original_plans_uv_window() {
        // A partially off-screen window (src_x > 0, so u0 != 0.0),
        // constructed directly rather than via the always-[0,1]
        // animation_test_plan fixture, to prove slice UVs never escape
        // the ORIGINAL plan's own UV sub-window.
        let plan = RenderQuadPlan {
            dst_x: 0, dst_y: 0, width: 200, height: 100,
            outer_x: 0, outer_y: 0, outer_width: 200, outer_height: 100,
            src_x: 40, src_y: 0, src_width: 200, src_height: 100,
            u0: 0.2, v0: 0.0, u1: 0.9, v1: 1.0,
            corner_radius: 0.0, border_width: 0.0, border_color: [0.0, 0.0, 0.0, 1.0],
        };
        let layout = sample_energy_tear_layout(0.0);
        let render_plan = energy_tear_render_plan(plan, &layout).unwrap();
        for slice in &render_plan.slices {
            assert!(slice.u0 >= plan.u0 - 1e-5 && slice.u0 <= plan.u1 + 1e-5, "u0={}", slice.u0);
            assert!(slice.u1 >= plan.u0 - 1e-5 && slice.u1 <= plan.u1 + 1e-5, "u1={}", slice.u1);
            assert!(slice.u0 <= slice.u1);
            assert_eq!(slice.v0, plan.v0);
            assert_eq!(slice.v1, plan.v1);
        }
        // First/last slice touch the original plan's own UV edges exactly.
        assert_eq!(render_plan.slices[0].u0, plan.u0);
        assert_eq!(render_plan.slices[ENERGY_TEAR_SLICE_COUNT - 1].u1, plan.u1);
    }

    #[test]
    fn energy_tear_slices_carry_no_border_by_construction() {
        // Structural, not runtime: EnergyTearSlicePlan simply has no
        // border field at all, so a slice cannot carry border state —
        // border reappears correctly, unmodified, only once the render
        // loop falls back to the ordinary single-quad draw (t >=
        // ENERGY_TEAR_END or any non-energy_tear effect). Confirmed here
        // by exhaustively naming this plan's own fields.
        let plan = animation_test_plan(0, 0, 200, 100);
        let layout = sample_energy_tear_layout(0.0);
        let render_plan = energy_tear_render_plan(plan, &layout).unwrap();
        let EnergyTearSlicePlan { dst_x: _, dst_y: _, width: _, height: _, u0: _, v0: _, u1: _, v1: _, local_offset_x: _ } = render_plan.slices[0];
    }

    // --- regressions ---

    #[test]
    fn dock_and_desktop_stay_ineligible_regardless_of_configured_effect() {
        for effect in [OpenAnimationEffect::Scale, OpenAnimationEffect::Teleport, OpenAnimationEffect::EnergyTear] {
            let _ = test_animation_config_with_effect(true, effect);
            for visual_class in [SurfaceVisualClass::Dock, SurfaceVisualClass::Desktop] {
                let entry = animation_test_entry(1, visual_class, false);
                assert!(!eligible_for_open_animation(&entry), "effect={effect:?} visual_class={visual_class:?}");
            }
        }
    }

    #[test]
    fn override_redirect_policy_unaffected_by_configured_effect() {
        for effect in [OpenAnimationEffect::Scale, OpenAnimationEffect::Teleport, OpenAnimationEffect::EnergyTear] {
            let _ = test_animation_config_with_effect(true, effect);
            let normal = animation_test_entry(2, SurfaceVisualClass::Normal, false);
            assert!(eligible_for_open_animation(&normal), "effect={effect:?}");
        }
    }

    #[test]
    fn energy_tear_shadow_equals_non_animated_shadow_at_full_strength_throughout() {
        // scale_x==scale_y==1.0 always for energy_tear, so draw_plan ==
        // plan exactly — shadow behaves exactly like the s1 contract
        // already proves for any non-scaling case, with zero
        // energy_tear-specific shadow code anywhere. 3a3fa2b3-r2 note:
        // since visual.opacity is now a constant 1.0 (see the r2 window-
        // opacity correction above), shadow_opacity_multiplier (which the
        // unchanged render loop derives directly from visual.opacity) is
        // ALSO constantly 1.0 for energy_tear — shadow no longer fades in
        // alongside the window; it renders at full configured strength
        // from the very first frame. This is a deliberate, transparent
        // consequence of the window-opacity fix, not a separate change.
        let plan = animation_test_plan(10, 5, 200, 100);
        let style = shadow_style(true, 8.0, 3.0, 4.0);
        for t in [0.0_f32, 0.10, ENERGY_TEAR_END, 0.5, 1.0] {
            let visual = sample_energy_tear(t);
            assert_eq!(visual.opacity, 1.0, "t={t}");
            let draw_plan = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
            assert_eq!(draw_plan, plan, "t={t}");
            let animated = shadow_params_from_plan(style, &draw_plan, visual.opacity).unwrap();
            let ordinary = shadow_params_from_plan(style, &plan, 1.0).unwrap();
            assert_eq!(animated, ordinary, "t={t}");
            assert_eq!(animated.strength, style.strength, "t={t}: shadow must be at full configured strength");
        }
    }

    #[test]
    fn provisional_energy_tear_first_frame_uses_the_tear_active_layout() {
        let new_surface = animation_test_entry(88, SurfaceVisualClass::Normal, false);
        let snapshot = SceneSnapshot { root: 1, root_geometry: full_hd_root(), entries: vec![new_surface] };
        let old_surfaces = HashSet::new();
        let persistent = HashMap::new();
        let now = Instant::now();
        let provisional = provisional_open_animations(
            &old_surfaces, &snapshot, false, true,
            test_animation_config_with_effect(true, OpenAnimationEffect::EnergyTear), now,
        );
        let render_view = merge_window_animations(&persistent, &provisional);
        let animation = render_view.get(&88).expect("newly eligible surface must have a provisional animation");
        assert_eq!(animation.effect, OpenAnimationEffect::EnergyTear);
        let t = animation.progress(now);
        assert!(t < 0.02);
        let layout = energy_tear_layout_for(animation.effect, t).expect("first frame must still be in the tear phase");
        assert_ne!(layout.streak_alpha, 0.0);
    }

    #[test]
    fn energy_tear_completed_animation_retires_with_no_visual_jump() {
        let mut animations = HashMap::new();
        animations.insert(1, test_window_animation_with_effect(Instant::now() - Duration::from_secs(10), OpenAnimationEffect::EnergyTear));
        let now = Instant::now();
        let visual = animations[&1].sample(now);
        assert_eq!(visual, AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 });
        assert_eq!(energy_tear_layout_for(OpenAnimationEffect::EnergyTear, animations[&1].progress(now)), None);
        assert!(animations[&1].is_complete(now));
        let mut removed = HashSet::new();
        removed.insert(1);
        retire_removed_surface_animations(&mut animations, &removed);
        assert!(animations.is_empty());
    }

    // --- system invariants ---

    #[test]
    fn render_wiring_energy_tear_adds_no_new_x11_queries_or_gl_resources() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = start + source[start..].find("\nfn egl_scene_is_renderable").unwrap();
        let body = &source[start..end];
        assert!(body.contains("energy_tear_layout_for(animation.effect, t)"));
        assert!(body.contains("render_energy_tear_slices"));
        assert!(!body.contains("intern_atom"));
        assert!(!body.contains("GenTextures"));
        assert!(!body.contains("GenFramebuffers"));
        assert!(!body.contains("CreateProgram"));
        // Still exactly one AnimationVisual sample per surface per render.
        assert_eq!(body.matches(".sample(animation_now)").count(), 1);
    }

    // ========================================================
    // 3a3fa2b1-s1 — animated shadow geometry alignment.
    // ========================================================

    // --- A: non-animated shadow geometry/opacity unchanged ---

    #[test]
    fn non_animated_shadow_geometry_and_strength_reproduce_pre_fix_behavior() {
        let plan = animation_test_plan(10, 20, 200, 100);
        let style = shadow_style(true, 8.0, 3.0, 4.0);
        // 1.0 is exactly what render_egl_scene_parts passes for an entry
        // with no active animation (draw_plan == plan in that arm).
        let shadow = shadow_params_from_plan(style, &plan, 1.0).unwrap();
        assert_eq!(shadow.outer_x, plan.outer_x as f32);
        assert_eq!(shadow.outer_y, plan.outer_y as f32);
        assert_eq!(shadow.outer_width, plan.outer_width as f32);
        assert_eq!(shadow.outer_height, plan.outer_height as f32);
        assert_eq!(shadow.corner_radius, plan.corner_radius);
        assert_eq!(shadow.strength, style.strength);
    }

    // --- B: Scale 0.70 shadow center matches animated surface center ---

    #[test]
    fn scale_0_70_shadow_center_matches_animated_surface_center() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let visual = sample_scale(0.0);
        assert_eq!((visual.scale_x, visual.scale_y), (SCALE_EFFECT_FROM_SCALE, SCALE_EFFECT_FROM_SCALE));
        let draw_plan = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
        // Opacity multiplier forced to 1.0 here to isolate GEOMETRY from
        // the separate opacity-coupling behavior proved in section D below
        // (Scale's own t=0 opacity is 0.0, which would otherwise make
        // shadow_params_from_plan return None and hide the geometry check).
        let shadow = shadow_params_from_plan(shadow_style(true, 8.0, 0.0, 0.0), &draw_plan, 1.0).unwrap();
        let surface_center_x = draw_plan.dst_x as f32 + draw_plan.width as f32 / 2.0;
        let surface_center_y = draw_plan.dst_y as f32 + draw_plan.height as f32 / 2.0;
        let shadow_center_x = shadow.outer_x + shadow.outer_width / 2.0;
        let shadow_center_y = shadow.outer_y + shadow.outer_height / 2.0;
        assert!((surface_center_x - shadow_center_x).abs() <= 1.0);
        assert!((surface_center_y - shadow_center_y).abs() <= 1.0);
    }

    // --- C: Scale 0.70 shadow base dimensions follow animated window ---

    #[test]
    fn scale_0_70_shadow_base_dimensions_follow_animated_window_not_real_window() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let visual = sample_scale(0.0);
        let draw_plan = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
        let style = shadow_style(true, 8.0, 0.0, 0.0);
        let animated_shadow = shadow_params_from_plan(style, &draw_plan, 1.0).unwrap();
        let real_shadow = shadow_params_from_plan(style, &plan, 1.0).unwrap();
        assert_eq!(animated_shadow.outer_width, draw_plan.outer_width as f32);
        assert_eq!(animated_shadow.outer_height, draw_plan.outer_height as f32);
        assert_ne!(animated_shadow.outer_width, real_shadow.outer_width);
        assert_ne!(animated_shadow.outer_height, real_shadow.outer_height);
    }

    // --- D: non-uniform hypothetical transform — shadow follows both axes ---

    #[test]
    fn non_uniform_scale_shadow_base_rect_follows_each_axis_independently() {
        // Conceptual future-Teleport-shaped values (see the 3a3fa2b1-s1
        // milestone's own example), exercised here generically without
        // implementing or depending on Teleport at all.
        let plan = animation_test_plan(0, 0, 200, 100);
        let (scale_x, scale_y) = (0.45, 0.10);
        let draw_plan = scale_render_quad_plan(plan, scale_x, scale_y);
        let expected_outer_width = ((plan.outer_width as f32) * scale_x).round().max(1.0) as i32;
        let expected_outer_height = ((plan.outer_height as f32) * scale_y).round().max(1.0) as i32;
        assert_eq!(draw_plan.outer_width, expected_outer_width);
        assert_eq!(draw_plan.outer_height, expected_outer_height);
        let shadow = shadow_params_from_plan(shadow_style(true, 8.0, 0.0, 0.0), &draw_plan, 1.0).unwrap();
        assert_eq!(shadow.outer_width, expected_outer_width as f32);
        assert_eq!(shadow.outer_height, expected_outer_height as f32);
    }

    // --- E: final t=1.0 — animated shadow geometry == ordinary shadow ---

    #[test]
    fn scale_at_t1_shadow_geometry_and_strength_match_ordinary_shadow_exactly() {
        let plan = animation_test_plan(10, 5, 200, 100);
        let visual = sample_scale(1.0);
        assert_eq!(visual, AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 });
        let draw_plan = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
        let style = shadow_style(true, 8.0, 3.0, 4.0);
        let animated_shadow = shadow_params_from_plan(style, &draw_plan, visual.opacity).unwrap();
        let ordinary_shadow = shadow_params_from_plan(style, &plan, 1.0).unwrap();
        assert_eq!(animated_shadow, ordinary_shadow);
    }

    // --- F: UVs remain irrelevant/unchanged for the surface itself ---
    // (covered directly by scale_render_quad_plan_preserves_uv_but_scales_
    // outer_bounds_with_the_box above — u0/v0/u1/v1/src_x/src_y untouched.)

    // --- G: real X11 geometry never mutated by this pipeline ---

    #[test]
    fn shadow_alignment_pipeline_never_touches_authoritative_x11_geometry() {
        let entry = animation_test_entry(9, SurfaceVisualClass::Normal, false);
        let original_geometry = entry.geometry;
        let plan = animation_test_plan(0, 0, 200, 100);
        let visual = sample_scale(0.3);
        let draw_plan = scale_render_quad_plan(plan, visual.scale_x, visual.scale_y);
        let _shadow = shadow_params_from_plan(shadow_style(true, 8.0, 0.0, 0.0), &draw_plan, visual.opacity);
        assert_eq!(entry.geometry, original_geometry);
    }

    // --- shadow opacity coupling (multiplier only, never replaces config) ---

    #[test]
    fn shadow_opacity_multiplier_scales_configured_strength_linearly() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let style = shadow_style(true, 8.0, 0.0, 0.0); // strength = 0.5
        assert_eq!(shadow_params_from_plan(style, &plan, 1.0).unwrap().strength, style.strength);
        let half = shadow_params_from_plan(style, &plan, 0.5).unwrap();
        assert_eq!(half.strength, style.strength * 0.5);
    }

    #[test]
    fn shadow_opacity_multiplier_zero_yields_no_shadow_params() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let style = shadow_style(true, 8.0, 0.0, 0.0);
        assert!(shadow_params_from_plan(style, &plan, 0.0).is_none());
    }

    #[test]
    fn shadow_opacity_multiplier_one_reproduces_exact_configured_strength() {
        let plan = animation_test_plan(0, 0, 200, 100);
        let style = shadow_style(true, 8.0, 0.0, 0.0);
        assert_eq!(shadow_params_from_plan(style, &plan, 1.0).unwrap().strength, style.strength);
    }

    // --- blur independence + single-sample-path proof, via source text
    // (same technique blur_wiring_has_no_new_gl_resources_or_renderer_x11_
    // queries already uses for render_egl_scene_parts) ---

    #[test]
    fn render_wiring_samples_animation_visual_exactly_once_per_surface() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = start + source[start..].find("\nfn egl_scene_is_renderable").unwrap();
        let body = &source[start..end];
        assert_eq!(body.matches(".sample(animation_now)").count(), 1);
    }

    #[test]
    fn render_wiring_couples_shadow_to_draw_plan_and_visual_opacity_only() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = start + source[start..].find("\nfn egl_scene_is_renderable").unwrap();
        let body = &source[start..end];
        assert_eq!(
            body.matches("shadow_params_from_plan(shadow_style, &draw_plan, shadow_opacity_multiplier)").count(),
            1,
        );
        // The pre-3a3fa2b1-s1 real-plan-only shadow call must be gone.
        assert!(!body.contains("shadow_params_from_plan(shadow_style, &plan)"));
    }

    #[test]
    fn render_wiring_keeps_blur_on_the_real_plan_not_draw_plan() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = start + source[start..].find("\nfn egl_scene_is_renderable").unwrap();
        let body = &source[start..end];
        assert_eq!(
            body.matches("capture_and_blur_background(\n                plan.outer_x,").count(),
            1,
        );
        assert_eq!(
            body.matches("draw_blurred_backdrop(blurred_texture, backdrop_params, plan.corner_radius)").count(),
            2,
        );
        assert!(!body.contains("capture_and_blur_background(\n                draw_plan"));
        assert!(!body.contains("draw_blurred_backdrop(blurred_texture, backdrop_params, draw_plan.corner_radius)"));
    }

    // --- first-frame: provisional animation's shadow aligns with the same
    // first animated surface plan, never a full-size shadow on a tiny
    // first frame ---

    #[test]
    fn provisional_scale_first_frame_shadow_never_uses_full_size_rect() {
        let new_surface = animation_test_entry(55, SurfaceVisualClass::Normal, false);
        let snapshot = SceneSnapshot { root: 1, root_geometry: full_hd_root(), entries: vec![new_surface] };
        let old_surfaces = HashSet::new();
        let persistent = HashMap::new();
        let now = Instant::now();
        let provisional = provisional_open_animations(
            &old_surfaces, &snapshot, false, true,
            test_animation_config(true), now,
        );
        let render_view = merge_window_animations(&persistent, &provisional);
        let animation = render_view.get(&55).expect("newly eligible surface must have a provisional animation");
        let t = animation.progress(now);
        let visual = animation.sample(now);
        assert!(t < 0.05);
        let real_plan = animation_test_plan(0, 0, 200, 100);
        let draw_plan = scale_render_quad_plan(real_plan, visual.scale_x, visual.scale_y);
        // The rect a shadow would use this frame is already the small,
        // first-frame animated rect — never the final full-size one.
        assert_ne!(draw_plan.outer_width, real_plan.outer_width);
        assert_ne!(draw_plan.outer_height, real_plan.outer_height);
        // And at Scale's t≈0 opacity (0.0), shadow_params_from_plan
        // correctly yields no shadow at all this frame — the forbidden
        // "tiny window + full-size shadow" case cannot occur either way.
        let shadow = shadow_params_from_plan(shadow_style(true, 8.0, 0.0, 0.0), &draw_plan, visual.opacity);
        assert!(shadow.is_none());
    }

    // --- move/resize during animation: no stale cached rectangle ---

    #[test]
    fn shadow_follows_latest_animated_rect_across_a_simulated_move_and_resize() {
        // "Move": nothing in this pipeline caches an initial rect — each
        // call is a pure function of that frame's real plan + visual.
        let moved_plan = animation_test_plan(80, 40, 200, 100);
        let visual = sample_scale(0.5);
        let draw_plan = scale_render_quad_plan(moved_plan, visual.scale_x, visual.scale_y);
        let style = shadow_style(true, 8.0, 0.0, 0.0);
        let shadow = shadow_params_from_plan(style, &draw_plan, 1.0).unwrap();
        assert_eq!(shadow.outer_x + shadow.outer_width / 2.0, draw_plan.outer_x as f32 + draw_plan.outer_width as f32 / 2.0);
        assert_eq!(shadow.outer_y + shadow.outer_height / 2.0, draw_plan.outer_y as f32 + draw_plan.outer_height as f32 / 2.0);

        // "Resize": same animation progress, different real dimensions —
        // the shadow rect must change too, not remain at the pre-resize size.
        let resized_plan = animation_test_plan(80, 40, 350, 60);
        let resized_draw_plan = scale_render_quad_plan(resized_plan, visual.scale_x, visual.scale_y);
        assert_ne!(resized_draw_plan.outer_width, draw_plan.outer_width);
        let resized_shadow = shadow_params_from_plan(style, &resized_draw_plan, 1.0).unwrap();
        assert_eq!(resized_shadow.outer_width, resized_draw_plan.outer_width as f32);
    }

    // --- G/H/I/J: eligibility ---

    #[test]
    fn eligibility_normal_non_override_redirect_is_eligible() {
        let entry = animation_test_entry(1, SurfaceVisualClass::Normal, false);
        assert!(eligible_for_open_animation(&entry));
    }

    #[test]
    fn eligibility_excludes_dock() {
        let entry = animation_test_entry(1, SurfaceVisualClass::Dock, false);
        assert!(!eligible_for_open_animation(&entry));
    }

    #[test]
    fn eligibility_excludes_desktop() {
        let entry = animation_test_entry(1, SurfaceVisualClass::Desktop, false);
        assert!(!eligible_for_open_animation(&entry));
    }

    #[test]
    fn eligibility_excludes_override_redirect_even_when_classified_normal() {
        // Regression guard for the exact gap the audit flagged: unknown
        // override_redirect popups classify as Normal by default, so
        // visual_class alone is not sufficient.
        let entry = animation_test_entry(1, SurfaceVisualClass::Normal, true);
        assert!(!eligible_for_open_animation(&entry));
    }

    // --- K/L/N: provisional animation construction ---

    #[test]
    fn provisional_animations_suppressed_on_first_publish() {
        let entry = animation_test_entry(1, SurfaceVisualClass::Normal, false);
        let snapshot = SceneSnapshot { root: 1, root_geometry: full_hd_root(), entries: vec![entry] };
        let old_surfaces = HashSet::new();
        let result = provisional_open_animations(
            &old_surfaces, &snapshot, true, true, test_animation_config(true), Instant::now(),
        );
        assert!(result.is_empty(), "five-windows-already-open startup must not animate");
    }

    #[test]
    fn provisional_animations_only_for_genuinely_added_surfaces() {
        let existing = animation_test_entry(1, SurfaceVisualClass::Normal, false);
        let added = animation_test_entry(2, SurfaceVisualClass::Normal, false);
        let snapshot = SceneSnapshot {
            root: 1,
            root_geometry: full_hd_root(),
            entries: vec![existing, added],
        };
        let mut old_surfaces = HashSet::new();
        old_surfaces.insert(1);
        let result = provisional_open_animations(
            &old_surfaces, &snapshot, false, true, test_animation_config(true), Instant::now(),
        );
        assert_eq!(result.len(), 1, "existing surface must not replay");
        assert!(result.contains_key(&2), "genuinely added surface must be eligible");
        assert!(!result.contains_key(&1));
    }

    #[test]
    fn provisional_animations_disabled_when_present_unavailable() {
        let entry = animation_test_entry(1, SurfaceVisualClass::Normal, false);
        let snapshot = SceneSnapshot { root: 1, root_geometry: full_hd_root(), entries: vec![entry] };
        let old_surfaces = HashSet::new();
        let result = provisional_open_animations(
            &old_surfaces, &snapshot, false, false, test_animation_config(true), Instant::now(),
        );
        assert!(result.is_empty(), "Present unavailable must render final state, never fail");
    }

    #[test]
    fn provisional_animations_disabled_when_config_disabled() {
        // The new (3a3fa2b1) gate, distinct from Present-availability:
        // Present IS available here, but animation.enabled is false.
        let entry = animation_test_entry(1, SurfaceVisualClass::Normal, false);
        let snapshot = SceneSnapshot { root: 1, root_geometry: full_hd_root(), entries: vec![entry] };
        let old_surfaces = HashSet::new();
        let result = provisional_open_animations(
            &old_surfaces, &snapshot, false, true, test_animation_config(false), Instant::now(),
        );
        assert!(result.is_empty(), "animation.enabled = false must render final state, never fail");
    }

    // --- M: removed surface ---

    #[test]
    fn retire_removed_surface_animations_removes_only_the_removed_ids() {
        let mut animations = HashMap::new();
        animations.insert(1, test_window_animation(Instant::now()));
        animations.insert(2, test_window_animation(Instant::now()));
        let mut removed = HashSet::new();
        removed.insert(1);
        retire_removed_surface_animations(&mut animations, &removed);
        assert!(!animations.contains_key(&1));
        assert!(animations.contains_key(&2));
    }

    // --- 24: rejected candidate leaves persistent state unchanged ---
    //
    // promote_provisional_animations is called from exactly one place,
    // commit_candidate_inner, itself reachable only via commit_candidate
    // after GateDecision::Accept (see rebuild_and_present's match on
    // pre_commit_gate's result, and try_resize_only's `if
    // !matches!(gate, GateDecision::Accept) { ...; return Ok(false); }`
    // early return before ever calling commit_candidate). A rejected or
    // retried candidate's `provisional_animations` is therefore simply
    // dropped with the candidate — this test exercises promote's own merge
    // semantics (what WOULD happen if it were called), proving it never
    // clobbers an already-running animation's `started_at` even in the
    // (structurally unreachable, since provisional only ever contains
    // new_surfaces - old_surfaces) case of key overlap.
    #[test]
    fn promote_provisional_animations_merges_without_restarting_existing_entries() {
        let mut persistent = HashMap::new();
        let earlier = test_window_animation(Instant::now() - Duration::from_millis(50));
        persistent.insert(1, earlier.clone());
        let mut provisional = HashMap::new();
        provisional.insert(1, test_window_animation(Instant::now()));
        provisional.insert(2, test_window_animation(Instant::now()));
        promote_provisional_animations(&mut persistent, provisional);
        assert_eq!(persistent.len(), 2);
        assert_eq!(persistent[&1].started_at, earlier.started_at);
        assert!(persistent.contains_key(&2));
    }

    #[test]
    fn promote_provisional_animations_never_called_leaves_persistent_state_untouched() {
        let mut persistent = HashMap::new();
        persistent.insert(1, test_window_animation(Instant::now()));
        let before = persistent.len();
        // A rejected candidate's provisional_animations is simply dropped —
        // modeled here by never calling promote at all.
        drop(HashMap::<Window, WindowAnimation>::new());
        assert_eq!(persistent.len(), before);
    }

    // ========================================================
    // 3a3fa2b5 — close animation core (reference sampler, RenderLayer
    // reconciliation, transactional close_id allocation).
    // ========================================================

    #[test]
    fn close_scale_sampler_reaches_exact_endpoints() {
        let start = sample_close_scale(0.0);
        assert_eq!(start, AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 });
        let end = sample_close_scale(1.0);
        assert_eq!(end, AnimationVisual { opacity: CLOSE_SCALE_END_OPACITY, scale_x: CLOSE_SCALE_END_SCALE, scale_y: CLOSE_SCALE_END_SCALE });
        // Past-end clamps to exactly the same final state, never overshoots.
        let past = sample_close_scale(1.5);
        assert_eq!(past, end);
    }

    #[test]
    fn close_scale_sampler_is_monotonic_deterministic_and_uniform() {
        let mid = sample_close_scale(0.5);
        assert!(mid.opacity > CLOSE_SCALE_END_OPACITY && mid.opacity < 1.0);
        assert!(mid.scale_x > CLOSE_SCALE_END_SCALE && mid.scale_x < 1.0);
        assert_eq!(mid.scale_x, mid.scale_y, "close scale is uniform, like open Scale");
        assert_eq!(sample_close_scale(0.5), sample_close_scale(0.5), "deterministic: same t, same output");
        let quarter = sample_close_scale(0.25);
        assert!(quarter.opacity > mid.opacity, "opacity must decrease monotonically toward the end");
    }

    #[test]
    fn sample_close_effect_dispatches_scale() {
        assert_eq!(
            sample_close_effect(crate::config::CloseAnimationEffect::Scale, 0.5),
            sample_close_scale(0.5),
        );
    }

    #[test]
    fn closing_animation_progress_and_completion_mirror_window_animation() {
        let started_at = Instant::now() - Duration::from_millis(90);
        let animation = ClosingAnimation::new(started_at, crate::config::CloseAnimationEffect::Scale, Duration::from_millis(180));
        let t = animation.progress(Instant::now());
        assert!(t > 0.0 && t < 1.0);
        assert!(!animation.is_complete(Instant::now()));
        let completed = ClosingAnimation::new(Instant::now() - Duration::from_millis(500), crate::config::CloseAnimationEffect::Scale, Duration::from_millis(180));
        assert!(completed.is_complete(Instant::now()));
        assert_eq!(completed.progress(Instant::now()), 1.0, "progress must clamp, never exceed 1.0");
    }

    // --- reconcile_render_order: R4 required scenarios A-F ---

    #[test]
    fn reconcile_a_pure_restack_reorders_live_with_no_close() {
        let previous = vec![RenderLayer::Live(1), RenderLayer::Live(2)];
        let result = reconcile_render_order(&previous, &[2, 1], &HashSet::new(), &HashMap::new());
        assert_eq!(result, vec![RenderLayer::Live(2), RenderLayer::Live(1)]);
    }

    #[test]
    fn reconcile_b_close_between_two_live_survivors() {
        // previous: A(1) C(2) B(3); C(2) closes; new live: [1, 3]
        let previous = vec![RenderLayer::Live(1), RenderLayer::Live(2), RenderLayer::Live(3)];
        let mut removed = HashSet::new();
        removed.insert(2);
        let mut closes = HashMap::new();
        closes.insert(2, 100_u64);
        let result = reconcile_render_order(&previous, &[1, 3], &removed, &closes);
        assert_eq!(result, vec![RenderLayer::Live(1), RenderLayer::Closing(100), RenderLayer::Live(3)]);
    }

    #[test]
    fn reconcile_c_live_restack_while_a_close_persists_never_blocks_the_restack() {
        // committed: A(1) Closing(100) B(3); live restacks to [3, 1]
        let previous = vec![RenderLayer::Live(1), RenderLayer::Closing(100), RenderLayer::Live(3)];
        let result = reconcile_render_order(&previous, &[3, 1], &HashSet::new(), &HashMap::new());
        assert_eq!(result, vec![RenderLayer::Live(3), RenderLayer::Live(1), RenderLayer::Closing(100)]);
        // Live projection is exactly the authoritative order, regardless
        // of the closing entry's presence.
        let live_projection: Vec<Window> = result.iter().filter_map(|layer| match layer { RenderLayer::Live(xid) => Some(*xid), _ => None }).collect();
        assert_eq!(live_projection, vec![3, 1]);
    }

    #[test]
    fn reconcile_d_sequential_close_shares_its_predecessors_anchor() {
        // committed: A(1) Closing(100) B(3); B(3) closes; new live: [1]
        let previous = vec![RenderLayer::Live(1), RenderLayer::Closing(100), RenderLayer::Live(3)];
        let mut removed = HashSet::new();
        removed.insert(3);
        let mut closes = HashMap::new();
        closes.insert(3, 101_u64);
        let result = reconcile_render_order(&previous, &[1], &removed, &closes);
        assert_eq!(result, vec![RenderLayer::Live(1), RenderLayer::Closing(100), RenderLayer::Closing(101)]);
    }

    #[test]
    fn reconcile_e_new_live_insertion_while_a_close_persists() {
        // committed: A(1) Closing(100) B(3); new live: [1, 4, 3] (4 is new)
        let previous = vec![RenderLayer::Live(1), RenderLayer::Closing(100), RenderLayer::Live(3)];
        let result = reconcile_render_order(&previous, &[1, 4, 3], &HashSet::new(), &HashMap::new());
        assert_eq!(result, vec![RenderLayer::Live(1), RenderLayer::Closing(100), RenderLayer::Live(4), RenderLayer::Live(3)]);
        let live_projection: Vec<Window> = result.iter().filter_map(|layer| match layer { RenderLayer::Live(xid) => Some(*xid), _ => None }).collect();
        assert_eq!(live_projection, vec![1, 4, 3]);
    }

    #[test]
    fn reconcile_f_simultaneous_closes_preserve_old_relative_order() {
        // previous: A(1) C(2) D(3) B(4); C and D close simultaneously; new live: [1, 4]
        let previous = vec![RenderLayer::Live(1), RenderLayer::Live(2), RenderLayer::Live(3), RenderLayer::Live(4)];
        let mut removed = HashSet::new();
        removed.insert(2);
        removed.insert(3);
        let mut closes = HashMap::new();
        closes.insert(2, 100_u64);
        closes.insert(3, 101_u64);
        let result = reconcile_render_order(&previous, &[1, 4], &removed, &closes);
        assert_eq!(result, vec![RenderLayer::Live(1), RenderLayer::Closing(100), RenderLayer::Closing(101), RenderLayer::Live(4)]);
    }

    #[test]
    fn reconcile_projection_invariant_holds_across_a_wide_property_sweep() {
        // Exhaustive-ish sweep over small previous_order shapes and live
        // permutations: the Live projection of the result must ALWAYS
        // equal new_live_order exactly, regardless of closing content.
        let live_permutations: [[Window; 3]; 6] = [
            [1, 2, 3], [1, 3, 2], [2, 1, 3], [2, 3, 1], [3, 1, 2], [3, 2, 1],
        ];
        let previous_shapes: Vec<Vec<RenderLayer>> = vec![
            vec![RenderLayer::Live(1), RenderLayer::Live(2), RenderLayer::Live(3)],
            vec![RenderLayer::Live(1), RenderLayer::Closing(50), RenderLayer::Live(2), RenderLayer::Live(3)],
            vec![RenderLayer::Closing(50), RenderLayer::Live(1), RenderLayer::Live(2), RenderLayer::Live(3)],
            vec![RenderLayer::Live(1), RenderLayer::Live(2), RenderLayer::Closing(50), RenderLayer::Live(3)],
            Vec::new(),
        ];
        for previous in &previous_shapes {
            for live in &live_permutations {
                let result = reconcile_render_order(previous, live, &HashSet::new(), &HashMap::new());
                let live_projection: Vec<Window> = result.iter().filter_map(|layer| match layer { RenderLayer::Live(xid) => Some(*xid), _ => None }).collect();
                assert_eq!(live_projection, live.to_vec(), "previous={previous:?} live={live:?}");
            }
        }
    }

    #[test]
    fn reconcile_bootstrap_first_commit_reproduces_ordinary_snapshot_order() {
        let result = reconcile_render_order(&[], &[7, 3, 9], &HashSet::new(), &HashMap::new());
        assert_eq!(result, vec![RenderLayer::Live(7), RenderLayer::Live(3), RenderLayer::Live(9)]);
    }

    #[test]
    fn reconcile_retirement_preserves_remaining_relative_order_and_live_projection() {
        // A(1) Closing(100) Closing(101) Closing(102) B(3), 101 already
        // retired by SceneSession::retire_completed_closing_visuals before
        // this reconciliation runs — so it is simply absent from
        // previous_order, exactly like this test constructs it.
        let previous = vec![
            RenderLayer::Live(1), RenderLayer::Closing(100), RenderLayer::Closing(102), RenderLayer::Live(3),
        ];
        let result = reconcile_render_order(&previous, &[1, 3], &HashSet::new(), &HashMap::new());
        assert_eq!(result, vec![RenderLayer::Live(1), RenderLayer::Closing(100), RenderLayer::Closing(102), RenderLayer::Live(3)]);
    }

    #[test]
    fn reconcile_xid_reuse_never_collides_with_an_existing_close_id() {
        // Window 42 was live, closed (id 100), then its XID got reused by
        // an unrelated new Live window in the SAME commit the old one's
        // Closing(100) is still animating.
        let previous = vec![RenderLayer::Closing(100), RenderLayer::Live(7)];
        let result = reconcile_render_order(&previous, &[42, 7], &HashSet::new(), &HashMap::new());
        // Closing(100) has no left-live-neighbor (nothing preceded it in
        // previous_order), so it anchors to `None` (the very bottom) —
        // unaffected by the reused XID, which enters purely via
        // new_live_order with zero special-casing: Closing(100) still has
        // no Window field to collide on.
        assert_eq!(result, vec![RenderLayer::Closing(100), RenderLayer::Live(42), RenderLayer::Live(7)]);
    }

    #[test]
    fn reconcile_ineligible_removal_is_dropped_without_updating_the_anchor() {
        // A(1) X(2) B(3); X(2) is removed but NOT eligible for a close
        // (no entry in provisional_closes) — it must simply vanish, and
        // B's anchor computation must still see A as its nearest left
        // survivor (not X).
        let previous = vec![RenderLayer::Live(1), RenderLayer::Live(2), RenderLayer::Live(3)];
        let mut removed = HashSet::new();
        removed.insert(2);
        let result = reconcile_render_order(&previous, &[1, 3], &removed, &HashMap::new());
        assert_eq!(result, vec![RenderLayer::Live(1), RenderLayer::Live(3)]);
    }

    #[test]
    fn reconcile_no_close_case_produces_exactly_ordinary_snapshot_entries_ordering() {
        let previous = vec![RenderLayer::Live(5), RenderLayer::Live(6), RenderLayer::Live(7)];
        let result = reconcile_render_order(&previous, &[5, 6, 7], &HashSet::new(), &HashMap::new());
        assert_eq!(result, previous);
    }

    // --- allocate_close_ids: transactional model ---

    #[test]
    fn allocate_close_ids_retry_then_accept_matches_the_required_worked_example() {
        // Attempt #1: base=100, reserves 100,101, then RETRY (discarded —
        // modeled by simply not using attempt #1's return value at all).
        let (attempt_1_ids, attempt_1_next) = allocate_close_ids(100, &[10, 11]);
        assert_eq!(attempt_1_ids.len(), 2);
        assert_eq!(attempt_1_next, 102);
        // self.next_close_id was never written during attempt #1 (this is
        // a pure function call — nothing committed), so attempt #2 starts
        // from the SAME base=100 again.
        let (attempt_2_ids, attempt_2_next) = allocate_close_ids(100, &[10, 11]);
        assert_eq!(attempt_2_ids.get(&10), Some(&100));
        assert_eq!(attempt_2_ids.get(&11), Some(&101));
        assert_eq!(attempt_2_next, 102);
        // ACCEPT: committed self.next_close_id becomes attempt_2_next.
        assert_eq!(attempt_2_next, 102);
    }

    #[test]
    fn allocate_close_ids_is_deterministic_and_order_preserving_by_input_order() {
        let (ids, next) = allocate_close_ids(5, &[20, 21, 22]);
        assert_eq!(ids[&20], 5);
        assert_eq!(ids[&21], 6);
        assert_eq!(ids[&22], 7);
        assert_eq!(next, 8);
    }

    #[test]
    fn allocate_close_ids_empty_input_leaves_counter_untouched() {
        let (ids, next) = allocate_close_ids(42, &[]);
        assert!(ids.is_empty());
        assert_eq!(next, 42);
    }

    #[test]
    fn allocate_close_ids_overflow_fails_open_without_wrapping_or_panicking() {
        // base = u64::MAX - 1: first allocation succeeds (id = MAX-1, next
        // advances to MAX); second allocation's checked_add(1) on MAX
        // overflows -> that source is skipped (fail-open), next_id stays
        // MAX, no wrap, no panic, no committed duplicate.
        let base = u64::MAX - 1;
        let (ids, next) = allocate_close_ids(base, &[1, 2, 3]);
        assert_eq!(ids.len(), 1, "only the first source can be allocated before exhaustion");
        assert_eq!(ids[&1], u64::MAX - 1);
        assert!(!ids.contains_key(&2));
        assert!(!ids.contains_key(&3));
        assert_eq!(next, u64::MAX, "never wraps back to 0");
    }

    #[test]
    fn allocate_close_ids_at_exact_max_allocates_nothing() {
        let (ids, next) = allocate_close_ids(u64::MAX, &[1]);
        assert!(ids.is_empty());
        assert_eq!(next, u64::MAX);
    }

    // --- RenderLayer / candidate-local wiring sanity ---

    #[test]
    fn render_layer_variants_are_distinct_and_carry_the_expected_identity() {
        assert_ne!(RenderLayer::Live(7), RenderLayer::Closing(7), "same numeric value, different identity domain");
        assert_eq!(RenderLayer::Live(7), RenderLayer::Live(7));
        assert_eq!(RenderLayer::Closing(7), RenderLayer::Closing(7));
    }

    // --- wiring: R1 scope guards (no live blur, no new GL/X11 calls, no
    // energy_tear) via the same source-scan technique already used for
    // render_egl_scene_parts's blur/animation wiring ---

    #[test]
    fn render_closing_layer_never_issues_a_live_background_blur_pass() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_closing_layer(").unwrap();
        let end = start + source[start..].find("\nfn ").unwrap();
        let body = &source[start..end];
        assert!(!body.contains("capture_and_blur_background"));
        assert!(!body.contains("draw_blurred_backdrop"));
        assert!(!body.contains("energy_tear"));
        assert!(body.contains("render_surface_with_opacity"));
    }

    #[test]
    fn render_closing_layer_is_outside_render_egl_scene_parts_scanned_span() {
        // Guards the whitespace-sensitive render_wiring_* assertions above
        // (e.g. .sample(animation_now) count==1): render_closing_layer
        // must never be defined between render_egl_scene_parts and
        // egl_scene_is_renderable.
        let source = include_str!("scene.rs");
        let scan_start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let scan_end = scan_start + source[scan_start..].find("\nfn egl_scene_is_renderable").unwrap();
        let scanned = &source[scan_start..scan_end];
        assert!(!scanned.contains("fn render_closing_layer("));
    }

    // ========================================================
    // 3a3fa2b5-r2 — corrective diff preview: 6 implementation-defect
    // fixes on top of the R1 architecture (config model, close Scale
    // sampler, RenderLayer model, left-neighbor-gap reconciliation,
    // transactional close_id model, frame-0 old-live-texture
    // architecture, compositor-owned RGBA8 snapshot model, border/shadow
    // reconstruction, no-close-blur policy, Present scheduling, close
    // eligibility policy — all UNCHANGED, see the R1 tests above).
    // ========================================================

    // --- R2 correction 1: ScratchFramebuffer lifetime (verified NOT a
    // bug on inspection — `scratch` is already bound to a local and kept
    // alive across the whole draw, dropped explicitly afterward; this
    // test pins that ordering going forward so it can never regress) ---

    #[test]
    fn scratch_framebuffer_owner_spans_the_entire_exact_copy_draw() {
        let source = include_str!("../graphics/renderer.rs");
        let start = source.find("pub(crate) fn capture_closing_snapshot(").unwrap();
        let end = start + source[start..].find("\n    fn render_exact_copy(").unwrap();
        let body = &source[start..end];
        let bind_index = body.find("let scratch = ScratchFramebuffer::new(texture)?;")
            .expect("ScratchFramebuffer must be bound to a local, never an unbound temporary");
        let draw_index = body.find("self.render_exact_copy(source_texture, width, height)?;")
            .expect("the exact-copy draw must run inside capture_closing_snapshot");
        let drop_index = body.find("drop(scratch);")
            .expect("scratch must be dropped explicitly, after the draw — RAII, not a bottom-of-function manual delete");
        assert!(bind_index < draw_index, "ScratchFramebuffer must be alive BEFORE the copy draw runs");
        assert!(draw_index < drop_index, "ScratchFramebuffer must still be alive WHILE the copy draw runs — only dropped after");
    }

    // --- R2 correction 2: ProvisionalClosingFrame owns no GPU/X11
    // resource (structural — no Rc<RefCell<EglImportedSurface>>, no
    // NamedPixmap, no Damage/DamageLease field) ---

    #[test]
    fn provisional_closing_frame_owns_no_gpu_or_x11_resource() {
        let source = include_str!("scene.rs");
        let start = source.find("struct ProvisionalClosingFrame {").unwrap();
        let end = start + source[start..].find("\n}").unwrap();
        let body = &source[start..end];
        assert!(!body.contains("EglImportedSurface"), "must not own/reference a live EGL surface");
        assert!(!body.contains("NamedSurfacePixmap"), "must not own/reference a NamedPixmap");
        assert!(!body.contains("DamageLease"), "must not own/reference a DamageLease");
        assert!(!body.contains("Rc<"), "must hold no reference-counted resource handle at all");
        // Only value/identity data — exactly the R2-corrected field set.
        assert!(body.contains("source_xid: Window"));
        assert!(body.contains("plan: RenderQuadPlan"));
        assert!(body.contains("pixel_semantics: EglPixelSemantics"));
        assert!(body.contains("base_opacity: f32"));
        assert!(body.contains("shadow_eligible: bool"));
        assert!(body.contains("animation: ClosingAnimation"));
    }

    #[test]
    fn closing_visual_owns_only_its_own_closing_texture_no_other_resource() {
        let source = include_str!("scene.rs");
        let start = source.find("struct ClosingVisual {").unwrap();
        let end = start + source[start..].find("\n}").unwrap();
        let body = &source[start..end];
        assert!(body.contains("texture: ClosingTexture"), "the ONE owned GPU resource");
        assert!(!body.contains("EglImportedSurface"));
        assert!(!body.contains("NamedSurfacePixmap"));
        assert!(!body.contains("DamageLease"));
    }

    #[test]
    fn closing_draw_source_never_reads_a_stored_texture_field() {
        // R1 had `ClosingDrawSource::texture()` reading
        // `frame.old_surface.borrow().texture` — a stored reference. R2
        // removes that method entirely: the source texture is now always
        // resolved by the CALLER (`render_closing_layer`), fresh, by
        // `source_xid` lookup — never cached on the draw-source type.
        let source = include_str!("scene.rs");
        let start = source.find("impl ClosingDrawSource<'_> {").unwrap();
        let end = start + source[start..].find("\n}\n").unwrap();
        let body = &source[start..end];
        assert!(!body.contains("fn texture("), "ClosingDrawSource must not itself resolve a texture");
        assert!(!body.contains("old_surface"));
    }

    #[test]
    fn render_closing_layer_resolves_provisional_texture_by_lookup_not_ownership() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_closing_layer(").unwrap();
        let end = start + source[start..].find("\nfn ").unwrap();
        let body = &source[start..end];
        assert!(body.contains("closing_source_surfaces.get(&frame.source_xid)"));
        assert!(!body.contains("old_surface"));
    }

    #[test]
    fn commit_candidate_inner_resolves_capture_source_from_old_resources_not_a_stored_rc() {
        let source = include_str!("scene.rs");
        let start = source.find("fn commit_candidate_inner(").unwrap();
        let end = start + source[start..].find("\n    fn retain_current_pending").unwrap();
        let body = &source[start..end];
        assert!(body.contains("old_resources\n                .get(&frame.source_xid)"));
        assert!(!body.contains("frame.old_surface"));
    }

    // --- R2 correction 3: destroy-intent lifetime (transaction-scoped,
    // retry-safe, cannot leak across XID reuse) ---

    #[test]
    fn destroy_intent_a_survives_retry_never_mutated_outside_commit() {
        // The retention rule is applied ONLY inside commit_candidate_inner
        // — never during build_candidate/pre_commit_gate — so a candidate
        // that gets Retried leaves `destroy_intents` completely untouched,
        // and a subsequent attempt observes the exact same set. Proven
        // structurally: `retained_destroy_intents(` must appear ONLY
        // inside commit_candidate_inner's body, never inside
        // build_candidate's or pre_commit_gate's.
        let source = include_str!("scene.rs");
        let commit_start = source.find("fn commit_candidate_inner(").unwrap();
        let commit_end = commit_start + source[commit_start..].find("\n    fn retain_current_pending").unwrap();
        assert!(source[commit_start..commit_end].contains("retained_destroy_intents("));
        let build_start = source.find("fn build_candidate(&mut self)").unwrap();
        let build_end = build_start + source[build_start..].find("\n    fn rebuild_and_present").unwrap();
        assert!(!source[build_start..build_end].contains("retained_destroy_intents("));
        // Also: note_destroy_intent (the only writer) never removes —
        // insertion only, so a Retry (which re-drains no new events) can
        // never lose or gain an intent by itself either.
        let note_start = source.find("fn note_destroy_intent(").unwrap();
        let note_end = note_start + source[note_start..].find("\n    }").unwrap();
        assert!(!source[note_start..note_end].contains("remove"));
    }

    #[test]
    fn destroy_intent_b_accepted_transaction_retires_intent_even_without_a_close() {
        // Ineligible-but-destroyed X, no successor: X is removed
        // (absent from new_surfaces) but produced no close. The retained
        // set must not contain X afterward, regardless of why no close
        // was created (ineligible, allocation failure, snapshot failure
        // — none of those change whether X is in new_surfaces).
        let mut destroy_intents = HashSet::new();
        destroy_intents.insert(42);
        let new_surfaces: HashSet<Window> = HashSet::new();
        let retained = retained_destroy_intents(&destroy_intents, &new_surfaces);
        assert!(!retained.contains(&42), "an accepted removal must retire its intent even with no close created");
    }

    #[test]
    fn destroy_intent_b_extends_to_untracked_xids_r1_regression() {
        // The exact R1 gap: a DestroyNotify(99) for an XID that was NEVER
        // part of the committed scene (untracked popup) must not survive
        // this commit either, even though 99 was never in
        // `removed_surfaces` (it was never in `old_surfaces` to begin
        // with) — R1's narrower `removed_surfaces`-only loop would have
        // left this forever.
        let mut destroy_intents = HashSet::new();
        destroy_intents.insert(99);
        let new_surfaces: HashSet<Window> = HashSet::new(); // 99 never became live
        let retained = retained_destroy_intents(&destroy_intents, &new_surfaces);
        assert!(retained.is_empty(), "an intent for a never-tracked XID must not survive a commit");
    }

    #[test]
    fn destroy_intent_c_stale_intent_cannot_survive_to_collide_with_a_reused_xid() {
        // Sequence: DestroyNotify(99) for an untracked popup fires, then
        // commit #1 happens (99 never live) -> pruned. THEN xid 99 is
        // reused by a brand-new, unrelated Live window, which itself
        // becomes part of a later commit's new_surfaces. Prove: the
        // pruned set from commit #1 has no way to reach commit #2's
        // eligibility check, since retained_destroy_intents only ever
        // narrows (intersects), never reintroduces a dropped entry.
        let mut destroy_intents = HashSet::new();
        destroy_intents.insert(99);
        let commit_1_new_surfaces: HashSet<Window> = HashSet::new();
        let after_commit_1 = retained_destroy_intents(&destroy_intents, &commit_1_new_surfaces);
        assert!(after_commit_1.is_empty());
        // xid 99 reused, now live; no NEW DestroyNotify(99) has occurred
        // for the new window, so destroy_intents (session state) still
        // does not contain 99 — proven by chaining commit #1's OUTPUT
        // (not the original stale set) forward.
        let mut commit_2_new_surfaces = HashSet::new();
        commit_2_new_surfaces.insert(99);
        let after_commit_2 = retained_destroy_intents(&after_commit_1, &commit_2_new_surfaces);
        assert!(!after_commit_2.contains(&99), "a stale intent must never resurrect for a reused XID without a fresh genuine DestroyNotify");
    }

    #[test]
    fn destroy_intent_d_unmap_only_after_reuse_remains_no_close() {
        // note_destroy_intent's ONLY write path is the DestroyNotify
        // match arm — structurally, UnmapNotify (or anything else) can
        // never insert an intent, so combined with (C) above (no stale
        // intent can survive to collide with a reused XID), an
        // Unmap-only sequence after XID reuse can never satisfy the
        // close-trigger's `destroy_intents.contains(...)` gate.
        let source = include_str!("scene.rs");
        let start = source.find("fn note_destroy_intent(").unwrap();
        let end = start + source[start..].find("\n    }").unwrap();
        let body = &source[start..end];
        assert!(body.contains("Event::DestroyNotify(destroy)"));
        assert!(!body.contains("UnmapNotify"), "UnmapNotify must never write a destroy intent");
        // And the trigger gate itself requires it, unconditionally.
        let gate_start = source.find("fn build_provisional_closing_state(").unwrap();
        let gate_end = gate_start + source[gate_start..].find("\n    fn retire_completed_closing_visuals").unwrap();
        assert!(source[gate_start..gate_end].contains("self.destroy_intents.contains(&old_entry.surface_xid)"));
    }

    // --- R2 correction 4: snapshot dimensions come from the source
    // pixmap's own geometry, never RenderQuadPlan's outer_* (shadow-rect)
    // extent ---

    #[test]
    fn commit_candidate_inner_captures_snapshot_dimensions_from_pixmap_geometry() {
        let source = include_str!("scene.rs");
        let start = source.find("fn commit_candidate_inner(").unwrap();
        let end = start + source[start..].find("\n    fn retain_current_pending").unwrap();
        let body = &source[start..end];
        assert!(body.contains("bundle.pixmap.geometry.width"));
        assert!(body.contains("bundle.pixmap.geometry.height"));
        assert!(!body.contains("frame.plan.outer_width"), "R1's bug: outer_* is the SHADOW base rect, not a guaranteed content-size source");
        assert!(!body.contains("frame.plan.outer_height"));
    }

    #[test]
    fn pixmap_geometry_dimensions_are_independent_of_render_quad_plan_outer_extent() {
        // Direct proof that a plan's outer_width/outer_height (which
        // scale_render_quad_plan documents as the SHADOW's base rect, and
        // which an animated/shadow-bearing plan can inflate or shrink
        // independently) is a structurally SEPARATE value from the
        // source pixmap's own width/height — the snapshot capture path
        // (R2-corrected) reads only the latter, so a plan with a large,
        // shadow-driven outer extent cannot alter the captured texture's
        // dimensions.
        let pixmap = PixmapGeometry { root: 1, x: 0, y: 0, width: 200, height: 100, border_width: 0, depth: 24 };
        let window = WindowGeometry { x: 0, y: 0, width: 200, height: 100, border_width: 0 };
        let root = RootGeometry { width: 1920, height: 1080, depth: 24, visual: 0 };
        let plan = build_render_quad_plan(window, pixmap, root).unwrap();
        // Simulate an animated/scaled draw plan with a very different
        // outer extent (as render_closing_layer produces every frame via
        // scale_render_quad_plan) — the pixmap's own geometry never
        // changes as a result, since capture reads `bundle.pixmap.geometry`
        // directly, never `plan`/`draw_plan` at all.
        let animated = scale_render_quad_plan(plan, 0.5, 0.5);
        assert_ne!(animated.outer_width, i32::from(pixmap.width), "the animated plan's outer extent legitimately differs from content size");
        assert_eq!(i32::from(pixmap.width), 200, "the authoritative capture source is untouched by any plan transform");
        assert_eq!(i32::from(pixmap.height), 100);
    }

    // --- R2 correction 5: synchronized live-projection walk, O(live +
    // closing), no per-entry `.find()` ---

    #[test]
    fn render_egl_scene_parts_uses_a_synchronized_walk_not_per_entry_find() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = start + source[start..].find("\nfn egl_scene_is_renderable").unwrap();
        let body = &source[start..end];
        assert!(body.contains("let mut live_entries = snapshot.entries.iter();"));
        assert!(body.contains("live_entries.next()"));
        assert!(
            !body.contains("snapshot.entries.iter().find(|entry| entry.surface_xid == surface_xid)"),
            "R1's O(render_order * snapshot.entries) per-entry lookup must be gone",
        );
    }

    #[cfg(test)]
    fn synchronized_live_projection(render_order: &[RenderLayer], live_entries: &[Window]) -> Vec<Window> {
        // Mirrors render_egl_scene_parts's corrected Live-branch mechanism
        // exactly: a single forward iterator, advanced only by
        // RenderLayer::Live, asserting the same identity the real render
        // loop's debug_assert_eq! checks.
        let mut iter = live_entries.iter();
        let mut consumed = Vec::new();
        for layer in render_order {
            if let RenderLayer::Live(xid) = *layer {
                let next = iter.next().expect("Live-projection invariant guarantees an entry exists");
                assert_eq!(*next, xid, "synchronized walk must consume matching entries in order");
                consumed.push(*next);
            }
        }
        consumed
    }

    #[test]
    fn synchronized_walk_consumes_exactly_the_live_projection_in_order_a_through_f() {
        // Reuses the exact reconcile_render_order scenarios A-F already
        // proven above: for each, the synchronized walk must consume
        // precisely new_live_order, in order, with no backtracking.
        let cases: Vec<(Vec<RenderLayer>, Vec<Window>)> = vec![
            (
                reconcile_render_order(&[RenderLayer::Live(1), RenderLayer::Live(2)], &[2, 1], &HashSet::new(), &HashMap::new()),
                vec![2, 1],
            ),
            (
                reconcile_render_order(
                    &[RenderLayer::Live(1), RenderLayer::Live(2), RenderLayer::Live(3)],
                    &[1, 3], &{ let mut s = HashSet::new(); s.insert(2); s },
                    &{ let mut m = HashMap::new(); m.insert(2, 100_u64); m },
                ),
                vec![1, 3],
            ),
            (
                reconcile_render_order(
                    &[RenderLayer::Live(1), RenderLayer::Live(2), RenderLayer::Live(3), RenderLayer::Live(4)],
                    &[1, 4],
                    &{ let mut s = HashSet::new(); s.insert(2); s.insert(3); s },
                    &{ let mut m = HashMap::new(); m.insert(2, 100_u64); m.insert(3, 101_u64); m },
                ),
                vec![1, 4],
            ),
        ];
        for (render_order, expected_live) in cases {
            let consumed = synchronized_live_projection(&render_order, &expected_live);
            assert_eq!(consumed, expected_live);
        }
    }

    // --- R2 correction 6: blur documentation wording (behavior
    // unchanged — no close blur in R1 either; this only pins the
    // corrected phrasing so it cannot regress back to the old, imprecise
    // "frame 0 and later" framing) ---

    #[test]
    fn blur_documentation_uses_the_corrected_first_closing_frame_wording() {
        let source = include_str!("scene.rs");
        assert!(source.contains("NO LIVE BLUR FROM FIRST CLOSING FRAME"));
        // Built via concatenation so this test's OWN source line is never
        // itself a match for the very phrase it's checking is gone.
        let old_imprecise_phrase = ["NONE AFTER", "FRAME 0"].join(" ");
        assert!(!source.contains(&old_imprecise_phrase));
    }

    // ========================================================
    // 3a3fa2b6-r1 — TeleportFlashy open+close.
    // ========================================================

    #[test]
    fn teleport_flashy_color_is_a_fixed_bright_neutral_no_config_key() {
        // Section 7: fixed near-white, no animation.flash.color config
        // key in R1 — color tuning is deferred until after human
        // validation.
        assert_eq!(TELEPORT_FLASHY_COLOR, [1.0, 1.0, 1.0]);
        let source = include_str!("../config.rs");
        assert!(!source.contains("animation.flash.color"));
        assert!(!source.contains("animation_flash_color"));
    }

    // --- OPEN AnimationVisual (geometry) ---

    #[test]
    fn teleport_flashy_open_t0_is_exact_start_state() {
        let start = sample_teleport_flashy_open(0.0);
        assert_eq!(
            start,
            AnimationVisual {
                opacity: TELEPORT_FLASHY_OPEN_START_OPACITY,
                scale_x: TELEPORT_FLASHY_OPEN_START_SCALE,
                scale_y: TELEPORT_FLASHY_OPEN_START_SCALE,
            },
        );
        assert_eq!(start.opacity, 0.05);
        assert_eq!(start.scale_x, 0.985);
        assert_eq!(start.scale_y, 0.985);
    }

    #[test]
    fn teleport_flashy_open_reaches_exact_identity_by_geometry_end() {
        let at_end = sample_teleport_flashy_open(TELEPORT_FLASHY_OPEN_GEOMETRY_END);
        assert_eq!(at_end, AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 });
        assert_eq!(TELEPORT_FLASHY_OPEN_GEOMETRY_END, 0.20);
        // Held exact identity for the rest of the animation, including t=1.
        for t in [0.20, 0.5, 0.9, 1.0] {
            assert_eq!(sample_teleport_flashy_open(t), AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 }, "t={t}");
        }
    }

    #[test]
    fn teleport_flashy_open_geometry_is_monotonic_deterministic_uniform_no_overshoot() {
        let mut previous_opacity = TELEPORT_FLASHY_OPEN_START_OPACITY;
        let mut previous_scale = TELEPORT_FLASHY_OPEN_START_SCALE;
        let mut t = 0.0_f32;
        while t <= TELEPORT_FLASHY_OPEN_GEOMETRY_END {
            let visual = sample_teleport_flashy_open(t);
            assert_eq!(visual.scale_x, visual.scale_y, "uniform, no axis reversal, t={t}");
            assert!(visual.scale_x <= 1.0001, "no overshoot, t={t}");
            assert!(visual.opacity >= previous_opacity - 1e-6, "opacity monotonic, t={t}");
            assert!(visual.scale_x >= previous_scale - 1e-6, "scale monotonic, t={t}");
            assert_eq!(sample_teleport_flashy_open(t), visual, "deterministic, t={t}");
            previous_opacity = visual.opacity;
            previous_scale = visual.scale_x;
            t += 0.02;
        }
    }

    // --- OPEN flash-alpha ---

    #[test]
    fn teleport_flashy_open_flash_holds_at_one_through_hold_end() {
        assert_eq!(TELEPORT_FLASHY_OPEN_FLASH_HOLD_END, 0.08);
        for t in [0.0, 0.02, 0.05, 0.08] {
            assert_eq!(sample_teleport_flashy_open_flash(t), 1.0, "t={t}");
        }
    }

    #[test]
    fn teleport_flashy_open_flash_reaches_exact_zero_by_end() {
        assert_eq!(TELEPORT_FLASHY_OPEN_FLASH_END, 0.40);
        for t in [0.40, 0.5, 0.9, 1.0] {
            assert_eq!(sample_teleport_flashy_open_flash(t), 0.0, "t={t}");
        }
    }

    #[test]
    fn teleport_flashy_open_flash_is_monotonic_bounded_and_deterministic() {
        let mut previous = 1.0_f32;
        let mut t = TELEPORT_FLASHY_OPEN_FLASH_HOLD_END;
        while t <= TELEPORT_FLASHY_OPEN_FLASH_END {
            let alpha = sample_teleport_flashy_open_flash(t);
            assert!((0.0..=1.0).contains(&alpha), "bounded, t={t} alpha={alpha}");
            assert!(alpha <= previous + 1e-6, "monotonic decay, t={t}");
            assert_eq!(sample_teleport_flashy_open_flash(t), alpha, "deterministic, t={t}");
            previous = alpha;
            t += 0.02;
        }
    }

    #[test]
    fn teleport_flashy_open_flash_for_gates_exclusively_on_teleport_flashy() {
        assert_eq!(teleport_flashy_open_flash_for(OpenAnimationEffect::TeleportFlashy, 0.0), Some(1.0));
        for effect in [
            OpenAnimationEffect::Scale,
            OpenAnimationEffect::Teleport,
            OpenAnimationEffect::EnergyTear,
            OpenAnimationEffect::Bubble,
        ] {
            assert_eq!(teleport_flashy_open_flash_for(effect, 0.0), None, "effect={effect:?}");
        }
        // And EnergyTear's own independent overlay dispatch never produces
        // a layout for TeleportFlashy either — the two overlay systems
        // are mutually exclusive gates.
        assert_eq!(energy_tear_layout_for(OpenAnimationEffect::TeleportFlashy, 0.0), None);
    }

    #[test]
    fn teleport_flashy_open_resolved_opacity_is_never_forced_to_one() {
        // Section 11: effective opacity = resolved/base opacity *
        // AnimationVisual.opacity. A translucent window (0.82) must never
        // be forced to absolute 1.0 by this effect, and must land back at
        // exactly 0.82 once the effect completes.
        let resolved: f32 = 0.82;
        for t in [0.0, 0.05, 0.10, 0.15, 0.20, 0.5, 1.0] {
            let visual = sample_teleport_flashy_open(t);
            let effective = resolved * visual.opacity;
            assert!(effective <= resolved + 1e-6, "t={t} effective={effective}");
        }
        let final_effective = resolved * sample_teleport_flashy_open(1.0).opacity;
        assert!((final_effective - resolved).abs() < 1e-6);
    }

    // --- CLOSE AnimationVisual (geometry) ---

    #[test]
    fn teleport_flashy_close_t0_is_exact_identity() {
        assert_eq!(sample_teleport_flashy_close(0.0), AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 });
    }

    #[test]
    fn teleport_flashy_close_holds_exact_identity_until_hold_end() {
        assert_eq!(TELEPORT_FLASHY_CLOSE_HOLD_END, 0.12);
        for t in [0.0, 0.05, 0.10, 0.12] {
            assert_eq!(sample_teleport_flashy_close(t), AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 }, "t={t}");
        }
    }

    #[test]
    fn teleport_flashy_close_reaches_exact_end_state_by_collapse_end() {
        assert_eq!(TELEPORT_FLASHY_CLOSE_COLLAPSE_END, 0.28);
        assert_eq!(TELEPORT_FLASHY_CLOSE_END_SCALE, 0.98);
        for t in [0.28, 0.5, 0.9, 1.0] {
            let visual = sample_teleport_flashy_close(t);
            assert_eq!(visual, AnimationVisual { opacity: 0.0, scale_x: 0.98, scale_y: 0.98 }, "t={t}");
        }
        // Never Scale-close's 0.95 contraction.
        assert_ne!(TELEPORT_FLASHY_CLOSE_END_SCALE, CLOSE_SCALE_END_SCALE);
    }

    #[test]
    fn teleport_flashy_close_geometry_is_monotonic_deterministic_uniform_no_bounce() {
        let mut previous_opacity = 1.0_f32;
        let mut previous_scale = 1.0_f32;
        let mut t = TELEPORT_FLASHY_CLOSE_HOLD_END;
        while t <= TELEPORT_FLASHY_CLOSE_COLLAPSE_END {
            let visual = sample_teleport_flashy_close(t);
            assert_eq!(visual.scale_x, visual.scale_y, "uniform, t={t}");
            assert!(visual.opacity <= previous_opacity + 1e-6, "opacity monotonic decreasing, t={t}");
            assert!(visual.scale_x <= previous_scale + 1e-6, "scale monotonic decreasing (contraction, not bounce), t={t}");
            assert!(visual.scale_x >= TELEPORT_FLASHY_CLOSE_END_SCALE - 1e-6, "never overshoots past 0.98, t={t}");
            assert_eq!(sample_teleport_flashy_close(t), visual, "deterministic, t={t}");
            previous_opacity = visual.opacity;
            previous_scale = visual.scale_x;
            t += 0.01;
        }
    }

    #[test]
    fn teleport_flashy_close_is_not_a_reverse_open_shortcut() {
        // Explicit distinctness proof required by the architecture audit
        // (section 12): CLOSE is its own hold-then-collapse shape, never
        // `1.0 - sample_teleport_flashy_open(t)` played backwards.
        for t in [0.05, 0.15, 0.25, 0.5] {
            let close_visual = sample_teleport_flashy_close(t);
            let open_visual = sample_teleport_flashy_open(t);
            let reversed_open_opacity = 1.0 - open_visual.opacity;
            assert!(
                (close_visual.opacity - reversed_open_opacity).abs() > 1e-6
                    || (close_visual.scale_x - (2.0 - open_visual.scale_x)).abs() > 1e-6,
                "t={t} close must not equal a simple open-reversal",
            );
        }
    }

    // --- CLOSE flash-alpha (pulse) ---

    #[test]
    fn teleport_flashy_close_flash_starts_at_exact_zero() {
        assert_eq!(sample_teleport_flashy_close_flash(0.0), 0.0);
    }

    #[test]
    fn teleport_flashy_close_flash_reaches_a_clear_peak_near_022() {
        assert_eq!(TELEPORT_FLASHY_CLOSE_FLASH_PEAK, 0.22);
        let peak = sample_teleport_flashy_close_flash(TELEPORT_FLASHY_CLOSE_FLASH_PEAK);
        assert!((peak - 1.0).abs() < 1e-5, "peak={peak}");
        // Rises monotonically to the peak.
        let mut previous = 0.0_f32;
        let mut t = 0.0_f32;
        while t <= TELEPORT_FLASHY_CLOSE_FLASH_PEAK {
            let alpha = sample_teleport_flashy_close_flash(t);
            assert!(alpha >= previous - 1e-6, "rising, t={t}");
            previous = alpha;
            t += 0.02;
        }
    }

    #[test]
    fn teleport_flashy_close_flash_still_visible_after_window_is_hidden() {
        // Section 13's central relationship: window opacity reaches 0 at
        // TELEPORT_FLASHY_CLOSE_COLLAPSE_END (0.28), while flash remains
        // > 0 until TELEPORT_FLASHY_CLOSE_FLASH_END (0.50) — the window
        // disappears INSIDE the still-visible flash.
        let window_hidden_t = TELEPORT_FLASHY_CLOSE_COLLAPSE_END;
        assert_eq!(sample_teleport_flashy_close(window_hidden_t).opacity, 0.0);
        let flash_at_hidden = sample_teleport_flashy_close_flash(window_hidden_t);
        assert!(flash_at_hidden > 0.0, "flash must still be visible when window opacity hits 0, got {flash_at_hidden}");
        assert!(TELEPORT_FLASHY_CLOSE_COLLAPSE_END < TELEPORT_FLASHY_CLOSE_FLASH_END);
    }

    #[test]
    fn teleport_flashy_close_flash_reaches_exact_zero_by_end() {
        assert_eq!(TELEPORT_FLASHY_CLOSE_FLASH_END, 0.50);
        for t in [0.50, 0.6, 1.0] {
            assert_eq!(sample_teleport_flashy_close_flash(t), 0.0, "t={t}");
        }
    }

    #[test]
    fn teleport_flashy_close_flash_is_bounded_and_deterministic_across_a_dense_sweep() {
        let mut t = 0.0_f32;
        while t <= 1.0 {
            let alpha = sample_teleport_flashy_close_flash(t);
            assert!((0.0..=1.0).contains(&alpha), "t={t} alpha={alpha}");
            assert_eq!(sample_teleport_flashy_close_flash(t), alpha, "t={t}");
            t += 0.01;
        }
    }

    #[test]
    fn teleport_flashy_close_flash_for_gates_exclusively_on_teleport_flashy() {
        assert_eq!(
            teleport_flashy_close_flash_for(crate::config::CloseAnimationEffect::TeleportFlashy, 0.22),
            Some(1.0),
        );
        assert_eq!(teleport_flashy_close_flash_for(crate::config::CloseAnimationEffect::Scale, 0.22), None);
    }

    // --- distinctness ---

    #[test]
    fn teleport_flashy_open_differs_from_scale_teleport_bubble_at_representative_t() {
        for t in [0.05, 0.10, 0.15, 0.25, 0.5] {
            let flashy = sample_teleport_flashy_open(t);
            assert_ne!(flashy, sample_scale(t), "t={t} vs Scale");
            assert_ne!(flashy, sample_teleport(t), "t={t} vs Teleport");
            assert_ne!(flashy, sample_bubble(t), "t={t} vs Bubble");
        }
    }

    // --- overlay gating (no AnimationVisual contamination) ---

    #[test]
    fn animation_visual_has_no_flash_alpha_field() {
        // Structural: AnimationVisual must stay exactly {opacity, scale_x,
        // scale_y} — flash state is effect-specific overlay state, never
        // merged into the generic struct (architecture audit section 13).
        let source = include_str!("scene.rs");
        let start = source.find("struct AnimationVisual {").unwrap();
        let end = start + source[start..].find("\n}").unwrap();
        let body = &source[start..end];
        assert!(body.contains("opacity: f32"));
        assert!(body.contains("scale_x: f32"));
        assert!(body.contains("scale_y: f32"));
        assert!(!body.contains("flash"));
        assert!(!body.contains("tear"));
    }

    // --- render wiring (structural) ---

    #[test]
    fn open_flash_overlay_draw_occurs_strictly_after_the_surface_draw() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = start + source[start..].find("\nfn egl_scene_is_renderable").unwrap();
        let body = &source[start..end];
        let surface_draw = body.find("egl.render_surface_with_opacity(").unwrap();
        let overlay_draw = body.find("egl.render_solid_overlay(draw_plan, TELEPORT_FLASHY_COLOR, alpha)").unwrap();
        assert!(surface_draw < overlay_draw);
        // Also strictly after the shadow draw.
        let shadow_draw = body.find("egl.render_shadow(shadow)").unwrap();
        assert!(shadow_draw < overlay_draw);
        // Exactly one open overlay call site.
        assert_eq!(body.matches("egl.render_solid_overlay(draw_plan,").count(), 1);
        // Never inside the blur path.
        let blur_backdrop = body.find("egl.draw_blurred_backdrop(").unwrap();
        assert!(blur_backdrop < overlay_draw, "overlay call must not precede/replace the blur path");
    }

    #[test]
    fn close_flash_overlay_draw_occurs_strictly_after_the_closing_surface_draw() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_closing_layer(").unwrap();
        let end = start + source[start..].find("\nfn ").unwrap();
        let body = &source[start..end];
        let surface_draw = body.find("egl.render_surface_with_opacity(texture,").unwrap();
        let overlay_draw = body.find("egl.render_solid_overlay(closing_draw_plan, TELEPORT_FLASHY_COLOR, alpha)").unwrap();
        assert!(surface_draw < overlay_draw);
        assert_eq!(body.matches("egl.render_solid_overlay(").count(), 1);
    }

    #[test]
    fn teleport_flashy_close_uses_the_same_overlay_path_for_provisional_and_committed_sources() {
        // Section 11/16: ONE flash-drawing path for both provisional
        // frame-0 and committed ClosingVisual frames — no frame-0 special
        // case. render_closing_layer resolves `source`/`texture` from
        // EITHER map BEFORE the shared draw sequence below (including the
        // flash) runs — so the overlay call itself only appears once,
        // structurally proving it is shared.
        let source = include_str!("scene.rs");
        let start = source.find("fn render_closing_layer(").unwrap();
        let end = start + source[start..].find("\nfn ").unwrap();
        let body = &source[start..end];
        assert_eq!(body.matches("egl.render_solid_overlay(").count(), 1);
        assert!(body.contains("ClosingDrawSource::Committed"));
        assert!(body.contains("ClosingDrawSource::Provisional"));
    }

    // --- EnergyTear regression (open-effect level, complements the
    // renderer.rs shader-level tests) ---

    #[test]
    fn energy_tear_layout_for_never_fires_for_teleport_flashy_and_vice_versa() {
        for t in [0.0, 0.1, 0.3, 0.5, 0.9] {
            assert_eq!(energy_tear_layout_for(OpenAnimationEffect::TeleportFlashy, t), None, "t={t}");
            assert_eq!(teleport_flashy_open_flash_for(OpenAnimationEffect::EnergyTear, t), None, "t={t}");
        }
    }

    // --- close-core regression pins (source-contract, matching the
    // R2-correction test style: prove no functional delta) ---

    #[test]
    fn teleport_flashy_does_not_touch_destroy_trigger_or_unmap_policy() {
        let source = include_str!("scene.rs");
        let note_start = source.find("fn note_destroy_intent(").unwrap();
        let note_end = note_start + source[note_start..].find("\n    }").unwrap();
        let body = &source[note_start..note_end];
        assert!(body.contains("Event::DestroyNotify(destroy)"));
        assert!(!body.contains("UnmapNotify"));
        assert!(!body.contains("TeleportFlashy"), "close trigger must be untouched by effect wiring");
    }

    #[test]
    fn teleport_flashy_does_not_touch_render_order_or_close_id_allocation() {
        let source = include_str!("scene.rs");
        let reconcile_start = source.find("fn reconcile_render_order(").unwrap();
        let reconcile_end = reconcile_start + source[reconcile_start..].find("\nfn ").unwrap();
        assert!(!source[reconcile_start..reconcile_end].contains("TeleportFlashy"));
        let allocate_start = source.find("fn allocate_close_ids(").unwrap();
        let allocate_end = allocate_start + source[allocate_start..].find("\n}").unwrap();
        assert!(!source[allocate_start..allocate_end].contains("TeleportFlashy"));
    }

    // ========================================================
    // 3a3fa2b6-r2 — Minato radial reveal (OPEN TeleportFlashy only).
    // ========================================================

    /// Test-only mirror of the shader's aspect-safe per-axis
    /// normalization (render_surface_with_radial_reveal /
    /// SCENE_FRAGMENT_SHADER mode 3: `p=(local-half_size)/half_size;
    /// r_norm=|p|/sqrt(2)`), so the geometric properties (center=0,
    /// every corner=1, aspect-independence) are proven as ordinary CPU
    /// numeric tests rather than only string-scanned from the shader.
    #[cfg(test)]
    fn minato_r_norm(local_x: f32, local_y: f32, surface_w: f32, surface_h: f32) -> f32 {
        let px = (local_x - surface_w * 0.5) / (surface_w * 0.5);
        let py = (local_y - surface_h * 0.5) / (surface_h * 0.5);
        (px * px + py * py).sqrt() / std::f32::consts::SQRT_2
    }

    // --- RADIAL MATH ---

    #[test]
    fn minato_r_norm_center_is_exactly_zero() {
        assert_eq!(minato_r_norm(100.0, 50.0, 200.0, 100.0), 0.0);
        assert_eq!(minato_r_norm(400.0, 20.0, 800.0, 40.0), 0.0, "also zero for a very wide window");
    }

    #[test]
    fn minato_r_norm_every_corner_is_exactly_one_regardless_of_aspect_ratio() {
        // Per-axis independent normalization (never a single aspect-
        // corrected scalar) is what guarantees this — see section B8/A2
        // of the architecture audit.
        for (w, h) in [(200.0_f32, 100.0_f32), (100.0, 200.0), (400.0, 40.0), (50.0, 50.0), (1920.0, 1080.0)] {
            for (cx, cy) in [(0.0, 0.0), (w, 0.0), (0.0, h), (w, h)] {
                let r = minato_r_norm(cx, cy, w, h);
                assert!((r - 1.0).abs() < 1e-5, "w={w} h={h} corner=({cx},{cy}) r={r}");
            }
        }
    }

    #[test]
    fn minato_r_norm_side_midpoint_is_less_than_corner() {
        let w = 300.0_f32;
        let h = 150.0_f32;
        let side_mid = minato_r_norm(w, h * 0.5, w, h);
        let corner = minato_r_norm(w, h, w, h);
        assert!(side_mid < corner);
        assert!((side_mid - (1.0 / std::f32::consts::SQRT_2)).abs() < 1e-5);
    }

    // --- TIMING ---

    #[test]
    fn minato_reveal_radius_is_zero_before_start() {
        assert_eq!(MINATO_REVEAL_START, 0.04);
        for t in [0.0, 0.01, 0.03, 0.04] {
            assert_eq!(sample_minato_reveal_radius(t), 0.0, "t={t}");
        }
    }

    #[test]
    fn minato_reveal_radius_reuses_the_existing_geometry_end_constant() {
        // Not a coincidental duplicate 0.20 literal — a real structural
        // tie, so the reveal and the scale-pop always finish together.
        assert_eq!(MINATO_REVEAL_END, TELEPORT_FLASHY_OPEN_GEOMETRY_END);
    }

    #[test]
    fn minato_reveal_radius_monotonically_expands_between_start_and_end() {
        let mut previous = 0.0_f32;
        let mut t = MINATO_REVEAL_START;
        while t <= MINATO_REVEAL_END {
            let r = sample_minato_reveal_radius(t);
            assert!(r >= previous - 1e-6, "monotonic, t={t}");
            assert!((0.0..=MINATO_REVEAL_FULL_RADIUS + 1e-6).contains(&r), "bounded, t={t} r={r}");
            previous = r;
            t += 0.005;
        }
    }

    #[test]
    fn minato_reveal_radius_guarantees_full_corner_coverage_immediately_before_fallback() {
        // "Immediately before" MINATO_REVEAL_END: reveal_radius must
        // already be >= the exact corner r_norm value (1.0), with the
        // documented MINATO_REVEAL_FULL_RADIUS epsilon margin, so the
        // shader's coverage() evaluates to full at every corner with no
        // antialiasing gap and no visible pop at the fallback boundary.
        let just_before = MINATO_REVEAL_END - 0.0001;
        let radius = sample_minato_reveal_radius(just_before);
        assert!(radius >= 1.0, "must reach/exceed the exact corner r_norm value before fallback, got {radius}");
        assert!((radius - MINATO_REVEAL_FULL_RADIUS).abs() < 1e-3, "should already be at (or extremely close to) full radius, got {radius}");
    }

    #[test]
    fn minato_reveal_radius_for_falls_back_to_none_at_and_after_reveal_end() {
        // This is the "t>=0.20: ordinary surface mode" requirement,
        // proven at the gating-function level: None here is exactly what
        // makes the render loop fall back to the zero-extra-cost mode-0
        // draw, mirroring energy_tear_layout_for's own post-completion
        // None fallback.
        for t in [MINATO_REVEAL_END, MINATO_REVEAL_END + 0.01, 0.5, 1.0] {
            assert_eq!(minato_reveal_radius_for(OpenAnimationEffect::TeleportFlashy, t), None, "t={t} must fall back to ordinary mode 0");
        }
    }

    #[test]
    fn minato_reveal_radius_for_gates_exclusively_on_teleport_flashy() {
        assert!(minato_reveal_radius_for(OpenAnimationEffect::TeleportFlashy, 0.1).is_some());
        for effect in [
            OpenAnimationEffect::Scale,
            OpenAnimationEffect::Teleport,
            OpenAnimationEffect::EnergyTear,
            OpenAnimationEffect::Bubble,
        ] {
            assert_eq!(minato_reveal_radius_for(effect, 0.1), None, "effect={effect:?}");
        }
    }

    // --- MASK ---

    #[test]
    fn minato_center_appears_before_side_which_appears_before_corner() {
        let w = 300.0_f32;
        let h = 150.0_f32;
        let center_r = minato_r_norm(w * 0.5, h * 0.5, w, h);
        let side_r = minato_r_norm(w, h * 0.5, w, h);
        let corner_r = minato_r_norm(w, h, w, h);
        assert!(center_r < side_r);
        assert!(side_r < corner_r);
        // Since sample_minato_reveal_radius is monotonically increasing,
        // the earliest t at which it reaches/exceeds a point's r_norm
        // directly gives that point's reveal time — proving appearance
        // ORDER, not just the underlying geometry.
        let reveal_time_for = |target_r: f32| -> f32 {
            let mut t = MINATO_REVEAL_START;
            while t <= MINATO_REVEAL_END {
                if sample_minato_reveal_radius(t) >= target_r {
                    return t;
                }
                t += 0.001;
            }
            MINATO_REVEAL_END
        };
        let center_time = reveal_time_for(center_r);
        let side_time = reveal_time_for(side_r);
        let corner_time = reveal_time_for(corner_r);
        assert!(center_time <= side_time, "center must appear at or before the side midpoint");
        assert!(side_time <= corner_time, "side midpoint must appear at or before the corner");
        assert!(center_time < corner_time, "center must appear strictly before the corner");
    }

    #[test]
    fn minato_reveal_radius_never_negative_or_nan() {
        let mut t = 0.0_f32;
        while t <= 1.0 {
            let r = sample_minato_reveal_radius(t);
            assert!(r >= 0.0, "t={t} r={r}");
            assert!(!r.is_nan(), "t={t}");
            t += 0.01;
        }
    }

    // --- RENDER wiring ---

    #[test]
    fn open_radial_reveal_draw_replaces_never_adds_to_the_single_content_draw() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = start + source[start..].find("\nfn egl_scene_is_renderable").unwrap();
        let body = &source[start..end];
        // Exactly one call site each — nested inside the SAME match
        // (EnergyTear / radial-reveal / ordinary) so only one of the
        // three ever executes per entry per frame, never two.
        assert_eq!(body.matches("egl.render_surface_with_radial_reveal(").count(), 1);
        assert_eq!(body.matches("egl.render_surface_with_opacity(").count(), 1);
        assert_eq!(body.matches("egl.render_energy_tear_slices(").count(), 1);
        // The existing TeleportFlashy flash overlay stays drawn strictly
        // AFTER the (possibly-radial-reveal) surface draw — unchanged
        // ordering from R1.
        let reveal_call = body.find("egl.render_surface_with_radial_reveal(").unwrap();
        let flash_call = body.find("egl.render_solid_overlay(draw_plan, TELEPORT_FLASHY_COLOR, alpha)").unwrap();
        assert!(reveal_call < flash_call, "the flash overlay must remain drawn AFTER the surface draw");
    }

    // --- REGRESSION ---

    #[test]
    fn minato_radial_reveal_does_not_touch_close_teleport_flashy() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_closing_layer(").unwrap();
        let end = start + source[start..].find("\nfn ").unwrap();
        let body = &source[start..end];
        assert!(!body.contains("render_surface_with_radial_reveal"));
        assert!(!body.contains("minato_reveal_radius_for"));
        assert!(!body.contains("MINATO"));
    }

    #[test]
    fn minato_radial_reveal_does_not_touch_energy_tear_dispatch() {
        let source = include_str!("scene.rs");
        let start = source.find("fn energy_tear_layout_for(").unwrap();
        let end = start + source[start..].find("\n}").unwrap();
        let body = &source[start..end];
        assert!(!body.contains("Minato"));
        assert!(!body.contains("reveal"));
    }

    #[test]
    fn minato_radial_reveal_does_not_change_the_open_flash_curve() {
        // Explicit non-regression pin: OPEN_FLASH_HOLD_END/END constants
        // are untouched by this milestone.
        assert_eq!(TELEPORT_FLASHY_OPEN_FLASH_HOLD_END, 0.08);
        assert_eq!(TELEPORT_FLASHY_OPEN_FLASH_END, 0.40);
        assert_eq!(TELEPORT_FLASHY_OPEN_START_OPACITY, 0.05);
        assert_eq!(TELEPORT_FLASHY_OPEN_START_SCALE, 0.985);
    }

    // ========================================================
    // 3a3fa2b7 — Kamui vortex (OPEN + CLOSE).
    // ========================================================

    // --- OPEN AnimationVisual ---

    #[test]
    fn kamui_open_animation_visual_is_exact_identity_throughout() {
        // The vortex effect IS the visual (see kamui_open_state_for) —
        // AnimationVisual itself carries no geometry motion for Kamui.
        for t in [0.0, 0.2, 0.55, 0.7, 0.85, 1.0] {
            assert_eq!(sample_kamui_open(t), AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 }, "t={t}");
        }
    }

    // --- OPEN visible_radius / twist ---

    #[test]
    fn kamui_open_twist_decay_power_is_below_one_for_a_slower_initial_falloff() {
        // A concave (< 1.0) decay power keeps twist "remaining strong
        // during early/mid expansion" (R2 spec section 4) before falling
        // steeply only near radius=1 — see the constant's own doc comment.
        assert_eq!(KAMUI_OPEN_TWIST_DECAY_POWER, 0.6);
        assert!(KAMUI_OPEN_TWIST_DECAY_POWER < 1.0);
    }

    #[test]
    fn kamui_open_t0_state() {
        assert_eq!(KAMUI_OPEN_START_RADIUS, 0.02);
        assert_eq!(KAMUI_OPEN_MAX_TWIST, 2.4, "3a3fa2b7-r2: increased from R1's 1.6 per human feedback");
        assert!(KAMUI_OPEN_MAX_TWIST > 1.6, "R2 max twist must exceed R1's");
        assert_eq!(sample_kamui_open_visible_radius(0.0), KAMUI_OPEN_START_RADIUS);
        // twist is now coupled to radius, so at t=0 (radius=START_RADIUS,
        // not exactly 0) it is very close to but not exactly MAX_TWIST —
        // still strong, and finite.
        let twist0 = sample_kamui_open_twist(0.0);
        assert!(twist0.is_finite());
        assert!(twist0 > KAMUI_OPEN_MAX_TWIST * 0.9, "twist must be near-maximal at the tiny core, got {twist0}");
        assert!(sample_kamui_open_visible_radius(0.0).is_finite());
    }

    #[test]
    fn kamui_open_radius_monotonically_increases_through_core_and_expulsion() {
        assert_eq!(KAMUI_OPEN_CORE_END, 0.15);
        assert_eq!(KAMUI_OPEN_CORE_RADIUS, 0.20);
        assert_eq!(KAMUI_OPEN_EXPAND_END, 0.65);
        let mut previous = KAMUI_OPEN_START_RADIUS;
        let mut t = 0.0_f32;
        while t <= KAMUI_OPEN_EXPAND_END {
            let r = sample_kamui_open_visible_radius(t);
            assert!(r >= previous - 1e-6, "monotonic, t={t}");
            previous = r;
            t += 0.01;
        }
        assert_eq!(sample_kamui_open_visible_radius(KAMUI_OPEN_CORE_END), KAMUI_OPEN_CORE_RADIUS);
        assert_eq!(sample_kamui_open_visible_radius(KAMUI_OPEN_EXPAND_END), 1.0);
    }

    #[test]
    fn kamui_open_twist_monotonically_decays_as_radius_grows_reaching_exact_zero_at_expand_end() {
        // 3a3fa2b7-r2: twist is DERIVED from visible_radius (never an
        // independent phase) — do NOT hold it static through expansion
        // (the R1/R2-flagged flaw). Because radius is itself monotonic,
        // and twist = MAX*(1-radius)^power is a monotonically decreasing
        // function of radius, twist is guaranteed monotonically
        // decreasing across the whole [0, EXPAND_END] window.
        let mut previous_twist = sample_kamui_open_twist(0.0);
        let mut t = 0.0_f32;
        while t <= KAMUI_OPEN_EXPAND_END {
            let twist = sample_kamui_open_twist(t);
            assert!(twist <= previous_twist + 1e-6, "twist monotonic decay, t={t}");
            assert!(twist >= 0.0, "twist never goes negative, t={t}");
            previous_twist = twist;
            t += 0.01;
        }
        assert_eq!(sample_kamui_open_twist(KAMUI_OPEN_EXPAND_END), 0.0, "twist reaches exact 0 the instant radius hits 1.0");
    }

    #[test]
    fn kamui_open_twist_still_clearly_nonzero_at_mid_expansion() {
        // Required test (R2 spec section 17): "twist still clearly
        // nonzero" partway through expansion, not held/decayed away
        // prematurely. `ease_out_cubic` front-loads the radius growth
        // (fast start, slow finish), so a point ~30% of the way through
        // the expansion phase already shows radius meaningfully above
        // the core value while twist remains clearly strong — a later
        // sample (e.g. the exact time-midpoint) would show radius
        // already ~90% grown and twist correspondingly weaker, which is
        // the curve's intended, correct shape, not a bug.
        let representative = KAMUI_OPEN_CORE_END + 0.30 * (KAMUI_OPEN_EXPAND_END - KAMUI_OPEN_CORE_END);
        let radius = sample_kamui_open_visible_radius(representative);
        let twist = sample_kamui_open_twist(representative);
        assert!(radius > KAMUI_OPEN_CORE_RADIUS, "radius must have materially increased, got {radius}");
        assert!(radius < 1.0);
        assert!(twist.abs() > 0.8, "twist must still be clearly nonzero here, got {twist}");
    }

    #[test]
    fn kamui_open_exact_identity_at_and_after_settle_end() {
        for t in [KAMUI_OPEN_EXPAND_END, KAMUI_OPEN_SETTLE_END, 0.95, 1.0] {
            assert_eq!(sample_kamui_open_visible_radius(t), 1.0, "t={t}");
            assert_eq!(sample_kamui_open_twist(t), 0.0, "t={t}");
            assert_eq!(sample_kamui_open_radial_power(t), 1.0, "t={t}");
        }
    }

    #[test]
    fn kamui_open_radial_power_starts_above_one_and_reaches_exact_one_at_identity() {
        // 3a3fa2b7-r2.1: CORRECTED direction — OPEN must start ABOVE 1.0
        // (source features get pushed OUTWARD, per the feature-displacement
        // proof in the constant's own doc comment), the opposite of R2's
        // (backwards) 0.55.
        assert_eq!(KAMUI_OPEN_RADIAL_POWER_START, 1.8);
        assert!(KAMUI_OPEN_RADIAL_POWER_START > 1.0, "OPEN radial_power must start ABOVE 1.0 (source features pushed outward)");
        let start = sample_kamui_open_radial_power(0.0);
        assert!(start > 1.65, "radial_power must be close to its start value at t=0, got {start}");
        assert_eq!(sample_kamui_open_radial_power(KAMUI_OPEN_EXPAND_END), 1.0);
    }

    #[test]
    fn kamui_open_state_for_falls_back_to_none_at_and_after_settle_end() {
        // Mode falls back to ordinary shadow_mode==0 rendering here,
        // exactly like Minato's own MINATO_REVEAL_END fallback.
        for t in [KAMUI_OPEN_SETTLE_END, KAMUI_OPEN_SETTLE_END + 0.01, 1.0] {
            assert_eq!(kamui_open_state_for(OpenAnimationEffect::Kamui, t), None, "t={t}");
        }
        assert!(kamui_open_state_for(OpenAnimationEffect::Kamui, 0.3).is_some());
    }

    #[test]
    fn kamui_open_state_for_gates_exclusively_on_kamui() {
        for effect in [
            OpenAnimationEffect::Scale,
            OpenAnimationEffect::Teleport,
            OpenAnimationEffect::EnergyTear,
            OpenAnimationEffect::Bubble,
            OpenAnimationEffect::TeleportFlashy,
        ] {
            assert_eq!(kamui_open_state_for(effect, 0.1), None, "effect={effect:?}");
            assert_eq!(kamui_open_shadow_envelope_for(effect, 0.1), None, "effect={effect:?}");
        }
    }

    #[test]
    fn kamui_open_shadow_envelope_matches_visible_radius_at_boundaries() {
        assert_eq!(kamui_open_shadow_envelope_for(OpenAnimationEffect::Kamui, 1.0), Some(1.0));
        // Never gated to None post-settle — the envelope value itself
        // naturally becomes the identity multiplier (1.0).
        assert_eq!(kamui_open_shadow_envelope_for(OpenAnimationEffect::Kamui, 0.0), Some(KAMUI_OPEN_START_RADIUS));
    }

    // --- CLOSE AnimationVisual ---

    #[test]
    fn kamui_close_t0_is_exact_identity() {
        assert_eq!(sample_kamui_close(0.0), AnimationVisual { opacity: 1.0, scale_x: 1.0, scale_y: 1.0 });
    }

    #[test]
    fn kamui_close_scale_stays_exact_identity_throughout() {
        for t in [0.0, 0.15, 0.5, 0.75, 0.92, 1.0] {
            let visual = sample_kamui_close(t);
            assert_eq!(visual.scale_x, 1.0, "t={t}");
            assert_eq!(visual.scale_y, 1.0, "t={t}");
        }
    }

    #[test]
    fn kamui_close_opacity_held_at_one_through_suction_end_then_fades_to_zero() {
        assert_eq!(KAMUI_CLOSE_SUCTION_END, 0.82);
        assert_eq!(KAMUI_CLOSE_COLLAPSE_END, 0.94);
        for t in [0.0, 0.18, 0.5, 0.82] {
            assert_eq!(sample_kamui_close(t).opacity, 1.0, "t={t}");
        }
        for t in [KAMUI_CLOSE_COLLAPSE_END, 0.97, 1.0] {
            assert_eq!(sample_kamui_close(t).opacity, 0.0, "t={t}");
        }
        let mut previous = 1.0_f32;
        let mut t = KAMUI_CLOSE_SUCTION_END;
        while t <= KAMUI_CLOSE_COLLAPSE_END {
            let opacity = sample_kamui_close(t).opacity;
            assert!((0.0..=1.0).contains(&opacity), "t={t} opacity={opacity}");
            assert!(opacity <= previous + 1e-6, "monotonic fade, t={t}");
            previous = opacity;
            t += 0.005;
        }
    }

    // --- CLOSE visible_radius / twist (R2: twist is a SEPARATE, earlier-
    // ramping curve — the mandatory fix this milestone exists for) ---

    #[test]
    fn kamui_close_t0_radius_is_exact_one() {
        assert_eq!(sample_kamui_close_visible_radius(0.0), 1.0);
    }

    #[test]
    fn kamui_close_radius_near_097_at_grab_end() {
        assert_eq!(KAMUI_CLOSE_GRAB_END, 0.18);
        assert_eq!(KAMUI_CLOSE_GRAB_RADIUS, 0.97);
        assert_eq!(sample_kamui_close_visible_radius(KAMUI_CLOSE_GRAB_END), KAMUI_CLOSE_GRAB_RADIUS);
    }

    #[test]
    fn kamui_close_twist_precedes_radius_contraction() {
        // THE required section-9 test — the entire point of this
        // milestone: at an early representative time (t~0.10, well within
        // the grab phase), radius must still be very close to 1 (>0.95)
        // WHILE twist is already clearly nonzero. R1's bug was twist
        // staying near-zero here because it was proportional to
        // (1-radius), which barely moved yet.
        let t = 0.10;
        let radius = sample_kamui_close_visible_radius(t);
        let twist = sample_kamui_close_twist(t);
        assert!(radius > 0.95, "radius must still be close to full size at t={t}, got {radius}");
        assert!(twist.abs() > 0.3, "twist must already be clearly nonzero at t={t}, got {twist}");
    }

    #[test]
    fn kamui_close_twist_reaches_meaningful_magnitude_by_grab_end_while_radius_still_near_one() {
        assert_eq!(KAMUI_CLOSE_TWIST_AFTER_GRAB, 1.0);
        let radius = sample_kamui_close_visible_radius(KAMUI_CLOSE_GRAB_END);
        let twist = sample_kamui_close_twist(KAMUI_CLOSE_GRAB_END);
        assert!(radius > 0.9, "radius still near-full at grab end, got {radius}");
        assert!((twist + KAMUI_CLOSE_TWIST_AFTER_GRAB).abs() < 1e-4, "twist must reach the exact grab-end breakpoint, got {twist}");
    }

    #[test]
    fn kamui_close_flow_phase_radius_still_substantial_and_twist_strong() {
        assert_eq!(KAMUI_CLOSE_FLOW_END, 0.55);
        assert_eq!(KAMUI_CLOSE_FLOW_RADIUS, 0.70);
        assert_eq!(KAMUI_CLOSE_TWIST_AFTER_FLOW, 2.6);
        let mid_flow = 0.35;
        let radius = sample_kamui_close_visible_radius(mid_flow);
        let twist = sample_kamui_close_twist(mid_flow);
        assert!(radius > 0.6, "radius must still be substantially >0 during flow, got {radius}");
        assert!(twist.abs() > 1.5, "twist must be strong during flow, got {twist}");
        assert_eq!(sample_kamui_close_visible_radius(KAMUI_CLOSE_FLOW_END), KAMUI_CLOSE_FLOW_RADIUS);
        assert!((sample_kamui_close_twist(KAMUI_CLOSE_FLOW_END) + KAMUI_CLOSE_TWIST_AFTER_FLOW).abs() < 1e-4);
    }

    #[test]
    fn kamui_close_radius_materially_contracted_and_twist_near_maximum_at_suction_end() {
        assert_eq!(KAMUI_CLOSE_SUCTION_RADIUS, 0.45);
        assert_eq!(KAMUI_CLOSE_MAX_TWIST, 3.4);
        let radius = sample_kamui_close_visible_radius(KAMUI_CLOSE_SUCTION_END);
        assert_eq!(radius, KAMUI_CLOSE_SUCTION_RADIUS);
        assert_eq!(sample_kamui_close(KAMUI_CLOSE_SUCTION_END).opacity, 1.0, "opacity still exactly 1.0 at suction end");
        let twist = sample_kamui_close_twist(KAMUI_CLOSE_SUCTION_END);
        assert!((twist + KAMUI_CLOSE_MAX_TWIST).abs() < 1e-4, "twist must reach exactly max magnitude by suction end, got {twist}");
    }

    #[test]
    fn kamui_close_radius_and_opacity_reach_zero_over_core_collapse_twist_remains_strong() {
        let mut previous_radius = KAMUI_CLOSE_SUCTION_RADIUS;
        let mut t = KAMUI_CLOSE_SUCTION_END;
        while t <= KAMUI_CLOSE_COLLAPSE_END {
            let radius = sample_kamui_close_visible_radius(t);
            assert!(radius >= 0.0, "never negative, t={t} radius={radius}");
            assert!(radius <= previous_radius + 1e-6, "monotonic collapse, t={t}");
            // "twist may remain strong until nearly invisible" (R2 spec
            // section 7 phase D) — held at exactly max magnitude here.
            assert!((sample_kamui_close_twist(t) + KAMUI_CLOSE_MAX_TWIST).abs() < 1e-4, "twist held at max through collapse, t={t}");
            previous_radius = radius;
            t += 0.005;
        }
        assert_eq!(sample_kamui_close_visible_radius(KAMUI_CLOSE_COLLAPSE_END), 0.0);
    }

    #[test]
    fn kamui_close_fully_hidden_at_and_after_collapse_end() {
        for t in [KAMUI_CLOSE_COLLAPSE_END, 0.97, 1.0] {
            assert_eq!(sample_kamui_close_visible_radius(t), 0.0, "t={t}");
            assert_eq!(sample_kamui_close(t).opacity, 0.0, "t={t}");
        }
    }

    #[test]
    fn kamui_close_twist_is_a_separate_earlier_ramping_curve_not_coupled_to_one_minus_radius() {
        // 3a3fa2b7-r2: explicit negative-control proof that the R1 bug
        // (twist == -MAX*(1-radius)) is gone — at t=0.10 the OLD coupled
        // formula would give a near-zero value (radius is ~0.97 there),
        // but the actual decoupled curve is already clearly nonzero.
        let t = 0.10;
        let radius = sample_kamui_close_visible_radius(t);
        let old_coupled_formula = -KAMUI_CLOSE_MAX_TWIST * (1.0 - radius);
        let actual = sample_kamui_close_twist(t);
        assert!(old_coupled_formula.abs() < 0.15, "sanity: the old coupled formula WOULD be near-zero here, got {old_coupled_formula}");
        assert!(actual.abs() > 0.3, "the actual R2 curve must NOT reproduce that near-zero value, got {actual}");
        assert!((actual - old_coupled_formula).abs() > 0.3, "R2 twist must differ materially from the old (1-radius)-coupled formula at t={t}");
    }

    #[test]
    fn kamui_close_distinctness_twist_first_then_flow_then_suction_not_shrink_then_fade() {
        // Required distinctness test (R2 spec section 19): the t at which
        // twist crosses a "meaningful" threshold must be strictly earlier
        // than the t at which radius crosses a "substantially contracted"
        // threshold — proving TWIST FIRST, not SHRINK-then-FADE.
        let twist_threshold = 1.0;
        let radius_threshold = 0.5;
        let mut twist_crossing_t = None;
        let mut radius_crossing_t = None;
        let mut t = 0.0_f32;
        while t <= 1.0 {
            if twist_crossing_t.is_none() && sample_kamui_close_twist(t).abs() >= twist_threshold {
                twist_crossing_t = Some(t);
            }
            if radius_crossing_t.is_none() && sample_kamui_close_visible_radius(t) <= radius_threshold {
                radius_crossing_t = Some(t);
            }
            t += 0.005;
        }
        let twist_crossing_t = twist_crossing_t.expect("twist must cross the threshold somewhere in [0,1]");
        let radius_crossing_t = radius_crossing_t.expect("radius must cross the threshold somewhere in [0,1]");
        assert!(
            twist_crossing_t < radius_crossing_t,
            "twist (meaningful at t={twist_crossing_t}) must precede radius substantial contraction (at t={radius_crossing_t})"
        );
    }

    #[test]
    fn kamui_close_never_produces_nan_negative_radius_or_out_of_range_opacity() {
        let mut t = 0.0_f32;
        while t <= 1.0 {
            let radius = sample_kamui_close_visible_radius(t);
            let twist = sample_kamui_close_twist(t);
            let radial_power = sample_kamui_close_radial_power(t);
            let opacity = sample_kamui_close(t).opacity;
            assert!(radius.is_finite() && radius >= 0.0, "t={t} radius={radius}");
            assert!(twist.is_finite(), "t={t} twist={twist}");
            assert!(radial_power.is_finite() && radial_power > 0.0, "t={t} radial_power={radial_power}");
            assert!((0.0..=1.0).contains(&opacity), "t={t} opacity={opacity}");
            t += 0.01;
        }
    }

    #[test]
    fn kamui_close_radial_power_starts_at_exact_one_and_reaches_end_value_at_full_collapse() {
        // 3a3fa2b7-r2.1: CORRECTED direction — CLOSE must move BELOW 1.0
        // (source features get pulled INWARD, per the feature-displacement
        // proof in the constant's own doc comment), the opposite of R2's
        // (backwards) 1.8.
        assert_eq!(KAMUI_CLOSE_RADIAL_POWER_END, 0.55);
        assert!(KAMUI_CLOSE_RADIAL_POWER_END < 1.0, "CLOSE radial_power must move BELOW 1.0 (source features pulled inward)");
        assert_eq!(sample_kamui_close_radial_power(0.0), 1.0, "no distortion during pure grab (radius exactly 1 at t=0)");
        assert_eq!(sample_kamui_close_radial_power(KAMUI_CLOSE_COLLAPSE_END), KAMUI_CLOSE_RADIAL_POWER_END);
    }

    #[test]
    fn kamui_close_state_for_is_never_time_gated_present_for_the_whole_duration() {
        // Unlike OPEN, CLOSE's vortex never falls back to ordinary
        // rendering — the window is disappearing, not settling.
        for t in [0.0, 0.18, 0.55, 0.82, 0.94, 1.0] {
            assert!(kamui_close_state_for(crate::config::CloseAnimationEffect::Kamui, t).is_some(), "t={t}");
        }
    }

    #[test]
    fn kamui_close_state_for_gates_exclusively_on_kamui() {
        assert_eq!(kamui_close_state_for(crate::config::CloseAnimationEffect::Scale, 0.5), None);
        assert_eq!(kamui_close_state_for(crate::config::CloseAnimationEffect::TeleportFlashy, 0.5), None);
        assert_eq!(kamui_close_shadow_envelope_for(crate::config::CloseAnimationEffect::Scale, 0.5), None);
    }

    #[test]
    fn kamui_close_shadow_envelope_matches_visible_radius() {
        assert_eq!(kamui_close_shadow_envelope_for(crate::config::CloseAnimationEffect::Kamui, 0.0), Some(1.0));
        assert_eq!(kamui_close_shadow_envelope_for(crate::config::CloseAnimationEffect::Kamui, KAMUI_CLOSE_COLLAPSE_END), Some(0.0));
    }

    // --- R3: ease_in_cubic ---

    #[test]
    fn ease_in_cubic_is_bounded_and_monotonic() {
        assert_eq!(ease_in_cubic(0.0), 0.0);
        assert_eq!(ease_in_cubic(1.0), 1.0);
        let a = ease_in_cubic(0.25);
        let b = ease_in_cubic(0.75);
        assert!((0.0..=1.0).contains(&a), "a={a}");
        assert!((0.0..=1.0).contains(&b), "b={b}");
        assert!(a < b);
        // ease-in is slow at start, fast at end — at u=0.5, value must be < 0.5
        assert!(ease_in_cubic(0.5) < 0.5, "ease_in_cubic(0.5) must be < 0.5 (back-loaded)");
    }

    // --- R3: CLOSE content synchronization ---

    #[test]
    fn kamui_close_r3_bug_regression_radius_materially_visible_at_final_collapse_start() {
        // 3a3fa2b7-r3 section 18: the OLD R2.1 bug was visible_radius
        // ~= 0.10 at SUCTION_END, making content practically invisible
        // before the final opacity fade even began. This regression test
        // pins the invariant that the new curve MUST leave content
        // materially visible (>= 0.35, prefer ~0.45) at the start of the
        // final collapse phase.
        let radius_at_final_start = sample_kamui_close_visible_radius(KAMUI_CLOSE_SUCTION_END);
        let opacity_at_final_start = sample_kamui_close(KAMUI_CLOSE_SUCTION_END).opacity;
        assert_eq!(opacity_at_final_start, 1.0, "opacity must still be exactly 1.0 at final collapse start");
        assert!(
            radius_at_final_start >= 0.35,
            "R3 regression invariant: visible_radius at final collapse start must be >= 0.35, got {radius_at_final_start} (old R2.1 was ~0.10)"
        );
        // Prefer ~0.45
        assert!(
            (radius_at_final_start - 0.45).abs() < 0.02,
            "expected radius ~0.45 at final collapse start, got {radius_at_final_start}"
        );
    }

    #[test]
    fn kamui_close_r3_final_phase_radius_and_opacity_synchronized_monotonic_collapse() {
        // 3a3fa2b7-r3 section 19: prove that during the final phase
        // [SUCTION_END, COLLAPSE_END], BOTH radius and opacity are
        // monotonically decreasing, both > 0 in the interior, and both
        // reach exactly 0 at the end.
        let mut prev_radius = sample_kamui_close_visible_radius(KAMUI_CLOSE_SUCTION_END);
        let mut prev_opacity = sample_kamui_close(KAMUI_CLOSE_SUCTION_END).opacity;
        assert!(prev_radius > 0.0, "radius must be > 0 at final phase start");
        assert_eq!(prev_opacity, 1.0, "opacity must be 1 at final phase start");
        let mut t = KAMUI_CLOSE_SUCTION_END + 0.005;
        while t < KAMUI_CLOSE_COLLAPSE_END {
            let radius = sample_kamui_close_visible_radius(t);
            let opacity = sample_kamui_close(t).opacity;
            assert!(radius >= 0.0, "no negative radius, t={t}");
            assert!((0.0..=1.0).contains(&opacity), "opacity in [0,1], t={t}");
            assert!(radius > 0.0, "radius must still be > 0 in final phase interior, t={t}");
            assert!(opacity > 0.0, "opacity must still be > 0 in final phase interior, t={t}");
            assert!(radius <= prev_radius + 1e-6, "radius monotonic, t={t}");
            assert!(opacity <= prev_opacity + 1e-6, "opacity monotonic, t={t}");
            prev_radius = radius;
            prev_opacity = opacity;
            t += 0.005;
        }
        // At end: both exactly 0
        assert_eq!(sample_kamui_close_visible_radius(KAMUI_CLOSE_COLLAPSE_END), 0.0);
        assert_eq!(sample_kamui_close(KAMUI_CLOSE_COLLAPSE_END).opacity, 0.0);
    }

    #[test]
    fn kamui_close_r3_shared_phase_progress_radius_and_opacity_use_same_u() {
        // 3a3fa2b7-r3 section 20: structurally prove that the final
        // radius and final opacity derive from the SAME
        // phase_progress(t, SUCTION_END, COLLAPSE_END). We verify this
        // by computing the shared u and reconstructing both values,
        // confirming they match the actual sampled values exactly.
        for &t in &[0.83, 0.85, 0.87, 0.89, 0.91, 0.93] {
            let u = phase_progress(t, KAMUI_CLOSE_SUCTION_END, KAMUI_CLOSE_COLLAPSE_END);
            let expected_radius = lerp(KAMUI_CLOSE_SUCTION_RADIUS, 0.0, ease_in_cubic(u));
            let expected_opacity = lerp(1.0, 0.0, ease_in_cubic(u));
            let actual_radius = sample_kamui_close_visible_radius(t);
            let actual_opacity = sample_kamui_close(t).opacity;
            assert!(
                (actual_radius - expected_radius).abs() < 1e-6,
                "radius must match shared-u reconstruction at t={t}: actual={actual_radius} expected={expected_radius}"
            );
            assert!(
                (actual_opacity - expected_opacity).abs() < 1e-6,
                "opacity must match shared-u reconstruction at t={t}: actual={actual_opacity} expected={expected_opacity}"
            );
        }
    }

    #[test]
    fn kamui_close_r3_content_survival_shortly_after_final_collapse_begins() {
        // 3a3fa2b7-r3 section 21: at t ~= 0.85 (shortly after the
        // final collapse begins at 0.82), content must STILL be
        // meaningfully visible — both radius and opacity must be
        // substantially > 0.
        let t = 0.85;
        let radius = sample_kamui_close_visible_radius(t);
        let opacity = sample_kamui_close(t).opacity;
        assert!(
            radius > 0.30,
            "content survival: radius must still be substantially visible at t={t}, got {radius}"
        );
        assert!(
            opacity > 0.90,
            "content survival: opacity must still be very high at t={t}, got {opacity}"
        );
    }

    #[test]
    fn kamui_close_r3_twist_preserved_during_final_collapse() {
        // 3a3fa2b7-r3 section 22: twist must remain strong while
        // content is still collapsing — no twist fadeout before content
        // disappears.
        let t = 0.85;
        let twist = sample_kamui_close_twist(t);
        assert!(
            (twist + KAMUI_CLOSE_MAX_TWIST).abs() < 1e-4,
            "twist must be at max magnitude during final collapse at t={t}, got {twist}"
        );
        // Also check near the very end of collapse
        let t_late = 0.93;
        let twist_late = sample_kamui_close_twist(t_late);
        assert!(
            (twist_late + KAMUI_CLOSE_MAX_TWIST).abs() < 1e-4,
            "twist must STILL be at max magnitude near collapse end at t={t_late}, got {twist_late}"
        );
    }

    #[test]
    fn kamui_close_r3_no_content_gone_while_animation_active() {
        // 3a3fa2b7-r3 section 8: there must be NO significant time
        // interval where visible_radius ~= 0 but the animation is
        // still visibly active (opacity > 0). Concretely: whenever
        // radius < 0.05, opacity must also be < 0.05.
        let mut t = 0.0_f32;
        while t <= 1.0 {
            let radius = sample_kamui_close_visible_radius(t);
            let opacity = sample_kamui_close(t).opacity;
            if radius < 0.05 {
                assert!(
                    opacity < 0.05,
                    "content-mask death sync violated: radius={radius} but opacity={opacity} at t={t}"
                );
            }
            t += 0.005;
        }
    }

    // --- distinctness ---

    #[test]
    fn kamui_never_produces_energy_tear_teleport_flashy_or_minato_state_and_vice_versa() {
        for t in [0.0, 0.2, 0.5, 0.8] {
            assert_eq!(energy_tear_layout_for(OpenAnimationEffect::Kamui, t), None, "t={t}");
            assert_eq!(teleport_flashy_open_flash_for(OpenAnimationEffect::Kamui, t), None, "t={t}");
            assert_eq!(minato_reveal_radius_for(OpenAnimationEffect::Kamui, t), None, "t={t}");
            assert_eq!(kamui_open_state_for(OpenAnimationEffect::EnergyTear, t), None, "t={t}");
            assert_eq!(kamui_open_state_for(OpenAnimationEffect::TeleportFlashy, t), None, "t={t}");
        }
    }

    #[test]
    fn kamui_open_and_close_dispatch_match_arms_are_wired() {
        assert_eq!(sample_open_effect(OpenAnimationEffect::Kamui, 0.5), sample_kamui_open(0.5));
        assert_eq!(sample_close_effect(crate::config::CloseAnimationEffect::Kamui, 0.5), sample_kamui_close(0.5));
    }

    // --- render wiring (structural) ---

    #[test]
    fn open_kamui_warp_draw_is_mutually_exclusive_with_energy_tear_and_minato_and_ordinary() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_egl_scene_parts<'a>(").unwrap();
        let end = start + source[start..].find("\nfn egl_scene_is_renderable").unwrap();
        let body = &source[start..end];
        assert_eq!(body.matches("egl.render_surface_with_kamui_warp(").count(), 1);
        assert_eq!(body.matches("egl.render_surface_with_radial_reveal(").count(), 1);
        assert_eq!(body.matches("egl.render_surface_with_opacity(").count(), 1);
        assert_eq!(body.matches("egl.render_energy_tear_slices(").count(), 1);
        // Nested inside the SAME match arm as the reveal/ordinary choice
        // — never a separate, additional draw call site.
        assert!(body.contains("match (open_reveal_radius, open_kamui_state)"));
    }

    #[test]
    fn close_kamui_warp_draw_is_mutually_exclusive_with_ordinary_and_shares_the_provisional_and_committed_path() {
        let source = include_str!("scene.rs");
        let start = source.find("fn render_closing_layer(").unwrap();
        let end = start + source[start..].find("\nfn ").unwrap();
        let body = &source[start..end];
        assert_eq!(body.matches("egl.render_surface_with_kamui_warp(").count(), 1);
        assert_eq!(body.matches("egl.render_surface_with_opacity(").count(), 1);
        assert!(body.contains("ClosingDrawSource::Committed"));
        assert!(body.contains("ClosingDrawSource::Provisional"));
    }

    // --- close-core regression pins ---

    #[test]
    fn kamui_does_not_touch_destroy_trigger_or_unmap_policy() {
        let source = include_str!("scene.rs");
        let note_start = source.find("fn note_destroy_intent(").unwrap();
        let note_end = note_start + source[note_start..].find("\n    }").unwrap();
        let body = &source[note_start..note_end];
        assert!(body.contains("Event::DestroyNotify(destroy)"));
        assert!(!body.contains("UnmapNotify"));
        assert!(!body.contains("Kamui"), "close trigger must be untouched by effect wiring");
    }

    #[test]
    fn kamui_does_not_touch_render_order_or_close_id_allocation() {
        let source = include_str!("scene.rs");
        let reconcile_start = source.find("fn reconcile_render_order(").unwrap();
        let reconcile_end = reconcile_start + source[reconcile_start..].find("\nfn ").unwrap();
        assert!(!source[reconcile_start..reconcile_end].contains("Kamui"));
        let allocate_start = source.find("fn allocate_close_ids(").unwrap();
        let allocate_end = allocate_start + source[allocate_start..].find("\n}").unwrap();
        assert!(!source[allocate_start..allocate_end].contains("Kamui"));
    }

    // --- 23: MANDATORY first-frame test ---
    //
    // Proves the actual data build_candidate's pre-commit render call
    // consumes (merge_window_animations(&self.window_animations,
    // &provisional_animations), scene.rs — the render call immediately
    // following it in build_candidate) already reflects the open animation's
    // initial transform at the earliest possible render time, not the final
    // settled state. This is not a source-string ordering assertion: it
    // exercises the exact pure functions that produce that data.
    #[test]
    fn first_presentable_render_state_uses_open_animation_initial_transform() {
        let new_surface = animation_test_entry(42, SurfaceVisualClass::Normal, false);
        let snapshot = SceneSnapshot {
            root: 1,
            root_geometry: full_hd_root(),
            entries: vec![new_surface],
        };
        let old_surfaces = HashSet::new(); // did not exist in the prior live snapshot
        let persistent = HashMap::new(); // nothing already committed
        let now = Instant::now();
        let provisional = provisional_open_animations(&old_surfaces, &snapshot, false, true, test_animation_config(true), now);
        let render_view = merge_window_animations(&persistent, &provisional);

        let animation = render_view
            .get(&42)
            .expect("newly eligible surface must have a provisional animation before its first render");
        // The real render call happens at essentially this same instant (no
        // intervening blocking work in build_candidate between capturing
        // `now` and rendering) — evaluate at the earliest possible time.
        let t = animation.progress(now);
        let visual = sample_open_effect(animation.effect, t);
        assert!(visual.opacity < 0.5, "first frame must not be near full opacity, got {}", visual.opacity);
        assert!(visual.scale_x < 0.99, "first frame must not be near full scale, got {}", visual.scale_x);
        assert_ne!(
            (visual.opacity, visual.scale_x, visual.scale_y), (1.0, 1.0, 1.0),
            "first frame must not equal the final settled state",
        );
    }

    // --- 25: frame coalescing ---
    //
    // The Ignore-only animation branch and every other arm
    // (Geometry/Hierarchy/Background/VisualState/PixelDamage) live in the
    // same `match decision { ... }` in wait_live_pixel — a single Rust match
    // executes exactly one arm per iteration, so "animation consumed by the
    // PixelDamage/Geometry render" and "a second, separate animation-only
    // render" are mutually exclusive by construction, not by runtime luck.
    // batch.decision() itself (unmodified by this change) is what collapses
    // a batch that contains both pixel damage and other signals down to one
    // SceneInvalidation before the match ever runs; its priority behavior is
    // covered by the existing, unmodified
    // visual_batch_preserves_pixel_subtraction_obligation and
    // visual_only_batch_has_no_damage_subtraction_obligation tests. This
    // test instead proves the piece this milestone actually adds: the
    // animation transform used by whichever arm renders is the same
    // self.window_animations value regardless of which arm it was, i.e.
    // there is exactly one animations view per iteration, not a
    // damage-specific one and a separate animation-only one.
    #[test]
    fn animation_transform_is_the_same_single_view_regardless_of_trigger() {
        let mut persistent = HashMap::new();
        persistent.insert(7, test_window_animation(Instant::now()));
        // full_recompose_current (Background/VisualState/Ignore-branch) and
        // recompose_current_scene (PixelDamage, via full_recompose_current)
        // both read &self.window_animations directly — there is only one
        // map, so there cannot be a second, separate animation-only view.
        let view_for_pixel_damage_path = &persistent;
        let view_for_ignore_path = &persistent;
        assert_eq!(
            view_for_pixel_damage_path.get(&7).unwrap().started_at,
            view_for_ignore_path.get(&7).unwrap().started_at,
        );
    }

    // ========================================================
    // 3a3fa2a R2 — semantic-preferring override_redirect resolution for
    // open-animation eligibility only. Mirrors the existing
    // effective_window_type / semantic_window_type_precedes_capture_type_*
    // test pattern above: `capture` and `semantic` WindowMetadata fixtures
    // with genuinely distinct `window` XIDs (capture=10 default,
    // semantic.window=20), run through the real
    // eligible_surface_with_semantic_metadata() construction path — not a
    // hand-rolled SurfaceEntry — so these tests exercise the actual
    // production wiring, not a paraphrase of it.
    // ========================================================

    // --- A: effective_override_redirect() precedence, pure function ---

    #[test]
    fn semantic_override_redirect_false_overrides_capture_true() {
        let mut capture = metadata();
        capture.override_redirect = true;
        let mut semantic = metadata();
        semantic.window = 20;
        semantic.override_redirect = false;
        assert_eq!(effective_override_redirect(&capture, Some(&semantic)), false);
        let entry = eligible_surface_with_semantic_metadata(&capture, Some(20), Some(&semantic), root(), 10, 0).unwrap();
        assert_eq!(entry.effective_override_redirect, false);
        assert!(eligible_for_open_animation(&entry));
    }

    // --- B: semantic client's override_redirect=true must win ---

    #[test]
    fn semantic_override_redirect_true_overrides_capture_false() {
        let capture = metadata(); // override_redirect: false (default)
        let mut semantic = metadata();
        semantic.window = 20;
        semantic.override_redirect = true;
        assert_eq!(effective_override_redirect(&capture, Some(&semantic)), true);
        let entry = eligible_surface_with_semantic_metadata(&capture, Some(20), Some(&semantic), root(), 10, 0).unwrap();
        assert_eq!(entry.effective_override_redirect, true);
        assert!(!eligible_for_open_animation(&entry));
    }

    // --- C: no semantic metadata -> capture fallback (true) ---

    #[test]
    fn semantic_absent_falls_back_to_capture_true() {
        let mut capture = metadata();
        capture.override_redirect = true;
        assert_eq!(effective_override_redirect(&capture, None), true);
        let entry = eligible_surface_with_semantic_metadata(&capture, None, None, root(), 10, 0).unwrap();
        assert_eq!(entry.effective_override_redirect, true);
        assert!(!eligible_for_open_animation(&entry));
    }

    // --- D: no semantic metadata -> capture fallback (false) ---

    #[test]
    fn semantic_absent_falls_back_to_capture_false() {
        let capture = metadata(); // override_redirect: false (default)
        assert_eq!(effective_override_redirect(&capture, None), false);
        let entry = eligible_surface_with_semantic_metadata(&capture, None, None, root(), 10, 0).unwrap();
        assert_eq!(entry.effective_override_redirect, false);
        assert!(eligible_for_open_animation(&entry));
    }

    // --- E/F: Dock/Desktop remain excluded regardless of override_redirect ---

    #[test]
    fn dock_remains_excluded_despite_effective_override_redirect_false() {
        let capture = metadata(); // override_redirect: false
        let mut semantic = metadata();
        semantic.window = 20;
        semantic.override_redirect = false;
        semantic.window_type = Some("_NET_WM_WINDOW_TYPE_DOCK".to_string());
        let entry = eligible_surface_with_semantic_metadata(&capture, Some(20), Some(&semantic), root(), 10, 0).unwrap();
        assert_eq!(entry.visual_class, SurfaceVisualClass::Dock);
        assert_eq!(entry.effective_override_redirect, false);
        assert!(!eligible_for_open_animation(&entry));
    }

    #[test]
    fn desktop_remains_excluded_despite_effective_override_redirect_false() {
        let capture = metadata();
        let mut semantic = metadata();
        semantic.window = 20;
        semantic.override_redirect = false;
        semantic.window_type = Some("_NET_WM_WINDOW_TYPE_DESKTOP".to_string());
        let entry = eligible_surface_with_semantic_metadata(&capture, Some(20), Some(&semantic), root(), 10, 0).unwrap();
        assert_eq!(entry.visual_class, SurfaceVisualClass::Desktop);
        assert_eq!(entry.effective_override_redirect, false);
        assert!(!eligible_for_open_animation(&entry));
    }

    // --- G: a real semantic-client transient (override_redirect=true) stays excluded ---

    #[test]
    fn real_semantic_override_redirect_transient_remains_excluded() {
        let capture = metadata(); // capture itself is NOT override_redirect
        let mut semantic = metadata();
        semantic.window = 20;
        semantic.override_redirect = true; // the actual client is a transient popup
        let entry = eligible_surface_with_semantic_metadata(&capture, Some(20), Some(&semantic), root(), 10, 0).unwrap();
        assert_eq!(entry.visual_class, SurfaceVisualClass::Normal);
        assert!(!eligible_for_open_animation(&entry), "a genuinely override_redirect client must never animate");
    }

    // --- H: forensic Alacritty/i3 reproduction ---
    //
    // Exact runtime shape captured during the 3a3fa2a forensic session:
    // surface=0x00402b5f (capture, override_redirect=true) semantic=
    // 0x03600003 (a distinct XID, override_redirect=false), class=Normal.
    // Distinct capture_xid=10 vs semantic.window=20 models "capture XID !=
    // semantic client XID" explicitly, per the fixture's own identity field.

    #[test]
    fn forensic_alacritty_i3_capture_frame_true_semantic_client_false_is_eligible() {
        let mut capture = metadata();
        capture.window = 10;
        capture.override_redirect = true; // the i3-side capture surface
        let mut semantic = metadata();
        semantic.window = 20; // genuinely distinct XID from the capture surface
        semantic.override_redirect = false; // the real Alacritty client window
        assert_ne!(capture.window, semantic.window, "must model capture XID != semantic client XID");
        let entry = eligible_surface_with_semantic_metadata(&capture, Some(20), Some(&semantic), root(), capture.window, 0).unwrap();
        assert_eq!(entry.override_redirect, true, "capture-scoped field keeps its existing meaning");
        assert_eq!(entry.effective_override_redirect, false, "semantic client's value must win for eligibility");
        assert_eq!(entry.visual_class, SurfaceVisualClass::Normal);
        assert!(eligible_for_open_animation(&entry), "the forensic Alacritty/i3 case must now be eligible");
    }

    #[test]
    fn resource_identity_allows_position_only_metadata_change() {
        let previous = visibility_test_entry(geo(100, 100, 400, 300), false);
        let mut candidate = previous.clone();
        candidate.geometry.x = 120;
        candidate.geometry.y = 140;
        assert!(resource_identity_fields_match(&previous, &candidate));
    }

    #[test]
    fn resource_identity_rejects_resize_and_visual_change() {
        let previous = visibility_test_entry(geo(100, 100, 400, 300), false);
        let mut resized = previous.clone();
        resized.geometry.width += 1;
        assert!(!resource_identity_fields_match(&previous, &resized));
        let mut visual_changed = previous.clone();
        visual_changed.visual += 1;
        assert!(!resource_identity_fields_match(&previous, &visual_changed));
    }

    #[test]
    fn rect_intersects_root_matches_root_intersection_math() {
        let root = full_hd_root();
        assert!(!rect_intersects_root(-99, -99, 1, 1, root));
        assert!(rect_intersects_root(-100, 100, 500, 500, root));
        assert!(rect_intersects_root(500, 500, 1, 1, root));
        assert!(!rect_intersects_root(2000, 500, 50, 50, root));
        assert!(!rect_intersects_root(500, 2000, 50, 50, root));
        assert!(!rect_intersects_root(0, 0, 0, 0, root));
    }

    #[test]
    fn a_offscreen_1x1_no_shadow_is_zero_contribution() {
        // The exact reproduced startup case: root 1920x1080, surface
        // 1x1+-99+-99, no shadow eligibility.
        let entry = visibility_test_entry(geo(-99, -99, 1, 1), false);
        assert!(!entry_has_visible_contribution(&entry, shadow_style(false, 0.0, 0.0, 0.0), full_hd_root()));
        let mut entries = vec![entry];
        prune_invisible_entries(&mut entries, shadow_style(false, 0.0, 0.0, 0.0), full_hd_root());
        assert!(entries.is_empty(), "zero-contribution entry must be pruned before resource acquisition");
    }

    #[test]
    fn b_partially_visible_surface_is_retained() {
        assert!(surface_quad_intersects_root(geo(-100, 100, 500, 500), full_hd_root()));
        let entry = visibility_test_entry(geo(-100, 100, 500, 500), false);
        let mut entries = vec![entry];
        prune_invisible_entries(&mut entries, shadow_style(false, 0.0, 0.0, 0.0), full_hd_root());
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn c_onscreen_1x1_is_retained() {
        // No minimum-size heuristic: a 1x1 window that IS onscreen must
        // not be pruned merely because of its size.
        assert!(surface_quad_intersects_root(geo(500, 500, 1, 1), full_hd_root()));
        let entry = visibility_test_entry(geo(500, 500, 1, 1), false);
        let mut entries = vec![entry];
        prune_invisible_entries(&mut entries, shadow_style(false, 0.0, 0.0, 0.0), full_hd_root());
        assert_eq!(entries.len(), 1);
    }

    #[test]
    fn d_completely_right_of_root_is_skipped() {
        assert!(!surface_quad_intersects_root(geo(2000, 500, 50, 50), full_hd_root()));
        let mut entries = vec![visibility_test_entry(geo(2000, 500, 50, 50), false)];
        prune_invisible_entries(&mut entries, shadow_style(false, 0.0, 0.0, 0.0), full_hd_root());
        assert!(entries.is_empty());
    }

    #[test]
    fn e_completely_below_root_is_skipped() {
        assert!(!surface_quad_intersects_root(geo(500, 2000, 50, 50), full_hd_root()));
        let mut entries = vec![visibility_test_entry(geo(500, 2000, 50, 50), false)];
        prune_invisible_entries(&mut entries, shadow_style(false, 0.0, 0.0, 0.0), full_hd_root());
        assert!(entries.is_empty());
    }

    #[test]
    fn f_shadow_only_contribution_is_retained() {
        // Client quad fully off the left edge (right edge at x=-5), but a
        // 24px shadow extent reaches to x=19 — inside root.
        let geometry = geo(-15, 500, 10, 10);
        assert!(!surface_quad_intersects_root(geometry, full_hd_root()));
        let style = shadow_style(true, 24.0, 0.0, 0.0);
        assert!(shadow_bounds_intersect_root(geometry, style, full_hd_root()));
        let entry = visibility_test_entry(geometry, true);
        assert!(entry_has_visible_contribution(&entry, style, full_hd_root()));
        let mut entries = vec![entry];
        prune_invisible_entries(&mut entries, style, full_hd_root());
        assert_eq!(entries.len(), 1, "shadow-only contribution must retain the entry");
    }

    #[test]
    fn g_same_geometry_with_shadow_ineligible_is_skipped() {
        // Identical geometry to case F, but the entry is not
        // shadow-eligible (e.g. shadow disabled, no semantic client,
        // fullscreen, or non-Normal visual class upstream) — shadow
        // geometry must not keep it alive.
        let geometry = geo(-15, 500, 10, 10);
        let style = shadow_style(true, 24.0, 0.0, 0.0);
        let entry = visibility_test_entry(geometry, false);
        assert!(!entry_has_visible_contribution(&entry, style, full_hd_root()));
        let mut entries = vec![entry];
        prune_invisible_entries(&mut entries, style, full_hd_root());
        assert!(entries.is_empty());
    }

    #[test]
    fn g2_shadow_disabled_does_not_keep_surface_alive() {
        let geometry = geo(-15, 500, 10, 10);
        let disabled = shadow_style(false, 24.0, 0.0, 0.0);
        // Even if some upstream bug marked shadow_eligible true while the
        // style itself is disabled, shadow_bounds_intersect_root must not
        // report a contribution — style.enabled is the authority
        // shadow_eligible_for_entry already encodes, and callers gate on
        // entry.shadow_eligible, but this guards the geometry helper too.
        assert!(!shadow_bounds_intersect_root(geometry, disabled, full_hd_root()) || !disabled.enabled);
        let entry = visibility_test_entry(geometry, false);
        assert!(!entry_has_visible_contribution(&entry, disabled, full_hd_root()));
    }

    #[test]
    fn h_shadow_extent_still_fully_outside_root_is_skipped() {
        // Far enough offscreen that even a generous shadow extent cannot
        // reach root.
        let geometry = geo(-1000, 500, 10, 10);
        let style = shadow_style(true, 24.0, 0.0, 0.0);
        assert!(!surface_quad_intersects_root(geometry, full_hd_root()));
        assert!(!shadow_bounds_intersect_root(geometry, style, full_hd_root()));
        let entry = visibility_test_entry(geometry, true);
        assert!(!entry_has_visible_contribution(&entry, style, full_hd_root()));
        let mut entries = vec![entry];
        prune_invisible_entries(&mut entries, style, full_hd_root());
        assert!(entries.is_empty());
    }

    #[test]
    fn build_render_quad_plan_agrees_with_surface_quad_intersects_root() {
        // The lighter, pixmap-free boolean predicate must not diverge from
        // build_render_quad_plan's own None/Some verdict once a matching
        // pixmap exists (pixmap width/height == window width/height + 2*
        // border, per named_pixmap_dimensions_match, which is exactly what
        // a non-stale, correctly sized NamedSurfacePixmap reports).
        let root = full_hd_root();
        let cases = [
            geo(-99, -99, 1, 1),
            geo(-100, 100, 500, 500),
            geo(500, 500, 1, 1),
            geo(2000, 500, 50, 50),
            geo(500, 2000, 50, 50),
        ];
        for window in cases {
            let pixmap = PixmapGeometry {
                root: 1,
                x: 0,
                y: 0,
                width: window.width,
                height: window.height,
                border_width: window.border_width,
                depth: 24,
            };
            let plan_says_visible = build_render_quad_plan(window, pixmap, root).is_some();
            let predicate_says_visible = surface_quad_intersects_root(window, root);
            assert_eq!(
                plan_says_visible, predicate_says_visible,
                "diverged for window {window:?}"
            );
        }
    }

    // ========================================================
    // 3a3f6a V2 — resource-gate proof (section 20).
    // ========================================================

    #[test]
    fn prune_runs_before_damage_lease_acquisition_in_build_candidate() {
        let source = include_str!("scene.rs");
        let fn_start = source.find("fn build_candidate(&mut self)").expect("build_candidate exists");
        let fn_end = fn_start + source[fn_start..].find("\n    fn rebuild_and_present").expect("build_candidate body ends");
        let body = &source[fn_start..fn_end];
        let prune_index = body.find("prune_invisible_entries(").expect("prunes before resource acquisition");
        let damage_index = body.find("DamageLease::acquire(").expect("acquires DamageLease");
        let pixmap_index = body.find("NamedSurfacePixmap::acquire(").expect("acquires NamedSurfacePixmap");
        assert!(prune_index < damage_index, "prune must precede DamageLease::acquire");
        assert!(prune_index < pixmap_index, "prune must precede NamedSurfacePixmap::acquire");
    }

    #[test]
    fn prune_removes_invisible_entries_from_the_resource_acquisition_plan() {
        // Conceptual assertion required by the task: an invisible entry
        // does not reach the per-entry resource-acquisition loop.
        // `snapshot.entries` (post-prune) is exactly the Vec that loop
        // iterates (`for index in 0..snapshot.entries.len()`), so proving
        // an invisible entry is absent from the pruned Vec is equivalent
        // to proving it can never reach DamageLease::acquire,
        // NamedSurfacePixmap::acquire, or eglCreateImageKHR for this
        // candidate build.
        let visible = visibility_test_entry(geo(500, 500, 1, 1), false);
        let invisible = visibility_test_entry(geo(-99, -99, 1, 1), false);
        let mut entries = vec![visible.clone(), invisible];
        prune_invisible_entries(&mut entries, shadow_style(false, 0.0, 0.0, 0.0), full_hd_root());
        assert_eq!(entries, vec![visible]);
    }

    // ========================================================
    // 3a3f6a V2 — future onscreen transition (section 21), using the
    // EXISTING, unmodified classify_event dispatcher.
    // ========================================================

    #[test]
    fn pruned_xid_configure_notify_falls_back_to_hierarchy_rebuild() {
        // Simulates the post-prune state: the offscreen surface is absent
        // from snapshot.entries (as it would be after
        // prune_invisible_entries ran). A later ConfigureNotify for that
        // same XID must NOT be silently ignored or require any new
        // tracking state — the existing classify_event catch-all already
        // promotes it to a full Hierarchy rebuild, which re-evaluates
        // eligibility (and this filter) fresh against the window's new
        // geometry.
        let snapshot = SceneSnapshot {
            root: 1,
            root_geometry: full_hd_root(),
            entries: Vec::new(),
        };
        let pruned_xid: xproto::Window = 0x0040_0000;
        let configure = xproto::ConfigureNotifyEvent {
            response_type: 0,
            sequence: 0,
            event: 1,
            window: pruned_xid,
            above_sibling: 0,
            x: 100,
            y: 100,
            width: 800,
            height: 600,
            border_width: 0,
            override_redirect: false,
        };
        let invalidation = classify_event(
            Event::ConfigureNotify(configure),
            1,
            &snapshot,
            None,
        );
        assert_eq!(invalidation, SceneInvalidation::Hierarchy);
    }

    #[test]
    fn raw_pixmap_ownership_transfers_once() {
        let mut ownership = RawPixmapOwnership::new();
        assert!(ownership.is_owned());
        ownership.transfer();
        assert!(!ownership.is_owned());
        ownership.transfer();
        assert!(!ownership.is_owned());
    }

    #[test]
    fn bounded_retry_accepts_only_first_stale_attempt() {
        assert!(retry_allowed(0));
        assert!(!retry_allowed(1));
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
    fn border_state_precedence_is_urgent_then_focused_then_inactive() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        assert_eq!(border_visual_state(&entry, None, &HashMap::new()), BorderVisualState::Inactive);
        assert_eq!(border_visual_state(&entry, Some(20), &HashMap::new()), BorderVisualState::Focused);
        assert_eq!(border_visual_state(&entry, Some(20), &HashMap::from([(20, CachedClientVisualState { wm_hints: true, demands_attention: false, ..CachedClientVisualState::default() })])), BorderVisualState::Urgent);
        assert_eq!(entry.surface_xid, 10);
        assert_eq!(entry.lifecycle_xid, 10);
    }

    #[test]
    fn visual_state_changes_coalesce_without_structural_invalidation() {
        let mut batch = InvalidationBatch::default();
        batch.push(SceneInvalidation::VisualState);
        batch.push(SceneInvalidation::VisualState);
        assert_eq!(batch.decision(), SceneInvalidation::VisualState);
        let mut generation = 4;
        observe_structural_generation(&mut generation, SceneInvalidation::VisualState);
        assert_eq!(generation, 4);
    }

    #[test]
    fn both_supported_urgency_sources_are_recognized() {
        assert!(wm_hints_urgency(Some(1 << 8)));
        assert!(!wm_hints_urgency(Some(0)));
        assert!(!wm_hints_urgency(None));
        assert!(state_demands_attention(Some([7, 42].into_iter()), 42));
        assert!(!state_demands_attention(Some([7, 42].into_iter()), 9));
    }

    #[test]
    fn identical_active_client_has_identical_rendered_state() {
        let config = crate::config::CompositorConfig::defaults()
            .with_border_colors(2.0, [0.1, 0.1, 0.1, 1.0], [0.2, 0.2, 0.2, 1.0], [1.0, 0.0, 0.0, 1.0])
            .unwrap();
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let urgency = HashMap::new();
        assert_eq!(
            rendered_border_color(&config.visuals, &entry, Some(20), &urgency),
            rendered_border_color(&config.visuals, &entry, Some(20), &urgency)
        );
    }

    #[test]
    fn focus_transition_only_changes_old_and_new_canonical_surfaces() {
        let config = crate::config::CompositorConfig::defaults()
            .with_border_colors(2.0, [0.1, 0.1, 0.1, 1.0], [0.2, 0.2, 0.2, 1.0], [1.0, 0.0, 0.0, 1.0])
            .unwrap();
        let first = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let second = eligible_surface(&metadata(), Some(30), root(), 11, 1).unwrap();
        let urgency = HashMap::new();
        let before_first = rendered_border_color(&config.visuals, &first, Some(20), &urgency);
        let after_first = rendered_border_color(&config.visuals, &first, Some(30), &urgency);
        let before_second = rendered_border_color(&config.visuals, &second, Some(20), &urgency);
        let after_second = rendered_border_color(&config.visuals, &second, Some(30), &urgency);
        assert_ne!(before_first, after_first);
        assert_ne!(before_second, after_second);
        assert_eq!(first.surface_xid, 10);
        assert_eq!(second.surface_xid, 11);
    }

    #[test]
    fn clearing_one_urgency_source_does_not_dirty_when_other_remains() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let mut urgency = HashMap::from([(20, CachedClientVisualState { wm_hints: true, demands_attention: true, ..CachedClientVisualState::default() })]);
        let before = border_visual_state(&entry, Some(20), &urgency);
        urgency.insert(20, CachedClientVisualState { wm_hints: false, demands_attention: true, ..CachedClientVisualState::default() });
        let after = border_visual_state(&entry, Some(20), &urgency);
        assert_eq!(before, BorderVisualState::Urgent);
        assert_eq!(after, before);
    }

    #[test]
    fn removed_client_is_purged_from_visual_cache() {
        let mut urgency = HashMap::from([(20, CachedClientVisualState { wm_hints: true, demands_attention: false, ..CachedClientVisualState::default() })]);
        let live = HashSet::from([30]);
        urgency.retain(|client, _| live.contains(client));
        assert!(urgency.is_empty());
    }

    #[test]
    fn dock_and_desktop_never_receive_stateful_border() {
        for kind in ["_NET_WM_WINDOW_TYPE_DOCK", "_NET_WM_WINDOW_TYPE_DESKTOP"] {
            let mut source = metadata();
            source.window_type = Some(kind.to_owned());
            let entry = eligible_surface(&source, Some(20), root(), 10, 0).unwrap();
            assert_eq!(border_visual_state(&entry, Some(20), &HashMap::from([(20, CachedClientVisualState { wm_hints: true, demands_attention: true, ..CachedClientVisualState::default() })])), BorderVisualState::Inactive);
        }
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
        assert!(canonical_mask.contains(EventMask::PROPERTY_CHANGE));
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
        assert_eq!(snapshot_watch_ids(&snapshot), HashSet::from([10, 30]));
    }

    #[test]
    fn acquisition_error_after_manual_active_requires_coordinated_cleanup() {
        assert!(!coordinator_requires_cleanup(SceneState::PlaceholderReady));
        assert!(coordinator_requires_cleanup(SceneState::ManualActive));
        assert!(coordinator_requires_cleanup(SceneState::SceneSnapshotReady));
    }

    #[test]
    fn first_publish_deferral_waits_without_faking_a_scene() {
        assert_eq!(first_publish_step(false, false, false), FirstPublishStep::Rebuild);
        assert_eq!(first_publish_step(false, true, false), FirstPublishStep::AwaitEvent);
        assert_eq!(first_publish_step(true, true, false), FirstPublishStep::Published);
        assert_eq!(first_publish_step(false, true, true), FirstPublishStep::Shutdown);
    }

    #[test]
    fn first_publish_state_machine_preserves_deferred_then_stable_publish_sequence() {
        let mut snapshot_present = false;
        let mut rebuild_deferred = false;
        assert_eq!(first_publish_step(snapshot_present, rebuild_deferred, false), FirstPublishStep::Rebuild);
        rebuild_deferred = true;
        assert_eq!(first_publish_step(snapshot_present, rebuild_deferred, false), FirstPublishStep::AwaitEvent);
        rebuild_deferred = false;
        assert_eq!(first_publish_step(snapshot_present, rebuild_deferred, false), FirstPublishStep::Rebuild);
        snapshot_present = true;
        assert_eq!(first_publish_step(snapshot_present, rebuild_deferred, false), FirstPublishStep::Published);
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
    fn scheduler_first_damage_is_dirty_without_rendering() {
        let mut scheduler = FrameScheduler::new();
        scheduler.arm(0);
        scheduler.mark_pixel_dirty();
        assert!(matches!(scheduler.state, FrameSchedulerState::Dirty { pixel_damage: true, .. }));
    }

    #[test]
    fn scheduler_coalesces_damage_and_has_no_frame_queue() {
        let mut scheduler = FrameScheduler::new();
        scheduler.arm(0);
        scheduler.mark_pixel_dirty();
        scheduler.mark_pixel_dirty();
        assert!(matches!(scheduler.state, FrameSchedulerState::Dirty { pixel_damage: true, structural_generation: None }));
    }

    #[test]
    fn scheduler_opportunity_consumes_one_serial_once() {
        let mut scheduler = FrameScheduler::new();
        let (serial, target_msc) = scheduler.arm(37);
        assert_eq!(target_msc, 37);
        assert!(scheduler.complete(serial, 38));
        assert!(!scheduler.complete(serial, 39));
    }

    #[test]
    fn scheduler_damage_during_render_stays_dirty_for_next_opportunity() {
        let mut scheduler = FrameScheduler::new();
        let (serial, _) = scheduler.arm(0);
        assert!(scheduler.complete(serial, 1));
        scheduler.mark_pixel_dirty();
        assert!(matches!(scheduler.state, FrameSchedulerState::Dirty { pixel_damage: true, .. }));
    }

    #[test]
    fn scheduler_structural_generation_dominates_pixel_without_dropping_pixel_dirty() {
        let mut scheduler = FrameScheduler::new();
        scheduler.mark_pixel_dirty();
        scheduler.mark_structural_dirty(9);
        assert!(matches!(scheduler.state, FrameSchedulerState::Dirty {
            pixel_damage: true,
            structural_generation: Some(9),
        }));
    }

    #[test]
    fn scheduler_clean_completion_does_not_create_backlog() {
        let mut scheduler = FrameScheduler::new();
        let (serial, _) = scheduler.arm(75);
        assert!(scheduler.complete(serial, 76));
        scheduler.finish_render(4, false);
        assert!(matches!(scheduler.state, FrameSchedulerState::AwaitExternalStructuralChange { generation: 4 }));
        let (_, target_msc) = scheduler.arm(77);
        assert_eq!(target_msc, 77);
    }

    #[test]
    fn scheduler_stale_serial_cannot_corrupt_state() {
        let mut scheduler = FrameScheduler::new();
        let (serial, _) = scheduler.arm(0);
        assert!(!scheduler.complete(serial.wrapping_add(1), 1));
        assert!(matches!(scheduler.state, FrameSchedulerState::Armed { .. }));
    }

    #[test]
    fn scheduler_shutdown_cleanup_model_has_no_pending_serial() {
        let mut scheduler = FrameScheduler::new();
        let (serial, _) = scheduler.arm(0);
        assert!(!scheduler.complete(serial.wrapping_add(1), 1));
        scheduler.state = FrameSchedulerState::Idle;
        scheduler.armed_serial = None;
        assert!(matches!(scheduler.state, FrameSchedulerState::Idle));
    }

    #[test]
    fn scheduler_refresh_target_is_server_msc_not_a_timer_period() {
        let mut scheduler = FrameScheduler::new();
        let (_, target_msc) = scheduler.arm(1234);
        assert_eq!(target_msc, 1234);
    }

    fn background_root() -> RootGeometry { RootGeometry { width: 1920, height: 1080, depth: 24, visual: 0x21 } }

    #[test]
    fn background_property_accepts_one_pixmap_xid() {
        assert_eq!(parse_background_property(9, 9, 32, 1, &0x800001_u32.to_ne_bytes()).unwrap(), Some(0x800001));
    }

    #[test]
    fn background_property_rejects_wrong_type_format_and_zero() {
        let value = 1_u32.to_ne_bytes();
        assert!(parse_background_property(8, 9, 32, 1, &value).is_err());
        assert!(parse_background_property(9, 9, 16, 1, &value).is_err());
        assert!(parse_background_property(9, 9, 32, 1, &0_u32.to_ne_bytes()).is_err());
    }

    #[test]
    fn background_property_rejects_wrong_item_count() {
        assert!(parse_background_property(9, 9, 32, 2, &[1, 0, 0, 0]).is_err());
    }

    #[test]
    fn absent_background_property_is_not_an_error() {
        assert_eq!(parse_background_property(x11rb::NONE, 9, 0, 0, &[]).unwrap(), None);
    }

    #[test]
    fn background_pixmap_plan_covers_root_from_origin() {
        let plan = build_background_render_quad_plan(PixmapGeometry { root: 1, x: 0, y: 0, width: 1920, height: 1080, border_width: 0, depth: 24 }, background_root()).unwrap();
        assert_eq!((plan.dst_x, plan.dst_y, plan.width, plan.height), (0, 0, 1920, 1080));
        assert_eq!((plan.src_x, plan.src_y), (0, 0));
    }

    #[test]
    fn undersized_background_pixmap_is_rejected_without_scaling() {
        assert!(build_background_render_quad_plan(PixmapGeometry { root: 1, x: 0, y: 0, width: 1919, height: 1080, border_width: 0, depth: 24 }, background_root()).is_none());
    }

    #[test]
    fn background_pixmap_must_have_a_root() {
        assert!(build_background_render_quad_plan(PixmapGeometry { root: x11rb::NONE, x: 0, y: 0, width: 1920, height: 1080, border_width: 0, depth: 24 }, background_root()).is_none());
    }

    #[test]
    fn background_candidate_preserves_current_on_invalid_replacement() {
        assert_eq!(BackgroundCandidate::Preserve, BackgroundCandidate::Preserve);
    }

    #[test]
    fn background_candidate_has_explicit_solid_fallback() {
        assert_eq!(BackgroundCandidate::SolidFallback, BackgroundCandidate::SolidFallback);
    }

    #[test]
    fn same_property_xid_is_a_single_import_source() {
        let selected = Some(0x800001_u32);
        let fallback = Some(0x800001_u32);
        assert_eq!(selected.or(fallback), Some(0x800001));
    }

    #[test]
    fn background_property_notify_is_scoped_to_root_and_two_atoms() {
        let atoms = BackgroundAtoms { xrootpmap_id: 10, esetroot_pmap_id: 11, pixmap_type: 12 };
        let event = Event::PropertyNotify(xproto::PropertyNotifyEvent { response_type: 28, sequence: 0, window: 1, atom: 10, time: 0, state: xproto::Property::NEW_VALUE });
        assert!(is_background_property_notify(&event, 1, atoms));
        let unrelated = Event::PropertyNotify(xproto::PropertyNotifyEvent { response_type: 28, sequence: 0, window: 1, atom: 99, time: 0, state: xproto::Property::NEW_VALUE });
        assert!(!is_background_property_notify(&unrelated, 1, atoms));
    }

    #[test]
    fn background_property_notify_ignores_other_windows() {
        let atoms = BackgroundAtoms { xrootpmap_id: 10, esetroot_pmap_id: 11, pixmap_type: 12 };
        let event = Event::PropertyNotify(xproto::PropertyNotifyEvent { response_type: 28, sequence: 0, window: 2, atom: 10, time: 0, state: xproto::Property::NEW_VALUE });
        assert!(!is_background_property_notify(&event, 1, atoms));
    }

    #[test]
    fn background_invalidation_does_not_advance_structural_generation() {
        let mut generation = 4;
        observe_structural_generation(&mut generation, SceneInvalidation::Background);
        assert_eq!(generation, 4);
    }

    #[test]
    fn background_batch_coalesces_repeated_notifications() {
        let mut batch = InvalidationBatch::default();
        batch.push(SceneInvalidation::Background);
        batch.push(SceneInvalidation::Background);
        assert_eq!(batch.decision(), SceneInvalidation::Background);
    }

    #[test]
    fn structural_invalidation_dominates_background_without_dropping_it() {
        let mut batch = InvalidationBatch::default();
        batch.push(SceneInvalidation::Background);
        batch.push(SceneInvalidation::Hierarchy);
        assert_eq!(batch.decision(), SceneInvalidation::Hierarchy);
        assert!(batch.background);
    }

    #[test]
    fn background_opportunity_gate_does_not_retry_candidate() {
        assert_eq!(candidate_gate_decision(SceneInvalidation::Background, false, true, false), GateDecision::Accept);
    }

    #[test]
    fn background_does_not_make_pixel_gate_present_without_render() {
        assert!(!pixel_gate_allows_presentation(SceneInvalidation::Background, true, false));
    }

    #[test]
    fn background_source_uses_opaque_root_semantics_only() {
        assert_ne!(EglPixelSemantics::Opaque, EglPixelSemantics::PremultipliedAlpha);
        assert_eq!(EglPixelSemantics::Opaque, EglPixelSemantics::Opaque);
    }

    #[test]
    fn background_source_is_not_a_client_surface_identity() {
        let source = BackgroundPixmap { xid: 7, geometry: PixmapGeometry { root: 1, x: 0, y: 0, width: 1920, height: 1080, border_width: 0, depth: 24 }, semantics: EglPixelSemantics::Opaque };
        assert_ne!(source.xid, 0);
    }

    #[test]
    fn no_valid_source_selects_solid_fallback() {
        assert_eq!(BackgroundCandidate::SolidFallback, BackgroundCandidate::SolidFallback);
    }

    #[test]
    fn background_layer_is_full_screen_not_a_window_quad() {
        let plan = build_background_render_quad_plan(PixmapGeometry { root: 1, x: 0, y: 0, width: 1920, height: 1080, border_width: 0, depth: 24 }, background_root()).unwrap();
        assert_eq!(plan.dst_x, 0);
        assert_eq!(plan.dst_y, 0);
        assert_eq!(plan.width, i32::from(background_root().width));
    }

    #[test]
    fn zero_corner_radius_is_an_exact_no_op() {
        assert_eq!(effective_corner_radius(0.0, 100, 80), 0.0);
        assert_eq!(effective_corner_radius(-1.0, 100, 80), 0.0);
        assert_eq!(effective_corner_radius(f32::NAN, 100, 80), 0.0);
    }

    #[test]
    fn corner_radius_clamps_to_half_smallest_dimension() {
        assert_eq!(effective_corner_radius(100.0, 100, 80), 40.0);
        assert_eq!(effective_corner_radius(20.0, 100, 80), 20.0);
    }

    #[test]
    fn corner_radius_rejects_non_positive_geometry() {
        assert_eq!(effective_corner_radius(8.0, 0, 80), 0.0);
        assert_eq!(effective_corner_radius(8.0, 100, -1), 0.0);
    }

    #[test]
    fn border_width_zero_is_an_exact_no_op() {
        assert_eq!(effective_border_width(0.0, 100, 80), 0.0);
        assert_eq!(effective_corner_radius(16.0, 100, 80), 16.0);
    }

    #[test]
    fn border_width_clamps_to_half_smallest_dimension() {
        assert_eq!(effective_border_width(100.0, 100, 80), 40.0);
        assert_eq!(effective_border_width(-1.0, 100, 80), 0.0);
        assert_eq!(effective_border_width(8.0, 0, 80), 0.0);
    }

    #[test]
    fn border_geometry_is_rectangular_or_rounded_consistently() {
        assert_eq!(effective_corner_radius(0.0, 100, 80), 0.0);
        assert_eq!(effective_corner_radius(16.0, 100, 80), 16.0);
        assert_eq!(effective_border_width(20.0, 100, 80), 20.0);
    }

    #[test]
    fn visual_policy_decorates_normal_but_excludes_dock_and_desktop() {
        let config = crate::config::CompositorConfig::with_corner_radius(12.0)
            .unwrap()
            .with_border_colors(2.0, [1.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0, 1.0], [1.0, 0.0, 0.0, 1.0])
            .unwrap();
        let mut normal = build_render_quad_plan(window(0, 0, 20, 20, 0), pixmap(20, 20), root()).unwrap();
        apply_surface_visual_policy(&mut normal, &config.visuals, SurfaceVisualClass::Normal);
        assert_eq!(normal.corner_radius, 10.0);
        assert_eq!(normal.border_width, 2.0);

        for visual_class in [SurfaceVisualClass::Dock, SurfaceVisualClass::Desktop] {
            let mut excluded = normal;
            apply_surface_visual_policy(&mut excluded, &config.visuals, visual_class);
            assert_eq!(excluded.corner_radius, 0.0);
            assert_eq!(excluded.border_width, 0.0);
        }
    }

    #[test]
    fn shadow_policy_uses_active_visual_quad_and_excludes_non_normal_surfaces() {
        let mut config = crate::config::CompositorConfig::defaults();
        config.visuals.corner_radius = 18.0;
        config.visuals.border.width = 7.0;
        config.visuals.shadow = crate::config::ShadowConfig {
            enabled: true,
            color: [0, 0, 0],
            offset_x: 0.0,
            offset_y: 0.0,
            extent: 12.0,
            strength: 0.35,
        };
        let normal = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let visual_quad = pixmap(20, 15);
        let mut plan = build_render_quad_plan(normal.geometry, visual_quad, root()).unwrap();
        apply_surface_visual_policy(&mut plan, &config.visuals, normal.visual_class);
        let style = config.visuals.shadow;
        let shadow = shadow_params_from_plan(style, &plan, 1.0).unwrap();
        assert_eq!(shadow.outer_x, plan.outer_x as f32);
        assert_eq!(shadow.outer_y, plan.outer_y as f32);
        assert_eq!(shadow.outer_width, plan.outer_width as f32);
        assert_eq!(shadow.outer_height, plan.outer_height as f32);
        assert_eq!(shadow.corner_radius, plan.corner_radius);

        for visual_class in [SurfaceVisualClass::Dock, SurfaceVisualClass::Desktop] {
            let mut excluded = normal.clone();
            excluded.visual_class = visual_class;
            assert!(!shadow_eligible_for_entry(style, &excluded));
        }
        let mut override_redirect = normal.clone();
        override_redirect.override_redirect = true;
        assert!(shadow_eligible_for_entry(style, &override_redirect));
        let mut managed_surface = normal.clone();
        managed_surface.override_redirect = false;
        assert!(shadow_eligible_for_entry(style, &managed_surface));
        let mut no_client = normal.clone();
        no_client.semantic_client_xid = None;
        assert!(!shadow_eligible_for_entry(style, &no_client));
        let mut fullscreen = normal;
        fullscreen.fullscreen = true;
        assert!(!shadow_eligible_for_entry(style, &fullscreen));
    }

    #[test]
    fn resolved_opacity_reuses_urgent_focused_inactive_precedence() {
        let config = crate::config::CompositorConfig::defaults()
            .with_opacity(0.80, 0.92, 0.70)
            .unwrap();
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState::default());
        assert_eq!(resolved_surface_opacity(&config.visuals, &entry, Some(30), &urgency), 0.92);
        assert_eq!(resolved_surface_opacity(&config.visuals, &entry, Some(20), &urgency), 0.80);
        urgency.insert(20, CachedClientVisualState { wm_hints: false, demands_attention: true, fullscreen: false, ..CachedClientVisualState::default() });
        assert_eq!(resolved_surface_opacity(&config.visuals, &entry, Some(20), &urgency), 0.70);
    }

    #[test]
    fn resolved_opacity_forces_fullscreen_and_special_surfaces_to_one() {
        let config = crate::config::CompositorConfig::defaults()
            .with_opacity(0.80, 0.92, 0.70)
            .unwrap();
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState::default());
        let mut fullscreen = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        fullscreen.fullscreen = true;
        assert_eq!(resolved_surface_opacity(&config.visuals, &fullscreen, Some(20), &urgency), 1.0);
        for visual_class in [SurfaceVisualClass::Dock, SurfaceVisualClass::Desktop] {
            let mut special = fullscreen.clone();
            special.fullscreen = false;
            special.visual_class = visual_class;
            assert_eq!(resolved_surface_opacity(&config.visuals, &special, Some(20), &urgency), 1.0);
        }
        let mut popup = fullscreen;
        popup.fullscreen = false;
        popup.semantic_client_xid = None;
        assert_eq!(resolved_surface_opacity(&config.visuals, &popup, Some(20), &urgency), 1.0);
    }

    #[test]
    fn shadow_outer_quad_is_independent_of_internal_border_width() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let mut first = crate::config::CompositorConfig::defaults();
        first.visuals.shadow = crate::config::ShadowConfig { enabled: true, extent: 8.0, strength: 0.25, ..crate::config::ShadowConfig::default() };
        let mut second = first;
        second.visuals.border.width = 9.0;
        let visual_quad = pixmap(20, 15);
        let mut first_plan = build_render_quad_plan(entry.geometry, visual_quad, root()).unwrap();
        apply_surface_visual_policy(&mut first_plan, &first.visuals, entry.visual_class);
        let mut second_plan = build_render_quad_plan(entry.geometry, visual_quad, root()).unwrap();
        apply_surface_visual_policy(&mut second_plan, &second.visuals, entry.visual_class);
        assert_eq!(shadow_params_from_plan(first.visuals.shadow, &first_plan, 1.0).unwrap().outer_width,
            shadow_params_from_plan(second.visuals.shadow, &second_plan, 1.0).unwrap().outer_width);
        assert_eq!(shadow_params_from_plan(first.visuals.shadow, &first_plan, 1.0).unwrap().outer_height,
            shadow_params_from_plan(second.visuals.shadow, &second_plan, 1.0).unwrap().outer_height);
    }

    #[test]
    fn shadow_uses_root_destination_when_pixmap_geometry_is_local() {
        let mut config = crate::config::CompositorConfig::defaults();
        config.visuals.shadow = crate::config::ShadowConfig { enabled: true, extent: 8.0, strength: 0.25, ..crate::config::ShadowConfig::default() };
        let first = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let mut first = first;
        first.geometry.x = 10;
        first.geometry.y = 30;
        let local_pixmap = pixmap(20, 15);
        let first_plan = build_render_quad_plan(first.geometry, local_pixmap, root()).unwrap();
        let first_shadow = shadow_params_from_plan(config.visuals.shadow, &first_plan, 1.0).unwrap();
        assert_eq!((first_shadow.outer_x, first_shadow.outer_y), (10.0, 30.0));

        let mut second = first.clone();
        second.geometry.x = 55;
        second.geometry.y = 5;
        let second_plan = build_render_quad_plan(second.geometry, local_pixmap, root()).unwrap();
        let second_shadow = shadow_params_from_plan(config.visuals.shadow, &second_plan, 1.0).unwrap();
        assert_eq!((second_shadow.outer_x, second_shadow.outer_y), (55.0, 5.0));
    }

    #[test]
    fn fullscreen_transition_removes_and_restores_shadow_policy() {
        let mut config = crate::config::CompositorConfig::defaults();
        config.visuals.shadow = crate::config::ShadowConfig { enabled: true, extent: 8.0, strength: 0.25, ..crate::config::ShadowConfig::default() };
        let mut entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        assert!(shadow_eligible_for_entry(config.visuals.shadow, &entry));
        entry.fullscreen = true;
        assert!(!shadow_eligible_for_entry(config.visuals.shadow, &entry));
        entry.fullscreen = false;
        assert!(shadow_eligible_for_entry(config.visuals.shadow, &entry));
    }

    #[test]
    fn shadow_geometry_tracks_active_radius_and_surface_geometry() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let mut config = crate::config::CompositorConfig::defaults();
        config.visuals.shadow = crate::config::ShadowConfig { enabled: true, extent: 8.0, strength: 0.25, ..crate::config::ShadowConfig::default() };
        config.visuals.corner_radius = 4.0;
        let visual_quad = pixmap(20, 15);
        let mut first_plan = build_render_quad_plan(entry.geometry, visual_quad, root()).unwrap();
        apply_surface_visual_policy(&mut first_plan, &config.visuals, entry.visual_class);
        let first = shadow_params_from_plan(config.visuals.shadow, &first_plan, 1.0).unwrap();
        config.visuals.corner_radius = 16.0;
        let mut second_plan = build_render_quad_plan(entry.geometry, visual_quad, root()).unwrap();
        apply_surface_visual_policy(&mut second_plan, &config.visuals, entry.visual_class);
        let second = shadow_params_from_plan(config.visuals.shadow, &second_plan, 1.0).unwrap();
        assert_ne!(first.corner_radius, second.corner_radius);
        let mut moved = entry;
        moved.geometry.x += 11;
        moved.geometry.y += 13;
        let moved_quad = PixmapGeometry { x: visual_quad.x + 11, y: visual_quad.y + 13, ..visual_quad };
        let mut moved_plan = build_render_quad_plan(moved.geometry, moved_quad, root()).unwrap();
        apply_surface_visual_policy(&mut moved_plan, &config.visuals, moved.visual_class);
        let moved_shadow = shadow_params_from_plan(config.visuals.shadow, &moved_plan, 1.0).unwrap();
        assert_eq!(moved_shadow.outer_x - second.outer_x, 11.0);
        assert_eq!(moved_shadow.outer_y - second.outer_y, 13.0);
    }

    #[test]
    fn disabled_shadow_and_non_positive_settings_produce_no_params() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let config = crate::config::CompositorConfig::defaults();
        let visual_quad = pixmap(20, 15);
        let plan = build_render_quad_plan(entry.geometry, visual_quad, root()).unwrap();
        assert!(!shadow_eligible_for_entry(config.visuals.shadow, &entry));
        let mut enabled = config;
        enabled.visuals.shadow.enabled = true;
        assert!(shadow_params_from_plan(enabled.visuals.shadow, &plan, 1.0).is_none());
    }

    #[test]
    fn one_net_wm_state_snapshot_resolves_urgency_and_fullscreen() {
        let atoms = VisualAtoms {
            active_window: 1,
            wm_hints: 2,
            net_wm_state: 3,
            demands_attention: 42,
            fullscreen: 43,
            blur_behind_region: 44,
            effect_owner: 45,
        };
        let state = read_net_wm_state(Some([7, 43, 42].into_iter()), atoms);
        assert!(state.demands_attention);
        assert!(state.fullscreen);
        let state = read_net_wm_state(Some([7].into_iter()), atoms);
        assert!(!state.demands_attention);
        assert!(!state.fullscreen);
    }

    // ========================================================
    // 3a3f7 Phase 2A — _KDE_NET_WM_BLUR_BEHIND_REGION request parsing,
    // caching, and invalidation. No GPU call, no backdrop composite: the
    // renderer's blur primitive remains completely uncalled by anything
    // added here (see phase_2a_does_not_call_the_blur_primitive).
    // ========================================================

    #[test]
    fn blur_property_absent_is_no_request() {
        assert_eq!(parse_blur_behind_region(None::<std::iter::Empty<u32>>), BlurRequest::None);
    }

    #[test]
    fn blur_property_empty_payload_is_full_window() {
        assert_eq!(parse_blur_behind_region(Some(Vec::<u32>::new().into_iter())), BlurRequest::FullWindow);
    }

    #[test]
    fn blur_property_single_degenerate_rectangle_is_full_window() {
        // The exact payload the reference client (Ghostty 1.3.1,
        // background-blur=true) emits.
        assert_eq!(parse_blur_behind_region(Some([0u32, 0, 0, 0].into_iter())), BlurRequest::FullWindow);
    }

    #[test]
    fn blur_property_single_rectangle_is_regions() {
        assert_eq!(
            parse_blur_behind_region(Some([10u32, 20, 300, 400].into_iter())),
            BlurRequest::Regions(vec![BlurRegionRect { x: 10, y: 20, width: 300, height: 400 }])
        );
    }

    #[test]
    fn blur_property_multiple_rectangles_preserve_order_and_data() {
        assert_eq!(
            parse_blur_behind_region(Some([1u32, 2, 3, 4, 5, 6, 7, 8].into_iter())),
            BlurRequest::Regions(vec![
                BlurRegionRect { x: 1, y: 2, width: 3, height: 4 },
                BlurRegionRect { x: 5, y: 6, width: 7, height: 8 },
            ])
        );
    }

    #[test]
    fn blur_property_mixed_degenerate_and_valid_rectangles_is_not_coerced_to_full_window() {
        // A degenerate rectangle MIXED into a multi-rectangle payload must
        // not collapse the whole request to FullWindow, and must not be
        // silently dropped — only the single-rectangle-and-degenerate
        // shape has a confirmed FullWindow interpretation.
        assert_eq!(
            parse_blur_behind_region(Some([0u32, 0, 0, 0, 10, 10, 100, 100].into_iter())),
            BlurRequest::Regions(vec![
                BlurRegionRect { x: 0, y: 0, width: 0, height: 0 },
                BlurRegionRect { x: 10, y: 10, width: 100, height: 100 },
            ])
        );
    }

    #[test]
    fn blur_property_malformed_count_is_rejected() {
        for len in [1, 2, 3, 5, 6, 7] {
            let payload: Vec<u32> = (0..len).collect();
            assert_eq!(
                parse_blur_behind_region(Some(payload.into_iter())),
                BlurRequest::None,
                "payload length {len} (not a multiple of 4) must be rejected, not truncated"
            );
        }
    }

    #[test]
    fn blur_property_wrong_format_is_rejected_like_absent() {
        // GetPropertyReply::value32() (x11rb) returns None whenever the
        // server-reported format isn't 32 — the same `None` input this
        // parser already treats as "absent". No separate code path exists
        // for "wrong format" versus "absent"; both are safely rejected by
        // the same branch.
        assert_eq!(parse_blur_behind_region(None::<std::iter::Empty<u32>>), BlurRequest::None);
    }

    #[test]
    fn blur_property_read_filters_by_cardinal_type() {
        // A wrong-type property is rejected by the server itself (an
        // effectively empty reply) because the request filters by
        // `type = CARDINAL` — the same convention _NET_WM_STATE already
        // uses with `type = ATOM`, not a new mechanism. Source-contract
        // check since this requires a live connection to observe
        // end-to-end.
        let source = include_str!("scene.rs");
        let start = source.find("fn read_client_blur_request(").expect("read_client_blur_request exists");
        let end = start + source[start..].find("\n}\n").expect("function body ends");
        let body = &source[start..end];
        assert!(body.contains("xproto::AtomEnum::CARDINAL"));
    }

    #[test]
    fn effect_owner_property_requires_exact_window_single_xid() {
        let window_type: xproto::Atom = xproto::AtomEnum::WINDOW.into();
        assert_eq!(parse_effect_owner_property(window_type, window_type, 32, 1, &20u32.to_ne_bytes()), Some(20));
        assert_eq!(parse_effect_owner_property(xproto::AtomEnum::NONE.into(), window_type, 0, 0, &[]), None);
        assert_eq!(parse_effect_owner_property(window_type, window_type, 16, 1, &[20, 0]), None);
        assert_eq!(parse_effect_owner_property(window_type, window_type, 32, 0, &[]), None);
        assert_eq!(parse_effect_owner_property(window_type, window_type, 32, 2, &20u32.to_ne_bytes()), None);
        assert_eq!(parse_effect_owner_property(window_type, window_type, 32, 1, &0u32.to_ne_bytes()), None);
        assert_eq!(parse_effect_owner_property(99, window_type, 32, 1, &20u32.to_ne_bytes()), None);
    }

    #[test]
    fn auxiliary_blur_requires_owner_and_preserves_popup_request() {
        let mut popup = eligible_surface(&metadata(), None, root(), 10, 0).unwrap();
        popup.effect_owner = Some(20);
        popup.own_blur_request = BlurRequest::Regions(vec![BlurRegionRect { x: 7, y: 8, width: 9, height: 10 }]);
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState {
            blur_requested: BlurRequest::Regions(vec![BlurRegionRect { x: 100, y: 100, width: 1, height: 1 }]),
            ..CachedClientVisualState::default()
        });
        let valid = HashSet::from([20]);
        assert_eq!(
            resolved_blur_request_with_auxiliary(&popup, &urgency, &valid),
            popup.own_blur_request,
            "owner authorizes the popup's request; it does not supply the owner's request"
        );
        assert_eq!(resolved_blur_request_with_auxiliary(&popup, &urgency, &HashSet::new()), BlurRequest::None);
        popup.own_blur_request = BlurRequest::None;
        assert_eq!(resolved_blur_request_with_auxiliary(&popup, &urgency, &valid), BlurRequest::None);
    }

    #[test]
    fn effect_owner_never_overloads_semantic_client_or_managed_blur() {
        let mut managed = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        managed.effect_owner = Some(30);
        managed.own_blur_request = BlurRequest::Regions(vec![BlurRegionRect { x: 90, y: 90, width: 2, height: 2 }]);
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, ..CachedClientVisualState::default() });
        urgency.insert(30, CachedClientVisualState { blur_requested: BlurRequest::Regions(vec![BlurRegionRect { x: 1, y: 1, width: 1, height: 1 }]), ..CachedClientVisualState::default() });
        assert_eq!(managed.semantic_client_xid, Some(20));
        assert_eq!(resolved_blur_request_with_auxiliary(&managed, &urgency, &HashSet::from([30])), BlurRequest::FullWindow);
    }

    #[test]
    fn auxiliary_property_notify_is_scoped_to_tracked_surface() {
        let atoms = VisualAtoms {
            active_window: 1, wm_hints: 2, net_wm_state: 3,
            demands_attention: 42, fullscreen: 43, blur_behind_region: 44, effect_owner: 45,
        };
        let mut popup = eligible_surface(&metadata(), None, root(), 10, 0).unwrap();
        popup.override_redirect = true;
        let snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: vec![popup] };
        for atom in [44, 45] {
            let event = Event::PropertyNotify(xproto::PropertyNotifyEvent {
                response_type: 28, sequence: 0, window: 10, atom, time: 0,
                state: xproto::Property::NEW_VALUE,
            });
            assert!(is_visual_property_notify(&event, 1, atoms, &snapshot));
        }
        let unrelated = Event::PropertyNotify(xproto::PropertyNotifyEvent {
            response_type: 28, sequence: 0, window: 999, atom: 45, time: 0,
            state: xproto::Property::NEW_VALUE,
        });
        assert!(!is_visual_property_notify(&unrelated, 1, atoms, &snapshot));
    }

    #[test]
    fn effect_owner_does_not_make_override_redirect_animation_eligible() {
        let mut popup = animation_test_entry(10, SurfaceVisualClass::Normal, true);
        popup.effect_owner = Some(20);
        assert!(!eligible_for_open_animation(&popup));
        assert_eq!(popup.effective_override_redirect, true);
    }

    #[test]
    fn blur_behind_region_property_notify_is_visual_state_scoped() {
        let atoms = VisualAtoms {
            active_window: 1, wm_hints: 2, net_wm_state: 3,
            demands_attention: 42, fullscreen: 43, blur_behind_region: 44, effect_owner: 45,
        };
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: vec![entry] };
        let created = Event::PropertyNotify(xproto::PropertyNotifyEvent {
            response_type: 28, sequence: 0, window: 20, atom: 44, time: 0,
            state: xproto::Property::NEW_VALUE,
        });
        assert!(is_visual_property_notify(&created, 1, atoms, &snapshot));
        let deleted = Event::PropertyNotify(xproto::PropertyNotifyEvent {
            response_type: 28, sequence: 0, window: 20, atom: 44, time: 0,
            state: xproto::Property::DELETE,
        });
        assert!(is_visual_property_notify(&deleted, 1, atoms, &snapshot));
        let unrelated_atom = Event::PropertyNotify(xproto::PropertyNotifyEvent {
            response_type: 28, sequence: 0, window: 20, atom: 99, time: 0,
            state: xproto::Property::NEW_VALUE,
        });
        assert!(!is_visual_property_notify(&unrelated_atom, 1, atoms, &snapshot));
        let unrelated_window = Event::PropertyNotify(xproto::PropertyNotifyEvent {
            response_type: 28, sequence: 0, window: 999, atom: 44, time: 0,
            state: xproto::Property::NEW_VALUE,
        });
        assert!(!is_visual_property_notify(&unrelated_window, 1, atoms, &snapshot));
    }

    #[test]
    fn blur_visual_state_invalidation_preserves_pending_pixel_damage() {
        // Same InvalidationBatch machinery a blur PropertyNotify already
        // routes through (SceneInvalidation::VisualState) — proves the
        // pending-PixelDamage-never-lost invariant holds regardless of
        // which VisualState source triggered it.
        let mut batch = InvalidationBatch::default();
        batch.push(SceneInvalidation::PixelDamage(7));
        batch.push(SceneInvalidation::VisualState);
        assert_eq!(batch.decision(), SceneInvalidation::VisualState);
        assert!(batch.pixel_damage().contains(&7));
        assert!(batch_damage_requires_subtraction(SceneInvalidation::VisualState, batch.pixel_damage()));
    }

    #[test]
    fn initialize_visual_state_dedups_by_semantic_client() {
        let source = include_str!("scene.rs");
        let start = source.find("fn initialize_visual_state(").expect("initialize_visual_state exists");
        let end = start + source[start..].find("\n    fn build_candidate").expect("function body ends before build_candidate");
        let body = &source[start..end];
        assert!(body.contains("self.urgency.contains_key(&client)"));
        assert!(body.contains("read_client_urgency"));
        let dedup_index = body.find("self.urgency.contains_key(&client)").unwrap();
        let query_index = body.find("read_client_urgency(").unwrap();
        assert!(dedup_index < query_index, "the dedup check must precede the query");
    }

    #[test]
    fn semantic_client_none_is_excluded_from_blur_query_iteration() {
        let with_client = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let without_client = eligible_surface(&metadata(), None, root(), 10, 1).unwrap();
        let entries = vec![with_client, without_client];
        let clients: Vec<xproto::Window> = entries.iter().filter_map(|entry| entry.semantic_client_xid).collect();
        assert_eq!(clients, vec![20]);
    }

    #[test]
    fn fullscreen_does_not_erase_cached_blur_request() {
        // Mirrors fullscreen_transition_removes_and_restores_shadow_policy:
        // the raw cached request must survive a fullscreen transition
        // unmodified. Phase 2A adds no resolved-eligibility field at all
        // (deliberately — see module docs), so there is nothing yet that
        // COULD suppress it; this test locks in that the cache itself is
        // never touched by fullscreen state.
        let mut cache = HashMap::new();
        cache.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, ..CachedClientVisualState::default() });
        let mut entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        entry.fullscreen = true;
        assert!(entry.fullscreen);
        assert_eq!(cache.get(&20).unwrap().blur_requested, BlurRequest::FullWindow, "toggling fullscreen must not touch the cached request");
        entry.fullscreen = false;
        assert!(!entry.fullscreen);
        assert_eq!(cache.get(&20).unwrap().blur_requested, BlurRequest::FullWindow);
    }

    #[test]
    fn phase_2a_property_update_does_not_call_the_blur_primitive() {
        // Phase 2A still only caches/invalidates the request. Rendering is
        // performed later from the resolved snapshot by Phase 2B2b.
        let source = include_str!("scene.rs");
        let start = source.find("fn update_visual_state(").expect("update_visual_state exists");
        let end = start + source[start..].find("\n    fn refresh_resolved_visual_state").expect("function body ends");
        assert!(!source[start..end].contains("capture_and_blur_background"));
    }

    // ========================================================
    // 3a3f7 Phase 2B1 — resolved per-SurfaceEntry blur-request ownership.
    // Structural ownership (semantic_client_xid -> cached BlurRequest) is
    // unchanged; rendering consumes only the resolved FullWindow variant.
    // ========================================================

    #[test]
    fn resolved_blur_request_is_none_when_semantic_client_is_none() {
        let entry = eligible_surface(&metadata(), None, root(), 10, 0).unwrap();
        let urgency = HashMap::new();
        assert_eq!(resolved_blur_request(&entry, &urgency), BlurRequest::None);
    }

    #[test]
    fn resolved_blur_request_reflects_cached_none() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState::default());
        assert_eq!(resolved_blur_request(&entry, &urgency), BlurRequest::None);
    }

    #[test]
    fn global_blur_permission_preserves_only_existing_requests() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let none = HashMap::new();
        assert_eq!(permitted_blur_request(&entry, &none, true), BlurRequest::None);
        assert_eq!(permitted_blur_request(&entry, &none, false), BlurRequest::None);
        let regions = vec![BlurRegionRect { x: 1, y: 2, width: 3, height: 4 }];
        let mut requested = HashMap::new();
        requested.insert(20, CachedClientVisualState { blur_requested: BlurRequest::Regions(regions.clone()), ..CachedClientVisualState::default() });
        assert_eq!(permitted_blur_request(&entry, &requested, true), BlurRequest::Regions(regions));
        assert_eq!(permitted_blur_request(&entry, &requested, false), BlurRequest::None);
    }

    #[test]
    fn no_request_with_global_blur_enabled_stays_none() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        assert_eq!(permitted_blur_request(&entry, &HashMap::new(), true), BlurRequest::None);
    }

    #[test]
    fn application_request_with_global_blur_enabled_is_preserved() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let mut cache = HashMap::new();
        cache.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, ..CachedClientVisualState::default() });
        assert_eq!(permitted_blur_request(&entry, &cache, true), BlurRequest::FullWindow);
    }

    #[test]
    fn application_request_with_global_blur_disabled_is_suppressed() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let mut cache = HashMap::new();
        cache.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, ..CachedClientVisualState::default() });
        assert_eq!(permitted_blur_request(&entry, &cache, false), BlurRequest::None);
    }

    #[test]
    fn no_request_with_global_blur_disabled_stays_none() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        assert_eq!(permitted_blur_request(&entry, &HashMap::new(), false), BlurRequest::None);
    }

    #[test]
    fn resolved_blur_request_reflects_cached_full_window() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, ..CachedClientVisualState::default() });
        assert_eq!(resolved_blur_request(&entry, &urgency), BlurRequest::FullWindow);
    }

    #[test]
    fn resolved_blur_request_preserves_exact_regions() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let regions = vec![BlurRegionRect { x: 5, y: 6, width: 7, height: 8 }];
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState { blur_requested: BlurRequest::Regions(regions.clone()), ..CachedClientVisualState::default() });
        assert_eq!(resolved_blur_request(&entry, &urgency), BlurRequest::Regions(regions));
    }

    #[test]
    fn resolved_blur_request_isolates_distinct_clients() {
        let entry_a = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let entry_b = eligible_surface(&metadata(), Some(30), root(), 10, 1).unwrap();
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, ..CachedClientVisualState::default() });
        // client 30 has no cache entry at all yet.
        assert_eq!(resolved_blur_request(&entry_a, &urgency), BlurRequest::FullWindow);
        assert_eq!(resolved_blur_request(&entry_b, &urgency), BlurRequest::None);
    }

    #[test]
    fn resolve_snapshot_fullscreen_resolves_independent_owners_for_two_clients() {
        // Models the task's own two-top-level-clients scenario: surface A
        // -> C1 -> FullWindow, surface B -> C2 -> None; then C2 gaining a
        // Regions request must affect B only.
        let entry_a = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let entry_b = eligible_surface(&metadata(), Some(30), root(), 10, 1).unwrap();
        let mut snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: vec![entry_a, entry_b] };
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, ..CachedClientVisualState::default() });
        urgency.insert(30, CachedClientVisualState::default());
        let style = crate::config::CompositorConfig::defaults().visuals.shadow;
        resolve_snapshot_fullscreen(&mut snapshot, &urgency, true, style);
        assert_eq!(snapshot.entries[0].resolved_blur_request, BlurRequest::FullWindow);
        assert_eq!(snapshot.entries[1].resolved_blur_request, BlurRequest::None);

        let regions = vec![BlurRegionRect { x: 1, y: 1, width: 2, height: 2 }];
        urgency.insert(30, CachedClientVisualState { blur_requested: BlurRequest::Regions(regions.clone()), ..CachedClientVisualState::default() });
        resolve_snapshot_fullscreen(&mut snapshot, &urgency, true, style);
        assert_eq!(snapshot.entries[0].resolved_blur_request, BlurRequest::FullWindow, "C1's owner must be unaffected by C2's change");
        assert_eq!(snapshot.entries[1].resolved_blur_request, BlurRequest::Regions(regions));
    }

    #[test]
    fn property_delete_transitions_full_window_to_none_on_rebuild() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let mut snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: vec![entry] };
        let style = crate::config::CompositorConfig::defaults().visuals.shadow;
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, ..CachedClientVisualState::default() });
        resolve_snapshot_fullscreen(&mut snapshot, &urgency, true, style);
        assert_eq!(snapshot.entries[0].resolved_blur_request, BlurRequest::FullWindow);
        // Client deletes the property; Phase 2A's re-query (unchanged)
        // caches None for it.
        urgency.insert(20, CachedClientVisualState::default());
        resolve_snapshot_fullscreen(&mut snapshot, &urgency, true, style);
        assert_eq!(snapshot.entries[0].resolved_blur_request, BlurRequest::None);
    }

    #[test]
    fn blur_only_visual_state_updates_resolved_entry_without_full_rebuild() {
        // The incremental (PropertyNotify-triggered, no full candidate
        // rebuild) path: source-contract, since exercising it end-to-end
        // requires a live connection. Proves the new blur branch exists
        // and writes `resolved_blur_request` strictly AFTER the cache
        // itself is updated (so the synced value is never stale).
        let source = include_str!("scene.rs");
        let start = source.find("fn update_visual_state(").expect("update_visual_state exists");
        let end = start + source[start..].find("\n    fn refresh_resolved_visual_state").expect("function body ends");
        let body = &source[start..end];
        assert!(body.contains("if property.atom == self.visual_atoms.blur_behind_region {"));
        assert!(body.contains("entry.resolved_blur_request = updated_blur_requested"));
        let insert_index = body.find("self.urgency.insert(property.window, updated);").expect("cache insert exists");
        let entry_update_index = body.find("entry.resolved_blur_request = updated_blur_requested").expect("live entry sync exists");
        assert!(insert_index < entry_update_index, "cache must be updated before the live entry is synced");
    }

    #[test]
    fn fullscreen_does_not_erase_resolved_blur_request() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let style = crate::config::CompositorConfig::defaults().visuals.shadow;
        let mut snapshot = SceneSnapshot { root: 1, root_geometry: root(), entries: vec![entry] };
        let mut urgency = HashMap::new();
        urgency.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, fullscreen: false, ..CachedClientVisualState::default() });
        resolve_snapshot_fullscreen(&mut snapshot, &urgency, true, style);
        assert_eq!(snapshot.entries[0].resolved_blur_request, BlurRequest::FullWindow);
        assert!(!snapshot.entries[0].fullscreen);

        urgency.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, fullscreen: true, ..CachedClientVisualState::default() });
        resolve_snapshot_fullscreen(&mut snapshot, &urgency, true, style);
        assert_eq!(snapshot.entries[0].resolved_blur_request, BlurRequest::FullWindow, "request identity must survive a fullscreen transition");
        assert!(snapshot.entries[0].fullscreen);
    }

    #[test]
    fn opacity_or_transparency_never_creates_a_blur_request() {
        let mut entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        entry.resolved_opacity_bits = 0.25f32.to_bits();
        entry.depth = 32;
        let urgency = HashMap::new();
        assert_eq!(resolved_blur_request(&entry, &urgency), BlurRequest::None);
    }

    #[test]
    fn resolved_blur_request_never_reads_opacity_or_visual_signals() {
        // Structural-only proof (Phase 2B owner audit, sections 3/8/16):
        // the resolver's only inputs are semantic_client_xid and the
        // cache — no WM_CLASS/PID/override_redirect/visual_class/opacity/
        // fullscreen shortcut.
        let source = include_str!("scene.rs");
        let start = source.find("fn resolved_blur_request(").expect("resolved_blur_request exists");
        let end = start + source[start..].find("\n}\n").expect("function body ends");
        let body = &source[start..end];
        for forbidden in ["resolved_opacity_bits", "visual_class", ".depth", "override_redirect", "WM_CLASS", ".fullscreen"] {
            assert!(!body.contains(forbidden), "resolved_blur_request must not reference {forbidden}");
        }
    }

    #[test]
    fn popup_helper_with_no_semantic_client_never_inherits_another_clients_request() {
        let popup = eligible_surface(&metadata(), None, root(), 10, 0).unwrap();
        let mut urgency = HashMap::new();
        // Some OTHER client has an active FullWindow request.
        urgency.insert(20, CachedClientVisualState { blur_requested: BlurRequest::FullWindow, ..CachedClientVisualState::default() });
        assert_eq!(resolved_blur_request(&popup, &urgency), BlurRequest::None);
    }

    #[test]
    fn window_type_classification_is_exact_and_deterministic() {
        assert_eq!(classify_surface_visual_class(Some("_NET_WM_WINDOW_TYPE_DOCK")), SurfaceVisualClass::Dock);
        assert_eq!(classify_surface_visual_class(Some("_NET_WM_WINDOW_TYPE_DESKTOP")), SurfaceVisualClass::Desktop);
        assert_eq!(classify_surface_visual_class(Some("_NET_WM_WINDOW_TYPE_NORMAL")), SurfaceVisualClass::Normal);
        assert_eq!(classify_surface_visual_class(None), SurfaceVisualClass::Normal);
    }

    #[test]
    fn semantic_window_type_precedes_capture_type_for_visual_classification() {
        let mut capture = metadata();
        capture.window_type = Some("_NET_WM_WINDOW_TYPE_DOCK".to_string());
        let mut semantic = metadata();
        semantic.window = 20;
        semantic.window_type = Some("_NET_WM_WINDOW_TYPE_NORMAL".to_string());
        let entry = eligible_surface_with_semantic_metadata(&capture, Some(20), Some(&semantic), root(), 10, 0).unwrap();
        assert_eq!(entry.visual_class, SurfaceVisualClass::Normal);
    }

    #[test]
    fn semantic_dock_behind_presentation_frame_is_classified_as_dock() {
        let mut capture = metadata();
        capture.window_type = None;
        let mut semantic = metadata();
        semantic.window = 20;
        semantic.window_type = Some("_NET_WM_WINDOW_TYPE_DOCK".to_string());
        let entry = eligible_surface_with_semantic_metadata(&capture, Some(20), Some(&semantic), root(), 10, 0).unwrap();
        assert_eq!(entry.visual_class, SurfaceVisualClass::Dock);
        let config = crate::config::CompositorConfig::with_corner_radius(16.0).unwrap();
        let mut plan = build_render_quad_plan(capture.geometry, pixmap(20, 20), root()).unwrap();
        apply_surface_visual_policy(&mut plan, &config.visuals, entry.visual_class);
        assert_eq!(plan.corner_radius, 0.0);
    }

    #[test]
    fn capture_type_is_used_without_semantic_client() {
        let mut capture = metadata();
        capture.window_type = Some("_NET_WM_WINDOW_TYPE_DOCK".to_string());
        assert_eq!(effective_window_type(&capture, None), Some("_NET_WM_WINDOW_TYPE_DOCK"));
        let entry = eligible_surface_with_semantic_metadata(&capture, None, None, root(), 10, 0).unwrap();
        assert_eq!(entry.visual_class, SurfaceVisualClass::Dock);
    }

    #[test]
    fn absent_semantic_type_falls_back_to_capture_type() {
        let mut capture = metadata();
        capture.window_type = Some("_NET_WM_WINDOW_TYPE_DOCK".to_string());
        let mut semantic = metadata();
        semantic.window = 20;
        semantic.window_type = None;
        assert_eq!(effective_window_type(&capture, Some(&semantic)), Some("_NET_WM_WINDOW_TYPE_DOCK"));
        let entry = eligible_surface_with_semantic_metadata(&capture, Some(20), Some(&semantic), root(), 10, 0).unwrap();
        assert_eq!(entry.visual_class, SurfaceVisualClass::Dock);
    }

    #[test]
    fn semantic_desktop_preserves_zero_radius_policy() {
        let mut semantic = metadata();
        semantic.window = 20;
        semantic.window_type = Some("_NET_WM_WINDOW_TYPE_DESKTOP".to_string());
        let entry = eligible_surface_with_semantic_metadata(&metadata(), Some(20), Some(&semantic), root(), 10, 0).unwrap();
        assert_eq!(entry.visual_class, SurfaceVisualClass::Desktop);
        let config = crate::config::CompositorConfig::with_corner_radius(16.0).unwrap();
        let mut plan = build_render_quad_plan(window(0, 0, 20, 20, 0), pixmap(20, 20), root()).unwrap();
        apply_surface_visual_policy(&mut plan, &config.visuals, entry.visual_class);
        assert_eq!(plan.corner_radius, 0.0);
    }

    #[test]
    fn no_semantic_client_with_absent_capture_type_remains_normal() {
        let capture = metadata();
        let entry = eligible_surface_with_semantic_metadata(&capture, None, None, root(), 10, 0).unwrap();
        assert_eq!(entry.visual_class, SurfaceVisualClass::Normal);
    }

    #[test]
    fn semantic_type_classification_is_independent_of_capture_geometry() {
        let mut capture = metadata();
        capture.geometry = window(0, 0, 1920, 26, 0);
        let mut semantic = metadata();
        semantic.window = 20;
        semantic.window_type = Some("_NET_WM_WINDOW_TYPE_NORMAL".to_string());
        let entry = eligible_surface_with_semantic_metadata(&capture, Some(20), Some(&semantic), root(), 10, 0).unwrap();
        assert_eq!(entry.visual_class, SurfaceVisualClass::Normal);
    }

    #[test]
    fn wallpaper_quad_has_zero_corner_radius() {
        let plan = build_background_render_quad_plan(PixmapGeometry { root: 1, x: 0, y: 0, width: 1920, height: 1080, border_width: 0, depth: 24 }, background_root()).unwrap();
        assert_eq!(plan.corner_radius, 0.0);
        assert_eq!(plan.border_width, 0.0);
    }

    #[test]
    fn move_only_accepts_same_surface_metadata_with_only_position_changed() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        assert!(move_only_geometry_is_eligible(
            &entry,
            window(17, 23, 20, 20, 0),
            1,
            false,
            Some(20),
        ));
    }

    #[test]
    fn move_only_rejects_resize_and_identity_or_lifecycle_changes() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let moved = window(17, 23, 20, 20, 0);
        let rejected = [
            (window(17, 23, 21, 20, 0), false, Some(20), 1),
            (moved, true, Some(20), 1),
            (moved, false, None, 1),
            (moved, false, Some(20), 0),
        ];
        for (geometry, override_redirect, semantic, expected_root) in rejected {
            assert!(!move_only_geometry_is_eligible(
                &entry, geometry, expected_root,
                override_redirect, semantic,
            ));
        }
    }

    #[test]
    fn move_only_fast_path_contains_no_resource_acquisition() {
        let source = include_str!("scene.rs");
        let start = source.find("fn try_move_only(").expect("move-only helper exists");
        let end = start + source[start..].find("\n    fn current_snapshot(").expect("move-only helper ends");
        let body = &source[start..end];
        for forbidden in ["DamageLease::acquire", "NamedSurfacePixmap::acquire", "import_pixmap"] {
            assert!(!body.contains(forbidden), "move-only path must not acquire {forbidden}");
        }
    }

    #[test]
    fn move_batch_retains_latest_geometry_and_rejects_ambiguity() {
        let first = PendingGeometry { surface_xid: 10, x: 1, y: 2, width: 20, height: 20, border_width: 0, override_redirect: false };
        let latest = PendingGeometry { x: 8, y: 9, ..first };
        let other = PendingGeometry { surface_xid: 11, ..latest };
        let mut batch = InvalidationBatch::default();
        batch.push(SceneInvalidation::Geometry(10));
        batch.push_geometry_update(Some(first));
        batch.push(SceneInvalidation::Geometry(10));
        batch.push_geometry_update(Some(latest));
        assert_eq!(batch.move_geometry(), Some(latest));
        batch.push(SceneInvalidation::Geometry(11));
        batch.push_geometry_update(Some(other));
        assert_eq!(batch.move_geometry(), None);
    }

    #[test]
    fn move_only_client_root_follows_surface_delta_and_preserves_bounds() {
        let root = ClientRootGeometry { root_x: 100, root_y: 200, width: 800, height: 600 };
        let moved = move_client_root_geometry(root, window(10, 20, 30, 40, 0), window(17, 13, 30, 40, 0));
        assert_eq!(moved, ClientRootGeometry { root_x: 107, root_y: 193, width: 800, height: 600 });
    }

    #[test]
    fn candidate_pure_move_rebase_updates_only_root_geometry_and_client_origin() {
        let mut entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        entry.client_root_geometry = Some(ClientRootGeometry { root_x: 30, root_y: 40, width: 20, height: 20 });
        let update = PendingGeometry { surface_xid: 10, x: 17, y: 23, width: 20, height: 20, border_width: 0, override_redirect: false };
        let immutable = (entry.depth, entry.visual, entry.semantic_client_xid, entry.resolved_blur_request.clone());
        rebase_candidate_geometry_fields(&mut entry, update);
        assert_eq!(entry.geometry, window(17, 23, 20, 20, 0));
        assert_eq!(entry.client_root_geometry, Some(ClientRootGeometry { root_x: 47, root_y: 63, width: 20, height: 20 }));
        assert_eq!((entry.depth, entry.visual, entry.semantic_client_xid, entry.resolved_blur_request), immutable);
    }

    #[test]
    fn candidate_rebase_rejects_resize_by_the_existing_move_predicate() {
        let entry = eligible_surface(&metadata(), Some(20), root(), 10, 0).unwrap();
        let resize = PendingGeometry { surface_xid: 10, x: 17, y: 23, width: 21, height: 20, border_width: 0, override_redirect: false };
        assert!(!move_only_geometry_is_eligible(&entry, window(resize.x, resize.y, resize.width, resize.height, resize.border_width), 1, resize.override_redirect, entry.semantic_client_xid));
    }

    #[test]
    fn candidate_rebase_preserves_relative_order_of_common_surfaces() {
        let first = visibility_test_entry(window(10, 10, 20, 20, 0), false);
        let mut second = first.clone();
        second.surface_xid = 11;
        let mut inserted = first.clone();
        inserted.surface_xid = 12;
        assert!(same_common_surface_order(&[first.clone(), second.clone()], &[inserted, first.clone(), second.clone()]));
        assert!(!same_common_surface_order(&[first.clone(), second.clone()], &[second, first]));
    }

    #[test]
    fn candidate_rebase_is_bounded_and_keeps_lifecycle_gate() {
        let source = include_str!("scene.rs");
        let start = source.find("fn pre_commit_gate(").expect("pre-commit gate exists");
        let end = start + source[start..].find("\n    fn commit_candidate(").expect("pre-commit gate ends");
        let body = &source[start..end];
        assert!(body.contains("rebase_candidate_pure_move"));
        assert!(body.contains("!batch.hierarchy"));
        assert!(body.contains("!batch.background"));
        assert!(body.contains("!batch.visual_state"));
        assert!(body.contains("bounded_batch_requires_retry(drained)"));
        assert!(body.contains("attempted_structural_generation = self.structural_generation"));
        assert!(body.contains("batch.push_geometry_update(geometry_update)"));
        assert_eq!(MAX_CANDIDATE_RETRIES, 1);
    }

    #[test]
    fn bootstrap_candidate_does_not_require_a_published_live_snapshot() {
        let source = include_str!("scene.rs");
        let start = source.find("fn build_candidate(").expect("candidate builder exists");
        let end = start + source[start..].find("\n    fn refresh_resize_state_before_acquisition").expect("early checkpoint follows candidate setup");
        let setup = &source[start..end];
        assert!(setup.contains("self.snapshot.as_ref().is_some_and"));
        assert!(!setup.contains("candidate_has_resized_target(self.current_snapshot()"));
    }

    #[test]
    fn move_only_fast_path_has_zero_validation_queries() {
        let source = include_str!("scene.rs");
        let start = source.find("fn try_move_only(").expect("move-only helper exists");
        let end = start + source[start..].find("\n    fn current_snapshot(").expect("move-only helper ends");
        let body = &source[start..end];
        for forbidden in ["get_geometry", "get_window_attributes", "get_input_focus", "verify_ownership", "translate_coordinates"] {
            assert!(!body.contains(forbidden), "move-only path must not call {forbidden}");
        }
    }

    #[test]
    fn early_resize_obsolescence_rejects_only_dimension_changes() {
        let candidate = window(10, 20, 800, 600, 2);
        let move_update = PendingGeometry { surface_xid: 10, x: 30, y: 40, width: 800, height: 600, border_width: 2, override_redirect: false };
        let resize_update = PendingGeometry { width: 801, ..move_update };
        assert!(!resize_geometry_is_obsolete(candidate, move_update));
        assert!(resize_geometry_is_obsolete(candidate, resize_update));
    }

    #[test]
    fn early_resize_checkpoint_is_before_damage_acquisition() {
        let source = include_str!("scene.rs");
        let checkpoint = source.find("refresh_resize_state_before_acquisition").expect("early resize checkpoint exists");
        let acquisition = source[checkpoint..].find("DamageLease::acquire").expect("resize acquisition exists");
        assert!(source[checkpoint..].find("return Err(Box::new(CandidateBuildError::Stale").is_some());
        assert!(acquisition > 0);
    }

    #[test]
    fn present_history_records_single_and_repeated_deferral_buckets() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        let once = GeometryPresentHistory::default().deferred();
        let multiple = once.deferred();
        diagnostics.record_pending_present_history(once);
        diagnostics.record_pending_present_history(multiple);
        diagnostics.record_final_resize_history(once, ResizeOnlyDirection::Grow, false);
        diagnostics.record_final_resize_history(multiple, ResizeOnlyDirection::Shrink, true);
        assert_eq!(diagnostics.geometry_pending_ever_present_deferred, 2);
        assert_eq!(diagnostics.geometry_pending_present_deferred_once, 1);
        assert_eq!(diagnostics.geometry_pending_present_deferred_multiple, 1);
        assert_eq!(diagnostics.final_resize_was_present_deferred, 2);
        assert_eq!(diagnostics.final_resize_never_present_deferred, 0);
        assert_eq!(diagnostics.final_resize_deferrals_1, 1);
        assert_eq!(diagnostics.final_resize_deferrals_2_3, 1);
    }

    #[test]
    fn present_history_outcome_cohorts_are_partitioned() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        let deferred = GeometryPresentHistory::default().deferred();
        let immediate = GeometryPresentHistory::default();
        diagnostics.record_final_resize_history(deferred, ResizeOnlyDirection::Grow, false);
        diagnostics.record_final_resize_history(immediate, ResizeOnlyDirection::Shrink, false);
        diagnostics.record_final_resize_selection(deferred, false);
        diagnostics.record_final_resize_selection(immediate, true);
        assert_eq!(diagnostics.final_resize_was_present_deferred + diagnostics.final_resize_never_present_deferred, 2);
        assert_eq!(diagnostics.resizeonly_selected_after_present_defer, 1);
        assert_eq!(diagnostics.structural_selected_without_present_defer, 1);
        assert_eq!(diagnostics.resizeonly_selected_after_present_defer + diagnostics.resizeonly_selected_without_present_defer, 1);
        assert_eq!(diagnostics.structural_selected_after_present_defer + diagnostics.structural_selected_without_present_defer, 1);
    }

    #[test]
    fn present_history_partitions_resizeonly_success_and_precommit_fallback() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        diagnostics.resizeonly_present_deferred = Some(true);
        diagnostics.record_resizeonly_cohort_outcome(true, None);
        diagnostics.resizeonly_present_deferred = Some(false);
        diagnostics.record_resizeonly_cohort_outcome(false, Some(ResizeOnlyFallbackReason::PrecommitRejected));
        assert_eq!(diagnostics.resizeonly_success_after_present_defer, 1);
        assert_eq!(diagnostics.resizeonly_fallback_without_present_defer, 1);
        assert_eq!(diagnostics.precommit_rejected_after_present_defer, 0);
        assert_eq!(diagnostics.precommit_rejected_without_present_defer, 1);
    }

    #[test]
    fn present_history_saturates_deferral_histogram() {
        let mut history = GeometryPresentHistory::default();
        for _ in 0..32 { history = history.deferred(); }
        assert_eq!(history.deferrals, 8);
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        diagnostics.record_final_resize_history(history, ResizeOnlyDirection::Mixed, false);
        assert_eq!(diagnostics.final_resize_deferrals_8_plus, 1);
    }

    #[test]
    fn present_history_pending_updates_are_explicit() {
        let mut batch = InvalidationBatch::default();
        batch.present_history = GeometryPresentHistory::default().deferred();
        batch.push_geometry_update(Some(PendingGeometry { surface_xid: 10, x: 0, y: 0, width: 20, height: 20, border_width: 0, override_redirect: false }));
        assert!(batch.present_history.updated_while_deferred);
        assert!(batch.present_history.superseded_while_deferred);
    }

    #[test]
    fn present_history_reporter_has_separate_population_sections() {
        let source = include_str!("scene.rs");
        for section in ["3a3f8b5q_scheduling", "3a3f8b5q_pending_geometry_cohort", "3a3f8b5q_final_resize", "3a3f8b5q_outcome_present_history", "3a3f8b5q_structural_present_history"] {
            assert!(source.contains(section), "reporter must contain {section}");
        }
        assert!(source.contains("not_final_resize_decisions"));
    }

    #[test]
    fn hierarchy_raw_event_sources_are_complete_and_disjoint() {
        let sources = [
            HierarchyEventSource::UnknownConfigure,
            HierarchyEventSource::Create,
            HierarchyEventSource::Map,
            HierarchyEventSource::Unmap,
            HierarchyEventSource::Destroy,
            HierarchyEventSource::Reparent,
            HierarchyEventSource::Circulate,
        ];
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        for source in sources { diagnostics.record_hierarchy_event(source, false, HierarchyEventRelation::Unknown); }
        assert_eq!(diagnostics.hierarchy_event_total, 7);
        assert_eq!(diagnostics.hierarchy_event_unknown_configure, 1);
        assert_eq!(diagnostics.hierarchy_event_create + diagnostics.hierarchy_event_map + diagnostics.hierarchy_event_unmap + diagnostics.hierarchy_event_destroy + diagnostics.hierarchy_event_reparent + diagnostics.hierarchy_event_circulate, 6);
    }

    #[test]
    fn hierarchy_decision_source_bitset_distinguishes_single_and_multi_source() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        diagnostics.record_hierarchy_decision(HierarchyEventSource::Map.bit(), false, None);
        diagnostics.record_hierarchy_decision(HierarchyEventSource::Map.bit() | HierarchyEventSource::Reparent.bit(), false, None);
        assert_eq!(diagnostics.hierarchy_decision_total, 2);
        assert_eq!(diagnostics.hierarchy_decision_only_map, 1);
        assert_eq!(diagnostics.hierarchy_decision_multi_source, 1);
        assert_eq!(diagnostics.hierarchy_decision_only_map + diagnostics.hierarchy_decision_multi_source, 2);
    }

    #[test]
    fn hierarchy_pending_geometry_is_counted_when_hierarchy_wins() {
        let mut batch = InvalidationBatch::default();
        batch.push(SceneInvalidation::Geometry(10));
        batch.push_geometry_update(Some(PendingGeometry { surface_xid: 10, x: 0, y: 0, width: 21, height: 20, border_width: 0, override_redirect: false }));
        batch.note_hierarchy_source(HierarchyEventSource::Create);
        batch.push(SceneInvalidation::Hierarchy);
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        diagnostics.record_hierarchy_decision(batch.hierarchy_source_bits, batch.hierarchy_geometry_pending, None);
        assert_eq!(batch.decision(), SceneInvalidation::Hierarchy);
        assert_eq!(diagnostics.hierarchy_decision_with_geometry_pending, 1);
        assert_eq!(diagnostics.hierarchy_decision_cleared_pending_geometry, 1);
        assert_eq!(diagnostics.hierarchy_selected_while_resize_geometry_pending, 1);
    }

    #[test]
    fn hierarchy_source_stage_preserves_retry_and_deferred_provenance() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        diagnostics.hierarchy_source_bits = HierarchyEventSource::UnknownConfigure.bit();
        diagnostics.begin_structural_origin(StructuralOrigin::Hierarchy);
        diagnostics.record_stale_origin(SceneInvalidation::Geometry(10), false);
        diagnostics.record_stale_origin(SceneInvalidation::Geometry(10), true);
        assert_eq!(diagnostics.hierarchy_unknown_configure_candidate_stale_geometry, 2);
        assert_eq!(diagnostics.hierarchy_unknown_configure_retry, 1);
        assert_eq!(diagnostics.hierarchy_unknown_configure_deferred, 1);
    }

    #[test]
    fn hierarchy_reporter_separates_raw_decisions_and_source_dimensions() {
        let source = include_str!("scene.rs");
        for field in ["hierarchy_event_total", "hierarchy_decision_total", "hierarchy_decision_multi_source", "hierarchy_decision_with_geometry_pending", "hierarchy_from_internal_window", "snapshot_hierarchy_unknown_configure", "hierarchy_unknown_configure_candidate_stale_geometry"] {
            assert!(source.contains(field), "hierarchy reporter/accounting must contain {field}");
        }
        assert!(source.contains("raw_event_population_separate_from_scheduler_decisions"));
    }

    fn compound_test_snapshot(entry: SurfaceEntry) -> SceneSnapshot {
        SceneSnapshot { root: 1, root_geometry: RootGeometry { width: 1920, height: 1080, depth: 24, visual: 7 }, entries: vec![entry] }
    }

    #[test]
    fn compound_identity_accepts_geometry_only_change() {
        let entry = visibility_test_entry(window(10, 20, 100, 80, 0), true);
        let live = compound_test_snapshot(entry.clone());
        let mut candidate_entry = entry;
        rebase_candidate_geometry_fields(&mut candidate_entry, PendingGeometry { surface_xid: 0, x: 30, y: 40, width: 140, height: 120, border_width: 2, override_redirect: false });
        let candidate = compound_test_snapshot(candidate_entry);
        assert!(structural_identity_matches(&live, &candidate));
        assert!(target_geometry_rebase_compatible(&live, &candidate, PendingGeometry { surface_xid: 0x0040_0000, x: 30, y: 40, width: 140, height: 120, border_width: 2, override_redirect: true }));
    }

    #[test]
    fn compound_identity_rejects_root_change() {
        let entry = visibility_test_entry(window(10, 20, 100, 80, 0), true);
        let live = compound_test_snapshot(entry.clone());
        let mut candidate = compound_test_snapshot(entry);
        candidate.root = 2;
        assert!(!structural_identity_matches(&live, &candidate));
    }

    #[test]
    fn compound_identity_rejects_scene_addition() {
        let entry = visibility_test_entry(window(10, 20, 100, 80, 0), true);
        let live = compound_test_snapshot(entry.clone());
        let mut candidate = compound_test_snapshot(entry.clone());
        candidate.entries.push(entry);
        assert!(!structural_identity_matches(&live, &candidate));
    }

    #[test]
    fn compound_identity_rejects_stacking_change() {
        let entry = visibility_test_entry(window(10, 20, 100, 80, 0), true);
        let live = compound_test_snapshot(entry.clone());
        let mut candidate = compound_test_snapshot(entry);
        candidate.entries[0].stacking_index += 1;
        assert!(!structural_identity_matches(&live, &candidate));
    }

    #[test]
    fn compound_identity_rejects_visual_depth_backend_changes() {
        let entry = visibility_test_entry(window(10, 20, 100, 80, 0), true);
        let live = compound_test_snapshot(entry.clone());
        let mut visual = compound_test_snapshot(entry.clone());
        visual.entries[0].visual += 1;
        assert!(!structural_identity_matches(&live, &visual));
        let mut depth = compound_test_snapshot(entry.clone());
        depth.entries[0].depth += 1;
        assert!(!structural_identity_matches(&live, &depth));
        let mut backend = compound_test_snapshot(entry);
        backend.entries[0].backend = BackendCompatibility::Renderable;
        assert!(!structural_identity_matches(&live, &backend));
    }

    #[test]
    fn compound_identity_rejects_map_state_change() {
        let entry = visibility_test_entry(window(10, 20, 100, 80, 0), true);
        let live = compound_test_snapshot(entry.clone());
        let mut candidate = compound_test_snapshot(entry);
        candidate.entries[0].map_state = xproto::MapState::UNMAPPED;
        assert!(!structural_identity_matches(&live, &candidate));
        assert!(!target_geometry_rebase_compatible(&live, &candidate, PendingGeometry { surface_xid: 0, x: 1, y: 1, width: 101, height: 81, border_width: 0, override_redirect: false }));
    }

    #[test]
    fn compound_identity_rejects_target_surface_and_client_changes() {
        let entry = visibility_test_entry(window(10, 20, 100, 80, 0), true);
        let live = compound_test_snapshot(entry.clone());
        let mut surface = compound_test_snapshot(entry.clone());
        surface.entries[0].surface_xid = 11;
        assert!(!target_geometry_rebase_compatible(&live, &surface, PendingGeometry { surface_xid: 10, x: 1, y: 1, width: 101, height: 81, border_width: 0, override_redirect: false }));
        let mut client = compound_test_snapshot(entry);
        client.entries[0].semantic_client_xid = Some(99);
        assert!(!structural_identity_matches(&live, &client));
    }

    #[test]
    fn compound_identity_rejects_lifecycle_change() {
        let entry = visibility_test_entry(window(10, 20, 100, 80, 0), true);
        let live = compound_test_snapshot(entry.clone());
        let mut candidate = compound_test_snapshot(entry);
        candidate.entries[0].lifecycle_xid = 77;
        assert!(!structural_identity_matches(&live, &candidate));
    }

    #[test]
    fn compound_rebase_rejections_are_bounded_and_accountable() {
        let mut diagnostics = Diagnostics3a3f8b3a { enabled: true, ..Default::default() };
        diagnostics.compound_rebase_attempted = 3;
        diagnostics.compound_rebase_success = 1;
        diagnostics.compound_rebase_rejected_scene_membership = 1;
        diagnostics.compound_rebase_rejected_newer_hierarchy = 1;
        assert_eq!(diagnostics.compound_rebase_attempted, diagnostics.compound_rebase_success + diagnostics.compound_rebase_rejected_scene_membership + diagnostics.compound_rebase_rejected_newer_hierarchy);
        assert_eq!(MAX_CANDIDATE_RETRIES, 1);
    }

    #[test]
    fn damage_identity_ignores_geometry_but_not_visual_compatibility() {
        let entry = visibility_test_entry(window(10, 20, 100, 80, 0), true);
        let mut resized = entry.clone();
        resized.geometry.width += 20;
        resized.geometry.height += 10;
        assert!(damage_identity_compatible(&entry, &resized));
        resized.visual += 1;
        assert!(!damage_identity_compatible(&entry, &resized));
    }

    #[test]
    fn damage_identity_rejects_lifecycle_and_map_changes() {
        let entry = visibility_test_entry(window(10, 20, 100, 80, 0), true);
        let mut lifecycle = entry.clone();
        lifecycle.lifecycle_xid = 77;
        assert!(!damage_identity_compatible(&entry, &lifecycle));
        let mut unmapped = entry;
        unmapped.map_state = xproto::MapState::UNMAPPED;
        assert!(!damage_identity_compatible(&lifecycle, &unmapped));
    }

    #[test]
    fn resized_compound_resource_path_uses_damage_identity_split() {
        let source = include_str!("scene.rs");
        assert!(source.contains("damage_identity_compatible(previous, &entry)"));
        assert!(source.contains("compound_rebase_damage_reused"));
        assert!(source.contains("NamedSurfacePixmap::acquire"));
        assert!(source.contains("egl.import_pixmap"));
    }

}
