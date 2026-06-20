//! Global store — `GET /catalogoverrides/globalshop`, `GET /globalshops/current`,
//! `POST /globalshops/current/purchase`.
//!
//! The store is the Sigil/Gem sink. The base catalogue (price list + contents) lives
//! in the client's asset bundles, so two things are capture-derived instead: the
//! *override* catalogue (special/limited offers, served verbatim) and a
//! `productId -> reward` map (what each bought product grants). The price comes from
//! the client's `expectedPrices` — which we sanity-check and then debit for real
//! (failing on insufficient funds). Per-character purchase counts live in
//! `server_state.global_shop_purchases`.
//!
//! Captured purchase:
//! ```jsonc
//! // request
//! { "globalShopProductId": "afc71167-…", "gemsPayment": false,
//!   "expectedPrices": [ { "currencyId": "470c8f58-…", "quantity": 1 } ] }
//! // response
//! { "inventory": …, "wallet": [ … ], "globalShop": { "globalShopPurchases": [ {id,quantity} ] },
//!   "reward": { "stackableItems": { … }, "townXp": 142 } }
//! ```

use std::collections::HashMap;

use serde::Serialize;
use thiserror::Error;
use uuid::Uuid;

use crate::economy::{self, Price};

#[derive(Debug, Error, PartialEq, Eq)]
pub enum PurchaseError {
    /// We have no `productId -> reward` mapping, so the purchase can't be fulfilled
    /// faithfully (the product was never seen in captures).
    #[error("no such global-shop product")]
    NoSuchProduct,
    /// The client's `expectedPrices` are empty, use a non-game currency, or are
    /// implausibly large — reject rather than charge something nonsensical.
    #[error("invalid expected prices")]
    InvalidPrice,
}

/// Hard ceiling on a single price line — global-shop prices are small (Sigil/Gems,
/// usually single/double digits). Anything larger is a malformed/abusive request.
const MAX_PRICE_QUANTITY: u64 = 1_000_000;

/// Validate the client-supplied `expectedPrices`: non-empty, each line a known game
/// currency with a plausible quantity. (The base price list lives in the client
/// bundles, so we trust the client's price but bound it.)
pub fn sanitize_prices(prices: &[Price]) -> Result<(), PurchaseError> {
    if prices.is_empty() {
        return Err(PurchaseError::InvalidPrice);
    }
    for p in prices {
        if !economy::is_currency(p.currency_id) || p.quantity == 0 || p.quantity > MAX_PRICE_QUANTITY
        {
            return Err(PurchaseError::InvalidPrice);
        }
    }
    Ok(())
}

/// One entry of the `globalShopPurchases` list.
#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PurchaseEntry {
    pub id: String,
    pub quantity: u64,
}

/// Build the `globalShopPurchases` list from per-product counts (base `productId`
/// entries; the retail server also emits `::override::…` tracking variants for
/// limited-time caps, which we omit).
pub fn purchases_list(counts: &HashMap<Uuid, u64>) -> Vec<PurchaseEntry> {
    let mut list: Vec<PurchaseEntry> = counts
        .iter()
        .map(|(id, quantity)| PurchaseEntry {
            id: id.to_string(),
            quantity: *quantity,
        })
        .collect();
    // Deterministic order (HashMap iteration is not stable).
    list.sort_by(|a, b| a.id.cmp(&b.id));
    list
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::{GEMS, GOLD, SIGIL};

    #[test]
    fn sanitize_accepts_a_normal_sigil_price() {
        assert_eq!(sanitize_prices(&[Price::new(SIGIL, 1)]), Ok(()));
        assert_eq!(sanitize_prices(&[Price::new(GEMS, 250)]), Ok(()));
        assert_eq!(sanitize_prices(&[Price::new(GOLD, 5000)]), Ok(()));
    }

    #[test]
    fn sanitize_rejects_empty_zero_unknown_and_absurd() {
        assert_eq!(sanitize_prices(&[]), Err(PurchaseError::InvalidPrice));
        assert_eq!(
            sanitize_prices(&[Price::new(SIGIL, 0)]),
            Err(PurchaseError::InvalidPrice)
        );
        let not_a_currency = Uuid::from_u128(0xdead);
        assert_eq!(
            sanitize_prices(&[Price::new(not_a_currency, 1)]),
            Err(PurchaseError::InvalidPrice)
        );
        assert_eq!(
            sanitize_prices(&[Price::new(GEMS, MAX_PRICE_QUANTITY + 1)]),
            Err(PurchaseError::InvalidPrice)
        );
    }

    #[test]
    fn purchases_list_is_sorted_and_complete() {
        let counts = HashMap::from([
            (Uuid::from_u128(2), 3u64),
            (Uuid::from_u128(1), 1u64),
        ]);
        let list = purchases_list(&counts);
        assert_eq!(list.len(), 2);
        assert!(list[0].id < list[1].id, "deterministic sorted order");
    }
}
