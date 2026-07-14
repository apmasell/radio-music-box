use futures::Stream;
use rubato::audioadapter_buffers::direct::InterleavedSlice;
use rubato::audioadapter_buffers::owned::InterleavedOwned;
use rubato::{
    Async, FixedAsync, ResampleError, Resampler, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};
use std::collections::VecDeque;
use std::fs::File;
use std::iter::repeat;
use std::mem::swap;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use symphonia::core::audio::conv::{FromSample, IntoSample};
use symphonia::core::audio::sample::Sample;
use symphonia::core::audio::{Audio, AudioBuffer, GenericAudioBufferRef, Position};
use symphonia::core::codecs::CodecParameters;
use symphonia::core::codecs::audio::AudioDecoder;
use symphonia::core::formats::{FormatReader, Track};
use symphonia::core::io::MediaSourceStream;

pub enum DecodedStream {
    Empty,
    Song {
        song: Arc<Path>,
        tracks: VecDeque<Track>,
        format_reader: Box<dyn FormatReader>,
        current_track: Option<(Box<dyn AudioDecoder>, u32, ResamplingCopy)>,
    },
}

impl From<Arc<Path>> for DecodedStream {
    fn from(song: Arc<Path>) -> Self {
        let Ok(file) = File::open(&song) else {
            eprintln!("Can't open {}", song.display());
            return DecodedStream::Empty;
        };
        let source = MediaSourceStream::new(Box::new(file), Default::default());
        match symphonia::default::get_probe().probe(
            &Default::default(),
            source,
            Default::default(),
            Default::default(),
        ) {
            Err(e) => {
                eprintln!("Failed to read {}: {}", song.display(), e);
                DecodedStream::Empty
            }
            Ok(format_reader) => {
                let tracks: VecDeque<_> = format_reader.tracks().into_iter().cloned().collect();
                if tracks.is_empty() {
                    eprintln!("No tracks in {}; Skipping", song.display());
                    return DecodedStream::Empty;
                }
                DecodedStream::Song {
                    song,
                    tracks,
                    format_reader,
                    current_track: None,
                }
            }
        }
    }
}

impl Stream for DecodedStream {
    type Item = Vec<Vec<f32>>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let DecodedStream::Song {
            song,
            format_reader,
            tracks,
            current_track,
        } = self.get_mut()
        else {
            return Poll::Ready(None);
        };
        loop {
            let packet = match format_reader.next_packet() {
                Ok(Some(packet)) => packet,
                Ok(None) => {
                    return Poll::Ready(None);
                }
                Err(e) => {
                    eprintln!("Bad packet in {}: {}", song.display(), e);
                    return Poll::Ready(None);
                }
            };
            let mut current_track_temp = None;
            swap(&mut current_track_temp, current_track);
            let still_current_track = match current_track_temp.as_mut() {
                Some((_, track_id, resampler)) => {
                    if packet.track_id == *track_id {
                        true
                    } else {
                        eprintln!("Draining resampler");
                        let output = resampler.drain();
                        if !output.is_empty() {
                            return Poll::Ready(Some(output));
                        }
                        false
                    }
                }
                None => false,
            };
            let (mut decoder, track_id, mut resampler) = if let Some(value) = current_track_temp
                && still_current_track
            {
                value
            } else {
                match tracks.pop_front() {
                    None => return Poll::Ready(None),
                    Some(track) => {
                        let Some(CodecParameters::Audio(params)) = &track.codec_params else {
                            continue;
                        };
                        let Ok(decoder) = symphonia::default::get_codecs()
                            .make_audio_decoder(params, &Default::default())
                        else {
                            eprintln!("Bad track in {}", song.display());
                            return Poll::Ready(None);
                        };
                        match track
                            .codec_params
                            .map(|params| match params {
                                CodecParameters::Audio(audio) => audio
                                    .sample_rate
                                    .map(|rate| ResamplingCopy::new(rate))
                                    .flatten(),
                                _ => None,
                            })
                            .flatten()
                        {
                            Some(resampler) => (decoder, track.id, resampler),
                            None => return Poll::Ready(None),
                        }
                    }
                }
            };

            let result = match decoder.decode(&packet) {
                Err(e) => {
                    eprintln!("Decode error in {}: {}", song.display(), e);
                    Some(Poll::Ready(None))
                }
                Ok(data) => {
                    if data.frames() == 0 {
                        None
                    } else {
                        let output = match data {
                            GenericAudioBufferRef::U8(buffer) => resampler.append(buffer),
                            GenericAudioBufferRef::U16(buffer) => resampler.append(buffer),
                            GenericAudioBufferRef::U24(buffer) => resampler.append(buffer),
                            GenericAudioBufferRef::U32(buffer) => resampler.append(buffer),
                            GenericAudioBufferRef::S8(buffer) => resampler.append(buffer),
                            GenericAudioBufferRef::S16(buffer) => resampler.append(buffer),
                            GenericAudioBufferRef::S24(buffer) => resampler.append(buffer),
                            GenericAudioBufferRef::S32(buffer) => resampler.append(buffer),
                            GenericAudioBufferRef::F32(buffer) => resampler.append(buffer),
                            GenericAudioBufferRef::F64(buffer) => resampler.append(buffer),
                        };
                        if output.is_empty() {
                            None
                        } else {
                            Some(Poll::Ready(Some(output)))
                        }
                    }
                }
            };
            *current_track = Some((decoder, track_id, resampler));
            if let Some(result) = result {
                break result;
            } else {
                continue;
            }
        }
    }
}
pub enum ResamplingCopy {
    Matched,
    Resample {
        buffer: Vec<f32>,
        resampler: Async<f32>,
    },
}

