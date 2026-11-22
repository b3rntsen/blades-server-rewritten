use super::*;
use actix_web::{App, test as http, web::Data};
use std::sync::Mutex;

use crate::arena::combat::calc::tests::reference_request;

const TOKEN: &str = "test-dev-token";

macro_rules! app {
    ($token:expr) => {
        http::init_service(
            App::new()
                .app_data(Data::new(DevToken($token)))
                .service(gear_calc_catalogue)
                .service(gear_calc_calculate),
        )
        .await
    };
}

const CALCULATE: &str = "/blades.bgs.services/api/dev/v1/gear-calc/calculate";
const CATALOGUE: &str = "/blades.bgs.services/api/dev/v1/gear-calc/catalogue";

#[actix_web::test]
async fn both_routes_refuse_a_missing_or_wrong_token() {
    let app = app!(Some(TOKEN.to_string()));
    for (header, want) in [
        (None, 401),
        (Some(("X-Import-Token", "wrong")), 403),
        (Some(("Authorization", "Bearer wrong")), 403),
        // A bare token in Authorization is not a token at all (the dev-route rule).
        (Some(("Authorization", TOKEN)), 401),
    ] {
        let mut get = http::TestRequest::get().uri(CATALOGUE);
        let mut post = http::TestRequest::post()
            .uri(CALCULATE)
            .set_json(reference_request());
        if let Some(h) = header {
            get = get.insert_header(h);
            post = post.insert_header(h);
        }
        let res = http::call_service(&app, get.to_request()).await;
        assert_eq!(res.status().as_u16(), want, "catalogue with {header:?}");
        let res = http::call_service(&app, post.to_request()).await;
        assert_eq!(res.status().as_u16(), want, "calculate with {header:?}");
    }
    // Control: the right token is what was missing.
    let res = http::call_service(
        &app,
        http::TestRequest::get()
            .uri(CATALOGUE)
            .insert_header(("X-Import-Token", TOKEN))
            .to_request(),
    )
    .await;
    assert_eq!(res.status().as_u16(), 200);
}

#[actix_web::test]
async fn with_no_token_configured_the_routes_are_off() {
    for token in [None, Some(String::new())] {
        let app = app!(token);
        let res = http::call_service(
            &app,
            http::TestRequest::post()
                .uri(CALCULATE)
                .insert_header(("X-Import-Token", ""))
                .set_json(reference_request())
                .to_request(),
        )
        .await;
        assert_eq!(res.status().as_u16(), 503);
    }
}

/// The fight the production server logged on 2026-11-22, through the route
/// (see `calc::tests::logged_cells` for how the logged values are pinned).
#[actix_web::test]
async fn the_endpoint_reproduces_the_logged_fight() {
    let app = app!(Some(TOKEN.to_string()));
    let body: Value = http::call_and_read_body_json(
        &app,
        http::TestRequest::post()
            .uri(CALCULATE)
            .insert_header(("X-Import-Token", TOKEN))
            .set_json(reference_request())
            .to_request(),
    )
    .await;

    assert_eq!(body["attack"]["effectiveRank"], 12);
    let near = |v: &Value, want: f64| (v.as_f64().unwrap() - want).abs() < 0.02;
    assert!(
        near(&body["attack"]["attack"], 261.30),
        "{}",
        body["attack"]
    );
    assert!(near(&body["attack"]["paralyzeThreshold"], 76.76));
    assert!(near(&body["attack"]["resistancePiercing"], 5.04));

    let cell = |row: &str, state: &str| -> Value {
        let r = body["rows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == row)
            .unwrap();
        r["cells"]
            .as_array()
            .unwrap()
            .iter()
            .find(|c| c["state"] == state)
            .unwrap()
            .clone()
    };
    for (row, state, want, paralysed) in crate::arena::combat::calc::tests::logged_cells() {
        let c = cell(row, state);
        assert!(
            near(&c["hit"]["landed"], f64::from(want)),
            "{row}/{state}: {}",
            c["hit"]
        );
        assert_eq!(c.get("paralysed").is_some(), paralysed, "{row}/{state}");
    }
    assert!(body["computeMicros"].as_u64().is_some());
}

#[actix_web::test]
async fn a_request_the_calculator_cannot_run_is_a_400_with_the_reason() {
    let app = app!(Some(TOKEN.to_string()));
    let mut req = reference_request();
    req["spell"]["uuid"] = json!(gamedata::ids::WARD);
    let res = http::call_service(
        &app,
        http::TestRequest::post()
            .uri(CALCULATE)
            .insert_header(("X-Import-Token", TOKEN))
            .set_json(req)
            .to_request(),
    )
    .await;
    assert_eq!(res.status().as_u16(), 400);
    let body: Value = http::read_body_json(res).await;
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("not a damaging spell")
    );
}

