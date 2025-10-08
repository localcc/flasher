use core::{
    borrow::{Borrow, BorrowMut},
    marker::PhantomData,
};

use crate::stream::{
    Sink, Stream,
    circular_bytes::CircularBytes,
    codec::{Decoder, Encoder},
};

pub struct Framed<const MAX_SIZE: usize, R, C, E> {
    inner: FramedImpl<R, RWFrame<MAX_SIZE>, C, E, MAX_SIZE>,
}

impl<const MAX_SIZE: usize, R, C, E> Framed<MAX_SIZE, R, C, E> {
    pub fn new(stream: R, codec: C) -> Self {
        Framed {
            inner: FramedImpl {
                stream,
                state: RWFrame::default(),
                codec,
                _marker: PhantomData,
            },
        }
    }
}

impl<const MAX_SIZE: usize, R, C, E> Stream for Framed<MAX_SIZE, R, C, E>
where
    R: embedded_io_async::BufRead,
    C: Decoder,
    E: From<C::Error> + From<R::Error>,
{
    type Item = Result<C::Item, E>;

    async fn next(&mut self) -> Option<Self::Item> {
        self.inner.next().await
    }
}

impl<const MAX_SIZE: usize, R, I, C, E> Sink<I> for Framed<MAX_SIZE, R, C, E>
where
    R: embedded_io_async::Write,
    C: Encoder<I>,
    E: From<C::Error> + From<R::Error>,
{
    type Error = E;

    async fn send(&mut self, value: I) -> Result<(), Self::Error> {
        self.inner.send(value).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.inner.flush().await
    }
}

#[derive(Debug)]
struct FramedImpl<R, S, C, E, const MAX_SIZE: usize> {
    stream: R,
    state: S,
    codec: C,
    _marker: PhantomData<E>,
}

#[derive(Debug)]
struct ReadFrame<const S: usize> {
    // eof: bool,s
    is_readable: bool,
    buf: CircularBytes<S>,
    // has_errored: bool,
}

impl<const S: usize> Default for ReadFrame<S> {
    fn default() -> Self {
        Self {
            // eof: false,
            is_readable: false,
            buf: CircularBytes::new(),
            // has_errored: false,
        }
    }
}

#[derive(Debug)]
struct WriteFrame<const S: usize> {
    buf: CircularBytes<S>,
}

impl<const S: usize> Default for WriteFrame<S> {
    fn default() -> Self {
        Self {
            buf: CircularBytes::new(),
        }
    }
}

#[derive(Debug, Default)]
struct RWFrame<const S: usize> {
    read: ReadFrame<S>,
    write: WriteFrame<S>,
}

impl<const S: usize> Borrow<ReadFrame<S>> for RWFrame<S> {
    fn borrow(&self) -> &ReadFrame<S> {
        &self.read
    }
}

impl<const S: usize> BorrowMut<ReadFrame<S>> for RWFrame<S> {
    fn borrow_mut(&mut self) -> &mut ReadFrame<S> {
        &mut self.read
    }
}

impl<const S: usize> Borrow<WriteFrame<S>> for RWFrame<S> {
    fn borrow(&self) -> &WriteFrame<S> {
        &self.write
    }
}

impl<const S: usize> BorrowMut<WriteFrame<S>> for RWFrame<S> {
    fn borrow_mut(&mut self) -> &mut WriteFrame<S> {
        &mut self.write
    }
}

impl<R, S, C, E, const MAX_SIZE: usize> Stream for FramedImpl<R, S, C, E, MAX_SIZE>
where
    R: embedded_io_async::BufRead,
    S: BorrowMut<ReadFrame<MAX_SIZE>>,
    C: Decoder,
    E: From<C::Error> + From<R::Error>,
{
    type Item = Result<C::Item, E>;

    async fn next(&mut self) -> Option<Self::Item> {
        loop {
            let state: &mut ReadFrame<MAX_SIZE> = self.state.borrow_mut();

            if state.is_readable {
                let decoded = match self.codec.decode(&mut state.buf) {
                    Ok(e) => e,
                    Err(e) => return Some(Err(e.into())),
                };

                if decoded.is_none() || state.buf.is_empty() {
                    state.is_readable = false;
                }
                if decoded.is_none() {
                    continue;
                }

                return decoded.map(Ok);
            }

            let bytes = match self.stream.fill_buf().await {
                Ok(e) => e,
                Err(e) => return Some(Err(e.into())),
            };
            let available = state.buf.capacity() - state.buf.len();
            let byte_count = available.min(bytes.len());

            state.buf.extend_from_slice(&bytes[..byte_count]);
            self.stream.consume(byte_count);

            state.is_readable |= byte_count != 0;
        }
    }
}

impl<R, S, I, C, E, const MAX_SIZE: usize> Sink<I> for FramedImpl<R, S, C, E, MAX_SIZE>
where
    R: embedded_io_async::Write,
    S: BorrowMut<WriteFrame<MAX_SIZE>>,
    C: Encoder<I>,
    E: From<C::Error> + From<R::Error>,
{
    type Error = E;

    async fn send(&mut self, value: I) -> Result<(), Self::Error> {
        let state: &mut WriteFrame<MAX_SIZE> = self.state.borrow_mut();
        self.codec.encode(value, &mut state.buf)?;

        let (s1, s2) = state.buf.as_slices();
        self.stream.write_all(s1).await?;
        self.stream.write_all(s2).await?;

        state.buf.clear();
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.stream.flush().await?;
        Ok(())
    }
}
