use super::*;

fn tmpdir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("lpframe-config-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn defaults_are_valid() {
    Config::default().validate().unwrap();
}

#[test]
fn defaults_round_trip_through_toml() {
    let text = toml::to_string_pretty(&Config::default()).unwrap();
    let parsed = Config::from_toml(&text, Path::new("<test>")).unwrap();
    assert_eq!(parsed, Config::default());
}

#[test]
fn a_missing_file_is_not_an_error() {
    let d = tmpdir("missing");
    let loaded = Config::load(&d.join("nope.toml"), &d.join("also-nope.toml")).unwrap();
    assert_eq!(loaded.config, Config::default());
    assert!(loaded.overridden.is_empty());
}

#[test]
fn a_typo_is_rejected_rather_than_ignored() {
    let err = Config::from_toml("[render]\nkenburns = true\n", Path::new("<test>")).unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
}

#[test]
fn local_overrides_win_per_key_and_are_reported() {
    let d = tmpdir("layer");
    let base = d.join("config.toml");
    let local = d.join("config.local.toml");
    std::fs::write(
        &base,
        r#"
[render]
ambient = false
crossfade = "600ms"

[power.display]
blank_after = "5m"
"#,
    )
    .unwrap();
    std::fs::write(
        &local,
        r#"
[render]
ambient = true

[power.display]
blank_after = "90s"
"#,
    )
    .unwrap();

    let loaded = Config::load(&base, &local).unwrap();
    assert!(loaded.config.render.ambient, "local should win");
    // Untouched sibling keys must survive the merge.
    assert_eq!(loaded.config.render.crossfade, Dur::from_millis(600));
    assert_eq!(loaded.config.power.display.blank_after, Dur::from_secs(90));

    assert_eq!(
        loaded.overridden.iter().cloned().collect::<Vec<_>>(),
        vec![
            "power.display.blank_after".to_string(),
            "render.ambient".to_string()
        ]
    );
}

#[test]
fn arrays_are_replaced_not_appended() {
    let d = tmpdir("arrays");
    let base = d.join("c.toml");
    let local = d.join("l.toml");
    std::fs::write(
        &base,
        "[enrichment]\nsources = [\"itunes\", \"musicbrainz\"]\ncontact = \"a@b.c\"\n",
    )
    .unwrap();
    std::fs::write(&local, "[enrichment]\nsources = [\"itunes\"]\n").unwrap();

    let loaded = Config::load(&base, &local).unwrap();
    assert_eq!(loaded.config.enrichment.sources, vec!["itunes".to_string()]);
}

#[test]
fn validation_rejects_an_unreachable_paused_state() {
    // stall must fire before session, or the device appears to jump straight
    // from playing to dark.
    let mut c = Config::default();
    c.timeouts.stall = Dur::from_secs(90);
    c.timeouts.session = Dur::from_secs(60);
    let err = c.validate().unwrap_err();
    assert!(err.to_string().contains("must be shorter"), "{err}");
}

#[test]
fn validation_rejects_ambient_after_blanking() {
    let mut c = Config::default();
    c.power.display.ambient_after = MaybeDuration::secs(600);
    c.power.display.blank_after = Dur::from_secs(300);
    assert!(c.validate().is_err());
}

#[test]
fn validation_requires_a_contact_for_musicbrainz() {
    // Not on by default, precisely because it cannot work without a contact.
    let mut c = Config::default();
    assert!(!c.enrichment.sources.iter().any(|s| s == "musicbrainz"));

    c.enrichment.sources.push("musicbrainz".into());
    let err = c.validate().unwrap_err();
    assert!(err.to_string().contains("contact"), "{err}");

    c.enrichment.contact = "me@example.com".into();
    c.validate().unwrap();
}

#[test]
fn validation_rejects_bad_rotation_and_dim() {
    let mut c = Config::default();
    c.display.rotation = 45;
    assert!(c.validate().is_err());

    let mut c = Config::default();
    c.render.ambient_dim = 1.5;
    assert!(c.validate().is_err());
}

