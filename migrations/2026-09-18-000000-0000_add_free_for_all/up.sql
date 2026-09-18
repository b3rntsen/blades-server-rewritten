-- Arena Giveaway — the recurring "turn up and earn Gems" window — plus runtime
-- gift overrides, which are a separate capability this ships alongside it.
--
-- WHY OVERRIDES: `static_data.gifts` is loaded once from `deploy/static/gifts.json`
-- at process start, and `deploy/static` is a bind mount that merging never ships
-- (see the capture repo's CLAUDE.md). So "give everyone 100 Gems this Saturday"
-- currently means editing a file on the box by hand and restarting the arena
-- server. This table moves a gift's payload and window into the database, where
-- an endpoint — and a console — can change it without a redeploy or a restart.
--
-- WHY THE GIVEAWAY IS NOT A GIFT: retail's own banner, read off player footage
-- (youtu.be/GiJDWvCEeXE at 78-84s), says:
--
--   "Arena Giveaway — Mark your calendar - some of the mightiest competitors are
--    gathering in the Arena to battle this Saturday from 10am-12pm ET. Join them
--    during that time and earn free Gems!"   [OK]
--
-- The button is OK, not Claim. The banner advertises; it grants nothing. The
-- Gems are EARNED by playing inside a two-hour window. So the giveaway hooks the
-- arena match-end path, and `free_for_all_grants` is the once-per-character
-- ledger that makes that idempotent. The gift tables below are a separate
-- capability that happens to ship in the same change.
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
    opens_at      BIGINT NOT NULL,
    closes_at     BIGINT NOT NULL,
    gems          BIGINT NOT NULL,
    -- 1 = paid on schedule, 2 = catching up for a skipped occurrence.
    multiplier    INTEGER NOT NULL DEFAULT 1,
    cadence       TEXT NOT NULL DEFAULT 'first_saturday',
    opened_by     TEXT,
    note          TEXT,
    created_at    BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint
);

-- Two windows opening at the same instant would pay twice. The constraint is in
-- the database because the console is not the only caller: a scheduler tick and a
-- curl can both reach the same endpoint.
CREATE UNIQUE INDEX IF NOT EXISTS free_for_all_runs_one_per_occurrence
    ON free_for_all_runs (opens_at);
CREATE INDEX IF NOT EXISTS free_for_all_runs_recent
    ON free_for_all_runs (opens_at DESC);

-- Who has already earned this window's Gems. The primary key IS the idempotency
-- guard: a player fighting five matches inside the window is paid once, and the
-- insert races safely against itself because two concurrent match-end writers
-- cannot both win the same (run, character).
CREATE TABLE IF NOT EXISTS free_for_all_grants (
    run_id       UUID NOT NULL REFERENCES free_for_all_runs(id) ON DELETE CASCADE,
    character_id UUID NOT NULL,
    gems         BIGINT NOT NULL,
    granted_at   BIGINT NOT NULL DEFAULT EXTRACT(EPOCH FROM now())::bigint,
    PRIMARY KEY (run_id, character_id)
);
