//! The `/api/perms` HTTP API: what the dashboard and scripts use to read and
//! edit the permission store. `docs/http.md` lists every route with examples.
//!
//! The routes are a thin layer over [`Authority`]: every write is one
//! [`Authority::apply_with`] (or the import) with actor `operator:<name>`, so
//! validation, limits and the change log are the same as for `/perms`, Lua
//! and the bus. The web service authenticates the request, decides whether
//! the operator may read (`perms` or `config`) or write (`config`), records
//! the operator audit entry and then inserts a [`Caller`] that these handlers
//! extract; nothing here checks credentials.
//!
//! Node ids in paths are the node's slot, `<kind>:<key>` or
//! `<kind>:<key>@<contexts>` with the canonical context string
//! (`permission:essentials.fly@server=survival`). They name the same node
//! across edits and restarts, unlike database row ids, and a client can build
//! one from the typed node JSON without asking the server.
use super::{Authority, LogQuery, PermError, write_export};
use axum::{
    Json, Router,
    body::Bytes,
    extract::{
        DefaultBodyLimit, Extension, Path, RawQuery,
        rejection::{BytesRejection, JsonRejection},
    },
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use rift_permissions::{
    Actor, Applied, Change, ContextSet, Node, NodeKind, Payload, SubjectRef, Uuid,
    change::{Filter, Patch, SubjectFilter},
    compiled::EntryKind,
    node::{NodeKey, normalize_node},
    subject::{normalize_group, normalize_track},
    unix_now,
    wire::ErrorCode,
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Map, Value, json};
use std::{collections::BTreeMap, sync::Arc};

/// Largest import body, compressed or not. The decompressed export is
/// capped separately by [`super::read_export`].
pub const MAX_IMPORT: usize = 32 * 1024 * 1024;
/// Rows a listing returns by default, and at most.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 1000;
/// Longest audit reason, as in Lua.
const MAX_REASON: usize = 256;

/// One backend as the bus layer sees it, for `GET /api/perms/backends`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackendStatus {
    /// The bus principal, conventionally `server.<name>`.
    pub identity: String,
    /// The `server` name from its last `rift.perms.hello`.
    pub server: String,
    /// The revision its replica reported or was last sent.
    pub revision: u64,
    /// When the proxy last heard from it, Unix milliseconds.
    pub last_seen_ms: u64,
    /// Whether its bus connection is open.
    pub connected: bool,
    /// `client_version` from its hello.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_version: Option<String>,
}

/// The source of `GET /api/perms/backends`. The bus layer implements it and
/// installs it with [`Authority::set_backends`].
pub trait BackendDirectory: Send + Sync {
    fn backends(&self) -> Vec<BackendStatus>;
}

/// The directory before the bus layer installs one: no backends.
pub struct NoBackends;

impl BackendDirectory for NoBackends {
    fn backends(&self) -> Vec<BackendStatus> {
        Vec::new()
    }
}

/// The authenticated request context the web service inserts as a request
/// extension before these handlers run.
#[derive(Clone)]
pub struct Caller {
    authority: Arc<Authority>,
    operator: String,
}

impl Caller {
    /// `operator` is the authenticated web operator; writes are recorded as
    /// `operator:<operator>`.
    pub fn new(authority: Arc<Authority>, operator: &str) -> Self {
        Self {
            authority,
            operator: operator.to_owned(),
        }
    }

    fn actor(&self) -> Actor {
        format!("operator:{}", self.operator)
            .parse()
            .unwrap_or_else(|_| Actor::Operator("dashboard".into()))
    }
}

/// A failed request: `{"error": "...", "code": "not_found"}` with the status
/// [`status`] gives the code.
#[derive(Debug)]
pub struct Failure {
    status: StatusCode,
    code: ErrorCode,
    message: String,
}

/// The HTTP status of a permission error code.
pub fn status(code: ErrorCode) -> StatusCode {
    match code {
        ErrorCode::InvalidRequest => StatusCode::BAD_REQUEST,
        ErrorCode::Forbidden => StatusCode::FORBIDDEN,
        ErrorCode::NotFound => StatusCode::NOT_FOUND,
        ErrorCode::Limit => StatusCode::UNPROCESSABLE_ENTITY,
        ErrorCode::Conflict => StatusCode::CONFLICT,
        ErrorCode::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        ErrorCode::Internal => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

impl Failure {
    fn new(code: ErrorCode, message: impl ToString) -> Self {
        Self {
            status: status(code),
            code,
            message: message.to_string(),
        }
    }

    fn invalid(message: impl ToString) -> Self {
        Self::new(ErrorCode::InvalidRequest, message)
    }

    fn with_status(mut self, status: StatusCode) -> Self {
        self.status = status;
        self
    }
}

impl From<PermError> for Failure {
    fn from(error: PermError) -> Self {
        Self::new(error.code(), error)
    }
}

impl From<rift_permissions::Error> for Failure {
    fn from(error: rift_permissions::Error) -> Self {
        PermError::from(error).into()
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        let message: String = self.message.chars().take(2048).collect();
        (
            self.status,
            Json(json!({"error": message, "code": self.code})),
        )
            .into_response()
    }
}

type Reply = Result<Response, Failure>;

fn ok(value: impl Serialize) -> Reply {
    Ok(Json(value).into_response())
}

/// Run blocking store work off the async workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, Failure> + Send + 'static,
) -> Result<T, Failure> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| Failure::new(ErrorCode::Internal, e))?
}

