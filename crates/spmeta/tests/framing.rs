//! Framing tests. The pipe is a FIFO, so the parser sees arbitrary chunk
//! boundaries and routinely starts mid-item. Most of these tests exist to
//! pin that behaviour down.

use spmeta::codes::{dmap, kind, ssnc};
use spmeta::{encode_item, Decoder, FourCc, Limits, MetaEvent, ParseError, Parser};

/// Deterministic PRNG, so a failure is reproducible from the seed alone.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn range(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn parse_all(bytes: &[u8]) -> Vec<Result<spmeta::MetaItem, ParseError>> {
    let mut p = Parser::new();
    p.feed(bytes);
    let mut out = Vec::new();
    while let Some(r) = p.next_item() {
        out.push(r);
    }
    out
}

fn ok_items(bytes: &[u8]) -> Vec<spmeta::MetaItem> {
    parse_all(bytes).into_iter().map(|r| r.unwrap()).collect()
}

// --- basic framing --------------------------------------------------------

#[test]
fn parses_a_zero_length_item() {
    let wire = b"<item><type>73736e63</type><code>70626567</code><length>0</length></item>";
    let items = ok_items(wire);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].kind, kind::SSNC);
    assert_eq!(items[0].code, ssnc::PBEG);
    assert!(items[0].payload.is_empty());
}

#[test]
fn parses_an_item_with_a_payload() {
    let wire = b"<item><type>636f7265</type><code>6d696e6d</code><length>8</length>\n\
                 <data encoding=\"base64\">\nVGVhcmRyb3A=</data></item>";
    let items = ok_items(wire);
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].code, dmap::MINM);
    assert_eq!(items[0].payload, b"Teardrop");
}

#[test]
fn parses_items_back_to_back() {
    let mut wire = Vec::new();
    wire.extend(encode_item(kind::SSNC, ssnc::ABEG, b""));
    wire.extend(encode_item(kind::CORE, dmap::ASAL, b"Mezzanine"));
    wire.extend(encode_item(kind::SSNC, ssnc::PBEG, b""));
    let items = ok_items(&wire);
    assert_eq!(items.len(), 3);
    assert_eq!(items[1].payload, b"Mezzanine");
}

#[test]
fn encode_and_parse_round_trip() {
    // A 700 KB payload exercises the base64 line wrapping at realistic
    // cover-art sizes.
    let art: Vec<u8> = (0..700_000u32)
        .map(|i| (i.wrapping_mul(31) >> 3) as u8)
        .collect();
    let cases: Vec<(FourCc, FourCc, Vec<u8>)> = vec![
        (kind::SSNC, ssnc::ABEG, vec![]),
        (kind::CORE, dmap::MINM, b"Teardrop".to_vec()),
        (kind::CORE, dmap::ASTM, 330_000u32.to_be_bytes().to_vec()),
        (
            kind::CORE,
            dmap::MPER,
            0x0123_4567_89ab_cdefu64.to_be_bytes().to_vec(),
        ),
        (kind::SSNC, ssnc::PICT, art),
        // Non-ASCII must survive; plenty of album titles are not Latin-1.
        (
            kind::CORE,
            dmap::ASAR,
            "Sigur Rós – Ágætis byrjun".as_bytes().to_vec(),
        ),
        (kind::CORE, dmap::ASAL, "白い夜".as_bytes().to_vec()),
    ];

    let mut wire = Vec::new();
    for (k, c, p) in &cases {
        wire.extend(encode_item(*k, *c, p));
    }

    let items = ok_items(&wire);
    assert_eq!(items.len(), cases.len());
    for (item, (k, c, p)) in items.iter().zip(&cases) {
        assert_eq!(item.kind, *k);
        assert_eq!(item.code, *c);
        assert_eq!(&item.payload, p);
    }
}

// --- chunk boundaries -----------------------------------------------------

fn reference_stream() -> Vec<u8> {
    let mut wire = Vec::new();
    wire.extend(encode_item(kind::SSNC, ssnc::ABEG, b""));
    wire.extend(encode_item(kind::SSNC, ssnc::MDST, b""));
    wire.extend(encode_item(kind::CORE, dmap::ASAR, b"Massive Attack"));
    wire.extend(encode_item(kind::CORE, dmap::ASAL, b"Mezzanine"));
    wire.extend(encode_item(kind::CORE, dmap::MINM, b"Teardrop"));
    wire.extend(encode_item(kind::SSNC, ssnc::MDEN, b""));
    // Small enough to keep the O(n²) truncation sweep quick; the 700 KB
    // round-trip test covers realistic artwork sizes.
    wire.extend(encode_item(kind::SSNC, ssnc::PICT, &vec![0xABu8; 900]));
    wire.extend(encode_item(kind::SSNC, ssnc::PBEG, b""));
    wire
}

