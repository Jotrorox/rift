//! `/perms`: the LuckPerms-style command grammar shared by the proxy's own
//! in-game `/perms` (alias `/rp`), commands forwarded by backends on
//! `rift.perms.command`, `rift admin perms` and the HTTP layer.
//!
//! [`parse`] turns arguments into a [`Request`]; [`execute`] authorizes it
//! for a [`Sender`], applies it through the [`Authority`] and answers an
//! [`Output`] that renders as plain text ([`Output::text`]) or Adventure
//! component JSON ([`Output::component`]). Every write goes through
//! [`Authority::apply_with`], so each surface produces identical audit rows.
//!
//! # Authorization
//!
//! A [`Sender::Player`] needs `rift.perms.<verb>` for the request's
//! [`Request::verb`] (for example `rift.perms.user.permission.set`), which
//! `rift.perms.*` covers. Granting a permission or group the player does not
//! hold themselves additionally needs `rift.perms.escalate`. A
//! [`Sender::Console`] (the CLI, an operator, a backend console with
//! `control = true`) may do everything.
use super::{Authority, Bulk, LogQuery, PermError, control::Bus, write_export};
use rift_permissions::{
    Actor, Applied, Change, ChangeEvent, ContextSet, DEFAULT_GROUP, Node, NodeKind, Payload,
    SubjectRef, Trace, Tristate, Uuid,
    change::{Filter, Op, Patch, SubjectFilter},
    check::{Candidate, Outcome, Specificity},
    compiled::EntryKind,
    node::NodeKey,
    unix_now,
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
    path::{Component, Path, PathBuf},
    sync::Arc,
    time::Duration,
};

/// The node that lets a player grant what they do not hold themselves.
pub const ESCALATE: &str = "rift.perms.escalate";
/// Prefix of every `/perms` authorization node.
pub const NODE_PREFIX: &str = "rift.perms.";
/// The proxy's command names: `/perms` and its alias `/rp`.
pub const COMMAND_NAMES: [&str; 2] = ["perms", "rp"];
/// Most rows a listing prints.
const LIST_LIMIT: usize = 100;
/// How long `sync` waits for backends to say `hello` again.
const SYNC_WAIT: Duration = Duration::from_secs(1);

/// Who runs a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Sender {
    /// The CLI, a dashboard operator or a backend console: every verb, files
    /// anywhere on the proxy host. The actor is what the audit log records.
    Console(Actor),
    /// A player: verbs need `rift.perms.<verb>`, grants need the granted
    /// node or `rift.perms.escalate`, and files are bare names in the proxy's
    /// working directory.
    Player(Uuid),
}

impl Sender {
    pub fn actor(&self) -> Actor {
        match self {
            Self::Console(actor) => actor.clone(),
            Self::Player(uuid) => Actor::Player(*uuid),
        }
    }
}

/// What a command may reach besides the authority.
#[derive(Clone)]
pub struct Env {
    pub authority: Arc<Authority>,
    /// The bus service, for `sync` and `backends`; absent in tests.
    pub bus: Option<Arc<Bus>>,
    /// `(command, permission)` of every extension command, for `doctor`.
    pub commands: Vec<(String, String)>,
}

impl Env {
    pub fn new(authority: Arc<Authority>) -> Self {
        Self {
            authority,
            bus: None,
            commands: Vec::new(),
        }
    }
}

/// How a line is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tone {
    Plain,
    Heading,
    Good,
    Bad,
    Muted,
}

impl Tone {
    fn color(self) -> Option<&'static str> {
        match self {
            Self::Plain => None,
            Self::Heading => Some("gold"),
            Self::Good => Some("green"),
            Self::Bad => Some("red"),
            Self::Muted => Some("gray"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Line {
    pub tone: Tone,
    pub text: String,
}

/// A command's answer.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Output {
    /// False when the command failed as a command (bad syntax, unknown
    /// player, missing permission, a refused change).
    pub ok: bool,
    /// The proxy's revision after the command.
    pub revision: u64,
    pub lines: Vec<Line>,
    /// Machine-readable result for the CLI and HTTP: the compiled subject,
    /// a trace, an applied change.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl Output {
    fn new() -> Self {
        Self {
            ok: true,
            revision: 0,
            lines: Vec::new(),
            data: None,
        }
    }

    fn failed(message: impl Into<String>) -> Self {
        let mut output = Self::new();
        output.fail(message);
        output
    }

    fn line(&mut self, tone: Tone, text: impl Into<String>) {
        self.lines.push(Line {
            tone,
            text: text.into(),
        });
    }

    fn fail(&mut self, message: impl Into<String>) {
        self.ok = false;
        self.line(Tone::Bad, message);
    }

    /// The lines joined by newlines, for terminals and logs.
    pub fn text(&self) -> String {
        let mut text = String::new();
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                text.push('\n');
            }
            text.push_str(&line.text);
        }
        text
    }

    /// Adventure component JSON to show in game.
    pub fn component(&self) -> Value {
        let mut extra = Vec::with_capacity(self.lines.len() * 2);
        for (index, line) in self.lines.iter().enumerate() {
            if index > 0 {
                extra.push(json!({"text": "\n"}));
            }
            let mut part = json!({"text": line.text});
            if let Some(color) = line.tone.color() {
                part["color"] = color.into();
            }
            extra.push(part);
        }
        if extra.is_empty() {
            return json!({"text": ""});
        }
        json!({"text": "", "extra": extra})
    }
}

/// A grammar or authorization failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    /// The usage lines for the part of the grammar that failed.
    pub usage: Vec<&'static str>,
}

type Parsed<T> = std::result::Result<T, ParseError>;

fn syntax(message: impl Into<String>, usage: &[&'static str]) -> ParseError {
    ParseError {
        message: message.into(),
        usage: usage.to_vec(),
    }
}

const USAGE_USER: &[&str] = &[
    "/perms user <player> info",
    "/perms user <player> permission set <node> [true|false] [context...] [--duration <time>|--expiry <unix>]",
    "/perms user <player> permission unset <node> [context...]",
    "/perms user <player> permission settemp <node> <true|false> <time> [context...]",
    "/perms user <player> permission unsettemp <node> [context...]",
    "/perms user <player> permission info | check <node> [context...]",
    "/perms user <player> parent add|remove|set <group> [context...]",
    "/perms user <player> parent addtemp <group> <time> [context...] | removetemp <group> [context...]",
    "/perms user <player> parent switchprimarygroup <group> | info",
    "/perms user <player> meta set <key> <value> [context...] | unset <key> [context...]",
    "/perms user <player> meta setprefix|setsuffix <priority> <text> [context...]",
    "/perms user <player> meta removeprefix|removesuffix <priority> [context...]",
    "/perms user <player> meta settemp <key> <value> <time> | settempprefix|settempsuffix <priority> <text> <time>",
    "/perms user <player> meta info | clear [context...]",
    "/perms user <player> promote|demote <track> [context...]",
    "/perms user <player> clear [context...]",
];
const USAGE_GROUP: &[&str] = &[
    "/perms group <group> create [weight] [display name] | delete | info | listmembers",
    "/perms group <group> setweight <weight> | setdisplayname [name]",
    "/perms group <group> permission|parent|meta ... (as for users)",
    "/perms group <group> clear [context...]",
];
const USAGE_TRACK: &[&str] = &[
    "/perms track <track> create [group...] | delete | info | clear",
    "/perms track <track> append <group> | insert <group> <position> | remove <group>",
];
const USAGE_OTHER: &[&str] = &[
    "/perms check|trace <player> <node> [context...]",
    "/perms listgroups | listtracks | search <node>",
    "/perms import <file> [--replace] | export <file>",
    "/perms bulk [users|groups|all] [key <like>] [kind <kind>] [value <v>] [context <k=v>] [source <s>] delete|set-value <bool>|set-context <k=v|global>|set-expiry <time>|permanent|set-source <s> [--dry-run]",
    "/perms log [--since <revision>] [--actor <actor>] [--subject <player|group:name>] [--limit <n>]",
    "/perms compiled <player> | registered | sync | backends | doctor",
    "Contexts are key=value (server=survival world=world_nether); times are 30m, 12h, 7d, 1w2d or Unix seconds.",
];

/// Every usage line.
pub fn usage() -> Vec<&'static str> {
    [USAGE_USER, USAGE_GROUP, USAGE_TRACK, USAGE_OTHER].concat()
}

// The grammar.

