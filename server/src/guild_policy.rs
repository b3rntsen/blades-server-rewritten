//! Guild ranks, guild types, and the rules that decide who may do what.
//!
//! This module is deliberately pure — no database, no actix, no clock. Everything
//! it exposes is a total function over plain values, so the permission matrix can
//! be tested exhaustively (including the negatives, which are the whole point of a
//! permission model) without standing up Postgres. `guild.rs` owns the I/O and
//! calls in here for every decision.
//!
//! # Provenance
//!
//! Nearly all of this is recovered retail data rather than invention, and the few
//! authored bits are labelled inline. `docs/guilds.md` reproduces the split.
//!
//! Three independent sources agree, which is why the confidence here is high:
//!
//! 1. **The shipped Unity asset.** `GuildData` and its nested `GuildRankData` were
//!    read out of `assets/Bundles/BuildPlayer-common.sharedAssets` (MonoBehaviour
//!    `path_id` 454) in the game APK, decoded against the field layout in
//!    `reference/il2cpp/dump.cs`. The reader consumed 13420 of 13420 bytes exactly
//!    — a wrong field order or bool alignment would have desynced by ~96 bytes —
//!    and every string field decoded to a localization key that resolves in
//!    `loc_strings_en.json`. The values are checked in at `data/guild_data.json`
//!    and the extractor at `script/extract_guild_data.py`.
//! 2. **The captured wire.** 61 428 recorded request/response pairs from the live
//!    game, of which 400-odd are guild traffic.
//! 3. **The game's own help text**, `UI.Help.Guilds.Description`, which describes
//!    the model in prose and matches (1) and (2) point for point.
//!
//! ## The headline finding: only the Grand Master has power
//!
//! `GuildData._guildRanksData` grants **every** permission flag to `GRANDMASTER`
//! and **none** to `MASTER`, `ELDER`, or `MEMBER` — all three are all-false across
//! `canEditGuild`, `canApproveGuildApplications`, `canBanNonMembers`, and every
//! row of their per-target `canKick`/`canBan` tables. `MASTER` and `ELDER` are
//! cosmetic titles that retail shipped but never wired up.
//!
//! Everything corroborates it. `UI.Help.Guilds.Description` reads: *"Joining an
//! 'Apply Only' guild requires the approval of the guild's creator, or Grand
//! Master, while joining an Open guild does not. The Grand Master also has the
//! power to set the guild to Closed (to prevent new applicants) and to remove any
//! members from the guild."* And across 1422 captured member records only two
//! ranks were ever actually assigned: `GRANDMASTER` (76) and `MEMBER` (1346) —
//! not one `MASTER`, not one `ELDER`.
//!
//! So this server implements exactly that: the Grand Master is the sole authority.
//! We keep the four-rank vocabulary because the wire, the enum and the UI strings
//! (`UI.Guild.Ranks.GrandMaster` / `.Master` / `.Edler` / `.Member` — the typo is
//! retail's) all carry it, but no endpoint here hands out a rank, because retail's
//! client has no request that does. The full guild endpoint surface in
//! `BGS.Shared.Rest.Api.BladeServer` (dump.cs:462204-462660) is apply / approve /
//! deny / ban / kick / leave / join / create / update / search / leaderboard /
//! messages / exchanges — there is no promote and no demote. Consistently,
//! `GuildRankData::CanPromote` and `CanDemote` are 8-byte stubs where the sibling
//! `CanKick` and `CanBan` are 0x128 bytes of real logic.

// ---------------------------------------------------------------------------
// Tunables — all four recovered from the shipped `GuildData` asset.
// ---------------------------------------------------------------------------

/// Maximum members in a guild. `GuildData._maxMembers`.
///
/// Corroborated twice over: across 175 guild objects in the 20260607 prod
/// snapshot `memberCount` ranges 1..=20 and never exceeds 20, and the in-game
/// help text opens "Guilds are groups of up to 20 players".
pub const MAX_MEMBERS: i64 = 20;

/// Maximum simultaneously-pending applications to one guild.
/// `GuildData._maxApplications`.
///
/// Corroborated by the client's own default guild search, which sends
/// `applicationCountMax=9` — one below the cap, exactly as its
/// `memberCountMax=19` sits one below `MAX_MEMBERS`.
pub const MAX_APPLICATIONS: i64 = 10;

/// Minimum character level required to join or apply to a guild.
/// `GuildData._minLevelToJoin`.
pub const MIN_LEVEL_TO_JOIN: u16 = 5;

/// How long a removal (kick, ban, or leaving voluntarily) blocks re-joining that
/// same guild, in seconds.
/// `GuildData._admissionTimeoutAfterRemovalFromGuildInSeconds` = 604800.0 —
/// exactly 7 days.
///
/// That such a window exists is also visible in il2cpp
/// `CanJoinGuildResult.UserRecentlyRemovedFromGuild` and in the UI string
/// `UI.Guild.Error.JoinTimeout.Body` = "You have been removed from this guild. You
/// may try to join again in {0}."
pub const REJOIN_COOLDOWN_SECS: i64 = 604_800;

/// How many message-board entries a page returns.
/// `GuildData._messageBoard._maxMessagesToDisplay`.
///
/// Strictly this is the number the CLIENT displays; retail's server-side page size
/// is not separately observable. Serving exactly what the client will show is the
/// conservative match, and no captured `guildMessageBoard` array exceeded it.
pub const MESSAGE_PAGE_LIMIT: i64 = 30;

/// Length bounds on a chat message.
/// `GuildData._messageBoard._clientTextValidation` (min 1, max 500).
pub const MESSAGE_MIN_LEN: usize = 1;
pub const MESSAGE_MAX_LEN: usize = 500;

/// Length bounds on the guild's own text fields, from `GuildData`'s three
/// `UserTextValidation` blocks (`_nameValidation`, `_shortDescriptionValidation`,
/// `_longDescriptionValidation`).
pub const NAME_MIN_LEN: usize = 3;
pub const NAME_MAX_LEN: usize = 40;
pub const SHORT_DESCRIPTION_MIN_LEN: usize = 1;
pub const SHORT_DESCRIPTION_MAX_LEN: usize = 200;
pub const LONG_DESCRIPTION_MAX_LEN: usize = 5000;

