//! Packs data/*.json into one gzip archive so the binary does not carry
//! megabytes of registry names as plain text. Layout of the unpacked archive:
//! a JSON object mapping each file's stem ("340", "rules/393-401", ...) to
//! its parsed content.
use flate2::{Compression, write::GzEncoder};
use std::{env, fs, io::Write, path::Path};

fn collect(root: &Path, directory: &Path, entries: &mut Vec<(String, String)>) {
    println!("cargo:rerun-if-changed={}", directory.display());
    let Ok(read) = fs::read_dir(directory) else {
        return;
    };
    let mut paths: Vec<_> = read.filter_map(|entry| Some(entry.ok()?.path())).collect();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect(root, &path, entries);
        } else if path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            println!("cargo:rerun-if-changed={}", path.display());
            let key = path
                .strip_prefix(root)
                .unwrap()
                .with_extension("")
                .to_string_lossy()
                .replace('\\', "/");
            let text = fs::read_to_string(&path).unwrap();
            entries.push((key, text));
        }
    }
}

fn main() {
    let root = Path::new(&env::var("CARGO_MANIFEST_DIR").unwrap()).join("data");
    let mut entries = Vec::new();
    collect(&root, &root, &mut entries);
    let mut archive = String::from("{");
    for (index, (key, text)) in entries.iter().enumerate() {
        if index > 0 {
            archive.push(',');
        }
        archive.push_str(&format!("{key:?}:{}", text.trim()));
    }
    archive.push('}');
    let out = Path::new(&env::var("OUT_DIR").unwrap()).join("data.json.gz");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
    encoder.write_all(archive.as_bytes()).unwrap();
    fs::write(out, encoder.finish().unwrap()).unwrap();
}
