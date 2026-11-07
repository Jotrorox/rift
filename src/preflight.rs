//! Checks for `rift check --strict` that look past the configuration text at
//! the host: whether files can be created and addresses can be bound.
use std::{
    fs, io,
    net::{SocketAddr, TcpListener, UdpSocket},
    path::Path,
};

/// Whether startup could create `path`: the nearest existing ancestor must be
/// a writable directory. Writability is probed with a temporary file that is
/// removed at once.
pub fn check_creatable(path: &Path) -> io::Result<()> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut ancestor = absolute.parent();
    while let Some(directory) = ancestor {
        match fs::metadata(directory) {
            Ok(metadata) if metadata.is_dir() => {
                let probe = directory.join(format!(".rift-check-{}", std::process::id()));
                fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&probe)
                    .map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!(
                                "{}: cannot create files in {}: {error}",
                                path.display(),
                                directory.display()
                            ),
                        )
                    })?;
                let _ = fs::remove_file(&probe);
                return Ok(());
            }
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!(
                        "{}: {} is not a directory",
                        path.display(),
                        directory.display()
                    ),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                ancestor = directory.parent();
            }
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("{}: no existing parent directory", path.display()),
    ))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    Tcp,
    Udp,
}

/// Bind `address` and release it at once.
pub fn check_bind(address: SocketAddr, transport: Transport) -> io::Result<()> {
    match transport {
        Transport::Tcp => TcpListener::bind(address).map(drop),
        Transport::Udp => UdpSocket::bind(address).map(drop),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creatable_paths_and_binds() {
        let root = std::env::temp_dir().join(format!("rift-preflight-{}", std::process::id()));
        fs::create_dir_all(&root).unwrap();
        check_creatable(&root.join("missing/nested/store.json")).unwrap();
        assert!(
            fs::read_dir(&root).unwrap().next().is_none(),
            "probe left behind"
        );
        let file = root.join("file");
        fs::write(&file, b"").unwrap();
        assert!(check_creatable(&file.join("store.json")).is_err());
        fs::remove_dir_all(&root).unwrap();

        let held = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = held.local_addr().unwrap();
        assert!(check_bind(address, Transport::Tcp).is_err());
        drop(held);
        check_bind(address, Transport::Tcp).unwrap();
        check_bind("127.0.0.1:0".parse().unwrap(), Transport::Udp).unwrap();
    }
}
