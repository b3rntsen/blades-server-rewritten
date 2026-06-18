pub mod avatar;
pub mod combat;
pub mod config;
pub mod enet_host;
pub mod key_submit;
pub mod leaderboards;
pub mod match_registry;
pub mod matchmaker;
pub mod matchmaking;
pub mod udp;

use serde::Serialize;
use uuid::Uuid;

/// Messages the matchmaker pushes to a waiting session's RMS WebSocket. Each
/// maps to one **binary** frame carrying the JSON envelope
/// `{"messageType":"matchmaking","payload":{...}}`. Shapes confirmed against
/// captured prod frames (all payload keys present; null until resolved).
#[derive(Debug, Clone)]
pub enum MatchmakingMessage {
    Searching {
        ticket_id: Uuid,
    },
    PotentialMatch {
        ticket_id: Uuid,
    },
    Succeeded {
        ticket_id: Uuid,
        player_session_id: String, // GameLift-style "psess-<uuid>"
        game_session_id: Uuid,
        address: String, // arena UDP host the client dials
        port: u16,
    },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RmsEnvelope<'a> {
    message_type: &'a str,
    payload: RmsPayload<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RmsPayload<'a> {
    ticket_id: Uuid,
    // Option fields serialize to explicit `null` (key always present) — matches
    // the captured Searching/PotentialMatch frames.
    player_session_id: Option<&'a str>,
    ticket_status: &'a str,
    game_session_id: Option<Uuid>,
    address: Option<&'a str>,
    port: Option<u16>,
}

impl MatchmakingMessage {
    pub fn ticket_id(&self) -> Uuid {
        match self {
            MatchmakingMessage::Searching { ticket_id }
            | MatchmakingMessage::PotentialMatch { ticket_id }
            | MatchmakingMessage::Succeeded { ticket_id, .. } => *ticket_id,
        }
    }

    /// Serialize to the exact captured RMS frame JSON (the binary WS payload).
    pub fn to_rms_json(&self) -> Vec<u8> {
        let payload = match self {
            MatchmakingMessage::Searching { ticket_id } => RmsPayload {
                ticket_id: *ticket_id,
                player_session_id: None,
                ticket_status: "MatchmakingSearching",
                game_session_id: None,
                address: None,
                port: None,
            },
            MatchmakingMessage::PotentialMatch { ticket_id } => RmsPayload {
                ticket_id: *ticket_id,
                player_session_id: None,
                ticket_status: "PotentialMatchCreated",
                game_session_id: None,
                address: None,
                port: None,
            },
            MatchmakingMessage::Succeeded {
                ticket_id,
                player_session_id,
                game_session_id,
                address,
                port,
            } => RmsPayload {
                ticket_id: *ticket_id,
                player_session_id: Some(player_session_id.as_str()),
                ticket_status: "MatchmakingSucceeded",
                game_session_id: Some(*game_session_id),
                address: Some(address.as_str()),
                port: Some(*port),
            },
        };
        serde_json::to_vec(&RmsEnvelope {
            message_type: "matchmaking",
            payload,
        })
        .expect("serialize RMS frame")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    use uuid::Uuid;

    fn parsed(msg: &MatchmakingMessage) -> Value {
        serde_json::from_slice(&msg.to_rms_json()).unwrap()
    }

    /// Reproduces a real captured `MatchmakingSucceeded` frame (prod ws id 4325)
    /// byte-shape exactly.
    #[test]
    fn succeeded_matches_captured_frame() {
        let msg = MatchmakingMessage::Succeeded {
            ticket_id: Uuid::parse_str("bb3b794d-2bf3-48eb-9c4b-67d696caccd0").unwrap(),
            player_session_id: "psess-ffa97bdf-7982-a309-6599-05c28c5ed98e".to_string(),
            game_session_id: Uuid::parse_str("7d4109bc-cde8-4068-997d-38bf065bd876").unwrap(),
            address: "3.78.254.65".to_string(),
            port: 5075,
        };
        let expected: Value = serde_json::from_str(
            r#"{"messageType":"matchmaking","payload":{"ticketId":"bb3b794d-2bf3-48eb-9c4b-67d696caccd0","playerSessionId":"psess-ffa97bdf-7982-a309-6599-05c28c5ed98e","ticketStatus":"MatchmakingSucceeded","gameSessionId":"7d4109bc-cde8-4068-997d-38bf065bd876","address":"3.78.254.65","port":5075}}"#,
        )
        .unwrap();
        assert_eq!(parsed(&msg), expected);
    }

    /// Searching/PotentialMatch carry every resolution key, all null (matches
    /// the captured frames).
    #[test]
    fn searching_has_all_null_payload_keys() {
        let tid = Uuid::parse_str("08029ef6-a37b-475a-b4cd-325c8186838d").unwrap();
        let v = parsed(&MatchmakingMessage::Searching { ticket_id: tid });
        assert_eq!(v["messageType"], json!("matchmaking"));
        let p = &v["payload"];
        assert_eq!(p["ticketStatus"], json!("MatchmakingSearching"));
        assert_eq!(p["ticketId"], json!("08029ef6-a37b-475a-b4cd-325c8186838d"));
        for k in ["playerSessionId", "gameSessionId", "address", "port"] {
            assert!(p.get(k).is_some(), "key {k} must be present");
            assert!(p[k].is_null(), "key {k} must be null until resolved");
        }
    }

    #[test]
    fn potential_match_status() {
        let v = parsed(&MatchmakingMessage::PotentialMatch { ticket_id: Uuid::nil() });
        assert_eq!(v["payload"]["ticketStatus"], json!("PotentialMatchCreated"));
        assert!(v["payload"]["address"].is_null());
    }

    #[test]
    fn ticket_id_accessor() {
        let tid = Uuid::parse_str("bb3b794d-2bf3-48eb-9c4b-67d696caccd0").unwrap();
        assert_eq!(
            MatchmakingMessage::Searching { ticket_id: tid }.ticket_id(),
            tid
        );
    }
}
