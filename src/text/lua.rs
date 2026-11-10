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
//! ```
use super::{Arg, Args, Profile, Text, catalog, legacy, markup};
use mlua::{Lua, Table, Value};
use serde_json::Value as Json;

const MAX_DEPTH: usize = 64;

/// Add `text` to the `rift` table `api`.
pub(crate) fn install(lua: &Lua, api: &Table) -> mlua::Result<()> {
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
        "to_legacy",
        lua.create_function(|_, value: Value| {
            let text = Text::lua(value, "rift.text.to_legacy").map_err(mlua::Error::runtime)?;
            Ok(text.to_legacy(Profile::UNKNOWN))
        })?,
    )?;
    text.raw_set(
        "plain",
        lua.create_function(|_, value: Value| {
            let text = Text::lua(value, "rift.text.plain").map_err(mlua::Error::runtime)?;
            Ok(text.to_string())
        })?,
    )?;
    text.raw_set(
        "to_mini",
        lua.create_function(|_, value: Value| {
            let text = Text::lua(value, "rift.text.to_mini").map_err(mlua::Error::runtime)?;
            Ok(match text {
                Text::Plain(plain) => markup::escape(&plain),
                Text::Component(component) => markup::serialize(&component),
            })
        })?,
    )?;
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

/// Markup arguments from a Lua table: strings and numbers are shown as
/// written, tables are chat components.
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
            value @ Value::Table(_) => match Text::lua(value, &label)? {
                Text::Plain(text) => Arg::Text(text),
                Text::Component(component) => Arg::Component(component),
            },
            value => return Err(format!("{label}: unsupported {}", value.type_name())),
        };
        parsed.insert(&name, arg);
    }
    parsed.validate("rift.text.mini")?;
    Ok(parsed)
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
        install(&lua, &api).unwrap();
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
        install(&lua, &api).unwrap();
        lua.globals().set("rift", api).unwrap();
        let error = lua
            .load(r#"return rift.text.mini("<bolt>")"#)
            .eval::<Value>()
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown tag"), "{error}");
    }
}
