-- Alts stay in their guilds (owner decision 2026-11-23).
--
-- "There should be no such things as being away. If I play another alt,
-- everything in guilds stay the same. I can manage things on web for all alts
-- that are guild masters, or I can change to another alt and manage through
-- the game."
--
-- BEFORE: switching alt parked the outgoing alt's seat inside its
-- `character_versions.server_state` (`parkedGuildMembers`), deleted the live
-- `guild_members` row, posted a LEAVE (and a PROMOTE to a successor when it was
-- the Grand Master), and posted a JOIN on the way back.
--
-- NOW: a seat whose alt is not the one being played lives here, in a real
-- table the guild reads. `guild_members` keeps meaning exactly what every
-- existing query assumes — the seat of the alt each account is PLAYING, one per
-- account — so nothing that resolves "my guild" changes. A switch moves one row
-- between the two tables and posts nothing.
--
-- WHICH ALT. `alt_key` is the alt's label (`character_versions.source_alt_uuid`,
-- or the version id for an outgoing alt that never had one). It is NOT unique
-- across accounts: a numbered copy ("Taheen 2", "HauDrauf 2") carries its
-- original's label — 17 labels on production belong to two accounts — so every
-- lookup by alt key is scoped by `user_id`.
--
-- WHICH ID THE GAME SEES. The client lists members by `userId`, names them
-- through `/social/characters`, and finds "me" by comparing a member's id with
-- its own (GuildsManager.SetCurrentGuild). The alt being PLAYED therefore keeps
-- the account's real user id; every other alt goes by its `member_id`: minted
-- once per (account, alt), random, checked against `users.id`, and never
-- reused — so it can equal neither an account nor another alt, and it stays the
-- same across any number of switches. `alt_member_ids` is that registry; it
-- also names alts that have left their guild but still author board entries.
CREATE TABLE IF NOT EXISTS alt_member_ids (
    member_id    UUID PRIMARY KEY,
    user_id      UUID NOT NULL,     -- the account (public arena user id)
    alt_key      UUID NOT NULL,
    character_id UUID,              -- the account's character row when minted
    UNIQUE (user_id, alt_key)
);
CREATE INDEX IF NOT EXISTS idx_alt_member_ids_character ON alt_member_ids(character_id);

CREATE TABLE IF NOT EXISTS guild_alt_seats (
    user_id      UUID   NOT NULL,   -- the account (public arena user id)
    alt_key      UUID   NOT NULL,
    member_id    UUID   NOT NULL UNIQUE,  -- = alt_member_ids.member_id
    guild_id     TEXT   NOT NULL,
    character_id UUID   NOT NULL,   -- the account's character row when it was parked
    rank         TEXT   NOT NULL DEFAULT 'MEMBER',
    join_date    BIGINT NOT NULL DEFAULT 0,
    parked_at    BIGINT NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, alt_key)
);
CREATE INDEX IF NOT EXISTS idx_guild_alt_seats_guild ON guild_alt_seats(guild_id);
CREATE INDEX IF NOT EXISTS idx_guild_alt_seats_character ON guild_alt_seats(character_id);

-- Who wrote a board entry and whom it is about, as (account, alt). The board
-- names users by id, and the id an alt goes by changes when it stops or starts
-- being played, so the board is re-addressed on every read from these. NULL =
-- written before this change: read as "this account's seat in this guild"
-- (see guild.rs `board_wire_sql`).
ALTER TABLE guild_messages ADD COLUMN IF NOT EXISTS alt_account    UUID;
ALTER TABLE guild_messages ADD COLUMN IF NOT EXISTS alt_key        UUID;
ALTER TABLE guild_messages ADD COLUMN IF NOT EXISTS target_account UUID;
ALTER TABLE guild_messages ADD COLUMN IF NOT EXISTS target_alt_key UUID;

