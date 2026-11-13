//! Crafting recipes and trade-pack ("specialty") selling.
//!
//! A craft is a *pack* craft when its product is a specialty good that some specialty NPC buys
//! (`specialty_bundle_items.item_id`); those are the recipes a trade run needs. Benches are the
//! doodad templates that offer a recipe (`DoodadFuncCraftPack` / `DoodadFuncCraftStart`
//! functions, plus the recipe's own `req_doodad_id`).
//!
//! Table semantics follow the server's `systems/economy/{craft_data,specialty}.rs` and
//! `systems/npcs/bridges.rs` (AAEmu Rust, GPL-3.0-or-later).

use anyhow::Result;
use rusqlite::Connection;
use rustc_hash::FxHashMap;

use crate::geom::IdMap;
use crate::skill::Skills;
use crate::sql::{each_row, RowExt};
use crate::text::{TextTable, Texts};

/// One crafting recipe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Craft {
    /// `crafts.id`.
    pub id: u32,
    /// English title (Korean if there is no translation).
    pub name: Box<str>,
    /// `crafts.skill_id`: the skill whose `labor` the craft costs.
    pub skill_id: u32,
    /// Labor power spent.
    pub labor: u16,
    /// The first product `(item id, count)`.
    pub product: (u32, u32),
    /// Required materials `(item id, count)`.
    pub materials: Box<[(u32, u32)]>,
    /// Produces a specialty trade pack that specialty NPCs buy.
    pub is_pack: bool,
    /// `crafts.req_doodad_id`: the doodad template the recipe requires (0 = none).
    pub req_doodad: u32,
    /// Doodad templates that offer this recipe.
    pub benches: Box<[u32]>,
    /// Character level the craft skill requires (the level `unit_reqs` of `crafts.skill_id`);
    /// 0 = none. Pack crafting needs 10.
    pub req_level: u8,
    /// `skills.actability_group_id` of the craft skill: the vocation whose points
    /// [`Craft::actability_limit`] is measured in (0 = none).
    pub actability_group: u16,
    /// Points in that vocation the craft needs (`crafts.actability_limit`); 0 = none.
    pub actability_limit: u32,
}

/// The server's best specialty price ratio in percent (`SpecialtyConfig.max_specialty_ratio`);
/// a zone starts there and sales drive it down.
const MAX_SPECIALTY_RATIO: f32 = 130.0;
/// Interest the server adds to the payout (`interest_rate` 5 %).
const SPECIALTY_INTEREST: f32 = 0.05;

/// Recipes and specialty selling.
#[derive(Debug, Default)]
pub struct Economy {
    crafts: IdMap<Craft>,
    /// Pack crafts by bench doodad template.
    pack_at: FxHashMap<u32, Box<[u32]>>,
    /// Specialty NPC templates by pack item.
    buyers: FxHashMap<u32, Box<[u32]>>,
    /// `(pack item, bundle, base price)` sorted: the server's `GetBasePriceForSpecialty`, i.e.
    /// `floor(profit * ratio / 1000) + item refund`, in copper.
    bundle_prices: Vec<(u32, u32, u32)>,
    /// Specialty NPC template -> the bundle it buys (`specialty_npcs`).
    npc_bundle: FxHashMap<u32, u32>,
    /// `specialties` rows `(from zone group, to zone group, ratio, profit)` sorted by the pair.
    zone_pairs: Vec<(u32, u32, u32, u32)>,
}