fn resample(buffer: &mut Vec<f32>, resampler: &mut Async<f32>) -> Vec<Vec<f32>> {
    let mut all_output = Vec::new();
    loop {
        let mut output = InterleavedOwned::new(0f32, 2, resampler.output_frames_next());
        let buffer_in = match InterleavedSlice::new(&buffer, 2, buffer.len() / 2) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("Failed to prepare input for resampling: {}", e);
                return vec![];
            }
        };
        let (input_frames, output_frames) =
            match resampler.process_into_buffer(&buffer_in, &mut output, None) {
                Ok(v) => v,
                Err(ResampleError::InsufficientInputBufferSize { .. }) => break,
                Err(e) => {
                    eprintln!("Resampling error: {}", e);
                    break;
                }
            };
        let mut output = output.take_data();
        output.truncate(2 * output_frames);
        drop(buffer_in);
        if input_frames != buffer.len() {
            buffer.drain(0..(2 * input_frames));
        } else {
            buffer.truncate(0);
        }
        all_output.push(output);
    }
    all_output
}
impl ResamplingCopy {
    pub fn new(rate: u32) -> Option<Self> {
        if rate == 44100 {
            Some(ResamplingCopy::Matched)
        } else {
            match Async::<f32>::new_sinc(
                44100.0 / (rate as f64),
                10.0,
                &SincInterpolationParameters {
                    sinc_len: 256,
                    f_cutoff: 0.95,
                    oversampling_factor: 128,
                    interpolation: SincInterpolationType::Cubic,
                    window: WindowFunction::BlackmanHarris2,
                },
                2048,
                2,
                FixedAsync::Output,
            ) {
                Ok(resampler) => Some(ResamplingCopy::Resample {
                    buffer: Vec::new(),
                    resampler,
                }),
                Err(e) => {
                    eprintln!("Failed to construct resampler: {}", e);
                    None
                }
            }
        }
    }
    pub fn append<T: Sample>(&mut self, input: &AudioBuffer<T>) -> Vec<Vec<f32>>
    where
        f32: FromSample<T>,
    {
        match self {
            ResamplingCopy::Matched => {
                let mut output = Vec::with_capacity(input.frames() * 2);
                input.copy_to_vec_interleaved(&mut output);
                vec![output]
            }
            ResamplingCopy::Resample { buffer, resampler } => {
                let start = buffer.len();
                buffer.resize(start + input.frames() * 2, 0.0);
                if input.num_planes() == 2 {
                    input.copy_to_slice_interleaved(&mut buffer[start..]);
                } else {
                    for frame in 0..input.frames() {
                        for (plane, position) in
                            [(0, Position::FRONT_LEFT), (1, Position::FRONT_RIGHT)]
                        {
                            buffer[start + frame * 2 + plane] = input
                                .plane_by_position(position)
                                .or(input.plane(0))
                                .expect("No plane available in audio")[frame]
                                .into_sample();
                        }
                    }
                }
                if resampler.input_frames_next() > buffer.len() {
                    vec![]
                } else {
                    resample(buffer, resampler)
                }
            }
        }
    }
    pub fn drain(&mut self) -> Vec<Vec<f32>> {
        match self {
            ResamplingCopy::Matched => vec![],
            ResamplingCopy::Resample { buffer, resampler } => {
                if buffer.is_empty() {
                    return vec![];
                }
                buffer.extend(repeat(0.0f32).take(resampler.input_frames_next() - buffer.len()));
                resample(buffer, resampler)
            }
        }
    }
}
