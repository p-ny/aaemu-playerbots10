//! Instance dungeons: the entrance doodads in the main world and the rules for entering.
//!
//! An entrance doodad carries a `DoodadFuncEnterInstance` whose `zone_id` (a `zones.id`) names
//! the dungeon zone; the client casts the function's skill on the doodad and the server
//! (`systems/instances/indun.rs: request_dungeon_instance`) then
//!
//! 1. resolves the zone (`zones.zone_key`, `zones.group_id`) and requires a world for the zone
//!    key and an `indun_zones` row for the zone group;
//! 2. checks level (`level_min..=level_max`) and, for `party_only`, a party;
//! 3. joins an existing run of the party or creates a new instance world, charging the entry
//!    (`charge_entry`): at most `EnterCount` entries per zone group within
//!    [`ENTRY_WINDOW_SECS`] (3, or 1000 for the Mirage Isle group 49 and `select_channel`
//!    zones; `data.rs:385`) and `indun_zones.item_id` (consumed when set);
//! 4. sends `SCLoadInstance` with the instance id and the zone key and moves the character to
//!    the world's spawn point.
//!
//! Zones marked `closed` are evicted by the server's closed-zone check for characters below
//! access level 100 (`world/zone_access.rs`), so [`Dungeon::closed`] dungeons are no targets
//! for ordinary bots.

use anyhow::{Context, Result};

use super::funcs::DoodadTravelFuncs;
use super::instance::{InstanceWorld, InstanceWorlds};
use super::{localized, str_bytes, TravelInputs};
use crate::geom::Vec3;
use crate::sql::{each_row, RowExt};
use crate::text::TextTable;

/// Entries counted per zone group within this window (4 hours, `indun.rs:78`).
pub const ENTRY_WINDOW_SECS: u32 = 4 * 3600;
/// `EnterCount` of ordinary dungeons (`data.rs:385`).
pub const DEFAULT_ENTER_COUNT: u32 = 3;
/// `EnterCount` of Mirage Isle and `select_channel` zones (effectively unlimited).
pub const UNLIMITED_ENTER_COUNT: u32 = 1000;
/// The Mirage Isle zone group (`zone_group_id == 49` has the unlimited count).
pub const MIRAGE_ZONE_GROUP: u32 = 49;

/// One way into a dungeon: a doodad that spawns in the main world.
#[derive(Clone, Debug, PartialEq)]
pub struct Entrance {
    /// Doodad template.
    pub doodad: u32,
    /// Skill to cast on the doodad (the function's own skill, else the doodad's use skill).
    pub skill: u32,
    /// Its spawn positions in the main world.
    pub spawns: Box<[Vec3]>,
}

/// An instance dungeon with at least one entrance in the main world.
#[derive(Clone, Debug, PartialEq)]
pub struct Dungeon {
    /// `zones.id` (what `doodad_func_enter_instances.zone_id` holds).
    pub zone_id: u32,
    /// Zone key (what `SCLoadInstance` carries).
    pub zone_key: u16,
    /// `zones.group_id` = `indun_zones.zone_group_id`; the entry limit counts per group.
    pub zone_group: u32,
    /// English name (`indun_zones` localization, else the zone group's, else the Korean name).
    pub name: Box<str>,
    /// Lowest character level.
    pub level_min: u8,
    /// Highest character level.
    pub level_max: u8,
    /// Players the instance admits.
    pub max_players: u16,
    /// Players may fight each other inside.
    pub pvp: bool,
    /// The character needs a party to enter.
    pub party_only: bool,
    /// `zones.closed`: the server evicts characters below access level 100.
    pub closed: bool,
    /// The instance is picked by channel (libraries; unlimited entries).
    pub select_channel: bool,
    /// The client drives the instance (the prologue dream).
    pub client_driven: bool,
    /// Item consumed on the first entry of a run (`indun_zones.item_id`, 0 = none).
    pub required_item: u32,
    /// Entries allowed per [`ENTRY_WINDOW_SECS`] and zone group.
    pub enter_count: u32,
    /// Entrances reachable in the main world.
    pub entrances: Box<[Entrance]>,
    world: Option<u16>,
}