// Requests.

/// Decode `%XX` escapes and `+` in a query component.
fn decode(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let hex = bytes.get(i + 1..i + 3)?;
                if !hex.iter().all(u8::is_ascii_hexdigit) {
                    return None;
                }
                out.push(u8::from_str_radix(std::str::from_utf8(hex).ok()?, 16).ok()?);
                i += 2;
            }
            byte => out.push(byte),
        }
        i += 1;
    }
    String::from_utf8(out).ok()
}

/// Query parameters in order.
struct Params(Vec<(String, String)>);

impl Params {
    fn parse(raw: Option<String>) -> Result<Self, Failure> {
        let mut pairs = Vec::new();
        for part in raw.as_deref().unwrap_or("").split('&') {
            if part.is_empty() {
                continue;
            }
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            match (decode(key), decode(value)) {
                (Some(key), Some(value)) => pairs.push((key, value)),
                _ => return Err(Failure::invalid("malformed query string")),
            }
        }
        Ok(Self(pairs))
    }

    /// Reject parameters other than `allowed`, so typos do not silently
    /// widen a query.
    fn only(self, allowed: &[&str]) -> Result<Self, Failure> {
        match self
            .0
            .iter()
            .find(|(key, _)| !allowed.contains(&key.as_str()))
        {
            Some((key, _)) => Err(Failure::invalid(format!(
                "unknown query parameter {key:?}; expected {}",
                allowed.join(", ")
            ))),
            None => Ok(self),
        }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0
            .iter()
            .rev()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
            .filter(|v| !v.is_empty())
    }

    fn number(&self, key: &str) -> Result<Option<u64>, Failure> {
        self.get(key)
            .map(|v| {
                v.parse()
                    .map_err(|_| Failure::invalid(format!("{key} must be a nonnegative integer")))
            })
            .transpose()
    }

    fn limit(&self) -> Result<usize, Failure> {
        match self.number("limit")? {
            None => Ok(DEFAULT_LIMIT),
            Some(0) => Err(Failure::invalid("limit must be at least 1")),
            Some(limit) => Ok((limit as usize).min(MAX_LIMIT)),
        }
    }

    /// A flag: present without a value, or `true`/`1`; `false`/`0` clear it.
    fn flag(&self, key: &str) -> Result<bool, Failure> {
        match self.0.iter().rev().find(|(k, _)| k == key) {
            None => Ok(false),
            Some((_, value)) => match value.as_str() {
                "" | "true" | "1" => Ok(true),
                "false" | "0" => Ok(false),
                _ => Err(Failure::invalid(format!("{key} must be true or false"))),
            },
        }
    }
}

/// A body that is not JSON, or too large.
fn rejected(rejection: JsonRejection) -> Failure {
    let status = match rejection.status() {
        status @ (StatusCode::UNSUPPORTED_MEDIA_TYPE | StatusCode::PAYLOAD_TOO_LARGE) => status,
        _ => StatusCode::BAD_REQUEST,
    };
    Failure::invalid(rejection.body_text()).with_status(status)
}

/// A JSON object body with only `allowed` members.
fn object(
    body: Result<Json<Value>, JsonRejection>,
    allowed: &[&str],
) -> Result<Map<String, Value>, Failure> {
    let Json(value) = body.map_err(rejected)?;
    let Value::Object(object) = value else {
        return Err(Failure::invalid("expected a JSON object"));
    };
    if let Some(key) = object.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(Failure::invalid(format!("unknown request field {key:?}")));
    }
    Ok(object)
}

/// An optional member of `object`, decoded as `T`; `null` counts as absent.
fn field<T: DeserializeOwned>(
    object: &Map<String, Value>,
    key: &str,
) -> Result<Option<T>, Failure> {
    match object.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => serde_json::from_value(value.clone())
            .map(Some)
            .map_err(|e| Failure::invalid(format!("{key}: {e}"))),
    }
}

fn required<T: DeserializeOwned>(object: &Map<String, Value>, key: &str) -> Result<T, Failure> {
    field(object, key)?.ok_or_else(|| Failure::invalid(format!("{key} is required")))
}

