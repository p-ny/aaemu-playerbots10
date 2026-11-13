//! Gear facts: what every equippable item is, which lunagems fit where, how many sockets an item
//! has, how likely a socket works, which merchants sell which gear, the regrade scrolls and the
//! gear a quest lets the player choose.
//!
//! The gear feature of the bots (equip the best piece, buy or mint the tier the level calls for,
//! socket lunagems, regrade) reads this. It is loaded with the world and kept compact: one
//! [`GearItem`] of 8 bytes per equippable template in an [`IdMap`] (about 8 500 templates in the
//! 1.2 data, some 124 KB with the rest), a few small tables, and the merchant goods that are gear
//! or gems.
//!
//! Sources (the server's `compact.sqlite3`):
//!
//! * `item_armors`, `item_weapons` (+ `holdables`), `item_accessories`: the equip slot type and
//!   the kind (the armor's wearable kind, the weapon's holdable id); `items`: level, level
//!   requirement, fixed grade, gender, whether it can be graded and regraded.
//! * The lunagems: items whose use skill has the `ItemSocketing` special effect (type 106) with
//!   `value1 != 0` (0 is the dawnstone, which takes every gem out); `item_sockets` names the equip
//!   slot group a gem fits (`equip_slot_group_maps`: the slot types of a group);
//!   `item_socket_level_limits` the lowest item level it goes into.
//! * `item_socket_num_limits` (sockets by slot type and grade), `item_socket_chances` (the chance
//!   of the next socket by the number already in the item; the server rolls it, a failed roll takes
//!   every gem out), `item_grades` (order, stat multiplier, regrade odds).
//! * The regrade scrolls: items whose use skill has the `GradeEnchant` special effect (type 92),
//!   the plain ones (`value1 == 0`, `value2 == 0`) by item class (`value3`: 1 weapon, 2 armor,
//!   24 accessory).
//! * `merchants` and `merchant_goods`: which merchant NPC (with a spawn in the world) sells a
//!   piece of gear or a gem for copper, at which grade (the grade the server normalizes the row to:
//!   the fixed grade, else the row's grade for a gradable item, else common).
//! * `quest_act_supply_selective_items`: the rewards a quest lets the player choose, in the order
//!   of the 1-based index the client sends (components by kind and id, acts by id: the server's
//!   `UpdateQuestComponentActs`).
//!
//! The server checks none of the rules a client keeps (level requirement, gem fit, socket count):
//! a bot must check them itself, from these tables.

use anyhow::{Context, Result};
use rusqlite::Connection;
use rustc_hash::FxHashMap;

use crate::geom::IdMap;
use crate::npc::NpcTemplate;
use crate::sql::{each_row, table_exists, RowExt};

/// `EquipmentItemSlotType`: the slot type of a piece (`allowed_gear_slots` on the server).
pub mod slot_type {
    /// Helmet.
    pub const HEAD: u8 = 1;
    /// Necklace.
    pub const NECK: u8 = 2;
    /// Shirt.
    pub const CHEST: u8 = 3;
    /// Belt.
    pub const WAIST: u8 = 4;
    /// Pants.
    pub const LEGS: u8 = 5;
    /// Gloves.
    pub const HANDS: u8 = 6;
    /// Boots.
    pub const FEET: u8 = 7;
    /// Sleeves.
    pub const ARMS: u8 = 8;
    /// Cloak.
    pub const BACK: u8 = 9;
    /// Earring (either ear).
    pub const EAR: u8 = 10;
    /// Ring (either finger).
    pub const FINGER: u8 = 11;
    /// Undershirt (costume).
    pub const UNDERSHIRT: u8 = 12;
    /// Underpants (costume).
    pub const UNDERPANTS: u8 = 13;
    /// One-handed weapon for the main hand only (maces, scepters, short spears).
    pub const MAINHAND: u8 = 14;
    /// Off-hand only.
    pub const OFFHAND: u8 = 15;
    /// Two-handed weapon (main hand, empties the off hand).
    pub const TWO_HANDED: u8 = 16;
    /// One-handed weapon for either hand.
    pub const ONE_HANDED: u8 = 17;
    /// Bow.
    pub const RANGED: u8 = 18;
    /// Shield (off hand).
    pub const SHIELD: u8 = 20;
    /// Instrument.
    pub const INSTRUMENT: u8 = 21;
    /// Costume.
    pub const COSPLAY: u8 = 31;
}

/// `EquipmentItemSlot`: the slots of the equipment container (0..28).
pub mod equip_slot {
    /// Head.
    pub const HEAD: u8 = 0;
    /// Neck.
    pub const NECK: u8 = 1;
    /// Chest.
    pub const CHEST: u8 = 2;
    /// Waist.
    pub const WAIST: u8 = 3;
    /// Legs.
    pub const LEGS: u8 = 4;
    /// Hands.
    pub const HANDS: u8 = 5;
    /// Feet.
    pub const FEET: u8 = 6;
    /// Arms.
    pub const ARMS: u8 = 7;
    /// Back (cloak).
    pub const BACK: u8 = 8;
    /// First earring.
    pub const EAR1: u8 = 9;
    /// Second earring.
    pub const EAR2: u8 = 10;
    /// First ring.
    pub const FINGER1: u8 = 11;
    /// Second ring.
    pub const FINGER2: u8 = 12;
    /// Main hand.
    pub const MAINHAND: u8 = 15;
    /// Off hand.
    pub const OFFHAND: u8 = 16;
    /// Ranged (bow).
    pub const RANGED: u8 = 17;
    /// Instrument.
    pub const MUSICAL: u8 = 18;
    /// The number of slots of the container.
    pub const COUNT: u8 = 28;
}

