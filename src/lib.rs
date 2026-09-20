pub mod database;
mod scanner;
pub mod audio;
mod midi_util;

use crate::database::{Database, Track};
use crate::scanner::{Scanner, ScanProgress};
use std::sync::{Arc, Mutex};
use rodio::{OutputStream, Sink};
use slint::{ComponentHandle, SharedString, Image, SharedPixelBuffer, Rgba8Pixel};
use std::path::{Path, PathBuf};
use std::collections::HashSet;
use chrono::{TimeZone, Local};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;
use rayon::prelude::*;

slint::include_modules!();

pub struct AppState {
    db: Arc<Mutex<Database>>,
    sink: Arc<Sink>,
    current_playback: Arc<Mutex<Option<(f64, std::time::Instant)>>>,
    seek_tx: Arc<Mutex<Option<crossbeam_channel::Sender<f64>>>>,
    playback_generation: Arc<std::sync::atomic::AtomicUsize>,
    is_scanning: Arc<std::sync::atomic::AtomicBool>,
    /// Scan generation: bumped once per scan start and once per scan
    /// completion. The progress-relay thread stamps events with the
    /// generation at scan start and the UI closure skips stale ones, so a
    /// queued event arriving after the completion update cannot flip
    /// is_scanning back on.
    scan_generation: Arc<std::sync::atomic::AtomicUsize>,
    /// Cancel flag for the running scan, checked periodically by
    /// `scan_directory` (collect + analyze loops). Set by on_cancel_scan,
    /// cleared on every scan start.
    scan_cancel: Arc<std::sync::atomic::AtomicBool>,
    /// Serializes all sink operations (stop/append/play) across play-worker
    /// threads and on_stop_track, closing the stop→recheck window where a
    /// stale thread could kill a newer generation's audio.
    /// Lock ordering: this is a LEAF lock. Hold it ONLY around sink ops,
    /// never while acquiring current_playback / seek_tx / db / ui_state,
    /// and never across slint::invoke_from_event_loop. Release before any
    /// UI update or state-lock acquisition.
    playback_lock: Arc<Mutex<()>>,
}

struct UiState {
    expanded_folders: HashSet<String>,
    search_query: String,
    selected_folder: String,
    all_tracks: Arc<Vec<Track>>, // Cached tracks in memory
    sort_column: String,
    sort_asc: bool,
    folder_search_query: String, // Added
    midi_mode: i32,    // ChannelMode ComboBox index: 0=Auto, 1=Synth, 2=Drums
    midi_program: i32, // Program ComboBox index: 0=Auto, 1..=128=GM program+1
}

/// Lock a mutex, recovering from poisoning instead of panicking.
///
/// If another thread panicked while holding the lock, the mutex becomes
/// poisoned. Rather than chaining a second panic (fatal with
/// `panic = "abort"` in the release profile), take the inner value and
/// keep running.
fn lock_mutex<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Max number of expanded-folder entries kept in the `expanded_folders`
/// setting, bounding the settings row size for large libraries.
const MAX_EXPANDED_FOLDERS: usize = 500;

fn serialize_expanded_folders(expanded: &HashSet<String>) -> String {
    let mut v: Vec<String> = expanded.iter().cloned().collect();
    v.sort();
    if v.len() > MAX_EXPANDED_FOLDERS {
        v.truncate(MAX_EXPANDED_FOLDERS);
    }
    serde_json::to_string(&v).unwrap_or_else(|_| "[]".to_string())
}

/// Persist the expanded-folder set off the UI thread (spawn + lock +
/// set_setting, same pattern as the other settings writes).
/// Debounced 500ms trailing-edge: rapid toggles stamp a newer generation
/// and only the latest snapshot is written, so a toggle burst costs one DB
/// write instead of one sleeping thread per toggle doing I/O.
fn persist_expanded_folders(db: Arc<Mutex<Database>>, expanded: HashSet<String>) {
    static PERSIST_GEN: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
    let gen = PERSIST_GEN.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(500));
        if PERSIST_GEN.load(std::sync::atomic::Ordering::Relaxed) != gen {
            return;
        }
        let json = serialize_expanded_folders(&expanded);
        let db_lock = lock_mutex(&db);
        let _ = db_lock.set_setting("expanded_folders", &json);
    });
}

/// Restore the persisted expanded-folder set: tolerate a missing/corrupt
/// value (→ empty set) and drop entries that no longer exist among the
/// current tracks' ancestor dirs.
fn restore_expanded_folders(tracks: &[Track], raw: Option<String>) -> HashSet<String> {
    let parsed: Vec<String> = raw
        .as_deref()
        .and_then(|s| serde_json::from_str::<Vec<String>>(s).ok())
        .unwrap_or_default();
    if parsed.is_empty() {
        return HashSet::new();
    }
    let mut valid = HashSet::new();
    for t in tracks {
        let mut p = PathBuf::from(&t.path);
        while let Some(parent) = p.parent() {
            if parent.as_os_str().is_empty() || parent.parent().is_none() {
                break;
            }
            valid.insert(parent.to_string_lossy().to_string());
            p = parent.to_path_buf();
        }
    }
    parsed.into_iter().filter(|e| valid.contains(e)).collect()
}

fn map_track_to_slint(t: &Track) -> TrackData {
    let filename = Path::new(&t.path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| t.path.clone());

    let mtime_str = Local
        .timestamp_opt(t.mtime, 0)
        .single()
        .map(|dt| dt.format("%Y-%m-%d %H:%M").to_string())
        .unwrap_or_else(|| "-".to_string());

    TrackData {
        id: t.id as i32,
        path: SharedString::from(t.path.clone()),
        filename: SharedString::from(filename),
        title: SharedString::from(t.title.clone().unwrap_or_default()),
        artist: SharedString::from(t.artist.clone().unwrap_or_default()),
        album: SharedString::from(t.album.clone().unwrap_or_default()),
        duration: SharedString::from(format!("{:.1}s", t.duration)),
        sample_rate: SharedString::from(t.sample_rate.map(|s| format!("{}Hz", s)).unwrap_or_else(|| "-".into())),
        bit_depth: SharedString::from(t.bit_depth.map(|s| format!("{}bit", s)).unwrap_or_else(|| "-".into())),
        channels: SharedString::from(t.channels.map(|s| format!("{}ch", s)).unwrap_or_else(|| "-".into())),
        mtime: SharedString::from(mtime_str),
    }
}

