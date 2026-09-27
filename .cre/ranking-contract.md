## Arena ranking v2: shared contract (fork arena server ⇄ newblades web)

### Owner goals (2026-09-27)
1. AI opponents must be priced near the player's own trophies. Today a bot is priced at the copied character's stored `matchmakingPvpTrophies`, which gives +1 for a win and −80 for a loss. The player must also not keep getting rematched against the same AI.
2. Every human-vs-human (h2h) match awards **+50 trophies to each human**, win or lose.
3. With only about 20 players who rarely meet, a good player must be able to reach Imperial (arena 6, 2500 trophies) in about 200 matches, mostly against AI.
4. A separate **h2h-only rating** feeds a public top-100 page. AI matches move you up the arenas but never touch the h2h board.
5. An admin page describes the current algorithm and lets the owner configure it: AI pricing, Elo parameters, or simpler fixed rules.

### Config (JSON), stored versioned in Postgres
Store it in `arena_ranking_config`: id, version, config jsonb, updated_by, updated_at, note. The highest version is the active one; old versions stay as history. The server reads it at match end, cached for at most 30 s, so changes apply without a restart. It falls back to built-in defaults when no row exists or the row doesn't validate.

The defaults below are the recommendation:

```json
{
  "bot": {
    "pricing": "own",             // own | own_jitter | mimic (mimic = today's behaviour)
    "jitter": 50,                 // own_jitter: bot priced/displayed at own ± uniform(jitter)
    "mode": "fixed",              // fixed | elo
    "fixed": { "win_2_0": 32, "win_2_1": 28, "loss_1_2": -16, "loss_0_2": -20, "tie": 0 },
    "avoid_recent_opponents": 3   // skip the player's last N bot opponents when the pool allows
  },
  "human": {
    "mode": "flat",               // flat | fixed | elo
    "flat_award": 50,             // both humans get this, win or lose
    "pair_daily_cap": 3,          // flat award only for the first N h2h matches per pair per UTC day; later ones fall back to 0
    "fixed": { "win_2_0": 40, "win_2_1": 35, "loss_1_2": -10, "loss_0_2": -15, "tie": 0 }
  },
  "elo": {                        // used by any side with mode "elo"; the defaults equal today's code
    "k_table": [[0,100],[500,90],[1000,80],[1500,70],[2000,60],[2500,50]],
    "score": { "win_2_0": 1.0, "win_2_1": 0.92, "loss_1_2": 0.12, "loss_0_2": 0.0, "tie": 0.5 },
    "min_win": 1, "max_loss": -1
  },
  "h2h_rating": { "start": 1000, "k": 32, "k_provisional": 48, "provisional_games": 10, "min_games_listed": 3 }
}
```

Every mode keeps the existing rules: the `(pre + delta).max(0)` floor, and the high-water `matchmakingPvpTrophies` that makes promotion sticky.

Default math: at a 60% win rate the net is about +11 per AI match, so 2500 trophies takes about 220 matches. At 50% it takes about 420. Below about 40% the player never gets there.

### Human-only rating
All-time table `arena_h2h_ratings`: character_id pk, rating, wins, losses, ties, matches, last_match_at.
Per-season table `arena_h2h_season_ratings`: season_id + character_id pk, rating, wins, losses, ties, matches, last_match_at.
- Both are updated at match end, and only when both sides are human, which means paired and neither side a bot.
- Standard Elo on the match result, starting at `start`. K is `k_provisional` for the first `provisional_games` matches, then `k`.
- The all-time board is never reset. Each season board starts fresh at `start`.
- Match results map to a season by `arena_match_results.recorded_at` between `arena_seasons.starts_at` inclusive and `arena_seasons.ends_at` exclusive. A match outside every season counts toward all-time only.
- Season start/end/reset flows must not delete h2h rows. Past seasons stay frozen and queryable.
- It can be rebuilt from history: `arena_match_results` where an `arena_matches` row for the same `game_session_id` has `paired = true`, in `recorded_at` order.

### Dev endpoints (fork)
All require `X-Import-Token`, like the existing season routes in `server/src/admin.rs`.
- `GET  /blades.bgs.services/api/dev/v1/arena-ranking/config` returns `{version, updated_at, updated_by, note, config, defaults}`.
- `PUT  /blades.bgs.services/api/dev/v1/arena-ranking/config` takes `{config, updated_by, note}`. It validates types, ranges and enum values, and returns 400 with a `field: reason` list when validation fails. It stores a new version and returns it.
- `GET  /blades.bgs.services/api/dev/v1/arena-ranking/config/history?limit=20` returns `[{version, updated_at, updated_by, note}]`.
- `GET  /blades.bgs.services/api/dev/v1/arena-ranking/h2h-top?limit=100&season=current|all|<season_id>` returns `{season: {id, name, starts_at, ends_at} | null, rows: [{rank, character_id, name, level, rating, wins, losses, ties, matches, last_match_at}]}`. The default is `season=current`. `season=all` returns the all-time board and `season: null`. It lists only characters with `matches >= min_games_listed`, ordered by rating desc, then matches desc.
- `GET  /blades.bgs.services/api/dev/v1/arena-ranking/h2h-seasons` returns `[{id, name, starts_at, ends_at, players}]`, newest first, only for seasons that have h2h matches.
- `POST /blades.bgs.services/api/dev/v1/arena-ranking/h2h-rebuild` takes `{dry_run: bool}` and returns `{characters, matches}`. With `dry_run: false`, it rebuilds all-time plus every season board from history.