/// The equipment slots a piece of slot type `slot_type` may go into (the server's
/// `GetAllowedGearSlots`, `models/container.rs`); empty for the slot types bots leave alone
/// (costumes, appearance parts, the backpack).
pub fn allowed_slots(slot_type: u8) -> &'static [u8] {
    use equip_slot::*;
    match slot_type {
        1 => &[HEAD],
        2 => &[NECK],
        3 => &[CHEST],
        4 => &[WAIST],
        5 => &[LEGS],
        6 => &[HANDS],
        7 => &[FEET],
        8 => &[ARMS],
        9 => &[BACK],
        10 => &[EAR1, EAR2],
        11 => &[FINGER1, FINGER2],
        14 | 16 => &[MAINHAND],
        15 | 20 => &[OFFHAND],
        17 => &[MAINHAND, OFFHAND],
        18 => &[RANGED],
        21 => &[MUSICAL],
        _ => &[],
    }
}

/// The name of an equipment slot for logs and the web UI.
pub fn slot_name(slot: u8) -> &'static str {
    match slot {
        0 => "head",
        1 => "neck",
        2 => "chest",
        3 => "waist",
        4 => "legs",
        5 => "hands",
        6 => "feet",
        7 => "arms",
        8 => "back",
        9 => "ear1",
        10 => "ear2",
        11 => "finger1",
        12 => "finger2",
        13 => "undershirt",
        14 => "underpants",
        15 => "mainhand",
        16 => "offhand",
        17 => "ranged",
        18 => "instrument",
        26 => "backpack",
        27 => "costume",
        _ => "other",
    }
}

/// The armor kinds (`wearable_kinds`, the armor's `type_id`).
pub mod armor_kind {
    /// Cloth.
    pub const CLOTH: u8 = 1;
    /// Leather.
    pub const LEATHER: u8 = 2;
    /// Plate.
    pub const PLATE: u8 = 3;
    /// Pet and mount gear (worn by a mate, not a character).
    pub const PET: u8 = 4;
    /// Costume pieces and underwear.
    pub const COSTUME: u8 = 5;
}

/// The weapon kinds as `holdables` ids (1.2): what a build's weapon names stand for.
pub mod holdable {
    /// Daggers.
    pub const DAGGER: &[u8] = &[2, 4];
    /// One-handed swords.
    pub const SWORD: &[u8] = &[1, 5, 6];
    /// Katanas (one-handed blades).
    pub const KATANA: &[u8] = &[7, 9, 25];
    /// One-handed axes.
    pub const AXE: &[u8] = &[10];
    /// Clubs (one-handed maces).
    pub const CLUB: &[u8] = &[12];
    /// Scepters (one-handed staves).
    pub const SCEPTER: &[u8] = &[14, 18];
    /// Short spears.
    pub const SHORTSPEAR: &[u8] = &[15, 26];
    /// Greatswords.
    pub const GREATSWORD: &[u8] = &[3];
    /// Nodachis (two-handed blades).
    pub const NODACHI: &[u8] = &[8];
    /// Greataxes.
    pub const GREATAXE: &[u8] = &[11];
    /// Greatclubs.
    pub const GREATCLUB: &[u8] = &[13];
    /// Long spears.
    pub const LONGSPEAR: &[u8] = &[16];
    /// Staves (two-handed).
    pub const STAFF: &[u8] = &[17];
    /// Bows.
    pub const BOW: &[u8] = &[19];
    /// Shields.
    pub const SHIELD: &[u8] = &[24];
    /// Instruments.
    pub const INSTRUMENT: &[u8] = &[21, 22, 23];

    /// The holdable ids of a weapon kind name as the class builds write it (`"Sword"`,
    /// `"Nodachi"`, ...; case does not matter). Empty for an unknown name.
    pub fn of_name(name: &str) -> &'static [u8] {
        let name = name.trim();
        NAMES
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map_or(&[], |(_, ids)| ids)
    }

    /// The weapon kind names and their holdables. Longer names first, so a word search in an
    /// item name finds "Greatsword" before "Sword".
    pub const NAMES: &[(&str, &[u8])] = &[
        ("greatsword", GREATSWORD),
        ("greatclub", GREATCLUB),
        ("greataxe", GREATAXE),
        ("longspear", LONGSPEAR),
        ("shortspear", SHORTSPEAR),
        ("nodachi", NODACHI),
        ("scepter", SCEPTER),
        ("katana", KATANA),
        ("dagger", DAGGER),
        ("shield", SHIELD),
        ("sword", SWORD),
        ("staff", STAFF),
        ("spear", SHORTSPEAR),
        ("club", CLUB),
        ("axe", AXE),
        ("bow", BOW),
        ("lute", INSTRUMENT),
        ("flute", INSTRUMENT),
        ("drum", INSTRUMENT),
    ];
}

/// The class of a piece of gear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GearClass {
    /// Worn armor (`item_armors`): head, chest, waist, legs, hands, feet, arms, back.
    Armor,
    /// A weapon, shield or instrument (`item_weapons`).
    Weapon,
    /// Necklace, earrings, rings (`item_accessories`).
    Accessory,
}

/// `items.fixed_grade` of an item that has none.
pub const NO_GRADE: u8 = u8::MAX;
/// `item_grades.id` of the common grade (the grade a merchant sells most gear at).
pub const GRADE_COMMON: u8 = 0;
/// The highest grade id (mythic).
pub const MAX_GRADE: u8 = 11;

/// The English name of a grade id (`item_grades.comments`).
pub fn grade_name(grade: u8) -> &'static str {
    match grade {
        0 => "common",
        1 => "poor",
        2 => "uncommon",
        3 => "rare",
        4 => "ancient",
        5 => "heroic",
        6 => "unique",
        7 => "artifact",
        8 => "wonder",
        9 => "epic",
        10 => "legendary",
        11 => "mythic",
        _ => "?",
    }
}

/// [`GearItem::flags`]: the item gets a grade when it is created (`items.gradable`).
pub const F_GRADABLE: u8 = 1;
/// [`GearItem::flags`]: a regrade scroll works on it (`items.grade_enchantable`).
pub const F_ENCHANTABLE: u8 = 2;

