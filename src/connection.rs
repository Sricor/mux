use std::io;
use std::sync::Arc;

use bytes::BytesMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;

use crate::frame::{Frame, FrameType};
use crate::stream::{Stream, StreamReceiver, StreamSender, StreamPool};

const DEFAULT_BUFFER_SIZE: usize = 8192;

pub struct Connection {
    pool: Arc<StreamPool>,
    accpet_streams: StreamReceiver
}

impl Connection {
    pub fn new<T>(transport: T) -> Self
    where
        T: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let (sender, mut receiver) = mpsc::channel(DEFAULT_BUFFER_SIZE);
        let (accept_streams_sender, accpet_streams) = mpsc::channel(DEFAULT_BUFFER_SIZE);

        let pool = Arc::new(StreamPool::new(1, sender));
        let result = Connection {
            pool: pool.clone(),
            accpet_streams,
        };

        let (mut inbound, mut outbound) = tokio::io::split(transport);

        tokio::spawn(async move {
            while let Some(frame) = receiver.recv().await {
                outbound.write_all(&frame.encode()).await.unwrap();
                outbound.flush().await.unwrap();
            }
        });

        tokio::spawn(async move {
            let mut buf = BytesMut::with_capacity(Frame::HEADER_LENGTH);

            loop {
                match inbound.read_buf(&mut buf).await.unwrap() {
                    0 if buf.is_empty() => return,
                    _ => {
                        if let Some(frame) = Frame::decode(&mut buf).unwrap() {
                            match frame.is_connection_control() {
                                true => Self::handle_connection_frame(frame).await.unwrap(),
                                false => Self::handle_stream_frame(&pool.clone(), &accept_streams_sender.clone(), frame)
                                    .await
                                    .unwrap(),
                            };
                        }
                    }
                };
            }
        });

        result
    }

    pub async fn open_stream(&self) -> io::Result<Stream> {
        match self.pool.next_stream_id() {
            Some(stream_id) => self.pool.open_stream(stream_id).await,
            None => Err(io::Error::other("stream limited"))
        }
    }

    pub async fn accept_stream(&mut self) -> Option<Stream> {
        self.accpet_streams.recv().await
    }

    async fn handle_connection_frame(frame: Frame) -> io::Result<()> {
        match frame.frame_type {
            FrameType::Ping => todo!(),
            FrameType::GoAway => todo!(),

            _ => todo!(),
        }
    }

    // receive stream frame
    async fn handle_stream_frame(pool: &StreamPool, accept_stream_sender: &StreamSender, frame: Frame) -> io::Result<()> {

        match frame.frame_type {
            FrameType::Data => {
                match pool.get(frame.stream_id).await {
                    Some(stream) => {
                        if stream.send(frame).await.is_err() {
                            return Err(io::Error::new(
                                io::ErrorKind::BrokenPipe,
                                "stream is closed",
                            ));
                        };
                    },

                    // Accept new stream
                    None => {
                        let stream = pool.open_stream(frame.stream_id).await.unwrap();

                        accept_stream_sender.send(stream).await.unwrap();
                    }
                };
            }

            _ => todo!(),
        }

        Ok(())
    }
}
