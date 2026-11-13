//! Boats: summon scrolls and the slaves (vehicles) they summon.
//!
//! A scroll is an item whose use skill carries the `SpawnSlave` special effect (type 60: skills
//! 15802, 19622, 22267); `item_summon_slaves` names the slave it summons
//! (`systems/skills/effects/special/misc.rs: spawn_slave` -> `systems/vehicles/slaves.rs:
//! create_from_skill`). Only scrolls of *boat* kinds are kept (`SlaveTemplate::is_a_boat`,
//! `vehicles/data.rs:282`): big / small sailing ship, speedboat, boat, fishboat, merchant ship.
//!
//! The summon needs water at the chosen spot (a ship "must be summoned on water") within
//! [`SlaveTemplate::summon_range`] of the caster; the caster then boards the driver seat
//! (attach point 1) and steers with throttle / steering input, passengers take points 2 and up.
//!
//! The top speed follows the server's ship physics (`systems/ai/physics/ship.rs`): the hull's
//! internal speed is capped at `ship_models.velocity * MoveSpeedMul / 2` and the body moves at
//! `speed * MoveSpeedMul / 4`, so the real top speed is `velocity * MoveSpeedMul^2 / 8`, where
//! `MoveSpeedMul` is the unit attribute (base 1000, so 1.0) plus the slave's `unit_modifiers`
//! rows (every boat carries +1000, so 2.0 and the real top speed is half of `velocity`: 3.2 m/s
//! for the rowboat, 6.5 for the clipper), clamped to `unit_attribute_limits`.
//!
//! [`ShipClass`] sums up what a bot needs to know to drive a hull: speeds, the turn radius, the
//! hull box, the draft, where the server puts a summoned hull, how long the summon takes, and the
//! cannons and pack racks the hull carries.

use anyhow::{Context, Result};
use rustc_hash::{FxHashMap, FxHashSet};

use super::{localized, str_bytes, SkillTiming, TravelInputs};
use crate::geom::IdMap;
use crate::sql::{each_row, RowExt};
use crate::text::TextTable;

/// `SlaveKind` values of boats (`vehicles/data.rs:167`).
pub mod slave_kind {
    pub const BIG_SAILING_SHIP: u8 = 1;
    pub const SMALL_SAILING_SHIP: u8 = 2;
    pub const SPEEDBOAT: u8 = 3;
    pub const BOAT: u8 = 4;
    pub const FISHBOAT: u8 = 9;
    pub const MERCHANT_SHIP: u8 = 10;
}

/// The `SpawnSlave` special effect type.
const SPAWN_SLAVE_EFFECT: u32 = 60;
/// The skill that casts the common boat scrolls.
pub const SUMMON_BOAT_SKILL: u32 = 15802;
/// The doodad template of a pack rack (`slave_doodad_bindings`): a trade pack is put in with skill
/// 15307 and taken out with 15309.
pub const PACK_RACK_DOODAD: u32 = 3446;
/// Driver seat attach point.
pub const DRIVER_POINT: u8 = 1;
/// Passenger attach points are `PASSENGER_FIRST..=PASSENGER_LAST` (`Passenger0..Passenger6`).
pub const PASSENGER_FIRST: u8 = 2;
/// Last passenger attach point.
pub const PASSENGER_LAST: u8 = 8;

// `UnitAttribute` ids in `unit_modifiers` / `unit_attribute_limits`.
const ATTR_STA: u32 = 2;
const ATTR_MAX_HEALTH: u32 = 6;
const ATTR_MOVE_SPEED_MUL: u32 = 10;
/// `unit_formulas` rows (owner type 2 = slave) the hit point estimate mirrors.
const MAX_HEALTH_FORMULA: &str = "(level * 70 + sta * 12)";
const STA_FORMULA: &str = "10 + (level - 1) * 2";

/// Whether a `slave_kind_id` is a boat.
pub fn is_boat_kind(kind: u8) -> bool {
    use slave_kind::*;
    matches!(
        kind,
        BIG_SAILING_SHIP | SMALL_SAILING_SHIP | SPEEDBOAT | BOAT | FISHBOAT | MERCHANT_SHIP
    )
}

/// A boat slave template.
#[derive(Clone, Debug, PartialEq)]
pub struct SlaveTemplate {
    /// `slaves.id`.
    pub id: u32,
    /// English name (Korean if there is no translation).
    pub name: Box<str>,
    /// `slaves.model_id`.
    pub model: u32,
    /// `slaves.slave_kind_id`; see [`slave_kind`].
    pub kind: u8,
    /// `slaves.level` (the level the server gives the slave unit).
    pub level: u8,
    /// Maximum hit points the server computes for the slave, estimated from its formulas and
    /// flat / percent modifiers (no buffs, no gear).
    pub max_hp: u32,
    /// `ship_models.velocity` of the model (0 without a ship model).
    pub velocity: f32,
    /// `ship_models.turn_accel` of the model (0 without a ship model or the column).
    pub turn_accel: f32,
    /// Final `MoveSpeedMul` of the template (1.0 = no modifier).
    pub move_speed_mul: f32,
    /// `slaves.spawn_valid_area_range`: how far from the caster the boat may be placed, metres.
    pub summon_range: u16,
    /// `slaves.mountable`.
    pub mountable: bool,
    /// The rest of the ship model: size and the speed physics.
    pub ship: ShipModelData,
    /// `slaves.spawn_x_offset`: how far ahead of the caster the server starts looking for a spot
    /// for the hull (it then tries one metre further for 40 steps).
    pub spawn_x: f32,
    /// `slaves.spawn_y_offset`: the lateral offsets (left and right) tried at every step.
    pub spawn_y: f32,
    /// `slaves.portal_time`: seconds after the summon during which the helm is ignored.
    pub portal_time: f32,
    /// Bit `n` set = the model declares attach point `n` (`model_bindings`), for `n` in 1..=8.
    seats: u16,
    /// Slaves attached to the hull (`slave_bindings`): cannons, harpoons.
    children: Box<[ChildSlave]>,
    /// Attach points of the pack racks (`slave_doodad_bindings`, doodad [`PACK_RACK_DOODAD`]).
    racks: Box<[u8]>,
}

/// A slave attached to a hull.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ChildSlave {
    /// The attach point on the hull.
    pub point: u8,
    /// The child's slave template (10 = cannon, 48 = harpoon gun).
    pub slave: u32,
    /// The child's `slave_kind_id` (8 = slave equipment, as the cannon; 7 = siege weapon).
    pub kind: u8,
}

/// The cannon's slave template (`slave_bindings.slave_id`).
pub const CANNON_SLAVE: u32 = 10;

/// `ship_models`: the hull box and the speed physics of a ship model (all 0 without a ship model).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ShipModelData {
    /// `accel`: the forward acceleration (internal speed units per second at full throttle).
    pub accel: f32,
    /// `reverse_accel`.
    pub reverse_accel: f32,
    /// `reverse_velocity`.
    pub reverse_velocity: f32,
    /// `mass_box_size_x`: the beam in metres.
    pub beam: f32,
    /// `mass_box_size_y`: the length.
    pub length: f32,
    /// `mass_box_size_z`: the height.
    pub height: f32,
    /// `mass_center_y`: how far the box centre lies ahead of the hull's origin.
    pub center_y: f32,
    /// `mass_center_z`.
    pub center_z: f32,
    /// `keel_height`.
    pub keel: f32,
}

