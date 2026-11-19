//! `rift.text` for every Lua environment: the configuration, extensions,
//! `on_status` and Lua plugins. A thin front end over the host engine, so
//! Lua cannot disagree with WebAssembly or the bus.
//!
//! ```lua
//! local hi = rift.text.mini("<gold>Hi <player>!", { player = ctx.name })
//! rift.players.message(ctx.uuid, hi)          -- a component table
//! rift.players.message(ctx.uuid, { mini = "<red>also works" })
//! rift.text.legacy("&cRed &lbold")            -- a component table
//! rift.text.to_legacy(hi)                     -- "§6Hi Steve!"
//! rift.text.plain(hi)                         -- "Hi Steve!"
//! rift.text.escape(untrusted)                 -- safe to splice into markup
//! local m = rift.text.message("greet", { name = ctx.name })  -- as written
//! rift.text.plain(m, "de")                    -- "Hallo Steve." (a plugin's own)
//! rift.text.render(m)                         -- a component table, default language
//! ```
use super::{Arg, Args, Form, Profile, Text, catalog, legacy, markup};
use mlua::{Lua, Table, Value};
use serde_json::Value as Json;

const MAX_DEPTH: usize = 64;

/// Add `text` to the `rift` table `api`, for a caller in `namespace`: a
/// persistent plugin's name, whose unqualified message keys are its own, or
/// `None` for `rift.lua` and script-mode plugins.
pub(crate) fn install(lua: &Lua, api: &Table, namespace: Option<&str>) -> mlua::Result<()> {
    let text = lua.create_table()?;
    text.raw_set(
        "mini",
        lua.create_function(|lua, (markup, args): (String, Option<Table>)| {
            let args = arguments(args).map_err(mlua::Error::runtime)?;
            let text = Text::mini(&markup, &args, &catalog::theme(), "rift.text.mini")
                .map_err(mlua::Error::runtime)?;
            to_lua(lua, &text.for_client(Profile::UNKNOWN))
        })?,
    )?;
    text.raw_set(
        "legacy",
        lua.create_function(|lua, (text, marker): (String, Option<String>)| {
            let marker = match marker.as_deref() {
                None => '&',
                Some(marker) => {
                    let mut chars = marker.chars();
                    match (chars.next(), chars.next()) {
                        (Some(c), None) => c,
                        _ => {
                            return Err(mlua::Error::runtime(
                                "rift.text.legacy: the marker must be one character",
                            ));
                        }
                    }
                }
            };
            let text =
                Text::legacy(&text, marker, "rift.text.legacy").map_err(mlua::Error::runtime)?;
            to_lua(lua, &text.for_client(Profile::UNKNOWN))
        })?,
    )?;
    text.raw_set(
        "message",
        lua.create_function(|lua, (key, args): (Value, Option<Table>)| {
            message(lua, key, args).map_err(mlua::Error::runtime)
        })?,
    )?;
    let own = namespace.map(str::to_owned);
    text.raw_set(
        "render",
        lua.create_function(move |lua, (value, language): (Value, Option<String>)| {
            let label = "rift.text.render";
            let text = Text::lua_as_written(value, label).map_err(mlua::Error::runtime)?;
            let text = super::resolve(&text, own.as_deref(), language.as_deref(), label)
                .map_err(mlua::Error::runtime)?;
            to_lua(lua, &text.for_client(Profile::UNKNOWN))
        })?,
    )?;
    for (name, form) in [
        ("to_legacy", Form::Legacy),
        ("plain", Form::Plain),
        ("to_mini", Form::Mini),
    ] {
        let own = namespace.map(str::to_owned);
        let label = format!("rift.text.{name}");
        text.raw_set(
            name,
            lua.create_function(move |_, (value, language): (Value, Option<String>)| {
                let text = Text::lua_as_written(value, &label).map_err(mlua::Error::runtime)?;
                super::convert(&text, own.as_deref(), language.as_deref(), form, &label)
                    .map_err(mlua::Error::runtime)
            })?,
        )?;
    }
    text.raw_set(
        "escape",
        lua.create_function(|_, text: String| Ok(markup::escape(&text)))?,
    )?;
    // Kept for completeness with `legacy`: § codes from a plain string.
    text.raw_set(
        "from_section",
        lua.create_function(|lua, text: String| to_lua(lua, &legacy::parse(&text, '§')))?,
    )?;
    api.raw_set("text", text)
}

