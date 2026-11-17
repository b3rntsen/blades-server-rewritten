# Building cut content: draugr, elemental spiders, new dungeons (#197, #198, #355)

For Mɾʂιɾι. Written 2026-11-17 against `b3rntsen/blades-server-rewritten` main
(`602006b7`), `dethele-com/newblades-project` main, the 1.31 APK
(`reference/apk/blades.apk`, Unity **2019.4.37f1**) and the live bundle mirror.

Each claim below is labelled **[checked]** (measured for this guide or earlier
and re-checked), **[doc]** (from an earlier write-up I did not re-run), or
**[unverified]** (nobody has seen it work on a device yet). Please treat the
unverified ones as experiments, not instructions.

---

## 0. The model you need first

Four facts decide everything that follows.

1. **The client decides which enemy stands in a dungeon. The server only decides
   level, XP and loot.** [checked] A dungeon is a `DungeonSettingsScriptableObject`
   (dump.cs TypeDefIndex 10546) in the built-in bundle `dungeonsettings`. Each
   enemy spawn group is a `SpawnGroupEnemy` (10623) carrying
   `EnemyDataPointer` → an `EnemyData` (12674) family. The server answers
   `/quests/<id>/accept` with `enemyGeneratedData`, keyed by spawn-group id, and
   each entry has only `enemyLevel, givenXP, lootTableLoot, spawnGroupLoot`
   (`EnemyGeneratedData`, 10655; fork `blades_lib/src/util/dungeon.rs`
   `generate_for_dungeon_inner`). There is no enemy-type field on the wire.

2. **A dungeon is assembled from rooms at runtime; there is no "level scene".**
   [checked] `DungeonSettings` names a **kit** (`_kit`), **sections**, **room
   constraints** and optionally a `DungeonSettingsFixedLayout` (rooms + which
   joint links to which). Each room is:
   - a `RoomInfo` entry in `RoomsData`, which lives in the **built-in**
     `gameplaymetadata` bundle (300 rooms: name, uid, kit, grid size, joints with
     grid position and direction, spawners with uid/type/position/rotation);
   - a prefab bundle `levels_<kit>dongeonlevel_<room>` (e.g.
     `levels_forestdongeonlevel_mediumroom_07`, 142,948 bytes) whose single prefab
     `assets/export/resources/<bundle>/<bundle>_room.prefab` holds 3 `RoomJoint`,
     29 `Spawner`, 34 `RoomPiece`, 66 `MeshCollider`, 21 `NavMeshModifierVolume`,
     a `NavigationDataHolder`, `LightmapLoader`s and a `UidAssetRoom`. It has only 3
     meshes of its own: the geometry is **kit pieces referenced from its
     dependencies** (`kit_forest_sab`, `clutter_camp_sab`, `clutter_common_sab`,
     `clutter_rocks_sab`, `duplicates_sab`, `landscape_sab`, `skybox_sab`, `vfx_sab`).
   - Room bundle counts by kit: forest 71, stone 68, ayleid 65, cave 63, plus 15
     transitions. `kit_nordic_sab` exists but is a cut kit: **5 textures**
     (`Candles01`, `RidgedStone01`, `RidgedStoneFloor01` alb/nrm), no rooms.
   - No baked `NavMeshData` is in the room bundle; the client has
     `NavMeshGenerator` / `RunTimeNavMeshSettings` (10428/10429), so the navmesh is
     most likely built at load from colliders and modifier volumes. **[unverified]**

3. **Quest objectives are client data.** [checked] A quest is a
   `DungeonQuestHolderScriptableObject` (11063) in the built-in `quests_sab`; its
   `_serializedJsonString` holds the whole quest including objectives. The server
   only echoes objective ids from `deploy/static/parsed.json` and stores the
   progress the client reports (no quota check for quests in the fork).

