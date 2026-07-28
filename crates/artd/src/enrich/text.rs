//! Normalisation and fuzzy matching for the text gate (DESIGN §5.4 ②).
//!
//! This is the gate that selects a candidate at all; the perceptual gate only
//! authorises the swap. Getting it wrong in the permissive direction puts the
//! wrong record on the wall, which is the single most visible failure this
//! product has.

/// Artist similarity a candidate must reach.
pub const ARTIST_THRESHOLD: f64 = 0.90;
/// Album similarity a candidate must reach.
pub const ALBUM_THRESHOLD: f64 = 0.85;
/// Above this on *both* fields the album is unambiguous, and the perceptual
/// gate is bypassed unless strictness is `strict` (DESIGN §5.4 ⑤).
pub const NEAR_EXACT_THRESHOLD: f64 = 0.97;

/// Words that mark a qualifier as an edition rather than part of the title.
///
/// Stripping *every* parenthetical would be simpler and is wrong: "Greatest
/// Hits (Volume 2)" and "Greatest Hits" are different records, and so are
/// "Kid A" and "Kid A Mnesia". Only qualifiers drawn from this vocabulary are
/// removed, which costs us the occasional unrecognised edition suffix — a
/// missed upgrade, not a wrong one.
const EDITION_WORDS: &[&str] = &[
    "deluxe",
    "remaster",
    "remastered",
    "edition",
    "explicit",
    "clean",
    "bonus",
    "anniversary",
    "expanded",
    "reissue",
    "special",
    "collectors",
    "collector",
    "limited",
    "original recording",
    "digitally remastered",
    "single version",
    "album version",
    "mono version",
    "stereo version",
    "the complete",
];

/// Lowercase, de-accent, drop edition qualifiers, collapse punctuation.
///
/// The output is only ever compared against another output of this function,
/// so it does not need to be pretty — it needs to be stable and to make the
/// same two records normalise to the same string.
pub fn normalise(s: &str) -> String {
    let folded: String = s.chars().flat_map(fold_char).collect();
    let stripped = strip_qualifiers(&folded);

    let mut out = String::with_capacity(stripped.len());
    let mut space = false;
    for c in stripped.chars() {
        if c.is_alphanumeric() {
            out.extend(c.to_lowercase());
            space = false;
        } else if !out.is_empty() && !space {
            // Every run of punctuation or whitespace becomes one space, so
            // "AC/DC", "AC-DC" and "AC DC" agree.
            out.push(' ');
            space = true;
        }
    }
    out.trim_end().to_string()
}

/// Remove bracketed groups and trailing dash-suffixes that name an edition.
fn strip_qualifiers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut depth = 0usize;
    let mut group = String::new();

    for c in s.chars() {
        match c {
            '(' | '[' | '{' => {
                depth += 1;
                if depth == 1 {
                    group.clear();
                    continue;
                }
            }
            ')' | ']' | '}' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    // Keep a qualifier that is part of the title; a bracketed
                    // subtitle is as load-bearing as an unbracketed one.
                    if !is_edition(&group) {
                        out.push(' ');
                        out.push_str(&group);
                    }
                    continue;
                }
            }
            _ => {}
        }
        if depth > 0 {
            group.push(c);
        } else {
            out.push(c);
        }
    }
    // An unterminated bracket means the string was truncated somewhere; keep
    // what we have rather than discarding the tail.
    if depth > 0 {
        out.push_str(&group);
    }

    // " - Remastered 2011" and friends. Only the last segment is considered:
    // a dash inside the title itself ("Songs - Ohia") must survive.
    if let Some(i) = out.rfind(" - ") {
        if is_edition(&out[i + 3..]) {
            out.truncate(i);
        }
    }
    out
}

fn is_edition(s: &str) -> bool {
    let lower = s.to_lowercase();
    EDITION_WORDS.iter().any(|w| lower.contains(w))
}

