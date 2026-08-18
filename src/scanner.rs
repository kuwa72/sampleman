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

pub struct Scanner<'a> {
    db: &'a Mutex<Database>,
}

impl<'a> Scanner<'a> {
    pub fn new(db: &'a Mutex<Database>) -> Self {
        Self { db }
    }

    pub fn scan_directory<P: AsRef<Path>>(&self, dir: P, progress_tx: crossbeam_channel::Sender<ScanProgress>) -> anyhow::Result<()> 
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

        let entries: Vec<_> = WalkDir::new(dir)
            .into_iter()
            .filter_map(|e| e.ok())
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
                            if db_mtime == mtime && db_size == size {
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
            return Ok(());
        }

        let (tx, rx) = crossbeam_channel::unbounded();
        let scanner_ref = self;

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
                        if let Ok(mut db) = scanner_ref.db.lock() {
                            if let Err(e) = db.batch_upsert_tracks(std::mem::take(&mut batch)) {
                                eprintln!("Database batch upsert error: {}", e);
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
                if let Ok(mut db) = scanner_ref.db.lock() {
                    if let Err(e) = db.batch_upsert_tracks(batch) {
                        eprintln!("Database final batch upsert error: {}", e);
                    }
                }
                println!("Final batch saved.");
            }
        });

        Ok(())
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
            let mut file = fs::File::open(path)?;
            let midi = rustysynth::MidiFile::new(&mut file).map_err(|e| anyhow::anyhow!("MIDI parse error: {:?}", e))?;
            let duration = midi.get_length();
            
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
            
        let duration = if let Some(n_frames) = codec_params.n_frames {
            n_frames as f64 / codec_params.sample_rate.unwrap_or(44100) as f64
        } else {
            0.0
        };

        let sample_rate = codec_params.sample_rate;
        let bit_depth = codec_params.bits_per_sample;
        let channels = codec_params.channels.map(|c| c.count() as u16);

        // Simplified Waveform
        let waveform = self.extract_waveform(&mut format, track_id)?;

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
        })
    }

    fn extract_waveform(&self, format: &mut Box<dyn symphonia::core::formats::FormatReader>, track_id: u32) -> anyhow::Result<Vec<u8>> {
        let mut decoder = symphonia::default::get_codecs().make(
            &format.tracks().iter().find(|t| t.id == track_id).unwrap().codec_params,
            &Default::default(),
        )?;

        let mut waveform = Vec::new();
        let mut sample_count = 0;
        let mut current_max: f32 = 0.0;
        let samples_per_pixel = 200; // Much higher resolution

        while let Ok(packet) = format.next_packet() {
            if packet.track_id() != track_id {
                continue;
            }

            match decoder.decode(&packet) {
                Ok(decoded) => {
                    let spec = *decoded.spec();
                    let mut buffer = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
                    buffer.copy_interleaved_ref(decoded);

                    for &sample in buffer.samples() {
                        current_max = current_max.max(sample.abs());
                        sample_count += 1;

                        if sample_count >= samples_per_pixel {
                            waveform.push((current_max * 255.0) as u8);
                            current_max = 0.0;
                            sample_count = 0;
                        }
                    }
                }
                Err(symphonia::core::errors::Error::DecodeError(_)) => continue,
                Err(e) => return Err(e.into()),
            }
            
            // Limit waveform size for performance but higher than before
            if waveform.len() >= 4000 {
                break;
            }
        }

        Ok(waveform)
    }
}
