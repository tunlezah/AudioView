//! The artwork cache: content-addressed files plus an SQLite index.
//!
//! `/var/lib/lpframe/cache/<aa>/<sha256>.jpg`, with the index carrying the
//! LRU order and the negative entries (DESIGN §5.4).
//!
//! **Why SQLite and not just a directory.** Eviction needs to know which
//! entry was used least recently, and the obvious source for that is
//! `st_atime`. It is not usable: `relatime` and `noatime` are both normal on
//! this device — §8.3 mounts with `noatime` deliberately — so access times
//! either lag by a day or never move at all. Rebuilding an LRU order from
//! `stat()` therefore looks like it works and quietly degrades to random
//! eviction. The index also gives us an atomic, crash-safe record of what is
//! present, which a directory listing after a power cut does not.
//!
//! **Ordering is deliberate: file first, row second.** A crash between the
//! two leaves an unreferenced file, which the next sweep removes. The reverse
//! ordering would leave a row promising bytes that are not there, and a cache
//! that lies is worse than one that forgets.

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result};
use lpframe_proto::ArtworkSource;
use rusqlite::{Connection, OptionalExtension};
use sha2::{Digest, Sha256};

const SCHEMA: &str = "
    CREATE TABLE IF NOT EXISTS art (
        key         TEXT PRIMARY KEY,
        sha256      TEXT NOT NULL,
        bytes       INTEGER NOT NULL,
        width       INTEGER,
        height      INTEGER,
        source      TEXT NOT NULL,
        created_at  INTEGER NOT NULL,
        last_access INTEGER NOT NULL
    );
    CREATE INDEX IF NOT EXISTS art_lru ON art (last_access);
    CREATE TABLE IF NOT EXISTS negative (
        key        TEXT PRIMARY KEY,
        created_at INTEGER NOT NULL
    );
";

/// What the index knows about a cached image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub sha256: String,
    pub bytes: u64,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub source: ArtworkSource,
}

/// What a sweep removed, for the log and the diagnostics page.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Swept {
    pub evicted: u64,
    pub expired_negatives: u64,
    pub orphan_files: u64,
}

impl Swept {
    pub fn is_empty(&self) -> bool {
        *self == Swept::default()
    }
}

pub struct Cache {
    dir: PathBuf,
    max_bytes: u64,
    negative_ttl_secs: i64,
    conn: Mutex<Connection>,
    /// Wall-clock seconds. Wall clock rather than monotonic because a
    /// seven-day TTL has to survive a reboot, and injectable because the
    /// alternative way to test expiry is to wait a week.
    now: Box<dyn Fn() -> i64 + Send + Sync>,
}

impl Cache {
    pub fn open(dir: &Path, max_bytes: u64, negative_ttl_secs: i64) -> Result<Cache> {
        std::fs::create_dir_all(dir)
            .with_context(|| format!("creating cache directory {}", dir.display()))?;
        let conn = Connection::open(dir.join("index.sqlite"))
            .with_context(|| format!("opening the cache index under {}", dir.display()))?;
        // WAL so a reader is never blocked by the writer, and NORMAL because
        // the durable artefact is the image file — losing the last index
        // write to a power cut costs one re-fetch, not correctness.
        conn.execute_batch(
            "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
        )
        .context("configuring the cache index")?;
        conn.execute_batch(SCHEMA)
            .context("creating the cache schema")?;

        Ok(Cache {
            dir: dir.to_path_buf(),
            max_bytes,
            negative_ttl_secs,
            conn: Mutex::new(conn),
            now: Box::new(unix_now),
        })
    }

    /// Replace the clock. Tests only; a device always uses the system clock.
    #[cfg(test)]
    fn with_clock(mut self, now: impl Fn() -> i64 + Send + Sync + 'static) -> Cache {
        self.now = Box::new(now);
        self
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("cache index mutex poisoned")
    }

    fn path_for(&self, sha256: &str) -> PathBuf {
        self.dir.join(&sha256[..2]).join(format!("{sha256}.jpg"))
    }

