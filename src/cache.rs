//! Opt-in persistent content-hash cache.

use std::collections::HashMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use directories::ProjectDirs;
use serde::{Deserialize, Serialize};

const CACHE_VERSION: u32 = 1;
const CACHE_FILE: &str = "hash-cache-v1.bin";
const MAX_RECORDS: usize = 1_000_000;
const MAX_AGE_SECS: u64 = 90 * 24 * 60 * 60;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub struct CachedHashes {
    pub prefix: [u8; 32],
    pub full: [u8; 32],
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheEntry {
    size: u64,
    modified_ns: u128,
    hashes: CachedHashes,
    last_seen: u64,
}

#[derive(Debug, Serialize, Deserialize)]
struct CacheFileData {
    version: u32,
    entries: HashMap<PathBuf, CacheEntry>,
}

#[derive(Serialize)]
struct CacheFileRef<'a> {
    version: u32,
    entries: &'a HashMap<PathBuf, CacheEntry>,
}

/// Loaded once per scan and mutated only by the scan coordinator.
pub struct HashCache {
    enabled: bool,
    min_size: u64,
    path: Option<PathBuf>,
    entries: HashMap<PathBuf, CacheEntry>,
    dirty: bool,
}

impl HashCache {
    pub fn open(enabled: bool, min_size: u64) -> (Self, Option<String>) {
        if !enabled {
            return (Self::disabled(min_size), None);
        }
        let Some(path) = default_cache_path() else {
            return (
                Self::disabled(min_size),
                Some("cache disabled: cannot determine the platform cache directory".into()),
            );
        };
        Self::open_at(path, min_size)
    }

    fn disabled(min_size: u64) -> Self {
        Self {
            enabled: false,
            min_size,
            path: None,
            entries: HashMap::new(),
            dirty: false,
        }
    }

    fn open_at(path: PathBuf, min_size: u64) -> (Self, Option<String>) {
        let mut warning = None;
        let entries = match fs::read(&path) {
            Ok(bytes) => match bincode::serde::decode_from_slice::<CacheFileData, _>(
                &bytes,
                bincode::config::standard(),
            ) {
                Ok((file, _)) if file.version == CACHE_VERSION => file.entries,
                Ok(_) => {
                    warning = Some("ignoring an incompatible hash cache".into());
                    HashMap::new()
                }
                Err(err) => {
                    warning = Some(format!("ignoring an unreadable hash cache: {err}"));
                    HashMap::new()
                }
            },
            Err(err) if err.kind() == io::ErrorKind::NotFound => HashMap::new(),
            Err(err) => {
                warning = Some(format!("cannot read hash cache: {err}"));
                HashMap::new()
            }
        };
        (
            Self {
                enabled: true,
                min_size,
                path: Some(path),
                entries,
                dirty: false,
            },
            warning,
        )
    }

    pub fn lookup(
        &mut self,
        path: &Path,
        size: u64,
        modified: Option<SystemTime>,
    ) -> Option<CachedHashes> {
        if !self.enabled || size < self.min_size {
            return None;
        }
        let stamp = modified.and_then(system_time_ns)?;
        let entry = self.entries.get_mut(path)?;
        if entry.size != size || entry.modified_ns != stamp {
            return None;
        }
        entry.last_seen = now_secs();
        self.dirty = true;
        Some(entry.hashes)
    }

    pub fn insert(
        &mut self,
        path: PathBuf,
        size: u64,
        modified: Option<SystemTime>,
        hashes: CachedHashes,
    ) {
        if !self.enabled || size < self.min_size {
            return;
        }
        let Some(modified_ns) = modified.and_then(system_time_ns) else {
            return;
        };
        self.entries.insert(
            path,
            CacheEntry {
                size,
                modified_ns,
                hashes,
                last_seen: now_secs(),
            },
        );
        self.dirty = true;
    }