/// Separator between a guild's name and its tag, e.g. `Shadowblades#7988`.
/// `GuildData._separator`.
///
/// The server never composes that string — the client does, from the `name` and
/// `tagId` it is sent separately. Recorded here so the extracted value has a home
/// and stays under the provenance test.
#[allow(dead_code)]
pub const TAG_SEPARATOR: &str = "#";

// ---------------------------------------------------------------------------
// Ranks
// ---------------------------------------------------------------------------

/// A member's rank within their guild.
///
/// Declaration order is retail's numeric order (il2cpp `enum GuildRank`,
/// dump.cs:540696: `GRANDMASTER = 0, MASTER = 1, ELDER = 2, MEMBER = 3`), so a
/// *lower* discriminant means *more* authority. Use [`GuildRank::outranks`] rather
/// than comparing directly — the inversion is easy to get backwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum GuildRank {
    Grandmaster,
    Master,
    Elder,
    Member,
}

impl GuildRank {
    /// Every rank, most authoritative first. Exists so the permission tests can
    /// sweep the whole matrix rather than spot-check it.
    #[allow(dead_code)]
    pub const ALL: [GuildRank; 4] = [
        GuildRank::Grandmaster,
        GuildRank::Master,
        GuildRank::Elder,
        GuildRank::Member,
    ];

    /// The string retail puts in `members[].rank`.
    pub fn as_wire(self) -> &'static str {
        match self {
            GuildRank::Grandmaster => "GRANDMASTER",
            GuildRank::Master => "MASTER",
            GuildRank::Elder => "ELDER",
            GuildRank::Member => "MEMBER",
        }
    }

    /// Parse a stored/wire rank. Unknown strings are rejected rather than
    /// defaulting to `Member`: a row we cannot classify must not silently become
    /// the lowest-privilege rank and then be treated as a valid member.
    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "GRANDMASTER" => Some(GuildRank::Grandmaster),
            "MASTER" => Some(GuildRank::Master),
            "ELDER" => Some(GuildRank::Elder),
            "MEMBER" => Some(GuildRank::Member),
            _ => None,
        }
    }

    /// Strictly more authoritative than `other`. Equal ranks do NOT outrank each
    /// other — which is why, in retail's matrix, even a Grand Master cannot remove
    /// another Grand Master.
    pub fn outranks(self, other: Self) -> bool {
        (self as u8) < (other as u8)
    }
}

// ---------------------------------------------------------------------------
// Guild types
// ---------------------------------------------------------------------------

/// How a guild admits new members. il2cpp `enum GuildType` (dump.cs:540447):
/// `OPEN = 0, APPLY_ONLY = 1, CLOSED = 2`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum GuildType {
    /// Permissionless: anyone may join instantly, no approval step.
    /// *"joining an Open guild does not [require approval]"* —
    /// `UI.Help.Guilds.Description`.
    Open,
    /// Join requests only: an applicant waits for the Grand Master to approve or
    /// deny. *"Joining an 'Apply Only' guild requires the approval of the guild's
    /// creator, or Grand Master"* — ibid.
    ApplyOnly,
    /// Nobody may join or apply. *"the power to set the guild to Closed (to
    /// prevent new applicants)"* — ibid.
    Closed,
}

impl GuildType {
    /// Every type, for exhaustive tests.
    #[allow(dead_code)]
    pub const ALL: [GuildType; 3] = [GuildType::Open, GuildType::ApplyOnly, GuildType::Closed];

    pub fn as_wire(self) -> &'static str {
        match self {
            GuildType::Open => "OPEN",
            GuildType::ApplyOnly => "APPLY_ONLY",
            GuildType::Closed => "CLOSED",
        }
    }

    pub fn from_wire(s: &str) -> Option<Self> {
        match s {
            "OPEN" => Some(GuildType::Open),
            "APPLY_ONLY" => Some(GuildType::ApplyOnly),
            "CLOSED" => Some(GuildType::Closed),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The permission matrix
// ---------------------------------------------------------------------------

/// What a rank may do that does not depend on a target.
///
/// Mirrors the target-independent booleans on il2cpp `GuildRankData`
/// (dump.cs:540764).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RankPowers {
    /// Edit the guild's descriptions, banner, region, and [`GuildType`].
    /// `GuildRankData._canEditGuild`.
    pub can_edit_guild: bool,
    /// Approve or deny pending join requests.
    /// `GuildRankData._canApproveGuildApplications`.
    pub can_approve_applications: bool,
    /// May ban at all — whether a *specific* target may be banned additionally
    /// depends on the target's rank, see [`can_ban`].
    /// `GuildRankData._canBanNonMembers`.
    pub can_ban: bool,
    /// May kick at all — see [`can_kick`] for the target check.
    /// Derived from `GuildRankData._rankPermissions[*]._canKick`.
    pub can_kick: bool,
}

/// The per-rank powers, transcribed from `GuildData._guildRanksData`.
///
/// | rank        | edit | approve | kick | ban |
/// |-------------|------|---------|------|-----|
/// | GRANDMASTER | yes  | yes     | yes  | yes |
/// | MASTER      | no   | no      | no   | no  |
/// | ELDER       | no   | no      | no   | no  |
/// | MEMBER      | no   | no      | no   | no  |
///
/// This is not a simplification and not a stand-in — it is what the shipped asset
/// literally contains. See the module docs for why three ranks carry nothing.
pub fn powers(rank: GuildRank) -> RankPowers {
    match rank {
        GuildRank::Grandmaster => RankPowers {
            can_edit_guild: true,
            can_approve_applications: true,
            can_ban: true,
            can_kick: true,
        },
        // Retail grants MASTER, ELDER and MEMBER nothing whatsoever. Written out
        // per-variant rather than as a catch-all so that adding a rank forces a
        // decision here instead of silently inheriting "no powers".
        GuildRank::Master | GuildRank::Elder | GuildRank::Member => RankPowers {
            can_edit_guild: false,
            can_approve_applications: false,
            can_ban: false,
            can_kick: false,
        },
    }
}

/// May `actor` kick `target`?
///
/// Two conditions, both required: the actor's rank must carry kick authority at
/// all, and it must **strictly** outrank the target. Retail's matrix encodes the
/// strictness explicitly — `GRANDMASTER` has `canKick = true` against MASTER,
/// ELDER and MEMBER but `canKick = false` against GRANDMASTER.
pub fn can_kick(actor: GuildRank, target: GuildRank) -> bool {
    powers(actor).can_kick && actor.outranks(target)
}

/// May `actor` ban `target`? Same two-part rule as [`can_kick`], against the ban
/// power. Retail's `GRANDMASTER` row has `canBan` set exactly where `canKick` is.
pub fn can_ban(actor: GuildRank, target: GuildRank) -> bool {
    powers(actor).can_ban && actor.outranks(target)
}

/// May `actor` approve or deny a pending application?
pub fn can_approve_applications(actor: GuildRank) -> bool {
    powers(actor).can_approve_applications
}

/// May `actor` change the guild's descriptions / banner / region / type?
pub fn can_edit_guild(actor: GuildRank) -> bool {
    powers(actor).can_edit_guild
}

// ---------------------------------------------------------------------------
// Joining
// ---------------------------------------------------------------------------

/// What a successful admission attempt actually does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinAdmission {
    /// Permissionless: the user becomes a member immediately.
    Join,
    /// The user's request is recorded and awaits the Grand Master's approval.
    Apply,
}

