mod packet_decoder;
pub use packet_decoder::decode_packet;

mod error;
pub use error::ReceivePacketError;

pub mod definitions;

const ENET_CMD_ACK: u8 = 1;
const ENET_CMD_CONNECT: u8 = 2;
const ENET_CMD_VERIFY_CONNECT: u8 = 3;
const ENET_CMD_DISCONNECT: u8 = 4;
const ENET_CMD_PING: u8 = 5;
const ENET_CMD_SEND_RELIABLE: u8 = 6;
#[allow(unused)]
const ENET_CMD_SEND_UNRELIABLE: u8 = 7;
const ENET_CMD_SEND_FRAGMENT: u8 = 8;
#[allow(unused)]
const ENET_CMD_SEND_UNSEQUENCED: u8 = 9;
#[allow(unused)]
const ENET_CMD_BANDWIDTH_LIMIT: u8 = 10;
#[allow(unused)]
const ENET_CMD_THROTTLE_CONFIGURE: u8 = 11;
const ENET_CMD_SEND_UNRELIABLE_FRAGMENT: u8 = 12;
