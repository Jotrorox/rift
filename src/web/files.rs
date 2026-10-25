//! Template and server folders for the web console, like CloudNet's template
//! file access. Paths stay inside their root: `..`, absolute paths and
//! symlinks are refused, and Rift's ownership markers cannot be changed.
use super::{App, error};
use axum::{
    body::Bytes,
    extract::{Path, RawQuery, State},
    http::{HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde_json::json;
use std::{
    fs, io,
    path::{Path as FsPath, PathBuf},
    time::UNIX_EPOCH,
};

/// Uploads and downloads both fit comfortably in memory.
pub(super) const MAX_FILE: usize = 64 * 1024 * 1024;
const MAX_ENTRIES: usize = 2000;
const MAX_DEPTH: usize = 64;

enum Root {
    Templates,
    Server(String),
}

pub(super) async fn templates(
    State(app): State<App>,
    method: Method,
    RawQuery(query): RawQuery,
    path: Option<Path<String>>,
    body: Bytes,
) -> Response {
    let relative = path.map(|Path(path)| path).unwrap_or_default();
    serve(app, Root::Templates, method, relative, query, body).await
}

pub(super) async fn server(
    State(app): State<App>,
    method: Method,
    RawQuery(query): RawQuery,
    Path(path): Path<Vec<(String, String)>>,
    body: Bytes,
) -> Response {
    let mut name = String::new();
    let mut relative = String::new();
    for (key, value) in path {
        match key.as_str() {
            "name" => name = value,
            _ => relative = value,
        }
    }
    serve(app, Root::Server(name), method, relative, query, body).await
}

async fn serve(
    app: App,
    root: Root,
    method: Method,
    relative: String,
    query: Option<String>,
    body: Bytes,
) -> Response {
    let snapshot = app.snapshot();
    let base = match &root {
        Root::Templates => snapshot.config.template_storage.clone(),
        Root::Server(name) => match snapshot.config.managed_servers.get(name) {
            Some(server) => server.directory.clone(),
            None => return error(StatusCode::NOT_FOUND, format!("unknown server {name:?}")),
        },
    };
    let templates = matches!(root, Root::Templates);
    drop(snapshot);
    let directory = query.as_deref() == Some("directory");
    let result = tokio::task::spawn_blocking(move || -> io::Result<Response> {
        let segments = segments(&relative)?;
        let target = resolve(&base, &segments)?;
        match method {
            Method::GET | Method::HEAD => read(&target, &segments, templates),
            Method::PUT => {
                protect(&segments)?;
                if segments.is_empty() {
                    return Err(invalid("choose a path inside the folder"));
                }
                if templates {
                    fs::create_dir_all(&base)?;
                } else if !base.is_dir() {
                    return Err(io::Error::new(
                        io::ErrorKind::NotFound,
                        "the server directory does not exist yet",
                    ));
                }
                write(&base, &segments, &target, directory, &body)
            }
            Method::DELETE => {
                protect(&segments)?;
                if segments.is_empty() {
                    return Err(invalid("the folder root cannot be deleted"));
                }
                let metadata = fs::symlink_metadata(&target)?;
                if metadata.is_dir() {
                    fs::remove_dir_all(&target)?;
                } else {
                    fs::remove_file(&target)?;
                }
                Ok(super::Json(json!({"deleted": segments.join("/")})).into_response())
            }
            _ => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "use GET, PUT or DELETE",
            )),
        }
    })
    .await;
    match result {
        Ok(Ok(response)) => response,
        Ok(Err(failure)) => error(
            match failure.kind() {
                io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
                io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
                io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
                io::ErrorKind::AlreadyExists | io::ErrorKind::IsADirectory => StatusCode::CONFLICT,
                io::ErrorKind::FileTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
                io::ErrorKind::Unsupported => StatusCode::METHOD_NOT_ALLOWED,
                _ => StatusCode::INTERNAL_SERVER_ERROR,
            },
            failure,
        ),
        Err(failure) => error(StatusCode::INTERNAL_SERVER_ERROR, failure),
    }
}

fn segments(relative: &str) -> io::Result<Vec<String>> {
    if relative.len() > 4096 {
        return Err(invalid("path exceeds 4096 bytes"));
    }
    let segments: Vec<String> = relative
        .split('/')
        .filter(|segment| !segment.is_empty())
        .map(str::to_owned)
        .collect();
    if segments.len() > MAX_DEPTH {
        return Err(invalid("path is nested too deeply"));
    }
    for segment in &segments {
        if segment == "."
            || segment == ".."
            || segment.len() > 255
            || segment.contains(['\\', '\0', ':'])
            || segment.chars().any(char::is_control)
        {
            return Err(invalid(format!("invalid path segment {segment:?}")));
        }
    }
    Ok(segments)
}

/// Refuse symlinks at every existing level below the root.
fn resolve(base: &FsPath, segments: &[String]) -> io::Result<PathBuf> {
    let mut path = base.to_path_buf();
    for segment in segments {
        path.push(segment);
        match fs::symlink_metadata(&path) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "symlinks are not served",
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(path)
}

/// Rift's ownership markers and staging files prove instance ownership.
fn protect(segments: &[String]) -> io::Result<()> {
    if segments.iter().any(|segment| segment.starts_with(".rift-")) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "Rift-owned files cannot be changed from the web console",
        ));
    }
    Ok(())
}