/// Fold one character to its unaccented ASCII equivalent.
///
/// A table rather than Unicode NFD plus combining-mark removal: NFD does not
/// decompose ø, đ, ł or ß at all, so a table would be needed regardless, and
/// this way the crate has no Unicode data dependency. Coverage is Latin-1 and
/// Latin Extended-A, which is every artist name we can realistically expect
/// from a catalogue that answers in a Latin script. Anything outside it falls
/// through unchanged and is compared as-is, which is correct: two Japanese
/// titles still match each other.
fn fold_char(c: char) -> impl Iterator<Item = char> {
    let replacement: &str = match c {
        'À'..='Å' | 'à'..='å' | 'Ā' | 'ā' | 'Ă' | 'ă' | 'Ą' | 'ą' => "a",
        'Æ' | 'æ' => "ae",
        'Ç' | 'ç' | 'Ć' | 'ć' | 'Ĉ' | 'ĉ' | 'Ċ' | 'ċ' | 'Č' | 'č' => "c",
        'Ď' | 'ď' | 'Đ' | 'đ' | 'Ð' | 'ð' => "d",
        'È'..='Ë' | 'è'..='ë' | 'Ē' | 'ē' | 'Ĕ' | 'ĕ' | 'Ė' | 'ė' | 'Ę' | 'ę' | 'Ě' | 'ě' => {
            "e"
        }
        'Ĝ' | 'ĝ' | 'Ğ' | 'ğ' | 'Ġ' | 'ġ' | 'Ģ' | 'ģ' => "g",
        'Ĥ' | 'ĥ' | 'Ħ' | 'ħ' => "h",
        'Ì'..='Ï' | 'ì'..='ï' | 'Ĩ' | 'ĩ' | 'Ī' | 'ī' | 'Ĭ' | 'ĭ' | 'Į' | 'į' | 'İ' | 'ı' => {
            "i"
        }
        'Ĵ' | 'ĵ' => "j",
        'Ķ' | 'ķ' => "k",
        'Ĺ' | 'ĺ' | 'Ļ' | 'ļ' | 'Ľ' | 'ľ' | 'Ŀ' | 'ŀ' | 'Ł' | 'ł' => "l",
        'Ñ' | 'ñ' | 'Ń' | 'ń' | 'Ņ' | 'ņ' | 'Ň' | 'ň' | 'ŉ' | 'Ŋ' | 'ŋ' => "n",
        'Ò'..='Ö' | 'ò'..='ö' | 'Ø' | 'ø' | 'Ō' | 'ō' | 'Ŏ' | 'ŏ' | 'Ő' | 'ő' => "o",
        'Œ' | 'œ' => "oe",
        'Ŕ' | 'ŕ' | 'Ŗ' | 'ŗ' | 'Ř' | 'ř' => "r",
        'Ś' | 'ś' | 'Ŝ' | 'ŝ' | 'Ş' | 'ş' | 'Š' | 'š' => "s",
        'ß' => "ss",
        'Ţ' | 'ţ' | 'Ť' | 'ť' | 'Ŧ' | 'ŧ' => "t",
        'Ù'..='Ü'
        | 'ù'..='ü'
        | 'Ũ'
        | 'ũ'
        | 'Ū'
        | 'ū'
        | 'Ŭ'
        | 'ŭ'
        | 'Ů'
        | 'ů'
        | 'Ű'
        | 'ű'
        | 'Ų'
        | 'ų' => "u",
        'Ŵ' | 'ŵ' => "w",
        'Ý' | 'ý' | 'ÿ' | 'Ŷ' | 'ŷ' | 'Ÿ' => "y",
        'Ź' | 'ź' | 'Ż' | 'ż' | 'Ž' | 'ž' => "z",
        'Þ' | 'þ' => "th",
        '&' => " and ",
        // Curly quotes and dashes, which catalogues and senders disagree on.
        '\u{2018}' | '\u{2019}' | '\u{201C}' | '\u{201D}' => "'",
        '\u{2013}' | '\u{2014}' => "-",
        _ => return Folded::Same(Some(c)),
    };
    Folded::Expanded(replacement.chars())
}