/// What a bot needs to know about one equippable template. 8 bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GearItem {
    /// The slot type ([`slot_type`]).
    pub slot_type: u8,
    /// Armor, weapon or accessory.
    pub class: GearClass,
    /// The armor's wearable kind ([`armor_kind`]; accessories have their own kinds) or the
    /// weapon's holdable id ([`holdable`]).
    pub kind: u8,
    /// `items.LEVEL` (the item level; up to 62 in 1.2).
    pub level: u8,
    /// `items.level_requirement`; 0 when the item has none.
    pub req: u8,
    /// `items.fixed_grade`, [`NO_GRADE`] when the grade is rolled at creation.
    pub fixed_grade: u8,
    /// `items.char_gender_id`: 0 any, 1 male, 2 female.
    pub gender: u8,
    /// [`F_GRADABLE`], [`F_ENCHANTABLE`].
    pub flags: u8,
}

impl GearItem {
    /// Whether a character of `level` and `gender` may wear it: the level requirement is met (the
    /// server does not check it), and an item without a requirement is at most two levels above
    /// the character (what a player would wear), and the gender fits.
    pub fn wearable_by(&self, level: u8, gender: u8) -> bool {
        let level_ok = if self.req > 0 {
            self.req <= level
        } else {
            self.level <= level.saturating_add(2)
        };
        level_ok && (self.gender == 0 || gender == 0 || self.gender == gender)
    }

    /// The equipment slots it goes into ([`allowed_slots`]).
    pub fn slots(&self) -> &'static [u8] {
        allowed_slots(self.slot_type)
    }

    /// Whether it takes both hands.
    pub fn two_handed(&self) -> bool {
        self.slot_type == slot_type::TWO_HANDED
    }

    /// The grade an item of this template has when it is made at `grade`: its fixed grade if it
    /// has one, `grade` if it is gradable, else common (the server's normalization).
    pub fn grade_at(&self, grade: u8) -> u8 {
        if self.fixed_grade != NO_GRADE {
            self.fixed_grade
        } else if self.flags & F_GRADABLE != 0 {
            grade
        } else {
            GRADE_COMMON
        }
    }

    /// Whether a regrade scroll works on it.
    pub fn enchantable(&self) -> bool {
        self.flags & F_ENCHANTABLE != 0
    }

    /// Whether a character wears it as gear: weapons and accessories, and armor of cloth, leather
    /// or plate (pet gear and costumes have armor rows too).
    pub fn character_gear(&self) -> bool {
        self.class != GearClass::Armor
            || matches!(
                self.kind,
                armor_kind::CLOTH | armor_kind::LEATHER | armor_kind::PLATE
            )
    }
}

/// A lunagem: where it fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Lunagem {
    /// Bit `t` is set when the gem fits slot type `t` (`equip_slot_group_maps` of its group).
    pub slot_types: u32,
    /// The lowest item level it goes into (`item_socket_level_limits`, 0 for none).
    pub min_level: u8,
}

impl Lunagem {
    /// Whether the gem goes into an item of `slot_type` and item level `level`.
    pub fn fits(&self, slot_type: u8, level: u8) -> bool {
        slot_type < 32 && self.slot_types & (1 << slot_type) != 0 && level >= self.min_level
    }
}

/// The facts of one grade (`item_grades`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GradeFacts {
    /// `grade_order`: poor 0, common 1, uncommon 2 ... mythic 11.
    pub order: u8,
    /// `stat_multiplier` in percent (common 100, poor 80, mythic 200).
    pub stat_pct: u16,
    /// The chance a regrade works, per 10 000.
    pub regrade_success: u16,
    /// The chance a failed regrade destroys the item, per 10 000.
    pub regrade_break: u16,
    /// The chance a failed regrade lowers the grade, per 10 000.
    pub regrade_downgrade: u16,
}

/// One piece of gear (or a gem) a merchant sells for copper.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SoldGear {
    /// The item template.
    pub item: u32,
    /// The grade the merchant sells it at (send it in `CSBuyItems`).
    pub grade: u8,
    /// The merchant NPC templates (with a spawn in the world) that sell it.
    pub npcs: Box<[u32]>,
}

/// The kinds of regrade scroll, by the item class they work on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RegradeScrolls {
    /// For weapons (`value3` 1).
    pub weapon: u32,
    /// For armor (`value3` 2).
    pub armor: u32,
    /// For accessories (`value3` 24).
    pub accessory: u32,
}

impl RegradeScrolls {
    /// The scroll for a piece of `class`; 0 when the data has none.
    pub fn of(&self, class: GearClass) -> u32 {
        match class {
            GearClass::Weapon => self.weapon,
            GearClass::Armor => self.armor,
            GearClass::Accessory => self.accessory,
        }
    }
}

/// `special_effects.special_effect_type_id` of `GradeEnchant` (regrade scrolls).
const SE_GRADE_ENCHANT: u32 = 92;
/// `special_effects.special_effect_type_id` of `ItemSocketing` (lunagems, the dawnstone).
const SE_ITEM_SOCKETING: u32 = 106;
/// Slot types the socket tables cover.
const SLOT_TYPES: usize = 32;
/// Grade ids the socket tables cover (0..=11).
const GRADES: usize = 12;

/// The gear facts of the world. See the module docs.
#[derive(Debug, Default)]
pub struct GearData {
    items: IdMap<GearItem>,
    gems: IdMap<Lunagem>,
    /// Sockets by slot type and grade id.
    sockets: Vec<[u8; GRADES]>,
    /// The chance of the next socket, per 10 000, by the number of gems already in the item.
    chances: [u16; 8],
    grades: [GradeFacts; GRADES],
    /// Sorted by item, then grade.
    sold: Vec<SoldGear>,
    scrolls: RegradeScrolls,
    rewards: FxHashMap<u32, Box<[u32]>>,
}

