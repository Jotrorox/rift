//! The few S3 calls backups need, signed with AWS Signature Version 4:
//! put, get, list, delete, and multipart uploads for large files. Works with
//! AWS and S3-compatible services (MinIO, Cloudflare R2, Hetzner, Backblaze
//! B2 and others).
use std::{
    fmt, io,
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use hmac::{Hmac, KeyInit, Mac};
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::config::S3Target;

/// Files up to this size go up in one request; larger ones in parts of this
/// size, which covers files up to 160 GB within S3's 10,000 parts.
const PART: usize = 16 * 1024 * 1024;
const MAX_PARTS: usize = 10_000;
/// Errors and listings are small; a larger body is cut off.
const MAX_SMALL_BODY: usize = 8 * 1024 * 1024;

#[derive(Clone)]
pub struct Bucket {
    client: reqwest::Client,
    target: S3Target,
    access_key: String,
    secret_key: String,
}

impl fmt::Debug for Bucket {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Bucket")
            .field("target", &self.target)
            .finish_non_exhaustive()
    }
}

/// A request ready to sign: where it goes and what the signature covers.
#[derive(Debug, PartialEq, Eq)]
struct Request {
    url: String,
    host: String,
    /// The URL path, already percent-encoded.
    path: String,
    /// The sorted, percent-encoded query string, also used in the URL.
    query: String,
}

