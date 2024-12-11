use std::io;

use bytes::{Bytes, BytesMut};

use crate::stream::StreamId;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameType {
    Data,
    Ping,
    GoAway,
    WindowUpdate,
    Settings,
    RstStream,
}

impl TryFrom<u8> for FrameType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        let frame_type = match value {
            0 => Self::Data,
            1 => Self::Ping,
            2 => Self::GoAway,
            3 => Self::WindowUpdate,
            4 => Self::Settings,
            5 => Self::RstStream,
            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("unknown frame type: {}", unknown),
                ))
            }
        };

        Ok(frame_type)
    }
}

impl From<FrameType> for u8 {
    fn from(frame_type: FrameType) -> u8 {
        match frame_type {
            FrameType::Data => 0,
            FrameType::Ping => 1,
            FrameType::GoAway => 2,
            FrameType::WindowUpdate => 3,
            FrameType::Settings => 4,
            FrameType::RstStream => 5,
        }
    }
}

// Frame flags
pub(crate) const FLAG_END_STREAM: u8 = 0x1;
pub(crate) const FLAG_ACK: u8 = 0x2;

#[derive(Debug)]
pub(crate) struct Frame {
    pub(crate) frame_type: FrameType,
    pub(crate) flags: u8,
    pub(crate) stream_id: StreamId,
    pub(crate) payload: Bytes,
}

impl Frame {
    pub(crate) const HEADER_LENGTH: usize = 9;

    pub(crate) fn new(
        frame_type: FrameType,
        flags: u8,
        stream_id: StreamId,
        payload: Bytes,
    ) -> Self {
        Self {
            frame_type,
            flags,
            stream_id,
            payload,
        }
    }

    pub(crate) fn with_data_payload(stream_id: StreamId, payload: Bytes) -> Self {
        Self::new(FrameType::Data, 0, stream_id, payload)
    }

    pub(crate) fn window_update(stream_id: StreamId, increment: u32) -> Self {
        let mut payload = BytesMut::with_capacity(4);
        payload.extend_from_slice(&increment.to_be_bytes());
        Self::new(FrameType::WindowUpdate, 0, stream_id, payload.freeze())
    }

    pub(crate) fn ping(payload: [u8; 8]) -> Self {
        Self::new(FrameType::Ping, 0, 0, Bytes::copy_from_slice(&payload))
    }

    pub(crate) fn ping_ack(payload: [u8; 8]) -> Self {
        Self::new(
            FrameType::Ping,
            FLAG_ACK,
            0,
            Bytes::copy_from_slice(&payload),
        )
    }

    pub(crate) fn go_away(last_stream_id: StreamId, error_code: u32) -> Self {
        let mut payload = BytesMut::with_capacity(8);
        payload.extend_from_slice(&last_stream_id.to_be_bytes());
        payload.extend_from_slice(&error_code.to_be_bytes());
        Self::new(FrameType::GoAway, 0, 0, payload.freeze())
    }

    pub(crate) fn rst_stream(stream_id: StreamId, error_code: u32) -> Self {
        let mut payload = BytesMut::with_capacity(4);
        payload.extend_from_slice(&error_code.to_be_bytes());
        Self::new(FrameType::RstStream, 0, stream_id, payload.freeze())
    }

    pub(crate) fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::HEADER_LENGTH + self.payload.len());

        buf.extend_from_slice(&(self.payload.len() as u32).to_be_bytes()[1..]);
        buf.extend_from_slice(&[self.frame_type.into()]);
        buf.extend_from_slice(&[self.flags]);
        buf.extend_from_slice(&self.stream_id.to_be_bytes());
        buf.extend_from_slice(&self.payload);

        buf.freeze()
    }

    pub(crate) fn decode(buf: &mut BytesMut) -> io::Result<Option<Frame>> {
        if buf.len() < Self::HEADER_LENGTH {
            return Ok(None);
        }

        let payload_len = ((buf[0] as usize) << 16) | ((buf[1] as usize) << 8) | (buf[2] as usize);

        if buf.len() < Self::HEADER_LENGTH + payload_len {
            return Ok(None);
        }

        let frame_type = FrameType::try_from(buf[3])?;
        let flags = buf[4];
        let stream_id = ((buf[5] as u32) << 24)
            | ((buf[6] as u32) << 16)
            | ((buf[7] as u32) << 8)
            | (buf[8] as u32);

        let mut payload = buf.split_to(Self::HEADER_LENGTH + payload_len);
        let payload = payload.split_off(Self::HEADER_LENGTH);

        Ok(Some(Frame {
            stream_id,
            frame_type,
            flags,
            payload: payload.freeze(),
        }))
    }

    pub(crate) fn is_connection_control(&self) -> bool {
        self.stream_id == 0
    }

    pub(crate) fn window_update_increment(&self) -> io::Result<u32> {
        if self.frame_type != FrameType::WindowUpdate {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "not a window update frame",
            ));
        }

        if self.payload.len() != 4 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid window update payload length",
            ));
        }

        let increment = u32::from_be_bytes(self.payload[..4].try_into().unwrap());
        if increment == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "window update increment cannot be zero",
            ));
        }

        Ok(increment)
    }

    pub(crate) fn is_end_stream(&self) -> bool {
        self.flags & FLAG_END_STREAM != 0
    }

    pub(crate) fn is_ack(&self) -> bool {
        self.flags & FLAG_ACK != 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_encode_decode() {
        let test_frame = Frame::with_data_payload(1, Bytes::from("Hello, World!"));
        let encoded = test_frame.encode();
        let mut buf = BytesMut::from(&encoded[..]);
        let decoded = Frame::decode(&mut buf).unwrap().unwrap();

        assert_eq!(decoded.stream_id, test_frame.stream_id);
        assert_eq!(decoded.frame_type, test_frame.frame_type);
        assert_eq!(decoded.flags, test_frame.flags);
        assert_eq!(decoded.payload, test_frame.payload);
    }

    #[test]
    fn test_window_update() {
        let frame = Frame::window_update(1, 1024);
        assert_eq!(frame.window_update_increment().unwrap(), 1024);
    }

    #[test]
    fn test_ping_ack() {
        let ping = Frame::ping([1; 8]);
        assert!(!ping.is_ack());

        let pong = Frame::ping_ack([1; 8]);
        assert!(pong.is_ack());
    }

    #[test]
    fn test_flags() {
        let mut frame = Frame::with_data_payload(1, Bytes::from("test"));
        assert!(!frame.is_end_stream());

        frame.flags |= FLAG_END_STREAM;
        assert!(frame.is_end_stream());
    }
}