fn named<'a>(list: &'a Value, name: &str) -> Option<&'a Value> {
    list.as_array().unwrap().iter().find(|x| x["name"] == name)
}

fn strs(v: &Value) -> Vec<&str> {
    v.as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect()
}

#[test]
fn the_catalogue_offers_end_game_gear_only() {
    let c: Value = serde_json::from_str(catalogue_json()).unwrap();
    let items = &c["items"];
    for name in [
        "Divine Chitin Shield",
        "Dragonscale Shield",
        "Dragonplate Shield",
        "Dragonbone Dagger",
        "Dragonscale Armor",
        "Dragonplate Helmet",
        "Divine Steel Boots",
    ] {
        assert!(named(items, name).is_some(), "{name} is missing");
    }
    // The low-tier twin of a Divine item is not offered under its own name.
    assert!(named(items, "Chitin Shield").is_none());
    assert!(named(items, "Steel Boots").is_none());
    // Every offered id is a tier-10, level-45+ template.
    for it in items.as_array().unwrap() {
        let id = it["id"].as_str().unwrap();
        let (tier, level) = gamedata::weapon(id)
            .map(|w| (w.tier, w.required_level))
            .or_else(|| gamedata::shield(id).map(|s| (s.tier, s.required_level)))
            .or_else(|| gamedata::armor(id).map(|a| (a.tier, a.required_level)))
            .expect("a known template");
        assert_eq!(tier, 10, "{}", it["name"]);
        assert!(level >= 45, "{}", it["name"]);
    }
    let chitin = named(items, "Divine Chitin Shield").unwrap();
    assert_eq!(chitin["block"], 360.0, "tempered to 10");
    assert!(
        chitin["properties"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["name"] == "Material Block Bonus Vs Poison")
    );
}

