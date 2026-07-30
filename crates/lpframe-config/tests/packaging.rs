//! The shipped configuration, the systemd units, the tmpfiles fragment and
//! the installer have to agree about paths, and nothing else checks that.
//!
//! Every assertion here is a mistake that would otherwise be found on a
//! device: a typo in the packaged `config.toml` that silently falls back to a
//! default, a unit whose `ReadWritePaths=` does not cover the socket it is
//! told to open, a binary the installer places somewhere `ExecStart=` is not
//! looking. All of them present as "it comes up but does nothing", which is
//! the most expensive kind of bug to chase from the far side of a room.
//!
//! These are string comparisons against files, deliberately. Parsing the
//! units properly would let a real disagreement pass because both sides
//! parsed to the same wrong thing.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .canonicalize()
        .expect("the workspace root")
}

fn read(rel: &str) -> String {
    let path = repo_root().join(rel);
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

/// The configuration a device actually runs: the shipped file over the
/// schema defaults.
///
/// The path assertions below must use this rather than `defaults()`.
/// `provisioning/config.toml` is free to move the socket, the artwork
/// directory or the cache, and a check against the schema would not notice
/// — which is the whole class of bug this file exists to catch.
fn shipped() -> std::collections::BTreeMap<String, toml::Value> {
    let dir = tempdir("shipped");
    let base = dir.join("config.toml");
    std::fs::copy(repo_root().join("provisioning/config.toml"), &base).unwrap();
    let loaded = lpframe_config::Config::load(&base, &dir.join("absent.toml"))
        .expect("provisioning/config.toml must load");
    lpframe_config::flatten(&loaded.config)
}

/// A string setting as the device would see it.
fn shipped_str(key: &str) -> String {
    shipped()
        .get(key)
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| panic!("{key} is not a string in the shipped config"))
        .to_string()
}

/// Every dotted key set by a TOML document.
fn keys_in(text: &str) -> BTreeSet<String> {
    let value: toml::Value = text.parse().expect("valid TOML");
    let mut out = BTreeSet::new();
    walk(&value, String::new(), &mut out);
    out
}

fn walk(value: &toml::Value, prefix: String, out: &mut BTreeSet<String>) {
    match value {
        toml::Value::Table(table) => {
            for (key, child) in table {
                let path = if prefix.is_empty() {
                    key.clone()
                } else {
                    format!("{prefix}.{key}")
                };
                walk(child, path, out);
            }
        }
        _ => {
            out.insert(prefix);
        }
    }
}

/// The value of a `Key=` line in a systemd unit, first occurrence.
fn unit_value(unit: &str, key: &str) -> Option<String> {
    unit.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .find_map(|line| line.strip_prefix(&format!("{key}=")))
        .map(|v| v.trim().to_string())
}

fn unit_values(unit: &str, key: &str) -> Vec<String> {
    unit.lines()
        .map(str::trim)
        .filter(|line| !line.starts_with('#'))
        .filter_map(|line| line.strip_prefix(&format!("{key}=")))
        .map(|v| v.trim().to_string())
        .collect()
}

// --- the shipped configuration ---------------------------------------------

#[test]
fn the_shipped_config_is_one_the_daemon_accepts() {
    let dir = tempdir("shipped-config");
    let base = dir.join("config.toml");
    std::fs::copy(repo_root().join("provisioning/config.toml"), &base).unwrap();

    let loaded = lpframe_config::Config::load(&base, &dir.join("absent.toml"))
        .expect("provisioning/config.toml must load");
    loaded
        .config
        .validate()
        .expect("provisioning/config.toml must validate");
}

#[test]
fn every_key_in_the_shipped_config_is_a_real_setting() {
    // A typo here does not fail: serde ignores it, the default applies, and
    // the device quietly does something other than what the file says.
    let known = lpframe_config::defaults();
    let shipped = keys_in(&read("provisioning/config.toml"));

    let unknown: Vec<&String> = shipped.iter().filter(|k| !known.contains_key(*k)).collect();
    assert!(
        unknown.is_empty(),
        "provisioning/config.toml sets keys the schema does not have: {unknown:?}"
    );
}

#[test]
fn the_shipped_config_mentions_every_setting_a_user_might_change() {
    // The file is the documentation. `web.password_hash` is the exception:
    // it is machine-written and the API refuses to read or set it.
    let shipped = keys_in(&read("provisioning/config.toml"));
    let missing: Vec<String> = lpframe_config::defaults()
        .keys()
        .filter(|k| *k != "web.password_hash")
        .filter(|k| !shipped.contains(*k))
        .cloned()
        .collect();
    assert!(
        missing.is_empty(),
        "settings exist but are not in provisioning/config.toml, so nobody \
         will discover them: {missing:?}"
    );
}