impl SlaveTemplate {
    /// Top speed in metres per second without wind: the internal speed cap is
    /// `velocity * MoveSpeedMul / 2` and the body moves at `speed * MoveSpeedMul / 4`
    /// (`systems/ai/physics/ship.rs`), so `velocity * MoveSpeedMul^2 / 8`.
    pub fn top_speed(&self) -> f32 {
        self.velocity * self.move_speed_mul * self.move_speed_mul / 8.0
    }

    /// Internal speed units per metre per second of real speed (`4 / MoveSpeedMul`, 2 for every
    /// boat): the server's turn factor and acceleration work on the internal speed.
    pub fn internal_per_real(&self) -> f32 {
        4.0 / self.move_speed_mul.max(0.1)
    }

    /// Cannons the hull carries (its child slaves of a cannon kind).
    pub fn cannon_points(&self) -> impl Iterator<Item = u8> + '_ {
        self.children
            .iter()
            .filter(|c| c.slave == CANNON_SLAVE)
            .map(|c| c.point)
    }

    /// The attach points of the pack racks.
    pub fn rack_points(&self) -> &[u8] {
        &self.racks
    }

    /// Slaves attached to the hull.
    pub fn children(&self) -> &[ChildSlave] {
        &self.children
    }

    /// What a bot needs to know to drive the hull; see [`ShipClass`].
    pub fn class(&self) -> ShipClass {
        ShipClass::of(self)
    }

    /// How the hull turns, from the server's ship physics (`systems/ai/physics/ship.rs`, per unit
    /// of helm at full speed): the angular acceleration at full helm in rad/s^2
    /// (`pi * turn_accel / 360 * 1.45`, the 1.45 being `STEERING_RESPONSIVENESS_MUL`) and the turn
    /// rate the helm settles at in rad/s (`steer_max_deg_fixed(kind)`, 4.35 degrees per second for
    /// a boat). Both fall to half at standstill (the turn factor rises from 0.5 to 1 over the
    /// first 2.5 m/s). `None` without a ship model.
    pub fn turn(&self) -> Option<(f32, f32)> {
        if self.turn_accel <= 0.0 {
            return None;
        }
        let accel = std::f32::consts::PI * self.turn_accel / 360.0 * 1.45;
        let cap_deg: f32 = match self.kind {
            slave_kind::BOAT => 4.35,
            slave_kind::SMALL_SAILING_SHIP => 2.6,
            slave_kind::BIG_SAILING_SHIP => 2.35,
            slave_kind::FISHBOAT => 3.35,
            slave_kind::SPEEDBOAT => 5.85,
            slave_kind::MERCHANT_SHIP => 2.85,
            _ => 3.1,
        };
        Some((accel, cap_deg.to_radians()))
    }

    /// Whether the model has a driver seat (attach point 1).
    pub fn has_driver_seat(&self) -> bool {
        self.seats & (1 << DRIVER_POINT) != 0
    }

    /// Passenger seats the model declares (attach points 2..=8).
    pub fn passenger_seats(&self) -> u8 {
        (PASSENGER_FIRST..=PASSENGER_LAST)
            .filter(|p| self.seats & (1 << p) != 0)
            .count() as u8
    }

    /// The seat attach points (driver first) the model declares among 1..=8.
    pub fn seat_points(&self) -> impl Iterator<Item = u8> + '_ {
        (DRIVER_POINT..=PASSENGER_LAST).filter(|p| self.seats & (1 << p) != 0)
    }

    /// The slave is a boat (it is for every template in this table).
    pub fn is_boat(&self) -> bool {
        is_boat_kind(self.kind)
    }
}

/// How big a hull is, which sets the water it needs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HullSize {
    /// Under 6 m long: the rowboat.
    Small,
    /// 6 to 20 m: the clipper and the fishing boats.
    Medium,
    /// 20 m and more: the cutter, junk, yawl, schooner and the liner.
    Large,
}

/// Terrain below this height (m) is deep enough for a [`HullSize::Large`] hull: 4.5 m of water.
pub const DEEP_FLOOR_M: f32 = 95.5;

/// What a bot needs to know to drive and berth one kind of hull, summed up from its slave
/// template ([`SlaveTemplate::class`]). All figures are the real ones of the server's physics, not
/// the nominal `ship_models` columns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ShipClass {
    /// `slaves.id`.
    pub slave: u32,
    /// `slaves.slave_kind_id`; see [`slave_kind`].
    pub kind: u8,
    /// The size class.
    pub size: HullSize,
    /// Top speed in m/s (see [`SlaveTemplate::top_speed`]).
    pub top_speed: f32,
    /// The turn rate the helm settles at in rad/s, at full turn factor (the kind's cap).
    pub turn_rate: f32,
    /// Angular acceleration at full helm in rad/s^2.
    pub turn_accel: f32,
    /// Turn radius at top speed in metres: `top_speed / turn_rate` (64 m for the clipper, 139 m
    /// for the cutter, 216 m for the yawl, 42 m for the rowboat). A hull turns tighter at lower
    /// speed.
    pub radius: f32,
    /// Beam of the hull box (`ship_models.mass_box_size_x`).
    pub beam: f32,
    /// Length of the hull box (`mass_box_size_y`).
    pub length: f32,
    /// Height of the hull box (`mass_box_size_z`).
    pub height: f32,
    /// A guess of how deep the keel lies below the waterline, metres: the offset the server gives
    /// the hull at the summon.
    pub draft: f32,
    /// How far ahead of the caster the server starts looking for a spot (m).
    pub spawn_ahead: f32,
    /// The lateral offsets it tries at every step (m).
    pub spawn_side: f32,
    /// Milliseconds after the summon during which the server ignores the helm.
    pub portal_ms: u32,
    /// Internal speed units per real m/s (2 for every boat).
    pub int_per_real: f32,
    /// Deceleration in m/s^2 with full astern throttle.
    pub brake: f32,
    /// Acceleration in m/s^2 with full ahead throttle at low speed.
    pub accel: f32,
    /// How far the hull box's centre lies ahead of the hull's origin (`mass_center_y`).
    pub center_y: f32,
    /// Water the hull box reaches down into at the summon, metres: the server tests the box
    /// (the model's bounding box, or the mass box when there is none) against the terrain.
    pub box_depth: f32,
    /// Cannons on the hull.
    pub cannons: u8,
    /// Pack racks on the hull.
    pub racks: u8,
    /// Passenger seats (attach points 2..=8 of the model).
    pub passengers: u8,
}

impl ShipClass {
    /// The rowboat (slave 15), as the data has it; checked against the real data by the
    /// `ship_classes` test of `travel_real_data`. For tests and for a bot without hull data.
    pub const ROWBOAT: ShipClass = ShipClass {
        slave: 15,
        kind: slave_kind::BOAT,
        size: HullSize::Small,
        top_speed: 3.2,
        turn_rate: 0.07592,
        turn_accel: 0.12526,
        radius: 42.15,
        beam: 1.0,
        length: 3.0,
        height: 0.5,
        draft: 0.6,
        spawn_ahead: 1.5,
        spawn_side: 1.5,
        portal_ms: 0,
        int_per_real: 2.0,
        brake: 0.6,
        accel: 0.25,
        center_y: 0.0,
        box_depth: 0.85,
        cannons: 0,
        racks: 0,
        passengers: 1,
    };