/// The optional audit `reason`: at most 256 bytes without control characters.
fn reason(object: &Map<String, Value>) -> Result<Option<String>, Failure> {
    let reason: Option<String> = field(object, "reason")?;
    if let Some(reason) = &reason
        && (reason.len() > MAX_REASON || reason.chars().any(char::is_control))
    {
        return Err(Failure::invalid(format!(
            "reason must have at most {MAX_REASON} bytes without control characters"
        )));
    }
    Ok(reason.filter(|reason| !reason.is_empty()))
}

/// A user in a path or body: a UUID (dashed or not) or a known player name.
fn user(authority: &Authority, text: &str) -> Result<Uuid, Failure> {
    if let Ok(uuid) = Uuid::parse(text) {
        return Ok(uuid);
    }
    if !(1..=16).contains(&text.len())
        || !text.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(Failure::invalid(format!(
            "{text:?} is neither a UUID nor a player name"
        )));
    }
    authority
        .user_by_name(text)?
        .ok_or_else(|| PermError::not_found("player", text).into())
}

// Node ids.

/// The path id of a node slot: `<kind>:<key>` or `<kind>:<key>@<contexts>`.
pub fn node_id(key: &NodeKey) -> String {
    if key.contexts.is_empty() {
        format!("{}:{}", key.kind, key.key)
    } else {
        format!("{}:{}@{}", key.kind, key.key, key.contexts)
    }
}

/// Parse a node id. Storage keys never contain `@`, so the first one ends
/// the key.
pub fn parse_node_id(id: &str) -> Result<NodeKey, Failure> {
    let (kind, rest) = id
        .split_once(':')
        .ok_or_else(|| Failure::invalid(format!("node id {id:?} must be <kind>:<key>")))?;
    let (key, contexts) = match rest.split_once('@') {
        Some((key, contexts)) => (key, ContextSet::parse(contexts)?),
        None => (rest, ContextSet::new()),
    };
    Ok(NodeKey::new(kind.parse::<NodeKind>()?, key, contexts)?)
}

fn node_entries(nodes: &[Node]) -> Vec<Value> {
    nodes
        .iter()
        .map(|node| json!({"id": node_id(&node.key()), "node": node}))
        .collect()
}

/// `Applied` with extra members.
fn applied(applied: &Applied, extra: Value) -> Value {
    let mut value = json!(applied);
    if let (Value::Object(target), Value::Object(extra)) = (&mut value, extra) {
        target.extend(extra);
    }
    value
}

/// A node from a body that may also carry `reason`.
fn node_body(mut object: Map<String, Value>) -> Result<(Node, Option<String>), Failure> {
    let reason = reason(&object)?;
    object.remove("reason");
    let node: Node = serde_json::from_value(Value::Object(object)).map_err(Failure::invalid)?;
    Ok((node.normalized()?, reason))
}

fn write(caller: &Caller, change: Change, reason: Option<&str>) -> Result<Applied, Failure> {
    Ok(caller
        .authority
        .apply_with(caller.actor(), change, reason, None)?)
}

// Shared node handlers for users and groups.

#[derive(Clone, Copy)]
enum Kind {
    User,
    Group,
}

fn subject(caller: &Caller, kind: Kind, name: &str) -> Result<SubjectRef, Failure> {
    Ok(match kind {
        Kind::User => SubjectRef::User(user(&caller.authority, name)?),
        Kind::Group => SubjectRef::Group(normalize_group(name)?),
    })
}

/// The subject's stored nodes; an unknown group is not found.
fn stored(caller: &Caller, subject: &SubjectRef) -> Result<Vec<Node>, Failure> {
    match subject {
        SubjectRef::User(uuid) => Ok(caller.authority.user(*uuid)?.nodes),
        SubjectRef::Group(name) => caller
            .authority
            .group(name)
            .map(|group| group.nodes)
            .ok_or_else(|| PermError::not_found("group", name).into()),
    }
}

async fn list_nodes(caller: Caller, kind: Kind, name: String) -> Reply {
    let value = blocking(move || {
        let subject = subject(&caller, kind, &name)?;
        let nodes = stored(&caller, &subject)?;
        Ok(json!({
            "subject": subject,
            "nodes": node_entries(&nodes),
            "revision": caller.authority.revision(),
        }))
    })
    .await?;
    ok(value)
}

async fn set_node(
    caller: Caller,
    kind: Kind,
    name: String,
    id: Option<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let Json(value) = body.map_err(rejected)?;
    let Value::Object(object) = value else {
        return Err(Failure::invalid("expected a typed node object"));
    };
    let (node, reason) = node_body(object)?;
    let value = blocking(move || {
        let subject = subject(&caller, kind, &name)?;
        let key = node.key();
        let change = match id {
            Some(id) => {
                let old = parse_node_id(&id)?;
                if !stored(&caller, &subject)?.iter().any(|n| n.key() == old) {
                    return Err(PermError::not_found("node", id).into());
                }
                if old == key {
                    Change::SetNode { subject, node }
                } else {
                    // Moving a node to another slot replaces whatever is there.
                    Change::Batch {
                        changes: vec![
                            Change::UnsetNode {
                                subject: subject.clone(),
                                key: old,
                            },
                            Change::SetNode { subject, node },
                        ],
                    }
                }
            }
            None => Change::SetNode { subject, node },
        };
        let result = write(&caller, change, reason.as_deref())?;
        Ok(applied(&result, json!({"id": node_id(&key)})))
    })
    .await?;
    ok(value)
}