impl Dungeon {
    /// Whether a character of `level` passes the level check.
    pub fn accepts_level(&self, level: u8) -> bool {
        (self.level_min..=self.level_max).contains(&level)
    }

    /// The entrance nearest to `from` and its nearest spawn (horizontal distance).
    pub fn nearest_entrance(&self, from: Vec3) -> Option<(&Entrance, Vec3)> {
        self.entrances
            .iter()
            .flat_map(|e| e.spawns.iter().map(move |p| (e, *p)))
            .min_by(|a, b| a.1.dist2d_sq(from).total_cmp(&b.1.dist2d_sq(from)))
    }
}

/// Dungeons plus the instance worlds.
#[derive(Debug, Default)]
pub struct Dungeons {
    /// Ordered by zone group, then zone id.
    dungeons: Vec<Dungeon>,
    worlds: InstanceWorlds,
    /// `(entrance doodad, index into dungeons)` sorted.
    by_entrance: Vec<(u32, u16)>,
    /// Exit doodad templates of all instance worlds, sorted.
    exit_doodads: Vec<u32>,
}

impl Dungeons {
    pub(crate) fn load(
        inp: &TravelInputs<'_>,
        funcs: &DoodadTravelFuncs,
        worlds: InstanceWorlds,
    ) -> Result<Dungeons> {
        let conn = inp.conn;
        let mut zones: Vec<(u32, u16, u32, bool, Box<str>)> = Vec::new();
        each_row(
            conn,
            "SELECT id, zone_key, group_id, closed, name FROM zones",
            |r| {
                zones.push((
                    r.u32(0),
                    r.u16(1),
                    r.u32(2),
                    r.flag(3),
                    r.text(4).trim().into(),
                ))
            },
        )
        .context("read zones")?;

        struct Rules {
            level: (u8, u8),
            max_players: u16,
            pvp: bool,
            party_only: bool,
            client_driven: bool,
            select_channel: bool,
            item: u32,
            korean: Box<str>,
        }
        let mut rules: Vec<(u32, Rules)> = Vec::new();
        each_row(
            conn,
            if crate::sql::has_column(conn, "indun_zones", "item_id") {
                "SELECT zone_group_id, level_min, level_max, max_players, pvp, item_id, party_only, \
             client_driven, select_channel, name FROM indun_zones"
            } else {
                "SELECT d.zone_group_id, d.level_min, d.level_max, d.max_players, d.pvp, NULL, d.party_only, \
             d.client_driven, d.select_channel, COALESCE(z.name, '') FROM indun_zones d \
             LEFT JOIN zone_groups z ON z.id = d.zone_group_id"
            },
            |r| {
                rules.push((
                    r.u32(0),
                    Rules {
                        level: (r.u8(1), r.u8(2)),
                        max_players: r.u16(3),
                        pvp: r.flag(4),
                        item: r.u32(5),
                        party_only: r.flag(6),
                        client_driven: r.flag(7),
                        select_channel: r.flag(8),
                        korean: r.text(9).trim().into(),
                    },
                ));
            },
        )
        .context("read indun_zones")?;
        let names = localized(conn, "indun_zones", "name");

        let mut dungeons: Vec<Dungeon> = Vec::new();
        for (zone_id, zone_key, group, closed, zone_name) in &zones {
            let Some((_, rule)) = rules.iter().find(|(g, _)| g == group) else {
                continue; // not a dungeon zone
            };
            // Entrances: enter functions into this zone whose doodad spawns in the main world.
            let mut entrances: Vec<Entrance> = Vec::new();
            for f in funcs.enters.iter().filter(|f| f.zone_id == *zone_id) {
                let spawns = inp.doodad_spawns.of(f.doodad);
                if spawns.is_empty() || entrances.iter().any(|e| e.doodad == f.doodad) {
                    continue;
                }
                let skill = if f.skill != 0 {
                    f.skill
                } else {
                    inp.doodads
                        .templates
                        .get(f.doodad)
                        .map_or(0, |d| d.use_skill)
                };
                entrances.push(Entrance {
                    doodad: f.doodad,
                    skill,
                    spawns: spawns.into(),
                });
            }
            if entrances.is_empty() {
                continue;
            }
            let name: Box<str> = names
                .get(group)
                .map(|n| &**n)
                .or_else(|| inp.texts.get(TextTable::ZoneGroup, *group))
                .unwrap_or(if rule.korean.is_empty() {
                    zone_name
                } else {
                    &rule.korean
                })
                .into();
            dungeons.push(Dungeon {
                zone_id: *zone_id,
                zone_key: *zone_key,
                zone_group: *group,
                name,
                level_min: rule.level.0,
                level_max: rule.level.1,
                max_players: rule.max_players,
                pvp: rule.pvp,
                party_only: rule.party_only,
                closed: *closed,
                select_channel: rule.select_channel,
                client_driven: rule.client_driven,
                required_item: rule.item,
                enter_count: if *group == MIRAGE_ZONE_GROUP || rule.select_channel {
                    UNLIMITED_ENTER_COUNT
                } else {
                    DEFAULT_ENTER_COUNT
                },
                entrances: entrances.into_boxed_slice(),
                world: worlds.index_of_zone(*zone_key).map(|i| i as u16),
            });
        }
        dungeons.sort_by_key(|d| (d.zone_group, d.zone_id));
        for d in dungeons.iter().filter(|d| d.world.is_none()) {
            tracing::warn!(
                "travel: dungeon zone {} ({}, key {}) has no instance world directory",
                d.zone_id,
                d.name,
                d.zone_key
            );
        }

        let mut by_entrance: Vec<(u32, u16)> = dungeons
            .iter()
            .enumerate()
            .flat_map(|(i, d)| d.entrances.iter().map(move |e| (e.doodad, i as u16)))
            .collect();
        by_entrance.sort_unstable();
        by_entrance.dedup();
        let mut exit_doodads: Vec<u32> = worlds
            .all()
            .iter()
            .flat_map(|w| w.exits().iter().map(|e| e.template))
            .collect();
        exit_doodads.sort_unstable();
        exit_doodads.dedup();
        Ok(Dungeons {
            dungeons,
            worlds,
            by_entrance,
            exit_doodads,
        })
    }

