//! MIDI parsing/rewriting helpers built on `midly`.
//!
//! Pure functions only (no Slint/rodio deps) so they stay unit-testable.
//! `rustysynth` internals are `pub(crate)`, so all MIDI inspection and
//! channel/program rewriting goes through `midly` here; playback then feeds
//! the rewritten bytes back into rustysynth.

use std::collections::{BTreeSet, HashMap};

use midly::{
    MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind,
    num::{u28, u4, u7},
};

/// How a MIDI file's channels are treated at playback time.
///
/// rustysynth routes 0-indexed channel 9 to the percussion bank (GM ch10)
/// and its `MidiFileSequencer` has no per-channel mute, so single-channel
/// files recorded on the "wrong" channel are inaudible without rewriting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ChannelMode {
    /// Play as-is.
    #[default]
    Auto,
    /// Move channel 9 (drums) messages to channel 0 so drum-only files
    /// play as pitched instruments.
    Synth,
    /// Move every channel message to channel 9 so anything plays as drums.
    Drums,
}

impl ChannelMode {
    /// Map a UI ComboBox index (Auto/Synth/Drums) to a mode.
    pub fn from_index(i: i32) -> Self {
        match i {
            1 => ChannelMode::Synth,
            2 => ChannelMode::Drums,
            _ => ChannelMode::Auto,
        }
    }
}

/// Playback options passed to [`crate::audio::MidiSource`].
#[derive(Clone, Copy, Debug, Default)]
pub struct MidiPlayOptions {
    pub mode: ChannelMode,
    /// GM program 0-127 forced on the affected channels, or `None` for
    /// whatever the file already requests.
    pub program: Option<u8>,
}

/// Convert a program ComboBox index to a GM program: 0 = Auto (`None`),
/// 1..=128 = program `i - 1`.
pub fn program_from_index(i: i32) -> Option<u8> {
    if (1..=128).contains(&i) {
        Some((i - 1) as u8)
    } else {
        None
    }
}

/// Compact GM program names, index = program number 0-127.
pub const GM_PROGRAM_NAMES: [&str; 128] = [
    "Acoustic Grand Piano",
    "Bright Acoustic Piano",
    "Electric Grand Piano",
    "Honky-tonk Piano",
    "Electric Piano 1",
    "Electric Piano 2",
    "Harpsichord",
    "Clavinet",
    "Celesta",
    "Glockenspiel",
    "Music Box",
    "Vibraphone",
    "Marimba",
    "Xylophone",
    "Tubular Bells",
    "Dulcimer",
    "Drawbar Organ",
    "Percussive Organ",
    "Rock Organ",
    "Church Organ",
    "Reed Organ",
    "Accordion",
    "Harmonica",
    "Tango Accordion",
    "Acoustic Guitar (nylon)",
    "Acoustic Guitar (steel)",
    "Electric Guitar (jazz)",
    "Electric Guitar (clean)",
    "Electric Guitar (muted)",
    "Overdriven Guitar",
    "Distortion Guitar",
    "Guitar Harmonics",
    "Acoustic Bass",
    "Electric Bass (finger)",
    "Electric Bass (pick)",
    "Fretless Bass",
    "Slap Bass 1",
    "Slap Bass 2",
    "Synth Bass 1",
    "Synth Bass 2",
    "Violin",
    "Viola",
    "Cello",
    "Contrabass",
    "Tremolo Strings",
    "Pizzicato Strings",
    "Orchestral Harp",
    "Timpani",
    "String Ensemble 1",
    "String Ensemble 2",
    "Synth Strings 1",
    "Synth Strings 2",
    "Choir Aahs",
    "Voice Oohs",
    "Synth Voice",
    "Orchestra Hit",
    "Trumpet",
    "Trombone",
    "Tuba",
    "Muted Trumpet",
    "French Horn",
    "Brass Section",
    "Synth Brass 1",
    "Synth Brass 2",
    "Soprano Sax",
    "Alto Sax",
    "Tenor Sax",
    "Baritone Sax",
    "Oboe",
    "English Horn",
    "Bassoon",
    "Clarinet",
    "Piccolo",
    "Flute",
    "Recorder",
    "Pan Flute",
    "Blown Bottle",
    "Shakuhachi",
    "Whistle",
    "Ocarina",
    "Lead 1 (square)",
    "Lead 2 (sawtooth)",
    "Lead 3 (calliope)",
    "Lead 4 (chiff)",
    "Lead 5 (charang)",
    "Lead 6 (voice)",
    "Lead 7 (fifths)",
    "Lead 8 (bass+lead)",
    "Pad 1 (new age)",
    "Pad 2 (warm)",
    "Pad 3 (polysynth)",
    "Pad 4 (choir)",
    "Pad 5 (bowed)",
    "Pad 6 (metallic)",
    "Pad 7 (halo)",
    "Pad 8 (sweep)",
    "FX 1 (rain)",
    "FX 2 (soundtrack)",
    "FX 3 (crystal)",
    "FX 4 (atmosphere)",
    "FX 5 (brightness)",
    "FX 6 (goblins)",
    "FX 7 (echoes)",
    "FX 8 (sci-fi)",
    "Sitar",
    "Banjo",
    "Shamisen",
    "Koto",
    "Kalimba",
    "Bagpipe",
    "Fiddle",
    "Shanai",
    "Tinkle Bell",
    "Agogo",
    "Steel Drums",
    "Woodblock",
    "Taiko Drum",
    "Melodic Tom",
    "Synth Drum",
    "Reverse Cymbal",
    "Guitar Fret Noise",
    "Breath Noise",
    "Seashore",
    "Bird Tweet",
    "Telephone Ring",
    "Helicopter",
    "Applause",
    "Gunshot",
];