impl Economy {
    /// Loads `crafts`, `craft_products`, `craft_materials`, `specialty_bundle_items` and
    /// `specialty_npcs`. `bench_pairs` are `(doodad template, craft)` pairs from the doodad loader.
    pub fn load(
        conn: &Connection,
        texts: &Texts,
        skills: &Skills,
        bench_pairs: &[(u32, u32)],
    ) -> Result<Economy> {
        let mut head: Vec<(u32, Box<str>, u32, u32, u32)> = Vec::new();
        each_row(
            conn,
            "SELECT id, title, skill_id, req_doodad_id, actability_limit FROM crafts",
            |r| {
                let id = r.u32(0);
                head.push((
                    id,
                    texts.name_or(TextTable::Craft, id, r.text(1)),
                    r.u32(2),
                    r.u32(3),
                    r.u32(4),
                ));
            },
        )?;
        // What a craft skill demands of the character: the level (`unit_reqs` kind 1) and the
        // vocation it belongs to.
        let mut skill_level: FxHashMap<u32, u8> = FxHashMap::default();
        each_row(
            conn,
            "SELECT owner_id, value1 FROM unit_reqs WHERE owner_type = 'Skill' AND kind_id = 1 ORDER BY id",
            |r| {
                skill_level.entry(r.u32(0)).or_insert(r.u8(1));
            },
        )?;
        let mut skill_group: FxHashMap<u32, u16> = FxHashMap::default();
        each_row(
            conn,
            "SELECT id, actability_group_id FROM skills WHERE actability_group_id != 0",
            |r| {
                skill_group.insert(r.u32(0), r.u16(1));
            },
        )?;
        let mut products: FxHashMap<u32, (u32, u32)> = FxHashMap::default();
        each_row(
            conn,
            "SELECT craft_id, item_id, amount FROM craft_products ORDER BY id",
            |r| {
                products
                    .entry(r.u32(0))
                    .or_insert((r.u32(1), r.u32(2).max(1)));
            },
        )?;
        let mut materials: FxHashMap<u32, Vec<(u32, u32)>> = FxHashMap::default();
        each_row(
            conn,
            "SELECT craft_id, item_id, amount FROM craft_materials ORDER BY id",
            |r| {
                materials
                    .entry(r.u32(0))
                    .or_default()
                    .push((r.u32(1), r.u32(2).max(1)))
            },
        )?;

        // Specialty: pack item -> bundles, bundle -> npcs.
        let mut bundles_of_item: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        each_row(
            conn,
            "SELECT item_id, specialty_bundle_id FROM specialty_bundle_items",
            |r| bundles_of_item.entry(r.u32(0)).or_default().push(r.u32(1)),
        )?;
        // The server keeps one bundle per NPC (the last row): that is what the NPC buys.
        let mut npc_bundle: FxHashMap<u32, u32> = FxHashMap::default();
        each_row(
            conn,
            "SELECT specialty_bundle_id, npc_id FROM specialty_npcs ORDER BY rowid",
            |r| {
                npc_bundle.insert(r.u32(1), r.u32(0));
            },
        )?;
        let mut npcs_of_bundle: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        for (&npc, &bundle) in &npc_bundle {
            npcs_of_bundle.entry(bundle).or_default().push(npc);
        }
        let mut bundle_prices: Vec<(u32, u32, u32)> = Vec::new();
        each_row(
            conn,
            if crate::sql::has_column(conn, "items", "refund") {
                "SELECT b.item_id, b.specialty_bundle_id, b.profit, b.ratio, COALESCE(i.refund, 0) \
             FROM specialty_bundle_items b LEFT JOIN items i ON i.id = b.item_id ORDER BY b.id"
            } else {
                "SELECT b.item_id, b.specialty_bundle_id, b.profit, b.ratio, \
             COALESCE(gp.refund, 0) \
             FROM specialty_bundle_items b LEFT JOIN (SELECT item_id, MIN(price) AS price, MIN(refund) AS refund FROM item_prices WHERE currency_id = 0 GROUP BY item_id) gp ON gp.item_id = b.item_id ORDER BY b.id"
            },
            |r| bundle_prices.push((r.u32(0), r.u32(1), base_price(r.u32(2), r.u32(3), r.u32(4)))),
        )?;
        // the server keeps the last row of an (item, bundle) pair
        bundle_prices.reverse();
        bundle_prices.sort_by_key(|p| (p.0, p.1));
        bundle_prices.dedup_by_key(|p| (p.0, p.1));
        let mut zone_pairs: Vec<(u32, u32, u32, u32)> = Vec::new();
        each_row(
            conn,
            // 10.x specialties carry no per-route ratio or profit: 100 % and no bonus
            if crate::sql::has_column(conn, "specialties", "ratio") {
                "SELECT row_zone_group_id, col_zone_group_id, ratio, profit FROM specialties"
            } else {
                "SELECT row_zone_group_id, col_zone_group_id, 100, 0 FROM specialties"
            },
            |r| zone_pairs.push((r.u32(0), r.u32(1), r.u32(2), r.u32(3))),
        )?;
        zone_pairs.sort_by_key(|p| (p.0, p.1));
        zone_pairs.dedup_by_key(|p| (p.0, p.1));
        let buyers: FxHashMap<u32, Box<[u32]>> = bundles_of_item
            .iter()
            .filter_map(|(item, bundles)| {
                let mut npcs: Vec<u32> = bundles
                    .iter()
                    .filter_map(|b| npcs_of_bundle.get(b))
                    .flatten()
                    .copied()
                    .collect();
                npcs.sort_unstable();
                npcs.dedup();
                (!npcs.is_empty()).then(|| (*item, npcs.into_boxed_slice()))
            })
            .collect();

        // Benches per craft.
        let mut benches: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        for &(doodad, craft) in bench_pairs {
            benches.entry(craft).or_default().push(doodad);
        }
        for (id, _, _, req, _) in &head {
            if *req != 0 {
                benches.entry(*id).or_default().push(*req);
            }
        }

        let crafts: Vec<(u32, Craft)> = head
            .into_iter()
            .filter_map(|(id, name, skill_id, req_doodad, actability_limit)| {
                let product = *products.get(&id)?;
                let mut b = benches.remove(&id).unwrap_or_default();
                b.sort_unstable();
                b.dedup();
                Some((
                    id,
                    Craft {
                        id,
                        name,
                        skill_id,
                        labor: skills.get(skill_id).map_or(0, |s| s.labor),
                        product,
                        materials: materials.remove(&id).unwrap_or_default().into_boxed_slice(),
                        is_pack: bundles_of_item.contains_key(&product.0),
                        req_doodad,
                        benches: b.into_boxed_slice(),
                        req_level: skill_level.get(&skill_id).copied().unwrap_or(0),
                        actability_group: skill_group.get(&skill_id).copied().unwrap_or(0),
                        actability_limit,
                    },
                ))
            })
            .collect();
        let mut eco = Economy::from_parts(IdMap::build(crafts), buyers);
        eco.bundle_prices = bundle_prices;
        eco.npc_bundle = npc_bundle;
        eco.zone_pairs = zone_pairs;
        Ok(eco)
    }

