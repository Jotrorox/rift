//! Objectives, scores and teams. Before 1.13 their names, prefixes and
//! suffixes are legacy text with length limits; 1.20.3 added number formats
//! and split score removal into its own packet; 1.21.5 sent team options as
//! IDs; 26.2 reordered the team packet and made its colour optional.
use super::{Cv, crosses, text_changes};
use crate::{
    codec::text::{
        Component,
        component::{Color, Style},
        legacy, style_from_nbt, style_to_nbt, version as v,
    },
    nbt::Nbt,
    wire::{Reader, Writer, invalid},
};
use std::io;

/// 1.20.2 sends the display slot as a VarInt.
const DISPLAY_VARINT: i32 = 764;
/// 1.20.3: number formats, score display names, `reset_score`.
const NUMBER_FORMATS: i32 = 765;
/// 1.21.5: team name tag visibility and collision rule as IDs.
const TEAM_OPTION_IDS: i32 = 770;
/// 26.2: the team packet's new field order and optional colour.
const TEAM_REORDERED: i32 = 776;

pub fn objective_needed(from: i32, to: i32) -> bool {
    text_changes(from, to) || crosses(from, to, v::FLATTENING) || crosses(from, to, NUMBER_FORMATS)
}

pub fn display_needed(from: i32, to: i32) -> bool {
    crosses(from, to, DISPLAY_VARINT)
}

pub fn score_needed(from: i32, to: i32) -> bool {
    text_changes(from, to) || crosses(from, to, NUMBER_FORMATS)
}

pub fn team_needed(from: i32, to: i32) -> bool {
    text_changes(from, to)
        || [v::FLATTENING, TEAM_OPTION_IDS, TEAM_REORDERED]
            .iter()
            .any(|boundary| crosses(from, to, *boundary))
}

/// A score number format (1.20.3+): blank, styled, or fixed text.
enum NumberFormat {
    Blank,
    Styled(Style),
    Fixed(Component),
}

fn read_number_format(cv: &mut Cv<'_>, r: &mut Reader<'_>) -> io::Result<Option<NumberFormat>> {
    if cv.from < NUMBER_FORMATS || !r.bool()? {
        return Ok(None);
    }
    Ok(Some(match r.varint()? {
        0 => NumberFormat::Blank,
        1 => {
            let tag = Nbt::read(r, false)?.ok_or_else(|| invalid("missing number format style"))?;
            NumberFormat::Styled(style_from_nbt(cv.from, &tag)?)
        }
        2 => NumberFormat::Fixed(cv.read_component(r)?),
        _ => return Err(invalid("unknown number format")),
    }))
}

fn write_number_format(cv: &Cv<'_>, w: &mut Writer, format: Option<&NumberFormat>) {
    if cv.to < NUMBER_FORMATS {
        return;
    }
    w.bool(format.is_some());
    match format {
        None => {}
        Some(NumberFormat::Blank) => drop(w.varint(0)),
        Some(NumberFormat::Styled(style)) => {
            w.varint(1);
            style_to_nbt(cv.to, style).write(w, false);
        }
        Some(NumberFormat::Fixed(component)) => {
            w.varint(2);
            cv.write_component(w, component);
        }
    }
}

/// Legacy text for a 1.12 client, cut to its limit.
fn legacy_text(component: &Component, max: usize) -> String {
    legacy::truncate(&legacy::to_legacy(component), max)
}

pub fn set_objective(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut r = Reader::new(data);
    let mut w = Writer::new();
    w.string(r.str()?);
    let mode = r.i8()?;
    w.i8(mode);
    if mode == 0 || mode == 2 {
        let display = if cv.from < v::FLATTENING {
            legacy::from_legacy(r.string(32)?)
        } else {
            cv.read_component(&mut r)?
        };
        let hearts = if cv.from < v::FLATTENING {
            r.string(16)? == "hearts"
        } else {
            r.varint()? == 1
        };
        let format = read_number_format(cv, &mut r)?;
        if cv.to < v::FLATTENING {
            w.string(&legacy_text(&display, 32));
            w.string(if hearts { "hearts" } else { "integer" });
        } else {
            cv.write_component(&mut w, &display);
            w.varint(i32::from(hearts));
        }
        write_number_format(cv, &mut w, format.as_ref());
    }
    r.finish()?;
    cv.emit("set_objective", w.into_inner());
    Ok(())
}

