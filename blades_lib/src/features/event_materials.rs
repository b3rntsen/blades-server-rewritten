//! Which crafting material an event-quest tier pays at a character level
//! (reports #362, #367).
//!
//! THE BUG. `event_quests.json` carries five reward ladders per event, one per sigil
//! level band (report #333), and each band holds whatever material its captured
//! instance happened to pay. But retail did not pick the material per band. It paid
//! the material of the claimant's OWN level: inside the 26-35 band a level-26
//! character got Quicksilver or Moonstone and a level-33 one Ebony; inside 16-25 a
//! level-17 got Orichalcum and a level-21 Dwarven. Serving the band's one captured
//! material gave a level-26 player Ebony ingots (#362), and derived bands copied a
//! material from a neighbouring band outright.
//!
//! THE RULE, measured over 409 distinct retail observations (29 templates, 67
//! characters, levels 10-100) with none contradicting it — see
//! `event_material_ladders.json` and `script/extract_event_material_ladders.py`:
//!
//! * metals step at the APK's gear unlock levels (1, 8, 13, 18, 23, 28, 33, 39, 45);
//! * soul gems step at their own measured levels (Petty → Glorious, 28/35/38/47/49/61;
//!   Grand at 49 is HauDrauf's, #370 — no capture at 49);
//! * healing potions and the gem bonuses follow the sigil band (16/26/36/46).
//!
//! A reward keeps its quantity; only the item changes.

use std::collections::{HashMap, HashSet};
use std::sync::OnceLock;

use serde::Deserialize;
use uuid::Uuid;

use crate::economy::RewardGrant;

static LADDERS_RAW: &str = include_str!("../event_material_ladders.json");

/// One material line: the item a character of at least `min_level` is paid, ascending.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MaterialLadder {
    pub name: String,
    pub steps: Vec<LadderStep>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LadderStep {
    pub min_level: i64,
    pub item: Uuid,
}

impl MaterialLadder {
    pub fn contains(&self, item: &Uuid) -> bool {
        self.steps.iter().any(|s| s.item == *item)
    }

    /// The step for `level`: the highest one it has reached, or the first below them all.
    pub fn item_at(&self, level: i64) -> Uuid {
        self.steps
            .iter()
            .filter(|s| s.min_level <= level)
            .max_by_key(|s| s.min_level)
            .or_else(|| self.steps.iter().min_by_key(|s| s.min_level))
            .map(|s| s.item)
            .expect("a ladder has steps")
    }
}

#[derive(Deserialize)]
struct LadderFile {
    ladders: Vec<MaterialLadder>,
}

/// The compiled-in ladders. Empty only if the file fails to parse, which a test pins.
pub fn ladders() -> &'static [MaterialLadder] {
    static TABLE: OnceLock<Vec<MaterialLadder>> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str::<LadderFile>(LADDERS_RAW)
            .map(|f| f.ladders.into_iter().filter(|l| !l.steps.is_empty()).collect())
            .unwrap_or_default()
    })
}

/// The ladder `item` climbs, given every item its template pays in the same slot
/// (`rewards[]` or `finalReward`) across all level bands.
///
/// Silver, Orichalcum, Dwarven, Ebony, Daedra Heart and Diamond sit on two lines
/// each. The template's other items in that slot say which line it is on — every
/// shipped slot that holds one of them also holds a line-specific item (its level-46
/// band's Dragon Bones or Dragon Scales, say). A tie keeps the first line.
pub fn ladder_for(item: &Uuid, slot: &HashSet<Uuid>) -> Option<&'static MaterialLadder> {
    let candidates: Vec<&MaterialLadder> = ladders().iter().filter(|l| l.contains(item)).collect();
    if candidates.len() <= 1 {
        return candidates.first().copied();
    }
    let unique_hits = |l: &MaterialLadder| {
        slot.iter()
            .filter(|x| l.contains(x))
            .filter(|x| !candidates.iter().any(|o| !std::ptr::eq(*o, l) && o.contains(x)))
            .count()
    };
    let best = candidates.iter().map(|l| unique_hits(l)).max().unwrap_or(0);
    candidates.iter().copied().find(|l| unique_hits(l) == best)
}

