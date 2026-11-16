//! Remote files larger than one bus message, moved in parts of
//! [`protocol::FILE_PART`].
use std::{io, path::PathBuf, time::Duration};

use serde_json::Value;

use super::{
    Nodes,
    protocol::{self, Command, Files, Upload},
};

/// Each part is one request with this deadline.
const PART_WAIT: Duration = Duration::from_secs(30);

/// What a remote `GET` found.
#[derive(Debug)]
pub enum Remote {
    /// A folder listing, as the agent returned it.
    Folder(Value),
    File(Vec<u8>),
}

impl Nodes {
    /// Read a folder listing or a whole file of up to
    /// [`protocol::MAX_REMOTE_FILE`] bytes. A file that changes between parts
    /// fails with [`io::ErrorKind::ResourceBusy`] instead of mixing versions.
    pub async fn read_file(&self, node: &str, mut request: Files) -> io::Result<Remote> {
        request.offset = Some(0);
        let first = self
            .request(node, &Command::Files(request.clone()), PART_WAIT)
            .await?;
        if first.get("content").is_none() {
            return Ok(Remote::Folder(first));
        }
        let mut contents = part_content(&first)?;
        // Agents from before parts ignore the offset and send the whole file.
        let Some(size) = first["size"].as_u64() else {
            return Ok(Remote::File(contents));
        };
        if size > protocol::MAX_REMOTE_FILE as u64 {
            return Err(too_large());
        }
        let changed = || {
            io::Error::new(
                io::ErrorKind::ResourceBusy,
                "the file changed during the download; try again",
            )
        };
        while (contents.len() as u64) < size {
            request.offset = Some(contents.len() as u64);
            let part = self
                .request(node, &Command::Files(request.clone()), PART_WAIT)
                .await?;
            let bytes = part_content(&part)?;
            if part["size"] != first["size"]
                || part["modified"] != first["modified"]
                || bytes.is_empty()
            {
                return Err(changed());
            }
            contents.extend_from_slice(&bytes);
        }
        if contents.len() as u64 != size {
            return Err(changed());
        }
        Ok(Remote::File(contents))
    }

    /// Create or atomically replace a file of up to
    /// [`protocol::MAX_REMOTE_FILE`] bytes. Files that fit one part use a
    /// single [`Command::Files`], which every agent understands.
    pub async fn write_file(
        &self,
        node: &str,
        server: &str,
        directory: PathBuf,
        path: String,
        contents: &[u8],
    ) -> io::Result<Value> {
        if contents.len() <= protocol::FILE_PART {
            let request = Files {
                server: server.to_owned(),
                directory,
                method: "PUT".into(),
                path,
                folder: false,
                content: Some(protocol::encode_base64(contents)),
                offset: None,
            };
            return self
                .request(node, &Command::Files(request), PART_WAIT)
                .await;
        }
        if contents.len() > protocol::MAX_REMOTE_FILE {
            return Err(too_large());
        }
        let mut random = [0u8; 16];
        aws_lc_rs::rand::fill(&mut random).map_err(|_| io::Error::other("random source failed"))?;
        let id: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
        let parts = contents.len().div_ceil(protocol::FILE_PART);
        let mut reply = Value::Null;
        for (index, part) in contents.chunks(protocol::FILE_PART).enumerate() {
            let upload = Upload {
                server: server.to_owned(),
                directory: directory.clone(),
                path: path.clone(),
                id: id.clone(),
                offset: (index * protocol::FILE_PART) as u64,
                content: protocol::encode_base64(part),
                done: index + 1 == parts,
            };
            reply = self
                .request(node, &Command::Upload(upload), PART_WAIT)
                .await
                .map_err(|error| outdated(node, error))?;
        }
        Ok(reply)
    }
}

fn part_content(reply: &Value) -> io::Result<Vec<u8>> {
    reply["content"]
        .as_str()
        .and_then(protocol::decode_base64)
        .ok_or_else(|| io::Error::other("node returned invalid file contents"))
}

fn too_large() -> io::Error {
    io::Error::new(
        io::ErrorKind::FileTooLarge,
        "files on nodes are limited to 64 MiB through the web console",
    )
}

/// An agent from before [`Command::Upload`] cannot parse it.
fn outdated(node: &str, error: io::Error) -> io::Error {
    if error.kind() == io::ErrorKind::InvalidInput
        && error.to_string().contains("unknown variant `upload`")
    {
        return io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("the agent on node {node} only accepts files up to 512 KiB; update it"),
        );
    }
    error
}