async fn unset_node(
    caller: Caller,
    kind: Kind,
    name: String,
    id: String,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["reason"])?;
    let reason = reason(&object)?;
    let value = blocking(move || {
        let subject = subject(&caller, kind, &name)?;
        let key = parse_node_id(&id)?;
        let result = write(
            &caller,
            Change::UnsetNode { subject, key },
            reason.as_deref(),
        )?;
        Ok(json!(result))
    })
    .await?;
    ok(value)
}

async fn clear_nodes(
    caller: Caller,
    kind: Kind,
    name: String,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["contexts", "reason"])?;
    let reason = reason(&object)?;
    let contexts: Option<ContextSet> = field(&object, "contexts")?;
    let value = blocking(move || {
        let subject = subject(&caller, kind, &name)?;
        let result = write(
            &caller,
            Change::ClearSubject { subject, contexts },
            reason.as_deref(),
        )?;
        Ok(json!(result))
    })
    .await?;
    ok(value)
}

// Routes.

/// Every `/api/perms` route. The web service mounts it behind its
/// authentication layer and a layer that inserts [`Caller`].
pub fn router<S: Clone + Send + Sync + 'static>() -> Router<S> {
    Router::new()
        .route("/api/perms/revision", get(revision))
        .route("/api/perms/backends", get(backends))
        .route("/api/perms/registered", get(registered))
        .route("/api/perms/search", get(search))
        .route("/api/perms/users", get(users))
        .route("/api/perms/users/{user}", get(user_info).patch(patch_user))
        .route(
            "/api/perms/users/{user}/nodes",
            get(user_nodes).put(put_user_node).delete(clear_user),
        )
        .route(
            "/api/perms/users/{user}/nodes/{id}",
            put(replace_user_node).delete(delete_user_node),
        )
        .route("/api/perms/users/{user}/compiled", get(compiled))
        .route("/api/perms/groups", get(groups).post(create_group))
        .route(
            "/api/perms/groups/{name}",
            get(group).patch(patch_group).delete(delete_group),
        )
        .route(
            "/api/perms/groups/{name}/nodes",
            get(group_nodes).put(put_group_node).delete(clear_group),
        )
        .route(
            "/api/perms/groups/{name}/nodes/{id}",
            put(replace_group_node).delete(delete_group_node),
        )
        .route("/api/perms/tracks", get(tracks))
        .route(
            "/api/perms/tracks/{name}",
            get(track).put(put_track).delete(delete_track),
        )
        .route("/api/perms/tracks/{name}/promote", post(promote))
        .route("/api/perms/tracks/{name}/demote", post(demote))
        .route("/api/perms/audit", get(audit))
        .route("/api/perms/trace", post(trace))
        .route("/api/perms/apply", post(apply))
        .route("/api/perms/bulk", post(bulk))
        .route(
            "/api/perms/import",
            post(import).layer(DefaultBodyLimit::max(MAX_IMPORT)),
        )
        .route("/api/perms/export", get(export))
}

async fn revision(Extension(caller): Extension<Caller>) -> Reply {
    ok(json!({"revision": caller.authority.revision()}))
}

async fn backends(Extension(caller): Extension<Caller>) -> Reply {
    let current = caller.authority.revision();
    let backends: Vec<Value> = caller
        .authority
        .backends()
        .into_iter()
        .map(|backend| {
            let stale = backend.revision < current;
            let mut value = json!(backend);
            value["stale"] = json!(stale);
            value
        })
        .collect();
    ok(json!({"revision": current, "backends": backends}))
}

async fn registered(Extension(caller): Extension<Caller>) -> Reply {
    let registered = blocking(move || Ok(caller.authority.registered()?)).await?;
    ok(json!({"registered": registered}))
}

/// Escape `%`, `_` and `\` for a `LIKE` pattern.
fn like_literal(text: &str) -> String {
    text.chars().fold(String::new(), |mut out, c| {
        if matches!(c, '%' | '_' | '\\') {
            out.push('\\');
        }
        out.push(c);
        out
    })
}

