//! Private, immutable content-addressed storage for trusted in-process callers.
//!
//! This module deliberately accepts bytes only. Callers cannot choose a path,
//! URL, or digest when publishing content.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Maximum content size accepted by the store (25 MiB).
pub const MAX_ARTIFACT_BYTES: usize = 25 * 1024 * 1024;

const DIGEST_HEX_LEN: usize = 64;

/// Identity and verified size of an immutable stored blob.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredArtifact {
    /// Lowercase SHA-256 hex digest.
    pub sha256: String,
    /// Content length in bytes.
    pub byte_length: u64,
}

/// A private content-addressed blob store.
#[derive(Debug, Clone)]
pub struct ContentStore {
    root: PathBuf,
    blobs: PathBuf,
}

impl ContentStore {
    /// Open or create a store rooted at `root`.
    ///
    /// The root and its `blobs` directory must be real directories, not
    /// symlinks. On Unix, both are restricted to owner access.
    pub fn open(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        ensure_private_dir(&root)?;
        let blobs = root.join("blobs");
        ensure_private_dir(&blobs)?;
        Ok(Self { root, blobs })
    }

    /// Store bytes immutably and return their content identity.
    pub fn put(&self, bytes: &[u8]) -> io::Result<StoredArtifact> {
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("artifact exceeds {MAX_ARTIFACT_BYTES} byte limit"),
            ));
        }

        let digest = hex(&Sha256::digest(bytes));
        let target = self.blob_path(&digest)?;
        if target.exists() {
            verify_file(&target, &digest, bytes.len() as u64)?;
            return Ok(StoredArtifact {
                sha256: digest,
                byte_length: bytes.len() as u64,
            });
        }

        let mut staging = tempfile::Builder::new()
            .prefix(".artifact-")
            .tempfile_in(&self.blobs)?;
        #[cfg(unix)]
        set_mode(staging.as_file(), 0o600)?;
        staging.write_all(bytes)?;
        staging.flush()?;
        staging.as_file().sync_all()?;
        verify_file(staging.path(), &digest, bytes.len() as u64)?;

        match staging.persist_noclobber(&target) {
            Ok(_) => sync_dir(&self.blobs)?,
            Err(error) if error.error.kind() == io::ErrorKind::AlreadyExists => {
                // Another writer may have won the race. Never accept a
                // pre-existing blob without checking its bytes.
                verify_file(&target, &digest, bytes.len() as u64)?;
            }
            Err(error) => return Err(error.error),
        }

        Ok(StoredArtifact {
            sha256: digest,
            byte_length: bytes.len() as u64,
        })
    }

    /// Store bytes and return the unforgeable metadata token accepted by the
    /// SQLite registry. Only this module can construct that token.
    pub fn put_verified(&self, bytes: &[u8]) -> io::Result<super::artifact::VerifiedContent> {
        let stored = self.put(bytes)?;
        Ok(super::artifact::VerifiedContent {
            sha256: stored.sha256,
            byte_length: stored.byte_length,
        })
    }

    /// Read and verify the exact blob named by a lowercase SHA-256 digest.
    pub fn read(&self, digest: &str) -> io::Result<Vec<u8>> {
        let path = self.blob_path(digest)?;
        let file = open_read_no_follow(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(invalid_data("artifact blob is not a regular file"));
        }
        if metadata.len() > MAX_ARTIFACT_BYTES as u64 {
            return Err(invalid_data("artifact blob exceeds size limit"));
        }

        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take(MAX_ARTIFACT_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() > MAX_ARTIFACT_BYTES {
            return Err(invalid_data("artifact blob exceeds size limit"));
        }
        if bytes.len() as u64 != metadata.len() {
            return Err(invalid_data("artifact length changed while reading"));
        }
        verify_bytes(&bytes, digest, metadata.len())?;
        Ok(bytes)
    }

    /// Return the store root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn blob_path(&self, digest: &str) -> io::Result<PathBuf> {
        if digest.len() != DIGEST_HEX_LEN
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "digest must be 64 lowercase hexadecimal characters",
            ));
        }
        Ok(self.blobs.join(digest))
    }
}

