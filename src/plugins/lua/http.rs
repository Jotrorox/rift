//! `rift.http.request(url, options)` for persistent Lua plugins: any HTTP or
//! HTTPS URL, no integration allow-list. Requests run on a small Tokio
//! runtime owned by the plugin, created on first use, and never outlive the
//! running call's deadline.
use super::bind::State;
use mlua::{Lua, Table, Value};
use reqwest::{
    Client, Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use std::{cell::RefCell, rc::Rc, time::Duration, time::Instant};

const MAX_BODY: usize = 16 * 1024 * 1024;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_TIMEOUT_MS: u64 = 60_000;
const MAX_REDIRECTS: usize = 5;

fn error(message: impl std::fmt::Display) -> mlua::Error {
    mlua::Error::runtime(message)
}

struct Http {
    runtime: tokio::runtime::Runtime,
    client: Client,
}

fn method(value: Value) -> mlua::Result<Method> {
    Ok(match value {
        Value::Nil => Method::GET,
        Value::String(name) => match name.to_str()?.as_ref() {
            "GET" => Method::GET,
            "POST" => Method::POST,
            "PUT" => Method::PUT,
            "PATCH" => Method::PATCH,
            "DELETE" => Method::DELETE,
            "HEAD" => Method::HEAD,
            "OPTIONS" => Method::OPTIONS,
            _ => return Err(error("unsupported HTTP method")),
        },
        _ => return Err(error("HTTP method must be a string")),
    })
}

fn headers(value: Value) -> mlua::Result<HeaderMap> {
    let mut headers = HeaderMap::new();
    match value {
        Value::Nil => {}
        Value::Table(table) => {
            for pair in table.pairs::<Value, Value>() {
                let (Value::String(name), Value::String(value)) = pair? else {
                    return Err(error("HTTP headers must map names to strings"));
                };
                let name = HeaderName::from_bytes(&name.as_bytes())
                    .map_err(|_| error("invalid HTTP header name"))?;
                let value = HeaderValue::from_bytes(&value.as_bytes())
                    .map_err(|_| error("invalid HTTP header value"))?;
                headers.append(name, value);
            }
        }
        _ => return Err(error("HTTP headers must be a table")),
    }
    Ok(headers)
}

pub(super) fn install(
    lua: &Lua,
    rift: &Table,
    state: &Rc<State>,
    network: bool,
) -> mlua::Result<()> {
    let http = lua.create_table()?;
    let client: RefCell<Option<Http>> = RefCell::new(None);
    let state = state.clone();
    http.raw_set(
        "request",
        lua.create_function(move |lua, (url, options): (String, Option<Table>)| {
            state.check()?;
            if !network {
                return Err(error(
                    "rift.http: network access is not granted (capabilities.network = false)",
                ));
            }
            let url = Url::parse(&url).map_err(|_| error("rift.http: invalid URL"))?;
            if !matches!(url.scheme(), "http" | "https") {
                return Err(error("rift.http: only http and https URLs are supported"));
            }
            let options = match options {
                Some(options) => options,
                None => lua.create_table()?,
            };
            for pair in options.clone().pairs::<Value, Value>() {
                let (key, _) = pair?;
                let known = matches!(&key, Value::String(key)
                    if matches!(key.to_str()?.as_ref(), "method" | "body" | "headers" | "timeout_ms"));
                if !known {
                    return Err(error(
                        "rift.http: options are method, body, headers and timeout_ms",
                    ));
                }
            }
            let method = method(options.raw_get("method")?)?;
            let body = match options.raw_get::<Value>("body")? {
                Value::Nil => Vec::new(),
                Value::String(body) if body.as_bytes().len() <= MAX_BODY => body.as_bytes().to_vec(),
                Value::String(_) => return Err(error("HTTP request body exceeds 16 MiB")),
                _ => return Err(error("HTTP request body must be a string")),
            };
            let headers = headers(options.raw_get("headers")?)?;
            let timeout = match options.raw_get::<Value>("timeout_ms")? {
                Value::Nil => DEFAULT_TIMEOUT_MS,
                Value::Integer(ms) if (1..=MAX_TIMEOUT_MS as i64).contains(&ms) => ms as u64,
                _ => {
                    return Err(error(format!(
                        "HTTP timeout_ms must be an integer in 1..={MAX_TIMEOUT_MS}"
                    )));
                }
            };
            // Never wait past the running call's deadline.
            let mut budget = Duration::from_millis(timeout);
            if let Some(deadline) = state.deadline.get() {
                budget = budget.min(deadline.saturating_duration_since(Instant::now()));
            }
            if budget.is_zero() {
                return Err(error("rift.http: the call's deadline has passed"));
            }
            let mut client = client.borrow_mut();
            if client.is_none() {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| self::error(format!("rift.http: {error}")))?;
                let built = Client::builder()
                    .redirect(reqwest::redirect::Policy::limited(MAX_REDIRECTS))
                    .build()
                    .map_err(|_| error("rift.http: could not initialize the HTTP client"))?;
                *client = Some(Http {
                    runtime,
                    client: built,
                });
            }
            let Http { runtime, client } = client.as_ref().unwrap();
            let (status, response_headers, body) = runtime.block_on(async {
                tokio::time::timeout(budget, async {
                    let mut response = client
                        .request(method, url)
                        .headers(headers)
                        .body(body)
                        .send()
                        .await
                        .map_err(|err| error(format!("HTTP request failed: {}", err.without_url())))?;
                    if response
                        .content_length()
                        .is_some_and(|length| length > MAX_BODY as u64)
                    {
                        return Err(error("HTTP response body exceeds 16 MiB"));
                    }
                    let status = response.status().as_u16();
                    let headers = response.headers().clone();
                    let mut body = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(|err| {
                        error(format!("HTTP response failed: {}", err.without_url()))
                    })? {
                        if body.len() + chunk.len() > MAX_BODY {
                            return Err(error("HTTP response body exceeds 16 MiB"));
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok((status, headers, body))
                })
                .await
                .map_err(|_| error("HTTP request timed out"))?
            })?;
            let result = lua.create_table()?;
            result.raw_set("status", status)?;
            result.raw_set("body", lua.create_string(&body)?)?;
            let headers = lua.create_table()?;
            for name in response_headers.keys() {
                let mut combined = Vec::new();
                for value in response_headers.get_all(name) {
                    if !combined.is_empty() {
                        combined.extend_from_slice(b", ");
                    }
                    combined.extend_from_slice(value.as_bytes());
                }
                headers.raw_set(name.as_str(), lua.create_string(&combined)?)?;
            }
            result.raw_set("headers", headers)?;
            Ok(result)
        })?,
    )?;
    rift.raw_set("http", http)
}
