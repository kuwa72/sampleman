mod database;
mod scanner;
mod audio;

use crate::database::{Database, Track};
use crate::scanner::{Scanner, ScanProgress};
use std::sync::{Arc, Mutex};
use rodio::{OutputStream, Sink};
use slint::{ComponentHandle, SharedString, VecModel, ModelRc, Image, SharedPixelBuffer, Rgba8Pixel};
use std::path::{Path, PathBuf};
use std::collections::HashSet;
use chrono::{TimeZone, Local};
use fuzzy_matcher::skim::SkimMatcherV2;
use fuzzy_matcher::FuzzyMatcher;

slint::include_modules!();

pub struct AppState {
    db: Arc<Mutex<Database>>,
    sink: Arc<Sink>,
    current_playback: Arc<Mutex<Option<(f64, std::time::Instant)>>>,
    seek_tx: Arc<Mutex<Option<crossbeam_channel::Sender<f64>>>>,
}

struct UiState {
    expanded_folders: HashSet<String>,
    search_query: String,
    selected_folder: String,
    all_tracks: Vec<Track>, // Cached tracks in memory
    sort_column: String,
    sort_asc: bool,
    folder_search_query: String, // Added
}

fn map_track_to_slint(t: &Track) -> TrackData {
    let filename = Path::new(&t.path)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| t.path.clone());

    let datetime = Local.timestamp_opt(t.mtime, 0).unwrap();
    let mtime_str = datetime.format("%Y-%m-%d %H:%M").to_string();

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
    
    let matcher = SkimMatcherV2::default();
    let mut visible_paths = HashSet::new();

    if !folder_query.is_empty() {
        for p in &all_paths {
            let name = p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            if matcher.fuzzy_match(&name, folder_query).unwrap_or(0) > 0 {
                let mut curr = p.clone();
                while !curr.as_os_str().is_empty() && curr.parent().is_some() {
                    visible_paths.insert(curr.clone());
                    curr = curr.parent().unwrap().to_path_buf();
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

fn get_filtered_tracks(tracks: &[Track], folder: &str, query: &str, sort_column: &str, sort_asc: bool) -> Vec<TrackData> {
    let matcher = SkimMatcherV2::default();
    
    let mut filtered: Vec<(&Track, i64)> = tracks.iter()
        .filter(|t| folder.is_empty() || t.path.starts_with(folder))
        .filter_map(|t| {
            if query.is_empty() {
                return Some((t, 0));
            }
            
            let filename = Path::new(&t.path).file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
            let score_fn = matcher.fuzzy_match(&filename, query).unwrap_or(0);
            
            let score_title = if let Some(ref title) = t.title {
                matcher.fuzzy_match(title, query).unwrap_or(0)
            } else { 0 };
            
            let score_path = matcher.fuzzy_match(&t.path, query).unwrap_or(0);
            let max_score = score_fn.max(score_title).max(score_path);
            
            if max_score > 0 {
                Some((t, max_score))
            } else {
                None
            }
        })
        .collect();

    filtered.sort_by(|a, b| {
        let (at, a_score) = a;
        let (bt, b_score) = b;
        
        if !query.is_empty() && sort_column == "name" {
            let s_cmp = b_score.cmp(a_score); // Score DESC
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

    filtered.into_iter().map(|(t, _)| map_track_to_slint(t)).collect()
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

pub fn run() -> anyhow::Result<()> {
    let db = Arc::new(Mutex::new(Database::new("library.db").expect("failed to open database")));
    
    let (_stream, stream_handle) = OutputStream::try_default().expect("failed to open audio output");
    let sink = Arc::new(Sink::try_new(&stream_handle).expect("failed to create audio sink"));
    
    Box::leak(Box::new(_stream));

    let ui = AppWindow::new()?;
    let ui_weak = ui.as_weak();

    let state = Arc::new(AppState {
        db: db.clone(),
        sink: sink.clone(),
        current_playback: Arc::new(Mutex::new(None)),
        seek_tx: Arc::new(Mutex::new(None)),
    });

    let (initial_tracks, saved_folder, saved_col, saved_asc) = {
        let db_lock = db.lock().unwrap();
        let tracks = db_lock.get_all_tracks().unwrap_or_default();
        let folder = db_lock.get_setting("selected_folder").unwrap_or_default().unwrap_or_default();
        let col = db_lock.get_setting("sort_column").unwrap_or_default().unwrap_or_else(|| String::from("name"));
        let asc_str = db_lock.get_setting("sort_asc").unwrap_or_default().unwrap_or_else(|| String::from("true"));
        let asc = asc_str == "true";
        (tracks, folder, col, asc)
    };

    let ui_state = Arc::new(Mutex::new(UiState {
        expanded_folders: HashSet::new(),
        search_query: String::new(),
        selected_folder: saved_folder,
        all_tracks: initial_tracks,
        sort_column: saved_col,
        sort_asc: saved_asc,
        folder_search_query: String::new(),
    }));

    let timer = slint::Timer::default();
    let ui_handle_timer = ui_weak.clone();
    let state_timer = state.clone();
    timer.start(slint::TimerMode::Repeated, std::time::Duration::from_millis(100), move || {
        if let Some(ui) = ui_handle_timer.upgrade() {
            if ui.get_is_playing() {
                let playback = state_timer.current_playback.lock().unwrap();
                if let Some((duration, start_time)) = *playback {
                    let elapsed = start_time.elapsed().as_secs_f64();
                    let progress = (elapsed / duration).min(1.0) as f32;
                    ui.set_play_progress(progress);
                    if progress >= 1.0 {
                        ui.set_is_playing(false);
                    }
                }
            }
        }
    });

    {
        let ui_state_guard = ui_state.lock().unwrap();
        ui.set_selected_folder(SharedString::from(&ui_state_guard.selected_folder));
        ui.set_current_sort_column(SharedString::from(&ui_state_guard.sort_column));
        ui.set_current_sort_asc(ui_state_guard.sort_asc);

        let folder_items = extract_folders_hierarchical(&ui_state_guard.all_tracks, &ui_state_guard.expanded_folders, &ui_state_guard.folder_search_query);
        ui.set_folders(ModelRc::new(VecModel::from(folder_items)));

        let track_data = get_filtered_tracks(&ui_state_guard.all_tracks, &ui_state_guard.selected_folder, &ui_state_guard.search_query, &ui_state_guard.sort_column, ui_state_guard.sort_asc);
        ui.set_tracks(ModelRc::new(VecModel::from(track_data)));
        ui.set_selected_index(-1);
    }

    let state_scan = state.clone();
    let ui_handle_scan = ui_weak.clone();
    let ui_state_scan = ui_state.clone();
    ui.on_scan_library(move |path_arg| {
        let state = state_scan.clone();
        let ui_weak = ui_handle_scan.clone();
        let ui_state = ui_state_scan.clone();
        let path_str = path_arg.to_string();

        let path = if path_str.is_empty() {
            println!("Add Library: opening FileDialog...");
            if let Some(folder) = rfd::FileDialog::new().pick_folder() {
                folder.to_string_lossy().to_string()
            } else {
                return;
            }
        } else {
            println!("Rescanning folder: {}", path_str);
            path_str
        };

        std::thread::spawn(move || {
            println!("Scan thread spawned for {}", path);
                let (progress_tx, progress_rx) = crossbeam_channel::unbounded::<ScanProgress>();
                let scanner = Scanner::new(&state.db);
                
                let ui_weak_progress = ui_weak.clone();
                std::thread::spawn(move || {
                    while let Ok(p) = progress_rx.recv() {
                        let ui_weak = ui_weak_progress.clone();
                        slint::invoke_from_event_loop(move || {
                            if let Some(ui) = ui_weak.upgrade() {
                                ui.set_is_scanning(true);
                                ui.set_scan_progress(p.current as f32 / p.total.max(1) as f32);
                                ui.set_status_text(SharedString::from(p.path));
                            }
                        }).ok();
                    }
                });

                println!("Calling scan_directory...");
                if let Err(e) = scanner.scan_directory(path, progress_tx) {
                    eprintln!("Scan error: {}", e);
                }
                println!("scan_directory finished. Querying matching tracks for sub-renders...");
                let tracks = {
                    let db = state.db.lock().unwrap();
                    db.get_all_tracks().unwrap_or_default()
                };
                println!("Tracks total after load: {}", tracks.len());
                
                let mut st = ui_state.lock().unwrap();
                st.all_tracks = tracks.clone(); // Update cache!
                let folder_items = extract_folders_hierarchical(&tracks, &st.expanded_folders, &st.folder_search_query);
                let track_data = get_filtered_tracks(&tracks, &st.selected_folder, &st.search_query, &st.sort_column, st.sort_asc);
                
                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_weak.upgrade() {
                        ui.set_is_scanning(false);
                        ui.set_status_text("Scan Complete".into());
                        ui.set_folders(ModelRc::new(VecModel::from(folder_items)));
                        ui.set_tracks(ModelRc::new(VecModel::from(track_data)));
                        ui.set_selected_index(-1);
                    }
                }).ok();
            });
    });

    let state_filter = state.clone();
    let ui_handle_filter = ui_weak.clone();
    let ui_state_filter = ui_state.clone();
    ui.on_select_folder(move |path| {
        let path_str = path.to_string();
        {
            let mut st = ui_state_filter.lock().unwrap();
            st.selected_folder = path_str.clone();
        }
        if let Some(ui) = ui_handle_filter.upgrade() {
            ui.set_selected_folder(SharedString::from(&path_str));
        }

        let db = state_filter.db.clone();
        let path_for_db = path_str.clone();
        std::thread::spawn(move || {
            let db_lock = db.lock().unwrap();
            let _ = db_lock.set_setting("selected_folder", &path_for_db);
        });

        let state = state_filter.clone();
        let ui_weak = ui_handle_filter.clone();
        let ui_state = ui_state_filter.clone();

        std::thread::spawn(move || {
            let st = ui_state.lock().unwrap();
            let track_data = get_filtered_tracks(&st.all_tracks, &st.selected_folder, &st.search_query, &st.sort_column, st.sort_asc);
            
            slint::invoke_from_event_loop(move || {
                if let Some(ui) = ui_weak.upgrade() {
                    ui.set_tracks(ModelRc::new(VecModel::from(track_data)));
                    ui.set_selected_index(-1);
                }
            }).ok();
        });
    });

    let state_toggle = state.clone();
    let ui_handle_toggle = ui_weak.clone();
    let ui_state_toggle = ui_state.clone();
    ui.on_toggle_folder(move |path| {
        let path_str = path.to_string();
        let mut st = ui_state_toggle.lock().unwrap();
        if st.expanded_folders.contains(&path_str) {
            st.expanded_folders.remove(&path_str);
        } else {
            st.expanded_folders.insert(path_str);
        }

        let folder_items = extract_folders_hierarchical(&st.all_tracks, &st.expanded_folders, &st.folder_search_query);
        if let Some(ui) = ui_handle_toggle.upgrade() {
            ui.set_folders(ModelRc::new(VecModel::from(folder_items)));
        }
    });

    let state_search = state.clone();
    let ui_handle_search = ui_weak.clone();
    let ui_state_search = ui_state.clone();
    ui.on_search_tracks(move |query| {
        let mut st = ui_state_search.lock().unwrap();
        st.search_query = query.to_string();

        let track_data = get_filtered_tracks(&st.all_tracks, &st.selected_folder, &st.search_query, &st.sort_column, st.sort_asc);
        if let Some(ui) = ui_handle_search.upgrade() {
            ui.set_tracks(ModelRc::new(VecModel::from(track_data)));
            ui.set_selected_index(-1);
        }
    });

    let state_play = state.clone();
    let ui_handle_play = ui_weak.clone();
    let ui_state_play = ui_state.clone();
    ui.on_play_track(move |path| {
        let state = state_play.clone();
        let ui_handle = ui_handle_play.clone();
        let path_str = path.to_string();

        {
            let mut st = ui_state_play.lock().unwrap();
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
                    
                    let folder_items = extract_folders_hierarchical(&st.all_tracks, &st.expanded_folders, &st.folder_search_query);
                    let sel_f = st.selected_folder.clone();
                    
                    let ui_weak = ui_handle.clone();
                    slint::invoke_from_event_loop(move || {
                        if let Some(ui) = ui_weak.upgrade() {
                            ui.set_folders(ModelRc::new(VecModel::from(folder_items)));
                            ui.set_selected_folder(SharedString::from(sel_f));
                        }
                    }).ok();
                }
            }
        }

        std::thread::spawn(move || {
            let track = {
                let db = state.db.lock().unwrap();
                db.get_track_by_path(&path_str).ok().flatten()
            };

            if let Some(t) = track {
                let waveform_data = t.waveform.clone();
                let filename = Path::new(&path_str)
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_else(|| path_str.clone());

                let duration = t.duration;
                let path_for_ui = path_str.clone();
                let (width, height, pixels) = create_waveform_pixels(&waveform_data.unwrap_or_default());

                slint::invoke_from_event_loop(move || {
                    if let Some(ui) = ui_handle.upgrade() {
                        ui.set_current_track_name(SharedString::from(filename));
                        ui.set_current_track_info(SharedString::from(path_for_ui));
                        
                        let mut pixel_buffer = SharedPixelBuffer::<Rgba8Pixel>::new(width, height);
                        pixel_buffer.make_mut_bytes().copy_from_slice(&pixels);
                        ui.set_waveform_image(Image::from_rgba8(pixel_buffer));
                        
                        ui.set_is_playing(true);
                        ui.set_play_progress(0.0);
                    }
                }).ok();

                {
                    let mut cp = state.current_playback.lock().unwrap();
                    *cp = Some((duration, std::time::Instant::now()));
                }

                let (seek_tx, seek_rx) = crossbeam_channel::unbounded::<f64>();
                let ext = Path::new(&path_str).extension().and_then(|s| s.to_str()).unwrap_or("").to_lowercase();
                
                let source = if ext == "mid" || ext == "midi" {
                    match crate::audio::MidiSource::new(&path_str, seek_rx) {
                        Ok(s) => crate::audio::DynamicSource::Midi(s),
                        Err(e) => {
                            eprintln!("Failed to create MidiSource: {}", e);
                            return;
                        }
                    }
                } else {
                    match crate::audio::SymphoniaSource::new(&path_str, seek_rx) {
                        Ok(s) => crate::audio::DynamicSource::Symphonia(s),
                        Err(e) => {
                            eprintln!("Failed to create SymphoniaSource: {}", e);
                            return;
                        }
                    }
                };

                state.sink.stop();
                state.sink.append(source);
                state.sink.play();

                let mut tx_guard = state.seek_tx.lock().unwrap();
                *tx_guard = Some(seek_tx);
            }
        });
    });

    let ui_stop_weak = ui_weak.clone();
    let state_stop = state.clone();
    ui.on_stop_track(move || {
        if let Some(ui) = ui_stop_weak.upgrade() {
            ui.set_is_playing(false);
        }
        state_stop.sink.stop();
    });

    let ui_handle_drag = ui_weak.clone();
    ui.on_start_drag(move |path| {
        if let Some(ui) = ui_handle_drag.upgrade() {
            let path_str = path.to_string();
            let item = drag::DragItem::Files(vec![PathBuf::from(path_str)]);
            
            use i_slint_backend_winit::WinitWindowAccessor;
            ui.window().with_winit_window(|winit_window| {
                let _ = drag::start_drag(
                    winit_window,
                    item,
                    drag::Image::Raw(vec![0; 4]),
                    |_, _| {},
                    drag::Options::default(),
                );
            });
        }
    });

    let state_seek = state.clone();
    ui.on_seek_track(move |progress| {
        let tx_guard = state_seek.seek_tx.lock().unwrap();
        if let Some(ref tx) = *tx_guard {
            let duration = {
                let cp = state_seek.current_playback.lock().unwrap();
                cp.map(|(d, _)| d).unwrap_or(1.0)
            };
            let secs = duration * progress as f64;
            let _ = tx.send(secs);

            let mut cp = state_seek.current_playback.lock().unwrap();
            *cp = Some((duration, std::time::Instant::now() - std::time::Duration::from_secs_f64(secs)));
        }
    });

    ui.on_open_in_file_manager(move |path| {
        let path_str = path.to_string();
        let p = std::path::PathBuf::from(&path_str);
        
        #[cfg(target_os = "windows")]
        {
            if p.is_file() {
                let _ = std::process::Command::new("explorer")
                    .arg("/select,")
                    .arg(&p)
                    .spawn();
            } else {
                let _ = std::process::Command::new("explorer")
                    .arg(&p)
                    .spawn();
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let target = if p.is_file() { p.parent().unwrap_or(&p) } else { &p };
            let cmd = if cfg!(target_os = "macos") { "open" } else { "xdg-open" };
            let _ = std::process::Command::new(cmd).arg(target).spawn();
        }
    });

    let state_sort = state.clone();
    let ui_handle_sort = ui_weak.clone();
    let ui_state_sort = ui_state.clone();
    ui.on_sort_tracks(move |column| {
        let mut st = ui_state_sort.lock().unwrap();
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
            let db_lock = db.lock().unwrap();
            let _ = db_lock.set_setting("sort_column", &col_for_db);
            let _ = db_lock.set_setting("sort_asc", if asc { "true" } else { "false" });
        });

        if let Some(ui) = ui_handle_sort.upgrade() {
            ui.set_current_sort_column(SharedString::from(&col));
            ui.set_current_sort_asc(asc);
        }

        let track_data = get_filtered_tracks(&st.all_tracks, &st.selected_folder, &st.search_query, &st.sort_column, st.sort_asc);
        if let Some(ui) = ui_handle_sort.upgrade() {
            ui.set_tracks(ModelRc::new(VecModel::from(track_data)));
            ui.set_selected_index(-1);
        }
    });

    let ui_state_folder_search = ui_state.clone();
    let ui_handle_folder_search = ui_weak.clone();
    ui.on_search_folders(move |query| {
        let mut st = ui_state_folder_search.lock().unwrap();
        st.folder_search_query = query.to_string();

        let folder_items = extract_folders_hierarchical(&st.all_tracks, &st.expanded_folders, &st.folder_search_query);
        if let Some(ui) = ui_handle_folder_search.upgrade() {
            ui.set_folders(ModelRc::new(VecModel::from(folder_items)));
        }
    });

    ui.run()?;
    Ok(())
}
