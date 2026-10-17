//! The cache's disk: verified objects and signed manifests under one byte budget,
//! evicted least recently used first.
//!
//! Layout under the cache directory:
//! - `objects/{sha[..2]}/{sha}`: exact object bytes, named by their SHA-256.
//! - `manifests/{copy}`: the owner's signed manifest envelope for a copy.
//! - `tmp/`: files being written. A file is written and synced here, then renamed into
//!   place, so a crash leaves only a stray temp file (removed at the next start), never
//!   a short object. Every read is checked again anyway (`MediaCache` verifies objects
//!   on each hit and manifests on each load).
//!
//! Every rename and delete of a stored file happens with the index lock held, so the
//! index and the files agree: an eviction cannot delete a file written after it chose
//! its victim, and a failed read forgets an entry only when its file is really gone.

use std::collections::{BTreeMap, HashMap};
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result};

#[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) enum Key {
    Object(String),
    Manifest(String),
}

#[derive(Default)]
struct Lru {
    /// Key -> (size in bytes, last-use tick, generation).
    entries: HashMap<Key, (u64, u64, u64)>,
    /// Last-use tick -> key; the first entry is the least recently used.
    order: BTreeMap<u64, Key>,
    total: u64,
    tick: u64,
    generation: u64,
}

impl Lru {
    fn touch(&mut self, key: &Key) {
        self.tick += 1;
        let tick = self.tick;
        if let Some(entry) = self.entries.get_mut(key) {
            self.order.remove(&entry.1);
            entry.1 = tick;
            self.order.insert(tick, key.clone());
        }
    }

    fn insert(&mut self, key: Key, size: u64) {
        self.remove(&key);
        self.tick += 1;
        self.generation += 1;
        self.entries
            .insert(key.clone(), (size, self.tick, self.generation));
        self.order.insert(self.tick, key);
        self.total += size;
    }

    fn remove(&mut self, key: &Key) {
        if let Some((size, tick, _)) = self.entries.remove(key) {
            self.order.remove(&tick);
            self.total -= size;
        }
    }

    /// Least recently used keys to drop until the total fits `max`, never `keep`.
    fn evict(&mut self, max: u64, keep: Option<&Key>) -> Vec<(Key, u64)> {
        let mut victims = Vec::new();
        while self.total > max {
            let Some(key) = self.order.values().find(|key| Some(*key) != keep).cloned() else {
                break;
            };
            let size = self.entries[&key].0;
            self.remove(&key);
            victims.push((key, size));
        }
        victims
    }
}

struct Inner {
    dir: PathBuf,
    max_bytes: u64,
    lru: Mutex<Lru>,
}

impl Inner {
    fn path(&self, key: &Key) -> PathBuf {
        match key {
            Key::Object(sha) => self.dir.join("objects").join(&sha[..2]).join(sha),
            Key::Manifest(copy) => self.dir.join("manifests").join(copy),
        }
    }
}

/// Cheap to clone; clones share one index.
#[derive(Clone)]
pub(crate) struct Store(Arc<Inner>);

