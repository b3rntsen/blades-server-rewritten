//! One-off arena events — HauDrauf's "Arena Happy Hour" (tracker #526).
//!
//! An event is a fixed window, compiled in like the December 2026 calendar, in
//! which the arena runs on different rules. Outside every window nothing here
//! changes anything: no fight is counted, no Sigil is paid and the matchmaker
//! hands out AI opponents exactly as before.
//!
//! # The rules inside a window (HauDrauf, #526, messages 3898 and 3960)
//!
//! * **No AI opponents.** Nobody is handed a bot while the window is open; a
//!   search waits for a human ([`ai_off_at`], read by the matchmaker).
//! * **Only human fights count.** A fight counts when it was PAIRED inside the
//!   window (the promise in message 3408: "the server counts every fight that
//!   begins in the window"), was finished (won or lost — the arena Gems' rule,
//!   [`super::arena_bonus_rewards::MatchFinish`]: the side that conceded or left
//!   does not count) and was against a human.
//! * **Counted per arena account** (`users.id`), like the every-25th-fight
//!   weapon: a player's alts share one count, so switching alts gains nothing.
//! * **Minimum 5 fights.** Nothing is paid before an account's 5th counted fight.
//! * **1 Sigil per fight.** The four Sigils of fights 1–4 are paid with the 5th
//!   (5 at once), then 1 for every further fight ([`ArenaEvent::sigils_for_fight`]).
//! * **A weapon after every 5th fight**, won or lost: a Divine Nordic Steel
//!   weapon (May's monthly material), blank at Arcane II — "zwei garantierte
//!   Verzauberungen", exactly as the monthly arena weapon is gifted
//!   ([`super::arena_fight_gift`]). Each account walks its own shuffled order of
//!   the nine weapon types, so its first nine weapons are nine different ones and
//!   the tenth is the first repeat ([`ArenaEvent::weapon_for`]).
//! * **A participation gift** at the 5th fight: 2 × decoration no. 93 of his
//!   list (Beacon / Leuchtfeuer) and 2 × the Divine Dark Seducer armour set
//!   (the December set), Arcane II.
//! * **"All levels get the highest level":** every item is tier 10, required
//!   level 45 (the Divine sets), whatever the player's level.
//! * **Inventory 95 %:** gear in these gifts fills the backpack only up to 95 %
//!   of its capacity; the rest is filed and paid from the end of the window on,
//!   as soon as there is room ([`blades_lib::features::gift_capacity`]).
//!   Decorations are stackables and always arrive.
//!
//! # Delivery
//!
//! Weapons and the participation gift are claimable gifts, one banner each,
//! claimed by the alt that fought the fight that earned them — stored as rows of
//! `arena_fight_gifts` with a NEGATIVE milestone ([`ArenaEvent::milestone`]), so
//! the banner, the gift view and the claim reuse the weapon gift's paths and can
//! never collide with its positive every-25th milestones. The Sigils ride the
//! fight's result card like the December Sigil does.

use anyhow::Result;
use blades_lib::features::gift_capacity::GearCapRule;
use blades_lib::static_data::GiftItem;
use diesel_async::{AsyncPgConnection, RunQueryDsl};
use uuid::Uuid;

use super::arena_fight_gift::{GiftWeapon, MONTHLY_WEAPON_SETS, WEAPON_GIFT_ARCANE_TIER};

/// One compiled-in arena event.
#[derive(Debug, PartialEq, Eq)]
pub struct ArenaEvent {
    /// Stable key, stored with every counted fight.
    pub id: &'static str,
    /// The event's number in the gift milestone space (see [`Self::milestone`]).
    pub ordinal: i64,
    /// First second of the window (unix, UTC).
    pub opens_at_secs: i64,
    /// First second after the window (unix, UTC).
    pub closes_at_secs: i64,
    /// Counted fights per weapon.
    pub fights_per_weapon: i64,
    /// Counted fights before anything is paid.
    pub min_fights: i64,
    /// Index into [`MONTHLY_WEAPON_SETS`].
    pub weapon_set: usize,
    /// Copies of each piece of [`PARTICIPATION_ARMOR_SET`] in the participation gift.
    pub armor_sets: u64,
    /// Decoration lines of the participation gift: (template, quantity).
    pub decorations: &'static [(Uuid, u64)],
    /// The share of the backpack gifted gear may fill, in percent.
    pub gear_fill_percent: u64,
}

