use ombrac_mux::Connection;
use std::io;
use tokio::io::{duplex, AsyncReadExt, AsyncWriteExt};
use tokio::time::{sleep, Duration};

#[tokio::test]
async fn test_basic_data_transfer() -> io::Result<()> {
    let (client_transport, server_transport) = duplex(1024);
    
    // 创建客户端和服务器连接
    let client_conn = Connection::new(client_transport);
    let mut server_conn = Connection::new(server_transport);
    
    // 等待连接完全建立
    sleep(Duration::from_millis(100)).await;
    
    // 客户端创建新流并发送数据
    let mut client_stream = client_conn.open_stream().await?;
    let test_data = b"Hello, World!";
    client_stream.write_all(test_data).await?;
    client_stream.flush().await?;
    
    // 服务器接受流并读取数据
    let mut server_stream = server_conn.accept_stream().await.expect("Failed to accept stream");
    let mut buf = vec![0; test_data.len()];
    server_stream.read_exact(&mut buf).await?;
    
    assert_eq!(&buf, test_data);
    Ok(())
}

#[tokio::test]
async fn test_bidirectional_transfer() -> io::Result<()> {
    let (client_transport, server_transport) = duplex(1024);
    
    // 创建客户端和服务器连接
    let client_conn = Connection::new(client_transport);
    let mut server_conn = Connection::new(server_transport);
    
    // 等待连接建立
    sleep(Duration::from_millis(100)).await;
    
    // 客户端创建流
    let mut client_stream = client_conn.open_stream().await?;
    
    // 客户端发送数据
    let client_data = b"Hello from client";
    client_stream.write_all(client_data).await?;
    client_stream.flush().await?;
    
    // 服务器接受流
    let mut server_stream = server_conn.accept_stream().await.expect("Failed to accept stream");
    
    // 服务器读取数据
    let mut buf = vec![0; client_data.len()];
    server_stream.read_exact(&mut buf).await?;
    assert_eq!(&buf, client_data);
    
    // 服务器发送响应
    let server_data = b"Hello from server";
    server_stream.write_all(server_data).await?;
    server_stream.flush().await?;
    
    // 客户端读取响应
    let mut buf = vec![0; server_data.len()];
    client_stream.read_exact(&mut buf).await?;
    assert_eq!(&buf, server_data);
    
    Ok(())
}

#[tokio::test]
async fn test_multiple_streams() -> io::Result<()> {
    let (client_transport, server_transport) = duplex(1024);
    
    let client_conn = Connection::new(client_transport);
    let mut server_conn = Connection::new(server_transport);
    
    sleep(Duration::from_millis(100)).await;
    
    // 创建多个流并同时发送数据
    let mut client_streams = vec![];
    let num_streams = 5;
    
    for i in 0..num_streams {
        let mut stream = client_conn.open_stream().await?;
        let data = format!("Stream {}", i);
        stream.write_all(data.as_bytes()).await?;
        stream.flush().await?;
        client_streams.push((stream, data));
    }
    
    // 服务器接收所有流和数据
    for i in 0..num_streams {
        let mut server_stream = server_conn.accept_stream().await.expect("Failed to accept stream");
        let mut buf = vec![0; 32];
        let n = server_stream.read(&mut buf).await?;
        let received = String::from_utf8_lossy(&buf[..n]);
        assert_eq!(received, format!("Stream {}", i));
    }
    
    Ok(())
}

#[tokio::test]
async fn test_connection_ping() -> io::Result<()> {
    let (client_transport, server_transport) = duplex(1024);
    
    let client_conn = Connection::new(client_transport);
    let _server_conn = Connection::new(server_transport);
    
    // 等待连接建立
    sleep(Duration::from_millis(100)).await;
    
    // 发送 ping 并等待响应
    let rtt = client_conn.ping().await?;
    assert!(rtt < Duration::from_secs(1));
    
    Ok(())
}

#[tokio::test]
async fn test_stream_close() -> io::Result<()> {
    let (client_transport, server_transport) = duplex(1024);
    
    let client_conn = Connection::new(client_transport);
    let mut server_conn = Connection::new(server_transport);
    
    sleep(Duration::from_millis(100)).await;
    
    // 创建流并发送数据
    let mut client_stream = client_conn.open_stream().await?;
    client_stream.write_all(b"Hello").await?;
    client_stream.flush().await?;
    
    // 服务器接收数据
    let mut server_stream = server_conn.accept_stream().await.expect("Failed to accept stream");
    let mut buf = vec![0; 5];
    server_stream.read_exact(&mut buf).await?;
    
    // 关闭客户端流
    client_stream.close().await?;
    
    // 服务器应该收到 EOF
    let mut buf = vec![0; 1];
    let n = server_stream.read(&mut buf).await?;
    assert_eq!(n, 0);
    
    Ok(())
}