    /// Dungeons with an entrance in the main world, ordered by zone group.
    pub fn dungeons(&self) -> &[Dungeon] {
        &self.dungeons
    }

    /// The dungeon an entrance doodad leads into. A doodad with several enter functions (the
    /// Howling Abyss gate also opens the ancient one) gives the lowest zone group; see
    /// [`Dungeons::by_entrance_doodad`] for all of them.
    pub fn dungeon_by_entrance_doodad(&self, template: u32) -> Option<&Dungeon> {
        self.by_entrance_doodad(template).next()
    }

    /// All dungeons an entrance doodad leads into, ordered by zone group.
    pub fn by_entrance_doodad(&self, template: u32) -> impl Iterator<Item = &Dungeon> {
        let start = self.by_entrance.partition_point(|e| e.0 < template);
        self.by_entrance[start..]
            .iter()
            .take_while(move |e| e.0 == template)
            .map(|e| &self.dungeons[usize::from(e.1)])
    }

    /// Whether `template` is a dungeon door: the entrance doodad of a dungeon in the main world
    /// or an exit doodad of an instance world (the doodads the dungeon skills are cast on).
    pub fn is_door(&self, template: u32) -> bool {
        self.by_entrance
            .binary_search_by_key(&template, |e| e.0)
            .is_ok()
            || self.exit_doodads.binary_search(&template).is_ok()
    }

