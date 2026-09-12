//! Chests — `POST /chests/{id}/collect`.
//!
//! Treasury chests (earned from dungeons / daily rewards, or already present on an
//! imported character) are opened for loot. Retail rolls each chest's loot at open
//! time; we don't have the per-tier loot tables (only post-open captures), so we draw
//! a *representative* loot bundle from a capture-derived pool. The choice is stable for
//! one granted chest but includes the character and treasury generation: numeric chest
//! ids are reused after collection, so id alone made every one-at-a-time dungeon chest
//! yield the exact same bundle (#120). The handler re-mints the instanced item ids before
//! granting (capture ids would collide across players).

use crate::economy::RewardGrant;

/// Pick a representative loot bundle for a chest, keyed deterministically by the
/// character, its treasury generation and the chest's own fields.
/// Returns `None` only if the pool is empty.
pub fn pick_loot<'a>(pool: &'a [RewardGrant], key: &str) -> Option<&'a RewardGrant> {
    if pool.is_empty() {
        return None;
    }
    let hash = key
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
    fn pick_is_deterministic_per_granted_chest() {
        let p = pool();
        let a = pick_loot(&p, "character-a:7:1:1:20").unwrap().currencies[&GOLD];
        let b = pick_loot(&p, "character-a:7:1:1:20").unwrap().currencies[&GOLD];
        assert_eq!(a, b, "same granted chest -> same loot");
    }

    #[test]
    fn a_reused_numeric_id_does_not_repeat_forever() {
        let p: Vec<_> = (0..40)
            .map(|n| RewardGrant {
                currencies: HashMap::from([(GOLD, n)]),
                ..Default::default()
            })
            .collect();
        let outcomes: std::collections::HashSet<_> = (1..=40)
            // Chest id `1` is reused whenever the previous only chest was opened.
            // Treasury version persists and advances across those grants.
            .map(|generation| {
                pick_loot(&p, &format!("character-a:1:1:20:{generation}"))
                    .unwrap()
                    .currencies[&GOLD]
            })
            .collect();
        assert!(
            outcomes.len() > 20,
            "reused id must still draw across the pool"
        );
    }

    #[test]
    fn empty_pool_yields_none() {
        assert!(pick_loot(&[], "character-a:1:1:20:1").is_none());
    }
}
