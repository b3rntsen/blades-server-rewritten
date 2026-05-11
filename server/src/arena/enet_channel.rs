use std::{convert::Infallible, sync::Arc, time::Duration};

use tokio_enet::{Event, Host, HostConfig};

use crate::ServerGlobal;

pub async fn run_enet_channel(globals: Arc<ServerGlobal>) -> Result<Infallible, anyhow::Error> {
    let config = HostConfig {
        address: Some(globals.enet_listen_addr.clone()),
        peer_count: 2000,
        ..Default::default()
    };
    let mut host = Host::new(config)?;

    // TODO: actual implementation
    loop {
        if let Some(event) = host.service(Duration::from_micros(100)).await? {
            match event {
                Event::Connect { peer_id, .. } => {
                    println!("peer {peer_id} connected");
                }
                Event::Disconnect { peer_id, .. } => {
                    println!("peer {peer_id} disconnected");
                }
                Event::Receive {
                    peer_id,
                    channel_id,
                    packet,
                } => {
                    println!(
                        "Received {} bytes from peer {peer_id} on channel {channel_id}",
                        packet.len()
                    );
                }
            }
        }
    }
}
