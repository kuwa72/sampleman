use symphonia::core::formats::{FormatReader, SeekMode, SeekTo};
use symphonia::core::codecs::{Decoder, DecoderOptions};
use symphonia::core::audio::SampleBuffer;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::probe::Hint;
use symphonia::core::errors::Error;
use std::path::Path;
use std::fs::File;
use rodio::Source;

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
