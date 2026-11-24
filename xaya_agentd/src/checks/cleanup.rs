//! Only durable records with an external ownership token authorize integration directory removal.
use super::Record;
use crate::runtime::{Runtime, user_id};
use crate::storage::{Store, can_resume};
use crate::{Error, Result};
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

/// The owned directory for `id`, whose checkout must be exactly `root`.
fn directory(store: &Store, id: &str, root: &str) -> Result<PathBuf> {
    let parent = store.integration_root()?;
    if !(id.starts_with("check-") || id.starts_with("review-"))
        || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
    {
        return Err(Error::Protocol("invalid integration directory identity"));
    }
    let directory = parent.join(id);
    if Path::new(root) != directory.join("checkout") {
        return Err(Error::Protocol("integration directory is outside its owned location"));
    }
    Ok(directory)
}

pub(super) fn create(store: &Store, record: &Record) -> Result<()> {
    let token = record
        .integration_token
        .as_ref()
        .ok_or(Error::Protocol("missing integration ownership token"))?;
    create_owned(store, &record.check.id, &record.check.root, token)
}

/// Create `<integrations>/<id>` with an ownership marker holding `token`, and an empty checkout.
pub(crate) fn create_owned(store: &Store, id: &str, root: &str, token: &str) -> Result<()> {
    let directory = directory(store, id, root)?;
    DirBuilder::new().mode(0o700).create(&directory)?;
    let mut owner = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join("owner"))?;
    owner.write_all(token.as_bytes())?;
    owner.sync_all()?;
    File::open(&directory)?.sync_all()?;
    DirBuilder::new().mode(0o700).create(directory.join("checkout"))?;
    Ok(())
}

pub(super) fn remove(store: &Store, record: &Record) -> Result<()> {
    can_resume(&record.launch)?;
    let token = record
        .integration_token
        .as_ref()
        .ok_or(Error::Protocol("missing integration ownership token"))?;
    remove_owned(store, &record.check.id, &record.check.root, token)
}

/// Remove an owned directory, only if its marker still holds `token`. The caller must already
/// know that nothing it launched still runs there.
pub(crate) fn remove_owned(store: &Store, id: &str, root: &str, token: &str) -> Result<()> {
    let directory = directory(store, id, root)?;
    match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        other => {
            other?;
        },
    }
    Runtime::prepare(Some(directory.clone()))?;
    let owner = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory.join("owner"))?;
    let metadata = owner.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != user_id()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(Error::Protocol("integration ownership marker is unsafe"));
    }
    let mut actual = String::new();
    owner.take(256).read_to_string(&mut actual)?;
    if actual != token {
        return Err(Error::Protocol("integration ownership token changed"));
    }
    // remove_dir_all does not follow symlinks; the checkout is contained by the private owner
    // directory.
    let parent = directory.parent().ok_or(Error::Protocol("missing integration parent"))?;
    fs::remove_dir_all(&directory)?;
    File::open(parent)?.sync_all()?;
    Ok(())
}