fn extract_folders_hierarchical(tracks: &[Track], expanded: &HashSet<String>, folder_query: &str) -> Vec<FolderItem> {
    let mut all_paths = HashSet::new();
    for t in tracks {
        let mut p = PathBuf::from(&t.path);
        while let Some(parent) = p.parent() {
            if parent.as_os_str().is_empty() || parent.parent().is_none() {
                break;
            }
            all_paths.insert(parent.to_path_buf());
            p = parent.to_path_buf();
        }
    }
    
    let matcher = SkimMatcherV2::default().ignore_case();
    let mut visible_paths = HashSet::new();

    if !folder_query.is_empty() {
        for p in &all_paths {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if matcher.fuzzy_match(&name, folder_query).unwrap_or(0) > 0 {
                let mut curr = p.clone();
                while !curr.as_os_str().is_empty() && curr.parent().is_some() {
                    visible_paths.insert(curr.clone());
                    let Some(parent) = curr.parent() else { break };
                    curr = parent.to_path_buf();
                }
            }
        }
    }

    let mut sorted_paths: Vec<_> = all_paths.iter().collect();
    sorted_paths.sort_by(|a, b| {
        let a_str = a.to_string_lossy().to_lowercase();
        let b_str = b.to_string_lossy().to_lowercase();
        a_str.cmp(&b_str)
    });

    let mut items = Vec::new();
    for p in sorted_paths {
        if !folder_query.is_empty() && !visible_paths.contains(p) {
            continue;
        }

        let mut parent = p.parent();
        let mut visible = true;
        
        if folder_query.is_empty() {
            while let Some(par) = parent {
                if par.as_os_str().is_empty() || par.parent().is_none() {
                    break;
                }
                if !expanded.contains(&par.to_string_lossy().to_string()) {
                    visible = false;
                    break;
                }
                parent = par.parent();
            }
        }

        if !visible {
            continue;
        }

        let name = p.file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| p.to_string_lossy().to_string());
        
        let path_str = p.to_string_lossy().to_string();
        let indent = (p.components().count().saturating_sub(1) * 15) as f32;
        let has_children = all_paths.iter().any(|other| other.parent() == Some(&p));
        
        items.push(FolderItem {
            path: path_str.clone().into(),
            name: name.into(),
            indent,
            is_expanded: if !folder_query.is_empty() { true } else { expanded.contains(&path_str) },
            has_children,
        });
    }
    items
}

