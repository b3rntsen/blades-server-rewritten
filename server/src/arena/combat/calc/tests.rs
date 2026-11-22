//! The calculator against numbers the PRODUCTION server logged.
//!
//! `reference_request` is the 2026-11-22 fight the calculator was asked for:
//! Dwalin's rank-12 Paralyze against a level-86 shield build. The expected
//! values come from that day's `combat event` lines, not from arithmetic done
//! here, so a failing test is the calculator and the live fight disagreeing.
//!
//! One step separates the log from this tree. Production that day ran fork/main
//! f352c8a, where the attacker's 5.04 Elemental Resistance Piercing did not come
//! off Resist Spells or Resist Elements. #682 (4067064, this branch's base) nets
//! it against the whole resistance, so every logged value that resistance had
//! not already driven to the 5 % floor is 5.04 higher here. [`logged_cells`]
//! keeps the logged number and says which cells that applies to.

use super::*;
use serde_json::{Value, json};

pub(crate) const PARALYZE: &str = gamedata::ids::PARALYZE;
const ICE_SPIKE: &str = "cfee0b02-6d91-4d34-869c-a7e54329060d";
const AUGMENTED_POISON: &str = "66faf7c6-426c-4689-99e6-ccdd25b9bc11";
const ENCHANTMENT_SYNERGY: &str = "780b82d1-a371-4454-baea-e18389f315e5";
const ELEMENTAL_PROTECTION: &str = "788aa75e-4796-4d57-bbab-b1b901623f16";
const MAXIMUM_POWER: &str = "83784ade-533e-4965-a540-05bfd4f056d8";
const FORTIFY_POISON: &str = "2b8fd511-80da-4128-81b1-bd7ab6023223";
const RESIST_SPELLS: &str = "33f866a5-f2f5-46af-ac13-fa5f02bedfbf";
const RESISTANCE_PIERCING: &str = "98757a01-33b8-40ea-bb45-6acd89811ae3";
const PARALYZE_BONUS_RANKS: &str = "d1749122-5ada-4017-918d-990fc7717481";
/// Divine Glass Shield: block 360 at tempering 10, bonus against Shock only.
const PLAIN_SHIELD: &str = "33ffc2ba-10d7-4f2a-9c1c-1dd9667334d6";
/// Divine Chitin Shield: block 360 at tempering 10, `Material Block Bonus Vs Poison`.
const POISON_SHIELD: &str = "0d491a77-762d-4c31-85ad-5baf39eb90a7";

fn enchanted(id: &str, n: usize) -> Vec<Value> {
    (0..n)
        .map(|_| json!({ "enchants": [{ "id": id, "tier": 10 }] }))
        .collect()
}

fn defender(shield: &str, resist_spells: usize) -> Value {
    let mut items = vec![json!({ "templateId": shield, "temperingLevel": 10 })];
    items.extend(enchanted(RESIST_SPELLS, resist_spells));
    json!({
        "level": 86,
        "items": items,
        "abilities": {
            ELEMENTAL_PROTECTION: 6,
            ENCHANTMENT_SYNERGY: 7,
            gamedata::ids::RESIST_ELEMENTS: 5,
            gamedata::ids::WARD: 3,
        },
    })
}

/// The request both this module and the HTTP test replay.
pub(crate) fn reference_request() -> Value {
    let mut attacker_items = enchanted(FORTIFY_POISON, 3);
    attacker_items.push(json!({ "enchants": [{ "id": RESISTANCE_PIERCING, "tier": 10 }] }));
    for _ in 0..2 {
        attacker_items.push(json!({ "grades": [{ "id": PARALYZE_BONUS_RANKS, "tier": 2 }] }));
    }
    json!({
        "spell": { "uuid": PARALYZE, "rank": 2 },
        "attacker": {
            "level": 86,
            "items": attacker_items,
            "abilities": { AUGMENTED_POISON: 8, ENCHANTMENT_SYNERGY: 7 },
        },
        "defenders": [
            { "id": "plain", "build": defender(PLAIN_SHIELD, 0) },
            { "id": "plain+rs", "build": defender(PLAIN_SHIELD, 3) },
            { "id": "poison+rs", "build": defender(POISON_SHIELD, 3) },
        ],
        "states": [
            { "id": "open", "block": "none" },
            { "id": "open+re", "block": "none", "resistElements": true },
            { "id": "low", "block": "low" },
            { "id": "low+re", "block": "low", "resistElements": true },
            { "id": "optimal", "block": "optimal" },
            { "id": "optimal+re", "block": "optimal", "resistElements": true },
            { "id": "open+potion", "block": "none", "potion": true },
            { "id": "open+ward", "block": "none", "ward": true },
        ],
    })
}