#[test]
fn the_catalogue_offers_only_what_changes_a_spell_hit() {
    let c: Value = serde_json::from_str(catalogue_json()).unwrap();

    let spells: Vec<&str> = c["spells"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect();
    for s in [
        "Paralyze",
        "Ice Spike",
        "Fireball",
        "Frostbite",
        "Lightning Bolt",
    ] {
        assert!(spells.contains(&s), "{s}");
    }
    for s in ["Ward", "Wall of Fire", "Blizzard Armor", "Absorb"] {
        assert!(
            !spells.contains(&s),
            "{s} is not a spell to fire at a defender"
        );
    }
    let paralyze = named(&c["spells"], "Paralyze").unwrap();
    assert_eq!(paralyze["paralyses"], true);
    assert_eq!(paralyze["element"], "poison");
    assert_eq!(named(&c["spells"], "Ice Spike").unwrap()["stuns"], true);
    assert_eq!(named(&c["spells"], "Fireball").unwrap()["stuns"], false);
    assert_eq!(
        named(&c["spells"], "Frostbite").unwrap()["channelled"],
        true
    );

    let e = &c["enchants"];
    let fortify = named(e, "Fortify Poison Damage").expect("Fortify Poison");
    assert_eq!(
        strs(&fortify["attack"]),
        ["poison"],
        "one element, attacker only"
    );
    assert!(strs(&fortify["defence"]).is_empty());
    assert_eq!(fortify["maxTier"], 10);
    let rs = named(e, "Resist Spells").expect("Resist Spells");
    assert_eq!(strs(&rs["defence"]).len(), 4, "every element");
    assert!(strs(&rs["attack"]).is_empty());
    assert_eq!(
        strs(&named(e, "Resist Frost").unwrap()["defence"]),
        ["frost"]
    );
    assert!(!strs(&named(e, "Elemental Resistance Piercing").unwrap()["attack"]).is_empty());
    assert!(named(e, "Fortify Primary Effect").is_some());
    // Real, modelled, and nothing to do with a spell landing.
    for name in [
        "Weapon Ravage Stamina",
        "Haste",
        "Physical Armor Piercing",
        "Resist Slashing",
    ] {
        assert!(
            named(e, name).is_none(),
            "{name} does not change a spell hit"
        );
    }
    // Material properties come with the item; they are not picked.
    assert!(named(e, "Material Block Bonus Vs Poison").is_none());

    let p = &c["perks"];
    assert_eq!(
        strs(&named(p, "Augmented Poison").unwrap()["attack"]),
        ["poison"]
    );
    assert_eq!(
        strs(&named(p, "Elemental Protection").unwrap()["defence"]).len(),
        4
    );
    assert!(strs(&named(p, "Elemental Protection").unwrap()["attack"]).is_empty());
    assert!(!strs(&named(p, "Maximum Power").unwrap()["attack"]).is_empty());
    assert!(named(p, "Enchantment Synergy").is_some());
    for name in ["Barbarian", "Matching Set", "Healing Surge"] {
        assert!(
            named(p, name).is_none(),
            "{name} does not change a spell hit"
        );
    }

    assert_eq!(c["resistElements"]["maxRank"], 15);
    assert!((c["potions"]["poison"]["value"].as_f64().unwrap() - 334.76).abs() < 0.01);
    assert_eq!(c["races"].as_array().unwrap().len(), 11);
    assert!(
        c["grades"]
            .as_array()
            .unwrap()
            .iter()
            .any(|g| g["abilityId"] == gamedata::ids::PARALYZE && g["slot"] == "ring")
    );
    // Small enough to send whole.
    assert!(
        catalogue_json().len() < 120_000,
        "{} bytes",
        catalogue_json().len()
    );
}

struct Capture(Mutex<Vec<String>>);
impl log::Log for &'static Capture {
    fn enabled(&self, _: &log::Metadata<'_>) -> bool {
        true
    }
    fn log(&self, record: &log::Record<'_>) {
        self.0.lock().unwrap().push(record.args().to_string());
    }
    fn flush(&self) {}
}

#[test]
fn a_calculation_writes_no_combat_log_lines() {
    use log::Log;
    static CAPTURE: Capture = Capture(Mutex::new(Vec::new()));
    let logger = ScopedLogger(&CAPTURE);
    let line = |msg: &str| {
        logger.log(
            &log::Record::builder()
                .args(format_args!("{msg}"))
                .level(log::Level::Info)
                .build(),
        )
    };
    let meta = log::Metadata::builder().level(log::Level::Info).build();

    line("before");
    quiet(|| {
        line("combat event: gsid=gear-calc");
        assert!(!logger.enabled(&meta));
        // Nested scopes do not switch it back on early.
        quiet(|| line("inner"));
        line("still quiet");
    });
    assert!(logger.enabled(&meta));
    line("after");
    assert_eq!(*CAPTURE.0.lock().unwrap(), ["before", "after"]);
}

