//! Item templates whose properties are fixed by the template itself (#8).
//!
//! THE BUG. The Sigil offer `SigilShop_Ultimate_RingOfShock` grants the Master
//! Ring of Shock, whose APK template already carries six `mandatory_properties`
//! (three tier-10 Shock bonuses and three tier-1 ring bonuses). The offer authors
//! no grade and no GRADING, and the purchase path read that as "a bare ring,
//! roll it" — so every buyer got `grade: 4` and three extra GRADING properties on
//! top of the six the template already gives, stacking the rank bonuses to +9.
//!
//! THE RULE. An item whose template authors its properties arrives exactly as
//! the template defines it: no rolled grade or GRADING, and no ENCHANTING rolled
//! at grant time. Measured against the retail data we hold: the 2026-06-07
//! capture snapshot has 25 distinct retail instances of mandatory-property
//! rings (979 responses; Ring of Shock 4, Warlock's Ring 12, Ring of Dremora 7,
//! Ring of Namira 3, Band of the Wraith 1, Jailmaster's Ring 1) and 0 of them
//! carry a grade or a GRADING property. Four Ring of Dremora instances carry
//! ENCHANTING — a player's own enchant, which stays allowed (`craft.rs`); only
//! the rolls the server makes when it GRANTS the item are skipped.
//!
//! The list is every template in the APK's `*TemplateList` with non-empty
//! `mandatory_properties` — 180 of them, of which 15 are jewellery. It is NOT the
//! `Item.Tag.Artifact` set (`store_bundles::ARTIFACT_TEMPLATES`, 24 entries):
//! the Ring of Shock is a fixed legendary without the artifact tag.

use std::collections::HashMap;
use std::sync::OnceLock;

use serde::Deserialize;
use uuid::Uuid;

static RAW: &str = include_str!("../data/mandatory_property_templates.json");

#[derive(Deserialize)]
struct File {
    templates: HashMap<Uuid, Template>,
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct Template {
    name: String,
    r#type: u64,
    #[serde(rename = "mandatoryProperties")]
    mandatory_properties: Vec<(Uuid, u64)>,
}

fn table() -> &'static HashMap<Uuid, Template> {
    static TABLE: OnceLock<HashMap<Uuid, Template>> = OnceLock::new();
    TABLE.get_or_init(|| {
        serde_json::from_str::<File>(RAW)
            .map(|f| f.templates)
            .unwrap_or_default()
    })
}

/// Whether this template authors its own properties, so nothing may be rolled
/// onto an instance of it.
pub fn has_mandatory_properties(template: Uuid) -> bool {
    table().contains_key(&template)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::combat::{artifact_properties, gamedata};

    const RING_OF_SHOCK: &str = "85c44edf-2fc6-4b4d-b8a9-3340f127f9f0";

    #[test]
    fn the_table_loads_every_mandatory_template() {
        let f: File = serde_json::from_str(RAW).expect("mandatory_property_templates.json parses");
        assert_eq!(f.templates.len(), 180);
        let jewellery = f
            .templates
            .values()
            .filter(|t| matches!(t.r#type, 10 | 11))
            .count();
        assert_eq!(jewellery, 15);
        let shock = &f.templates[&RING_OF_SHOCK.parse::<Uuid>().unwrap()];
        assert_eq!(shock.mandatory_properties.len(), 6);
        assert!(has_mandatory_properties(RING_OF_SHOCK.parse().unwrap()));
    }

    /// Identity test against the independently generated combat tables: every
    /// template those say has mandatory properties is in this list, with the
    /// same properties.
    #[test]
    fn agrees_with_the_combat_tables() {
        let uuids = gamedata::WEAPONS
            .iter()
            .map(|w| w.uuid)
            .chain(gamedata::ARMORS.iter().map(|a| a.uuid))
            .chain(gamedata::SHIELDS.iter().map(|s| s.uuid))
            .chain(["23607f09-a103-4ed3-a0de-33e0498f8018"]);
        let mut checked = 0;
        for uuid in uuids {
            let combat: Vec<(Uuid, u64)> = gamedata::mandatory_properties(uuid)
                .iter()
                .chain(artifact_properties::mandatory_properties(uuid))
                .map(|(p, t)| (p.parse().unwrap(), *t as u64))
                .collect();
            if combat.is_empty() {
                continue;
            }
            let ours = &table()
                .get(&uuid.parse::<Uuid>().unwrap())
                .unwrap_or_else(|| panic!("{uuid} has mandatory properties but is not listed"))
                .mandatory_properties;
            let mut a = combat.clone();
            let mut b = ours.clone();
            a.sort();
            b.sort();
            assert_eq!(a, b, "{uuid}");
            checked += 1;
        }
        assert!(checked >= 100, "only {checked} templates cross-checked");
    }

    #[test]
    fn an_ordinary_ring_is_not_fixed() {
        // Brass Pearl Ring, an ordinary merchant blank.
        let ring: Uuid = "869bf6f4-f7fb-43cf-b264-02b84f9a5425".parse().unwrap();
        assert!(!has_mandatory_properties(ring));
    }
}