/// Swap every laddered material in `grant` for its step at `level`, keeping the
/// quantity. Anything not on a ladder (currencies, lumber, limestone) is untouched.
pub fn scale_to_level(grant: &mut RewardGrant, level: i64, slot: &HashSet<Uuid>) {
    if !grant.stackable_items.keys().any(|id| ladder_for(id, slot).is_some()) {
        return;
    }
    let mut out: HashMap<Uuid, u64> = HashMap::with_capacity(grant.stackable_items.len());
    for (id, n) in grant.stackable_items.drain() {
        let id = ladder_for(&id, slot).map_or(id, |l| l.item_at(level));
        *out.entry(id).or_insert(0) += n;
    }
    grant.stackable_items = out;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(s: &str) -> Uuid {
        s.parse().unwrap()
    }
    const SILVER: &str = "b74a5c55-a687-4604-aa59-ba3ddfddcd2a";
    const ORICHALCUM: &str = "74f091b5-fd88-464b-a98a-f60a5e8a0f25";
    const DWARVEN: &str = "f11fb90b-b441-4d72-a33f-50d14d3d6778";
    const QUICKSILVER: &str = "e80bee76-f92c-4005-9eff-20d1e8c64d24";
    const MOONSTONE: &str = "4312b0ed-e397-4815-9693-a511ecda71de";
    const EBONY: &str = "75112030-b248-49b0-9c70-0da8dea150d1";
    const DAEDRA_HEART: &str = "f9181a67-b094-4c37-a145-ced9dfe610d6";
    const DRAGON_BONES: &str = "d523932f-8c7f-4192-9112-5dbd60883c2b";
    const DRAGON_SCALES: &str = "34ddfe0e-5119-4e52-8eef-a77b6bc810a7";
    const LUMBER: &str = "e7193116-d761-479b-8a20-5633737977f5";

    #[test]
    fn the_compiled_in_ladders_parse() {
        assert_eq!(ladders().len(), 6, "two metal lines, soul gems, potions, two gem lines");
        for l in ladders() {
            let mins: Vec<i64> = l.steps.iter().map(|s| s.min_level).collect();
            assert!(mins.windows(2).all(|w| w[0] < w[1]), "{}: steps must ascend {mins:?}", l.name);
            assert_eq!(mins[0], 1, "{}: the first step covers level 1", l.name);
        }
    }

    /// The metal line is the APK's gear unlock ladder.
    #[test]
    fn metals_step_at_the_gear_unlock_levels() {
        let slot: HashSet<Uuid> = [id(DRAGON_BONES)].into();
        let line = ladder_for(&id(EBONY), &slot).expect("ebony is laddered");
        for (level, want) in [
            (8, SILVER), (12, SILVER), (13, ORICHALCUM), (17, ORICHALCUM), (18, DWARVEN),
            (22, DWARVEN), (23, QUICKSILVER), (32, "8ef9f10c-3c46-492c-9a00-29fd1626d85e"),
            (33, EBONY), (38, EBONY), (39, DAEDRA_HEART), (44, DAEDRA_HEART),
            (45, DRAGON_BONES), (100, DRAGON_BONES),
        ] {
            assert_eq!(line.item_at(level), id(want), "level {level}");
        }
    }

    /// A shared item follows its template's other items: the same Ebony is a
    /// Quicksilver-line ingot next to Dragon Bones and a Moonstone-line one next to
    /// Dragon Scales.
    #[test]
    fn a_shared_metal_follows_its_slot() {
        let bones: HashSet<Uuid> = [id(EBONY), id(DRAGON_BONES)].into();
        let scales: HashSet<Uuid> = [id(EBONY), id(DRAGON_SCALES)].into();
        assert_eq!(ladder_for(&id(EBONY), &bones).unwrap().item_at(26), id(QUICKSILVER));
        assert_eq!(ladder_for(&id(EBONY), &scales).unwrap().item_at(26), id(MOONSTONE));
    }

    /// Report #370: Grand from 49 (HauDrauf; no capture at 49), Exceptional at the
    /// captured 47-48 just below it.
    #[test]
    fn grand_soul_gems_start_at_49() {
        let exceptional = id("3932e499-441e-4c6d-b671-9a03131ebe6f");
        let grand = id("68d7941e-8c8d-47bf-9f66-becb058f1817");
        let line = ladder_for(&grand, &HashSet::new()).expect("soul gems are laddered");
        assert_eq!(line.item_at(48), exceptional);
        assert_eq!(line.item_at(49), grand);
    }

    #[test]
    fn scaling_keeps_quantities_and_leaves_unladdered_items_alone() {
        let slot: HashSet<Uuid> = [id(EBONY), id(DRAGON_BONES)].into();
        let mut g = RewardGrant::default();
        g.stackable_items.insert(id(EBONY), 96);
        g.stackable_items.insert(id(LUMBER), 372);
        g.currencies.insert(id("c64bcb53-41f4-41ba-892a-fe2cca423caa"), 16);
        scale_to_level(&mut g, 17, &slot);
        assert_eq!(g.stackable_items.get(&id(ORICHALCUM)), Some(&96));
        assert_eq!(g.stackable_items.get(&id(LUMBER)), Some(&372));
        assert_eq!(g.stackable_items.len(), 2);
        assert_eq!(g.currencies.len(), 1);
    }
}