/// `rift.text.message(key, args)`: the `message` object as a table, left
/// as written. It resolves when it is handed to a call, with the caller's
/// namespace and the recipient's language, so only its shape is checked
/// here.
fn message(lua: &Lua, key: Value, args: Option<Table>) -> Result<Table, String> {
    const LABEL: &str = "rift.text.message";
    let key = match key {
        Value::String(key) => key,
        other => {
            return Err(format!(
                "{LABEL}: the key must be a string, got {}",
                other.type_name()
            ));
        }
    };
    if key.as_bytes().is_empty() {
        return Err(format!("{LABEL}: the key must not be empty"));
    }
    let object = lua.create_table().map_err(|error| error.to_string())?;
    object
        .raw_set("message", key)
        .map_err(|error| error.to_string())?;
    if let Some(args) = args {
        for pair in args.pairs::<Value, Value>() {
            let (name, value) = pair.map_err(|error| error.to_string())?;
            let Value::String(name) = name else {
                return Err(format!("{LABEL}: argument names must be strings"));
            };
            match value {
                Value::String(_)
                | Value::Integer(_)
                | Value::Number(_)
                | Value::Boolean(_)
                | Value::Table(_) => {}
                value => {
                    return Err(format!(
                        "{LABEL} argument {:?}: unsupported {}",
                        name.to_string_lossy(),
                        value.type_name()
                    ));
                }
            }
        }
        object
            .raw_set("args", args)
            .map_err(|error| error.to_string())?;
    }
    Ok(object)
}

/// Markup arguments from a Lua table: strings and numbers are shown as
/// written, `{ style = "<red>" }` is a style, other tables are chat
/// components.
fn arguments(args: Option<Table>) -> Result<Args, String> {
    let mut parsed = Args::new();
    let Some(args) = args else {
        return Ok(parsed);
    };
    for pair in args.pairs::<Value, Value>() {
        let (name, value) = pair.map_err(|error| error.to_string())?;
        let Value::String(name) = name else {
            return Err("rift.text.mini: argument names must be strings".into());
        };
        let name = name
            .to_str()
            .map_err(|_| "rift.text.mini: argument names must be UTF-8".to_owned())?
            .to_owned();
        let label = format!("rift.text.mini argument {name:?}");
        let arg = match value {
            Value::String(text) => Arg::Text(
                text.to_str()
                    .map_err(|_| format!("{label} must be UTF-8"))?
                    .to_owned(),
            ),
            Value::Integer(number) => Arg::Text(number.to_string()),
            Value::Number(number) => Arg::Text(number.to_string()),
            Value::Boolean(value) => Arg::Text(value.to_string()),
            Value::Table(table) => {
                let style = table
                    .raw_get::<Value>("style")
                    .map_err(|error| format!("{label}: {error}"))?;
                if style.is_nil() {
                    match Text::lua(Value::Table(table), &label)? {
                        Text::Plain(text) => Arg::Text(text),
                        Text::Component(component) => Arg::Component(component),
                    }
                } else {
                    Arg::Style(style_argument(&table, style, &label)?)
                }
            }
            value => return Err(format!("{label}: unsupported {}", value.type_name())),
        };
        parsed.insert(&name, arg);
    }
    parsed.validate("rift.text.mini")?;
    Ok(parsed)
}

