-- Repair one deterministic arena-promotion stackable grant.
--
-- Required psql variables:
--   character_id  character UUID
--   threshold     crossed trophy threshold
--   item_uuid     stackable item-template UUID
--   quantity      positive quantity to grant
--
-- Example (run through the production Postgres container):
--   psql -X -v ON_ERROR_STOP=1 \
--     -v character_id=00000000-0000-0000-0000-000000000000 \
--     -v threshold=200 \
--     -v item_uuid=d94bab85-53d5-4c9c-a637-acd94fc66c98 \
--     -v quantity=3 \
--     -f script/repair_arena_promotion_stackable.sql
--
-- This is deliberately a one-row primary-key update, not a backfill. It takes a
-- row lock, checks the durable high-water mark, writes a server-only idempotency
-- marker, grants the item, and bumps backpackVersion in one transaction. A retry
-- returns no row and cannot grant twice, even if the player has since spent the
-- item. The short timeouts make it fail harmlessly rather than wait behind a busy
-- arena economy writer.

\if :{?character_id}
\else
  \echo 'missing -v character_id=...'
  \quit 2
\endif
\if :{?threshold}
\else
  \echo 'missing -v threshold=...'
  \quit 2
\endif
\if :{?item_uuid}
\else
  \echo 'missing -v item_uuid=...'
  \quit 2
\endif
\if :{?quantity}
\else
  \echo 'missing -v quantity=...'
  \quit 2
\endif

BEGIN;
SET LOCAL lock_timeout = '2s';
SET LOCAL statement_timeout = '5s';

WITH eligible AS MATERIALIZED (
    SELECT id, inventory, server_state
    FROM characters
    WHERE id = :'character_id'::uuid
      AND COALESCE(("character" ->> 'matchmakingPvpTrophies')::bigint, 0)
          >= :threshold::bigint
      AND :quantity::bigint > 0
      AND jsonb_typeof(
              COALESCE(inventory -> 'backpack' -> 'stackableItems', '[]'::jsonb)
          ) = 'array'
      AND jsonb_typeof(
              COALESCE(server_state -> 'arenaPromotionLootGrants', '[]'::jsonb)
          ) = 'array'
      AND NOT COALESCE(server_state -> 'arenaPromotionLootGrants', '[]'::jsonb)
              @> jsonb_build_array(:threshold::bigint)
    FOR UPDATE
), prepared AS MATERIALIZED (
    SELECT
        eligible.id,
        CASE
            WHEN EXISTS (
                SELECT 1
                FROM jsonb_array_elements(
                    COALESCE(
                        eligible.inventory -> 'backpack' -> 'stackableItems',
                        '[]'::jsonb
                    )
                ) AS existing(item)
                WHERE existing.item ->> 'itemTemplateId' = :'item_uuid'
            ) THEN (
                SELECT jsonb_agg(
                    CASE
                        WHEN existing.item ->> 'itemTemplateId' = :'item_uuid'
                        THEN jsonb_set(
                            existing.item,
                            '{count}',
                            to_jsonb(
                                COALESCE((existing.item ->> 'count')::bigint, 0)
                                    + :quantity::bigint
                            ),
                            true
                        )
                        ELSE existing.item
                    END
                    ORDER BY existing.ordinal
                )
                FROM jsonb_array_elements(
                    COALESCE(
                        eligible.inventory -> 'backpack' -> 'stackableItems',
                        '[]'::jsonb
                    )
                ) WITH ORDINALITY AS existing(item, ordinal)
            )
            ELSE COALESCE(
                    eligible.inventory -> 'backpack' -> 'stackableItems',
                    '[]'::jsonb
                ) || jsonb_build_array(
                    jsonb_build_object(
                        'itemTemplateId', :'item_uuid',
                        'count', :quantity::bigint
                    )
                )
        END AS stackable_items
    FROM eligible
), repaired AS (
    UPDATE characters AS c
    SET inventory = jsonb_set(
            jsonb_set(
                c.inventory,
                '{backpack,stackableItems}',
                prepared.stackable_items,
                true
            ),
            '{backpackVersion}',
            to_jsonb(COALESCE((c.inventory ->> 'backpackVersion')::bigint, 0) + 1),
            true
        ),
        server_state = jsonb_set(
            c.server_state,
            '{arenaPromotionLootGrants}',
            COALESCE(c.server_state -> 'arenaPromotionLootGrants', '[]'::jsonb)
                || jsonb_build_array(:threshold::bigint),
            true
        )
    FROM prepared
    WHERE c.id = prepared.id
    RETURNING c.id, c.inventory, c.server_state
)
SELECT
    repaired.id,
    repaired.inventory ->> 'backpackVersion' AS backpack_version,
    (
        SELECT item ->> 'count'
        FROM jsonb_array_elements(
            repaired.inventory -> 'backpack' -> 'stackableItems'
        ) AS granted(item)
        WHERE granted.item ->> 'itemTemplateId' = :'item_uuid'
        LIMIT 1
    ) AS item_count,
    repaired.server_state -> 'arenaPromotionLootGrants' AS recorded_thresholds
FROM repaired;

COMMIT;
