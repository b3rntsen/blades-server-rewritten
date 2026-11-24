//! The December 2026 gift calendar (tracker #499): an advent calendar, one
//! gift a day from 1 to 24 December, and one more on New Year's Eve.
//!
//! Proposed by HauDrauf (message 3687) and approved in full by the owner on
//! 2026-11-23. NOT retail: retail never ran one. Every number in it is his.
//!
//! * **Every day** — the day's decorations (1-3 kinds, none of them sold in a
//!   shop) and, alternating day by day, the contents of the Incredible
//!   Treasure Pack (odd days) or the Heroic Enchanter Pack (even days).
//! * **6 December** — also 600 Gems and one Divine Dark Seducer armour set.
//! * **24 December** — also 1,200 Gems and three of that set.
//! * **25-30 December** — no gift.
//!
//! # Why it is compiled in
//!
//! `deploy/static` does not ship by merging, and a gift in `gift_overrides` is
//! live for every player the moment it is written. This calendar is data in the
//! binary with its dates in it: merging and deploying it before 1 December
//! changes nothing, because no door is open until its own day begins (UTC).
//!
//! # The windows
//!
//! A door is claimable on its own day only: from 00:00:00 to 23:59:59 UTC.
//! A day that is missed is gone — HauDrauf, 2026-11-24 (message 3924): "Alle
//! Türchen gelten nur für den Tag, ich möchte regelmäßiges spielen belohnen."
//! The New Year's Eve gift likewise only on the 31st.
//!
//! # One door a day per account
//!
//! Each day's door goes to ONE character of an account: the first one the
//! server sees that day (HauDrauf, same message: "Die Geschenke gibt es nur für
//! den Charakter der sich an dem Tag als erstes einloggt"). The account's other
//! alts are not offered that day's door. Which character that is, and how it is
//! recorded, is the server's half: `server/src/december.rs`. Here a door is
//! still claimable once per character, which on top of that rule makes it once
//! per account.
//!
//! # The armour
//!
//! Gear takes backpack slots, so the two armour gifts carry a backpack rule
//! ([`crate::features::gift_capacity`], HauDrauf's rule): the claim grants only
//! the pieces that keep the backpack at or below 95 % of its capacity, whole
//! sets first, and the rest is paid from 25 December 00:00 UTC once it fits.
//!
//! # The data
//!
//! `december_2026_calendar.json`, built by `script/build_december_2026_calendar.py`
//! from his proposal verbatim, the game definitions and the APK's English and
//! German names. The tests below pin it against his proposal, the game data
//! (`deploy/static/parsed.json`) and the shipped pack definitions.

use std::sync::LazyLock;

use serde::Deserialize;
use uuid::Uuid;

use crate::economy::GEMS;
use crate::features::gift_capacity::GearCapRule;
use crate::static_data::{GiftDef, GiftItem};

static RAW: &str = include_str!("../december_2026_calendar.json");

const SECS_PER_DAY: i64 = 86_400;

/// 2026-12-01 00:00:00 UTC, when door 1 opens.
pub const DECEMBER_1_SECS: i64 = 1_796_083_200;

/// 2026-12-25 00:00:00 UTC, from when armour that did not fit is paid out.
pub const DEFERRED_ARMOR_FROM_SECS: i64 = DECEMBER_1_SECS + 24 * SECS_PER_DAY;

/// How long every door stays claimable: its own UTC day, no longer.
pub const DOOR_WINDOW_SECS: i64 = SECS_PER_DAY;

/// The share of the backpack the gifted armour may fill (HauDrauf's rule).
pub const ARMOR_FILL_PERCENT: u64 = 95;