/// [`MONTHLY_WEAPON_SETS`] index of Divine Nordic Steel (May's monthly material).
pub const NORDIC_STEEL_SET: usize = 7;

/// Decoration no. 93 of HauDrauf's numbered list (#499, message 3172):
/// "Leuchtfeuer (das blaue) / Beacon", `Items.Name.Small_Beacon`.
pub const BEACON_DECORATION: Uuid = Uuid::from_u128(0x3bed_ef8a_2a4b_4953_81fa_0d38_a8b9_7634);

/// The Divine (tier 10, level 45) Dark Seducer set — the December 2026 armour
/// set (#499): armour, boots, gauntlets, helmet. The Divine set has no shield.
pub const PARTICIPATION_ARMOR_SET: [(Uuid, &str); 4] = [
    (
        Uuid::from_u128(0x1063_c3f9_1bbf_4962_b80d_fb98_3207_5c1c),
        "Divine Dark Seducer Armor",
    ),
    (
        Uuid::from_u128(0x2c9a_c3da_4a90_4fcb_bb59_4202_fdec_ffe0),
        "Divine Dark Seducer Boots",
    ),
    (
        Uuid::from_u128(0x15f9_963e_059c_464c_86ad_3a7e_aa7c_ea9d),
        "Divine Dark Seducer Gauntlets",
    ),
    (
        Uuid::from_u128(0x0c28_69fb_3a95_45c6_8dcc_706d_43f0_fab0),
        "Divine Dark Seducer Helmet 03",
    ),
];

/// HauDrauf's first Arena Happy Hour (#526, message 3960): Saturday
/// 28 November 2026, 20:00–21:00 German time = 19:00–20:00 UTC. One hour: the
/// event was planned as a "Happy Hour" ("in dieser einen Stunde", the report itself).
pub const HAPPY_HOUR_2026_11_28: ArenaEvent = ArenaEvent {
    id: "happy-hour-2026-11-28",
    ordinal: 20_261_128,
    opens_at_secs: 1_795_892_400,
    closes_at_secs: 1_795_896_000,
    fights_per_weapon: 5,
    min_fights: 5,
    weapon_set: NORDIC_STEEL_SET,
    armor_sets: 2,
    decorations: &[(BEACON_DECORATION, 2)],
    gear_fill_percent: 95,
};

/// Every event, in time order.
pub const EVENTS: &[ArenaEvent] = &[HAPPY_HOUR_2026_11_28];

/// Milestones of one event occupy `-(ordinal * MILESTONE_STRIDE + seq)`.
const MILESTONE_STRIDE: i64 = 1_000_000;

#[cfg(test)]
thread_local! {
    /// A test's pinned event clock, for the matchmaker actor tests: a
    /// current-thread tokio test runs its actor on this same thread.
    pub static TEST_CLOCK: std::cell::Cell<Option<i64>> = const { std::cell::Cell::new(None) };
}

/// The clock event windows are read against: the wall clock (a test may pin it).
pub fn now_secs() -> i64 {
    #[cfg(test)]
    if let Some(t) = TEST_CLOCK.with(|c| c.get()) {
        return t;
    }
    super::arena_fight_gift::unix_now()
}

/// The event whose window contains `unix_secs`.
pub fn event_at(unix_secs: i64) -> Option<&'static ArenaEvent> {
    EVENTS
        .iter()
        .find(|e| (e.opens_at_secs..e.closes_at_secs).contains(&unix_secs))
}

