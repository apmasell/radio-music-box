use futures::Stream;
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
        current_track: Option<(Box<dyn Decoder>, u32)>,
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
                eprintln!("Tracks in {}: {}", song.display(), tracks.len());
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
            let Ok(packet) = prober.format.next_packet() else {
                eprintln!("Bad packet in {}", song.display());
                return Poll::Ready(None);
            };
            let mut current_track_temp = None;
            swap(&mut current_track_temp, current_track);
            let (mut decoder, track_id) =
                match current_track_temp.filter(|(_, track_id)| packet.track_id() == *track_id) {
                    None => match tracks.pop_front() {
                        None => return Poll::Ready(None),
                        Some(track) => {
                            let Ok(decoder) = symphonia::default::get_codecs()
                                .make(&track.codec_params, &Default::default())
                            else {
                                eprintln!("Bad track in {}", song.display());
                                return Poll::Ready(None);
                            };
                            (decoder, track.id)
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
                        Some(Poll::Ready(Some(match data {
                            AudioBufferRef::U8(buffer) => stingy_copy(buffer.as_ref()),
                            AudioBufferRef::U16(buffer) => stingy_copy(buffer.as_ref()),
                            AudioBufferRef::U24(buffer) => stingy_copy(buffer.as_ref()),
                            AudioBufferRef::U32(buffer) => stingy_copy(buffer.as_ref()),
                            AudioBufferRef::S8(buffer) => stingy_copy(buffer.as_ref()),
                            AudioBufferRef::S16(buffer) => stingy_copy(buffer.as_ref()),
                            AudioBufferRef::S24(buffer) => stingy_copy(buffer.as_ref()),
                            AudioBufferRef::S32(buffer) => stingy_copy(buffer.as_ref()),
                            AudioBufferRef::F32(buffer) => stingy_copy(buffer.as_ref()),
                            AudioBufferRef::F64(buffer) => stingy_copy(buffer.as_ref()),
                        })))
                    }
                }
            };
            *current_track = Some((decoder, track_id));
            if let Some(result) = result {
                break result;
            } else {
                continue;
            }
        }
    }
}
fn stingy_copy<T: Sample + IntoSample<i16>>(input: &AudioBuffer<T>) -> AudioBuffer<i16> {
    let mut output = AudioBuffer::<i16>::new(
        input.frames() as u64,
        SignalSpec::new(44100, Channels::FRONT_LEFT | Channels::FRONT_RIGHT),
    );
    output.render_reserved(Some(input.frames()));
    for channel in 0..2 {
        for (dest, src) in output.chan_mut(channel).iter_mut().zip(input.chan(channel)) {
            *dest = (*src).into_sample();
        }
    }
    output
}
