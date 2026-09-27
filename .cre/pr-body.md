## Summary

Implements arena ranking v2 for the fork: versioned ranking config, configurable bot/human trophy deltas, h2h-only Elo ratings, bot opponent cup pricing, recent bot avoidance, and token-gated dev endpoints for config/history/top/seasons/rebuild.

The h2h board now keeps an all-time rating plus per-season ratings. New seasons start from the configured h2h baseline while old season boards remain frozen and queryable.

Tests added cover contract default JSON, validation failures, the +1/-80 compatibility mode, default AI progression, h2h flat awards and daily cap, h2h Elo provisional K, rebuild ordering, avoid-recent filtering, season mapping/all-time replay, fresh season baselines, season selection parsing, rebuild parity, and the 60% AI climb simulation.

## Before merging

- Apply `migrations/2026-09-27-000000-0000_add_arena_ranking_v2/up.sql` on existing boxes before deploying the binary. It creates `arena_ranking_config` and the all-time `arena_h2h_ratings`, plus supporting indexes.
- Apply `migrations/2026-09-27-010000-0000_add_arena_h2h_season_ratings/up.sql` as well. It creates the per-season h2h board and a season-boundary lookup index.
- Existing installs may need the migration run manually because this repo's migrate one-shot skips once `users` exists.
- After deploy, the server uses built-in defaults until an `arena_ranking_config` row is written. Config changes are cached for up to 30 seconds.
- Use `POST /blades.bgs.services/api/dev/v1/arena-ranking/h2h-rebuild` with `{ "dryRun": false }` to rebuild both all-time and per-season h2h boards from historical paired match results.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
