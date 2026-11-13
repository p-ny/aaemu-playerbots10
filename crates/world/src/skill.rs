//! Skill facts bots need: class (ability) skills for rotations, plus every skill's casting
//! numbers and the labor a craft skill consumes.

use anyhow::Result;
use rusqlite::Connection;

use crate::geom::IdMap;
use crate::sql::{each_row, RowExt};
use crate::text::{TextTable, Texts};

/// Highest `skills.ability_id` of a class ability (1..=10 in 1.2 data).
pub const MAX_ABILITY: usize = 10;

/// One `skills` row (subset).
#[derive(Clone, Debug, PartialEq)]
pub struct SkillInfo {
    /// `skills.id`.
    pub id: u32,
    /// Class ability tree the skill belongs to (1..=10), 0 for non-class skills.
    pub ability_id: u8,
    /// Ability level at which the skill becomes available (1..=99; high values are entries no
    /// normal level reaches).
    pub ability_level: u8,
    /// Cooldown in milliseconds.
    pub cooldown_time: u32,
    /// Cast time in milliseconds.
    pub casting_time: u32,
    /// Base mana spent (`skills.mana_cost`); [`SkillInfo::mana_cost`] adds the level-scaled part.
    pub mana_cost: u32,
    /// Multiplier of [`mana_level_scale`] in the mana cost (`skills.mana_level_md`).
    pub mana_level_md: f32,
    /// Minimum target distance in metres.
    pub min_range: u16,
    /// Maximum target distance in metres (0 = melee / self).
    pub max_range: u16,
    /// `skills.target_type_id`.
    pub target_type_id: u8,
    /// `skills.target_relation_id` (which relation the target must have to the caster).
    pub target_relation_id: u8,
    /// Labor power consumed (`consume_lp`); crafts spend this.
    pub labor: u16,
    /// Learned automatically when the ability level is reached.
    pub auto_learn: bool,
    /// Must be bought/learned by the player.
    pub need_learn: bool,
    /// English name (Korean if there is no translation).
    pub name: Box<str>,
}

/// `unit_formulas` text of the character `LevelDps` (kind 12) and `LevelMana` (kind 13) rows the
/// server evaluates for the mana cost; [`mana_level_scale`] hard-codes them (checked at load).
const LEVEL_DPS_FORMULA: &str = "(ab_level * 1.6 + 8) * 3";
const LEVEL_MANA_FORMULA: &str = "level_dps / 3.65";

/// Mana per `mana_level_md` point at ability level `level`: the character formulas
/// `level_dps = (level * 1.6 + 8) * 3`, `level_mana = level_dps / 3.65` of `unit_formulas` (the
/// server's `mana_level_scale`). The bot only knows its character level, which stands in for the
/// ability level.
pub fn mana_level_scale(level: u8) -> f64 {
    let level_dps = (f64::from(level) * 1.6 + 8.0) * 3.0;
    level_dps / 3.65
}

/// Logs a warning when the data's character mana formulas are not the ones
/// [`mana_level_scale`] hard-codes (the table may be missing: nothing to check then).
fn warn_on_other_mana_formulas(conn: &Connection) {
    let mut found = [false, false];
    let mut other = false;
    let read = each_row(
        conn,
        "SELECT kind_id, formula FROM unit_formulas WHERE owner_type_id = 0 AND kind_id IN (12, 13)",
        |r| {
            let (slot, want) = match r.int(0) {
                12 => (0, LEVEL_DPS_FORMULA),
                _ => (1, LEVEL_MANA_FORMULA),
            };
            found[slot] = true;
            other |= r.text(1).trim() != want;
        },
    );
    if read.is_ok() && (other || found != [true, true]) {
        tracing::warn!(
            "the character mana formulas in unit_formulas differ from the built-in ones; \
             SkillInfo::mana_cost may be off"
        );
    }
}

impl SkillInfo {
    /// Mana the skill costs a caster of ability level `level`:
    /// `mana_cost + mana_level_md * mana_level_scale(level)`, truncated like the server's
    /// `Skill.ManaCost`. Buff and gear modifiers are not included.
    pub fn mana_cost(&self, level: u8) -> u32 {
        let cost =
            f64::from(self.mana_level_md) * mana_level_scale(level) + f64::from(self.mana_cost);
        cost.max(0.0) as u32
    }
}

/// All skills plus the per-ability ordered lists.
#[derive(Debug, Default)]
pub struct Skills {
    by_id: IdMap<SkillInfo>,
    /// `abilities[a]` = ids of ability `a`'s skills ordered by `(ability_level, id)`.
    abilities: Vec<Box<[u32]>>,
}

impl Skills {
    /// Loads `skills`.
    pub fn load(conn: &Connection, texts: &Texts) -> Result<Skills> {
        let mut rows = Vec::new();
        each_row(
            conn,
            if crate::sql::has_column(conn, "skills", "need_learn") {
                "SELECT id, name, ability_id, ability_level, cooldown_time, casting_time, mana_cost, \
             min_range, max_range, target_type_id, target_relation_id, consume_lp, auto_learn, \
             need_learn, mana_level_md FROM skills"
            } else {
                "SELECT id, name, ability_id, ability_level, cooldown_time, casting_time, mana_cost, \
             min_range, max_range, target_type_id, target_relation_id, consume_lp, auto_learn, \
             CASE WHEN skill_points > 0 THEN 't' ELSE 'f' END, mana_level_md FROM skills"
            },
            |r| {
                let id = r.u32(0);
                rows.push((
                    id,
                    SkillInfo {
                        id,
                        ability_id: r.u8(2),
                        ability_level: r.u8(3),
                        cooldown_time: r.u32(4),
                        casting_time: r.u32(5),
                        mana_cost: r.u32(6),
                        min_range: r.u16(7),
                        max_range: r.u16(8),
                        target_type_id: r.u8(9),
                        target_relation_id: r.u8(10),
                        labor: r.u16(11),
                        auto_learn: r.flag(12),
                        need_learn: r.flag(13),
                        mana_level_md: r.f32(14),
                        name: texts.name_or(TextTable::Skill, id, r.text(1)),
                    },
                ));
            },
        )?;
        warn_on_other_mana_formulas(conn);
        Ok(Skills::from_rows(rows))
    }

