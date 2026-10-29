use thiserror::Error;

pub const MAGIC: u8 = 0x24;
pub const FRAME_HEADER_SIZE: usize = 9;
pub const MAX_PAYLOAD: usize = 4 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u16)]
pub enum Command {
    SafetyInfo = 0x0000,
    SafetyAuth = 0x0002,
    GetDeviceInfo = 0x001e,
    GetState = 0x001c,
    GetVolume = 0x000e,
    SetVolume = 0x000c,
    GetMediaInfo = 0x0014,
    Heartbeat = 0x001a,
    SetPlaySource = 0x0040,
    // MiPlay reuses command 0x0000 in separate handshake/media namespaces.
    // Keep an internal discriminant and map it with `raw_code`.
    Open = 0x8000,
    Pause = 0x0004,
    Resume = 0x0006,
    Close = 0x0008,
}

impl Command {
    pub fn raw_code(self) -> u16 {
        match self {
            Self::Open => 0x0000,
            other => other as u16,
        }
    }

    pub fn from_raw(value: u16) -> Option<Self> {
        Some(match value {
            0x0000 => Self::SafetyInfo,
            0x0002 => Self::SafetyAuth,
            0x001e => Self::GetDeviceInfo,
            0x001c => Self::GetState,
            0x000e => Self::GetVolume,
            0x000c => Self::SetVolume,
            0x0014 => Self::GetMediaInfo,
            0x001a => Self::Heartbeat,
            0x0040 => Self::SetPlaySource,
            0x0004 => Self::Pause,
            0x0006 => Self::Resume,
            0x0008 => Self::Close,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommandFrame {
    pub command: u16,
    pub sequence: u16,
    pub payload: Vec<u8>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ProtocolError {
    #[error("invalid frame magic")]
    InvalidMagic,
    #[error("payload exceeds limit: {0}")]
    PayloadTooLarge(usize),
    #[error("truncated frame")]
    Truncated,
    #[error("frame length overflow")]
    LengthOverflow,
}

pub fn encode_frame(frame: &CommandFrame) -> Result<Vec<u8>, ProtocolError> {
    if frame.payload.len() > MAX_PAYLOAD {
        return Err(ProtocolError::PayloadTooLarge(frame.payload.len()));
    }
    let length = u32::try_from(frame.payload.len()).map_err(|_| ProtocolError::LengthOverflow)?;
    let mut output = Vec::with_capacity(FRAME_HEADER_SIZE + frame.payload.len());
    output.push(MAGIC);
    output.extend_from_slice(&frame.command.to_be_bytes());
    output.extend_from_slice(&frame.sequence.to_be_bytes());
    output.extend_from_slice(&length.to_be_bytes());
    output.extend_from_slice(&frame.payload);
    Ok(output)
}

#[derive(Default)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
}

impl FrameDecoder {
    pub fn feed(&mut self, bytes: &[u8]) -> Result<Vec<CommandFrame>, ProtocolError> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < FRAME_HEADER_SIZE {
                break;
            }
            if self.buffer[0] != MAGIC {
                return Err(ProtocolError::InvalidMagic);
            }
            let command = u16::from_be_bytes([self.buffer[1], self.buffer[2]]);
            let sequence = u16::from_be_bytes([self.buffer[3], self.buffer[4]]);
            let length = u32::from_be_bytes([
                self.buffer[5],
                self.buffer[6],
                self.buffer[7],
                self.buffer[8],
            ]) as usize;
            if length > MAX_PAYLOAD {
                return Err(ProtocolError::PayloadTooLarge(length));
            }
            let total = FRAME_HEADER_SIZE
                .checked_add(length)
                .ok_or(ProtocolError::LengthOverflow)?;
            if self.buffer.len() < total {
                break;
            }
            let payload = self.buffer[FRAME_HEADER_SIZE..total].to_vec();
            self.buffer.drain(..total);
            frames.push(CommandFrame {
                command,
                sequence,
                payload,
            });
        }
        Ok(frames)
    }

    pub fn finish(self) -> Result<(), ProtocolError> {
        if self.buffer.is_empty() {
            Ok(())
        } else {
            Err(ProtocolError::Truncated)
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionState {
    New,
    Authenticated,
    Ready,
    Playing,
    Paused,
    Closed,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SessionError {
    #[error("command {command:#06x} is invalid in state {state:?}")]
    InvalidTransition { command: u16, state: SessionState },
}

pub fn apply_command(state: &mut SessionState, command: Command) -> Result<(), SessionError> {
    let allowed = matches!(
        (*state, command),
        (SessionState::New, Command::SafetyInfo | Command::SafetyAuth)
            | (
                SessionState::Authenticated,
                Command::GetDeviceInfo | Command::GetState | Command::GetVolume
            )
            | (SessionState::Authenticated, Command::Open)
            | (
                SessionState::Ready,
                Command::SetVolume | Command::GetVolume | Command::Open
            )
            | (SessionState::Ready, Command::Resume)
            | (
                SessionState::Playing,
                Command::Pause | Command::Close | Command::Heartbeat | Command::SetVolume
            )
            | (
                SessionState::Paused,
                Command::Resume | Command::Close | Command::Heartbeat | Command::SetVolume
            )
            | (_, Command::Heartbeat)
    );
    if !allowed {
        return Err(SessionError::InvalidTransition {
            command: command.raw_code(),
            state: *state,
        });
    }
    *state = match command {
        Command::SafetyAuth => SessionState::Authenticated,
        Command::Open => SessionState::Ready,
        Command::Resume => SessionState::Playing,
        Command::Pause => SessionState::Paused,
        Command::Close => SessionState::Closed,
        _ => *state,
    };
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decodes_fragmented_and_coalesced_frames() {
        let first = encode_frame(&CommandFrame {
            command: 0x1c,
            sequence: 7,
            payload: b"one".to_vec(),
        })
        .unwrap();
        let second = encode_frame(&CommandFrame {
            command: 0x1a,
            sequence: 8,
            payload: b"two".to_vec(),
        })
        .unwrap();
        let mut decoder = FrameDecoder::default();
        assert!(decoder.feed(&first[..4]).unwrap().is_empty());
        let mut combined = first[4..].to_vec();
        combined.extend_from_slice(&second);
        let frames = decoder.feed(&combined).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].payload, b"one");
        assert_eq!(frames[1].sequence, 8);
    }

    #[test]
    fn rejects_invalid_magic_and_large_payload() {
        let mut decoder = FrameDecoder::default();
        assert!(decoder.feed(&[0]).unwrap().is_empty());
        assert_eq!(decoder.finish().unwrap_err(), ProtocolError::Truncated);
        let mut decoder = FrameDecoder::default();
        assert_eq!(
            decoder.feed(&[1, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap_err(),
            ProtocolError::InvalidMagic
        );
        let frame = CommandFrame {
            command: 1,
            sequence: 1,
            payload: vec![0; MAX_PAYLOAD + 1],
        };
        assert_eq!(
            encode_frame(&frame).unwrap_err(),
            ProtocolError::PayloadTooLarge(MAX_PAYLOAD + 1)
        );
    }

    #[test]
    fn applies_basic_playback_state_machine() {
        let mut state = SessionState::New;
        apply_command(&mut state, Command::SafetyInfo).unwrap();
        apply_command(&mut state, Command::SafetyAuth).unwrap();
        apply_command(&mut state, Command::Open).unwrap();
        apply_command(&mut state, Command::Resume).unwrap();
        apply_command(&mut state, Command::Pause).unwrap();
        assert_eq!(state, SessionState::Paused);
    }
}
