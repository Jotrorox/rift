use super::*;
use std::time::Duration;

fn request() -> HttpRequest {
    HttpRequest {
        method: "POST".into(),
        path: "/extensions/echo".into(),
        query: "verbose=true".into(),
        body: "hello".into(),
        headers: BTreeMap::from([("x-client".into(), "test".into())]),
        context: serde_json::json!({"generation": 3, "backends": ["lobby"], "ready": true}),
    }
}

fn script(body: &str) -> HttpScript {
    HttpScript::new(
        &format!("return {{ on_http = function(req) {body} end }}"),
        "http-test.lua",
    )
}

fn run(body: &str) -> Result<Option<HttpResponse>, HttpError> {
    script(body).evaluate(&request(), Instant::now() + Duration::from_secs(2))
}

#[test]
fn hook_receives_metadata_body_headers_and_status_context() {
    let response = run(r#"
        assert(req.method == 'POST')
        assert(req.path == '/extensions/echo')
        assert(req.query == 'verbose=true')
        assert(req.headers['x-client'] == 'test')
        assert(req.context.generation == 3)
        assert(req.context.backends[1] == 'lobby')
        assert(req.context.ready == true)
        return { status = 201, content_type = 'text/plain', body = req.body .. ' world' }
    "#)
    .unwrap()
    .unwrap();
    assert_eq!(
        response,
        HttpResponse {
            status: 201,
            content_type: "text/plain".into(),
            body: "hello world".into()
        }
    );
    assert_eq!(run("return nil").unwrap(), None);
    assert_eq!(run("return { body = '' }").unwrap().unwrap().status, 200);
    assert_eq!(
        run("return { status = 204, body = '' }")
            .unwrap()
            .unwrap()
            .status,
        204
    );
}

#[test]
fn responses_reject_invalid_status_shape_headers_and_body() {
    for value in [
        "true",
        "42",
        "'hello'",
        "{}",
        "{ body = 'x', unknown = true }",
        "{ status = 101, body = '' }",
        "{ status = 600, body = '' }",
        "{ status = 200.5, body = '' }",
        "{ status = '200', body = '' }",
        "{ status = 204, body = 'forbidden' }",
        "{ body = 10 }",
        "{ body = string.char(255) }",
        "{ body = string.rep('x', 256 * 1024 + 1) }",
        "{ body = '', content_type = 'text/plain\\r\\nX-Injected: true' }",
        "{ body = '', content_type = 'invalid' }",
        "{ body = '', content_type = '*/plain' }",
        "{ body = '', content_type = 'text/plain;' }",
        "{ body = '', content_type = true }",
    ] {
        let error = run(&format!("return {value}")).unwrap_err().to_string();
        assert!(error.contains("http-test.lua: on_http"), "{value}: {error}");
    }
}

#[test]
fn restricted_sandbox_and_budgets_apply_to_http_extensions() {
    run(r#"
        assert(io == nil and os == nil and package == nil and debug == nil)
        assert(load == nil and loadstring == nil and dofile == nil and require == nil)
        assert(jit == nil and coroutine == nil and pcall == nil)
        assert(string.find == nil and string.match == nil)
        return nil
    "#)
    .unwrap();
    let error = run("while true do end").unwrap_err().to_string();
    assert!(
        error.contains("instruction limit") || error.contains("deadline"),
        "{error}"
    );
    let error = run("return { body = string.rep('x', 9 * 1024 * 1024) }")
        .unwrap_err()
        .to_string();
    assert!(error.contains("memory"), "{error}");
    assert!(
        script("return nil")
            .evaluate(&request(), Instant::now())
            .unwrap_err()
            .to_string()
            .contains("deadline")
    );
    let mut oversized = request();
    oversized.body = "x".repeat(MAX_HTTP_BODY_BYTES + 1);
    assert!(
        script("return nil")
            .evaluate(&oversized, Instant::now() + Duration::from_secs(1))
            .unwrap_err()
            .to_string()
            .contains("request exceeds")
    );
}

#[test]
fn script_generated_diagnostics_are_bounded_before_crossing_http_boundary() {
    let HttpError::Script(message) = run("error(string.rep('x', 1024 * 1024))").unwrap_err() else {
        panic!("expected a script error")
    };
    assert!(message.starts_with("http-test.lua: on_http:"));
    assert!(message.len() <= 2048);
}

#[test]
fn every_request_gets_a_fresh_vm_and_owned_types_are_send_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<HttpScript>();
    assert_send_sync::<HttpRequest>();
    assert_send_sync::<HttpResponse>();
    let script = HttpScript::new(
        "local n = 0; return { on_http = function(req) n = n + 1; return { body = tostring(n) } end }",
        "test.lua",
    );
    for _ in 0..2 {
        assert_eq!(
            script
                .evaluate(&request(), Instant::now() + Duration::from_secs(1))
                .unwrap()
                .unwrap()
                .body,
            "1"
        );
    }
}

#[tokio::test]
async fn async_execution_rejects_overload_and_uses_shared_capacity() {
    let permits = SLOTS
        .try_acquire_many(crate::script::MAX_CONCURRENT as u32)
        .unwrap();
    let script = script("return { body = req.body }");
    assert_eq!(
        script.execute(request()).await.unwrap_err(),
        HttpError::Busy
    );
    drop(permits);
    assert_eq!(
        script.execute(request()).await.unwrap().unwrap().body,
        "hello"
    );
}

#[tokio::test]
async fn http_extensions_publish_binary_payloads_and_reply_subjects() {
    let broker = crate::messaging::Broker::default();
    let mut subscription = broker.subscribe("http.requests", None).unwrap();
    let response = script("local sent = rift.publish('http.requests', string.char(0, 255) .. req.body, 'http.reply'); assert(sent.delivered == 1); return { status = 202, body = 'accepted' }")
        .execute_with_messaging(request(), broker).await.unwrap().unwrap();
    assert_eq!(response.status, 202);
    let message = tokio::time::timeout(Duration::from_secs(1), subscription.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&message.payload[..], b"\0\xffhello");
    assert_eq!(message.reply.as_deref(), Some("http.reply"));
}
