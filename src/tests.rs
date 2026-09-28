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
    let proxy = tokio::spawn(relay(accepted, backend_addr));
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
    timeout(Duration::from_secs(5), async {
        // Reserving a port without listening gives a deterministic refusal.
        let reserved = tokio::net::TcpSocket::new_v4().unwrap();
        reserved.bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let frontend = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let mut client = TcpStream::connect(frontend.local_addr().unwrap())
            .await
            .unwrap();
        let (accepted, _) = frontend.accept().await.unwrap();
        let error = relay(accepted, reserved.local_addr().unwrap())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
        assert_eq!(client.read(&mut [0]).await.unwrap(), 0);
    })
    .await
    .unwrap();
}