/// Whether AI opponents are switched off at `unix_secs`: inside any event window.
pub fn ai_off_at(unix_secs: i64) -> bool {
    event_at(unix_secs).is_some()
}

/// Whether an event window is open or close enough (an hour either side) that
/// a queueing player's event count is worth reading for the result card.
pub fn event_near(unix_secs: i64) -> Option<&'static ArenaEvent> {
    EVENTS
        .iter()
        .find(|e| (e.opens_at_secs - 3_600..e.closes_at_secs + 3_600).contains(&unix_secs))
}

/// The event a fight counts toward: paired inside its window, finished (won or
/// lost), against a human. `None` for every other fight.
pub fn counted_event(
    paired_at_secs: i64,
    finished: bool,
    versus_human: bool,
) -> Option<&'static ArenaEvent> {
    if !finished || !versus_human {
        return None;
    }
    event_at(paired_at_secs)
}

/// The event (and the sequence number within it) a gift milestone belongs to.
/// Every positive milestone is the every-25th-fight weapon's.
pub fn event_of_milestone(milestone: i64) -> Option<(&'static ArenaEvent, i64)> {
    if milestone >= 0 {
        return None;
    }
    let m = milestone.checked_neg()?;
    let (ordinal, seq) = (m / MILESTONE_STRIDE, m % MILESTONE_STRIDE);
    EVENTS
        .iter()
        .find(|e| e.ordinal == ordinal)
        .map(|e| (e, seq))
}