impl Bucket {
    pub fn new(target: &S3Target) -> io::Result<Self> {
        let variable = |name: &str| {
            std::env::var(name).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::NotFound,
                    format!("set {name} for the backups.s3 credentials"),
                )
            })
        };
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            // Per request: a part or a small object, never a whole backup.
            .timeout(Duration::from_secs(600))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(io::Error::other)?;
        Ok(Self {
            client,
            target: target.clone(),
            access_key: variable(&target.access_key_env)?,
            secret_key: variable(&target.secret_key_env)?,
        })
    }

    pub fn describe(&self) -> String {
        format!("s3://{}/{}", self.target.bucket, self.target.prefix)
    }

    pub async fn put(&self, key: &str, body: Vec<u8>) -> io::Result<()> {
        self.call(Method::PUT, key, &[], body).await.map(|_| ())
    }

    /// Upload a local file, in parts if it is larger than [`PART`].
    pub async fn put_file(&self, key: &str, path: &Path) -> io::Result<()> {
        let mut file = tokio::fs::File::open(path).await?;
        let size = file.metadata().await?.len();
        if size <= PART as u64 {
            let body = read_part(&mut file).await?;
            return self.put(key, body).await;
        }
        if size.div_ceil(PART as u64) > MAX_PARTS as u64 {
            return Err(io::Error::new(
                io::ErrorKind::FileTooLarge,
                format!("{key} is too large for an S3 upload"),
            ));
        }
        let created = self
            .call(Method::POST, key, &[("uploads", String::new())], Vec::new())
            .await?;
        let upload = tag(&created, "UploadId")
            .map(unescape)
            .ok_or_else(|| io::Error::other("S3 started no multipart upload"))?;
        let result = self.upload_parts(key, &upload, &mut file).await;
        if result.is_err() {
            let _ = self
                .call(Method::DELETE, key, &[("uploadId", upload)], Vec::new())
                .await;
        }
        result
    }

    async fn upload_parts(
        &self,
        key: &str,
        upload: &str,
        file: &mut tokio::fs::File,
    ) -> io::Result<()> {
        let mut parts = String::new();
        for number in 1..=MAX_PARTS {
            let body = read_part(file).await?;
            if body.is_empty() {
                break;
            }
            let query = [
                ("partNumber", number.to_string()),
                ("uploadId", upload.to_owned()),
            ];
            let response = self.send(Method::PUT, key, &query, body).await?;
            let etag = response
                .headers()
                .get("etag")
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
                .ok_or_else(|| io::Error::other("S3 returned no ETag for a part"))?;
            check(response, key).await?;
            parts.push_str(&format!(
                "<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>",
                escape(&etag)
            ));
        }
        let body = format!("<CompleteMultipartUpload>{parts}</CompleteMultipartUpload>");
        let completed = self
            .call(
                Method::POST,
                key,
                &[("uploadId", upload.to_owned())],
                body.into_bytes(),
            )
            .await?;
        // S3 may report a failed completion with status 200.
        if completed.contains("<Error>") {
            return Err(error_message(StatusCode::OK, &completed, key));
        }
        Ok(())
    }

    /// A small object, refused if it is larger than `limit`.
    pub async fn get(&self, key: &str, limit: usize) -> io::Result<Vec<u8>> {
        let mut response = self.send(Method::GET, key, &[], Vec::new()).await?;
        if !response.status().is_success() {
            return Err(failure(response, key).await);
        }
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
            if body.len() + chunk.len() > limit {
                return Err(io::Error::new(
                    io::ErrorKind::FileTooLarge,
                    format!("{key} is too large to read"),
                ));
            }
            body.extend_from_slice(&chunk);
        }
        Ok(body)
    }

    /// Download an object into a new local file.
    pub async fn download(&self, key: &str, path: &Path) -> io::Result<()> {
        let mut response = self.send(Method::GET, key, &[], Vec::new()).await?;
        if !response.status().is_success() {
            return Err(failure(response, key).await);
        }
        let mut file = tokio::fs::File::create(path).await?;
        while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
            file.write_all(&chunk).await?;
        }
        file.flush().await
    }

    /// Deleting a missing object succeeds.
    pub async fn delete(&self, key: &str) -> io::Result<()> {
        let response = self.send(Method::DELETE, key, &[], Vec::new()).await?;
        if response.status() == StatusCode::NOT_FOUND {
            return Ok(());
        }
        check(response, key).await.map(|_| ())
    }

    /// Every key below `prefix`, without the configured prefix.
    pub async fn list(&self, prefix: &str) -> io::Result<Vec<String>> {
        let full = format!("{}{prefix}", self.target.prefix);
        let mut keys = Vec::new();
        let mut token = None;
        loop {
            let mut query = vec![("list-type", "2".to_owned()), ("prefix", full.clone())];
            if let Some(token) = token.take() {
                query.push(("continuation-token", token));
            }
            let page = self.call_bucket(Method::GET, &query).await?;
            for contents in tags(&page, "Contents") {
                if let Some(key) = tag(contents, "Key").map(unescape)
                    && let Some(key) = key.strip_prefix(&self.target.prefix)
                {
                    keys.push(key.to_owned());
                }
            }
            match tag(&page, "NextContinuationToken") {
                Some(next) if tag(&page, "IsTruncated") == Some("true") => {
                    token = Some(unescape(next));
                }
                _ => return Ok(keys),
            }
        }
    }

    /// Send a request for `key` (below the configured prefix) and return
    /// its body, failing on any error status.
    async fn call(
        &self,
        method: Method,
        key: &str,
        query: &[(&str, String)],
        body: Vec<u8>,
    ) -> io::Result<String> {
        let response = self.send(method, key, query, body).await?;
        check(response, key).await
    }

    /// Like [`Bucket::call`], for the bucket itself.
    async fn call_bucket(&self, method: Method, query: &[(&str, String)]) -> io::Result<String> {
        let request = self.request(None, query);
        let response = self.dispatch(method, request, Vec::new()).await?;
        check(response, &self.target.bucket).await
    }

    async fn send(
        &self,
        method: Method,
        key: &str,
        query: &[(&str, String)],
        body: Vec<u8>,
    ) -> io::Result<reqwest::Response> {
        let key = format!("{}{key}", self.target.prefix);
        let request = self.request(Some(&key), query);
        self.dispatch(method, request, body).await
    }

    async fn dispatch(
        &self,
        method: Method,
        request: Request,
        body: Vec<u8>,
    ) -> io::Result<reqwest::Response> {
        let payload = hex(&Sha256::digest(&body));
        let date = super::snapshot_id(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
        let headers = [
            ("host", request.host.as_str()),
            ("x-amz-content-sha256", payload.as_str()),
            ("x-amz-date", date.as_str()),
        ];
        let authorization = self.authorization(
            method.as_str(),
            &request.path,
            &request.query,
            &headers,
            &payload,
            &date,
        );
        self.client
            .request(method, &request.url)
            .header("x-amz-content-sha256", &payload)
            .header("x-amz-date", &date)
            .header("authorization", authorization)
            .body(body)
            .send()
            .await
            .map_err(|error| {
                io::Error::new(
                    io::ErrorKind::ConnectionRefused,
                    format!("S3 request failed: {error}"),
                )
            })
    }

    /// Where a request for `key` (or the bucket) goes. Keys are encoded
    /// segment by segment, as SigV4 signs them.
    fn request(&self, key: Option<&str>, query: &[(&str, String)]) -> Request {
        let (scheme, origin) = self
            .target
            .endpoint
            .split_once("://")
            .unwrap_or(("https", self.target.endpoint.as_str()));
        let encoded = key.map(|key| encode(key, false)).unwrap_or_default();
        let (host, path) = if self.target.path_style {
            let path = match key {
                Some(_) => format!("/{}/{encoded}", self.target.bucket),
                None => format!("/{}", self.target.bucket),
            };
            (origin.to_owned(), path)
        } else {
            (
                format!("{}.{origin}", self.target.bucket),
                format!("/{encoded}"),
            )
        };
        let mut pairs: Vec<(String, String)> = query
            .iter()
            .map(|(name, value)| (encode(name, true), encode(value, true)))
            .collect();
        pairs.sort();
        let query = pairs
            .iter()
            .map(|(name, value)| format!("{name}={value}"))
            .collect::<Vec<_>>()
            .join("&");
        let url = if query.is_empty() {
            format!("{scheme}://{host}{path}")
        } else {
            format!("{scheme}://{host}{path}?{query}")
        };
        Request {
            url,
            host,
            path,
            query,
        }
    }

    /// The `Authorization` header for a request with these `headers`, sorted
    /// by lowercase name.
    fn authorization(
        &self,
        method: &str,
        path: &str,
        query: &str,
        headers: &[(&str, &str)],
        payload: &str,
        date: &str,
    ) -> String {
        let day = &date[..8];
        let scope = format!("{day}/{}/s3/aws4_request", self.target.region);
        let canonical_headers: String = headers
            .iter()
            .map(|(name, value)| format!("{name}:{}\n", value.trim()))
            .collect();
        let signed = headers
            .iter()
            .map(|(name, _)| *name)
            .collect::<Vec<_>>()
            .join(";");
        let canonical =
            format!("{method}\n{path}\n{query}\n{canonical_headers}\n{signed}\n{payload}");
        let string_to_sign = format!(
            "AWS4-HMAC-SHA256\n{date}\n{scope}\n{}",
            hex(&Sha256::digest(canonical.as_bytes()))
        );
        let key = [day, self.target.region.as_str(), "s3", "aws4_request"]
            .iter()
            .fold(
                format!("AWS4{}", self.secret_key).into_bytes(),
                |key, part| hmac(&key, part.as_bytes()),
            );
        let signature = hex(&hmac(&key, string_to_sign.as_bytes()));
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}",
            self.access_key
        )
    }
}

