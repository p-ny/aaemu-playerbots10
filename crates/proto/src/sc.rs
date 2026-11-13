//! Server → client packet parsing into typed events.
//!
//! [`crate::sc_world`] covers the world, units, the own character, inventory, combat, chat and
//! loot, and (with the payload types of [`crate::sc_travel`]) the teleport book, instance loads
//! and boats; [`crate::sc_activity`] covers quests, duels, trade, teams, houses, mail and
//! crafting.

use std::cell::Cell;

use crate::stream::MarshalError;

thread_local! {
    static UNREAD: Cell<usize> = const { Cell::new(0) };
}

pub(crate) fn note_unread(n: usize) {
    UNREAD.with(|u| u.set(n));
}

/// Bytes the last parser on this thread left unread (layout checks against captured packets:
/// a correct parser of a packet bots read in full leaves 0).
pub fn last_unread() -> usize {
    UNREAD.with(|u| u.get())
}

pub use crate::sc_activity::ActivityEvent;
pub use crate::sc_world::WorldEvent;

#[derive(Debug, Clone)]
pub enum ServerEvent {
    World(WorldEvent),
    Activity(ActivityEvent),
}

/// Parses one S→C packet (level 1 game or level 2 proxy; DD04 batches are expanded by
/// [`crate::game`] first). `Ok(None)` when bots do not need the packet; `Err` when the body does
/// not match the expected layout.
pub fn parse(level: u8, opcode: u16, body: &[u8]) -> Result<Option<ServerEvent>, MarshalError> {
    note_unread(0);
    if let Some(e) = crate::sc_world::parse(level, opcode, body)? {
        return Ok(Some(ServerEvent::World(e)));
    }
    if let Some(e) = crate::sc_activity::parse(level, opcode, body)? {
        return Ok(Some(ServerEvent::Activity(e)));
    }
    Ok(None)
}

/// The ship companion of [`parse`]: a ship's `SCOneUnitMovementPacket` (move type 3) as
/// [`WorldEvent::ShipMoved`] (heading, velocity, throttle, steering). [`parse`] returns
/// [`WorldEvent::UnitMoved`] for the same packet, so existing position tracking is unchanged; a
/// bot that follows ships feeds the packet to both. Every other packet is `Ok(None)` and costs
/// no body read.
pub fn parse_ship(
    level: u8,
    opcode: u16,
    body: &[u8],
) -> Result<Option<ServerEvent>, MarshalError> {
    Ok(crate::sc_world::parse_ship(level, opcode, body)?.map(ServerEvent::World))
}
