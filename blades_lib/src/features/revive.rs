//! Reviving after death inside a dungeon or an Abyss run.
//!
//! The client reports only *that* the player revived; the server charges for it.
//! Every one of the 94 captured `revive` actions (80 in `/dungeons/current/update`,
//! 14 in `/abysses/current/update`) comes back with exactly one stackable in the
//! inventory diff — the Scroll of Revival — at its new count. The client never sends
//! an `item_consumed` for the scroll.
//!
//! The price rises with the number of revives already used in the same run. Two
//! independent sources agree on the ladder:
//!
//! * **The APK.** `MiscGameplayConstants._consecutiveRevive` is a list of ten
//!   entries, each with a `_reviveItemCostList` naming the scroll and a quantity:
//!   1, 2, 4, 4, 4, 4, 4, 4, 4, 4.
//! * **The captures.** Scroll-count deltas across consecutive revives in one run:
//!   13 observations of 1 at the first revive, 17 of 2 at the second, and 7, 2 and 1
//!   of 4 at the third, fourth and fifth.
//!
//! Each entry also carries `_playerStats` of 1.0/1.0/1.0 — a revive restores full
//! health, stamina and magicka. The server tracks none of those mid-dungeon, so that
//! half of the constant is the client's to apply.

use uuid::Uuid;

/// `05a7d501-f096-41e9-8443-fd6a090d8425` — "Scroll of Revival"
/// (`Items.Name.ReviveScroll`), the only tender in every `_reviveItemCostList`.
pub const REVIVE_SCROLL_TEMPLATE: Uuid =
    Uuid::from_u128(0x05a7_d501_f096_41e9_8443_fd6a_090d_8425);

/// Scrolls charged for the 1st, 2nd, … revive of a run, from
/// `MiscGameplayConstants._consecutiveRevive`.
const CONSECUTIVE_REVIVE_SCROLL_COST: [u64; 10] = [1, 2, 4, 4, 4, 4, 4, 4, 4, 4];

/// Scrolls owed for the revive that follows `revives_already_used` in this run.
///
/// Past the tenth the APK has no entry — `MiscGameplayConstants.REVIVE_UNAVAILABLE`
/// is what the client resolves, and it stops offering the button. Nothing stops a
/// modified client from asking anyway, so this keeps charging the last rung rather
/// than letting an 11th revive through free.
pub fn scroll_cost(revives_already_used: u64) -> u64 {
    let index = usize::try_from(revives_already_used).unwrap_or(usize::MAX);
    CONSECUTIVE_REVIVE_SCROLL_COST
        .get(index)
        .copied()
        .unwrap_or(CONSECUTIVE_REVIVE_SCROLL_COST[CONSECUTIVE_REVIVE_SCROLL_COST.len() - 1])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The ladder as the APK stores it, and as the capture deltas measured it.
    #[test]
    fn ladder_matches_the_apk() {
        let charged: Vec<u64> = (0..10).map(scroll_cost).collect();
        assert_eq!(charged, vec![1, 2, 4, 4, 4, 4, 4, 4, 4, 4]);
    }

    /// An 11th revive is not free just because the constant ran out.
    #[test]
    fn past_the_last_rung_keeps_charging() {
        assert_eq!(scroll_cost(10), 4);
        assert_eq!(scroll_cost(9_999), 4);
        assert_eq!(scroll_cost(u64::MAX), 4);
    }

    /// Guards the template id against a typo: it is the one the captures show being
    /// decremented, and a wrong one would silently charge nothing.
    #[test]
    fn scroll_template_is_the_captured_one() {
        assert_eq!(
            REVIVE_SCROLL_TEMPLATE.to_string(),
            "05a7d501-f096-41e9-8443-fd6a090d8425"
        );
    }
}
