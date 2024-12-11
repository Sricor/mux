use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use tokio::sync::mpsc;

use crate::frame::{Frame, FrameReceiver, FrameSender, FrameType, FLAG_END_STREAM};

pub(crate) type StreamId = u32;
pub(crate) type StreamSender = mpsc::Sender<Stream>;
pub(crate) type StreamReceiver = mpsc::Receiver<Stream>;

const DEFAULT_BUFFER_SIZE: usize = 8192;
const DEFAULT_WINDOW_SIZE: usize = 65_535;
const MAX_WINDOW_SIZE: usize = 1_073_741_823; // 2^30 - 1

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum StreamState {
    Open,
    HalfClosedLocal,
    HalfClosedRemote,
    Closed,
}

pub(crate) struct WindowControl {
    send_window: AtomicUsize,
    recv_window: AtomicUsize,
    max_window_size: usize,
}

impl WindowControl {
    fn new() -> Self {
        Self {
            send_window: AtomicUsize::new(DEFAULT_WINDOW_SIZE),
            recv_window: AtomicUsize::new(DEFAULT_WINDOW_SIZE),
            max_window_size: MAX_WINDOW_SIZE,
        }
    }

    fn update_send_window(&self, increment: usize) -> io::Result<()> {
        let current = self.send_window.load(Ordering::Relaxed);
        let new_size = current
            .checked_add(increment)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "window size overflow"))?;

        if new_size > self.max_window_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "window size exceeds maximum allowed",
            ));
        }

        self.send_window.store(new_size, Ordering::Relaxed);
        Ok(())
    }

    fn consume_send_window(&self, size: usize) -> io::Result<()> {
        let current = self.send_window.load(Ordering::Relaxed);
        if size > current {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "insufficient window size",
            ));
        }

        self.send_window.fetch_sub(size, Ordering::Relaxed);
        Ok(())
    }

    fn should_update_recv_window(&self, consumed: usize) -> Option<usize> {
        let current = self.recv_window.fetch_sub(consumed, Ordering::Relaxed);
        if current - consumed <= DEFAULT_WINDOW_SIZE / 2 {
            Some(DEFAULT_WINDOW_SIZE - (current - consumed))
        } else {
            None
        }
    }
}

pub(crate) struct StreamControl {
    pub(crate) state: Mutex<StreamState>,
    pub(crate) window: WindowControl,
    pub(crate) sender: FrameSender,   // 发送数据到对端
    pub(crate) receiver: FrameSender, // 发送数据到本地Stream
}

impl StreamControl {
    fn new(sender: FrameSender, receiver: FrameSender) -> Self {
        Self {
            state: Mutex::new(StreamState::Open),
            window: WindowControl::new(),
            sender,
            receiver,
        }
    }

    fn set_state(&self, new_state: StreamState) {
        *self.state.lock().unwrap() = new_state;
    }

    fn get_state(&self) -> StreamState {
        *self.state.lock().unwrap()
    }
}

pub struct Stream {
    id: StreamId,
    control: Arc<StreamControl>,
    reader: FrameReceiver,
}

impl Stream {
    fn new(id: StreamId, control: Arc<StreamControl>, reader: FrameReceiver) -> Self {
        Self {
            id,
            control,
            reader,
        }
    }

    pub async fn close(&self) -> io::Result<()> {
        let frame = Frame::new(FrameType::Data, FLAG_END_STREAM, self.id, Bytes::new());

        self.control
            .sender
            .send(frame)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "failed to send close frame"))?;

        self.control.set_state(StreamState::Closed);
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.control.get_state(), StreamState::Closed)
    }
}

pub(crate) struct StreamPool {
    streams: Mutex<HashMap<StreamId, Arc<StreamControl>>>,
    next_stream_id: AtomicU32,
    frame_sender: FrameSender,
}

impl StreamPool {
    pub(crate) fn new(init_stream_id: StreamId, frame_sender: FrameSender) -> Self {
        Self {
            streams: Mutex::new(HashMap::new()),
            next_stream_id: AtomicU32::new(init_stream_id),
            frame_sender,
        }
    }

    pub(crate) fn next_stream_id(&self) -> Option<StreamId> {
        let id = self.next_stream_id.fetch_add(2, Ordering::Relaxed);
        // 检查是否溢出
        if id > 0x7fffffff {
            return None;
        }
        Some(id)
    }

    pub(crate) async fn create_stream(&self, stream_id: StreamId) -> io::Result<Stream> {
        let (stream_sender, stream_receiver) = mpsc::channel(DEFAULT_BUFFER_SIZE);
        let control = Arc::new(StreamControl::new(self.frame_sender.clone(), stream_sender));

        {
            let mut streams = self.streams.lock().unwrap();
            streams.insert(stream_id, Arc::clone(&control));
        }

        Ok(Stream::new(stream_id, control, stream_receiver))
    }

