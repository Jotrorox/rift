//! Sandboxed HTTP extensions with owned inputs and bounded execution.
//!
//! Each request evaluates the current immutable Lua source in a fresh VM.
//! No Lua handles cross threads, and HTTP script capacity survives reloads.

use std::{collections::BTreeMap, fmt, sync::Arc, time::Instant};

use mlua::{Function, Lua, Table, Value};
use tokio::{sync::Semaphore, task::spawn_blocking, time::timeout};

pub const MAX_HTTP_BODY_BYTES: usize = 256 * 1024;
static SLOTS: Semaphore = Semaphore::const_new(crate::script::MAX_CONCURRENT);

/// Request metadata plus a read-only snapshot of runtime status in `context`.
/// Authentication headers should be removed by the HTTP adapter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub path: String,
    pub query: String,
    pub body: String,
    pub headers: BTreeMap<String, String>,
    pub context: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub content_type: String,
    pub body: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HttpError {
    Busy,
    TimedOut,
    Script(String),
}

impl fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Busy => formatter.write_str("HTTP script capacity exhausted"),
            Self::TimedOut => formatter.write_str("HTTP script deadline exceeded"),
            Self::Script(message) => write!(formatter, "{message:.2048}"),
        }
    }
}

impl std::error::Error for HttpError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpScript {
    source: Arc<str>,
    name: Arc<str>,
}

impl HttpScript {
    pub(crate) fn new(source: &str, name: &str) -> Self {
        Self {
            source: source.into(),
            name: name.into(),
        }
    }

    /// Run with process-wide admission control on a blocking worker. `None`
    /// means that the script declined the request (HTTP 404 at the adapter).
    pub async fn execute(&self, request: HttpRequest) -> Result<Option<HttpResponse>, HttpError> {
        self.execute_inner(request, None).await
    }

    pub async fn execute_with_messaging(
        &self,
        request: HttpRequest,
        broker: crate::messaging::Broker,
    ) -> Result<Option<HttpResponse>, HttpError> {
        self.execute_inner(request, Some(broker)).await
    }

    async fn execute_inner(
        &self,
        request: HttpRequest,
        messaging: Option<crate::messaging::Broker>,
    ) -> Result<Option<HttpResponse>, HttpError> {
        let permit = SLOTS.try_acquire().map_err(|_| HttpError::Busy)?;
        let script = self.clone();
        let deadline = Instant::now() + crate::script::EXECUTION_TIMEOUT;
        let task = spawn_blocking(move || {
            // Retain capacity until the VM stops, even after caller cancellation.
            let _permit = permit;
            script.evaluate_inner(&request, deadline, messaging)
        });
        timeout(crate::script::EXECUTION_TIMEOUT, task)
            .await
            .map_err(|_| HttpError::TimedOut)?
            .map_err(|error| HttpError::Script(format!("HTTP script worker: {error}")))?
    }

    /// Synchronous embedding interface. Async callers should use `execute` to
    /// avoid blocking executor threads and to share concurrency admission.
    pub fn evaluate(
        &self,
        request: &HttpRequest,
        deadline: Instant,
    ) -> Result<Option<HttpResponse>, HttpError> {
        self.evaluate_inner(request, deadline, None)
    }

    fn evaluate_inner(
        &self,
        request: &HttpRequest,
        deadline: Instant,
        messaging: Option<crate::messaging::Broker>,
    ) -> Result<Option<HttpResponse>, HttpError> {
        let run = || -> mlua::Result<Option<HttpResponse>> {
            let input_bytes = request.method.len()
                + request.path.len()
                + request.query.len()
                + request
                    .headers
                    .iter()
                    .map(|(key, value)| key.len() + value.len())
                    .sum::<usize>();
            if request.body.len() > MAX_HTTP_BODY_BYTES || input_bytes > MAX_HTTP_BODY_BYTES {
                return Err(mlua::Error::runtime("HTTP request exceeds 256 KiB limit"));
            }
            let (lua, root) = crate::script::load(&self.source, &self.name, deadline)?;
            crate::script::install_messaging(&lua, messaging, deadline)?;
            let Value::Table(root) = root else {
                return Err(mlua::Error::runtime("configuration must return a table"));
            };
            let hook: Function = root.raw_get("on_http")?;
            let input = lua.create_table()?;
            input.raw_set("method", request.method.as_str())?;
            input.raw_set("path", request.path.as_str())?;
            input.raw_set("query", request.query.as_str())?;
            input.raw_set("body", request.body.as_str())?;
            let headers = lua.create_table()?;
            for (key, value) in &request.headers {
                headers.raw_set(key.as_str(), value.as_str())?;
            }
            input.raw_set("headers", headers)?;
            input.raw_set("context", json_value(&lua, &request.context, 0, deadline)?)?;
            let result = response(hook.call::<Value>(input)?)?;
            crate::script::check_deadline(deadline)?;
            Ok(result)
        };
        run().map_err(|error| {
            let message = format!("{}: on_http: {error}", self.name);
            HttpError::Script(message.chars().take(2048).collect())
        })
    }
}

