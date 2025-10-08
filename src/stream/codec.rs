use defmt::Format;
use num::{
    Unsigned,
    cast::AsPrimitive,
    traits::{FromBytes, ToBytes},
};
use thiserror::Error;

use crate::stream::circular_bytes::CircularBytes;

pub trait Encoder<Item> {
    type Error: From<CodecError>;

    fn encode<const S: usize>(
        &mut self,
        item: Item,
        dst: &mut CircularBytes<S>,
    ) -> Result<(), Self::Error>;
}
pub trait Decoder {
    type Item;
    type Error: From<CodecError>;

    fn decode<const S: usize>(
        &mut self,
        src: &mut CircularBytes<S>,
    ) -> Result<Option<Self::Item>, Self::Error>;
}

#[derive(Clone)]
pub struct LengthDelimitedCodec<const DECODE_SIZE: usize, LenType: Unsigned> {
    max_frame_len: LenType,
    state: LengthDelimitedCodecState,
}

impl<const DECODE_SIZE: usize, LenType: Unsigned> LengthDelimitedCodec<DECODE_SIZE, LenType> {
    pub fn new(max_frame_len: LenType) -> Self {
        Self {
            max_frame_len,
            state: LengthDelimitedCodecState::Head,
        }
    }
}

#[derive(Clone)]
enum LengthDelimitedCodecState {
    Head,
    Data(usize),
}

impl<const DECODE_SIZE: usize, LenType> Encoder<&[u8]>
    for LengthDelimitedCodec<DECODE_SIZE, LenType>
where
    LenType: Unsigned + PartialOrd + Ord + ToBytes + Copy + 'static,
    usize: AsPrimitive<LenType>,
{
    type Error = CodecError;

    fn encode<const S: usize>(
        &mut self,
        item: &[u8],
        dst: &mut CircularBytes<S>,
    ) -> Result<(), Self::Error> {
        let len = item.len();
        let len: LenType = len.as_();

        if len > self.max_frame_len {
            return Err(CodecError::ValueTooLong);
        }

        let bytes = len.to_le_bytes();
        let bytes = bytes.as_ref();

        let want = bytes.len() + item.len();
        if want > dst.capacity() {
            panic!(
                "destination buffer too small, want: {}, got: {}",
                want,
                dst.capacity()
            );
        }

        dst.extend_from_slice(bytes);
        dst.extend_from_slice(item);

        Ok(())
    }
}

impl<const DECODE_SIZE: usize, LenType> LengthDelimitedCodec<DECODE_SIZE, LenType>
where
    LenType: Sized + Unsigned + FromBytes + AsPrimitive<usize>,
    LenType::Bytes: for<'a> TryFrom<&'a [u8]> + Sized,
    [u8; size_of::<LenType>()]: Sized,
{
    fn decode_head<const S: usize>(
        &self,
        src: &mut CircularBytes<S>,
    ) -> Result<Option<usize>, CodecError> {
        let n = {
            let mut bytes = [0u8; size_of::<LenType>()];
            if !src.consume_to_exact(&mut bytes) {
                return Ok(None);
            }

            let bytes: LenType::Bytes = bytes.as_ref().try_into().unwrap_or_else(|_| {
                panic!("failed to convert byteslice to LenType, this should never happen")
            });
            LenType::from_le_bytes(&bytes).as_()
        };

        Ok(Some(n))
    }

    fn decode_data<const S: usize>(
        &self,
        n: usize,
        src: &mut CircularBytes<S>,
    ) -> Option<CircularBytes<DECODE_SIZE>> {
        if src.len() < n {
            // info!("src {}", src.len());
            return None;
        }

        let mut data = CircularBytes::new();
        data.fill(0);

        // data is contiguous at this point
        let slice = &mut data.as_mut_slices().0;
        let slice = &mut slice[..n];

        src.consume_to(slice);

        data.truncate_back(n);

        Some(data)
    }
}

impl<const DECODE_SIZE: usize, LenType> Decoder for LengthDelimitedCodec<DECODE_SIZE, LenType>
where
    LenType: Sized + Unsigned + FromBytes + AsPrimitive<usize>,
    LenType::Bytes: for<'a> TryFrom<&'a [u8]> + Sized,
    [u8; size_of::<LenType>()]: Sized,
{
    type Item = CircularBytes<DECODE_SIZE>;
    type Error = CodecError;

    fn decode<const S: usize>(
        &mut self,
        src: &mut CircularBytes<S>,
    ) -> Result<Option<Self::Item>, Self::Error> {
        let n = match self.state {
            LengthDelimitedCodecState::Head => match self.decode_head(src)? {
                Some(n) => {
                    self.state = LengthDelimitedCodecState::Data(n);
                    n
                }
                None => return Ok(None),
            },
            LengthDelimitedCodecState::Data(n) => n,
        };

        match self.decode_data(n, src) {
            Some(data) => {
                self.state = LengthDelimitedCodecState::Head;
                Ok(Some(data))
            }
            None => Ok(None),
        }
    }
}

#[derive(Debug, Format, Error)]
pub enum CodecError {
    #[error("value too long to fit into the stream")]
    ValueTooLong,
}
