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

/// The Sigil every finished arena fight pays in December 2026 — an
/// **owner-approved deviation from retail** (tracker #499, HauDrauf's December
/// plan, approved by the owner 2026-11-23: "1 Sigil per arena fight for the
/// whole of December, no limit, fights against the AI included"). Retail paid
/// Sigils for event quests only.
///
/// # What counts as a fight
///
/// Exactly what [`match_gems`] pays: a match played to its end, for the winner
/// AND the loser, against a human or the AI. The side that conceded or left
/// and a voided no-show (never sent a byte in a live round) are paid nothing;
/// the opponent who stayed is paid as the winner. A fight counts by the moment
/// it ENDS.
pub const FIGHT_SIGIL: u64 = 1;

/// 2026-12-01 00:00:00 UTC: the first second a finished fight pays the Sigil.
pub const FIGHT_SIGIL_FROM_SECS: i64 = 1_796_083_200;

/// 2027-01-01 00:00:00 UTC: the first second it no longer does.
pub const FIGHT_SIGIL_UNTIL_SECS: i64 = 1_798_761_600;

/// The Sigil one side of a match earns, for a match that ended at
/// `completed_at_secs` (unix seconds).
pub fn match_sigil(finish: MatchFinish, completed_at_secs: i64) -> u64 {
    let in_window = (FIGHT_SIGIL_FROM_SECS..FIGHT_SIGIL_UNTIL_SECS).contains(&completed_at_secs);
    if in_window && finish.is_finished() {
        FIGHT_SIGIL
    } else {
        0
    }
}

/// `ARENA_FIGHT_SIGIL_REHEARSAL=1`: pay the fight Sigil outside the window to
/// marked TEST characters only (`test_characters`), so the result card and the
/// wallet credit can be seen on a device before 1 December. Unset — the normal
/// state — nothing is paid outside the window to anyone.
pub fn fight_sigil_rehearsal_enabled() -> bool {
    std::env::var("ARENA_FIGHT_SIGIL_REHEARSAL")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false)
}

/// The Sigil for a rehearsing test character: paid for any finished fight,
/// whenever it ends. See [`fight_sigil_rehearsal_enabled`].
pub fn rehearsal_sigil(finish: MatchFinish) -> u64 {
    if finish.is_finished() { FIGHT_SIGIL } else { 0 }
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

    fn utc(y: i32, m: u32, d: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .timestamp()
    }

    /// The window is the calendar month of December 2026, UTC, to the second.
    #[test]
    fn the_sigil_window_is_december_2026_utc() {
        assert_eq!(FIGHT_SIGIL_FROM_SECS, utc(2026, 12, 1));
        assert_eq!(FIGHT_SIGIL_UNTIL_SECS, utc(2027, 1, 1));
    }

    /// One Sigil for a finished fight, won or lost, and none for an abandoned
    /// one — the same rule as the match Gems.
    #[test]
    fn a_finished_december_fight_pays_one_sigil_to_winner_and_loser() {
        let mid = utc(2026, 12, 15) + 12 * 3_600;
        assert_eq!(match_sigil(MatchFinish::Won, mid), 1);
        assert_eq!(match_sigil(MatchFinish::Lost, mid), 1);
        assert_eq!(match_sigil(MatchFinish::Abandoned, mid), 0);
        assert_eq!(match_sigil(MatchFinish::of(true, true), mid), 0);
    }

    /// NOTHING BEFORE 1 DECEMBER 00:00 UTC, nothing from 1 January 00:00 UTC:
    /// the last second before, the first and last second inside, the first
    /// second after — and the day this was written.
    #[test]
    fn the_sigil_starts_and_stops_on_the_second() {
        for finish in [MatchFinish::Won, MatchFinish::Lost] {
            assert_eq!(match_sigil(finish, utc(2026, 11, 23)), 0);
            assert_eq!(match_sigil(finish, FIGHT_SIGIL_FROM_SECS - 1), 0);
            assert_eq!(match_sigil(finish, FIGHT_SIGIL_FROM_SECS), 1);
            assert_eq!(match_sigil(finish, FIGHT_SIGIL_UNTIL_SECS - 1), 1);
            assert_eq!(match_sigil(finish, FIGHT_SIGIL_UNTIL_SECS), 0);
            assert_eq!(
                match_sigil(finish, utc(2027, 12, 15)),
                0,
                "not next December"
            );
            assert_eq!(match_sigil(finish, 0), 0);
        }
    }

    /// A rehearsal pays a finished fight whenever it ends, and still nothing
    /// for an abandoned one.
    #[test]
    fn a_rehearsal_pays_only_finished_fights() {
        assert_eq!(rehearsal_sigil(MatchFinish::Won), 1);
        assert_eq!(rehearsal_sigil(MatchFinish::Lost), 1);
        assert_eq!(rehearsal_sigil(MatchFinish::Abandoned), 0);
    }

    #[test]
    fn promotion_gems_are_doubled() {
        assert_eq!(promotion_gems(50), 100);
        assert_eq!(promotion_gems(300), 600);
        assert_eq!(promotion_gems(0), 0);
    }
}