    pub(crate) async fn handle_frame(&self, frame: Frame) -> io::Result<()> {
        let control = {
            let streams = self.streams.lock().unwrap();
            streams
                .get(&frame.stream_id)
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "stream not found"))?
                .clone()
        };

        match frame.frame_type {
            FrameType::Data => {
                // 检查流状态
                match control.get_state() {
                    StreamState::Closed | StreamState::HalfClosedRemote => {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "stream is not readable",
                        ));
                    }
                    _ => {}
                }

                // 检查并更新接收窗口
                control.window.consume_send_window(frame.payload.len())?;

                // 检查是否是结束帧
                let is_end_stream = frame.is_end_stream();

                // 发送到本地 Stream
                control
                    .receiver
                    .send(frame)
                    .await
                    .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "stream closed"))?;

                // 如果是结束帧，更新状态
                if is_end_stream {
                    match control.get_state() {
                        StreamState::Open => control.set_state(StreamState::HalfClosedRemote),
                        StreamState::HalfClosedLocal => control.set_state(StreamState::Closed),
                        _ => {}
                    }
                }
            }

            FrameType::WindowUpdate => {
                let increment = frame.window_update_increment()? as usize;
                control.window.update_send_window(increment)?;
            }

            FrameType::RstStream => {
                control.set_state(StreamState::Closed);
                return Err(io::Error::new(
                    io::ErrorKind::ConnectionReset,
                    "stream reset by peer",
                ));
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

    pub(crate) fn remove_stream(&self, stream_id: StreamId) {
        let mut streams = self.streams.lock().unwrap();
        streams.remove(&stream_id);
    }

    pub(crate) fn get_control(&self, stream_id: StreamId) -> Option<Arc<StreamControl>> {
        let streams = self.streams.lock().unwrap();
        streams.get(&stream_id).cloned()
    }
}

// AsyncRead/AsyncWrite 实现
mod impl_tokio_io_async {
    use bytes::Bytes;
    use std::io;
    use std::pin::Pin;
    use std::task::{Context, Poll};
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    use super::*;

    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let stream = self.get_mut();

            // 检查流状态
            match stream.control.get_state() {
                StreamState::Closed | StreamState::HalfClosedRemote => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "stream is not readable",
                    )));
                }
                _ => {}
            }

            match stream.reader.poll_recv(cx) {
                Poll::Ready(Some(frame)) => {
                    let payload = frame.payload;
                    let payload_len = payload.len();

                    if buf.remaining() >= payload_len {
                        buf.put_slice(&payload);

                        // 检查是否需要更新接收窗口
                        if let Some(increment) =
                            stream.control.window.should_update_recv_window(payload_len)
                        {
                            let frame = Frame::window_update(stream.id, increment as u32);
                            // 在后台发送窗口更新帧
                            let sender = stream.control.sender.clone();
                            tokio::spawn(async move {
                                let _ = sender.send(frame).await;
                            });
                        }

                        Poll::Ready(Ok(()))
                    } else {
                        Poll::Ready(Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "not enough space in buffer",
                        )))
                    }
                }
                Poll::Ready(None) => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "stream closed by peer",
                ))),
                Poll::Pending => Poll::Pending,
            }
        }
    }

    impl AsyncWrite for Stream {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<io::Result<usize>> {
            let stream = self.get_mut();

            // 检查流状态
            match stream.control.get_state() {
                StreamState::Closed | StreamState::HalfClosedLocal => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::BrokenPipe,
                        "stream is not writable",
                    )));
                }
                _ => {}
            }

            // 检查发送窗口
            if let Err(e) = stream.control.window.consume_send_window(buf.len()) {
                return Poll::Ready(Err(e));
            }

            let frame = Frame::with_data_payload(stream.id, Bytes::copy_from_slice(buf));

            // 使用 try_send 避免异步上下文
            match stream.control.sender.try_send(frame) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => {
                    // 恢复窗口大小
                    stream
                        .control
                        .window
                        .send_window
                        .fetch_add(buf.len(), Ordering::Relaxed);
                    Poll::Pending
                }
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(
                    io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"),
                )),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            // 由于我们使用的是 mpsc channel，写入即刷新
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let stream = self.get_mut();

            // 发送带有 END_STREAM 标志的空帧
            let frame = Frame::new(FrameType::Data, FLAG_END_STREAM, stream.id, Bytes::new());

            match stream.control.sender.try_send(frame) {
                Ok(()) => {
                    match stream.control.get_state() {
                        StreamState::Open => stream.control.set_state(StreamState::HalfClosedLocal),
                        StreamState::HalfClosedRemote => {
                            stream.control.set_state(StreamState::Closed)
                        }
                        _ => {}
                    }
                    Poll::Ready(Ok(()))
                }
                Err(tokio::sync::mpsc::error::TrySendError::Full(_)) => Poll::Pending,
                Err(tokio::sync::mpsc::error::TrySendError::Closed(_)) => Poll::Ready(Err(
                    io::Error::new(io::ErrorKind::BrokenPipe, "connection closed"),
                )),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test]
    async fn test_stream_create() {
        let (tx, _rx) = mpsc::channel(1);
        let pool = StreamPool::new(1, tx);
        let stream = pool.create_stream(1).await.unwrap();
        assert!(!stream.is_closed());
    }

    #[tokio::test]
    async fn test_stream_close() {
        let (tx, _rx) = mpsc::channel(1);
        let pool = StreamPool::new(1, tx);
        let stream = pool.create_stream(1).await.unwrap();
        stream.close().await.unwrap();
        assert!(stream.is_closed());
    }
}