    /// Fetch a cached image, verifying the bytes still match the index.
    ///
    /// A row whose file is missing or corrupt is dropped rather than
    /// returned: the cost is one re-fetch, and the alternative is putting a
    /// truncated image on the wall.
    pub fn get(&self, key: &str) -> Option<(Entry, Vec<u8>)> {
        let entry: Entry = {
            let conn = self.lock();
            conn.query_row(
                "SELECT sha256, bytes, width, height, source FROM art WHERE key = ?1",
                [key],
                |r| {
                    Ok(Entry {
                        sha256: r.get(0)?,
                        bytes: r.get::<_, i64>(1)? as u64,
                        width: r.get::<_, Option<i64>>(2)?.map(|v| v as u32),
                        height: r.get::<_, Option<i64>>(3)?.map(|v| v as u32),
                        source: source_from_str(&r.get::<_, String>(4)?),
                    })
                },
            )
            .optional()
            .ok()
            .flatten()?
        };

        let path = self.path_for(&entry.sha256);
        let bytes = std::fs::read(&path).ok().filter(|b| {
            b.len() as u64 == entry.bytes && format!("{:x}", Sha256::digest(b)) == entry.sha256
        });
        let Some(bytes) = bytes else {
            tracing::debug!("cache entry {key} is missing or corrupt; dropping it");
            let _ = self.forget(key);
            return None;
        };

        let _ = self.lock().execute(
            "UPDATE art SET last_access = ?2 WHERE key = ?1",
            rusqlite::params![key, (self.now)()],
        );
        Some((entry, bytes))
    }

    /// Store an image against an album key. Returns its sha256.
    pub fn put(
        &self,
        key: &str,
        bytes: &[u8],
        dimensions: Option<(u32, u32)>,
        source: ArtworkSource,
    ) -> Result<String> {
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let path = self.path_for(&sha256);
        let parent = path.parent().expect("sha path always has a parent");
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;

        // Temp-then-rename inside the same directory, so a reader never sees
        // a partial file and a crash leaves at most a .part to be swept.
        let tmp = parent.join(format!("{sha256}.part"));
        std::fs::write(&tmp, bytes).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, &path)
            .with_context(|| format!("renaming into {}", path.display()))?;

        let now = (self.now)();
        self.lock()
            .execute(
                "INSERT INTO art (key, sha256, bytes, width, height, source, created_at, \
                 last_access) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7) \
                 ON CONFLICT(key) DO UPDATE SET sha256 = ?2, bytes = ?3, width = ?4, \
                 height = ?5, source = ?6, last_access = ?7",
                rusqlite::params![
                    key,
                    sha256,
                    bytes.len() as i64,
                    dimensions.map(|d| d.0 as i64),
                    dimensions.map(|d| d.1 as i64),
                    source_to_str(source),
                    now,
                ],
            )
            .context("recording a cache entry")?;

