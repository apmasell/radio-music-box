mod encoder;

use async_watcher::notify::{Error, ErrorKind};
use async_watcher::{
    AsyncDebouncer, DebouncedEvent,
    notify::{EventKind, RecursiveMode},
};
use clap::Parser;
use futures::FutureExt;
use futures::future::BoxFuture;
use http_body_util::{Full, StreamBody};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::header::CONTENT_TYPE;
use hyper::service::Service;
use hyper::{Method, Request, Response, StatusCode, http};
use hyper_util::rt::TokioIo;
use rand::seq::SliceRandom;
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::fs::File;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use symphonia::core::audio::{AudioBuffer, Channels, Signal, SignalSpec};
use symphonia::core::io::MediaSourceStream;
use tokio::net::TcpListener;
use tokio::signal::unix::SignalKind;
use tokio::sync::{Mutex, broadcast, mpsc};
use tokio_stream::wrappers::ReceiverStream;
use walkdir::WalkDir;

type SongList = Arc<Mutex<BTreeSet<Arc<Path>>>>;
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Arguments {
    #[arg(short, long)]
    port: u16,
    #[arg(value_name = "DIRECTORY")]
    path: PathBuf,
}
fn scan(path: &Path) -> BTreeSet<Arc<Path>> {
    let result: BTreeSet<Arc<Path>> = WalkDir::new(path)
        .follow_links(true)
        .into_iter()
        .filter_map(|entry| match entry {
            Err(e) => {
                eprintln!("Failed searching files: {}", e);
                None
            }
            Ok(entry) => {
                if entry.file_type().is_file() {
                    Some(entry.into_path().into())
                } else {
                    None
                }
            }
        })
        .collect();
    eprintln!("Scanned {} files", result.len());
    result
}
pub type StreamWrite = Result<Frame<Bytes>, Infallible>;
async fn play(songs: SongList, exit: broadcast::Receiver<()>, output: mpsc::Sender<StreamWrite>) {
    let Ok(mut encoder) = encoder::Encoder::new(output, exit) else {
        return;
    };
    let mut current_songs = Vec::new();
    let mut progress = false;
    loop {
        if current_songs.is_empty() {
            current_songs.extend(songs.clone().lock().await.iter().cloned());
            current_songs.shuffle(&mut rand::thread_rng());
        }
        'playlist: while let Some(song) = current_songs.pop() {
            eprintln!("Trying to play {}", song.display());
            let Ok(file) = File::open(&song) else {
                eprintln!("Can't open {}", song.display());
                continue;
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
                }
                Ok(mut prober) => {
                    let tracks: Vec<_> = prober.format.tracks().into_iter().cloned().collect();
                    if tracks.is_empty() {
                        eprintln!("No tracks in {}; Skipping", song.display());
                        continue;
                    }
                    eprintln!("Tracks in {}: {}", song.display(), tracks.len());
                    for track in tracks {
                        let Ok(mut decoder) = symphonia::default::get_codecs()
                            .make(&track.codec_params, &Default::default())
                        else {
                            eprintln!("Bad track in {}", song.display());
                            continue;
                        };
                        loop {
                            let Ok(packet) = prober.format.next_packet() else {
                                eprintln!("Bad packet in {}", song.display());
                                break 'playlist;
                            };
                            if packet.track_id() != track.id {
                                continue;
                            }

                            match decoder.decode(&packet) {
                                Err(e) => {
                                    eprintln!("Decode error in {}: {}", song.display(), e);
                                    break 'playlist;
                                }
                                Ok(data) => {
                                    let mut output_buffer = AudioBuffer::<i16>::new(
                                        data.capacity() as u64,
                                        SignalSpec::new(
                                            44100,
                                            Channels::FRONT_LEFT | Channels::FRONT_RIGHT,
                                        ),
                                    );
                                    data.convert(&mut output_buffer);
                                    if encoder
                                        .write(output_buffer.chan(0), output_buffer.chan(1))
                                        .await
                                        .is_err()
                                    {
                                        eprintln!("Encoding error. Giving up on stream.");
                                        return;
                                    }
                                }
                            }
                        }
                    }
                    progress = true;
                }
            }

            if !progress {
                eprintln!("Nothing to play. Giving up on stream.");
                encoder.flush().await;
                return;
            }
        }
    }
}

#[derive(Clone)]
struct Songs {
    songs: SongList,
    exit: broadcast::Sender<()>,
}
type BoxedBody = Box<dyn Body<Data = Bytes, Error = Infallible> + Unpin + Send + 'static>;
impl Service<Request<Incoming>> for Songs {
    type Response = Response<BoxedBody>;
    type Error = http::Error;
    type Future = BoxFuture<'static, Result<Self::Response, Self::Error>>;

