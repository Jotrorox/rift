//! `/perms` forwarding helpers: splitting the typed arguments and turning
//! the proxy's reply into chat text.

use rift_permissions_client::Error;
use serde_json::Value;

/// Split a `/perms` argument string the way LuckPerms does: on whitespace,
/// with double quotes grouping words (`"&6[VIP] "`) and `\"`, `\\` escaping
/// inside quotes. An unterminated quote runs to the end.
pub fn split_args(input: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut in_arg = false;
    let mut quoted = false;
    let mut chars = input.chars();
    while let Some(c) = chars.next() {
        match c {
            '"' => {
                quoted = !quoted;
                in_arg = true;
            }
            '\\' if quoted => match chars.next() {
                Some(next @ ('"' | '\\')) => current.push(next),
                Some(next) => {
                    current.push('\\');
                    current.push(next);
                }
                None => current.push('\\'),
            },
            c if c.is_whitespace() && !quoted => {
                if in_arg {
                    args.push(std::mem::take(&mut current));
                    in_arg = false;
                }
            }
            c => {
                current.push(c);
                in_arg = true;
            }
        }
    }
    if in_arg {
        args.push(current);
    }
    args
}

/// The plain text of an Adventure component, for when it cannot be shown as
/// a component: `text` and `extra` concatenated in order.
pub fn plain_text(component: &Value) -> String {
    fn walk(component: &Value, out: &mut String) {
        match component {
            Value::String(text) => out.push_str(text),
            Value::Array(parts) => parts.iter().for_each(|part| walk(part, out)),
            Value::Object(map) => {
                if let Some(Value::String(text)) = map.get("text") {
                    out.push_str(text);
                } else if let Some(Value::String(key)) = map.get("translate") {
                    out.push_str(key);
                }
                if let Some(extra) = map.get("extra") {
                    walk(extra, out);
                }
            }
            _ => {}
        }
    }
    let mut out = String::new();
    walk(component, &mut out);
    out
}

/// What the sender sees when `/perms` could not be forwarded.
pub fn error_message(error: &Error) -> String {
    match error {
        Error::Unavailable => {
            "Permissions are unavailable: the proxy is not reachable. Try again later.".to_owned()
        }
        Error::Timeout => {
            "The proxy did not answer in time; the command may still have been applied.".to_owned()
        }
        Error::Rejected(reply) => format!("The proxy refused the command: {}", reply.error),
        other => format!("The command could not be forwarded: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rift_permissions::wire::{ErrorCode, ErrorReply};
    use serde_json::json;

    #[test]
    fn splits_like_luckperms() {
        assert_eq!(
            split_args("user Alice permission set fly true"),
            ["user", "Alice", "permission", "set", "fly", "true"]
        );
        assert_eq!(
            split_args(r#"  group vip meta setprefix 100 "&6[VIP] "  "#),
            ["group", "vip", "meta", "setprefix", "100", "&6[VIP] "]
        );
        assert_eq!(split_args(r#"a "" b"#), ["a", "", "b"]);
        assert_eq!(
            split_args(r#"say "a \"b\" \\ \n""#),
            ["say", r#"a "b" \ \n"#]
        );
        assert_eq!(split_args(r#"x"y z"w"#), ["xy zw"]);
        assert_eq!(split_args(r#""open quote"#), ["open quote"]);
        assert!(split_args("   ").is_empty());
    }

    #[test]
    fn component_text_fallback() {
        let component = json!({"text": "Set fly ", "color": "green",
                               "extra": [{"text": "true"}, " for ", {"translate": "x.y"}]});
        assert_eq!(plain_text(&component), "Set fly true for x.y");
        assert_eq!(plain_text(&json!("plain")), "plain");
        assert_eq!(plain_text(&json!(5)), "");
    }

    #[test]
    fn errors_explain_themselves() {
        assert!(error_message(&Error::Unavailable).contains("unavailable"));
        assert!(error_message(&Error::Timeout).contains("may still"));
        let refused = Error::Rejected(ErrorReply {
            code: ErrorCode::Forbidden,
            error: "missing rift.perms.user".into(),
        });
        assert_eq!(
            error_message(&refused),
            "The proxy refused the command: missing rift.perms.user"
        );
        assert!(error_message(&Error::Config("x".into())).contains("could not be forwarded"));
    }
}