-- Seats parked the old way come back into their guilds.
--
-- Source: per alt that is not live, its NEWEST version that carries the
-- `parkedGuildMembers` key at all (an older version cannot be newer news; a
-- version without the key predates parking and says nothing). A guild deleted
-- since is skipped. A parked Grand Master gets the title back only if the guild
-- has no Grand Master now (the old switch handed it to a successor, who keeps
-- it) and it is the earliest-joined such seat. The key is left in the versions:
-- nothing reads it any more, and it is the record of what was moved.
-- Idempotent: an alt already seated is skipped.
--
-- FORWARD-ONLY. Once a binary with this change has served, the board names
-- alts by member id; the previous binary would read those as non-members (the
-- #439 stall) and down.sql would drop the seats. Do not roll back by hand.
DROP TABLE IF EXISTS alt_seat_backfill;
CREATE TEMP TABLE alt_seat_backfill AS
WITH newest AS (
    SELECT DISTINCT ON (v.user_id, v.source_alt_uuid)
           v.user_id, v.source_alt_uuid AS alt_key, v.character_id, v.saved_at,
           v.server_state -> 'parkedGuildMembers' AS seats
      FROM character_versions v
     WHERE v.source_alt_uuid IS NOT NULL
       AND jsonb_typeof(v.server_state -> 'parkedGuildMembers') = 'array'
     ORDER BY v.user_id, v.source_alt_uuid, v.saved_at DESC, v.id DESC
), parked AS (
    SELECT n.alt_key, n.user_id, COALESCE(c.id, n.character_id) AS character_id,
           n.saved_at, e ->> 'guild_id' AS guild_id,
           COALESCE(e ->> 'rank', 'MEMBER') AS rank,
           COALESCE((e ->> 'join_date')::bigint, 0) AS join_date
      FROM newest n
      CROSS JOIN LATERAL jsonb_array_elements(n.seats) e
      LEFT JOIN characters c ON c.user_id = n.user_id
     WHERE NOT EXISTS (SELECT 1 FROM characters l
                        WHERE l.user_id = n.user_id AND l.source_alt_uuid = n.alt_key)
       AND NOT EXISTS (SELECT 1 FROM guild_alt_seats s
                        WHERE s.user_id = n.user_id AND s.alt_key = n.alt_key)
       AND EXISTS (SELECT 1 FROM guilds g WHERE g.id = e ->> 'guild_id')
), ranked AS (
    SELECT p.*,
           ROW_NUMBER() OVER (PARTITION BY p.guild_id, (p.rank = 'GRANDMASTER')
                              ORDER BY p.join_date, p.user_id, p.alt_key) AS gm_order
      FROM parked p
)
SELECT DISTINCT ON (r.user_id, r.alt_key)
       r.user_id, r.alt_key, r.guild_id, r.character_id,
       CASE WHEN r.rank = 'GRANDMASTER'
                 AND (r.gm_order > 1
                      OR EXISTS (SELECT 1 FROM guild_members m
                                  WHERE m.guild_id = r.guild_id AND m.rank = 'GRANDMASTER'))
            THEN 'MEMBER' ELSE r.rank END AS rank,
       r.join_date, r.saved_at
  FROM ranked r
 ORDER BY r.user_id, r.alt_key, r.join_date;

INSERT INTO alt_member_ids (member_id, user_id, alt_key, character_id)
SELECT gen_random_uuid(), b.user_id, b.alt_key, b.character_id
  FROM alt_seat_backfill b
ON CONFLICT (user_id, alt_key) DO NOTHING;

INSERT INTO guild_alt_seats
       (user_id, alt_key, member_id, guild_id, character_id, rank, join_date, parked_at)
SELECT b.user_id, b.alt_key, r.member_id, b.guild_id, b.character_id, b.rank, b.join_date,
       b.saved_at
  FROM alt_seat_backfill b
  JOIN alt_member_ids r ON r.user_id = b.user_id AND r.alt_key = b.alt_key
ON CONFLICT (user_id, alt_key) DO NOTHING;

DROP TABLE IF EXISTS alt_seat_backfill;

-- A member id must never be an account id (the client would take that member
-- for the account). gen_random_uuid() makes it astronomically unlikely; this
-- makes it impossible to ship unnoticed.
DO $$
DECLARE n bigint;
BEGIN
  IF to_regclass('users') IS NOT NULL THEN
    IF EXISTS (SELECT 1 FROM alt_member_ids r
                WHERE EXISTS (SELECT 1 FROM characters c WHERE c.user_id = r.member_id)) THEN
      RAISE EXCEPTION 'alt_member_ids: a member id equals an account id';
    END IF;
    EXECUTE 'SELECT count(*) FROM alt_member_ids r JOIN users u ON u.id = r.member_id' INTO n;
    IF n > 0 THEN
      RAISE EXCEPTION 'alt_member_ids: a member id equals a users.id';
    END IF;
  END IF;
END $$;
