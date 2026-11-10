//! What a WebAssembly plugin may touch, from its manifest's `[capabilities]`:
//!
//! - `filesystem`: `data` preopens the plugin's `data/` directory as `/data`
//!   (created if missing) read-write, `host` preopens the configuration
//!   directory as `/host` read-only, and an absolute path is preopened
//!   read-write at the same guest path (configuration loading refuses one
//!   that covers the plugin cache or the plugins folder). Nothing else of
//!   the host is visible. `host` is read-only so that no plugin can rewrite
//!   the configuration, other plugins or the compiled component cache.
//! - `network`: TCP, UDP and DNS through `wasi:sockets`, and outbound HTTP
//!   through `wasi:http`. Without it every socket address is refused, name
//!   lookups fail and `wasi:http` requests fail with `HTTP-request-denied`.
//!   The imports stay linked either way so one component loads under both.
//! - `env`: Rift's whole environment, or only the listed variables that are
//!   set.
//!
//! Stdin is empty. Stdout and stderr never reach Rift's own streams: they are
//! split into lines and written to the plugin log.
use super::bindings::{CONFIG_DIR, DATA_DIR};
use crate::plugins::{PluginSpec, api::Level, manifest::Filesystem, services::Services};
use bytes::Bytes;
use std::{
    io,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};
use tokio::io::AsyncWrite;
use wasmtime_wasi::{
    FsPerms, WasiCtx, WasiCtxBuilder,
    cli::{IsTerminal, StdoutStream},
    p2::{OutputStream, Pollable, StreamResult},
};
use wasmtime_wasi_http::{Error as HttpError, RequestOptions, WasiBody, WasiHttpHooks};

/// A line longer than this is split; the log truncates further anyway.
const MAX_LINE: usize = 4096;
/// How much one `write` may hand over; output is never back-pressured.
const PERMIT: usize = 64 * 1024;

/// The WASI context for one instance. Fails when a granted directory cannot
/// be opened.
pub(super) fn context(
    spec: &PluginSpec,
    stdout: LogPipe,
    stderr: LogPipe,
) -> Result<WasiCtx, String> {
    let capabilities = &spec.manifest.capabilities;
    let mut builder = WasiCtxBuilder::new();
    builder
        .allow_blocking_current_thread(true)
        .stdout(stdout)
        .stderr(stderr)
        .arg(&spec.name);
    for (host, guest) in preopens(spec) {
        if guest == DATA_DIR {
            std::fs::create_dir_all(&host)
                .map_err(|error| format!("cannot create {}: {error}", host.display()))?;
        }
        let permissions = if guest == CONFIG_DIR {
            FsPerms::ReadOnly
        } else {
            FsPerms::ReadWrite
        };
        builder
            .preopened_dir(&host, &guest, permissions)
            .map_err(|error| {
                format!(
                    "capabilities.filesystem: cannot open {} as {guest}: {error}",
                    host.display()
                )
            })?;
    }
    builder
        .allow_tcp(capabilities.network)
        .allow_udp(capabilities.network)
        .allow_ip_name_lookup(capabilities.network);
    if capabilities.network {
        builder.inherit_network();
    }
    match &capabilities.env {
        // "The whole environment" means Rift's environment minus Rift's own
        // secrets, as for managed servers; a manifest that lists one of them
        // by name still gets it.
        None => {
            for (name, value) in std::env::vars() {
                if !crate::environment::is_protected(&name) {
                    builder.env(name, value);
                }
            }
        }
        Some(names) => {
            for name in names {
                if let Ok(value) = std::env::var(name) {
                    builder.env(name, value);
                }
            }
        }
    }
    Ok(builder.build())
}

/// Host directory and guest path of every filesystem grant.
pub(super) fn preopens(spec: &PluginSpec) -> Vec<(std::path::PathBuf, String)> {
    spec.manifest
        .capabilities
        .filesystem
        .iter()
        .map(|grant| match grant {
            Filesystem::Data => (spec.data_dir(), DATA_DIR.to_owned()),
            Filesystem::Host => (spec.config_root.clone(), CONFIG_DIR.to_owned()),
            Filesystem::Path(path) => (path.clone(), path.to_string_lossy().into_owned()),
        })
        .collect()
}

