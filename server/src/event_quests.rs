use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use uuid::Uuid;
use diesel::prelude::*;
use chrono::NaiveDateTime;
use diesel_async::AsyncPgConnection;
use diesel_async::RunQueryDsl;
use actix_web::http::StatusCode;

use crate::{
    BladeApiError,
    models::CharacterDbEntryCharacterWalletInventory,
};
use blades_lib::user_data::{CompleteWallet, InventoryChangeTracker};

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventQuestMeta {
    pub description: String,
    pub authoritative: String,
    pub derivation: String,
    #[serde(rename = "rewardModel")]
    pub reward_model: String,
    #[serde(rename = "payableRewards")]
    pub payable_rewards: String,
    #[serde(rename = "currencyItemIds")]
    pub currency_item_ids: Vec<Uuid>,
    pub templates: usize,
}

/// A reward payload.
///
/// `rename_all` is load-bearing and its absence was silent: the corpus writes
/// `characterXp`/`stackableItems`/`townXp`, none of which bind to a snake_case
/// field, and every one of them carries `#[serde(default)]` -- so a tier parsed
/// into an entirely EMPTY reward and the event paid nothing at all. `currencies`
/// worked only because its JSON name already equals its Rust name, which is
/// exactly why the failure was invisible: something always deserialised.
#[derive(Debug, Clone, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct EventQuestReward {
    #[serde(default)]
    pub character_xp: Option<u64>,
    #[serde(default)]
    pub stackable_items: HashMap<Uuid, u64>,
    #[serde(default)]
    pub currencies: HashMap<Uuid, u64>,
    #[serde(default)]
    pub town_xp: Option<u64>,
}

