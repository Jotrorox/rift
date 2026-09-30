//! Named HTTP integrations with a fixed origin, path prefix, and execution budget.
use mlua::{Lua, Table, Value};
use reqwest::{
    Client, Method, Url,
    header::{HeaderMap, HeaderName, HeaderValue},
};
use std::{
    collections::BTreeMap,
    time::{Duration, Instant},
};

const MAX_BODY: usize = 64 * 1024;
const MAX_HEADERS: usize = 32;
const MAX_HEADER_BYTES: usize = 8192;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Integration {
    url: Url,
    timeout: Duration,
    headers: HeaderMap,
}

pub(super) type Integrations = BTreeMap<String, Integration>;

fn error(message: impl Into<String>) -> mlua::Error {
    mlua::Error::runtime(message.into())
}

fn fields(table: &Table, allowed: &[&str]) -> mlua::Result<()> {
    for entry in table.clone().pairs::<Value, Value>() {
        let (key, _) = entry?;
        let Value::String(key) = key else {
            return Err(error("HTTP integration fields must be named"));
        };
        if !allowed.contains(&key.to_str()?.as_ref()) {
            return Err(error("unknown HTTP integration field"));
        }
    }
    Ok(())
}

fn string(value: Value, label: &str) -> mlua::Result<String> {
    let Value::String(value) = value else {
        return Err(error(format!("{label} must be a string")));
    };
    Ok(value.to_str()?.to_owned())
}

fn check_headers(headers: &HeaderMap) -> mlua::Result<()> {
    if headers.len() > MAX_HEADERS
        || headers
            .iter()
            .map(|(key, value)| key.as_str().len() + value.as_bytes().len())
            .sum::<usize>()
            > MAX_HEADER_BYTES
    {
        return Err(error("HTTP headers exceed the 32 header / 8 KiB limit"));
    }
    Ok(())
}

fn headers(value: Value) -> mlua::Result<HeaderMap> {
    let mut result = HeaderMap::new();
    if value.is_nil() {
        return Ok(result);
    }
    let Value::Table(table) = value else {
        return Err(error("HTTP headers must be a table"));
    };
    let mut count = 0;
    let mut bytes = 0;
    for entry in table.pairs::<Value, Value>() {
        let (key, value) = entry?;
        let key = string(key, "HTTP header name")?;
        let value = string(value, "HTTP header value")?;
        count += 1;
        bytes += key.len() + value.len();
        if count > MAX_HEADERS || bytes > MAX_HEADER_BYTES {
            return Err(error("HTTP headers exceed the 32 header / 8 KiB limit"));
        }
        let key = HeaderName::from_bytes(key.as_bytes())
            .map_err(|_| error("invalid HTTP header name"))?;
        if matches!(
            key.as_str(),
            "host"
                | "connection"
                | "content-length"
                | "transfer-encoding"
                | "upgrade"
                | "te"
                | "trailer"
        ) || key.as_str().starts_with("proxy-")
        {
            return Err(error(
                "HTTP routing and framing headers cannot be overridden",
            ));
        }
        let value =
            HeaderValue::from_str(&value).map_err(|_| error("invalid HTTP header value"))?;
        if result.insert(key, value).is_some() {
            return Err(error("duplicate HTTP header name"));
        }
    }
    Ok(result)
}