    fn from_parts(crafts: IdMap<Craft>, buyers: FxHashMap<u32, Box<[u32]>>) -> Economy {
        let mut at: FxHashMap<u32, Vec<u32>> = FxHashMap::default();
        for c in crafts.values().iter().filter(|c| c.is_pack) {
            for b in c.benches.iter() {
                at.entry(*b).or_default().push(c.id);
            }
        }
        let pack_at = at
            .into_iter()
            .map(|(b, mut v)| {
                v.sort_unstable();
                v.dedup();
                (b, v.into_boxed_slice())
            })
            .collect();
        Economy {
            crafts,
            pack_at,
            buyers,
            ..Economy::default()
        }
    }

    /// One recipe.
    pub fn craft(&self, id: u32) -> Option<&Craft> {
        self.crafts.get(id)
    }

    /// Number of recipes with a product.
    pub fn craft_count(&self) -> usize {
        self.crafts.len()
    }

    /// Trade-pack recipes offered by a bench doodad template.
    pub fn pack_crafts_at(&self, doodad_template: u32) -> &[u32] {
        self.pack_at.get(&doodad_template).map_or(&[], |v| &v[..])
    }

    /// NPC templates that buy `pack_item`.
    pub fn specialty_buyers(&self, pack_item: u32) -> &[u32] {
        self.buyers.get(&pack_item).map_or(&[], |v| &v[..])
    }

    /// What a specialty NPC pays for `pack_item` in copper(-equivalent), at the server's best
    /// price ratio (a zone starts there; every pack sold in a zone group lowers it for a while, down
    /// to 70 %): `(floor(profit * ratio / 1000) + refund) * 130 / 100 * 1.05`, the numbers of
    /// `specialty_bundle_items` and the item's refund as the server's `SellSpecialty` reads them.
    /// `None` when `buyer_npc` does not buy that pack (the server refuses the sale).
    ///
    /// The AAEmu server prices a pack by pack and buyer alone, so `from_zone` (the zone group the
    /// pack comes from, `items.specialty_zone_id`, 0 = unknown) does not change the estimate; the
    /// route table of the real game is in [`Economy::zone_pair`].
    pub fn pack_price_estimate(
        &self,
        pack_item: u32,
        buyer_npc: u32,
        _from_zone: u32,
    ) -> Option<u32> {
        let bundle = *self.npc_bundle.get(&buyer_npc)?;
        let i = self
            .bundle_prices
            .binary_search_by_key(&(pack_item, bundle), |p| (p.0, p.1))
            .ok()?;
        let base = self.bundle_prices[i].2 as f32;
        let at_ratio = base * (MAX_SPECIALTY_RATIO / 100.0);
        Some((at_ratio + at_ratio * SPECIALTY_INTEREST).round_ties_even() as u32)
    }

