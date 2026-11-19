//! LuckPerms `/lp export` files in and out. The crate decodes string nodes
//! ([`rift_permissions::luckperms`]); this writes the result in one revision
//! ([`super::ops::PermBody::Import`]).
use super::{
    Authority, PermError,
    ops::{PermBody, PermDetail},
    store,
};
use flate2::{Compression, read::GzDecoder, write::GzEncoder};
use rift_permissions::{
    Actor,
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

impl Authority {
    /// Import a LuckPerms export in one revision. Additive: nodes land in
    /// their slots with source `import` and nothing else is deleted, except
    /// that `replace` first removes every earlier `import` node.
    /// `server=<name>` naming one of the configuration's service groups
    /// becomes `group=<name>` (see [`Authority::set_service_groups`]).
    pub fn import_luckperms(
        &self,
        actor: Actor,
        reader: impl Read,
        replace: bool,
    ) -> Result<ImportReport, PermError> {
        let import = read_export(reader)?.import_in(&self.service_groups());
        let mut skipped = import.skipped.clone();
        let body = PermBody::Import { import, replace };
        let (detail, applied) = self.write(&actor, Some("LuckPerms import"), None, body)?;
        let (groups, users, tracks) = match detail {
            PermDetail::Import {
                groups,
                users,
                tracks,
                skipped: more,
            } => {
                skipped.extend(more);
                (groups, users, tracks)
            }
            _ => (0, 0, 0),
        };
        Ok(ImportReport {
            revision: applied.revision,
            ops: applied.ops,
            groups,
            users,
            tracks,
            skipped,
        })
    }

    /// Everything in the LuckPerms export format, which `/lp import` reads
    /// back. Contexts with several values of one key keep the first value.
    pub fn export_luckperms(&self) -> Result<Export, PermError> {
        let users = store::load_users(&self.database().read())?;
        let groups: Vec<_> = self.groups().values().cloned().collect();
        let tracks: Vec<_> = self.tracks().values().cloned().collect();
        Ok(Export::from_model(&users, &groups, &tracks))
    }
}
