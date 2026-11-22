//! **Gear calculator** — the dev routes the website's calculator page calls.
//!
//! | route | |
//! |---|---|
//! | `GET  …/dev/v1/gear-calc/catalogue` | what can be picked: spells, end-game gear, enchantments, perks |
//! | `POST …/dev/v1/gear-calc/calculate` | one spell against a table of defender builds × defender states |
//! | `GET  …/dev/v1/gear-calc/loadouts?userId=` | a player's live character as calculator builds |
//!
//! All three are `X-Import-Token` gated like every other dev route, and all three
//! are read-only: the calculation never touches the database or a live match
//! ([`combat::calc`] builds throwaway fighters), and the loadout route is one
//! `SELECT`.
//!
//! # The catalogue is derived, not curated
//!
//! * **Gear** is every weapon, armour and shield template at tier 10 for level
//!   45+ — Dragonbone / Dragonscale / Dragonplate, the "Divine" line (the tier-10
//!   twin of a lower material), and the tier-10 artifacts.
//! * **Enchantments** are the rollable families at their top tier, and **perks**
//!   every player perk at its top rank — each kept only if the combat code shows
//!   it changing a spell hit. That is measured, by running every one through the
//!   calculator on each side against every spell ([`relevance`]), so an effect the
//!   engine does not model cannot be offered and one it starts modelling appears
//!   by itself.

use std::cell::Cell;
use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, OnceLock};

use actix_web::{
    HttpRequest, HttpResponse, get,
    http::StatusCode,
    post,
    web::{self, Json},
};
use diesel::prelude::*;
use diesel_async::RunQueryDsl;
use serde::Serialize;
use serde_json::{Value, json};
use uuid::Uuid;

use blades_lib::user_data::{CompleteCharacter, CompleteInventory, Item};

use crate::{
    ServerGlobal,
    admin::{IMPORT_SERVICE_ID, check_dev_token, check_import_token},
    arena::combat::{
        calc::{self, Build, BuildItem, Property},
        gamedata::{self, AbilityKind},
        loadout,
    },
    error::BladeApiError,
    models::CharacterDbEntryCharacterWalletInventory,
    schema::characters,
};

// ---------------------------------------------------------------------------
// Quiet scope
// ---------------------------------------------------------------------------

thread_local! {
    static QUIET: Cell<bool> = const { Cell::new(false) };
}

/// Run `f` with the combat engine's log lines dropped on this thread.
///
/// The engine logs every hit and status at `info` (`combat event: gsid=…`), and
/// those lines are what a fight is reconstructed from afterwards. A calculation
/// lands hundreds of hits that never happened; written to the journal they would
/// read as real ones.
pub fn quiet<T>(f: impl FnOnce() -> T) -> T {
    struct Reset(bool);
    impl Drop for Reset {
        fn drop(&mut self) {
            QUIET.with(|q| q.set(self.0));
        }
    }
    let _reset = Reset(QUIET.with(|q| q.replace(true)));
    f()
}

/// A logger that drops records made inside [`quiet`].
pub struct ScopedLogger<L>(pub L);

impl<L: log::Log> log::Log for ScopedLogger<L> {
    fn enabled(&self, metadata: &log::Metadata<'_>) -> bool {
        !QUIET.with(Cell::get) && self.0.enabled(metadata)
    }
    fn log(&self, record: &log::Record<'_>) {
        if !QUIET.with(Cell::get) {
            self.0.log(record);
        }
    }
    fn flush(&self) {
        self.0.flush();
    }
}

/// `env_logger::init()`, with the [`quiet`] scope in front of it.
pub fn init_logger() {
    let logger = env_logger::Builder::from_default_env().build();
    let max_level = logger.filter();
    if log::set_boxed_logger(Box::new(ScopedLogger(logger))).is_ok() {
        log::set_max_level(max_level);
    }
}

