use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// A tiny HTTP/1.1 fixture: each path maps to a fixed body.
struct Upstream {
    base: String,
    hits: Arc<Mutex<HashMap<String, usize>>>,
}

impl Upstream {
    async fn start(routes: Vec<(&str, Vec<u8>)>) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let routes: Arc<HashMap<String, Vec<u8>>> = Arc::new(
            routes
                .into_iter()
                .map(|(path, body)| (path.replace("{base}", &base), body))
                .map(|(path, body)| {
                    let body = String::from_utf8(body.clone())
                        .map(|text| text.replace("{base}", &base).into_bytes())
                        .unwrap_or(body);
                    (path, body)
                })
                .collect(),
        );
        let hits: Arc<Mutex<HashMap<String, usize>>> = Arc::default();
        let recorded = hits.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut stream, _)) = listener.accept().await else {
                    return;
                };
                let (routes, hits) = (routes.clone(), recorded.clone());
                tokio::spawn(async move {
                    let mut request = Vec::new();
                    let mut buffer = [0; 1024];
                    while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                        match stream.read(&mut buffer).await {
                            Ok(0) | Err(_) => return,
                            Ok(read) => request.extend_from_slice(&buffer[..read]),
                        }
                    }
                    let line = String::from_utf8_lossy(&request);
                    let path = line.split(' ').nth(1).unwrap_or("/").to_owned();
                    *hits.lock().unwrap().entry(path.clone()).or_default() += 1;
                    let (status, body) = match routes.get(&path) {
                        Some(body) => ("200 OK", body.clone()),
                        None => ("404 Not Found", b"missing".to_vec()),
                    };
                    let head = format!(
                        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                        body.len()
                    );
                    let _ = stream.write_all(head.as_bytes()).await;
                    let _ = stream.write_all(&body).await;
                });
            }
        });
        Self { base, hits }
    }

    fn endpoints(&self) -> Endpoints {
        Endpoints {
            fill: self.base.clone(),
            purpur: self.base.clone(),
            mojang: self.base.clone(),
            fabric: self.base.clone(),
        }
    }

    fn hits(&self, path: &str) -> usize {
        self.hits.lock().unwrap().get(path).copied().unwrap_or(0)
    }
}

