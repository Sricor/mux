use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::time::timeout;

use crate::frame::{Frame, FrameType, FLAG_ACK};
use crate::stream::pool::StreamPool;
use crate::stream::{Stream, StreamId, StreamReceiver, StreamSender};

const DEFAULT_BUFFER_SIZE: usize = 8192;
const PING_TIMEOUT: Duration = Duration::from_secs(5);

pub struct Connection {
    pool: Arc<StreamPool>,
    next_stream_id: AtomicU32,
    accept_streams: StreamReceiver,
}

impl Connection {
    pub fn new<T>(transport: T) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (frame_sender, mut frame_receiver) = mpsc::channel(DEFAULT_BUFFER_SIZE);
        let (accept_streams_sender, accept_streams) = mpsc::channel(DEFAULT_BUFFER_SIZE);

        let pool = Arc::new(StreamPool::new(frame_sender));
        // TODO: insert connection control stream

        let result = Connection {
            pool: pool.clone(),
            accept_streams,
            next_stream_id: AtomicU32::new(1),
        };

        let (mut read_half, mut write_half) = tokio::io::split(transport);

        // 处理出站帧
        tokio::spawn({
            async move {
                while let Some(data) = frame_receiver.recv().await {
                    write_half.write_all(&data).await.unwrap();
                    write_half.flush().await.unwrap();
                }
                // 连接关闭时清理资源
                if let Err(e) = write_half.shutdown().await {
                    eprintln!("Error shutting down write half: {}", e);
                }
            }
        });

        // 处理入站帧
        tokio::spawn({
            let pool = pool.clone();
            async move {
                let mut buf = BytesMut::with_capacity(Frame::HEADER_LENGTH);
                loop {
                    match read_frame(&mut read_half, &mut buf).await {
                        Ok(Some(frame)) => {
                            if let Err(e) = handle_frame(&pool, &accept_streams_sender, frame).await
                            {
                                eprintln!("Failed to handle frame: {}", e);
                                break;
                            }
                        }
                        Ok(None) => continue,
                        Err(e) => {
                            eprintln!("Failed to read frame: {}", e);
                            break;
                        }
                    }
                }
            }
        });

        result
    }

    pub async fn open_stream(&self) -> io::Result<Stream> {
        let stream_id = self
            .next_stream_id()
            .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "stream id exhausted"))?;

        self.pool.create_stream(stream_id).await
    }

    pub(crate) fn next_stream_id(&self) -> Option<StreamId> {
        let id = self.next_stream_id.fetch_add(2, Ordering::Relaxed);
        // 检查是否溢出
        if id > 0x7fffffff {
            return None;
        }
        Some(id)
    }

    // 接受一个新的入站流
    pub async fn accept_stream(&mut self) -> Option<Stream> {
        self.accept_streams.recv().await
    }

    // 关闭连接
    pub async fn close(self) -> io::Result<()> {
        // 发送 GOAWAY 帧
        let frame = Frame::go_away(0, 0);
        // TODO: 通过某种方式发送最后的 GOAWAY 帧
        Ok(())
    }

    // 发送 PING 并等待响应
    pub async fn ping(&self) -> io::Result<Duration> {
        todo!("Implement ping functionality")
    }
}

// 从传输层读取帧
async fn read_frame<T: AsyncRead + Unpin>(
    read_half: &mut T,
    buf: &mut BytesMut,
) -> io::Result<Option<Frame>> {
    loop {
        // 尝试解码一个完整的帧
        if let Some(frame) = Frame::decode(buf)? {
            return Ok(Some(frame));
        }

        // 需要更多数据
        if 0 == read_half.read_buf(buf).await? {
            if buf.is_empty() {
                return Ok(None);
            } else {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "connection closed mid-frame",
                ));
            }
        }
    }
}

// 处理接收到的帧
async fn handle_frame(
    pool: &StreamPool,
    accept_streams_sender: &StreamSender,
    frame: Frame,
) -> io::Result<()> {
    // 处理连接级别的帧
    if frame.is_connection_control() {
        return handle_connection_frame(pool, frame).await;
    }

    // 处理流级别的帧
    match frame.frame_type {
        FrameType::Data | FrameType::WindowUpdate => {
            match pool.get(frame.stream_id) {
                Some(_) => {
                    // 已存在的流
                    pool.handle_frame(frame).await?;
                }
                None if frame.frame_type == FrameType::Data => {
                    // 新的入站流
                    let stream = pool.create_stream(frame.stream_id).await?;
                    accept_streams_sender.send(stream).await.map_err(|_| {
                        io::Error::new(io::ErrorKind::Other, "failed to accept stream")
                    })?;

                    // 处理第一个数据帧
                    pool.handle_frame(frame).await?;
                }
                None => {
                    return Err(io::Error::new(io::ErrorKind::NotFound, "stream not found"));
                }
            }
        }
        FrameType::RstStream => {
            if let Some(control) = pool.get(frame.stream_id) {
                // 处理流重置
                pool.handle_frame(frame).await?;
            }
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected frame type",
            ));
        }
    }

    Ok(())
}

// 处理连接级别的帧
async fn handle_connection_frame(pool: &StreamPool, frame: Frame) -> io::Result<()> {
    match frame.frame_type {
        FrameType::Ping => {
            if !frame.is_ack() {
                // 收到 PING，回复 PONG
                let pong = Frame::ping_ack(frame.payload[..8].try_into().unwrap());
                if let Some(control) = pool.get(0) {
                    control.outbound.send(pong.encode()).await.map_err(|_| {
                        io::Error::new(io::ErrorKind::BrokenPipe, "failed to send pong")
                    })?;
                }
            }
        }
        FrameType::GoAway => {
            // TODO: 实现优雅关闭
            // 1. 停止接受新的流
            // 2. 等待现有流完成
            // 3. 关闭连接
        }
        FrameType::Settings => {
            // TODO: 实现设置的处理
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unexpected connection frame type",
            ));
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn test_connection_creation() {
        let (client, _server) = duplex(1024);
        let _conn = Connection::new(client);
    }

    #[tokio::test]
    async fn test_stream_creation() {
        let (client, _server) = duplex(1024);
        let conn = Connection::new(client);
        let stream = conn.open_stream().await.unwrap();
        assert!(!stream.is_closed());
    }
}