async fn search(Extension(caller): Extension<Caller>, RawQuery(query): RawQuery) -> Reply {
    let params =
        Params::parse(query)?.only(&["node", "like", "kind", "subjects", "source", "limit"])?;
    let kind: Option<NodeKind> = params.get("kind").map(str::parse).transpose()?;
    let key_like = match (params.get("node"), params.get("like")) {
        (Some(_), Some(_)) => return Err(Failure::invalid("use node or like, not both")),
        (Some(node), None) => Some(like_literal(&match kind {
            Some(kind) => NodeKey::new(kind, node, ContextSet::new())?.key,
            None => node.to_ascii_lowercase(),
        })),
        (None, like) => like.map(str::to_owned),
    };
    let subjects = match params.get("subjects") {
        None | Some("any") => SubjectFilter::Any,
        Some("users") => SubjectFilter::Users,
        Some("groups") => SubjectFilter::Groups,
        Some(other) => {
            return Err(Failure::invalid(format!(
                "subjects {other:?}: expected any, users or groups"
            )));
        }
    };
    let filter = Filter {
        subjects,
        kind,
        key_like,
        source: params.get("source").map(str::parse).transpose()?,
        ..Filter::default()
    };
    let limit = params.limit()?;
    let value = blocking(move || {
        let holders = caller.authority.search(&filter, limit + 1)?;
        let truncated = holders.len() > limit;
        let mut names = BTreeMap::new();
        let mut rows = Vec::new();
        for holder in holders.into_iter().take(limit) {
            let name = match &holder.subject {
                SubjectRef::User(uuid) => match names.get(uuid) {
                    Some(name) => name,
                    None => names
                        .entry(*uuid)
                        .or_insert(caller.authority.user_name(*uuid)?),
                }
                .clone(),
                SubjectRef::Group(_) => None,
            };
            rows.push(json!({
                "subject": holder.subject,
                "name": name,
                "id": node_id(&holder.node.key()),
                "node": holder.node,
            }));
        }
        Ok(json!({"holders": rows, "truncated": truncated}))
    })
    .await?;
    ok(value)
}

async fn users(Extension(caller): Extension<Caller>, RawQuery(query): RawQuery) -> Reply {
    let params = Params::parse(query)?.only(&["q", "group", "limit"])?;
    let text = params.get("q").unwrap_or("").to_owned();
    if text.len() > 64 {
        return Err(Failure::invalid("q must have at most 64 bytes"));
    }
    let group = params.get("group").map(normalize_group).transpose()?;
    let limit = params.limit()?;
    let value = blocking(move || {
        let (users, truncated) = caller.authority.users(&text, group.as_deref(), limit)?;
        Ok(json!({"users": users, "truncated": truncated}))
    })
    .await?;
    ok(value)
}

async fn user_info(Extension(caller): Extension<Caller>, Path(name): Path<String>) -> Reply {
    let value = blocking(move || {
        let authority = &caller.authority;
        let uuid = user(authority, &name)?;
        let stored = authority.user(uuid)?;
        let compiled = authority.compiled(uuid);
        // Tracks listing a group the user inherits directly, in any context.
        let tracks: Vec<String> = authority
            .tracks()
            .values()
            .filter(|track| {
                stored.nodes.iter().any(|node| {
                    matches!(&node.payload, Payload::Inherit { group, value: true } if track.groups.contains(group))
                })
            })
            .map(|track| track.name.clone())
            .collect();
        Ok(json!({
            "uuid": uuid,
            "name": authority.user_name(uuid)?,
            "online": authority.is_online(uuid),
            "primary_group": stored.primary_group,
            "effective_primary_group": compiled.primary_group,
            "tracks": tracks,
            "contexts": authority.contexts(uuid),
            "nodes": node_entries(&stored.nodes),
            "revision": authority.revision(),
        }))
    })
    .await?;
    ok(value)
}

async fn patch_user(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["primary_group", "reason"])?;
    let reason = reason(&object)?;
    if !object.contains_key("primary_group") {
        return Err(Failure::invalid(
            "primary_group is required (null clears it)",
        ));
    }
    let group: Option<String> = field(&object, "primary_group")?;
    let value = blocking(move || {
        let uuid = user(&caller.authority, &name)?;
        let result = write(
            &caller,
            Change::SetPrimaryGroup { user: uuid, group },
            reason.as_deref(),
        )?;
        Ok(json!(result))
    })
    .await?;
    ok(value)
}

async fn user_nodes(Extension(caller): Extension<Caller>, Path(name): Path<String>) -> Reply {
    list_nodes(caller, Kind::User, name).await
}

async fn put_user_node(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    set_node(caller, Kind::User, name, None, body).await
}