fn json_value(
    lua: &Lua,
    value: &serde_json::Value,
    depth: usize,
    deadline: Instant,
) -> mlua::Result<Value> {
    crate::script::check_deadline(deadline)?;
    if depth > 64 {
        return Err(mlua::Error::runtime("HTTP context exceeds nesting limit"));
    }
    Ok(match value {
        serde_json::Value::Null => Value::Nil,
        serde_json::Value::Bool(value) => Value::Boolean(*value),
        serde_json::Value::Number(value) => {
            if let Some(value) = value.as_i64() {
                Value::Integer(value)
            } else {
                Value::Number(
                    value
                        .as_f64()
                        .ok_or_else(|| mlua::Error::runtime("invalid context number"))?,
                )
            }
        }
        serde_json::Value::String(value) => Value::String(lua.create_string(value)?),
        serde_json::Value::Array(values) => {
            let table = lua.create_table()?;
            for (index, value) in values.iter().enumerate() {
                table.raw_set(index + 1, json_value(lua, value, depth + 1, deadline)?)?;
            }
            Value::Table(table)
        }
        serde_json::Value::Object(values) => {
            let table = lua.create_table()?;
            for (key, value) in values {
                table.raw_set(key.as_str(), json_value(lua, value, depth + 1, deadline)?)?;
            }
            Value::Table(table)
        }
    })
}

fn response(value: Value) -> mlua::Result<Option<HttpResponse>> {
    if value.is_nil() {
        return Ok(None);
    }
    let Value::Table(table) = value else {
        return Err(mlua::Error::runtime(
            "expected nil or an HTTP response table",
        ));
    };
    for pair in table.clone().pairs::<Value, Value>() {
        let (key, _) = pair?;
        match key {
            Value::String(key)
                if matches!(key.to_str()?.as_ref(), "status" | "content_type" | "body") => {}
            _ => return Err(mlua::Error::runtime("unknown HTTP response field")),
        }
    }
    let status = match table.raw_get::<Value>("status")? {
        Value::Nil => 200,
        Value::Integer(status) if (200..=599).contains(&status) => status as u16,
        Value::Number(status) if (200.0..=599.0).contains(&status) && status.fract() == 0.0 => {
            status as u16
        }
        _ => {
            return Err(mlua::Error::runtime(
                "HTTP response status must be an integer in 200..=599",
            ));
        }
    };
    let content_type = match table.raw_get::<Value>("content_type")? {
        Value::Nil => "text/plain; charset=utf-8".to_owned(),
        Value::String(value) => value.to_str()?.to_owned(),
        _ => return Err(mlua::Error::runtime("HTTP content_type must be a string")),
    };
    if !valid_content_type(&content_type) {
        return Err(mlua::Error::runtime(
            "HTTP content_type must be a valid MIME type of at most 256 bytes",
        ));
    }
    let body = response_body(&table)?;
    if matches!(status, 204 | 205 | 304) && !body.is_empty() {
        return Err(mlua::Error::runtime(
            "HTTP response status requires an empty body",
        ));
    }
    Ok(Some(HttpResponse {
        status,
        content_type,
        body,
    }))
}

fn response_body(table: &Table) -> mlua::Result<String> {
    match table.raw_get::<Value>("body")? {
        Value::String(body) if body.as_bytes().len() <= MAX_HTTP_BODY_BYTES => {
            Ok(body.to_str()?.to_owned())
        }
        _ => Err(mlua::Error::runtime(
            "HTTP response body must be a UTF-8 string of at most 256 KiB",
        )),
    }
}

fn mime_token(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&byte))
}

fn valid_content_type(value: &str) -> bool {
    if value.len() > 256
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return false;
    }
    let mut segments = value.split(';');
    let Some((kind, subtype)) = segments
        .next()
        .and_then(|segment| segment.trim().split_once('/'))
    else {
        return false;
    };
    if !mime_token(kind) || !mime_token(subtype) || kind == "*" || subtype == "*" {
        return false;
    }
    segments.all(|segment| {
        let Some((name, value)) = segment.trim().split_once('=') else {
            return false;
        };
        mime_token(name.trim()) && (mime_token(value.trim()) || valid_quoted(value.trim()))
    })
}

fn valid_quoted(value: &str) -> bool {
    let Some(value) = value
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
    else {
        return false;
    };
    let mut escaped = false;
    for byte in value.bytes() {
        if escaped {
            escaped = false;
        } else if byte == b'\\' {
            escaped = true;
        } else if byte == b'"' {
            return false;
        }
    }
    !escaped
}

#[cfg(test)]
mod tests;
