# Non-arena feature gaps — newblades server

The complete map of retail (non-arena) Blades features vs. our server, built from the
captured traffic (`api_captures` on prod / the retained snapshot), the IL2CPP dump
(`reference/il2cpp/dump.cs` in blades-capture), our docs, and bladesarena.com / UESP
for mechanic magnitudes. **Arena play is out of scope** (separate worktree).

This branch (`feature/non-arena-gaps`) turns the empty/404 stubs into real, stateful
features. Captures are the source of truth for wire shapes; where a number isn't in
captures (costs, XP curves, loot tables, prices) it's flagged and either derived
representatively or left lenient — never invented silently.

## Architecture

- **Economy core** — `blades_lib/src/economy/`: currency consts (Gold `f8d27767`,
  Sigil `c64bcb53`, Gems `470c8f58`), wallet debit/credit/`try_pay`, `RewardGrant` +
  `apply_reward`, stackable/item/chest grant+consume. Pure, no DB. Real debits, fail on
  insufficient funds.
- **Static data** — `blades_lib/src/static_data/` + `script/extract_static_data.py`
  derives JSON catalogs from captures into `deploy/static/*.json`, loaded at startup by
  `server/src/static_loader.rs` (each file optional; missing → empty, no crash). A
  loader unit test parses every committed file into the structs (catches shape drift).
- **Server state** — `characters.server_state` JSONB (migration `add_server_state`) +
  `blades_lib/src/server_state/`: per-character bookkeeping the captured character JSON
  doesn't model (gift claims, shop purchase counts, daily-reward period, challenge set).
  Never sent to the client.
- **Feature logic** — `blades_lib/src/features/*`: one pure, unit-tested module per
  feature. Server handlers stay thin: load → call pure fn → persist → serialize,
  following the existing `repair.rs` transaction pattern.

## Status by feature