        // Drop any negative entry: we have an answer now.
        let _ = self
            .lock()
            .execute("DELETE FROM negative WHERE key = ?1", [key]);
        Ok(sha256)
    }

    /// Record that no catalogue had this album, so we stop asking.
    pub fn note_miss(&self, key: &str) -> Result<()> {
        self.lock()
            .execute(
                "INSERT INTO negative (key, created_at) VALUES (?1, ?2) \
                 ON CONFLICT(key) DO UPDATE SET created_at = ?2",
                rusqlite::params![key, (self.now)()],
            )
            .context("recording a negative cache entry")?;
        Ok(())
    }

    /// Whether this album was missed recently enough to skip the network.
    pub fn is_negative(&self, key: &str) -> bool {
        let created: Option<i64> = self
            .lock()
            .query_row(
                "SELECT created_at FROM negative WHERE key = ?1",
                [key],
                |r| r.get(0),
            )
            .optional()
            .ok()
            .flatten();
        match created {
            Some(at) => (self.now)().saturating_sub(at) < self.negative_ttl_secs,
            None => false,
        }
    }

    fn forget(&self, key: &str) -> Result<()> {
        self.lock()
            .execute("DELETE FROM art WHERE key = ?1", [key])?;
        Ok(())
    }

    pub fn total_bytes(&self) -> u64 {
        self.lock()
            .query_row("SELECT COALESCE(SUM(bytes), 0) FROM art", [], |r| {
                r.get::<_, i64>(0)
            })
            .map(|v| v.max(0) as u64)
            .unwrap_or(0)
    }

    pub fn entries(&self) -> u64 {
        self.lock()
            .query_row("SELECT COUNT(*) FROM art", [], |r| r.get::<_, i64>(0))
            .map(|v| v.max(0) as u64)
            .unwrap_or(0)
    }

    /// Expire negatives, evict to `max_bytes`, and delete unreferenced files.
    ///
    /// Run at startup and hourly. The startup run is the one that matters:
    /// it is where a crash's orphans and a power cut's half-written files go.
    pub fn sweep(&self) -> Result<Swept> {
        let now = (self.now)();
        let expired_negatives = self
            .lock()
            .execute(
                "DELETE FROM negative WHERE created_at <= ?1",
                [now - self.negative_ttl_secs],
            )
            .context("expiring negative cache entries")? as u64;
        let mut swept = Swept {
            expired_negatives,
            ..Default::default()
        };

        // LRU eviction. Least recently used first, until we are under budget.
        let mut total = self.total_bytes();
        if total > self.max_bytes {
            let victims: Vec<(String, String, i64)> = {
                let conn = self.lock();
                let mut stmt =
                    conn.prepare("SELECT key, sha256, bytes FROM art ORDER BY last_access ASC")?;
                let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
                rows.collect::<Result<_, _>>()?
            };
            for (key, sha256, bytes) in victims {
                if total <= self.max_bytes {
                    break;
                }
                let _ = std::fs::remove_file(self.path_for(&sha256));
                self.forget(&key)?;
                total = total.saturating_sub(bytes.max(0) as u64);
                swept.evicted += 1;
            }
        }

        swept.orphan_files = self.remove_unreferenced_files()?;
        Ok(swept)
    }

    /// Delete `.part` leftovers and any image the index does not know about.
    fn remove_unreferenced_files(&self) -> Result<u64> {
        let known: std::collections::HashSet<String> = {
            let conn = self.lock();
            let mut stmt = conn.prepare("SELECT sha256 FROM art")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            rows.collect::<Result<_, _>>()?
        };

        let mut removed = 0;
        let Ok(shards) = std::fs::read_dir(&self.dir) else {
            return Ok(0);
        };
        for shard in shards.flatten() {
            if !shard.path().is_dir() {
                continue;
            }
            let Ok(files) = std::fs::read_dir(shard.path()) else {
                continue;
            };
            for file in files.flatten() {
                let path = file.path();
                let stem = path.file_stem().map(|s| s.to_string_lossy().to_string());
                let is_part = path.extension().is_some_and(|e| e == "part");
                let orphan = stem.as_deref().is_none_or(|s| !known.contains(s));
                if is_part || orphan {
                    let _ = std::fs::remove_file(&path);
                    removed += 1;
                }
            }
        }
        Ok(removed)
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn source_to_str(s: ArtworkSource) -> &'static str {
    match s {
        ArtworkSource::Airplay => "airplay",
        ArtworkSource::Itunes => "itunes",
        ArtworkSource::CoverArtArchive => "coverartarchive",
        ArtworkSource::Cache => "cache",
        ArtworkSource::Placeholder => "placeholder",
    }
}

