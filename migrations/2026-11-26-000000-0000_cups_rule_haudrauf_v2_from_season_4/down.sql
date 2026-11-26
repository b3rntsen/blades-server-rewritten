-- #580 revert: drop the version this migration added (only if it is still the newest).
DELETE FROM arena_ranking_config
WHERE note = '#580: cups_rule haudrauf_2500_v2 from season 4'
  AND version = (SELECT max(version) FROM arena_ranking_config);
