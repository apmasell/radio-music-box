use futures::{Stream, StreamExt};
use std::mem::swap;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

pub struct PausableStream<S: Stream> {
    pause_emitted: bool,
    state: Arc<Mutex<(bool, Option<Waker>)>>,
    stream: S,
    when_paused: S::Item,
}
#[derive(Clone)]
pub struct PauseResume(Arc<Mutex<(bool, Option<Waker>)>>);

impl<S: Stream> PausableStream<S> {
    pub fn new(stream: S, is_paused: bool, when_paused: S::Item) -> (Self, PauseResume) {
        let state = Arc::new(Mutex::new((is_paused, None)));

        (
            PausableStream {
                pause_emitted: false,
                state: state.clone(),
                stream,
                when_paused,
            },
            PauseResume(state),
        )
    }
}

impl<S: Stream + Unpin> Stream for PausableStream<S>
where
    S::Item: Clone + Unpin,
{
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let PausableStream {
            pause_emitted,
            state,
            stream,
            when_paused,
        } = self.get_mut();
        let mut guard = state
            .lock()
            .expect("Failed to unlock state in pausable stream");
        let (is_paused, waker) = &mut *guard;
        if *is_paused {
            if waker
                .as_ref()
                .map(|waker| !waker.will_wake(cx.waker()))
                .unwrap_or(true)
            {
                *waker = Some(cx.waker().clone());
            }
            if *pause_emitted {
                Poll::Pending
            } else {
                *pause_emitted = true;
                Poll::Ready(Some(when_paused.clone()))
            }
        } else {
            *pause_emitted = false;
            stream.poll_next_unpin(cx)
        }
    }
}

impl PauseResume {
    pub fn is_paused(&self) -> bool {
        let guard = self
            .0
            .lock()
            .expect("Failed to unlock state in pausable stream");
        let (is_paused, _) = &*guard;
        *is_paused
    }
    pub fn pause_resume(&self) -> bool {
        let mut guard = self
            .0
            .lock()
            .expect("Failed to unlock state in pausable stream");
        let (is_paused, waker) = &mut *guard;
        if *is_paused {
            *is_paused = false;
            let mut w = None;
            swap(&mut w, waker);
            if let Some(waker) = w {
                waker.wake();
            }
        } else {
            *is_paused = true;
        }
        *is_paused
    }
}
