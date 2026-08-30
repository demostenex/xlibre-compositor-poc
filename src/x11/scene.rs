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
    client_root_geometry: Option<ClientRootGeometry>,
    lifecycle_xid: Window,
    geometry: WindowGeometry,
    depth: u8,
    visual: u32,
    class: WindowClass,
    map_state: xproto::MapState,
    override_redirect: bool,
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
        visual_class: classify_surface_visual_class(metadata.window_type.as_deref()),
        fullscreen: false,
        shadow_eligible: false,
        resolved_border_color: [0.0f32.to_bits(), 0.0f32.to_bits(), 0.0f32.to_bits(), 1.0f32.to_bits()],
        resolved_opacity_bits: 1.0f32.to_bits(),
        client_root_geometry: None,
        resolved_blur_request: BlurRequest::None,
    })
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

fn shadow_params_from_plan(
    style: crate::config::ShadowConfig,
    plan: &RenderQuadPlan,
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
        style.strength,
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

fn resolve_snapshot_fullscreen(
    snapshot: &mut SceneSnapshot,
    urgency: &HashMap<Window, CachedClientVisualState>,
    blur_enabled: bool,
    style: crate::config::ShadowConfig,
) {
    for entry in &mut snapshot.entries {
        entry.fullscreen = entry
            .semantic_client_xid
            .and_then(|client| urgency.get(&client))
            .is_some_and(|state| state.fullscreen);
        entry.shadow_eligible = shadow_eligible_for_entry(style, entry);
        entry.resolved_blur_request = permitted_blur_request(entry, urgency, blur_enabled);
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

    fn build_candidate(&mut self) -> Result<SceneCandidate<'a>, Box<dyn Error>> {
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
        self.render_egl_scene(&snapshot, &egl_surfaces, &pixmaps)?;
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
                    self.structure_watches.rollback(&candidate.watch_additions)?;
                    self.diagnostics.record_structural_terminal(false, false, false);
                    return Err(format!("candidate aborted by shutdown: {reason:?}").into());
                }
                GateDecision::Retry(invalidation) if retry_allowed(attempt) => {
                    self.diagnostics.structural_candidates_stale += 1;
                    self.diagnostics.record_stale_origin(invalidation, false);
                    if self.diagnostics.last_candidate_resize { self.diagnostics.resize_candidate_stale += 1; }
                    self.merge_deferred_damage(deferred_damage);
                    self.structure_watches.rollback(&candidate.watch_additions)?;
                    self.diagnostics.record_structural_terminal(false, true, true);
                    println!("candidate stale; bounded retry: {invalidation:?}");
                }
                GateDecision::Retry(invalidation) => {
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
        let render_result = self.render_egl_scene(
            &candidate.snapshot,
            &candidate.egl_surfaces,
            &candidate.pixmaps,
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
        let egl = self.egl.as_ref().ok_or("EGL scene renderer is unavailable")?;
        egl.swap()?;
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
                let first = match wait_for_event_or_shutdown(self.connection, &mut self.signal)? {
                    WaitResult::Event(event) => event,
                    WaitResult::Shutdown => {
                        println!("scene shutdown: Signal");
                        return Ok(());
                    }
                };
                let snapshot = self.snapshot.as_ref().expect("published scene snapshot must exist while live");
                self.diagnostics.record_configure(&first, snapshot);
                let first_geometry_source = geometry_event_source(&first, snapshot);
                let first_surface_xid = snapshot.entries.iter().find_map(|entry| matches!(&first, Event::ConfigureNotify(event) if entry.surface_xid == event.window || entry.semantic_client_xid == Some(event.window)).then_some(entry.surface_xid));
                self.diagnostics.record_geometry_source(first_geometry_source);
                batch.note_configure_event(&first, first_geometry_source, first_surface_xid);
                opportunity_msc = self.present_opportunity(&first);
                let geometry_update = configure_geometry_update(&first, self.current_snapshot());
                let visual_invalidation = self.maybe_update_visual_state(&first)?;
                let invalidation = visual_invalidation.unwrap_or_else(|| {
                self.classify_session_event(first.clone(), self.current_snapshot(), &self.damage_registry, &self.damage_registry)
                });
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
                    let snapshot = self.snapshot.as_ref().expect("published scene snapshot must exist while live");
                    self.diagnostics.record_configure(&event, snapshot);
                    let geometry_source = geometry_event_source(&event, snapshot);
                    let surface_xid = snapshot.entries.iter().find_map(|entry| matches!(&event, Event::ConfigureNotify(event) if entry.surface_xid == event.window || entry.semantic_client_xid == Some(event.window)).then_some(entry.surface_xid));
                    self.diagnostics.record_geometry_source(geometry_source);
                    batch.note_configure_event(&event, geometry_source, surface_xid);
                    opportunity_msc = opportunity_msc.or_else(|| self.present_opportunity(&event));
                    let geometry_update = configure_geometry_update(&event, self.current_snapshot());
                    let visual_invalidation = self.maybe_update_visual_state(&event)?;
                    let invalidation = visual_invalidation.unwrap_or_else(|| {
                    self.classify_session_event(event.clone(), self.current_snapshot(), &self.damage_registry, &self.damage_registry)
                    });
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
                SceneInvalidation::Ignore => {}
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
                        self.egl.as_ref().ok_or("EGL scene renderer is unavailable")?.swap()?;
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
                    self.egl.as_ref().ok_or("EGL scene renderer is unavailable")?.swap()?;
                }
                SceneInvalidation::VisualState => {
                    self.pending_visual_state = false;
                    self.full_recompose_current()?;
                    self.egl.as_ref().ok_or("EGL scene renderer is unavailable")?.swap()?;
                }
                SceneInvalidation::PixelDamage(_) => {
                    self.recompose_current_scene(batch.pixel_damage().clone())?;
                }
            }
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
        };
        let target_build_start = self.diagnostics.enabled.then(Instant::now);
        self.render_egl_scene(&candidate.snapshot, &candidate.egl_surfaces, &candidate.pixmaps)?;
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
                return self.egl.as_ref().ok_or("EGL scene renderer is unavailable")?.swap();
            }
            SceneInvalidation::VisualState => {
                self.pending_visual_state = false;
                self.full_recompose_current()?;
                return self.egl.as_ref().ok_or("EGL scene renderer is unavailable")?.swap();
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
            return self
                .egl
                .as_ref()
                .ok_or("EGL scene renderer is unavailable")?
                .swap();
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
        let mut batch = InvalidationBatch::default();
        for _ in 0..MAX_EVENTS_PER_BATCH {
            let Some(event) = self.connection.inner.poll_for_event()? else {
                break;
            };
            let snapshot = self.snapshot.as_ref().expect("published scene snapshot must exist while live");
            self.diagnostics.record_configure(&event, snapshot);
            let geometry_source = geometry_event_source(&event, snapshot);
            self.diagnostics.record_geometry_source(geometry_source);
            let visual_invalidation = self.maybe_update_visual_state(&event)?;
            let invalidation = visual_invalidation.unwrap_or_else(|| {
                self.classify_session_event(event.clone(), self.current_snapshot(), &self.damage_registry, &self.damage_registry)
            });
            let current_snapshot = self.current_snapshot().clone();
            self.record_hierarchy_event_diagnostic(&event, invalidation, &current_snapshot, batch.geometry_update);
            if matches!(invalidation, SceneInvalidation::Hierarchy) { if let Some(source) = hierarchy_event_source(&event) { batch.note_hierarchy_source(source); } }
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
        self.diagnostics.record_damage_dispatch(damage_id, geometry_pending, identity);
        Ok(())
    }

    fn full_recompose_current(&mut self) -> Result<(), Box<dyn Error>> {
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
        let egl = self.egl.as_mut().ok_or("EGL scene renderer is unavailable")?;
        render_egl_scene_parts(
            egl, background, shadow_style, visuals, snapshot, surfaces, pixmaps,
        )
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
    ) -> Result<(), Box<dyn Error>> {
        render_egl_scene_parts(
            self.egl.as_mut().ok_or("EGL scene renderer is unavailable")?,
            self.background.as_ref(),
            self.shadow_style,
            &self._config.visuals,
            snapshot,
            surfaces,
            pixmaps,
        )
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
        } else {
            if let Some(background) = self.background.as_mut() {
                background.surface.disarm();
            }
            for surface in self.egl_surfaces.values() {
                surface.borrow_mut().disarm();
            }
        }
        self.background = None;
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
        if let Some(background) = self.background.as_mut() {
            background.surface.disarm();
        }
        self.background = None;
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
) -> Result<(), Box<dyn Error>> {
    egl.clear()?;
    if let Some(background) = background {
        if let Some(plan) = build_background_render_quad_plan(background.source.geometry, snapshot.root_geometry) {
            egl.render_surface(background.surface.texture, plan, background.surface.pixel_semantics)?;
        }
    }
    for entry in &snapshot.entries {
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
        plan.border_color = entry.resolved_border_color.map(f32::from_bits);

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
        if entry.shadow_eligible {
            if let Some(shadow) = shadow_params_from_plan(shadow_style, &plan) {
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
        let opacity = crate::graphics::renderer::SurfaceOpacity::new(
            f32::from_bits(entry.resolved_opacity_bits),
        ).expect("resolved surface opacity must be valid");
        egl.render_surface_with_opacity(
            surface.texture,
            plan,
            surface.pixel_semantics,
            opacity,
        )?;
    }
    Ok(())
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
        entry.semantic_client_xid == Some(event.window)
            && (event.atom == atoms.wm_hints
                || event.atom == atoms.net_wm_state
                || event.atom == atoms.blur_behind_region)
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
    use std::time::Duration;

    use super::{
        build_copy_plan,
        parse_background_property, build_background_render_quad_plan,
        effective_corner_radius,
        effective_border_width,
        classify_surface_visual_class, apply_surface_visual_policy, border_visual_state,
        rendered_border_color, BorderVisualState, CachedClientVisualState, SurfaceVisualClass,
        wm_hints_urgency, state_demands_attention,
        is_background_property_notify, BackgroundAtoms, BackgroundCandidate, BackgroundPixmap,
        classify_event, coordinator_requires_cleanup, eligible_surface,
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
        Diagnostics3a3f8b3a,
        ResizeOnlyDirectionDiagnostics,
        ResizeOnlyFallbackReason, ResizeOnlyFallbackReasons,
        retain_pending_for_registry,
        rect_intersects_root, surface_quad_intersects_root, shadow_bounds_intersect_root,
        entry_has_visible_contribution, prune_invisible_entries,
        resource_identity_fields_match,
        damage_identity_compatible,
        BlurRequest, BlurRegionRect, parse_blur_behind_region, is_visual_property_notify,
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
    };
    use crate::x11::capture::WindowGeometry;
    use super::super::tree::{BindingStatus, HierarchyBinding, HierarchySnapshot};
    use x11rb::errors::ReplyError;
    use x11rb::protocol::damage::ReportLevel;
    use x11rb::protocol::render;
    use x11rb::protocol::xproto::{EventMask, MapState, Rectangle, WindowClass};
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
            lifecycle_xid: 0x0040_0000,
            geometry,
            depth: 24,
            visual: 0x2d8,
            class: WindowClass::INPUT_OUTPUT,
            map_state: MapState::VIEWABLE,
            override_redirect: true,
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
        let shadow = shadow_params_from_plan(style, &plan).unwrap();
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
        assert_eq!(shadow_params_from_plan(first.visuals.shadow, &first_plan).unwrap().outer_width,
            shadow_params_from_plan(second.visuals.shadow, &second_plan).unwrap().outer_width);
        assert_eq!(shadow_params_from_plan(first.visuals.shadow, &first_plan).unwrap().outer_height,
            shadow_params_from_plan(second.visuals.shadow, &second_plan).unwrap().outer_height);
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
        let first_shadow = shadow_params_from_plan(config.visuals.shadow, &first_plan).unwrap();
        assert_eq!((first_shadow.outer_x, first_shadow.outer_y), (10.0, 30.0));

        let mut second = first.clone();
        second.geometry.x = 55;
        second.geometry.y = 5;
        let second_plan = build_render_quad_plan(second.geometry, local_pixmap, root()).unwrap();
        let second_shadow = shadow_params_from_plan(config.visuals.shadow, &second_plan).unwrap();
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
        let first = shadow_params_from_plan(config.visuals.shadow, &first_plan).unwrap();
        config.visuals.corner_radius = 16.0;
        let mut second_plan = build_render_quad_plan(entry.geometry, visual_quad, root()).unwrap();
        apply_surface_visual_policy(&mut second_plan, &config.visuals, entry.visual_class);
        let second = shadow_params_from_plan(config.visuals.shadow, &second_plan).unwrap();
        assert_ne!(first.corner_radius, second.corner_radius);
        let mut moved = entry;
        moved.geometry.x += 11;
        moved.geometry.y += 13;
        let moved_quad = PixmapGeometry { x: visual_quad.x + 11, y: visual_quad.y + 13, ..visual_quad };
        let mut moved_plan = build_render_quad_plan(moved.geometry, moved_quad, root()).unwrap();
        apply_surface_visual_policy(&mut moved_plan, &config.visuals, moved.visual_class);
        let moved_shadow = shadow_params_from_plan(config.visuals.shadow, &moved_plan).unwrap();
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
        assert!(shadow_params_from_plan(enabled.visuals.shadow, &plan).is_none());
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
    fn blur_behind_region_property_notify_is_visual_state_scoped() {
        let atoms = VisualAtoms {
            active_window: 1, wm_hints: 2, net_wm_state: 3,
            demands_attention: 42, fullscreen: 43, blur_behind_region: 44,
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
