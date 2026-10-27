//! One knob for how long the server's timed actions take (tracker #436).
//!
//! HauDrauf asked to halve every wait in the game: building, enchanting,
//! tempering and the rest. Retail's durations stay in the data (the APK's
//! `constructionMs` / `upgradeMs` and recipe durations); this factor is applied
//! where a timer's END is stored — a craft job's `completedAt` and a building's
//! `constructionEnd` — so the time the client counts down, the server's stored
//! end and the gem skip price (which is priced from the stored remaining time)
//! all agree.
//!
//! The client takes both end times from the server: its `RealTimeTimer` for a
//! building or a crafting station is filled from the server's JSON and never
//! restarted from local seconds (checked in the il2cpp disassembly: the only
//! `StartCountDownNow` caller for a building passes the timer's stored end time).
//! The recipe and build menus still show retail's full duration BEFORE the
//! action starts — that number is client data.
//!
//! A timer already running keeps the end it was given. To revert, set
//! [`TIMER_SCALE_PERCENT`] back to 100.

/// Percentage of retail's duration a timed action takes. 100 = retail.
pub const TIMER_SCALE_PERCENT: u64 = 50;

/// `duration_ms` scaled by [`TIMER_SCALE_PERCENT`]. A non-positive duration
/// (instant job) stays as it is.
pub fn scale_ms(duration_ms: i64) -> i64 {
    if duration_ms <= 0 {
        return duration_ms;
    }
    let scaled = i128::from(duration_ms) * i128::from(TIMER_SCALE_PERCENT) / 100;
    i64::try_from(scaled).unwrap_or(i64::MAX)
}

/// [`scale_ms`] for the unsigned durations the town code carries.
pub fn scale_ms_u64(duration_ms: u64) -> u64 {
    let scaled = u128::from(duration_ms) * u128::from(TIMER_SCALE_PERCENT) / 100;
    u64::try_from(scaled).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn report_436_every_timer_runs_at_half_retail() {
        assert_eq!(TIMER_SCALE_PERCENT, 50);
        assert_eq!(scale_ms(600_000), 300_000);
        assert_eq!(scale_ms_u64(18_300_000), 9_150_000);
        // An instant job stays instant; nothing goes negative.
        assert_eq!(scale_ms(0), 0);
        assert_eq!(scale_ms(-5), -5);
        assert_eq!(scale_ms_u64(0), 0);
        // Odd milliseconds round down, never up past half.
        assert_eq!(scale_ms(1_199_911), 599_955);
    }
}
