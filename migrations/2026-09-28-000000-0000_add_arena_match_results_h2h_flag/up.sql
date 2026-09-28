ALTER TABLE arena_match_results
    ADD COLUMN IF NOT EXISTS is_h2h BOOLEAN NOT NULL DEFAULT false;

UPDATE arena_match_results r
SET is_h2h = true
WHERE r.is_h2h = false
  AND r.game_session_id IS NOT NULL
  AND r.opponent_character_id IS NOT NULL
  AND (
      EXISTS (
          SELECT 1
          FROM arena_matches m
          WHERE m.game_session_id = r.game_session_id
            AND m.paired = true
      )
      OR (
          (
              SELECT COUNT(*)
              FROM arena_matches m
              WHERE m.game_session_id = r.game_session_id
          ) >= 2
          AND EXISTS (
              SELECT 1
              FROM arena_match_results reciprocal
              WHERE reciprocal.game_session_id = r.game_session_id
                AND reciprocal.character_id = r.opponent_character_id
                AND reciprocal.opponent_character_id = r.character_id
          )
      )
  );

CREATE INDEX IF NOT EXISTS arena_match_results_h2h_flag_rebuild_idx
    ON arena_match_results (game_session_id, recorded_at, id)
    WHERE is_h2h = true;