/// Up to [`PART`] bytes from `file`.
async fn read_part(file: &mut tokio::fs::File) -> io::Result<Vec<u8>> {
    let mut body = Vec::new();
    (&mut *file)
        .take(PART as u64)
        .read_to_end(&mut body)
        .await?;
    Ok(body)
}

/// The body of a successful response, or the error S3 sent.
async fn check(response: reqwest::Response, key: &str) -> io::Result<String> {
    if !response.status().is_success() {
        return Err(failure(response, key).await);
    }
    small_body(response).await
}

async fn failure(response: reqwest::Response, key: &str) -> io::Error {
    let status = response.status();
    let body = small_body(response).await.unwrap_or_default();
    let error = error_message(status, &body, key);
    match status {
        StatusCode::NOT_FOUND => io::Error::new(io::ErrorKind::NotFound, error.to_string()),
        StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED => {
            io::Error::new(io::ErrorKind::PermissionDenied, error.to_string())
        }
        _ => error,
    }
}

async fn small_body(mut response: reqwest::Response) -> io::Result<String> {
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(io::Error::other)? {
        if body.len() + chunk.len() > MAX_SMALL_BODY {
            break;
        }
        body.extend_from_slice(&chunk);
    }
    Ok(String::from_utf8_lossy(&body).into_owned())
}

fn error_message(status: StatusCode, body: &str, key: &str) -> io::Error {
    let code = tag(body, "Code").map(unescape).unwrap_or_default();
    let message = tag(body, "Message").map(unescape).unwrap_or_default();
    io::Error::other(
        format!("S3 refused {key}: {status} {code} {message}")
            .trim_end()
            .to_owned(),
    )
}

fn hmac(key: &[u8], data: &[u8]) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(data);
    mac.finalize().into_bytes().to_vec()
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// SigV4 URI encoding: everything but unreserved characters, and `/` too
/// unless it separates path segments.
fn encode(value: &str, slash: bool) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                encoded.push(byte as char);
            }
            b'/' if !slash => encoded.push('/'),
            _ => encoded.push_str(&format!("%{byte:02X}")),
        }
    }
    encoded
}

/// The text of the first `<name>` element.
fn tag<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    tags(xml, name).next()
}

/// The text of every `<name>` element, in order.
fn tags<'a>(xml: &'a str, name: &str) -> impl Iterator<Item = &'a str> {
    let (open, close) = (format!("<{name}>"), format!("</{name}>"));
    let mut rest = xml;
    std::iter::from_fn(move || {
        let start = rest.find(&open)? + open.len();
        let length = rest[start..].find(&close)?;
        let text = &rest[start..start + length];
        rest = &rest[start + length + close.len()..];
        Some(text)
    })
}