/// Why an admission attempt was refused.
///
/// One-to-one with il2cpp `enum CanJoinGuildResult` (dump.cs:540987), minus its
/// `Success`. The discriminants are retail's, and [`JoinRefusal::error_code`]
/// reports them so a client (or a log) can tell the cases apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JoinRefusal {
    AlreadyHasGuild = 1,
    AlreadyAppliedToGuild = 2,
    GuildIsClosed = 3,
    GuildIsAtMaxMembers = 4,
    UserRecentlyRemovedFromGuild = 5,
    GuildIsAtMaxApplications = 6,
    GuildIsInvalid = 7,
    /// Not a `CanJoinGuildResult` variant — retail's client gates on
    /// `GuildData._minLevelToJoin` before it ever offers the button, so it has no
    /// refusal code for a case it believes cannot happen. The server checks anyway
    /// (a client is not a security boundary), and numbers it past retail's range
    /// so it can never be confused for one of theirs.
    BelowMinimumLevel = 100,
}

impl JoinRefusal {
    pub fn error_code(self) -> u64 {
        self as u64
    }
}

/// A prior removal of this user from this guild.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Removal {
    pub removed_at: i64,
    /// A ban never expires; a kick or a voluntary leave expires after
    /// [`REJOIN_COOLDOWN_SECS`].
    pub banned: bool,
}

impl Removal {
    /// Does this removal still block admission at `now`?
    pub fn blocks(self, now: i64) -> bool {
        self.banned || now.saturating_sub(self.removed_at) < REJOIN_COOLDOWN_SECS
    }
}

/// Everything [`evaluate_join`] needs. Assembled by the caller from the database.
#[derive(Debug, Clone, Copy)]
pub struct JoinContext {
    /// `None` when the guild does not exist.
    pub guild_type: Option<GuildType>,
    /// The applicant's character level, for the [`MIN_LEVEL_TO_JOIN`] gate.
    pub character_level: u16,
    /// The user is already in some guild (retail allows exactly one).
    pub already_in_guild: bool,
    /// The user already has a pending application somewhere.
    pub already_applied: bool,
    pub member_count: i64,
    /// Pending applications to *this* guild.
    pub application_count: i64,
    /// This user's prior removal from *this* guild, if any.
    pub removal: Option<Removal>,
    pub now: i64,
}

