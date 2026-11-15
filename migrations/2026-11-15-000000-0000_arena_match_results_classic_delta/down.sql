-- Restore the classic deltas before dropping the column, so a rollback loses nothing.
UPDATE arena_match_results SET trophy_delta = classic_trophy_delta
 WHERE classic_trophy_delta IS NOT NULL AND trophy_delta <> classic_trophy_delta;
ALTER TABLE arena_match_results DROP COLUMN IF EXISTS classic_trophy_delta;
