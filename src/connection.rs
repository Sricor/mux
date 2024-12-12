use std::io;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{mpsc, oneshot};
use tokio::time::timeout;

use crate::frame::{Frame, FrameType, FLAG_ACK};
use crate::stream::pool::StreamPool;
use crate::stream::{Stream, StreamId, StreamReceiver, StreamSender};

const DEFAULT_BUFFER_SIZE: usize = 8192;
const PING_TIMEOUT: Duration = Duration::from_secs(5);

// PingTracker 结构体
#[derive(Default)]
struct PingTracker {
    pending_ping: Option<(oneshot::Sender<Duration>, Instant)>,
}

pub struct Connection {
    pool: Arc<StreamPool>,
    next_stream_id: AtomicU32,
    accept_streams: StreamReceiver,
    ping_tracker: Arc<Mutex<PingTracker>>,
}

impl Connection {
    pub fn new<T>(transport: T) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (frame_sender, mut frame_receiver) = mpsc::channel(DEFAULT_BUFFER_SIZE);
        let (accept_streams_sender, accept_streams) = mpsc::channel(DEFAULT_BUFFER_SIZE);

        let pool = Arc::new(StreamPool::new(frame_sender.clone()));
        let ping_tracker = Arc::new(Mutex::new(PingTracker::default()));

        // 初始化 连接控制/PING流
        pool.create_stream(0).unwrap();

        // 创建结果实例
        let result = Connection {
            pool: pool.clone(),
            accept_streams,
            next_stream_id: AtomicU32::new(1), // 客户端从1开始，服务器从2开始
            ping_tracker: ping_tracker.clone(),
        };

        // 在这里创建控制流 (ID = 0)
        let pool_clone = pool.clone();
        tokio::spawn(async move {
            if let Err(e) = pool_clone.create_stream(0) {
                eprintln!("Failed to create control stream: {}", e);
            }
        });

        let (mut read_half, mut write_half) = tokio::io::split(transport);

        // 处理出站帧
        tokio::spawn({
            async move {
                while let Some(data) = frame_receiver.recv().await {
                    if let Err(e) = write_half.write_all(&data).await {
                        eprintln!("Error writing frame: {}", e);
                        break;
                    }
                    if let Err(e) = write_half.flush().await {
                        eprintln!("Error flushing write half: {}", e);
                        break;
                    }
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
            let ping_tracker = ping_tracker.clone();
            async move {
                let mut buf = BytesMut::with_capacity(Frame::HEADER_LENGTH);
                loop {
                    match read_frame(&mut read_half, &mut buf).await {
                        Ok(Some(frame)) => {
                            if let Err(e) =
                                handle_frame(&pool, &accept_streams_sender, &ping_tracker, frame)
                                    .await
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

        self.pool.create_stream(stream_id)
    }

    pub(crate) fn next_stream_id(&self) -> Option<StreamId> {
        let id = self.next_stream_id.fetch_add(2, Ordering::Relaxed);
        if id > 0x7fffffff {
            return None;
        }
        Some(id)
    }

    pub async fn accept_stream(&mut self) -> Option<Stream> {
        self.accept_streams.recv().await
    }

    pub async fn close(self) -> io::Result<()> {
        // 发送 GOAWAY 帧
        let frame = Frame::go_away(0, 0);
        if let Some(control) = self.pool.get(0) {
            control
                .outbound
                .send(frame.encode())
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "failed to send goaway"))?;
        }
        Ok(())
    }

    pub async fn ping(&self) -> io::Result<Duration> {
        let (tx, rx) = oneshot::channel();

        // 生成随机 payload
        let payload = rand::random::<[u8; 8]>();
        let ping_frame = Frame::ping(payload);

        // 记录 ping 请求
        {
            let mut tracker = self.ping_tracker.lock().unwrap();
            if tracker.pending_ping.is_some() {
                return Err(io::Error::new(
                    io::ErrorKind::Other,
                    "another ping already in progress",
                ));
            }
            tracker.pending_ping = Some((tx, Instant::now()));
        }

        // 发送 ping 帧
        if let Some(control) = self.pool.get(0) {
            control
                .outbound
                .send(ping_frame.encode())
                .await
                .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "failed to send ping"))?;
        } else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                "control stream not found",
            ));
        }

        // 等待响应，带超时
        match timeout(PING_TIMEOUT, rx).await {
            Ok(Ok(rtt)) => Ok(rtt),
            Ok(Err(_)) => Err(io::Error::new(
                io::ErrorKind::Other,
                "ping response channel closed",
            )),
            Err(_) => {
                // 清理未完成的 ping
                let mut tracker = self.ping_tracker.lock().unwrap();
                tracker.pending_ping = None;
                Err(io::Error::new(io::ErrorKind::TimedOut, "ping timed out"))
            }
        }
    }
}

// 从传输层读取帧
async fn read_frame<T: AsyncRead + Unpin>(
    read_half: &mut T,
    buf: &mut BytesMut,
) -> io::Result<Option<Frame>> {
    loop {
        if let Some(frame) = Frame::decode(buf)? {
            return Ok(Some(frame));
        }

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

// 修改：处理接收到的帧
async fn handle_frame(
    pool: &StreamPool,
    accept_streams_sender: &StreamSender,
    ping_tracker: &Arc<Mutex<PingTracker>>,
    frame: Frame,
) -> io::Result<()> {
    // 处理连接级别的帧
    if frame.is_connection_control() {
        return handle_connection_frame(pool, ping_tracker, frame).await;
    }

    // 处理流级别的帧
    match frame.frame_type {
        FrameType::Data | FrameType::WindowUpdate => match pool.get(frame.stream_id) {
            Some(_) => {
                pool.handle_frame(frame).await?;
            }
            None if frame.frame_type == FrameType::Data => {
                let stream = pool.create_stream(frame.stream_id)?;
                accept_streams_sender
                    .send(stream)
                    .await
                    .map_err(|_| io::Error::new(io::ErrorKind::Other, "failed to accept stream"))?;
                pool.handle_frame(frame).await?;
            }
            None => {
                return Err(io::Error::new(io::ErrorKind::NotFound, "stream not found"));
            }
        },
        FrameType::RstStream => {
            if let Some(_) = pool.get(frame.stream_id) {
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

// 修改：处理连接级别的帧
async fn handle_connection_frame(
    pool: &StreamPool,
    ping_tracker: &Arc<Mutex<PingTracker>>,
    frame: Frame,
) -> io::Result<()> {
    match frame.frame_type {
        FrameType::Ping => {
            if frame.is_ack() {
                // 处理 PING 响应
                let mut tracker = ping_tracker.lock().unwrap();
                if let Some((tx, start_time)) = tracker.pending_ping.take() {
                    let _ = tx.send(start_time.elapsed());
                }
            } else {
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
    use tokio::time::sleep;

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

    #[tokio::test]
    async fn test_ping_pong() {
        let (client, server) = duplex(1024);
        let client_conn = Connection::new(client);
        let _server_conn = Connection::new(server);

        // 等待连接建立和控制流创建
        sleep(Duration::from_millis(100)).await;

        // 发送 ping 并等待响应
        let rtt = client_conn.ping().await.unwrap();
        assert!(rtt < Duration::from_secs(1));
    }

    #[tokio::test]
    async fn test_ping_timeout() {
        let (client, _server) = duplex(1024);
        let conn = Connection::new(client);

        // 等待控制流创建
        sleep(Duration::from_millis(100)).await;

        // 发送 ping 到一个没有响应的连接
        let result = conn.ping().await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
    }
}