4. **New client data ships as an asset pack, and packs can only REPLACE existing
   bundles.** [checked] `tools/bundle-reskin/build_pack_v2.py` repoints an existing
   bundle name to new bytes (manifest + `_ExtraData`). There is no tooling for a
   brand-new bundle name. Built-in bundles (`dungeonsettings`, `quests_sab`,
   `gameplaymetadata`, `charactercompound_sab`) can be overridden this way:
   the client prefers the download once the manifest hash differs
   (`docs/halloween-builtin.md`) [doc]. A pack must be built from the
   **mirror** copy of a mirror bundle, never the APK's `assets/Bundles/*`
   (stale snapshot), except for the built-ins, which only exist in the APK.

---

## A. Draugr and elemental spiders in a dungeon

### A1. What the client already has

| | Draugr | Elemental spider |
|---|---|---|
| `EnemyData` family | **yes** [checked]: `Draugr Family` `67d9e5a6-191c-4c8f-863e-4c608d7cf3d3` (`enemies_sab`, mirror copy), name `Enemy.Name.Draugr`, gold loot table `871c2e9b…`, XP table `817cc363…`, one band levels 1–50. Also `Draugr McGuffin Family` `74ca3472…` | **no** [checked]: the mirror `enemies_sab` (252,861 bytes) holds 48 spider/draugr-named objects and none is elemental |
| `EnemyVariant` (stats, AI, abilities) | `DraugrWarrior_Basic` `711b754e-561b-4a92-b320-33f9b54604f0`: health 50 + 5/level, damage 5 + 1/level, block 17, one known ability `ce6b63e9…` (Power Attack per the doc), full AI parameters [checked]. Prototype-grade numbers | none |
| `EnemyGroup` (for kill objectives) | `Draugr` `55afebe1-49bc-44de-b26e-5d0167426d54` [checked] | `Spiders` `375208a3…` covers ordinary spiders only |
| Compound (model link) | `DraugrDefaultCompound` `c614da13…` in `charactercompound_sab` [checked] | `SpiderElemental{Fire,Frost,Shock,Poison}Compound` exist in `charactercompound_sab` [checked]; visual-category weight **0.0** [doc] |
| Art on the mirror | `visual_enemies_maledraugrvisual`, `character_draugr_sab` (16 MB, rig + clips), `weapons_draugr_sab`, `weapons_ancientnordicweapons`, `wwise_sb_draugr`: all present [checked 2026-11-17] | `character_spider_elemental_sab` (3.6 MB) and all four `visual_enemies_spiderelemental*visual`: present [checked] |
| Spawned anywhere | only Bethesda's dev gym `Gym01_DungeonSettings_archetypes` (no quest) [checked] | never |

So the draugr needs **no new enemy authoring**, only spawn groups. The elemental
spider needs an enemy definition.

How elemental damage is expressed [checked]: `EnemyVariant._stats._baseDamageTypes`
is a list of `{_type, _percentOfTotal}` with `DamageType` (dump.cs 13141)
`Fire=4, Frost=5, Shock=6, Poison=7`. `SpiderCave_Basic`
(`16f2fae8-3f20-48b5-992b-bdca7ebbcbbc`) is already `Cleaving 0.35 + Poison 0.65`,
which makes a "poison elemental" spider nearly free. `Dragon_2` uses
`Cleaving 0.75 + Fire 0.25`, so mixed physical/elemental is a retail shape.

### A2. Draugr: the client edits

All of these are `dungeonsettings` edits. `tools/bundle-reskin/newcontent.py`
already does the first one with stock-value guards and readback; the second is a
small extension of it.

1. **Swap (already built):** `newcontent/nb-draugr-proof.json` repoints EQ15's
   first "Skeletons" group `b98ac1b1…` from `EQ15_Skeleton_Family` (`95d51870…`)
   to `Draugr Family`. Offline proof: exactly one changed leaf. **It has never been
   on a device** [checked: no such pack is registered on prod]. This is the first
   thing to run (milestone C1).
