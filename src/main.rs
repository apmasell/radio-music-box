mod decoder;
mod encoder;
mod exit_filter;
mod playlist;
mod rate_limited_stream;
mod scanner;

use crate::decoder::DecodedStream;
use crate::encoder::EncodedStream;
use crate::exit_filter::ExitFilter;
use crate::playlist::Playlist;
use crate::rate_limited_stream::RateLimitedStream;
use clap::Parser;
use futures::future::BoxFuture;
use futures::{FutureExt, StreamExt};
use http_body_util::{Full, StreamBody};
use hyper::body::{Body, Bytes, Frame, Incoming};
use hyper::header::{CACHE_CONTROL, CONTENT_TYPE};
use hyper::service::Service;
use hyper::{Method, Request, Response, StatusCode, http};
use hyper_util::rt::TokioIo;
use std::collections::BTreeSet;
use std::convert::Infallible;
use std::net::{IpAddr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::net::TcpListener;
use tokio::sync::{RwLock, broadcast};

type SongList = Arc<RwLock<BTreeSet<Arc<Path>>>>;
#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Arguments {
    #[arg(short, long)]
    port: u16,
    #[arg(value_name = "DIRECTORY")]
    path: PathBuf,
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
                    match EncodedStream::new(ExitFilter::new(
                        exit,
                        Playlist::from(songs).flat_map(DecodedStream::from),
                    )) {
                        Ok(stream) => Response::builder()
                            .header(CONTENT_TYPE, "audio/mp3")
                            .header(CACHE_CONTROL, "no-cache")
                            .body(Box::new(StreamBody::new(
                                RateLimitedStream::new(stream).map(|data| Ok(Frame::data(data))),
                            )) as BoxedBody),
                        Err(_) => Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(Box::new(Full::new(Bytes::from(
                                "Failed to initalise audio encoder",
                            ))) as BoxedBody),
                    }
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
    let (exit_tx, mut exit_rx) = broadcast::channel(1);
    let songs = scanner::create_scanner(root_path, &exit_tx).await?;
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