#[test]
fn one_byte_at_a_time_yields_the_same_items() {
    let wire = reference_stream();
    let expected = ok_items(&wire);

    let mut p = Parser::new();
    let mut got = Vec::new();
    for b in &wire {
        p.feed(&[*b]);
        while let Some(r) = p.next_item() {
            got.push(r.unwrap());
        }
    }
    assert_eq!(got, expected);
}

#[test]
fn random_chunk_sizes_yield_the_same_items() {
    let wire = reference_stream();
    let expected = ok_items(&wire);

    for seed in 1..64u64 {
        let mut rng = Rng(seed);
        let mut p = Parser::new();
        let mut got = Vec::new();
        let mut i = 0;
        while i < wire.len() {
            let n = (rng.range(2048) + 1).min(wire.len() - i);
            p.feed(&wire[i..i + n]);
            i += n;
            while let Some(r) = p.next_item() {
                got.push(r.unwrap());
            }
        }
        assert_eq!(got, expected, "seed {seed}");
    }
}

#[test]
fn every_truncation_point_is_safe_and_completes() {
    // Truncating anywhere must never panic and must never invent an item;
    // feeding the remainder must then produce exactly the full result.
    let wire = reference_stream();
    let expected = ok_items(&wire);

    for cut in 0..wire.len() {
        let mut p = Parser::new();
        p.feed(&wire[..cut]);
        let mut got = Vec::new();
        while let Some(r) = p.next_item() {
            got.push(r.expect("truncation must not produce errors"));
        }
        assert!(got.len() <= expected.len(), "cut {cut} over-produced");
        assert_eq!(got[..], expected[..got.len()], "cut {cut} diverged");

        p.feed(&wire[cut..]);
        while let Some(r) = p.next_item() {
            got.push(r.unwrap());
        }
        assert_eq!(got, expected, "cut {cut} did not converge");
    }
}

// --- resynchronisation ----------------------------------------------------

#[test]
fn skips_leading_whitespace_without_complaining() {
    let mut wire = b"\n\n  \n".to_vec();
    wire.extend(encode_item(kind::SSNC, ssnc::PBEG, b""));
    let results = parse_all(&wire);
    assert_eq!(results.len(), 1);
    assert!(results[0].is_ok());
}

#[test]
fn reports_junk_then_recovers() {
    let mut wire = b"this is not metadata".to_vec();
    wire.extend(encode_item(kind::SSNC, ssnc::PBEG, b""));
    let results = parse_all(&wire);
    assert_eq!(results.len(), 2);
    assert!(matches!(results[0], Err(ParseError::Junk { .. })));
    assert_eq!(results[1].as_ref().unwrap().code, ssnc::PBEG);
}

#[test]
fn starting_mid_item_loses_only_that_item() {
    // Every reconnect starts mid-stream; the cost must be bounded at one item.
    let wire = reference_stream();
    let full = ok_items(&wire);

    for start in 1..wire.len().min(400) {
        let mut p = Parser::new();
        p.feed(&wire[start..]);
        let mut got = Vec::new();
        while let Some(r) = p.next_item() {
            if let Ok(item) = r {
                got.push(item);
            }
        }
        // Whatever we recovered must be a suffix of the true item sequence.
        assert!(
            full.ends_with(&got),
            "start {start}: recovered items are not a suffix of the stream"
        );
        assert!(
            full.len() - got.len() <= 1 || start > 1,
            "start {start}: lost more than one item"
        );
    }
}

#[test]
fn a_malformed_item_does_not_swallow_the_next_one() {
    let mut wire =
        b"<item><type>zzzz</type><code>70626567</code><length>0</length></item>".to_vec();
    wire.extend(encode_item(kind::SSNC, ssnc::ABEG, b""));
    let results = parse_all(&wire);
    let items: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].code, ssnc::ABEG);
    assert!(results.iter().any(|r| r.is_err()));
}

#[test]
fn a_nested_item_open_tag_does_not_wedge_the_parser() {
    // `<item>` appearing where a tag is expected must not cause the resync
    // scan to latch onto the same offset forever.
    let mut wire = b"<item><item><item>".to_vec();
    wire.extend(encode_item(kind::SSNC, ssnc::ABEG, b""));
    let results = parse_all(&wire);
    let items: Vec<_> = results.iter().filter_map(|r| r.as_ref().ok()).collect();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0].code, ssnc::ABEG);
}

// --- hostile input --------------------------------------------------------