/// A style argument: a table with only `style`, markup made only of
/// formatting tags.
fn style_argument(table: &Table, style: Value, label: &str) -> Result<markup::Style, String> {
    if table.pairs::<Value, Value>().count() != 1 {
        return Err(format!("{label}: a style argument takes only style"));
    }
    let Value::String(source) = style else {
        return Err(format!("{label}: style must be a string of markup"));
    };
    let source = source
        .to_str()
        .map_err(|_| format!("{label}: style must be UTF-8"))?;
    markup::style_arg(&source).map_err(|error| format!("{label}: {error}"))
}

/// A JSON value as Lua: arrays are sequences, objects tables.
fn to_lua(lua: &Lua, value: &Json) -> mlua::Result<Value> {
    fn convert(lua: &Lua, value: &Json, depth: usize) -> mlua::Result<Value> {
        if depth > MAX_DEPTH {
            return Err(mlua::Error::runtime("text nests too deeply"));
        }
        Ok(match value {
            Json::Null => Value::Nil,
            Json::Bool(value) => Value::Boolean(*value),
            Json::Number(number) => match number.as_i64() {
                Some(value) => Value::Integer(value),
                None => Value::Number(number.as_f64().unwrap_or(f64::NAN)),
            },
            Json::String(text) => Value::String(lua.create_string(text)?),
            Json::Array(values) => {
                let table = lua.create_table_with_capacity(values.len(), 0)?;
                for (index, value) in values.iter().enumerate() {
                    table.raw_set(index + 1, convert(lua, value, depth + 1)?)?;
                }
                Value::Table(table)
            }
            Json::Object(values) => {
                let table = lua.create_table_with_capacity(0, values.len())?;
                for (key, value) in values {
                    table.raw_set(key.as_str(), convert(lua, value, depth + 1)?)?;
                }
                Value::Table(table)
            }
        })
    }
    convert(lua, value, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eval(code: &str) -> String {
        let lua = Lua::new();
        let api = lua.create_table().unwrap();
        install(&lua, &api, None).unwrap();
        lua.globals().set("rift", api).unwrap();
        lua.load(code).eval::<String>().unwrap()
    }

    #[test]
    fn lua_builds_components_from_markup_and_legacy_codes() {
        assert_eq!(
            eval(
                r#"local t = rift.text.mini("<red>Hi <p>", { p = "<b>x" }) return t.color .. "|" .. t.text"#
            ),
            "red|Hi <b>x"
        );
        assert_eq!(
            eval(r#"return rift.text.to_legacy(rift.text.legacy("&cRed &lbold"))"#),
            "§cRed §lbold"
        );
        assert_eq!(
            eval(r#"return rift.text.plain(rift.text.mini("<gold>a<br>b", { }))"#),
            "a\nb"
        );
        assert_eq!(eval(r#"return rift.text.escape("<red>\\")"#), r"\<red>\\");
        assert_eq!(
            eval(r#"return rift.text.to_mini({ text = "a", color = "red" })"#),
            "<red>a</red>"
        );
        assert_eq!(
            eval(
                r#"local t = rift.text.mini("[<r>]", { r = { mini = "<gold>Admin" } })
                   return t.extra[2].color"#
            ),
            "gold"
        );
        let lua = Lua::new();
        let api = lua.create_table().unwrap();
        install(&lua, &api, None).unwrap();
        lua.globals().set("rift", api).unwrap();
        let error = lua
            .load(r#"return rift.text.mini("<bolt>")"#)
            .eval::<Value>()
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown tag"), "{error}");
    }

    #[test]
    fn lua_style_arguments_wrap_template_text() {
        assert_eq!(
            eval(
                r#"local t = rift.text.mini("<team>[<tag>]</team>",
                       { team = { style = "<#55ffff><bold>" }, tag = "x" })
                   return t.color .. "|" .. tostring(t.bold) .. "|" .. t.text"#
            ),
            "#55ffff|true|[x]"
        );
        assert_eq!(
            eval(r#"return rift.text.mini("<s>a</s>", { s = { style = "" } }).text"#),
            "a"
        );
        let lua = Lua::new();
        let api = lua.create_table().unwrap();
        install(&lua, &api, None).unwrap();
        lua.globals().set("rift", api).unwrap();
        let error = |code: &str| lua.load(code).eval::<Value>().unwrap_err().to_string();
        let extra = error(r#"return rift.text.mini("<s>a", { s = { style = "<red>", x = 1 } })"#);
        assert!(
            extra.contains("a style argument takes only style"),
            "{extra}"
        );
        let text = error(r#"return rift.text.mini("<s>a", { s = { style = "<red>words" } })"#);
        assert!(text.contains("only formatting tags are allowed"), "{text}");
        let number = error(r#"return rift.text.mini("<s>a", { s = { style = 1 } })"#);
        assert!(
            number.contains("style must be a string of markup"),
            "{number}"
        );
    }

    /// `rift.text` for a caller in `namespace`, with the test catalogue.
    fn messages_lua(namespace: Option<&str>) -> Lua {
        let lua = Lua::new();
        let api = lua.create_table().unwrap();
        install(&lua, &api, namespace).unwrap();
        lua.globals().set("rift", api).unwrap();
        lua
    }

    #[test]
    fn lua_messages_stay_as_written_until_a_call_renders_them() {
        let _catalog = catalog::loading(std::sync::Arc::new(catalog::test_catalog()));
        let lua = messages_lua(Some("party"));
        let eval = |code: &str| lua.load(code).eval::<String>().unwrap();
        let error = |code: &str| lua.load(code).eval::<Value>().unwrap_err().to_string();
        // The object as a table, not resolved: an unknown key is fine here.
        assert_eq!(
            eval(
                r#"local m = rift.text.message("greet", { name = "Alex" })
                   return m.message .. "|" .. m.args.name .. "|" .. tostring(next(m, nil) ~= nil)"#
            ),
            "greet|Alex|true"
        );
        assert_eq!(
            eval(r#"local m = rift.text.message("nope") return m.message .. tostring(m.args)"#),
            "nopenil"
        );
        assert!(error(r#"return rift.text.message("")"#).contains("must not be empty"));
        assert!(error(r#"return rift.text.message(1)"#).contains("must be a string"));
        assert!(
            error(r#"return rift.text.message("greet", { name = function() end })"#)
                .contains("unsupported function")
        );
        // Unqualified keys are the caller's own, in the language asked for.
        assert_eq!(
            eval(r#"return rift.text.plain(rift.text.message("greet", { name = "Alex" }), "de")"#),
            "Hallo Alex."
        );
        assert_eq!(
            eval(r#"return rift.text.plain(rift.text.message("greet", { name = "Alex" }))"#),
            "Hello Alex."
        );
        assert_eq!(
            eval(r#"return rift.text.to_legacy({ message = "created" }, "de")"#),
            "Erstellt."
        );
        assert_eq!(
            eval(
                r#"return rift.text.plain(rift.text.render(rift.text.message("created"), "de_DE"))"#
            ),
            "Erstellt."
        );
        // `render` resolves markup and legacy codes as well; plain text
        // stays a string.
        assert_eq!(
            eval(
                r#"local t = rift.text.render({ mini = "<red>Hi <p>", args = { p = "x" } })
                   return t.color .. "|" .. t.text"#
            ),
            "red|Hi x"
        );
        assert_eq!(
            eval(r#"return rift.text.render("<red>as written")"#),
            "<red>as written"
        );
        assert!(
            error(r#"return rift.text.render(rift.text.message("created"), "d")"#)
                .contains("rift.text.render: a language is")
        );
        assert!(
            error(r#"return rift.text.plain(rift.text.message("nope"), "de")"#)
                .contains("rift.text.plain")
        );
        // Without a namespace (rift.lua) a key needs its plugin's name.
        let lua = messages_lua(None);
        let plain = |code: &str| lua.load(code).eval::<String>();
        assert_eq!(
            plain(r#"return rift.text.plain({ message = "party:created" }, "de")"#).unwrap(),
            "Erstellt."
        );
        assert!(plain(r#"return rift.text.plain({ message = "created" })"#).is_err());
    }
}