#[test]
fn validation_rejects_an_unparseable_web_bind() {
    let mut c = Config::default();
    c.web.bind = "lpframe.local:8730".into();
    assert!(c.validate().is_err());

    // ...but not when the interface is off.
    c.web.enabled = false;
    c.validate().unwrap();
}

#[test]
fn validation_rejects_too_few_retained_artwork_revisions() {
    let mut c = Config::default();
    c.ipc.art_retain = 1;
    assert!(c.validate().is_err());
}

#[test]
fn byte_sizes_parse_and_render() {
    #[derive(Deserialize)]
    struct W {
        x: ByteSize,
    }
    for (text, expect) in [
        ("x = \"2GiB\"", 2u64 << 30),
        ("x = \"512MiB\"", 512 << 20),
        ("x = \"1024\"", 1024),
        ("x = 4096", 4096),
    ] {
        let w: W = toml::from_str(text).unwrap();
        assert_eq!(w.x.0, expect, "{text}");
    }
    assert!(toml::from_str::<W>("x = \"2 parsecs\"").is_err());

    let s = toml::to_string(&Cache::default()).unwrap();
    assert!(s.contains("2GiB"), "{s}");
}

#[test]
fn validation_refuses_a_lan_bind_with_authentication_off() {
    let mut c = Config::default();
    c.web.bind = "0.0.0.0:8730".into();
    c.web.auth = false;
    let err = c.validate().unwrap_err().to_string();
    assert!(err.contains("web.auth = false"), "{err}");

    // Loopback is fine unauthenticated: reaching it already means being on
    // the device.
    c.web.bind = "127.0.0.1:8730".into();
    c.validate().unwrap();

    // ...and so is the LAN, once the escape hatch is deliberately set.
    c.web.bind = "0.0.0.0:8730".into();
    c.web.insecure_no_auth = true;
    c.validate().unwrap();
}

#[test]
fn every_schema_key_has_a_dotted_path_and_a_default() {
    let d = defaults();
    for key in ["render.ambient", "web.bind", "power.amp.gpio_line"] {
        assert!(d.contains_key(key), "{key} missing from {:?}", d.keys());
    }
    // The hash is a setting like any other as far as the schema goes; the
    // web layer is what refuses to hand it out.
    assert!(d.contains_key("web.password_hash"));
}

// --- the write path -------------------------------------------------------

fn layered(name: &str) -> (PathBuf, PathBuf) {
    let d = tmpdir(name);
    (d.join("config.toml"), d.join("config.local.toml"))
}

fn edits(pairs: &[(&str, toml::Value)]) -> BTreeMap<String, toml::Value> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}

#[test]
fn a_set_value_survives_a_reload() {
    let (base, local) = layered("set");
    std::fs::write(&base, "[render]\nambient = false\n").unwrap();

    let written = set_overrides(&base, &local, &edits(&[("render.ambient", true.into())])).unwrap();
    assert!(written.config.render.ambient);

    let reloaded = Config::load(&base, &local).unwrap();
    assert!(reloaded.config.render.ambient);
    assert!(reloaded.overridden.contains("render.ambient"));
}

#[test]
fn a_reset_key_falls_back_to_the_base_value() {
    let (base, local) = layered("reset");
    std::fs::write(&base, "[render]\nambient = false\ncrossfade = \"600ms\"\n").unwrap();
    set_overrides(
        &base,
        &local,
        &edits(&[
            ("render.ambient", true.into()),
            ("render.crossfade", "2s".into()),
        ]),
    )
    .unwrap();

    reset_overrides(&base, &local, &["render.ambient".to_string()]).unwrap();

    let reloaded = Config::load(&base, &local).unwrap();
    assert!(!reloaded.config.render.ambient, "back to the base value");
    // The sibling override is untouched.
    assert_eq!(reloaded.config.render.crossfade, Dur::from_secs(2));
    assert_eq!(
        reloaded.overridden.iter().cloned().collect::<Vec<_>>(),
        vec!["render.crossfade".to_string()]
    );
}