    /// The clipper (slave 52); see [`ShipClass::ROWBOAT`].
    pub const CLIPPER: ShipClass = ShipClass {
        slave: 52,
        kind: slave_kind::SPEEDBOAT,
        size: HullSize::Medium,
        top_speed: 6.5,
        turn_rate: 0.1021,
        turn_accel: 0.018979,
        radius: 63.66,
        beam: 4.0,
        length: 8.0,
        height: 7.0,
        draft: 0.3,
        spawn_ahead: 5.0,
        spawn_side: 5.0,
        portal_ms: 7000,
        int_per_real: 2.0,
        brake: 0.6,
        accel: 0.25,
        center_y: 0.0,
        box_depth: 0.8,
        cannons: 0,
        racks: 0,
        passengers: 0,
    };

    /// The Eznan cutter (slave 21); see [`ShipClass::ROWBOAT`].
    pub const CUTTER: ShipClass = ShipClass {
        slave: 21,
        kind: slave_kind::SMALL_SAILING_SHIP,
        size: HullSize::Large,
        top_speed: 6.3,
        turn_rate: 0.045379,
        turn_accel: 0.012654,
        radius: 138.8,
        beam: 6.5,
        length: 30.0,
        height: 14.5,
        draft: 2.2,
        spawn_ahead: 30.0,
        spawn_side: 20.0,
        portal_ms: 7000,
        int_per_real: 2.0,
        brake: 0.6,
        accel: 0.25,
        center_y: 3.0,
        box_depth: 13.45,
        cannons: 8,
        racks: 4,
        passengers: 0,
    };

    /// The class of a slave template.
    pub fn of(t: &SlaveTemplate) -> ShipClass {
        let top = t.top_speed();
        let (turn_accel, turn_rate) = t.turn().unwrap_or((0.1254, 0.0759));
        let m = &t.ship;
        let length = if m.length > 0.0 { m.length } else { 3.0 };
        let beam = if m.beam > 0.0 { m.beam } else { 1.0 };
        // `summon_offset_z` of the server: the hull sits (center_z / 2 when below the waterline)
        // plus the keel height under the surface
        let draft = ((if m.center_z < 0.0 {
            -m.center_z / 2.0
        } else {
            0.0
        }) + m.keel)
            .max(0.3);
        let int_per_real = t.internal_per_real();
        let size = if length < 6.0 {
            HullSize::Small
        } else if length < 20.0 {
            HullSize::Medium
        } else {
            HullSize::Large
        };
        // (the opposing throttle: the larger of the two accelerations times 2.4, per internal
        // speed unit and second, halved by the server's tick arithmetic)
        let brake_int = m.accel.max(m.reverse_accel).max(0.1) * 2.4 / 2.0;
        ShipClass {
            slave: t.id,
            kind: t.kind,
            size,
            top_speed: top,
            turn_rate,
            turn_accel,
            radius: if turn_rate > 0.0 {
                top / turn_rate
            } else {
                0.0
            },
            beam,
            length,
            height: m.height,
            draft,
            spawn_ahead: t.spawn_x,
            spawn_side: t.spawn_y,
            portal_ms: (t.portal_time.max(0.0) * 1000.0).round() as u32,
            int_per_real,
            brake: brake_int / int_per_real,
            accel: m.accel.max(0.1) / 2.0 / int_per_real,
            center_y: m.center_y,
            box_depth: (draft - m.center_z + m.height / 2.0).max(0.0),
            cannons: t.cannon_points().count().min(255) as u8,
            racks: t.racks.len().min(255) as u8,
            passengers: t.passenger_seats(),
        }
    }

    /// The terrain height below which a sea cell is deep enough for the hull: 2 m of water for
    /// the small and medium hulls ([`crate::water::NAV_FLOOR_M`]), [`DEEP_FLOOR_M`] for the large.
    pub fn sea_floor_m(&self) -> f32 {
        match self.size {
            HullSize::Large => DEEP_FLOOR_M,
            _ => crate::water::NAV_FLOOR_M,
        }
    }

    /// What the server's summon search needs around the caster for this hull, with `depth` metres
    /// of water required under the footprint (0: any sea cell). See [`crate::water::LaunchReq`].
    pub fn launch_req(&self, depth: f32) -> crate::water::LaunchReq {
        crate::water::LaunchReq {
            ahead: self.spawn_ahead,
            side: self.spawn_side,
            length: self.length,
            beam: self.beam,
            depth,
            margin: 2.0,
        }
    }

    /// Cells of open water (16 m each) a route keeps around the hull: half its length, a third
    /// of the turn radius (a turn cuts the corner) and half a cell, at least 2 and at most 6.
    pub fn route_clear(&self) -> i32 {
        let m = self.length / 2.0 + self.radius * 0.35 + crate::water::CELL_M * 0.5;
        ((m / crate::water::CELL_M).ceil() as i32).clamp(2, 6)
    }

    /// Clearances to try, widest first (the planner falls back to a narrower margin when the
    /// wide one has no route), ending with one cell.
    pub fn route_clears(&self) -> Vec<i32> {
        let mut v: Vec<i32> = (2..=self.route_clear()).rev().collect();
        v.push(1);
        v
    }

    /// Distance in metres the hull needs to stop from `speed` m/s with the brake at `throttle`
    /// (0..=1 of full astern), counting the helm's lag of a third of a second.
    pub fn stopping_m(&self, speed: f32, throttle: f32) -> f32 {
        let a = (self.brake * throttle.clamp(0.05, 1.0)).max(0.01);
        speed * 0.35 + speed * speed / (2.0 * a)
    }
}

/// A kind of boat a bot may be told to sail (`[sail] ship` of the config).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ShipFamily {
    /// The rowboat (slave 15).
    Rowboat,
    /// The clipper, in its variants (52 plain, 76 Adventurer's with a cannon, 14 harpoon).
    Clipper,
    /// The Eznan cutter (slave 21): 8 cannons, 4 pack racks.
    Cutter,
    /// The Lutesong junk (slave 92).
    Junk,
    /// The Growling yawl (slave 24).
    Yawl,
    /// The Merchant Schooner (slave 75): 2 cannons, 20 pack racks.
    Schooner,
}

impl ShipFamily {
    /// Every family.
    pub const ALL: [ShipFamily; 6] = [
        ShipFamily::Rowboat,
        ShipFamily::Clipper,
        ShipFamily::Cutter,
        ShipFamily::Junk,
        ShipFamily::Yawl,
        ShipFamily::Schooner,
    ];

