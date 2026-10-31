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
use rift::node::protocol::{self, Files};
use serde_json::json;
use std::{fs, io, path::Path as FsPath};

/// Uploads and downloads both fit comfortably in memory.
pub(super) const MAX_FILE: usize = 64 * 1024 * 1024;

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
            Some(server) => {
                if let Some(node) = &server.node {
                    let request = Files {
                        server: name.clone(),
                        directory: server.directory.clone(),
                        method: method.to_string(),
                        path: relative,
                        folder: query.as_deref() == Some("directory"),
                        content: (method == Method::PUT).then(|| protocol::encode_base64(&body)),
                    };
                    let nodes = snapshot.managed.nodes().clone();
                    let node = node.clone();
                    drop(snapshot);
                    return remote(nodes, node, request, method, body.len()).await;
                }
                server.directory.clone()
            }
            None => return error(StatusCode::NOT_FOUND, format!("unknown server {name:?}")),
        },
    };
    let templates = matches!(root, Root::Templates);
    drop(snapshot);
    let directory = query.as_deref() == Some("directory");
    let manifest = query.as_deref() == Some("manifest");
    let result = tokio::task::spawn_blocking(move || -> io::Result<Response> {
        let segments = segments(&relative)?;
        let target = resolve(&base, &segments)?;
        match method {
            Method::GET if manifest && templates => {
                Ok(super::Json(rift::files::manifest(&target)?).into_response())
            }
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
                let value = rift::files::write(&base, &segments, &target, directory, &body)?;
                Ok(if directory {
                    (StatusCode::CREATED, super::Json(value)).into_response()
                } else {
                    super::Json(value).into_response()
                })
            }
            Method::DELETE => {
                Ok(super::Json(rift::files::delete(&target, &segments)?).into_response())
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
        Ok(Err(failure)) => failed(failure),
        Err(failure) => error(StatusCode::INTERNAL_SERVER_ERROR, failure),
    }
}

/// A server folder on a node, through its agent. Files are limited to what
/// fits a bus message.
async fn remote(
    nodes: rift::node::Nodes,
    node: String,
    request: Files,
    method: Method,
    size: usize,
) -> Response {
    if size > protocol::MAX_REMOTE_FILE {
        return error(
            StatusCode::PAYLOAD_TOO_LARGE,
            "files on nodes are limited to 512 KiB through the web console",
        );
    }
    let folder = request.folder;
    let segments = match segments(&request.path) {
        Ok(segments) => segments,
        Err(failure) => return failed(failure),
    };
    let result = nodes
        .request(
            &node,
            &protocol::Command::Files(request),
            std::time::Duration::from_secs(30),
        )
        .await;
    let value = match result {
        Ok(value) => value,
        Err(failure) => return failed(failure),
    };
    match (
        &method,
        value.get("content").and_then(|content| content.as_str()),
    ) {
        (&Method::GET | &Method::HEAD, Some(content)) => match protocol::decode_base64(content) {
            Some(contents) => download(&segments, contents),
            None => error(
                StatusCode::BAD_GATEWAY,
                "node returned invalid file contents",
            ),
        },
        (&Method::PUT, _) if folder => (StatusCode::CREATED, super::Json(value)).into_response(),
        _ => super::Json(value).into_response(),
    }
}

fn failed(failure: io::Error) -> Response {
    error(
        match failure.kind() {
            io::ErrorKind::NotFound => StatusCode::NOT_FOUND,
            io::ErrorKind::InvalidInput => StatusCode::BAD_REQUEST,
            io::ErrorKind::PermissionDenied => StatusCode::FORBIDDEN,
            io::ErrorKind::AlreadyExists | io::ErrorKind::IsADirectory => StatusCode::CONFLICT,
            io::ErrorKind::FileTooLarge => StatusCode::PAYLOAD_TOO_LARGE,
            io::ErrorKind::Unsupported => StatusCode::METHOD_NOT_ALLOWED,
            io::ErrorKind::NotConnected => StatusCode::CONFLICT,
            io::ErrorKind::TimedOut => StatusCode::GATEWAY_TIMEOUT,
            _ => StatusCode::INTERNAL_SERVER_ERROR,
        },
        failure,
    )
}

use rift::files::{protect, resolve, segments};

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
        return Ok(super::Json(rift::files::list(target, segments)?).into_response());
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
    Ok(download(segments, fs::read(target)?))
}

fn download(segments: &[String], contents: Vec<u8>) -> Response {
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
    (
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
        .into_response()
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message.into())
}
