use bytes::BytesMut;
use ombrac_mux::Connection;
use std::error::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("Server listening on 127.0.0.1:8080");

    loop {
        let (socket, _) = listener.accept().await?;

        tokio::spawn(async move {
            let mut connection = Connection::new(socket);

            while let Some(mut stream) = connection.accept_stream().await {
                tokio::spawn(async move {
                    let mut buf = BytesMut::with_capacity(1024);

                    loop {
                        match stream.read_buf(&mut buf).await {
                            Ok(0) => {
                                println!("Stream {} closed by client", stream.id());
                                break;
                            }
                            Ok(n) => {
                                if let Err(e) = stream.write_all(&buf[..n]).await {
                                    eprintln!("Failed to write to stream {}: {}", stream.id(), e);
                                    break;
                                }
                                buf.clear();
                            }
                            Err(e) => {
                                eprintln!("Error reading from stream {}: {}", stream.id(), e);
                                break;
                            }
                        }
                    }
                });
            }
        });
    }
}