    /// The config name.
    pub fn name(self) -> &'static str {
        match self {
            ShipFamily::Rowboat => "rowboat",
            ShipFamily::Clipper => "clipper",
            ShipFamily::Cutter => "cutter",
            ShipFamily::Junk => "junk",
            ShipFamily::Yawl => "yawl",
            ShipFamily::Schooner => "schooner",
        }
    }

    /// The family with this config name.
    pub fn parse(name: &str) -> Option<ShipFamily> {
        ShipFamily::ALL.into_iter().find(|f| f.name() == name)
    }

    /// The slave templates of the family, the preferred one first.
    pub fn slaves(self) -> &'static [u32] {
        match self {
            ShipFamily::Rowboat => &[15],
            ShipFamily::Clipper => &[52, 76, 14],
            ShipFamily::Cutter => &[21],
            ShipFamily::Junk => &[92],
            ShipFamily::Yawl => &[24],
            ShipFamily::Schooner => &[75],
        }
    }
}

/// An item that summons a boat.
#[derive(Clone, Debug, PartialEq)]
pub struct BoatScroll {
    /// Item template.
    pub item: u32,
    /// English item name.
    pub name: Box<str>,
    /// `items.price` (the vendor sell value, copper; 0 for event / test items).
    pub price: u32,
    /// The skill using the item casts, with its cast time and cooldown.
    pub skill: SkillTiming,
    /// The summoned slave template (`item_summon_slaves`).
    pub slave: u32,
    /// NPC templates whose merchant goods list the item (empty = not sold).
    pub merchants: Box<[u32]>,
}

/// Boat data.
#[derive(Debug, Default)]
pub struct Boats {
    /// Sorted by `(price, item)`.
    scrolls: Vec<BoatScroll>,
    slaves: IdMap<SlaveTemplate>,
}

impl Boats {
    pub(crate) fn load(inp: &TravelInputs<'_>) -> Result<Boats> {
        let conn = inp.conn;

        // Skills with the SpawnSlave effect, and the items that cast them.
        let mut summon_skills: FxHashSet<u32> = FxHashSet::default();
        each_row(
            conn,
            &format!(
                "SELECT DISTINCT sk.skill_id FROM skill_effects sk \
                 JOIN effects e ON e.id = sk.effect_id AND e.actual_type = 'SpecialEffect' \
                 JOIN special_effects se ON se.id = e.actual_id \
                 WHERE se.special_effect_type_id = {SPAWN_SLAVE_EFFECT}"
            ),
            |r| {
                summon_skills.insert(r.u32(0));
            },
        )
        .context("read SpawnSlave skills")?;
        if !summon_skills.contains(&SUMMON_BOAT_SKILL) {
            tracing::warn!("travel: skill {SUMMON_BOAT_SKILL} has no SpawnSlave effect");
        }

        let mut items: Vec<(u32, u32, u32, u32)> = Vec::new(); // item, skill, price, slave
        each_row(
            conn,
            if crate::sql::has_column(conn, "items", "price") {
                "SELECT i.id, i.use_skill_id, i.price, m.slave_id FROM items i \
             JOIN item_summon_slaves m ON m.item_id = i.id WHERE i.use_skill_id != 0 ORDER BY i.id"
            } else {
                "SELECT i.id, i.use_skill_id, \
             COALESCE(gp.price, 0), m.slave_id FROM items i \
             LEFT JOIN (SELECT item_id, MIN(price) AS price, MIN(refund) AS refund FROM item_prices WHERE currency_id = 0 GROUP BY item_id) gp ON gp.item_id = i.id \
             JOIN item_summon_slaves m ON m.item_id = i.id WHERE i.use_skill_id != 0 ORDER BY i.id"
            },
            |r| {
                if summon_skills.contains(&r.u32(1)) {
                    items.push((r.u32(0), r.u32(1), r.u32(2), r.u32(3)));
                }
            },
        )
        .context("read summon scrolls")?;
        items.dedup_by_key(|i| i.0);

        // The slaves they summon, keeping boats only.
        let wanted: FxHashSet<u32> = items.iter().map(|i| i.3).collect();
        let slave_names = localized(conn, "slaves", "name");
        let mut rows: Vec<SlaveRow> = Vec::new();
        let mut slave_kinds: FxHashMap<u32, u8> = FxHashMap::default();
        each_row(
            conn,
            "SELECT id, name, model_id, mountable, level, slave_kind_id, spawn_valid_area_range, \
             spawn_x_offset, spawn_y_offset, portal_time FROM slaves",
            |r| {
                let id = r.u32(0);
                slave_kinds.insert(id, r.u8(5));
                if wanted.contains(&id) && is_boat_kind(r.u8(5)) {
                    rows.push(SlaveRow {
                        id,
                        name: slave_names
                            .get(&id)
                            .map(|n| &**n)
                            .unwrap_or_else(|| r.text(1))
                            .into(),
                        model: r.u32(2),
                        mountable: r.flag(3),
                        level: r.u8(4),
                        kind: r.u8(5),
                        range: r.u16(6),
                        spawn_x: r.f32(7),
                        spawn_y: r.f32(8),
                        portal_time: r.f32(9),
                    });
                }
            },
        )
        .context("read slaves")?;
        let boat_slaves: FxHashSet<u32> = rows.iter().map(|r| r.id).collect();
        let models: FxHashSet<u32> = rows.iter().map(|r| r.model).collect();

        let ship = ship_stats(conn, &models)?;
        let seats = model_seats(conn, &models)?;
        let (mut children, racks) = hull_fittings(conn, &boat_slaves, &slave_kinds)?;
        let mods = SlaveModifiers::load(conn, &boat_slaves)?;
        check_hp_formulas(conn);

        let mut templates = Vec::new();
        for row in rows {
            let m = row.id;
            let level = i32::from(row.level);
            let sta = mods.apply(ATTR_STA, 10 + (level - 1) * 2, m);
            let hp = mods.apply(ATTR_MAX_HEALTH, level * 70 + sta * 12, m);
            templates.push((
                row.id,
                SlaveTemplate {
                    id: row.id,
                    name: row.name,
                    model: row.model,
                    kind: row.kind,
                    level: row.level,
                    max_hp: hp.max(0) as u32,
                    velocity: ship.get(&row.model).map_or(0.0, |s| s.velocity),
                    turn_accel: ship.get(&row.model).map_or(0.0, |s| s.turn_accel),
                    move_speed_mul: mods.move_speed_mul(m),
                    summon_range: row.range,
                    mountable: row.mountable,
                    ship: ship.get(&row.model).map(|s| s.data).unwrap_or_default(),
                    spawn_x: row.spawn_x,
                    spawn_y: row.spawn_y,
                    portal_time: row.portal_time,
                    seats: seats.get(&row.model).copied().unwrap_or(0),
                    children: children.remove(&m).unwrap_or_default().into_boxed_slice(),
                    racks: racks
                        .get(&m)
                        .cloned()
                        .unwrap_or_default()
                        .into_boxed_slice(),
                },
            ));
        }
        let slaves = IdMap::build(templates);

        // Vendors per scroll.
        let scroll_ids: FxHashSet<u32> = items
            .iter()
            .filter(|i| slaves.contains(i.3))
            .map(|i| i.0)
            .collect();
        let mut sold_by: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        each_row(
            conn,
            "SELECT m.npc_id, g.item_id FROM merchants m \
             JOIN merchant_goods g ON g.merchant_pack_id = m.merchant_pack_id",
            |r| {
                if scroll_ids.contains(&r.u32(1)) {
                    sold_by.entry(r.u32(1)).or_default().push(r.u32(0));
                }
            },
        )
        .context("read merchant goods")?;

        let mut scrolls: Vec<BoatScroll> = items
            .iter()
            .filter(|i| slaves.contains(i.3))
            .map(|&(item, skill, price, slave)| {
                let mut merchants = sold_by.remove(&item).unwrap_or_default();
                merchants.sort_unstable();
                merchants.dedup();
                BoatScroll {
                    item,
                    name: inp
                        .items
                        .get(item)
                        .unwrap_or_else(|| inp.texts.get(TextTable::Item, item).unwrap_or(""))
                        .into(),
                    price,
                    skill: SkillTiming::of(inp.skills, skill, "boat summon"),
                    slave,
                    merchants: merchants.into_boxed_slice(),
                }
            })
            .collect();
        scrolls.sort_by_key(|s| (s.price, s.item));
        Ok(Boats { scrolls, slaves })
    }

