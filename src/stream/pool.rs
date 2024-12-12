use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::sync::Mutex;

use bytes::Bytes;
use tokio::sync::mpsc;

use super::{Stream, StreamControl, StreamId, StreamState};

use crate::frame::{Frame, FrameType};

const DEFAULT_BUFFER_SIZE: usize = 8192;

pub(crate) struct StreamPool {
    inner: Mutex<HashMap<StreamId, Arc<StreamControl>>>,
    outbound: mpsc::Sender<Bytes>,
}

impl StreamPool {
    pub(crate) fn new(outbound: mpsc::Sender<Bytes>) -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            outbound,
        }
    }

    pub(crate) fn create_stream(&self, stream_id: StreamId) -> io::Result<Stream> {
        let (stream_sender, stream_receiver) = mpsc::channel(DEFAULT_BUFFER_SIZE);
        let control = Arc::new(StreamControl::new(self.outbound.clone(), stream_sender));

        {
            let mut streams = self.inner.lock().unwrap();
            streams.insert(stream_id, Arc::clone(&control));
        }

        Ok(Stream::new(stream_id, control, stream_receiver))
    }

    pub(crate) async fn handle_frame(&self, frame: Frame) -> io::Result<()> {
        let control = {
            let streams = self.inner.lock().unwrap();
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

    fn remove(&self, stream_id: StreamId) -> Option<Arc<StreamControl>> {
        let mut streams = self.inner.lock().unwrap();
        streams.remove(&stream_id)
    }

    pub fn get(&self, stream_id: StreamId) -> Option<Arc<StreamControl>> {
        let streams = self.inner.lock().unwrap();
        streams.get(&stream_id).cloned()
    }
}

#[cfg(test)]
mod tests_pool {
    use tokio::sync::mpsc;

    use super::StreamPool;

    #[tokio::test]
    async fn test_stream_create() {
        let (tx, _rx) = mpsc::channel(1);
        let pool = StreamPool::new(tx);
        let stream = pool.create_stream(1).unwrap();
        assert!(!stream.is_closed());
    }

    #[tokio::test]
    async fn test_stream_close() {
        let (tx, _rx) = mpsc::channel(1);
        let pool = StreamPool::new(tx);
        let stream = pool.create_stream(1).unwrap();
        stream.close().await.unwrap();
        assert!(stream.is_closed());
    }
}
