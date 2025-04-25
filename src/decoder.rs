use futures::Stream;
use rubato::{FftFixedIn, Resampler};
use std::collections::VecDeque;
use std::fs::File;
use std::mem::swap;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use symphonia::core::audio::{AudioBuffer, AudioBufferRef, Channels, Signal, SignalSpec};
use symphonia::core::codecs::Decoder;
use symphonia::core::conv::IntoSample;
use symphonia::core::errors::Error;
use symphonia::core::formats::Track;
use symphonia::core::io::MediaSourceStream;
use symphonia::core::probe::ProbeResult;
use symphonia::core::sample::Sample;

pub enum DecodedStream {
    Empty,
    Song {
        song: Arc<Path>,
        tracks: VecDeque<Track>,
        prober: ProbeResult,
        current_track: Option<(Box<dyn Decoder>, u32, ResamplingCopy)>,
    },
}

impl From<Arc<Path>> for DecodedStream {
    fn from(song: Arc<Path>) -> Self {
        let Ok(file) = File::open(&song) else {
            eprintln!("Can't open {}", song.display());
            return DecodedStream::Empty;
        };
        let source = MediaSourceStream::new(Box::new(file), Default::default());
        match symphonia::default::get_probe().format(
            &Default::default(),
            source,
            &Default::default(),
            &Default::default(),
        ) {
            Err(e) => {
                eprintln!("Failed to read {}: {}", song.display(), e);
                DecodedStream::Empty
            }
            Ok(prober) => {
                let tracks: VecDeque<_> = prober.format.tracks().into_iter().cloned().collect();
                if tracks.is_empty() {
                    eprintln!("No tracks in {}; Skipping", song.display());
                    return DecodedStream::Empty;
                }
                DecodedStream::Song {
                    song,
                    tracks,
                    prober,
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
            prober,
            tracks,
            current_track,
        } = self.get_mut()
        else {
            return Poll::Ready(None);
        };
        loop {
            let packet = match prober.format.next_packet() {
                Ok(packet) => packet,
                Err(Error::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                    return Poll::Ready(None);
                }
                Err(e) => {
                    eprintln!("Bad packet in {}: {}", song.display(), e);
                    return Poll::Ready(None);
                }
            };
            let mut current_track_temp = None;
            swap(&mut current_track_temp, current_track);
            let (mut decoder, track_id, mut resampler) = match current_track_temp
                .filter(|(_, track_id, _)| packet.track_id() == *track_id)
            {
                None => match tracks.pop_front() {
                    None => return Poll::Ready(None),
                    Some(track) => {
                        let Ok(decoder) = symphonia::default::get_codecs()
                            .make(&track.codec_params, &Default::default())
                        else {
                            eprintln!("Bad track in {}", song.display());
                            return Poll::Ready(None);
                        };
                        match track
                            .codec_params
                            .sample_rate
                            .map(|rate| ResamplingCopy::new(rate))
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
                            AudioBufferRef::U8(buffer) => resampler.append(buffer.as_ref()),
                            AudioBufferRef::U16(buffer) => resampler.append(buffer.as_ref()),
                            AudioBufferRef::U24(buffer) => resampler.append(buffer.as_ref()),
                            AudioBufferRef::U32(buffer) => resampler.append(buffer.as_ref()),
                            AudioBufferRef::S8(buffer) => resampler.append(buffer.as_ref()),
                            AudioBufferRef::S16(buffer) => resampler.append(buffer.as_ref()),
                            AudioBufferRef::S24(buffer) => resampler.append(buffer.as_ref()),
                            AudioBufferRef::S32(buffer) => resampler.append(buffer.as_ref()),
                            AudioBufferRef::F32(buffer) => resampler.append(buffer.as_ref()),
                            AudioBufferRef::F64(buffer) => resampler.append(buffer.as_ref()),
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
        resampler: FftFixedIn<f32>,
    },
}
impl ResamplingCopy {
    pub fn new(rate: u32) -> Option<Self> {
        if rate == 44100 {
            Some(ResamplingCopy::Matched)
        } else {
            match FftFixedIn::<f32>::new(rate as usize, 44100, 1024, 2, 2) {
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
                    input.frames() as u64,
                    SignalSpec::new(44100, Channels::FRONT_LEFT | Channels::FRONT_RIGHT),
                );
                output.render_reserved(Some(input.frames()));
                for channel in 0..2 {
                    for (dest, src) in output.chan_mut(channel).iter_mut().zip(input.chan(
                        if input.spec().channels.contains(Channels::FRONT_RIGHT) {
                            channel
                        } else {
                            0
                        },
                    )) {
                        *dest = (*src).into_sample();
                    }
                }
                Some(output)
            }
            ResamplingCopy::Resample { inputs, resampler } => {
                for channel in 0..2 {
                    inputs[channel].extend(
                        input
                            .chan(if input.spec().channels.contains(Channels::FRONT_RIGHT) {
                                channel
                            } else {
                                0
                            })
                            .iter()
                            .map(|&s| <T as IntoSample<f32>>::into_sample(s)),
                    );
                }
                let mut buffer: Vec<_> = (0..2)
                    .into_iter()
                    .map(|_| vec![0f32; resampler.output_frames_next()])
                    .collect();

                let (input_consumed, output_frames) =
                    match resampler.process_partial_into_buffer(Some(inputs), &mut buffer, None) {
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
                    output_frames as u64,
                    SignalSpec::new(44100, Channels::FRONT_LEFT | Channels::FRONT_RIGHT),
                );
                output.render_reserved(Some(output_frames));
                for (channel, buffer) in buffer.into_iter().enumerate() {
                    for (dest, src) in output.chan_mut(channel).iter_mut().zip(buffer) {
                        *dest = src.into_sample();
                    }
                }
                Some(output)
            }
        }
    }
}
