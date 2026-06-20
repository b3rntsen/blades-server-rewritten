"""
build_parsed.py — Extract game data from Unity APK bundles into parsed.json

Output format matches:
  /Users/berntsen/Projects/blades-server-fill-the-gaps/script/data_parser/main.py
  /Users/berntsen/Projects/blades-server-fill-the-gaps/blades_lib/src/game_data/mod.rs

4 top-level keys: items_template, interactables, quests, dungeons
"""

import UnityPy
from UnityPy.helpers.TypeTreeGenerator import TypeTreeGenerator
import json
import sys

S = "/private/tmp/claude-501/-Users-berntsen-Projects-blades-capture/cd521a94-7b18-432b-bd3d-d2388570339f/scratchpad"
OUTPUT = "/Users/berntsen/Projects/blades-server-fill-the-gaps/deploy/static/parsed.json"

BUNDLES = S + "/apk-bundles/assets/Bundles"
SO      = S + "/apk-il2cpp/lib/arm64-v8a/libil2cpp.so"
META    = S + "/apk-il2cpp/assets/bin/Data/Managed/Metadata/global-metadata.dat"

# ── Setup ──────────────────────────────────────────────────────────────────────

print("Loading bundles...")
env = UnityPy.load(BUNDLES)
objs = list(env.objects)
uv = objs[0].assets_file.unity_version
print(f"unity version: {uv} | objects: {len(objs)}")

print("Loading TypeTreeGenerator with il2cpp metadata...")
g = TypeTreeGenerator(uv)
g.load_il2cpp(open(SO, "rb").read(), open(META, "rb").read())
env.typetree_generator = g
print("Ready.\n")

# ── Build a path_id → UnityPy object map for cross-file reference resolution ──

path_id_map = {o.path_id: o for o in objs}


def resolve_ref_obj(ref_dict):
    """Resolve a Unity {m_FileID, m_PathID} reference to a read_typetree() dict, or None."""
    pid = ref_dict.get("m_PathID")
    if pid and pid in path_id_map:
        try:
            return path_id_map[pid].read_typetree()
        except Exception as e:
            return None
    return None


# ── parse_reward_givers (mirrors parse_util.py) ────────────────────────────────

def parse_reward_givers(reward_givers):
    result = []
    for entry in reward_givers:
        reward = entry.get("_reward", {})
        items_to_reward = []
        inventory = reward.get("_inventory", {})
        for item_entry in inventory.get("_itemsList", []):
            item_uid = item_entry.get("Item", {}).get("_uid", {}).get("_id")
            if item_uid:
                items_to_reward.append({
                    "count": int(item_entry.get("_count", 0)),
                    "template_uuid": item_uid
                })
        chest_uid = reward.get("_chest", {}).get("_chestCycle", {}).get("_uid", {}).get("_id", "0")
        # normalize "0" UUID to nil
        if chest_uid == "0":
            chest_uid = "00000000-0000-0000-0000-000000000000"
        result.append({
            "experience": float(reward.get("_experience", 0)),
            "town_points": int(reward.get("_townPoints", 0)),
            "chest_is_none": chest_uid == "00000000-0000-0000-0000-000000000000",
            "items_to_reward": items_to_reward
        })
    return result


# ── parse_apparition_settings (mirrors parse_util.py) ─────────────────────────

def parse_apparition_settings(settings):
    result = []
    for entry in settings:
        interactable_uid = entry.get("_clutterType", {}).get("_uid", {}).get("_id")
        if not interactable_uid:
            continue
        weight_raw = entry.get("_weightMultiplier", 1)
        # weights come through as float (e.g. 3.0) — round to int
        weight = int(round(float(weight_raw)))
        result.append({
            "interactable_uuid": interactable_uid,
            "weight": weight,
            "mandatory": int(entry.get("_mandatory", 0))
        })
    return result


# ── resolve_refs (mirrors resolve_refs.py) ────────────────────────────────────