    /// The `specialties` row between two zone groups: `(ratio, profit)`. The server loads the
    /// table but does not price by it.
    pub fn zone_pair(&self, from_group: u32, to_group: u32) -> Option<(u32, u32)> {
        let i = self
            .zone_pairs
            .binary_search_by_key(&(from_group, to_group), |p| (p.0, p.1))
            .ok()?;
        Some((self.zone_pairs[i].2, self.zone_pairs[i].3))
    }

    /// The lowest character level any pack craft needs (0 when none or unknown).
    pub fn min_pack_level(&self) -> u8 {
        self.crafts
            .values()
            .iter()
            .filter(|c| c.is_pack)
            .map(|c| c.req_level)
            .min()
            .unwrap_or(0)
    }

    /// Pack items that at least one specialty NPC buys.
    pub fn pack_items(&self) -> impl Iterator<Item = u32> + '_ {
        self.buyers.keys().copied()
    }

    /// Every pack recipe.
    pub fn pack_crafts(&self) -> impl Iterator<Item = &Craft> {
        self.crafts.values().iter().filter(|c| c.is_pack)
    }

    /// Heap bytes held.
    pub fn bytes(&self) -> usize {
        let crafts: usize = self
            .crafts
            .values()
            .iter()
            .map(|c| c.name.len() + c.materials.len() * 8 + c.benches.len() * 4)
            .sum();
        let idx = |m: &FxHashMap<u32, Box<[u32]>>| {
            m.values().map(|v| v.len() * 4 + 16 + 8).sum::<usize>()
        };
        self.crafts.shallow_bytes()
            + crafts
            + idx(&self.pack_at)
            + idx(&self.buyers)
            + self.bundle_prices.capacity() * 12
            + self.npc_bundle.capacity() * 16
            + self.zone_pairs.capacity() * 16
    }
}

