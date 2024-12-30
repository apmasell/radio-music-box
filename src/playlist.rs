use crate::SongList;
use futures::Stream;
use rand::seq::SliceRandom;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

pub struct Playlist {
    current: Vec<Arc<Path>>,
    all: SongList,
}

impl From<SongList> for Playlist {
    fn from(value: SongList) -> Self {
        Playlist {
            all: value,
            current: Default::default(),
        }
    }
}

impl Stream for Playlist {
    type Item = Arc<Path>;

    fn poll_next(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let playlist = self.get_mut();
        if let Some(song) = playlist.current.pop() {
            return Poll::Ready(Some(song));
        }
        match playlist.all.try_read() {
            Ok(guard) => {
                playlist.current.extend(guard.iter().cloned());
                playlist.current.shuffle(&mut rand::thread_rng());
                Poll::Ready(playlist.current.pop())
            }
            Err(_) => Poll::Pending,
        }
    }
}