/// The gift id of the door for `day` of December 2026. Fixed, so a claim made
/// before a restart or a redeploy is still a claim after it.
pub const fn door_gift_id(day: u32) -> Uuid {
    Uuid::from_u128(0xad7e2026_1200_4d0c_9a00_000000000000 + day as u128)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawLine {
    item_template_id: Uuid,
    quantity: u64,
}

#[derive(Deserialize)]
struct RawPack {
    items: Vec<RawLine>,
}

#[derive(Deserialize)]
struct RawPacks {
    #[serde(rename = "incredibleTreasure")]
    incredible_treasure: RawPack,
    #[serde(rename = "heroicEnchanter")]
    heroic_enchanter: RawPack,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawPiece {
    item_template_id: Uuid,
}

#[derive(Deserialize)]
struct RawArmorSet {
    pieces: Vec<RawPiece>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawDoor {
    day: u32,
    decorations: Vec<RawLine>,
    pack: String,
    gems: u64,
    armor_sets: u64,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RawCalendar {
    packs: RawPacks,
    armor_set: RawArmorSet,
    doors: Vec<RawDoor>,
}

/// One day of the calendar.
#[derive(Debug, Clone)]
pub struct Door {
    /// Day of December 2026 (1-24, 31).
    pub day: u32,
    /// The gift as claimed: its id, contents, window and label.
    pub gift: GiftDef,
    /// Unix seconds the door opens (00:00 UTC on its day).
    pub opens_at_secs: i64,
    /// Unix seconds from which it can no longer be claimed.
    pub closes_at_secs: i64,
    /// How many armour sets the gift carries (0 on most days).
    pub armor_sets: u64,
}

impl Door {
    /// Whether the door can be opened at `now`.
    pub fn is_open(&self, now: i64) -> bool {
        self.opens_at_secs <= now && now < self.closes_at_secs
    }
}

/// The label a door carries: German as HauDrauf asked, with an English line.
pub fn door_label(day: u32) -> String {
    if day <= 24 {
        format!("Adventskalender Türchen Nr. {day} / Advent calendar door no. {day}")
    } else {
        "Silvester-Geschenk / New Year's Eve gift".to_string()
    }
}

fn line(item_template_id: Uuid, quantity: u64) -> GiftItem {
    GiftItem {
        item_template_id,
        quantity,
        arcane_tier: None,
        enchanting: Vec::new(),
    }
}

fn build() -> Vec<Door> {
    let raw: RawCalendar = serde_json::from_str(RAW).expect("december_2026_calendar.json is valid");
    raw.doors
        .iter()
        .map(|door| {
            let pack = match door.pack.as_str() {
                "incredibleTreasure" => &raw.packs.incredible_treasure,
                "heroicEnchanter" => &raw.packs.heroic_enchanter,
                other => panic!("december_2026_calendar.json: unknown pack {other}"),
            };
            // Decorations, the pack, Gems, then the armour — one line per
            // piece of the set, the quantity the number of sets.
            let mut items: Vec<GiftItem> = door
                .decorations
                .iter()
                .chain(&pack.items)
                .map(|l| line(l.item_template_id, l.quantity))
                .collect();
            if door.gems > 0 {
                items.push(line(GEMS, door.gems));
            }
            if door.armor_sets > 0 {
                items.extend(
                    raw.armor_set
                        .pieces
                        .iter()
                        .map(|p| line(p.item_template_id, door.armor_sets)),
                );
            }
            let opens_at_secs = DECEMBER_1_SECS + (i64::from(door.day) - 1) * SECS_PER_DAY;
            let closes_at_secs = opens_at_secs + DOOR_WINDOW_SECS;
            Door {
                day: door.day,
                gift: GiftDef {
                    global_gift_id: door_gift_id(door.day),
                    items,
                    chests: Vec::new(),
                    start_time: opens_at_secs,
                    // `can_claim` still accepts `now == end_time`, so the last
                    // claimable second is the one before the door closes.
                    end_time: closes_at_secs - 1,
                    claim_count_limit: 1,
                    description: Some(door_label(door.day)),
                },
                opens_at_secs,
                closes_at_secs,
                armor_sets: door.armor_sets,
            }
        })
        .collect()
}

static CALENDAR: LazyLock<Vec<Door>> = LazyLock::new(build);

/// Every door, in date order.
pub fn doors() -> &'static [Door] {
    &CALENDAR
}

/// The door whose gift id this is.
pub fn door(gift_id: Uuid) -> Option<&'static Door> {
    doors().iter().find(|d| d.gift.global_gift_id == gift_id)
}

/// The calendar gift with this id, whatever the date: the window is in the
/// definition and the claim checks it.
pub fn gift(gift_id: Uuid) -> Option<&'static GiftDef> {
    door(gift_id).map(|d| &d.gift)
}

/// The door that can be opened at `now`, if any: windows never overlap, so
/// this yields at most one.
pub fn open_doors(now: i64) -> impl Iterator<Item = &'static Door> {
    doors().iter().filter(move |d| d.is_open(now))
}