    /// Boat summon scrolls, cheapest first (ties by item id).
    pub fn scrolls(&self) -> &[BoatScroll] {
        &self.scrolls
    }

    /// A scroll by item template.
    pub fn scroll(&self, item: u32) -> Option<&BoatScroll> {
        self.scrolls.iter().find(|s| s.item == item)
    }

    /// A boat slave template (those some scroll summons).
    pub fn slave_template(&self, id: u32) -> Option<&SlaveTemplate> {
        self.slaves.get(id)
    }

    /// The boat a scroll item summons.
    pub fn boat_for_scroll(&self, item: u32) -> Option<&SlaveTemplate> {
        self.slave_template(self.scroll(item)?.slave)
    }

    /// All boat slave templates in id order.
    pub fn slave_templates(&self) -> &[SlaveTemplate] {
        self.slaves.values()
    }

    /// The cheapest scroll whose boat has a driver seat and a (non-zero) top speed of at least
    /// `min_speed` metres per second.
    pub fn cheapest_scroll(&self, min_speed: f32) -> Option<&BoatScroll> {
        self.scrolls.iter().find(|s| {
            self.slave_template(s.slave).is_some_and(|b| {
                b.has_driver_seat() && b.top_speed() > 0.0 && b.top_speed() >= min_speed
            })
        })
    }

    /// The scroll to summon a boat of `family` with: one the bag holds (`bag_count` says how many
    /// of an item), else the cheapest scroll of the family's preferred slave, else of the next
    /// one. `None` when the data has no scroll of the family or its hull has no driver seat.
    pub fn scroll_for(
        &self,
        family: ShipFamily,
        bag_count: impl Fn(u32) -> i64,
    ) -> Option<&BoatScroll> {
        let drives = |s: &&BoatScroll| {
            family.slaves().contains(&s.slave)
                && self
                    .slave_template(s.slave)
                    .is_some_and(SlaveTemplate::has_driver_seat)
        };
        if let Some(held) = self
            .scrolls
            .iter()
            .filter(drives)
            .find(|s| bag_count(s.item) > 0)
        {
            return Some(held);
        }
        family.slaves().iter().find_map(|&slave| {
            self.scrolls
                .iter()
                .filter(drives)
                .find(|s| s.slave == slave)
        })
    }

    /// Approximate heap bytes held.
    pub fn bytes(&self) -> usize {
        self.scrolls.capacity() * std::mem::size_of::<BoatScroll>()
            + str_bytes(self.scrolls.iter().map(|s| &*s.name))
            + self
                .scrolls
                .iter()
                .map(|s| s.merchants.len() * 4)
                .sum::<usize>()
            + self.slaves.shallow_bytes()
            + self
                .slaves
                .values()
                .iter()
                .map(|s| s.children.len() * std::mem::size_of::<ChildSlave>() + s.racks.len())
                .sum::<usize>()
            + str_bytes(self.slaves.values().iter().map(|s| &*s.name))
    }
}

struct SlaveRow {
    id: u32,
    name: Box<str>,
    model: u32,
    mountable: bool,
    level: u8,
    kind: u8,
    range: u16,
    spawn_x: f32,
    spawn_y: f32,
    portal_time: f32,
}

/// The ship model columns [`Boats::load`] keeps.
#[derive(Clone, Copy)]
struct ShipStats {
    velocity: f32,
    turn_accel: f32,
    data: ShipModelData,
}

/// `model id -> ship_models columns` for the models of the boats (`models.sub_type =
/// 'ShipModel'`, `sub_id` = the ship model).
fn ship_stats(
    conn: &rusqlite::Connection,
    models: &FxHashSet<u32>,
) -> Result<FxHashMap<u32, ShipStats>> {
    let mut ship_of: FxHashMap<u32, u32> = FxHashMap::default();
    each_row(
        conn,
        "SELECT id, sub_id FROM models WHERE sub_type = 'ShipModel'",
        |r| {
            if models.contains(&r.u32(0)) {
                ship_of.insert(r.u32(0), r.u32(1));
            }
        },
    )
    .context("read ship models of boats")?;
    let mut stats_of_ship: FxHashMap<u32, ShipStats> = FxHashMap::default();
    each_row(
        conn,
        // 10.x ship models have no linear/reverse acceleration columns: steer_vel stands in
        if crate::sql::has_column(conn, "ship_models", "accel") {
            "SELECT id, velocity, turn_accel, accel, reverse_accel, reverse_velocity, \
         mass_box_size_x, mass_box_size_y, mass_box_size_z, mass_center_y, mass_center_z, \
         keel_height FROM ship_models"
        } else {
            "SELECT id, velocity, turn_accel, steer_vel, steer_vel, reverse_velocity, \
         mass_box_size_x, mass_box_size_y, mass_box_size_z, mass_center_y, mass_center_z, \
         keel_height FROM ship_models"
        },
        |r| {
            stats_of_ship.insert(
                r.u32(0),
                ShipStats {
                    velocity: r.f32(1),
                    turn_accel: r.f32(2),
                    data: ShipModelData {
                        accel: r.f32(3),
                        reverse_accel: r.f32(4),
                        reverse_velocity: r.f32(5),
                        beam: r.f32(6).abs(),
                        length: r.f32(7).abs(),
                        height: r.f32(8).abs(),
                        center_y: r.f32(9),
                        center_z: r.f32(10),
                        keel: r.f32(11),
                    },
                },
            );
        },
    )
    .context("read ship_models")?;
    Ok(ship_of
        .into_iter()
        .filter_map(|(model, ship)| Some((model, *stats_of_ship.get(&ship)?)))
        .collect())
}

