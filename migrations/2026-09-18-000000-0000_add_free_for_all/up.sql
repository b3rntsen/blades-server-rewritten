-- Free for All — the recurring gem giveaway, plus the runtime gift overrides it
-- needs to exist at all.
--
-- WHY OVERRIDES: `static_data.gifts` is loaded once from `deploy/static/gifts.json`
-- at process start, and `deploy/static` is a bind mount that merging never ships
-- (see the capture repo's CLAUDE.md). So "give everyone 100 Gems this Saturday"
-- currently means editing a file on the box by hand and restarting the arena
-- server. This table moves a gift's payload and window into the database, where
-- an endpoint — and a console — can change it without a redeploy or a restart.
--
-- WHY A NEW GIFT ID PER RUN: the client discovers gift ids at runtime, from
-- `/announcements` — an entry's assetUrl resolves to a manifest carrying a
-- GlobalGiftId, and the Claim button posts to that. Retail leaned on this hard:
-- captured characters carry 311 distinct claimed gift ids between them. Claim
-- counts are permanent per (character, gift), so minting a fresh id each month
-- gives everyone exactly one claim, where reusing an id would need its limit
-- raised every time and would hand a brand-new player a claim they never earned.
--
-- WHY SEPARATE FROM schema.rs: `server/src/schema.rs` is diesel's COMPILE-TIME
-- description of the database; adding `diesel::table!` blocks creates nothing.
-- The migrate one-shot skips everything once `users` exists, so APPLY THIS BY
-- HAND on the box before shipping the binary — same caveat as add_arena_seasons.
--
-- Times are BIGINT unix seconds, this schema's convention.

CREATE TABLE IF NOT EXISTS gift_overrides (
    gift_id            UUID PRIMARY KEY,
    -- `[{"itemTemplateId": "<uuid>", "quantity": <n>}, …]` — the GiftDef.items
    -- shape verbatim, so the override deserializes with the same struct the
    -- static file uses and no second format can drift from it.
    items              JSONB NOT NULL DEFAULT '[]'::jsonb,
    -- `[{…GiftChest…}, …]`. Present so an override can represent ANY static gift
    -- losslessly: a gift whose payload is chests would otherwise come back from
    -- the override with its chests silently dropped.
    chests             JSONB NOT NULL DEFAULT '[]'::jsonb,
    start_time         BIGINT NOT NULL DEFAULT 0,
    end_time           BIGINT NOT NULL DEFAULT 0,
    claim_count_limit  BIGINT NOT NULL DEFAULT 1,
    description        TEXT,
    updated_at         BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint,
    -- Who last changed it. The web app logs its own admin actions, but the arena
    -- server is not its only caller and a grant to every player should carry an
    -- author on the row itself.
    updated_by         TEXT
);

-- One row per giveaway that was actually opened. This is the ledger the doubling
-- rule reads: "was one skipped" is derived from the gaps between these rows, not
-- from a flag somebody has to remember to set.
CREATE TABLE IF NOT EXISTS free_for_all_runs (
    id            UUID PRIMARY KEY,
    gift_id       UUID NOT NULL,
    opens_at      BIGINT NOT NULL,
    closes_at     BIGINT NOT NULL,
    gems          BIGINT NOT NULL,
    -- 1 = paid on schedule, 2 = catching up for a skipped occurrence.
    multiplier    INTEGER NOT NULL DEFAULT 1,
    -- The claim limit this run wrote onto the gift, kept so a re-run can tell
    -- whether it already bumped the counter.
    claim_limit   BIGINT NOT NULL,
    cadence       TEXT NOT NULL DEFAULT 'first_saturday',
    opened_by     TEXT,
    note          TEXT,
    created_at    BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint
);

-- Two runs on the same day would double-bump the claim limit and pay twice. The
-- constraint is in the database because the console is not the only caller: the
-- scheduler tick and a curl can both reach the same endpoint.
CREATE UNIQUE INDEX IF NOT EXISTS free_for_all_runs_one_per_occurrence
    ON free_for_all_runs (gift_id, opens_at);
CREATE INDEX IF NOT EXISTS free_for_all_runs_recent
    ON free_for_all_runs (opens_at DESC);