/// The backpack rule of a calendar gift: the two armour gifts carry one.
pub fn gear_cap_rule(gift_id: Uuid) -> Option<GearCapRule> {
    door(gift_id)
        .filter(|d| d.armor_sets > 0)
        .map(|_| GearCapRule {
            fill_percent: ARMOR_FILL_PERCENT,
            payable_from_secs: DEFERRED_ARMOR_FROM_SECS,
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::{self, ItemBucket, RewardGrant};
    use crate::features::gift_capacity;
    use crate::features::gifts::{self, GiftError};
    use crate::features::repair::RepairData;
    use crate::game_data::GameDataItem;
    use crate::user_data::{
        Backpack, CompleteCharacter, CompleteInventory, Item, ItemPropertiesAll, Loadout, Treasury,
    };
    use serde_json::Value;
    use std::collections::{BTreeMap, HashMap};

    // --- what he asked for, verbatim -------------------------------------

    /// HauDrauf's proposal, message 3687 on tracker #499, character for
    /// character: "Datum, Deko, Menge".
    const PROPOSAL: &str = "1.12. 93/3,2. 19/2, 3. 66/3, 4. 7/1, 5. 45/3 6. 53/3 50/3, 7. 24/3, \
        8. 75/3, 9.45/3, 10. 17/2, 11. 40/3, 12. 1/1,  13. 24/3, 14. 9/1, 15. 66/3, 16. 11/1, \
        17. 93/3, 18. 92/1, 19. 40/3, 20. 24/3, 21. 66/3, 22. 40/3, 23. 4/1, 24. 52/3 51/3 49/3, \
        31. 92/1 93/3 45/3";

    /// The same proposal read by eye, day by day: `(day, [(number, quantity)])`.
    /// Written out separately from the parser below so that neither a parsing
    /// slip nor a typing slip can pass unnoticed — the two must agree.
    const BY_EYE: [(u32, &[(u32, u64)]); 25] = [
        (1, &[(93, 3)]),
        (2, &[(19, 2)]),
        (3, &[(66, 3)]),
        (4, &[(7, 1)]),
        (5, &[(45, 3)]),
        (6, &[(53, 3), (50, 3)]),
        (7, &[(24, 3)]),
        (8, &[(75, 3)]),
        (9, &[(45, 3)]),
        (10, &[(17, 2)]),
        (11, &[(40, 3)]),
        (12, &[(1, 1)]),
        (13, &[(24, 3)]),
        (14, &[(9, 1)]),
        (15, &[(66, 3)]),
        (16, &[(11, 1)]),
        (17, &[(93, 3)]),
        (18, &[(92, 1)]),
        (19, &[(40, 3)]),
        (20, &[(24, 3)]),
        (21, &[(66, 3)]),
        (22, &[(40, 3)]),
        (23, &[(4, 1)]),
        (24, &[(52, 3), (51, 3), (49, 3)]),
        (31, &[(92, 1), (93, 3), (45, 3)]),
    ];

    /// His numbered list (messages 3168 and 3172), for the numbers he used:
    /// number, German, English, the game's name key, the template id.
    const NUMBERED: [(u32, &str, &str, &str, &str); 19] = [
        (
            1,
            "Vogelsangbaum",
            "Birdsong Tree",
            "Items.Name.Large_Pine_Tree_01",
            "8c8c1e22-c08f-48bf-a817-180e3143e345",
        ),
        (
            4,
            "Drachenschneekugel",
            "Dragon Snow Globe",
            "Items.Name.Large_DragonSnowGlobe",
            "56a1ce35-e3c7-47e7-993f-b63ff14e05b1",
        ),
        (
            7,
            "Mächtiger Saturalia-Baum",
            "Grand Saturalia Tree",
            "Items.Name.Large_SnowTree_01",
            "eded85b4-554f-4746-a27a-d624bc1da7cf",
        ),
        (
            9,
            "Saturalia-Hain",
            "Saturalia Grove",
            "Items.Name.Large_SnowTree_02",
            "708e0da9-d609-4b86-8ecb-4569961946ee",
        ),
        (
            11,
            "Waldbrunnen",
            "Sylvan Fountain",
            "Items.Name.Large_Pine_Fountain",
            "c064e1c9-69da-4235-80c4-7771f2d8e8bc",
        ),
        (
            17,
            "Brunnen des gefrorenen Herzens",
            "Frozen Heart Fountain",
            "Items.Name.Medium_Iceheart_Fountain",
            "a53e20cb-16e4-4fd5-b2e9-0e33a0a43fde",
        ),
        (
            19,
            "Saturalia-Baum",
            "Saturalia Tree",
            "Items.Name.Medium_LifeTree_01",
            "85164324-f536-40b6-ad86-559932529ae8",
        ),
        (
            24,
            "Amethyst-Leuchtfeuer",
            "Amethyst Beacon",
            "Items.Name.Small_Beacon_03",
            "8841cbbd-c404-4713-9f98-73b6c9730f13",
        ),
        (
            40,
            "Smaragd-Leuchtfeuer",
            "Emerald Beacon",
            "Items.Name.Small_Beacon_04",
            "f075e3ea-df65-4a65-afb4-9776ba0e35cf",
        ),
        (
            45,
            "Granat-Leuchtfeuer",
            "Garnet Beacon",
            "Items.Name.Small_Beacon_01",
            "04d05817-08f3-4368-8f95-64d617699b19",
        ),
        (
            49,
            "Eiselch",
            "Ice Elk",
            "Items.Name.Small_Ice_Elk",
            "b3c128e2-d811-4f85-a9b9-62aefdde092d",
        ),
        (
            50,
            "Eisziege",
            "Ice Goat",
            "Items.Name.Small_Ice_Goat",
            "ccf16487-e58c-44d5-91e4-d4b33029e1fa",
        ),
        (
            51,
            "Eiskhajiit",
            "Ice Khajiit",
            "Items.Name.Small_Ice_Kajit",
            "46d4de4d-2a4b-43c4-8623-14b305376d56",
        ),
        (
            52,
            "Eismammut",
            "Ice Mammoth",
            "Items.Name.Small_Ice_Mammoth",
            "c8e8e85c-2f1c-46f8-a047-9c328de69d20",
        ),
        (
            53,
            "Eiseinhorn",
            "Ice Unicorn",
            "Items.Name.Small_Ice_UniCorn",
            "b53d6051-48a4-410a-a4f6-e9edc3e624f6",
        ),
        (
            66,
            "Peridot-Leuchtfeuer",
            "Peridot Beacon",
            "Items.Name.Small_Beacon_02",
            "5b6ec37e-e969-4832-a1c4-9bb84a4227a7",
        ),
        (
            75,
            "Kleine Fontäne",
            "Small Fountain",
            "Items.Name.Small_CuteFountain",
            "20d2232a-4d86-4f52-be53-fb84179b53bc",
        ),
        (
            92,
            "Große Ulme",
            "Large Elm Tree",
            "Items.Name.Large_Tree1",
            "ea5b4d2c-61cc-49e6-acaa-e41d6dd4adc9",
        ),
        (
            93,
            "Leuchtfeuer",
            "Beacon",
            "Items.Name.Small_Beacon",
            "3bedef8a-2a4b-4953-81fa-0d38a8b97634",
        ),
    ];

    /// The Divine Dark Seducer set: armour, boots, gauntlets, helmet. The
    /// Divine set has no shield.
    const ARMOR_SET: [(&str, &str); 4] = [
        (
            "1063c3f9-1bbf-4962-b80d-fb9832075c1c",
            "Items.Name.DarkSeducerArmor",
        ),
        (
            "2c9ac3da-4a90-4fcb-bb59-4202fdecffe0",
            "Items.Name.DarkSeducerBoots",
        ),
        (
            "15f9963e-059c-464c-86ad-3a7eaa7cea9d",
            "Items.Name.DarkSeducerGauntlets",
        ),
        (
            "0c2869fb-3a95-45c6-8dcc-706d43f0fab0",
            "Items.Name.DarkSeducerHelmet",
        ),
    ];

    const INCREDIBLE_TREASURE_OFFER: &str = "c18127a3-57f0-4b5f-aa7d-54b3095e97ff";
    const HEROIC_ENCHANTER_OFFER: &str = "6ec8f67f-2cef-41aa-a7fc-f46237ae809c";

    fn id(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    /// `"1.12. 93/3,2. 19/2, …"` → `[(day, [(number, quantity)])]`. A day is a
    /// number followed by a dot; a decoration is `number/quantity`.
    fn parse_proposal(text: &str) -> Vec<(u32, Vec<(u32, u64)>)> {
        let text = text.replace("1.12.", "1.");
        let mut days: Vec<(u32, Vec<(u32, u64)>)> = Vec::new();
        let mut number = String::new();
        let mut pending: Option<u32> = None;
        for c in text.chars().chain(std::iter::once(' ')) {
            match c {
                '0'..='9' => number.push(c),
                '.' => {
                    days.push((number.parse().unwrap(), Vec::new()));
                    number.clear();
                }
                '/' => {
                    pending = Some(number.parse().unwrap());
                    number.clear();
                }
                _ => {
                    if let Some(deco) = pending.take() {
                        days.last_mut()
                            .unwrap()
                            .1
                            .push((deco, number.parse().unwrap()));
                    }
                    number.clear();
                }
            }
        }
        days
    }

    fn static_file(name: &str) -> Value {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static")
            .join(name);
        serde_json::from_slice(&std::fs::read(&path).unwrap_or_else(|e| panic!("{path:?}: {e}")))
            .unwrap_or_else(|e| panic!("{path:?}: {e}"))
    }

    /// The game's own item table, as the server loads it.
    fn game_items() -> HashMap<Uuid, GameDataItem> {
        serde_json::from_value(static_file("parsed.json")["items_template"].clone())
            .expect("items_template parses")
    }

    fn repair() -> RepairData {
        RepairData::from_json(
            &static_file("item_durability.json"),
            &static_file("repair_costs.json"),
        )
    }

    fn utc(day_of_december: u32, hour: i64) -> i64 {
        DECEMBER_1_SECS + (i64::from(day_of_december) - 1) * SECS_PER_DAY + hour * 3_600
    }

    fn reward_of(day: u32) -> RewardGrant {
        let gift = gift(door_gift_id(day)).expect("a door");
        gifts::build_gift_reward(gift, &game_items(), &repair())
    }

    fn pack_lines(offer: &str) -> BTreeMap<Uuid, u64> {
        static_file("global_shop_offer_contents.json")["offers"][offer]["contents"]
            .as_array()
            .unwrap_or_else(|| panic!("offer {offer} has contents"))
            .iter()
            .map(|c| {
                (
                    id(c["itemTemplateId"].as_str().unwrap()),
                    c["quantity"].as_u64().unwrap(),
                )
            })
            .collect()
    }

    // --- the dates --------------------------------------------------------

    /// The constants are the calendar dates they claim to be (UTC).
    #[test]
    fn the_dates_are_december_2026_utc() {
        // 2026-12-18 00:00 UTC is the anchor of the committed December holiday
        // event (`game_events.json`), an independent source for the epoch.
        let events = static_file("game_events.json");
        let long_night = events
            .as_array()
            .unwrap()
            .iter()
            .find(|e| e["questId"] == "b82b5384-5d68-4703-9450-1815f6d7d28b")
            .expect("the December holiday event");
        assert_eq!(
            long_night["recurrence"]["startTimeSecs"].as_i64().unwrap(),
            utc(18, 0)
        );
        assert_eq!(DECEMBER_1_SECS % SECS_PER_DAY, 0, "a UTC midnight");
        assert_eq!(DEFERRED_ARMOR_FROM_SECS, utc(25, 0));
        assert_eq!(DOOR_WINDOW_SECS, SECS_PER_DAY);
    }

    /// One door a day from 1 to 24 December and one on the 31st; nothing on
    /// 25-30 December.
    #[test]
    fn there_is_a_door_for_1_to_24_december_and_the_31st() {
        let days: Vec<u32> = doors().iter().map(|d| d.day).collect();
        let mut expected: Vec<u32> = (1..=24).collect();
        expected.push(31);
        assert_eq!(days, expected);
        for day in 25..=30 {
            assert!(
                gift(door_gift_id(day)).is_none(),
                "{day} December has no gift"
            );
        }
        let ids: std::collections::BTreeSet<Uuid> =
            doors().iter().map(|d| d.gift.global_gift_id).collect();
        assert_eq!(ids.len(), 25, "every door has its own gift id");
        for d in doors() {
            assert_eq!(d.gift.global_gift_id, door_gift_id(d.day));
            assert_eq!(d.opens_at_secs, utc(d.day, 0));
            assert_eq!(d.gift.claim_count_limit, 1);
            assert!(d.gift.chests.is_empty());
        }
    }

    // --- every day's contents, against his proposal ------------------------

    /// The parser and the by-eye reading of his message agree, so the table
    /// below is his proposal and not a misreading of it.
    #[test]
    fn the_proposal_reads_the_same_by_eye_and_by_parser() {
        let parsed = parse_proposal(PROPOSAL);
        assert_eq!(parsed.len(), 25);
        for ((day, decos), (eye_day, eye_decos)) in parsed.iter().zip(BY_EYE) {
            assert_eq!(*day, eye_day);
            assert_eq!(decos.as_slice(), eye_decos, "{day} December");
        }
    }

    /// THE TABLE. For every date of his proposal: exactly the decorations he
    /// named, in the quantities he named, and nothing else that is a
    /// decoration.
    #[test]
    fn every_day_holds_the_decorations_of_his_proposal() {
        let items = game_items();
        let template_of: BTreeMap<u32, Uuid> = NUMBERED.iter().map(|n| (n.0, id(n.4))).collect();
        for (day, decos) in parse_proposal(PROPOSAL) {
            let reward = reward_of(day);
            let wanted: BTreeMap<Uuid, u64> = decos
                .iter()
                .map(|(number, quantity)| (template_of[number], *quantity))
                .collect();
            assert_eq!(
                wanted.len(),
                decos.len(),
                "{day} December names a number twice"
            );
            let got: BTreeMap<Uuid, u64> = reward
                .stackable_items
                .iter()
                .filter(|(template, _)| items[*template].r#type == 4)
                .map(|(template, n)| (*template, *n))
                .collect();
            assert_eq!(got, wanted, "{day} December: decorations");
            assert!(
                (1..=3).contains(&got.len()),
                "{day} December: one to three decorations"
            );
        }
    }

    /// Each number he used resolves to exactly the decoration his list named:
    /// the template is a decoration in the game data, its name key is the one
    /// whose English and German strings are the names he was sent, and the
    /// calendar file records the same names.
    #[test]
    fn every_number_is_the_decoration_his_list_names() {
        let items = game_items();
        let raw: Value = serde_json::from_str(RAW).unwrap();
        let mut seen = 0;
        for door in raw["doors"].as_array().unwrap() {
            for deco in door["decorations"].as_array().unwrap() {
                let number = deco["number"].as_u64().unwrap() as u32;
                let (_, de, en, key, template) = NUMBERED
                    .iter()
                    .find(|n| n.0 == number)
                    .unwrap_or_else(|| panic!("number {number} is not one he used"));
                assert_eq!(deco["itemTemplateId"], *template, "number {number}");
                assert_eq!(deco["de"], *de, "number {number}");
                assert_eq!(deco["en"], *en, "number {number}");
                assert_eq!(deco["nameKey"], *key, "number {number}");
                let item = &items[&id(template)];
                assert_eq!(item.r#type, 4, "number {number} is a decoration");
                assert_eq!(item.name, *key, "number {number}: the game's name key");
                seen += 1;
            }
        }
        assert_eq!(seen, 30, "thirty decoration lines over the 25 days");
        // No two numbers share a template, and no two share a name.
        let templates: std::collections::BTreeSet<&str> = NUMBERED.iter().map(|n| n.4).collect();
        let names: std::collections::BTreeSet<&str> = NUMBERED.iter().map(|n| n.2).collect();
        assert_eq!((templates.len(), names.len()), (19, 19));
        // The two no player has ever held are marked for a device check.
        for door in raw["doors"].as_array().unwrap() {
            for deco in door["decorations"].as_array().unwrap() {
                let never = matches!(deco["number"].as_u64(), Some(11 | 50));
                assert_eq!(deco["neverOwned"].as_bool().unwrap_or(false), never);
            }
        }
    }

    // --- the packs ---------------------------------------------------------

    /// Odd days carry the Incredible Treasure Pack (`IAP_GemPack02_c`), even
    /// days the Heroic Enchanter Pack (`IAP_GemPack05_d`), each exactly as the
    /// shipped offer definition grants it — every line, every quantity,
    /// including the pack's 50,000 gold.
    #[test]
    fn the_packs_alternate_and_match_the_shipped_definitions() {
        let offers = static_file("global_shop_offer_contents.json");
        assert_eq!(
            offers["offers"][INCREDIBLE_TREASURE_OFFER]["editorName"],
            "IAP_GemPack02_c"
        );
        assert_eq!(
            offers["offers"][HEROIC_ENCHANTER_OFFER]["editorName"],
            "IAP_GemPack05_d_HeroicEnchanter"
        );
        let incredible = pack_lines(INCREDIBLE_TREASURE_OFFER);
        let enchanter = pack_lines(HEROIC_ENCHANTER_OFFER);
        assert_eq!((incredible.len(), enchanter.len()), (14, 11));
        // Spot values from the feasibility table sent to him.
        assert_eq!(incredible[&economy::GOLD], 50_000);
        assert_eq!(incredible[&id("d94bab85-53d5-4c9c-a637-acd94fc66c98")], 5);
        assert_eq!(enchanter[&id("a3351353-f613-4368-bac7-05783f857b07")], 48);

        let items = game_items();
        for door in doors() {
            let pack = if door.day % 2 == 1 {
                &incredible
            } else {
                &enchanter
            };
            let reward = reward_of(door.day);
            // Everything in the gift that is not a decoration, the day's Gems
            // or armour is the pack — and is the whole pack.
            let mut got: BTreeMap<Uuid, u64> = reward
                .stackable_items
                .iter()
                .filter(|(template, _)| items[*template].r#type != 4)
                .map(|(template, n)| (*template, *n))
                .collect();
            if let Some(gold) = reward.currencies.get(&economy::GOLD) {
                got.insert(economy::GOLD, *gold);
            }
            assert_eq!(&got, pack, "{} December", door.day);
        }
    }

    // --- gems and armour ---------------------------------------------------

    /// 600 Gems on 6 December, 1,200 on 24 December, none on any other day —
    /// and no Sigil in any gift.
    #[test]
    fn gems_are_600_on_the_6th_and_1200_on_the_24th() {
        for door in doors() {
            let reward = reward_of(door.day);
            let want = match door.day {
                6 => Some(600),
                24 => Some(1_200),
                _ => None,
            };
            assert_eq!(
                reward.currencies.get(&GEMS).copied(),
                want,
                "{} December",
                door.day
            );
            assert!(!reward.currencies.contains_key(&economy::SIGIL));
        }
    }

    /// The Divine Dark Seducer set — armour, boots, gauntlets, helmet — once
    /// on 6 December and three times on 24 December, each piece a real
    /// instance at full durability, and no gear on any other day.
    #[test]
    fn the_armour_is_one_set_on_the_6th_and_three_on_the_24th() {
        let items = game_items();
        let table = static_file("item_durability.json");
        for (template, key) in ARMOR_SET {
            assert_eq!(items[&id(template)].name, key);
            assert_eq!(items[&id(template)].r#type, 3, "armour");
            assert_eq!(
                economy::bucket_for_template(id(template), &items),
                Some(ItemBucket::Instance)
            );
            // Divine: 325 at tempering 0, where the ordinary set has 300.
            assert_eq!(table[template]["0"], 325.0, "{key} is the Divine piece");
        }
        for door in doors() {
            let reward = reward_of(door.day);
            let sets = match door.day {
                6 => 1,
                24 => 3,
                _ => 0,
            };
            assert_eq!(door.armor_sets, sets as u64);
            assert_eq!(reward.items.len(), 4 * sets, "{} December", door.day);
            for (template, _) in ARMOR_SET {
                let pieces: Vec<_> = reward
                    .items
                    .iter()
                    .filter(|p| p.item.item_template_id == id(template))
                    .collect();
                assert_eq!(pieces.len(), sets, "{} December: {template}", door.day);
                for piece in pieces {
                    assert_eq!(piece.item.durability, 325.0);
                    assert_eq!(piece.item.tempering_level, 0);
                    assert_eq!(piece.item.properties, ItemPropertiesAll::default());
                    assert_eq!(piece.item.arcane_tier, None);
                    assert_eq!(piece.item.grade, None);
                }
            }
            // Armour never lands as a stackable.
            for (template, _) in ARMOR_SET {
                assert!(!reward.stackable_items.contains_key(&id(template)));
            }
        }
    }

    /// A gifted piece on the wire has exactly the keys of a Divine Dark
    /// Seducer armour a player already holds in production.
    #[test]
    fn a_gifted_piece_has_the_shape_of_a_held_one() {
        let held: Value = serde_json::from_str(
            r#"{"id": "fec565d0-13d7-49c1-9bf6-6f412ad2cec6", "durability": 675.0,
                "properties": {"GRADING": [], "ENCHANTING": [
                    {"id": "b4de8d1b-d8a1-4575-8517-c1e5800a7525", "tier": 10},
                    {"id": "334f4ed9-5402-4a5a-981c-44772c799b88", "tier": 10}]},
                "itemTemplateId": "1063c3f9-1bbf-4962-b80d-fb9832075c1c",
                "temperingLevel": 10}"#,
        )
        .unwrap();
        let reward = reward_of(6);
        let piece = reward
            .items
            .iter()
            .find(|p| p.item.item_template_id == id(ARMOR_SET[0].0))
            .unwrap();
        let gifted = serde_json::to_value(piece).unwrap();
        let keys = |v: &Value| {
            let mut k: Vec<String> = v.as_object().unwrap().keys().cloned().collect();
            k.sort();
            k
        };
        assert_eq!(keys(&gifted), keys(&held));
        assert_eq!(keys(&gifted["properties"]), keys(&held["properties"]));
    }

    // --- the windows -------------------------------------------------------

    /// NOTHING BEFORE 1 DECEMBER 00:00 UTC: on the day this was written, on
    /// 30 November and in the last second of it, no door is open and no door
    /// can be claimed. One second later door 1 — and only door 1 — can.
    #[test]
    fn no_door_opens_before_1_december() {
        let written = DECEMBER_1_SECS - 8 * SECS_PER_DAY; // 23 November 2026
        for now in [
            0,
            written,
            DECEMBER_1_SECS - SECS_PER_DAY,
            DECEMBER_1_SECS - 1,
        ] {
            assert_eq!(open_doors(now).count(), 0, "t={now}");
            for door in doors() {
                assert_eq!(
                    gifts::can_claim(&door.gift, 0, now),
                    Err(GiftError::NotActive),
                    "t={now}: door {}",
                    door.day
                );
            }
        }
        let open: Vec<u32> = open_doors(DECEMBER_1_SECS).map(|d| d.day).collect();
        assert_eq!(open, vec![1]);
        assert_eq!(
            gifts::can_claim(&doors()[0].gift, 0, DECEMBER_1_SECS),
            Ok(())
        );
        assert_eq!(
            gifts::can_claim(&doors()[1].gift, 0, DECEMBER_1_SECS),
            Err(GiftError::NotActive)
        );
    }

    /// ONLY ON ITS OWN DAY (HauDrauf, 2026-11-24): each door opens at
    /// 00:00:00 UTC on its day and closes at 00:00:00 the next, to the second;
    /// `is_open` and the claim check agree at every edge.
    #[test]
    fn every_door_is_claimable_on_its_own_day_only() {
        for door in doors() {
            let (open, close) = (door.opens_at_secs, door.closes_at_secs);
            for (now, claimable) in [
                (open - 1, false),
                (open, true),
                (close - 1, true),
                (close, false),
            ] {
                assert_eq!(door.is_open(now), claimable, "door {} t={now}", door.day);
                assert_eq!(
                    gifts::can_claim(&door.gift, 0, now).is_ok(),
                    claimable,
                    "door {} t={now}",
                    door.day
                );
            }
            // Once per character.
            assert_eq!(
                gifts::can_claim(&door.gift, 1, open),
                Err(GiftError::LimitReached)
            );
            assert_eq!(open, utc(door.day, 0), "door {}", door.day);
            assert_eq!(close, utc(door.day + 1, 0), "door {}", door.day);
            assert_eq!(door.gift.start_time, open);
            assert_eq!(door.gift.end_time, close - 1, "23:59:59 is the last second");
        }
    }

    /// Day by day, second by second at the edges: on day N only door N is open
    /// (1-24 and the 31st), yesterday's door is shut from 00:00:00, 25-30
    /// December have none, and nothing is open from 1 January on.
    #[test]
    fn the_open_doors_follow_the_calendar() {
        for day in 1..=31u32 {
            let want: Vec<u32> = if day <= 24 || day == 31 {
                vec![day]
            } else {
                Vec::new()
            };
            for now in [utc(day, 0), utc(day, 12), utc(day + 1, 0) - 1] {
                let open: Vec<u32> = open_doors(now).map(|d| d.day).collect();
                assert_eq!(open, want, "{day} December t={now}");
            }
            // Yesterday's door, the second today begins.
            if day > 1 && day - 1 <= 24 {
                let yesterday = door(door_gift_id(day - 1)).unwrap();
                assert!(
                    !yesterday.is_open(utc(day, 0)),
                    "door {} on the {day}",
                    day - 1
                );
                assert_eq!(
                    gifts::can_claim(&yesterday.gift, 0, utc(day, 0)),
                    Err(GiftError::NotActive)
                );
            }
        }
        assert_eq!(open_doors(utc(32, 0)).count(), 0, "1 January");
        assert_eq!(open_doors(utc(32, 0) + 400 * SECS_PER_DAY).count(), 0);
        // Never two at once.
        let mut t = DECEMBER_1_SECS - SECS_PER_DAY;
        while t < utc(33, 0) {
            assert!(open_doors(t).count() <= 1, "t={t}");
            t += 3_600;
        }
    }

    /// The label, German first as he asked, with an English line.
    #[test]
    fn every_door_carries_its_label() {
        assert_eq!(
            gift(door_gift_id(1)).unwrap().description.as_deref(),
            Some("Adventskalender Türchen Nr. 1 / Advent calendar door no. 1")
        );
        assert_eq!(
            gift(door_gift_id(24)).unwrap().description.as_deref(),
            Some("Adventskalender Türchen Nr. 24 / Advent calendar door no. 24")
        );
        assert_eq!(
            gift(door_gift_id(31)).unwrap().description.as_deref(),
            Some("Silvester-Geschenk / New Year's Eve gift")
        );
        for door in doors().iter().filter(|d| d.day <= 24) {
            assert!(
                door.gift
                    .description
                    .as_deref()
                    .unwrap()
                    .starts_with(&format!("Adventskalender Türchen Nr. {} ", door.day))
            );
        }
    }

    /// What the client is shown is retail's line shape and nothing else.
    #[test]
    fn the_client_sees_plain_gift_lines() {
        for door in doors() {
            let shown = serde_json::to_value(door.gift.for_client()).unwrap();
            for item in shown["items"].as_array().unwrap() {
                let mut keys: Vec<&str> = item
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                keys.sort_unstable();
                assert_eq!(keys, ["itemTemplateId", "quantity"]);
                assert!(item["quantity"].as_u64().unwrap() > 0);
            }
            // Every template is one the game knows, so nothing is granted
            // into a bucket by guesswork.
            let items = game_items();
            for item in &door.gift.items {
                assert!(
                    economy::bucket_for_template(item.item_template_id, &items).is_some(),
                    "door {}: unknown template {}",
                    door.day,
                    item.item_template_id
                );
            }
        }
    }

    // --- the 95 % rule on the real gifts -----------------------------------

    fn character(inventory_level: u16) -> CompleteCharacter {
        CompleteCharacter {
            inventory_level,
            ..CompleteCharacter::default()
        }
    }

    fn backpack_with(gear: u64) -> CompleteInventory {
        let mut inv = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 1,
            treasury_version: 0,
        };
        for n in 0..gear {
            inv.backpack.items.0.insert(
                Uuid::from_u128(0x1000 + u128::from(n)),
                Item {
                    item_template_id: id(ARMOR_SET[1].0),
                    grade: None,
                    tempering_level: 0,
                    durability: 325.0,
                    properties: ItemPropertiesAll::default(),
                    arcane_tier: None,
                },
            );
        }
        inv
    }

    /// Only the two armour gifts carry the backpack rule; it is 95 % and pays
    /// out from 25 December 00:00 UTC. Registered with the gift machinery.
    #[test]
    fn only_the_armour_gifts_carry_the_backpack_rule() {
        for door in doors() {
            let rule = gift_capacity::rule_for(door.gift.global_gift_id);
            if matches!(door.day, 6 | 24) {
                assert_eq!(
                    rule,
                    Some(GearCapRule {
                        fill_percent: 95,
                        payable_from_secs: utc(25, 0),
                    }),
                    "{} December",
                    door.day
                );
            } else {
                assert_eq!(rule, None, "{} December", door.day);
            }
        }
        // CONTROL: no other gift is capped — an authored gift, the payout gift.
        assert_eq!(
            gift_capacity::rule_for(id("6f7c99c5-e403-4928-bc68-a82f6be18ded")),
            None
        );
        assert_eq!(
            gift_capacity::rule_for(gift_capacity::DEFERRED_GEAR_GIFT_ID),
            None
        );
    }

    /// THE WHOLE RULE, on 24 December's real gift. A backpack at 107 of 120
    /// has room for seven pieces under 95 % (114): the claim hands over one
    /// whole set and three pieces of the second, pays the decorations, the
    /// pack and the 1,200 Gems in full, and files five pieces. Nothing is paid
    /// on the 24th however much room appears; from 25 December 00:00 UTC it is
    /// paid as it fits, and in the end every one of the twelve arrived once.
    #[test]
    fn the_24th_fills_to_95_percent_and_pays_the_rest_from_the_25th() {
        let gift_id = door_gift_id(24);
        let ch = character(8);
        let mut inv = backpack_with(107);
        let mut deferred = Vec::new();

        let mut reward = reward_of(24);
        for (n, piece) in reward.items.iter_mut().enumerate() {
            piece.id = Uuid::from_u128(0x9000 + n as u128);
        }
        let stackables_before = reward.stackable_items.clone();
        let held = gift_capacity::cap_gift_gear(
            &mut reward,
            gift_capacity::rule_for(gift_id).unwrap(),
            gift_id,
            &ch,
            &inv,
            &mut deferred,
        );
        assert_eq!(held, 5);
        assert_eq!(reward.items.len(), 7);
        let granted: Vec<&str> = reward
            .items
            .iter()
            .map(|p| {
                ARMOR_SET
                    .iter()
                    .find(|(t, _)| id(t) == p.item.item_template_id)
                    .unwrap()
                    .1
            })
            .collect();
        assert_eq!(
            granted,
            [
                "Items.Name.DarkSeducerArmor",
                "Items.Name.DarkSeducerBoots",
                "Items.Name.DarkSeducerGauntlets",
                "Items.Name.DarkSeducerHelmet",
                "Items.Name.DarkSeducerArmor",
                "Items.Name.DarkSeducerBoots",
                "Items.Name.DarkSeducerGauntlets",
            ],
            "one whole set, then the second as far as it fits"
        );
        assert_eq!(reward.currencies[&GEMS], 1_200);
        assert_eq!(reward.stackable_items, stackables_before);
        assert_eq!(gift_capacity::owed(&deferred), 5);

        let mut tally: BTreeMap<Uuid, u64> = BTreeMap::new();
        let mut take = |items: &[economy::RewardItem], inv: &mut CompleteInventory| {
            for piece in items {
                *tally.entry(piece.item.item_template_id).or_default() += 1;
                let next = Uuid::from_u128(0xA000 + inv.backpack.items.0.len() as u128);
                inv.backpack.items.0.insert(next, piece.item.clone());
            }
        };
        take(&reward.items, &mut inv);
        assert_eq!(
            gift_capacity::backpack_used(&inv),
            114,
            "exactly 95 % of 120"
        );

        // The player sells twenty items on the evening of the 24th: still
        // nothing is paid before the 25th.
        let mut roomy = backpack_with(94);
        let early =
            gift_capacity::pay_deferred(&mut deferred, &ch, &roomy, utc(25, 0) - 1, &repair());
        assert!(early.is_empty());
        assert!(gift_capacity::payout_gift_def(&deferred, &ch, &roomy, utc(25, 0) - 1).is_none());

        // 25 December, backpack still full: nothing fits, nothing is lost.
        let full = gift_capacity::pay_deferred(&mut deferred, &ch, &inv, utc(25, 0), &repair());
        assert!(full.is_empty());
        assert_eq!(gift_capacity::owed(&deferred), 5);

        // Room for three: three arrive, two stay on file.
        let mut three = backpack_with(111);
        let first = gift_capacity::pay_deferred(&mut deferred, &ch, &three, utc(25, 9), &repair());
        assert_eq!(first.items.len(), 3);
        take(&first.items, &mut three);
        assert_eq!(gift_capacity::backpack_used(&three), 114);
        assert_eq!(gift_capacity::owed(&deferred), 2);

        // Weeks later, with room: the last two, and the file is closed.
        let last = gift_capacity::pay_deferred(
            &mut deferred,
            &ch,
            &roomy,
            utc(25, 0) + 40 * SECS_PER_DAY,
            &repair(),
        );
        assert_eq!(last.items.len(), 2);
        take(&last.items, &mut roomy);
        assert!(deferred.is_empty());

        // Twelve pieces in all, three of each, none twice and none missing.
        assert_eq!(tally.len(), 4);
        assert!(tally.values().all(|n| *n == 3), "{tally:?}");
    }

    /// A backpack with room takes the whole gift at once and owes nothing; a
    /// backpack already over its limit takes the decorations, the pack and the
    /// Gems and is owed the whole set.
    #[test]
    fn the_6th_is_paid_whole_with_room_and_deferred_whole_without() {
        let gift_id = door_gift_id(6);
        let rule = gift_capacity::rule_for(gift_id).unwrap();

        let mut reward = reward_of(6);
        let mut deferred = Vec::new();
        let held = gift_capacity::cap_gift_gear(
            &mut reward,
            rule,
            gift_id,
            &character(8),
            &backpack_with(60),
            &mut deferred,
        );
        assert_eq!((held, reward.items.len()), (0, 4));
        assert!(deferred.is_empty());

        let mut reward = reward_of(6);
        let held = gift_capacity::cap_gift_gear(
            &mut reward,
            rule,
            gift_id,
            &character(0),
            &backpack_with(300),
            &mut deferred,
        );
        assert_eq!((held, reward.items.len()), (4, 0));
        assert_eq!(reward.currencies[&GEMS], 600);
        assert_eq!(
            reward.stackable_items.len(),
            2 + 11,
            "two decorations and the pack"
        );
        assert_eq!(gift_capacity::owed(&deferred), 4);
        assert!(deferred.iter().all(|d| d.gift_id == gift_id));
    }
}
