use crate::stream::circular_bytes::CircularBytes;

pub mod circular_bytes;
pub mod codec;
pub mod framed;
pub mod serde;

pub trait Stream {
    type Item;

    async fn next(&mut self) -> Option<Self::Item>;
}

impl<T: Stream> Stream for &mut T {
    type Item = T::Item;

    async fn next(&mut self) -> Option<Self::Item> {
        T::next(self).await
    }
}

pub trait TryStream {
    type Ok;
    type Error;

    async fn try_next(&mut self) -> Option<Result<Self::Ok, Self::Error>>;
}

impl<S, T, E> TryStream for S
where
    S: Stream<Item = Result<T, E>>,
{
    type Ok = T;
    type Error = E;

    async fn try_next(&mut self) -> Option<Result<Self::Ok, Self::Error>> {
        self.next().await
    }
}

pub trait Sink<Item> {
    type Error;

    async fn send(&mut self, value: Item) -> Result<(), Self::Error>;
    #[allow(unused)]
    async fn flush(&mut self) -> Result<(), Self::Error>;
}

impl<Item, T: Sink<Item>> Sink<Item> for &mut T {
    type Error = T::Error;

    async fn send(&mut self, value: Item) -> Result<(), Self::Error> {
        T::send(self, value).await
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        T::flush(self).await
    }
}

/// BUF_SIZE is the size of the internal circular buffer
/// GRAB_SIZE is the maximum amount of data to load into the internal circular buffer at once
pub struct BufReader<const BUF_SIZE: usize, const GRAB_SIZE: usize, S> {
    buffer: CircularBytes<BUF_SIZE>,
    stream: S,
}

impl<const BUF_SIZE: usize, const GRAB_SIZE: usize, S> BufReader<BUF_SIZE, GRAB_SIZE, S> {
    pub fn new(stream: S) -> Self {
        Self {
            buffer: CircularBytes::new(),
            stream,
        }
    }
}

impl<const BUF_SIZE: usize, const GRAB_SIZE: usize, S> embedded_io_async::BufRead
    for BufReader<BUF_SIZE, GRAB_SIZE, S>
where
    S: embedded_io_async::Read,
{
    async fn fill_buf(&mut self) -> Result<&[u8], Self::Error> {
        let mut buf = [0u8; GRAB_SIZE];
        let count = self.stream.read(&mut buf).await?;
        let buf = &buf[..count];
        self.buffer.extend_from_slice(&buf);

        Ok(self.buffer.as_slices().0)
    }

    fn consume(&mut self, amt: usize) {
        self.buffer
            .truncate_front(self.buffer.len().saturating_sub(amt));
    }
}

impl<const BUF_SIZE: usize, const GRAB_SIZE: usize, S> embedded_io_async::ErrorType
    for BufReader<BUF_SIZE, GRAB_SIZE, S>
where
    S: embedded_io_async::ErrorType,
{
    type Error = S::Error;
}
