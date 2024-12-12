pub(crate) mod pool;
pub(crate) mod window;

use std::io;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};

use bytes::{Bytes, BytesMut};
use tokio::io::{ReadHalf, WriteHalf};
use tokio::sync::mpsc;
use window::WindowControl;

use crate::frame::{Frame, FrameType, FLAG_END_STREAM};

pub(crate) type StreamId = u32;
pub(crate) type StreamSender = mpsc::Sender<Stream>;
pub(crate) type StreamReceiver = mpsc::Receiver<Stream>;
type FrameSender = mpsc::Sender<Frame>;
type FrameReceiver = mpsc::Receiver<Frame>;

#[derive(Debug, Clone, Copy, PartialEq)]
enum StreamState {
    Open,
    HalfClosedLocal,
    HalfClosedRemote,
    Closed,
}

pub struct StreamControl {
    state: Mutex<StreamState>,
    window: WindowControl,
    pub outbound: mpsc::Sender<Bytes>, // 发送数据到对端
    receiver: FrameSender,             // 发送数据到本地Stream
}

impl StreamControl {
    fn new(outbound: mpsc::Sender<Bytes>, receiver: FrameSender) -> Self {
        Self {
            state: Mutex::new(StreamState::Open),
            window: WindowControl::new(),
            outbound,
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
    reader: Arc<Mutex<BytesMut>>,
}

impl Stream {
    fn new(id: StreamId, control: Arc<StreamControl>, mut inbound: FrameReceiver) -> Self {
        let reader = Arc::new(Mutex::new(bytes::BytesMut::new()));

        let reader_task = reader.clone();

        tokio::spawn(async move {
            while let Some(data) = inbound.recv().await {
                let mut r = reader_task.lock().unwrap();
                r.extend_from_slice(&data.payload);
            };
        });

        Self {
            id,
            control,
            reader
        }
    }

    pub fn id(&self) -> StreamId {
        self.id
    }

    pub fn split(self) -> (ReadHalf<Stream>, WriteHalf<Stream>) {
        tokio::io::split(self)
    }

    pub async fn close(&self) -> io::Result<()> {
        let frame = Frame::new(FrameType::Data, FLAG_END_STREAM, self.id, Bytes::new());

        self.control
            .outbound
            .send(frame.encode())
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "failed to send close frame"))?;

        self.control.set_state(StreamState::Closed);
        Ok(())
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.control.get_state(), StreamState::Closed)
    }
}

// Tokio AsyncRead/AsyncWrite 实现
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

            let s = stream.reader.lock().unwrap();

            let len = buf.remaining();

            buf.put_slice(&s[..len]);

            Poll::Ready(Ok(()))
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
            match stream.control.outbound.try_send(frame.encode()) {
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
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            let stream = self.get_mut();

            // 发送带有 END_STREAM 标志的空帧
            let frame = Frame::new(FrameType::Data, FLAG_END_STREAM, stream.id, Bytes::new());

            match stream.control.outbound.try_send(frame.encode()) {
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
