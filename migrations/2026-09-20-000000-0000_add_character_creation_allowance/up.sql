-- One row per character that has been given its character-creation allowance
-- (#193, "game is defaulting player to a level 1 Argonian ... unable to rename
-- or choose race / gender due to gem requirement").
--
-- WHY THE ALLOWANCE EXISTS: in retail you chose race, sex and name during the
-- first-time user experience, for free, before any cost existed. Our FTUE is
-- patched out by design and a server-provisioned starter character compensates
-- for it — but that character is a male Argonian called "Adventurer", and the
-- only way to change any of that is the town appearance NPC, which the APK's own
-- UpdateCostData prices at 50 Gems. A fresh character has none: measured on the
-- box, 30 of the 73 characters still wearing the default Argonian visual cannot
-- afford the change. So the one free choice retail gave everybody is, here,
-- unreachable.
--
-- The allowance is exactly the appearance-change cost from
-- appearance_change_cost.json, granted once, so the player can make the choice
-- retail gave them and is left with nothing spare. It is not a gem grant: the
-- amount is read from the same table the debit reads, so the two cannot drift.
--
-- WHY A LEDGER RATHER THAN A CONDITION: "still the default Argonian and cannot
-- afford it" is self-terminating for the intended use — after the change the
-- visual differs and it never fires again — but a player who spends the gems on
-- something else and relaunches would qualify again, which is 50 Gems per
-- relaunch. The primary key is the idempotency guard, and it is in the database
-- because the login path can race itself.
--
-- WHY SEPARATE FROM schema.rs: `server/src/schema.rs` is diesel's COMPILE-TIME
-- description of the database; adding `diesel::table!` blocks creates nothing.
-- The migrate one-shot skips everything once `users` exists, so APPLY THIS BY
-- HAND on the box before shipping the binary — same caveat as add_free_for_all.
--
-- Until the table exists the backfill grant is skipped entirely (never granted
-- twice, never granted in a loop); characters created after the deploy get the
-- allowance at creation and do not depend on this table at all.
--
-- Times are BIGINT unix seconds, this schema's convention.

CREATE TABLE IF NOT EXISTS character_creation_allowance (
    character_id UUID PRIMARY KEY,
    -- What was actually granted, so a later change to the cost table leaves an
    -- honest record of what this character received.
    currency_id  UUID NOT NULL,
    amount       BIGINT NOT NULL,
    -- 'creation' for a character born with it, 'backfill' for one that existed
    -- before the allowance did. Worth keeping: the second population is finite
    -- and knowing which is which makes the next economy question answerable.
    reason       TEXT NOT NULL DEFAULT 'creation',
    granted_at   BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint
);
