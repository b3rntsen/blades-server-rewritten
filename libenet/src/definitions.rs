use binrw::BinRead;

#[derive(Debug)]
pub enum ConnectionMessageByType {
    Ack(Acknowledge),                     // 1
    Connect(Connect),                     // 2
    VerifyConnect(VerifyConnect),         // 3
    Disconnect(Disconnect),               // 4
    Ping,                                 // 5
    SendReliable(SendReliable),           // 6
    SendReliableFragment(SendFragment),   // 8
    SendUnreliableFragment(SendFragment), // 12
}

#[derive(Debug)]
pub struct ConnectionMessage {
    pub packet_header: PacketHeader,
    pub command: ConnectionMessageByType,
}

impl ConnectionMessage {
    pub fn new(packet_header: PacketHeader, command: ConnectionMessageByType) -> Self {
        Self {
            packet_header,
            command,
        }
    }
}

#[derive(Debug, BinRead)]
#[br(big)]
pub struct SendFragment {
    pub start_sequence_number: u16,
    pub data_length: u16,
    pub fragment_count: u32,
    pub fragment_number: u32,
    pub total_length: u32,
    pub fragment_offset: u32,
    #[br(count = data_length)]
    pub data: Vec<u8>,
}

#[derive(Debug, BinRead)]
#[br(big)]
pub struct SendReliable {
    pub data_lenght: u16,
    #[br(count = data_lenght)]
    pub data: Vec<u8>,
}

#[derive(Debug, BinRead)]
#[br(big)]
pub struct Disconnect {
    pub data: u32,
}

#[derive(Debug, BinRead)]
#[br(big)]
pub struct ConnectHeader {
    pub outgoing_peer_id: u16,
    pub incoming_session_id: u8,
    pub outgoing_session_id: u8,
    pub mtu: u32,
    pub window_size: u32,
    pub channel_count: u32,
    pub incoming_bandwidth: u32,
    pub outgoing_bandwidth: u32,
    pub packet_throttle_interval: u32,
    pub packet_throttle_acceleration: u32,
    pub packet_throttle_deceleration: u32,
    pub connect_id: u32,
}

#[derive(Debug, BinRead)]
#[br(big)]
pub struct Connect {
    pub header: ConnectHeader,
    pub data: u32,
}

#[derive(Debug, BinRead)]
#[br(big)]
pub struct VerifyConnect {
    pub header: ConnectHeader,
}

#[derive(Debug, BinRead)]
#[br(big)]
pub struct Acknowledge {
    pub reliable_sequence_number: u16,
    pub sent_time: u16,
}

#[derive(Debug, BinRead, Clone, Copy)]
#[br(big)]
pub struct PacketHeader {
    pub peer_id: u16,
    #[br(if(peer_id & 0x4000 != 0))]
    pub timestamp: Option<u16>,
}

#[derive(Debug, BinRead, Clone, Copy)]
#[br(big)]
pub struct CommandHeader {
    pub command: u8,
    pub channel_id: u8,
    pub reliable_sequence_number: u16,
}

impl CommandHeader {
    pub fn get_instruction(&self) -> u8 {
        self.command & 0x0F
    }
}