fn ensure_private_dir(path: &Path) -> io::Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            return Err(invalid_data("artifact store path must be a real directory"));
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => fs::create_dir_all(path)?,
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(invalid_data("artifact store path must be a real directory"));
    }
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn verify_file(path: &Path, digest: &str, expected_len: u64) -> io::Result<()> {
    let mut file = open_read_no_follow(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file() {
        return Err(invalid_data("artifact blob is not a regular file"));
    }
    if metadata.len() != expected_len {
        return Err(invalid_data("artifact length does not match its identity"));
    }
    let mut hasher = Sha256::new();
    let mut length = 0u64;
    let mut buffer = [0u8; 8192];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        length += read as u64;
        hasher.update(&buffer[..read]);
    }
    if length != expected_len || hex(&hasher.finalize()) != digest {
        return Err(invalid_data("artifact content does not match its digest"));
    }
    Ok(())
}

fn open_read_no_follow(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW);
    }
    options.open(path)
}

fn verify_bytes(bytes: &[u8], digest: &str, expected_len: u64) -> io::Result<()> {
    if bytes.len() as u64 != expected_len || hex(&Sha256::digest(bytes)) != digest {
        return Err(invalid_data(
            "artifact content does not match its digest or length",
        ));
    }
    Ok(())
}

fn hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for &byte in bytes {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
}

fn invalid_data(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

#[cfg(unix)]
fn set_mode(file: &File, mode: u32) -> io::Result<()> {
    file.set_permissions(fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn set_mode(_file: &File, _mode: u32) -> io::Result<()> {
    Ok(())
}

#[cfg(unix)]
fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_dir(_path: &Path) -> io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_deduplicates_content() {
        let temp = tempfile::tempdir().unwrap();
        let store = ContentStore::open(temp.path().join("private")).unwrap();
        let first = store.put(b"artifact bytes").unwrap();
        let second = store.put(b"artifact bytes").unwrap();
        assert_eq!(first, second);
        assert_eq!(store.read(&first.sha256).unwrap(), b"artifact bytes");
        assert_eq!(fs::read_dir(store.blobs).unwrap().count(), 1);
    }

    #[test]
    fn rejects_oversize_content() {
        let temp = tempfile::tempdir().unwrap();
        let store = ContentStore::open(temp.path().join("private")).unwrap();
        let bytes = vec![0; MAX_ARTIFACT_BYTES + 1];
        assert_eq!(
            store.put(&bytes).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn detects_corrupted_content() {
        let temp = tempfile::tempdir().unwrap();
        let store = ContentStore::open(temp.path().join("private")).unwrap();
        let artifact = store.put(b"original").unwrap();
        fs::write(store.blob_path(&artifact.sha256).unwrap(), b"tampered").unwrap();
        assert_eq!(
            store.read(&artifact.sha256).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn rejects_invalid_digests() {
        let temp = tempfile::tempdir().unwrap();
        let store = ContentStore::open(temp.path().join("private")).unwrap();
        for digest in ["../outside", &"A".repeat(64), &"a".repeat(63)] {
            assert_eq!(
                store.read(digest).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn applies_private_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().join("private");
        let store = ContentStore::open(&root).unwrap();
        let artifact = store.put(b"secret").unwrap();
        let root_mode = fs::metadata(&root).unwrap().permissions().mode() & 0o777;
        let blobs_mode = fs::metadata(root.join("blobs"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        let blob_mode = fs::metadata(store.blob_path(&artifact.sha256).unwrap())
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(root_mode, 0o700);
        assert_eq!(blobs_mode, 0o700);
        assert_eq!(blob_mode, 0o600);
    }
}