fn read(target: &FsPath, segments: &[String], templates: bool) -> io::Result<Response> {
    let metadata = match fs::symlink_metadata(target) {
        // An unused template storage folder is simply empty.
        Err(error)
            if error.kind() == io::ErrorKind::NotFound && segments.is_empty() && templates =>
        {
            return Ok(
                super::Json(json!({"path":"","entries":[],"truncated":false})).into_response(),
            );
        }
        result => result?,
    };
    if metadata.is_dir() {
        let mut entries = Vec::new();
        let mut truncated = false;
        for entry in fs::read_dir(target)? {
            let entry = entry?;
            if entries.len() >= MAX_ENTRIES {
                truncated = true;
                break;
            }
            let metadata = entry.metadata()?;
            let kind = if metadata.file_type().is_symlink() {
                "symlink"
            } else if metadata.is_dir() {
                "directory"
            } else if metadata.is_file() {
                "file"
            } else {
                "other"
            };
            entries.push(json!({
                "name": entry.file_name().to_string_lossy(),
                "type": kind,
                "size": metadata.is_file().then_some(metadata.len()),
                "modified": metadata.modified().ok()
                    .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                    .map(|time| time.as_millis() as u64),
            }));
        }
        entries.sort_by(|left, right| {
            (left["type"] != "directory", left["name"].as_str())
                .cmp(&(right["type"] != "directory", right["name"].as_str()))
        });
        return Ok(super::Json(
            json!({"path": segments.join("/"), "entries": entries, "truncated": truncated}),
        )
        .into_response());
    }
    if !metadata.is_file() {
        return Err(invalid("only regular files and folders are served"));
    }
    if metadata.len() > MAX_FILE as u64 {
        return Err(io::Error::new(
            io::ErrorKind::FileTooLarge,
            "file exceeds the 64 MiB web console limit",
        ));
    }
    let contents = fs::read(target)?;
    let filename: String = segments
        .last()
        .map(|name| {
            name.chars()
                .filter(|c| c.is_ascii_graphic() && *c != '"')
                .collect()
        })
        .unwrap_or_default();
    let disposition = HeaderValue::from_str(&format!("attachment; filename=\"{filename}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment"));
    Ok((
        [
            (
                header::CONTENT_TYPE,
                HeaderValue::from_static("application/octet-stream"),
            ),
            (header::CONTENT_DISPOSITION, disposition),
            (
                header::X_CONTENT_TYPE_OPTIONS,
                HeaderValue::from_static("nosniff"),
            ),
        ],
        contents,
    )
        .into_response())
}

fn write(
    base: &FsPath,
    segments: &[String],
    target: &FsPath,
    directory: bool,
    body: &[u8],
) -> io::Result<Response> {
    // Create missing parents one level at a time, refusing symlinks raced in.
    let mut parent = base.to_path_buf();
    for segment in &segments[..segments.len() - if directory { 0 } else { 1 }] {
        parent.push(segment);
        match fs::symlink_metadata(&parent) {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::AlreadyExists,
                    format!("{segment} is not a folder"),
                ));
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir(&parent)?,
            Err(error) => return Err(error),
        }
    }
    let path = segments.join("/");
    if directory {
        return Ok((
            StatusCode::CREATED,
            super::Json(json!({"path": path, "type": "directory"})),
        )
            .into_response());
    }
    if fs::symlink_metadata(target).is_ok_and(|metadata| !metadata.is_file()) {
        return Err(io::Error::new(
            io::ErrorKind::IsADirectory,
            "a folder already exists at this path",
        ));
    }
    let mut random = [0; 8];
    aws_lc_rs::rand::fill(&mut random).map_err(|_| io::Error::other("random source failed"))?;
    let suffix: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
    let staged = target.with_file_name(format!(".rift-upload-{suffix}"));
    let result = (|| {
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&staged)?;
        io::Write::write_all(&mut file, body)?;
        file.sync_all()?;
        fs::rename(&staged, target)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&staged);
    }
    result?;
    Ok(super::Json(json!({"path": path, "type": "file", "size": body.len()})).into_response())
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_stay_inside_their_root() {
        assert_eq!(segments("").unwrap(), Vec::<String>::new());
        assert_eq!(segments("/a//b/").unwrap(), ["a", "b"]);
        for path in ["..", "a/../b", "./a", "a\\b", "c:", "a\0b", "a\nb"] {
            assert!(segments(path).is_err(), "{path}");
        }
        assert!(segments(&"a/".repeat(MAX_DEPTH + 1)).is_err());
        assert!(protect(&["world".into(), ".rift-instance.json".into()]).is_err());
        assert!(protect(&["plugins".into(), "game.jar".into()]).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn symlinks_are_refused_at_every_level() {
        let root = std::env::temp_dir().join(format!("rift-files-{}", std::process::id()));
        fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink("/", root.join("escape")).unwrap();
        resolve(&root, &["real".into(), "new.txt".into()]).unwrap();
        let error = resolve(&root, &["escape".into(), "etc".into()]).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        write(
            &root,
            &["real".into(), "nested".into(), "a.txt".into()],
            &root.join("real/nested/a.txt"),
            false,
            b"hi",
        )
        .unwrap();
        assert_eq!(fs::read(root.join("real/nested/a.txt")).unwrap(), b"hi");
        let error = write(
            &root,
            &["escape".into(), "x".into()],
            &root.join("escape/x"),
            false,
            b"",
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        fs::remove_dir_all(root).unwrap();
    }
}
