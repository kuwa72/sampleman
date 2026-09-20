//! Filename metadata extraction for DTM sample packs (issue #7, Tier 1).
//!
//! Pure functions only (no Slint deps), mirroring `midi_util` style.
//! Only filename conventions are used — no audio-content BPM/key detection
//! (too heavy); document this limit wherever results are surfaced.

/// Metadata parsed from a sample file name.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct FileNameMeta {
    pub bpm: Option<f32>,
    pub musical_key: Option<String>,
    pub instrument: Option<String>,
}

fn is_sep(c: char) -> bool {
    matches!(c, ' ' | '_' | '-' | '.' | '(' | ')' | '[' | ']')
}

/// Parse BPM / musical key / instrument from a file name (extension ignored).
pub fn parse_filename_meta(file_name: &str) -> FileNameMeta {
    let stem = file_name
        .rsplit_once('.')
        .map(|(s, _)| s)
        .unwrap_or(file_name);
    FileNameMeta {
        bpm: parse_bpm(stem),
        musical_key: parse_key(stem),
        instrument: parse_instrument(stem),
    }
}

/// 2-3 digit numbers 60-200 accepted when adjacent to a `bpm` marker
/// (`120BPM`, `120_BPM`) or surrounded by separators (`_120_`).
fn parse_bpm(stem: &str) -> Option<f32> {
    let lower = stem.to_lowercase();
    let bytes = lower.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i].is_ascii_digit() {
            let mut j = i;
            while j < bytes.len() && bytes[j].is_ascii_digit() {
                j += 1;
            }
            if (2..=3).contains(&(j - i)) {
                if let Ok(n) = lower[i..j].parse::<f32>() {
                    if (60.0..=200.0).contains(&n) {
                        let before = lower[..i].chars().next_back();
                        let after = &lower[j..];
                        let near_marker = lower[..i].ends_with("bpm")
                            || after.starts_with("bpm")
                            || (before == Some('_')
                                && after.trim_start_matches('_').starts_with("bpm"));
                        let bare = matches!(before, None | Some(' '))
                            || before.map(is_sep).unwrap_or(true)
                            && after
                                .chars()
                                .next()
                                .map(is_sep)
                                .unwrap_or(true);
                        if near_marker || bare {
                            return Some(n);
                        }
                    }
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    None
}

/// Key tokens like `Am`, `Cmin`, `F#maj`, `Bbmin`, `G_maj`.
/// Tokens split on separators; adjacent pairs are also joined so `G_maj`
/// (split into `G` + `maj`) still matches. First match wins. A bare
/// single letter only counts when uppercase (avoids `a`/`c` word noise).
fn parse_key(stem: &str) -> Option<String> {
    let tokens: Vec<&str> = stem.split(is_sep).filter(|t| !t.is_empty()).collect();
    for (n, tok) in tokens.iter().enumerate() {
        let joined = if n + 1 < tokens.len() {
            Some(format!("{tok}{}", tokens[n + 1]))
        } else {
            None
        };
        // Single letters (`G` in `G_maj`) prefer the joined form so the
        // mode suffix is not lost to the bare-letter match.
        if tok.len() == 1 {
            if let Some(j) = &joined {
                if let Some(k) = match_key_token(j, false) {
                    if k.len() > 1 {
                        return Some(k);
                    }
                }
            }
        }
        if let Some(k) = match_key_token(tok, true) {
            return Some(k);
        }
        if let Some(j) = &joined {
            if let Some(k) = match_key_token(j, false) {
                return Some(k);
            }
        }
    }
    None
}

fn match_key_token(tok: &str, allow_bare: bool) -> Option<String> {
    let lower = tok.to_lowercase();
    let b = lower.as_bytes();
    if b.is_empty() || !(b[0] as char).is_ascii_alphabetic() {
        return None;
    }
    let root = (b[0] as char).to_ascii_uppercase();
    if !('A'..='G').contains(&root) {
        return None;
    }
    let mut rest = &lower[1..];
    // Optional accidental: `#` or flat `b` (e.g. `F#`, `Bb`, `Bbmin`).
    // Disambiguate root-B + mode (`Bmin`) from flat (`Bbmin`): consume the
    // `b` as a flat only when the remainder is itself a mode suffix.
    let mut acc = String::new();
    if rest.starts_with('#') {
        acc.push('#');
        rest = &rest[1..];
    } else if rest.starts_with('b')
        && matches!(&rest[1..], "" | "m" | "maj" | "major" | "min" | "minor")
    {
        acc.push('b');
        rest = &rest[1..];
    }
    let mode = match rest {
        "" => {
            if tok.len() == 1 && !allow_bare {
                return None;
            }
            // Bare single letters only when uppercase in the original.
            if tok.len() == 1 && tok.chars().next() != Some(root) {
                return None;
            }
            String::new()
        }
        "m" => "m".to_string(),
        "maj" | "major" => "maj".to_string(),
        "min" | "minor" => "min".to_string(),
        _ => return None,
    };
    Some(format!("{root}{acc}{mode}"))
}

/// Instrument keyword scan over the lowercased stem; first hit in
/// priority order wins (ordered drums-first so `drum_loop_bass` → Drums).
const INSTRUMENTS: &[(&str, &str)] = &[
    ("kick", "Kick"),
    ("snare", "Snare"),
    ("clap", "Clap"),
    ("hihat", "Hi-Hat"),
    ("hi-hat", "Hi-Hat"),
    ("hat", "Hi-Hat"),
    ("cymbal", "Cymbal"),
    ("tom", "Tom"),
    ("shaker", "Shaker"),
    ("perc", "Perc"),
    ("drum", "Drums"),
    ("bass", "Bass"),
    ("lead", "Lead"),
    ("pad", "Pad"),
    ("pluck", "Pluck"),
    ("piano", "Keys"),
    ("keys", "Keys"),
    ("guitar", "Guitar"),
    ("vocal", "Vocal"),
    ("vox", "Vocal"),
    ("choir", "Vocal"),
    ("string", "Strings"),
    ("brass", "Brass"),
    ("synth", "Synth"),
    ("fx", "FX"),
];

fn parse_instrument(stem: &str) -> Option<String> {
    let lower = stem.to_lowercase();
    INSTRUMENTS
        .iter()
        .find(|(kw, _)| lower.contains(kw))
        .map(|(_, name)| name.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bpm_with_marker() {
        assert_eq!(parse_filename_meta("loop_120BPM_Am.wav").bpm, Some(120.0));
        assert_eq!(parse_filename_meta("Loop 128 BPM Cmin.wav").bpm, Some(128.0));
        assert_eq!(parse_filename_meta("kick_140_BPM_F.wav").bpm, Some(140.0));
    }

    #[test]
    fn bpm_bare_between_separators() {
        assert_eq!(parse_filename_meta("snare_174_fill.wav").bpm, Some(174.0));
        // Out of range / 4-digit years are ignored.
        assert_eq!(parse_filename_meta("loop_59.wav").bpm, None);
        assert_eq!(parse_filename_meta("loop_2024.wav").bpm, None);
    }

    #[test]
    fn key_tokens() {
        assert_eq!(
            parse_filename_meta("loop_120BPM_Am.wav").musical_key,
            Some("Am".into())
        );
        assert_eq!(
            parse_filename_meta("pad_Cmin_90bpm.wav").musical_key,
            Some("Cmin".into())
        );
        assert_eq!(
            parse_filename_meta("lead_F#maj_128BPM.wav").musical_key,
            Some("F#maj".into())
        );
        assert_eq!(
            parse_filename_meta("keys_Bbmin_100.wav").musical_key,
            Some("Bbmin".into())
        );
        assert_eq!(
            parse_filename_meta("pluck_G_maj_122.wav").musical_key,
            Some("Gmaj".into())
        );
    }

    #[test]
    fn instrument_keywords() {
        assert_eq!(
            parse_filename_meta("Deep_Kick_128BPM.wav").instrument,
            Some("Kick".into())
        );
        assert_eq!(
            parse_filename_meta("neuro_bass_Cmin_174.wav").instrument,
            Some("Bass".into())
        );
        assert_eq!(
            parse_filename_meta("chopped_vocal_Am_90.wav").instrument,
            Some("Vocal".into())
        );
    }

    #[test]
    fn no_meta_gives_nones() {
        assert_eq!(
            parse_filename_meta("field_recording_take3.wav"),
            FileNameMeta {
                bpm: None,
                musical_key: None,
                instrument: None,
            }
        );
    }

    #[test]
    fn lowercase_noise_word_is_not_a_key() {
        assert_eq!(parse_filename_meta("a_mixdown_120BPM.wav").musical_key, None);
        assert_eq!(
            parse_filename_meta("Kick_C_140.wav").musical_key,
            Some("C".into())
        );
    }
}
