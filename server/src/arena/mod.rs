use uuid::Uuid;

pub mod avatar;
pub mod enet_channel;
pub mod leaderboards;
pub mod matchmaking;

pub enum MatchmakingMessage {
    InitiateMatchmaking { ticket_id: Uuid },
}
