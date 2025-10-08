use core::marker::PhantomData;

use crate::stream::{Sink, Stream, TryStream, circular_bytes::CircularBytes};

pub trait Serializer<const MAX_SIZE: usize, T> {
    type Error;

    fn serialize(&mut self, item: &T) -> Result<CircularBytes<MAX_SIZE>, Self::Error>;
}

pub trait Deserializer<T> {
    type Error;

    fn deserialize(&mut self, src: &[u8]) -> Result<T, Self::Error>;
}

pub struct SerdeFramed<
    const STREAM_SIZE: usize,
    const SINK_SIZE: usize,
    Transport,
    StreamItem,
    SinkItem,
    Codec,
    E,
> {
    transport: Transport,
    codec: Codec,
    _marker: PhantomData<(StreamItem, SinkItem, E)>,
}

impl<const STREAM_SIZE: usize, const SINK_SIZE: usize, Transport, StreamItem, SinkItem, Codec, E>
    SerdeFramed<STREAM_SIZE, SINK_SIZE, Transport, StreamItem, SinkItem, Codec, E>
{
    pub fn new(transport: Transport, codec: Codec) -> Self {
        Self {
            transport,
            codec,
            _marker: PhantomData,
        }
    }

    pub fn get_transport_mut(&mut self) -> &mut Transport {
        &mut self.transport
    }
}

impl<const STREAM_SIZE: usize, const SINK_SIZE: usize, Transport, StreamItem, SinkItem, Codec, E>
    Stream for SerdeFramed<STREAM_SIZE, SINK_SIZE, Transport, StreamItem, SinkItem, Codec, E>
where
    Transport: TryStream<Ok = CircularBytes<STREAM_SIZE>>,
    Codec: Deserializer<StreamItem>,
    E: From<Transport::Error> + From<Codec::Error>,
{
    type Item = Result<StreamItem, E>;

    async fn next(&mut self) -> Option<Self::Item> {
        let bytes = self.transport.try_next().await?;
        let bytes = match bytes {
            Ok(e) => e,
            Err(e) => return Some(Err(e.into())),
        };
        let deserialized = match self.codec.deserialize(bytes.as_slices().0) {
            Ok(e) => e,
            Err(e) => return Some(Err(e.into())),
        };
        Some(Ok(deserialized))
    }
}

impl<const STREAM_SIZE: usize, const SINK_SIZE: usize, Transport, StreamItem, SinkItem, Codec, E>
    Sink<SinkItem>
    for SerdeFramed<STREAM_SIZE, SINK_SIZE, Transport, StreamItem, SinkItem, Codec, E>
where
    Transport: for<'a> Sink<&'a [u8]>,
    Codec: Serializer<SINK_SIZE, SinkItem>,
    E: for<'a> From<<Transport as Sink<&'a [u8]>>::Error> + From<Codec::Error>,
{
    type Error = E;

    async fn send(&mut self, value: SinkItem) -> Result<(), Self::Error> {
        let bytes = self.codec.serialize(&value)?;
        self.transport.send(bytes.as_slices().0).await?;
        Ok(())
    }

    async fn flush(&mut self) -> Result<(), Self::Error> {
        self.transport.flush().await?;
        Ok(())
    }
}

pub mod formats {
    pub mod postcard {
        use core::marker::PhantomData;

        use postcard::experimental::max_size::MaxSize;
        use serde::{Deserialize, Serialize};

        use crate::stream::{
            circular_bytes::CircularBytes,
            serde::{Deserializer, Serializer},
        };

        pub struct Postcard<StreamItem, SinkItem> {
            _marker: PhantomData<(StreamItem, SinkItem)>,
        }

        impl<StreamItem, SinkItem> Postcard<StreamItem, SinkItem> {
            pub fn new() -> Self {
                Postcard {
                    _marker: PhantomData,
                }
            }
        }

        impl<StreamItem, SinkItem> Clone for Postcard<StreamItem, SinkItem> {
            fn clone(&self) -> Self {
                Self {
                    _marker: PhantomData,
                }
            }
        }

        impl<StreamItem, SinkItem> Deserializer<StreamItem> for Postcard<StreamItem, SinkItem>
        where
            for<'a> StreamItem: Deserialize<'a>,
        {
            type Error = postcard::Error;

            fn deserialize(&mut self, src: &[u8]) -> Result<StreamItem, Self::Error> {
                postcard::from_bytes(&src)
            }
        }

        impl<StreamItem, SinkItem> Serializer<{ SinkItem::POSTCARD_MAX_SIZE }, SinkItem>
            for Postcard<StreamItem, SinkItem>
        where
            SinkItem: Serialize + MaxSize,
        {
            type Error = postcard::Error;

            fn serialize(
                &mut self,
                item: &SinkItem,
            ) -> Result<CircularBytes<{ SinkItem::POSTCARD_MAX_SIZE }>, Self::Error> {
                let mut bytes = CircularBytes::new();
                bytes.fill(0);
                let slice = postcard::to_slice(item, bytes.as_mut_slices().0)?;
                let len = slice.len();
                bytes.truncate_back(len);
                Ok(bytes)
            }
        }
    }
}
