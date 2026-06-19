//! Chests — `POST /chests/{id}/collect`.
//!
//! Treasury chests (earned from dungeons / daily rewards, or already present on an
//! imported character) are opened for loot. Retail rolls each chest's loot at open
//! time; we don't have the per-tier loot tables (only post-open captures), so we draw
//! a *representative* loot bundle from a capture-derived pool, chosen deterministically
//! by chest id (so a given chest always yields the same thing). The handler re-mints
//! the instanced item ids before granting (capture ids would collide across players).

use crate::economy::RewardGrant;

/// Pick a representative loot bundle for a chest, keyed deterministically by its id.
/// Returns `None` only if the pool is empty.
pub fn pick_loot<'a>(pool: &'a [RewardGrant], chest_id: &str) -> Option<&'a RewardGrant> {
    if pool.is_empty() {
        return None;
    }
    let hash = chest_id
        .bytes()
        .fold(0u64, |acc, b| acc.wrapping_mul(31).wrapping_add(b as u64));
    Some(&pool[(hash as usize) % pool.len()])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::economy::GOLD;
    use std::collections::HashMap;

    fn pool() -> Vec<RewardGrant> {
        vec![
            RewardGrant {
                currencies: HashMap::from([(GOLD, 10)]),
                ..Default::default()
            },
            RewardGrant {
                currencies: HashMap::from([(GOLD, 20)]),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn pick_is_deterministic_per_chest_id() {
        let p = pool();
        let a = pick_loot(&p, "1").unwrap().currencies[&GOLD];
        let b = pick_loot(&p, "1").unwrap().currencies[&GOLD];
        assert_eq!(a, b, "same id -> same loot");
    }

    #[test]
    fn empty_pool_yields_none() {
        assert!(pick_loot(&[], "1").is_none());
    }
}