    /// The dungeon of a zone group.
    pub fn by_zone_group(&self, zone_group: u32) -> Option<&Dungeon> {
        self.dungeons.iter().find(|d| d.zone_group == zone_group)
    }

    /// The dungeon whose zone key is `zone_key`.
    pub fn by_zone_key(&self, zone_key: u16) -> Option<&Dungeon> {
        self.dungeons.iter().find(|d| d.zone_key == zone_key)
    }

    /// The instance worlds.
    pub fn worlds(&self) -> &InstanceWorlds {
        &self.worlds
    }

    /// The instance world a dungeon loads.
    pub fn world_of(&self, dungeon: &Dungeon) -> Option<&InstanceWorld> {
        self.worlds.get(usize::from(dungeon.world?))
    }

    /// The instance world that loads `zone_key`.
    pub fn world_by_zone_key(&self, zone_key: u16) -> Option<&InstanceWorld> {
        self.worlds.by_zone_key(zone_key)
    }

    /// An instance world by directory name.
    pub fn world(&self, name: &str) -> Option<&InstanceWorld> {
        self.worlds.by_name(name)
    }

    /// Approximate heap bytes held.
    pub fn bytes(&self) -> usize {
        self.dungeons.capacity() * std::mem::size_of::<Dungeon>()
            + str_bytes(self.dungeons.iter().map(|d| &*d.name))
            + self
                .dungeons
                .iter()
                .map(|d| {
                    std::mem::size_of_val(&*d.entrances)
                        + d.entrances
                            .iter()
                            .map(|e| std::mem::size_of_val(&*e.spawns))
                            .sum::<usize>()
                })
                .sum::<usize>()
            + self.by_entrance.capacity() * 8
            + self.exit_doodads.capacity() * 4
            + self.worlds.bytes()
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;

    use super::*;
    use crate::doodad::DoodadData;
    use crate::geom::{IdMap, StrTable};
    use crate::npc::NpcTemplate;
    use crate::skill::Skills;
    use crate::spatial::SpawnIndex;
    use crate::text::Texts;
    use crate::travel::funcs;
    use crate::travel::testutil::{temp_dir, write};

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        funcs::create_tables(&conn);
        conn.execute_batch(
            "CREATE TABLE zones (id INT, zone_key INT, name TEXT, group_id INT, closed TEXT);
             CREATE TABLE indun_zones (zone_group_id INT, name TEXT, level_min INT, level_max INT,
               max_players INT, pvp TEXT, item_id INT, party_only TEXT, client_driven TEXT, select_channel TEXT);
             CREATE TABLE localized_texts (id INT, tbl_name TEXT, tbl_column_name TEXT, idx INT, en_us TEXT);
             INSERT INTO zones VALUES
               (184, 262, 'instance_cuttingwind_deadmine', 50, 'f'),
               (192, 278, 'instance_nachashgar', 55, 't'),
               (183, 260, 'arche_mall', 49, 'f'),
               (185, 265, 'instance_howling_abyss', 51, 'f'),
               (194, 280, 'instance_howling_abyss_2', 58, 'f'),
               (170, 240, 'instance_sal_temple', 47, 'f'),
               (100, 124, 'main_world_0_2', NULL, 'f');
             INSERT INTO indun_zones VALUES
               (50, 'korean mine', 18, 55, 3, 't', NULL, 'f', 'f', 'f'),
               (55, '나차쉬가르', 50, 55, 10, 't', 27965, 't', 'f', 'f'),
               (49, 'mirage', 1, 55, 190, 'f', NULL, 'f', 'f', 'f'),
               (51, 'abyss', 40, 55, 5, 't', NULL, 'f', 'f', 'f'),
               (58, 'abyss2', 50, 55, 5, 't', NULL, 'f', 'f', 'f'),
               (47, 'palace', 18, 55, 3, 't', NULL, 'f', 'f', 'f');
             INSERT INTO localized_texts VALUES (1, 'indun_zones', 'name', 50, 'Sharpwind Mines');
             INSERT INTO doodad_func_enter_instances VALUES (11, 184, 0), (23, 192, NULL), (12, 185, 0), (8000001, 194, NULL), (6, 170, 0);
             INSERT INTO doodad_func_groups VALUES (1, 5075, 1), (2, 6805, 1), (3, 5085, 1), (4, 4088, 1);
             INSERT INTO doodad_funcs VALUES
               (10, 1, 11, 'DoodadFuncEnterInstance', 19836),
               (11, 2, 23, 'DoodadFuncEnterInstance', 20963),
               (12, 3, 12, 'DoodadFuncEnterInstance', 18339),
               (13, 3, 8000001, 'DoodadFuncEnterInstance', 21401),
               (14, 4, 6, 'DoodadFuncEnterInstance', 17743);",
        )
        .unwrap();
        conn
    }

