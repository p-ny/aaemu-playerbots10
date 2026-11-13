// Packet layouts ported from the S->C writers of AAEmu-1.2-Rust rust/crates/aaemu-game/src
// (systems/instances/packets.rs), GPL-3.0-or-later.
//! Server → client parsers for the tower defence events ("rifts"): the list of running events
//! and the start, wave and end notices (see [`crate::sc_world::WorldEvent`], whose variants carry
//! the payload types below).
//!
//! The server sends the four packets to every character in the world instance of an event: the
//! start, each wave and the end each followed by a fresh list, and the list again to a character
//! that changes zone once any event has run. Writer citations are `file:line` below
//! `rust/crates/aaemu-game/src/` of AAEmu-1.2-Rust, abbreviated `G/`.

use crate::pos::convert_long_to_coord;
use crate::sc_world::{check_count, some, Res, WorldEvent};
use crate::stream::PacketStream;

/// The most entries the 1.2 client (and so the server) puts in one list
/// (`SCTowerDefListPacket::MAX_INFOS`, G/systems/instances/packets.rs:732).
pub const MAX_LIST: usize = 100;

/// Bytes of one list entry: key 6 + zone 4 + spot 4 + target object 3 + x 8 + y 8 + z 4 + step 4.
const INFO_BYTES: usize = 41;

/// The identity of an event: the definition (`tower_defs.id`) and the zone group it runs in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct EventId {
    /// `tower_defs.id` (3 is the Crimson Rift).
    pub def: u32,
    /// The zone group the event runs in.
    pub zone_group: u16,
}

/// One running event of `SCTowerDefListPacket` (`TowerDefInfo`, G/systems/instances/packets.rs:113).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct TowerDefInfo {
    /// Which event.
    pub id: EventId,
    /// The zone key the event was started in.
    pub zone_key: u32,
    /// The spawner of the defended target (`tower_defs.target_npc_spawner_id`).
    pub spot_id: u32,
    /// Object id of the defended target NPC, 0 while none is alive.
    pub target_obj: u32,
    /// World position of that target; all zero while there is none.
    pub pos: (f32, f32, f32),
    /// The wave the event is in (0 before the first).
    pub step: u32,
}

impl TowerDefInfo {
    /// Whether the entry carries the position of a live target.
    pub fn has_target(&self) -> bool {
        self.target_obj != 0
    }
}

/// `TowerDefKey` (G/systems/instances/packets.rs:101): `u32 definition, u16 zone group`.
fn read_key(s: &mut PacketStream) -> Res<EventId> {
    Ok(EventId {
        def: s.read_u32()?,
        zone_group: s.read_u16()?,
    })
}

/// `SCTowerDefListPacket` (G/systems/instances/packets.rs:735): a count byte and the entries.
pub(crate) fn read_list(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let count = usize::from(s.read_u8()?);
    check_count(s, count, INFO_BYTES, "tower defense list")?;
    let mut list = Vec::with_capacity(count.min(MAX_LIST));
    for _ in 0..count {
        let id = read_key(s)?;
        let zone_key = s.read_u32()?;
        let spot_id = s.read_u32()?;
        let target_obj = s.read_bc()?;
        let x = convert_long_to_coord(s.read_i64()?);
        let y = convert_long_to_coord(s.read_i64()?);
        let z = s.read_f32()?;
        let step = s.read_u32()?;
        list.push(TowerDefInfo {
            id,
            zone_key,
            spot_id,
            target_obj,
            pos: (x, y, z),
            step,
        });
    }
    some(WorldEvent::TowerDefList(list.into_boxed_slice()))
}

/// `SCTowerDefStartPacket` (G/systems/instances/packets.rs:756): the key and the zone key.
pub(crate) fn read_start(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let id = read_key(s)?;
    let zone_key = s.read_u32()?;
    s.read_bool()?; // start seamless world
    s.read_bool()?; // broadcast seamless
    some(WorldEvent::TowerDefStart { id, zone_key })
}

/// `SCTowerDefEndPacket` (G/systems/instances/packets.rs:774): the key and the zone key.
pub(crate) fn read_end(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let id = read_key(s)?;
    let zone_key = s.read_u32()?;
    s.read_bool()?;
    s.read_bool()?;
    some(WorldEvent::TowerDefEnd { id, zone_key })
}

/// `SCTowerDefWaveStartPacket` (G/systems/instances/packets.rs:793): the key, the zone key and
/// the wave number (from 1).
pub(crate) fn read_wave(s: &mut PacketStream) -> Res<Option<WorldEvent>> {
    let id = read_key(s)?;
    let zone_key = s.read_u32()?;
    let step = s.read_u32()?;
    s.read_bool()?; // sync step
    some(WorldEvent::TowerDefWave { id, zone_key, step })
}
