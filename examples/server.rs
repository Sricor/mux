use bytes::BytesMut;
use ombrac_mux::Connection;
use std::error::Error;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // 监听TCP连接
    let listener = TcpListener::bind("127.0.0.1:8080").await?;
    println!("Server listening on 127.0.0.1:8080");

    loop {
        let (socket, _) = listener.accept().await?;
        println!("New connection accepted");
        
        // 为每个连接创建新的任务
        tokio::spawn(async move {
            let mut connection = Connection::new(socket);
            
            // 接受新的流
            while let Some(mut stream) = connection.accept_stream().await {
                println!("New stream accepted: {}", stream.id());
                
                // 为每个流创建新的任务
                tokio::spawn(async move {
                    let mut buf = BytesMut::with_capacity(1024);
                    
                    loop {
                        match stream.read_buf(&mut buf).await {
                            Ok(0) => {
                                println!("Stream {} closed by client", stream.id());
                                break;
                            }
                            Ok(n) => {
                                println!("Received {} bytes on stream {}", n, stream.id());
                                // Echo back the received data
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
