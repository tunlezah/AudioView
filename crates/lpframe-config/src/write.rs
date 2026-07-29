//! Writing the local override file (DESIGN §7.1).
//!
//! Three properties, in the order they matter:
//!
//! 1. **Sparse.** Only keys the user has actually changed are written, so
//!    "reset to default" is a deletion rather than a guess about what the
//!    default was, and `/etc` stays diffable against the package.
//! 2. **Validated before committing.** The merged result is parsed and run
//!    through [`Config::validate`] first. A value that makes the config
//!    invalid is refused with the reason and the file is not touched — a
//!    device that refuses to boot is not a recoverable state on an appliance
//!    with no keyboard.
//! 3. **Atomic.** Temp file in the same directory, `fsync`, `rename`, then
//!    `fsync` the directory. A half-written file that fails to parse would
//!    fail the service at the next boot, which is the same brick by a slower
//!    route.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::Path;

use crate::{collect_paths, defaults, merge, read_toml, Config, ConfigError, Loaded};

/// Written at the top of every generated override file.
const HEADER: &str = "\
# LP Frame local overrides.
#
# Written by the web interface; only settings changed from
# /etc/lpframe/config.toml appear here. Hand edits survive, but this file is
# rewritten whole on the next change, so comments in it will not.
";

/// The mode the override file is created with. World-readable is fine for
/// the settings themselves, but this file also holds `web.password_hash`,
/// and a hash is worth a little less exposure than nothing.
const LOCAL_MODE: u32 = 0o640;

/// Set dotted keys in the local override, merged into whatever is there.
pub fn set_overrides(
    base: &Path,
    local: &Path,
    updates: &BTreeMap<String, toml::Value>,
) -> Result<Loaded, ConfigError> {
    let edits = updates
        .iter()
        .map(|(k, v)| (k.clone(), Some(v.clone())))
        .collect();
    edit_overrides(base, local, &edits)
}

/// Drop dotted keys from the local override, so they fall back to the base.
pub fn reset_overrides(base: &Path, local: &Path, keys: &[String]) -> Result<Loaded, ConfigError> {
    let edits = keys.iter().map(|k| (k.clone(), None)).collect();
    edit_overrides(base, local, &edits)
}

/// Apply a set of edits to the local override file and reload.
///
/// `Some(value)` sets a key, `None` drops it. Both directions are one
/// primitive because confirm-or-revert has to restore a mixture: some of the
/// keys it is undoing were overridden before, and some were not.
///
/// Returns the configuration as it now reads on disk. On any error the file
/// is left exactly as it was.
pub fn edit_overrides(
    base: &Path,
    local: &Path,
    edits: &BTreeMap<String, Option<toml::Value>>,
) -> Result<Loaded, ConfigError> {
    let known = defaults();
    for key in edits.keys() {
        if !known.contains_key(key) {
            return Err(ConfigError::BadKey(format!(
                "{key:?} is not a setting. Dotted paths only, e.g. \"render.ambient\"."
            )));
        }
    }

    let mut table = read_local(local)?;
    for (key, value) in edits {
        match value {
            Some(v) => insert_path(&mut table, key, v.clone())?,
            None => {
                remove_path(&mut table, key);
            }
        }
    }

    // Validate the merged result, not the override in isolation: the override
    // is sparse, so on its own it says nothing about whether, say,
    // `timeouts.stall` is still shorter than `timeouts.session`.
    let local_value = toml::Value::Table(table.clone());
    let mut merged = read_toml(base)?.unwrap_or_else(|| toml::Value::Table(Default::default()));
    merge(&mut merged, &local_value);
    let config: Config = merged.try_into().map_err(|source| ConfigError::Parse {
        path: local.to_path_buf(),
        source,
    })?;
    config.validate()?;

    let mut text = String::from(HEADER);
    if !table.is_empty() {
        text.push('\n');
        text.push_str(&toml::to_string_pretty(&local_value).map_err(|e| {
            ConfigError::Invalid(format!("the override could not be serialised: {e}"))
        })?);
    }
    write_atomic(local, text.as_bytes(), LOCAL_MODE)?;

    let mut overridden = BTreeSet::new();
    collect_paths(&local_value, String::new(), &mut overridden);
    Ok(Loaded { config, overridden })
}

