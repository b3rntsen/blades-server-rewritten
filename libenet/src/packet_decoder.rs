use std::io::{ErrorKind, Read, Seek};

use binrw::BinRead;
use smallvec::SmallVec;

use crate::{
    ENET_CMD_ACK, ENET_CMD_CONNECT, ENET_CMD_DISCONNECT, ENET_CMD_PING, ENET_CMD_SEND_FRAGMENT,
    ENET_CMD_SEND_RELIABLE, ENET_CMD_SEND_UNRELIABLE_FRAGMENT, ENET_CMD_VERIFY_CONNECT,
    definitions::{
        Acknowledge, CommandHeader, Connect, ConnectionMessage, ConnectionMessageByType,
        Disconnect, PacketHeader, SendFragment, SendReliable, VerifyConnect,
    },
    error::ReceivePacketError,
};

/**
* Read a packet, return the contained data (low level) (may be empty)
*
* the reader is expected to contain the UDP packet data.
*
* Seek is only used because binrw requires it, even if it doesn’t use it. You may use the NoSeek wrapper.
*/
pub fn decode_packet<R: Read + Seek>(
    reader: &mut R,
) -> Result<SmallVec<[ConnectionMessage; 1]>, ReceivePacketError> {
    let mut result = SmallVec::new();

    // given that udp packets themselves are of limited size, there is no risk of infinite loops
    let packet_header = PacketHeader::read(reader)?;

    loop {
        let command_header = match CommandHeader::read(reader) {
            Ok(header) => header,
            Err(err) => match err.root_cause() {
                binrw::Error::Io(io_error) => {
                    if io_error.kind() == ErrorKind::UnexpectedEof {
                        break;
                    } else {
                        return Err(ReceivePacketError::BinRW(err));
                    }
                }
                _ => {
                    return Err(ReceivePacketError::BinRW(err));
                }
            },
        };

        let instruction = command_header.get_instruction();

        match instruction {
            ENET_CMD_ACK => {
                let message = Acknowledge::read(reader)?;
                result.push(ConnectionMessage::new(
                    packet_header,
                    crate::definitions::ConnectionMessageByType::Ack(message),
                ));
            }
            ENET_CMD_CONNECT => {
                let message = Connect::read(reader)?;
                result.push(ConnectionMessage::new(
                    packet_header,
                    crate::definitions::ConnectionMessageByType::Connect(message),
                ));
            }
            ENET_CMD_VERIFY_CONNECT => {
                let message = VerifyConnect::read(reader)?;
                result.push(ConnectionMessage::new(
                    packet_header,
                    crate::definitions::ConnectionMessageByType::VerifyConnect(message),
                ));
            }
            ENET_CMD_PING => {
                // empty body
                result.push(ConnectionMessage::new(
                    packet_header,
                    crate::definitions::ConnectionMessageByType::Ping,
                ));
            }
            ENET_CMD_SEND_FRAGMENT => {
                let message = SendFragment::read(reader)?;
                result.push(ConnectionMessage::new(
                    packet_header,
                    ConnectionMessageByType::SendReliableFragment(message),
                ));
            }
            ENET_CMD_SEND_UNRELIABLE_FRAGMENT => {
                let message = SendFragment::read(reader)?;
                result.push(ConnectionMessage::new(
                    packet_header,
                    ConnectionMessageByType::SendUnreliableFragment(message),
                ));
            }
            ENET_CMD_SEND_RELIABLE => {
                let message = SendReliable::read(reader)?;
                result.push(ConnectionMessage::new(
                    packet_header,
                    ConnectionMessageByType::SendReliable(message),
                ))
            }
            ENET_CMD_DISCONNECT => {
                let message = Disconnect::read(reader)?;
                result.push(ConnectionMessage::new(
                    packet_header,
                    ConnectionMessageByType::Disconnect(message),
                ))
            }
            // my test recording did not hit any unreliable data, send_unsequenced or CMD bandwidth/throttle configuration
            other => {
                return Err(ReceivePacketError::UnrecognizedInstruction(other));
            }
        }
    }

    Ok(result)
}