fn unescape(text: &str) -> String {
    text.replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&#34;", "\"")
        .replace("&amp;", "&")
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bucket(path_style: bool, endpoint: &str) -> Bucket {
        Bucket {
            client: reqwest::Client::new(),
            target: S3Target {
                bucket: "examplebucket".into(),
                region: "us-east-1".into(),
                endpoint: endpoint.into(),
                prefix: String::new(),
                path_style,
                access_key_env: "UNUSED".into(),
                secret_key_env: "UNUSED".into(),
            },
            access_key: "AKIAIOSFODNN7EXAMPLE".into(),
            secret_key: "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY".into(),
        }
    }

    const EMPTY: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    /// The examples of the S3 SigV4 documentation ("Signature calculations
    /// for the Authorization header").
    #[test]
    fn signatures_match_the_aws_examples() {
        let bucket = bucket(false, "https://s3.amazonaws.com");
        let request = bucket.request(Some("test.txt"), &[]);
        assert_eq!(request.host, "examplebucket.s3.amazonaws.com");
        assert_eq!(request.path, "/test.txt");
        let authorization = bucket.authorization(
            "GET",
            &request.path,
            &request.query,
            &[
                ("host", request.host.as_str()),
                ("range", "bytes=0-9"),
                ("x-amz-content-sha256", EMPTY),
                ("x-amz-date", "20130524T000000Z"),
            ],
            EMPTY,
            "20130524T000000Z",
        );
        assert_eq!(
            authorization,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
        let listing = bucket.request(
            None,
            &[("prefix", "J".to_owned()), ("max-keys", "2".to_owned())],
        );
        assert_eq!(listing.path, "/");
        assert_eq!(listing.query, "max-keys=2&prefix=J");
        let authorization = bucket.authorization(
            "GET",
            &listing.path,
            &listing.query,
            &[
                ("host", listing.host.as_str()),
                ("x-amz-content-sha256", EMPTY),
                ("x-amz-date", "20130524T000000Z"),
            ],
            EMPTY,
            "20130524T000000Z",
        );
        assert!(
            authorization.ends_with(
                "Signature=34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7"
            ),
            "{authorization}"
        );
    }

    #[test]
    fn path_style_requests_name_the_bucket_in_the_path() {
        let mut bucket = bucket(true, "http://127.0.0.1:9000");
        bucket.target.prefix = "rift/".into();
        let key = format!(
            "{}snapshots/survival-1/20261118T123456Z.json",
            bucket.target.prefix
        );
        let request = bucket.request(Some(&key), &[]);
        assert_eq!(
            request.url,
            "http://127.0.0.1:9000/examplebucket/rift/snapshots/survival-1/20261118T123456Z.json"
        );
        assert_eq!(request.host, "127.0.0.1:9000");
        let listing = bucket.request(
            None,
            &[
                ("list-type", "2".to_owned()),
                ("prefix", "rift/objects/".to_owned()),
                ("continuation-token", "1a/b+c=".to_owned()),
            ],
        );
        assert_eq!(listing.path, "/examplebucket");
        assert_eq!(
            listing.query,
            "continuation-token=1a%2Fb%2Bc%3D&list-type=2&prefix=rift%2Fobjects%2F"
        );
        assert_eq!(
            listing.url,
            format!("http://127.0.0.1:9000/examplebucket?{}", listing.query)
        );
        // A bare flag is signed and sent as `name=`.
        assert_eq!(
            bucket
                .request(Some("k"), &[("uploads", String::new())])
                .query,
            "uploads="
        );
    }

    #[test]
    fn responses_are_read_without_an_xml_parser() {
        let page = "<ListBucketResult><IsTruncated>true</IsTruncated>\
            <Contents><Key>rift/objects/ab/1</Key><Size>1</Size></Contents>\
            <Contents><Key>rift/a&amp;b</Key></Contents>\
            <NextContinuationToken>t+1</NextContinuationToken></ListBucketResult>";
        let keys: Vec<_> = tags(page, "Contents")
            .filter_map(|contents| tag(contents, "Key").map(unescape))
            .collect();
        assert_eq!(keys, ["rift/objects/ab/1", "rift/a&b"]);
        assert_eq!(tag(page, "IsTruncated"), Some("true"));
        assert_eq!(tag(page, "NextContinuationToken"), Some("t+1"));
        assert_eq!(tag(page, "Missing"), None);
        let error = error_message(
            StatusCode::FORBIDDEN,
            "<Error><Code>AccessDenied</Code><Message>Access Denied</Message></Error>",
            "objects/ab/1",
        );
        assert_eq!(
            error.to_string(),
            "S3 refused objects/ab/1: 403 Forbidden AccessDenied Access Denied"
        );
        assert_eq!(escape("\"a&b\""), "&quot;a&amp;b&quot;");
        assert_eq!(encode("a b/ü~", false), "a%20b/%C3%BC~");
        assert_eq!(encode("a/b", true), "a%2Fb");
    }
}