enum Folded {
    Same(Option<char>),
    Expanded(std::str::Chars<'static>),
}

impl Iterator for Folded {
    type Item = char;
    fn next(&mut self) -> Option<char> {
        match self {
            Folded::Same(c) => c.take(),
            Folded::Expanded(it) => it.next(),
        }
    }
}

/// Jaro-Winkler similarity of two already-normalised strings, in `0..=1`.
///
/// Winkler's prefix bonus is what makes this the right metric for catalogue
/// text: the informative part of an artist or album name is at the front, and
/// the noise ("…and the Bad Seeds", a trailing edition we failed to strip) is
/// at the back.
pub fn jaro_winkler(a: &str, b: &str) -> f64 {
    let j = jaro(a, b);
    // Winkler's own condition. Below it the strings are different enough that
    // a shared prefix is coincidence, and boosting would only blur the gate.
    if j <= 0.7 {
        return j;
    }
    let (ac, bc): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let prefix = ac
        .iter()
        .zip(bc.iter())
        .take(4)
        .take_while(|(x, y)| x == y)
        .count() as f64;
    j + prefix * 0.1 * (1.0 - j)
}

fn jaro(a: &str, b: &str) -> f64 {
    let (ac, bc): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if ac.is_empty() && bc.is_empty() {
        return 1.0;
    }
    if ac.is_empty() || bc.is_empty() {
        return 0.0;
    }

    let window = (ac.len().max(bc.len()) / 2).saturating_sub(1);
    let mut a_matched = vec![false; ac.len()];
    let mut b_matched = vec![false; bc.len()];
    let mut matches = 0usize;

    for (i, ca) in ac.iter().enumerate() {
        let lo = i.saturating_sub(window);
        let hi = (i + window + 1).min(bc.len());
        for j in lo..hi {
            if !b_matched[j] && bc[j] == *ca {
                a_matched[i] = true;
                b_matched[j] = true;
                matches += 1;
                break;
            }
        }
    }
    if matches == 0 {
        return 0.0;
    }

    let mut transpositions = 0usize;
    let mut k = 0usize;
    for (i, m) in a_matched.iter().enumerate() {
        if !m {
            continue;
        }
        while !b_matched[k] {
            k += 1;
        }
        if ac[i] != bc[k] {
            transpositions += 1;
        }
        k += 1;
    }

    let m = matches as f64;
    (m / ac.len() as f64 + m / bc.len() as f64 + (m - transpositions as f64 / 2.0) / m) / 3.0
}

/// Jaro-Winkler with a veto on disagreeing numbers.
///
/// Jaro-Winkler's blind spot is a short token appended to a long string:
/// "trainspotting" against "trainspotting 2" scores 0.97, and "led zeppelin
/// ii" against "led zeppelin iii" scores higher still. Those are different
/// records, and on a wall they are an obviously wrong one. A volume number is
/// never a typo, so a disagreement about numbers is decisive rather than a
/// small penalty.
///
/// The cost is a missed upgrade when a catalogue writes "Vol. 2" and the
/// sender writes "Volume Two" — the failure lands on the safe side.
pub fn similarity(a: &str, b: &str) -> f64 {
    if numbers(a) != numbers(b) {
        return 0.0;
    }
    jaro_winkler(a, b)
}

/// Numeric and roman-numeral tokens, in order.
fn numbers(s: &str) -> Vec<&str> {
    s.split(' ')
        .filter(|t| {
            !t.is_empty()
                && (t.chars().all(|c| c.is_ascii_digit())
                    || t.chars().all(|c| "ivxlcdm".contains(c)))
        })
        .collect()
}

/// How well a candidate's artist and album match what is playing.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Scores {
    pub artist: f64,
    pub album: f64,
}

impl Scores {
    pub fn compare(
        want_artist: &str,
        want_album: &str,
        got_artist: &str,
        got_album: &str,
    ) -> Scores {
        Scores {
            artist: similarity(&normalise(want_artist), &normalise(got_artist)),
            album: similarity(&normalise(want_album), &normalise(got_album)),
        }
    }

    pub fn passes(&self) -> bool {
        self.artist >= ARTIST_THRESHOLD && self.album >= ALBUM_THRESHOLD
    }

    /// Unambiguous enough to trust without a picture (DESIGN §5.4 ⑤ bypass).
    pub fn near_exact(&self) -> bool {
        self.artist >= NEAR_EXACT_THRESHOLD && self.album >= NEAR_EXACT_THRESHOLD
    }
}

