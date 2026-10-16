//! Arena Gems on top of retail's match rewards — an **owner-approved deviation
//! from retail** (tracker #390/#391, approved by the owner 2026-10-16, proposed by
//! HauDrauf to bring players to the arena).
//!
//! Retail paid no premium currency for an ordinary arena match: the op49 card's
//! `reward.currencies` carried arena gold only (108/108 captured cards), and the
//! only Gems the ladder paid were the fixed rows in the new-arena promotion loot
//! tables (50 / 100 / 150 / 200 / 300 at Arenas 2-6). Every number below is
//! therefore authored, not measured, and lives here so it stays reviewable in one
//! place instead of disappearing into the retail-calibrated tables.
//!
//! # Which matches pay
//!
//! * **Win** — [`MATCH_WIN_GEMS`].
//! * **Loss** — [`MATCH_LOSS_GEMS`]. A match never ends drawn: the engine settles a
//!   timed-out round on remaining health and then on the lower trophy count
//!   (`MatchCombat::draw_tiebreak_winner`), so the side a tiebreak goes against is
//!   paid exactly as a loss.
//! * **Abandoned / no-show** — nothing. The player who conceded (the exit button or
//!   an ENet departure) is not paid; the opponent who stayed is paid the win. A
//!   player whose result is voided for never sending a byte in a live round
//!   against our AI (`void_ai_match_result`) did not play and is not paid. Paying
//!   a concession would make "queue against the AI, quit at once" a gem farm.
//!
//! # Promotions
//!
//! The Gems a new-arena promotion pays are multiplied by
//! [`PROMOTION_GEM_MULTIPLIER`]. Applied where the promotion is computed
//! (`arena_ladder::promotion_rewards`), so the card and the wallet stay one number.
//! The one-off `promotion_gem_backfill` keeps retail's amounts: it repairs what
//! retail-rate live promotions failed to pay before #413, not this policy.

/// Gems for winning a finished arena match. Owner-approved deviation (#390/#391,
/// 2026-10-16); retail paid none.
pub const MATCH_WIN_GEMS: u64 = 6;

/// Gems for losing a finished arena match. Owner-approved deviation (#390/#391,
/// 2026-10-16); retail paid none.
pub const MATCH_LOSS_GEMS: u64 = 3;

/// Multiplier on the Gems a new-arena promotion pays. Owner-approved deviation
/// (#390/#391, 2026-10-16); retail's loot tables pay ×1.
pub const PROMOTION_GEM_MULTIPLIER: u64 = 2;

/// How one player's side of a match ended, for the Gem payout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchFinish {
    /// Played to the end and won (including a win by the opponent's concession).
    Won,
    /// Played to the end and lost (including a tiebreak loss).
    Lost,
    /// Conceded, left, or never played (voided no-show): not a finished fight.
    Abandoned,
}

impl MatchFinish {
    /// Classify one side. `abandoned` wins over the result: a player who conceded
    /// is recorded as the loser, and a voided no-show is recorded as not winning.
    pub fn of(win: bool, abandoned: bool) -> Self {
        match (abandoned, win) {
            (true, _) => MatchFinish::Abandoned,
            (false, true) => MatchFinish::Won,
            (false, false) => MatchFinish::Lost,
        }
    }

    /// Whether this side counts as a finished fight — the same rule that decides
    /// whether it is paid at all.
    pub fn is_finished(self) -> bool {
        self != MatchFinish::Abandoned
    }
}

/// The Gems one side of a match earns.
pub fn match_gems(finish: MatchFinish) -> u64 {
    match finish {
        MatchFinish::Won => MATCH_WIN_GEMS,
        MatchFinish::Lost => MATCH_LOSS_GEMS,
        MatchFinish::Abandoned => 0,
    }
}

/// A promotion's Gem amount after the owner-approved multiplier.
pub fn promotion_gems(retail_gems: u64) -> u64 {
    retail_gems.saturating_mul(PROMOTION_GEM_MULTIPLIER)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_win_pays_six_and_a_loss_three() {
        assert_eq!(match_gems(MatchFinish::of(true, false)), 6);
        assert_eq!(match_gems(MatchFinish::of(false, false)), 3);
    }

    /// A conceded or voided match is not a finished fight and pays nothing, even
    /// on the side the engine recorded as the winner of a voided match.
    #[test]
    fn an_abandoned_match_pays_nothing() {
        assert_eq!(match_gems(MatchFinish::of(false, true)), 0);
        assert_eq!(match_gems(MatchFinish::of(true, true)), 0);
        assert!(!MatchFinish::of(false, true).is_finished());
        assert!(MatchFinish::of(false, false).is_finished());
    }

    #[test]
    fn promotion_gems_are_doubled() {
        assert_eq!(promotion_gems(50), 100);
        assert_eq!(promotion_gems(300), 600);
        assert_eq!(promotion_gems(0), 0);
    }
}
