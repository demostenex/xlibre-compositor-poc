pub mod connection;
pub mod capture;
pub mod compositor;

use x11rb::protocol::xproto::MapState;

pub(crate) fn map_state_name(state: MapState) -> &'static str {
    match state {
        MapState::UNMAPPED => "UNMAPPED",
        MapState::UNVIEWABLE => "UNVIEWABLE",
        MapState::VIEWABLE => "VIEWABLE",
        _ => "UNKNOWN",
    }
}