async fn replace_user_node(
    Extension(caller): Extension<Caller>,
    Path((name, id)): Path<(String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    set_node(caller, Kind::User, name, Some(id), body).await
}

async fn delete_user_node(
    Extension(caller): Extension<Caller>,
    Path((name, id)): Path<(String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    unset_node(caller, Kind::User, name, id, body).await
}

async fn clear_user(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    clear_nodes(caller, Kind::User, name, body).await
}

async fn compiled(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    RawQuery(query): RawQuery,
) -> Reply {
    let params = Params::parse(query)?;
    let mut requested = ContextSet::new();
    for (key, value) in &params.0 {
        requested.insert(key, value)?;
    }
    let value = blocking(move || {
        let authority = &caller.authority;
        let uuid = user(authority, &name)?;
        let (active, from) = if params.0.is_empty() {
            (authority.contexts(uuid), "proxy")
        } else {
            (requested, "request")
        };
        let subject = authority.compiled(uuid);
        let now = unix_now();
        let mut groups: Vec<&str> = Vec::new();
        for entry in &subject.entries {
            if entry.kind == EntryKind::Group
                && entry.expires_at.is_none_or(|at| at > now)
                && entry.contexts.is_subset(&active)
                && !groups.contains(&entry.key.as_str())
            {
                groups.push(&entry.key);
            }
        }
        Ok(json!({
            "subject": &*subject,
            "contexts": active,
            "contexts_from": from,
            "meta": subject.meta(&active, now),
            "groups": groups,
        }))
    })
    .await?;
    ok(value)
}

async fn groups(Extension(caller): Extension<Caller>) -> Reply {
    let authority = &caller.authority;
    let tracks = authority.tracks();
    let groups: Vec<Value> = authority
        .groups()
        .values()
        .map(|group| {
            let parents: Vec<&Node> = group
                .nodes
                .iter()
                .filter(|node| node.kind() == NodeKind::Inherit)
                .collect();
            let on: Vec<&str> = tracks
                .values()
                .filter(|track| track.groups.contains(&group.name))
                .map(|track| track.name.as_str())
                .collect();
            json!({
                "name": group.name,
                "weight": group.weight,
                "display_name": group.display_name,
                "parents": parents,
                "nodes": group.nodes.len(),
                "tracks": on,
            })
        })
        .collect();
    ok(json!({"revision": authority.revision(), "groups": groups}))
}

async fn create_group(
    Extension(caller): Extension<Caller>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["name", "weight", "display_name", "reason"])?;
    let reason = reason(&object)?;
    let name: String = required(&object, "name")?;
    let weight: i32 = field(&object, "weight")?.unwrap_or(0);
    let display_name: Option<String> = field(&object, "display_name")?;
    let value = blocking(move || {
        let name = normalize_group(&name)?;
        let change = Change::CreateGroup {
            name: name.clone(),
            weight,
            display_name,
        };
        let result = write(&caller, change, reason.as_deref())?;
        Ok(applied(&result, json!({"name": name})))
    })
    .await?;
    Ok((StatusCode::CREATED, Json(value)).into_response())
}

async fn group(Extension(caller): Extension<Caller>, Path(name): Path<String>) -> Reply {
    let name = normalize_group(&name)?;
    let authority = &caller.authority;
    let group = authority
        .group(&name)
        .ok_or_else(|| PermError::not_found("group", &name))?;
    let tracks: Vec<String> = authority
        .tracks()
        .values()
        .filter(|track| track.groups.contains(&name))
        .map(|track| track.name.clone())
        .collect();
    ok(json!({
        "name": group.name,
        "weight": group.weight,
        "display_name": group.display_name,
        "tracks": tracks,
        "nodes": node_entries(&group.nodes),
        "revision": authority.revision(),
    }))
}

async fn patch_group(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["weight", "display_name", "reason"])?;
    let reason = reason(&object)?;
    let name = normalize_group(&name)?;
    let mut changes = Vec::new();
    if let Some(weight) = field::<i32>(&object, "weight")? {
        changes.push(Change::SetWeight {
            group: name.clone(),
            weight,
        });
    } else if object.contains_key("weight") {
        return Err(Failure::invalid("weight must be an integer"));
    }
    if object.contains_key("display_name") {
        changes.push(Change::SetDisplayName {
            group: name.clone(),
            display_name: field(&object, "display_name")?,
        });
    }
    let change = match changes.len() {
        0 => return Err(Failure::invalid("set weight or display_name")),
        1 => changes.remove(0),
        _ => Change::Batch { changes },
    };
    let value = blocking(move || Ok(json!(write(&caller, change, reason.as_deref())?))).await?;
    ok(value)
}

async fn delete_group(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["reason"])?;
    let reason = reason(&object)?;
    let value = blocking(move || {
        let change = Change::DeleteGroup { name };
        Ok(json!(write(&caller, change, reason.as_deref())?))
    })
    .await?;
    ok(value)
}

async fn group_nodes(Extension(caller): Extension<Caller>, Path(name): Path<String>) -> Reply {
    list_nodes(caller, Kind::Group, name).await
}

async fn put_group_node(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    set_node(caller, Kind::Group, name, None, body).await
}

