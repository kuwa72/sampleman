use symphonia::core::formats::{FormatReader, SeekMode, SeekTo};
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::probe::Hint;
use symphonia::core::errors::Error;
use std::path::Path;
use std::fs::File;
use rodio::Source;
use rustysynth::{Synthesizer, SynthesizerSettings, SoundFont, MidiFile, MidiFileSequencer};
use std::sync::Arc;
use std::io::BufReader;
use std::env;

pub struct SymphoniaSource {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_rate: u32,
    channels: u16,
    sample_buf: Option<SampleBuffer<f32>>,
    buf_index: usize,
    seek_rx: crossbeam_channel::Receiver<f64>,
}

impl SymphoniaSource {
    pub fn new<P: AsRef<Path>>(path: P, seek_rx: crossbeam_channel::Receiver<f64>) -> anyhow::Result<Self> {
        let file = File::open(path.as_ref())?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        
        // Feed extension hint
        if let Some(ext) = Path::new(path.as_ref()).extension().and_then(|s| s.to_str()) {
            hint.with_extension(ext);
        }

        let probed = symphonia::default::get_probe().format(
            &hint,
            mss,
            &Default::default(),
            &Default::default(),
        )?;
        let format = probed.format;

        let track = format.tracks().get(0)
            .ok_or_else(|| anyhow::anyhow!("No tracks found in file"))?;
        let track_id = track.id;
        let codec_params = &track.codec_params;

        let sample_rate = codec_params.sample_rate.unwrap_or(44100);
        let channels = codec_params.channels.map(|c| c.count() as u16).unwrap_or(2);

        let decoder = symphonia::default::get_codecs().make(
            codec_params,
            &DecoderOptions::default(),
        )?;

        Ok(Self {
            format,
            decoder,
            track_id,
            sample_rate,
            channels,
            sample_buf: None,
            buf_index: 0,
            seek_rx,
        })
    }
}

impl Iterator for SymphoniaSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        // 1. Process seek requests
        if let Ok(secs) = self.seek_rx.try_recv() {
            let ts = symphonia::core::units::Time::new(secs as u64, secs.fract());
            let _ = self.format.seek(SeekMode::Coarse, SeekTo::Time { time: ts, track_id: None });
            let _ = self.decoder.reset();
            self.sample_buf = None;
            self.buf_index = 0;
        }

        // 2. Continuous sample output from current buffer
        if let Some(ref buf) = self.sample_buf {
            let samples = buf.samples();
            if self.buf_index < samples.len() {
                let s = samples[self.buf_index];
                self.buf_index += 1;
                return Some(s);
            }
        }

        // 3. Decode next packet into buffer when exhausted
        self.sample_buf = None;
        self.buf_index = 0;

        while let Ok(packet) = self.format.next_packet() {
            if packet.track_id() != self.track_id {
                continue;
            }
            match self.decoder.decode(&packet) {
                Ok(decoded) => {
                    let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, *decoded.spec());
                    buf.copy_interleaved_ref(decoded);
                    self.sample_buf = Some(buf);
                    break;
                }
                Err(Error::DecodeError(_)) => continue,
                Err(e) => {
                    eprintln!("Symphonia decode error: {}", e);
                    return None;
                }
            }
        }

        if let Some(ref buf) = self.sample_buf {
            let samples = buf.samples();
            if !samples.is_empty() {
                self.buf_index = 1;
                return Some(samples[0]);
            }
        }
        None
    }
}

impl Source for SymphoniaSource {
    fn current_frame_len(&self) -> Option<usize> { None }
    fn channels(&self) -> u16 { self.channels }
    fn sample_rate(&self) -> u32 { self.sample_rate }
    fn total_duration(&self) -> Option<std::time::Duration> { None }
}

pub struct MidiSource {
    sequencer: MidiFileSequencer,
    midi: Arc<MidiFile>,
    left_buf: Vec<f32>,
    right_buf: Vec<f32>,
    buf_index: usize,
    sample_rate: u32,
    channels: u16,
    seek_rx: crossbeam_channel::Receiver<f64>,
}

