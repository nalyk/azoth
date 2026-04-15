//! Content-addressed blob store at `.azoth/artifacts/<sha256>`.
//!
//! Artifacts hold tool output and large evidence payloads so ContextPackets
//! stay small and the replay log stays reference-based.

use crate::schemas::ArtifactId;
use sha2::{Digest, Sha256};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    pub fn open<P: AsRef<Path>>(root: P) -> io::Result<Self> {
        let root = root.as_ref().to_path_buf();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Write raw bytes and return the artifact identifier (`art_<sha256>`).
    pub fn put(&self, bytes: &[u8]) -> io::Result<ArtifactId> {
        let hash = sha256_hex(bytes);
        let path = self.root.join(&hash);
        if !path.exists() {
            fs::write(&path, bytes)?;
        }
        Ok(ArtifactId::from(format!("art_{hash}")))
    }

    pub fn get(&self, id: &ArtifactId) -> io::Result<Vec<u8>> {
        let hash = id.as_str().strip_prefix("art_").unwrap_or(id.as_str());
        fs::read(self.root.join(hash))
    }

    pub fn contains(&self, id: &ArtifactId) -> bool {
        let hash = id.as_str().strip_prefix("art_").unwrap_or(id.as_str());
        self.root.join(hash).exists()
    }
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn put_then_get_roundtrips() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::open(dir.path()).unwrap();
        let id = store.put(b"hello").unwrap();
        assert!(store.contains(&id));
        assert_eq!(store.get(&id).unwrap(), b"hello");
    }

    #[test]
    fn identical_content_is_deduplicated() {
        let dir = tempdir().unwrap();
        let store = ArtifactStore::open(dir.path()).unwrap();
        let a = store.put(b"same").unwrap();
        let b = store.put(b"same").unwrap();
        assert_eq!(a.as_str(), b.as_str());
    }
}
