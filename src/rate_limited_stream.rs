use futures::{FutureExt, Stream};
use hyper::body::Bytes;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use symphonia::core::audio::{AudioBuffer, Signal};
use tokenbucket::TokenBucket;
use tokio::time::Sleep;

pub struct RateLimitedStream<S> {
    stream: S,
    bucket: TokenBucket,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl<S: Stream> RateLimitedStream<S>
where
    S::Item: Rated,
{
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            bucket: TokenBucket::new(S::Item::RATE, S::Item::RATE * 10.0),
            sleep: None,
        }
    }
}
pub trait Rated {
    const RATE: f64;
    fn quantity(&self) -> f64;
}
impl Rated for Bytes {
    const RATE: f64 = 100_000.0;
    fn quantity(&self) -> f64 {
        self.len() as f64
    }
}
impl Rated for AudioBuffer<i16> {
    const RATE: f64 = 44_100.0;

    fn quantity(&self) -> f64 {
        self.frames() as f64
    }
}

impl<S: Stream + Unpin> Stream for RateLimitedStream<S>
where
    S::Item: Rated,
{
    type Item = S::Item;

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
            Poll::Ready(Some(item)) => {
                if let Err(required_tokens) = bucket.acquire(item.quantity()) {
                    // Wake up a bit early to give processing time
                    let duration = ((required_tokens / S::Item::RATE * 1.0e6) as u64)
                        .checked_sub(100)
                        .unwrap_or(0);
                    let mut s = Box::pin(tokio::time::sleep(Duration::from_millis(duration)));
                    let _ = s.poll_unpin(cx);
                    *sleep = Some(s);
                }
                Poll::Ready(Some(item))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}