/// `wasi:http` hooks: Wasmtime's client with `network`, a refusal without.
pub(super) struct Http {
    pub network: bool,
}

impl WasiHttpHooks for Http {
    fn send_request(
        &mut self,
        request: http::Request<WasiBody>,
        options: Option<RequestOptions>,
        fut: Box<dyn Future<Output = Result<(), HttpError>> + Send>,
    ) -> Box<
        dyn Future<
                Output = Result<
                    (
                        http::Response<WasiBody>,
                        Box<dyn Future<Output = Result<(), HttpError>> + Send>,
                    ),
                    HttpError,
                >,
            > + Send,
    > {
        drop(fut);
        if !self.network {
            return Box::new(async { Err(HttpError::HttpRequestDenied) });
        }
        Box::new(async move {
            use http_body_util::BodyExt;
            let (response, io) = wasmtime_wasi_http::default_send_request(request, options).await?;
            Ok((
                response.map(BodyExt::boxed_unsync),
                Box::new(io) as Box<dyn Future<Output = _> + Send>,
            ))
        })
    }
}

/// Every line written to a plugin log, as `<plugin>: <stream>: <line>`.
#[cfg(test)]
pub(super) static CAPTURED: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Guest stdout or stderr, written to the plugin log line by line with a
/// `stdout:` or `stderr:` prefix. Cheap to clone; clones share the buffer.
#[derive(Clone)]
pub(super) struct LogPipe(Arc<Pipe>);

struct Pipe {
    services: Services,
    label: &'static str,
    level: Level,
    pending: Mutex<Vec<u8>>,
}

impl LogPipe {
    pub(super) fn new(services: Services, label: &'static str, level: Level) -> Self {
        Self(Arc::new(Pipe {
            services,
            label,
            level,
            pending: Mutex::default(),
        }))
    }

    fn emit(&self, line: &[u8]) {
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        let line = format!("{}: {}", self.0.label, String::from_utf8_lossy(line));
        #[cfg(test)]
        CAPTURED
            .lock()
            .unwrap()
            .push(format!("{}: {line}", self.0.services.meta().name()));
        self.0.services.log(self.0.level, &line);
    }

    fn push(&self, bytes: &[u8]) {
        let mut pending = self.0.pending.lock().unwrap();
        pending.extend_from_slice(bytes);
        let mut start = 0;
        while let Some(end) = pending[start..].iter().position(|b| *b == b'\n') {
            self.emit(&pending[start..start + end]);
            start += end + 1;
        }
        while pending.len() - start >= MAX_LINE {
            self.emit(&pending[start..start + MAX_LINE]);
            start += MAX_LINE;
        }
        pending.drain(..start);
    }

    /// Write out an unterminated last line.
    pub(super) fn finish(&self) {
        let rest = std::mem::take(&mut *self.0.pending.lock().unwrap());
        if !rest.is_empty() {
            self.emit(&rest);
        }
    }
}

impl IsTerminal for LogPipe {
    fn is_terminal(&self) -> bool {
        false
    }
}

impl StdoutStream for LogPipe {
    fn async_stream(&self) -> Box<dyn AsyncWrite + Send + Sync> {
        Box::new(self.clone())
    }

    fn p2_stream(&self) -> Box<dyn OutputStream> {
        Box::new(self.clone())
    }
}

impl OutputStream for LogPipe {
    fn write(&mut self, bytes: Bytes) -> StreamResult<()> {
        self.push(&bytes);
        Ok(())
    }

    fn flush(&mut self) -> StreamResult<()> {
        Ok(())
    }

    fn check_write(&mut self) -> StreamResult<usize> {
        Ok(PERMIT)
    }
}

#[wasmtime_wasi::async_trait]
impl Pollable for LogPipe {
    async fn ready(&mut self) {}
}

impl AsyncWrite for LogPipe {
    fn poll_write(
        self: Pin<&mut Self>,
        _: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<io::Result<usize>> {
        self.push(bytes);
        Poll::Ready(Ok(bytes.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}