pub fn set_display_objective(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut r = Reader::new(data);
    let slot = if cv.from < DISPLAY_VARINT {
        i32::from(r.i8()?)
    } else {
        r.varint()?
    };
    let mut w = Writer::new();
    if cv.to < DISPLAY_VARINT {
        w.i8(i8::try_from(slot).map_err(|_| invalid("display slot out of range"))?);
    } else {
        w.varint(slot);
    }
    w.bytes(r.rest());
    cv.emit("set_display_objective", w.into_inner());
    Ok(())
}

pub fn set_score(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut r = Reader::new(data);
    let owner = r.str()?;
    if cv.from < NUMBER_FORMATS {
        let action = r.varint()?;
        let objective = r.str()?;
        let value = if action == 0 { Some(r.varint()?) } else { None };
        r.finish()?;
        let mut w = Writer::new();
        match value {
            Some(value) if cv.to >= NUMBER_FORMATS => {
                w.string(owner)
                    .string(objective)
                    .varint(value)
                    .bool(false)
                    .bool(false);
                cv.emit("set_score", w.into_inner());
            }
            Some(value) => {
                w.string(owner).varint(0).string(objective).varint(value);
                cv.emit("set_score", w.into_inner());
            }
            None => removal(cv, owner, (!objective.is_empty()).then_some(objective)),
        }
        return Ok(());
    }
    let objective = r.str()?;
    let value = r.varint()?;
    let display = cv.read_optional_component(&mut r)?;
    let format = read_number_format(cv, &mut r)?;
    r.finish()?;
    let mut w = Writer::new();
    if cv.to >= NUMBER_FORMATS {
        w.string(owner).string(objective).varint(value);
        cv.write_optional_component(&mut w, display.as_ref());
        write_number_format(cv, &mut w, format.as_ref());
    } else {
        w.string(owner).varint(0).string(objective).varint(value);
    }
    cv.emit("set_score", w.into_inner());
    Ok(())
}

pub fn reset_score(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut r = Reader::new(data);
    let owner = r.str()?;
    let objective = if r.bool()? { Some(r.str()?) } else { None };
    r.finish()?;
    removal(cv, owner, objective);
    Ok(())
}

/// A score removal: `reset_score` from 1.20.3, before that `set_score`
/// with the remove action (an empty objective means every objective).
fn removal(cv: &mut Cv<'_>, owner: &str, objective: Option<&str>) {
    let mut w = Writer::new();
    if cv.to >= NUMBER_FORMATS {
        w.string(owner).bool(objective.is_some());
        if let Some(objective) = objective {
            w.string(objective);
        }
        cv.emit("reset_score", w.into_inner());
    } else {
        w.string(owner).varint(1).string(objective.unwrap_or(""));
        cv.emit("set_score", w.into_inner());
    }
}

const VISIBILITY: [&str; 4] = ["always", "never", "hideForOtherTeams", "hideForOwnTeam"];
const COLLISION: [&str; 4] = ["always", "never", "pushOtherTeams", "pushOwnTeam"];
/// `ChatFormatting.RESET`, the old "no colour" value.
const RESET: i32 = 21;

/// A team's parameters in any protocol's terms.
struct Team {
    display: Component,
    prefix: Component,
    suffix: Component,
    options: i8,
    visibility: usize,
    collision: usize,
    /// A named colour, or none.
    color: Option<u8>,
}

fn read_option(cv: &Cv<'_>, r: &mut Reader<'_>, names: &[&str; 4]) -> io::Result<usize> {
    if cv.from >= TEAM_OPTION_IDS {
        let id = r.varint()?;
        usize::try_from(id)
            .ok()
            .filter(|id| *id < 4)
            .ok_or_else(|| invalid("unknown team option"))
    } else {
        // Vanilla falls back to the default for unknown names.
        let name = r.string(40)?;
        Ok(names.iter().position(|known| *known == name).unwrap_or(0))
    }
}

fn write_option(cv: &Cv<'_>, w: &mut Writer, names: &[&str; 4], value: usize) {
    if cv.to >= TEAM_OPTION_IDS {
        w.varint(value as i32);
    } else {
        w.string(names[value]);
    }
}