def resolve_refs(data):
    """Resolve Unity $ref/$id reference cycles in a parsed JSON dict."""
    ref_map = {}

    def _collect(obj):
        if isinstance(obj, dict):
            if "_id" in obj and obj["_id"] == "0":
                obj["_id"] = "00000000-0000-0000-0000-000000000000"
            if "$id" in obj and "_id" in obj:
                ref_map[obj["$id"]] = obj
            for v in obj.values():
                _collect(v)
        elif isinstance(obj, list):
            for v in obj:
                _collect(v)

    _collect(data)

    def _resolve(obj):
        if isinstance(obj, dict):
            if "$ref" in obj and "_id" not in obj:
                ref = obj["$ref"]
                if ref in ref_map:
                    return _resolve(ref_map[ref])
            return {k: _resolve(v) for k, v in obj.items()}
        if isinstance(obj, list):
            return [_resolve(v) for v in obj]
        return obj

    return _resolve(data)


# ═══════════════════════════════════════════════════════════════════════════════
# 1. items_template
#    Classes: ArmorTemplateList, WeaponTemplateList, ShieldTemplateList,
#             ItemTemplateList, ConsumableTemplateList, BookTemplateList,
#             EmoteTemplateList, QuestItemTemplateList
# ═══════════════════════════════════════════════════════════════════════════════

TEMPLATE_LIST_CLASSES = {
    "ArmorTemplateList", "WeaponTemplateList", "ShieldTemplateList",
    "ItemTemplateList", "ConsumableTemplateList", "BookTemplateList",
    "EmoteTemplateList", "QuestItemTemplateList",
}

print("Extracting items_template...")
items_template = {}
items_errors = 0

for o in objs:
    if o.type.name != "MonoBehaviour":
        continue
    try:
        obj = o.read()
        cn = obj.m_Script.read().m_ClassName
    except Exception:
        continue
    if cn not in TEMPLATE_LIST_CLASSES:
        continue
    try:
        tt = o.read_typetree()
        for item in tt.get("_templateList", []):
            uid = item.get("_uid", {}).get("_id")
            name_key = item.get("_name", {}).get("_key")
            item_type = item.get("_type")
            if uid and uid != "0" and uid != "00000000-0000-0000-0000-000000000000":
                items_template[uid] = {
                    "name": name_key or "",
                    "type": int(item_type) if item_type is not None else 0
                }
    except Exception as e:
        items_errors += 1

print(f"  items_template: {len(items_template)} items  (errors: {items_errors})")


# ═══════════════════════════════════════════════════════════════════════════════
# 2. interactables
#    Class: InteractableItemData (1 instance)
#    Each entry in _keyItemDataList has Key (uuid) + ItemData (cross-ref)
#    The referenced object has _lootTableList with _lootTableId._uid._id
# ═══════════════════════════════════════════════════════════════════════════════

print("Extracting interactables...")
interactables = {}
interactable_errors = 0

for o in objs:
    if o.type.name != "MonoBehaviour":
        continue
    try:
        obj = o.read()
        cn = obj.m_Script.read().m_ClassName
    except Exception:
        continue
    if cn != "InteractableItemData":
        continue
    try:
        tt = o.read_typetree()
        for entry in tt.get("_keyItemDataList", []):
            key = entry.get("Key")
            if not key:
                continue
            item_ref = entry.get("ItemData", {})
            ref_tt = resolve_ref_obj(item_ref)
            if ref_tt is None:
                interactable_errors += 1
                continue
            loot_table = {}
            for loot_entry in ref_tt.get("_lootTableList", []):
                loot_id = loot_entry.get("_lootTableId", {}).get("_uid", {}).get("_id")
                if loot_id and loot_id != "0" and loot_id != "00000000-0000-0000-0000-000000000000":
                    loot_table[loot_id] = {}
            interactables[key] = {"loot_table": loot_table}
    except Exception as e:
        interactable_errors += 1

print(f"  interactables: {len(interactables)} entries  (errors: {interactable_errors})")


# ═══════════════════════════════════════════════════════════════════════════════
# 3. quests
#    Class: DungeonQuestHolderScriptableObject (171 instances)
#    Each has _serializedJsonString → JSON with _dungeonQuest
#    Quest UUID = _dungeonQuest._uid._id
# ═══════════════════════════════════════════════════════════════════════════════