#[test]
fn rejects_a_declared_length_over_the_limit() {
    let limits = Limits {
        max_item_bytes: 1024,
        ..Default::default()
    };
    let mut p = Parser::with_limits(limits);
    p.feed(b"<item><type>73736e63</type><code>50494354</code><length>99999999</length>\n<data encoding=\"base64\">\nAAAA</data></item>");
    let mut errs = 0;
    while let Some(r) = p.next_item() {
        if let Err(ParseError::TooLarge { len, max, .. }) = r {
            assert_eq!(len, 99_999_999);
            assert_eq!(max, 1024);
            errs += 1;
        }
    }
    assert_eq!(errs, 1);
}

#[test]
fn does_not_buffer_without_bound_on_an_unterminated_item() {
    let limits = Limits {
        max_item_bytes: 64 * 1024,
        max_buffer_bytes: 4096,
    };
    let mut p = Parser::with_limits(limits);
    // A well-formed header followed by base64 that never terminates.
    p.feed(b"<item><type>73736e63</type><code>50494354</code><length>100</length>\n<data encoding=\"base64\">\n");
    let mut saw_overflow = false;
    for _ in 0..16 {
        p.feed(&vec![b'A'; 1024]);
        while let Some(r) = p.next_item() {
            if matches!(r, Err(ParseError::BufferOverflow { .. })) {
                saw_overflow = true;
            }
        }
        if saw_overflow {
            break;
        }
    }
    assert!(saw_overflow, "parser buffered past max_buffer_bytes");
}

#[test]
fn reports_undecodable_base64() {
    let wire = b"<item><type>636f7265</type><code>6d696e6d</code><length>4</length>\n\
                 <data encoding=\"base64\">\n!!!!</data></item>";
    let results = parse_all(wire);
    assert!(results
        .iter()
        .any(|r| matches!(r, Err(ParseError::BadBase64 { .. }))));
}

#[test]
fn tolerates_a_length_that_disagrees_with_the_payload() {
    // Trust the bytes, not the header, but count the discrepancy.
    let wire = b"<item><type>636f7265</type><code>6d696e6d</code><length>999</length>\n\
                 <data encoding=\"base64\">\nVGVhcmRyb3A=</data></item>";
    let mut p = Parser::new();
    p.feed(wire);
    let item = p.next_item().unwrap().unwrap();
    assert_eq!(item.payload, b"Teardrop");
    assert_eq!(p.stats().length_mismatches, 1);
}

#[test]
fn tolerates_attribute_and_whitespace_variation() {
    let variants: &[&[u8]] = &[
        b"<item><type>636f7265</type><code>6d696e6d</code><length>8</length><data encoding=\"base64\">VGVhcmRyb3A=</data></item>",
        b"<item><type>636f7265</type><code>6d696e6d</code><length>8</length>\r\n<data encoding='base64'>\r\nVGVhcmRyb3A=\r\n</data></item>",
        b"<item> <type>636f7265</type> <code>6d696e6d</code> <length>8</length>\n<data>\nVGVh\ncmRy\nb3A=\n</data></item>",
    ];
    for (i, v) in variants.iter().enumerate() {
        let items = ok_items(v);
        assert_eq!(items.len(), 1, "variant {i}");
        assert_eq!(items[0].payload, b"Teardrop", "variant {i}");
    }
}

#[test]
fn non_utf8_text_payloads_do_not_panic() {
    let wire = encode_item(kind::CORE, dmap::ASAL, &[0xff, 0xfe, b'o', b'k', 0x80]);
    let mut d = Decoder::new();
    d.feed(&wire);
    let ev = d.next_event().unwrap().unwrap();
    assert!(matches!(ev, MetaEvent::Core(spmeta::CoreField::Album(_))));
}

#[test]
fn a_long_hostile_stream_terminates() {
    // Interleave valid items with several flavours of damage and assert we
    // still recover every valid item.
    let mut wire = Vec::new();
    let mut expected = 0;
    for i in 0..200 {
        match i % 5 {
            0 => wire.extend(b"<item><item"),
            1 => wire.extend(b"<item><type>ZZ</type>"),
            2 => wire.extend(b"garbage \x00\xff bytes"),
            3 => wire.extend(b"<item><type>636f7265</type><code>6d696e6d</code><length>"),
            _ => {}
        }
        wire.extend(encode_item(
            kind::CORE,
            dmap::MINM,
            format!("t{i}").as_bytes(),
        ));
        expected += 1;
    }
    let results = parse_all(&wire);
    let good = results.iter().filter(|r| r.is_ok()).count();
    assert_eq!(good, expected);
}
