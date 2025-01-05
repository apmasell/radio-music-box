use futures::{FutureExt, Stream};
use hyper::body::Bytes;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokenbucket::TokenBucket;
use tokio::time::Sleep;

pub struct RateLimitedStream<S> {
    stream: S,
    bucket: TokenBucket,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl<S> RateLimitedStream<S> {
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            bucket: TokenBucket::new(100_000.0, 200_000.0),
            sleep: None,
        }
    }
}

impl<S: Stream<Item = Bytes> + Unpin> Stream for RateLimitedStream<S> {
    type Item = Bytes;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let RateLimitedStream {
            stream,
            bucket,
            sleep,
        } = self.get_mut();
        if let Some(sleep) = sleep.as_mut() {
            if let Poll::Pending = sleep.poll_unpin(cx) {
                return Poll::Pending;
            }
        }
        *sleep = None;
        match Pin::new(stream).poll_next(cx) {
            Poll::Ready(None) => Poll::Ready(None),
            Poll::Ready(Some(bytes)) => {
                if let Err(rate) = bucket.acquire(bytes.len() as f64) {
                    let duration = (1.0e6 / rate) as u64;
                    let mut s = Box::pin(tokio::time::sleep(Duration::from_millis(duration)));
                    let _ = s.poll_unpin(cx);
                    *sleep = Some(s);
                }
                Poll::Ready(Some(bytes))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