impl EventQuestReward {
    /// The last tier pays `rewards[last] + finalReward`, so the two payloads have
    /// to be added rather than one replacing the other. Currencies and stackable
    /// items accumulate per id; the XP fields sum, treating absent as zero.
    pub fn merged_with(&self, other: &EventQuestReward) -> EventQuestReward {
        let mut out = self.clone();
        for (id, n) in &other.stackable_items {
            *out.stackable_items.entry(*id).or_insert(0) += *n;
        }
        for (id, n) in &other.currencies {
            *out.currencies.entry(*id).or_insert(0) += *n;
        }
        if other.character_xp.is_some() {
            out.character_xp = Some(out.character_xp.unwrap_or(0) + other.character_xp.unwrap_or(0));
        }
        if other.town_xp.is_some() {
            out.town_xp = Some(out.town_xp.unwrap_or(0) + other.town_xp.unwrap_or(0));
        }
        out
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventQuestTemplate {
    #[serde(rename = "gldQuestId")]
    pub gld_quest_id: Uuid,
    pub version: u32,
    #[serde(rename = "objectiveIds")]
    pub objective_ids: Vec<Uuid>,
    pub rewards: Vec<EventQuestReward>,
    #[serde(rename = "finalReward")]
    pub final_reward: Option<EventQuestReward>,
    #[serde(rename = "eventIds")]
    pub event_ids: Vec<Uuid>,
     #[serde(rename = "payableRewards")]
    #[serde(default)]
    pub payable_rewards: HashMap<String, EventQuestPayableReward>,
    #[serde(rename = "_meta")]
    pub _meta: EventQuestTemplateMeta,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventQuestTemplateMeta {
    #[serde(rename = "instancesObserved")]
    pub instances_observed: u32,
    #[serde(rename = "rewardsObservations")]
    pub rewards_observations: u32,
    #[serde(rename = "rewardsVariants")]
    #[serde(default)]
    pub rewards_variants: Vec<EventQuestRewardVariant>,
    #[serde(rename = "finalRewardObservations")]
    pub final_reward_observations: u32,
    #[serde(rename = "finalRewardVariants")]
    #[serde(default)]
    pub final_reward_variants: Vec<EventQuestFinalRewardVariant>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventQuestRewardVariant {
    pub n: u32,
    pub value: Vec<EventQuestReward>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventQuestFinalRewardVariant {
    pub n: u32,
    pub value: EventQuestReward,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventQuestPayableReward {
    pub reward: EventQuestReward,
    pub observations: u32,
    pub variants: Vec<EventQuestPayableRewardVariant>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventQuestPayableRewardVariant {
    pub n: u32,
    pub value: EventQuestReward,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct EventQuestData {
    pub _meta: EventQuestMeta,
    pub templates: HashMap<Uuid, EventQuestTemplate>,
}

#[derive(Debug, Queryable, Selectable, Insertable, AsChangeset)]
#[diesel(table_name = crate::schema::event_completions)]
#[diesel(check_for_backend(diesel::pg::Pg))]
pub struct EventCompletion {
    pub id: Uuid,
    pub character_id: Uuid,
    pub event_id: Uuid,
    pub completion_count: i32,
    pub last_completed_at: NaiveDateTime,
    pub created_at: NaiveDateTime,
}

#[derive(Debug, Insertable)]
#[diesel(table_name = crate::schema::event_completions)]
pub struct NewEventCompletion {
    pub character_id: Uuid,
    pub event_id: Uuid,
}

impl EventQuestTemplate {
    /// What the `n`th completion of this event quest pays, or `None` when the
    /// event is finished.
    ///
    /// The model is documented in the data's own `_meta`, capture-derived and
    /// checked against 93 retail instances and 300+ completions:
    ///
    /// > The Nth completion of an event-quest instance pays `rewards[N]`; the
    /// > last one pays `rewards[last] + finalReward`.
    ///
    /// Two things follow that the first implementation got wrong. `finalReward`
    /// is a bonus ON the last tier, not a tier after it. And once the tiers are
    /// spent there is nothing left to pay -- paying `finalReward` alone on every
    /// later completion made the event farmable without limit (tracker #98).
    pub fn payout_for_completion(&self, n: usize) -> Option<EventQuestReward> {
        // `rewards[]` is the DISPLAY form -- the data's own _meta records that
        // retail lists the gem currency there under stackableItems, and under
        // `currencies` in the /complete body that actually grants it. Paying from
        // rewards[] would hand the player an item where retail gave currency, so
        // prefer payableRewards, which is that granting form keyed by completion
        // index. rewards[] remains the fallback for any template without one.
        let tier = match self.payable_rewards.get(&n.to_string()) {
            Some(payable) => payable.reward.clone(),
            None => self.rewards.get(n)?.clone(),
        };

        // Bounds still come from rewards[]: it is the authoritative tier count
        // (verbatim from retail), and payableRewards is keyed by whatever indices
        // happened to be observed.
        if n >= self.rewards.len() {
            return None;
        }

        if n + 1 == self.rewards.len() {
            return Some(match self.final_reward.as_ref() {
                Some(bonus) => tier.merged_with(bonus),
                None => tier,
            });
        }
        Some(tier)
    }
}

impl EventQuestData {
    pub fn from_json(value: &serde_json::Value) -> Self {
        if value.is_null() {
            log::warn!("[events] event_quests.json is null or missing; using empty event data");
            return Self {
                _meta: EventQuestMeta {
                    description: String::new(),
                    authoritative: String::new(),
                    derivation: String::new(),
                    reward_model: String::new(),
                    payable_rewards: String::new(),
                    currency_item_ids: Vec::new(),
                    templates: 0,
                },
                templates: HashMap::new(),
            };
        }
        
        match serde_json::from_value::<Self>(value.clone()) {
            Ok(data) => {
                data
            }
            Err(e) => {
                log::error!("[events] failed to parse event_quests.json: {}", e);
                Self {
                    _meta: EventQuestMeta {
                        description: String::new(),
                        authoritative: String::new(),
                        derivation: String::new(),
                        reward_model: String::new(),
                        payable_rewards: String::new(),
                        currency_item_ids: Vec::new(),
                        templates: 0,
                    },
                    templates: HashMap::new(),
                }
            }
        }
    }
}

impl EventCompletion {
    pub async fn get_or_create(
        conn: &mut AsyncPgConnection,
        char_id: Uuid,
        ev_id: Uuid,
    ) -> Result<Self, BladeApiError> {
        use crate::schema::event_completions::dsl::*;

        match event_completions
            .filter(character_id.eq(char_id))
            .filter(event_id.eq(ev_id))
            .select(EventCompletion::as_select())
            .first::<Self>(conn)
            .await
        {
            Ok(completion) => Ok(completion),
            Err(diesel::NotFound) => {
                let new = NewEventCompletion {
                    character_id: char_id,
                    event_id: ev_id,
                };
                diesel::insert_into(event_completions)
                    .values(&new)
                    .returning(EventCompletion::as_returning())
                    .get_result(conn)
                    .await
                    .map_err(|_e| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))
            }
            Err(_e) => Err(BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3)),
        }
    }

    pub async fn increment_completion(
        &mut self,
        conn: &mut AsyncPgConnection,
    ) -> Result<(), BladeApiError> {
        use crate::schema::event_completions::dsl::*;
        
        self.completion_count += 1;
        self.last_completed_at = chrono::Utc::now().naive_utc();
        
        diesel::update(event_completions)
            .filter(id.eq(self.id))
            .set((
                completion_count.eq(self.completion_count),
                last_completed_at.eq(self.last_completed_at),
            ))
            .execute(conn)
            .await
            .map_err(|_e| BladeApiError::new(StatusCode::INTERNAL_SERVER_ERROR, 20001, 3))?;
        
        Ok(())
    }
}

pub fn apply_event_rewards(
    rewards: &EventQuestReward,
    character_data: &mut CharacterDbEntryCharacterWalletInventory,
    wallet: &mut CompleteWallet,
    inventory_modification_tracker: &mut InventoryChangeTracker,
) -> Result<(), BladeApiError> {
    use blades_lib::economy::{apply_reward, RewardGrant};
    
    let mut reward_grant = RewardGrant::default();

    // Add XP
    if let Some(xp) = rewards.character_xp {
        character_data.character.0.experience += xp;
    }
    
    // Apply stackable items
    for (item_id, count) in &rewards.stackable_items {
        reward_grant.stackable_items.insert(*item_id, *count);
    }
    
    // Apply currencies
    for (currency_id, amount) in &rewards.currencies {
        reward_grant.currencies.insert(*currency_id, *amount);
    }

    if !reward_grant.currencies.is_empty() || !reward_grant.stackable_items.is_empty() {
        apply_reward(
            &reward_grant,
            wallet,
            &mut character_data.inventory.0,
            &mut character_data.character.0,
            inventory_modification_tracker,
        );
    }

    Ok(())
}

#[cfg(test)]
mod tier_progression {
    use super::*;

    /// The committed corpus the server actually loads -- not a hand-built fixture.
    /// A fixture here would only prove that my own assumption is self-consistent;
    /// the question is what the SHIPPED data does.
    fn data() -> EventQuestData {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/event_quests.json");
        let raw = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {path:?}: {e}"));
        EventQuestData::from_json(&serde_json::from_str(&raw).expect("valid event_quests.json"))
    }

    /// The granting payload for tier `n`, read straight from the corpus.
    /// Deliberately NOT `payout_for_completion` -- that is the code under test.
    fn granting_tier(tpl: &EventQuestTemplate, n: usize) -> EventQuestReward {
        match tpl.payable_rewards.get(&n.to_string()) {
            Some(p) => p.reward.clone(),
            None => tpl.rewards[n].clone(),
        }
    }

    /// Tracker #98: "I can do it over and over."
    ///
    /// Past the last tier there is nothing left to pay. The first implementation
    /// fell back to `finalReward` alone here and paid it on EVERY later exit,
    /// which made every event quest an unlimited source of its currency.
    #[test]
    fn an_exhausted_event_pays_nothing_however_many_times_it_is_run() {
        let d = data();
        assert!(!d.templates.is_empty(), "corpus must not be empty");

        for (id, tpl) in &d.templates {
            let n = tpl.rewards.len();
            for extra in 0..5 {
                assert!(
                    tpl.payout_for_completion(n + extra).is_none(),
                    "template {id} paid out on completion {} of {n} tiers",
                    n + extra
                );
            }
        }
    }

    /// The other half of the same rule, and the half that is easy to lose while
    /// fixing the first: the LAST tier pays `rewards[last] + finalReward`.
    #[test]
    fn the_last_tier_adds_the_final_reward_on_top() {
        let d = data();
        let mut checked = 0;

        for (id, tpl) in &d.templates {
            let Some(bonus) = tpl.final_reward.as_ref() else { continue };
            let last = tpl.rewards.len() - 1;
            let tier = granting_tier(tpl, last);
            let paid = tpl.payout_for_completion(last).expect("last tier must pay");

            for (cur, amount) in &bonus.currencies {
                let want = tier.currencies.get(cur).copied().unwrap_or(0) + amount;
                assert_eq!(
                    paid.currencies.get(cur).copied().unwrap_or(0),
                    want,
                    "template {id}: final reward currency {cur} must be ADDED to the last tier"
                );
            }
            for (item, qty) in &bonus.stackable_items {
                let want = tier.stackable_items.get(item).copied().unwrap_or(0) + qty;
                assert_eq!(
                    paid.stackable_items.get(item).copied().unwrap_or(0),
                    want,
                    "template {id}: final reward item {item} must be ADDED to the last tier"
                );
            }
            checked += 1;
        }

        assert!(checked > 0, "no template carried a finalReward — the test proved nothing");
    }

    /// Every tier before the last pays exactly its own entry, untouched. This is
    /// the control: without it, returning `finalReward` for everything would
    /// still satisfy the test above.
    #[test]
    fn earlier_tiers_pay_exactly_their_own_entry() {
        let d = data();
        for (id, tpl) in &d.templates {
            for n in 0..tpl.rewards.len().saturating_sub(1) {
                let paid = tpl.payout_for_completion(n).expect("tier must pay");
                let want = granting_tier(tpl, n);
                assert_eq!(
                    paid, want,
                    "template {id} tier {n}: must pay exactly its own granting entry"
                );
            }
        }
    }

    /// The reward payloads must actually CARRY something.
    ///
    /// `EventQuestReward` shipped without `rename_all`, so `characterXp`,
    /// `stackableItems` and `townXp` -- the only fields `rewards[]` uses -- bound
    /// to nothing and defaulted to empty. Every tier parsed successfully and paid
    /// absolutely nothing, and no test noticed because the struct still
    /// deserialised: `currencies` matched by accident, being the one field whose
    /// JSON name already equals its Rust name.
    ///
    /// This asserts on the VALUES, not the shape. A serde regression that empties
    /// the payload again fails here instead of reaching a player.
    #[test]
    fn every_field_the_corpus_specifies_survives_parsing() {
        let d = data();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../deploy/static/event_quests.json");
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let raw_templates = raw["templates"].as_object().expect("templates object");

        let mut checked_xp = 0;
        let mut checked_items = 0;

        for (id, tpl) in &d.templates {
            let raw_tpl = &raw_templates[&id.to_string()];
            for n in 0..tpl.rewards.len() {
                // the same source the implementation grants from, read raw
                let raw_tier = {
                    let payable = &raw_tpl["payableRewards"][n.to_string()]["reward"];
                    if payable.is_object() { payable.clone() } else { raw_tpl["rewards"][n].clone() }
                };
                let paid = tpl.payout_for_completion(n).expect("tier must pay");
                let is_last = n + 1 == tpl.rewards.len();

                if let Some(xp) = raw_tier["characterXp"].as_u64() {
                    if xp > 0 {
                        assert_eq!(
                            paid.character_xp.unwrap_or(0), xp,
                            "template {id} tier {n}: characterXp {xp} was lost in parsing"
                        );
                        checked_xp += 1;
                    }
                }

                if let Some(items) = raw_tier["stackableItems"].as_object() {
                    for (item, qty) in items {
                        let want = qty.as_u64().unwrap_or(0);
                        if want == 0 { continue; }
                        let key: Uuid = item.parse().expect("item id");
                        let got = paid.stackable_items.get(&key).copied().unwrap_or(0);
                        // the last tier also folds in finalReward, so it may exceed
                        let ok = if is_last { got >= want } else { got == want };
                        assert!(
                            ok,
                            "template {id} tier {n}: stackableItems[{item}] = {want} in the \
                             corpus but {got} after parsing"
                        );
                        checked_items += 1;
                    }
                }
            }
        }

        // Controls: if the corpus specified none of these, the loop above would
        // assert nothing at all and pass regardless of the code.
        assert!(checked_xp > 0, "no characterXp in the corpus — the test proved nothing");
        assert!(checked_items > 0, "no stackableItems in the corpus — the test proved nothing");
    }

    /// Specifically that the camelCase fields bind at all -- the single mistake
    /// behind the emptiness above.
    #[test]
    fn camel_case_reward_fields_bind() {
        let one: EventQuestReward = serde_json::from_str(
            r#"{"characterXp":700,"stackableItems":{"e7193116-d761-479b-8a20-5633737977f5":23},
                "currencies":{"c64bcb53-41f4-41ba-892a-fe2cca423caa":1},"townXp":5}"#,
        )
        .expect("a captured reward payload must deserialize");

        assert_eq!(one.character_xp, Some(700), "characterXp must bind");
        assert_eq!(one.town_xp, Some(5), "townXp must bind");
        assert_eq!(one.stackable_items.len(), 1, "stackableItems must bind");
        assert_eq!(one.currencies.len(), 1, "currencies must bind");
    }

    /// The corpus itself must keep the shape the rule assumes. If a future
    /// extraction ships a template with no tiers, `payout_for_completion(0)`
    /// returns None and that event silently pays nothing forever -- a failure
    /// that would otherwise only show up as a player complaint.
    #[test]
    fn every_template_has_tiers_to_pay() {
        let d = data();
        for (id, tpl) in &d.templates {
            assert!(!tpl.rewards.is_empty(), "template {id} ships no reward tiers");
            assert!(
                tpl.payout_for_completion(0).is_some(),
                "template {id} pays nothing on a first completion"
            );
        }
    }
}