fn read_team(cv: &mut Cv<'_>, r: &mut Reader<'_>) -> io::Result<Team> {
    if cv.from < v::FLATTENING {
        let display = legacy::from_legacy(r.string(32)?);
        let prefix_text = r.string(16)?;
        let prefix = legacy::from_legacy(prefix_text);
        let suffix = legacy::from_legacy(r.string(16)?);
        let options = r.i8()?;
        let visibility = read_option(cv, r, &VISIBILITY)?;
        let collision = read_option(cv, r, &COLLISION)?;
        let color = u8::try_from(r.i8()?).ok().filter(|color| *color < 16);
        // A 1.12 prefix colours the name after it; 1.13 uses the team colour.
        let color = color.or_else(|| legacy::trailing_color(prefix_text));
        return Ok(Team {
            display,
            prefix,
            suffix,
            options,
            visibility,
            collision,
            color,
        });
    }
    if cv.from >= TEAM_REORDERED {
        let display = cv.read_component(r)?;
        let prefix = cv.read_component(r)?;
        let suffix = cv.read_component(r)?;
        let visibility = read_option(cv, r, &VISIBILITY)?;
        let collision = read_option(cv, r, &COLLISION)?;
        let color = if r.bool()? {
            Some(
                u8::try_from(r.varint()?)
                    .ok()
                    .filter(|color| *color < 16)
                    .ok_or_else(|| invalid("unknown team colour"))?,
            )
        } else {
            None
        };
        let options = r.i8()?;
        return Ok(Team {
            display,
            prefix,
            suffix,
            options,
            visibility,
            collision,
            color,
        });
    }
    let display = cv.read_component(r)?;
    let options = r.i8()?;
    let visibility = read_option(cv, r, &VISIBILITY)?;
    let collision = read_option(cv, r, &COLLISION)?;
    let color = u8::try_from(r.varint()?).ok().filter(|color| *color < 16);
    let prefix = cv.read_component(r)?;
    let suffix = cv.read_component(r)?;
    Ok(Team {
        display,
        prefix,
        suffix,
        options,
        visibility,
        collision,
        color,
    })
}

fn write_team(cv: &Cv<'_>, w: &mut Writer, team: &Team) {
    if cv.to < v::FLATTENING {
        w.string(&legacy_text(&team.display, 32));
        // The team colour has to travel in the prefix for 1.12 clients.
        let mut prefix = legacy_text(&team.prefix, if team.color.is_some() { 14 } else { 16 });
        if let Some(color) = team.color {
            prefix.push('§');
            prefix.push(legacy::code_of_color(color));
        }
        w.string(&prefix);
        w.string(&legacy_text(&team.suffix, 16));
        w.i8(team.options);
        write_option(cv, w, &VISIBILITY, team.visibility);
        write_option(cv, w, &COLLISION, team.collision);
        w.i8(team.color.map_or(-1, |color| color as i8));
        return;
    }
    if cv.to >= TEAM_REORDERED {
        cv.write_component(w, &team.display);
        cv.write_component(w, &team.prefix);
        cv.write_component(w, &team.suffix);
        write_option(cv, w, &VISIBILITY, team.visibility);
        write_option(cv, w, &COLLISION, team.collision);
        w.bool(team.color.is_some());
        if let Some(color) = team.color {
            w.varint(i32::from(color));
        }
        w.i8(team.options);
        return;
    }
    cv.write_component(w, &team.display);
    w.i8(team.options);
    write_option(cv, w, &VISIBILITY, team.visibility);
    write_option(cv, w, &COLLISION, team.collision);
    w.varint(team.color.map_or(RESET, i32::from));
    cv.write_component(w, &team.prefix);
    cv.write_component(w, &team.suffix);
}

pub fn set_player_team(cv: &mut Cv<'_>, data: &[u8]) -> io::Result<()> {
    let mut r = Reader::new(data);
    let mut w = Writer::new();
    w.string(r.str()?);
    let mode = r.i8()?;
    w.i8(mode);
    if mode == 0 || mode == 2 {
        let team = read_team(cv, &mut r)?;
        write_team(cv, &mut w, &team);
    }
    if matches!(mode, 0 | 3 | 4) {
        let count = r.count()?;
        w.varint(count as i32);
        for _ in 0..count {
            w.string(r.str()?);
        }
    }
    r.finish()?;
    cv.emit("set_player_team", w.into_inner());
    Ok(())
}

/// A colour as the team's named colour, for other codecs.
#[allow(dead_code)]
fn team_color(color: Color) -> Option<u8> {
    match color.nearest_named() {
        Color::Named(index) => Some(index),
        _ => None,
    }
}
