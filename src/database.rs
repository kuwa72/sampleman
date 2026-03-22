use rusqlite::{params, Connection, Result};
use std::path::Path;
use serde::{Serialize, Deserialize};
use std::collections::HashMap;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Track {
    pub id: i64,
    pub path: String,
    pub mtime: i64, // Added
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub duration: f64,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u16>,
    pub comment: Option<String>,
    pub waveform: Option<Vec<u8>>,
}

pub struct TrackData {
    pub path: String,
    pub mtime: i64,
    pub size: i64,
    pub title: Option<String>,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub genre: Option<String>,
    pub duration: f64,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub channels: Option<u16>,
    pub comment: Option<String>,
    pub waveform: Option<Vec<u8>>,
}

pub struct Database {
    conn: Connection,
}

impl Database {
    pub fn new<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path)?;
        
        // Performance optimizations
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        
        Self::init_schema(&conn)?;
        Ok(Self { conn })
    }

    fn init_schema(conn: &Connection) -> Result<()> {
        conn.execute(
            "CREATE TABLE IF NOT EXISTS tracks (
                id INTEGER PRIMARY KEY,
                path TEXT UNIQUE NOT NULL,
                mtime INTEGER NOT NULL,
                size INTEGER NOT NULL,
                title TEXT,
                artist TEXT,
                album TEXT,
                genre TEXT,
                duration REAL,
                sample_rate INTEGER,
                bit_depth INTEGER,
                channels INTEGER,
                comment TEXT,
                waveform BLOB,
                spectrogram BLOB
            )",
            [],
        )?;

        // Simple migration for existing tables
        let columns = ["sample_rate", "bit_depth", "channels", "comment"];
        for col in columns {
            let exists: bool = conn.query_row(
                "SELECT count(*) FROM pragma_table_info('tracks') WHERE name=?",
                params![col],
                |row| row.get(0),
            ).unwrap_or(0) > 0;

            if !exists {
                let type_str = if col == "comment" { "TEXT" } else { "INTEGER" };
                conn.execute(&format!("ALTER TABLE tracks ADD COLUMN {} {}", col, type_str), [])?;
            }
        }
        
        conn.execute(
            "CREATE INDEX IF NOT EXISTS idx_tracks_path ON tracks(path)",
            [],
        )?;
        
        Ok(())
    }

    pub fn get_track_metadata(&self, path: &str) -> Result<Option<(i64, i64)>> {
        let mut stmt = self.conn.prepare("SELECT mtime, size FROM tracks WHERE path = ?")?;
        let mut rows = stmt.query(params![path])?;
        
        if let Some(row) = rows.next()? {
            Ok(Some((row.get(0)?, row.get(1)?)))
        } else {
            Ok(None)
        }
    }

    pub fn get_all_metadata(&self) -> Result<HashMap<String, (i64, i64)>> {
        let mut stmt = self.conn.prepare("SELECT path, mtime, size FROM tracks")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, (row.get::<_, i64>(1)?, row.get::<_, i64>(2)?)))
        })?;

        let mut map = HashMap::new();
        for row in rows {
            let (path, meta) = row?;
            map.insert(path, meta);
        }
        Ok(map)
    }

    pub fn upsert_track(&self, data: TrackData) -> Result<()> {
        self.conn.execute(
            "INSERT INTO tracks (path, mtime, size, title, artist, album, genre, duration, sample_rate, bit_depth, channels, comment, waveform)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(path) DO UPDATE SET
                mtime = excluded.mtime,
                size = excluded.size,
                title = excluded.title,
                artist = excluded.artist,
                album = excluded.album,
                genre = excluded.genre,
                duration = excluded.duration,
                sample_rate = excluded.sample_rate,
                bit_depth = excluded.bit_depth,
                channels = excluded.channels,
                comment = excluded.comment,
                waveform = excluded.waveform",
            params![
                data.path, data.mtime, data.size, data.title, data.artist, data.album, data.genre,
                data.duration, data.sample_rate, data.bit_depth, data.channels, data.comment, data.waveform
            ],
        )?;
        Ok(())
    }

    pub fn batch_upsert_tracks(&mut self, tracks: Vec<TrackData>) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO tracks (path, mtime, size, title, artist, album, genre, duration, sample_rate, bit_depth, channels, comment, waveform)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
                 ON CONFLICT(path) DO UPDATE SET
                    mtime = excluded.mtime,
                    size = excluded.size,
                    title = excluded.title,
                    artist = excluded.artist,
                    album = excluded.album,
                    genre = excluded.genre,
                    duration = excluded.duration,
                    sample_rate = excluded.sample_rate,
                    bit_depth = excluded.bit_depth,
                    channels = excluded.channels,
                    comment = excluded.comment,
                    waveform = excluded.waveform"
            )?;

            for track in tracks {
                stmt.execute(params![
                    track.path, track.mtime, track.size, track.title, track.artist, track.album, track.genre,
                    track.duration, track.sample_rate, track.bit_depth, track.channels, track.comment, track.waveform
                ])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_track_waveform(&self, id: i64) -> Result<Option<Vec<u8>>> {
        let mut stmt = self.conn.prepare("SELECT waveform FROM tracks WHERE id = ?")?;
        let mut rows = stmt.query(params![id])?;
        if let Some(row) = rows.next()? {
            Ok(row.get(0)?)
        } else {
            Ok(None)
        }
    }

    fn row_to_track_no_waveform(&self, row: &rusqlite::Row) -> Result<Track> {
        Ok(Track {
            id: row.get(0)?,
            path: row.get(1)?,
            mtime: row.get(2)?,
            title: row.get(3)?,
            artist: row.get(4)?,
            album: row.get(5)?,
            genre: row.get(6)?,
            duration: row.get(7)?,
            sample_rate: row.get(8)?,
            bit_depth: row.get(9)?,
            channels: row.get(10)?,
            comment: row.get(11)?,
            waveform: None,
        })
    }

    fn row_to_track(&self, row: &rusqlite::Row) -> Result<Track> {
        Ok(Track {
            id: row.get(0)?,
            path: row.get(1)?,
            mtime: row.get(2)?,
            title: row.get(3)?,
            artist: row.get(4)?,
            album: row.get(5)?,
            genre: row.get(6)?,
            duration: row.get(7)?,
            sample_rate: row.get(8)?,
            bit_depth: row.get(9)?,
            channels: row.get(10)?,
            comment: row.get(11)?,
            waveform: row.get(12)?,
        })
    }

    pub fn get_track_by_path(&self, path: &str) -> Result<Option<Track>> {
        let mut stmt = self.conn.prepare("SELECT id, path, mtime, title, artist, album, genre, duration, sample_rate, bit_depth, channels, comment, waveform FROM tracks WHERE path = ?")?;
        let mut rows = stmt.query_map([path], |row| self.row_to_track(row))?;

        if let Some(track_res) = rows.next() {
            return Ok(Some(track_res?));
        }
        Ok(None)
    }

    pub fn get_all_tracks(&self) -> Result<Vec<Track>> {
        let mut stmt = self.conn.prepare("SELECT id, path, mtime, title, artist, album, genre, duration, sample_rate, bit_depth, channels, comment FROM tracks")?;
        let track_iter = stmt.query_map([], |row| self.row_to_track_no_waveform(row))?;

        let mut tracks = Vec::new();
        for track in track_iter {
            tracks.push(track?);
        }
        Ok(tracks)
    }

    pub fn remove_tracks_by_prefix(&self, prefix: &str) -> Result<()> {
        self.conn.execute(
            "DELETE FROM tracks WHERE path LIKE ?",
            params![format!("{}%", prefix)],
        )?;
        Ok(())
    }
}
