DROP INDEX IF EXISTS arena_match_results_h2h_flag_rebuild_idx;
ALTER TABLE arena_match_results DROP COLUMN IF EXISTS is_h2h;
