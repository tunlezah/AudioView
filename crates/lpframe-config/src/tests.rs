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
fn the_shipped_example_config_matches_the_schema() {
    // docs and code drifting apart is the usual failure here.
    let example = include_str!("../../../provisioning/config.toml");
    let parsed = Config::from_toml(example, Path::new("provisioning/config.toml")).unwrap();
    parsed.validate().unwrap();
}