impl std::fmt::Display for Scores {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "artist {:.2} album {:.2}", self.artist, self.album)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalisation_folds_case_accents_and_punctuation() {
        let cases = [
            ("Sigur Rós", "sigur ros"),
            ("Motörhead", "motorhead"),
            ("Beyoncé", "beyonce"),
            ("Björk", "bjork"),
            ("AC/DC", "ac dc"),
            ("Simon & Garfunkel", "simon and garfunkel"),
            ("  Wu-Tang   Clan  ", "wu tang clan"),
            ("Kraftwerk", "kraftwerk"),
            ("Blue Öyster Cult", "blue oyster cult"),
            ("Mötley Crüe", "motley crue"),
            ("Godspeed You! Black Emperor", "godspeed you black emperor"),
            ("Anaïs Mitchell", "anais mitchell"),
        ];
        for (input, want) in cases {
            assert_eq!(normalise(input), want, "normalising {input:?}");
        }
    }

    #[test]
    fn edition_qualifiers_are_stripped_but_real_subtitles_are_not() {
        let cases = [
            ("Nevermind (Deluxe Edition)", "nevermind"),
            ("Rumours [Explicit]", "rumours"),
            ("OK Computer - Remastered 2011", "ok computer"),
            ("Abbey Road (2019 Remaster)", "abbey road"),
            ("Blue (Bonus Track Version)", "blue"),
            (
                "Definitely Maybe (Deluxe Edition Remastered)",
                "definitely maybe",
            ),
            // Not editions: these change which record is meant.
            ("Greatest Hits (Volume 2)", "greatest hits volume 2"),
            ("Music From Big Pink", "music from big pink"),
            ("Songs - Ohia", "songs ohia"),
            ("Zaireeka (Disc 1)", "zaireeka disc 1"),
        ];
        for (input, want) in cases {
            assert_eq!(normalise(input), want, "normalising {input:?}");
        }
    }

    #[test]
    fn jaro_winkler_matches_its_published_reference_values() {
        // The canonical examples, so a rewrite of the metric is caught.
        for (a, b, want) in [
            ("martha", "marhta", 0.961),
            ("dixon", "dicksonx", 0.813),
            ("dwayne", "duane", 0.840),
            ("", "", 1.0),
            ("abc", "abc", 1.0),
            ("abc", "xyz", 0.0),
        ] {
            let got = jaro_winkler(a, b);
            assert!(
                (got - want).abs() < 0.002,
                "{a}/{b}: got {got}, want {want}"
            );
        }
    }

    /// Real-world pairs and what the gate must decide about each.
    #[test]
    fn the_text_gate_accepts_matches_and_rejects_near_misses() {
        struct Case {
            playing: (&'static str, &'static str),
            candidate: (&'static str, &'static str),
            accept: bool,
            why: &'static str,
        }
        let cases = [
            Case {
                playing: ("Massive Attack", "Mezzanine"),
                candidate: ("Massive Attack", "Mezzanine"),
                accept: true,
                why: "exact",
            },
            Case {
                playing: ("Massive Attack", "Mezzanine"),
                candidate: ("Massive Attack", "Mezzanine (2019 Remaster)"),
                accept: true,
                why: "a remaster of the same record",
            },
            Case {
                playing: ("Sigur Ros", "Ágætis byrjun"),
                candidate: ("Sigur Rós", "Agaetis byrjun"),
                accept: true,
                why: "the sender and the catalogue disagree about accents",
            },
            Case {
                playing: ("Beyoncé", "Lemonade"),
                candidate: ("Beyonce", "Lemonade [Explicit]"),
                accept: true,
                why: "explicit tag only",
            },
            Case {
                playing: ("The Beatles", "Abbey Road"),
                candidate: ("The Beatles", "Revolver"),
                accept: false,
                why: "a different album by the same artist",
            },
            Case {
                playing: ("Queen", "Greatest Hits"),
                candidate: ("Fleetwood Mac", "Greatest Hits"),
                accept: false,
                why: "the same compilation title by another artist",
            },
            Case {
                playing: ("Radiohead", "Kid A"),
                candidate: ("Radiohead", "Amnesiac"),
                accept: false,
                why: "adjacent album, similar length title",
            },
            Case {
                playing: ("Nirvana", "Nevermind"),
                candidate: ("Nirvana", "Nevermind (Deluxe Edition)"),
                accept: true,
                why: "deluxe edition of the same record",
            },
            Case {
                playing: ("Bob Dylan", "Blood on the Tracks"),
                candidate: ("Bob Dylan", "Blonde on Blonde"),
                accept: false,
                why: "same artist, alliterative neighbour",
            },
            Case {
                playing: ("Daft Punk", "Discovery"),
                candidate: ("Daft Punk", "Homework"),
                accept: false,
                why: "same artist, different album",
            },
            Case {
                playing: ("Air", "Moon Safari"),
                candidate: ("AIR", "Moon Safari"),
                accept: true,
                why: "case only",
            },
            Case {
                playing: ("Simon & Garfunkel", "Bookends"),
                candidate: ("Simon and Garfunkel", "Bookends"),
                accept: true,
                why: "ampersand spelled out",
            },
            Case {
                playing: ("Various Artists", "Trainspotting"),
                candidate: ("Various Artists", "Trainspotting 2"),
                accept: false,
                why: "a sequel soundtrack is a different record",
            },
            Case {
                playing: ("Led Zeppelin", "Led Zeppelin II"),
                candidate: ("Led Zeppelin", "Led Zeppelin III"),
                accept: false,
                why: "consecutive numbered albums, which Jaro-Winkler alone \
                      scores above the threshold",
            },
            Case {
                playing: ("Frank Ocean", "Blonde"),
                candidate: ("Frank Ocean", "Blond"),
                accept: true,
                why: "the same record, spelled two ways by two catalogues",
            },
        ];

        for c in cases {
            let s = Scores::compare(c.playing.0, c.playing.1, c.candidate.0, c.candidate.1);
            assert_eq!(
                s.passes(),
                c.accept,
                "{:?} vs {:?} ({}): {s}",
                c.playing,
                c.candidate,
                c.why
            );
        }
    }

    #[test]
    fn a_wrong_album_is_rejected_by_the_text_gate() {
        let s = Scores::compare("The Beatles", "Abbey Road", "The Beatles", "Let It Be");
        assert!(s.artist >= ARTIST_THRESHOLD, "artist should still match");
        assert!(s.album < ALBUM_THRESHOLD, "album scored {}", s.album);
        assert!(!s.passes());
    }

    #[test]
    fn greatest_hits_by_two_artists_is_rejected_by_the_artist_score() {
        let s = Scores::compare("Queen", "Greatest Hits", "Tom Petty", "Greatest Hits");
        assert!(s.album >= ALBUM_THRESHOLD, "album titles are identical");
        assert!(s.artist < ARTIST_THRESHOLD, "artist scored {}", s.artist);
        assert!(!s.passes());
    }

    #[test]
    fn only_an_unambiguous_match_bypasses_the_perceptual_gate() {
        let exact = Scores::compare("Massive Attack", "Mezzanine", "Massive Attack", "Mezzanine");
        assert!(exact.near_exact());

        // Passes the gate, but not well enough to skip looking at the picture.
        let loose = Scores::compare(
            "Massive Attack",
            "Mezzanine",
            "Massive Attack",
            "Mezzanine Live",
        );
        assert!(loose.passes());
        assert!(!loose.near_exact(), "{loose}");
    }

    #[test]
    fn a_disagreement_about_numbers_is_decisive() {
        // Jaro-Winkler alone rates all three of these pairs above the album
        // threshold, which is exactly the wrong answer.
        for (a, b) in [
            ("trainspotting", "trainspotting 2"),
            ("led zeppelin ii", "led zeppelin iii"),
            ("greatest hits", "greatest hits volume 2"),
            ("chapter 1", "chapter 2"),
        ] {
            assert!(jaro_winkler(a, b) > ALBUM_THRESHOLD, "premise: {a}/{b}");
            assert_eq!(similarity(a, b), 0.0, "{a}/{b}");
        }
        // Agreeing numbers are left alone.
        assert!(similarity("blink 182", "blink 182") > 0.99);
        assert!(similarity("chapter 2", "chapter 2 deluxe") > 0.85);
    }

    #[test]
    fn normalising_empty_and_punctuation_only_input_is_safe() {
        assert_eq!(normalise(""), "");
        assert_eq!(normalise("   "), "");
        assert_eq!(normalise("!!!"), "");
        assert_eq!(normalise("(Deluxe Edition)"), "");
        // An unterminated bracket keeps its contents rather than eating them.
        assert_eq!(normalise("Kid A (unfinished"), "kid a unfinished");
        assert_eq!(jaro_winkler("", "anything"), 0.0);
    }
}