pub(super) fn parse(value: Value) -> mlua::Result<Integrations> {
    let mut result = Integrations::new();
    if value.is_nil() {
        return Ok(result);
    }
    let Value::Table(table) = value else {
        return Err(error("extensions.integrations must be a table"));
    };
    for entry in table.pairs::<Value, Value>() {
        let (name, integration) = entry?;
        let name = string(name, "integration name")?;
        if !super::token(&name) || result.len() >= 16 {
            return Err(error("invalid or excessive HTTP integration name"));
        }
        let Value::Table(integration) = integration else {
            return Err(error("HTTP integration must be a table"));
        };
        fields(&integration, &["url", "timeout_ms", "headers"])?;
        let raw_url = string(integration.raw_get("url")?, "integration URL")?;
        if raw_url.len() > 4096 || raw_url.bytes().any(|byte| byte.is_ascii_whitespace()) {
            return Err(error("invalid integration URL"));
        }
        let mut url = Url::parse(&raw_url).map_err(|_| error("invalid integration URL"))?;
        if !matches!(url.scheme(), "http" | "https")
            || url.host_str().is_none()
            || !url.username().is_empty()
            || url.password().is_some()
            || raw_url.split_once("://").is_some_and(|(_, rest)| {
                rest.split(['/', '\\', '?', '#'])
                    .next()
                    .is_some_and(|authority| authority.contains('@'))
            })
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err(error(
                "integration URL must be HTTP(S), without credentials, query, or fragment",
            ));
        }
        if !url.path().ends_with('/') {
            url.set_path(&format!("{}/", url.path()));
        }
        let timeout_ms = match integration.raw_get::<Value>("timeout_ms")? {
            Value::Nil => 1000,
            Value::Integer(value) if (1..=5000).contains(&value) => value as u64,
            _ => {
                return Err(error(
                    "integration timeout_ms must be an integer from 1 to 5000",
                ));
            }
        };
        result.insert(
            name,
            Integration {
                url,
                timeout: Duration::from_millis(timeout_ms),
                headers: headers(integration.raw_get("headers")?)?,
            },
        );
    }
    Ok(result)
}

fn request_url(integration: &Integration, path: &str) -> mlua::Result<Url> {
    if path.len() > 4096
        || path.starts_with('/')
        || path.contains(['\\', '#'])
        || path
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte.is_ascii_control())
    {
        return Err(error(
            "HTTP path must be a relative path within the integration URL",
        ));
    }
    let pathname = path.split('?').next().unwrap_or_default();
    if pathname.split('/').any(|part| part == "." || part == "..")
        || pathname
            .split('/')
            .next()
            .is_some_and(|part| part.contains(':'))
    {
        return Err(error("HTTP path traversal and absolute URLs are forbidden"));
    }
    // Reject separators and nested encodings before a remote server can decode
    // them differently from the URL parser and escape the configured path.
    let mut bytes = pathname.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte == b'%' {
            let high = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            let low = bytes.next().and_then(|byte| (byte as char).to_digit(16));
            let (Some(high), Some(low)) = (high, low) else {
                return Err(error("invalid HTTP path encoding"));
            };
            let decoded = (high * 16 + low) as u8;
            if matches!(decoded, b'.' | b'/' | b'\\' | b'%') || decoded.is_ascii_control() {
                return Err(error("encoded HTTP path traversal is forbidden"));
            }
        }
    }
    let url = integration
        .url
        .join(path)
        .map_err(|_| error("invalid HTTP path"))?;
    if url.origin() != integration.url.origin()
        || !url.path().starts_with(integration.url.path())
        || url.fragment().is_some()
        || !url.username().is_empty()
        || url.password().is_some()
    {
        return Err(error("HTTP path escapes its configured integration"));
    }
    Ok(url)
}

