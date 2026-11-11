//! `update_advancements` in every protocol's layout.
use super::{Stack, Text, stack::read_template, stack::write_template, window};
use crate::wire::{Reader, Writer, invalid};
use std::io;

#[derive(Clone, Debug, PartialEq)]
pub struct Display {
    pub title: Text,
    pub description: Text,
    pub icon: Stack,
    pub frame: i32,
    /// 1: has a background, 2: shows a toast, 4: hidden.
    pub flags: i32,
    pub background: Option<String>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Advancement {
    pub id: String,
    pub parent: Option<String>,
    pub display: Option<Display>,
    /// The position in the tab: inside the display before 26.3, after the
    /// telemetry flag (for every advancement) from 26.3.
    pub x: f32,
    pub y: f32,
    /// Criterion names, before 1.20.2.
    pub criteria: Vec<String>,
    pub requirements: Vec<Vec<String>>,
    /// 1.20 on.
    pub telemetry: bool,
}

/// An advancement's criteria and when each was obtained.
pub type Progress = (String, Vec<(String, Option<i64>)>);

#[derive(Clone, Debug, PartialEq)]
pub struct Advancements {
    pub reset: bool,
    pub added: Vec<Advancement>,
    pub removed: Vec<String>,
    /// Per advancement, each criterion and when it was obtained.
    pub progress: Vec<Progress>,
    /// 1.21.5 on.
    pub show: bool,
}

/// The icon is a non-empty stack template from 26.1 on.
fn template_icons(protocol: i32) -> bool {
    protocol >= 775
}

/// The position left the display in 26.3.
fn outer_position(protocol: i32) -> bool {
    protocol >= 777
}

const IDENTIFIER: usize = 32767;

fn strings(r: &mut Reader<'_>) -> io::Result<Vec<String>> {
    let count = r.count()?;
    (0..count)
        .map(|_| Ok(r.string(IDENTIFIER)?.to_owned()))
        .collect()
}

fn write_strings(w: &mut Writer, values: &[String]) {
    w.varint(values.len() as i32);
    for value in values {
        w.string(value);
    }
}

fn optional_string(r: &mut Reader<'_>) -> io::Result<Option<String>> {
    Ok(if r.bool()? {
        Some(r.string(IDENTIFIER)?.to_owned())
    } else {
        None
    })
}

fn write_optional_string(w: &mut Writer, value: &Option<String>) {
    w.bool(value.is_some());
    if let Some(value) = value {
        w.string(value);
    }
}

impl Advancements {
    pub fn read(protocol: i32, r: &mut Reader<'_>) -> io::Result<Self> {
        let reset = r.bool()?;
        let count = r.count()?;
        let mut added = Vec::with_capacity(count.min(r.remaining()));
        for _ in 0..count {
            let id = r.string(IDENTIFIER)?.to_owned();
            let parent = optional_string(r)?;
            let (mut x, mut y) = (0.0, 0.0);
            let display = if r.bool()? {
                let title = Text::read(protocol, r)?;
                let description = Text::read(protocol, r)?;
                let icon = if template_icons(protocol) {
                    Some(read_template(protocol, r)?)
                } else {
                    window::read_stack(protocol, r)?
                };
                let frame = r.varint()?;
                let flags = r.i32()?;
                let background = if flags & 1 != 0 {
                    Some(r.string(IDENTIFIER)?.to_owned())
                } else {
                    None
                };
                if !outer_position(protocol) {
                    x = r.f32()?;
                    y = r.f32()?;
                }
                Some(Display {
                    title,
                    description,
                    icon,
                    frame,
                    flags,
                    background,
                })
            } else {
                None
            };
            let criteria = if protocol < 764 {
                // A map of criterion names to nothing.
                strings(r)?
            } else {
                Vec::new()
            };
            let groups = r.count()?;
            let requirements = (0..groups).map(|_| strings(r)).collect::<io::Result<_>>()?;
            let telemetry = if protocol >= 763 { r.bool()? } else { false };
            if outer_position(protocol) {
                x = r.f32()?;
                y = r.f32()?;
            }
            added.push(Advancement {
                id,
                parent,
                display,
                x,
                y,
                criteria,
                requirements,
                telemetry,
            });
        }
        let removed = strings(r)?;
        let count = r.count()?;
        let mut progress = Vec::with_capacity(count.min(r.remaining()));
        for _ in 0..count {
            let id = r.string(IDENTIFIER)?.to_owned();
            let criteria = r.count()?;
            let criteria = (0..criteria)
                .map(|_| {
                    let name = r.string(IDENTIFIER)?.to_owned();
                    let obtained = if r.bool()? { Some(r.i64()?) } else { None };
                    Ok((name, obtained))
                })
                .collect::<io::Result<_>>()?;
            progress.push((id, criteria));
        }
        let show = if protocol >= 770 { r.bool()? } else { true };
        Ok(Self {
            reset,
            added,
            removed,
            progress,
            show,
        })
    }

    pub fn write(&self, protocol: i32, w: &mut Writer) -> io::Result<()> {
        w.bool(self.reset).varint(self.added.len() as i32);
        for advancement in &self.added {
            w.string(&advancement.id);
            write_optional_string(w, &advancement.parent);
            w.bool(advancement.display.is_some());
            if let Some(display) = &advancement.display {
                display.title.write(protocol, w)?;
                display.description.write(protocol, w)?;
                if template_icons(protocol) {
                    let icon = display
                        .icon
                        .as_ref()
                        .ok_or_else(|| invalid("an advancement icon cannot be empty"))?;
                    write_template(protocol, w, icon)?;
                } else {
                    window::write_stack(protocol, w, &display.icon)?;
                }
                let flags = match display.background {
                    Some(_) => display.flags | 1,
                    None => display.flags & !1,
                };
                w.varint(display.frame).i32(flags);
                if let Some(background) = &display.background {
                    w.string(background);
                }
                if !outer_position(protocol) {
                    w.f32(advancement.x).f32(advancement.y);
                }
            }
            if protocol < 764 {
                write_strings(w, &advancement.criteria);
            }
            w.varint(advancement.requirements.len() as i32);
            for group in &advancement.requirements {
                write_strings(w, group);
            }
            if protocol >= 763 {
                w.bool(advancement.telemetry);
            }
            if outer_position(protocol) {
                w.f32(advancement.x).f32(advancement.y);
            }
        }
        write_strings(w, &self.removed);
        w.varint(self.progress.len() as i32);
        for (id, criteria) in &self.progress {
            w.string(id).varint(criteria.len() as i32);
            for (name, obtained) in criteria {
                w.string(name).bool(obtained.is_some());
                if let Some(obtained) = obtained {
                    w.i64(*obtained);
                }
            }
        }
        if protocol >= 770 {
            w.bool(self.show);
        }
        Ok(())
    }

    /// Criteria are named by the requirements from 1.20.2 on; older
    /// clients need the names listed.
    pub fn fill_criteria(&mut self) {
        for advancement in &mut self.added {
            if advancement.criteria.is_empty() {
                let mut names: Vec<String> = Vec::new();
                for name in advancement.requirements.iter().flatten() {
                    if !names.contains(name) {
                        names.push(name.clone());
                    }
                }
                advancement.criteria = names;
            }
        }
    }
}