// ---------------------------------------------------------------------------
// Catalogue
// ---------------------------------------------------------------------------

const ELEMENTS: [&str; 4] = ["fire", "frost", "shock", "poison"];
/// Enchantment Synergy and Fortify Primary Effect do nothing alone — they scale
/// other enchantments — so the one-at-a-time probe cannot see them.
const ENCHANTMENT_SYNERGY: &str = "EnchantmentSynergy";
const FORTIFY_PRIMARY_LOGIC: &str = "FortifyPrimaryEnchantPropertyLogic";
pub const MAX_TEMPERING: u8 = 10;
const END_GAME_TIER: u8 = 10;
const END_GAME_LEVEL: u16 = 45;

/// "Divine <material>" is the tier-10 twin of a material that also exists lower
/// down. The generated tables carry only the display name, which is the same for
/// both, so the prefix is put back here. Checked against the 342 end-game
/// templates in `reference/game-defs/items.json`: it agrees with every
/// `editor_name`.
fn end_game_name(name: &str, has_lower_tier_twin: bool) -> String {
    if has_lower_tier_twin {
        format!("Divine {name}")
    } else {
        name.to_string()
    }
}

fn property_names(props: &[(&'static str, u8)]) -> Vec<Value> {
    props
        .iter()
        .filter_map(|(uuid, tier)| {
            let f = gamedata::enchant_family(uuid)?;
            Some(json!({ "id": f.uuid, "name": f.name, "tier": tier }))
        })
        .collect()
}

fn items() -> Vec<Value> {
    let mut out = Vec::new();
    let end_game = |tier: u8, level: u16| tier == END_GAME_TIER && level >= END_GAME_LEVEL;

    for w in gamedata::WEAPONS
        .iter()
        .filter(|w| end_game(w.tier, w.required_level))
    {
        let twin = gamedata::WEAPONS
            .iter()
            .any(|o| o.name == w.name && o.tier < END_GAME_TIER);
        out.push(json!({
            "id": w.uuid,
            "name": end_game_name(w.name, twin),
            "slot": "weapon",
            "class": format!("{:?}", w.weapon_class),
            "block": loadout::blocking_item_rating(w.uuid, w.block_base, u64::from(MAX_TEMPERING)),
            "properties": property_names(gamedata::mandatory_properties(w.uuid)),
        }));
    }
    for s in gamedata::SHIELDS
        .iter()
        .filter(|s| end_game(s.tier, s.required_level) && s.block_base > 0.0)
    {
        let twin = gamedata::SHIELDS
            .iter()
            .any(|o| o.name == s.name && o.tier < END_GAME_TIER);
        out.push(json!({
            "id": s.uuid,
            "name": end_game_name(s.name, twin),
            "slot": "shield",
            "class": if s.armor_class == 1 { "Light" } else { "Heavy" },
            "block": loadout::blocking_item_rating(s.uuid, s.block_base, u64::from(MAX_TEMPERING)),
            "properties": property_names(gamedata::mandatory_properties(s.uuid)),
        }));
    }
    for a in gamedata::ARMORS
        .iter()
        .filter(|a| end_game(a.tier, a.required_level) && a.armor_rating > 0.0)
    {
        let slot = match a.equipment_slot {
            1 => "helmet",
            3 => "chest",
            4 => "gauntlets",
            7 => "boots",
            _ => continue,
        };
        let twin = gamedata::ARMORS
            .iter()
            .any(|o| o.name == a.name && o.tier < END_GAME_TIER);
        out.push(json!({
            "id": a.uuid,
            "name": end_game_name(a.name, twin),
            "slot": slot,
            "class": if a.armor_class == 1 { "Light" } else { "Heavy" },
            "armor": loadout::armor_piece_rating(a, u64::from(MAX_TEMPERING)).ceil(),
            "properties": property_names(gamedata::mandatory_properties(a.uuid)),
        }));
    }
    out.sort_by(|a, b| {
        (a["slot"].as_str(), a["name"].as_str()).cmp(&(b["slot"].as_str(), b["name"].as_str()))
    });
    out
}

/// Which elements each enchantment family and perk changes a spell hit for, on
/// the attacker's side and on the defender's.
struct Relevance {
    /// family uuid -> (attack elements, defence elements)
    enchants: HashMap<&'static str, (BTreeSet<&'static str>, BTreeSet<&'static str>)>,
    perks: HashMap<&'static str, (BTreeSet<&'static str>, BTreeSet<&'static str>)>,
}

fn max_tier(f: &gamedata::EnchantFamily) -> u8 {
    f.tiers().iter().map(|t| t.tier).max().unwrap_or(0)
}

/// A plain end-game shield (no material properties), so a block exists to probe.
fn probe_shield() -> Option<Uuid> {
    gamedata::SHIELDS
        .iter()
        .find(|s| {
            s.tier == END_GAME_TIER
                && s.required_level >= END_GAME_LEVEL
                && s.block_base > 0.0
                && s.mandatory_properties.is_empty()
        })
        .and_then(|s| Uuid::parse_str(s.uuid).ok())
}

fn relevance() -> &'static Relevance {
    static CELL: OnceLock<Relevance> = OnceLock::new();
    CELL.get_or_init(|| quiet(measure_relevance))
}

fn measure_relevance() -> Relevance {
    let spells: Vec<&'static gamedata::Ability> = gamedata::ABILITIES
        .iter()
        .filter(|a| calc::is_castable_spell(a))
        .collect();
    let base_attacker = Build {
        level: 60,
        ..Default::default()
    };
    let base_defender = Build {
        level: 60,
        items: vec![BuildItem {
            template_id: probe_shield(),
            tempering_level: MAX_TEMPERING,
            ..Default::default()
        }],
        abilities: HashMap::from([
            (gamedata::ids::RESIST_ELEMENTS.to_string(), 1),
            (gamedata::ids::WARD.to_string(), 1),
        ]),
        ..Default::default()
    };
    // What a probe compares: everything a cell shows, at no prior damage and with
    // the condition already primed (so a change to its duration is visible too).
    let fingerprint = |spell: &gamedata::Ability, attacker: &Build, defender: &Build| -> String {
        let mut out = String::new();
        for prior in [0.0_f32, 100_000.0] {
            let req = calc::Request {
                spell: calc::Spell {
                    uuid: spell.uuid.to_string(),
                    rank: spell.maximum_purchaseable_level.max(1) as u8,
                },
                attacker: attacker.clone(),
                magicka_full: true,
                prior_element_damage: prior,
                defenders: vec![calc::DefenderRow {
                    id: "d".into(),
                    build: defender.clone(),
                }],
                states: [
                    (calc::Block::None, false, false),
                    (calc::Block::Low, false, false),
                    (calc::Block::Optimal, false, false),
                    (calc::Block::Optimal, true, false),
                    (calc::Block::None, false, true),
                ]
                .into_iter()
                .enumerate()
                .map(|(i, (block, re, ward))| calc::DefenderState {
                    id: i.to_string(),
                    block,
                    resist_elements: re,
                    ward,
                    potion: false,
                })
                .collect(),
            };
            match calc::calculate(&req) {
                Ok(r) => {
                    out.push_str(&format!(
                        "{:.3}|{:.3}|{}|",
                        r.attack.attack, r.attack.resistance_piercing, r.attack.effective_rank
                    ));
                    for c in &r.rows[0].cells {
                        out.push_str(&serde_json::to_string(c).unwrap_or_default());
                    }
                }
                Err(e) => out.push_str(&e.to_string()),
            }
        }
        out
    };
    let baseline: Vec<String> = spells
        .iter()
        .map(|s| fingerprint(s, &base_attacker, &base_defender))
        .collect();
    let element_of = |s: &gamedata::Ability| -> &'static str {
        s.damage_type
            .map(loadout::map_damage_type)
            .map(calc::damage_type_name)
            .unwrap_or("none")
    };
    let probe = |attacker: &Build, defender: &Build| -> BTreeSet<&'static str> {
        spells
            .iter()
            .zip(&baseline)
            .filter(|(s, base)| fingerprint(s, attacker, defender) != **base)
            .map(|(s, _)| element_of(s))
            .collect()
    };
    let all: BTreeSet<&'static str> = ELEMENTS.into_iter().collect();

    let mut enchants = HashMap::new();
    for f in gamedata::ENCHANT_FAMILIES
        .iter()
        .filter(|f| f.affix_type != 0)
    {
        if f.logic == FORTIFY_PRIMARY_LOGIC {
            enchants.insert(f.uuid, (all.clone(), all.clone()));
            continue;
        }
        let Ok(id) = Uuid::parse_str(f.uuid) else {
            continue;
        };
        let item = BuildItem {
            enchants: vec![Property {
                id,
                tier: max_tier(f),
            }],
            ..Default::default()
        };
        let mut attacker = base_attacker.clone();
        attacker.items.push(item.clone());
        let mut defender = base_defender.clone();
        // On the shield as well as on a second piece: a shield-only effect needs
        // the first, an armour one is indifferent.
        defender.items[0].enchants = item.enchants.clone();
        defender.items.push(item);
        enchants.insert(
            f.uuid,
            (
                probe(&attacker, &base_defender),
                probe(&base_attacker, &defender),
            ),
        );
    }

    let mut perks = HashMap::new();
    for a in gamedata::ABILITIES
        .iter()
        .filter(|a| a.kind == AbilityKind::Perk && !a.enemy_only)
    {
        if a.editor_name == ENCHANTMENT_SYNERGY {
            perks.insert(a.uuid, (all.clone(), all.clone()));
            continue;
        }
        let rank = a.maximum_level.min(u16::from(u8::MAX)) as u8;
        let mut attacker = base_attacker.clone();
        attacker.abilities.insert(a.uuid.to_string(), rank);
        let mut defender = base_defender.clone();
        defender.abilities.insert(a.uuid.to_string(), rank);
        perks.insert(
            a.uuid,
            (
                probe(&attacker, &base_defender),
                probe(&base_attacker, &defender),
            ),
        );
    }
    Relevance { enchants, perks }
}

