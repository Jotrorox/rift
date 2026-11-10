//! How registries map between two adjacent protocols, for writing a pair's
//! `data/rules/<older>-<newer>.json`:
//!
//! ```sh
//! cargo run -p rift-translation --example mapping_report -- 754 755 [registry]
//! cargo run -p rift-translation --example mapping_report -- --all
//! ```
//!
//! For each direction and registry: how many entries are identical, renamed,
//! stood in for, mapped to the closest block state or unmapped, then the
//! unmapped names. `--all` prints only the unmapped counts of every pair.
use rift_translation::{
    mapping::{self, Counts},
    version::{self, PROTOCOLS},
};
use std::process::ExitCode;

fn main() -> ExitCode {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    match arguments.as_slice() {
        [all] if all == "--all" => {
            summary();
            ExitCode::SUCCESS
        }
        [first, second, rest @ ..] if rest.len() <= 1 => {
            let (Ok(first), Ok(second)) = (first.parse::<i32>(), second.parse::<i32>()) else {
                return usage();
            };
            if !mapping::adjacent(first, second) {
                eprintln!("{first} and {second} are not adjacent protocols");
                return ExitCode::FAILURE;
            }
            let (older, newer) = (first.min(second), first.max(second));
            pair(older, newer, rest.first().map(String::as_str));
            ExitCode::SUCCESS
        }
        _ => usage(),
    }
}

fn usage() -> ExitCode {
    eprintln!("usage: mapping_report <protocol> <adjacent protocol> [registry] | --all");
    ExitCode::FAILURE
}

fn line(registry: &str, counts: Counts) -> String {
    format!(
        "  {registry:<36} identical {:>6}  renamed {:>5}  stand-in {:>5}  closest {:>6}  unmapped {:>5}",
        counts.identical, counts.renamed, counts.stand_in, counts.closest, counts.unmapped
    )
}

fn pair(older: i32, newer: i32, only: Option<&str>) {
    let registries = mapping::shared_registries(older, newer);
    for (from, to, label) in [(older, newer, "up"), (newer, older, "down")] {
        println!(
            "{} ({from}) -> {} ({to}), `{label}` rules:",
            version::release(from),
            version::release(to)
        );
        let mut unmapped = Vec::new();
        for registry in registries
            .iter()
            .filter(|registry| only.is_none_or(|only| only == registry.as_str()))
        {
            let Some(mapping) = mapping::try_mapping(from, to, registry) else {
                println!("  {registry:<36} no mapping");
                continue;
            };
            println!("{}", line(registry, mapping.counts()));
            let names = mapping::unmapped(from, to, registry);
            if !names.is_empty() {
                unmapped.push((registry, names));
            }
        }
        for (registry, names) in unmapped {
            println!("  unmapped {registry}:");
            for (id, name) in names {
                println!("    {id:>6} {name}");
            }
        }
        println!();
    }
}

fn summary() {
    let mut total = 0;
    for window in PROTOCOLS.windows(2) {
        let (older, newer) = (window[0].0, window[1].0);
        for (from, to) in [(older, newer), (newer, older)] {
            let mut parts = Vec::new();
            for registry in mapping::shared_registries(older, newer) {
                let count = mapping::try_mapping(from, to, &registry)
                    .map_or(0, |mapping| mapping.counts().unmapped);
                if count > 0 {
                    parts.push(format!("{registry} {count}"));
                    total += count;
                }
            }
            println!(
                "{:>7} -> {:<7} {}",
                version::release(from),
                version::release(to),
                if parts.is_empty() {
                    "complete".to_owned()
                } else {
                    parts.join(", ")
                }
            );
        }
    }
    println!("{total} unmapped entries in total");
}
