use thiserror::Error;

#[derive(Debug, Error)]
pub enum ReceivePacketError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("BinRW error: {0}")]
    BinRW(#[from] binrw::Error),
    #[error("Unrecognized instruction: {0}")]
    UnrecognizedInstruction(u8),
}
