//! War-zone awareness: the everyday goals do not choose a destination in a zone group that is at
//! war (`[events] avoid_war_zones`).
//!
//! The server tells every bot the conflict state of each zone group
//! ([`BotState::at_war`](super::state::BotState::at_war)). A zone group at war is a battlefield:
//! the factions fight there, and a quester walking in meets players who attack it. The grind,
//! wander and quest goals therefore leave out destinations in such a zone group
//! ([`Ctx::in_war_zone`]). The goals that go to war on purpose (`warzone`, `gank`) never ask.
//!
//! This is only about where a bot *chooses* to go. A bot that stands in a zone group when war
//! breaks out, or whom a fight carries in, is not moved out.

use aa_world::World;

use super::ctx::Ctx;
use super::state::BotState;
use crate::config::EventsCfg;

/// Whether (x, y) lies in a zone group at war, as far as `state` knows. `false` when the zone
/// grid is not baked, the position is outside it, or the server never announced the zone group.
pub fn at_war(world: &World, state: &BotState, x: f32, y: f32) -> bool {
    let Some(key) = world.zone_key_at(x, y) else {
        return false;
    };
    world
        .travel()
        .zone_groups
        .of_key(key)
        .and_then(|g| u16::try_from(g).ok())
        .is_some_and(|g| state.at_war(g))
}

/// Whether the bot keeps out of zones at war: `[events]` is on and so is `avoid_war_zones`.
pub fn avoiding(cfg: &EventsCfg) -> bool {
    cfg.enabled && cfg.avoid_war_zones
}

/// Whether (x, y) lies outside the zones of `[bots] zones` (never when the list is empty; a
/// position outside the baked zone grid counts as outside a non-empty list).
pub fn outside_zones(world: &World, zones: &[u32], x: f32, y: f32) -> bool {
    !zones.is_empty() && !world.zone_key_at(x, y).is_some_and(|k| zones.contains(&u32::from(k)))
}

impl Ctx<'_> {
    /// Whether the everyday goals must not choose (x, y) as a destination: the bot avoids war
    /// zones and (x, y) lies in a zone group at war, or (x, y) is outside the zones of
    /// `[bots] zones`.
    pub fn in_war_zone(&self, x: f32, y: f32) -> bool {
        (avoiding(&self.cfg.events) && at_war(self.world, self.state, x, y))
            || outside_zones(self.world, &self.cfg.bots.zones, x, y)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_switches_both_have_to_be_on() {
        let mut cfg = EventsCfg::default();
        assert!(!avoiding(&cfg), "the feature is off by default");
        cfg.enabled = true;
        assert!(avoiding(&cfg));
        cfg.avoid_war_zones = false;
        assert!(!avoiding(&cfg));
    }
}
