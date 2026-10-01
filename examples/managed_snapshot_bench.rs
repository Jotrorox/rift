//! Informational full-snapshot benchmark; no processes are started.
//! Run: cargo run --release --example managed_snapshot_bench -- 200
use std::{error::Error, hint::black_box, sync::Arc, time::Instant};

use rift::{
    config::{Config, ManagedServer},
    managed::ManagedServers,
    players::PlayerRegistry,
};

fn main() -> Result<(), Box<dyn Error>> {
    const SERVERS: usize = 128;
    const PLAYERS: usize = 4096;
    let samples = std::env::args()
        .nth(1)
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(200);
    if samples == 0 {
        return Err("sample count must be positive".into());
    }
    let mut config = Config::from_addresses("127.0.0.1:0", "127.0.0.1:25565")?;
    let backend = config.backends["default"].clone();
    let definition = ManagedServer {
        command: vec!["unused".into()],
        directory: std::env::temp_dir(),
        autostart: false,
        start_on_connect: true,
        idle_timeout: None,
        start_timeout: std::time::Duration::from_secs(1),
        stop_timeout: std::time::Duration::from_secs(1),
        restart_delay: std::time::Duration::from_secs(1),
        restart_retries: 0,
    };
    let names: Vec<_> = (0..SERVERS)
        .map(|index| format!("server_{index:03}"))
        .collect();
    for name in &names {
        config.backends.insert(name.clone(), backend.clone());
        config
            .managed_servers
            .insert(name.clone(), definition.clone());
    }
    let players = Arc::new(PlayerRegistry::default());
    let registrations: Vec<_> = (0..PLAYERS)
        .map(|index| {
            players.register(
                (index as u128).to_be_bytes(),
                format!("Player{index}"),
                &names[index % SERVERS],
            )
        })
        .collect::<Result<_, _>>()?;
    let manager = ManagedServers::new(&config, players);
    let snapshot = manager.snapshot();
    assert_eq!(snapshot.len(), SERVERS);
    assert!(
        snapshot
            .iter()
            .all(|server| server.players == PLAYERS / SERVERS)
    );
    for _ in 0..10 {
        black_box(manager.snapshot());
    }
    let mut timings = Vec::with_capacity(samples);
    for _ in 0..samples {
        let start = Instant::now();
        black_box(manager.snapshot());
        timings.push(start.elapsed().as_nanos());
    }
    timings.sort_unstable();
    let percentile = |percent| timings[(samples - 1) * percent / 100] as f64 / 1000.0;
    println!(
        "managed snapshot ({SERVERS} servers, {PLAYERS} players, {samples} samples): p50={:.3} us p95={:.3} us p99={:.3} us",
        percentile(50),
        percentile(95),
        percentile(99),
    );
    black_box(&registrations);
    Ok(())
}