async fn replace_group_node(
    Extension(caller): Extension<Caller>,
    Path((name, id)): Path<(String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    set_node(caller, Kind::Group, name, Some(id), body).await
}

async fn delete_group_node(
    Extension(caller): Extension<Caller>,
    Path((name, id)): Path<(String, String)>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    unset_node(caller, Kind::Group, name, id, body).await
}

async fn clear_group(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    clear_nodes(caller, Kind::Group, name, body).await
}

async fn tracks(Extension(caller): Extension<Caller>) -> Reply {
    let tracks: Vec<_> = caller.authority.tracks().values().cloned().collect();
    ok(json!({"revision": caller.authority.revision(), "tracks": tracks}))
}

async fn track(Extension(caller): Extension<Caller>, Path(name): Path<String>) -> Reply {
    let name = normalize_track(&name)?;
    let track = caller
        .authority
        .tracks()
        .get(&name)
        .cloned()
        .ok_or_else(|| PermError::not_found("track", &name))?;
    ok(track)
}

async fn put_track(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["groups", "reason"])?;
    let reason = reason(&object)?;
    let groups: Vec<String> = required(&object, "groups")?;
    let value = blocking(move || {
        let change = Change::SetTrack { name, groups };
        Ok(json!(write(&caller, change, reason.as_deref())?))
    })
    .await?;
    ok(value)
}

async fn delete_track(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["reason"])?;
    let reason = reason(&object)?;
    let value = blocking(move || {
        let change = Change::DeleteTrack { name };
        Ok(json!(write(&caller, change, reason.as_deref())?))
    })
    .await?;
    ok(value)
}

async fn promote(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    track_move(caller, name, body, true).await
}

async fn demote(
    Extension(caller): Extension<Caller>,
    Path(name): Path<String>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    track_move(caller, name, body, false).await
}

async fn track_move(
    caller: Caller,
    track: String,
    body: Result<Json<Value>, JsonRejection>,
    up: bool,
) -> Reply {
    let object = object(body, &["user", "contexts", "reason"])?;
    let reason = reason(&object)?;
    let name: String = required(&object, "user")?;
    let contexts: ContextSet = field(&object, "contexts")?.unwrap_or_default();
    let value = blocking(move || {
        let uuid = user(&caller.authority, &name)?;
        let change = if up {
            Change::Promote {
                user: uuid,
                track,
                contexts,
            }
        } else {
            Change::Demote {
                user: uuid,
                track,
                contexts,
            }
        };
        let result = write(&caller, change, reason.as_deref())?;
        Ok(applied(&result, json!({"user": uuid})))
    })
    .await?;
    ok(value)
}

async fn audit(Extension(caller): Extension<Caller>, RawQuery(query): RawQuery) -> Reply {
    let params = Params::parse(query)?.only(&["since", "actor", "subject", "limit"])?;
    let actor: Option<Actor> = params.get("actor").map(str::parse).transpose()?;
    // A bare UUID or group name is accepted as well as user:/group: forms.
    let subject = params
        .get("subject")
        .map(|text| match text.parse::<SubjectRef>() {
            Ok(subject) => Ok(subject),
            Err(error) if text.contains(':') => Err(error),
            Err(_) => match Uuid::parse(text) {
                Ok(uuid) => Ok(SubjectRef::User(uuid)),
                Err(_) => normalize_group(text).map(SubjectRef::Group),
            },
        })
        .transpose()?;
    let query = LogQuery {
        since: params.number("since")?,
        actor,
        subject,
        limit: params.limit()?,
    };
    let value = blocking(move || {
        let events = caller.authority.log(&query)?;
        Ok(json!({"revision": caller.authority.revision(), "events": events}))
    })
    .await?;
    ok(value)
}

async fn trace(
    Extension(caller): Extension<Caller>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["user", "node", "contexts"])?;
    let name: String = required(&object, "user")?;
    let node: String = required(&object, "node")?;
    let node = normalize_node(&node)?;
    let contexts: Option<ContextSet> = field(&object, "contexts")?;
    let value = blocking(move || {
        let authority = &caller.authority;
        let uuid = user(authority, &name)?;
        let (active, from) = match contexts {
            Some(contexts) => (contexts, "request"),
            None => (authority.contexts(uuid), "proxy"),
        };
        let subject = authority.compiled(uuid);
        let trace = subject.trace(&node, &active, unix_now());
        // Which subject each rank stands for: 0 is the user, then one rank
        // per group in the order the compiler ranked them.
        let mut ranks: BTreeMap<u32, String> =
            BTreeMap::from([(0, SubjectRef::User(uuid).to_string())]);
        for entry in &subject.entries {
            if entry.kind == EntryKind::Group {
                ranks
                    .entry(entry.rank)
                    .or_insert_with(|| format!("group:{}", entry.key));
            }
        }
        let ranks: Vec<Value> = ranks
            .into_iter()
            .map(|(rank, subject)| json!({"rank": rank, "subject": subject}))
            .collect();
        let mut value = json!(trace);
        value["uuid"] = json!(uuid);
        value["contexts"] = json!(active);
        value["contexts_from"] = json!(from);
        value["revision"] = json!(subject.revision);
        value["ranks"] = json!(ranks);
        Ok(value)
    })
    .await?;
    ok(value)
}

