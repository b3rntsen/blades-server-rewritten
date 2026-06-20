use std::collections::{HashMap, HashSet};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::generate_map_to_vec_serialization;

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(rename_all = "camelCase")]
struct StackableItemStorageEntry {
    count: u64,
}

generate_map_to_vec_serialization!(
    stackable_item_serde,
    StackableItemStorageEntry,
    item_template_id
);

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StackableItems(
    #[serde(
        serialize_with = "stackable_item_serde::serialize",
        deserialize_with = "stackable_item_serde::deserialize"
    )]
    HashMap<Uuid, StackableItemStorageEntry>,
);

impl StackableItems {
    /// Current count of a stackable template (0 if absent).
    pub fn count(&self, template: Uuid) -> u64 {
        self.0.get(&template).map(|e| e.count).unwrap_or(0)
    }

    /// Grant `count` of a template, returning the new total.
    pub fn add(&mut self, template: Uuid, count: u64) -> u64 {
        let entry = self
            .0
            .entry(template)
            .or_insert(StackableItemStorageEntry { count: 0 });
        entry.count += count;
        entry.count
    }

    /// Consume `count` of a template. Returns the remaining count on success, or
    /// `Err(available)` if there isn't enough. A template consumed to zero is
    /// removed from the map (so the inventory diff reports it as removed).
    pub fn remove(&mut self, template: Uuid, count: u64) -> Result<u64, u64> {
        let have = self.count(template);
        if have < count {
            return Err(have);
        }
        let entry = self.0.get_mut(&template).expect("checked above");
        entry.count -= count;
        let remaining = entry.count;
        if remaining == 0 {
            self.0.remove(&template);
        }
        Ok(remaining)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct ItemSingleProperty {
    pub id: Uuid,
    pub tier: u64,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "UPPERCASE")]
pub struct ItemPropertiesAll {
    // Defaulted so an item carrying only one of the two property kinds (common in
    // capture-derived rewards) still deserializes.
    #[serde(default)]
    pub enchanting: Vec<ItemSingleProperty>,
    #[serde(default)]
    pub grading: Vec<ItemSingleProperty>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Item {
    pub item_template_id: Uuid,
    // Defaulted so capture-derived reward items that omit them (special/consumable
    // items carrying only a grade) still deserialize; normal gear always sends them.
    #[serde(default)]
    pub tempering_level: u64,
    #[serde(default)]
    pub durability: f64,
    //TODO: do not serialize if there is no property
    #[serde(default)]
    pub properties: ItemPropertiesAll,
}

generate_map_to_vec_serialization!(items_serde, Item, id);

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Items(
    #[serde(
        serialize_with = "items_serde::serialize",
        deserialize_with = "items_serde::deserialize"
    )]
    pub HashMap<Uuid, Item>,
);
impl Items {
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Backpack {
    pub stackable_items: StackableItems,
    pub items: Items,
}

// camelCase is REQUIRED: the client reads `removedItems`/`stackableItems`/
// `removedStackableItems` from the inventory diff. Serializing snake_case meant the
// client never saw removed items → backpack desync → the screen hung on any inventory
// mutation (temper/craft/buy/sell/repair/salvage). Matches the captured wire shape.
#[derive(Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct BackpackUpdate {
    pub stackable_items: StackableItems,
    pub items: Items,
    pub removed_items: HashSet<Uuid>,
    pub removed_stackable_items: HashSet<Uuid>,
}