/// `GetBasePriceForSpecialty`: `floor(profit * (ratio / 1000)) + refund`, in `f32` like the server.
fn base_price(profit: u32, ratio: u32, refund: u32) -> u32 {
    let scaled = (profit as f32 * (ratio as f32 / 1000.0)).floor();
    scaled as u32 + refund
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill::SkillInfo;

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE crafts (id INT, title TEXT, skill_id INT, req_doodad_id INT, actability_limit INT);
             CREATE TABLE unit_reqs (id INT, owner_id INT, owner_type TEXT, kind_id INT, value1 INT, value2 INT);
             CREATE TABLE skills (id INT, actability_group_id INT);
             CREATE TABLE items (id INT, refund INT);
             CREATE TABLE specialties (id INT, row_zone_group_id INT, col_zone_group_id INT, ratio INT, profit INT);
             CREATE TABLE craft_products (id INT, craft_id INT, item_id INT, amount INT);
             CREATE TABLE craft_materials (id INT, craft_id INT, item_id INT, amount INT);
             CREATE TABLE specialty_bundle_items (id INT, item_id INT, specialty_bundle_id INT, profit INT, ratio INT);
             CREATE TABLE specialty_npcs (id INT, name TEXT, npc_id INT, specialty_bundle_id INT);
             INSERT INTO crafts VALUES (6226, 'pack', 16766, 4233, 0), (6227, 'other', 0, 0, 20000), (6228, 'no product', 0, 0, 0);
             INSERT INTO unit_reqs VALUES (1, 16766, 'Skill', 1, 10, 0), (2, 16766, 'Skill', 27, 99, 0), (3, 5, 'QuestComponent', 1, 40, 0);
             INSERT INTO skills VALUES (16766, 31), (16767, 0);
             INSERT INTO items VALUES (31855, 10000), (31856, 0);
             INSERT INTO specialties VALUES (1, 2, 15, 5500, 11000), (2, 2, 16, 2000, 4000);
             INSERT INTO craft_products VALUES (6989, 6226, 31855, 1), (6990, 6226, 5, 1), (6991, 6227, 100, 3);
             INSERT INTO craft_materials VALUES (1, 6226, 4747, 1), (2, 6226, 30901, 50), (3, 6226, 14627, 8);
             INSERT INTO specialty_bundle_items VALUES (1, 31855, 15, 12100, 3273), (2, 31855, 16, 100, 1000), (3, 31855, 99, 1, 1000), (4, 31856, 15, 50, 2000);
             INSERT INTO specialty_npcs VALUES (1, 'a', 14766, 15), (2, 'b', 14767, 16), (3, 'c', 14766, 16), (4, 'd', 14769, 15);",
        )
        .unwrap();
        conn
    }

    fn skills() -> Skills {
        Skills::from_infos(vec![SkillInfo {
            id: 16766,
            ability_id: 0,
            ability_level: 0,
            cooldown_time: 0,
            casting_time: 0,
            mana_cost: 0,
            mana_level_md: 0.0,
            min_range: 0,
            max_range: 0,
            target_type_id: 0,
            target_relation_id: 0,
            labor: 50,
            auto_learn: false,
            need_learn: false,
            name: "".into(),
        }])
    }

    #[test]
    fn packs_are_priced_like_the_servers_sale() {
        let conn = fixture();
        let eco = Economy::load(&conn, &Texts::empty(), &skills(), &[]).unwrap();
        // NPC 14769 buys bundle 15: floor(12100 * 3.273) + 10000 = 49603, times 1.30 times 1.05
        assert_eq!(eco.pack_price_estimate(31855, 14769, 0), Some(67_708));
        // the buyer of bundle 16 pays profit 100 at ratio 1.0: 100 + 10000 refund
        assert_eq!(eco.pack_price_estimate(31855, 14767, 0), Some(13_786));
        // a NPC listed twice buys what its last row says (bundle 16)
        assert_eq!(eco.pack_price_estimate(31855, 14766, 0), Some(13_786));
        // the route does not change the AAEmu server's price
        assert_eq!(eco.pack_price_estimate(31855, 14769, 22), Some(67_708));
        // an NPC that does not buy the pack, an unknown pack, an unknown NPC
        assert_eq!(eco.pack_price_estimate(31856, 14767, 0), None);
        assert_eq!(eco.pack_price_estimate(999, 14769, 0), None);
        assert_eq!(eco.pack_price_estimate(31855, 1, 0), None);
        // a pack without a refund row: profit 50 at ratio 2.0 -> 100 * 1.3 * 1.05 = 136.5
        let cheap = eco.pack_price_estimate(31856, 14769, 0).unwrap();
        assert!((136..=137).contains(&cheap), "{cheap}");
        assert_eq!(eco.zone_pair(2, 15), Some((5500, 11000)));
        assert_eq!(eco.zone_pair(15, 2), None);
    }

    #[test]
    fn pack_craft_bench_materials_and_buyers() {
        let conn = fixture();
        // Craft 6226 is offered at 4233 via a craft pack and at 4300 by a craft start.
        let eco = Economy::load(
            &conn,
            &Texts::empty(),
            &skills(),
            &[(4233, 6226), (4300, 6226), (4300, 6227)],
        )
        .unwrap();
        let c = eco.craft(6226).unwrap();
        assert!(c.is_pack);
        assert_eq!(c.product, (31855, 1));
        assert_eq!(&c.materials[..], &[(4747, 1), (30901, 50), (14627, 8)]);
        assert_eq!(c.labor, 50);
        // the craft skill needs character level 10 and trains the commerce vocation
        assert_eq!(
            (c.req_level, c.actability_group, c.actability_limit),
            (10, 31, 0)
        );
        assert_eq!(&c.benches[..], &[4233, 4300]);
        assert_eq!(c.req_doodad, 4233);
        assert_eq!(eco.pack_crafts_at(4233), &[6226]);
        assert_eq!(eco.pack_crafts_at(4300), &[6226]);
        assert!(eco.pack_crafts_at(1).is_empty());
        assert_eq!(eco.specialty_buyers(31855), &[14766, 14767, 14769]);
        assert!(eco.specialty_buyers(5).is_empty());
        // 6227 is no pack, 6228 has no product row.
        assert!(!eco.craft(6227).unwrap().is_pack);
        let other = eco.craft(6227).unwrap();
        assert_eq!(
            (
                other.req_level,
                other.actability_group,
                other.actability_limit
            ),
            (0, 0, 20000)
        );
        assert_eq!(eco.craft(6227).unwrap().product, (100, 3));
        assert!(eco.craft(6228).is_none());
        assert_eq!(eco.craft_count(), 2);
    }
}
