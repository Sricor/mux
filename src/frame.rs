use std::io;

use bytes::{Bytes, BytesMut};

#[derive(Debug, PartialEq)]
pub enum FrameType {
    Data,
    WindowUpdate,
    Ping,
    GoAway,
}

// +-----------------------------------------------+
// |                Length (24 bits)               |
// +---------------+---------------+---------------+
// |   Type (8)    |   Flags (8)   |               |
// +-+-------------+---------------+---------------+
// |                  ID (32)                      |
// +-----------------------------------------------+
// |                   Payload                     |
// +-----------------------------------------------+
#[derive(Debug)]
pub struct Frame {
    pub(crate) stream_id: u32,
    pub(crate) frame_type: FrameType,
    pub(crate) flags: u8,
    pub(crate) payload: Bytes,
}

impl Frame {
    // 帧头部长度：3(length) + 1(type) + 1(flags) + 4(stream_id) = 9 bytes
    pub(crate) const HEADER_LENGTH: usize = 9;

    pub(crate) fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::HEADER_LENGTH + self.payload.len());

        // Length (24 bits)
        buf.extend_from_slice(&(self.payload.len() as u32).to_be_bytes()[1..]);

        // Type (8 bits)
        buf.extend_from_slice(&[match self.frame_type {
            FrameType::Data => 0,
            FrameType::WindowUpdate => 1,
            FrameType::Ping => 2,
            FrameType::GoAway => 3,
        }]);

        // Flags (8 bits)
        buf.extend_from_slice(&[self.flags]);

        // Stream ID (32 bits)
        buf.extend_from_slice(&self.stream_id.to_be_bytes());

        // Payload
        buf.extend_from_slice(&self.payload);

        buf.freeze()
    }

    pub(crate) fn decode(buf: &mut BytesMut) -> io::Result<Option<Frame>> {
        // 检查是否有足够的数据来读取头部
        if buf.len() < Self::HEADER_LENGTH {
            return Ok(None);
        }

        // 读取长度 (24 bits)
        let payload_len = ((buf[0] as usize) << 16) | ((buf[1] as usize) << 8) | (buf[2] as usize);

        // 检查是否有完整的帧数据
        if buf.len() < Self::HEADER_LENGTH + payload_len {
            return Ok(None);
        }

        // 读取类型 (8 bits)
        let frame_type = match buf[3] {
            0 => FrameType::Data,
            1 => FrameType::WindowUpdate,
            2 => FrameType::Ping,
            3 => FrameType::GoAway,
            unknown => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("Unknown frame type: {}", unknown),
                ))
            }
        };

        // 读取标志位 (8 bits)
        let flags = buf[4];

        // 读取流ID (32 bits)
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

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Flags(u8);

impl Flags {
    pub const END_STREAM: u8 = 0x1;
    pub const ACK: u8 = 0x2;

    pub fn new(bits: u8) -> Self {
        Flags(bits)
    }

    pub fn contains(&self, flag: u8) -> bool {
        (self.0 & flag) == flag
    }
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_frame_encode_decode() {
        // 创建测试帧
        let test_frame = Frame {
            stream_id: 1,
            frame_type: FrameType::Data,
            flags: 0,
            payload: Bytes::from("Hello, World!"),
        };

        // 编码
        let encoded = test_frame.encode();

        // 解码
        let mut buf = BytesMut::from(&encoded[..]);
        let decoded = Frame::decode(&mut buf).unwrap().unwrap();

        // 验证解码结果
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
        
        // 只取部分数据
        let mut buf = BytesMut::from(&encoded[..5]);
        
        // 应该返回 None，因为数据不完整
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
            frame_type: FrameType::WindowUpdate,
            flags: 1,
            payload: Bytes::from("Second frame"),
        };

        // 将两个帧的数据连接在一起
        let mut combined = BytesMut::new();
        combined.extend_from_slice(&frame1.encode());
        combined.extend_from_slice(&frame2.encode());

        // 解码第一帧
        let decoded1 = Frame::decode(&mut combined).unwrap().unwrap();
        assert_eq!(decoded1.stream_id, frame1.stream_id);
        assert_eq!(decoded1.frame_type, frame1.frame_type);
        assert_eq!(decoded1.payload, frame1.payload);

        // 解码第二帧
        let decoded2 = Frame::decode(&mut combined).unwrap().unwrap();
        assert_eq!(decoded2.stream_id, frame2.stream_id);
        assert_eq!(decoded2.frame_type, frame2.frame_type);
        assert_eq!(decoded2.payload, frame2.payload);

        // 确保没有剩余数据
        assert!(Frame::decode(&mut combined).unwrap().is_none());
    }

    #[test]
    fn test_invalid_frame_type() {
        let mut buf = BytesMut::with_capacity(Frame::HEADER_LENGTH + 5);
        buf.extend_from_slice(&[0, 0, 5]); // length
        buf.extend_from_slice(&[255]); // invalid frame type
        buf.extend_from_slice(&[0]); // flags
        buf.extend_from_slice(&[0, 0, 0, 1]); // stream id
        buf.extend_from_slice(b"hello"); // payload

        assert!(Frame::decode(&mut buf).is_err());
    }
}