print("Extracting quests...")
quests = {}
quest_errors = 0
quest_no_dungeon_quest = 0

for o in objs:
    if o.type.name != "MonoBehaviour":
        continue
    try:
        obj = o.read()
        cn = obj.m_Script.read().m_ClassName
    except Exception:
        continue
    if cn != "DungeonQuestHolderScriptableObject":
        continue
    try:
        tt = o.read_typetree()
        raw_json = tt.get("_serializedJsonString", "")
        if not raw_json:
            quest_no_dungeon_quest += 1
            continue

        quest_data = resolve_refs(json.loads(raw_json))

        dungeon_info = None
        dq = quest_data.get("_dungeonQuest")
        if dq:
            # Quest UUID: _dungeonQuest._uid._id
            quest_uuid = dq.get("_uid", {}).get("_id")
            if not quest_uuid or quest_uuid == "0" or quest_uuid == "00000000-0000-0000-0000-000000000000":
                # Fallback to m_Name
                quest_uuid = tt.get("m_Name", f"unknown_{len(quests)}")

            dungeon_uuid = dq.get("DungeonSettingsPointer", {}).get("_uid", {}).get("_id")
            if dungeon_uuid == "0":
                dungeon_uuid = "00000000-0000-0000-0000-000000000000"

            objectives = {}
            for obj_entry in dq.get("_objectives", []):
                obj_uid = obj_entry.get("_uid", {}).get("_id")
                if not obj_uid or obj_uid == "0":
                    continue
                desc_key = obj_entry.get("Description", {}).get("_key", "")
                quota = float(obj_entry.get("_quota", 1.0))
                reward_givers_raw = obj_entry.get("RewardGivers", [])
                try:
                    rewards = parse_reward_givers(reward_givers_raw)
                except Exception as re:
                    rewards = []
                objectives[obj_uid] = {
                    "description": desc_key,
                    "quota": quota,
                    "rewards": rewards
                }

            version_raw = dq.get("_questVersion", 0)
            dungeon_info = {
                "objectives": objectives,
                "version": int(version_raw),
                "dungeon_uuid": dungeon_uuid or "00000000-0000-0000-0000-000000000000"
            }
        else:
            # No _dungeonQuest key → dungeon_info stays None
            quest_uuid = tt.get("m_Name", f"unknown_{len(quests)}")
            quest_no_dungeon_quest += 1

        quests[quest_uuid] = {"dungeon_info": dungeon_info}

    except Exception as e:
        quest_errors += 1
        # Uncomment for debugging:
        # print(f"  Quest error: {e}", file=sys.stderr)

print(f"  quests: {len(quests)} entries  (errors: {quest_errors}, no_dungeon_quest: {quest_no_dungeon_quest})")


# ═══════════════════════════════════════════════════════════════════════════════
# 4. dungeons
#    Class: DungeonSettingsScriptableObject (417 instances)
#    UUID: _settings._uid._id
#    handle: m_Name (no ResourceHandle in bundle objects)
#    spawn_info: from _settings._spawnSettings
# ═══════════════════════════════════════════════════════════════════════════════

print("Extracting dungeons...")
dungeons = {}
dungeon_errors = 0
dungeon_no_spawn = 0