/// A splitmix64 step: the deterministic stream the weapon order is drawn from.
fn splitmix(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl ArenaEvent {
    /// The Sigil paid for an account's `n`th counted fight (1-based): nothing
    /// before the minimum, the backlog of every fight so far on the fight that
    /// reaches it, then one a fight.
    pub fn sigils_for_fight(&self, n: i64) -> u64 {
        if n < self.min_fights {
            0
        } else if n == self.min_fights {
            self.min_fights as u64
        } else {
            1
        }
    }

    /// The weapon number (1-based) the account's `n`th counted fight earns.
    pub fn weapon_due(&self, n: i64) -> Option<i64> {
        (n >= self.min_fights && n > 0 && n % self.fights_per_weapon == 0)
            .then(|| n / self.fights_per_weapon)
    }

    /// Whether the `n`th counted fight earns the participation gift.
    pub fn participation_due(&self, n: i64) -> bool {
        n == self.min_fights
    }

    /// This account's order of the weapon set's nine weapon types: a shuffle
    /// seeded by the account and the event, so it is fixed for the account and
    /// differs between accounts.
    pub fn weapon_order(&self, user_id: Uuid) -> [usize; 9] {
        let mut order = [0, 1, 2, 3, 4, 5, 6, 7, 8];
        let mut state =
            crate::jewelry_roll::seed(&[user_id.as_bytes(), self.id.as_bytes()], 0x0526);
        for i in (1..order.len()).rev() {
            let j = (splitmix(&mut state) % (i as u64 + 1)) as usize;
            order.swap(i, j);
        }
        order
    }

    /// The account's `k`th weapon (1-based): its shuffled order walked in turn,
    /// so weapons 1–9 are the nine types once each and the 10th is the first
    /// repeat.
    pub fn weapon_for(&self, user_id: Uuid, k: i64) -> GiftWeapon {
        let order = self.weapon_order(user_id);
        let slot = (k.max(1) - 1) as usize % order.len();
        MONTHLY_WEAPON_SETS[self.weapon_set].1[order[slot]]
    }

    /// The gift line for the account's `k`th weapon: blank, Arcane II.
    pub fn weapon_gift(&self, user_id: Uuid, k: i64) -> GiftItem {
        GiftItem {
            item_template_id: self.weapon_for(user_id, k).0,
            quantity: 1,
            arcane_tier: Some(WEAPON_GIFT_ARCANE_TIER),
            enchanting: Vec::new(),
        }
    }

    /// The participation gift's lines: the decorations, then the armour set
    /// (Arcane II, `armor_sets` copies of each piece).
    pub fn participation_gift(&self) -> Vec<GiftItem> {
        let decorations = self.decorations.iter().map(|(t, q)| GiftItem {
            item_template_id: *t,
            quantity: *q,
            arcane_tier: None,
            enchanting: Vec::new(),
        });
        let armour = PARTICIPATION_ARMOR_SET.iter().map(|(t, _)| GiftItem {
            item_template_id: *t,
            quantity: self.armor_sets,
            arcane_tier: Some(WEAPON_GIFT_ARCANE_TIER),
            enchanting: Vec::new(),
        });
        decorations.chain(armour).collect()
    }

    /// The gift milestone of sequence number `seq` (0 = the participation gift,
    /// `k` = the `k`th weapon): negative, so it never meets a weapon-gift one.
    pub fn milestone(&self, seq: i64) -> i64 {
        -(self.ordinal * MILESTONE_STRIDE + seq)
    }

    /// The backpack rule the event's gifts carry: gear fills at most
    /// `gear_fill_percent` of the backpack; the rest is paid from the end of
    /// the window.
    pub fn gear_rule(&self) -> GearCapRule {
        GearCapRule {
            fill_percent: self.gear_fill_percent,
            payable_from_secs: self.closes_at_secs,
        }
    }

    /// The server-side description of the gift at `seq`.
    pub fn gift_description(&self, seq: i64) -> String {
        if seq == 0 {
            format!("Arena event {}: participation gift", self.id)
        } else {
            format!("Arena event {}: weapon {seq}", self.id)
        }
    }
}

/// What one counted fight did for its account in an event.
#[derive(Debug, Clone, PartialEq)]
pub struct EventTally {
    /// The account's counted fights in the event after this one.
    pub fights: i64,
    /// Sigil this fight pays.
    pub sigil: u64,
    /// Gifts minted by this fight: (gift id, milestone).
    pub gifts: Vec<(Uuid, i64)>,
}

/// The account's counted fights in `event` so far (0 before its first). Read at
/// queue time so the result card can show the Sigil.
pub async fn account_event_fights(
    conn: &mut AsyncPgConnection,
    event: &ArenaEvent,
    user_id: Uuid,
) -> Result<i64> {
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        fights: i64,
    }
    let rows: Vec<Count> = diesel::sql_query(
        "SELECT fights FROM arena_event_counts WHERE event_id = $1 AND user_id = $2",
    )
    .bind::<diesel::sql_types::Text, _>(event.id)
    .bind::<diesel::sql_types::Uuid, _>(user_id)
    .load(conn)
    .await?;
    Ok(rows.into_iter().next().map_or(0, |r| r.fights))
}

