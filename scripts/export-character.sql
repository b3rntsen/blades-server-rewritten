SELECT jsonb_pretty(jsonb_build_object(
  'userId',   c.user_id,
  'character', c.character,
  'data',      c.data,
  'inventory', c.inventory,
  'wallet',    c.wallet,
  'town',      c.town,
  'quests', COALESCE((
      SELECT jsonb_agg(q.info || jsonb_build_object('questId', q.id))
      FROM quests q WHERE q.character_id = c.id), '[]'::jsonb),
  'dungeonGeneratedDataList', COALESCE((
      SELECT jsonb_agg(q.generated_data || jsonb_build_object('questId', q.id))
      FROM quests q WHERE q.character_id = c.id AND q.generated_data IS NOT NULL), '[]'::jsonb)
))
FROM characters c
WHERE c.id = :'cid';
