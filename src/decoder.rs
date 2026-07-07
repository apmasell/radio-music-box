use futures::Stream;
use rubato::audioadapter_buffers::direct::InterleavedSliceOfVecs;
use rubato::{Fft, FixedSync, Resampler};
use std::collections::VecDeque;
use std::fs::File;
use std::mem::swap;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use symphonia::core::audio::conv::IntoSample;
use symphonia::core::audio::sample::Sample;
use symphonia::core::audio::{
    Audio, AudioBuffer, AudioMut, AudioSpec, Channels, GenericAudioBufferRef, Position,
};
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
    type Item = AudioBuffer<i16>;

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
            let (mut decoder, track_id, mut resampler) =
                match current_track_temp.filter(|(_, track_id, _)| packet.track_id == *track_id) {
                    None => match tracks.pop_front() {
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
                    },
                    Some(value) => value,
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
                        Some(Poll::Ready(match data {
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
                        }))
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
        inputs: Vec<Vec<f32>>,
        resampler: Fft<f32>,
    },
}
impl ResamplingCopy {
    pub fn new(rate: u32) -> Option<Self> {
        if rate == 44100 {
            Some(ResamplingCopy::Matched)
        } else {
            match Fft::<f32>::new(rate as usize, 44100, 1024, 2, 2, FixedSync::Input) {
                Ok(resampler) => Some(ResamplingCopy::Resample {
                    inputs: vec![Vec::new(), Vec::new()],
                    resampler,
                }),
                Err(e) => {
                    eprintln!("Failed to construct resampler: {}", e);
                    None
                }
            }
        }
    }
    pub fn append<T: Sample + IntoSample<i16> + IntoSample<f32>>(
        &mut self,
        input: &AudioBuffer<T>,
    ) -> Option<AudioBuffer<i16>> {
        match self {
            ResamplingCopy::Matched => {
                let mut output = AudioBuffer::<i16>::new(
                    AudioSpec::new(
                        44100,
                        Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
                    ),
                    input.frames(),
                );
                output.render_uninit(Some(input.frames()));
                for channel in 0..2 {
                    let Channels::Positioned(positions) = input.spec().channels() else {
                        continue;
                    };
                    let Some(dest) = output.plane_mut(channel) else {
                        return None;
                    };
                    let Some(src) = input.plane(if positions.contains(Position::FRONT_RIGHT) {
                        channel
                    } else {
                        0
                    }) else {
                        return None;
                    };
                    for (dest, src) in dest.iter_mut().zip(src) {
                        *dest = (*src).into_sample();
                    }
                }
                Some(output)
            }
            ResamplingCopy::Resample { inputs, resampler } => {
                let Channels::Positioned(positions) = input.spec().channels() else {
                    return None;
                };
                for channel in 0..2 {
                    let Some(plane) = input.plane(if positions.contains(Position::FRONT_RIGHT) {
                        channel
                    } else {
                        0
                    }) else {
                        continue;
                    };
                    inputs[channel].extend(
                        plane
                            .iter()
                            .map(|&s| <T as IntoSample<f32>>::into_sample(s)),
                    );
                }
                let mut buffer: Vec<_> = (0..2)
                    .into_iter()
                    .map(|_| vec![0f32; resampler.output_frames_next()])
                    .collect();

                let (input_consumed, output_frames) = match resampler.process_into_buffer(
                    &InterleavedSliceOfVecs::new(&inputs, 2, input.frames())
                        .expect("Failed to set up resampling input"),
                    &mut InterleavedSliceOfVecs::new_mut(
                        &mut buffer,
                        2,
                        resampler.output_frames_next(),
                    )
                    .expect("Failed to set up resampling output"),
                    None,
                ) {
                    Ok(v) => v,
                    Err(e) => {
                        eprintln!("Resampling error: {}", e);
                        return None;
                    }
                };
                for input in inputs {
                    input.drain(0..input_consumed);
                }
                let mut output = AudioBuffer::<i16>::new(
                    AudioSpec::new(
                        44100,
                        Channels::Positioned(Position::FRONT_LEFT | Position::FRONT_RIGHT),
                    ),
                    output_frames,
                );
                output.render_uninit(Some(output_frames));
                for (channel, buffer) in buffer.into_iter().enumerate() {
                    let Some(plane) = output.plane_mut(channel) else {
                        return None;
                    };
                    for (dest, src) in plane.iter_mut().zip(buffer) {
                        *dest = src.into_sample();
                    }
                }
                Some(output)
            }
        }
    }
}