/// The stored source is what the image *came from*, so an unrecognised value
/// degrades to `cache` rather than to a guess.
fn source_from_str(s: &str) -> ArtworkSource {
    match s {
        "itunes" => ArtworkSource::Itunes,
        "coverartarchive" => ArtworkSource::CoverArtArchive,
        "airplay" => ArtworkSource::Airplay,
        _ => ArtworkSource::Cache,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicI64, Ordering};
    use std::sync::Arc;

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("artd-cache-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    fn cache(name: &str, max_bytes: u64) -> (Cache, PathBuf) {
        let dir = scratch(name);
        let c = Cache::open(&dir, max_bytes, 7 * 24 * 3600).unwrap();
        (c, dir)
    }

    #[test]
    fn a_stored_image_comes_back_byte_for_byte() {
        let (c, dir) = cache("roundtrip", 1 << 20);
        let bytes = vec![7u8; 4096];
        let sha = c
            .put("k1", &bytes, Some((3000, 3000)), ArtworkSource::Itunes)
            .unwrap();

        let (entry, got) = c.get("k1").expect("entry missing");
        assert_eq!(got, bytes);
        assert_eq!(entry.sha256, sha);
        assert_eq!(entry.bytes, 4096);
        assert_eq!(entry.width, Some(3000));
        assert_eq!(entry.source, ArtworkSource::Itunes);
        assert!(c.get("nothing").is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn eviction_removes_the_least_recently_used_entry_first() {
        let clock = Arc::new(AtomicI64::new(1_000));
        let dir = scratch("lru");
        let c = {
            let clock = clock.clone();
            Cache::open(&dir, 3_000, 7 * 24 * 3600)
                .unwrap()
                .with_clock(move || clock.load(Ordering::SeqCst))
        };

        for (i, key) in ["a", "b", "c"].into_iter().enumerate() {
            clock.store(1_000 + i as i64, Ordering::SeqCst);
            c.put(key, &vec![i as u8 + 1; 1_000], None, ArtworkSource::Itunes)
                .unwrap();
        }
        assert_eq!(c.total_bytes(), 3_000);

        // Touch "a" so it is no longer the oldest, then overflow the budget.
        clock.store(2_000, Ordering::SeqCst);
        assert!(c.get("a").is_some());
        clock.store(2_001, Ordering::SeqCst);
        c.put("d", &vec![9u8; 1_000], None, ArtworkSource::Itunes)
            .unwrap();

        let swept = c.sweep().unwrap();
        assert_eq!(swept.evicted, 1, "{swept:?}");
        assert!(c.get("b").is_none(), "the least recently used survived");
        assert!(c.get("a").is_some(), "a recently used entry was evicted");
        assert!(c.get("c").is_some());
        assert!(c.get("d").is_some());
        assert!(c.total_bytes() <= 3_000);

        // The evicted entry's file went with it.
        let files: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().is_dir())
            .flat_map(|e| std::fs::read_dir(e.path()).unwrap().flatten())
            .collect();
        assert_eq!(files.len(), 3, "an evicted file was left on disk");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_negative_entry_expires_after_its_ttl() {
        let clock = Arc::new(AtomicI64::new(1_000_000));
        let dir = scratch("negative");
        let c = {
            let clock = clock.clone();
            Cache::open(&dir, 1 << 20, 60)
                .unwrap()
                .with_clock(move || clock.load(Ordering::SeqCst))
        };

        assert!(!c.is_negative("k"), "nothing recorded yet");
        c.note_miss("k").unwrap();
        assert!(c.is_negative("k"));

        clock.store(1_000_059, Ordering::SeqCst);
        assert!(c.is_negative("k"), "expired one second early");
        clock.store(1_000_060, Ordering::SeqCst);
        assert!(!c.is_negative("k"), "still negative past the TTL");

        let swept = c.sweep().unwrap();
        assert_eq!(swept.expired_negatives, 1);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_successful_fetch_clears_the_negative_entry() {
        let (c, dir) = cache("negative-cleared", 1 << 20);
        c.note_miss("k").unwrap();
        assert!(c.is_negative("k"));
        c.put("k", b"bytes", None, ArtworkSource::Itunes).unwrap();
        assert!(!c.is_negative("k"), "a hit did not clear the miss");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_crash_between_the_file_and_the_row_leaves_the_index_consistent() {
        // The ordering that makes this safe: bytes land first, so the worst
        // outcome of dying mid-write is a file nobody references.
        let (c, dir) = cache("crash", 1 << 20);
        c.put("survivor", b"real bytes", None, ArtworkSource::Itunes)
            .unwrap();

        // Simulate the crash: a finished file and a half-written .part, with
        // no index rows for either.
        let orphan_sha = format!("{:x}", Sha256::digest(b"interrupted"));
        let shard = dir.join(&orphan_sha[..2]);
        std::fs::create_dir_all(&shard).unwrap();
        std::fs::write(shard.join(format!("{orphan_sha}.jpg")), b"interrupted").unwrap();
        std::fs::write(shard.join(format!("{orphan_sha}.part")), b"half").unwrap();

        // Reopening must not see them, and a sweep must clean them up.
        drop(c);
        let c = Cache::open(&dir, 1 << 20, 7 * 24 * 3600).unwrap();
        assert_eq!(c.entries(), 1, "an orphan file became an index entry");
        assert_eq!(c.total_bytes(), 10);

        let swept = c.sweep().unwrap();
        assert_eq!(swept.orphan_files, 2, "{swept:?}");
        assert!(c.get("survivor").is_some(), "the committed entry was lost");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_row_whose_file_was_corrupted_is_dropped_rather_than_served() {
        let (c, dir) = cache("corrupt", 1 << 20);
        let sha = c
            .put("k", b"original contents", None, ArtworkSource::Itunes)
            .unwrap();

        std::fs::write(dir.join(&sha[..2]).join(format!("{sha}.jpg")), b"tampered").unwrap();
        assert!(c.get("k").is_none(), "corrupt bytes were served");
        assert_eq!(c.entries(), 0, "the bad row survived");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_index_survives_being_reopened() {
        let dir = scratch("reopen");
        {
            let c = Cache::open(&dir, 1 << 20, 3600).unwrap();
            c.put("k", b"persisted", None, ArtworkSource::CoverArtArchive)
                .unwrap();
            c.note_miss("gone").unwrap();
        }
        let c = Cache::open(&dir, 1 << 20, 3600).unwrap();
        let (entry, bytes) = c.get("k").expect("entry did not survive a reopen");
        assert_eq!(bytes, b"persisted");
        assert_eq!(entry.source, ArtworkSource::CoverArtArchive);
        assert!(c.is_negative("gone"));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
