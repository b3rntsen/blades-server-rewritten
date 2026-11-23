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
-- `alt_key` is the alt's own stable id (`character_versions.source_alt_uuid`,
-- or the version id for an outgoing alt that never had a label). It is also the
-- id the GAME sees for that member while the alt is not played: the client
-- lists members by `userId` and names them through `/social/characters`, which
-- resolves an alt key to that alt's newest version. The alt being played keeps
-- the account's real user id, which is what the client compares with its own id
-- to find "me" (GuildsManager.SetCurrentGuild).
CREATE TABLE IF NOT EXISTS guild_alt_seats (
    alt_key      UUID   PRIMARY KEY,
    user_id      UUID   NOT NULL,   -- the account (public arena user id)
    guild_id     TEXT   NOT NULL,
    character_id UUID   NOT NULL,   -- the account's character row when it was parked
    rank         TEXT   NOT NULL DEFAULT 'MEMBER',
    join_date    BIGINT NOT NULL DEFAULT 0,
    parked_at    BIGINT NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS idx_guild_alt_seats_guild ON guild_alt_seats(guild_id);
CREATE INDEX IF NOT EXISTS idx_guild_alt_seats_user  ON guild_alt_seats(user_id);

-- Which alt wrote a board entry, and which alt it is about (kicked, promoted,
-- approved, denied, banned, donated to). The board names users by id, and the
-- id an alt goes by changes when it stops or starts being played, so the board
-- is re-addressed on every read from these. NULL = written before this change:
-- read as "this account's seat in this guild" (see guild.rs `board_ids`).
ALTER TABLE guild_messages ADD COLUMN IF NOT EXISTS alt_key        UUID;
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
       AND NOT EXISTS (SELECT 1 FROM guild_alt_seats s WHERE s.alt_key = n.alt_key)
       AND EXISTS (SELECT 1 FROM guilds g WHERE g.id = e ->> 'guild_id')
), ranked AS (
    SELECT p.*,
           ROW_NUMBER() OVER (PARTITION BY p.guild_id, (p.rank = 'GRANDMASTER')
                              ORDER BY p.join_date, p.alt_key) AS gm_order
      FROM parked p
)
INSERT INTO guild_alt_seats (alt_key, user_id, guild_id, character_id, rank, join_date, parked_at)
SELECT DISTINCT ON (r.alt_key)
       r.alt_key, r.user_id, r.guild_id, r.character_id,
       CASE WHEN r.rank = 'GRANDMASTER'
                 AND (r.gm_order > 1
                      OR EXISTS (SELECT 1 FROM guild_members m
                                  WHERE m.guild_id = r.guild_id AND m.rank = 'GRANDMASTER'))
            THEN 'MEMBER' ELSE r.rank END,
       r.join_date, r.saved_at
  FROM ranked r
 ORDER BY r.alt_key, r.join_date
ON CONFLICT (alt_key) DO NOTHING;
