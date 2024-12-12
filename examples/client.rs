use bytes::Bytes;
use ombrac_mux::Connection;
use std::error::Error;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::sleep;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    // 连接到服务器
    let socket = TcpStream::connect("127.0.0.1:8080").await?;
    println!("Connected to server");

    let connection = Connection::new(socket);
    
    // 测试 ping
    println!("Testing ping...");
    match connection.ping().await {
        Ok(rtt) => println!("Ping RTT: {:?}", rtt),
        Err(e) => eprintln!("Ping failed: {}", e),
    }

    // 创建多个并发流进行测试
    let mut handles = vec![];
    
    let connection = Arc::new(connection);

    for i in 0..3 {
        let connection_clone = connection.clone();
        let handle = tokio::spawn(async move {
            // 创建新的流
            let mut stream = connection_clone.open_stream().await?;
            println!("Opened stream {}", stream.id());

            // 发送测试数据
            let message = format!("Hello from stream {}!", i);
            stream.write_all(message.as_bytes()).await?;
            println!("Sent message on stream {}: {}", stream.id(), message);

            // 读取响应
            let mut buf = vec![0; 1024];
            let n = stream.read(&mut buf).await?;
            let response = String::from_utf8_lossy(&buf[..n]);
            println!("Received response on stream {}: {}", stream.id(), response);

            // 等待一会儿再发送更多数据
            sleep(Duration::from_millis(500)).await;

            // 发送第二条消息
            let message = format!("Second message from stream {}!", i);
            stream.write_all(message.as_bytes()).await?;
            println!("Sent second message on stream {}: {}", stream.id(), message);

            // 读取第二个响应
            let n = stream.read(&mut buf).await?;
            let response = String::from_utf8_lossy(&buf[..n]);
            println!("Received second response on stream {}: {}", stream.id(), response);

            Ok::<_, Box<dyn Error + Send + Sync>>(())
        });
        handles.push(handle);
    }

    // 等待所有流完成
    for handle in handles {
        if let Err(e) = handle.await? {
            eprintln!("Stream error: {}", e);
        }
    }

    // 等待一会儿确保所有消息都已处理
    sleep(Duration::from_secs(1)).await;
    println!("Test completed");

    Ok(())
}