// --- units against the configuration ---------------------------------------

#[test]
fn the_units_point_at_the_config_the_installer_writes() {
    let install = read("provisioning/install.sh");
    assert!(
        install.contains(r#"CONF_DIR="/etc/lpframe""#),
        "install.sh moved CONF_DIR; the units below hard-code it"
    );
    assert!(
        install.contains(r#"STATE_DIR="/var/lib/lpframe""#),
        "install.sh moved STATE_DIR"
    );

    for unit in ["lpframe-artd.service", "lpframe-lprender.service"] {
        let text = read(&format!("systemd/{unit}"));
        let exec = unit_value(&text, "ExecStart").expect("ExecStart");
        assert!(
            exec.contains("--config /etc/lpframe/config.toml"),
            "{unit} does not read the config the installer writes: {exec}"
        );
        assert!(
            exec.contains("--config-local /var/lib/lpframe/config.local.toml"),
            "{unit} does not read the overrides the web interface writes: {exec}"
        );
    }
}

#[test]
fn the_units_execute_binaries_the_installer_installs() {
    let install = read("provisioning/install.sh");
    let deb = read("provisioning/build-deb.sh");
    assert!(
        install.contains(r#"BIN_DIR="/usr/bin""#),
        "install.sh moved BIN_DIR"
    );

    for (unit, binary) in [
        ("lpframe-artd.service", "artd"),
        ("lpframe-lprender.service", "lprender"),
    ] {
        let text = read(&format!("systemd/{unit}"));
        let exec = unit_value(&text, "ExecStart").expect("ExecStart");
        assert!(
            exec.starts_with(&format!("/usr/bin/{binary} ")),
            "{unit} runs {exec}, which is not where anything puts {binary}"
        );
        assert!(
            install.contains(r#""$from/$binary" "$BIN_DIR/$binary""#) && install.contains(binary),
            "install.sh does not install {binary}"
        );
        assert!(
            deb.contains(r#""$STAGE/usr/bin/$binary""#) && deb.contains(binary),
            "build-deb.sh does not package {binary}"
        );
    }
}

#[test]
fn every_writable_path_a_daemon_needs_is_granted_to_it() {
    // ProtectSystem=strict makes the whole filesystem read-only except what
    // ReadWritePaths= names. A path the daemon opens for writing and that is
    // not listed here fails at runtime, not at start, and only on the code
    // path that touches it.
    let string = shipped_str;

    let artd = read("systemd/lpframe-artd.service");
    let granted = unit_values(&artd, "ReadWritePaths").join(" ");

    for (key, path) in [
        ("ipc.socket", string("ipc.socket")),
        ("ipc.art_dir", string("ipc.art_dir")),
        ("cache.dir", string("cache.dir")),
    ] {
        let parent = Path::new(&path)
            .parent()
            .expect("a path with a parent")
            .to_string_lossy()
            .to_string();
        assert!(
            granted.split_whitespace().any(|allowed| {
                let allowed = allowed.trim_start_matches('-');
                path.starts_with(allowed) || parent.starts_with(allowed)
            }),
            "lpframe-artd cannot write {key} = {path}; ReadWritePaths is {granted:?}"
        );
    }

    // The renderer only reads artwork and talks to the socket, but the
    // socket connection itself needs write on the directory.
    let lprender = read("systemd/lpframe-lprender.service");
    let granted = unit_values(&lprender, "ReadWritePaths").join(" ");
    let socket_dir = Path::new(&string("ipc.socket"))
        .parent()
        .unwrap()
        .to_string_lossy()
        .to_string();
    assert!(
        granted.contains(&socket_dir),
        "lpframe-lprender cannot reach {socket_dir}; ReadWritePaths is {granted:?}"
    );
}

#[test]
fn the_runtime_directory_survives_artd_restarting() {
    // lprender bind-mounts /run/lpframe into its namespace. Without
    // RuntimeDirectoryPreserve, systemd deletes that directory when artd
    // stops and the renderer is left holding an unlinked inode — it
    // reconnects to a socket that will never appear again. The failure is
    // "the panel keeps the last frame forever after an artd restart", which
    // is exactly the property systemd/README.md promises works.
    let artd = read("systemd/lpframe-artd.service");
    assert_eq!(
        unit_value(&artd, "RuntimeDirectory").as_deref(),
        Some("lpframe")
    );
    assert_eq!(
        unit_value(&artd, "RuntimeDirectoryPreserve").as_deref(),
        Some("yes"),
        "removing this breaks the renderer across an artd restart"
    );
}

#[test]
fn tmpfiles_creates_the_directories_the_socket_and_artwork_live_in() {
    let tmpfiles = read("provisioning/lpframe.tmpfiles.conf");

    let created: BTreeSet<&str> = tmpfiles
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('d'))
        .filter_map(|l| l.split_whitespace().nth(1))
        .collect();

    for key in ["ipc.socket", "ipc.art_dir"] {
        let path = shipped_str(key);
        let dir = if key == "ipc.socket" {
            Path::new(&path)
                .parent()
                .unwrap()
                .to_string_lossy()
                .to_string()
        } else {
            path.to_string()
        };
        assert!(
            created.contains(dir.as_str()),
            "nothing creates {dir} for {key}; tmpfiles.d makes {created:?}"
        );
    }

    // Owned by the user the units run as, or artd cannot bind the socket.
    let user = unit_value(&read("systemd/lpframe-artd.service"), "User").expect("User=");
    for line in tmpfiles
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with('d'))
    {
        let fields: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(fields[3], user, "wrong owner in tmpfiles: {line}");
    }
}

#[test]
fn the_placeholder_the_config_names_is_the_one_that_gets_installed() {
    // Without this the renderer logs "no placeholder at …" once per artless
    // track and fades to black, which reads as a fault rather than a choice.
    assert_eq!(
        shipped_str("render.placeholder"),
        "/usr/share/lpframe/placeholder.png"
    );

    let asset = repo_root().join("provisioning/placeholder.png");
    assert!(asset.is_file(), "provisioning/placeholder.png is missing");
    let bytes = std::fs::read(&asset).unwrap();
    assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n", "not a PNG");

    let install = read("provisioning/install.sh");
    assert!(
        install.contains("placeholder.png\" \"$SHARE_DIR/placeholder.png\""),
        "install.sh does not install the placeholder"
    );
    assert!(
        install.contains(r#"SHARE_DIR="/usr/share/lpframe""#),
        "install.sh's SHARE_DIR no longer matches render.placeholder"
    );
    assert!(
        read("provisioning/build-deb.sh").contains("usr/share/lpframe/placeholder.png"),
        "build-deb.sh does not package the placeholder"
    );
}

#[test]
fn the_shairport_ordering_constraint_is_still_declared() {
    // The single easiest thing in this repository to "fix" into a bug:
    // shairport-sync discards metadata written while no reader is attached,
    // so artd must be up first. systemd/README.md explains it at length;
    // this makes deleting it fail rather than lose the first track's art on
    // every boot.
    let artd = read("systemd/lpframe-artd.service");
    assert!(
        unit_values(&artd, "Before")
            .iter()
            .any(|v| v.contains("shairport-sync.service")),
        "lpframe-artd.service no longer orders itself before shairport-sync"
    );
    let shairport = read("systemd/shairport-sync.service");
    assert!(
        unit_values(&shairport, "After")
            .iter()
            .any(|v| v.contains("lpframe-artd.service")),
        "shairport-sync.service no longer waits for lpframe-artd"
    );

    // PrivateTmp on either end would give them different /tmp namespaces and
    // the FIFO would simply never be shared.
    for unit in ["lpframe-artd.service", "shairport-sync.service"] {
        assert_eq!(
            unit_value(&read(&format!("systemd/{unit}")), "PrivateTmp").as_deref(),
            Some("no"),
            "{unit} must share the real /tmp — the metadata FIFO is in it"
        );
    }
    let pipe = shipped_str("device.metadata_pipe");
    assert!(
        pipe.starts_with("/tmp/"),
        "the FIFO moved to {pipe}; the PrivateTmp=no comments above are now wrong"
    );
}

#[test]
fn neither_daemon_requires_the_other() {
    // The snapshot protocol exists so these two restart independently. A
    // Requires= here would turn a five-second renderer reconnect into a
    // restart of the audio path.
    for (unit, other) in [
        ("lpframe-artd.service", "lpframe-lprender.service"),
        ("lpframe-lprender.service", "lpframe-artd.service"),
    ] {
        let text = read(&format!("systemd/{unit}"));
        let requires = unit_values(&text, "Requires").join(" ");
        assert!(
            !requires.contains(other),
            "{unit} Requires={other}, which couples two services the protocol decouples"
        );
    }
}

// --- the installer ----------------------------------------------------------

#[test]
fn the_installer_and_the_package_agree_on_what_a_device_gets() {
    let install = read("provisioning/install.sh");
    let deb = read("provisioning/build-deb.sh");

    for artefact in [
        "lpframe-artd.service",
        "lpframe-lprender.service",
        "lpframe.tmpfiles.conf",
        "placeholder.png",
        "lpframe-rw",
        "lpframe-ro",
    ] {
        assert!(
            install.contains(artefact),
            "install.sh does not install {artefact}"
        );
        assert!(
            deb.contains(artefact),
            "build-deb.sh does not package {artefact}"
        );
    }

    // Deliberately only in the installer: upstream's daemons are upstream's
    // to package, and vendoring them would make us answerable for their
    // security updates.
    for vendored in ["shairport-sync.service", "nqptp.service"] {
        assert!(install.contains(vendored));
        assert!(
            !deb.contains(&format!("systemd/{vendored}")),
            "build-deb.sh packages {vendored}; see the header of that file"
        );
    }
}

#[test]
fn the_free_space_parser_reads_the_extent_and_not_the_header() {
    // `sfdisk --list-free` puts a "Start End Sectors Size" header above the
    // extents. Selecting rows by leading whitespace picks that header, whose
    // third field is the word "Sectors" — which reads as zero free space, so
    // make-writable-partition.sh refuses on every disk in the world and says
    // to go and shrink a filesystem that is already small enough.
    //
    // The awk program is lifted out of the script rather than restated, so
    // this fails if the script's copy changes and this one does not.
    let script = read("provisioning/make-writable-partition.sh");
    let program = script
        .split_once("free_sectors=\"$(sfdisk --list-free \"$disk\" 2>/dev/null | awk '")
        .and_then(|(_, rest)| rest.split_once("')\""))
        .map(|(program, _)| program)
        .expect("the free-space awk program moved; update this test with it");

    // Real output, from an 8 GiB image holding a 64 MiB boot partition and a
    // 3 GiB root — the layout docs/BUILD.md tells you to create.
    let sample = "\
Unpartitioned space /dev/sda: 4.94 GiB, 5300551680 bytes, 10352640 sectors
Units: sectors of 1 * 512 = 512 bytes
Sector size (logical/physical): 512 bytes / 512 bytes

  Start      End  Sectors  Size
6424576 16777215 10352640  4.9G
";
    assert_eq!(run_awk(program, sample).trim(), "10352640");

    // A disk with no gap prints the banner and nothing else.
    let full = "\
Unpartitioned space /dev/sda: 0 B, 0 bytes, 0 sectors
Units: sectors of 1 * 512 = 512 bytes
";
    assert_eq!(run_awk(program, full).trim(), "0");
    assert_eq!(run_awk(program, "").trim(), "0");

    // More than one gap: the last is the only one that can be appended to
    // without moving an existing partition.
    let two_gaps = "\
  Start      End  Sectors  Size
   34567    99999    65433   32M
 6424576 16777215 10352640  4.9G
";
    assert_eq!(run_awk(program, two_gaps).trim(), "10352640");
}

fn run_awk(program: &str, input: &str) -> String {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let mut child = Command::new("awk")
        .arg(program)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("awk");
    child
        .stdin
        .take()
        .unwrap()
        .write_all(input.as_bytes())
        .unwrap();
    let out = child.wait_with_output().unwrap();
    String::from_utf8(out.stdout).unwrap()
}

#[test]
fn the_installer_is_syntactically_valid_bash() {
    // Cheap, and it has caught a stray quote. Shellcheck runs in CI where it
    // is available; this runs everywhere.
    for script in [
        "provisioning/install.sh",
        "provisioning/build-deb.sh",
        "provisioning/make-writable-partition.sh",
        "provisioning/lpframe-rw",
        "provisioning/lpframe-ro",
    ] {
        let path = repo_root().join(script);
        let out = std::process::Command::new("bash")
            .arg("-n")
            .arg(&path)
            .output()
            .expect("bash");
        assert!(
            out.status.success(),
            "{script} does not parse:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );

        let mode = {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&path).unwrap().permissions().mode()
        };
        assert_ne!(mode & 0o111, 0, "{script} is not executable");
    }
}

#[test]
fn the_installer_changes_nothing_on_a_dry_run() {
    // The claim --dry-run makes is only as good as the discipline that every
    // mutation goes through run(). This checks the discipline: a bare
    // privileged command at the start of a line is one somebody forgot.
    let install = read("provisioning/install.sh");
    let offenders: Vec<(usize, &str)> = install
        .lines()
        .enumerate()
        .map(|(i, l)| (i + 1, l.trim()))
        .filter(|(_, l)| {
            [
                "apt-get ",
                "useradd ",
                "usermod ",
                "groupadd ",
                "systemctl ",
                "install -",
            ]
            .iter()
            .any(|cmd| l.starts_with(cmd))
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "these mutate the system without going through run(), so --dry-run \
         would perform them: {offenders:?}"
    );
}

fn tempdir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lpframe-packaging-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
