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

#[derive(Debug, Clone, Deserialize, Serialize)]
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