    fn from_rows(rows: Vec<(u32, SkillInfo)>) -> Skills {
        let by_id = IdMap::build(rows);
        let mut lists: Vec<Vec<(u8, u32)>> = vec![Vec::new(); MAX_ABILITY + 1];
        for s in by_id.values() {
            if s.ability_id > 0 && usize::from(s.ability_id) <= MAX_ABILITY {
                lists[usize::from(s.ability_id)].push((s.ability_level, s.id));
            }
        }
        let abilities = lists
            .into_iter()
            .map(|mut l| {
                l.sort_unstable();
                l.into_iter().map(|(_, id)| id).collect()
            })
            .collect();
        Skills { by_id, abilities }
    }

    /// Builds from rows (tests).
    #[cfg(test)]
    pub fn from_infos(infos: Vec<SkillInfo>) -> Skills {
        Skills::from_rows(infos.into_iter().map(|s| (s.id, s)).collect())
    }

    /// One skill.
    pub fn get(&self, id: u32) -> Option<&SkillInfo> {
        self.by_id.get(id)
    }

    /// Skills of class ability `ability` ordered by required ability level.
    pub fn ability_skills(&self, ability: u8) -> &[u32] {
        self.abilities
            .get(usize::from(ability))
            .map_or(&[], |l| &l[..])
    }

    /// Number of skills.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether there are no skills.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        self.by_id.shallow_bytes()
            + self
                .by_id
                .values()
                .iter()
                .map(|s| s.name.len())
                .sum::<usize>()
            + self
                .abilities
                .iter()
                .map(|l| l.len() * 4 + 16)
                .sum::<usize>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: u32, ability: u8, level: u8) -> SkillInfo {
        SkillInfo {
            id,
            ability_id: ability,
            ability_level: level,
            cooldown_time: 0,
            casting_time: 0,
            mana_cost: 0,
            mana_level_md: 0.0,
            min_range: 0,
            max_range: 0,
            target_type_id: 0,
            target_relation_id: 0,
            labor: 0,
            auto_learn: false,
            need_learn: false,
            name: "".into(),
        }
    }

    #[test]
    fn ability_lists_are_sorted_by_level_then_id() {
        let s = Skills::from_infos(vec![
            info(30, 1, 10),
            info(20, 1, 1),
            info(10, 1, 1),
            info(99, 0, 5),
            info(40, 2, 3),
        ]);
        assert_eq!(s.ability_skills(1), &[10, 20, 30]);
        assert_eq!(s.ability_skills(2), &[40]);
        assert!(s.ability_skills(0).is_empty());
        assert!(s.ability_skills(200).is_empty());
        assert_eq!(s.get(99).map(|k| k.ability_id), Some(0));
    }

    #[test]
    fn mana_scales_with_the_level() {
        // level_dps = (level * 1.6 + 8) * 3, level_mana = level_dps / 3.65
        assert!((mana_level_scale(1) - 7.890).abs() < 0.01);
        assert!((mana_level_scale(50) - 72.33).abs() < 0.01);
        let mut k = info(1, 1, 1);
        assert_eq!(k.mana_cost(50), 0, "free skills stay free");
        k.mana_cost = 40;
        assert_eq!(k.mana_cost(50), 40, "a flat cost does not scale");
        k.mana_level_md = 1.5;
        assert_eq!(k.mana_cost(1), 40 + 11); // 1.5 * 7.89 = 11.8 -> truncated
        assert_eq!(k.mana_cost(50), 40 + 108); // 1.5 * 72.33 = 108.5
        assert!(k.mana_cost(50) > k.mana_cost(10));
    }

    #[test]
    fn loads_from_sqlite() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE skills (id INT, name TEXT, ability_id INT, ability_level INT, cooldown_time INT,
               casting_time INT, mana_cost INT, min_range INT, max_range INT, target_type_id INT,
               target_relation_id INT, consume_lp INT, auto_learn NUM, need_learn NUM, mana_level_md REAL);
             INSERT INTO skills VALUES (7, '검', 3, 5, 12000, 1000, 40, 0, 12, 4, 1, 0, 't', 'f', 1.5);
             INSERT INTO skills VALUES (8, 'x', 0, NULL, NULL, NULL, NULL, NULL, NULL, NULL, NULL, 100, 'f', 't', NULL);",
        )
        .unwrap();
        let mut texts = Texts::empty();
        texts.insert(TextTable::Skill, 7, "Slash");
        let s = Skills::load(&conn, &texts).unwrap();
        let k = s.get(7).unwrap();
        assert_eq!(&*k.name, "Slash");
        assert_eq!(
            (k.ability_id, k.ability_level, k.cooldown_time, k.max_range),
            (3, 5, 12000, 12)
        );
        assert!(k.auto_learn && !k.need_learn);
        assert_eq!((k.mana_cost, k.mana_level_md), (40, 1.5));
        let k8 = s.get(8).unwrap();
        assert_eq!(
            (k8.labor, k8.cooldown_time, k8.mana_level_md),
            (100, 0, 0.0)
        );
        assert!(k8.need_learn);
        assert_eq!(s.ability_skills(3), &[7]);
    }
}