| Feature | Endpoints | Status | Notes |
|---|---|---|---|
| **Economy primitives** | — | ✅ Done | Gold/Sigil/Gems debit+credit, reward grants, inventory/treasury mutation. 30+ unit tests. |
| **Global gifts** | `GET/POST /globalgifts`, `/globalgifts/{id}` | ✅ Done | Window + per-char claim limit; currency-template items credit the wallet. |
| **Announcements** | `GET /announcements` | ✅ Done | 156 capture-derived entries (assets are on Bethesda's dead CDN). |
| **Global store** | `GET /catalogoverrides/globalshop`, `GET /globalshops/current`, `POST /globalshops/current/purchase` | ✅ Done | Sigil/Gem sink. Price from client `expectedPrices` (sanity-checked); grant from 50 capture-derived products. Base price list lives in client bundles. |
| **IAP** | `GET /catalogoverrides/iap` | ✅ Done (placeholder) | Real-money SKUs served as priced placeholders, all inactive. No purchase flow, by design. |
| **Challenges** | `POST /challenges`, `/challenges/{id}`(+`/complete`,`/abandon`) | ✅ Done | 45 templates; rotating active set of 4; progress (client-absolute), reward + season points on complete. |
| **Level-up** | `POST /levelup` | ✅ Done* | +1 level, +1 STAMINA/MAGICKA. *No XP/cost gate (curve not captured) — trusts client. |
| **Abilities / respec** | `POST /abilities`, `/respec` | ✅ Done* | Learn/upgrade + reallocate points. *Cost not captured → lenient. |
| **Inventory upgrade/destroy** | `POST /inventories/current/upgrade`, `/destroy` | ✅ Done* | Capacity tier up; destroy items. *Upgrade gem cost not captured → lenient. |
| **Loadouts** | `POST /loadouts/current`, `/loadouts/profiles/{n}` | ✅ Done | Equip/unequip (backpack↔slot), set ability slots, save named profiles. |
| **Chests** | `POST /chests/{id}/collect` | ✅ Done* | Treasury model + collect grants loot, removes chest. *Loot is a representative capture-derived bundle (per-tier tables not captured). |
| **Daily login reward** | `POST /towns/current/rewards/current`(+`/collect`) | ✅ Done | 7-reward 24h rotation (stackables or a chest), once per period. |
| **Daily/Sigil quest events** | `POST /gameevents` | ✅ Done (advertise) | 32-event library; 2-3 active per day with current windows. **Playing** the event quest needs quest defs — see below. |
| **Guild** | create/view/search/leaderboard/join/leave/kick/chat | ✅ Done | New tables; full social CRUD + typed message board. Exchange ("gift") deferred — see below. |
| **Salvage** | `POST /salvages` | ✅ Done* | Remove gear → grant materials. *Representative yield (retail randomises). 122 recipes. |
| **Repair** | `POST /repairs` | ✅ Pre-existing | Restores durability; **no gold charge** (cost not captured). |
| **Town vendor shops** | `POST /shops/{id}`(+`/sell`,`/purchase`,`/buybacks/{id}`,`/auth/refreshloot`) | ⛔ Deferred | See plan below. |
| **Crafting / temper / enchant** | `GET/POST /crafts`, `/crafts/{id}/finish` | ⛔ Deferred | See plan below. |
| **Guild exchange ("gift")** | `GET/POST /guilds/current/exchanges`(+`/donate`,`/redeem`) | ⛔ Deferred | See plan below. |
| **Abyss** | `POST /abysses/current`(+`/start`,`/update`,`/end`) | ⛔ Deferred | See plan below. |
| **Town building** | `POST /towns/current/buildings`(+`/{id}/{upgrade,complete,destroy,styles/{id}}`,`/props`,`/name`) | ⛔ Deferred | See plan below. |

## Deferred features — captured shapes + implementation plan

### Town vendor shops
- **Open** `POST /shops/{shopId}` (req `null`) → `{shop:{id,catalogId,sales[],revenue[]}, catalog:{id,templateId,bundles:[{id,quantity}],wallet[],start,expiration,expired}}`.
- **Buy** `POST /shops/{shopId}/purchase` `{bundles:[{id,quantity}],gemsPayment}` → `{character, shop:{sales,revenue}, inventory, wallet}`. `revenue.balance` = price paid (positive).
- **Sell** `POST /shops/{shopId}/sell` `{items:[id], stackableItems:{}}` → `{shop, inventory:{backpack:{removedItems}}, wallet, buybacks:[{id,shopId,item|stackableItem,expiration,price}]}`.
- **Plan**: the catalog lists only bundle ids + remaining stock; **bundle price + contents are not in the catalog** — derive `bundleId → {price=revenue/qty, grant=inventory-diff}` from buy captures (extractor). Open serves a derived catalog. Sell credits a per-template sell price (approx; from `buyback.price`) and pushes a buyback. **Blocker**: shop ids are per-character building instances → a faithful catalog-per-shop needs the town/building model (below); a representative-catalog interim is possible.

### Crafting / tempering / enchanting
- **Create** `POST /crafts` `{recipeId,buildingId,temperingLevel,gemsPayment,batchSize}` → `{craft:{id,...,craftingTypeId,completedAt,results:{items|stackableItems}}, inventory, wallet}`.
- **Finish** `POST /crafts/{id}/finish` `{speedUp}` → `{character,reward,wallet,inventory}`. **List** `GET /crafts` → active jobs.
- **Plan**: derive `recipeId → {craftingTypeId, output}` (deterministic, ~5 recipes captured). Create a timed job in `server_state.craft_jobs` (consume materials if derivable — **input cost not captured**, lenient otherwise), `completedAt = now + duration`. Finish grants the output (gems = instant). Temper raises `item.temperingLevel`; enchant appends `properties.ENCHANTING` (enchant recipe ids in `blades_uid.json`). Low capture coverage → faithful for seen recipes, lenient costs.

### Guild exchange ("gift")
- `GET /guilds/current/exchanges` → `{guildExchanges:[{guildId,requesterUserId,requesterCharacterId,itemTemplateId,requestedAmount,maxDonationAmount,donations:[{donatorUserId,donatorCharacterId,donatedAmount}],creationTime,donationSum}]}`.
- `POST .../exchanges/donate` `{requesterUserId,requesterCharacterId,itemTemplateId}` → donor's `{wallet,inventory,character}` (debits the item).
- `POST .../exchanges/redeem` → `{inventory, guildExchangeRedeem:{reward:{stackableItems}}}` (requester gets the donated sum).
- **Plan**: a `guild_exchanges` table; donate debits a stackable from the donor + appends a donation; redeem credits the requester the donated sum. Cross-player item movement — fully captured, deferred only for the multi-player debit/credit care.

### Abyss (endless dungeon)
- `POST /abysses/current` (get/start), `/start`, `/update` (opaque gzip-b64 state like quest dungeons), `/end`.
- **Plan**: reuse the existing `dungeon`/`dungeon_update` FSM. `start` generates floors (scaling from dump.cs `AbyssScaling` + UESP for magnitudes); `update` ticks the FSM; `end` grants floor-scaled gold/XP/chests and bumps `maximumAbyssLevelReached`. The opaque b64 state is the main work (shared with quest dungeons).

### Town building
- `POST /towns/current/buildings` (place), `/{id}/upgrade`, `/{id}/complete`, `/{id}/destroy`, `/{id}/styles/{styleId}`, `/props`, `/name`.
- **Plan**: model the town JSONB (currently served verbatim as opaque `Value`) into a structured `Town` with buildings; place/upgrade cost gold + grant townXp, with a build timer (gems = instant). This is the main townXp sink (townXp is currently only echoed in rewards).

### Event-quest playing (quest definitions)
- `/gameevents` now advertises the event library and the quest accept→dungeon→complete flow exists, **but** `GameData` (`deploy/static/parsed.json`) is a 67-byte stub, so accepting/playing a quest with no definition fails.
- **Plan**: extend `extract_static_data.py` to derive `parsed.json` quest/dungeon definitions (objectives, rewards, spawn/loot) from `quests/{id}/accept` + `dungeons/current/exit` captures, so both event quests and regular quests become playable for non-imported characters. High value, unblocks the whole quest loop.

## Testing (no server/DB)

`cargo test --workspace` — pure-function + serde round-trip + golden-fixture + the
static-data loader test. No Postgres, no running server. Mirrors `repair.rs`'s
`#[cfg(test)]` pattern. Regenerate data: `python3 script/extract_static_data.py --db
<snapshot.db> --out deploy/static`.

## Deploy notes

- Two **idempotent** migrations add `characters.server_state` and the `guild*` tables.
  Per the fork's migrate-one-shot behaviour, **apply both by hand on prod** before the
  new binary (the migrate step skips once `users` exists). Both use `IF NOT EXISTS`.
- New `deploy/static/*.json` ship with the server (the `--static-data` dir).
- After deploy, hit each new route once and spot-check the wire vs. these captured shapes.
