use futures::{FutureExt, Stream};
use hyper::body::Bytes;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::{Duration, SystemTime};
use symphonia::core::audio::{AudioBuffer, Signal};
use tokio::time::Sleep;

pub struct RateLimitedStream<S> {
    stream: S,
    tokens: i128,
    last_checked: SystemTime,
    sleep: Option<Pin<Box<Sleep>>>,
}

impl<S: Stream> RateLimitedStream<S>
where
    S::Item: Rated,
{
    pub fn new(stream: S) -> Self {
        Self {
            stream,
            tokens: (S::Item::RATE * 1_000) as i128,
            last_checked: SystemTime::now(),
            sleep: None,
        }
    }
}
pub trait Rated {
    const RATE: u64;
    fn quantity(&self) -> u64;
}
impl Rated for Bytes {
    const RATE: u64 = 100_000;
    fn quantity(&self) -> u64 {
        self.len() as u64
    }
}
impl Rated for AudioBuffer<i16> {
    const RATE: u64 = 44_100;

    fn quantity(&self) -> u64 {
        self.frames() as u64
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
            tokens,
            last_checked,
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
                let now = SystemTime::now();
                *tokens += (now
                    .duration_since(*last_checked)
                    .unwrap_or_default()
                    .as_millis() as i128)
                    .checked_mul(S::Item::RATE as i128)
                    .unwrap_or(S::Item::RATE as i128 * 3_000)
                    / 1000
                    - item.quantity() as i128;
                *last_checked = now;

                if *tokens < 0 {
                    // Wake up a bit early to give processing time
                    let duration = u64::try_from(-*tokens / S::Item::RATE as i128 * 1000)
                        .unwrap_or_default()
                        .checked_sub(100)
                        .unwrap_or_default();
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