pub(crate) fn valid_sha(sha: &str) -> bool {
    sha.len() == 64
        && sha
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl Store {
    /// Open (or create) the store, drop leftover temp files and anything that is not a
    /// store file, and evict down to the budget. Older files count as less recently
    /// used.
    pub(crate) fn open(dir: &Path, max_bytes: u64) -> Result<Self> {
        for sub in ["objects", "manifests", "tmp"] {
            std::fs::create_dir_all(dir.join(sub))
                .with_context(|| format!("create {}", dir.join(sub).display()))?;
        }
        for entry in std::fs::read_dir(dir.join("tmp"))? {
            remove_any(&entry?.path());
        }
        let mut found: Vec<(std::time::SystemTime, Key, u64)> = Vec::new();
        for entry in std::fs::read_dir(dir.join("manifests"))? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let meta = entry.metadata()?;
            if meta.is_file() && crate::valid_media_copy(&name) {
                found.push((meta.modified()?, Key::Manifest(name), meta.len()));
            } else {
                remove_any(&entry.path());
            }
        }
        for prefix in std::fs::read_dir(dir.join("objects"))? {
            let prefix = prefix?;
            let prefix_name = prefix.file_name().to_string_lossy().into_owned();
            if !prefix.metadata()?.is_dir()
                || prefix_name.len() != 2
                || !valid_sha(&format!("{prefix_name}{}", "0".repeat(62)))
            {
                remove_any(&prefix.path());
                continue;
            }
            for entry in std::fs::read_dir(prefix.path())? {
                let entry = entry?;
                let name = entry.file_name().to_string_lossy().into_owned();
                let meta = entry.metadata()?;
                if meta.is_file() && valid_sha(&name) && name.starts_with(&prefix_name) {
                    found.push((meta.modified()?, Key::Object(name), meta.len()));
                } else {
                    remove_any(&entry.path());
                }
            }
        }
        found.sort();
        let mut lru = Lru::default();
        for (_, key, size) in found {
            lru.insert(key, size);
        }
        let inner = Inner {
            dir: dir.to_path_buf(),
            max_bytes,
            lru: Mutex::new(lru),
        };
        {
            let mut lru = inner.lru.lock().unwrap();
            for (key, size) in lru.evict(max_bytes, None) {
                if !remove_any(&inner.path(&key)) {
                    lru.insert(key, size);
                }
            }
        }
        Ok(Self(Arc::new(inner)))
    }

    pub(crate) fn total_bytes(&self) -> u64 {
        self.0.lru.lock().unwrap().total
    }

    #[cfg(test)]
    pub(crate) fn contains(&self, key: &Key) -> bool {
        self.0.lru.lock().unwrap().entries.contains_key(key)
    }

    /// The stored bytes and generation, marked as just used. `None` when absent.
    pub(crate) async fn get(&self, key: &Key) -> Result<Option<(Vec<u8>, u64)>> {
        let generation = {
            let lru = self.0.lru.lock().unwrap();
            match lru.entries.get(key) {
                Some(entry) => entry.2,
                None => return Ok(None),
            }
        };
        let path = self.0.path(key);
        match tokio::fs::read(&path).await {
            Ok(bytes) => {
                let mut lru = self.0.lru.lock().unwrap();
                if lru
                    .entries
                    .get(key)
                    .is_some_and(|entry| entry.2 == generation)
                {
                    lru.touch(key);
                }
                Ok(Some((bytes, generation)))
            }
            Err(error) if error.kind() == ErrorKind::NotFound => {
                // Evicted meanwhile. Forget it only if no newer copy was written since.
                let mut lru = self.0.lru.lock().unwrap();
                if lru
                    .entries
                    .get(key)
                    .is_some_and(|entry| entry.2 == generation)
                    && !path.exists()
                {
                    lru.remove(key);
                }
                Ok(None)
            }
            Err(error) => Err(error).context("read cache file"),
        }
    }

    /// Store `bytes` under `key` (temp file, sync, rename), then evict down to the
    /// budget. A value larger than the whole budget is not stored. The work runs to its
    /// end on a blocking thread even when the caller is cancelled, so the index always
    /// counts every file in place.
    pub(crate) async fn put(&self, key: &Key, bytes: Vec<u8>) -> Result<()> {
        if bytes.len() as u64 > self.0.max_bytes {
            return Ok(());
        }
        let inner = self.0.clone();
        let key = key.clone();
        tokio::task::spawn_blocking(move || put_blocking(&inner, key, &bytes)).await?
    }

    /// Delete a failed entry only if it has not been replaced since the read.
    pub(crate) fn remove(&self, key: &Key, generation: u64) {
        let mut lru = self.0.lru.lock().unwrap();
        if lru
            .entries
            .get(key)
            .is_some_and(|entry| entry.2 == generation)
            && remove_any(&self.0.path(key))
        {
            lru.remove(key);
        }
    }
}

fn put_blocking(inner: &Inner, key: Key, bytes: &[u8]) -> Result<()> {
    let final_path = inner.path(&key);
    let temp = inner
        .dir
        .join("tmp")
        .join(uuid::Uuid::new_v4().simple().to_string());
    let written = (|| -> std::io::Result<()> {
        if let Some(parent) = final_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()
    })();
    if let Err(error) = written {
        let _ = std::fs::remove_file(&temp);
        return Err(error).context("write cache file");
    }
    let mut lru = inner.lru.lock().unwrap();
    if let Err(error) = std::fs::rename(&temp, &final_path) {
        let _ = std::fs::remove_file(&temp);
        return Err(error).context("place cache file");
    }
    lru.insert(key.clone(), bytes.len() as u64);
    for (victim, size) in lru.evict(inner.max_bytes, Some(&key)) {
        if !remove_any(&inner.path(&victim)) {
            lru.insert(victim, size);
        }
    }
    Ok(())
}

fn remove_any(path: &Path) -> bool {
    let result = if path.is_dir() {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match result {
        Ok(()) => true,
        Err(error) if error.kind() == ErrorKind::NotFound => true,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "media cache could not remove a file");
            false
        }
    }
}
