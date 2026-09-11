//! Arena-promotion stackables generated from retail's shipped loot tables.
//!
//! `[Class 1 — shipped game data]`. This intentionally includes only fixed,
//! guaranteed entries with a `specific_item_uid`; randomized equipment needs
//! the retail loot generator and is not silently approximated here.
//!
//! Regenerate with `script/gen_arena_promotion_loot.py` and set
//! `BLADES_CAPTURE_DIR` when the capture repository is not a sibling.

#![allow(dead_code)]

pub const SOURCE_LOOT_JSON_SHA256: &str = "b68d2d46aa1d2a95836238faa2b7068056b45d5b4d571a268b132a6952b6f245";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StackableLoot {
    pub template_uuid: &'static str,
    pub quantity: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PromotionLootBand {
    pub loot_table: &'static str,
    pub min_character_level: u16,
    pub stackables: &'static [StackableLoot],
}

pub const PROMOTION_LOOT_BANDS: [PromotionLootBand; 226] = [
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel1",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel2",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel3",
        min_character_level: 1,
        stackables: &[
            // Iron Ingot
            StackableLoot { template_uuid: "55e82826-2d68-469c-8870-753665ca62cd", quantity: 10 },
            // Potion of Minor Healing
            StackableLoot { template_uuid: "d5ccf370-0795-4554-9dab-68ccbb97473d", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel3",
        min_character_level: 5,
        stackables: &[
            // Steel Ingot
            StackableLoot { template_uuid: "b81952e0-c3c8-4a5c-92c0-8215d3eb71af", quantity: 10 },
            // Potion of Light Healing
            StackableLoot { template_uuid: "d826ea12-e583-47c1-a50f-4de608281735", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel3",
        min_character_level: 10,
        stackables: &[
            // Potion of Healing
            StackableLoot { template_uuid: "819094ad-e749-4c02-9210-38c3bb1ec535", quantity: 3 },
            // Silver Ingot
            StackableLoot { template_uuid: "b74a5c55-a687-4604-aa59-ba3ddfddcd2a", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel3",
        min_character_level: 15,
        stackables: &[
            // Orichalcum Ingot
            StackableLoot { template_uuid: "74f091b5-fd88-464b-a98a-f60a5e8a0f25", quantity: 10 },
            // Potion of Strong Healing
            StackableLoot { template_uuid: "aa52a6a4-6500-44b5-ad74-9eb1d58e38c7", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel3",
        min_character_level: 20,
        stackables: &[
            // Dwarven Metal Ingot
            StackableLoot { template_uuid: "f11fb90b-b441-4d72-a33f-50d14d3d6778", quantity: 10 },
            // Potion of Major Healing
            StackableLoot { template_uuid: "f9d2869a-a517-4080-abac-be6af47e5556", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel3",
        min_character_level: 25,
        stackables: &[
            // Potion of Plentiful Healing
            StackableLoot { template_uuid: "21e6557f-17ca-4bd3-9379-00184efe0edc", quantity: 3 },
            // Quicksilver Ingot
            StackableLoot { template_uuid: "e80bee76-f92c-4005-9eff-20d1e8c64d24", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel3",
        min_character_level: 30,
        stackables: &[
            // Potion of Vigorous Healing
            StackableLoot { template_uuid: "1c5c5ce3-178b-4938-89f3-faf3fa7f0664", quantity: 3 },
            // Malachite Ingot
            StackableLoot { template_uuid: "85ed5500-3581-4699-8095-4b5ff6514355", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel3",
        min_character_level: 35,
        stackables: &[
            // Potion of Intense Healing
            StackableLoot { template_uuid: "605786d6-1b00-48ec-b765-eb81c5e4a1df", quantity: 3 },
            // Ebony Ingot
            StackableLoot { template_uuid: "75112030-b248-49b0-9c70-0da8dea150d1", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel3",
        min_character_level: 40,
        stackables: &[
            // Potion of Extreme Healing
            StackableLoot { template_uuid: "61b31323-8ba2-49f2-befe-f43111c6e2c7", quantity: 3 },
            // Daedra Heart
            StackableLoot { template_uuid: "f9181a67-b094-4c37-a145-ced9dfe610d6", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel3",
        min_character_level: 45,
        stackables: &[
            // Potion of Ultimate Healing
            StackableLoot { template_uuid: "c2139cd9-1d9d-4d4e-80b2-133e07440158", quantity: 3 },
            // Dragon Bones
            StackableLoot { template_uuid: "d523932f-8c7f-4192-9112-5dbd60883c2b", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel4",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel5",
        min_character_level: 1,
        stackables: &[
            // Petty Soul Gem
            StackableLoot { template_uuid: "19ce1a65-057f-4f34-a0ed-27de7c085662", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel5",
        min_character_level: 5,
        stackables: &[
            // Lesser Soul Gem
            StackableLoot { template_uuid: "790a188b-3fa0-4f38-99d9-bc8d3675bc46", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel5",
        min_character_level: 10,
        stackables: &[
            // Middling Soul Gem
            StackableLoot { template_uuid: "eca5bd64-5e5d-4d0d-bfa3-b6fd427be029", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel5",
        min_character_level: 15,
        stackables: &[
            // Common Soul Gem
            StackableLoot { template_uuid: "1ba210b4-8cca-4f2f-b942-8fab80a52fd8", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel5",
        min_character_level: 20,
        stackables: &[
            // Exceptional Soul Gem
            StackableLoot { template_uuid: "3932e499-441e-4c6d-b671-9a03131ebe6f", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel5",
        min_character_level: 25,
        stackables: &[
            // Greater Soul Gem
            StackableLoot { template_uuid: "a1d41da0-51e0-4a80-ba9a-b8e9046be27e", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel5",
        min_character_level: 30,
        stackables: &[
            // Elevated Soul Gem
            StackableLoot { template_uuid: "a3351353-f613-4368-bac7-05783f857b07", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel5",
        min_character_level: 35,
        stackables: &[
            // Grand Soul Gem
            StackableLoot { template_uuid: "68d7941e-8c8d-47bf-9f66-becb058f1817", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel5",
        min_character_level: 40,
        stackables: &[
            // Glorious Soul Gem
            StackableLoot { template_uuid: "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel5",
        min_character_level: 45,
        stackables: &[
            // Transcendent Soul Gem
            StackableLoot { template_uuid: "d94bab85-53d5-4c9c-a637-acd94fc66c98", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel6",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel7",
        min_character_level: 1,
        stackables: &[
            // Potion of Minor Stamina
            StackableLoot { template_uuid: "42a22694-40eb-4c34-a267-c9aef2d96f56", quantity: 3 },
            // Iron Ingot
            StackableLoot { template_uuid: "55e82826-2d68-469c-8870-753665ca62cd", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel7",
        min_character_level: 5,
        stackables: &[
            // Potion of Light Stamina
            StackableLoot { template_uuid: "979f5025-b9b8-4b1e-b312-f96f294f97ae", quantity: 3 },
            // Steel Ingot
            StackableLoot { template_uuid: "b81952e0-c3c8-4a5c-92c0-8215d3eb71af", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel7",
        min_character_level: 10,
        stackables: &[
            // Silver Ingot
            StackableLoot { template_uuid: "b74a5c55-a687-4604-aa59-ba3ddfddcd2a", quantity: 10 },
            // Potion of Stamina
            StackableLoot { template_uuid: "fc29f4b5-3764-4241-aa1c-c623c4fbeff4", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel7",
        min_character_level: 15,
        stackables: &[
            // Potion of Strong Stamina
            StackableLoot { template_uuid: "45fbef4d-9244-456e-8fc0-f23f0750b3fa", quantity: 3 },
            // Orichalcum Ingot
            StackableLoot { template_uuid: "74f091b5-fd88-464b-a98a-f60a5e8a0f25", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel7",
        min_character_level: 20,
        stackables: &[
            // Potion of Major Stamina
            StackableLoot { template_uuid: "1e12dc39-9029-4cfa-a7f1-9eebfbe240cf", quantity: 3 },
            // Dwarven Metal Ingot
            StackableLoot { template_uuid: "f11fb90b-b441-4d72-a33f-50d14d3d6778", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel7",
        min_character_level: 25,
        stackables: &[
            // Potion of Plentiful Stamina
            StackableLoot { template_uuid: "34988d74-de88-43af-adea-445def4949be", quantity: 3 },
            // Quicksilver Ingot
            StackableLoot { template_uuid: "e80bee76-f92c-4005-9eff-20d1e8c64d24", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel7",
        min_character_level: 30,
        stackables: &[
            // Malachite Ingot
            StackableLoot { template_uuid: "85ed5500-3581-4699-8095-4b5ff6514355", quantity: 10 },
            // Potion of Vigorous Stamina
            StackableLoot { template_uuid: "af316825-826d-4fba-8554-a99d2fe291e9", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel7",
        min_character_level: 35,
        stackables: &[
            // Ebony Ingot
            StackableLoot { template_uuid: "75112030-b248-49b0-9c70-0da8dea150d1", quantity: 10 },
            // Potion of Intense Stamina
            StackableLoot { template_uuid: "a578cb1a-c6d9-4cf7-be42-be56e403b937", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel7",
        min_character_level: 40,
        stackables: &[
            // Potion of Extreme Stamina
            StackableLoot { template_uuid: "52e2f139-fd26-4707-8a96-8e823de59a66", quantity: 3 },
            // Daedra Heart
            StackableLoot { template_uuid: "f9181a67-b094-4c37-a145-ced9dfe610d6", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel7",
        min_character_level: 45,
        stackables: &[
            // Potion of Ultimate Stamina
            StackableLoot { template_uuid: "8da5101c-2e7c-446b-b3bd-d1b9aa5c44f6", quantity: 3 },
            // Dragon Bones
            StackableLoot { template_uuid: "d523932f-8c7f-4192-9112-5dbd60883c2b", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel8",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel9",
        min_character_level: 1,
        stackables: &[
            // Pearl
            StackableLoot { template_uuid: "89ece62c-9ff6-470a-a152-d45ef4e0c222", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel9",
        min_character_level: 5,
        stackables: &[
            // Topaz
            StackableLoot { template_uuid: "3ca59113-a093-4cc8-8389-0f578ad94851", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel9",
        min_character_level: 10,
        stackables: &[
            // Garnet
            StackableLoot { template_uuid: "9df9233f-e7b9-47e8-bc75-f37707917759", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel9",
        min_character_level: 15,
        stackables: &[
            // Amethyst
            StackableLoot { template_uuid: "3ec6cf6f-d90e-4b76-bb7f-82da251ab5e5", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel9",
        min_character_level: 20,
        stackables: &[
            // Ruby
            StackableLoot { template_uuid: "cf4b1b42-a736-4aa1-99b8-baca9f9c2276", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel9",
        min_character_level: 25,
        stackables: &[
            // Sapphire
            StackableLoot { template_uuid: "014606cd-8898-4c1d-8029-63220a179c47", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel9",
        min_character_level: 30,
        stackables: &[
            // Emerald
            StackableLoot { template_uuid: "4d1231be-d3fa-4282-81b2-0f7bed4aabff", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel9",
        min_character_level: 35,
        stackables: &[
            // Diamond
            StackableLoot { template_uuid: "16e102fb-b1c0-42de-8106-0aa27e77f7f0", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel9",
        min_character_level: 40,
        stackables: &[
            // Atronite
            StackableLoot { template_uuid: "ab97efe1-bae9-4d16-8fd9-e05bad9ecb95", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena1_ArenaLevel9",
        min_character_level: 45,
        stackables: &[
            // Faerite
            StackableLoot { template_uuid: "70f2c013-813e-4f63-8b29-7a54a13b5e58", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel1",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel2",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel3",
        min_character_level: 1,
        stackables: &[
            // Iron Ingot
            StackableLoot { template_uuid: "55e82826-2d68-469c-8870-753665ca62cd", quantity: 10 },
            // Potion of Minor Healing
            StackableLoot { template_uuid: "d5ccf370-0795-4554-9dab-68ccbb97473d", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel3",
        min_character_level: 5,
        stackables: &[
            // Steel Ingot
            StackableLoot { template_uuid: "b81952e0-c3c8-4a5c-92c0-8215d3eb71af", quantity: 10 },
            // Potion of Light Healing
            StackableLoot { template_uuid: "d826ea12-e583-47c1-a50f-4de608281735", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel3",
        min_character_level: 10,
        stackables: &[
            // Potion of Healing
            StackableLoot { template_uuid: "819094ad-e749-4c02-9210-38c3bb1ec535", quantity: 3 },
            // Silver Ingot
            StackableLoot { template_uuid: "b74a5c55-a687-4604-aa59-ba3ddfddcd2a", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel3",
        min_character_level: 15,
        stackables: &[
            // Orichalcum Ingot
            StackableLoot { template_uuid: "74f091b5-fd88-464b-a98a-f60a5e8a0f25", quantity: 10 },
            // Potion of Strong Healing
            StackableLoot { template_uuid: "aa52a6a4-6500-44b5-ad74-9eb1d58e38c7", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel3",
        min_character_level: 20,
        stackables: &[
            // Dwarven Metal Ingot
            StackableLoot { template_uuid: "f11fb90b-b441-4d72-a33f-50d14d3d6778", quantity: 10 },
            // Potion of Major Healing
            StackableLoot { template_uuid: "f9d2869a-a517-4080-abac-be6af47e5556", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel3",
        min_character_level: 25,
        stackables: &[
            // Potion of Plentiful Healing
            StackableLoot { template_uuid: "21e6557f-17ca-4bd3-9379-00184efe0edc", quantity: 3 },
            // Quicksilver Ingot
            StackableLoot { template_uuid: "e80bee76-f92c-4005-9eff-20d1e8c64d24", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel3",
        min_character_level: 30,
        stackables: &[
            // Potion of Vigorous Healing
            StackableLoot { template_uuid: "1c5c5ce3-178b-4938-89f3-faf3fa7f0664", quantity: 3 },
            // Malachite Ingot
            StackableLoot { template_uuid: "85ed5500-3581-4699-8095-4b5ff6514355", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel3",
        min_character_level: 35,
        stackables: &[
            // Potion of Intense Healing
            StackableLoot { template_uuid: "605786d6-1b00-48ec-b765-eb81c5e4a1df", quantity: 3 },
            // Ebony Ingot
            StackableLoot { template_uuid: "75112030-b248-49b0-9c70-0da8dea150d1", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel3",
        min_character_level: 40,
        stackables: &[
            // Potion of Extreme Healing
            StackableLoot { template_uuid: "61b31323-8ba2-49f2-befe-f43111c6e2c7", quantity: 3 },
            // Daedra Heart
            StackableLoot { template_uuid: "f9181a67-b094-4c37-a145-ced9dfe610d6", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel3",
        min_character_level: 45,
        stackables: &[
            // Potion of Ultimate Healing
            StackableLoot { template_uuid: "c2139cd9-1d9d-4d4e-80b2-133e07440158", quantity: 3 },
            // Dragon Bones
            StackableLoot { template_uuid: "d523932f-8c7f-4192-9112-5dbd60883c2b", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel4",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel5",
        min_character_level: 1,
        stackables: &[
            // Petty Soul Gem
            StackableLoot { template_uuid: "19ce1a65-057f-4f34-a0ed-27de7c085662", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel5",
        min_character_level: 5,
        stackables: &[
            // Lesser Soul Gem
            StackableLoot { template_uuid: "790a188b-3fa0-4f38-99d9-bc8d3675bc46", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel5",
        min_character_level: 10,
        stackables: &[
            // Middling Soul Gem
            StackableLoot { template_uuid: "eca5bd64-5e5d-4d0d-bfa3-b6fd427be029", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel5",
        min_character_level: 15,
        stackables: &[
            // Common Soul Gem
            StackableLoot { template_uuid: "1ba210b4-8cca-4f2f-b942-8fab80a52fd8", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel5",
        min_character_level: 20,
        stackables: &[
            // Exceptional Soul Gem
            StackableLoot { template_uuid: "3932e499-441e-4c6d-b671-9a03131ebe6f", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel5",
        min_character_level: 25,
        stackables: &[
            // Greater Soul Gem
            StackableLoot { template_uuid: "a1d41da0-51e0-4a80-ba9a-b8e9046be27e", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel5",
        min_character_level: 30,
        stackables: &[
            // Elevated Soul Gem
            StackableLoot { template_uuid: "a3351353-f613-4368-bac7-05783f857b07", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel5",
        min_character_level: 35,
        stackables: &[
            // Grand Soul Gem
            StackableLoot { template_uuid: "68d7941e-8c8d-47bf-9f66-becb058f1817", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel5",
        min_character_level: 40,
        stackables: &[
            // Glorious Soul Gem
            StackableLoot { template_uuid: "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel5",
        min_character_level: 45,
        stackables: &[
            // Transcendent Soul Gem
            StackableLoot { template_uuid: "d94bab85-53d5-4c9c-a637-acd94fc66c98", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel6",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel7",
        min_character_level: 1,
        stackables: &[
            // Potion of Minor Stamina
            StackableLoot { template_uuid: "42a22694-40eb-4c34-a267-c9aef2d96f56", quantity: 3 },
            // Iron Ingot
            StackableLoot { template_uuid: "55e82826-2d68-469c-8870-753665ca62cd", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel7",
        min_character_level: 5,
        stackables: &[
            // Potion of Light Stamina
            StackableLoot { template_uuid: "979f5025-b9b8-4b1e-b312-f96f294f97ae", quantity: 3 },
            // Steel Ingot
            StackableLoot { template_uuid: "b81952e0-c3c8-4a5c-92c0-8215d3eb71af", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel7",
        min_character_level: 10,
        stackables: &[
            // Silver Ingot
            StackableLoot { template_uuid: "b74a5c55-a687-4604-aa59-ba3ddfddcd2a", quantity: 10 },
            // Potion of Stamina
            StackableLoot { template_uuid: "fc29f4b5-3764-4241-aa1c-c623c4fbeff4", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel7",
        min_character_level: 15,
        stackables: &[
            // Potion of Strong Stamina
            StackableLoot { template_uuid: "45fbef4d-9244-456e-8fc0-f23f0750b3fa", quantity: 3 },
            // Orichalcum Ingot
            StackableLoot { template_uuid: "74f091b5-fd88-464b-a98a-f60a5e8a0f25", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel7",
        min_character_level: 20,
        stackables: &[
            // Potion of Major Stamina
            StackableLoot { template_uuid: "1e12dc39-9029-4cfa-a7f1-9eebfbe240cf", quantity: 3 },
            // Dwarven Metal Ingot
            StackableLoot { template_uuid: "f11fb90b-b441-4d72-a33f-50d14d3d6778", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel7",
        min_character_level: 25,
        stackables: &[
            // Potion of Plentiful Stamina
            StackableLoot { template_uuid: "34988d74-de88-43af-adea-445def4949be", quantity: 3 },
            // Quicksilver Ingot
            StackableLoot { template_uuid: "e80bee76-f92c-4005-9eff-20d1e8c64d24", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel7",
        min_character_level: 30,
        stackables: &[
            // Malachite Ingot
            StackableLoot { template_uuid: "85ed5500-3581-4699-8095-4b5ff6514355", quantity: 10 },
            // Potion of Vigorous Stamina
            StackableLoot { template_uuid: "af316825-826d-4fba-8554-a99d2fe291e9", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel7",
        min_character_level: 35,
        stackables: &[
            // Ebony Ingot
            StackableLoot { template_uuid: "75112030-b248-49b0-9c70-0da8dea150d1", quantity: 10 },
            // Potion of Intense Stamina
            StackableLoot { template_uuid: "a578cb1a-c6d9-4cf7-be42-be56e403b937", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel7",
        min_character_level: 40,
        stackables: &[
            // Potion of Extreme Stamina
            StackableLoot { template_uuid: "52e2f139-fd26-4707-8a96-8e823de59a66", quantity: 3 },
            // Daedra Heart
            StackableLoot { template_uuid: "f9181a67-b094-4c37-a145-ced9dfe610d6", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel7",
        min_character_level: 45,
        stackables: &[
            // Potion of Ultimate Stamina
            StackableLoot { template_uuid: "8da5101c-2e7c-446b-b3bd-d1b9aa5c44f6", quantity: 3 },
            // Dragon Bones
            StackableLoot { template_uuid: "d523932f-8c7f-4192-9112-5dbd60883c2b", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel8",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel9",
        min_character_level: 1,
        stackables: &[
            // Pearl
            StackableLoot { template_uuid: "89ece62c-9ff6-470a-a152-d45ef4e0c222", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel9",
        min_character_level: 5,
        stackables: &[
            // Topaz
            StackableLoot { template_uuid: "3ca59113-a093-4cc8-8389-0f578ad94851", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel9",
        min_character_level: 10,
        stackables: &[
            // Garnet
            StackableLoot { template_uuid: "9df9233f-e7b9-47e8-bc75-f37707917759", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel9",
        min_character_level: 15,
        stackables: &[
            // Amethyst
            StackableLoot { template_uuid: "3ec6cf6f-d90e-4b76-bb7f-82da251ab5e5", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel9",
        min_character_level: 20,
        stackables: &[
            // Ruby
            StackableLoot { template_uuid: "cf4b1b42-a736-4aa1-99b8-baca9f9c2276", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel9",
        min_character_level: 25,
        stackables: &[
            // Sapphire
            StackableLoot { template_uuid: "014606cd-8898-4c1d-8029-63220a179c47", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel9",
        min_character_level: 30,
        stackables: &[
            // Emerald
            StackableLoot { template_uuid: "4d1231be-d3fa-4282-81b2-0f7bed4aabff", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel9",
        min_character_level: 35,
        stackables: &[
            // Diamond
            StackableLoot { template_uuid: "16e102fb-b1c0-42de-8106-0aa27e77f7f0", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel9",
        min_character_level: 40,
        stackables: &[
            // Atronite
            StackableLoot { template_uuid: "ab97efe1-bae9-4d16-8fd9-e05bad9ecb95", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena2_ArenaLevel9",
        min_character_level: 45,
        stackables: &[
            // Faerite
            StackableLoot { template_uuid: "70f2c013-813e-4f63-8b29-7a54a13b5e58", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel1",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel2",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel3",
        min_character_level: 1,
        stackables: &[
            // Iron Ingot
            StackableLoot { template_uuid: "55e82826-2d68-469c-8870-753665ca62cd", quantity: 10 },
            // Potion of Minor Healing
            StackableLoot { template_uuid: "d5ccf370-0795-4554-9dab-68ccbb97473d", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel3",
        min_character_level: 5,
        stackables: &[
            // Steel Ingot
            StackableLoot { template_uuid: "b81952e0-c3c8-4a5c-92c0-8215d3eb71af", quantity: 10 },
            // Potion of Light Healing
            StackableLoot { template_uuid: "d826ea12-e583-47c1-a50f-4de608281735", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel3",
        min_character_level: 10,
        stackables: &[
            // Potion of Healing
            StackableLoot { template_uuid: "819094ad-e749-4c02-9210-38c3bb1ec535", quantity: 3 },
            // Silver Ingot
            StackableLoot { template_uuid: "b74a5c55-a687-4604-aa59-ba3ddfddcd2a", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel3",
        min_character_level: 15,
        stackables: &[
            // Orichalcum Ingot
            StackableLoot { template_uuid: "74f091b5-fd88-464b-a98a-f60a5e8a0f25", quantity: 10 },
            // Potion of Strong Healing
            StackableLoot { template_uuid: "aa52a6a4-6500-44b5-ad74-9eb1d58e38c7", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel3",
        min_character_level: 20,
        stackables: &[
            // Dwarven Metal Ingot
            StackableLoot { template_uuid: "f11fb90b-b441-4d72-a33f-50d14d3d6778", quantity: 10 },
            // Potion of Major Healing
            StackableLoot { template_uuid: "f9d2869a-a517-4080-abac-be6af47e5556", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel3",
        min_character_level: 25,
        stackables: &[
            // Potion of Plentiful Healing
            StackableLoot { template_uuid: "21e6557f-17ca-4bd3-9379-00184efe0edc", quantity: 3 },
            // Quicksilver Ingot
            StackableLoot { template_uuid: "e80bee76-f92c-4005-9eff-20d1e8c64d24", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel3",
        min_character_level: 30,
        stackables: &[
            // Potion of Vigorous Healing
            StackableLoot { template_uuid: "1c5c5ce3-178b-4938-89f3-faf3fa7f0664", quantity: 3 },
            // Malachite Ingot
            StackableLoot { template_uuid: "85ed5500-3581-4699-8095-4b5ff6514355", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel3",
        min_character_level: 35,
        stackables: &[
            // Potion of Intense Healing
            StackableLoot { template_uuid: "605786d6-1b00-48ec-b765-eb81c5e4a1df", quantity: 3 },
            // Ebony Ingot
            StackableLoot { template_uuid: "75112030-b248-49b0-9c70-0da8dea150d1", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel3",
        min_character_level: 40,
        stackables: &[
            // Potion of Extreme Healing
            StackableLoot { template_uuid: "61b31323-8ba2-49f2-befe-f43111c6e2c7", quantity: 3 },
            // Daedra Heart
            StackableLoot { template_uuid: "f9181a67-b094-4c37-a145-ced9dfe610d6", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel3",
        min_character_level: 45,
        stackables: &[
            // Potion of Ultimate Healing
            StackableLoot { template_uuid: "c2139cd9-1d9d-4d4e-80b2-133e07440158", quantity: 3 },
            // Dragon Bones
            StackableLoot { template_uuid: "d523932f-8c7f-4192-9112-5dbd60883c2b", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel4",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel5",
        min_character_level: 1,
        stackables: &[
            // Petty Soul Gem
            StackableLoot { template_uuid: "19ce1a65-057f-4f34-a0ed-27de7c085662", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel5",
        min_character_level: 5,
        stackables: &[
            // Lesser Soul Gem
            StackableLoot { template_uuid: "790a188b-3fa0-4f38-99d9-bc8d3675bc46", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel5",
        min_character_level: 10,
        stackables: &[
            // Middling Soul Gem
            StackableLoot { template_uuid: "eca5bd64-5e5d-4d0d-bfa3-b6fd427be029", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel5",
        min_character_level: 15,
        stackables: &[
            // Common Soul Gem
            StackableLoot { template_uuid: "1ba210b4-8cca-4f2f-b942-8fab80a52fd8", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel5",
        min_character_level: 20,
        stackables: &[
            // Exceptional Soul Gem
            StackableLoot { template_uuid: "3932e499-441e-4c6d-b671-9a03131ebe6f", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel5",
        min_character_level: 25,
        stackables: &[
            // Greater Soul Gem
            StackableLoot { template_uuid: "a1d41da0-51e0-4a80-ba9a-b8e9046be27e", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel5",
        min_character_level: 30,
        stackables: &[
            // Elevated Soul Gem
            StackableLoot { template_uuid: "a3351353-f613-4368-bac7-05783f857b07", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel5",
        min_character_level: 35,
        stackables: &[
            // Grand Soul Gem
            StackableLoot { template_uuid: "68d7941e-8c8d-47bf-9f66-becb058f1817", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel5",
        min_character_level: 40,
        stackables: &[
            // Glorious Soul Gem
            StackableLoot { template_uuid: "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel5",
        min_character_level: 45,
        stackables: &[
            // Transcendent Soul Gem
            StackableLoot { template_uuid: "d94bab85-53d5-4c9c-a637-acd94fc66c98", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel6",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel7",
        min_character_level: 1,
        stackables: &[
            // Potion of Minor Stamina
            StackableLoot { template_uuid: "42a22694-40eb-4c34-a267-c9aef2d96f56", quantity: 3 },
            // Iron Ingot
            StackableLoot { template_uuid: "55e82826-2d68-469c-8870-753665ca62cd", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel7",
        min_character_level: 5,
        stackables: &[
            // Potion of Light Stamina
            StackableLoot { template_uuid: "979f5025-b9b8-4b1e-b312-f96f294f97ae", quantity: 3 },
            // Steel Ingot
            StackableLoot { template_uuid: "b81952e0-c3c8-4a5c-92c0-8215d3eb71af", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel7",
        min_character_level: 10,
        stackables: &[
            // Silver Ingot
            StackableLoot { template_uuid: "b74a5c55-a687-4604-aa59-ba3ddfddcd2a", quantity: 10 },
            // Potion of Stamina
            StackableLoot { template_uuid: "fc29f4b5-3764-4241-aa1c-c623c4fbeff4", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel7",
        min_character_level: 15,
        stackables: &[
            // Potion of Strong Stamina
            StackableLoot { template_uuid: "45fbef4d-9244-456e-8fc0-f23f0750b3fa", quantity: 3 },
            // Orichalcum Ingot
            StackableLoot { template_uuid: "74f091b5-fd88-464b-a98a-f60a5e8a0f25", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel7",
        min_character_level: 20,
        stackables: &[
            // Potion of Major Stamina
            StackableLoot { template_uuid: "1e12dc39-9029-4cfa-a7f1-9eebfbe240cf", quantity: 3 },
            // Dwarven Metal Ingot
            StackableLoot { template_uuid: "f11fb90b-b441-4d72-a33f-50d14d3d6778", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel7",
        min_character_level: 25,
        stackables: &[
            // Potion of Plentiful Stamina
            StackableLoot { template_uuid: "34988d74-de88-43af-adea-445def4949be", quantity: 3 },
            // Quicksilver Ingot
            StackableLoot { template_uuid: "e80bee76-f92c-4005-9eff-20d1e8c64d24", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel7",
        min_character_level: 30,
        stackables: &[
            // Malachite Ingot
            StackableLoot { template_uuid: "85ed5500-3581-4699-8095-4b5ff6514355", quantity: 10 },
            // Potion of Vigorous Stamina
            StackableLoot { template_uuid: "af316825-826d-4fba-8554-a99d2fe291e9", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel7",
        min_character_level: 35,
        stackables: &[
            // Ebony Ingot
            StackableLoot { template_uuid: "75112030-b248-49b0-9c70-0da8dea150d1", quantity: 10 },
            // Potion of Intense Stamina
            StackableLoot { template_uuid: "a578cb1a-c6d9-4cf7-be42-be56e403b937", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel7",
        min_character_level: 40,
        stackables: &[
            // Potion of Extreme Stamina
            StackableLoot { template_uuid: "52e2f139-fd26-4707-8a96-8e823de59a66", quantity: 3 },
            // Daedra Heart
            StackableLoot { template_uuid: "f9181a67-b094-4c37-a145-ced9dfe610d6", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel7",
        min_character_level: 45,
        stackables: &[
            // Potion of Ultimate Stamina
            StackableLoot { template_uuid: "8da5101c-2e7c-446b-b3bd-d1b9aa5c44f6", quantity: 3 },
            // Dragon Bones
            StackableLoot { template_uuid: "d523932f-8c7f-4192-9112-5dbd60883c2b", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel8",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel9",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel9",
        min_character_level: 5,
        stackables: &[
            // Topaz
            StackableLoot { template_uuid: "3ca59113-a093-4cc8-8389-0f578ad94851", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel9",
        min_character_level: 10,
        stackables: &[
            // Garnet
            StackableLoot { template_uuid: "9df9233f-e7b9-47e8-bc75-f37707917759", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel9",
        min_character_level: 15,
        stackables: &[
            // Amethyst
            StackableLoot { template_uuid: "3ec6cf6f-d90e-4b76-bb7f-82da251ab5e5", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel9",
        min_character_level: 20,
        stackables: &[
            // Ruby
            StackableLoot { template_uuid: "cf4b1b42-a736-4aa1-99b8-baca9f9c2276", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel9",
        min_character_level: 25,
        stackables: &[
            // Sapphire
            StackableLoot { template_uuid: "014606cd-8898-4c1d-8029-63220a179c47", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel9",
        min_character_level: 30,
        stackables: &[
            // Emerald
            StackableLoot { template_uuid: "4d1231be-d3fa-4282-81b2-0f7bed4aabff", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel9",
        min_character_level: 35,
        stackables: &[
            // Diamond
            StackableLoot { template_uuid: "16e102fb-b1c0-42de-8106-0aa27e77f7f0", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel9",
        min_character_level: 40,
        stackables: &[
            // Atronite
            StackableLoot { template_uuid: "ab97efe1-bae9-4d16-8fd9-e05bad9ecb95", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena3_ArenaLevel9",
        min_character_level: 45,
        stackables: &[
            // Faerite
            StackableLoot { template_uuid: "70f2c013-813e-4f63-8b29-7a54a13b5e58", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel1",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel2",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel3",
        min_character_level: 1,
        stackables: &[
            // Iron Ingot
            StackableLoot { template_uuid: "55e82826-2d68-469c-8870-753665ca62cd", quantity: 10 },
            // Potion of Minor Healing
            StackableLoot { template_uuid: "d5ccf370-0795-4554-9dab-68ccbb97473d", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel3",
        min_character_level: 5,
        stackables: &[
            // Steel Ingot
            StackableLoot { template_uuid: "b81952e0-c3c8-4a5c-92c0-8215d3eb71af", quantity: 10 },
            // Potion of Light Healing
            StackableLoot { template_uuid: "d826ea12-e583-47c1-a50f-4de608281735", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel3",
        min_character_level: 10,
        stackables: &[
            // Potion of Healing
            StackableLoot { template_uuid: "819094ad-e749-4c02-9210-38c3bb1ec535", quantity: 3 },
            // Silver Ingot
            StackableLoot { template_uuid: "b74a5c55-a687-4604-aa59-ba3ddfddcd2a", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel3",
        min_character_level: 15,
        stackables: &[
            // Orichalcum Ingot
            StackableLoot { template_uuid: "74f091b5-fd88-464b-a98a-f60a5e8a0f25", quantity: 10 },
            // Potion of Strong Healing
            StackableLoot { template_uuid: "aa52a6a4-6500-44b5-ad74-9eb1d58e38c7", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel3",
        min_character_level: 20,
        stackables: &[
            // Dwarven Metal Ingot
            StackableLoot { template_uuid: "f11fb90b-b441-4d72-a33f-50d14d3d6778", quantity: 10 },
            // Potion of Major Healing
            StackableLoot { template_uuid: "f9d2869a-a517-4080-abac-be6af47e5556", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel3",
        min_character_level: 25,
        stackables: &[
            // Potion of Plentiful Healing
            StackableLoot { template_uuid: "21e6557f-17ca-4bd3-9379-00184efe0edc", quantity: 3 },
            // Quicksilver Ingot
            StackableLoot { template_uuid: "e80bee76-f92c-4005-9eff-20d1e8c64d24", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel3",
        min_character_level: 30,
        stackables: &[
            // Potion of Vigorous Healing
            StackableLoot { template_uuid: "1c5c5ce3-178b-4938-89f3-faf3fa7f0664", quantity: 3 },
            // Malachite Ingot
            StackableLoot { template_uuid: "85ed5500-3581-4699-8095-4b5ff6514355", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel3",
        min_character_level: 35,
        stackables: &[
            // Potion of Intense Healing
            StackableLoot { template_uuid: "605786d6-1b00-48ec-b765-eb81c5e4a1df", quantity: 3 },
            // Ebony Ingot
            StackableLoot { template_uuid: "75112030-b248-49b0-9c70-0da8dea150d1", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel3",
        min_character_level: 40,
        stackables: &[
            // Potion of Extreme Healing
            StackableLoot { template_uuid: "61b31323-8ba2-49f2-befe-f43111c6e2c7", quantity: 3 },
            // Daedra Heart
            StackableLoot { template_uuid: "f9181a67-b094-4c37-a145-ced9dfe610d6", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel3",
        min_character_level: 45,
        stackables: &[
            // Potion of Ultimate Healing
            StackableLoot { template_uuid: "c2139cd9-1d9d-4d4e-80b2-133e07440158", quantity: 3 },
            // Dragon Bones
            StackableLoot { template_uuid: "d523932f-8c7f-4192-9112-5dbd60883c2b", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel4",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel5",
        min_character_level: 1,
        stackables: &[
            // Petty Soul Gem
            StackableLoot { template_uuid: "19ce1a65-057f-4f34-a0ed-27de7c085662", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel5",
        min_character_level: 5,
        stackables: &[
            // Lesser Soul Gem
            StackableLoot { template_uuid: "790a188b-3fa0-4f38-99d9-bc8d3675bc46", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel5",
        min_character_level: 10,
        stackables: &[
            // Middling Soul Gem
            StackableLoot { template_uuid: "eca5bd64-5e5d-4d0d-bfa3-b6fd427be029", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel5",
        min_character_level: 15,
        stackables: &[
            // Common Soul Gem
            StackableLoot { template_uuid: "1ba210b4-8cca-4f2f-b942-8fab80a52fd8", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel5",
        min_character_level: 20,
        stackables: &[
            // Exceptional Soul Gem
            StackableLoot { template_uuid: "3932e499-441e-4c6d-b671-9a03131ebe6f", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel5",
        min_character_level: 25,
        stackables: &[
            // Greater Soul Gem
            StackableLoot { template_uuid: "a1d41da0-51e0-4a80-ba9a-b8e9046be27e", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel5",
        min_character_level: 30,
        stackables: &[
            // Elevated Soul Gem
            StackableLoot { template_uuid: "a3351353-f613-4368-bac7-05783f857b07", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel5",
        min_character_level: 35,
        stackables: &[
            // Grand Soul Gem
            StackableLoot { template_uuid: "68d7941e-8c8d-47bf-9f66-becb058f1817", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel5",
        min_character_level: 40,
        stackables: &[
            // Glorious Soul Gem
            StackableLoot { template_uuid: "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel5",
        min_character_level: 45,
        stackables: &[
            // Transcendent Soul Gem
            StackableLoot { template_uuid: "d94bab85-53d5-4c9c-a637-acd94fc66c98", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel6",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel7",
        min_character_level: 1,
        stackables: &[
            // Potion of Minor Stamina
            StackableLoot { template_uuid: "42a22694-40eb-4c34-a267-c9aef2d96f56", quantity: 3 },
            // Iron Ingot
            StackableLoot { template_uuid: "55e82826-2d68-469c-8870-753665ca62cd", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel7",
        min_character_level: 5,
        stackables: &[
            // Potion of Light Stamina
            StackableLoot { template_uuid: "979f5025-b9b8-4b1e-b312-f96f294f97ae", quantity: 3 },
            // Steel Ingot
            StackableLoot { template_uuid: "b81952e0-c3c8-4a5c-92c0-8215d3eb71af", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel7",
        min_character_level: 10,
        stackables: &[
            // Silver Ingot
            StackableLoot { template_uuid: "b74a5c55-a687-4604-aa59-ba3ddfddcd2a", quantity: 10 },
            // Potion of Stamina
            StackableLoot { template_uuid: "fc29f4b5-3764-4241-aa1c-c623c4fbeff4", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel7",
        min_character_level: 15,
        stackables: &[
            // Potion of Strong Stamina
            StackableLoot { template_uuid: "45fbef4d-9244-456e-8fc0-f23f0750b3fa", quantity: 3 },
            // Orichalcum Ingot
            StackableLoot { template_uuid: "74f091b5-fd88-464b-a98a-f60a5e8a0f25", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel7",
        min_character_level: 20,
        stackables: &[
            // Potion of Major Stamina
            StackableLoot { template_uuid: "1e12dc39-9029-4cfa-a7f1-9eebfbe240cf", quantity: 3 },
            // Dwarven Metal Ingot
            StackableLoot { template_uuid: "f11fb90b-b441-4d72-a33f-50d14d3d6778", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel7",
        min_character_level: 25,
        stackables: &[
            // Potion of Plentiful Stamina
            StackableLoot { template_uuid: "34988d74-de88-43af-adea-445def4949be", quantity: 3 },
            // Quicksilver Ingot
            StackableLoot { template_uuid: "e80bee76-f92c-4005-9eff-20d1e8c64d24", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel7",
        min_character_level: 30,
        stackables: &[
            // Malachite Ingot
            StackableLoot { template_uuid: "85ed5500-3581-4699-8095-4b5ff6514355", quantity: 10 },
            // Potion of Vigorous Stamina
            StackableLoot { template_uuid: "af316825-826d-4fba-8554-a99d2fe291e9", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel7",
        min_character_level: 35,
        stackables: &[
            // Ebony Ingot
            StackableLoot { template_uuid: "75112030-b248-49b0-9c70-0da8dea150d1", quantity: 10 },
            // Potion of Intense Stamina
            StackableLoot { template_uuid: "a578cb1a-c6d9-4cf7-be42-be56e403b937", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel7",
        min_character_level: 40,
        stackables: &[
            // Potion of Extreme Stamina
            StackableLoot { template_uuid: "52e2f139-fd26-4707-8a96-8e823de59a66", quantity: 3 },
            // Daedra Heart
            StackableLoot { template_uuid: "f9181a67-b094-4c37-a145-ced9dfe610d6", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel7",
        min_character_level: 45,
        stackables: &[
            // Potion of Ultimate Stamina
            StackableLoot { template_uuid: "8da5101c-2e7c-446b-b3bd-d1b9aa5c44f6", quantity: 3 },
            // Dragon Bones
            StackableLoot { template_uuid: "d523932f-8c7f-4192-9112-5dbd60883c2b", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel8",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel9",
        min_character_level: 1,
        stackables: &[
            // Pearl
            StackableLoot { template_uuid: "89ece62c-9ff6-470a-a152-d45ef4e0c222", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel9",
        min_character_level: 5,
        stackables: &[
            // Topaz
            StackableLoot { template_uuid: "3ca59113-a093-4cc8-8389-0f578ad94851", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel9",
        min_character_level: 10,
        stackables: &[
            // Garnet
            StackableLoot { template_uuid: "9df9233f-e7b9-47e8-bc75-f37707917759", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel9",
        min_character_level: 15,
        stackables: &[
            // Amethyst
            StackableLoot { template_uuid: "3ec6cf6f-d90e-4b76-bb7f-82da251ab5e5", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel9",
        min_character_level: 20,
        stackables: &[
            // Ruby
            StackableLoot { template_uuid: "cf4b1b42-a736-4aa1-99b8-baca9f9c2276", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel9",
        min_character_level: 25,
        stackables: &[
            // Sapphire
            StackableLoot { template_uuid: "014606cd-8898-4c1d-8029-63220a179c47", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel9",
        min_character_level: 30,
        stackables: &[
            // Emerald
            StackableLoot { template_uuid: "4d1231be-d3fa-4282-81b2-0f7bed4aabff", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel9",
        min_character_level: 35,
        stackables: &[
            // Diamond
            StackableLoot { template_uuid: "16e102fb-b1c0-42de-8106-0aa27e77f7f0", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel9",
        min_character_level: 40,
        stackables: &[
            // Atronite
            StackableLoot { template_uuid: "ab97efe1-bae9-4d16-8fd9-e05bad9ecb95", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena4_ArenaLevel9",
        min_character_level: 45,
        stackables: &[
            // Faerite
            StackableLoot { template_uuid: "70f2c013-813e-4f63-8b29-7a54a13b5e58", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel1",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel2",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel3",
        min_character_level: 1,
        stackables: &[
            // Iron Ingot
            StackableLoot { template_uuid: "55e82826-2d68-469c-8870-753665ca62cd", quantity: 10 },
            // Potion of Minor Healing
            StackableLoot { template_uuid: "d5ccf370-0795-4554-9dab-68ccbb97473d", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel3",
        min_character_level: 5,
        stackables: &[
            // Steel Ingot
            StackableLoot { template_uuid: "b81952e0-c3c8-4a5c-92c0-8215d3eb71af", quantity: 10 },
            // Potion of Light Healing
            StackableLoot { template_uuid: "d826ea12-e583-47c1-a50f-4de608281735", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel3",
        min_character_level: 10,
        stackables: &[
            // Potion of Healing
            StackableLoot { template_uuid: "819094ad-e749-4c02-9210-38c3bb1ec535", quantity: 3 },
            // Silver Ingot
            StackableLoot { template_uuid: "b74a5c55-a687-4604-aa59-ba3ddfddcd2a", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel3",
        min_character_level: 15,
        stackables: &[
            // Orichalcum Ingot
            StackableLoot { template_uuid: "74f091b5-fd88-464b-a98a-f60a5e8a0f25", quantity: 10 },
            // Potion of Strong Healing
            StackableLoot { template_uuid: "aa52a6a4-6500-44b5-ad74-9eb1d58e38c7", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel3",
        min_character_level: 20,
        stackables: &[
            // Dwarven Metal Ingot
            StackableLoot { template_uuid: "f11fb90b-b441-4d72-a33f-50d14d3d6778", quantity: 10 },
            // Potion of Major Healing
            StackableLoot { template_uuid: "f9d2869a-a517-4080-abac-be6af47e5556", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel3",
        min_character_level: 25,
        stackables: &[
            // Potion of Plentiful Healing
            StackableLoot { template_uuid: "21e6557f-17ca-4bd3-9379-00184efe0edc", quantity: 3 },
            // Quicksilver Ingot
            StackableLoot { template_uuid: "e80bee76-f92c-4005-9eff-20d1e8c64d24", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel3",
        min_character_level: 30,
        stackables: &[
            // Potion of Vigorous Healing
            StackableLoot { template_uuid: "1c5c5ce3-178b-4938-89f3-faf3fa7f0664", quantity: 3 },
            // Malachite Ingot
            StackableLoot { template_uuid: "85ed5500-3581-4699-8095-4b5ff6514355", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel3",
        min_character_level: 35,
        stackables: &[
            // Potion of Intense Healing
            StackableLoot { template_uuid: "605786d6-1b00-48ec-b765-eb81c5e4a1df", quantity: 3 },
            // Ebony Ingot
            StackableLoot { template_uuid: "75112030-b248-49b0-9c70-0da8dea150d1", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel3",
        min_character_level: 40,
        stackables: &[
            // Potion of Extreme Healing
            StackableLoot { template_uuid: "61b31323-8ba2-49f2-befe-f43111c6e2c7", quantity: 3 },
            // Daedra Heart
            StackableLoot { template_uuid: "f9181a67-b094-4c37-a145-ced9dfe610d6", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel3",
        min_character_level: 45,
        stackables: &[
            // Potion of Ultimate Healing
            StackableLoot { template_uuid: "c2139cd9-1d9d-4d4e-80b2-133e07440158", quantity: 3 },
            // Dragon Bones
            StackableLoot { template_uuid: "d523932f-8c7f-4192-9112-5dbd60883c2b", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel4",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel5",
        min_character_level: 1,
        stackables: &[
            // Petty Soul Gem
            StackableLoot { template_uuid: "19ce1a65-057f-4f34-a0ed-27de7c085662", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel5",
        min_character_level: 5,
        stackables: &[
            // Lesser Soul Gem
            StackableLoot { template_uuid: "790a188b-3fa0-4f38-99d9-bc8d3675bc46", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel5",
        min_character_level: 10,
        stackables: &[
            // Middling Soul Gem
            StackableLoot { template_uuid: "eca5bd64-5e5d-4d0d-bfa3-b6fd427be029", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel5",
        min_character_level: 15,
        stackables: &[
            // Common Soul Gem
            StackableLoot { template_uuid: "1ba210b4-8cca-4f2f-b942-8fab80a52fd8", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel5",
        min_character_level: 20,
        stackables: &[
            // Exceptional Soul Gem
            StackableLoot { template_uuid: "3932e499-441e-4c6d-b671-9a03131ebe6f", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel5",
        min_character_level: 25,
        stackables: &[
            // Greater Soul Gem
            StackableLoot { template_uuid: "a1d41da0-51e0-4a80-ba9a-b8e9046be27e", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel5",
        min_character_level: 30,
        stackables: &[
            // Elevated Soul Gem
            StackableLoot { template_uuid: "a3351353-f613-4368-bac7-05783f857b07", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel5",
        min_character_level: 35,
        stackables: &[
            // Grand Soul Gem
            StackableLoot { template_uuid: "68d7941e-8c8d-47bf-9f66-becb058f1817", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel5",
        min_character_level: 40,
        stackables: &[
            // Glorious Soul Gem
            StackableLoot { template_uuid: "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel5",
        min_character_level: 45,
        stackables: &[
            // Transcendent Soul Gem
            StackableLoot { template_uuid: "d94bab85-53d5-4c9c-a637-acd94fc66c98", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel6",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel7",
        min_character_level: 1,
        stackables: &[
            // Potion of Minor Stamina
            StackableLoot { template_uuid: "42a22694-40eb-4c34-a267-c9aef2d96f56", quantity: 3 },
            // Iron Ingot
            StackableLoot { template_uuid: "55e82826-2d68-469c-8870-753665ca62cd", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel7",
        min_character_level: 5,
        stackables: &[
            // Potion of Light Stamina
            StackableLoot { template_uuid: "979f5025-b9b8-4b1e-b312-f96f294f97ae", quantity: 3 },
            // Steel Ingot
            StackableLoot { template_uuid: "b81952e0-c3c8-4a5c-92c0-8215d3eb71af", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel7",
        min_character_level: 10,
        stackables: &[
            // Silver Ingot
            StackableLoot { template_uuid: "b74a5c55-a687-4604-aa59-ba3ddfddcd2a", quantity: 10 },
            // Potion of Stamina
            StackableLoot { template_uuid: "fc29f4b5-3764-4241-aa1c-c623c4fbeff4", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel7",
        min_character_level: 15,
        stackables: &[
            // Potion of Strong Stamina
            StackableLoot { template_uuid: "45fbef4d-9244-456e-8fc0-f23f0750b3fa", quantity: 3 },
            // Orichalcum Ingot
            StackableLoot { template_uuid: "74f091b5-fd88-464b-a98a-f60a5e8a0f25", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel7",
        min_character_level: 20,
        stackables: &[
            // Potion of Major Stamina
            StackableLoot { template_uuid: "1e12dc39-9029-4cfa-a7f1-9eebfbe240cf", quantity: 3 },
            // Dwarven Metal Ingot
            StackableLoot { template_uuid: "f11fb90b-b441-4d72-a33f-50d14d3d6778", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel7",
        min_character_level: 25,
        stackables: &[
            // Potion of Plentiful Stamina
            StackableLoot { template_uuid: "34988d74-de88-43af-adea-445def4949be", quantity: 3 },
            // Quicksilver Ingot
            StackableLoot { template_uuid: "e80bee76-f92c-4005-9eff-20d1e8c64d24", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel7",
        min_character_level: 30,
        stackables: &[
            // Malachite Ingot
            StackableLoot { template_uuid: "85ed5500-3581-4699-8095-4b5ff6514355", quantity: 10 },
            // Potion of Vigorous Stamina
            StackableLoot { template_uuid: "af316825-826d-4fba-8554-a99d2fe291e9", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel7",
        min_character_level: 35,
        stackables: &[
            // Ebony Ingot
            StackableLoot { template_uuid: "75112030-b248-49b0-9c70-0da8dea150d1", quantity: 10 },
            // Potion of Intense Stamina
            StackableLoot { template_uuid: "a578cb1a-c6d9-4cf7-be42-be56e403b937", quantity: 3 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel7",
        min_character_level: 40,
        stackables: &[
            // Potion of Extreme Stamina
            StackableLoot { template_uuid: "52e2f139-fd26-4707-8a96-8e823de59a66", quantity: 3 },
            // Daedra Heart
            StackableLoot { template_uuid: "f9181a67-b094-4c37-a145-ced9dfe610d6", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel7",
        min_character_level: 45,
        stackables: &[
            // Potion of Ultimate Stamina
            StackableLoot { template_uuid: "8da5101c-2e7c-446b-b3bd-d1b9aa5c44f6", quantity: 3 },
            // Dragon Bones
            StackableLoot { template_uuid: "d523932f-8c7f-4192-9112-5dbd60883c2b", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel8",
        min_character_level: 1,
        stackables: &[],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel9",
        min_character_level: 1,
        stackables: &[
            // Pearl
            StackableLoot { template_uuid: "89ece62c-9ff6-470a-a152-d45ef4e0c222", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel9",
        min_character_level: 5,
        stackables: &[
            // Topaz
            StackableLoot { template_uuid: "3ca59113-a093-4cc8-8389-0f578ad94851", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel9",
        min_character_level: 10,
        stackables: &[
            // Garnet
            StackableLoot { template_uuid: "9df9233f-e7b9-47e8-bc75-f37707917759", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel9",
        min_character_level: 15,
        stackables: &[
            // Amethyst
            StackableLoot { template_uuid: "3ec6cf6f-d90e-4b76-bb7f-82da251ab5e5", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel9",
        min_character_level: 20,
        stackables: &[
            // Ruby
            StackableLoot { template_uuid: "cf4b1b42-a736-4aa1-99b8-baca9f9c2276", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel9",
        min_character_level: 25,
        stackables: &[
            // Sapphire
            StackableLoot { template_uuid: "014606cd-8898-4c1d-8029-63220a179c47", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel9",
        min_character_level: 30,
        stackables: &[
            // Emerald
            StackableLoot { template_uuid: "4d1231be-d3fa-4282-81b2-0f7bed4aabff", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel9",
        min_character_level: 35,
        stackables: &[
            // Diamond
            StackableLoot { template_uuid: "16e102fb-b1c0-42de-8106-0aa27e77f7f0", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel9",
        min_character_level: 40,
        stackables: &[
            // Atronite
            StackableLoot { template_uuid: "ab97efe1-bae9-4d16-8fd9-e05bad9ecb95", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena5_ArenaLevel9",
        min_character_level: 45,
        stackables: &[
            // Faerite
            StackableLoot { template_uuid: "70f2c013-813e-4f63-8b29-7a54a13b5e58", quantity: 10 },
        ],
    },
    PromotionLootBand {
        loot_table: "LootTable_Arena6_ArenaLevel1",
        min_character_level: 1,
        stackables: &[],
    },
];

/// Select the highest captured character-level band not above `level`.
/// Empty bands are returned as empty rather than falling back farther.
pub fn stackables_for(loot_table: &str, level: u16) -> &'static [StackableLoot] {
    PROMOTION_LOOT_BANDS
        .iter()
        .filter(|band| band.loot_table == loot_table && band.min_character_level <= level)
        .max_by_key(|band| band.min_character_level)
        .map(|band| band.stackables)
        .unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn level_86_arena_one_level_five_awards_transcendent_soul_gems() {
        let loot = stackables_for("LootTable_Arena1_ArenaLevel5", 86);
        assert_eq!(loot.len(), 1);
        assert_eq!(loot[0].template_uuid, "d94bab85-53d5-4c9c-a637-acd94fc66c98");
        assert_eq!(loot[0].quantity, 3);
    }

    #[test]
    fn level_bands_do_not_bleed_into_each_other() {
        let glorious = stackables_for("LootTable_Arena1_ArenaLevel5", 44);
        assert_eq!(glorious[0].template_uuid, "bafe6ed5-6473-4a4c-aef5-421d3af5c8cb");
        let transcendent = stackables_for("LootTable_Arena1_ArenaLevel5", 45);
        assert_eq!(transcendent[0].template_uuid, "d94bab85-53d5-4c9c-a637-acd94fc66c98");
    }

    #[test]
    fn every_generated_template_id_is_a_uuid() {
        for band in PROMOTION_LOOT_BANDS {
            for item in band.stackables {
                Uuid::parse_str(item.template_uuid).unwrap();
            }
        }
    }
}