fn run(req: Value) -> Response {
    let req: Request = serde_json::from_value(req).expect("request deserialises");
    calculate(&req).expect("calculation runs")
}

fn cell<'a>(r: &'a Response, row: &str, state: &str) -> &'a Cell {
    r.rows
        .iter()
        .find(|x| x.id == row)
        .unwrap_or_else(|| panic!("row {row}"))
        .cells
        .iter()
        .find(|c| c.state == state)
        .unwrap_or_else(|| panic!("state {state}"))
}

fn close(got: f32, want: f32) -> bool {
    (got - want).abs() < 0.02
}

#[test]
fn the_attack_is_the_one_the_server_logged() {
    let r = run(reference_request());
    let a = &r.attack;
    assert_eq!(
        a.effective_rank, 12,
        "rank 2 + two tier-2 rings, capped at 12"
    );
    assert!(close(a.base, 198.56), "base {}", a.base);
    // 198.56 + 19.80 (Augmented Poison r8) + 3 x 11.45 x 1.25 (Enchantment Synergy).
    assert!(close(a.attack, 261.30), "attack {}", a.attack);
    assert!(close(a.paralyze_threshold.unwrap(), 76.76));
    assert!(close(a.paralyze_secs.unwrap(), 3.1));
    assert!(close(a.resistance_piercing, 5.04));
    assert_eq!(a.condition, Some("poisoned"));
}

/// The piercing #682 switched on for gear and spell resistance.
const PIERCING: f32 = 5.04;

/// `(row, state, poison that lands, paralysed)` for the cells production logged.
///
/// Each value is the LOGGED one (f352c8a) plus the 5.04 piercing #682 now takes
/// off the defender's resistance — except where the resistance left only the 5 %
/// floor, which piercing cannot raise.
pub(crate) fn logged_cells() -> Vec<(&'static str, &'static str, f32, bool)> {
    // (row, state, logged at f352c8a, resistance was at the floor, paralysed)
    [
        ("plain", "optimal+re", 95.97, false, true),
        ("plain+rs", "optimal", 129.62, false, true),
        ("plain+rs", "optimal+re", 32.97, false, false),
        ("poison+rs", "open", 198.30, false, true),
        ("poison+rs", "low", 80.41, false, true),
        ("poison+rs", "optimal", 50.89, false, false),
        ("poison+rs", "optimal+re", 5.69, true, false),
    ]
    .into_iter()
    .map(|(row, state, logged, floored, paralysed)| {
        let now = if floored { logged } else { logged + PIERCING };
        (row, state, now, paralysed)
    })
    .collect()
}

#[test]
fn the_logged_post_mitigation_values_are_reproduced() {
    let r = run(reference_request());
    for (row, state, want, paralysed) in logged_cells() {
        let c = cell(&r, row, state);
        assert!(
            close(c.hit.landed, want),
            "{row}/{state}: landed {} want {want}",
            c.hit.landed
        );
        assert!(close(c.hit.element_landed, want));
        assert_eq!(
            c.paralysed.is_some(),
            paralysed,
            "{row}/{state}: paralysed flag at {}",
            c.hit.landed
        );
        if let Some(p) = &c.paralysed {
            assert!(close(p.secs, 3.1), "paralysis lasts the rank's 3.1 s");
        }
        // The engine's own health bar agrees with the resolved hit.
        assert!(
            (c.total_health - c.hit.landed).abs() < 0.01,
            "{row}/{state}: health lost {} vs hit {}",
            c.total_health,
            c.hit.landed
        );
    }
}

/// Control for the pin above: without the attacker's piercing enchant the
/// calculator gives the values exactly as production logged them.
#[test]
fn without_the_piercing_enchant_the_values_are_exactly_the_logged_ones() {
    let mut req = reference_request();
    let items = req["attacker"]["items"].as_array_mut().unwrap();
    items.retain(|i| i["enchants"][0]["id"] != RESISTANCE_PIERCING);
    assert_eq!(
        items.len(),
        5,
        "three Fortify Poison and two rings are left"
    );
    let r = run(req);
    assert!(close(r.attack.resistance_piercing, 0.0));
    for (row, state, logged) in [
        ("plain", "optimal+re", 95.97),
        ("plain+rs", "optimal", 129.62),
        ("plain+rs", "optimal+re", 32.97),
        ("poison+rs", "open", 198.30),
        ("poison+rs", "low", 80.41),
        ("poison+rs", "optimal", 50.89),
        ("poison+rs", "optimal+re", 5.69),
    ] {
        let c = cell(&r, row, state);
        assert!(
            close(c.hit.landed, logged),
            "{row}/{state}: {}",
            c.hit.landed
        );
    }
}

