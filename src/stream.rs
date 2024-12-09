use bytes::{Bytes, BytesMut};
use tokio::io::{AsyncRead, AsyncWrite, AsyncReadExt, AsyncWriteExt};
use std::sync::atomic::{AtomicU32, Ordering};
use std::{collections::HashMap, io};
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};

use crate::frame::{Frame, FrameType};

const DEFAULT_BUFFER_SIZE: usize = 8192;

// 流的状态
#[derive(Debug, Clone, Copy, PartialEq)]
enum StreamState {
    Open,
    HalfClosed,
    Closed,
}

// 流的实现
pub struct Stream {
    id: u32,
    state: StreamState,
    buffer: Vec<u8>,
    tx: mpsc::Sender<Bytes>,
    rx: mpsc::Receiver<Bytes>,
    window_size: usize,
}

impl Stream {
    fn new(id: u32) -> (Self, mpsc::Sender<Bytes>) {
        let (tx1, rx) = mpsc::channel(DEFAULT_BUFFER_SIZE);
        let (tx2, _) = mpsc::channel(DEFAULT_BUFFER_SIZE);
        
        (Stream {
            id,
            state: StreamState::Open,
            buffer: Vec::new(),
            tx: tx1,
            rx,
            window_size: DEFAULT_BUFFER_SIZE,
        }, tx2)
    }

    async fn send(&mut self, data: Bytes) -> Result<()> {
        if self.state == StreamState::Closed {
            return Err(anyhow::anyhow!("Stream is closed"));
        }
        self.tx.send(data).await?;
        Ok(())
    }

    async fn recv(&mut self) -> Option<Bytes> {
        self.rx.recv().await
    }
}
pub struct Multiplexer<T> {
    transport: T,
    streams: Arc<Mutex<HashMap<u32, Stream>>>,
    next_stream_id: AtomicU32,
    default_window_size: u32,
}

impl<T> Multiplexer<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub fn new(transport: T) -> Self {
        Multiplexer {
            transport,
            streams: Arc::new(Mutex::new(HashMap::new())),
            next_stream_id: AtomicU32::new(1),
            default_window_size: 65535,
        }
    }

    pub async fn create_stream(&self) -> io::Result<(u32, mpsc::Sender<Bytes>)> {
        let stream_id = self.next_stream_id.fetch_add(2, Ordering::Relaxed);
        let (stream, tx) = Stream::new(stream_id, self.default_window_size);
        
        let mut streams = self.streams.lock().await;
        streams.insert(stream_id, stream);
        
        Ok((stream_id, tx))
    }

    async fn send_frame(&mut self, frame: Frame) -> io::Result<()> {
        let encoded = frame.encode();
        self.transport.write_all(&encoded).await?;

        Ok(())
    }

    async fn read_frame(&mut self) -> io::Result<Option<Frame>> {
        let mut buf = BytesMut::with_capacity(Frame::HEADER_LENGTH);

        loop {
            match self.transport.read_buf(&mut buf).await? {
                0 if buf.is_empty() => return Ok(None),
                0 => return Err(io::Error::other("Connection closed unexpectedly")),
                _ => {
                    if let Some(frame) = Frame::decode(&mut buf)? {
                        return Ok(Some(frame));
                    }
                }
            }
        }
    }

    pub async fn run(&mut self) -> io::Result<()> {
        loop {
            match self.read_frame().await? {
                Some(frame) => self.handle_frame(frame).await?,
                None => break,
            }
        }
        Ok(())
    }

    async fn handle_frame(&mut self, frame: Frame) -> io::Result<()> {
        let mut streams = self.streams.lock().await;
        
        match frame.frame_type {
            FrameType::Data => {
                let stream = streams.get_mut(&frame.stream_id)
                    .ok_or(Error::StreamNotFound(frame.stream_id))?;
                
                if stream.state == StreamState::Closed {
                    return Err(Error::StreamClosed(frame.stream_id));
                }

                if stream.window_size < frame.payload.len() as u32 {
                    return Err(Error::Protocol("Flow control violation".into()));
                }

                stream.window_size -= frame.payload.len() as u32;
                stream.sender.send(frame.payload)
                    .await
                    .map_err(|e| Error::SendError(e.to_string()))?;

                if frame.flags.contains(Flags::END_STREAM) {
                    stream.state = match stream.state {
                        StreamState::Open => StreamState::HalfClosedRemote,
                        StreamState::HalfClosedLocal => StreamState::Closed,
                        _ => return Err(Error::Protocol("Invalid stream state transition".into())),
                    };
                }
            }
            FrameType::WindowUpdate => {
                let stream = streams.get_mut(&frame.stream_id)
                    .ok_or(Error::StreamNotFound(frame.stream_id))?;

                if frame.payload.len() != 4 {
                    return Err(Error::InvalidFrame("Invalid window update size".into()));
                }

                let increment = u32::from_be_bytes([
                    frame.payload[0], frame.payload[1],
                    frame.payload[2], frame.payload[3],
                ]);

                stream.window_size += increment;
            }
            FrameType::Ping => {
                if frame.stream_id != 0 {
                    return Err(Error::Protocol("PING must have stream ID 0".into()));
                }

                if !frame.flags.contains(Flags::ACK) {
                    let response = Frame::new(
                        0,
                        FrameType::Ping,
                        Flags::new(Flags::ACK),
                        frame.payload,
                    );
                    self.send_frame(response).await?;
                }
            }
            FrameType::GoAway => {
                let stream_id = u32::from_be_bytes([
                    frame.payload[0], frame.payload[1],
                    frame.payload[2], frame.payload[3],
                ]);

                for (id, stream) in streams.iter_mut() {
                    if *id <= stream_id {
                        stream.state = StreamState::Closed;
                    }
                }
            }
        }

        Ok(())
    }

    pub async fn close(&mut self, stream_id: u32) -> io::Result<()> {
        let mut streams = self.streams.lock().await;

        if let Some(stream) = streams.get_mut(&stream_id) {
            stream.state = StreamState::Closed;
            // todo: drop stream
            streams.remove(&stream_id);
        }
        
        Ok(())
    }
}