fn cache() -> PathBuf {
    static NEXT: AtomicUsize = AtomicUsize::new(0);
    std::env::temp_dir().join(format!(
        "rift-software-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ))
}

fn software(kind: Kind, version: &str) -> Software {
    Software {
        kind,
        version: version.into(),
        url: None,
        sha256: None,
    }
}

fn sha256(bytes: &[u8]) -> String {
    hex(&Sha256::digest(bytes))
}

#[test]
fn validates_names_and_derives_cloudnet_style_cache_entries() {
    let paper = software(Kind::Paper, "1.21.8");
    paper.validate().unwrap();
    assert_eq!(paper.key(), "paper-1.21.8");
    assert_eq!(
        jar_path(Path::new("/cache"), &paper),
        Path::new("/cache/paper-1.21.8/server.jar")
    );
    software(Kind::Fabric, "latest").validate().unwrap();
    for version in [
        "",
        "../up",
        "1.21/8",
        ".hidden",
        "with space",
        &"9".repeat(65),
    ] {
        assert!(
            software(Kind::Paper, version).validate().is_err(),
            "{version}"
        );
    }
    let mut pinned = software(Kind::Paper, "1.21.8");
    pinned.sha256 = Some("0".repeat(64));
    assert!(pinned.validate().is_err());

    let mut custom = Software {
        kind: Kind::Url,
        version: String::new(),
        url: Some("https://example.com/server.jar".into()),
        sha256: Some("a".repeat(64)),
    };
    custom.validate().unwrap();
    assert!(custom.key().starts_with("url-") && custom.key().len() == 20);
    custom.sha256 = Some("xyz".into());
    assert!(custom.validate().is_err());
    custom.sha256 = None;
    custom.url = Some("http://example.com/server.jar".into());
    assert!(custom.validate().is_err());
    custom.url = Some("http://127.0.0.1:8080/server.jar".into());
    custom.validate().unwrap();
    custom.version = "1.0".into();
    assert!(custom.validate().is_err());
    assert_eq!(Kind::parse("folia"), Some(Kind::Folia));
    assert_eq!(Kind::parse("Paper"), None);
    assert!(Kind::Purpur.paper_family() && !Kind::Fabric.paper_family());
}

#[test]
fn orders_release_names_numerically() {
    let mut versions = vec!["1.9", "1.21.8", "1.21.10", "26.1", "1.21"];
    versions.sort_by(|left, right| compare(right, left));
    assert_eq!(versions, ["26.1", "1.21.10", "1.21.8", "1.21", "1.9"]);
    assert_eq!(
        redact("https://a.example/x.jar?token=secret"),
        "https://a.example/x.jar"
    );
}

#[tokio::test]
async fn downloads_paper_once_verifies_its_digest_and_refreshes_on_request() {
    let jar = b"paper server jar".to_vec();
    let upstream = Upstream::start(vec![
        (
            "/v3/projects/paper",
            br#"{"versions":{"1.21":["1.21.8","1.21.10","1.21.11-rc1"],"1.20":["1.20.6"]}}"#
                .to_vec(),
        ),
        (
            "/v3/projects/paper/versions/1.21.10/builds/latest",
            format!(
                r#"{{"id":7,"downloads":{{"server:default":{{"url":"{{base}}/objects/paper.jar","checksums":{{"sha256":"{}"}}}}}}}}"#,
                sha256(&jar)
            )
            .into_bytes(),
        ),
        ("/objects/paper.jar", jar.clone()),
    ])
    .await;
    let cache = cache();
    let latest = software(Kind::Paper, "latest");
    let first = download(&cache, &latest, false, &upstream.endpoints())
        .await
        .unwrap();
    assert_eq!(first["cached"], true);
    assert_eq!(first["resolved_version"], "1.21.10");
    assert_eq!(first["build"], "7");
    assert_eq!(first["sha256"], sha256(&jar));
    assert_eq!(fs::read(jar_path(&cache, &latest)).unwrap(), jar);
    assert!(
        !fs::read_dir(cache.join("paper-latest"))
            .unwrap()
            .any(|entry| entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .contains(".part"))
    );

    download(&cache, &latest, false, &upstream.endpoints())
        .await
        .unwrap();
    assert_eq!(
        upstream.hits("/objects/paper.jar"),
        1,
        "cached jars are reused"
    );
    download(&cache, &latest, true, &upstream.endpoints())
        .await
        .unwrap();
    assert_eq!(
        upstream.hits("/objects/paper.jar"),
        2,
        "refresh downloads again"
    );
    assert_eq!(status(&cache, &latest)["cached"], true);
    fs::remove_dir_all(cache).unwrap();
}

#[tokio::test]
async fn rejects_digest_mismatches_without_publishing_a_jar() {
    let upstream = Upstream::start(vec![
        (
            "/v3/projects/folia/versions/1.21.8/builds/latest",
            format!(
                r#"{{"id":1,"downloads":{{"server:default":{{"url":"{{base}}/objects/folia.jar","checksums":{{"sha256":"{}"}}}}}}}}"#,
                "0".repeat(64)
            )
            .into_bytes(),
        ),
        ("/objects/folia.jar", b"tampered".to_vec()),
    ])
    .await;
    let cache = cache();
    let folia = software(Kind::Folia, "1.21.8");
    let error = download(&cache, &folia, false, &upstream.endpoints())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("SHA-256 mismatch"), "{error}");
    assert_eq!(status(&cache, &folia)["cached"], false);
    assert_eq!(fs::read_dir(cache.join("folia-1.21.8")).unwrap().count(), 0);
    fs::remove_dir_all(cache).unwrap();
}

#[tokio::test]
async fn resolves_vanilla_purpur_fabric_and_pinned_urls() {
    let vanilla = b"vanilla".to_vec();
    let sha1 = hex(aws_lc_rs::digest::digest(
        &aws_lc_rs::digest::SHA1_FOR_LEGACY_USE_ONLY,
        &vanilla,
    )
    .as_ref());
    let upstream = Upstream::start(vec![
        (
            "/mc/game/version_manifest_v2.json",
            br#"{"latest":{"release":"1.21.8"},"versions":[{"id":"25w01a","type":"snapshot","url":"{base}/snapshot.json"},{"id":"1.21.8","type":"release","url":"{base}/1.21.8.json"}]}"#.to_vec(),
        ),
        (
            "/1.21.8.json",
            format!(r#"{{"downloads":{{"server":{{"url":"{{base}}/vanilla.jar","sha1":"{sha1}"}}}}}}"#)
                .into_bytes(),
        ),
        ("/vanilla.jar", vanilla.clone()),
        (
            "/v2/purpur",
            br#"{"versions":["1.21.1","1.21.10","1.21.8"]}"#.to_vec(),
        ),
        ("/v2/purpur/1.21.10/latest", br#"{"build":"2500"}"#.to_vec()),
        (
            "/v2/versions/loader",
            br#"[{"version":"0.18.0","stable":false},{"version":"0.17.2","stable":true}]"#.to_vec(),
        ),
        (
            "/v2/versions/installer",
            br#"[{"version":"1.1.0","stable":true}]"#.to_vec(),
        ),
        (
            "/v2/versions/game",
            br#"[{"version":"25w01a","stable":false},{"version":"1.21.8","stable":true}]"#.to_vec(),
        ),
        ("/pinned.jar", b"custom".to_vec()),
    ])
    .await;
    let endpoints = upstream.endpoints();
    let client = client().unwrap();
    let cache = cache();

    let latest = software(Kind::Vanilla, "latest");
    let downloaded = download(&cache, &latest, false, &endpoints).await.unwrap();
    assert_eq!(downloaded["resolved_version"], "1.21.8");
    assert_eq!(fs::read(jar_path(&cache, &latest)).unwrap(), vanilla);
    let missing = download(&cache, &software(Kind::Vanilla, "0.0.1"), false, &endpoints).await;
    assert!(
        missing
            .unwrap_err()
            .to_string()
            .contains("unknown Minecraft version")
    );

    let purpur = resolve(&client, &software(Kind::Purpur, "latest"), &endpoints)
        .await
        .unwrap();
    assert_eq!(purpur.version, "1.21.10");
    assert_eq!(
        purpur.url,
        format!("{}/v2/purpur/1.21.10/2500/download", upstream.base)
    );

    let fabric = resolve(&client, &software(Kind::Fabric, "latest"), &endpoints)
        .await
        .unwrap();
    assert_eq!(
        fabric.url,
        format!(
            "{}/v2/versions/loader/1.21.8/0.17.2/1.1.0/server/jar",
            upstream.base
        )
    );

    let mut pinned = Software {
        kind: Kind::Url,
        version: String::new(),
        url: Some(format!("{}/pinned.jar", upstream.base)),
        sha256: Some(sha256(b"custom")),
    };
    download(&cache, &pinned, false, &endpoints).await.unwrap();
    assert_eq!(fs::read(jar_path(&cache, &pinned)).unwrap(), b"custom");
    pinned.sha256 = Some(sha256(b"other"));
    pinned.url = Some(format!("{}/pinned.jar?v=2", upstream.base));
    assert!(download(&cache, &pinned, false, &endpoints).await.is_err());
    let missing = Software {
        url: Some(format!("{}/absent.jar", upstream.base)),
        sha256: None,
        ..pinned
    };
    let error = download(&cache, &missing, false, &endpoints)
        .await
        .unwrap_err();
    assert!(error.to_string().contains("HTTP 404"), "{error}");
    fs::remove_dir_all(cache).unwrap();
}