#[test]
fn the_defender_numbers_are_the_logged_ones() {
    let r = run(reference_request());
    let plain = &r.rows[0].defender;
    assert!(close(plain.block_rating, 360.0));
    assert!(close(plain.block_rating_optimal, 720.0));
    // + Elemental Protection r6 (117.5), added after the optimal doubling.
    assert!(close(plain.element_block_rating, 477.5));
    assert!(close(plain.element_block_rating_optimal, 837.5));
    assert_eq!(plain.resist_elements_rank, 5);
    let poison = &r.rows[2].defender;
    // + the Chitin shield's poison block bonus (960.12).
    assert!(
        close(poison.element_block_rating, 360.0 + 117.5 + 960.12),
        "{}",
        poison.element_block_rating
    );
    // 3 x Resist Spells T10 (16.8) x 1.25 Enchantment Synergy.
    assert!(
        close(poison.resistance_rating, 63.0),
        "{}",
        poison.resistance_rating
    );
    // Resist Elements r5 adds 96.65 for the hit it covers.
    let c = cell(&r, "poison+rs", "open+re");
    assert!(close(c.hit.resistance_rating, 63.0 + 96.65));
}

#[test]
fn the_steps_of_a_hit_add_up() {
    let r = run(reference_request());
    let c = cell(&r, "poison+rs", "low");
    assert!(close(c.hit.attack, 261.30));
    // 1437.62 block rating x 0.082 = 117.88 off.
    assert!(close(c.hit.after_block, 143.42), "{}", c.hit.after_block);
    // 63.0 resistance less the 5.04 piercing.
    assert!(close(c.hit.after_resistance, 80.41 + PIERCING));
    assert!(c.hit.blocked && !c.hit.optimal);
    assert!(cell(&r, "poison+rs", "optimal").hit.optimal);
    assert!(!cell(&r, "poison+rs", "open").hit.blocked);
}

#[test]
fn a_resist_potion_and_a_ward_both_stop_the_paralysis() {
    let r = run(reference_request());
    let open = cell(&r, "plain", "open");
    assert!(close(open.hit.landed, 261.30) && open.paralysed.is_some());
    // The top-tier poison potion is worth 334.76: only the 5 % floor is left.
    let potion = cell(&r, "plain", "open+potion");
    assert!(
        close(potion.hit.landed, 261.30 * 0.05),
        "{}",
        potion.hit.landed
    );
    assert!(potion.paralysed.is_none());
    // A Ward eats elemental damage before it reaches the bar, and Paralyze
    // compares what landed (#512).
    let ward = cell(&r, "plain", "open+ward");
    assert!(ward.hit.landed < open.hit.landed);
    assert!(
        close(ward.hit.after_resistance, 261.30),
        "the Ward is a pool, not a resistance"
    );
    assert_eq!(ward.paralysed.is_some(), ward.hit.element_landed > 76.76);
}

#[test]
fn poisoned_needs_a_quarter_of_the_untripled_pool() {
    let mut req = reference_request();
    let r = run(req.clone());
    let thr = r.rows[0].defender.condition_threshold;
    assert!(close(
        thr,
        r.rows[0].defender.max_health as f32 / 3.0 * 0.25
    ));
    // Alone, a cast conditions exactly when what landed reaches the threshold.
    let mut seen = [false, false];
    for row in &r.rows {
        for c in &row.cells {
            let lands = c.hit.element_landed >= row.defender.condition_threshold;
            assert_eq!(
                c.conditioned.is_some(),
                lands,
                "{}/{}: {} vs {}",
                row.id,
                c.state,
                c.hit.element_landed,
                row.defender.condition_threshold
            );
            seen[usize::from(lands)] = true;
        }
    }
    assert_eq!(seen, [true, true], "the table shows both outcomes");
    let weak = cell(&r, "poison+rs", "optimal");
    assert!(weak.conditioned.is_none());
    // ...and on top of poison already taken inside the 5 s window, the same weak
    // hit tips it over.
    req["priorElementDamage"] = json!(thr - weak.hit.element_landed + 1.0);
    let r = run(req);
    let c = cell(&r, "poison+rs", "optimal");
    assert_eq!(
        c.conditioned.as_ref().and_then(|f| f.status),
        Some("poisoned")
    );
    assert!(c.conditioned.as_ref().unwrap().secs > 0.0);
    assert!(c.paralysed.is_none(), "conditioning is not paralysis");
}