/// The slaves attached to the hulls (`slave_bindings`, with the child's kind from
/// `slave_kinds`) and the attach points of their pack racks (`slave_doodad_bindings`), by hull
/// slave. A compact without the tables just has none.
#[allow(clippy::type_complexity)]
fn hull_fittings(
    conn: &rusqlite::Connection,
    hulls: &FxHashSet<u32>,
    slave_kinds: &FxHashMap<u32, u8>,
) -> Result<(FxHashMap<u32, Vec<ChildSlave>>, FxHashMap<u32, Vec<u8>>)> {
    let mut children: FxHashMap<u32, Vec<ChildSlave>> = FxHashMap::default();
    let _ = each_row(
        conn,
        "SELECT owner_id, slave_id, attach_point_id FROM slave_bindings \
         WHERE owner_type = 'Slave' ORDER BY attach_point_id, id",
        |r| {
            let (owner, child) = (r.u32(0), r.u32(1));
            if hulls.contains(&owner) {
                children.entry(owner).or_default().push(ChildSlave {
                    point: r.u8(2),
                    slave: child,
                    kind: slave_kinds.get(&child).copied().unwrap_or(0),
                });
            }
        },
    );
    let mut racks: FxHashMap<u32, Vec<u8>> = FxHashMap::default();
    let _ = each_row(
        conn,
        &format!(
            "SELECT owner_id, attach_point_id FROM slave_doodad_bindings \
             WHERE owner_type = 'Slave' AND doodad_id = {PACK_RACK_DOODAD} ORDER BY attach_point_id"
        ),
        |r| {
            if hulls.contains(&r.u32(0)) {
                racks.entry(r.u32(0)).or_default().push(r.u8(1));
            }
        },
    );
    Ok((children, racks))
}

/// `model id -> seat mask` from `model_bindings` (attach points 1..=8).
fn model_seats(
    conn: &rusqlite::Connection,
    models: &FxHashSet<u32>,
) -> Result<FxHashMap<u32, u16>> {
    let mut seats: FxHashMap<u32, u16> = FxHashMap::default();
    each_row(
        conn,
        "SELECT owner_id, attach_point_id FROM model_bindings WHERE owner_type = 'Model'",
        |r| {
            let (model, point) = (r.u32(0), r.int(1));
            if models.contains(&model)
                && (i64::from(DRIVER_POINT)..=i64::from(PASSENGER_LAST)).contains(&point)
            {
                *seats.entry(model).or_default() |= 1u16 << point;
            }
        },
    )
    .context("read model_bindings")?;
    Ok(seats)
}

/// The slaves' `unit_modifiers` for Sta, MaxHealth and MoveSpeedMul and the attribute limits.
struct SlaveModifiers {
    /// `(slave, attribute, percent, value)` in table order.
    rows: Vec<(u32, u32, bool, i64)>,
    /// `(attribute, minimum, maximum)`.
    limits: Vec<(u32, i64, i64)>,
}

impl SlaveModifiers {
    fn load(conn: &rusqlite::Connection, slaves: &FxHashSet<u32>) -> Result<SlaveModifiers> {
        let mut rows = Vec::new();
        each_row(
            conn,
            &format!(
                "SELECT owner_id, unit_attribute_id, unit_modifier_type_id, value FROM unit_modifiers \
                 WHERE owner_type = 'Slave' AND unit_attribute_id IN \
                 ({ATTR_STA}, {ATTR_MAX_HEALTH}, {ATTR_MOVE_SPEED_MUL}) ORDER BY id"
            ),
            |r| {
                if slaves.contains(&r.u32(0)) {
                    rows.push((r.u32(0), r.u32(1), r.int(2) == 1, r.int(3)));
                }
            },
        )
        .context("read slave unit_modifiers")?;
        let mut limits = Vec::new();
        each_row(
            conn,
            &format!(
                "SELECT unit_attribute_id, minimum, maximum FROM unit_attribute_limits \
                 WHERE unit_attribute_id IN ({ATTR_STA}, {ATTR_MAX_HEALTH}, {ATTR_MOVE_SPEED_MUL})"
            ),
            |r| limits.push((r.u32(0), r.int(1), r.int(2))),
        )
        .context("read unit_attribute_limits")?;
        Ok(SlaveModifiers { rows, limits })
    }

    fn clamp(&self, attr: u32, value: f64) -> f64 {
        match self.limits.iter().find(|l| l.0 == attr) {
            Some(&(_, min, max)) => value.clamp(min as f64, max as f64),
            None => value,
        }
    }

    /// An integer attribute with the slave's modifiers (the server's `int_bonuses`: in row
    /// order, a percent adds the truncated share, a value adds itself; then the limits).
    fn apply(&self, attr: u32, base: i32, slave: u32) -> i32 {
        let mut res = f64::from(base);
        for &(_, _, percent, value) in self.rows.iter().filter(|r| r.0 == slave && r.1 == attr) {
            if percent {
                res += f64::from((res as f32 * value as f32 / 100.0).trunc());
            } else {
                res += value as f64;
            }
        }
        self.clamp(attr, res) as i32
    }

    /// `CalculateWithBonuses(1000, MoveSpeedMul) / 1000`: values first, then percents.
    fn move_speed_mul(&self, slave: u32) -> f32 {
        let mut res = 1000.0f64;
        let rows = || {
            self.rows
                .iter()
                .filter(move |r| r.0 == slave && r.1 == ATTR_MOVE_SPEED_MUL)
        };
        for &(_, _, _, value) in rows().filter(|r| !r.2) {
            res += value as f64;
        }
        for &(_, _, _, value) in rows().filter(|r| r.2) {
            res += res * value as f64 / 100.0;
        }
        (self.clamp(ATTR_MOVE_SPEED_MUL, res) / 1000.0) as f32
    }
}