    fn call(&self, req: Request<Incoming>) -> Self::Future {
        let Songs { songs, exit } = self.clone();
        async move {
            match (req.method(), req.uri().path()) {
                (&Method::GET, "/") => {
                    Response::builder()
                        .header(CONTENT_TYPE, "text/html")
                        .body(
                            Box::new(Full::new(Bytes::from(&include_bytes!("audio.html")[..])))
                                as BoxedBody,
                        )
                }
                (&Method::GET, "/stream.mp3") => {
                    let (tx, rx) = mpsc::channel(5);
                    tokio::spawn(play(songs, exit.subscribe(), tx));

                    Response::builder()
                        .header(CONTENT_TYPE, "audio/mp3")
                        .body(Box::new(StreamBody::new(ReceiverStream::new(rx))) as BoxedBody)
                }
                _ => Response::builder()
                    .status(StatusCode::NOT_FOUND)
                    .body(Box::new(Full::new(Bytes::from("Not found"))) as BoxedBody),
            }
        }
        .boxed()
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Arguments {
        port,
        path: root_path,
    } = Arguments::parse();
    let songs = Arc::new(Mutex::new(scan(&root_path)));
    let (exit_tx, mut exit_rx) = tokio::sync::broadcast::channel(1);

    {
        let songs = songs.clone();
        let mut exit_rx = exit_tx.subscribe();
        let (mut debouncer, mut file_events) =
            AsyncDebouncer::new_with_channel(Duration::from_secs(1), Some(Duration::from_secs(1)))
                .await?;
        debouncer
            .watcher()
            .watch(&root_path, RecursiveMode::Recursive)
            .expect("Failed to scan directory");
        tokio::spawn(async move {
            use async_watcher::notify::Error as NotifyError;
            enum WatcherEvent {
                Files(Option<Result<Vec<DebouncedEvent>, Vec<NotifyError>>>),
                Exit,
                Rescan,
            }
            let Ok(hup) = tokio::signal::unix::signal(SignalKind::hangup()) else {
                eprintln!("Couldn't bind to SIGHUP");
                return;
            };
            let mut hup = Box::pin(hup);
            loop {
                let event = tokio::select! {
                    e = file_events.recv() => WatcherEvent::Files(e),
                    _ = exit_rx.recv() => WatcherEvent::Exit,
                    _ = hup.recv() => WatcherEvent::Rescan,
                };
                match event {
                    WatcherEvent::Exit | WatcherEvent::Files(None) => break,
                    WatcherEvent::Rescan => {
                        let mut songs = songs.lock().await;
                        *songs = scan(&root_path);
                    }
                    WatcherEvent::Files(Some(Ok(events))) => {
                        let mut songs = songs.lock().await;
                        for DebouncedEvent { path, event, .. } in events {
                            match event.kind {
                                EventKind::Any => {}
                                EventKind::Access(_) => {}
                                EventKind::Create(_) => {
                                    songs.insert(path.into());
                                }
                                EventKind::Modify(_) => {}
                                EventKind::Remove(_) => {
                                    songs.remove(path.as_path());
                                }
                                EventKind::Other => {
                                    eprintln!(
                                        "Got an other event for {}. Triggering rescan.",
                                        path.display()
                                    );
                                    *songs = scan(&root_path)
                                }
                            }
                        }
                    }
                    WatcherEvent::Files(Some(Err(errors))) => {
                        for Error { kind, paths } in errors {
                            match kind {
                                ErrorKind::Generic(e) => {
                                    eprintln!("Error watching files: {}", e);
                                }
                                ErrorKind::Io(e) => {
                                    eprintln!("Error watching files: {}", e)
                                }
                                ErrorKind::PathNotFound => {
                                    let mut songs = songs.lock().await;
                                    for path in paths {
                                        songs.remove(path.as_path());
                                    }
                                }
                                ErrorKind::WatchNotFound => {
                                    eprintln!("Watch not found");
                                }
                                ErrorKind::InvalidConfig(_) => {
                                    eprintln!("Invalid config");
                                }
                                ErrorKind::MaxFilesWatch => {
                                    eprintln!("Watching too many files")
                                }
                            }
                        }
                    }
                }
            }
        });
    }

    let songs = Songs {
        songs,
        exit: exit_tx.clone(),
    };
    let listener =
        TcpListener::bind(&SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)).await?;
    if let Ok(addr) = listener.local_addr() {
        eprintln!("Listening on {}", addr);
    }
    tokio::spawn(async move {
        loop {
            let connection = tokio::select! {biased;
                _ = exit_rx.recv() => break,
                r = listener.accept() => r,
            };
            match connection {
                Ok((stream, addr)) => {
                    if let Err(e) = hyper::server::conn::http1::Builder::new()
                        .serve_connection(TokioIo::new(stream), songs.clone())
                        .await
                    {
                        eprintln!("Failed to handle incoming connection for {}: {}", e, addr);
                    }
                }
                Err(e) => {
                    eprintln!("Failed to accept connection: {}", e);
                }
            }
        }
    });
    if let Err(e) = tokio::signal::ctrl_c().await {
        eprintln!("Error waiting for ^C: {}", e);
    }

    eprintln!("Shutting down...");

    exit_tx.send(()).expect("Failed to shutdown");

    Ok(())
}