impl GearData {
    /// Reads the gear facts. `npcs` and `has_spawn` decide which merchants count: an NPC template
    /// with the merchant role and at least one spawn. A database without the gear tables (a test
    /// fixture) gives empty facts.
    pub fn load(
        conn: &Connection,
        npcs: &IdMap<NpcTemplate>,
        has_spawn: &dyn Fn(u32) -> bool,
    ) -> Result<GearData> {
        let mut g = GearData {
            sockets: vec![[0; GRADES]; SLOT_TYPES],
            ..GearData::default()
        };
        if !table_exists(conn, "items")? {
            return Ok(g);
        }
        g.items = load_items(conn).context("read the equippable items")?;
        g.gems = load_gems(conn).context("read the lunagems")?;
        if table_exists(conn, "item_socket_num_limits")? {
            each_row(
                conn,
                "SELECT slot_id, grade_id, num_socket FROM item_socket_num_limits",
                |r| {
                    let (slot, grade) = (r.u32(0) as usize, r.u32(1) as usize);
                    if slot < SLOT_TYPES && grade < GRADES {
                        g.sockets[slot][grade] = r.u8(2);
                    }
                },
            )
            .context("read item_socket_num_limits")?;
        }
        if table_exists(conn, "item_socket_chances")? {
            each_row(
                conn,
                // 10.x: one row per chance table, socket0..socket9 = the chance of the n-th socket
                if crate::sql::has_column(conn, "item_socket_chances", "num_sockets") {
                "SELECT num_sockets, success_ratio FROM item_socket_chances"
            } else {
                "SELECT n, ratio FROM (SELECT 1 AS n, socket0 AS ratio FROM item_socket_chances WHERE id = 1 \
                 UNION ALL SELECT 2, socket1 FROM item_socket_chances WHERE id = 1 \
                 UNION ALL SELECT 3, socket2 FROM item_socket_chances WHERE id = 1 \
                 UNION ALL SELECT 4, socket3 FROM item_socket_chances WHERE id = 1 \
                 UNION ALL SELECT 5, socket4 FROM item_socket_chances WHERE id = 1 \
                 UNION ALL SELECT 6, socket5 FROM item_socket_chances WHERE id = 1 \
                 UNION ALL SELECT 7, socket6 FROM item_socket_chances WHERE id = 1 \
                 UNION ALL SELECT 8, socket7 FROM item_socket_chances WHERE id = 1 \
                 UNION ALL SELECT 9, socket8 FROM item_socket_chances WHERE id = 1)"
            },
                |r| {
                    // keyed by the socket being made (1 = the first gem)
                    let n = r.u32(0) as usize;
                    if (1..=g.chances.len()).contains(&n) {
                        g.chances[n - 1] = r.u16(1);
                    }
                },
            )
            .context("read item_socket_chances")?;
        }
        if table_exists(conn, "item_grades")? {
            each_row(
                conn,
                // 10.x keeps the regrade odds per grade in item_enchant_ratios (group 1 = gear)
                if crate::sql::has_column(conn, "item_grades", "grade_enchant_success_ratio") {
                "SELECT id, grade_order, stat_multiplier, grade_enchant_success_ratio, \
                 grade_enchant_break_ratio, grade_enchant_downgrade_ratio FROM item_grades"
            } else {
                "SELECT g.id, g.grade_order, g.stat_multiplier, \
                 COALESCE(r.grade_enchant_success_ratio, 0), COALESCE(r.grade_enchant_break_ratio, 0), \
                 COALESCE(r.grade_enchant_downgrade_ratio, 0) FROM item_grades g \
                 LEFT JOIN item_enchant_ratios r ON r.grade = g.id AND r.item_enchant_ratio_group_id = 1"
            },
                |r| {
                    let id = r.u32(0) as usize;
                    if id < GRADES {
                        g.grades[id] = GradeFacts {
                            order: r.u8(1),
                            stat_pct: r.u16(2),
                            regrade_success: r.u16(3),
                            regrade_break: r.u16(4),
                            regrade_downgrade: r.u16(5),
                        };
                    }
                },
            )
            .context("read item_grades")?;
        }
        g.scrolls = load_scrolls(conn).context("read the regrade scrolls")?;
        g.sold = load_sold(conn, &g.items, &g.gems, npcs, has_spawn)
            .context("read the gear merchants")?;
        g.rewards = load_rewards(conn).context("read the selective quest rewards")?;
        Ok(g)
    }

    /// The facts of an equippable template; `None` for anything that is not gear.
    pub fn item(&self, template: u32) -> Option<&GearItem> {
        self.items.get(template)
    }

    /// Every equippable template, in ascending id order.
    pub fn items(&self) -> impl Iterator<Item = (u32, &GearItem)> {
        self.items.iter()
    }

    /// The number of equippable templates.
    pub fn len(&self) -> usize {
        self.items.len()
    }

    /// Whether there are no gear facts (a database without the gear tables).
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// The lunagem `template`, `None` for anything else.
    pub fn gem(&self, template: u32) -> Option<&Lunagem> {
        self.gems.get(template)
    }

    /// Every lunagem, in ascending id order.
    pub fn gems(&self) -> impl Iterator<Item = (u32, &Lunagem)> {
        self.gems.iter()
    }

    /// How many sockets an item of `slot_type` and grade `grade` has.
    pub fn sockets(&self, slot_type: u8, grade: u8) -> u8 {
        self.sockets
            .get(usize::from(slot_type))
            .and_then(|row| row.get(usize::from(grade)))
            .copied()
            .unwrap_or(0)
    }

    /// The chance, per 10 000, that the next gem goes into an item that holds `held` gems (the
    /// first one always works in 1.2). A failure takes every gem out.
    pub fn socket_chance(&self, held: u8) -> u16 {
        self.chances.get(usize::from(held)).copied().unwrap_or(0)
    }

    /// The facts of grade id `grade`.
    pub fn grade(&self, grade: u8) -> GradeFacts {
        self.grades
            .get(usize::from(grade))
            .copied()
            .unwrap_or_default()
    }

    /// The stat multiplier of a grade as a factor (common 1.0); 1.0 for an unknown grade.
    pub fn grade_factor(&self, grade: u8) -> f32 {
        match self.grade(grade).stat_pct {
            0 => 1.0,
            pct => f32::from(pct) / 100.0,
        }
    }