/// Warns when the data's slave hit point formulas are not the ones [`Boats::load`] mirrors.
fn check_hp_formulas(conn: &rusqlite::Connection) {
    let mut seen = [false, false];
    let mut other = false;
    let read = each_row(
        conn,
        "SELECT kind_id, formula FROM unit_formulas WHERE owner_type_id = 2 AND kind_id IN (14, 24)",
        |r| {
            let (slot, want) = match r.int(0) {
                14 => (0, MAX_HEALTH_FORMULA),
                _ => (1, STA_FORMULA),
            };
            seen[slot] = true;
            other |= r.text(1).trim() != want;
        },
    );
    if read.is_ok() && (other || seen != [true, true]) {
        tracing::warn!(
            "travel: the slave hit point formulas in unit_formulas differ from the built-in ones; \
             SlaveTemplate::max_hp may be off"
        );
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::doodad::DoodadData;
    use crate::geom::StrTable;
    use crate::npc::NpcTemplate;
    use crate::skill::Skills;
    use crate::spatial::SpawnIndex;
    use crate::text::Texts;

    /// Rowboat (slave 15, model 129: driver 1 + passenger 2), clipper (slave 52, model 128:
    /// driver only), a tank and scrolls for them, one sold by an NPC.
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE skill_effects (id INT, skill_id INT, effect_id INT);
             CREATE TABLE effects (id INT, actual_id INT, actual_type TEXT);
             CREATE TABLE special_effects (id INT, special_effect_type_id INT);
             CREATE TABLE items (id INT, name TEXT, use_skill_id INT, price INT);
             CREATE TABLE item_summon_slaves (id INT, item_id INT, slave_id INT);
             CREATE TABLE slaves (id INT, name TEXT, model_id INT, mountable TEXT, level INT,
               slave_kind_id INT, spawn_valid_area_range INT, spawn_x_offset REAL,
               spawn_y_offset REAL, portal_time REAL);
             CREATE TABLE models (id INT, sub_id INT, sub_type TEXT);
             CREATE TABLE ship_models (id INT, velocity REAL, turn_accel REAL, accel REAL,
               reverse_accel REAL, reverse_velocity REAL, mass_box_size_x REAL,
               mass_box_size_y REAL, mass_box_size_z REAL, mass_center_y REAL,
               mass_center_z REAL, keel_height REAL);
             CREATE TABLE slave_bindings (id INT, owner_id INT, owner_type TEXT, slave_id INT,
               attach_point_id INT);
             CREATE TABLE slave_doodad_bindings (id INT, owner_id INT, owner_type TEXT,
               attach_point_id INT, doodad_id INT);
             CREATE TABLE model_bindings (id INT, owner_id INT, owner_type TEXT, attach_point_id INT);
             CREATE TABLE unit_modifiers (id INT, owner_id INT, owner_type TEXT, unit_attribute_id INT,
               unit_modifier_type_id INT, value INT);
             CREATE TABLE unit_attribute_limits (id INT, unit_attribute_id INT, minimum INT, maximum INT);
             CREATE TABLE unit_formulas (id INT, formula TEXT, kind_id INT, owner_type_id INT);
             CREATE TABLE merchants (id INT, npc_id INT, merchant_pack_id INT);
             CREATE TABLE merchant_goods (id INT, merchant_pack_id INT, item_id INT, grade_id INT);
             CREATE TABLE localized_texts (id INT, tbl_name TEXT, tbl_column_name TEXT, idx INT, en_us TEXT);
             INSERT INTO skill_effects VALUES (1, 15802, 10), (2, 12345, 11);
             INSERT INTO effects VALUES (10, 7, 'SpecialEffect'), (11, 8, 'SpecialEffect');
             INSERT INTO special_effects VALUES (7, 60), (8, 25);
             INSERT INTO items VALUES (17906, 's', 15802, 0), (15817, 's', 15802, 10000), (17626, 's', 15802, 50000),
               (150, 'siege', 15802, 200000), (999, 'x', 12345, 5), (4045, 'book', 11216, 3000),
               (5223, 'cutter', 15802, 1250000);
             INSERT INTO item_summon_slaves VALUES (1, 17906, 15), (2, 15817, 15), (3, 17626, 52), (4, 150, 100), (5, 999, 15),
               (6, 5223, 21);
             INSERT INTO slaves VALUES (15, 'rowboat', 129, 't', 50, 4, 50, 1.5, 1.5, 0),
               (52, 'clipper', 128, 't', 50, 3, 50, 5, 5, 7),
               (21, 'cutter', 393, 't', 50, 2, 50, 30, 20, 7),
               (10, 'cannon', 117, 't', 50, 8, 50, 0, 0, 0),
               (48, 'harpoon', 895, 't', 50, 7, 50, 0, 0, 0),
               (100, 'tank', 5, 't', 50, 5, 50, 0, 0, 0);
             INSERT INTO models VALUES (129, 4, 'ShipModel'), (128, 3, 'ShipModel'),
               (393, 6, 'ShipModel'), (5, 9, 'VehicleModel');
             INSERT INTO ship_models VALUES (4, 6.4, 9.9, 1, 1, 3.5, 1, 3, 0.5, 0, 0, 0.6),
               (3, 13.0, 1.5, 1, 0.5, 5, 4, 8, 7, 0, 3, 0.1),
               (6, 12.6, 1.0, 1, 0.5, 5, 6.5, 30, 14.5, 3, -4, 0.2);
             INSERT INTO slave_bindings VALUES (1, 21, 'Slave', 10, 10), (2, 21, 'Slave', 10, 9),
               (3, 21, 'Slave', 48, 12), (4, 15, 'Item', 10, 9);
             INSERT INTO slave_doodad_bindings VALUES (1, 21, 'Slave', 61, 3446),
               (2, 21, 'Slave', 60, 3446), (3, 21, 'Slave', 59, 2612), (4, 52, 'Slave', 60, 3446);
             INSERT INTO model_bindings VALUES (1, 129, 'Model', 1), (2, 129, 'Model', 2), (3, 129, 'Model', 35),
               (4, 128, 'Model', 1), (5, 128, 'Model', 9), (6, 129, 'Object', 3),
               (7, 393, 'Model', 1);
             INSERT INTO unit_modifiers VALUES (1, 15, 'Slave', 10, 0, 1000), (2, 15, 'Slave', 6, 0, 1420),
               (3, 52, 'Slave', 10, 0, 1000), (4, 52, 'Slave', 6, 0, 2420), (5, 15, 'Item', 10, 0, 7777),
               (6, 52, 'Slave', 10, 1, 50), (7, 21, 'Slave', 10, 0, 1000);
             INSERT INTO unit_attribute_limits VALUES (1, 10, -10000, 3000), (2, 6, -2000000000, 10000000);
             INSERT INTO unit_formulas VALUES (48, '(level * 70 + sta * 12)', 14, 2), (141, '10 + (level - 1) * 2', 24, 2);
             INSERT INTO merchants VALUES (1, 4242, 7), (2, 4243, 7);
             INSERT INTO merchant_goods VALUES (1, 7, 17626, 1), (2, 7, 17626, 1), (3, 8, 15817, 1);
             INSERT INTO localized_texts VALUES (1, 'slaves', 'name', 15, 'Rowboat');",
        )
        .unwrap();
        conn
    }

    fn load(conn: &Connection) -> Boats {
        let dir = std::env::temp_dir();
        let npcs = IdMap::<NpcTemplate>::build(Vec::new());
        let doodads = DoodadData::default();
        let texts = Texts::empty();
        let skills = Skills::default();
        let spawns = SpawnIndex::default();
        let items = StrTable::build(vec![
            (17906, "Scroll: Sharani's Rowboat"),
            (15817, "Scroll: Rowboat"),
            (17626, "Scroll: Clipper"),
        ]);
        let inp = TravelInputs {
            conn,
            texts: &texts,
            server_data: &dir,
            main_world: "main_world",
            npcs: &npcs,
            doodads: &doodads,
            skills: &skills,
            doodad_spawns: &spawns,
            items: &items,
        };
        Boats::load(&inp).unwrap()
    }

    #[test]
    fn scrolls_are_sorted_cheapest_first_and_only_boats_remain() {
        let b = load(&fixture());
        let order: Vec<(u32, u32)> = b.scrolls().iter().map(|s| (s.item, s.price)).collect();
        assert_eq!(
            order,
            vec![
                (17906, 0),
                (15817, 10_000),
                (17626, 50_000),
                (5223, 1_250_000)
            ]
        );
        // the siege weapon scroll, the non-SpawnSlave skill and the book are not boat scrolls
        assert!(b.scroll(150).is_none() && b.scroll(999).is_none() && b.scroll(4045).is_none());
        assert_eq!(&*b.scroll(17906).unwrap().name, "Scroll: Sharani's Rowboat");
        assert_eq!(b.scroll(15817).unwrap().skill.id, 15802);
        assert_eq!(b.boat_for_scroll(15817).map(|s| s.id), Some(15));
        assert_eq!(b.boat_for_scroll(17626).map(|s| s.id), Some(52));
        assert!(b.boat_for_scroll(150).is_none());
        assert!(b.slave_template(100).is_none(), "the tank is not a boat");
        // vendors, deduplicated
        assert_eq!(&*b.scroll(17626).unwrap().merchants, &[4242, 4243]);
        assert!(b.scroll(17906).unwrap().merchants.is_empty());
    }

    #[test]
    fn slave_templates_have_seats_speed_and_hit_points() {
        let b = load(&fixture());
        let row = b.slave_template(15).unwrap();
        assert_eq!(&*row.name, "Rowboat", "the localized name wins");
        assert_eq!(
            (row.model, row.kind, row.level, row.summon_range),
            (129, 4, 50, 50)
        );
        // driver 1 + passenger 2; point 35 and the 'Object' row are not seats
        assert!(row.has_driver_seat());
        assert_eq!(row.passenger_seats(), 1);
        assert_eq!(row.seat_points().collect::<Vec<_>>(), vec![1, 2]);
        // +1000 flat -> MoveSpeedMul 2.0, the Item modifier is not the slave's
        assert_eq!(row.velocity, 6.4);
        assert_eq!(row.turn_accel, 9.9);
        // the rowboat turns slowly: 0.125 rad/s^2 at full helm, settling at 4.35 degrees a second
        let (accel, cap) = row.turn().unwrap();
        assert!((accel - 0.1254).abs() < 0.001, "{accel}");
        assert!((cap.to_degrees() - 4.35).abs() < 1e-3);
        assert_eq!(row.move_speed_mul, 2.0);
        // the body moves at speed * msm / 4 and the speed is capped at velocity * msm / 2: the
        // rowboat does 3.2 m/s on the water, not 6.4
        assert!((row.top_speed() - 3.2).abs() < 1e-5, "{}", row.top_speed());
        // level 50: sta 108, 50 * 70 + 108 * 12 = 4796, + 1420
        assert_eq!(row.max_hp, 6216);

        let clipper = b.slave_template(52).unwrap();
        assert!(clipper.has_driver_seat() && clipper.passenger_seats() == 0);
        // +1000 flat, then +50 percent: 2000 * 1.5 = 3000 (the limit)
        assert_eq!(clipper.move_speed_mul, 3.0);
        // (the fixture's clipper carries a percent modifier too: 3.0 instead of 2.0)
        assert!((clipper.top_speed() - 13.0 * 9.0 / 8.0).abs() < 1e-4);
        assert_eq!(clipper.max_hp, 3500 + 108 * 12 + 2420);
        assert!(row.is_boat() && b.slave_templates().len() == 3);
    }

    #[test]
    fn cheapest_scroll_needs_a_driver_seat_and_speed() {
        let b = load(&fixture());
        assert_eq!(b.cheapest_scroll(0.0).map(|s| s.item), Some(17906));
        assert_eq!(b.cheapest_scroll(7.0).map(|s| s.item), Some(17626));
        assert!(b.cheapest_scroll(100.0).is_none());
        // the rowboat is not 4 m/s fast
        assert_eq!(b.cheapest_scroll(4.0).map(|s| s.item), Some(17626));
        assert!(b.bytes() > 0);
    }

    #[test]
    fn ship_classes_sum_up_the_hulls() {
        let b = load(&fixture());
        let row = b.slave_template(15).unwrap().class();
        assert_eq!(row.size, HullSize::Small);
        assert!((row.top_speed - 3.2).abs() < 1e-5);
        // 4.35 degrees a second: a 42 m radius at top speed
        assert!(
            (row.radius - 3.2 / 4.35f32.to_radians()).abs() < 0.1,
            "{}",
            row.radius
        );
        assert!((row.draft - 0.6).abs() < 1e-6 && row.cannons == 0 && row.racks == 0);
        assert_eq!(
            (row.spawn_ahead, row.portal_ms, row.passengers),
            (1.5, 0, 1)
        );
        assert_eq!(row.route_clear(), 2);
        assert_eq!(row.sea_floor_m(), crate::water::NAV_FLOOR_M);

        let cutter = b.slave_template(21).unwrap();
        let c = cutter.class();
        assert_eq!(c.size, HullSize::Large);
        // msm 2.0: half of the 12.6 of the data, and a 139 m turn at the 2.6 degree cap
        assert!((c.top_speed - 6.3).abs() < 1e-5);
        assert!(
            (c.radius - 6.3 / 2.6f32.to_radians()).abs() < 0.5,
            "{}",
            c.radius
        );
        assert!((c.radius - 139.0).abs() < 1.0);
        assert_eq!((c.beam, c.length, c.height), (6.5, 30.0, 14.5));
        // below the waterline: 4 / 2 + the keel
        assert!((c.draft - 2.2).abs() < 1e-6);
        assert_eq!(
            (c.spawn_ahead, c.spawn_side, c.portal_ms),
            (30.0, 20.0, 7000)
        );
        // the cannon child at points 9 and 10, the harpoon at 12 is not a cannon; two racks
        assert_eq!(cutter.cannon_points().collect::<Vec<_>>(), vec![9, 10]);
        assert_eq!(cutter.children().len(), 3);
        assert_eq!(cutter.rack_points(), &[60, 61]);
        assert_eq!((c.cannons, c.racks, c.passengers), (2, 2, 0));
        assert_eq!(c.sea_floor_m(), DEEP_FLOOR_M);
        assert_eq!(c.route_clear(), 5);
        assert_eq!(c.route_clears(), vec![5, 4, 3, 2, 1]);
        // a stop from full speed takes some seventy metres with the brake at half
        let stop = c.stopping_m(c.top_speed, 0.5);
        assert!((50.0..160.0).contains(&stop), "{stop}");
        assert!(c.stopping_m(c.top_speed, 1.0) < stop);
        // the clipper keeps to the standard sea mask and a narrower margin
        let clipper = b.slave_template(52).unwrap().class();
        assert_eq!(clipper.size, HullSize::Medium);
        assert_eq!(clipper.sea_floor_m(), crate::water::NAV_FLOOR_M);
        assert!(
            (3..=6).contains(&clipper.route_clear()),
            "wider than the rowboat's"
        );
        assert_eq!(clipper.racks, 1);
    }

    #[test]
    fn families_name_their_slaves() {
        for f in ShipFamily::ALL {
            assert_eq!(ShipFamily::parse(f.name()), Some(f));
            assert!(!f.slaves().is_empty());
        }
        assert_eq!(ShipFamily::parse("galleon"), None);
        assert_eq!(ShipFamily::Cutter.slaves(), &[21]);
        let b = load(&fixture());
        assert_eq!(
            b.scroll_for(ShipFamily::Cutter, |_| 0).map(|s| s.item),
            Some(5223)
        );
        assert_eq!(
            b.scroll_for(ShipFamily::Clipper, |_| 0).map(|s| s.item),
            Some(17626)
        );
        // what the bag holds comes first
        assert_eq!(
            b.scroll_for(ShipFamily::Rowboat, |i| i64::from(i == 15817))
                .map(|s| s.item),
            Some(15817)
        );
        assert_eq!(
            b.scroll_for(ShipFamily::Rowboat, |_| 0).map(|s| s.item),
            Some(17906)
        );
        assert!(b.scroll_for(ShipFamily::Schooner, |_| 0).is_none());
    }

    #[test]
    fn boat_kinds_match_the_servers_is_a_boat() {
        for k in [1, 2, 3, 4, 9, 10] {
            assert!(is_boat_kind(k), "{k}");
        }
        for k in [0, 5, 6, 7, 8, 11] {
            assert!(!is_boat_kind(k), "{k}");
        }
    }
}
