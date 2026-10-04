use std::fs;
use std::path::Path;
use walkdir::WalkDir;
use symphonia::core::formats::FormatOptions;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::audio::SampleBuffer;
use std::sync::Mutex;
use crate::database::{Database, TrackData};
use serde::Serialize;

#[derive(Clone, Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    pub total: usize,
    pub current: usize,
    pub path: String,
    pub stage: String, // "Scanning" | "Analyzing" | "Saving"
}

/// Outcome counts for one `scan_directory` run, surfaced in the UI status.
pub struct ScanReport {
    pub saved: usize,
    pub walk_errors: usize,
    pub failed_batches: usize,
}

/// Stored waveform format version. Bump to force a one-time full rescan
/// that regenerates waveforms saved in an older format.
/// v3: adds MIDI summary columns (midi_channels/midi_programs/has_drums).
const WAVEFORM_VERSION: &str = "3";
/// Fixed number of peaks stored per track: whole-file coverage resampled
/// by max-pooling (stereo/mono unified, DB size bounded).
const WAVEFORM_PEAKS: usize = 1200;
/// Fine-grained frames aggregated into one peak before resampling.
const FRAMES_PER_PEAK: usize = 200;

pub struct Scanner<'a> {
    db: &'a Mutex<Database>,
}

impl<'a> Scanner<'a> {
    pub fn new(db: &'a Mutex<Database>) -> Self {
        Self { db }
    }