fn build_catalogue() -> Value {
    let rel = relevance();
    let spells: Vec<Value> = gamedata::ABILITIES
        .iter()
        .filter(|a| calc::is_castable_spell(a))
        .map(|a| {
            let top = gamedata::ability_rank_clamped(a.uuid, a.maximum_level);
            json!({
                "id": a.uuid,
                "name": a.name,
                "element": a.damage_type.map(loadout::map_damage_type).map(calc::damage_type_name),
                "maxRank": a.maximum_level,
                "maxLearnedRank": a.maximum_purchaseable_level,
                "paralyses": loadout::ability_tag_for_template(a.uuid)
                    == crate::arena::combat::state::AbilityTag::Paralyze,
                "stuns": top.is_some_and(|r| r.damage_to_cause_stagger().is_some()),
                "channelled": top.is_some_and(|r| r.damage().is_none()),
            })
        })
        .collect();

    let mut enchants: Vec<Value> = gamedata::ENCHANT_FAMILIES
        .iter()
        .filter_map(|f| {
            let (attack, defence) = rel.enchants.get(f.uuid)?;
            if attack.is_empty() && defence.is_empty() {
                return None;
            }
            let tier = max_tier(f);
            Some(json!({
                "id": f.uuid,
                "name": f.name,
                "maxTier": tier,
                "value": gamedata::enchant_magnitude(f.uuid, tier),
                "attack": attack,
                "defence": defence,
                "scalesOthers": f.logic == FORTIFY_PRIMARY_LOGIC,
            }))
        })
        .collect();
    enchants.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let mut perks: Vec<Value> = gamedata::ABILITIES
        .iter()
        .filter_map(|a| {
            let (attack, defence) = rel.perks.get(a.uuid)?;
            if attack.is_empty() && defence.is_empty() {
                return None;
            }
            Some(json!({
                "id": a.uuid,
                "name": a.name,
                "maxRank": a.maximum_level,
                "maxLearnedRank": a.maximum_purchaseable_level,
                "attack": attack,
                "defence": defence,
            }))
        })
        .collect();
    perks.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let buff = |uuid: &str| {
        gamedata::ability(uuid).map(|a| {
            json!({
                "id": a.uuid,
                "name": a.name,
                "maxRank": a.maximum_level,
                "maxLearnedRank": a.maximum_purchaseable_level,
            })
        })
    };
    let grades: Vec<Value> = gamedata::GRADE_PROPERTIES
        .iter()
        .map(|g| {
            json!({
                "id": g.uuid,
                "abilityId": g.ability_uuid,
                "slot": g.slot.to_ascii_lowercase(),
            })
        })
        .collect();
    let potions: serde_json::Map<String, Value> = ELEMENTS
        .iter()
        .filter_map(|e| {
            let ty = match *e {
                "fire" => crate::arena::combat::state::DamageType::Fire,
                "frost" => crate::arena::combat::state::DamageType::Frost,
                "shock" => crate::arena::combat::state::DamageType::Shock,
                _ => crate::arena::combat::state::DamageType::Poison,
            };
            let p = calc::resist_potion(ty)?;
            Some((
                (*e).to_string(),
                json!({ "id": p.uuid, "value": p.value, "secs": p.duration }),
            ))
        })
        .collect();

    json!({
        "spells": spells,
        "items": items(),
        "enchants": enchants,
        "perks": perks,
        "resistElements": buff(gamedata::ids::RESIST_ELEMENTS),
        "ward": buff(gamedata::ids::WARD),
        "grades": grades,
        // Tier 1 and 2 are the only grade tiers the game ships.
        "gradeBonusRanks": { "1": 4, "2": 5 },
        "races": loadout::RACE_CATEGORIES
            .iter()
            .map(|(id, name)| json!({ "id": id, "name": name }))
            .collect::<Vec<_>>(),
        "potions": potions,
        "maxTempering": MAX_TEMPERING,
        "maxLevel": 100,
        "limits": {
            "defenders": calc::MAX_DEFENDERS,
            "states": calc::MAX_STATES,
            "items": calc::MAX_ITEMS,
            "propertiesPerItem": calc::MAX_PROPERTIES_PER_ITEM,
        },
    })
}