/// Count one fight toward `event` for `user_id` and mint what it earned.
///
/// Idempotent per fight: the fight is logged under its `gameSessionId`, and a
/// fight already logged counts nothing and pays nothing (`None`). A match with
/// no session id (a dev match) is logged under a fresh id. Run it inside the
/// match-reward transaction, as a savepoint.
pub async fn record_event_fight(
    conn: &mut AsyncPgConnection,
    event: &ArenaEvent,
    user_id: Uuid,
    character_id: Uuid,
    alt: Option<Uuid>,
    game_session_id: Option<Uuid>,
    now_secs: i64,
) -> Result<Option<EventTally>> {
    let fight = game_session_id.unwrap_or_else(Uuid::new_v4);
    let logged = diesel::sql_query(
        "INSERT INTO arena_event_fights (event_id, user_id, game_session_id, character_id, recorded_at) \
         VALUES ($1, $2, $3, $4, $5) ON CONFLICT DO NOTHING",
    )
    .bind::<diesel::sql_types::Text, _>(event.id)
    .bind::<diesel::sql_types::Uuid, _>(user_id)
    .bind::<diesel::sql_types::Uuid, _>(fight)
    .bind::<diesel::sql_types::Uuid, _>(character_id)
    .bind::<diesel::sql_types::BigInt, _>(now_secs)
    .execute(conn)
    .await?;
    if logged != 1 {
        return Ok(None);
    }
    #[derive(diesel::QueryableByName)]
    struct Count {
        #[diesel(sql_type = diesel::sql_types::BigInt)]
        fights: i64,
    }
    let Count { fights } = diesel::sql_query(
        "INSERT INTO arena_event_counts (event_id, user_id, fights, updated_at) \
         VALUES ($1, $2, 1, $3) \
         ON CONFLICT (event_id, user_id) DO UPDATE SET \
             fights = arena_event_counts.fights + 1, updated_at = EXCLUDED.updated_at \
         RETURNING fights",
    )
    .bind::<diesel::sql_types::Text, _>(event.id)
    .bind::<diesel::sql_types::Uuid, _>(user_id)
    .bind::<diesel::sql_types::BigInt, _>(now_secs)
    .get_result(conn)
    .await?;

    let mut due: Vec<(i64, serde_json::Value)> = Vec::new();
    if event.participation_due(fights) {
        due.push((0, serde_json::to_value(event.participation_gift())?));
    }
    if let Some(k) = event.weapon_due(fights) {
        due.push((k, serde_json::to_value(event.weapon_gift(user_id, k))?));
    }
    let mut gifts = Vec::new();
    for (seq, item) in due {
        let id = Uuid::new_v4();
        let milestone = event.milestone(seq);
        let inserted = diesel::sql_query(
            "INSERT INTO arena_fight_gifts \
             (id, user_id, character_id, source_alt_uuid, milestone, item, created_at, granted_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, NULL) \
             ON CONFLICT (user_id, milestone) DO NOTHING",
        )
        .bind::<diesel::sql_types::Uuid, _>(id)
        .bind::<diesel::sql_types::Uuid, _>(user_id)
        .bind::<diesel::sql_types::Uuid, _>(character_id)
        .bind::<diesel::sql_types::Nullable<diesel::sql_types::Uuid>, _>(alt)
        .bind::<diesel::sql_types::BigInt, _>(milestone)
        .bind::<diesel::sql_types::Jsonb, _>(item)
        .bind::<diesel::sql_types::BigInt, _>(now_secs)
        .execute(conn)
        .await?;
        if inserted == 1 {
            gifts.push((id, milestone));
        }
    }
    Ok(Some(EventTally {
        fights,
        sigil: event.sigils_for_fight(fights),
        gifts,
    }))
}

/// The gift lines stored in an `arena_fight_gifts.item`: one line (the weapon
/// gifts) or a list (the participation gift).
pub fn gift_lines(item: &serde_json::Value) -> Option<Vec<GiftItem>> {
    if item.is_array() {
        serde_json::from_value(item.clone()).ok()
    } else {
        serde_json::from_value::<GiftItem>(item.clone())
            .ok()
            .map(|line| vec![line])
    }
}

