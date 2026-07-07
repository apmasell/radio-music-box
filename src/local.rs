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
use symphonia::core::audio::{Audio, AudioBuffer};
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
    let thread_name = format!("Player for {}", &device);
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
    let mut stream = ExitFilter::new(exit, stream);
    let (sender, mut receiver) = tokio::sync::mpsc::channel(10);
    tokio::spawn(async move {
        while let Some(buffer) = stream.next().await {
            if let Err(_) = sender.send(buffer).await {
                eprintln!("Quitting ALSA stream");
                return;
            }
        }
    });

    thread::Builder::new().name(thread_name).spawn(move || {
        let io = match pcm.io_i16() {
            Ok(io) => io,
            Err(e) => {
                eprintln!("Failed to get ALSA IO handle: {}", e);
                return;
            }
        };

        while let Some(buffer) = receiver.blocking_recv() {
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
                            let Some(left_plane) = buffer.plane(0) else {
                                continue;
                            };
                            let Some(right_plane) = buffer.plane(1) else {
                                continue;
                            };
                            interleaved.push(left_plane[frame]);
                            interleaved.push(right_plane[frame]);
                        }
                        match io.writei(&interleaved) {
                            Ok(written) => offset += written,
                            Err(e) if e.errno() == libc::EPIPE => {
                                if let Err(e) = pcm.recover(libc::EPIPE, false) {
                                    eprintln!("Failed to recover from ALSA underrun: {}", e);
                                    return;
                                }
                            }
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
    })?;

    Ok(pause_resume)
}
