//! Pure gameplay logic for the town/RPG features, one module per feature. Each
//! `apply_*`/`can_*` function takes static definitions + the player's wallet /
//! inventory / character / server-state and returns the outcome, with no DB or HTTP
//! involved — so it is exhaustively unit-testable against captured fixtures. The
//! server handlers are thin: load → call these → persist → serialize.

pub mod challenges;
pub mod character_ops;
pub mod chests;
pub mod daily_reward;
pub mod game_events;
pub mod gifts;
pub mod global_shop;