    /// All gear and gems merchants sell for copper, by item then grade.
    pub fn sold(&self) -> &[SoldGear] {
        &self.sold
    }

    /// The offers of item `template` (one per grade).
    pub fn sold_of(&self, template: u32) -> &[SoldGear] {
        let lo = self.sold.partition_point(|s| s.item < template);
        let hi = self.sold.partition_point(|s| s.item <= template);
        &self.sold[lo..hi]
    }

    /// The regrade scrolls.
    pub fn scrolls(&self) -> RegradeScrolls {
        self.scrolls
    }

    /// The rewards quest `quest` lets the player choose, in the order of the 1-based index sent
    /// with `CSCompleteQuestContext`; empty when it has no choice.
    pub fn quest_rewards(&self, quest: u32) -> &[u32] {
        self.rewards.get(&quest).map_or(&[], |v| &v[..])
    }

    /// Heap bytes held (for the world's memory report).
    pub fn bytes(&self) -> usize {
        self.items.shallow_bytes()
            + self.gems.shallow_bytes()
            + self.sockets.capacity() * GRADES
            + self
                .sold
                .iter()
                .map(|s| std::mem::size_of::<SoldGear>() + s.npcs.len() * 4)
                .sum::<usize>()
            + self
                .rewards
                .values()
                .map(|v| 16 + v.len() * 4)
                .sum::<usize>()
    }
}

/// The equippable templates of the three item class tables, with their `items` columns.
fn load_items(conn: &Connection) -> Result<IdMap<GearItem>> {
    // 1.2 has a separate regrade flag; 10.x regrades every gradable item
    let regrade = if crate::sql::has_column(conn, "items", "grade_enchantable") {
        "i.grade_enchantable"
    } else {
        "i.gradable"
    };
    let cols = format!(
        "i.LEVEL, i.level_requirement, i.fixed_grade, i.char_gender_id, i.gradable, {regrade}"
    );
    let sources: [(&str, String, GearClass); 3] = [
        (
            "item_armors",
            format!(
                "SELECT x.item_id, x.slot_type_id, x.type_id, {cols} FROM item_armors x \
                 JOIN items i ON i.id = x.item_id"
            ),
            GearClass::Armor,
        ),
        (
            "item_weapons",
            format!(
                "SELECT x.item_id, h.slot_type_id, x.holdable_id, {cols} FROM item_weapons x \
                 JOIN holdables h ON h.id = x.holdable_id JOIN items i ON i.id = x.item_id"
            ),
            GearClass::Weapon,
        ),
        (
            "item_accessories",
            format!(
                "SELECT x.item_id, x.slot_type_id, x.type_id, {cols} FROM item_accessories x \
                 JOIN items i ON i.id = x.item_id"
            ),
            GearClass::Accessory,
        ),
    ];
    let mut out: Vec<(u32, GearItem)> = Vec::new();
    for (table, sql, class) in sources {
        if !table_exists(conn, table)?
            || (class == GearClass::Weapon && !table_exists(conn, "holdables")?)
        {
            continue;
        }
        each_row(conn, &sql, |r| {
            let fixed = r.int(5);
            let mut flags = 0;
            if r.flag(7) {
                flags |= F_GRADABLE;
            }
            if r.flag(8) {
                flags |= F_ENCHANTABLE;
            }
            out.push((
                r.u32(0),
                GearItem {
                    slot_type: r.u8(1),
                    class,
                    kind: r.u8(2),
                    level: r.u8(3),
                    req: r.u8(4),
                    fixed_grade: if (0..=i64::from(MAX_GRADE)).contains(&fixed) {
                        fixed as u8
                    } else {
                        NO_GRADE
                    },
                    gender: r.u8(6),
                    flags,
                },
            ));
        })
        .with_context(|| format!("read {table}"))?;
    }
    Ok(IdMap::build(out))
}

/// The items whose use skill has a special effect of `kind`, with the effect's values:
/// `(item, value1, value2, value3)`.
fn items_with_effect(conn: &Connection, kind: u32) -> Result<Vec<(u32, i64, i64, i64)>> {
    for t in ["special_effects", "effects", "skill_effects"] {
        if !table_exists(conn, t)? {
            return Ok(Vec::new());
        }
    }
    let mut out = Vec::new();
    each_row(
        conn,
        &format!(
            "SELECT DISTINCT i.id, se.value1, se.value2, se.value3 FROM special_effects se \
             JOIN effects e ON e.actual_id = se.id AND e.actual_type = 'SpecialEffect' \
             JOIN skill_effects s ON s.effect_id = e.id \
             JOIN items i ON i.use_skill_id = s.skill_id \
             WHERE se.special_effect_type_id = {kind} ORDER BY i.id"
        ),
        |r| out.push((r.u32(0), r.int(1), r.int(2), r.int(3))),
    )?;
    Ok(out)
}

/// The lunagems: socketing items (`value1 != 0`) with a slot group.
fn load_gems(conn: &Connection) -> Result<IdMap<Lunagem>> {
    if !table_exists(conn, "item_sockets")? || !table_exists(conn, "equip_slot_group_maps")? {
        return Ok(IdMap::default());
    }
    let mut group_types: FxHashMap<u32, u32> = FxHashMap::default();
    each_row(
        conn,
        "SELECT equip_slot_group_id, equip_slot_type_id FROM equip_slot_group_maps",
        |r| {
            let t = r.u32(1);
            if t < 32 {
                *group_types.entry(r.u32(0)).or_insert(0) |= 1 << t;
            }
        },
    )?;
    let mut group_of: FxHashMap<u32, u32> = FxHashMap::default();
    each_row(
        conn,
        "SELECT item_id, equip_slot_group_id FROM item_sockets",
        |r| {
            if r.u32(1) != 0 {
                group_of.insert(r.u32(0), r.u32(1));
            }
        },
    )?;
    let mut min_level: FxHashMap<u32, u8> = FxHashMap::default();
    if table_exists(conn, "item_socket_level_limits")? {
        each_row(
            conn,
            "SELECT item_id, LEVEL FROM item_socket_level_limits",
            |r| {
                min_level.insert(r.u32(0), r.u8(1));
            },
        )?;
    }
    let mut out = Vec::new();
    for (item, value1, _, _) in items_with_effect(conn, SE_ITEM_SOCKETING)? {
        if value1 == 0 {
            continue; // the dawnstone takes the gems out
        }
        let Some(types) = group_of.get(&item).and_then(|g| group_types.get(g)) else {
            continue;
        };
        out.push((
            item,
            Lunagem {
                slot_types: *types,
                min_level: min_level.get(&item).copied().unwrap_or(0),
            },
        ));
    }
    Ok(IdMap::build(out))
}

