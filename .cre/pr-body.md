## Summary

Implements arena ranking v2 for the fork: versioned ranking config, configurable bot/human trophy deltas, h2h-only Elo ratings, bot opponent cup pricing, recent bot avoidance, and token-gated dev endpoints for config/history/top/rebuild.

Tests added cover contract default JSON, validation failures, the +1/-80 compatibility mode, default AI progression, h2h flat awards and daily cap, h2h Elo provisional K, rebuild ordering, avoid-recent filtering, and the 60% AI climb simulation.

## Before merging

- Apply `migrations/2026-09-27-000000-0000_add_arena_ranking_v2/up.sql` on existing boxes before deploying the binary. It creates `arena_ranking_config` and `arena_h2h_ratings`, plus supporting indexes.
- Existing installs may need the migration run manually because this repo's migrate one-shot skips once `users` exists.
- After deploy, the server uses built-in defaults until an `arena_ranking_config` row is written. Config changes are cached for up to 30 seconds.
- Use `POST /blades.bgs.services/api/dev/v1/arena-ranking/h2h-rebuild` with `{ "dryRun": false }` if the h2h board should be rebuilt from historical paired match results.

🤖 Generated with [Claude Code](https://claude.com/claude-code)
