use crate::SongList;
use futures::future::BoxFuture;
use futures::{FutureExt, Stream};
use rand::seq::SliceRandom;
use std::collections::BTreeSet;
use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use tokio::sync::OwnedRwLockReadGuard;

pub struct Playlist {
    current: Vec<Arc<Path>>,
    all: SongList,
    waiting: Option<BoxFuture<'static, OwnedRwLockReadGuard<BTreeSet<Arc<Path>>>>>,
}

impl From<SongList> for Playlist {
    fn from(value: SongList) -> Self {
        Playlist {
            all: value,
            current: Default::default(),
            waiting: None,
        }
    }
}

impl Stream for Playlist {
    type Item = Arc<Path>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let Playlist {
            current,
            all,
            waiting,
        } = self.get_mut();
        loop {
            if let Some(guard) = waiting.as_mut() {
                let Poll::Ready(guard) = guard.poll_unpin(cx) else {
                    return Poll::Pending;
                };
                current.extend(guard.iter().cloned());
                current.shuffle(&mut rand::rng());
            }
            *waiting = None;
            if let Some(song) = current.pop() {
                return Poll::Ready(Some(song));
            }
            *waiting = Some(all.clone().read_owned().boxed());
        }
    }
}