fn get_filtered_track_indices(tracks: &[Track], folder: &str, query: &str, sort_column: &str, sort_asc: bool) -> Vec<usize> {
    let mut filtered: Vec<(usize, i64)> = tracks.par_iter()
        .enumerate()
        .filter(|(_, t)| folder.is_empty() || Path::new(&t.path).starts_with(folder))
        .filter_map(|(idx, t)| {
            if query.is_empty() {
                return Some((idx, 0));
            }
            
            let matcher = SkimMatcherV2::default().ignore_case();
            let filename = Path::new(&t.path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let score_fn = matcher.fuzzy_match(&filename, query).unwrap_or(0);
            
            let score_title = match t.title {
                Some(ref title) => matcher.fuzzy_match(title, query).unwrap_or(0),
                None => 0,
            };
            
            let score_path = matcher.fuzzy_match(&t.path, query).unwrap_or(0);
            let max_score = score_fn.max(score_title).max(score_path);
            
            if max_score > 0 {
                Some((idx, max_score))
            } else {
                None
            }
        })
        .collect();

    filtered.sort_by(|&(a_idx, a_score), &(b_idx, b_score)| {
        let at = &tracks[a_idx];
        let bt = &tracks[b_idx];
        
        if !query.is_empty() && sort_column == "name" {
            let s_cmp = b_score.cmp(&a_score); // Score DESC
            if s_cmp != std::cmp::Ordering::Equal {
                return s_cmp;
            }
        }
        
        let res = match sort_column {
            "name" => {
                let a_name = Path::new(&at.path).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                let b_name = Path::new(&bt.path).file_name().map(|n| n.to_string_lossy().to_lowercase()).unwrap_or_default();
                a_name.cmp(&b_name)
            }
            "duration" => at.duration.partial_cmp(&bt.duration).unwrap_or(std::cmp::Ordering::Equal),
            "bit_depth" => at.bit_depth.unwrap_or(0).cmp(&bt.bit_depth.unwrap_or(0)),
            "sample_rate" => at.sample_rate.unwrap_or(0).cmp(&bt.sample_rate.unwrap_or(0)),
            "channels" => at.channels.unwrap_or(0).cmp(&bt.channels.unwrap_or(0)),
            "mtime" => at.mtime.cmp(&bt.mtime),
            _ => std::cmp::Ordering::Equal,
        };
        if sort_asc { res } else { res.reverse() }
    });

    filtered.into_iter().map(|(idx, _)| idx).collect()
}

fn create_waveform_pixels(waveform: &[u8]) -> (u32, u32, Vec<u8>) {
    let width = 800;
    let height = 100;
    let mut pixels = vec![0u8; (width * height * 4) as usize];

    for i in 0..pixels.len() / 4 {
        pixels[i * 4] = 0;
        pixels[i * 4 + 1] = 0;
        pixels[i * 4 + 2] = 0;
        pixels[i * 4 + 3] = 255;
    }

    if !waveform.is_empty() {
        let step = (waveform.len() as f32 / width as f32).max(1.0);
        for x in 0..width {
            let idx = (x as f32 * step) as usize;
            if idx < waveform.len() {
                let val = waveform[idx] as f32 / 255.0;
                let h = (val * height as f32) as u32;
                let start_y = (height - h) / 2;
                let end_y = start_y + h;
                
                for y in start_y..end_y {
                    let p_idx = (y * width + x) as usize * 4;
                    if p_idx + 3 < pixels.len() {
                        pixels[p_idx] = 0;
                        pixels[p_idx + 1] = 180;
                        pixels[p_idx + 2] = 255;
                        pixels[p_idx + 3] = 255;
                    }
                }
            }
        }
    }

    (width, height, pixels)
}

fn create_piano_roll_pixels(notes: &[midi_util::NoteEv]) -> (u32, u32, Vec<u8>) {
    let width = 800;
    let height = 100;
    let mut pixels = vec![0u8; (width * height * 4) as usize];

    for i in 0..pixels.len() / 4 {
        pixels[i * 4] = 0;
        pixels[i * 4 + 1] = 0;
        pixels[i * 4 + 2] = 0;
        pixels[i * 4 + 3] = 255;
    }

    let max_end = notes.iter().map(|n| n.end_sec).fold(0.0f64, f64::max);
    if !(max_end > 0.0) {
        return (width, height, pixels);
    }

    for n in notes {
        let x0 = (n.start_sec / max_end * width as f64).clamp(0.0, width as f64 - 1.0) as u32;
        let x1 = ((n.end_sec / max_end * width as f64).ceil() as u32).max(x0 + 1).min(width);
        // Pitch 0-127 maps bottom-to-top; channel 9 (drums) in red/orange.
        let y_top = (height - 1).saturating_sub((n.pitch as u32 * height) / 128);
        let (r, g, b) = if n.channel == 9 {
            (255, 110, 30)
        } else {
            match n.channel % 4 {
                0 => (0, 180, 255),
                1 => (0, 255, 170),
                2 => (150, 255, 80),
                _ => (190, 130, 255),
            }
        };
        for x in x0..x1 {
            for dy in 0..2 {
                let y = (y_top + dy).min(height - 1);
                let p_idx = (y * width + x) as usize * 4;
                if p_idx + 3 < pixels.len() {
                    pixels[p_idx] = r;
                    pixels[p_idx + 1] = g;
                    pixels[p_idx + 2] = b;
                    pixels[p_idx + 3] = 255;
                }
            }
        }
    }

    (width, height, pixels)
}

struct TrackListModel {
    tracks: Arc<Vec<Track>>,
    indices: Vec<usize>,
    notify: slint::ModelNotify,
}

impl slint::Model for TrackListModel {
    type Data = TrackData;

    fn row_count(&self) -> usize {
        self.indices.len()
    }

    fn row_data(&self, row: usize) -> Option<Self::Data> {
        self.indices.get(row).and_then(|&idx| self.tracks.get(idx).map(map_track_to_slint))
    }

    fn model_tracker(&self) -> &dyn slint::ModelTracker {
        &self.notify
    }
}

fn update_tracks_ui(
    ui_weak: &slint::Weak<AppWindow>,
    st: &UiState,
    search_generation: &Arc<std::sync::atomic::AtomicUsize>,
) {
    let folder = st.selected_folder.clone();
    let query = st.search_query.clone();
    let col = st.sort_column.clone();
    let asc = st.sort_asc;
    let all_tracks = st.all_tracks.clone();

    let gen = search_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let s_gen = search_generation.clone();
    let ui_handle = ui_weak.clone();

    std::thread::spawn(move || {
        // Debounce 180ms: only the latest keystroke's snapshot proceeds.
        // Cost per keystroke is one short-lived sleeping thread (cheap);
        // filtering itself stays single-flight via the generation check.
        std::thread::sleep(std::time::Duration::from_millis(180));
        if s_gen.load(std::sync::atomic::Ordering::Relaxed) != gen {
            return;
        }
        let indices = get_filtered_track_indices(&all_tracks, &folder, &query, &col, asc);
        
        if s_gen.load(std::sync::atomic::Ordering::Relaxed) != gen {
            return;
        }

        slint::invoke_from_event_loop(move || {
            let model = TrackListModel {
                tracks: all_tracks,
                indices,
                notify: slint::ModelNotify::default(),
            };
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_tracks(slint::ModelRc::new(model));
                ui.set_selected_index(-1);
            }
        }).ok();
    });
}

fn update_folders_ui(
    ui_weak: &slint::Weak<AppWindow>,
    st: &UiState,
    folder_search_generation: &Arc<std::sync::atomic::AtomicUsize>,
) {
    let expanded = st.expanded_folders.clone();
    let query = st.folder_search_query.clone();
    let all_tracks = st.all_tracks.clone();

    let gen = folder_search_generation.fetch_add(1, std::sync::atomic::Ordering::Relaxed) + 1;
    let f_gen = folder_search_generation.clone();
    let ui_handle = ui_weak.clone();

    std::thread::spawn(move || {
        // Same 180ms debounce as update_tracks_ui (see above).
        std::thread::sleep(std::time::Duration::from_millis(180));
        if f_gen.load(std::sync::atomic::Ordering::Relaxed) != gen {
            return;
        }
        let folders = extract_folders_hierarchical(&all_tracks, &expanded, &query);
        
        if f_gen.load(std::sync::atomic::Ordering::Relaxed) != gen {
            return;
        }

        slint::invoke_from_event_loop(move || {
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_folders(slint::ModelRc::new(slint::VecModel::from(folders)));
            }
        }).ok();
    });
}