pub(super) fn install(
    lua: &Lua,
    api: &Table,
    settings: &Integrations,
    deadline: Instant,
) -> mlua::Result<()> {
    let integrations = settings.clone();
    let handle = tokio::runtime::Handle::try_current().ok();
    let client = Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .build()
        .map_err(|_| error("could not initialize HTTP integration client"))?;
    let http = lua.create_table()?;
    http.set(
        "request",
        lua.create_function(move |lua, (name, options): (Value, Table)| {
            let name = string(name, "HTTP integration name")?;
            let integration = integrations
                .get(&name)
                .ok_or_else(|| error("unknown HTTP integration"))?;
            fields(&options, &["method", "path", "body", "headers"])?;
            let method = match options.raw_get::<Value>("method")? {
                Value::Nil => Method::GET,
                value => match string(value, "HTTP method")?.as_str() {
                    "GET" => Method::GET,
                    "POST" => Method::POST,
                    "PUT" => Method::PUT,
                    "PATCH" => Method::PATCH,
                    "DELETE" => Method::DELETE,
                    "HEAD" => Method::HEAD,
                    "OPTIONS" => Method::OPTIONS,
                    _ => return Err(error("unsupported HTTP method")),
                },
            };
            let path = match options.raw_get::<Value>("path")? {
                Value::Nil => String::new(),
                value => string(value, "HTTP path")?,
            };
            let url = request_url(integration, &path)?;
            let body = match options.raw_get::<Value>("body")? {
                Value::Nil => Vec::new(),
                Value::String(value) if value.as_bytes().len() <= MAX_BODY => {
                    value.as_bytes().to_vec()
                }
                Value::String(_) => return Err(error("HTTP request body exceeds 64 KiB")),
                _ => return Err(error("HTTP request body must be a string")),
            };
            let mut request_headers = integration.headers.clone();
            request_headers.extend(headers(options.raw_get("headers")?)?);
            check_headers(&request_headers)?;
            let budget = deadline
                .saturating_duration_since(Instant::now())
                .min(integration.timeout);
            if budget.is_zero() {
                return Err(error("HTTP integration execution deadline exceeded"));
            }
            let handle = handle
                .as_ref()
                .ok_or_else(|| error("HTTP integrations require a Tokio runtime"))?;
            let (status, response_headers, body) = handle.block_on(async {
                tokio::time::timeout(budget, async {
                    let mut response = client
                        .request(method, url)
                        .headers(request_headers)
                        .body(body)
                        .timeout(budget)
                        .send()
                        .await
                        .map_err(|err| {
                            error(format!("HTTP integration failed: {}", err.without_url()))
                        })?;
                    check_headers(response.headers())?;
                    if response
                        .content_length()
                        .is_some_and(|length| length > MAX_BODY as u64)
                    {
                        return Err(error("HTTP response body exceeds 64 KiB"));
                    }
                    let status = response.status().as_u16();
                    let headers = response.headers().clone();
                    let mut body = Vec::new();
                    while let Some(chunk) = response.chunk().await.map_err(|err| {
                        error(format!("HTTP response failed: {}", err.without_url()))
                    })? {
                        if body.len() + chunk.len() > MAX_BODY {
                            return Err(error("HTTP response body exceeds 64 KiB"));
                        }
                        body.extend_from_slice(&chunk);
                    }
                    Ok((status, headers, body))
                })
                .await
                .map_err(|_| error("HTTP integration timed out"))?
            })?;
            let result = lua.create_table()?;
            result.set("status", status)?;
            result.set("body", lua.create_string(&body)?)?;
            let headers = lua.create_table()?;
            for name in response_headers.keys() {
                let mut combined = Vec::new();
                for value in response_headers.get_all(name) {
                    if !combined.is_empty() {
                        combined.extend_from_slice(b", ");
                    }
                    combined.extend_from_slice(value.as_bytes());
                }
                headers.set(name.as_str(), lua.create_string(&combined)?)?;
            }
            result.set("headers", headers)?;
            Ok(result)
        })?,
    )?;
    api.set("http", http)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    fn config(lua: &Lua, expression: &str) -> mlua::Result<Integrations> {
        parse(lua.load(format!("return {expression}")).eval()?)
    }

    #[test]
    fn validates_configuration_and_header_limits() {
        let lua = Lua::new();
        assert!(config(&lua, "nil").unwrap().is_empty());
        let valid = config(
            &lua,
            "{service={url='https://example.com/api',headers={Authorization='Bearer token'}}}",
        )
        .unwrap();
        assert_eq!(valid["service"].url.as_str(), "https://example.com/api/");
        for expression in [
            "false",
            "{[1]={url='https://example.com'}}",
            "{service={url='file:///tmp/test'}}",
            "{service={url='https://user:pass@example.com'}}",
            "{service={url='https://@example.com'}}",
            "{service={url='https://example.com/?query'}}",
            "{service={url='https://example.com/#fragment'}}",
            "{service={url='https://example.com',unknown=true}}",
            "{service={url='https://example.com',timeout_ms=0}}",
            "{service={url='https://example.com',timeout_ms=5001}}",
            "{service={url='https://example.com',timeout_ms='1'}}",
            "{service={url='https://example.com',headers={Host='elsewhere'}}}",
            "{service={url='https://example.com',headers={['Content-Length']='10'}}}",
            "{service={url='https://example.com',headers={['X-Test']='a\\r\\nb'}}}",
            "{service={url='https://example.com',headers={['X-Test']='a',['x-test']='b'}}}",
            "{service={url='https://example.com',headers={['X-Test']=123}}}",
            "{service={url='https://example.com',headers={['X-Test']=string.rep('a',8192)}}}",
            "(function() local t={} for i=1,17 do t['s'..i]={url='https://example.com'} end return t end)()",
            "(function() local h={} for i=1,33 do h['x-'..i]='a' end return {service={url='https://example.com',headers=h}} end)()",
        ] {
            assert!(config(&lua, expression).is_err(), "{expression}");
        }
    }

    #[test]
    fn only_relative_paths_inside_configured_prefix_are_allowed() {
        let lua = Lua::new();
        let settings = config(&lua, "{service={url='https://example.com/api/'}}").unwrap();
        let service = &settings["service"];
        assert_eq!(
            request_url(service, "users/123?filter=a%2Fb")
                .unwrap()
                .as_str(),
            "https://example.com/api/users/123?filter=a%2Fb"
        );
        assert_eq!(request_url(service, "").unwrap(), service.url);
        for path in [
            "/api/users",
            "//other.example/",
            "https://other.example/",
            "http:other",
            "../secret",
            "users/../../secret",
            "./users",
            "users/..",
            "%2e%2e/secret",
            "..%2Fsecret",
            "%252e%252e/secret",
            "users%5c..",
            "users\\..",
            "users#fragment",
            " users",
            "users\n",
            "invalid%xx",
            "nul%00",
        ] {
            assert!(request_url(service, path).is_err(), "{path}");
        }
    }

    async fn serve(
        response: Vec<u8>,
        delay: Duration,
    ) -> (String, tokio::task::JoinHandle<Vec<u8>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/api/", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let read = socket.read(&mut chunk).await.unwrap();
                assert!(read > 0);
                request.extend_from_slice(&chunk[..read]);
                if let Some(end) = request.windows(4).position(|part| part == b"\r\n\r\n") {
                    let headers = String::from_utf8_lossy(&request[..end]);
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .and_then(|value| value.parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            tokio::time::sleep(delay).await;
            let _ = socket.write_all(&response).await;
            request
        });
        (url, task)
    }

    async fn call(
        url: String,
        timeout_ms: u64,
        budget: Duration,
        script: String,
    ) -> Result<(), String> {
        tokio::task::spawn_blocking(move || {
            let run = || -> mlua::Result<()> {
                let lua = Lua::new();
                let settings = config(&lua, &format!("{{service={{url='{url}',timeout_ms={timeout_ms},headers={{Authorization='Bearer token'}}}}}}"))?;
                let api = lua.create_table()?;
                install(&lua, &api, &settings, Instant::now() + budget)?;
                lua.globals().set("rift", api)?;
                lua.load(script).exec()
            };
            run().map_err(|error| error.to_string())
        }).await.unwrap()
    }

    #[tokio::test]
    async fn named_integration_sends_headers_and_body_and_returns_response() {
        let (url, server) = serve(b"HTTP/1.1 201 Created\r\nContent-Length: 5\r\nX-Reply: received\r\nConnection: close\r\n\r\nhello".to_vec(), Duration::ZERO).await;
        call(url, 1000, Duration::from_secs(2), "local r=rift.http.request('service',{method='POST',path='users?active=true',body='request body',headers={['X-Test']='test'}}); assert(r.status==201 and r.body=='hello' and r.headers['x-reply']=='received')".into()).await.unwrap();
        let request = String::from_utf8(server.await.unwrap()).unwrap();
        assert!(request.starts_with("POST /api/users?active=true HTTP/1.1\r\n"));
        assert!(request.contains("authorization: Bearer token\r\n"));
        assert!(request.contains("x-test: test\r\n"));
        assert!(request.ends_with("request body"));
    }

    #[tokio::test]
    async fn rejects_unknown_integrations_bad_requests_and_expired_deadlines() {
        for (script, expected) in [
            ("rift.http.request('other',{})", "unknown HTTP integration"),
            ("rift.http.request(123,{})", "name must be a string"),
            (
                "rift.http.request('service',{path='https://elsewhere.example'})",
                "absolute URLs are forbidden",
            ),
            (
                "rift.http.request('service',{method='CONNECT'})",
                "unsupported HTTP method",
            ),
            (
                "rift.http.request('service',{path='../escape'})",
                "path traversal",
            ),
            (
                "rift.http.request('service',{typo=true})",
                "unknown HTTP integration field",
            ),
            (
                "rift.http.request('service',{headers={Host='elsewhere'}})",
                "headers cannot be overridden",
            ),
            (
                "rift.http.request('service',{body=string.rep('a',65537)})",
                "request body exceeds 64 KiB",
            ),
            (
                "local h={} for i=1,32 do h['x-'..i]='a' end rift.http.request('service',{headers=h})",
                "32 header / 8 KiB limit",
            ),
        ] {
            let result = call(
                "http://127.0.0.1:1/api/".into(),
                1000,
                Duration::from_secs(1),
                script.into(),
            )
            .await
            .unwrap_err();
            assert!(result.contains(expected), "{script}: {result}");
        }
        let expired = call(
            "http://127.0.0.1:1/api/".into(),
            1000,
            Duration::ZERO,
            "rift.http.request('service',{})".into(),
        )
        .await
        .unwrap_err();
        assert!(expired.contains("deadline exceeded"), "{expired}");
    }

    #[tokio::test]
    async fn configured_timeout_and_enclosing_deadline_bound_network_wait() {
        for (timeout_ms, budget) in [
            (20, Duration::from_secs(1)),
            (1000, Duration::from_millis(20)),
        ] {
            let (url, server) = serve(
                b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n".to_vec(),
                Duration::from_millis(500),
            )
            .await;
            let result = tokio::time::timeout(
                Duration::from_millis(300),
                call(
                    url,
                    timeout_ms,
                    budget,
                    "rift.http.request('service',{})".into(),
                ),
            )
            .await
            .unwrap();
            assert!(result.is_err());
            server.abort();
        }
    }

    #[tokio::test]
    async fn redirects_are_returned_without_contacting_the_target() {
        let target = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let response = format!(
            "HTTP/1.1 302 Found\r\nLocation: http://{}/secret\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            target.local_addr().unwrap()
        );
        let (url, server) = serve(response.into_bytes(), Duration::ZERO).await;
        call(
            url,
            1000,
            Duration::from_secs(2),
            "assert(rift.http.request('service',{}).status==302)".into(),
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(30), target.accept())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn response_body_limits_cover_content_length_and_chunked_streams() {
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n10001\r\n{}\r\n0\r\n\r\n",
            "a".repeat(MAX_BODY + 1)
        );
        for response in [
            b"HTTP/1.1 200 OK\r\nContent-Length: 65537\r\nConnection: close\r\n\r\n".to_vec(),
            chunked.into_bytes(),
        ] {
            let (url, server) = serve(response, Duration::ZERO).await;
            let result = call(
                url,
                1000,
                Duration::from_secs(2),
                "rift.http.request('service',{})".into(),
            )
            .await
            .unwrap_err();
            assert!(result.contains("response body exceeds 64 KiB"), "{result}");
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn full_size_binary_request_and_response_bodies_are_allowed() {
        let mut response =
            format!("HTTP/1.1 200 OK\r\nContent-Length: {MAX_BODY}\r\nConnection: close\r\n\r\n")
                .into_bytes();
        response.extend(vec![0; MAX_BODY]);
        let (url, server) = serve(response, Duration::ZERO).await;
        call(url, 1000, Duration::from_secs(2), "local body=string.rep(string.char(0),65536); local r=rift.http.request('service',{method='POST',body=body}); assert(r.body==body)".into()).await.unwrap();
        assert!(server.await.unwrap().ends_with(&vec![0; MAX_BODY]));
    }

    #[tokio::test]
    async fn response_header_count_and_aggregate_size_are_bounded() {
        let too_many = (0..33).map(|i| format!("X-{i}: a\r\n")).collect::<String>();
        for headers in [
            too_many,
            format!("X-Large: {}\r\n", "a".repeat(MAX_HEADER_BYTES)),
        ] {
            let response = format!(
                "HTTP/1.1 200 OK\r\n{headers}Content-Length: 0\r\nConnection: close\r\n\r\n"
            );
            let (url, server) = serve(response.into_bytes(), Duration::ZERO).await;
            let result = call(
                url,
                1000,
                Duration::from_secs(2),
                "rift.http.request('service',{})".into(),
            )
            .await
            .unwrap_err();
            assert!(result.contains("32 header / 8 KiB limit"), "{result}");
            server.await.unwrap();
        }
    }
}