#[test]
fn resetting_the_last_key_in_a_table_prunes_the_table() {
    let (base, local) = layered("prune");
    set_overrides(
        &base,
        &local,
        &edits(&[("power.amp.off_delay", "20m".into())]),
    )
    .unwrap();
    reset_overrides(&base, &local, &["power.amp.off_delay".to_string()]).unwrap();

    let text = std::fs::read_to_string(&local).unwrap();
    assert!(!text.contains("power"), "empty tables left behind:\n{text}");
    assert!(read_local(&local).unwrap().is_empty());
}

#[test]
fn an_invalid_change_leaves_the_file_byte_identical() {
    let (base, local) = layered("invalid");
    set_overrides(&base, &local, &edits(&[("timeouts.stall", "10s".into())])).unwrap();
    let before = std::fs::read(&local).unwrap();

    // stall must stay shorter than session, or the paused state is
    // unreachable and the device appears to jump straight to dark.
    let err = set_overrides(&base, &local, &edits(&[("timeouts.stall", "90s".into())]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("must be shorter"), "{err}");

    assert_eq!(std::fs::read(&local).unwrap(), before);
    assert_eq!(
        Config::load(&base, &local).unwrap().config.timeouts.stall,
        Dur::from_secs(10)
    );
}

#[test]
fn a_value_of_the_wrong_type_is_refused_without_writing() {
    let (base, local) = layered("wrong-type");
    let err = set_overrides(
        &base,
        &local,
        &edits(&[("render.ambient", "yes please".into())]),
    )
    .unwrap_err();
    assert!(matches!(err, ConfigError::Parse { .. }), "{err}");
    assert!(!local.exists());
}

#[test]
fn an_unknown_key_is_refused_by_name() {
    let (base, local) = layered("unknown-key");
    let err = set_overrides(&base, &local, &edits(&[("render.ambiant", true.into())]))
        .unwrap_err()
        .to_string();
    assert!(err.contains("ambiant"), "{err}");
    assert!(!local.exists());
}

#[test]
fn edits_can_set_and_drop_keys_in_one_commit() {
    // This is the shape confirm-or-revert needs: some of the keys it puts
    // back were overridden before, and some were not.
    let (base, local) = layered("mixed");
    set_overrides(&base, &local, &edits(&[("display.rotation", 90.into())])).unwrap();

    let mut mixed: BTreeMap<String, Option<toml::Value>> = BTreeMap::new();
    mixed.insert("display.rotation".into(), None);
    mixed.insert("display.mode".into(), Some("1920x1920@60".into()));
    let loaded = edit_overrides(&base, &local, &mixed).unwrap();

    assert_eq!(loaded.config.display.rotation, 0);
    assert_eq!(loaded.config.display.mode, "1920x1920@60");
}

#[test]
fn the_override_file_is_not_world_readable() {
    use std::os::unix::fs::PermissionsExt;
    let (base, local) = layered("mode");
    set_overrides(&base, &local, &edits(&[("render.ambient", true.into())])).unwrap();
    let mode = std::fs::metadata(&local).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o640, "it holds web.password_hash");
}

#[test]
fn no_temporary_files_are_left_behind() {
    let (base, local) = layered("tmp");
    set_overrides(&base, &local, &edits(&[("render.ambient", true.into())])).unwrap();
    let leftovers: Vec<_> = std::fs::read_dir(local.parent().unwrap())
        .unwrap()
        .filter_map(|e| e.ok().map(|e| e.file_name().to_string_lossy().to_string()))
        .filter(|n| n.contains(".tmp."))
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn the_shipped_example_config_matches_the_schema() {
    // docs and code drifting apart is the usual failure here.
    let example = include_str!("../../../provisioning/config.toml");
    let parsed = Config::from_toml(example, Path::new("provisioning/config.toml")).unwrap();
    parsed.validate().unwrap();
}
