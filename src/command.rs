use defmt::Format;
use postcard::experimental::max_size::MaxSize;
use serde::{Deserialize, Serialize};
use serde_with::serde_as;

#[derive(Serialize, Deserialize, MaxSize)]
pub enum Mode {
    Flasher,
    Uart,
}

#[serde_as]
#[derive(Format, Debug, Copy, Clone, Serialize, Deserialize, MaxSize)]
pub enum Request {
    StopSmc,
    StartSmc,
    GetFlashConfig,
    Cancel,
    ReadLbaRange { start: u32, count: u32 },
    WriteLbaRange { start: u32, count: u32 },
}

#[derive(Format, Debug, Clone, Serialize, Deserialize, MaxSize)]
pub enum Response {
    Ok,
    Error(heapless::String<256>),
    FlashConfig(u32),
}
