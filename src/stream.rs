use std::sync::atomic::{AtomicU32, Ordering};
use std::collections::HashMap;
use std::io;
use std::sync::Mutex;

use tokio::sync::mpsc;

use crate::frame::{Frame, FrameReceiver, FrameSender};

pub(crate) type StreamSender = mpsc::Sender<Stream>;
pub(crate) type StreamReceiver = mpsc::Receiver<Stream>;

pub(crate) type StreamId = u32;
pub(crate) type StreamInbound = FrameSender;

const DEFAULT_BUFFER_SIZE: usize = 8192;

#[derive(Debug, Clone, Copy, PartialEq)]
enum StreamState {
    Open,
    HalfClosed,
    Closed,
}

pub struct Stream {
    id: u32,
    state: StreamState,
    writer: FrameSender,
    reader: FrameReceiver,
}

impl Stream {
    fn new(id: u32, writer: FrameSender) -> (Self, FrameSender) {
        let (tx2, reader) = mpsc::channel(DEFAULT_BUFFER_SIZE);

        (
            Stream {
                id,
                state: StreamState::Open,
                writer,
                reader,
            },
            tx2,
        )
    }

    pub async fn close(&mut self) {
        self.state = StreamState::Closed;
    }

    pub fn is_closed(&self) -> bool {
        matches!(self.state, StreamState::Closed)
    }
}

pub(crate) struct StreamPool {
    inner: Mutex<HashMap<StreamId, StreamInbound>>,
    next_stream_id: AtomicU32,
    stream_outbound: FrameSender,
}

impl StreamPool {
    pub(crate) fn new(init_stream_id: StreamId, stream_outbound: FrameSender) -> Self {
        Self { inner: Mutex::new(HashMap::new()), next_stream_id: AtomicU32::new(init_stream_id), stream_outbound }    
    }

    pub(crate) async fn open_stream(&self, stream_id: StreamId) -> io::Result<Stream> {
        let (stream, stream_inbound) = Stream::new(stream_id, self.stream_outbound.clone());

        self.insert((stream.id, stream_inbound)).await;

        Ok(stream)
    }

    // TODO: 溢出时无法开启新的流，返回 None
    /// when to max, return none
    pub(crate) fn next_stream_id(&self) -> Option<StreamId> {
        Some(self.next_stream_id.fetch_add(2, Ordering::Relaxed))
    }

    async fn insert(&self, object: (StreamId, StreamInbound)) -> Option<StreamInbound> {
        let mut pool = self.inner.lock().unwrap();

        pool.insert(object.0, object.1)
    }

    pub(crate) async fn get(&self, stream_id: StreamId) -> Option<StreamInbound> {
        let pool = self.inner.lock().unwrap();

        pool.get(&stream_id).cloned()
    }
}

mod impl_tokio_io_async {
    use std::io;
    use std::pin::Pin;
    use std::task::Context;
    use std::task::Poll;

    use bytes::Bytes;
    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

    use super::{Frame, Stream, StreamState};

    impl AsyncRead for Stream {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<io::Result<()>> {
            let stream = self.get_mut();

            match stream.reader.poll_recv(cx) {
                Poll::Ready(Some(frame)) => {
                    let payload = frame.payload;

                    if buf.remaining() >= payload.len() {
                        buf.put_slice(&payload);

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
                    "channel closed",
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
            use tokio::sync::mpsc::error::TrySendError;

            let stream = self.get_mut();

            if stream.state == StreamState::Closed {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "stream is closed",
                )));
            }

            let frame = Frame::with_data_payload(stream.id, Bytes::copy_from_slice(buf));

            match stream.writer.try_send(frame) {
                Ok(()) => Poll::Ready(Ok(buf.len())),
                Err(TrySendError::Full(_)) => Poll::Pending,
                Err(TrySendError::Closed(_)) => Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "connection is closed",
                ))),
            }
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
}