pub fn run() -> anyhow::Result<()> {
    let db = Arc::new(Mutex::new(
        Database::new("library.db").map_err(|e| anyhow::anyhow!("failed to open database: {e}"))?,
    ));

    let (_stream, stream_handle) = OutputStream::try_default()
        .map_err(|e| anyhow::anyhow!("failed to open audio output: {e}"))?;
    let sink = Arc::new(
        Sink::try_new(&stream_handle)
            .map_err(|e| anyhow::anyhow!("failed to create audio sink: {e}"))?,
    );
    
    Box::leak(Box::new(_stream));

    let ui = AppWindow::new()?;
    let ui_weak = ui.as_weak();

    let state = Arc::new(AppState {
        db: db.clone(),
        sink: sink.clone(),
        current_playback: Arc::new(Mutex::new(None)),
        seek_tx: Arc::new(Mutex::new(None)),
        playback_generation: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        is_scanning: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        scan_cancel: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        scan_generation: Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        playback_lock: Arc::new(Mutex::new(())),
    });

    let (initial_tracks, saved_folder, saved_col, saved_asc, saved_mode, saved_prog, saved_expanded_raw, saved_sidebar_width) = {
        let db_lock = lock_mutex(&db);
        let tracks = db_lock.get_all_tracks().unwrap_or_default();
        let folder = db_lock.get_setting("selected_folder").unwrap_or_default().unwrap_or_default();
        let col = db_lock.get_setting("sort_column").unwrap_or_default().unwrap_or_else(|| String::from("name"));
        let asc_str = db_lock.get_setting("sort_asc").unwrap_or_default().unwrap_or_else(|| String::from("true"));
        let asc = asc_str == "true";
        let mode = db_lock.get_setting("midi_mode").unwrap_or_default()
            .and_then(|s| s.parse::<i32>().ok()).filter(|i| (0..=2).contains(i)).unwrap_or(0);
        let prog = db_lock.get_setting("midi_program").unwrap_or_default()
            .and_then(|s| s.parse::<i32>().ok()).filter(|i| (0..=128).contains(i)).unwrap_or(0);
        let expanded_raw = db_lock.get_setting("expanded_folders").unwrap_or_default();
        let sidebar_width = db_lock.get_setting("sidebar_width").unwrap_or_default()
            .and_then(|s| s.parse::<f32>().ok()).filter(|w| (150.0..=500.0).contains(w));
        (tracks, folder, col, asc, mode, prog, expanded_raw, sidebar_width)
    };

    let restored_expanded = restore_expanded_folders(&initial_tracks, saved_expanded_raw);

    // Fall back to empty selection when the saved folder no longer matches
    // any track (deleted/renamed library dir), instead of showing an empty
    // list. Persist the fallback so the stale value is not re-read.
    let mut selected_folder = saved_folder;
    if !selected_folder.is_empty()
        && !initial_tracks.iter().any(|t| Path::new(&t.path).starts_with(&selected_folder))
    {
        selected_folder = String::new();
        let db_lock = lock_mutex(&db);
        let _ = db_lock.set_setting("selected_folder", &selected_folder);
    }

    let ui_state = Arc::new(Mutex::new(UiState {
        expanded_folders: restored_expanded,
        search_query: String::new(),
        selected_folder,
        all_tracks: Arc::new(initial_tracks),
        sort_column: saved_col,
        sort_asc: saved_asc,
        folder_search_query: String::new(),
        midi_mode: saved_mode,
        midi_program: saved_prog,
    }));

    let search_generation = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let folder_search_generation = Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let timer = slint::Timer::default();
    let ui_handle_timer = ui_weak.clone();
    let state_timer = state.clone();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(100), move || {
        if let Some(ui) = ui_handle_timer.upgrade() {
            if ui.get_is_playing() {
                let playback = lock_mutex(&state_timer.current_playback);
                if let Some((duration, start_time)) = *playback {
                    if !duration.is_finite() || duration <= 0.0 {
                        ui.set_play_progress(0.0);
                    } else {
                        let elapsed = start_time.elapsed().as_secs_f64();
                        let progress = (elapsed / duration).min(1.0) as f32;
                        ui.set_play_progress(progress);
                        if progress >= 1.0 || state_timer.sink.empty() {
                            ui.set_is_playing(false);
                        }
                    }
                } else if state_timer.sink.empty() {
                    ui.set_is_playing(false);
                }
            }
        }
    });

    {
        let ui_state_guard = lock_mutex(&ui_state);
        ui.set_selected_folder(SharedString::from(&ui_state_guard.selected_folder));
        ui.set_current_sort_column(SharedString::from(&ui_state_guard.sort_column));
        ui.set_current_sort_asc(ui_state_guard.sort_asc);
        ui.set_midi_mode_index(ui_state_guard.midi_mode);
        ui.set_midi_program_index(ui_state_guard.midi_program);
        if let Some(w) = saved_sidebar_width {
            ui.set_sidebar_width(w);
        }
        let prog_model: Vec<SharedString> = midi_util::program_model_entries()
            .into_iter()
            .map(SharedString::from)
            .collect();
        ui.set_midi_program_model(slint::ModelRc::new(slint::VecModel::from(prog_model)));

        update_folders_ui(&ui_weak, &ui_state_guard, &folder_search_generation);
        update_tracks_ui(&ui_weak, &ui_state_guard, &search_generation);
    }

    let state_scan = state.clone();
    let ui_handle_scan = ui_weak.clone();
    let ui_state_scan = ui_state.clone();
    let scan_search_gen = search_generation.clone();
    let scan_folder_gen = folder_search_generation.clone();
    ui.on_scan_library(move |path_arg| {
        let state = state_scan.clone();
        let ui_weak = ui_handle_scan.clone();
        let ui_state = ui_state_scan.clone();
        let s_gen = scan_search_gen.clone();
        let f_gen = scan_folder_gen.clone();
        let path_str = path_arg.to_string();

        // Open the folder dialog off the UI thread so the event loop never blocks.
        std::thread::spawn(move || {
            let is_add_library = path_str.is_empty();
            let path = if is_add_library {
                println!("Add Library: opening FileDialog...");
                match rfd::FileDialog::new().pick_folder() {
                    Some(folder) => folder.to_string_lossy().to_string(),
                    None => return,
                }
            } else {
                println!("Rescanning folder: {}", path_str);
                path_str
            };

            // Double-scan guard: only one scan runs at a time.
            if state
                .is_scanning
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_err()
            {
                let ui_weak = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_status_text("Scan already in progress".into());
                    }
                })
                .ok();
                return;
            }

            // Stamp this scan; relay events carry this generation and the UI
            // closure skips stale ones, so a queued event arriving after the
            // completion update cannot flip is_scanning back on.
            // Also clear any stale cancel request from a previous run.
            state
                .scan_cancel
                .store(false, std::sync::atomic::Ordering::SeqCst);
            let scan_gen = state
                .scan_generation
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                + 1;

            // WalkDir on a missing dir yields zero entries ("All files up to
            // date"), so check existence up front.
            if !Path::new(&path).is_dir() {
                let msg = format!("Folder not found: {}", path);
                let ui_weak = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_status_text(msg.into());
                    }
                })
                .ok();
                state
                    .is_scanning
                    .store(false, std::sync::atomic::Ordering::SeqCst);
                return;
            }

            if is_add_library {
                {
                    let mut st = lock_mutex(&ui_state);
                    st.selected_folder = path.clone();
                    st.expanded_folders.insert(path.clone());
                }
                let path_for_ui = path.clone();
                let ui_weak = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_selected_folder(SharedString::from(&path_for_ui));
                        ui.set_is_scanning(true);
                        ui.set_scan_progress(0.0);
                        ui.set_scan_stage("Scanning".into());
                        ui.set_status_text("Indexing directory files...".into());
                    }
                })
                .ok();
                let path_for_db = path.clone();
                let db = state.db.clone();
                {
                    let db_lock = lock_mutex(&db);
                    let _ = db_lock.set_setting("selected_folder", &path_for_db);
                    let expanded_json = serialize_expanded_folders(&lock_mutex(&ui_state).expanded_folders);
                    let _ = db_lock.set_setting("expanded_folders", &expanded_json);
                }
            } else {
                let ui_weak = ui_weak.clone();
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_is_scanning(true);
                        ui.set_scan_progress(0.0);
                        ui.set_scan_stage("Scanning".into());
                        ui.set_status_text("Indexing directory files...".into());
                    }
                })
                .ok();
            }

            println!("Scan thread spawned for {}", path);
            let (progress_tx, progress_rx) = crossbeam_channel::unbounded::<ScanProgress>();
            let scanner = Scanner::new(&state.db);

            let ui_weak_progress = ui_weak.clone();
            let state_progress = state.clone();
            std::thread::spawn(move || {
                while let Ok(p) = progress_rx.recv() {
                    let ui_weak = ui_weak_progress.clone();
                    let gen_state = state_progress.clone();
                    slint::invoke_from_event_loop(move || {
                        if gen_state.scan_generation.load(std::sync::atomic::Ordering::SeqCst) != scan_gen {
                            return;
                        }
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_is_scanning(true);
                            ui.set_scan_stage(SharedString::from(p.stage.clone()));
                            if p.total > 0 {
                                ui.set_scan_progress(p.current as f32 / p.total as f32);
                                ui.set_status_text(format!("{} ({}/{})", p.path, p.current, p.total).into());
                            } else {
                                // Indeterminate phase (indexing): keep the bar
                                // at 0 and carry the hint in stage + status text.
                                ui.set_scan_progress(0.0);
                                ui.set_status_text(SharedString::from(p.path));
                            }
                        }
                    }).ok();
                }
            });

            println!("Calling scan_directory...");
            let scan_result = scanner.scan_directory(&path, progress_tx, &state.scan_cancel);
            if let Err(ref e) = scan_result {
                eprintln!("Scan error: {}", e);
            }
            println!("scan_directory finished. Querying matching tracks for sub-renders...");
            // GC stale records + reload, under a single DB lock.
            let (tracks, gc_removed) = {
                let db = lock_mutex(&state.db);
                let gc_removed = db.remove_missing_under(&path).unwrap_or(0);
                let tracks = db.get_all_tracks().unwrap_or_default();
                (tracks, gc_removed)
            };
            println!(
                "Tracks total after load: {} (gc removed {})",
                tracks.len(),
                gc_removed
            );

            let mut st = lock_mutex(&ui_state);
            st.all_tracks = Arc::new(tracks); // Update cache!

            update_folders_ui(&ui_weak, &st, &f_gen);
            update_tracks_ui(&ui_weak, &st, &s_gen);

            let status = match scan_result {
                Ok(report) if report.cancelled => {
                    SharedString::from("Scan cancelled")
                }
                Ok(report) => {
                    let mut msg = format!("Scan Complete: {} added/updated", report.saved);
                    if report.walk_errors > 0 {
                        msg += &format!(" ({} unreadable entries skipped)", report.walk_errors);
                    }
                    if report.analyze_errors > 0 {
                        msg += &format!(" ({} failed)", report.analyze_errors);
                    }
                    SharedString::from(msg)
                }
                Err(e) => SharedString::from(format!("Scan failed: {}", e)),
            };
            let ui_weak_complete = ui_weak.clone();
            let gen_state = state.clone();
            slint::invoke_from_event_loop(move || {
                if gen_state.scan_generation.load(std::sync::atomic::Ordering::SeqCst) != scan_gen {
                    return;
                }
                if let Some(ui) = ui_weak_complete.upgrade() {
                    ui.set_is_scanning(false);
                    ui.set_scan_stage("".into());
                    ui.set_status_text(status);
                }
                // Invalidate any relay events still queued behind this update.
                gen_state.scan_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }).ok();
            state
                .is_scanning
                .store(false, std::sync::atomic::Ordering::SeqCst);
        });
    });

    // Cancel button (visible while is-scanning): the scan loops poll this
    // flag and stop promptly; the scan thread then reloads partial results
    // from the DB and reports "Scan cancelled".
    let state_cancel = state.clone();
    ui.on_cancel_scan(move || {
        state_cancel
            .scan_cancel
            .store(true, std::sync::atomic::Ordering::SeqCst);
    });

    let state_filter = state.clone();
    let ui_handle_filter = ui_weak.clone();
    let ui_state_filter = ui_state.clone();
    let select_folder_search_gen = search_generation.clone();
    ui.on_select_folder(move |path| {
        let path_str = path.to_string();
        {
            let mut st = lock_mutex(&ui_state_filter);
            st.selected_folder = path_str.clone();
            update_tracks_ui(&ui_handle_filter, &st, &select_folder_search_gen);
        }
        if let Some(ui) = ui_handle_filter.upgrade() {
            ui.set_selected_folder(SharedString::from(&path_str));
        }

        let db = state_filter.db.clone();
        let path_for_db = path_str.clone();
        std::thread::spawn(move || {
            let db_lock = lock_mutex(&db);
            let _ = db_lock.set_setting("selected_folder", &path_for_db);
        });
    });

    let ui_handle_toggle = ui_weak.clone();
    let ui_state_toggle = ui_state.clone();
    let toggle_folder_gen = folder_search_generation.clone();
    let state_toggle = state.clone();
    ui.on_toggle_folder(move |path| {
        let path_str = path.to_string();
        let mut st = lock_mutex(&ui_state_toggle);
        if st.expanded_folders.contains(&path_str) {
            st.expanded_folders.remove(&path_str);
        } else {
            st.expanded_folders.insert(path_str);
        }

        persist_expanded_folders(state_toggle.db.clone(), st.expanded_folders.clone());
        update_folders_ui(&ui_handle_toggle, &st, &toggle_folder_gen);
    });

    let ui_handle_search = ui_weak.clone();
    let ui_state_search = ui_state.clone();
    let search_track_gen = search_generation.clone();
    ui.on_search_tracks(move |query| {
        let mut st = lock_mutex(&ui_state_search);
        st.search_query = query.to_string();

        update_tracks_ui(&ui_handle_search, &st, &search_track_gen);
    });

    let state_play = state.clone();
    let ui_handle_play = ui_weak.clone();
    let ui_state_play = ui_state.clone();
    let play_folder_gen = folder_search_generation.clone();
    ui.on_play_track(move |path, initial_progress| {
        let state = state_play.clone();
        let ui_handle = ui_handle_play.clone();
        let path_str = path.to_string();
        let initial_progress = initial_progress as f64;

        // Short-op busy display: decoding happens on the worker thread
        // below, so announce it now (overwritten on success/failure).
        {
            let filename = Path::new(&path_str)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path_str.clone());
            if let Some(ui) = ui_handle.upgrade() {
                ui.set_status_text(SharedString::from(format!("Loading {}...", filename)));
            }
        }

        {
            let mut st = lock_mutex(&ui_state_play);
            let p = PathBuf::from(&path_str);
            if let Some(parent) = p.parent() {
                let parent_str = parent.to_string_lossy().to_string();
                if !parent_str.is_empty() {
                    st.selected_folder = parent_str.clone();
                    
                    let mut curr = parent.to_path_buf();
                    while let Some(par) = curr.parent() {
                        if par.as_os_str().is_empty() || par.parent().is_none() {
                            break;
                        }
                        st.expanded_folders.insert(par.to_string_lossy().to_string());
                        curr = par.to_path_buf();
                    }
                    st.expanded_folders.insert(parent_str.clone());
                    
                    persist_expanded_folders(state_play.db.clone(), st.expanded_folders.clone());
                    let cur_folder = st.selected_folder.clone();
                    update_folders_ui(&ui_handle, &st, &play_folder_gen);
                    
                    let ui_weak = ui_handle.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_selected_folder(SharedString::from(cur_folder));
                        }
                    }).ok();
                }
            }
        }

        // Snapshot the MIDI channel/program selections on the UI thread.
        let midi_options = {
            let st = lock_mutex(&ui_state_play);
            crate::audio::MidiPlayOptions {
                mode: midi_util::ChannelMode::from_index(st.midi_mode),
                program: midi_util::program_from_index(st.midi_program),
            }
        };

        std::thread::spawn(move || {
            let my_gen = state.playback_generation.fetch_add(1, std::sync::atomic::Ordering::SeqCst) + 1;
            let is_latest = || state.playback_generation.load(std::sync::atomic::Ordering::SeqCst) == my_gen;
            let initial_progress = if initial_progress.is_finite() {
                initial_progress.clamp(0.0, 1.0)
            } else {
                0.0
            };

            let track = {
                let db = lock_mutex(&state.db);
                db.get_track_by_path(&path_str).ok().flatten()
            };

            let Some(t) = track else {
                if !is_latest() {
                    return;
                }
                let msg = SharedString::from(format!("File not found in library: {}", path_str));
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_handle.upgrade() {
                        ui.set_is_playing(false);
                        ui.set_status_text(msg);
                    }
                }).ok();
                return;
            };

            let duration = t.duration;
            let waveform_data = t.waveform.clone();
            let filename = Path::new(&path_str)
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| path_str.clone());

            // Create the audio source BEFORE touching UI/playback state so a
            // failure cannot leave `is_playing=true` stuck on.
            let (seek_tx, seek_rx) = crossbeam_channel::unbounded::<f64>();
            let ext = Path::new(&path_str).extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();

            let source = if ext == "mid" || ext == "midi" {
                match crate::audio::MidiSource::new(&path_str, seek_rx, midi_options) {
                    Ok(s) => crate::audio::DynamicSource::Midi(s),
                    Err(e) => {
                        eprintln!("Failed to create MidiSource: {}", e);
                        if !is_latest() {
                            return;
                        }
                        let msg = SharedString::from(format!("Playback failed: {}", e));
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_handle.upgrade() {
                                ui.set_is_playing(false);
                                ui.set_status_text(msg);
                            }
                        }).ok();
                        return;
                    }
                }
            } else {
                match crate::audio::SymphoniaSource::new(&path_str, seek_rx) {
                    Ok(s) => crate::audio::DynamicSource::Symphonia(s),
                    Err(e) => {
                        eprintln!("Failed to create SymphoniaSource: {}", e);
                        if !is_latest() {
                            return;
                        }
                        let msg = SharedString::from(format!("Playback failed: {}", e));
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_handle.upgrade() {
                                ui.set_is_playing(false);
                                ui.set_status_text(msg);
                            }
                        }).ok();
                        return;
                    }
                }
            };

            // A newer play request supersedes this one: skip sink/state updates.
            if !is_latest() {
                return;
            }

            if initial_progress > 0.0 && duration.is_finite() && duration > 0.0 {
                let secs = duration * initial_progress;
                if secs.is_finite() && secs >= 0.0 {
                    let _ = seek_tx.send(secs);
                }
            }

            // Serialize sink ops under playback_lock for the whole
            // stop→recheck→append→play sequence, so a stale thread can never
            // kill a newer generation's audio in the stop→recheck window.
            // Leaf lock: held ONLY here, released before state locks / UI
            // updates (see AppState docs). Generation rechecks stay as
            // belt-and-suspenders alongside the lock.
            {
                let _playback_guard = lock_mutex(&state.playback_lock);
                state.sink.stop();
                // Recheck after the (possibly slow) sink stop: another play may
                // have started while we were blocked.
                if !is_latest() {
                    return;
                }
                state.sink.append(source);
                state.sink.play();
            }

            {
                let mut cp = lock_mutex(&state.current_playback);
                // Guard Duration::from_secs_f64 (panics on negative/NaN).
                let offset = duration * initial_progress;
                let offset = if offset.is_finite() && offset > 0.0 { offset } else { 0.0 };
                let start_time = std::time::Instant::now() - std::time::Duration::from_secs_f64(offset);
                *cp = Some((duration, start_time));
            }

            {
                let mut tx_guard = lock_mutex(&state.seek_tx);
                // Only the latest play owns the seek channel.
                if is_latest() {
                    *tx_guard = Some(seek_tx);
                }
            }

            let path_for_ui = path_str.clone();
            // MIDI tracks render a piano roll into the waveform slot (files
            // are small, so parse on demand); audio keeps the DB waveform.
            // Empty/parse-failure falls back to blank + a status-text note.
            let (width, height, pixels, roll_note): (u32, u32, Vec<u8>, Option<String>) =
                if ext == "mid" || ext == "midi" {
                    match std::fs::read(&path_str) {
                        Ok(bytes) => match midi_util::notes_for_roll(&bytes) {
                            Ok(notes) if !notes.is_empty() => {
                                let (w, h, px) = create_piano_roll_pixels(&notes);
                                (w, h, px, None)
                            }
                            Ok(_) => {
                                eprintln!("MIDI piano roll: no notes in {}", path_str);
                                let (w, h, px) = create_piano_roll_pixels(&[]);
                                (w, h, px, Some(format!("MIDI: no notes found in {}", filename)))
                            }
                            Err(e) => {
                                eprintln!("MIDI piano roll parse failed for {}: {}", path_str, e);
                                let (w, h, px) = create_piano_roll_pixels(&[]);
                                (w, h, px, Some(format!("MIDI piano roll unavailable: {}", e)))
                            }
                        },
                        Err(e) => {
                            eprintln!("MIDI piano roll read failed for {}: {}", path_str, e);
                            let (w, h, px) = create_piano_roll_pixels(&[]);
                            (w, h, px, Some(format!("MIDI piano roll unavailable: {}", e)))
                        }
                    }
                } else {
                    let (w, h, px) = create_waveform_pixels(&waveform_data.unwrap_or_default());
                    (w, h, px, None)
                };
            let gen_for_ui = state.playback_generation.clone();

            slint::invoke_from_event_loop(move || {
                if gen_for_ui.load(std::sync::atomic::Ordering::SeqCst) != my_gen {
                    return;
                }
                if let Some(ui) = ui_handle.upgrade() {
                    ui.set_current_track_name(SharedString::from(filename));
                    ui.set_current_track_info(SharedString::from(path_for_ui));

                    let mut pixel_buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
                    let dest = pixel_buffer.make_mut_bytes();
                    if dest.len() == pixels.len() {
                        dest.copy_from_slice(&pixels);
                    }
                    ui.set_waveform_image(Image::from_rgba8(pixel_buffer));

                    if let Some(note) = roll_note {
                        ui.set_status_text(SharedString::from(note));
                    }

                    ui.set_is_playing(true);
                    ui.set_play_progress(initial_progress as f32);
                }
            }).ok();
        });
    });

    let ui_stop_weak = ui_weak.clone();
    let state_stop = state.clone();
    ui.on_stop_track(move || {
        if let Some(ui) = ui_stop_weak.upgrade() {
            ui.set_is_playing(false);
        }
        // Serialize with play workers (leaf lock: sink ops only, released
        // before touching state locks — see AppState docs).
        {
            let _playback_guard = lock_mutex(&state_stop.playback_lock);
            state_stop.sink.stop();
        }
        // Clear playback state so a later seek press no-ops (the seek handler
        // only acts on Some(tx)) instead of resurrecting a dead timestamp.
        // Sequential scopes: never hold current_playback while acquiring
        // seek_tx or vice versa.
        {
            let mut cp = lock_mutex(&state_stop.current_playback);
            *cp = None;
        }
        {
            let mut tx_guard = lock_mutex(&state_stop.seek_tx);
            *tx_guard = None;
        }
    });

    let ui_handle_drag = ui_weak.clone();
    ui.on_start_drag(move |path| {
        if let Some(ui) = ui_handle_drag.upgrade() {
            let path_str = path.to_string();
            let _item = drag::DragItem::Files(vec![PathBuf::from(path_str)]);
            
            #[cfg(any(target_os = "windows", target_os = "macos"))]
            {
                use i_slint_backend_winit::WinitWindowAccessor;
                ui.window().with_winit_window(|winit_window| {
                    let _ = drag::start_drag(
                        winit_window,
                        _item,
                        drag::Image::Raw(vec![0; 4]),
                        |_, _| {},
                        drag::Options::default(),
                    );
                });
            }
        }
    });

    let state_seek = state.clone();
    ui.on_seek_track(move |progress| {
        // Copy the sender out first, then drop the guard before touching
        // `current_playback` (fixed lock ordering: never hold `seek_tx`
        // while acquiring `current_playback`).
        let tx_opt = { lock_mutex(&state_seek.seek_tx).clone() };
        if let Some(tx) = tx_opt {
            let duration = {
                let cp = lock_mutex(&state_seek.current_playback);
                cp.map(|(d, _)| d).unwrap_or(1.0)
            };
            let progress_f = progress as f64;
            let progress_f = if progress_f.is_finite() { progress_f.clamp(0.0, 1.0) } else { 0.0 };
            let raw_secs = duration * progress_f;
            // Guard Duration::from_secs_f64 (panics on negative/NaN).
            let secs = if raw_secs.is_finite() && raw_secs >= 0.0 { raw_secs } else { 0.0 };
            if let Err(e) = tx.send(secs) {
                eprintln!("Failed to send seek request ({}s): {}", secs, e);
            }

            let mut cp = lock_mutex(&state_seek.current_playback);
            *cp = Some((duration, std::time::Instant::now() - std::time::Duration::from_secs_f64(secs)));
        }
    });

    let ui_handle_fm = ui_weak.clone();
    ui.on_open_in_file_manager(move |path| {
        let path_str = path.to_string();
        let p = std::path::PathBuf::from(&path_str);

        #[cfg(target_os = "windows")]
        let res = if p.is_file() {
            std::process::Command::new("explorer")
                .arg("/select,")
                .arg(&p)
                .spawn()
                .map(|_| ())
        } else {
            std::process::Command::new("explorer")
                .arg(&p)
                .spawn()
                .map(|_| ())
        };
        #[cfg(not(target_os = "windows"))]
        let res = {
            let target = if p.is_file() { p.parent().unwrap_or(&p) } else { &p };
            let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
            std::process::Command::new(cmd).arg(target).spawn().map(|_| ())
        };
        if let Err(e) = res {
            eprintln!("Failed to open file manager for {}: {}", path_str, e);
            if let Some(ui) = ui_handle_fm.upgrade() {
                ui.set_status_text(SharedString::from(format!(
                    "Could not open file manager: {}",
                    e
                )));
            }
        }
    });

    let state_sort = state.clone();
    let ui_handle_sort = ui_weak.clone();
    let ui_state_sort = ui_state.clone();
    let sort_track_gen = search_generation.clone();
    ui.on_sort_tracks(move |column| {
        let mut st = lock_mutex(&ui_state_sort);
        let col_str = column.to_string();
        if st.sort_column == col_str {
            st.sort_asc = !st.sort_asc;
        } else {
            st.sort_column = col_str;
            st.sort_asc = true;
        }
        
        let asc = st.sort_asc;
        let col = st.sort_column.clone();
        
        let db = state_sort.db.clone();
        let col_for_db = col.clone();
        std::thread::spawn(move || {
            let db_lock = lock_mutex(&db);
            let _ = db_lock.set_setting("sort_column", &col_for_db);
            let _ = db_lock.set_setting("sort_asc", if asc { "true" } else { "false" });
        });

        if let Some(ui) = ui_handle_sort.upgrade() {
            ui.set_current_sort_column(SharedString::from(&col));
            ui.set_current_sort_asc(asc);
        }

        update_tracks_ui(&ui_handle_sort, &st, &sort_track_gen);
    });

    let ui_state_folder_search = ui_state.clone();
    let ui_handle_folder_search = ui_weak.clone();
    let folder_search_gen = folder_search_generation.clone();
    ui.on_search_folders(move |query| {
        let mut st = lock_mutex(&ui_state_folder_search);
        st.folder_search_query = query.to_string();

        update_folders_ui(&ui_handle_folder_search, &st, &folder_search_gen);
    });

    // MIDI channel-mode / program selections: live in UiState (read at play
    // time) and persist in settings like sort_column. ComboBox `selected`
    // carries the value string; resolve back to the model index here.
    let state_midi_mode = state.clone();
    let ui_state_midi_mode = ui_state.clone();
    ui.on_midi_mode_changed(move |value| {
        let idx = match value.as_str() {
            "Synth" => 1,
            "Drums" => 2,
            _ => 0,
        };
        {
            let mut st = lock_mutex(&ui_state_midi_mode);
            st.midi_mode = idx;
        }
        let db = state_midi_mode.db.clone();
        std::thread::spawn(move || {
            let db_lock = lock_mutex(&db);
            let _ = db_lock.set_setting("midi_mode", &idx.to_string());
        });
    });

    let state_midi_prog = state.clone();
    let ui_state_midi_prog = ui_state.clone();
    ui.on_midi_program_changed(move |value| {
        // Model entries are "Auto" or "N: name" with N = GM program.
        let idx = if value.as_str() == "Auto" {
            0
        } else {
            value.as_str().split(':').next()
                .and_then(|n| n.trim().parse::<i32>().ok())
                .map(|p| p + 1)
                .filter(|i| (1..=128).contains(i))
                .unwrap_or(0)
        };
        {
            let mut st = lock_mutex(&ui_state_midi_prog);
            st.midi_program = idx;
        }
        let db = state_midi_prog.db.clone();
        std::thread::spawn(move || {
            let db_lock = lock_mutex(&db);
            let _ = db_lock.set_setting("midi_program", &idx.to_string());
        });
    });

    let state_sidebar = state.clone();
    ui.on_sidebar_width_changed(move |width| {
        let db = state_sidebar.db.clone();
        let px = width.to_string();
        std::thread::spawn(move || {
            let db_lock = lock_mutex(&db);
            let _ = db_lock.set_setting("sidebar_width", &px);
        });
    });

    ui.run()?;
    Ok(())
}