/// Where a temporary node ends.
pub type Expiry = u64;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PermissionAction {
    Info,
    Set {
        node: String,
        value: bool,
        contexts: ContextSet,
        expiry: Option<Expiry>,
    },
    Unset {
        node: String,
        contexts: ContextSet,
        temporary: bool,
    },
    Check {
        node: String,
        contexts: ContextSet,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParentAction {
    Info,
    Add {
        group: String,
        contexts: ContextSet,
        expiry: Option<Expiry>,
    },
    Remove {
        group: String,
        contexts: ContextSet,
        temporary: bool,
    },
    /// Replace every parent in exactly these contexts with `group`.
    Set {
        group: String,
        contexts: ContextSet,
    },
    SwitchPrimary {
        group: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decoration {
    Prefix,
    Suffix,
}

impl Decoration {
    fn name(self) -> &'static str {
        match self {
            Self::Prefix => "prefix",
            Self::Suffix => "suffix",
        }
    }

    fn kind(self) -> NodeKind {
        match self {
            Self::Prefix => NodeKind::Prefix,
            Self::Suffix => NodeKind::Suffix,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetaAction {
    Info,
    Set {
        key: String,
        value: String,
        contexts: ContextSet,
        expiry: Option<Expiry>,
    },
    Unset {
        key: String,
        contexts: ContextSet,
    },
    SetDecoration {
        which: Decoration,
        priority: i32,
        text: String,
        contexts: ContextSet,
        expiry: Option<Expiry>,
    },
    RemoveDecoration {
        which: Decoration,
        priority: i32,
        contexts: ContextSet,
    },
    Clear {
        contexts: Option<ContextSet>,
    },
}

/// What can be done to a user or a group.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectAction {
    Info,
    Permission(PermissionAction),
    Parent(ParentAction),
    Meta(MetaAction),
    Promote { track: String, contexts: ContextSet },
    Demote { track: String, contexts: ContextSet },
    Clear { contexts: Option<ContextSet> },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GroupAction {
    Subject(SubjectAction),
    Create {
        weight: i32,
        display_name: Option<String>,
    },
    Delete,
    SetWeight(i32),
    SetDisplayName(Option<String>),
    ListMembers,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrackAction {
    Create(Vec<String>),
    Delete,
    Info,
    Append(String),
    /// Insert at a 1-based position.
    Insert(String, usize),
    Remove(String),
    Clear,
}

/// The `bulk` verb: a filter, a patch and whether to only preview it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BulkRequest {
    pub filter: Filter,
    /// A `subject user:<name>` filter, resolved at execution.
    pub user: Option<String>,
    pub patch: Patch,
    pub dry_run: bool,
}

/// A parsed `/perms` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Help,
    User {
        target: String,
        action: SubjectAction,
    },
    Group {
        name: String,
        action: GroupAction,
    },
    Track {
        name: String,
        action: TrackAction,
    },
    Check {
        target: String,
        node: String,
        contexts: Option<ContextSet>,
    },
    Trace {
        target: String,
        node: String,
        contexts: Option<ContextSet>,
    },
    ListGroups,
    ListTracks,
    Search {
        node: String,
    },
    Import {
        file: String,
        replace: bool,
    },
    Export {
        file: String,
    },
    Bulk(BulkRequest),
    Sync,
    Log {
        since: Option<u64>,
        actor: Option<String>,
        subject: Option<String>,
        limit: usize,
    },
    Compiled {
        target: String,
    },
    Registered,
    Backends,
    Doctor,
}

impl Request {
    /// The verb tree path this request is authorized by:
    /// `rift.perms.<verb>`.
    pub fn verb(&self) -> String {
        fn subject_verb(action: &SubjectAction) -> String {
            match action {
                SubjectAction::Info => "info".into(),
                SubjectAction::Permission(action) => format!(
                    "permission.{}",
                    match action {
                        PermissionAction::Info => "info",
                        PermissionAction::Set { expiry: None, .. } => "set",
                        PermissionAction::Set { .. } => "settemp",
                        PermissionAction::Unset {
                            temporary: false, ..
                        } => "unset",
                        PermissionAction::Unset { .. } => "unsettemp",
                        PermissionAction::Check { .. } => "check",
                    }
                ),
                SubjectAction::Parent(action) => format!(
                    "parent.{}",
                    match action {
                        ParentAction::Info => "info",
                        ParentAction::Add { expiry: None, .. } => "add",
                        ParentAction::Add { .. } => "addtemp",
                        ParentAction::Remove {
                            temporary: false, ..
                        } => "remove",
                        ParentAction::Remove { .. } => "removetemp",
                        ParentAction::Set { .. } => "set",
                        ParentAction::SwitchPrimary { .. } => "switchprimarygroup",
                    }
                ),
                SubjectAction::Meta(action) => format!(
                    "meta.{}",
                    match action {
                        MetaAction::Info => "info",
                        MetaAction::Set { expiry: None, .. } => "set",
                        MetaAction::Set { .. } => "settemp",
                        MetaAction::Unset { .. } => "unset",
                        MetaAction::SetDecoration {
                            which: Decoration::Prefix,
                            expiry: None,
                            ..
                        } => "setprefix",
                        MetaAction::SetDecoration {
                            which: Decoration::Suffix,
                            expiry: None,
                            ..
                        } => "setsuffix",
                        MetaAction::SetDecoration {
                            which: Decoration::Prefix,
                            ..
                        } => "settempprefix",
                        MetaAction::SetDecoration { .. } => "settempsuffix",
                        MetaAction::RemoveDecoration {
                            which: Decoration::Prefix,
                            ..
                        } => "removeprefix",
                        MetaAction::RemoveDecoration { .. } => "removesuffix",
                        MetaAction::Clear { .. } => "clear",
                    }
                ),
                SubjectAction::Promote { .. } => "promote".into(),
                SubjectAction::Demote { .. } => "demote".into(),
                SubjectAction::Clear { .. } => "clear".into(),
            }
        }
        match self {
            Self::Help => "help".into(),
            Self::User { action, .. } => format!("user.{}", subject_verb(action)),
            Self::Group { action, .. } => match action {
                GroupAction::Subject(action) => format!("group.{}", subject_verb(action)),
                GroupAction::Create { .. } => "group.create".into(),
                GroupAction::Delete => "group.delete".into(),
                GroupAction::SetWeight(_) => "group.setweight".into(),
                GroupAction::SetDisplayName(_) => "group.setdisplayname".into(),
                GroupAction::ListMembers => "group.listmembers".into(),
            },
            Self::Track { action, .. } => format!(
                "track.{}",
                match action {
                    TrackAction::Create(_) => "create",
                    TrackAction::Delete => "delete",
                    TrackAction::Info => "info",
                    TrackAction::Append(_) => "append",
                    TrackAction::Insert(..) => "insert",
                    TrackAction::Remove(_) => "remove",
                    TrackAction::Clear => "clear",
                }
            ),
            Self::Check { .. } => "check".into(),
            Self::Trace { .. } => "trace".into(),
            Self::ListGroups => "listgroups".into(),
            Self::ListTracks => "listtracks".into(),
            Self::Search { .. } => "search".into(),
            Self::Import { .. } => "import".into(),
            Self::Export { .. } => "export".into(),
            Self::Bulk(_) => "bulk".into(),
            Self::Sync => "sync".into(),
            Self::Log { .. } => "log".into(),
            Self::Compiled { .. } => "compiled".into(),
            Self::Registered => "registered".into(),
            Self::Backends => "backends".into(),
            Self::Doctor => "doctor".into(),
        }
    }

    /// The node a player needs to run this request.
    pub fn node(&self) -> String {
        format!("{NODE_PREFIX}{}", self.verb())
    }
}

/// Whether a player holds any `/perms` node, so the proxy shows them the
/// command in their command tree.
pub fn may_use(authority: &Authority, uuid: Uuid) -> bool {
    let compiled = authority.compiled(uuid);
    let active = authority.contexts(uuid);
    let now = unix_now();
    compiled.entries.iter().any(|entry| {
        entry.kind == EntryKind::Permission
            && entry.value
            && entry.expires_at.is_none_or(|at| at > now)
            && entry.contexts.is_subset(&active)
            && (entry.key == "*" || entry.key == "rift.*" || entry.key.starts_with(NODE_PREFIX))
    })
}

/// Split a typed command line into arguments; `"double quotes"` keep
/// spaces, and `\"` inside quotes is a literal quote.
pub fn split(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    let mut started = false;
    let mut chars = line.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' if quoted && chars.peek() == Some(&'"') => {
                current.push('"');
                chars.next();
            }
            '"' => {
                quoted = !quoted;
                started = true;
            }
            c if c.is_whitespace() && !quoted => {
                if started {
                    args.push(std::mem::take(&mut current));
                    started = false;
                }
            }
            c => {
                current.push(c);
                started = true;
            }
        }
    }
    if started {
        args.push(current);
    }
    args
}

/// Arguments from a backend that split on spaces: quotes typed by the player
/// are rejoined, so `"&6[VIP] "` arrives as one argument.
pub fn rejoin(args: &[String]) -> Vec<String> {
    if args.iter().any(|arg| arg.contains('"')) {
        split(&args.join(" "))
    } else {
        args.to_vec()
    }
}

/// A Unix time from `text`: a duration from `now` (`90s`, `30m`, `12h`,
/// `7d`, `2w`, `1mo`, `1y`, combined like `1d12h`; a bare number is seconds)
/// or an absolute Unix time (ten or more digits).
pub fn parse_expiry(text: &str, now: u64) -> Result<Expiry, String> {
    let invalid = || format!("invalid time {text:?}: use 30m, 12h, 7d, 1w2d or Unix seconds");
    if text.is_empty() {
        return Err(invalid());
    }
    if text.bytes().all(|b| b.is_ascii_digit()) {
        let value: u64 = text.parse().map_err(|_| invalid())?;
        if text.len() >= 10 {
            if value <= now {
                return Err(format!("time {text} is in the past"));
            }
            return Ok(value);
        }
        if value == 0 {
            return Err(invalid());
        }
        return now.checked_add(value).ok_or_else(invalid);
    }
    let mut total: u64 = 0;
    let mut rest = text.to_ascii_lowercase();
    while !rest.is_empty() {
        let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
        if digits == 0 {
            return Err(invalid());
        }
        let number: u64 = rest[..digits].parse().map_err(|_| invalid())?;
        rest.drain(..digits);
        let units: &[(&str, u64)] = &[
            ("mo", 30 * 86_400),
            ("s", 1),
            ("m", 60),
            ("h", 3600),
            ("d", 86_400),
            ("w", 7 * 86_400),
            ("y", 365 * 86_400),
        ];
        let (unit, seconds) = units
            .iter()
            .find(|(unit, _)| rest.starts_with(unit))
            .ok_or_else(invalid)?;
        rest.drain(..unit.len());
        total = number
            .checked_mul(*seconds)
            .and_then(|s| total.checked_add(s))
            .ok_or_else(invalid)?;
    }
    if total == 0 {
        return Err(invalid());
    }
    now.checked_add(total).ok_or_else(invalid)
}

fn parse_bool(text: &str) -> Option<bool> {
    match text.to_ascii_lowercase().as_str() {
        "true" | "t" | "yes" | "allow" => Some(true),
        "false" | "f" | "no" | "deny" => Some(false),
        _ => None,
    }
}

/// Trailing options: `key=value` contexts, `global`, `--duration <time>` and
/// `--expiry <unix>` (when `expiry` is allowed).
struct Options {
    contexts: ContextSet,
    any_context: bool,
    expiry: Option<Expiry>,
}

fn options(
    rest: &[String],
    allow_expiry: bool,
    now: u64,
    usage: &[&'static str],
) -> Parsed<Options> {
    let mut parsed = Options {
        contexts: ContextSet::new(),
        any_context: false,
        expiry: None,
    };
    let mut args = rest.iter();
    while let Some(arg) = args.next() {
        if allow_expiry && matches!(arg.as_str(), "--duration" | "--expiry") {
            let value = args
                .next()
                .ok_or_else(|| syntax(format!("{arg} needs a time"), usage))?;
            if parsed.expiry.is_some() {
                return Err(syntax("give the expiry once", usage));
            }
            parsed.expiry = Some(parse_expiry(value, now).map_err(|e| syntax(e, usage))?);
        } else if arg.eq_ignore_ascii_case("global") {
            parsed.any_context = true;
        } else if let Some((key, value)) = arg.split_once('=') {
            // LuckPerms writes server=global for "every server".
            if !value.eq_ignore_ascii_case("global") {
                parsed
                    .contexts
                    .insert(key, value)
                    .map_err(|e| syntax(e.to_string(), usage))?;
            }
            parsed.any_context = true;
        } else {
            return Err(syntax(
                format!("unexpected argument {arg:?}; contexts are key=value"),
                usage,
            ));
        }
    }
    Ok(parsed)
}

fn contexts_only(rest: &[String], now: u64, usage: &[&'static str]) -> Parsed<ContextSet> {
    Ok(options(rest, false, now, usage)?.contexts)
}

/// `clear [context...]`: `None` (every node) without contexts.
fn clear_contexts(rest: &[String], now: u64, usage: &[&'static str]) -> Parsed<Option<ContextSet>> {
    let options = options(rest, false, now, usage)?;
    Ok(options.any_context.then_some(options.contexts))
}

fn take<'a>(
    args: &'a [String],
    index: usize,
    what: &str,
    usage: &[&'static str],
) -> Parsed<&'a str> {
    args.get(index)
        .map(String::as_str)
        .ok_or_else(|| syntax(format!("missing {what}"), usage))
}

fn int<T: std::str::FromStr>(text: &str, what: &str, usage: &[&'static str]) -> Parsed<T> {
    text.parse().map_err(|_| {
        syntax(
            format!("{what} must be a whole number, not {text:?}"),
            usage,
        )
    })
}

/// Parse `/perms` arguments (without the command name) at time `now`.
pub fn parse(args: &[String], now: u64) -> Parsed<Request> {
    let Some(verb) = args.first() else {
        return Ok(Request::Help);
    };
    let rest = &args[1..];
    let lower = verb.to_ascii_lowercase();
    match lower.as_str() {
        "help" | "?" => Ok(Request::Help),
        "user" | "u" | "player" => {
            let target = take(rest, 0, "player name or UUID", USAGE_USER)?.to_owned();
            let action = parse_subject(&rest[1..], now, true, USAGE_USER)?;
            Ok(Request::User { target, action })
        }
        "group" | "g" => {
            let name = take(rest, 0, "group name", USAGE_GROUP)?.to_owned();
            let action = parse_group(&rest[1..], now)?;
            Ok(Request::Group { name, action })
        }
        "creategroup" => {
            let name = take(rest, 0, "group name", USAGE_GROUP)?.to_owned();
            let action = parse_group(&[&["create".to_owned()][..], &rest[1..]].concat(), now)?;
            Ok(Request::Group { name, action })
        }
        "deletegroup" => Ok(Request::Group {
            name: take(rest, 0, "group name", USAGE_GROUP)?.to_owned(),
            action: GroupAction::Delete,
        }),
        "track" | "t" => {
            let name = take(rest, 0, "track name", USAGE_TRACK)?.to_owned();
            let action = parse_track(&rest[1..])?;
            Ok(Request::Track { name, action })
        }
        "createtrack" => Ok(Request::Track {
            name: take(rest, 0, "track name", USAGE_TRACK)?.to_owned(),
            action: TrackAction::Create(rest[1..].to_vec()),
        }),
        "deletetrack" => Ok(Request::Track {
            name: take(rest, 0, "track name", USAGE_TRACK)?.to_owned(),
            action: TrackAction::Delete,
        }),
        "check" | "trace" => {
            let target = take(rest, 0, "player name or UUID", USAGE_OTHER)?.to_owned();
            let node = take(rest, 1, "permission node", USAGE_OTHER)?.to_owned();
            let parsed = options(&rest[2..], false, now, USAGE_OTHER)?;
            let contexts = parsed.any_context.then_some(parsed.contexts);
            Ok(if lower == "check" {
                Request::Check {
                    target,
                    node,
                    contexts,
                }
            } else {
                Request::Trace {
                    target,
                    node,
                    contexts,
                }
            })
        }
        "listgroups" => no_more(rest, Request::ListGroups),
        "listtracks" => no_more(rest, Request::ListTracks),
        "search" => {
            let node = take(rest, 0, "permission node", USAGE_OTHER)?.to_owned();
            no_more(&rest[1..], Request::Search { node })
        }
        "import" => {
            let file = take(rest, 0, "file", USAGE_OTHER)?.to_owned();
            let replace = match &rest[1..] {
                [] => false,
                [flag] if flag == "--replace" => true,
                _ => return Err(syntax("import takes a file and --replace", USAGE_OTHER)),
            };
            Ok(Request::Import { file, replace })
        }
        "export" => {
            let file = take(rest, 0, "file", USAGE_OTHER)?.to_owned();
            no_more(&rest[1..], Request::Export { file })
        }
        "bulk" | "bulkupdate" => parse_bulk(rest, now).map(Request::Bulk),
        "sync" => no_more(rest, Request::Sync),
        "log" => parse_log(rest),
        "compiled" => {
            let target = take(rest, 0, "player name or UUID", USAGE_OTHER)?.to_owned();
            no_more(&rest[1..], Request::Compiled { target })
        }
        "backends" => no_more(rest, Request::Backends),
        "registered" => no_more(rest, Request::Registered),
        "doctor" => no_more(rest, Request::Doctor),
        _ => Err(syntax(format!("unknown command {verb:?}"), &usage())),
    }
}

fn no_more(rest: &[String], request: Request) -> Parsed<Request> {
    match rest.first() {
        None => Ok(request),
        Some(arg) => Err(syntax(format!("unexpected argument {arg:?}"), USAGE_OTHER)),
    }
}

fn parse_subject(
    args: &[String],
    now: u64,
    user: bool,
    usage: &[&'static str],
) -> Parsed<SubjectAction> {
    let Some(section) = args.first() else {
        return Ok(SubjectAction::Info);
    };
    let rest = &args[1..];
    match section.to_ascii_lowercase().as_str() {
        "info" => Ok(SubjectAction::Info),
        "permission" | "perm" | "p" => {
            parse_permission(rest, now, usage).map(SubjectAction::Permission)
        }
        "parent" | "group" => parse_parent(rest, now, user, usage).map(SubjectAction::Parent),
        "meta" => parse_meta(rest, now, usage).map(SubjectAction::Meta),
        verb @ ("promote" | "demote") if user => {
            let track = take(rest, 0, "track name", usage)?.to_owned();
            let contexts = contexts_only(&rest[1..], now, usage)?;
            Ok(if verb == "promote" {
                SubjectAction::Promote { track, contexts }
            } else {
                SubjectAction::Demote { track, contexts }
            })
        }
        "clear" => Ok(SubjectAction::Clear {
            contexts: clear_contexts(rest, now, usage)?,
        }),
        other => Err(syntax(format!("unknown action {other:?}"), usage)),
    }
}

fn parse_permission(args: &[String], now: u64, usage: &[&'static str]) -> Parsed<PermissionAction> {
    let action = take(args, 0, "permission action", usage)?.to_ascii_lowercase();
    let rest = &args[1..];
    match action.as_str() {
        "info" => Ok(PermissionAction::Info),
        "set" | "settemp" => {
            let node = take(rest, 0, "permission node", usage)?.to_owned();
            let mut index = 1;
            let value = match rest.get(1).and_then(|arg| parse_bool(arg)) {
                Some(value) => {
                    index = 2;
                    value
                }
                None if action == "settemp" => {
                    return Err(syntax("settemp needs true or false", usage));
                }
                None => true,
            };
            let mut expiry = None;
            if action == "settemp" {
                let time = take(rest, index, "time", usage)?;
                expiry = Some(parse_expiry(time, now).map_err(|e| syntax(e, usage))?);
                index += 1;
            }
            let parsed = options(&rest[index..], action == "set", now, usage)?;
            Ok(PermissionAction::Set {
                node,
                value,
                contexts: parsed.contexts,
                expiry: expiry.or(parsed.expiry),
            })
        }
        "unset" | "unsettemp" => Ok(PermissionAction::Unset {
            node: take(rest, 0, "permission node", usage)?.to_owned(),
            contexts: contexts_only(&rest[1..], now, usage)?,
            temporary: action == "unsettemp",
        }),
        "check" => Ok(PermissionAction::Check {
            node: take(rest, 0, "permission node", usage)?.to_owned(),
            contexts: contexts_only(&rest[1..], now, usage)?,
        }),
        other => Err(syntax(
            format!("unknown permission action {other:?}"),
            usage,
        )),
    }
}

fn parse_parent(
    args: &[String],
    now: u64,
    user: bool,
    usage: &[&'static str],
) -> Parsed<ParentAction> {
    let action = take(args, 0, "parent action", usage)?.to_ascii_lowercase();
    let rest = &args[1..];
    match action.as_str() {
        "info" | "list" => Ok(ParentAction::Info),
        "add" => {
            let group = take(rest, 0, "group", usage)?.to_owned();
            let parsed = options(&rest[1..], true, now, usage)?;
            Ok(ParentAction::Add {
                group,
                contexts: parsed.contexts,
                expiry: parsed.expiry,
            })
        }
        "addtemp" => {
            let group = take(rest, 0, "group", usage)?.to_owned();
            let time = take(rest, 1, "time", usage)?;
            let expiry = parse_expiry(time, now).map_err(|e| syntax(e, usage))?;
            Ok(ParentAction::Add {
                group,
                contexts: contexts_only(&rest[2..], now, usage)?,
                expiry: Some(expiry),
            })
        }
        "remove" | "removetemp" => Ok(ParentAction::Remove {
            group: take(rest, 0, "group", usage)?.to_owned(),
            contexts: contexts_only(&rest[1..], now, usage)?,
            temporary: action == "removetemp",
        }),
        "set" => Ok(ParentAction::Set {
            group: take(rest, 0, "group", usage)?.to_owned(),
            contexts: contexts_only(&rest[1..], now, usage)?,
        }),
        "switchprimarygroup" | "setprimarygroup" if user => {
            let group = take(rest, 0, "group", usage)?.to_owned();
            if let Some(arg) = rest.get(1) {
                return Err(syntax(format!("unexpected argument {arg:?}"), usage));
            }
            Ok(ParentAction::SwitchPrimary { group })
        }
        other => Err(syntax(format!("unknown parent action {other:?}"), usage)),
    }
}

fn parse_meta(args: &[String], now: u64, usage: &[&'static str]) -> Parsed<MetaAction> {
    let action = take(args, 0, "meta action", usage)?.to_ascii_lowercase();
    let rest = &args[1..];
    let decoration = |name: &str| {
        if name.contains("prefix") {
            Decoration::Prefix
        } else {
            Decoration::Suffix
        }
    };
    match action.as_str() {
        "info" => Ok(MetaAction::Info),
        "set" | "settemp" => {
            let key = take(rest, 0, "meta key", usage)?.to_owned();
            let value = take(rest, 1, "meta value", usage)?.to_owned();
            let (expiry, from) = if action == "settemp" {
                let time = take(rest, 2, "time", usage)?;
                (
                    Some(parse_expiry(time, now).map_err(|e| syntax(e, usage))?),
                    3,
                )
            } else {
                (None, 2)
            };
            let parsed = options(&rest[from..], action == "set", now, usage)?;
            Ok(MetaAction::Set {
                key,
                value,
                contexts: parsed.contexts,
                expiry: expiry.or(parsed.expiry),
            })
        }
        "unset" | "unsettemp" => Ok(MetaAction::Unset {
            key: take(rest, 0, "meta key", usage)?.to_owned(),
            contexts: contexts_only(&rest[1..], now, usage)?,
        }),
        "setprefix" | "addprefix" | "setsuffix" | "addsuffix" | "settempprefix"
        | "addtempprefix" | "settempsuffix" | "addtempsuffix" => {
            let priority = int(take(rest, 0, "priority", usage)?, "priority", usage)?;
            let text = take(rest, 1, "text", usage)?.to_owned();
            let temporary = action.contains("temp");
            let (expiry, from) = if temporary {
                let time = take(rest, 2, "time", usage)?;
                (
                    Some(parse_expiry(time, now).map_err(|e| syntax(e, usage))?),
                    3,
                )
            } else {
                (None, 2)
            };
            let parsed = options(&rest[from..], !temporary, now, usage)?;
            Ok(MetaAction::SetDecoration {
                which: decoration(&action),
                priority,
                text,
                contexts: parsed.contexts,
                expiry: expiry.or(parsed.expiry),
            })
        }
        "removeprefix" | "removesuffix" | "removetempprefix" | "removetempsuffix" => {
            Ok(MetaAction::RemoveDecoration {
                which: decoration(&action),
                priority: int(take(rest, 0, "priority", usage)?, "priority", usage)?,
                contexts: contexts_only(&rest[1..], now, usage)?,
            })
        }
        "clear" => Ok(MetaAction::Clear {
            contexts: clear_contexts(rest, now, usage)?,
        }),
        other => Err(syntax(format!("unknown meta action {other:?}"), usage)),
    }
}

fn parse_group(args: &[String], now: u64) -> Parsed<GroupAction> {
    let usage = USAGE_GROUP;
    let Some(section) = args.first() else {
        return Ok(GroupAction::Subject(SubjectAction::Info));
    };
    let rest = &args[1..];
    match section.to_ascii_lowercase().as_str() {
        "create" => {
            let (weight, name_from) = match rest.first() {
                Some(arg) if arg.parse::<i32>().is_ok() => (int(arg, "weight", usage)?, 1),
                _ => (0, 0),
            };
            let display_name = rest[name_from..].join(" ");
            Ok(GroupAction::Create {
                weight,
                display_name: (!display_name.is_empty()).then_some(display_name),
            })
        }
        "delete" => no_more_group(rest, GroupAction::Delete),
        "setweight" => {
            let weight = int(take(rest, 0, "weight", usage)?, "weight", usage)?;
            no_more_group(&rest[1..], GroupAction::SetWeight(weight))
        }
        "setdisplayname" => {
            let name = rest.join(" ");
            Ok(GroupAction::SetDisplayName(
                (!name.is_empty() && name != "--clear").then_some(name),
            ))
        }
        "listmembers" | "members" => no_more_group(rest, GroupAction::ListMembers),
        _ => parse_subject(args, now, false, usage).map(GroupAction::Subject),
    }
}

fn no_more_group(rest: &[String], action: GroupAction) -> Parsed<GroupAction> {
    match rest.first() {
        None => Ok(action),
        Some(arg) => Err(syntax(format!("unexpected argument {arg:?}"), USAGE_GROUP)),
    }
}

fn parse_track(args: &[String]) -> Parsed<TrackAction> {
    let usage = USAGE_TRACK;
    let Some(action) = args.first() else {
        return Ok(TrackAction::Info);
    };
    let rest = &args[1..];
    let one = |action: TrackAction| match rest.get(1) {
        None => Ok(action),
        Some(arg) => Err(syntax(format!("unexpected argument {arg:?}"), usage)),
    };
    match action.to_ascii_lowercase().as_str() {
        "create" => Ok(TrackAction::Create(rest.to_vec())),
        "delete" => no_track_args(rest, TrackAction::Delete),
        "info" => no_track_args(rest, TrackAction::Info),
        "clear" => no_track_args(rest, TrackAction::Clear),
        "append" => one(TrackAction::Append(
            take(rest, 0, "group", usage)?.to_owned(),
        )),
        "remove" => one(TrackAction::Remove(
            take(rest, 0, "group", usage)?.to_owned(),
        )),
        "insert" => {
            let group = take(rest, 0, "group", usage)?.to_owned();
            let position: usize = int(take(rest, 1, "position", usage)?, "position", usage)?;
            if position == 0 {
                return Err(syntax("positions start at 1", usage));
            }
            match rest.get(2) {
                None => Ok(TrackAction::Insert(group, position)),
                Some(arg) => Err(syntax(format!("unexpected argument {arg:?}"), usage)),
            }
        }
        other => Err(syntax(format!("unknown track action {other:?}"), usage)),
    }
}

fn no_track_args(rest: &[String], action: TrackAction) -> Parsed<TrackAction> {
    match rest.first() {
        None => Ok(action),
        Some(arg) => Err(syntax(format!("unexpected argument {arg:?}"), USAGE_TRACK)),
    }
}

fn parse_bulk(args: &[String], now: u64) -> Parsed<BulkRequest> {
    let usage = USAGE_OTHER;
    let mut request = BulkRequest {
        filter: Filter::default(),
        user: None,
        patch: Patch::default(),
        dry_run: false,
    };
    let mut contexts: Option<ContextSet> = None;
    let mut target: Option<ContextSet> = None;
    let mut args = args.iter().peekable();
    match args.peek().map(|arg| arg.to_ascii_lowercase()).as_deref() {
        Some("users") => {
            request.filter.subjects = SubjectFilter::Users;
            args.next();
        }
        Some("groups") => {
            request.filter.subjects = SubjectFilter::Groups;
            args.next();
        }
        Some("all") => {
            args.next();
        }
        _ => {}
    }
    let value = |args: &mut std::iter::Peekable<std::slice::Iter<'_, String>>, what: &str| {
        args.next()
            .cloned()
            .ok_or_else(|| syntax(format!("{what} needs a value"), usage))
    };
    while let Some(arg) = args.next() {
        match arg.to_ascii_lowercase().as_str() {
            "--dry-run" => request.dry_run = true,
            "key" => request.filter.key_like = Some(value(&mut args, "key")?),
            "kind" => {
                let kind = value(&mut args, "kind")?;
                request.filter.kind = Some(
                    kind.parse()
                        .map_err(|e: rift_permissions::Error| syntax(e.to_string(), usage))?,
                );
            }
            "value" => request.filter.value = Some(value(&mut args, "value")?),
            "source" => {
                let source = value(&mut args, "source")?;
                request.filter.source = Some(
                    source
                        .parse()
                        .map_err(|e: rift_permissions::Error| syntax(e.to_string(), usage))?,
                );
            }
            "subject" => {
                let subject = value(&mut args, "subject")?;
                match subject.split_once(':') {
                    Some(("group", name)) => {
                        request.filter.subject = Some(SubjectRef::Group(name.to_owned()));
                    }
                    Some(("user", name)) => request.user = Some(name.to_owned()),
                    _ => {
                        return Err(syntax("subject is user:<player> or group:<name>", usage));
                    }
                }
            }
            "context" => {
                let pair = value(&mut args, "context")?;
                let set = contexts.get_or_insert_with(ContextSet::new);
                if !pair.eq_ignore_ascii_case("global") {
                    let (key, value) = pair
                        .split_once('=')
                        .ok_or_else(|| syntax("context is key=value or global", usage))?;
                    set.insert(key, value)
                        .map_err(|e| syntax(e.to_string(), usage))?;
                }
            }
            "delete" => request.patch.delete = true,
            "set-value" => {
                let text = value(&mut args, "set-value")?;
                request.patch.value = Some(
                    parse_bool(&text)
                        .ok_or_else(|| syntax("set-value takes true or false", usage))?,
                );
            }
            "set-context" => {
                let pair = value(&mut args, "set-context")?;
                let set = target.get_or_insert_with(ContextSet::new);
                if !pair.eq_ignore_ascii_case("global") {
                    let (key, value) = pair
                        .split_once('=')
                        .ok_or_else(|| syntax("set-context is key=value or global", usage))?;
                    set.insert(key, value)
                        .map_err(|e| syntax(e.to_string(), usage))?;
                }
            }
            "set-expiry" => {
                let time = value(&mut args, "set-expiry")?;
                request.patch.expires_at =
                    Some(parse_expiry(&time, now).map_err(|e| syntax(e, usage))?);
            }
            "permanent" => request.patch.permanent = true,
            "set-source" => {
                let source = value(&mut args, "set-source")?;
                request.patch.source = Some(
                    source
                        .parse()
                        .map_err(|e: rift_permissions::Error| syntax(e.to_string(), usage))?,
                );
            }
            other => return Err(syntax(format!("unknown bulk argument {other:?}"), usage)),
        }
    }
    request.filter.contexts = contexts;
    request.patch.contexts = target;
    if request.patch.is_empty() {
        return Err(syntax(
            "bulk needs an action: delete, set-value, set-context, set-expiry, permanent or set-source",
            usage,
        ));
    }
    Ok(request)
}

fn parse_log(args: &[String]) -> Parsed<Request> {
    let usage = USAGE_OTHER;
    let (mut since, mut actor, mut subject, mut limit) = (None, None, None, 20);
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let value = args
            .next()
            .ok_or_else(|| syntax(format!("{flag} needs a value"), usage))?;
        match flag.as_str() {
            "--since" => since = Some(int(value, "revision", usage)?),
            "--actor" => actor = Some(value.clone()),
            "--subject" => subject = Some(value.clone()),
            "--limit" => {
                limit = int::<usize>(value, "limit", usage)?;
                if !(1..=1000).contains(&limit) {
                    return Err(syntax("--limit must be 1 to 1000", usage));
                }
            }
            other => return Err(syntax(format!("unknown log option {other:?}"), usage)),
        }
    }
    Ok(Request::Log {
        since,
        actor,
        subject,
        limit,
    })
}

// Execution.

/// Run `/perms` arguments for `sender`. Never panics on input; failures
/// come back as an [`Output`] with `ok == false`.
pub async fn execute(env: &Env, sender: &Sender, args: &[String]) -> Output {
    let now = unix_now();
    let request = match parse(args, now) {
        Ok(request) => request,
        Err(error) => {
            let mut output = Output::failed(error.message);
            for line in error.usage {
                output.line(Tone::Muted, line);
            }
            output.revision = env.authority.revision();
            return output;
        }
    };
    execute_request(env, sender, request).await
}

/// [`execute`] for an already parsed request.
pub async fn execute_request(env: &Env, sender: &Sender, request: Request) -> Output {
    if let Request::Sync = request {
        let mut output = match authorize(env, sender, &request) {
            Ok(()) => sync(env).await,
            Err(output) => output,
        };
        output.revision = env.authority.revision();
        return output;
    }
    let env = env.clone();
    let sender = sender.clone();
    let authority = env.authority.clone();
    let mut output = tokio::task::spawn_blocking(move || run(&env, &sender, request))
        .await
        .unwrap_or_else(|error| Output::failed(format!("command failed: {error}")));
    output.revision = authority.revision();
    output
}

/// Run a request synchronously; it may block on the database. Callers on
/// the async runtime use [`execute_request`]. `sync` is answered without
/// waiting for backends.
pub fn run(env: &Env, sender: &Sender, request: Request) -> Output {
    if let Err(output) = authorize(env, sender, &request) {
        return output;
    }
    let mut output = match dispatch(env, sender, request) {
        Ok(output) => output,
        Err(error) => Output::failed(error.message()),
    };
    output.revision = env.authority.revision();
    output
}

fn authorize(env: &Env, sender: &Sender, request: &Request) -> Result<(), Output> {
    let Sender::Player(uuid) = sender else {
        return Ok(());
    };
    if matches!(request, Request::Help) {
        return Ok(());
    }
    let node = request.node();
    if env.authority.check_active(*uuid, &node) == Tristate::True {
        Ok(())
    } else {
        Err(Output::failed(format!(
            "You do not have permission to do that ({node})."
        )))
    }
}

/// A failure while running a request.
enum Failure {
    Perm(PermError),
    Message(String),
}

impl Failure {
    fn message(self) -> String {
        match self {
            Self::Perm(error) => error.to_string(),
            Self::Message(message) => message,
        }
    }
}

impl From<PermError> for Failure {
    fn from(error: PermError) -> Self {
        Self::Perm(error)
    }
}

impl From<rift_permissions::Error> for Failure {
    fn from(error: rift_permissions::Error) -> Self {
        Self::Perm(error.into())
    }
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

type Run<T> = std::result::Result<T, Failure>;

/// Resolve a player name or UUID.
pub fn resolve_user(authority: &Authority, target: &str) -> std::result::Result<Uuid, String> {
    if let Ok(uuid) = Uuid::parse(target) {
        return Ok(uuid);
    }
    if !crate::players::valid_username(target) {
        return Err(format!("{target:?} is neither a player name nor a UUID"));
    }
    match authority.user_by_name(target) {
        Ok(Some(uuid)) => Ok(uuid),
        Ok(None) => Err(format!(
            "Unknown player {target:?}: they must have joined once, or use their UUID."
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// `Alice (0123…)` or the bare UUID.
fn user_label(authority: &Authority, uuid: Uuid) -> String {
    match authority.last_name(uuid) {
        Some(name) => format!("{name} ({uuid})"),
        None => uuid.to_string(),
    }
}

fn subject_label(authority: &Authority, subject: &SubjectRef) -> String {
    match subject {
        SubjectRef::User(uuid) => authority
            .last_name(*uuid)
            .unwrap_or_else(|| uuid.to_string()),
        SubjectRef::Group(name) => format!("group {name}"),
    }
}

fn group_name(name: &str) -> Run<String> {
    Ok(rift_permissions::subject::normalize_group(name)?)
}

fn require_group(authority: &Authority, name: &str) -> Run<String> {
    let name = group_name(name)?;
    if authority.group(&name).is_none() {
        return Err(PermError::NotFound {
            what: "group",
            name,
        }
        .into());
    }
    Ok(name)
}

fn dispatch(env: &Env, sender: &Sender, request: Request) -> Run<Output> {
    let authority = &*env.authority;
    match request {
        Request::Help => {
            let mut output = Output::new();
            output.line(Tone::Heading, "Rift permissions");
            for line in usage() {
                output.line(Tone::Plain, line);
            }
            Ok(output)
        }
        Request::User { target, action } => {
            let uuid = resolve_user(authority, &target)?;
            subject_action(env, sender, SubjectRef::User(uuid), action)
        }
        Request::Group { name, action } => group_action(env, sender, &name, action),
        Request::Track { name, action } => track_action(env, sender, &name, action),
        Request::Check {
            target,
            node,
            contexts,
        } => {
            let uuid = resolve_user(authority, &target)?;
            let node = rift_permissions::node::normalize_node(&node)?;
            let active = contexts.unwrap_or_else(|| authority.contexts(uuid));
            let result = authority.check(uuid, &node, &active);
            let mut output = Output::new();
            let (tone, word) = match result {
                Tristate::True => (Tone::Good, "true"),
                Tristate::False => (Tone::Bad, "false"),
                Tristate::Undefined => (Tone::Muted, "undefined"),
            };
            output.line(
                tone,
                format!(
                    "{} has {node} = {word} in {}",
                    user_label(authority, uuid),
                    contexts_text(&active)
                ),
            );
            output.data =
                Some(json!({"uuid": uuid, "node": node, "contexts": active, "result": result}));
            Ok(output)
        }
        Request::Trace {
            target,
            node,
            contexts,
        } => {
            let uuid = resolve_user(authority, &target)?;
            let active = contexts.unwrap_or_else(|| authority.contexts(uuid));
            let trace = authority.trace(uuid, &node, &active);
            let mut output = Output::new();
            output.lines = trace_lines(authority, uuid, &active, &trace);
            output.data = Some(json!({"uuid": uuid, "contexts": active, "trace": trace}));
            Ok(output)
        }
        Request::ListGroups => {
            let mut groups: Vec<_> = authority.groups().values().cloned().collect();
            groups.sort_by(|a, b| b.weight.cmp(&a.weight).then(a.name.cmp(&b.name)));
            let mut output = Output::new();
            output.line(Tone::Heading, format!("{} groups", groups.len()));
            for group in &groups {
                let mut line = format!("{} (weight {})", group.name, group.weight);
                if let Some(display) = &group.display_name {
                    let _ = write!(line, " \"{display}\"");
                }
                let _ = write!(line, ", {} nodes", group.nodes.len());
                output.line(Tone::Plain, line);
            }
            Ok(output)
        }
        Request::ListTracks => {
            let tracks = authority.tracks();
            let mut output = Output::new();
            output.line(Tone::Heading, format!("{} tracks", tracks.len()));
            for track in tracks.values() {
                output.line(
                    Tone::Plain,
                    format!("{}: {}", track.name, track_text(&track.groups)),
                );
            }
            Ok(output)
        }
        Request::Search { node } => search(authority, &node),
        Request::Import { file, replace } => {
            let path = file_path(sender, &file)?;
            let reader = std::fs::File::open(&path)
                .map_err(|e| format!("cannot open {}: {e}", path.display()))?;
            let report = authority.import_luckperms(sender.actor(), reader, replace)?;
            let mut output = Output::new();
            output.line(
                Tone::Good,
                format!(
                    "Imported {} groups, {} users and {} tracks from {} ({} changes, revision {})",
                    report.groups,
                    report.users,
                    report.tracks,
                    path.display(),
                    report.ops,
                    report.revision
                ),
            );
            if !report.skipped.is_empty() {
                output.line(Tone::Bad, format!("Skipped {}:", report.skipped.len()));
                for skipped in report.skipped.iter().take(20) {
                    output.line(
                        Tone::Muted,
                        format!("  {} {}: {}", skipped.subject, skipped.key, skipped.reason),
                    );
                }
            }
            output.data = Some(serde_json::to_value(&report).unwrap_or_default());
            Ok(output)
        }
        Request::Export { file } => {
            let path = file_path(sender, &file)?;
            let export = authority.export_luckperms()?;
            let gzip = path
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("gz"));
            let file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&path)
                .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            write_export(&export, std::io::BufWriter::new(file), gzip)
                .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
            let mut output = Output::new();
            output.line(
                Tone::Good,
                format!(
                    "Exported {} groups, {} users and {} tracks to {} (LuckPerms format)",
                    export.groups.len(),
                    export.users.len(),
                    export.tracks.len(),
                    path.display()
                ),
            );
            Ok(output)
        }
        Request::Bulk(request) => bulk(env, sender, request),
        Request::Sync => {
            // Synchronous callers do not wait for the backends to answer.
            let revision = request_resync(env)?;
            let mut output = Output::new();
            output.line(
                Tone::Good,
                format!("Asked every backend to resync to revision {revision}."),
            );
            Ok(output)
        }
        Request::Log {
            since,
            actor,
            subject,
            limit,
        } => log(authority, since, actor, subject, limit),
        Request::Compiled { target } => {
            let uuid = resolve_user(authority, &target)?;
            let subject = authority.compiled(uuid);
            let mut output = Output::new();
            output.line(
                Tone::Plain,
                serde_json::to_string_pretty(&*subject).unwrap_or_default(),
            );
            output.data = Some(serde_json::to_value(&*subject).unwrap_or_default());
            Ok(output)
        }
        Request::Registered => {
            let registered = authority.registered()?;
            let mut output = Output::new();
            output.line(
                Tone::Heading,
                format!("{} registered permissions", registered.len()),
            );
            for entry in registered.iter().take(LIST_LIMIT) {
                let permission = &entry.permission;
                let mut line = format!(
                    "  {} (default {}) from {}",
                    permission.node, permission.default, entry.owner
                );
                if let Some(plugin) = &permission.plugin {
                    let _ = write!(line, "/{plugin}");
                }
                if let Some(description) = &permission.description {
                    let _ = write!(line, ": {description}");
                }
                output.line(Tone::Plain, line);
            }
            output.data = Some(serde_json::to_value(&registered).unwrap_or_default());
            Ok(output)
        }
        Request::Backends => Ok(backends_output(env)),
        Request::Doctor => doctor(env),
    }
}

/// Apply `change` for `sender`, checking escalation for players.
fn apply(env: &Env, sender: &Sender, change: Change) -> std::result::Result<Applied, Failure> {
    if let Sender::Player(uuid) = sender {
        escalation(&env.authority, *uuid, &change).map_err(Failure::Message)?;
    }
    Ok(env
        .authority
        .apply_with(sender.actor(), change, Some("/perms"), None)?)
}

/// Refuse a change that grants `actor` something they do not hold, unless
/// they hold `rift.perms.escalate`. A grant is a positive permission or
/// group membership the change adds or alters; the actor must hold the
/// permission (or be in the group) in the node's contexts, completed with
/// their own current contexts.
pub fn escalation(
    authority: &Authority,
    actor: Uuid,
    change: &Change,
) -> std::result::Result<(), String> {
    if authority.check_active(actor, ESCALATE) == Tristate::True {
        return Ok(());
    }
    let ops = authority
        .preview(change.clone())
        .map_err(|e| e.to_string())?;
    let mine = authority.contexts(actor);
    let compiled = authority.compiled(actor);
    let now = unix_now();
    for op in &ops {
        let Op::Node {
            after: Some(node),
            before,
            ..
        } = op
        else {
            continue;
        };
        if before.as_ref() == Some(node) {
            continue;
        }
        let mut active = node.contexts.clone();
        for (key, value) in mine.iter() {
            if active.get(key).is_none() {
                let _ = active.insert(key, value);
            }
        }
        let (held, what) = match &node.payload {
            Payload::Permission { key, value: true } => (
                compiled.check(key, &active, now) == Tristate::True,
                format!("the permission {key}"),
            ),
            Payload::Inherit { group, value: true } => (
                compiled.in_group(group, &active, now),
                format!("the group {group}"),
            ),
            _ => continue,
        };
        if !held {
            return Err(format!(
                "You cannot grant {what} because you do not have it yourself ({ESCALATE} allows it)."
            ));
        }
    }
    Ok(())
}

/// A bare file name in the working directory for players; any path for the
/// console.
fn file_path(sender: &Sender, file: &str) -> Run<PathBuf> {
    let path = Path::new(file);
    if matches!(sender, Sender::Player(_)) {
        let mut components = path.components();
        let bare = matches!(
            (components.next(), components.next()),
            (Some(Component::Normal(_)), None)
        ) && !file.starts_with('.');
        if !bare {
            return Err(Failure::Message(format!(
                "{file:?}: in game, name a file in the proxy's working directory without a path"
            )));
        }
    }
    if file.is_empty() {
        return Err(Failure::Message("missing file".into()));
    }
    Ok(path.to_path_buf())
}

fn changed(output: &mut Output, applied: &Applied, done: String, unchanged: String) {
    if applied.ops == 0 {
        output.line(Tone::Muted, unchanged);
    } else {
        output.line(
            Tone::Good,
            format!("{done} (revision {})", applied.revision),
        );
    }
    output.data = Some(serde_json::to_value(applied).unwrap_or_default());
}

fn in_contexts(contexts: &ContextSet) -> String {
    if contexts.is_empty() {
        String::new()
    } else {
        format!(" in {contexts}")
    }
}

fn contexts_text(contexts: &ContextSet) -> String {
    if contexts.is_empty() {
        "no contexts".into()
    } else {
        contexts.to_string()
    }
}

fn expiry_text(expiry: Option<u64>, now: u64) -> String {
    match expiry {
        None => String::new(),
        Some(at) if at <= now => " (expired)".into(),
        Some(at) => format!(" for {}", duration_text(at - now)),
    }
}

/// `1d 2h 5m`, at most the two largest units.
pub fn duration_text(seconds: u64) -> String {
    let units = [(86_400, "d"), (3600, "h"), (60, "m"), (1, "s")];
    let mut parts = Vec::new();
    let mut rest = seconds;
    for (size, unit) in units {
        if rest >= size {
            parts.push(format!("{}{unit}", rest / size));
            rest %= size;
        }
        if parts.len() == 2 {
            break;
        }
    }
    if parts.is_empty() {
        "0s".into()
    } else {
        parts.join(" ")
    }
}

/// One node as text: `essentials.fly = true (server=survival) for 2h [import]`.
pub fn describe_node(node: &Node, now: u64) -> String {
    let mut text = match &node.payload {
        Payload::Permission { key, value } => format!("{key} = {value}"),
        Payload::Inherit { group, value: true } => format!("parent {group}"),
        Payload::Inherit {
            group,
            value: false,
        } => format!("blocks parent {group}"),
        Payload::Prefix { priority, text } => format!("prefix {priority} \"{text}\""),
        Payload::Suffix { priority, text } => format!("suffix {priority} \"{text}\""),
        Payload::Meta { key, value } => format!("meta {key} = \"{value}\""),
        Payload::Weight { weight } => format!("weight {weight}"),
        Payload::DisplayName { name } => format!("display name \"{name}\""),
    };
    if !node.contexts.is_empty() {
        let _ = write!(text, " ({})", node.contexts);
    }
    text.push_str(&expiry_text(node.expires_at, now));
    if node.source != rift_permissions::Source::Store {
        let _ = write!(text, " [{}]", node.source);
    }
    text
}

fn track_text(groups: &[String]) -> String {
    if groups.is_empty() {
        "(empty)".into()
    } else {
        groups.join(" → ")
    }
}

fn subject_nodes(authority: &Authority, subject: &SubjectRef) -> Run<Vec<Node>> {
    match subject {
        SubjectRef::User(uuid) => Ok(authority.user(*uuid)?.nodes),
        SubjectRef::Group(name) => {
            authority
                .group(name)
                .map(|group| group.nodes)
                .ok_or_else(|| {
                    PermError::NotFound {
                        what: "group",
                        name: name.clone(),
                    }
                    .into()
                })
        }
    }
}

fn list_nodes(
    output: &mut Output,
    title: &str,
    nodes: &[Node],
    keep: impl Fn(&Node) -> bool,
    now: u64,
) {
    let selected: Vec<&Node> = nodes.iter().filter(|node| keep(node)).collect();
    output.line(Tone::Heading, format!("{title} ({})", selected.len()));
    for node in selected.iter().take(LIST_LIMIT) {
        output.line(Tone::Plain, format!("  {}", describe_node(node, now)));
    }
    if selected.len() > LIST_LIMIT {
        output.line(
            Tone::Muted,
            format!("  … and {} more", selected.len() - LIST_LIMIT),
        );
    }
}

fn subject_action(
    env: &Env,
    sender: &Sender,
    subject: SubjectRef,
    action: SubjectAction,
) -> Run<Output> {
    let authority = &*env.authority;
    let label = subject_label(authority, &subject);
    let now = unix_now();
    let mut output = Output::new();
    match action {
        SubjectAction::Info => return info(authority, &subject),
        SubjectAction::Permission(action) => match action {
            PermissionAction::Info => {
                let nodes = subject_nodes(authority, &subject)?;
                output.line(Tone::Heading, format!("Permissions of {label}"));
                list_nodes(
                    &mut output,
                    "permissions",
                    &nodes,
                    |node| node.kind() == NodeKind::Permission,
                    now,
                );
            }
            PermissionAction::Set {
                node,
                value,
                contexts,
                expiry,
            } => {
                let mut built = Node::permission(&node, value)?.with_contexts(contexts.clone())?;
                if let Some(at) = expiry {
                    built = built.expiring_at(at);
                }
                let key = built.payload.key();
                let applied = apply(
                    env,
                    sender,
                    Change::SetNode {
                        subject: subject.clone(),
                        node: built,
                    },
                )?;
                changed(
                    &mut output,
                    &applied,
                    format!(
                        "Set {key} to {value} for {label}{}{}",
                        in_contexts(&contexts),
                        expiry_text(expiry, now)
                    ),
                    format!(
                        "{label} already has {key} set to {value}{}",
                        in_contexts(&contexts)
                    ),
                );
            }
            PermissionAction::Unset {
                node,
                contexts,
                temporary,
            } => {
                let key = NodeKey::new(NodeKind::Permission, &node, contexts.clone())?;
                if temporary {
                    require_temporary(authority, &subject, &key)?;
                }
                let name = key.key.clone();
                let applied = apply(
                    env,
                    sender,
                    Change::UnsetNode {
                        subject: subject.clone(),
                        key,
                    },
                )?;
                changed(
                    &mut output,
                    &applied,
                    format!("Unset {name} for {label}{}", in_contexts(&contexts)),
                    format!("{label} does not have {name} set{}", in_contexts(&contexts)),
                );
            }
            PermissionAction::Check { node, contexts } => {
                let node = rift_permissions::node::normalize_node(&node)?;
                let nodes = subject_nodes(authority, &subject)?;
                let own: Vec<&Node> = nodes
                    .iter()
                    .filter(
                        |n| matches!(&n.payload, Payload::Permission { key, .. } if *key == node),
                    )
                    .collect();
                if own.is_empty() {
                    output.line(Tone::Muted, format!("{label} does not set {node} directly"));
                }
                for found in own {
                    output.line(
                        Tone::Plain,
                        format!("{label} sets {}", describe_node(found, now)),
                    );
                }
                if let SubjectRef::User(uuid) = &subject {
                    let active = if contexts.is_empty() {
                        authority.contexts(*uuid)
                    } else {
                        contexts
                    };
                    let result = authority.check(*uuid, &node, &active);
                    output.line(
                        Tone::Plain,
                        format!(
                            "Resolved in {}: {}",
                            contexts_text(&active),
                            tristate_text(result)
                        ),
                    );
                }
            }
        },
        SubjectAction::Parent(action) => match action {
            ParentAction::Info => {
                let nodes = subject_nodes(authority, &subject)?;
                output.line(Tone::Heading, format!("Parents of {label}"));
                list_nodes(
                    &mut output,
                    "parents",
                    &nodes,
                    |node| node.kind() == NodeKind::Inherit,
                    now,
                );
                if let SubjectRef::User(uuid) = &subject {
                    output.line(
                        Tone::Plain,
                        format!("Primary group: {}", authority.compiled(*uuid).primary_group),
                    );
                }
            }
            ParentAction::Add {
                group,
                contexts,
                expiry,
            } => {
                let group = require_group(authority, &group)?;
                let mut node = Node::inherit(&group, true)?.with_contexts(contexts.clone())?;
                if let Some(at) = expiry {
                    node = node.expiring_at(at);
                }
                let applied = apply(
                    env,
                    sender,
                    Change::SetNode {
                        subject: subject.clone(),
                        node,
                    },
                )?;
                changed(
                    &mut output,
                    &applied,
                    format!(
                        "{label} now inherits {group}{}{}",
                        in_contexts(&contexts),
                        expiry_text(expiry, now)
                    ),
                    format!("{label} already inherits {group}{}", in_contexts(&contexts)),
                );
            }
            ParentAction::Remove {
                group,
                contexts,
                temporary,
            } => {
                let key = NodeKey::new(NodeKind::Inherit, &group, contexts.clone())?;
                if temporary {
                    require_temporary(authority, &subject, &key)?;
                }
                let group = key.key.clone();
                let applied = apply(
                    env,
                    sender,
                    Change::UnsetNode {
                        subject: subject.clone(),
                        key,
                    },
                )?;
                changed(
                    &mut output,
                    &applied,
                    format!(
                        "{label} no longer inherits {group}{}",
                        in_contexts(&contexts)
                    ),
                    format!("{label} does not inherit {group}{}", in_contexts(&contexts)),
                );
            }
            ParentAction::Set { group, contexts } => {
                let group = require_group(authority, &group)?;
                let nodes = subject_nodes(authority, &subject)?;
                let mut changes: Vec<Change> = nodes
                    .iter()
                    .filter(|node| node.kind() == NodeKind::Inherit && node.contexts == contexts)
                    .filter(|node| !matches!(&node.payload, Payload::Inherit { group: g, value: true } if *g == group))
                    .map(|node| Change::UnsetNode {
                        subject: subject.clone(),
                        key: node.key(),
                    })
                    .collect();
                changes.push(Change::SetNode {
                    subject: subject.clone(),
                    node: Node::inherit(&group, true)?.with_contexts(contexts.clone())?,
                });
                let applied = apply(env, sender, Change::Batch { changes })?;
                changed(
                    &mut output,
                    &applied,
                    format!(
                        "{label}'s only parent is now {group}{}",
                        in_contexts(&contexts)
                    ),
                    format!("{label} already has only {group}{}", in_contexts(&contexts)),
                );
            }
            ParentAction::SwitchPrimary { group } => {
                let SubjectRef::User(uuid) = subject else {
                    return Err(Failure::Message("groups have no primary group".into()));
                };
                let group = require_group(authority, &group)?;
                let applied = apply(
                    env,
                    sender,
                    Change::SetPrimaryGroup {
                        user: uuid,
                        group: Some(group.clone()),
                    },
                )?;
                changed(
                    &mut output,
                    &applied,
                    format!("{label}'s primary group is now {group}"),
                    format!("{label}'s primary group already is {group}"),
                );
            }
        },
        SubjectAction::Meta(action) => match action {
            MetaAction::Info => {
                let nodes = subject_nodes(authority, &subject)?;
                output.line(Tone::Heading, format!("Meta of {label}"));
                list_nodes(
                    &mut output,
                    "prefixes and suffixes",
                    &nodes,
                    |node| matches!(node.kind(), NodeKind::Prefix | NodeKind::Suffix),
                    now,
                );
                list_nodes(
                    &mut output,
                    "meta",
                    &nodes,
                    |node| node.kind() == NodeKind::Meta,
                    now,
                );
                if let SubjectRef::User(uuid) = &subject {
                    let view = authority.meta(*uuid, &authority.contexts(*uuid));
                    output.line(
                        Tone::Plain,
                        format!(
                            "In effect: prefix {}, suffix {}",
                            view.prefix
                                .as_deref()
                                .map_or("none".into(), |p| format!("\"{p}\"")),
                            view.suffix
                                .as_deref()
                                .map_or("none".into(), |s| format!("\"{s}\""))
                        ),
                    );
                }
            }
            MetaAction::Set {
                key,
                value,
                contexts,
                expiry,
            } => {
                let mut node = Node::meta(&key, &value)?.with_contexts(contexts.clone())?;
                if let Some(at) = expiry {
                    node = node.expiring_at(at);
                }
                let key = node.payload.key();
                let applied = apply(
                    env,
                    sender,
                    Change::SetNode {
                        subject: subject.clone(),
                        node,
                    },
                )?;
                changed(
                    &mut output,
                    &applied,
                    format!(
                        "Set meta {key} to \"{value}\" for {label}{}{}",
                        in_contexts(&contexts),
                        expiry_text(expiry, now)
                    ),
                    format!("{label} already has meta {key} = \"{value}\""),
                );
            }
            MetaAction::Unset { key, contexts } => {
                let key = NodeKey::new(NodeKind::Meta, &key, contexts.clone())?;
                let name = key.key.clone();
                let applied = apply(
                    env,
                    sender,
                    Change::UnsetNode {
                        subject: subject.clone(),
                        key,
                    },
                )?;
                changed(
                    &mut output,
                    &applied,
                    format!("Unset meta {name} for {label}{}", in_contexts(&contexts)),
                    format!("{label} has no meta {name}{}", in_contexts(&contexts)),
                );
            }
            MetaAction::SetDecoration {
                which,
                priority,
                text,
                contexts,
                expiry,
            } => {
                let node = match which {
                    Decoration::Prefix => Node::prefix(priority, &text)?,
                    Decoration::Suffix => Node::suffix(priority, &text)?,
                };
                let mut node = node.with_contexts(contexts.clone())?;
                if let Some(at) = expiry {
                    node = node.expiring_at(at);
                }
                let applied = apply(
                    env,
                    sender,
                    Change::SetNode {
                        subject: subject.clone(),
                        node,
                    },
                )?;
                let name = which.name();
                changed(
                    &mut output,
                    &applied,
                    format!(
                        "Set {name} \"{text}\" at priority {priority} for {label}{}{}",
                        in_contexts(&contexts),
                        expiry_text(expiry, now)
                    ),
                    format!("{label} already has that {name}"),
                );
            }
            MetaAction::RemoveDecoration {
                which,
                priority,
                contexts,
            } => {
                let key = NodeKey::new(which.kind(), &priority.to_string(), contexts.clone())?;
                let applied = apply(
                    env,
                    sender,
                    Change::UnsetNode {
                        subject: subject.clone(),
                        key,
                    },
                )?;
                let name = which.name();
                changed(
                    &mut output,
                    &applied,
                    format!("Removed the {name} at priority {priority} from {label}"),
                    format!(
                        "{label} has no {name} at priority {priority}{}",
                        in_contexts(&contexts)
                    ),
                );
            }
            MetaAction::Clear { contexts } => {
                let nodes = subject_nodes(authority, &subject)?;
                let changes: Vec<Change> = nodes
                    .iter()
                    .filter(|node| {
                        matches!(
                            node.kind(),
                            NodeKind::Prefix | NodeKind::Suffix | NodeKind::Meta
                        )
                    })
                    .filter(|node| contexts.as_ref().is_none_or(|c| node.contexts == *c))
                    .map(|node| Change::UnsetNode {
                        subject: subject.clone(),
                        key: node.key(),
                    })
                    .collect();
                if changes.is_empty() {
                    output.line(Tone::Muted, format!("{label} has no meta to clear"));
                    return Ok(output);
                }
                let applied = apply(env, sender, Change::Batch { changes })?;
                changed(
                    &mut output,
                    &applied,
                    format!("Cleared the meta of {label}"),
                    format!("{label} has no meta to clear"),
                );
            }
        },
        SubjectAction::Promote { track, contexts } => {
            return track_move(env, sender, &subject, &track, contexts, true);
        }
        SubjectAction::Demote { track, contexts } => {
            return track_move(env, sender, &subject, &track, contexts, false);
        }
        SubjectAction::Clear { contexts } => {
            let applied = apply(
                env,
                sender,
                Change::ClearSubject {
                    subject: subject.clone(),
                    contexts: contexts.clone(),
                },
            )?;
            let scope = contexts.as_ref().map_or(String::new(), in_contexts);
            changed(
                &mut output,
                &applied,
                format!("Cleared every node of {label}{scope}"),
                format!("{label} has no nodes to clear{scope}"),
            );
        }
    }
    Ok(output)
}

fn tristate_text(result: Tristate) -> &'static str {
    match result {
        Tristate::True => "true",
        Tristate::False => "false",
        Tristate::Undefined => "undefined",
    }
}

/// `unsettemp` and `removetemp` only remove temporary nodes.
fn require_temporary(authority: &Authority, subject: &SubjectRef, key: &NodeKey) -> Run<()> {
    let nodes = subject_nodes(authority, subject)?;
    match nodes.iter().find(|node| node.key() == *key) {
        Some(node) if node.expires_at.is_some() => Ok(()),
        Some(_) => Err(Failure::Message(format!(
            "{} is permanent; use the non-temporary command to remove it",
            key.key
        ))),
        None => Err(Failure::Message(format!(
            "{} has no temporary {} {}{}",
            subject_label(authority, subject),
            key.kind,
            key.key,
            in_contexts(&key.contexts)
        ))),
    }
}

fn info(authority: &Authority, subject: &SubjectRef) -> Run<Output> {
    let now = unix_now();
    let mut output = Output::new();
    let nodes = subject_nodes(authority, subject)?;
    match subject {
        SubjectRef::User(uuid) => {
            output.line(
                Tone::Heading,
                format!("User {}", user_label(authority, *uuid)),
            );
            match authority.server_of(*uuid) {
                Some(server) => output.line(Tone::Good, format!("Online on {server}")),
                None => output.line(Tone::Muted, "Offline"),
            }
            let compiled = authority.compiled(*uuid);
            let active = authority.contexts(*uuid);
            let view = compiled.meta(&active, now);
            output.line(
                Tone::Plain,
                format!("Primary group: {}", compiled.primary_group),
            );
            output.line(
                Tone::Plain,
                format!(
                    "Groups in effect: {}",
                    compiled.groups(&active, now).join(", ")
                ),
            );
            output.line(
                Tone::Plain,
                format!("Active contexts: {}", contexts_text(&active)),
            );
            if let Some(prefix) = &view.prefix {
                output.line(Tone::Plain, format!("Prefix: \"{prefix}\""));
            }
            if let Some(suffix) = &view.suffix {
                output.line(Tone::Plain, format!("Suffix: \"{suffix}\""));
            }
        }
        SubjectRef::Group(name) => {
            let group = authority.group(name).ok_or_else(|| PermError::NotFound {
                what: "group",
                name: name.clone(),
            })?;
            output.line(Tone::Heading, format!("Group {}", group.name));
            output.line(Tone::Plain, format!("Weight: {}", group.weight));
            if let Some(display) = &group.display_name {
                output.line(Tone::Plain, format!("Display name: \"{display}\""));
            }
            let (members, more) = authority.group_members(&group.name, 1000)?;
            output.line(
                Tone::Plain,
                format!(
                    "Direct members: {}{}",
                    members.len(),
                    if more { "+" } else { "" }
                ),
            );
            let tracks: Vec<String> = authority
                .tracks()
                .values()
                .filter(|track| track.groups.contains(&group.name))
                .map(|track| track.name.clone())
                .collect();
            if !tracks.is_empty() {
                output.line(Tone::Plain, format!("On tracks: {}", tracks.join(", ")));
            }
        }
    }
    list_nodes(
        &mut output,
        "Parents",
        &nodes,
        |n| n.kind() == NodeKind::Inherit,
        now,
    );
    list_nodes(
        &mut output,
        "Permissions",
        &nodes,
        |n| n.kind() == NodeKind::Permission,
        now,
    );
    list_nodes(
        &mut output,
        "Prefixes, suffixes and meta",
        &nodes,
        |n| {
            matches!(
                n.kind(),
                NodeKind::Prefix | NodeKind::Suffix | NodeKind::Meta
            )
        },
        now,
    );
    output.data = Some(json!({"subject": subject, "nodes": nodes}));
    Ok(output)
}

fn track_move(
    env: &Env,
    sender: &Sender,
    subject: &SubjectRef,
    track: &str,
    contexts: ContextSet,
    promote: bool,
) -> Run<Output> {
    let authority = &*env.authority;
    let SubjectRef::User(uuid) = subject else {
        return Err(Failure::Message(
            "only users are promoted and demoted".into(),
        ));
    };
    let track = rift_permissions::subject::normalize_track(track)?;
    if !authority.tracks().contains_key(&track) {
        return Err(PermError::NotFound {
            what: "track",
            name: track,
        }
        .into());
    }
    let change = if promote {
        Change::Promote {
            user: *uuid,
            track: track.clone(),
            contexts: contexts.clone(),
        }
    } else {
        Change::Demote {
            user: *uuid,
            track: track.clone(),
            contexts: contexts.clone(),
        }
    };
    let applied = apply(env, sender, change)?;
    let label = user_label(authority, *uuid);
    let mut output = Output::new();
    let (mut from, mut to) = (None, None);
    if applied.ops > 0
        && let Ok(events) = authority.log(&LogQuery {
            since: Some(applied.revision - 1),
            limit: 1,
            ..LogQuery::default()
        })
    {
        for op in events.iter().flat_map(|event| &event.ops) {
            if let Op::Node { before, after, .. } = op {
                if let Some(Node {
                    payload: Payload::Inherit { group, .. },
                    ..
                }) = before
                {
                    from = Some(group.clone());
                }
                if let Some(Node {
                    payload: Payload::Inherit { group, .. },
                    ..
                }) = after
                {
                    to = Some(group.clone());
                }
            }
        }
    }
    let verb = if promote { "Promoted" } else { "Demoted" };
    let from = from.unwrap_or_else(|| DEFAULT_GROUP.into());
    let to = to.unwrap_or_else(|| DEFAULT_GROUP.into());
    changed(
        &mut output,
        &applied,
        format!(
            "{verb} {label} on {track}: {from} → {to}{}",
            in_contexts(&contexts)
        ),
        format!("{label} did not move on {track}"),
    );
    Ok(output)
}

fn group_action(env: &Env, sender: &Sender, name: &str, action: GroupAction) -> Run<Output> {
    let authority = &*env.authority;
    let mut output = Output::new();
    match action {
        GroupAction::Subject(action) => {
            let name = require_group(authority, name)?;
            return subject_action(env, sender, SubjectRef::Group(name), action);
        }
        GroupAction::Create {
            weight,
            display_name,
        } => {
            let name = group_name(name)?;
            if authority.group(&name).is_some() {
                return Err(Failure::Message(format!("group {name} already exists")));
            }
            let applied = apply(
                env,
                sender,
                Change::CreateGroup {
                    name: name.clone(),
                    weight,
                    display_name,
                },
            )?;
            changed(
                &mut output,
                &applied,
                format!("Created group {name} with weight {weight}"),
                format!("group {name} already exists"),
            );
        }
        GroupAction::Delete => {
            let name = require_group(authority, name)?;
            let applied = apply(env, sender, Change::DeleteGroup { name: name.clone() })?;
            changed(
                &mut output,
                &applied,
                format!("Deleted group {name}; parent nodes naming it now grant nothing"),
                format!("group {name} does not exist"),
            );
        }
        GroupAction::SetWeight(weight) => {
            let name = require_group(authority, name)?;
            let applied = apply(
                env,
                sender,
                Change::SetWeight {
                    group: name.clone(),
                    weight,
                },
            )?;
            changed(
                &mut output,
                &applied,
                format!("Set the weight of {name} to {weight}"),
                format!("{name} already has weight {weight}"),
            );
        }
        GroupAction::SetDisplayName(display_name) => {
            let name = require_group(authority, name)?;
            let shown = display_name
                .as_deref()
                .map_or("cleared".into(), |d| format!("\"{d}\""));
            let applied = apply(
                env,
                sender,
                Change::SetDisplayName {
                    group: name.clone(),
                    display_name,
                },
            )?;
            changed(
                &mut output,
                &applied,
                format!("Display name of {name}: {shown}"),
                format!("{name} already has that display name"),
            );
        }
        GroupAction::ListMembers => {
            let name = require_group(authority, name)?;
            let (members, more) = authority.group_members(&name, LIST_LIMIT)?;
            output.line(
                Tone::Heading,
                format!(
                    "Members of {name} ({}{})",
                    members.len(),
                    if more { "+" } else { "" }
                ),
            );
            for member in &members {
                output.line(
                    Tone::Plain,
                    match &member.name {
                        Some(last) => format!("  {last} ({})", member.uuid),
                        None => format!("  {}", member.uuid),
                    },
                );
            }
            output.data = Some(json!({"members": members, "more": more}));
        }
    }
    Ok(output)
}

fn track_action(env: &Env, sender: &Sender, name: &str, action: TrackAction) -> Run<Output> {
    let authority = &*env.authority;
    let name = rift_permissions::subject::normalize_track(name)?;
    let current = authority
        .tracks()
        .get(&name)
        .map(|track| track.groups.clone());
    let mut output = Output::new();
    let existing = || {
        current.clone().ok_or_else(|| {
            Failure::Perm(PermError::NotFound {
                what: "track",
                name: name.clone(),
            })
        })
    };
    let groups = match action {
        TrackAction::Info => {
            let groups = existing()?;
            output.line(Tone::Heading, format!("Track {name}"));
            output.line(Tone::Plain, track_text(&groups));
            output.data = Some(json!({"name": name, "groups": groups}));
            return Ok(output);
        }
        TrackAction::Delete => {
            existing()?;
            let applied = apply(env, sender, Change::DeleteTrack { name: name.clone() })?;
            changed(
                &mut output,
                &applied,
                format!("Deleted track {name}"),
                format!("track {name} does not exist"),
            );
            return Ok(output);
        }
        TrackAction::Create(groups) => {
            if current.is_some() {
                return Err(Failure::Message(format!("track {name} already exists")));
            }
            groups
                .iter()
                .map(|group| require_group(authority, group))
                .collect::<Run<Vec<_>>>()?
        }
        TrackAction::Append(group) => {
            let mut groups = existing()?;
            groups.push(require_group(authority, &group)?);
            groups
        }
        TrackAction::Insert(group, position) => {
            let mut groups = existing()?;
            if position > groups.len() + 1 {
                return Err(Failure::Message(format!(
                    "position {position} is past the end of {name} ({} groups)",
                    groups.len()
                )));
            }
            groups.insert(position - 1, require_group(authority, &group)?);
            groups
        }
        TrackAction::Remove(group) => {
            let mut groups = existing()?;
            let group = group_name(&group)?;
            let before = groups.len();
            groups.retain(|g| *g != group);
            if groups.len() == before {
                return Err(Failure::Message(format!("{group} is not on track {name}")));
            }
            groups
        }
        TrackAction::Clear => {
            existing()?;
            Vec::new()
        }
    };
    let text = track_text(&groups);
    let applied = apply(
        env,
        sender,
        Change::SetTrack {
            name: name.clone(),
            groups,
        },
    )?;
    changed(
        &mut output,
        &applied,
        format!("Track {name}: {text}"),
        format!("track {name} is unchanged"),
    );
    Ok(output)
}

/// Escape `text` for a SQL `LIKE` pattern.
pub fn like_literal(text: &str) -> String {
    let mut escaped = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '\\' | '%' | '_') {
            escaped.push('\\');
        }
        escaped.push(c);
    }
    escaped
}

/// `node` itself and every wildcard that would grant it: `a.b.c`,
/// `a.b.*`, `a.*`, `*`.
pub fn covering_patterns(node: &str) -> Vec<String> {
    let mut patterns = vec![node.to_owned()];
    let body = node.strip_suffix(".*").unwrap_or(node);
    let mut prefix = body;
    while let Some((head, _)) = prefix.rsplit_once('.') {
        patterns.push(format!("{head}.*"));
        prefix = head;
    }
    if node != "*" {
        patterns.push("*".into());
    }
    patterns.dedup();
    patterns
}

fn search(authority: &Authority, node: &str) -> Run<Output> {
    let raw = node.contains('%');
    let pattern = if raw {
        node.to_ascii_lowercase()
    } else {
        rift_permissions::node::normalize_node(node)?
    };
    let patterns = if raw {
        vec![pattern.clone()]
    } else {
        covering_patterns(&pattern)
    };
    let mut holders = Vec::new();
    for (index, candidate) in patterns.iter().enumerate() {
        let filter = Filter {
            kind: Some(NodeKind::Permission),
            key_like: Some(if raw {
                candidate.clone()
            } else {
                like_literal(candidate)
            }),
            ..Filter::default()
        };
        for holder in authority.search(&filter, LIST_LIMIT)? {
            holders.push((index > 0, holder));
        }
    }
    let now = unix_now();
    let mut output = Output::new();
    output.line(
        Tone::Heading,
        format!("{} nodes match {pattern}", holders.len()),
    );
    for (wildcard, holder) in holders.iter().take(LIST_LIMIT) {
        output.line(
            if *wildcard { Tone::Muted } else { Tone::Plain },
            format!(
                "  {}: {}",
                subject_label(authority, &holder.subject),
                describe_node(&holder.node, now)
            ),
        );
    }
    let listed: Vec<_> = holders.iter().map(|(_, holder)| holder).collect();
    output.data = Some(json!({ "holders": listed }));
    Ok(output)
}

fn bulk(env: &Env, sender: &Sender, mut request: BulkRequest) -> Run<Output> {
    let authority = &*env.authority;
    if let Some(user) = &request.user {
        request.filter.subject = Some(SubjectRef::User(resolve_user(authority, user)?));
    }
    let now = unix_now();
    let mut output = Output::new();
    let change = Change::Bulk {
        filter: request.filter.clone(),
        patch: request.patch.clone(),
    };
    if request.dry_run {
        let ops = authority.preview(change)?;
        output.line(
            Tone::Heading,
            format!("Dry run: {} nodes would change", ops.len()),
        );
        for op in ops.iter().take(20) {
            output.line(
                Tone::Plain,
                format!("  {}", describe_op(authority, op, now)),
            );
        }
        if ops.len() > 20 {
            output.line(Tone::Muted, format!("  … and {} more", ops.len() - 20));
        }
        output.data = Some(serde_json::to_value(Bulk::Preview(ops)).unwrap_or_default());
        return Ok(output);
    }
    let applied = apply(env, sender, change)?;
    changed(
        &mut output,
        &applied,
        format!("Bulk update changed {} nodes", applied.ops),
        "Bulk update matched nothing".into(),
    );
    Ok(output)
}

/// One recorded effect as text.
pub fn describe_op(authority: &Authority, op: &Op, now: u64) -> String {
    match op {
        Op::Node {
            subject,
            before,
            after,
        } => {
            let subject = subject_label(authority, subject);
            match (before, after) {
                (None, Some(after)) => format!("{subject}: + {}", describe_node(after, now)),
                (Some(before), None) => format!("{subject}: - {}", describe_node(before, now)),
                (Some(before), Some(after)) => format!(
                    "{subject}: {} → {}",
                    describe_node(before, now),
                    describe_node(after, now)
                ),
                (None, None) => subject,
            }
        }
        Op::Group {
            name,
            before,
            after,
        } => match (before, after) {
            (None, Some(_)) => format!("group {name} created"),
            (Some(_), None) => format!("group {name} deleted"),
            (Some(b), Some(a)) => format!(
                "group {name}: weight {} → {}, display name {:?} → {:?}",
                b.weight, a.weight, b.display_name, a.display_name
            ),
            (None, None) => format!("group {name}"),
        },
        Op::Track {
            name,
            before,
            after,
        } => match (before, after) {
            (_, Some(groups)) => format!("track {name}: {}", track_text(groups)),
            (Some(_), None) => format!("track {name} deleted"),
            (None, None) => format!("track {name}"),
        },
        Op::PrimaryGroup {
            user,
            before,
            after,
        } => format!(
            "{}: primary group {} → {}",
            user_label(authority, *user),
            before.as_deref().unwrap_or("computed"),
            after.as_deref().unwrap_or("computed")
        ),
    }
}

fn log(
    authority: &Authority,
    since: Option<u64>,
    actor: Option<String>,
    subject: Option<String>,
    limit: usize,
) -> Run<Output> {
    let actor = match actor {
        None => None,
        Some(actor) => Some(match actor.parse::<Actor>() {
            Ok(actor) => actor,
            // A player name stands for uuid:<their UUID>.
            Err(_) => Actor::Player(resolve_user(authority, &actor).map_err(|_| {
                format!(
                    "unknown actor {actor:?}: use uuid:<hex>, operator:<name>, lua:<name>, backend:<name>, cli, import, system or a player name"
                )
            })?),
        }),
    };
    let subject = match subject {
        None => None,
        Some(subject) => Some(match subject.strip_prefix("group:") {
            Some(group) => SubjectRef::Group(group_name(group)?),
            None => {
                let user = subject.strip_prefix("user:").unwrap_or(&subject);
                SubjectRef::User(resolve_user(authority, user)?)
            }
        }),
    };
    let events = authority.log(&LogQuery {
        since,
        actor,
        subject,
        limit,
    })?;
    let now = unix_now();
    let mut output = Output::new();
    output.line(Tone::Heading, format!("{} changes", events.len()));
    for event in &events {
        output.line(Tone::Plain, event_headline(event, now));
        for op in event.ops.iter().take(10) {
            output.line(
                Tone::Muted,
                format!("    {}", describe_op(authority, op, now)),
            );
        }
        if event.ops.len() > 10 || event.ops_truncated {
            output.line(Tone::Muted, "    …");
        }
    }
    output.data = Some(serde_json::to_value(&events).unwrap_or_default());
    Ok(output)
}

fn event_headline(event: &ChangeEvent, now: u64) -> String {
    let ago = duration_text(now.saturating_sub(event.at));
    let mut line = format!("#{} {ago} ago by {}", event.revision, event.actor);
    if let Some(reason) = &event.reason {
        let _ = write!(line, " ({reason})");
    }
    line
}

/// Ask every backend to resync; the revision they resync to.
fn request_resync(env: &Env) -> Run<u64> {
    let bus = env
        .bus
        .as_ref()
        .ok_or_else(|| Failure::Message("the permission bus is not running".into()))?;
    bus.resync().map_err(Failure::Message)
}

async fn sync(env: &Env) -> Output {
    let started = unix_now();
    let revision = match request_resync(env) {
        Ok(revision) => revision,
        Err(error) => return Output::failed(error.message()),
    };
    tokio::time::sleep(SYNC_WAIT).await;
    let mut output = backends_output(env);
    output.lines.insert(
        0,
        Line {
            tone: Tone::Good,
            text: format!("Asked every backend to resync to revision {revision}."),
        },
    );
    if let Some(bus) = &env.bus {
        let answered = bus
            .backends()
            .iter()
            .filter(|backend| backend.hello_at.is_some_and(|at| at >= started))
            .count();
        output.line(
            Tone::Muted,
            format!(
                "{answered} backends said hello again within {}s.",
                SYNC_WAIT.as_secs()
            ),
        );
    }
    output
}

/// The backends the bus has heard from, with their revisions.
pub fn backends_output(env: &Env) -> Output {
    let mut output = Output::new();
    let Some(bus) = &env.bus else {
        output.fail("the permission bus is not running");
        return output;
    };
    let now = unix_now();
    let current = env.authority.revision();
    let backends = bus.backends();
    output.line(
        Tone::Heading,
        format!("{} backends (proxy revision {current})", backends.len()),
    );
    for backend in &backends {
        let revision = backend
            .revision
            .map_or("no hello yet".into(), |r| format!("revision {r} at hello"));
        output.line(
            if backend.stale {
                Tone::Bad
            } else {
                Tone::Plain
            },
            format!(
                "  {} ({}): {revision}, last seen {} ago{}",
                backend.principal,
                backend.server.as_deref().unwrap_or("?"),
                duration_text(now.saturating_sub(backend.last_seen)),
                backend
                    .client_version
                    .as_deref()
                    .map_or(String::new(), |v| format!(", {v}"))
            ),
        );
    }
    output.data = Some(json!({"revision": current, "backends": backends}));
    output
}

fn doctor(env: &Env) -> Run<Output> {
    let authority = &*env.authority;
    let mut findings: Vec<String> = Vec::new();
    let groups = authority.groups();
    let tracks = authority.tracks();
    let inherits = authority.search(
        &Filter {
            kind: Some(NodeKind::Inherit),
            ..Filter::default()
        },
        super::authority::MAX_RESULTS,
    )?;
    let mut inherited: BTreeSet<String> = BTreeSet::new();
    let mut dangling: BTreeMap<String, usize> = BTreeMap::new();
    for holder in &inherits {
        if let Payload::Inherit { group, value } = &holder.node.payload {
            if !groups.contains_key(group) {
                *dangling.entry(group.clone()).or_default() += 1;
            } else if *value {
                inherited.insert(group.clone());
            }
        }
    }
    for (group, count) in &dangling {
        findings.push(format!(
            "dangling: {count} parent nodes name the missing group {group}; they grant nothing until it exists"
        ));
    }
    for track in tracks.values() {
        for group in &track.groups {
            if !groups.contains_key(group) {
                findings.push(format!(
                    "dangling: track {} lists the missing group {group}",
                    track.name
                ));
            }
            inherited.insert(group.clone());
        }
    }
    for name in groups.keys() {
        if name != DEFAULT_GROUP && !inherited.contains(name) {
            findings.push(format!(
                "unreachable: nobody inherits group {name} and no track lists it"
            ));
        }
    }
    let expired = authority.expired_unswept()?;
    if expired > 0 {
        findings.push(format!(
            "expired: {expired} nodes have expired but are not swept yet; they already grant nothing"
        ));
    }
    if let Some(bus) = &env.bus {
        let current = authority.revision();
        for backend in bus.backends() {
            match backend.revision {
                None => findings.push(format!(
                    "backend {} has made requests but never said hello",
                    backend.principal
                )),
                Some(revision) if backend.stale => findings.push(format!(
                    "stale: backend {} reported revision {revision} at hello, the proxy is at {current}; run /perms sync",
                    backend.principal
                )),
                Some(_) => {}
            }
        }
    }
    let registered: BTreeSet<String> = authority
        .registered()?
        .into_iter()
        .map(|r| r.permission.node)
        .collect();
    for (command, permission) in &env.commands {
        if registered.contains(permission) {
            continue;
        }
        let mut granted = false;
        for pattern in covering_patterns(permission) {
            let filter = Filter {
                kind: Some(NodeKind::Permission),
                key_like: Some(like_literal(&pattern)),
                value: Some("true".into()),
                ..Filter::default()
            };
            if !authority.search(&filter, 1)?.is_empty() {
                granted = true;
                break;
            }
        }
        if !granted {
            findings.push(format!(
                "command /{command} needs {permission}, which is neither registered nor granted anywhere"
            ));
        }
    }
    let mut output = Output::new();
    if findings.is_empty() {
        output.line(Tone::Good, "No problems found.");
    } else {
        output.line(Tone::Heading, format!("{} findings", findings.len()));
        for finding in &findings {
            output.line(Tone::Bad, format!("  {finding}"));
        }
    }
    output.data = Some(json!({ "findings": findings }));
    Ok(output)
}

fn outcome_text(outcome: Outcome) -> &'static str {
    match outcome {
        Outcome::Won => "won",
        Outcome::Expired => "expired",
        Outcome::ContextMismatch => "its contexts are not active",
        Outcome::LessSpecific => "a more specific pattern won",
        Outcome::FewerContexts => "an entry with more contexts won",
        Outcome::HigherRank => "a closer subject won",
        Outcome::DenyWins => "false beats true at equal precedence",
        Outcome::Duplicate => "same as the winner, which came first",
    }
}

fn candidate_text(
    compiled: &rift_permissions::CompiledSubject,
    candidate: &Candidate,
    now: u64,
) -> String {
    let entry = &candidate.entry;
    let from = match compiled.rank_subject(entry.rank) {
        Some(group) => format!("group {group}"),
        None => "the player".into(),
    };
    let what = match entry.kind {
        EntryKind::Permission => format!("{} = {}", entry.key, entry.value),
        EntryKind::Group => format!("membership of {}", entry.key),
    };
    let specificity = match candidate.specificity {
        Specificity::Exact => "exact".to_owned(),
        Specificity::Prefix(length) => format!("wildcard, {length} bytes"),
        Specificity::Any => "*".to_owned(),
    };
    let mut text = format!("{what} from {from} (rank {}, {specificity}", entry.rank);
    if !entry.contexts.is_empty() {
        let _ = write!(text, ", {}", entry.contexts);
    }
    text.push(')');
    text.push_str(&expiry_text(entry.expires_at, now));
    text
}

/// `/perms trace` as lines: the answer, the winning entry and every loser
/// with the reason it lost.
pub fn trace_lines(
    authority: &Authority,
    uuid: Uuid,
    active: &ContextSet,
    trace: &Trace,
) -> Vec<Line> {
    let now = unix_now();
    let compiled = authority.compiled(uuid);
    let mut lines = Vec::new();
    let (tone, word) = match trace.result {
        Tristate::True => (Tone::Good, "true"),
        Tristate::False => (Tone::Bad, "false"),
        Tristate::Undefined => (Tone::Muted, "undefined"),
    };
    lines.push(Line {
        tone,
        text: format!(
            "{} has {} = {word} in {}",
            user_label(authority, uuid),
            trace.node,
            contexts_text(active)
        ),
    });
    match &trace.winner {
        Some(winner) => lines.push(Line {
            tone: Tone::Plain,
            text: format!("Winner: {}", candidate_text(&compiled, winner, now)),
        }),
        None => lines.push(Line {
            tone: Tone::Muted,
            text: "No entry matches; the caller's default applies.".into(),
        }),
    }
    for loser in &trace.losers {
        lines.push(Line {
            tone: Tone::Muted,
            text: format!(
                "Lost: {}: {}",
                candidate_text(&compiled, loser, now),
                outcome_text(loser.outcome)
            ),
        });
    }
    lines
}

#[cfg(test)]
#[path = "command_tests.rs"]
mod tests;
