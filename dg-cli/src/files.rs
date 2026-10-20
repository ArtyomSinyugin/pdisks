//! Bounded reads and atomic publication of user-selected draft/plan files.

use crate::Error;
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    os::unix::fs::OpenOptionsExt,
    path::Path,
};

/// Maximum accepted UTF-8 file size, also checked by the underlying document validators.
pub const MAX_INPUT_BYTES: usize = disksgraph::MAX_PLAN_JSON_BYTES;

/// Reads a regular file with a hard allocation bound, including files that grow after metadata.
pub fn read_document(path: &Path) -> Result<String, Error> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32)
        .open(path)?;
    if !file.metadata()?.is_file() {
        return Err(Error::Document("input must be a regular file".into()));
    }
    let mut bytes = Vec::new();
    file.take(MAX_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_INPUT_BYTES {
        return Err(Error::Document(
            "input exceeds the document size limit".into(),
        ));
    }
    String::from_utf8(bytes).map_err(|error| Error::Document(error.to_string()))
}

/// Publishes complete private bytes with a same-directory rename and file/directory synchronization.
/// A failure before rename leaves the previous destination intact; a directory-sync error after
/// rename reports uncertain crash durability even though the new file is already visible.
pub fn save_document(path: &Path, document: &str) -> Result<(), Error> {
    if document.len() > MAX_INPUT_BYTES {
        return Err(Error::Document(
            "output exceeds the document size limit".into(),
        ));
    }
    if path.file_name().is_none() {
        return Err(Error::Usage("output must name a file".into()));
    }
    let parent = path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let temporary = parent.join(format!(".dg-cli-{}.tmp", uuid::Uuid::now_v7()));
    let result = (|| -> Result<(), Error> {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(document.as_bytes())?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{MetadataExt, symlink};

    /// A user-selected FIFO must not stall before the regular-file check.
    #[test]
    fn rejects_fifo_documents_without_waiting_for_a_writer()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!("dg-cli-fifo-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root)?;
        let path = root.join("input");
        rustix::fs::mkfifoat(
            rustix::fs::CWD,
            &path,
            rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        )?;
        assert!(matches!(read_document(&path), Err(Error::Document(_))));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    /// Replacement is private and does not follow an existing destination symlink.
    #[test]
    fn atomic_replace_and_bounds() -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!("dg-cli-files-{}", uuid::Uuid::now_v7()));
        fs::create_dir(&root)?;
        let target = root.join("target");
        let output = root.join("draft.json");
        fs::write(&target, "keep")?;
        symlink(&target, &output)?;
        save_document(&output, "{\"revision\":1}")?;
        assert_eq!(fs::read_to_string(&target)?, "keep");
        assert_eq!(read_document(&output)?, "{\"revision\":1}");
        assert_eq!(fs::metadata(&output)?.mode() & 0o777, 0o600);
        let oversized = "x".repeat(MAX_INPUT_BYTES + 1);
        assert!(save_document(&output, &oversized).is_err());
        assert_eq!(read_document(&output)?, "{\"revision\":1}");
        fs::write(&target, oversized)?;
        assert!(read_document(&target).is_err());
        assert!(read_document(&root).is_err());
        assert!(save_document(&root.join("absent/out"), "x").is_err());
        assert_eq!(fs::read_dir(&root)?.count(), 2);
        fs::remove_dir_all(root)?;
        Ok(())
    }
}
