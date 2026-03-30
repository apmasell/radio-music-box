use crate::SongList;
use crate::decoder::DecodedStream;
use crate::exit_filter::ExitFilter;
use crate::pausable_stream::{PausableStream, PauseResume};
use crate::playlist::Playlist;
use alsa::pcm::{Access, Format, HwParams, State};
use alsa::{Direction, PCM, ValueOr};
use futures::StreamExt;
use std::ffi::CString;
use std::thread;
use symphonia::core::audio::{AudioBuffer, Signal};
use tokio::sync::broadcast;

#[derive(Clone)]
enum NextBuffer {
    Buffer(AudioBuffer<i16>),
    Paused,
}

pub fn start(
    songs: SongList,
    exit: broadcast::Sender<()>,
    device: String,
    start_paused: bool,
) -> Result<PauseResume, Box<dyn std::error::Error>> {
    let device = CString::new(device.into_bytes())?;
    let pcm = PCM::open(&device, Direction::Playback, false)?;
    let hwp = HwParams::any(&pcm)?;
    hwp.set_channels(2)?;
    hwp.set_rate(44100, ValueOr::Nearest)?;
    hwp.set_format(Format::s16())?;
    hwp.set_access(Access::RWInterleaved)?;
    pcm.hw_params(&hwp)?;
    drop(hwp);

    let hwp = pcm.hw_params_current()?;
    let swp = pcm.sw_params_current()?;
    swp.set_start_threshold(hwp.get_buffer_size()?)?;
    pcm.sw_params(&swp)?;
    drop(hwp);
    drop(swp);

    let (stream, pause_resume) = PausableStream::new(
        Playlist::from(songs)
            .flat_map(DecodedStream::from)
            .map(NextBuffer::Buffer),
        start_paused,
        NextBuffer::Paused,
    );
    let stream = ExitFilter::new(exit, stream);

    thread::spawn(move || {
        let io = match pcm.io_i16() {
            Ok(io) => io,
            Err(e) => {
                eprintln!("Failed to get ALSA IO handle: {}", e);
                return;
            }
        };

        for buffer in futures::executor::block_on_stream(stream) {
            if pcm.state() == State::Setup {
                if let Err(e) = pcm.prepare() {
                    eprintln!("Failed to prepare to ALSA: {}", e);
                    return;
                }
            }
            match buffer {
                NextBuffer::Buffer(buffer) => {
                    let mut offset = 0;
                    while offset < buffer.frames() {
                        let mut interleaved = Vec::with_capacity(buffer.frames() * 2);
                        for frame in 0..buffer.frames() {
                            interleaved.push(buffer.chan(0)[frame]);
                            interleaved.push(buffer.chan(1)[frame]);
                        }
                        match io.writei(&interleaved) {
                            Ok(written) => offset += written,
                            Err(e) => {
                                eprintln!("Failed to write to ALSA: {}", e);
                                return;
                            }
                        }
                    }
                    if pcm.state() != State::Running {
                        if let Err(e) = pcm.start() {
                            eprintln!("Failed to start to ALSA: {}", e);
                            return;
                        }
                    }
                }
                NextBuffer::Paused => {
                    if pcm.state() == State::Running {
                        if let Err(e) = pcm.drain() {
                            eprintln!("Failed to start to ALSA: {}", e);
                            return;
                        }
                    }
                }
            }
        }
    });

    Ok(pause_resume)
}
