use super::*;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    task::JoinHandle,
};

// Exercise real sockets so EOF propagation and backpressure are covered.
async fn connection() -> (TcpStream, TcpStream, JoinHandle<io::Result<(u64, u64)>>) {
    let backend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let backend_addr = backend.local_addr().unwrap();
    let frontend = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let client = TcpStream::connect(frontend.local_addr().unwrap())
        .await
        .unwrap();
    let (accepted, _) = frontend.accept().await.unwrap();
    let proxy = tokio::spawn(relay(accepted, backend_addr, Limits::default()));
    let (server, _) = backend.accept().await.unwrap();
    (client, server, proxy)
}

#[tokio::test]
async fn simultaneous_bulk_traffic_is_byte_exact() {
    timeout(Duration::from_secs(10), async {
        let (mut client, mut server, proxy) = connection().await;
        let upload: Vec<u8> = (0..2 * 1024 * 1024).map(|n| (n % 251) as u8).collect();
        let download: Vec<u8> = (0..3 * 1024 * 1024).map(|n| (n % 239) as u8).collect();
        let (mut client_read, mut client_write) = client.split();
        let (mut server_read, mut server_write) = server.split();
        let mut received_upload = Vec::new();
        let mut received_download = Vec::new();
        tokio::join!(
            async {
                client_write.write_all(&upload).await.unwrap();
                client_write.shutdown().await.unwrap();
            },
            async {
                server_write.write_all(&download).await.unwrap();
                server_write.shutdown().await.unwrap();
            },
            async { server_read.read_to_end(&mut received_upload).await.unwrap() },
            async {
                client_read
                    .read_to_end(&mut received_download)
                    .await
                    .unwrap()
            },
        );
        assert_eq!(received_upload, upload);
        assert_eq!(received_download, download);
        assert_eq!(
            proxy.await.unwrap().unwrap(),
            (upload.len() as u64, download.len() as u64)
        );
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn client_half_close_still_receives_the_backend_response() {
    timeout(Duration::from_secs(5), async {
        let (mut client, mut server, proxy) = connection().await;
        client.write_all(b"request").await.unwrap();
        client.shutdown().await.unwrap();
        let mut request = Vec::new();
        server.read_to_end(&mut request).await.unwrap();
        assert_eq!(request, b"request");
        server.write_all(b"response after EOF").await.unwrap();
        server.shutdown().await.unwrap();
        let mut response = Vec::new();
        client.read_to_end(&mut response).await.unwrap();
        assert_eq!(response, b"response after EOF");
        assert_eq!(proxy.await.unwrap().unwrap(), (7, 18));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn backend_half_close_still_accepts_client_data() {
    timeout(Duration::from_secs(5), async {
        let (mut client, mut server, proxy) = connection().await;
        server.write_all(b"hello").await.unwrap();
        server.shutdown().await.unwrap();
        let mut greeting = Vec::new();
        client.read_to_end(&mut greeting).await.unwrap();
        assert_eq!(greeting, b"hello");
        client.write_all(b"reply after EOF").await.unwrap();
        client.shutdown().await.unwrap();
        let mut reply = Vec::new();
        server.read_to_end(&mut reply).await.unwrap();
        assert_eq!(reply, b"reply after EOF");
        assert_eq!(proxy.await.unwrap().unwrap(), (15, 5));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn unavailable_backend_closes_the_client() {
    timeout(
        Limits::default().connect_timeout + Duration::from_secs(5),
        async {
            // Reserve the port to prevent another test from listening on it.
            // Connecting may be refused or time out, as on the macOS runner.
            let reserved = tokio::net::TcpSocket::new_v4().unwrap();
            reserved.bind("127.0.0.1:0".parse().unwrap()).unwrap();
            let frontend = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let mut client = TcpStream::connect(frontend.local_addr().unwrap())
                .await
                .unwrap();
            let (accepted, _) = frontend.accept().await.unwrap();
            let error = relay(accepted, reserved.local_addr().unwrap(), Limits::default())
                .await
                .unwrap_err();
            assert!(
                matches!(
                    error.kind(),
                    io::ErrorKind::ConnectionRefused | io::ErrorKind::TimedOut
                ),
                "unexpected backend connection error: {error}"
            );
            assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn slow_reader_preserves_data_across_buffer_boundaries() {
    timeout(Duration::from_secs(15), async {
        let (mut client, mut server, proxy) = connection().await;
        // Larger than both relay buffers and typical socket buffers, with a
        // non-divisible tail to catch truncation at buffer boundaries.
        let payload: Vec<u8> = (0..8 * 1024 * 1024 + 17).map(|n| (n % 251) as u8).collect();
        let expected = payload.clone();
        let writer = tokio::spawn(async move {
            client.write_all(&payload).await.unwrap();
            client.shutdown().await.unwrap();
            let mut reply = Vec::new();
            client.read_to_end(&mut reply).await.unwrap();
            assert_eq!(reply, b"received");
        });
        sleep(Duration::from_millis(100)).await;
        let mut received = Vec::new();
        let mut small_buffer = [0; 997];
        loop {
            let count = server.read(&mut small_buffer).await.unwrap();
            if count == 0 {
                break;
            }
            received.extend_from_slice(&small_buffer[..count]);
            tokio::task::yield_now().await;
        }
        assert_eq!(received, expected);
        server.write_all(b"received").await.unwrap();
        server.shutdown().await.unwrap();
        writer.await.unwrap();
        assert_eq!(proxy.await.unwrap().unwrap(), (expected.len() as u64, 8));
    })
    .await
    .unwrap();
}

#[tokio::test]
async fn concurrent_fragmented_sessions_remain_isolated() {
    timeout(Duration::from_secs(15), async {
        let mut sessions = tokio::task::JoinSet::new();
        for id in 0..32u8 {
            sessions.spawn(async move {
                let (mut client, mut server, proxy) = connection().await;
                // Deliberately split writes and leave the connection idle before
                // the final response. TCP boundaries must not become messages.
                let peer = tokio::spawn(async move {
                    let mut data = Vec::new();
                    server.read_to_end(&mut data).await.unwrap();
                    assert_eq!(data, vec![id; 257]);
                    sleep(Duration::from_millis(20)).await;
                    server.write_all(&data).await.unwrap();
                    server.shutdown().await.unwrap();
                });
                for _ in 0..257 {
                    client.write_all(&[id]).await.unwrap();
                    tokio::task::yield_now().await;
                }
                client.shutdown().await.unwrap();
                let mut data = Vec::new();
                client.read_to_end(&mut data).await.unwrap();
                assert_eq!(data, vec![id; 257]);
                peer.await.unwrap();
                assert_eq!(proxy.await.unwrap().unwrap(), (257, 257));
            });
        }
        while let Some(result) = sessions.join_next().await {
            result.unwrap();
        }
    })
    .await
    .unwrap();
}