for o in objs:
    if o.type.name != "MonoBehaviour":
        continue
    try:
        obj = o.read()
        cn = obj.m_Script.read().m_ClassName
    except Exception:
        continue
    if cn != "DungeonSettingsScriptableObject":
        continue
    try:
        tt = o.read_typetree()
        settings = tt.get("_settings", {})
        dungeon_uuid = settings.get("_uid", {}).get("_id")
        if not dungeon_uuid or dungeon_uuid == "0":
            dungeon_errors += 1
            continue

        handle = tt.get("m_Name", dungeon_uuid)
        spawn = settings.get("_spawnSettings", {})

        if not spawn:
            dungeon_no_spawn += 1
            # Emit with empty spawn info (still valid for Rust)
            dungeons[dungeon_uuid] = {
                "handle": handle,
                "spawn_info": {
                    "chest": {},
                    "item": {},
                    "enemy_spawn_groups": {}
                }
            }
            continue

        # Chests
        chest_spawn_info = {}
        for chest in spawn.get("_spawnGroupsChest", []):
            chest_uid = chest.get("_uid", {}).get("_id")
            if chest_uid and chest_uid != "0":
                chest_spawn_info[chest_uid] = {}

        # Items
        item_spawn_info = {}
        for item in spawn.get("_spawnGroupsItem", []):
            item_uid = item.get("_uid", {}).get("_id")
            if not item_uid or item_uid == "0":
                continue
            item_name = item.get("_name") or None  # "" → None for Option<String>
            if item_name == "":
                item_name = None
            try:
                app_settings = parse_apparition_settings(item.get("_apparitionSettings", []))
            except Exception:
                app_settings = []
            item_spawn_info[item_uid] = {
                "name": item_name,
                "apparition_settings": app_settings
            }

        # Enemies
        enemy_spawn_info = {}
        for enemy in spawn.get("_spawnGroupsEnemy", []):
            enemy_uid = enemy.get("_uid", {}).get("_id")
            if not enemy_uid or enemy_uid == "0":
                continue
            quantity = int(enemy.get("_quantity", 0))
            enemy_spawn_info[enemy_uid] = {"quantity": quantity}

        dungeons[dungeon_uuid] = {
            "handle": handle,
            "spawn_info": {
                "chest": chest_spawn_info,
                "item": item_spawn_info,
                "enemy_spawn_groups": enemy_spawn_info
            }
        }

    except Exception as e:
        dungeon_errors += 1
        # Uncomment for debugging:
        # print(f"  Dungeon error: {e}", file=sys.stderr)

print(f"  dungeons: {len(dungeons)} entries  (errors: {dungeon_errors}, no_spawn: {dungeon_no_spawn})")


# ═══════════════════════════════════════════════════════════════════════════════
# Assemble and write
# ═══════════════════════════════════════════════════════════════════════════════

result = {
    "items_template": items_template,
    "interactables": interactables,
    "quests": quests,
    "dungeons": dungeons,
}

print(f"\nWriting to {OUTPUT} ...")
with open(OUTPUT, "w") as f:
    json.dump(result, f, indent="\t")

print("Done.")
print(f"\nSummary:")
print(f"  items_template:  {len(items_template)}")
print(f"  interactables:   {len(interactables)}")
print(f"  quests:          {len(quests)}")
print(f"  dungeons:        {len(dungeons)}")

# Sample entries
print("\n--- Sample quest entry ---")
sample_quest = next(
    ((k, v) for k, v in quests.items() if v.get("dungeon_info") is not None),
    next(iter(quests.items()), None)
)
if sample_quest:
    print(f"  UUID: {sample_quest[0]}")
    di = sample_quest[1].get("dungeon_info")
    if di:
        print(f"  dungeon_uuid: {di['dungeon_uuid']}")
        print(f"  version: {di['version']}")
        print(f"  objectives count: {len(di['objectives'])}")
        if di["objectives"]:
            first_obj = next(iter(di["objectives"].items()))
            print(f"  first objective: {first_obj[0]} => {first_obj[1]}")

print("\n--- Sample dungeon entry ---")
sample_dungeon = next(
    ((k, v) for k, v in dungeons.items() if v["spawn_info"]["enemy_spawn_groups"]),
    next(iter(dungeons.items()), None)
)
if sample_dungeon:
    print(f"  UUID: {sample_dungeon[0]}")
    print(f"  handle: {sample_dungeon[1]['handle']}")
    si = sample_dungeon[1]["spawn_info"]
    print(f"  chests: {len(si['chest'])}, items: {len(si['item'])}, enemies: {len(si['enemy_spawn_groups'])}")
    if si["enemy_spawn_groups"]:
        eg = next(iter(si["enemy_spawn_groups"].items()))
        print(f"  first enemy group: {eg[0]} => {eg[1]}")
