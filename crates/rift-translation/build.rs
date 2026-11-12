//! Compresses each data/*.json file on its own so the binary does not carry
//! megabytes of registry names as plain text, and so a program decompresses
//! and parses only the versions it uses. Writes `files.rs`, a table of each
//! file's stem ("340", "rules/393-401", ...) and its gzip bytes.
use flate2::{Compression, write::GzEncoder};
use std::{env, fmt::Write as _, fs, io::Write, path::Path};

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
    let out = Path::new(&env::var("OUT_DIR").unwrap()).to_path_buf();
    let mut entries = Vec::new();
    collect(&root, &root, &mut entries);
    let mut table = String::from("&[\n");
    for (index, (key, text)) in entries.iter().enumerate() {
        let mut encoder = GzEncoder::new(Vec::new(), Compression::best());
        encoder.write_all(text.as_bytes()).unwrap();
        let path = out.join(format!("data-{index}.json.gz"));
        fs::write(&path, encoder.finish().unwrap()).unwrap();
        writeln!(
            table,
            "    ({key:?}, include_bytes!({:?})),",
            path.display()
        )
        .unwrap();
    }
    table.push(']');
    fs::write(out.join("files.rs"), table).unwrap();
}