/// The catalogue as served. It depends only on compiled-in game data, so it is
/// built once per process.
pub fn catalogue_json() -> &'static str {
    static CELL: OnceLock<String> = OnceLock::new();
    CELL.get_or_init(|| build_catalogue().to_string())
}

// ---------------------------------------------------------------------------
// Loadouts
// ---------------------------------------------------------------------------

/// Equipment slot ids, as `blades_lib::features::character_ops` maps them.
const SLOTS: [(&str, &str); 9] = [
    ("417e79de-c810-42f8-8273-f9759df6ae25", "weapon"),
    ("862605de-c67f-4bce-b527-4e5fb6f25162", "shield"),
    ("48021ab1-a1a6-487b-80a4-ca472a4d0c77", "helmet"),
    ("897a600c-91d6-4449-af09-173da88a907e", "chest"),
    ("58b6d121-2e23-4fa4-b892-c92ae2e2c4c5", "gauntlets"),
    ("e273a4d7-fb87-4f7e-8f1e-398be59afbcb", "boots"),
    ("959c1931-bf85-4587-92ec-8ecaa58b06d5", "ring1"),
    ("0d8f2023-4701-41e8-8bd5-92381d787456", "ring2"),
    ("36d141e4-7783-466c-9565-6f90f09de428", "necklace"),
];