/// Decide what `POST /guilds/{id}/join` or `POST /guilds/{id}/apply` should do.
///
/// The check ORDER follows retail's `CanJoinGuildResult` discriminant order, so
/// that when several conditions hold at once the reported one matches what the
/// client's own pre-check would have reported.
///
/// Two judgement calls worth flagging, both MODELLED — retail's behaviour in these
/// two corners is not recoverable from any of the three sources:
///
/// 1. **A full guild still accepts applications.** Joining a full guild is refused
///    (`GuildIsAtMaxMembers`), but *applying* to one is allowed, and the fullness
///    check moves to approval time ([`evaluate_approval`]). Retail's search UI
///    treats member count and application count as independent filters, which
///    reads as "you may queue for a guild that is currently full"; and the
///    alternative — silently refusing applications to every popular guild — would
///    make `APPLY_ONLY` guilds unjoinable in practice, since popular guilds sit at
///    20.
/// 2. **A removal blocks applying as well as joining.** The UI string says "You
///    have been removed from this guild. You may try to join again in {0}" without
///    distinguishing the two paths, and a cooldown that could be walked around by
///    applying instead would not be a cooldown.
pub fn evaluate_join(ctx: JoinContext) -> Result<JoinAdmission, JoinRefusal> {
    let guild_type = match ctx.guild_type {
        Some(t) => t,
        None => return Err(JoinRefusal::GuildIsInvalid),
    };

    if ctx.already_in_guild {
        return Err(JoinRefusal::AlreadyHasGuild);
    }
    if ctx.already_applied {
        return Err(JoinRefusal::AlreadyAppliedToGuild);
    }
    // Resolve the guild's type into an admission MODE here, in the one place
    // retail's precedence puts the closed check. Collapsing it to two variants
    // now means the capacity match below has no `Closed` arm to leave dead —
    // there is exactly one rule about closed guilds, not two copies that could
    // drift apart.
    let admission = match guild_type {
        GuildType::Closed => return Err(JoinRefusal::GuildIsClosed),
        GuildType::Open => JoinAdmission::Join,
        GuildType::ApplyOnly => JoinAdmission::Apply,
    };

    if ctx.character_level < MIN_LEVEL_TO_JOIN {
        return Err(JoinRefusal::BelowMinimumLevel);
    }
    if let Some(removal) = ctx.removal {
        if removal.blocks(ctx.now) {
            return Err(JoinRefusal::UserRecentlyRemovedFromGuild);
        }
    }

    match admission {
        JoinAdmission::Join => {
            if ctx.member_count >= MAX_MEMBERS {
                return Err(JoinRefusal::GuildIsAtMaxMembers);
            }
            Ok(JoinAdmission::Join)
        }
        JoinAdmission::Apply => {
            if ctx.application_count >= MAX_APPLICATIONS {
                return Err(JoinRefusal::GuildIsAtMaxApplications);
            }
            Ok(JoinAdmission::Apply)
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalRefusal {
    NotPermitted,
    GuildIsAtMaxMembers,
}

/// Whether an approving Grand Master may actually seat this applicant right now.
///
/// Separate from [`evaluate_join`] because the guild can fill up between applying
/// and being approved — that gap is exactly why the fullness check has to run
/// again here.
pub fn evaluate_approval(actor: GuildRank, member_count: i64) -> Result<(), ApprovalRefusal> {
    if !can_approve_applications(actor) {
        return Err(ApprovalRefusal::NotPermitted);
    }
    if member_count >= MAX_MEMBERS {
        return Err(ApprovalRefusal::GuildIsAtMaxMembers);
    }
    Ok(())
}

/// Is a chat message within `GuildData._messageBoard._clientTextValidation`'s
/// length bounds? Counted in characters, not bytes — retail's validation is a
/// code-point whitelist, so its lengths are code-point counts.
///
/// Only the LENGTH bounds are enforced here. Retail also applied a 9-range
/// code-point whitelist, a 507-entry blocklist, and a six-language profanity
/// filter that rewrote `text` and preserved the original in `unfilteredText`. We
/// implement none of that: this server has no profanity list, so `text` is always
/// the player's own words and `unfilteredText` is correctly never emitted.
pub fn message_length_ok(text: &str) -> bool {
    let len = text.chars().count();
    (MESSAGE_MIN_LEN..=MESSAGE_MAX_LEN).contains(&len)
}

/// Are a guild's text fields within `GuildData`'s validation bounds?
pub fn guild_text_ok(name: &str, short_description: &str, long_description: &str) -> bool {
    // MEASURE THE TRIMMED TEXT for the fields that have a MINIMUM.
    //
    // This used to count raw characters, so "   " passed: three chars, and
    // NAME_MIN_LEN is 3. The rename handler happened to trim before calling,
    // but guild CREATION (guild.rs) passed `body.name` straight through — so a
    // guild could be created named entirely of spaces, in a file whose own
    // docblock records four prod guilds already carrying an EMPTY name.
    //
    // A validator must not depend on every caller remembering to trim. The test
    // that claimed to pin this asserted on `"   ".trim()`, which is `""`, so it
    // duplicated the empty case and never exercised whitespace at all.
    //
    // long_description has only a MAXIMUM, so it is measured raw — the stricter
    // reading, and trimming there could only ever admit more.
    let name_len = name.trim().chars().count();
    let short_len = short_description.trim().chars().count();
    let long_len = long_description.chars().count();
    (NAME_MIN_LEN..=NAME_MAX_LEN).contains(&name_len)
        && (SHORT_DESCRIPTION_MIN_LEN..=SHORT_DESCRIPTION_MAX_LEN).contains(&short_len)
        && long_len <= LONG_DESCRIPTION_MAX_LEN
}

// ---------------------------------------------------------------------------
// Succession
// ---------------------------------------------------------------------------

/// Who inherits the guild when the Grand Master walks out.
///
/// MODELLED. Retail's behaviour here is unrecoverable — no capture contains a
/// Grand Master leaving, and the client has no promotion request to observe. But
/// *something* has to happen, and it matters more here than it would in a game
/// with a broader permission matrix: since the Grand Master is the **only** holder
/// of every guild power, a guild that lost theirs with no successor would be
/// permanently frozen — unable to admit anyone, edit itself, or remove anyone,
/// forever.
///
/// The rule: the most senior surviving member inherits, ties broken by earliest
/// `join_date` — i.e. rank first, then longest service. Returns `None` when the
/// guild is now empty (the caller should delete it).
///
/// `members` is `(handle, rank, join_date)`; the handle is whatever the caller
/// wants back.
pub fn successor<T: Copy>(members: &[(T, GuildRank, i64)]) -> Option<T> {
    members
        .iter()
        .min_by_key(|(_, rank, join_date)| (*rank as u8, *join_date))
        .map(|(handle, _, _)| *handle)
}

// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ---- provenance --------------------------------------------------------

    const ASSET: &str = include_str!("../../data/guild_data.json");

    fn asset() -> serde_json::Value {
        serde_json::from_str(ASSET).expect("data/guild_data.json parses")
    }

    /// Every constant above, checked against the values actually extracted from
    /// the shipped `GuildData` asset and committed at `data/guild_data.json`.
    ///
    /// This is the test that enforces the project's "never invent a number" rule
    /// for this module. The boundary tests below are written *relative* to the
    /// constants, so they would happily keep passing if someone edited one; this
    /// one would not.
    #[test]
    fn constants_match_the_extracted_asset() {
        let d = asset();
        assert_eq!(d["max_members"].as_i64(), Some(MAX_MEMBERS));
        assert_eq!(d["max_applications"].as_i64(), Some(MAX_APPLICATIONS));
        assert_eq!(
            d["min_level_to_join"].as_i64(),
            Some(i64::from(MIN_LEVEL_TO_JOIN))
        );
        assert_eq!(
            d["admission_timeout_after_removal_from_guild_in_seconds"].as_f64(),
            Some(REJOIN_COOLDOWN_SECS as f64)
        );
        assert_eq!(d["separator"].as_str(), Some(TAG_SEPARATOR));

        let board = &d["message_board"];
        assert_eq!(
            board["max_messages_to_display"].as_i64(),
            Some(MESSAGE_PAGE_LIMIT)
        );
        let msg = &board["client_text_validation"];
        assert_eq!(msg["min_length"].as_u64(), Some(MESSAGE_MIN_LEN as u64));
        assert_eq!(msg["max_length"].as_u64(), Some(MESSAGE_MAX_LEN as u64));

        let nv = &d["name_validation"];
        assert_eq!(nv["min_length"].as_u64(), Some(NAME_MIN_LEN as u64));
        assert_eq!(nv["max_length"].as_u64(), Some(NAME_MAX_LEN as u64));
        let sv = &d["short_description_validation"];
        assert_eq!(
            sv["min_length"].as_u64(),
            Some(SHORT_DESCRIPTION_MIN_LEN as u64)
        );
        assert_eq!(
            sv["max_length"].as_u64(),
            Some(SHORT_DESCRIPTION_MAX_LEN as u64)
        );
        assert_eq!(
            d["long_description_validation"]["max_length"].as_u64(),
            Some(LONG_DESCRIPTION_MAX_LEN as u64)
        );
    }

    /// The permission matrix, read out of the committed asset rather than
    /// hand-transcribed, so a transcription slip in [`powers`] cannot hide behind
    /// a table that agrees with itself.
    #[test]
    fn permission_matrix_matches_the_extracted_asset() {
        let d = asset();
        let ranks = d["guild_ranks_data"].as_array().expect("guild_ranks_data");
        assert_eq!(ranks.len(), 4, "precondition: the asset lists all four ranks");

        for entry in ranks {
            let rank = GuildRank::from_wire(entry["guild_rank"].as_str().unwrap())
                .expect("asset rank parses");
            let p = powers(rank);
            assert_eq!(
                p.can_edit_guild,
                entry["can_edit_guild"].as_bool().unwrap(),
                "{rank:?} canEditGuild"
            );
            assert_eq!(
                p.can_approve_applications,
                entry["can_approve_guild_applications"].as_bool().unwrap(),
                "{rank:?} canApproveGuildApplications"
            );
            assert_eq!(
                p.can_ban,
                entry["can_ban_non_members"].as_bool().unwrap(),
                "{rank:?} canBanNonMembers"
            );

            let perms = entry["rank_permissions"]
                .as_array()
                .expect("rank_permissions");
            assert_eq!(perms.len(), 4, "precondition: a row per target rank");
            for row in perms {
                let target = GuildRank::from_wire(row["guild_rank"].as_str().unwrap())
                    .expect("asset target rank parses");
                assert_eq!(
                    can_kick(rank, target),
                    row["can_kick"].as_bool().unwrap(),
                    "{rank:?} canKick {target:?}"
                );
                assert_eq!(
                    can_ban(rank, target),
                    row["can_ban"].as_bool().unwrap(),
                    "{rank:?} canBan {target:?}"
                );
            }
        }
    }

    // ---- rank vocabulary ---------------------------------------------------

    #[test]
    fn rank_wire_strings_round_trip() {
        for rank in GuildRank::ALL {
            assert_eq!(GuildRank::from_wire(rank.as_wire()), Some(rank));
        }
    }

    /// The exact strings retail puts on the wire. Guards against someone
    /// "tidying" these back into the invented LEADER/OFFICER vocabulary this
    /// module replaced.
    #[test]
    fn rank_wire_strings_match_capture() {
        assert_eq!(GuildRank::Grandmaster.as_wire(), "GRANDMASTER");
        assert_eq!(GuildRank::Master.as_wire(), "MASTER");
        assert_eq!(GuildRank::Elder.as_wire(), "ELDER");
        assert_eq!(GuildRank::Member.as_wire(), "MEMBER");
        assert_eq!(GuildRank::from_wire("LEADER"), None);
        assert_eq!(GuildRank::from_wire("OFFICER"), None);
    }

    #[test]
    fn unknown_rank_does_not_default_to_member() {
        assert_eq!(GuildRank::from_wire(""), None);
        assert_eq!(GuildRank::from_wire("member"), None);
        assert_eq!(GuildRank::from_wire("ADMIN"), None);
    }

    #[test]
    fn outranking_is_strict_and_antisymmetric() {
        for a in GuildRank::ALL {
            assert!(!a.outranks(a), "{a:?} must not outrank itself");
            for b in GuildRank::ALL {
                if a != b {
                    assert_ne!(
                        a.outranks(b),
                        b.outranks(a),
                        "exactly one of {a:?}/{b:?} must outrank the other"
                    );
                }
            }
        }
        assert!(GuildRank::Grandmaster.outranks(GuildRank::Master));
        assert!(GuildRank::Master.outranks(GuildRank::Elder));
        assert!(GuildRank::Elder.outranks(GuildRank::Member));
    }

    #[test]
    fn guild_type_wire_strings_round_trip() {
        for t in GuildType::ALL {
            assert_eq!(GuildType::from_wire(t.as_wire()), Some(t));
        }
        assert_eq!(GuildType::Open.as_wire(), "OPEN");
        assert_eq!(GuildType::ApplyOnly.as_wire(), "APPLY_ONLY");
        assert_eq!(GuildType::Closed.as_wire(), "CLOSED");
        // The UI offers an "Invite only" label (UI.Guild.Create.GuildType.InviteOnly)
        // but `enum GuildType` has no such member and it never appears on the wire.
        assert_eq!(GuildType::from_wire("INVITE_ONLY"), None);
    }

    // ---- the permission matrix, against the shipped asset -------------------

    /// Transcription check against `data/guild_data.json` — the whole matrix, cell
    /// by cell, in the same shape the asset stores it. If someone "improves" the
    /// model by handing MASTER or ELDER some authority, this is what stops them.
    #[test]
    fn permission_matrix_matches_the_shipped_asset() {
        // GuildData._guildRanksData, in order.
        let expected: [(GuildRank, bool, bool, bool, [bool; 4]); 4] = [
            // rank, canEditGuild, canApproveApplications, canBanNonMembers,
            // canKick vs [GRANDMASTER, MASTER, ELDER, MEMBER]
            (
                GuildRank::Grandmaster,
                true,
                true,
                true,
                [false, true, true, true],
            ),
            (GuildRank::Master, false, false, false, [false; 4]),
            (GuildRank::Elder, false, false, false, [false; 4]),
            (GuildRank::Member, false, false, false, [false; 4]),
        ];

        for (rank, edit, approve, ban_flag, kick_row) in expected {
            let p = powers(rank);
            assert_eq!(p.can_edit_guild, edit, "{rank:?} canEditGuild");
            assert_eq!(
                p.can_approve_applications, approve,
                "{rank:?} canApproveGuildApplications"
            );
            assert_eq!(p.can_ban, ban_flag, "{rank:?} canBanNonMembers");
            for (target, expected_kick) in GuildRank::ALL.iter().zip(kick_row) {
                assert_eq!(
                    can_kick(rank, *target),
                    expected_kick,
                    "{rank:?} canKick {target:?}"
                );
                // The asset sets canBan on exactly the same cells as canKick.
                assert_eq!(
                    can_ban(rank, *target),
                    expected_kick,
                    "{rank:?} canBan {target:?}"
                );
            }
        }
    }

    // ---- the negatives: who may NOT do what --------------------------------
    //
    // A permission model tested only on its positives is untested. These are the
    // tests that matter.

    #[test]
    fn a_plain_member_can_do_nothing_to_anybody() {
        let p = powers(GuildRank::Member);
        assert!(!p.can_edit_guild);
        assert!(!p.can_approve_applications);
        assert!(!p.can_kick);
        assert!(!p.can_ban);
        for target in GuildRank::ALL {
            assert!(
                !can_kick(GuildRank::Member, target),
                "a MEMBER must not kick a {target:?}"
            );
            assert!(
                !can_ban(GuildRank::Member, target),
                "a MEMBER must not ban a {target:?}"
            );
        }
        assert!(!can_approve_applications(GuildRank::Member));
        assert!(!can_edit_guild(GuildRank::Member));
    }

    /// The finding that most distinguishes retail from the obvious guess: MASTER
    /// and ELDER are titles, not roles.
    #[test]
    fn master_and_elder_are_cosmetic_and_hold_no_authority() {
        for actor in [GuildRank::Master, GuildRank::Elder] {
            assert!(!can_edit_guild(actor), "{actor:?} must not edit the guild");
            assert!(
                !can_approve_applications(actor),
                "{actor:?} must not approve applications"
            );
            for target in GuildRank::ALL {
                assert!(
                    !can_kick(actor, target),
                    "a {actor:?} must not kick a {target:?}"
                );
                assert!(
                    !can_ban(actor, target),
                    "a {actor:?} must not ban a {target:?}"
                );
            }
        }
    }

    #[test]
    fn nobody_can_kick_or_ban_a_grandmaster() {
        for actor in GuildRank::ALL {
            assert!(
                !can_kick(actor, GuildRank::Grandmaster),
                "a {actor:?} must not kick the GRANDMASTER"
            );
            assert!(
                !can_ban(actor, GuildRank::Grandmaster),
                "a {actor:?} must not ban the GRANDMASTER"
            );
        }
    }

    #[test]
    fn equal_ranks_cannot_remove_each_other() {
        for rank in GuildRank::ALL {
            assert!(
                !can_kick(rank, rank),
                "a {rank:?} must not kick another {rank:?}"
            );
            assert!(
                !can_ban(rank, rank),
                "a {rank:?} must not ban another {rank:?}"
            );
        }
    }

    #[test]
    fn removal_always_requires_strictly_outranking_the_target() {
        for actor in GuildRank::ALL {
            for target in GuildRank::ALL {
                if can_kick(actor, target) || can_ban(actor, target) {
                    assert!(
                        actor.outranks(target),
                        "{actor:?} was allowed to remove {target:?} without outranking it"
                    );
                }
            }
        }
    }

    #[test]
    fn the_grandmaster_may_remove_every_other_rank() {
        assert!(can_kick(GuildRank::Grandmaster, GuildRank::Master));
        assert!(can_kick(GuildRank::Grandmaster, GuildRank::Elder));
        assert!(can_kick(GuildRank::Grandmaster, GuildRank::Member));
        assert!(can_ban(GuildRank::Grandmaster, GuildRank::Master));
        assert!(can_ban(GuildRank::Grandmaster, GuildRank::Elder));
        assert!(can_ban(GuildRank::Grandmaster, GuildRank::Member));
        assert!(can_edit_guild(GuildRank::Grandmaster));
        assert!(can_approve_applications(GuildRank::Grandmaster));
    }

    #[test]
    fn exactly_one_rank_holds_power() {
        let with_power: Vec<_> = GuildRank::ALL
            .into_iter()
            .filter(|r| {
                let p = powers(*r);
                p.can_edit_guild || p.can_approve_applications || p.can_kick || p.can_ban
            })
            .collect();
        assert_eq!(with_power, vec![GuildRank::Grandmaster]);
    }

    // ---- joining -----------------------------------------------------------

    fn ctx(guild_type: GuildType) -> JoinContext {
        JoinContext {
            guild_type: Some(guild_type),
            character_level: MIN_LEVEL_TO_JOIN,
            already_in_guild: false,
            already_applied: false,
            member_count: 1,
            application_count: 0,
            removal: None,
            now: 1_000_000,
        }
    }

    #[test]
    fn an_open_guild_is_permissionless() {
        assert_eq!(evaluate_join(ctx(GuildType::Open)), Ok(JoinAdmission::Join));
    }

    #[test]
    fn an_apply_only_guild_produces_a_request_not_a_membership() {
        assert_eq!(
            evaluate_join(ctx(GuildType::ApplyOnly)),
            Ok(JoinAdmission::Apply)
        );
    }

    #[test]
    fn a_closed_guild_admits_nobody_by_either_path() {
        assert_eq!(
            evaluate_join(ctx(GuildType::Closed)),
            Err(JoinRefusal::GuildIsClosed)
        );
    }

    /// Refusals are reported in retail's `CanJoinGuildResult` order, so a caller
    /// who trips several conditions at once hears the same one retail's client
    /// would have pre-checked.
    ///
    /// This also pins WHICH of the two closed-guild checks fires. `evaluate_join`
    /// rejects `Closed` early and again in its final match; without the early one
    /// the guild is still unjoinable, but a below-level applicant to a closed
    /// guild would be told they are under-levelled rather than that the guild is
    /// shut — and would keep being refused after levelling up.
    #[test]
    fn refusals_are_reported_in_retails_precedence_order() {
        // Every later condition also failing, so only precedence decides.
        let all_bad = |t: GuildType| JoinContext {
            guild_type: Some(t),
            character_level: 0,
            already_in_guild: false,
            already_applied: false,
            member_count: MAX_MEMBERS,
            application_count: MAX_APPLICATIONS,
            removal: Some(Removal {
                removed_at: 1_000_000,
                banned: true,
            }),
            now: 1_000_000,
        };

        // GuildIsClosed (3) beats BelowMinimumLevel, the caps, and the cooldown.
        assert_eq!(
            evaluate_join(all_bad(GuildType::Closed)),
            Err(JoinRefusal::GuildIsClosed)
        );

        // AlreadyHasGuild (1) beats everything, including being closed.
        let mut c = all_bad(GuildType::Closed);
        c.already_in_guild = true;
        assert_eq!(evaluate_join(c), Err(JoinRefusal::AlreadyHasGuild));

        // AlreadyAppliedToGuild (2) beats GuildIsClosed (3).
        let mut c = all_bad(GuildType::Closed);
        c.already_applied = true;
        assert_eq!(evaluate_join(c), Err(JoinRefusal::AlreadyAppliedToGuild));

        // GuildIsInvalid outranks the lot: there is nothing to reason about.
        let mut c = all_bad(GuildType::Closed);
        c.guild_type = None;
        c.already_in_guild = true;
        assert_eq!(evaluate_join(c), Err(JoinRefusal::GuildIsInvalid));

        // The level gate is checked before the cooldown and the caps.
        assert_eq!(
            evaluate_join(all_bad(GuildType::Open)),
            Err(JoinRefusal::BelowMinimumLevel)
        );

        // ...and with the level satisfied, the cooldown is heard before the caps.
        let mut c = all_bad(GuildType::Open);
        c.character_level = MIN_LEVEL_TO_JOIN;
        assert_eq!(
            evaluate_join(c),
            Err(JoinRefusal::UserRecentlyRemovedFromGuild)
        );
    }

    #[test]
    fn a_missing_guild_is_invalid_not_a_crash() {
        let mut c = ctx(GuildType::Open);
        c.guild_type = None;
        assert_eq!(evaluate_join(c), Err(JoinRefusal::GuildIsInvalid));
    }

    #[test]
    fn you_cannot_join_a_second_guild() {
        for t in GuildType::ALL {
            let mut c = ctx(t);
            c.already_in_guild = true;
            assert_eq!(evaluate_join(c), Err(JoinRefusal::AlreadyHasGuild));
        }
    }

    #[test]
    fn you_cannot_hold_two_applications() {
        let mut c = ctx(GuildType::ApplyOnly);
        c.already_applied = true;
        assert_eq!(evaluate_join(c), Err(JoinRefusal::AlreadyAppliedToGuild));
    }

    #[test]
    fn a_character_below_the_minimum_level_cannot_join_or_apply() {
        for t in [GuildType::Open, GuildType::ApplyOnly] {
            let mut c = ctx(t);
            c.character_level = MIN_LEVEL_TO_JOIN - 1;
            assert_eq!(evaluate_join(c), Err(JoinRefusal::BelowMinimumLevel));
            // ...and exactly at the minimum, they can.
            c.character_level = MIN_LEVEL_TO_JOIN;
            assert!(evaluate_join(c).is_ok());
        }
    }

    #[test]
    fn a_full_open_guild_refuses_new_members() {
        let mut c = ctx(GuildType::Open);
        c.member_count = MAX_MEMBERS;
        assert_eq!(evaluate_join(c), Err(JoinRefusal::GuildIsAtMaxMembers));
        // One under the cap still admits — proves the boundary, not just the
        // refusal.
        c.member_count = MAX_MEMBERS - 1;
        assert_eq!(evaluate_join(c), Ok(JoinAdmission::Join));
    }

    #[test]
    fn a_full_apply_only_guild_still_accepts_applications() {
        // MODELLED behaviour — see evaluate_join's docs. Asserted so that changing
        // the model is a deliberate act with a failing test attached.
        let mut c = ctx(GuildType::ApplyOnly);
        c.member_count = MAX_MEMBERS;
        assert_eq!(evaluate_join(c), Ok(JoinAdmission::Apply));
    }

    #[test]
    fn an_application_backlog_at_the_cap_refuses_more() {
        let mut c = ctx(GuildType::ApplyOnly);
        c.application_count = MAX_APPLICATIONS;
        assert_eq!(evaluate_join(c), Err(JoinRefusal::GuildIsAtMaxApplications));
        c.application_count = MAX_APPLICATIONS - 1;
        assert_eq!(evaluate_join(c), Ok(JoinAdmission::Apply));
    }

    #[test]
    fn a_recent_kick_blocks_rejoining_until_the_cooldown_lapses() {
        let now = 10_000_000;
        let mut c = ctx(GuildType::Open);
        c.now = now;
        c.removal = Some(Removal {
            removed_at: now - 1,
            banned: false,
        });
        assert_eq!(
            evaluate_join(c),
            Err(JoinRefusal::UserRecentlyRemovedFromGuild)
        );

        // One second short of seven days: still blocked.
        c.removal = Some(Removal {
            removed_at: now - (REJOIN_COOLDOWN_SECS - 1),
            banned: false,
        });
        assert_eq!(
            evaluate_join(c),
            Err(JoinRefusal::UserRecentlyRemovedFromGuild)
        );

        // Exactly at the boundary the cooldown is over.
        c.removal = Some(Removal {
            removed_at: now - REJOIN_COOLDOWN_SECS,
            banned: false,
        });
        assert_eq!(evaluate_join(c), Ok(JoinAdmission::Join));
    }

    #[test]
    fn the_rejoin_cooldown_is_the_assets_seven_days() {
        assert_eq!(REJOIN_COOLDOWN_SECS, 7 * 24 * 60 * 60);
    }

    #[test]
    fn a_ban_never_lapses() {
        let now = 10_000_000;
        let mut c = ctx(GuildType::Open);
        c.now = now;
        c.removal = Some(Removal {
            removed_at: now - REJOIN_COOLDOWN_SECS * 1_000,
            banned: true,
        });
        assert_eq!(
            evaluate_join(c),
            Err(JoinRefusal::UserRecentlyRemovedFromGuild)
        );
    }

    #[test]
    fn a_removal_blocks_applying_as_well_as_joining() {
        let now = 10_000_000;
        let mut c = ctx(GuildType::ApplyOnly);
        c.now = now;
        c.removal = Some(Removal {
            removed_at: now - 1,
            banned: false,
        });
        assert_eq!(
            evaluate_join(c),
            Err(JoinRefusal::UserRecentlyRemovedFromGuild)
        );
    }

    #[test]
    fn refusal_codes_match_retail_can_join_guild_result_ordinals() {
        // il2cpp CanJoinGuildResult: Success=0, AlreadyHasGuild=1,
        // AlreadyAppliedToGuild=2, GuildIsClosed=3, GuildIsAtMaxMembers=4,
        // UserRecentlyRemovedFromGuild=5, GuildIsAtMaxApplications=6,
        // GuildIsInvalid=7.
        assert_eq!(JoinRefusal::AlreadyHasGuild.error_code(), 1);
        assert_eq!(JoinRefusal::AlreadyAppliedToGuild.error_code(), 2);
        assert_eq!(JoinRefusal::GuildIsClosed.error_code(), 3);
        assert_eq!(JoinRefusal::GuildIsAtMaxMembers.error_code(), 4);
        assert_eq!(JoinRefusal::UserRecentlyRemovedFromGuild.error_code(), 5);
        assert_eq!(JoinRefusal::GuildIsAtMaxApplications.error_code(), 6);
        assert_eq!(JoinRefusal::GuildIsInvalid.error_code(), 7);
        // Ours, deliberately outside retail's range.
        assert_eq!(JoinRefusal::BelowMinimumLevel.error_code(), 100);
    }

    // ---- approval ----------------------------------------------------------

    #[test]
    fn only_the_grandmaster_can_approve_an_application() {
        assert_eq!(evaluate_approval(GuildRank::Grandmaster, 1), Ok(()));
        for actor in [GuildRank::Master, GuildRank::Elder, GuildRank::Member] {
            assert_eq!(
                evaluate_approval(actor, 1),
                Err(ApprovalRefusal::NotPermitted),
                "a {actor:?} must not approve an application"
            );
        }
    }

    #[test]
    fn approval_stops_at_the_member_cap() {
        assert_eq!(
            evaluate_approval(GuildRank::Grandmaster, MAX_MEMBERS - 1),
            Ok(())
        );
        assert_eq!(
            evaluate_approval(GuildRank::Grandmaster, MAX_MEMBERS),
            Err(ApprovalRefusal::GuildIsAtMaxMembers)
        );
    }

    #[test]
    fn permission_is_checked_before_capacity() {
        // A MEMBER attacking a full guild must be told it is not their call, not
        // that the guild is full — otherwise the refusal leaks the wrong reason
        // and, worse, would start succeeding if a slot opened up.
        assert_eq!(
            evaluate_approval(GuildRank::Member, MAX_MEMBERS),
            Err(ApprovalRefusal::NotPermitted)
        );
    }

    // ---- text validation ---------------------------------------------------

    #[test]
    fn chat_messages_respect_the_assets_length_bounds() {
        assert!(!message_length_ok(""), "empty message must be rejected");
        assert!(message_length_ok("a"));
        assert!(message_length_ok(&"x".repeat(MESSAGE_MAX_LEN)));
        assert!(!message_length_ok(&"x".repeat(MESSAGE_MAX_LEN + 1)));
        assert_eq!(MESSAGE_MAX_LEN, 500);
    }

    #[test]
    fn message_length_counts_characters_not_bytes() {
        // 500 multi-byte characters is 500 characters, not 1500 bytes' worth of
        // rejection. Retail validates against a code-point whitelist, so its
        // bounds are code-point counts.
        let msg = "é".repeat(MESSAGE_MAX_LEN);
        assert!(msg.len() > MESSAGE_MAX_LEN, "precondition: this is multi-byte");
        assert!(message_length_ok(&msg));
        assert!(!message_length_ok(&"é".repeat(MESSAGE_MAX_LEN + 1)));
    }

    #[test]
    fn guild_text_respects_the_assets_length_bounds() {
        assert!(guild_text_ok("abc", "s", ""));
        assert!(!guild_text_ok("ab", "s", ""), "name below 3 must be rejected");
        assert!(
            !guild_text_ok(&"x".repeat(NAME_MAX_LEN + 1), "s", ""),
            "name above 40 must be rejected"
        );
        assert!(
            !guild_text_ok("abc", "", ""),
            "empty short description must be rejected"
        );
        assert!(!guild_text_ok(
            "abc",
            &"x".repeat(SHORT_DESCRIPTION_MAX_LEN + 1),
            ""
        ));
        assert!(guild_text_ok("abc", "s", &"x".repeat(LONG_DESCRIPTION_MAX_LEN)));
        assert!(!guild_text_ok(
            "abc",
            "s",
            &"x".repeat(LONG_DESCRIPTION_MAX_LEN + 1)
        ));
    }

    // ---- succession --------------------------------------------------------

    #[test]
    fn succession_prefers_rank_then_seniority() {
        let members = [
            (10u32, GuildRank::Member, 100),
            (11, GuildRank::Elder, 500),
            (12, GuildRank::Master, 900),
            (13, GuildRank::Master, 400),
        ];
        // Master beats Elder and Member; between the two Masters the earlier
        // join_date wins.
        assert_eq!(successor(&members), Some(13));
    }

    #[test]
    fn succession_falls_through_to_the_longest_serving_member() {
        let members = [
            (1u32, GuildRank::Member, 900),
            (2, GuildRank::Member, 300),
            (3, GuildRank::Member, 700),
        ];
        assert_eq!(successor(&members), Some(2));
    }

    #[test]
    fn an_empty_guild_has_no_successor() {
        let members: [(u32, GuildRank, i64); 0] = [];
        assert_eq!(successor(&members), None);
    }

    /// The four nameless guilds on prod exist because an empty name was once
    /// storable. `guild_text_ok` is what stops that recurring, so pin it —
    /// including the whitespace case, since a rename now trims before checking
    /// and " " must not become a legal name.
    #[test]
    fn a_guild_cannot_be_named_nothing() {
        assert!(!guild_text_ok("", "a short one", ""), "empty name accepted");
        // NOT `"   ".trim()`. That is `""`, so the assertion duplicated the line
        // above and tested nothing — while the docblock claimed it pinned the
        // whitespace case. Pass the spaces through as the handler receives them.
        assert!(
            !guild_text_ok("   ", "a short one", ""),
            "all-whitespace name accepted"
        );
        assert!(
            !guild_text_ok("\t \n", "a short one", ""),
            "tabs/newlines name accepted"
        );
        // And pin the sequence the handler actually performs (guild.rs: trim,
        // then validate), so a rename cannot smuggle a blank name past either
        // half on its own.
        for raw in ["   ", "\t", " \n ", ""] {
            assert!(
                !guild_text_ok(raw.trim(), "a short one", ""),
                "trim-then-validate accepted {raw:?}"
            );
        }
        assert!(guild_text_ok("Bladeworks", "a short one", ""), "a real name was rejected");
    }
}
