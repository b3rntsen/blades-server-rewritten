DROP INDEX CONCURRENTLY IF EXISTS arena_match_results_h2h_rebuild_idx;
DROP INDEX IF EXISTS arena_h2h_ratings_top_idx;
DROP TABLE IF EXISTS arena_h2h_ratings;
DROP INDEX IF EXISTS arena_ranking_config_version_idx;
DROP TABLE IF EXISTS arena_ranking_config;
