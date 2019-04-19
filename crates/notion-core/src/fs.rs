//! Provides utilities for operating on the filesystem.

use std::fs::{self, create_dir_all, read_dir, DirEntry, File, Metadata};
use std::io::{self, ErrorKind};
use std::path::{Path, PathBuf};

use crate::error::ErrorDetails;
use notion_fail::{Fallible, ResultExt};

pub fn delete_dir_error<P: AsRef<Path>>(directory: &P) -> impl FnOnce(&io::Error) -> ErrorDetails {
    let directory = directory.as_ref().to_string_lossy().to_string();
    |_| ErrorDetails::DeleteDirectoryError { directory }
}

pub fn touch(path: &Path) -> Fallible<File> {
    if !path.is_file() {
        let basedir = path.parent().unwrap();
        create_dir_all(basedir).unknown()?;
        File::create(path).unknown()?;
    }
    File::open(path).unknown()
}

/// This creates the parent directory of the input path, assuming the input path is a file.
pub fn ensure_containing_dir_exists<P: AsRef<Path>>(path: &P) -> Fallible<()> {
    path.as_ref()
        .parent()
        .ok_or(
            ErrorDetails::ContainingDirError {
                path: path.as_ref().to_string_lossy().to_string(),
            }
            .into(),
        )
        .and_then(|dir| {
            fs::create_dir_all(dir).with_context(|_| ErrorDetails::CreateDirError {
                dir: dir.to_string_lossy().to_string(),
            })
        })
}

/// This deletes the input directory, if it exists
pub fn ensure_dir_does_not_exist<P: AsRef<Path>>(path: &P) -> Fallible<()> {
    if path.as_ref().exists() {
        // remove the directory and all of its contents
        fs::remove_dir_all(path).with_context(delete_dir_error(path))?;
    }
    Ok(())
}

/// Reads a file, if it exists.
pub fn read_file_opt(path: &PathBuf) -> io::Result<Option<String>> {
    let result: io::Result<String> = fs::read_to_string(path);

    match result {
        Ok(string) => Ok(Some(string)),
        Err(error) => match error.kind() {
            ErrorKind::NotFound => Ok(None),
            _ => Err(error),
        },
    }
}

/// Reads the full contents of a directory, eagerly extracting each directory entry
/// and its metadata and returning an iterator over them. Returns `Error` if any of
/// these steps fails.
///
/// This function makes it easier to write high level logic for manipulating the
/// contents of directories (map, filter, etc).
///
/// Note that this function allocates an intermediate vector of directory entries to
/// construct the iterator from, so if a directory is expected to be very large, it
/// will allocate temporary data proportional to the number of entries.
pub fn read_dir_eager(dir: &Path) -> Fallible<impl Iterator<Item = (DirEntry, Metadata)>> {
    Ok(read_dir(dir)
        .unknown()?
        .map(|entry| {
            let entry = entry.unknown()?;
            let metadata = entry.metadata().unknown()?;
            Ok((entry, metadata))
        })
        .collect::<Fallible<Vec<(DirEntry, Metadata)>>>()?
        .into_iter())
}

/// Reads the contents of a directory and returns a Vec of the matched results
/// from the input function
pub fn dir_entry_match<T, F>(dir: &Path, mut f: F) -> Fallible<Vec<T>>
where
    F: FnMut(&DirEntry) -> Option<T>,
{
    Ok(read_dir_eager(dir)?
        .filter(|(_, metadata)| metadata.is_file())
        .filter_map(|(entry, _)| f(&entry))
        .collect::<Vec<T>>())
}
