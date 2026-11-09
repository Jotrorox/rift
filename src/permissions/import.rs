//! LuckPerms `/lp export` files in and out. The crate decodes string nodes
//! ([`rift_permissions::luckperms`]); this writes the result in one revision.
use super::{Authority, PermError, store};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use rift_permissions::{
    Actor, SubjectRef,
    luckperms::{Export, Skipped},
};
use serde::Serialize;
use std::io::{self, BufRead, BufReader, Read, Write};

/// Largest export accepted, after decompression.
const MAX_IMPORT_BYTES: u64 = 256 * 1024 * 1024;

/// What an import wrote and what it left out.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct ImportReport {
    /// The revision of the import, or the current one when nothing changed.
    pub revision: u64,
    /// Recorded changes; 0 when the store already held everything.
    pub ops: usize,
    pub groups: usize,
    pub users: usize,
    pub tracks: usize,
    /// Nodes and subjects not imported: regex nodes, invalid names, nodes
    /// past a limit, and expiries already in the past.
    pub skipped: Vec<Skipped>,
}

/// Parse an export, gzipped (as `/lp export` writes it) or plain JSON.
pub fn read_export(reader: impl Read) -> Result<Export, PermError> {
    let mut reader = BufReader::new(reader);
    let gzip = reader
        .fill_buf()
        .map_err(|e| PermError::Import(format!("cannot read the export: {e}")))?
        .starts_with(&[0x1f, 0x8b]);
    let mut text = Vec::new();
    let read = if gzip {
        GzDecoder::new(reader)
            .take(MAX_IMPORT_BYTES + 1)
            .read_to_end(&mut text)
    } else {
        reader.take(MAX_IMPORT_BYTES + 1).read_to_end(&mut text)
    };
    read.map_err(|e| PermError::Import(format!("cannot read the export: {e}")))?;
    if text.len() as u64 > MAX_IMPORT_BYTES {
        return Err(PermError::Import(format!(
            "the export exceeds {} MiB",
            MAX_IMPORT_BYTES >> 20
        )));
    }
    serde_json::from_slice(&text)
        .map_err(|e| PermError::Import(format!("not a LuckPerms export: {e}")))
}

/// Write an export as JSON, gzipped when `gzip` is set.
pub fn write_export(export: &Export, writer: impl Write, gzip: bool) -> io::Result<()> {
    if gzip {
        let mut encoder = GzEncoder::new(writer, Compression::default());
        serde_json::to_writer(&mut encoder, export)?;
        encoder.finish()?.flush()
    } else {
        let mut writer = writer;
        serde_json::to_writer(&mut writer, export)?;
        writer.flush()
    }
}

/// Validation and limit failures skip one item; storage failures abort.
fn skip(
    skipped: &mut Vec<Skipped>,
    subject: String,
    key: &str,
    error: PermError,
) -> Result<(), PermError> {
    if matches!(error, PermError::Storage(_)) {
        return Err(error);
    }
    skipped.push(Skipped {
        subject,
        key: key.to_owned(),
        reason: error.to_string(),
    });
    Ok(())
}

impl Authority {
    /// Import a LuckPerms export in one revision. Additive: nodes land in
    /// their slots with source `import` and nothing else is deleted, except
    /// that `replace` first removes every earlier `import` node.
    pub fn import_luckperms(
        &self,
        actor: Actor,
        reader: impl Read,
        replace: bool,
    ) -> Result<ImportReport, PermError> {
        let import = read_export(reader)?.import();
        let mut skipped = import.skipped.clone();
        let (counts, applied) = self.write(&actor, Some("LuckPerms import"), None, |work| {
            if replace {
                work.delete_imported()?;
            }
            let mut counts = (0, 0, 0);
            for group in &import.groups {
                let label = format!("group:{}", group.name);
                if !work.group_exists(&group.name)?
                    && let Err(error) = work.create_group(&group.name, 0, None)
                {
                    skip(&mut skipped, label, "", error)?;
                    continue;
                }
                counts.0 += 1;
                let subject = SubjectRef::Group(group.name.clone());
                for node in &group.nodes {
                    if let Err(error) = work.set_node(&subject, node.clone()) {
                        skip(&mut skipped, label.clone(), &node.payload.key(), error)?;
                    }
                }
            }
            for user in &import.users {
                let label = format!("user:{}", user.uuid.hyphenated());
                if let Some(name) = &user.last_name {
                    store::set_last_name(work.connection(), user.uuid, name, work.now())?;
                }
                counts.1 += 1;
                let subject = SubjectRef::User(user.uuid);
                for node in &user.nodes {
                    if let Err(error) = work.set_node(&subject, node.clone()) {
                        skip(&mut skipped, label.clone(), &node.payload.key(), error)?;
                    }
                }
            }
            for track in &import.tracks {
                match work.set_track(&track.name, track.groups.clone()) {
                    Ok(()) => counts.2 += 1,
                    Err(error) => skip(&mut skipped, format!("track:{}", track.name), "", error)?,
                }
            }
            Ok(counts)
        })?;
        Ok(ImportReport {
            revision: applied.revision,
            ops: applied.ops,
            groups: counts.0,
            users: counts.1,
            tracks: counts.2,
            skipped,
        })
    }

    /// Everything in the LuckPerms export format, which `/lp import` reads
    /// back. Contexts with several values of one key keep the first value.
    pub fn export_luckperms(&self) -> Result<Export, PermError> {
        let users = store::load_users(&self.database().lock())?;
        let groups: Vec<_> = self.groups().values().cloned().collect();
        let tracks: Vec<_> = self.tracks().values().cloned().collect();
        Ok(Export::from_model(&users, &groups, &tracks))
    }
}