/// A stored character becomes builds the calculator accepts unchanged.
#[test]
fn a_stored_character_exports_as_builds_the_calculator_runs() {
    let rows: Vec<Value> =
        serde_json::from_str(include_str!("../combat/testdata/soak_prod_characters.json")).unwrap();
    let row = rows
        .iter()
        .find(|r| r["equippedItems"].as_object().is_some_and(|m| m.len() >= 6))
        .expect("a geared fixture");
    let equipped: blades_lib::user_data::EquippedItems =
        serde_json::from_value(row["equippedItems"].clone()).unwrap();
    let first = equipped.0.values().next().unwrap().clone();
    let mut character = CompleteCharacter {
        name: "Fixture".into(),
        level: row["level"].as_u64().unwrap() as u16,
        abilities: row["abilities"].clone(),
        equipped_abilities: row["equippedAbilities"].clone(),
        ..Default::default()
    };
    // One saved profile wearing a single piece, one that names nothing we hold.
    character.loadout_profiles = json!([
        { "name": "One piece", "equippedItems": [
            { "slot": first.slot, "itemId": first.id },
            { "slot": "417e79de-c810-42f8-8273-f9759df6ae25" },
        ]},
        null,
        { "name": "Gone", "equippedItems": [
            { "slot": first.slot, "itemId": "00000000-0000-4000-8000-00000000dead" },
        ]},
    ]);
    let n_equipped = equipped.0.len();
    let inventory = CompleteInventory {
        backpack: Default::default(),
        loadout: blades_lib::user_data::Loadout {
            equipped_items: equipped,
            ..Default::default()
        },
        treasury: Default::default(),
        overflow_treasury: Default::default(),
        backpack_version: 1,
        treasury_version: 0,
    };

    let builds = builds_for(&character, &inventory, &row["customization"]);
    assert_eq!(
        builds.iter().map(|b| b.name.as_str()).collect::<Vec<_>>(),
        ["Equipped now", "One piece"],
        "a profile whose items are gone is not offered"
    );
    let now = &builds[0].build;
    assert_eq!(now.items.len(), n_equipped);
    assert_eq!(now.level, character.level);
    assert!(
        now.items.iter().all(|i| i.slot.is_some()),
        "every slot is named"
    );
    assert!(now.race.is_some(), "race comes from the customization blob");
    assert_eq!(builds[1].build.items.len(), 1);
    assert_eq!(builds[1].build.abilities, now.abilities);

    // The exported build is exactly what the production loadout parser reads.
    let exported = calc::loadout_for(now, "x").unwrap();
    let direct = loadout::from_character(&character, &inventory);
    assert_eq!(exported.armor_rating, direct.armor_rating);
    assert_eq!(exported.block_rating, direct.block_rating);
    // (Equipped items are a map, so the order they are applied in is not fixed.)
    let sorted = |v: &[(crate::arena::combat::state::DamageType, f32)]| {
        let mut v: Vec<String> = v.iter().map(|x| format!("{x:?}")).collect();
        v.sort();
        v
    };
    assert_eq!(sorted(&exported.resistances), sorted(&direct.resistances));
    assert_eq!(
        sorted(&exported.element_fortify),
        sorted(&direct.element_fortify)
    );
    assert_eq!(exported.max_health_bonus, direct.max_health_bonus);
    assert_eq!(
        format!("{:?}", exported.perks),
        format!("{:?}", direct.perks)
    );

    // And it round-trips through JSON into a calculation, on either side.
    let build = serde_json::to_value(now).unwrap();
    let mut req = reference_request();
    req["attacker"] = build.clone();
    req["defenders"] = json!([{ "id": "me", "build": build }]);
    let req: calc::Request = serde_json::from_value(req).unwrap();
    let r = calc::calculate(&req).unwrap();
    assert!(r.rows[0].cells[0].total_health > 0.0);
}

/// Writes the catalogue and the reference calculation where `GEAR_CALC_FIXTURE_DIR`
/// points, for the website's tests:
///
/// ```text
/// GEAR_CALC_FIXTURE_DIR=<capture repo>/web/lib/fixtures \
///   cargo test -p server gear_calc::tests::write_web_fixture -- --ignored
/// ```
#[test]
#[ignore = "writes files; run on purpose"]
fn write_web_fixture() {
    let dir = std::env::var("GEAR_CALC_FIXTURE_DIR").expect("GEAR_CALC_FIXTURE_DIR");
    let dir = std::path::Path::new(&dir);
    let catalogue: Value = serde_json::from_str(catalogue_json()).unwrap();
    std::fs::write(
        dir.join("gear-calc-catalogue.json"),
        serde_json::to_string(&catalogue).unwrap(),
    )
    .unwrap();
    let req: calc::Request = serde_json::from_value(reference_request()).unwrap();
    let response = quiet(|| calc::calculate(&req)).unwrap();
    std::fs::write(
        dir.join("gear-calc-reference.json"),
        serde_json::to_string_pretty(&json!({
            "request": reference_request(),
            "response": response,
        }))
        .unwrap(),
    )
    .unwrap();
}
