use hyper::body::{Bytes, Frame};
use mp3lame_sys::{
    lame_close, lame_encode_buffer, lame_encode_flush, lame_init, lame_init_params,
    lame_set_in_samplerate, lame_set_num_channels, lame_set_quality, lame_t,
};
use std::ffi::{c_int, c_short, c_uchar};
use std::ptr::null_mut;
use tokio::sync::broadcast;
use tokio::sync::mpsc::Sender;
use tokio::sync::mpsc::error::SendError;

pub struct Encoder {
    output: Sender<super::StreamWrite>,
    exit: broadcast::Receiver<()>,
    lame: lame_t,
}
unsafe impl Send for Encoder {}
impl Encoder {
    pub fn new(
        output: Sender<super::StreamWrite>,
        exit: broadcast::Receiver<()>,
    ) -> Result<Self, ()> {
        Ok(Encoder {
            output,
            exit,
            lame: unsafe {
                let handle = lame_init();
                if handle == null_mut() {
                    return Err(());
                }

                lame_set_num_channels(handle, 2);
                lame_set_in_samplerate(handle, 44100);
                lame_set_quality(handle, 2);
                let err = lame_init_params(handle);
                if err < 0 {
                    return Err(());
                }

                handle
            },
        })
    }
    pub async fn flush(&mut self) {
        let mut buffer = vec![0_u8; 7200];
        let result =
            unsafe { lame_encode_flush(self.lame, buffer.as_mut_ptr(), buffer.len() as c_int) };
        if result > 0 {
            buffer.truncate(result as usize);
            let _ = self.output.send(Ok(Frame::data(Bytes::from(buffer)))).await;
        }
    }
    pub async fn write(&mut self, left: &[i16], right: &[i16]) -> Result<(), ()> {
        if left.len() != right.len() {
            return Err(());
        }
        let mut buffer = vec![0_u8; left.len() + left.len() / 3 + 7200];
        let length = unsafe {
            lame_encode_buffer(
                self.lame,
                left.as_ptr() as *const c_short,
                right.as_ptr() as *const c_short,
                left.len() as c_int,
                buffer.as_ptr() as *mut c_uchar,
                buffer.len() as c_int,
            )
        };
        if length < 0 {
            Err(())
        } else {
            buffer.truncate(length as usize);
            let result = tokio::select! {biased;
                _ = self.exit.recv() => return Err(()),
                v = self.output.send(Ok(Frame::data(Bytes::from(buffer)))) => v,
            };
            match result {
                Err(SendError(_)) => {
                    eprintln!("Failed to send to channel");
                    Err(())
                }
                Ok(()) => Ok(()),
            }
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe {
            lame_close(self.lame);
            self.lame = null_mut();
        }
    }
}