/// `"0: Acoustic Grand Piano"` style model entries, prefixed with `"Auto"`.
pub fn program_model_entries() -> Vec<String> {
    let mut out = Vec::with_capacity(129);
    out.push("Auto".to_string());
    for (i, name) in GM_PROGRAM_NAMES.iter().enumerate() {
        out.push(format!("{i}: {name}"));
    }
    out
}

/// Fast scan-time summary of a MIDI file.
#[derive(Clone, Debug, Default)]
pub struct MidiSummary {
    /// Sorted unique 0-indexed channels carrying any channel message.
    pub channels: Vec<u8>,
    /// Latest program per channel, sorted by channel.
    pub programs: Vec<(u8, u8)>,
    /// Whether channel 9 (GM ch10 percussion) is used.
    pub has_drums: bool,
    /// Number of NoteOn (velocity > 0) events.
    pub note_count: usize,
}

/// Tolerant scan-time summary: walks every event, skipping nothing fatal.
/// Returns an error only when the byte stream is not an SMF at all.
pub fn parse_summary(bytes: &[u8]) -> anyhow::Result<MidiSummary> {
    let smf = Smf::parse(bytes).map_err(|e| anyhow::anyhow!("MIDI parse error: {e:?}"))?;
    let mut channels = BTreeSet::new();
    let mut programs: HashMap<u8, u8> = HashMap::new();
    let mut note_count = 0usize;

    for track in &smf.tracks {
        for ev in track {
            if let TrackEventKind::Midi { channel, message } = ev.kind {
                let ch = channel.as_int();
                channels.insert(ch);
                match message {
                    MidiMessage::NoteOn { vel, .. } => {
                        if vel.as_int() > 0 {
                            note_count += 1;
                        }
                    }
                    MidiMessage::ProgramChange { program } => {
                        programs.insert(ch, program.as_int());
                    }
                    _ => {}
                }
            }
        }
    }

    let mut channels: Vec<u8> = channels.into_iter().collect();
    channels.sort_unstable();
    let mut programs: Vec<(u8, u8)> = programs.into_iter().collect();
    programs.sort_unstable_by_key(|(ch, _)| *ch);
    let has_drums = channels.contains(&9);

    Ok(MidiSummary {
        channels,
        programs,
        has_drums,
        note_count,
    })
}

/// A single note for piano-roll display, times in seconds.
#[derive(Clone, Debug)]
pub struct NoteEv {
    pub start_sec: f64,
    pub end_sec: f64,
    pub pitch: u8,
    pub channel: u8,
}

/// Cap on notes returned (display-only; files are small but be safe).
pub const MAX_ROLL_NOTES: usize = 20_000;

/// Default tempo when the file carries no Tempo meta event (500000 usec/qn).
const DEFAULT_TEMPO_USEC: f64 = 500_000.0;