/// Whether `template` is one of the weapons any event can gift.
pub fn is_event_weapon(template: Uuid) -> bool {
    EVENTS.iter().any(|e| {
        MONTHLY_WEAPON_SETS[e.weapon_set]
            .1
            .iter()
            .any(|w| w.0 == template)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::arena::arena_bonus_rewards::MatchFinish;
    use std::collections::BTreeSet;

    fn utc(y: i32, m: u32, d: u32, h: u32, min: u32) -> i64 {
        chrono::NaiveDate::from_ymd_opt(y, m, d)
            .unwrap()
            .and_hms_opt(h, min, 0)
            .unwrap()
            .and_utc()
            .timestamp()
    }

    const E: &ArenaEvent = &HAPPY_HOUR_2026_11_28;

    /// Saturday 28.11.2026, 20:00 German time (CET, UTC+1) = 19:00 UTC, one hour.
    #[test]
    fn the_happy_hour_is_saturday_28_november_19_to_20_utc() {
        assert_eq!(E.opens_at_secs, utc(2026, 11, 28, 19, 0));
        assert_eq!(E.closes_at_secs, utc(2026, 11, 28, 20, 0));
        use chrono::Datelike;
        let open = chrono::DateTime::from_timestamp(E.opens_at_secs, 0).unwrap();
        assert_eq!(open.weekday(), chrono::Weekday::Sat);
        assert_eq!(MONTHLY_WEAPON_SETS[E.weapon_set].0, "Nordic Steel");
    }

    /// AI is off exactly inside the window, to the second, and nowhere else.
    #[test]
    fn ai_is_off_only_inside_the_window() {
        assert!(!ai_off_at(E.opens_at_secs - 1));
        assert!(ai_off_at(E.opens_at_secs));
        assert!(ai_off_at(E.closes_at_secs - 1));
        assert!(!ai_off_at(E.closes_at_secs));
        assert!(!ai_off_at(utc(2026, 11, 25, 12, 0)), "today");
        assert!(!ai_off_at(utc(2026, 12, 5, 19, 30)), "next Saturday");
    }

    /// Only finished fights against a human, paired inside the window, count.
    #[test]
    fn only_finished_human_fights_paired_in_the_window_count() {
        let mid = E.opens_at_secs + 1_800;
        let finished = |f: MatchFinish| f.is_finished();
        assert_eq!(
            counted_event(mid, finished(MatchFinish::Won), true),
            Some(E)
        );
        assert_eq!(
            counted_event(mid, finished(MatchFinish::Lost), true),
            Some(E)
        );
        assert_eq!(
            counted_event(mid, finished(MatchFinish::Abandoned), true),
            None
        );
        assert_eq!(counted_event(mid, true, false), None, "versus the AI");
        assert_eq!(counted_event(E.opens_at_secs - 1, true, true), None);
        assert_eq!(counted_event(E.closes_at_secs - 1, true, true), Some(E));
        assert_eq!(counted_event(E.closes_at_secs, true, true), None);
    }

    /// Minimum 5: fights 1–4 pay nothing, the 5th pays all five Sigils, then one
    /// a fight — 1 Sigil per fight for anyone who reaches five.
    #[test]
    fn sigils_wait_for_the_fifth_fight_then_one_a_fight() {
        let paid: Vec<u64> = (1..=12).map(|n| E.sigils_for_fight(n)).collect();
        assert_eq!(paid, [0, 0, 0, 0, 5, 1, 1, 1, 1, 1, 1, 1]);
        for n in 5..=60 {
            let total: u64 = (1..=n).map(|i| E.sigils_for_fight(i)).sum();
            assert_eq!(total, n as u64, "after {n} fights, {n} Sigil");
        }
        for n in 1..=4 {
            let total: u64 = (1..=n).map(|i| E.sigils_for_fight(i)).sum();
            assert_eq!(total, 0, "below the minimum nothing");
        }
    }

    /// A weapon after every 5th fight and only then; the participation gift once,
    /// at the 5th.
    #[test]
    fn a_weapon_every_fifth_fight_and_the_gift_at_the_fifth() {
        let weapons: Vec<(i64, i64)> = (1..=50)
            .filter_map(|n| E.weapon_due(n).map(|k| (n, k)))
            .collect();
        assert_eq!(
            weapons,
            (1..=10).map(|k| (5 * k, k)).collect::<Vec<_>>(),
            "fights 5, 10, … 50 earn weapons 1 … 10"
        );
        let gifts: Vec<i64> = (1..=50).filter(|n| E.participation_due(*n)).collect();
        assert_eq!(gifts, [5]);
    }

    /// The first nine weapons of an account are nine different types; the 10th
    /// repeats the 1st; and the order is fixed per account but not the same for
    /// everyone.
    #[test]
    fn no_repeat_until_the_tenth_weapon() {
        let mut orders = BTreeSet::new();
        for n in 0..200u128 {
            let user = Uuid::from_u128(0x5eed_0000 + n);
            let first_nine: BTreeSet<Uuid> = (1..=9).map(|k| E.weapon_for(user, k).0).collect();
            assert_eq!(first_nine.len(), 9, "user {n}: nine different weapons");
            for k in 1..=9 {
                assert_eq!(
                    E.weapon_for(user, k + 9),
                    E.weapon_for(user, k),
                    "user {n} k {k}"
                );
            }
            assert_eq!(E.weapon_order(user), E.weapon_order(user), "deterministic");
            orders.insert(E.weapon_order(user));
        }
        assert!(
            orders.len() > 150,
            "orders differ between accounts: {}",
            orders.len()
        );
        // Every first weapon is common: no type is starved.
        let firsts: BTreeSet<Uuid> = (0..200u128)
            .map(|n| E.weapon_for(Uuid::from_u128(0x5eed_0000 + n), 1).0)
            .collect();
        assert_eq!(firsts.len(), 9);
    }

    /// The weapon line: a Divine Nordic Steel weapon, blank, Arcane II — the
    /// monthly gift's "two guaranteed enchantments".
    #[test]
    fn the_weapon_is_divine_nordic_steel_blank_arcane_two() {
        let user = Uuid::from_u128(42);
        for k in 1..=9 {
            let line = E.weapon_gift(user, k);
            let (template, _, name) = E.weapon_for(user, k);
            assert_eq!(line.item_template_id, template);
            assert!(name.starts_with("Divine Nordic Steel "), "{name}");
            assert_eq!(line.quantity, 1);
            assert_eq!(line.arcane_tier, Some(2));
            assert!(line.enchanting.is_empty());
            assert!(is_event_weapon(template));
        }
    }

    /// The participation gift: 2 × Beacon (no. 93) and 2 × the four Divine Dark
    /// Seducer pieces, each at Arcane II.
    #[test]
    fn the_participation_gift_is_two_beacons_and_two_divine_dark_seducer_sets() {
        let lines = E.participation_gift();
        assert_eq!(lines.len(), 5);
        assert_eq!(lines[0].item_template_id, BEACON_DECORATION);
        assert_eq!(lines[0].quantity, 2);
        assert_eq!(lines[0].arcane_tier, None);
        for (line, (template, _)) in lines[1..].iter().zip(PARTICIPATION_ARMOR_SET) {
            assert_eq!(line.item_template_id, template);
            assert_eq!(line.quantity, 2);
            assert_eq!(line.arcane_tier, Some(2));
        }
        assert_eq!(
            E.gear_rule(),
            GearCapRule {
                fill_percent: 95,
                payable_from_secs: E.closes_at_secs
            }
        );
    }

    /// Event milestones are negative, decode back to their event and sequence,
    /// and every positive milestone stays the monthly weapon's.
    #[test]
    fn event_milestones_never_meet_the_monthly_weapon() {
        for seq in [0, 1, 9, 10, 999] {
            let m = E.milestone(seq);
            assert!(m < 0);
            assert_eq!(event_of_milestone(m), Some((E, seq)));
        }
        for m in [0, 25, 50, 1_000_000] {
            assert_eq!(event_of_milestone(m), None);
        }
        assert_eq!(event_of_milestone(-25), None, "no such event ordinal");
    }

    /// The stored item reads back as one line (weapons) or a list (the gift).
    #[test]
    fn stored_items_read_back_as_lines() {
        let one = serde_json::to_value(E.weapon_gift(Uuid::nil(), 1)).unwrap();
        assert_eq!(gift_lines(&one).unwrap().len(), 1);
        let many = serde_json::to_value(E.participation_gift()).unwrap();
        assert_eq!(gift_lines(&many).unwrap().len(), 5);
    }

    /// Against Postgres: a full hour for one account — 12 human fights — pays
    /// 12 Sigil from the 5th on, the participation gift once at the 5th, weapons
    /// at the 5th and 10th, nothing for a replayed fight, and nothing at all for
    /// an account that stops at 4.
    #[tokio::test]
    async fn an_hour_of_fights_against_postgres() {
        use diesel_async::AsyncConnection;
        let Ok(url) = std::env::var("TEST_DATABASE_URL") else {
            eprintln!("SKIP: TEST_DATABASE_URL unset");
            return;
        };
        let mut conn = AsyncPgConnection::establish(&url).await.unwrap();
        conn.begin_test_transaction().await.unwrap();
        for sql in [
            "CREATE TEMP TABLE users (id UUID PRIMARY KEY)",
            "CREATE TEMP TABLE characters (id UUID PRIMARY KEY, source_alt_uuid UUID)",
            "CREATE TEMP TABLE arena_fight_gifts (id UUID PRIMARY KEY, user_id UUID NOT NULL, \
             character_id UUID NOT NULL, source_alt_uuid UUID, milestone BIGINT NOT NULL, \
             item JSONB NOT NULL, created_at BIGINT NOT NULL, granted_at BIGINT, \
             UNIQUE (user_id, milestone))",
            "CREATE TEMP TABLE arena_event_fights (event_id TEXT NOT NULL, user_id UUID NOT NULL, \
             game_session_id UUID NOT NULL, character_id UUID NOT NULL, recorded_at BIGINT NOT NULL, \
             PRIMARY KEY (event_id, user_id, game_session_id))",
            "CREATE TEMP TABLE arena_event_counts (event_id TEXT NOT NULL, user_id UUID NOT NULL, \
             fights BIGINT NOT NULL DEFAULT 0, updated_at BIGINT NOT NULL, \
             PRIMARY KEY (event_id, user_id))",
        ] {
            diesel::sql_query(sql).execute(&mut conn).await.unwrap();
        }
        let (user, character) = (Uuid::from_u128(1), Uuid::from_u128(2));
        let now = E.opens_at_secs + 60;
        let mut sigil = 0;
        let mut gifts = Vec::new();
        for n in 1..=12u128 {
            let session = Some(Uuid::from_u128(1000 + n));
            let t = record_event_fight(&mut conn, E, user, character, None, session, now)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(t.fights, n as i64);
            sigil += t.sigil;
            gifts.extend(t.gifts.iter().map(|(_, m)| *m));
            // The same fight again counts nothing and pays nothing.
            assert_eq!(
                record_event_fight(&mut conn, E, user, character, None, session, now)
                    .await
                    .unwrap(),
                None
            );
        }
        assert_eq!(sigil, 12);
        assert_eq!(gifts, [E.milestone(0), E.milestone(1), E.milestone(2)]);
        assert_eq!(account_event_fights(&mut conn, E, user).await.unwrap(), 12);

        let stopper = Uuid::from_u128(3);
        for n in 1..=4u128 {
            let t = record_event_fight(
                &mut conn,
                E,
                stopper,
                Uuid::from_u128(4),
                None,
                Some(Uuid::from_u128(2000 + n)),
                now,
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(t.sigil, 0);
            assert!(t.gifts.is_empty(), "nothing below five fights");
        }
        // The stored participation gift is the 5-line list.
        #[derive(diesel::QueryableByName)]
        struct Item {
            #[diesel(sql_type = diesel::sql_types::Jsonb)]
            item: serde_json::Value,
        }
        let row: Item = diesel::sql_query(
            "SELECT item FROM arena_fight_gifts WHERE user_id = $1 AND milestone = $2",
        )
        .bind::<diesel::sql_types::Uuid, _>(user)
        .bind::<diesel::sql_types::BigInt, _>(E.milestone(0))
        .get_result(&mut conn)
        .await
        .unwrap();
        assert_eq!(gift_lines(&row.item).unwrap().len(), 5);
    }
}