fn slot_name(slot: &Uuid) -> Option<&'static str> {
    let s = slot.as_hyphenated().to_string();
    SLOTS.iter().find(|(id, _)| *id == s).map(|(_, n)| *n)
}

fn template_name(template: &str) -> Option<&'static str> {
    gamedata::weapon(template)
        .map(|w| w.name)
        .or_else(|| gamedata::shield(template).map(|s| s.name))
        .or_else(|| gamedata::armor(template).map(|a| a.name))
}

fn build_item(slot: &Uuid, item: &Item) -> BuildItem {
    let template = item.item_template_id.as_hyphenated().to_string();
    let props = |ps: &[blades_lib::user_data::ItemSingleProperty]| -> Vec<Property> {
        ps.iter()
            .take(calc::MAX_PROPERTIES_PER_ITEM)
            .map(|p| Property {
                id: p.id,
                tier: p.tier.min(u64::from(u8::MAX)) as u8,
            })
            .collect()
    };
    BuildItem {
        template_id: Some(item.item_template_id),
        tempering_level: item.tempering_level.min(u64::from(u8::MAX)) as u8,
        enchants: props(&item.properties.enchanting),
        grades: props(&item.properties.grading),
        slot: slot_name(slot).map(str::to_string),
        name: template_name(&template).map(str::to_string),
    }
}