/// Extract notes with tick->second conversion via the file's tempo map.
/// Pairs NoteOn/NoteOff per (channel, pitch); NoteOn with velocity 0 counts
/// as NoteOff. Unterminated notes end at the last event time.
pub fn notes_for_roll(bytes: &[u8]) -> anyhow::Result<Vec<NoteEv>> {
    let smf = Smf::parse(bytes).map_err(|e| anyhow::anyhow!("MIDI parse error: {e:?}"))?;

    let ticks_per_quarter: f64 = match smf.header.timing {
        Timing::Metrical(ticks) => {
            let t = ticks.as_int() as f64;
            if t > 0.0 {
                t
            } else {
                480.0
            }
        }
        // SMPTE timecode timing: fall back to a common resolution.
        _ => 480.0,
    };

    // Absolute-tick event list across all tracks so the tempo map is global.
    let mut timed: Vec<(u64, TrackEventKind)> = Vec::new();
    for track in &smf.tracks {
        let mut abs: u64 = 0;
        for ev in track {
            abs = abs.saturating_add(ev.delta.as_int() as u64);
            timed.push((abs, ev.kind));
        }
    }
    // Stable order: tempo changes apply deterministically at equal ticks.
    timed.sort_by_key(|(tick, _)| *tick);

    // Tick -> seconds conversion with tempo changes.
    let mut tempo_map: Vec<(u64, f64)> = vec![(0, DEFAULT_TEMPO_USEC)];
    for (tick, kind) in &timed {
        if let TrackEventKind::Meta(MetaMessage::Tempo(mpqn)) = kind {
            let usec = mpqn.as_int() as f64;
            if usec > 0.0 {
                // Coalesce duplicate tempo entries at the same tick.
                if let Some(last) = tempo_map.last_mut() {
                    if last.0 == *tick {
                        last.1 = usec;
                        continue;
                    }
                }
                tempo_map.push((*tick, usec));
            }
        }
    }

    let tick_to_sec = |tick: u64| -> f64 {
        let mut secs = 0.0;
        let mut prev_tick = 0u64;
        let mut usec = DEFAULT_TEMPO_USEC;
        for (t_tick, t_usec) in &tempo_map {
            if *t_tick > tick {
                break;
            }
            secs += (*t_tick - prev_tick) as f64 * usec / ticks_per_quarter / 1_000_000.0;
            prev_tick = *t_tick;
            usec = *t_usec;
        }
        secs += (tick - prev_tick) as f64 * usec / ticks_per_quarter / 1_000_000.0;
        secs
    };

    let max_tick = timed.iter().map(|(t, _)| *t).max().unwrap_or(0);
    let total_sec = tick_to_sec(max_tick);

    let mut open: HashMap<(u8, u8), f64> = HashMap::new();
    let mut notes: Vec<NoteEv> = Vec::new();

    for (tick, kind) in &timed {
        if notes.len() >= MAX_ROLL_NOTES {
            break;
        }
        if let TrackEventKind::Midi { channel, message } = kind {
            let ch = channel.as_int();
            match *message {
                MidiMessage::NoteOn { key, vel } => {
                    let pitch = key.as_int();
                    if vel.as_int() > 0 {
                        open.insert((ch, pitch), tick_to_sec(*tick));
                    } else if let Some(start) = open.remove(&(ch, pitch)) {
                        let end = tick_to_sec(*tick).max(start);
                        notes.push(NoteEv {
                            start_sec: start,
                            end_sec: end,
                            pitch,
                            channel: ch,
                        });
                    }
                }
                MidiMessage::NoteOff { key, .. } => {
                    let pitch = key.as_int();
                    if let Some(start) = open.remove(&(ch, pitch)) {
                        let end = tick_to_sec(*tick).max(start);
                        notes.push(NoteEv {
                            start_sec: start,
                            end_sec: end,
                            pitch,
                            channel: ch,
                        });
                    }
                }
                _ => {}
            }
        }
    }
    // Unterminated notes: ring until the end of the file.
    for ((ch, pitch), start) in open {
        if notes.len() >= MAX_ROLL_NOTES {
            break;
        }
        notes.push(NoteEv {
            start_sec: start,
            end_sec: total_sec.max(start + 0.1),
            pitch,
            channel: ch,
        });
    }

    notes.sort_by(|a, b| {
        a.start_sec
            .partial_cmp(&b.start_sec)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(notes)
}

/// Rewrite channel nibbles (and optionally force a GM program) for playback,
/// then serialize back to SMF bytes for rustysynth.
///
/// - `Auto`: bytes unchanged apart from a possible program override.
/// - `Synth`: channel 9 messages move to channel 0.
/// - `Drums`: every channel message moves to channel 9.
///
/// With `program_override = Some(p)`, existing ProgramChange messages on the
/// affected channels are dropped and a `ProgramChange { program: p }` is
/// inserted at tick 0 of the first track for each affected channel.
pub fn remap_for_playback(
    bytes: &[u8],
    mode: ChannelMode,
    program_override: Option<u8>,
) -> anyhow::Result<Vec<u8>> {
    let mut smf = Smf::parse(bytes).map_err(|e| anyhow::anyhow!("MIDI parse error: {e:?}"))?;

    // Channels actually used, before rewriting.
    let mut used = BTreeSet::new();
    for track in &smf.tracks {
        for ev in track {
            if let TrackEventKind::Midi { channel, .. } = ev.kind {
                used.insert(channel.as_int());
            }
        }
    }

    let map_channel = |ch: u8| -> u8 {
        match mode {
            ChannelMode::Auto => ch,
            ChannelMode::Synth => {
                if ch == 9 {
                    0
                } else {
                    ch
                }
            }
            ChannelMode::Drums => 9,
        }
    };

    // Channels that will be audible after rewriting (for program override).
    let affected: BTreeSet<u8> = match mode {
        ChannelMode::Drums => [9].into_iter().collect(),
        _ => used.iter().map(|c| map_channel(*c)).collect(),
    };

    let prog = program_override.map(|p| u7::new(p));
    // Clone per-track so we can rebuild with filtered/inserted events.
    let mut new_tracks: Vec<Vec<TrackEvent>> = Vec::with_capacity(smf.tracks.len());
    for track in &smf.tracks {
        let mut nt: Vec<TrackEvent> = Vec::with_capacity(track.len());
        for ev in track {
            match ev.kind {
                TrackEventKind::Midi { channel, message } => {
                    let ch = map_channel(channel.as_int());
                    // Drop existing program changes on affected channels when
                    // forcing a program; the replacement goes to tick 0.
                    if prog.is_some()
                        && matches!(message, MidiMessage::ProgramChange { .. })
                        && affected.contains(&ch)
                    {
                        continue;
                    }
                    nt.push(TrackEvent {
                        delta: ev.delta,
                        kind: TrackEventKind::Midi {
                            channel: u4::new(ch),
                            message,
                        },
                    });
                }
                kind => nt.push(TrackEvent {
                    delta: ev.delta,
                    kind,
                }),
            }
        }
        new_tracks.push(nt);
    }

    if let Some(program) = prog {
        if !affected.is_empty() {
            if new_tracks.is_empty() {
                new_tracks.push(Vec::new());
            }
            let mut chans: Vec<u8> = affected.into_iter().collect();
            chans.sort_unstable();
            // Insert in reverse so final order at tick 0 is ascending.
            for ch in chans.into_iter().rev() {
                new_tracks[0].insert(
                    0,
                    TrackEvent {
                        delta: u28::new(0),
                        kind: TrackEventKind::Midi {
                            channel: u4::new(ch),
                            message: MidiMessage::ProgramChange { program },
                        },
                    },
                );
            }
        }
    }

    smf.tracks = new_tracks;
    let mut out = Vec::new();
    smf.write_std(&mut out)
        .map_err(|e| anyhow::anyhow!("MIDI serialize error: {e:?}"))?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use midly::Smf;
    use midly::num::{u15, u24};

    /// Minimal one-note SMF (96 ticks/qn, tempo 500000) for round-trip tests.
    fn one_note_smf() -> Vec<u8> {
        let header = midly::Header::new(midly::Format::SingleTrack, Timing::Metrical(u15::new(96)));
        let mut smf = Smf::new(header);
        smf.tracks.push(vec![
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(500_000))),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::ProgramChange {
                        program: u7::new(5),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOn {
                        key: u7::new(60),
                        vel: u7::new(100),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(96),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOff {
                        key: u7::new(60),
                        vel: u7::new(64),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
            },
        ]);
        let mut out = Vec::new();
        smf.write_std(&mut out).unwrap();
        out
    }

    #[test]
    fn summary_finds_channel_program_and_note() {
        let bytes = one_note_smf();
        let s = parse_summary(&bytes).unwrap();
        assert_eq!(s.channels, vec![0]);
        assert_eq!(s.programs, vec![(0, 5)]);
        assert!(!s.has_drums);
        assert_eq!(s.note_count, 1);
    }

    #[test]
    fn roll_converts_ticks_with_tempo_map() {
        let bytes = one_note_smf();
        let notes = notes_for_roll(&bytes).unwrap();
        assert_eq!(notes.len(), 1);
        assert!((notes[0].start_sec - 0.0).abs() < 1e-6);
        // 96 ticks @ 96 tpq, 500000 usec/qn = 0.5s.
        assert!((notes[0].end_sec - 0.5).abs() < 1e-6);
        assert_eq!(notes[0].pitch, 60);
    }

    #[test]
    fn remap_drums_moves_channel_and_forces_program() {
        let bytes = one_note_smf();
        let out = remap_for_playback(&bytes, ChannelMode::Drums, Some(0)).unwrap();
        let s = parse_summary(&out).unwrap();
        assert_eq!(s.channels, vec![9]);
        assert!(s.has_drums);
        assert_eq!(s.programs, vec![(9, 0)]);
        assert_eq!(s.note_count, 1);
    }

    #[test]
    fn remap_auto_keeps_channels_without_override() {
        let bytes = one_note_smf();
        let out = remap_for_playback(&bytes, ChannelMode::Auto, None).unwrap();
        let s = parse_summary(&out).unwrap();
        assert_eq!(s.channels, vec![0]);
        assert_eq!(s.programs, vec![(0, 5)]);
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse_summary(b"not a midi file at all........").is_err());
        assert!(notes_for_roll(b"RIFF....").is_err());
    }
}