#[test]
fn ice_spike_stuns_only_above_its_threshold_and_maximum_power_can_carry_it_over() {
    let build = |max_power: u8| {
        let mut req = reference_request();
        req["spell"] = json!({ "uuid": ICE_SPIKE, "rank": 4 });
        req["attacker"] = json!({ "level": 86, "abilities": { MAXIMUM_POWER: max_power } });
        req["magickaFull"] = json!(true);
        req
    };
    let r = run(build(0));
    let thr = r
        .attack
        .stun_threshold
        .expect("Ice Spike ships a stun threshold");
    assert!(r.attack.stun_secs.is_some());
    assert_eq!(r.attack.element, "frost");
    for row in &r.rows {
        for c in &row.cells {
            assert_eq!(
                c.stunned.is_some(),
                c.total_health > thr,
                "{}/{}: {} vs threshold {thr}",
                row.id,
                c.state,
                c.total_health
            );
            assert!(c.paralysed.is_none(), "Ice Spike does not paralyse");
        }
    }
    assert!(cell(&r, "plain", "open").stunned.is_some());
    assert!(cell(&r, "plain", "optimal+re").stunned.is_none());
    // Maximum Power makes the same cast bigger.
    let boosted = run(build(6));
    assert!(boosted.attack.attack > r.attack.attack);
}

#[test]
fn every_offered_spell_deals_damage_and_a_channel_is_summed_over_its_ticks() {
    let mut n = 0;
    for a in gamedata::ABILITIES.iter().filter(|a| is_castable_spell(a)) {
        n += 1;
        let mut req = reference_request();
        req["spell"] = json!({ "uuid": a.uuid, "rank": a.maximum_purchaseable_level.max(1) });
        req["attacker"] = json!({ "level": 86 });
        let r = run(req);
        let open = cell(&r, "plain", "open");
        assert!(open.total_health > 0.0, "{} dealt nothing", a.name);
        assert!(
            open.total_health >= cell(&r, "poison+rs", "optimal+re").total_health,
            "{}: a defended target cannot take more",
            a.name
        );
        if r.attack.ticks > 1 {
            assert!(
                open.ticks > 1 && open.total_health > open.hit.landed * 1.5,
                "{}: {} ticks, total {} vs first hit {}",
                a.name,
                open.ticks,
                open.total_health,
                open.hit.landed
            );
        }
    }
    assert!(n >= 8, "the spell list shrank to {n}");
}

#[test]
fn requests_are_bounded_and_unknown_input_is_refused() {
    let parse = |v: Value| serde_json::from_value::<Request>(v);

    let mut req = reference_request();
    req["spell"]["uuid"] = json!("00000000-0000-4000-8000-000000000000");
    assert_eq!(
        calculate(&parse(req).unwrap()).unwrap_err(),
        CalcError::UnknownSpell
    );

    // Ward is a real ability, but not something to fire at a defender.
    let mut req = reference_request();
    req["spell"]["uuid"] = json!(gamedata::ids::WARD);
    assert_eq!(
        calculate(&parse(req).unwrap()).unwrap_err(),
        CalcError::NotACastableSpell
    );

    let mut req = reference_request();
    let row = req["defenders"][0].clone();
    req["defenders"] = json!(vec![row; MAX_DEFENDERS + 1]);
    assert!(matches!(
        calculate(&parse(req).unwrap()).unwrap_err(),
        CalcError::TooLarge(_)
    ));

    let mut req = reference_request();
    req["attacker"]["items"] = json!(enchanted(FORTIFY_POISON, MAX_ITEMS + 1));
    assert!(matches!(
        calculate(&parse(req).unwrap()).unwrap_err(),
        CalcError::TooLarge(_)
    ));

    let mut req = reference_request();
    req["attacker"]["abilities"] = json!({ "not-an-ability": 3 });
    assert!(matches!(
        calculate(&parse(req).unwrap()).unwrap_err(),
        CalcError::BadBuild(_)
    ));

    // A field the calculator does not know is a caller bug, not something to ignore.
    let mut req = reference_request();
    req["attacker"]["damage"] = json!(9999);
    assert!(parse(req).is_err());
}

#[test]
fn the_same_request_gives_the_same_answer() {
    let a = serde_json::to_value(run(reference_request())).unwrap();
    let b = serde_json::to_value(run(reference_request())).unwrap();
    assert_eq!(a, b, "the calculation keeps no state between calls");
}
