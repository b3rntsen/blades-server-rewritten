-- #580: HauDrauf's second cups rule (`haudrauf_2500_v2`) from arena season 4 on.
-- Seasons 1-3 keep `cups_rule` (season 3 runs `haudrauf_2500` since d54), so
-- nothing already played changes; the match path resolves the rule for the
-- ACTIVE season (ranking::load_for_match).
--
-- A new config VERSION, copied from the newest one plus the schedule, so it
-- shows in the admin page's history and a revert is one more version. A
-- database with no config row (fresh/dev) gets nothing and keeps the defaults.
-- Idempotent: skipped when the newest config already carries a schedule.
INSERT INTO arena_ranking_config (id, version, config, updated_by, note)
SELECT gen_random_uuid(),
       c.version + 1,
       c.config || '{"cups_rule_from_season": [{"from_season": 4, "rule": "haudrauf_2500_v2"}]}'::jsonb,
       'migration',
       '#580: cups_rule haudrauf_2500_v2 from season 4'
FROM (SELECT version, config FROM arena_ranking_config ORDER BY version DESC LIMIT 1) c
WHERE NOT (c.config ? 'cups_rule_from_season');
