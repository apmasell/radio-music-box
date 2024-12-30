use futures::{Stream, StreamExt};
use std::pin::Pin;
use std::task::{Context, Poll};
use tokio::sync::broadcast;

pub struct ExitFilter<S> {
    exit: broadcast::Receiver<()>,
    input: S,
}

impl<S> ExitFilter<S> {
    pub fn new(exit: broadcast::Sender<()>, input: S) -> ExitFilter<S> {
        ExitFilter {
            exit: exit.subscribe(),
            input,
        }
    }
}
impl<S: Stream + Unpin> Stream for ExitFilter<S> {
    type Item = S::Item;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let ef = self.get_mut();
        if let Ok(()) = ef.exit.try_recv() {
            Poll::Ready(None)
        } else {
            ef.input.poll_next_unpin(cx)
        }
    }
}
