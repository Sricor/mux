use std::io;

use bytes::{Bytes, BytesMut};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameType {
    Data,
    Ping,
    Headers,
    GoAway,
}

impl TryFrom<u8> for FrameType {
    type Error = io::Error;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        let frame_type = match value {
            0 => Self::Data,
            1 => Self::Ping,
            2 => Self::Headers,
            3 => Self::GoAway,
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

impl Into<u8> for FrameType {
    fn into(self) -> u8 {
        match self {
            Self::Data => 0,
            Self::Ping => 1,
            Self::Headers => 2,
            Self::GoAway => 3,
        }
    }
}

// +-----------------------------------------------+
// |                Length (24 bits)               |
// +---------------+---------------+---------------+
// |   Type (8)    |   Flags (8)   |               |
// +-+-------------+---------------+-------------------------------+
// |                  ID (32)                                      |
// +---------------------------------------------------------------+
// |                   Payload (*)                                 |
// +---------------------------------------------------------------+
#[derive(Debug)]
pub struct Frame {
    pub(crate) stream_id: u32,
    pub(crate) frame_type: FrameType,
    pub(crate) flags: u8,
    pub(crate) payload: Bytes,
}

impl Frame {
    pub(crate) const HEADER_LENGTH: usize = 9;

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_encode_decode() {
        let test_frame = Frame {
            stream_id: 1,
            frame_type: FrameType::Data,
            flags: 0,
            payload: Bytes::from("Hello, World!"),
        };

        let encoded = test_frame.encode();

        let mut buf = BytesMut::from(&encoded[..]);
        let decoded = Frame::decode(&mut buf).unwrap().unwrap();

        assert_eq!(decoded.stream_id, test_frame.stream_id);
        assert_eq!(decoded.frame_type, test_frame.frame_type);
        assert_eq!(decoded.flags, test_frame.flags);
        assert_eq!(decoded.payload, test_frame.payload);
    }

    #[test]
    fn test_partial_frame() {
        let test_frame = Frame {
            stream_id: 1,
            frame_type: FrameType::Data,
            flags: 0,
            payload: Bytes::from("Hello, World!"),
        };

        let encoded = test_frame.encode();

        let mut buf = BytesMut::from(&encoded[..5]);

        assert!(Frame::decode(&mut buf).unwrap().is_none());
    }

    #[test]
    fn test_multiple_frames() {
        let frame1 = Frame {
            stream_id: 1,
            frame_type: FrameType::Data,
            flags: 0,
            payload: Bytes::from("First frame"),
        };

        let frame2 = Frame {
            stream_id: 2,
            frame_type: FrameType::Ping,
            flags: 1,
            payload: Bytes::from("Second frame"),
        };

        let mut combined = BytesMut::new();
        combined.extend_from_slice(&frame1.encode());
        combined.extend_from_slice(&frame2.encode());

        let decoded1 = Frame::decode(&mut combined).unwrap().unwrap();
        assert_eq!(decoded1.stream_id, frame1.stream_id);
        assert_eq!(decoded1.frame_type, frame1.frame_type);
        assert_eq!(decoded1.payload, frame1.payload);

        let decoded2 = Frame::decode(&mut combined).unwrap().unwrap();
        assert_eq!(decoded2.stream_id, frame2.stream_id);
        assert_eq!(decoded2.frame_type, frame2.frame_type);
        assert_eq!(decoded2.payload, frame2.payload);

        assert!(Frame::decode(&mut combined).unwrap().is_none());
    }

    #[test]
    fn test_invalid_frame_type() {
        let mut buf = BytesMut::with_capacity(Frame::HEADER_LENGTH + 5);
        buf.extend_from_slice(&[0, 0, 5]);
        buf.extend_from_slice(&[255]);
        buf.extend_from_slice(&[0]);
        buf.extend_from_slice(&[0, 0, 0, 1]);
        buf.extend_from_slice(b"hello");

        assert!(Frame::decode(&mut buf).is_err());
    }
}