impl MidiSource {
    pub fn new<P: AsRef<Path>>(path: P, seek_rx: crossbeam_channel::Receiver<f64>) -> anyhow::Result<Self> {
        let soundfont_path = "soundfont.sf2"; // Fallback in current dir
        
        // Try current dir first, then look in exe dir
        let soundfont_file = {
            if Path::new(soundfont_path).exists() {
                File::open(soundfont_path)?
            } else {
                let exe_dir = env::current_exe()?.parent().map(|p| p.to_path_buf()).unwrap_or_else(|| Path::new(".").to_path_buf());
                let exe_sf = exe_dir.join(soundfont_path);
                if exe_sf.exists() {
                    File::open(exe_sf)?
                } else {
                    return Err(anyhow::anyhow!("SoundFont file 'soundfont.sf2' not found. Please place it in the project root."));
                }
            }
        };

        let mut sf_reader = BufReader::new(soundfont_file);
        let soundfont = Arc::new(SoundFont::new(&mut sf_reader).map_err(|e| anyhow::anyhow!("Failed to load SoundFont: {:?}", e))?);
        
        let settings = SynthesizerSettings::new(44100);
        let synthesizer = Synthesizer::new(&soundfont, &settings).map_err(|e| anyhow::anyhow!("Failed to create synthesizer: {:?}", e))?;
        
        let file = File::open(path.as_ref())?;
        let mut midi_reader = BufReader::new(file);
        let midi = MidiFile::new(&mut midi_reader).map_err(|e| anyhow::anyhow!("Failed to load MIDI file: {:?}", e))?;
        let midi_arc = Arc::new(midi);
        
        let mut sequencer = MidiFileSequencer::new(synthesizer);
        sequencer.play(&midi_arc, false);
        
        Ok(Self {
            sequencer,
            midi: midi_arc,
            left_buf: vec![0.0; 512],
            right_buf: vec![0.0; 512],
            buf_index: 512, // Force render on first next()
            sample_rate: 44100,
            channels: 2,
            seek_rx,
        })
    }
}

impl Iterator for MidiSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        // 1. Process seek requests
        if let Ok(secs) = self.seek_rx.try_recv() {
            // Re-play from start
            self.sequencer.play(&self.midi, false);
            
            // Fast-forward to target time
            let mut skip_left = vec![0.0; 1024];
            let mut skip_right = vec![0.0; 1024];
            while self.sequencer.get_position() < secs && !self.sequencer.end_of_sequence() {
                self.sequencer.render(&mut skip_left, &mut skip_right);
            }
            self.buf_index = self.left_buf.len(); // Force refills
        }

        // 2. Play from buffer
        if self.buf_index < self.left_buf.len() * 2 {
            let s = if self.buf_index % 2 == 0 {
                self.left_buf[self.buf_index / 2]
            } else {
                self.right_buf[self.buf_index / 2]
            };
            self.buf_index += 1;
            return Some(s);
        }

        // 3. Render next chunk
        if self.sequencer.end_of_sequence() {
            return None;
        }

        self.sequencer.render(&mut self.left_buf, &mut self.right_buf);
        self.buf_index = 0;

        if !self.left_buf.is_empty() {
            let s = self.left_buf[0];
            self.buf_index = 1;
            return Some(s);
        }

        None
    }
}

impl Source for MidiSource {
    fn current_frame_len(&self) -> Option<usize> { None }
    fn channels(&self) -> u16 { self.channels }
    fn sample_rate(&self) -> u32 { self.sample_rate }
    fn total_duration(&self) -> Option<std::time::Duration> { None }
}

pub enum DynamicSource {
    Symphonia(SymphoniaSource),
    Midi(MidiSource),
}

impl Iterator for DynamicSource {
    type Item = f32;

    fn next(&mut self) -> Option<Self::Item> {
        match self {
            DynamicSource::Symphonia(s) => s.next(),
            DynamicSource::Midi(s) => s.next(),
        }
    }
}

impl Source for DynamicSource {
    fn current_frame_len(&self) -> Option<usize> {
        match self {
            DynamicSource::Symphonia(s) => s.current_frame_len(),
            DynamicSource::Midi(s) => s.current_frame_len(),
        }
    }

    fn channels(&self) -> u16 {
        match self {
            DynamicSource::Symphonia(s) => s.channels(),
            DynamicSource::Midi(s) => s.channels(),
        }
    }

    fn sample_rate(&self) -> u32 {
        match self {
            DynamicSource::Symphonia(s) => s.sample_rate(),
            DynamicSource::Midi(s) => s.sample_rate(),
        }
    }

    fn total_duration(&self) -> Option<std::time::Duration> {
        match self {
            DynamicSource::Symphonia(s) => s.total_duration(),
            DynamicSource::Midi(s) => s.total_duration(),
        }
    }
}