/// The plain regrade scroll of each item class (the lowest item id wins).
fn load_scrolls(conn: &Connection) -> Result<RegradeScrolls> {
    let mut s = RegradeScrolls::default();
    for (item, lucky, event, class) in items_with_effect(conn, SE_GRADE_ENCHANT)? {
        if lucky != 0 || event != 0 {
            continue;
        }
        let slot = match class {
            1 => &mut s.weapon,
            2 => &mut s.armor,
            24 => &mut s.accessory,
            _ => continue,
        };
        if *slot == 0 {
            *slot = item;
        }
    }
    Ok(s)
}

/// The gear and gems merchants sell for copper (a price, no honor or vocation price), by the NPC
/// templates that have the merchant role and a spawn. The last `merchants` row of an NPC wins,
/// like the server's loader.
fn load_sold(
    conn: &Connection,
    items: &IdMap<GearItem>,
    gems: &IdMap<Lunagem>,
    npcs: &IdMap<NpcTemplate>,
    has_spawn: &dyn Fn(u32) -> bool,
) -> Result<Vec<SoldGear>> {
    for t in ["merchants", "merchant_goods"] {
        if !table_exists(conn, t)? {
            return Ok(Vec::new());
        }
    }
    let mut pack_of_npc: FxHashMap<u32, u32> = FxHashMap::default();
    each_row(
        conn,
        "SELECT npc_id, merchant_pack_id FROM merchants",
        |r| {
            pack_of_npc.insert(r.u32(0), r.u32(1));
        },
    )?;
    let mut npcs_of_pack: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    for (&npc, &pack) in &pack_of_npc {
        if npcs.get(npc).is_some_and(NpcTemplate::is_merchant) && has_spawn(npc) {
            npcs_of_pack.entry(pack).or_default().push(npc);
        }
    }
    // the copper price of the candidates (no honor or vocation price)
    let mut copper: FxHashMap<u32, bool> = FxHashMap::default();
    each_row(
        conn,
        // 10.x: prices per currency in item_prices (0 = gold; other currencies count as honor or
        // living points)
        if crate::sql::has_column(conn, "items", "honor_price") {
            "SELECT id, price, honor_price, living_point_price FROM items"
        } else {
            "SELECT i.id, COALESCE(gp.price, 0), COALESCE(op.price, 0), 0 FROM items i \
         LEFT JOIN (SELECT item_id, MIN(price) AS price, MIN(refund) AS refund FROM item_prices WHERE currency_id = 0 GROUP BY item_id) gp ON gp.item_id = i.id \
         LEFT JOIN (SELECT item_id, MAX(price) AS price FROM item_prices WHERE currency_id != 0 \
         GROUP BY item_id) op ON op.item_id = i.id"
        },
        |r| {
            let id = r.u32(0);
            if items.contains(id) || gems.contains(id) {
                copper.insert(id, r.int(1) > 0 && r.int(2) <= 0 && r.int(3) <= 0);
            }
        },
    )?;
    let mut offers: FxHashMap<(u32, u8), Vec<u32>> = FxHashMap::default();
    each_row(
        conn,
        "SELECT merchant_pack_id, item_id, grade_id FROM merchant_goods",
        |r| {
            let item = r.u32(1);
            if !copper.get(&item).copied().unwrap_or(false) {
                return;
            }
            let Some(sellers) = npcs_of_pack.get(&r.u32(0)) else {
                return;
            };
            // the grade the server sells the row at (`sells_item_at_grade`)
            let grade = match items.get(item) {
                Some(g) => g.grade_at(r.u8(2)),
                None => GRADE_COMMON,
            };
            offers
                .entry((item, grade))
                .or_default()
                .extend(sellers.iter().copied());
        },
    )?;
    let mut sold: Vec<SoldGear> = offers
        .into_iter()
        .map(|((item, grade), mut npcs)| {
            npcs.sort_unstable();
            npcs.dedup();
            SoldGear {
                item,
                grade,
                npcs: npcs.into_boxed_slice(),
            }
        })
        .collect();
    sold.sort_by_key(|s| (s.item, s.grade));
    Ok(sold)
}