impl Backpack {
    pub fn generate_client_update(&self, tracker: &BackpackChangeTracker) -> BackpackUpdate {
        let mut update = BackpackUpdate::default();

        for changed_stackable_id in &tracker.stackable_items {
            if let Some(item) = self.stackable_items.0.get(changed_stackable_id) {
                update
                    .stackable_items
                    .0
                    .insert(*changed_stackable_id, item.clone());
            } else {
                update.removed_stackable_items.insert(*changed_stackable_id);
            }
        }

        for changed_item_id in &tracker.items {
            if let Some(item) = self.items.0.get(changed_item_id) {
                update.items.0.insert(*changed_item_id, item.clone());
            } else {
                update.removed_items.insert(*changed_item_id);
            }
        }
        update
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SingleEquippedItem {
    pub id: Uuid,
    /// This should be kept up to date with what slot it is in the parent EquippedItems
    pub slot: Uuid,
    #[serde(flatten)]
    pub item: Item,
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct EquippedItems(
    /// the UUID is the slot, and NOT the item id
    pub HashMap<Uuid, SingleEquippedItem>,
);

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Loadout {
    pub equipped_items: EquippedItems,
}

#[derive(Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct LoadoutUpdate {
    pub equipped_items: EquippedItems,
    pub unequipped_item_slots: HashSet<Uuid>,
}

impl Loadout {
    pub fn generate_client_update(&self, tracker: &LoadoutChangeTracker) -> LoadoutUpdate {
        let mut update = LoadoutUpdate::default();

        for updated_loadout in &tracker.modified_equipped_items {
            if let Some(item) = self.equipped_items.0.get(&updated_loadout) {
                update
                    .equipped_items
                    .0
                    .insert(*updated_loadout, item.clone());
            } else {
                update.unequipped_item_slots.insert(*updated_loadout);
            }
        }
        update
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Chest {
    /// A treasury chest id is a number stored as a string ("1", "2", …).
    pub id: String,
    pub tier: u64,
    pub level: u64,
}

impl Chest {
    pub fn new(id: String, tier: u64, level: u64) -> Self {
        Chest { id, tier, level }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct Treasury {
    chests: Vec<Chest>,
}

impl Treasury {
    pub fn chests(&self) -> &[Chest] {
        &self.chests
    }

    /// Next free numeric chest id (max existing + 1), as a string.
    fn next_id(&self) -> String {
        let max = self
            .chests
            .iter()
            .filter_map(|c| c.id.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        (max + 1).to_string()
    }

    /// Add a chest of the given tier/level, returning its new id.
    pub fn add_chest(&mut self, tier: u64, level: u64) -> String {
        let id = self.next_id();
        self.chests.push(Chest::new(id.clone(), tier, level));
        id
    }

    pub fn get_chest(&self, id: &str) -> Option<&Chest> {
        self.chests.iter().find(|c| c.id == id)
    }

    /// Remove a chest by id, returning it if present.
    pub fn remove_chest(&mut self, id: &str) -> Option<Chest> {
        let pos = self.chests.iter().position(|c| c.id == id)?;
        Some(self.chests.remove(pos))
    }
}

/// The treasury diff returned to the client: chests added this request (full) and the
/// ids removed (`removedChests`). Default is `{chests:[]}` — wire-identical to the
/// previous always-empty `Treasury::default()` the inventory update used to send.
#[derive(Serialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct TreasuryUpdate {
    pub chests: Vec<Chest>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub removed_chests: Vec<String>,
}

#[derive(Default, Debug, Clone)]
pub struct TreasuryChangeTracker {
    /// Ids of chests added this request.
    pub added: Vec<String>,
    /// Ids of chests removed this request.
    pub removed: Vec<String>,
}

impl Treasury {
    pub fn generate_client_update(&self, tracker: &TreasuryChangeTracker) -> TreasuryUpdate {
        TreasuryUpdate {
            chests: tracker
                .added
                .iter()
                .filter_map(|id| self.get_chest(id).cloned())
                .collect(),
            removed_chests: tracker.removed.clone(),
        }
    }
}

#[derive(Serialize, Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CompleteInventory {
    pub backpack: Backpack,
    pub loadout: Loadout,
    pub treasury: Treasury,
    // what is this overflow treasury responsible for?
    pub overflow_treasury: Treasury,
    pub backpack_version: u64,
    pub treasury_version: u64,
}

#[derive(Serialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct CompleteInventoryUpdate {
    pub backpack: BackpackUpdate,
    pub loadout: LoadoutUpdate,
    pub treasury: TreasuryUpdate,
    pub overflow_treasury: TreasuryUpdate,
    pub backpack_version: u64,
    pub treasury_version: u64,
}

impl CompleteInventory {
    pub fn generate_client_update(
        &self,
        tracker: &InventoryChangeTracker,
    ) -> CompleteInventoryUpdate {
        CompleteInventoryUpdate {
            backpack_version: self.backpack_version,
            treasury_version: self.treasury_version,
            backpack: self
                .backpack
                .generate_client_update(&tracker.modified_backpack),
            loadout: self
                .loadout
                .generate_client_update(&tracker.modified_loadout),
            treasury: self
                .treasury
                .generate_client_update(&tracker.modified_treasury),
            overflow_treasury: TreasuryUpdate::default(),
        }
    }
}

#[derive(Default, Debug, Clone)]
pub struct BackpackChangeTracker {
    pub stackable_items: HashSet<Uuid>,
    pub items: HashSet<Uuid>,
}

#[derive(Default, Debug, Clone)]
pub struct LoadoutChangeTracker {
    pub modified_equipped_items: HashSet<Uuid>,
}

#[derive(Default, Debug, Clone)]
pub struct InventoryChangeTracker {
    pub modified_loadout: LoadoutChangeTracker,
    pub modified_backpack: BackpackChangeTracker,
    pub modified_treasury: TreasuryChangeTracker,
}

#[cfg(test)]
mod wire_camelcase_tests {
    use super::*;
    #[test]
    fn inventory_diff_is_camelcase() {
        let mut bp = BackpackUpdate::default();
        bp.removed_items.insert(uuid::Uuid::nil());
        let j = serde_json::to_string(&bp).unwrap();
        assert!(j.contains("removedItems"), "BackpackUpdate not camelCase: {j}");
        assert!(!j.contains("removed_items"), "BackpackUpdate still snake: {j}");
        let lo = LoadoutUpdate::default();
        let lj = serde_json::to_string(&lo).unwrap();
        assert!(lj.contains("equippedItems") && lj.contains("unequippedItemSlots"), "LoadoutUpdate not camelCase: {lj}");
    }
}
