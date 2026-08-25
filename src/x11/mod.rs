pub mod connection;
pub mod capture;
pub mod compositor;
pub mod tree;
pub mod tree_watch;
pub mod preflight;
pub mod shutdown;
pub mod overlay;
pub mod manual;

use x11rb::protocol::xproto::MapState;

pub(crate) fn map_state_name(state: MapState) -> &'static str {
    match state {
        MapState::UNMAPPED => "UNMAPPED",
        MapState::UNVIEWABLE => "UNVIEWABLE",
        MapState::VIEWABLE => "VIEWABLE",
        _ => "UNKNOWN",
    }
}