/// The selectable rewards of every quest, in index order.
fn load_rewards(conn: &Connection) -> Result<FxHashMap<u32, Box<[u32]>>> {
    for t in [
        "quest_act_supply_selective_items",
        "quest_acts",
        "quest_components",
    ] {
        if !table_exists(conn, t)? {
            return Ok(FxHashMap::default());
        }
    }
    let mut by_quest: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
    each_row(
        conn,
        "SELECT qc.quest_context_id, s.item_id FROM quest_act_supply_selective_items s \
         JOIN quest_acts a ON a.act_detail_id = s.id \
              AND a.act_detail_type = 'QuestActSupplySelectiveItem' \
         JOIN quest_components qc ON qc.id = a.quest_component_id \
         ORDER BY qc.quest_context_id, qc.component_kind_id, qc.id, a.id",
        |r| by_quest.entry(r.u32(0)).or_default().push(r.u32(1)),
    )?;
    Ok(by_quest
        .into_iter()
        .map(|(q, v)| (q, v.into_boxed_slice()))
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::npc::NpcRoles;

    /// A trimmed database: a plate helmet (100, level 10, rolled grade), a cloth helmet (101, level
    /// 30, requirement 30), a greatsword (102, two-handed, fixed rare), a ring (103), a female-only
    /// chest piece (104); a lunagem 200 for the armor group (head, chest) from item level 20, the
    /// dawnstone 201, a regrade scroll for armor 300; merchant 50 (pack 5) sells the helmet at grade
    /// 2 and the gem, merchant 51 is no merchant, 52 has no spawn; quest 7 offers 102 then 103.
    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (id INT, LEVEL INT, level_requirement INT, fixed_grade INT,
               char_gender_id INT, gradable TEXT, grade_enchantable TEXT, use_skill_id INT,
               price INT, honor_price INT, living_point_price INT);
             CREATE TABLE item_armors (id INT, item_id INT, slot_type_id INT, type_id INT);
             CREATE TABLE item_weapons (id INT, item_id INT, holdable_id INT);
             CREATE TABLE holdables (id INT, kind_id INT, slot_type_id INT);
             CREATE TABLE item_accessories (id INT, item_id INT, slot_type_id INT, type_id INT);
             CREATE TABLE item_sockets (id INT, item_id INT, equip_slot_group_id INT);
             CREATE TABLE equip_slot_group_maps (id INT, equip_slot_group_id INT, equip_slot_type_id INT);
             CREATE TABLE item_socket_level_limits (id INT, name TEXT, item_id INT, LEVEL INT);
             CREATE TABLE item_socket_num_limits (id INT, name TEXT, slot_id INT, grade_id INT, num_socket INT);
             CREATE TABLE item_socket_chances (id INT, num_sockets INT, success_ratio INT);
             CREATE TABLE item_grades (id INT, grade_order INT, stat_multiplier INT,
               grade_enchant_success_ratio INT, grade_enchant_break_ratio INT,
               grade_enchant_downgrade_ratio INT);
             CREATE TABLE special_effects (id INT, special_effect_type_id INT, value1 INT, value2 INT,
               value3 INT, value4 INT);
             CREATE TABLE effects (id INT, actual_id INT, actual_type TEXT);
             CREATE TABLE skill_effects (id INT, skill_id INT, effect_id INT);
             CREATE TABLE merchants (id INT, npc_id INT, merchant_pack_id INT);
             CREATE TABLE merchant_goods (id INT, merchant_pack_id INT, item_id INT, grade_id INT);
             CREATE TABLE quest_act_supply_selective_items (id INT, item_id INT, count INT, grade_id INT);
             CREATE TABLE quest_acts (id INT, quest_component_id INT, act_detail_id INT, act_detail_type TEXT);
             CREATE TABLE quest_components (id INT, quest_context_id INT, component_kind_id INT);
             INSERT INTO items VALUES
               (100, 10, 0, -1, 0, 't', 't', 0, 500, 0, 0),
               (101, 30, 30, -1, 0, 't', 'f', 0, 900, 0, 0),
               (102, 40, 38, 3, 0, 't', 't', 0, 0, 2000, 0),
               (103, 12, 10, -1, 0, 'f', 'f', 0, 300, 0, 0),
               (104, 5, 0, -1, 2, 't', 't', 0, 100, 0, 0),
               (200, 1, 0, 0, 0, 'f', 'f', 23728, 12800, 0, 0),
               (201, 1, 0, 0, 0, 'f', 'f', 23729, 500, 0, 0),
               (300, 1, 0, 0, 0, 'f', 'f', 22522, 10000, 0, 0),
               (301, 1, 0, 0, 0, 'f', 'f', 22524, 10000, 0, 0);
             INSERT INTO item_armors VALUES (1, 100, 1, 3), (2, 101, 1, 1), (3, 104, 3, 1);
             INSERT INTO holdables VALUES (3, 0, 16);
             INSERT INTO item_weapons VALUES (1, 102, 3);
             INSERT INTO item_accessories VALUES (1, 103, 11, 5);
             INSERT INTO item_sockets VALUES (1, 200, 15), (2, 201, NULL);
             INSERT INTO equip_slot_group_maps VALUES (1, 15, 1), (2, 15, 3);
             INSERT INTO item_socket_level_limits VALUES (1, 'gem', 200, 20);
             INSERT INTO item_socket_num_limits VALUES (1, 'head common', 1, 0, 0), (2, 'head rare', 1, 3, 1),
               (3, 'chest uncommon', 3, 2, 1);
             INSERT INTO item_socket_chances VALUES (1, 0, 0), (2, 1, 10000), (3, 2, 5000);
             INSERT INTO item_grades VALUES (0, 1, 100, 5000, 0, 0), (1, 0, 80, 10000, 0, 0),
               (2, 2, 108, 4000, 0, 0), (5, 5, 132, 2000, 0, 8000), (7, 7, 150, 1000, 10000, 0);
             INSERT INTO special_effects VALUES (1, 106, 1, 0, 0, 0), (2, 106, 0, 0, 0, 0),
               (3, 92, 0, 0, 2, 0), (4, 92, 1, 0, 2, 0);
             INSERT INTO effects VALUES (10, 1, 'SpecialEffect'), (11, 2, 'SpecialEffect'),
               (12, 3, 'SpecialEffect'), (13, 4, 'SpecialEffect');
             INSERT INTO skill_effects VALUES (1, 23728, 10), (2, 23729, 11), (3, 22522, 12), (4, 22524, 13);
             INSERT INTO merchants VALUES (1, 50, 5), (2, 51, 5), (3, 52, 5);
             INSERT INTO merchant_goods VALUES (1, 5, 100, 2), (2, 5, 200, 0), (3, 5, 102, 0), (4, 5, 103, 4);
             INSERT INTO quest_components VALUES (70, 7, 8), (71, 7, 1);
             INSERT INTO quest_acts VALUES (701, 70, 1, 'QuestActSupplySelectiveItem'),
               (700, 70, 2, 'QuestActSupplySelectiveItem'), (702, 71, 3, 'QuestActSupplyItem');
             INSERT INTO quest_act_supply_selective_items VALUES (1, 103, 1, 0), (2, 102, 1, 0);",
        )
        .unwrap();
        conn
    }

    fn npcs() -> IdMap<NpcTemplate> {
        let mk = |id: u32, merchant: bool| NpcTemplate {
            id,
            name: "npc".into(),
            level: 1,
            faction_id: 1,
            kind: 1,
            grade: 1,
            aggressive: false,
            roles: if merchant {
                NpcRoles::MERCHANT
            } else {
                NpcRoles::NONE
            },
            is_quest_giver: false,
        };
        IdMap::build(vec![
            (50, mk(50, true)),
            (51, mk(51, false)),
            (52, mk(52, true)),
        ])
    }

    fn load() -> GearData {
        GearData::load(&fixture(), &npcs(), &|n| n != 52).unwrap()
    }

    #[test]
    fn the_items_carry_slot_kind_level_and_grade() {
        let g = load();
        assert_eq!(g.len(), 5);
        let helm = *g.item(100).unwrap();
        assert_eq!(helm.class, GearClass::Armor);
        assert_eq!(
            (helm.slot_type, helm.kind, helm.level, helm.req),
            (1, 3, 10, 0)
        );
        assert_eq!(helm.fixed_grade, NO_GRADE);
        assert!(helm.enchantable());
        assert_eq!(helm.grade_at(2), 2);
        assert_eq!(helm.slots(), [equip_slot::HEAD]);
        let sword = g.item(102).unwrap();
        assert_eq!(sword.class, GearClass::Weapon);
        assert!(sword.two_handed());
        assert_eq!(
            (sword.kind, sword.fixed_grade, sword.grade_at(0)),
            (3, 3, 3)
        );
        let ring = g.item(103).unwrap();
        assert_eq!(ring.class, GearClass::Accessory);
        assert_eq!(ring.slots(), [equip_slot::FINGER1, equip_slot::FINGER2]);
        // not gradable, no fixed grade: common
        assert_eq!(ring.grade_at(4), GRADE_COMMON);
        assert!(g.item(200).is_none(), "a gem is no gear");
    }

    #[test]
    fn wearing_follows_the_requirement_the_level_and_the_gender() {
        let g = load();
        let helm = g.item(100).unwrap(); // level 10, no requirement
        assert!(!helm.wearable_by(7, 1));
        assert!(helm.wearable_by(8, 1));
        let cloth = g.item(101).unwrap(); // requirement 30
        assert!(!cloth.wearable_by(29, 1) && cloth.wearable_by(30, 1));
        let dress = g.item(104).unwrap(); // female only
        assert!(!dress.wearable_by(10, 1) && dress.wearable_by(10, 2));
    }

    #[test]
    fn gems_sockets_chances_and_grades() {
        let g = load();
        let gem = g.gem(200).unwrap();
        assert!(gem.fits(slot_type::HEAD, 20) && gem.fits(slot_type::CHEST, 40));
        assert!(!gem.fits(slot_type::HEAD, 19), "below the gem's item level");
        assert!(!gem.fits(slot_type::HANDS, 40), "not in the gem's group");
        assert!(g.gem(201).is_none(), "the dawnstone is no lunagem");
        assert_eq!(g.gems().count(), 1);
        assert_eq!(g.sockets(slot_type::HEAD, 0), 0);
        assert_eq!(g.sockets(slot_type::HEAD, 3), 1);
        assert_eq!(g.sockets(slot_type::CHEST, 2), 1);
        assert_eq!(g.sockets(40, 3), 0);
        assert_eq!(g.socket_chance(0), 10_000, "the first gem always works");
        assert_eq!(g.socket_chance(1), 5_000);
        assert_eq!(g.socket_chance(7), 0);
        assert_eq!(g.grade(1).order, 0);
        assert_eq!(g.grade_factor(2), 1.08);
        assert_eq!(g.grade_factor(9), 1.0, "unknown grade");
        assert_eq!(g.grade(5).regrade_downgrade, 8000);
        assert_eq!(g.grade(7).regrade_break, 10_000);
        assert_eq!(g.scrolls().of(GearClass::Armor), 300);
        assert_eq!(g.scrolls().of(GearClass::Weapon), 0);
    }

    #[test]
    fn merchants_sell_gear_for_copper_at_the_normalized_grade() {
        let g = load();
        // the helmet at grade 2 by 50 only (51 is no merchant, 52 has no spawn)
        assert_eq!(
            g.sold_of(100),
            [SoldGear {
                item: 100,
                grade: 2,
                npcs: vec![50].into()
            }]
        );
        assert_eq!(g.sold_of(200).len(), 1, "the gem");
        assert!(
            g.sold_of(102).is_empty(),
            "an honor-priced piece is not for copper"
        );
        // the ring is not gradable: the row's grade 4 is sold as common
        assert_eq!(g.sold_of(103)[0].grade, GRADE_COMMON);
        assert!(g.sold_of(999).is_empty());
        assert_eq!(g.sold().len(), 3);
    }

    #[test]
    fn quest_rewards_follow_the_component_and_act_order() {
        let g = load();
        // act 700 (item 102) before act 701 (item 103); the supply act of component 71 is no choice
        assert_eq!(g.quest_rewards(7), [102, 103]);
        assert!(g.quest_rewards(8).is_empty());
        assert!(g.bytes() > 0);
    }

    #[test]
    fn a_database_without_gear_tables_gives_empty_facts() {
        let conn = Connection::open_in_memory().unwrap();
        let g = GearData::load(&conn, &npcs(), &|_| true).unwrap();
        assert!(g.is_empty());
        assert_eq!(g.sockets(1, 3), 0);
        assert_eq!(g.socket_chance(0), 0);
        assert!(g.sold().is_empty());
    }

    #[test]
    fn weapon_names_map_to_holdables() {
        assert_eq!(holdable::of_name("Nodachi"), holdable::NODACHI);
        assert_eq!(holdable::of_name(" bow "), holdable::BOW);
        assert!(holdable::of_name("Trident").is_empty());
        assert_eq!(slot_name(equip_slot::OFFHAND), "offhand");
        assert!(allowed_slots(slot_type::COSPLAY).is_empty());
    }
}