fn learned_abilities(character: &CompleteCharacter) -> HashMap<String, u8> {
    character
        .abilities
        .as_object()
        .map(|m| {
            m.iter()
                .filter(|(uuid, _)| gamedata::ability(uuid).is_some_and(|a| !a.enemy_only))
                .filter_map(|(uuid, level)| {
                    let level = level.as_u64().filter(|l| *l > 0)?;
                    Some((uuid.clone(), level.min(u64::from(u8::MAX)) as u8))
                })
                .take(calc::MAX_ABILITIES)
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NamedBuild {
    pub id: String,
    pub name: String,
    pub build: Build,
}

/// A character as calculator builds: what it wears now, then each saved loadout
/// profile (the in-game Loadouts screen). A profile names item instances, which
/// are looked up among the equipped items and the backpack; one that names an
/// item the character no longer holds simply lacks that slot.
pub fn builds_for(
    character: &CompleteCharacter,
    inventory: &CompleteInventory,
    customization: &Value,
) -> Vec<NamedBuild> {
    let base = Build {
        level: character.level,
        race: loadout::race_category(customization).map(str::to_string),
        stamina_points: character.stamina_attribute_points.min(u32::from(u16::MAX)) as u16,
        magicka_points: character.magicka_attribute_points.min(u32::from(u16::MAX)) as u16,
        items: Vec::new(),
        abilities: learned_abilities(character),
    };
    let order = |b: &BuildItem| {
        SLOTS
            .iter()
            .position(|(_, n)| Some(*n) == b.slot.as_deref())
            .unwrap_or(SLOTS.len())
    };

    let mut equipped: Vec<BuildItem> = inventory
        .loadout
        .equipped_items
        .0
        .iter()
        .map(|(slot, eq)| build_item(slot, &eq.item))
        .collect();
    equipped.sort_by_key(order);
    equipped.truncate(calc::MAX_ITEMS);
    let mut out = vec![NamedBuild {
        id: "equipped".into(),
        name: "Equipped now".into(),
        build: Build {
            items: equipped,
            ..base.clone()
        },
    }];

    let by_instance = |id: &Uuid| -> Option<&Item> {
        inventory
            .loadout
            .equipped_items
            .0
            .values()
            .find(|eq| eq.id == *id)
            .map(|eq| &eq.item)
            .or_else(|| inventory.backpack.items.0.get(id))
    };
    for (i, profile) in character
        .loadout_profiles
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
    {
        let Some(entries) = profile.get("equippedItems").and_then(Value::as_array) else {
            continue;
        };
        let mut items: Vec<BuildItem> = entries
            .iter()
            .filter_map(|e| {
                let slot = Uuid::parse_str(e.get("slot")?.as_str()?).ok()?;
                let id = Uuid::parse_str(e.get("itemId")?.as_str()?).ok()?;
                Some(build_item(&slot, by_instance(&id)?))
            })
            .collect();
        if items.is_empty() {
            continue;
        }
        items.sort_by_key(order);
        items.truncate(calc::MAX_ITEMS);
        let name = profile
            .get("name")
            .and_then(Value::as_str)
            .filter(|n| !n.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("Loadout {}", i + 1));
        out.push(NamedBuild {
            id: format!("profile-{i}"),
            name,
            build: Build {
                items,
                ..base.clone()
            },
        });
    }
    out
}

// ---------------------------------------------------------------------------
// Routes
// ---------------------------------------------------------------------------

/// The dev token, registered on its own so the two routes that need nothing else
/// from [`ServerGlobal`] can be served — and tested — without a database.
pub struct DevToken(pub Option<String>);

#[get("/blades.bgs.services/api/dev/v1/gear-calc/catalogue")]
pub async fn gear_calc_catalogue(
    req: HttpRequest,
    token: web::Data<DevToken>,
) -> Result<HttpResponse, BladeApiError> {
    check_dev_token(token.0.as_deref(), &req)?;
    Ok(HttpResponse::Ok()
        .content_type("application/json")
        .body(catalogue_json()))
}

#[post("/blades.bgs.services/api/dev/v1/gear-calc/calculate")]
pub async fn gear_calc_calculate(
    req: HttpRequest,
    token: web::Data<DevToken>,
    body: Json<calc::Request>,
) -> Result<HttpResponse, BladeApiError> {
    check_dev_token(token.0.as_deref(), &req)?;
    let started = std::time::Instant::now();
    match quiet(|| calc::calculate(&body)) {
        Ok(response) => {
            let mut value = serde_json::to_value(&response).unwrap_or(Value::Null);
            value["computeMicros"] = json!(started.elapsed().as_micros() as u64);
            Ok(HttpResponse::Ok().json(value))
        }
        Err(e) => Ok(HttpResponse::BadRequest().json(json!({ "error": e.to_string() }))),
    }
}

/// `GET …/dev/v1/gear-calc/loadouts?userId=` — the live character on that arena
/// account as calculator builds. `character` is null when the account has none.
#[get("/blades.bgs.services/api/dev/v1/gear-calc/loadouts")]
pub async fn gear_calc_loadouts(
    req: HttpRequest,
    app_state: web::Data<Arc<ServerGlobal>>,
    query: web::Query<HashMap<String, String>>,
) -> Result<HttpResponse, BladeApiError> {
    check_import_token(&app_state, &req)?;
    let user_id = query
        .get("userId")
        .and_then(|s| Uuid::parse_str(s).ok())
        .ok_or_else(|| BladeApiError::new(StatusCode::BAD_REQUEST, IMPORT_SERVICE_ID, 4))?;
    let mut conn = app_state.db_pool.get().await?;
    let row = characters::table
        .filter(characters::user_id.eq(user_id))
        .select(CharacterDbEntryCharacterWalletInventory::as_select())
        .first::<CharacterDbEntryCharacterWalletInventory>(&mut conn)
        .await
        .optional()?;
    Ok(HttpResponse::Ok().json(match row {
        Some(row) => json!({
            "character": {
                "id": row.id,
                "name": row.character.0.name,
                "level": row.character.0.level,
            },
            "loadouts": builds_for(&row.character.0, &row.inventory.0, &row.data.0.customization),
        }),
        None => json!({ "character": null, "loadouts": [] }),
    }))
}

#[cfg(test)]
mod tests;