    pub fn scan_directory<P: AsRef<Path>>(&self, dir: P, progress_tx: crossbeam_channel::Sender<ScanProgress>) -> anyhow::Result<ScanReport> 
    {
        use rayon::prelude::*;
        use std::collections::HashMap;

        progress_tx.send(ScanProgress {
            total: 0,
            current: 0,
            path: "Indexing directory files...".into(),
            stage: "Scanning".into(),
        }).ok();

        // Fetch existing metadata once
        let existing_meta: HashMap<String, (i64, i64)> = {
            let db = self.db.lock().map_err(|_| anyhow::anyhow!("failed to lock database"))?;
            db.get_all_metadata()?
        };

        // Old-format (truncated/variable-length) waveforms are regenerated
        // once: a version mismatch disables the up-to-date early-out below,
        // forcing a full rescan. The version is persisted at the end of the
        // scan, after which normal incremental behavior resumes.
        let waveform_stale: bool = {
            let db = self.db.lock().map_err(|_| anyhow::anyhow!("failed to lock database"))?;
            db.get_setting("waveform_version")?.as_deref() != Some(WAVEFORM_VERSION)
        };

        let mut walk_errors: usize = 0;
        let entries: Vec<_> = WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| match e {
                Ok(entry) => Some(entry),
                Err(err) => {
                    walk_errors += 1;
                    eprintln!("Scan walk error: {}", err);
                    None
                }
            })
            .filter(|e| e.file_type().is_file() && self.is_audio_file(e.path()))
            .filter_map(|e| {
                let path = e.path();
                let path_str = path.to_string_lossy().to_string();
                
                if let Ok(metadata) = fs::metadata(path) {
                    let mtime = metadata.modified()
                        .ok()
                        .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64);
                    let size = metadata.len() as i64;
                    
                    if let Some(mtime) = mtime {
                        if let Some(&(db_mtime, db_size)) = existing_meta.get(&path_str) {
                            if !waveform_stale && db_mtime == mtime && db_size == size {
                                return None;
                            }
                        }
                        return Some((path.to_path_buf(), path_str, mtime, size));
                    }
                }
                None
            })
            .collect();

        let total = entries.len();
        println!("Found {} potential audio files to analyze.", total);
        if total == 0 {
            progress_tx.send(ScanProgress {
                total: 0,
                current: 0,
                path: "All files up to date".into(),
                stage: "Done".into(),
            }).ok();
            if let Ok(db) = self.db.lock() {
                if let Err(e) = db.set_setting("waveform_version", WAVEFORM_VERSION) {
                    eprintln!("Failed to persist waveform_version: {}", e);
                }
            }
            if walk_errors > 0 {
                return Err(anyhow::anyhow!("{walk_errors} unreadable entries skipped, no files scanned"));
            }
            return Ok(ScanReport { saved: 0, walk_errors: 0, failed_batches: 0 });
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        let scanner_ref = self;
        let mut saved: usize = 0;
        let mut failed_batches: usize = 0;

        rayon::scope(|s| {
            // Spawn parallel analysis in the background of the scope
            s.spawn(|_| {
                entries.into_par_iter().for_each_with(tx, |tx, (path, path_str, mtime, size)| {
                    println!("Analyzing: {}", path_str);
                    match scanner_ref.analyze_file(&path, &path_str, mtime, size) {
                        Ok(data) => {
                            tx.send(Some(data)).ok();
                        }
                        Err(e) => {
                            eprintln!("Error analyzing {}: {}", path_str, e);
                            tx.send(None).ok();
                        }
                    }
                });
            });

            // Collect results in the "main" thread of the scope
            let mut current = 0;
            let mut batch = Vec::new();
            let mut last_emit = std::time::Instant::now();
            
            while let Ok(result) = rx.recv() {
                current += 1;
                if let Some(data) = result {
                    let path_clone = data.path.clone();
                    batch.push(data);
                    
                    if batch.len() >= 50 {
                        println!("Saving batch of {} files...", batch.len());
                        let pending = std::mem::take(&mut batch);
                        let n = pending.len();
                        match scanner_ref.db.lock() {
                            Ok(mut db) => match db.batch_upsert_tracks(pending) {
                                Ok(()) => saved += n,
                                Err(e) => {
                                    failed_batches += 1;
                                    eprintln!("Database batch upsert error: {}", e);
                                }
                            },
                            Err(e) => {
                                failed_batches += 1;
                                eprintln!("Database lock error during batch upsert: {}", e);
                            }
                        }
                        println!("Batch saved.");
                    }
                    
                    if last_emit.elapsed() >= std::time::Duration::from_millis(100) || current == total {
                        progress_tx.send(ScanProgress {
                            total,
                            current,
                            path: path_clone,
                            stage: "Analyzing".into(),
                        }).ok();
                        last_emit = std::time::Instant::now();
                    }
                } else {
                    if last_emit.elapsed() >= std::time::Duration::from_millis(100) || current == total {
                        progress_tx.send(ScanProgress {
                            total,
                            current,
                            path: "Error".into(),
                            stage: "Analyzing".into(),
                        }).ok();
                        last_emit = std::time::Instant::now();
                    }
                }

                if current >= total {
                    break;
                }
            }
            
            // Final batch
            if !batch.is_empty() {
                println!("Saving final batch of {} files...", batch.len());
                let n = batch.len();
                match scanner_ref.db.lock() {
                    Ok(mut db) => match db.batch_upsert_tracks(batch) {
                        Ok(()) => saved += n,
                        Err(e) => {
                            failed_batches += 1;
                            eprintln!("Database final batch upsert error: {}", e);
                        }
                    },
                    Err(e) => {
                        failed_batches += 1;
                        eprintln!("Database lock error during final batch upsert: {}", e);
                    }
                }
                println!("Final batch saved.");
            }
        });

        // Waveforms are now current; the next scan resumes incremental behavior.
        if let Ok(db) = self.db.lock() {
            if let Err(e) = db.set_setting("waveform_version", WAVEFORM_VERSION) {
                eprintln!("Failed to persist waveform_version: {}", e);
            }
        }

        if failed_batches > 0 {
            let walk_note = if walk_errors > 0 {
                format!(" ({} unreadable entries skipped)", walk_errors)
            } else {
                String::new()
            };
            return Err(anyhow::anyhow!("{failed_batches} batch(es) failed to save{walk_note}"));
        }

        Ok(ScanReport { saved, walk_errors, failed_batches: 0 })
    }

    fn is_audio_file(&self, path: &Path) -> bool {
        let filename = path.file_name().and_then(|s| s.to_str()).unwrap_or("");
        if filename.starts_with('.') {
            return false;
        }

        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        matches!(ext.as_str(), "wav" | "mp3" | "flac" | "aif" | "aiff" | "m4a" | "ogg" | "wma" | "mid" | "midi" | "aac")
    }


    fn analyze_file(&self, path: &Path, path_str: &str, mtime: i64, size: i64) -> anyhow::Result<TrackData> {
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
        
        if ext == "mid" || ext == "midi" {
            let bytes = fs::read(path)?;
            // Scan-time MIDI summary (midly, no synth needed). Tolerated:
            // a file rustysynth can play but midly chokes on still scans
            // with NULL MIDI columns.
            let summary = crate::midi_util::parse_summary(&bytes).ok();
            let midi = rustysynth::MidiFile::new(&mut &bytes[..]).map_err(|e| anyhow::anyhow!("MIDI parse error: {:?}", e))?;
            let duration = midi.get_length();
            let (midi_channels, midi_programs, has_drums) = match summary {
                Some(s) => (
                    Some(s.channels.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(",")),
                    Some(s.programs.iter().map(|(c, p)| format!("{c}:{p}")).collect::<Vec<_>>().join(",")),
                    Some(if s.has_drums { 1i64 } else { 0i64 }),
                ),
                None => (None, None, None),
            };

            return Ok(TrackData {
                path: path_str.to_string(),
                mtime,
                size,
                title: None,
                artist: None,
                album: None,
                genre: None,
                duration,
                sample_rate: Some(44100), // Default synth rate
                bit_depth: Some(16),
                channels: Some(2),
                comment: None,
                waveform: Some(Vec::new()), // Empty waveform
                midi_channels,
                midi_programs,
                has_drums,
            });
        }

        // Open the media source
        let file = fs::File::open(path)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());

        // Create a hint to help the format reader
        let mut hint = Hint::new();
        hint.with_extension(&ext);

        // Use default options
        let format_opts = FormatOptions::default();
        let metadata_opts = MetadataOptions::default();

        // Probe the media source
        let probed = symphonia::default::get_probe().format(&hint, mss, &format_opts, &metadata_opts)?;
        let mut format = probed.format;

        // Metadata extraction
        let mut title = None;
        let mut artist = None;
        let mut album = None;
        let mut genre = None;
        let mut comment = None;

        // Try to get metadata from tags
        if let Some(metadata_rev) = format.metadata().current() {
            for tag in metadata_rev.tags() {
                match tag.std_key {
                    Some(symphonia::core::meta::StandardTagKey::TrackTitle) => title = Some(tag.value.to_string()),
                    Some(symphonia::core::meta::StandardTagKey::Artist) => artist = Some(tag.value.to_string()),
                    Some(symphonia::core::meta::StandardTagKey::Album) => album = Some(tag.value.to_string()),
                    Some(symphonia::core::meta::StandardTagKey::Genre) => genre = Some(tag.value.to_string()),
                    Some(symphonia::core::meta::StandardTagKey::Comment) => comment = Some(tag.value.to_string()),
                    _ => {}
                }
            }
        }

        // Get the first track
        let track = format.tracks().get(0)
            .ok_or_else(|| anyhow::anyhow!("no tracks found"))?;
        let track_id = track.id;
        let codec_params = &track.codec_params;
            
        let header_duration = codec_params.n_frames.map(|n_frames| {
            n_frames as f64 / codec_params.sample_rate.unwrap_or(44100) as f64
        });

        let sample_rate = codec_params.sample_rate;
        let bit_depth = codec_params.bits_per_sample;
        let channels = codec_params.channels.map(|c| c.count() as u16);

        // Whole-file waveform plus measured duration (fallback when the
        // header has no frame count, e.g. mp3). The header value wins when
        // available.
        let (waveform, measured_duration) = self.extract_waveform(&mut format, track_id)?;
        let duration = header_duration.unwrap_or(measured_duration);

        Ok(TrackData {
            path: path_str.to_string(),
            mtime,
            size,
            title,
            artist,
            album,
            genre,
            duration,
            sample_rate,
            bit_depth,
            channels,
            comment,
            waveform: Some(waveform),
            midi_channels: None,
            midi_programs: None,
            has_drums: None,
        })
    }

    /// Decodes the whole track, collecting peaks in FRAMES (interleaved
    /// samples divided by the per-buffer channel count, so stereo/mono map
    /// identically) plus the total decoded frame count.
    ///
    /// Returns `(peaks, duration_secs)` where peaks are resampled to exactly
    /// [`WAVEFORM_PEAKS`] entries covering the full duration (empty when
    /// nothing could be decoded) and `duration_secs` is
    /// `total_frames / sample_rate` measured from the decoded stream.
    fn extract_waveform(&self, format: &mut Box<dyn symphonia::core::formats::FormatReader>, track_id: u32) -> anyhow::Result<(Vec<u8>, f64)> {
        let mut decoder = {
            let track = format
                .tracks()
                .iter()
                .find(|t| t.id == track_id)
                .ok_or_else(|| anyhow::anyhow!("track id {track_id} not found in format tracks"))?;
            symphonia::default::get_codecs().make(&track.codec_params, &Default::default())?
        };

        let mut fine: Vec<u8> = Vec::new();
        let mut total_frames: u64 = 0;
        let mut sample_rate: u32 = 0;
        let mut frames_in_peak = 0;
        let mut current_max: f32 = 0.0;

        while let Ok(packet) = format.next_packet() {
            if packet.track_id() != track_id {
                continue;
            }

            match decoder.decode(&packet) {
                Ok(decoded) => {
                    // Channel count may differ per packet; read it per buffer
                    // and guard against zero to avoid division by zero.
                    let channels = decoded.spec().channels.count().max(1);
                    sample_rate = decoded.spec().rate;
                    let spec = *decoded.spec();
                    let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                    buffer.copy_interleaved_ref(decoded);

                    let samples = buffer.samples();
                    let n_frames = samples.len() / channels;
                    for f in 0..n_frames {
                        let mut frame_peak: f32 = 0.0;
                        for c in 0..channels {
                            frame_peak = frame_peak.max(samples[f * channels + c].abs());
                        }
                        current_max = current_max.max(frame_peak);
                        frames_in_peak += 1;

                        if frames_in_peak >= FRAMES_PER_PEAK {
                            fine.push((current_max * 255.0) as u8);
                            current_max = 0.0;
                            frames_in_peak = 0;
                        }
                    }
                    total_frames += n_frames as u64;
                }
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(e) => return Err(e.into()),
            }
        }

        // Keep the trailing partial window so the file tail is covered.
        if frames_in_peak > 0 {
            fine.push((current_max * 255.0) as u8);
        }

        let measured_duration = if sample_rate > 0 {
            total_frames as f64 / sample_rate as f64
        } else {
            0.0
        };

        Ok((resample_peaks(&fine, WAVEFORM_PEAKS), measured_duration))
    }
}

/// Resamples fine peaks to exactly `target` entries covering the same
/// duration: max-pooling when downsampling, nearest-neighbor stretch when
/// upsampling. Either way the linear time mapping used by the display is
/// preserved. An empty input stays empty (nothing decodable).
fn resample_peaks(fine: &[u8], target: usize) -> Vec<u8> {
    if fine.is_empty() || fine.len() == target {
        return fine.to_vec();
    }
    let mut out = Vec::with_capacity(target);
    if fine.len() > target {
        for i in 0..target {
            let start = i * fine.len() / target;
            let end = ((i + 1) * fine.len() / target).max(start + 1);
            out.push(fine[start..end].iter().copied().max().unwrap_or(0));
        }
    } else {
        for i in 0..target {
            out.push(fine[i * fine.len() / target]);
        }
    }
    out
}