2. **Add groups to the dungeon you own.** Copy a section-targeted group from a
   dungeon on the same kit and change four things. The template I suggest is
   `EQ02_Forest_DungeonSettings` group `c1c5b925…` (forest kit, `Spawner` uid `0`
   = placed procedurally on any Enemy-type spawner in the section,
   `_targetSectionIndex` 1, `_quantity` 4, `_countPerSpawnerMin/Max` 1/1):
   - `_uid._id` → a fresh UUID (also used on the server, see A4);
   - `EnemyDataPointer._uid._id` → `67d9e5a6-191c-4c8f-863e-4c608d7cf3d3`;
   - `_targetSectionIndex` → `0` (EQ18 has exactly one section, see B3);
   - `Spawner._uidDungeon._id` → the target dungeon's uid.
   Appending to the `_spawnGroupsEnemy` list is a typetree edit
   (`o.save_typetree(t)`), not a new object, so no path-id work is needed.
   `_levelDeltaSequence` (e.g. `[4]` for a mini-boss) shifts that spawner's level.
3. **Equip them with Ancient Nordic weapons (#197 follow-up).** `SpawnGroupEnemy`
   has an `_equipment._weapon.Item` pointer. Point it at an Ancient Nordic weapon
   template uid. **[unverified]** whether a draugr's animator accepts any weapon
   class; the art ships `weapons_draugr_sab` with its own swords, so test one.
4. **Level band.** `Draugr Family` stops at 50. The client calls
   `EnemyData.GetVariantAndLevel(targetLevel, canExceedMaxLevel, out effectiveLevel)`,
   so high players should meet a capped draugr rather than a failed spawn
   **[unverified]**. Widening is `newcontent.py`'s `enemyBands` edit, but
   `enemies_sab`'s `_ExtraData` record (`0xd88a1`, 252,876) differs from its served
   size (252,861), so the pack builder cannot pin it yet [doc]. Leave it for v2.

### A3. Elemental spiders: authoring the enemy

Two ways; I recommend the first.

- **Repurpose an unused spider pair (two field edits, no new objects).** Pick an
  `EnemyData` + `EnemyVariant` that no dungeon, abyss slice or job references
  (we can compute the list for you; there are 151 families and 632 variants).
  On the variant, set `_compound.m_PathID` to the elemental compound
  (same external file as the ordinary spider's compound, `m_FileID` 1:
  `SpiderElementalFireCompound` -4159744213676899863,
  `…FrostCompound` -7866889347834672155, `…ShockCompound` -8936200137922647999,
  `…PoisonCompound` -4945786741784671575) and rewrite `_baseDamageTypes` to the
  element. Rename `_enemyName` to a `Newblades:`-safe loc key if you want a label.
- **Clone new objects** (`SpiderCave_Basic` + `Spider Cave Monster Family`
  `0b6028b5…`). Adding objects means new path ids, and very likely an entry in the
  enemy library that `GameDataLibrary` (in `common`) points at. That is untried
  here; avoid it unless the first way fails.

Open risks, all **[unverified]**: the 0.0 visual-category weight on the elemental
compounds (does the client still resolve a compound that is the only candidate?);
whether `override_controller_critterspider_elemental` drives the ordinary spider
AI without errors. Test one fire spider in one room before anything else.

`enemies_sab` cannot be pinned by the pack builder yet (record/served-size mismatch
above), so either fix `extradata.locate` for it or pin it by hand the way
`dungeonsettings` (`#0xced83`) and `localization` (`#0x115f2d`) are pinned in
`docs/newblades-content-system.md` §5. We can do that part for you.

### A4. The server side (fork)

| file | what to add | why |
|---|---|---|
| `deploy/static/newblades_content.json` | an entry `nb-…` with `questIds`, `dungeonIds`, `enemyDataIds`, `loc` (every text starts `Newblades:`), and `extraSpawnGroups: {<dungeon uid>: {<new group uid>: <quantity>}}` | `blades_lib/src/game_data/newblades_content.rs::apply` merges those groups into `GameData` at start so `enemyGeneratedData` contains them. Its `validate` refuses a missing prefix, an id without `nb-`, a quest/dungeon the client does not have, or a group id that is already retail |
| `blades_lib/src/spawn_group_counts.json` | `{min, max, levelDeltaSequence}` for each new group, if `_countPerSpawnerMax` > 1 or you use a level delta | `enemies_per_spawner()` returns 1 for an unknown group; the client spawns exactly as many as the inner list holds (#301). Compiled in |
| `blades_lib/src/enemy_group_tables.json` | optional: group → loot-table distribution | a group it does not list rolls the level-scaled gold table only. Draugr dropping Ancient Nordic gear is a loot-table choice: owner decision |
| nothing | for a pure swap like the EQ15 proof | the group id is unchanged, so the server already sends its data |

No change to `parsed.json`: it stays pure APK output by design.

Run `TEST_DATABASE_URL=… cargo test --workspace`; the test
`the_committed_registry_is_valid_against_the_client_data` validates your registry
entry against `parsed.json`.

### A5. Testing on an emulator with a local server

The bring-up for an emulator is scripted in the capture repo:
`tools/emulator/setup-newblades-emulator.sh` (`build | snapshot | restore | launch`).
The three traps: Vulkan off (`-feature -Vulkan`) on Apple Silicon, a
**debug-signed** APK, and the capture CA in the **user** trust store.

Your local server already serves the mirror: the fork's `server/src/main.rs`
mounts `<--static-data>/bundles.blades.bgs.services/` as static files. So for a
local test [unverified on your setup, but these are the same files prod serves]:

1. Build the patched bundles (A2/A3) and the pack:
   `APK_EXTRACT=<unzipped apk> python tools/bundle-reskin/newcontent.py newcontent/<id>.json --dungeonsettings <apk>/assets/Bundles/dungeonsettings --localization <mirror>/localization --out out-nc`
   then `tools/bundle-reskin/build_pack_v2.py <id> "<name>" dungeonsettings=out-nc/dungeonsettings#0xced83 localization=out-nc/localization#0x115f2d`
   (exact lines in `docs/newblades-content-system.md` §5). It needs a venv with
   `UnityPy` + `TypeTreeGeneratorAPI` and the APK's `libil2cpp.so` +
   `global-metadata.dat` (memory note: load an APK bundle first; loading a mirror
   bundle first made the il2cpp loader fail for me today).
2. Copy the pack's bundles into your local mirror under
   `Android/Bundles/<first two hex>/<hash>`, and its manifest and `_ExtraData`
   variants over `Android/Manifests/1.31.0.4306485.26246488` and `…_ExtraData`
   (back up the originals).
3. The client caches `_ExtraData` by **version string** and never refetches it
   (`docs/custom-assets-delivery.md`). Either bump the version your MetaManifest
   advertises (prod's `scripts/bundle-mirror.py::_pack_bundle_version` does this,
   and it must only ever go **up**: a lower version hangs every client on the
   title screen), or delete the cached `ManifestExtraData/<version>_ExtraData` in
   the app's files directory on the emulator.
4. Success signal is the server log, not the screen: `MetaManifest.json`, then
   the manifest and `_ExtraData`, then your new bundle hashes, within a minute.
   MetaManifest followed by silence means the client rejected the manifest.
5. Accept the quest, walk into the room, and look. For the EQ15 proof: the event
   must be open, the list should read "Newblades: A Battle Unceasing", and the
   first room should hold two draugr.

If you would rather not run a mirror: send us the pack directory (or the
`newcontent/<id>.json`), we upload it on `/dashboard/asset-packs`, put it on the
**Beta** channel and tick your WireGuard peer. Beta is per-peer, so it only works
with the tunnel up.

---

## B. A new map or dungeon

### B1. How the client finds a dungeon

[checked] There is no server-side list of dungeons. The quest JSON in `quests_sab`
points at a `DungeonSettings` uid (`DungeonSettingsPointer`), and that uid must
exist in `dungeonsettings`. `parsed.json` (`quests`, `dungeons`) is the server's
copy of those two built-in bundles, extracted by `script/extract_parsed_json.py`.
An event quest is offered by the server through `deploy/static/game_events.json`
(calendar) + `deploy/static/event_quests.json` (objective ids and rewards per
completion tier).

So a "new dungeon" needs, in increasing cost:

| route | client | server |
|---|---|---|
| **Reuse an unreleased slot** (recommended) | edit `EQ18_DungeonSettings` / `EQ18_Quest` in place | calendar + template + registry entry |
| New quest and dungeon objects | new objects in `quests_sab` and `dungeonsettings` (new path ids, `_ExtraData` asset-path lists) | same, plus a regenerated `parsed.json` entry |

### B2. The free slot: EQ18

[checked] `EQ18_Quest` `1a38ac88-2b00-47a4-a626-cb2e8af2ab1f` is the only event
whose title is a placeholder (`[PL] EQ18 Title`). It is an event quest (`_type` 2,
`_questDifficulty._targetLevel` 10, `_difficultyOffsets` [-6,-3,0,3,7],
level-based rewards Lumber + Sigils (Medium), final reward Smithing 1).
Its dungeon `EQ18_DungeonSettings` `57da57d2-4ca4-43a1-ac87-26801fe602d0`:
forest kit, **one section of 5 rooms**, a fixed layout, one player spawn group,
**zero** enemy, chest or item groups. Its rooms, with how many Enemy-type
spawners each `RoomInfo` declares:

| room | random-selectable elsewhere | enemy spawners |
|---|---|---|
| `Levels_ForestDongeonLevel_SpecialEntranceRoom_05_Room` | no | 3 |
| `Levels_ForestDongeonLevel_LargeRoom_Village_transition_01_Room` | no | 0 |
| `Levels_ForestDongeonLevel_SmallRoom_02_Room` | no | 7 |
| `Levels_ForestDongeonLevel_SmallRoom_08_Room` | **yes** | 3 |
| `Levels_ForestDongeonLevel_LargeRoom_Village_02_entrance_Room` | no | 47 |
| `Levels_ForestDongeonLevel_LargeRoom_Village_blocker_Room` | no | 3 |
| `Levels_ForestDongeonLevel_Hall_08_Room` | **yes** | 3 |

(Room constraints, so some repeat.) Plenty of enemy spawners, so section-0 groups
from A2 have somewhere to stand.

Its one objective `b7be0ecc-74f2-46fe-b382-b1fb19be7767` is a
`DiscoverAreaObjective` on room constraint `7e48e355…` (room #0, the entrance), so
**as shipped it would complete the moment you walk in**. Change it (B4).

This covers the draugr dungeon ("Newblades: Clear off the undead"). The spider
dungeon needs a second slot; there is no second placeholder event. Options: a
quest template retail never released (we would have to find one), or route B's
new objects. That is an open decision; draugr first.

### B3. "Kill 3 trolls": the objective

[checked] Retail's own template is `GH02_TrollingTheThalmor`
(`45c042e1-538b-4079-8b92-06f1d7677b6f`), objective `bf64333d…`:

```
"$type": "BGS.Game.Objectives.KillObjective", "_quota": 3.0,
"EnemyDataPointer": { "_uid": { "_id": "9e5bdf01-fae0-430a-b177-c269127ee04f",
                      "_description": "Trolls", "_type": "BGS.Game.Data.EnemyGroup" } },
"EnemySpawner": { … "_description": "EnemySpawn_01", "_type": "BGS.Game.Level.Spawner",
                  "_uidParent": { … "GH02_TrollCamp" RoomConstraint } },
"_enemySpawnGroupPointer": { "_uid": { "_id": "0" } … }
```

A second shape targets a spawn group instead (EQ15's boss objective sets
`_enemySpawnGroupPointer` to group `927d54b5…` and leaves the others at `0`).

For EQ18, rewrite objective `b7be0ecc…` **keeping its uid** as a `KillObjective`:
`_quota` 3, `EnemyDataPointer` → EnemyGroup `Draugr` `55afebe1-…`, `EnemySpawner`
uid `0` (any spawner). Keeping the uid means the server needs no new objective id.
Mind the Json.NET `$id`/`$ref` graph inside `_serializedJsonString`: EQ18's
dungeon is `$id "3"` and the quest is `$id "27"`; copy GH02's objective shape but
re-point its `$ref`s to those. Set `Description._key` to a key you add through
the `localization` pack (`newcontent.py`'s `loc` edit appends missing keys).
Whether a `KillObjective` counts by EnemyGroup when the spawner is `0`:
**[unverified]**; the spawn-group form is the fallback.

`quests_sab` is a built-in bundle like `dungeonsettings`; its `_ExtraData` record
has not been located yet. Expect to pin it as for `dungeonsettings`.

### B4. Server data for EQ18 (fork `deploy/static/`)

1. `event_quests.json` → `templates["1a38ac88-…"]`: `gldQuestId`, `version` 1
   (from `parsed.json`), `objectiveIds: ["b7be0ecc-…"]`, five `rewards` tiers
   (one per completion), `finalReward`, and `_meta.authored: true`. The test
   `authored_templates_are_labelled_as_such` in `server/src/event_quests.rs` holds
   you to the label. Copy reward sizes from a comparable retail event; the amounts
   are an owner call.
2. `game_events.json`: a calendar entry `{eventId (new uuid), questId, important,
   instanceDurationSecs, recurrence{…}, _meta{name, authored: true, note}}`. See the
   EQ28 entry for an authored example.
3. `newblades_content.json`: the A4 entry with `questIds: [EQ18]`,
   `dungeonIds: [EQ18 dungeon]`, `loc: {"EQ18_Title": "Newblades: Clear off the undead"}`.

**These three are `deploy/static` data: merging the PR does not ship them.**
`deploy/static` is a bind mount on the box; it ships with `deploy/arena.sh static`
(never `--delete`). We run that after the merge.

### B5. New geometry: three tiers

**Tier 1, no Unity (do this first): rearrange an existing room.** A room prefab is
mostly Transforms pointing at kit meshes in other bundles, so moving, rotating,
hiding or re-texturing pieces is a UnityPy typetree edit of the room bundle, the
same machinery the Halloween packs used. Rules:
- Keep every `RoomJoint` and `Spawner` object, uid and position. `RoomsData` in
  `gameplaymetadata` duplicates their positions and the room's grid size, and the
  layout generator works from that copy.
- Edit a room that is **not** random-selectable (`_allowRandomSelection` 0, 100 of
  the 300), or your change appears in every forest dungeon that rolls it. In EQ18,
  `SmallRoom_02`, `SpecialEntranceRoom_05` and the Village rooms are safe;
  `SmallRoom_08` and `Hall_08` are not. Check other fixed layouts too; we can list
  every dungeon that uses a room.
- Baked lightmaps (`LightmapLoader.m_lightmap`) will not match moved geometry.
  Setting `LightMapEnabled` false on moved pieces is the obvious first try.
  **[unverified]**
- Colliders move with their GameObject. If the navmesh is runtime-built
  (see §0.2), enemies will path over the new shape. **[unverified]**
- Duplicating pieces (new GameObjects) needs new path ids. Possible with UnityPy,
  untried here.

**Tier 2, Unity editor: rebuild a room bundle under an existing name.**
1. Install **Unity 2019.4.37f1** (every bundle header carries it), Android module.
2. AssetRipper the room bundle plus its dependencies. Scripts come out as stubs:
   the game's classes are in `Assembly-CSharp.dll` (namespaces `BGS.Game.Level`
   for `Spawner`/`RoomJoint`, `BGS.Game.Uids` for `UidAssetRoom`, `BGS.Game.LevelGeneration`).
   The stubs must keep assembly, namespace, class and serialized field names
   identical, or the built prefab's MonoScript references will not bind to the
   game's code.
3. Build the prefab at the same container path
   (`assets/export/resources/<bundle>/<bundle>_room.prefab`), same AssetBundle
   name, Android target, LZ4 (chunk-based) compression, and the **same dependency
   bundle names** (your meshes must come from bundles the client already has, or
   be inside this one).
4. Keep the joints, spawners and grid size identical to that room's `RoomInfo`,
   or also patch `RoomsData` in `gameplaymetadata` (built-in; pack it like
   `dungeonsettings`).
5. Ship it as a replacement for that one room bundle through build_pack_v2.

This is the realistic route to "my own map", but **nobody has loaded a
Unity-built room into this client yet**. The usual failure is a prefab that loads
with missing-script components; your first test should be an unchanged
round-trip (export → rebuild → load) before you change any geometry.

**Tier 3, a new kit or new room names (e.g. a real Nordic barrow kit).** Needs new
bundle names in the manifest (no tooling: `build_pack_v2` only repoints), new
`RoomInfo` entries, new `KitScriptableObject` in `kits_sab`. Not recommended
before Tier 2 works.

### B6. Shipping through the website (Beta → production)

From `docs/asset-packs-operating.md` and `docs/asset-pack-layers.md`:

1. Build the pack directory (`build_pack_v2.py` output: bundles by hash +
   manifest variant + `_ExtraData` variant + `pack.json`).
2. `/dashboard/asset-packs` → **Add a pack** (admin-only; we do this for you).
   It lands **retired**.
3. Put it on **Beta**. Beta devices are WireGuard peers ticked on
   `/dashboard/devices`, so testers need the tunnel up.
4. Packs are layers composed by `scripts/asset_pack_compose.py`; two layers may not
   override the same bundle. Production currently overrides `common`,
   `duplicates_sab` and `town_alcove_alcove_castle_00` (pack `d43-stock-r2`), so
   avoid `common` or build on top of that `common`.
5. The owner moves it to production. Per #197, new content goes to Beta first.
   Pausing a pack once clients hold its version is not a safe rollback (they hang);
   roll forward instead.

---

## C. A weekend milestone, and what we do when you open the PR

### C1. First evening: see one draugr on your screen

Build `newcontent/nb-draugr-proof.json` exactly as in §A5 and load it locally.
EQ15 must be open on your server (add a `game_events.json` window if it is not).
Pass: "Newblades: A Battle Unceasing" in the list, two draugr in the first room,
they fight, and they die. This answers the open questions about the draugr
(spawn, animator, level band, death) with no new code. Record a short video;
it is the first draugr anyone has fought.

### C2. The weekend: "Newblades: Clear off the undead" on EQ18

1. Extend `newcontent.py` with an `enemyGroupsAdd` edit (A2.2) and a quest
   objective edit for `quests_sab` (B3). Keep the existing style: every edit
   names the stock value it replaces, and every output is read back.
2. New `newcontent/nb-draugr-barrow.json`: 3–4 draugr groups in section 0 of
   `EQ18_DungeonSettings`, objective `b7be0ecc…` → KillObjective quota 3 on EnemyGroup
   Draugr, loc `EQ18_Title` = "Newblades: Clear off the undead" + an objective key.
3. Fork PR: the registry entry, `spawn_group_counts.json` rows, the
   `event_quests.json` template and a `game_events.json` window (B4).
4. Test locally (A5). Pass: quest appears with the Newblades title, killing three
   draugr completes it, rewards pay.

Open two PRs: the `newcontent/` definition and tool change on
`dethele-com/newblades-project`, the server data on `b3rntsen/blades-server-rewritten`.
Put the pack directory (or a link) in the newblades PR.

### C3. What we do on your PR

- Review; run `cargo test --workspace` and the registry validation; add whatever
  server pieces you skipped (spawn counts, loot mapping, calendar).
- Pin the `_ExtraData` records the builder refuses (`quests_sab`, `enemies_sab`),
  compose the pack with the current layers, upload it, put it on **Beta** and tick
  your peer and the testers'.
- After the fork merge, run `deploy/arena.sh static` for the `deploy/static`
  files, then check the arena log for the registry line and the mirror log for the
  pack's bundle downloads.
- Ask the owner for: reward sizes, draugr loot (Ancient Nordic drops), and when it
  leaves Beta. The elemental-spider dungeon (A3) is the follow-up once the draugr
  one works.

### Known unknowns, in the order they are likely to bite

1. Draugr have never spawned on a device in this project (C1 settles it).
2. `_ExtraData` records for `quests_sab` and `enemies_sab` are not pinned.
3. KillObjective by EnemyGroup with spawner `0` (B3).
4. Elemental compounds with visual-category weight 0.0 (A3).
5. Runtime navmesh and lightmaps after geometry edits (B5).
6. A Unity-built room prefab binding to the game's scripts (B5 Tier 2).
7. A second free quest slot for the spider dungeon (B2).