async fn apply(
    Extension(caller): Extension<Caller>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["change", "reason"])?;
    let reason = reason(&object)?;
    let change: Change = required(&object, "change")?;
    let value = blocking(move || Ok(json!(write(&caller, change, reason.as_deref())?))).await?;
    ok(value)
}

async fn bulk(
    Extension(caller): Extension<Caller>,
    body: Result<Json<Value>, JsonRejection>,
) -> Reply {
    let object = object(body, &["filter", "patch", "dry_run", "reason"])?;
    let reason = reason(&object)?;
    let filter: Filter = required(&object, "filter")?;
    let patch: Patch = required(&object, "patch")?;
    let dry_run: bool = field(&object, "dry_run")?.unwrap_or(false);
    let value = blocking(move || {
        let change = Change::Bulk { filter, patch };
        if dry_run {
            let ops = caller.authority.preview(change)?;
            Ok(json!({"dry_run": true, "count": ops.len(), "ops": ops}))
        } else {
            let result = write(&caller, change, reason.as_deref())?;
            Ok(applied(&result, json!({"dry_run": false})))
        }
    })
    .await?;
    ok(value)
}

async fn import(
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
    RawQuery(query): RawQuery,
    body: Result<Bytes, BytesRejection>,
) -> Reply {
    let params = Params::parse(query)?.only(&["replace"])?;
    let replace = params.flag("replace")?;
    let media = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.split(';').next())
        .map(|value| value.trim().to_ascii_lowercase());
    if !matches!(
        media.as_deref(),
        Some(
            "application/json"
                | "application/gzip"
                | "application/x-gzip"
                | "application/octet-stream"
        )
    ) {
        return Err(Failure::invalid(
            "send the export as application/json, or gzipped as application/gzip",
        )
        .with_status(StatusCode::UNSUPPORTED_MEDIA_TYPE));
    }
    let body = body.map_err(|rejection| {
        Failure::invalid(rejection.body_text()).with_status(rejection.status())
    })?;
    let value = blocking(move || {
        let report = caller
            .authority
            .import_luckperms(caller.actor(), &body[..], replace)?;
        Ok(json!(report))
    })
    .await?;
    ok(value)
}

async fn export(Extension(caller): Extension<Caller>, RawQuery(query): RawQuery) -> Reply {
    let params = Params::parse(query)?.only(&["gzip"])?;
    let gzip = params.flag("gzip")?;
    let body = blocking(move || {
        let export = caller.authority.export_luckperms()?;
        let mut body = Vec::new();
        write_export(&export, &mut body, gzip).map_err(|e| Failure::new(ErrorCode::Internal, e))?;
        Ok(body)
    })
    .await?;
    let mut response = body.into_response();
    let headers = response.headers_mut();
    if gzip {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/gzip"),
        );
        headers.insert(
            header::CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"luckperms-export.json.gz\""),
        );
    } else {
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_ids_round_trip_and_normalise() {
        let node = Node::permission("Essentials.Fly", true)
            .unwrap()
            .with_context("server", "survival")
            .unwrap()
            .with_context("region", "a@b")
            .unwrap();
        let id = node_id(&node.key());
        assert_eq!(id, "permission:essentials.fly@region=a@b;server=survival");
        assert_eq!(parse_node_id(&id).unwrap(), node.key());
        assert_eq!(
            parse_node_id("inherit:VIP").unwrap(),
            Node::inherit("vip", true).unwrap().key()
        );
        assert_eq!(
            node_id(&Node::prefix(100, "x").unwrap().key()),
            "prefix:100"
        );
        for bad in ["", "permission", "nope:x", "prefix:high", "permission:a b"] {
            assert!(parse_node_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn queries_decode_and_reject_unknown_parameters() {
        let params = Params::parse(Some("q=al+ice&group=vip%2Dplus&limit=5&flag".into())).unwrap();
        assert_eq!(params.get("q"), Some("al ice"));
        assert_eq!(params.get("group"), Some("vip-plus"));
        assert_eq!(params.limit().unwrap(), 5);
        assert!(params.flag("flag").unwrap());
        assert!(!params.flag("missing").unwrap());
        assert!(Params::parse(Some("q=%zz".into())).is_err());
        assert!(
            Params::parse(Some("x=1".into()))
                .unwrap()
                .only(&["q"])
                .is_err()
        );
        let huge = Params::parse(Some("limit=999999".into())).unwrap();
        assert_eq!(huge.limit().unwrap(), MAX_LIMIT);
        assert!(
            Params::parse(Some("limit=0".into()))
                .unwrap()
                .limit()
                .is_err()
        );
    }

    #[test]
    fn error_codes_map_to_statuses() {
        let failure = Failure::from(PermError::not_found("group", "vip"));
        assert_eq!(
            (failure.status, failure.code),
            (StatusCode::NOT_FOUND, ErrorCode::NotFound)
        );
        assert_eq!(status(ErrorCode::Limit), StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(status(ErrorCode::Conflict), StatusCode::CONFLICT);
    }
}