    pub fn save(&mut self) -> io::Result<()> {
        if !self.enabled || !self.dirty {
            return Ok(());
        }
        self.prune();
        let path = self.path.as_ref().expect("enabled cache has a path");
        let parent = path
            .parent()
            .ok_or_else(|| io::Error::other("cache path has no parent"))?;
        fs::create_dir_all(parent)?;
        let bytes = bincode::serde::encode_to_vec(
            CacheFileRef {
                version: CACHE_VERSION,
                entries: &self.entries,
            },
            bincode::config::standard(),
        )
        .map_err(io::Error::other)?;

        let mut temp = tempfile::NamedTempFile::new_in(parent)?;
        temp.write_all(&bytes)?;
        temp.as_file().sync_all()?;
        temp.persist(path).map_err(|err| err.error)?;
        self.dirty = false;
        Ok(())
    }

    fn prune(&mut self) {
        let cutoff = now_secs().saturating_sub(MAX_AGE_SECS);
        self.entries.retain(|_, entry| entry.last_seen >= cutoff);
        if self.entries.len() <= MAX_RECORDS {
            return;
        }
        let remove_count = self.entries.len() - MAX_RECORDS;
        let mut ages: Vec<_> = self
            .entries
            .iter()
            .map(|(path, entry)| (entry.last_seen, path.clone()))
            .collect();
        ages.sort_unstable_by_key(|(seen, _)| *seen);
        for (_, path) in ages.into_iter().take(remove_count) {
            self.entries.remove(&path);
        }
    }
}

pub fn clear_default() -> io::Result<bool> {
    let Some(path) = default_cache_path() else {
        return Ok(false);
    };
    match fs::remove_file(path) {
        Ok(()) => Ok(true),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(err) => Err(err),
    }
}

fn default_cache_path() -> Option<PathBuf> {
    ProjectDirs::from("", "", "dupefind").map(|dirs| dirs.cache_dir().join(CACHE_FILE))
}

fn system_time_ns(time: SystemTime) -> Option<u128> {
    time.duration_since(UNIX_EPOCH).ok().map(|d| d.as_nanos())
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn a_cache_hit_requires_matching_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.bin");
        let file = dir.path().join("file.bin");
        let modified = UNIX_EPOCH + Duration::from_secs(123);
        let hashes = CachedHashes {
            prefix: [1; 32],
            full: [2; 32],
        };
        let (mut cache, warning) = HashCache::open_at(path, 0);
        assert!(warning.is_none());
        cache.insert(file.clone(), 10, Some(modified), hashes);
        assert_eq!(cache.lookup(&file, 10, Some(modified)), Some(hashes));
        assert_eq!(cache.lookup(&file, 11, Some(modified)), None);
    }

    #[test]
    fn corrupt_cache_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("cache.bin");
        fs::write(&path, b"not bincode").unwrap();
        let (cache, warning) = HashCache::open_at(path, 0);
        assert!(warning.is_some());
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn cache_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("cache.bin");
        let file = dir.path().join("file.bin");
        let modified = UNIX_EPOCH + Duration::from_secs(456);
        let hashes = CachedHashes {
            prefix: [3; 32],
            full: [4; 32],
        };
        let (mut cache, _) = HashCache::open_at(cache_path.clone(), 0);
        cache.insert(file.clone(), 20, Some(modified), hashes);
        cache.save().unwrap();
        let (mut loaded, warning) = HashCache::open_at(cache_path, 0);
        assert!(warning.is_none());
        assert_eq!(loaded.lookup(&file, 20, Some(modified)), Some(hashes));
    }

    #[test]
    fn saving_again_replaces_the_previous_cache_file() {
        let dir = tempfile::tempdir().unwrap();
        let cache_path = dir.path().join("cache.bin");
        let file = dir.path().join("file.bin");
        let modified = UNIX_EPOCH + Duration::from_secs(789);
        let first = CachedHashes {
            prefix: [5; 32],
            full: [6; 32],
        };
        let second = CachedHashes {
            prefix: [7; 32],
            full: [8; 32],
        };
        let (mut cache, _) = HashCache::open_at(cache_path.clone(), 0);
        cache.insert(file.clone(), 30, Some(modified), first);
        cache.save().unwrap();
        cache.insert(file.clone(), 30, Some(modified), second);
        cache.save().unwrap();

        let (mut loaded, warning) = HashCache::open_at(cache_path, 0);
        assert!(warning.is_none());
        assert_eq!(loaded.lookup(&file, 30, Some(modified)), Some(second));
    }
}
