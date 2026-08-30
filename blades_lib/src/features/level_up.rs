use serde::Deserialize;
use std::collections::HashMap;
use log::{info, warn};

#[derive(Deserialize, Clone, Debug)]
pub struct LevelItemReward {
    pub template_id: String,
    pub quantity: u32,
}

#[derive(Deserialize, Clone, Debug)]
pub struct LevelReward {
    pub xp_to_reach: u32,
    pub skill_points: u32,
    pub gold_reward: u32,
    pub gems_reward: u32,
    pub attribute_points: u32,
    pub health_bonus: u32,
    pub reset_cost: u32,
    #[serde(default)]
    pub items: Vec<LevelItemReward>,
}

#[derive(Default, Clone, Debug)]
pub struct LevelUpData {
    pub rewards: HashMap<u32, LevelReward>,
}

impl LevelUpData {
    pub fn from_json(value: &serde_json::Value) -> Self {
        match serde_json::from_value::<HashMap<u32, LevelReward>>(value.clone()) {
            Ok(rewards) => {
                info!("[levelup] loaded {} level rewards", rewards.len());
                LevelUpData { rewards }
            }
            Err(e) => {
                warn!("[levelup] failed to parse level rewards: {e}; using empty");
                LevelUpData::default()
            }
        }
    }

    pub fn get_reward(&self, level: u32) -> Option<&LevelReward> {
        self.rewards.get(&level)
    }
}