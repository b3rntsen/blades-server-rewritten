use std::sync::Arc;

use actix_web::{
    get,
    web::{self, Json},
};
use blades_lib::user_data::{Backpack, CompleteInventory, Loadout, Treasury};
use diesel::{ExpressionMethods, QueryDsl, SelectableHelper};
use diesel_async::RunQueryDsl;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    BladeApiError, ServerGlobal, models::CharacterDbEntryInventory, session::SessionLookedUpMaybe,
    util::get_only_single_character_and_check_permission,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct InventoryQuery {
    // Defaulted: these params are optional on the wire (the client omits them for a full
    // fetch). Absent → false → the full inventory, as before.
    #[serde(default)]
    consumable_stackable_items_only: bool,
    #[serde(default)]
    equipped_items_only: bool,
}

#[derive(Serialize, Deserialize)]
struct GetInventoryResponse {
    inventory: CompleteInventory,
}

/// Apply the `consumableStackableItemsOnly` / `equippedItemsOnly` query filters to a
/// full inventory, returning the requested subset. Pure so it can be unit-tested
/// without a DB. When both are false this returns the inventory unchanged (the common
/// full-fetch path); otherwise it keeps only the requested slices and blanks the rest
/// (versions are preserved so the client's optimistic-concurrency check still passes).
///
/// This REPLACES a `debug_assert`-style `assert!(both == false)` that PANICKED the
/// handler — and dropped the connection ("Unable to connect") — if the client ever sent
/// either param `=true`. A request param must never crash a handler.
fn apply_inventory_filters(
    inv: CompleteInventory,
    consumable_stackable_items_only: bool,
    equipped_items_only: bool,
) -> CompleteInventory {
    // No filter requested → full inventory (unchanged behaviour).
    if !consumable_stackable_items_only && !equipped_items_only {
        return inv;
    }
    // Additive: decide which slices to KEEP from the flags, blank the rest. This composes
    // when BOTH flags are set (keep the loadout AND the consumable stackables) — a
    // destructive "clear everything but X" order would drop one slice the other wanted.
    // Treasuries are always dropped under either filter (neither param asks for them).
    let mut out = CompleteInventory {
        backpack: Backpack::default(),
        loadout: Loadout::default(),
        treasury: Treasury::default(),
        overflow_treasury: Treasury::default(),
        backpack_version: inv.backpack_version, // versions preserved for optimistic concurrency
        treasury_version: inv.treasury_version,
    };
    // `equippedItemsOnly`: keep the loadout (equipped gear + consumables).
    if equipped_items_only {
        out.loadout = inv.loadout;
    }
    // `consumableStackableItemsOnly`: keep the backpack's stackable items (consumables).
    if consumable_stackable_items_only {
        out.backpack.stackable_items = inv.backpack.stackable_items;
    }
    out
}

#[get("/api/game/v1/public/character/{character_id}/inventories/current")]
pub async fn get_inventory(
    session: SessionLookedUpMaybe,
    app_state: web::Data<Arc<ServerGlobal>>,
    path: web::Path<Uuid>,
    query: web::Query<InventoryQuery>,
) -> Result<Json<GetInventoryResponse>, BladeApiError> {
    let session = session.get_session_or_error()?;
    let character_id = path.into_inner();
    let mut conn = app_state.db_pool.get().await.unwrap();

    let inventory_result = {
        use crate::schema::characters::dsl::*;
        characters
            .filter(id.eq(character_id))
            .select(CharacterDbEntryInventory::as_select())
            .load(&mut conn)
            .await
            .unwrap()
    };

    let inventory =
        get_only_single_character_and_check_permission(inventory_result, &session.session)?;

    // Honor the query filters instead of panicking on them (a request param must never
    // crash the handler → "Unable to connect").
    let filtered = apply_inventory_filters(
        inventory.inventory.0,
        query.consumable_stackable_items_only,
        query.equipped_items_only,
    );

    Ok(Json(GetInventoryResponse {
        inventory: filtered,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use blades_lib::user_data::{Item, ItemPropertiesAll, SingleEquippedItem};
    use uuid::Uuid;

    fn full_inventory() -> CompleteInventory {
        let mut inv = CompleteInventory {
            backpack: Backpack::default(),
            loadout: Loadout::default(),
            treasury: Treasury::default(),
            overflow_treasury: Treasury::default(),
            backpack_version: 7,
            treasury_version: 3,
        };
        // One instanced gear item + one stackable consumable + one equipped item.
        let gear_id = Uuid::from_u128(1);
        inv.backpack.items.0.insert(
            gear_id,
            Item {
                item_template_id: Uuid::from_u128(9),
                tempering_level: 0,
                durability: 75.0,
                grade: None,
                arcane_tier: None,
                properties: ItemPropertiesAll::default(),
            },
        );
        inv.backpack.stackable_items.add(Uuid::from_u128(2), 5);
        let slot = Uuid::from_u128(100);
        inv.loadout.equipped_items.0.insert(
            slot,
            SingleEquippedItem {
                id: Uuid::from_u128(3),
                slot,
                item: Item {
                    item_template_id: Uuid::from_u128(10),
                    tempering_level: 0,
                    durability: 100.0,
                    grade: None,
                    arcane_tier: None,
                    properties: ItemPropertiesAll::default(),
                },
            },
        );
        inv
    }

    /// No filter → the full inventory is returned unchanged (the common full-fetch path,
    /// and the pre-existing behaviour before the panic was removed).
    #[test]
    fn no_filter_returns_everything() {
        let out = apply_inventory_filters(full_inventory(), false, false);
        assert!(!out.backpack.items.is_empty());
        assert!(!out.backpack.stackable_items.is_empty());
        assert!(out.loadout.equipped_items.0.len() == 1);
    }

    /// `consumableStackableItemsOnly=true` must NOT panic (the reported crash) and must
    /// keep only the stackable consumables; instanced gear + treasuries are dropped.
    #[test]
    fn consumable_only_keeps_stackables_no_panic() {
        let out = apply_inventory_filters(full_inventory(), true, false);
        assert!(!out.backpack.stackable_items.is_empty(), "stackables kept");
        assert!(out.backpack.items.is_empty(), "instanced gear dropped");
        assert!(out.treasury.chests().is_empty(), "treasury dropped");
        assert_eq!(out.backpack_version, 7, "versions preserved for optimistic concurrency");
    }

    /// `equippedItemsOnly=true` must NOT panic and must keep only the loadout; the
    /// backpack + treasuries are dropped.
    #[test]
    fn equipped_only_keeps_loadout_no_panic() {
        let out = apply_inventory_filters(full_inventory(), false, true);
        assert_eq!(out.loadout.equipped_items.0.len(), 1, "loadout kept");
        assert!(out.backpack.items.is_empty(), "backpack gear dropped");
        assert!(out.backpack.stackable_items.is_empty(), "backpack stackables dropped");
    }

    /// Both filters at once must not panic (the old assert crashed on either being true).
    #[test]
    fn both_filters_do_not_panic() {
        let out = apply_inventory_filters(full_inventory(), true, true);
        // equippedItemsOnly keeps the loadout; consumableStackableItemsOnly keeps stackables.
        assert_eq!(out.loadout.equipped_items.0.len(), 1);
        assert!(!out.backpack.stackable_items.is_empty());
        assert!(out.backpack.items.is_empty());
    }
}
