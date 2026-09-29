//! Reproducible informational latency/throughput benchmark, without CI thresholds.
//! Run: cargo run --release --example messaging_bench -- 20000
use bytes::Bytes;
use rift::messaging::{Broker, BrokerConfig, ConsumerConfig, Stream, StreamConfig, quic};
use std::{
    error::Error,
    path::PathBuf,
    time::{Instant, SystemTime, UNIX_EPOCH},
};

struct TemporaryStore(PathBuf);
impl Drop for TemporaryStore {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn report(name: &str, mut samples: Vec<u128>) {
    samples.sort_unstable();
    let percentile = |p: usize| samples[(samples.len() - 1) * p / 100] as f64 / 1000.0;
    println!(
        "{name}: p50={:.3} us p95={:.3} us p99={:.3} us ({} samples)",
        percentile(50),
        percentile(95),
        percentile(99),
        samples.len()
    );
}

#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
async fn main() -> Result<(), Box<dyn Error>> {
    let count = std::env::args()
        .nth(1)
        .map(|v| v.parse())
        .transpose()?
        .unwrap_or(20_000usize);
    if !(100..=10_000_000).contains(&count) {
        return Err("sample count must be 100..=10000000".into());
    }
    let broker = Broker::new(BrokerConfig {
        subscription_capacity: 8192,
        ..Default::default()
    })?;
    let payload = Bytes::from(vec![42u8; 64]);
    let mut local = broker.subscribe("bench.local", None)?;
    let mut samples = Vec::with_capacity(count);
    for i in 0..count + 1000 {
        let start = Instant::now();
        broker.publish("bench.local", payload.clone())?;
        assert_eq!(local.recv().await?.payload, payload);
        if i >= 1000 {
            samples.push(start.elapsed().as_nanos());
        }
    }
    report("local publish+receive (64 B, same task)", samples);

    let cert = rcgen::generate_simple_self_signed(vec!["localhost".into()])?;
    let server_tls = quic::server_config_from_der(
        vec![cert.cert.der().clone()],
        rustls::pki_types::PrivatePkcs8KeyDer::from(cert.signing_key.serialize_der()).into(),
    )?;
    let client_tls = quic::client_config_from_der(vec![cert.cert.der().clone()])?;
    let auth = quic::AuthConfig {
        identities: vec![quic::Identity {
            name: "benchmark".into(),
            token: "benchmark-only-token-32-bytes-long".into(),
            publish: vec!["bench.>".into()],
            subscribe: vec!["bench.>".into()],
            control: false,
        }],
        ..Default::default()
    };
    let server = quic::Server::bind("127.0.0.1:0".parse()?, server_tls, broker, auth)?;
    let client = quic::Client::connect(
        server.local_addr()?,
        "localhost",
        client_tls,
        "benchmark-only-token-32-bytes-long",
    )
    .await?;
    let mut subscriber = client.subscribe("bench.quic", None).await?;
    let mut publisher = client.publisher().await?;
    let mut samples = Vec::with_capacity(count);
    for i in 0..count + 1000 {
        let start = Instant::now();
        publisher
            .publish("bench.quic", None, payload.clone())
            .await?;
        assert_eq!(subscriber.recv().await?.payload, payload);
        if i >= 1000 {
            samples.push(start.elapsed().as_nanos());
        }
    }
    report(
        "QUIC client -> broker -> client (64 B, warm connection)",
        samples,
    );
    let start = Instant::now();
    let mut received = 0;
    while received < count {
        let batch = (count - received).min(256);
        for _ in 0..batch {
            publisher
                .publish("bench.quic", None, payload.clone())
                .await?;
        }
        publisher.flush().await?;
        for _ in 0..batch {
            assert_eq!(subscriber.recv().await?.payload, payload);
        }
        received += batch;
    }
    println!(
        "QUIC pipelined delivery (64 B, batches of 256): {:.0} messages/s",
        count as f64 / start.elapsed().as_secs_f64()
    );
    client.close();
    server.shutdown().await;

    let stream = Stream::memory(StreamConfig::default())?;
    let consumer = stream
        .consumer("benchmark", ConsumerConfig::default())
        .await?;
    let mut samples = Vec::with_capacity(count);
    for _ in 0..count {
        let start = Instant::now();
        let record = stream
            .publish("bench.stream", None, payload.clone())
            .await?;
        let delivery = consumer.fetch(1).await?;
        assert_eq!(delivery[0].record.sequence, record.sequence);
        consumer.ack(record.sequence).await?;
        samples.push(start.elapsed().as_nanos());
    }
    report("memory stream publish+fetch+ack (64 B)", samples);

    let directory = TemporaryStore(std::env::temp_dir().join(format!(
        "rift-messaging-bench-{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos()
    )));
    let durable = Stream::open(&directory.0, StreamConfig::default()).await?;
    let mut samples = Vec::with_capacity(count.min(1000));
    for _ in 0..count.min(1000) {
        let start = Instant::now();
        durable
            .publish("bench.stream", None, payload.clone())
            .await?;
        samples.push(start.elapsed().as_nanos());
    }
    report(
        "file stream publish+sync (64 B, OS temporary directory)",
        samples,
    );
    drop(durable);
    Ok(())
}