    #[test]
    fn dungeons_combine_zones_rules_entrances_and_worlds() {
        let conn = fixture();
        let dir = temp_dir("dungeon");
        write(
            &dir,
            "Worlds/instance_cuttingwind_deadmine/npc_spawns.json",
            "[]",
        );
        write(&dir, "Worlds/instance_nachashgar/npc_spawns.json", "[]");
        write(
            &dir,
            "Worlds/world_spawns.json",
            r#"[ { "Name": "instance_cuttingwind_deadmine", "SpawnPosition": { "ZoneId": 262, "X": 796.3, "Y": 325.386, "Z": 249.54, "Yaw": 90.0 } } ]"#,
        );
        let npcs = IdMap::<NpcTemplate>::build(Vec::new());
        let doodads = DoodadData::default();
        let texts = Texts::empty();
        let skills = Skills::default();
        // 4088 (Sal Temple) does not spawn in the main world; 5085 does
        let spawns = SpawnIndex::build(vec![
            (5075, Vec3::new(11354.8, 13124.0, 143.8)),
            (6805, Vec3::new(5000.0, 5000.0, 100.0)),
            (5085, Vec3::new(1.0, 2.0, 3.0)),
        ]);
        let items = StrTable::default();
        let inp = TravelInputs {
            conn: &conn,
            texts: &texts,
            server_data: &dir,
            main_world: "main_world",
            npcs: &npcs,
            doodads: &doodads,
            skills: &skills,
            doodad_spawns: &spawns,
            items: &items,
        };
        let f = DoodadTravelFuncs::load(&conn).unwrap();
        let worlds = InstanceWorlds::load(&inp, &f).unwrap();
        let d = Dungeons::load(&inp, &f, worlds).unwrap();

        // Sal Temple has no reachable entrance, the mall zone has no entrance function at all
        let groups: Vec<u32> = d.dungeons().iter().map(|d| d.zone_group).collect();
        assert_eq!(groups, vec![50, 51, 55, 58]);

        let mine = d.dungeon_by_entrance_doodad(5075).unwrap();
        assert_eq!(&*mine.name, "Sharpwind Mines", "the localized name wins");
        assert_eq!(
            (mine.zone_id, mine.zone_key, mine.zone_group),
            (184, 262, 50)
        );
        assert_eq!(
            (mine.level_min, mine.level_max, mine.max_players),
            (18, 55, 3)
        );
        assert!(mine.pvp && !mine.party_only && !mine.closed);
        assert_eq!(
            (mine.required_item, mine.enter_count),
            (0, DEFAULT_ENTER_COUNT)
        );
        assert_eq!(mine.entrances.len(), 1);
        assert_eq!(mine.entrances[0].skill, 19836);
        assert_eq!(
            &*mine.entrances[0].spawns,
            &[Vec3::new(11354.8, 13124.0, 143.8)]
        );
        assert!(mine.accepts_level(18) && mine.accepts_level(55));
        assert!(!mine.accepts_level(17) && !mine.accepts_level(56));
        let (_, near) = mine
            .nearest_entrance(Vec3::new(11000.0, 13000.0, 0.0))
            .unwrap();
        assert_eq!(near, Vec3::new(11354.8, 13124.0, 143.8));
        assert_eq!(
            d.world_of(mine).map(|w| &*w.name),
            Some("instance_cuttingwind_deadmine")
        );
        assert_eq!(
            d.world_by_zone_key(262)
                .and_then(|w| w.spawn_for_zone(262))
                .map(|s| s.pos),
            Some(Vec3::new(796.3, 325.386, 249.54))
        );

        // closed, party only, with a required item; name falls back to the Korean one
        let serpent = d.dungeon_by_entrance_doodad(6805).unwrap();
        assert_eq!(&*serpent.name, "나차쉬가르");
        assert!(serpent.closed && serpent.party_only);
        assert_eq!(serpent.required_item, 27965);

        // one doodad, two enter functions (skills differ): both dungeons, lowest group first;
        // the second has no world directory
        let abyss: Vec<(u32, u32)> = d
            .by_entrance_doodad(5085)
            .map(|d| (d.zone_group, d.entrances[0].skill))
            .collect();
        assert_eq!(abyss, vec![(51, 18339), (58, 21401)]);
        assert_eq!(
            d.dungeon_by_entrance_doodad(5085).map(|d| d.zone_group),
            Some(51)
        );
        assert!(d.world_of(d.by_zone_group(58).unwrap()).is_none());

        assert!(d.dungeon_by_entrance_doodad(4088).is_none());
        assert!(d.dungeon_by_entrance_doodad(1).is_none());
        // doors: the entrances of the main world
        assert!(d.is_door(5075) && d.is_door(5085) && !d.is_door(4088) && !d.is_door(1));
        assert_eq!(d.by_zone_key(278).map(|d| d.zone_id), Some(192));
        assert!(d.bytes() > 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mirage_and_channel_zones_have_unlimited_entries() {
        let conn = fixture();
        conn.execute_batch(
            "INSERT INTO doodad_func_enter_instances VALUES (40, 183, 0);
             INSERT INTO doodad_func_groups VALUES (9, 7000, 1);
             INSERT INTO doodad_funcs VALUES (30, 9, 40, 'DoodadFuncEnterInstance', 5);
             UPDATE indun_zones SET select_channel = 't' WHERE zone_group_id = 58;",
        )
        .unwrap();
        let dir = temp_dir("dungeon-mirage");
        let npcs = IdMap::<NpcTemplate>::build(Vec::new());
        let doodads = DoodadData::default();
        let texts = Texts::empty();
        let skills = Skills::default();
        let spawns = SpawnIndex::build(vec![
            (7000, Vec3::new(1.0, 1.0, 1.0)),
            (5085, Vec3::new(2.0, 2.0, 2.0)),
        ]);
        let items = StrTable::default();
        let inp = TravelInputs {
            conn: &conn,
            texts: &texts,
            server_data: &dir,
            main_world: "main_world",
            npcs: &npcs,
            doodads: &doodads,
            skills: &skills,
            doodad_spawns: &spawns,
            items: &items,
        };
        let f = DoodadTravelFuncs::load(&conn).unwrap();
        let d = Dungeons::load(&inp, &f, InstanceWorlds::default()).unwrap();
        let count = |group| d.by_zone_group(group).map(|d| d.enter_count);
        assert_eq!(count(49), Some(UNLIMITED_ENTER_COUNT));
        assert_eq!(count(58), Some(UNLIMITED_ENTER_COUNT));
        assert_eq!(count(51), Some(DEFAULT_ENTER_COUNT));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