/// The local override file as a raw table, empty if it does not exist.
///
/// Exposed so a caller can record exactly which keys were overridden and to
/// what, and put that back later — which is what the display confirm-or-revert
/// countdown does when nobody clicks *Keep this*.
pub fn read_local(local: &Path) -> Result<toml::Table, ConfigError> {
    match read_toml(local)? {
        Some(toml::Value::Table(t)) => Ok(t),
        Some(_) => Err(ConfigError::Invalid(format!(
            "{} is not a table",
            local.display()
        ))),
        None => Ok(toml::Table::new()),
    }
}

/// Read one dotted path out of a table.
pub fn get_path<'a>(table: &'a toml::Table, path: &str) -> Option<&'a toml::Value> {
    let mut cursor = table;
    let mut parts = path.split('.').peekable();
    while let Some(part) = parts.next() {
        let value = cursor.get(part)?;
        if parts.peek().is_none() {
            return Some(value);
        }
        cursor = value.as_table()?;
    }
    None
}

fn insert_path(table: &mut toml::Table, path: &str, value: toml::Value) -> Result<(), ConfigError> {
    let parts: Vec<&str> = path.split('.').collect();
    let (last, parents) = parts.split_last().expect("split never yields nothing");

    let mut cursor = table;
    for part in parents {
        let entry = cursor
            .entry(part.to_string())
            .or_insert_with(|| toml::Value::Table(Default::default()));
        cursor = entry.as_table_mut().ok_or_else(|| {
            ConfigError::BadKey(format!(
                "{path:?} runs through {part:?}, which is not a table"
            ))
        })?;
    }
    cursor.insert(last.to_string(), value);
    Ok(())
}

/// Remove a dotted path, pruning tables the removal leaves empty.
///
/// Pruning matters because an empty `[render]` left behind still counts as a
/// hand-written section to anyone reading the file, and the whole point of a
/// sparse override is that its contents are exactly the user's changes.
fn remove_path(table: &mut toml::Table, path: &str) -> bool {
    let Some((head, rest)) = path.split_once('.') else {
        return table.remove(path).is_some();
    };
    let Some(child) = table.get_mut(head).and_then(|v| v.as_table_mut()) else {
        return false;
    };
    let removed = remove_path(child, rest);
    if child.is_empty() {
        table.remove(head);
    }
    removed
}

/// Write a file atomically: temp file alongside, `fsync`, `rename`.
///
/// The directory is `fsync`ed too. Without that the rename can still be lost
/// to a power cut, which on an appliance people switch off at the wall is not
/// a theoretical ordering.
pub fn write_atomic(path: &Path, contents: &[u8], mode: u32) -> Result<(), ConfigError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

    let dir = path.parent().filter(|p| !p.as_os_str().is_empty());
    let wrap = |source: std::io::Error| ConfigError::Write {
        path: path.to_path_buf(),
        source,
    };

    if let Some(dir) = dir {
        std::fs::create_dir_all(dir).map_err(wrap)?;
    }

    let name = path.file_name().unwrap_or_default().to_string_lossy();
    let tmp = match dir {
        Some(d) => d.join(format!(".{name}.tmp.{}", std::process::id())),
        None => Path::new(".").join(format!(".{name}.tmp.{}", std::process::id())),
    };

    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(mode)
        .open(&tmp)
        .map_err(wrap)?;
    // `mode` only applies when the file is created, and a temp file left by a
    // previous crash would keep whatever mode it had.
    file.set_permissions(std::fs::Permissions::from_mode(mode))
        .map_err(wrap)?;
    let write = (|| {
        file.write_all(contents)?;
        file.sync_all()
    })();
    if let Err(e) = write {
        let _ = std::fs::remove_file(&tmp);
        return Err(wrap(e));
    }
    drop(file);

    if let Err(e) = std::fs::rename(&tmp, path) {
        let _ = std::fs::remove_file(&tmp);
        return Err(wrap(e));
    }
    if let Some(dir) = dir {
        // Best effort: some filesystems refuse to open a directory for the
        // fsync, and failing the write after the rename has already landed
        // would be a lie in the other direction.
        if let Ok(handle) = std::fs::File::open(dir) {
            let _ = handle.sync_all();
        }
    }
    Ok(())
}
