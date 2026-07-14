use futures::{Stream, StreamExt};
use hyper::body::Bytes;
use mp3lame_sys::{
    lame_close, lame_encode_buffer_interleaved_ieee_float, lame_encode_flush, lame_init,
    lame_init_params, lame_set_in_samplerate, lame_set_num_channels, lame_set_quality, lame_t,
};
use std::ffi::{c_float, c_int, c_uchar};
use std::pin::Pin;
use std::ptr::null_mut;
use std::task::{Context, Poll};

pub struct EncodedStream<I> {
    input: I,
    lame: lame_t,
}
unsafe impl<I: Send> Send for EncodedStream<I> {}
impl<I> EncodedStream<I> {
    pub fn new(input: I) -> Result<Self, ()> {
        Ok(EncodedStream {
            input,
            lame: unsafe {
                let handle = lame_init();
                if handle == null_mut() {
                    return Err(());
                }

                lame_set_num_channels(handle, 2);
                lame_set_in_samplerate(handle, 44_100);
                lame_set_quality(handle, 2);
                let err = lame_init_params(handle);
                if err < 0 {
                    return Err(());
                }

                handle
            },
        })
    }
}
impl<I: Stream<Item = Vec<f32>> + Unpin> Stream for EncodedStream<I> {
    type Item = Bytes;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let encoder = self.get_mut();
        match encoder.input.poll_next_unpin(cx) {
            Poll::Ready(None) => {
                if encoder.lame == null_mut() {
                    Poll::Ready(None)
                } else {
                    let mut buffer = vec![0_u8; 7200];
                    let result = unsafe {
                        lame_encode_flush(encoder.lame, buffer.as_mut_ptr(), buffer.len() as c_int)
                    };
                    unsafe {
                        lame_close(encoder.lame);
                    }
                    encoder.lame = null_mut();
                    if result > 0 {
                        buffer.truncate(result as usize);
                        Poll::Ready(Some(Bytes::from(buffer)))
                    } else {
                        Poll::Ready(None)
                    }
                }
            }
            Poll::Ready(Some(value)) => {
                let mut buffer = vec![0_u8; value.capacity() + value.capacity() / 3 + 7200];
                let length = unsafe {
                    lame_encode_buffer_interleaved_ieee_float(
                        encoder.lame,
                        value.as_ptr() as *const c_float,
                        (value.len() / 2) as c_int,
                        buffer.as_ptr() as *mut c_uchar,
                        buffer.len() as c_int,
                    )
                };
                if length < 0 {
                    Poll::Ready(None)
                } else {
                    buffer.truncate(length as usize);
                    Poll::Ready(Some(Bytes::from(buffer)))
                }
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

impl<I> Drop for EncodedStream<I> {
    fn drop(&mut self) {
        if self.lame != null_mut() {
            unsafe {
                lame_close(self.lame);
            }
        }
        self.lame = null_mut();
    }
}
