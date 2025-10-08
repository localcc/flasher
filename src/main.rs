#![no_main]
#![no_std]
#![allow(incomplete_features)]
#![feature(generic_const_exprs)]

use core::sync::atomic::Ordering;

use defmt::{Format, info};
use embassy_executor::{Spawner, SpawnerTraceExt};
use embassy_futures::{join::join, select::select};
use embassy_rp::Peri;
use embassy_rp::peripherals::PIO0;
use embassy_rp::peripherals::USB;
use embassy_rp::peripherals::{
    DMA_CH1, DMA_CH2, DMA_CH3, DMA_CH4, PIN_16, PIN_17, PIN_18, PIN_19, PIN_20, PIN_21, UART0,
};
use embassy_rp::pio::{Instance, Pio};
use embassy_sync::blocking_mutex::raw::ThreadModeRawMutex;
use embassy_time::{Duration, Instant};
use embassy_usb::{
    class::cdc_acm::{CdcAcmClass, Receiver, Sender},
    driver::EndpointError,
};
use portable_atomic::AtomicBool;
use postcard::experimental::max_size::MaxSize;
use static_cell::StaticCell;

use crate::{
    command::{Request, Response},
    spiex::{DMAState, LBA_SIZE, LbaWriteData, SpiexState},
    stream::{
        BufReader, Sink, Stream,
        codec::{CodecError, LengthDelimitedCodec},
        framed::Framed,
        serde::{SerdeFramed, formats},
    },
    usb::{UsbReadStream, UsbWriteStream},
    xbox::{XboxPins, XboxState},
};

use defmt_rtt as _;
use panic_probe as _;

mod command;
mod spiex;
mod stream;
mod usb;
mod xbox;

struct SpiexPeripherals {
    pio0: Peri<'static, PIO0>,
    spiex_dma0: Peri<'static, DMA_CH1>,
    spiex_dma1: Peri<'static, DMA_CH2>,

    write_buf_dma0: Peri<'static, DMA_CH3>,
    write_buf_dma1: Peri<'static, DMA_CH4>,

    spi_tx: Peri<'static, PIN_19>,
    spi_rx: Peri<'static, PIN_16>,
    spi_clk: Peri<'static, PIN_18>,
    spi_cs: Peri<'static, PIN_17>,
    dbg_en: Peri<'static, PIN_20>,
    rst_xdk: Peri<'static, PIN_21>,
}

embassy_rp::bind_interrupts!(struct Irqs {
    USBCTRL_IRQ => embassy_rp::usb::InterruptHandler<USB>;
    UART0_IRQ => embassy_rp::uart::InterruptHandler<UART0>;
    PIO0_IRQ_0 => embassy_rp::pio::InterruptHandler<embassy_rp::peripherals::PIO0>;
});

const MAX_FULL_SPEED_PACKET_SIZE: usize = 64;

type RawMtx = ThreadModeRawMutex;

type LengthCodec = LengthDelimitedCodec<{ LBA_SIZE + core::mem::size_of::<u16>() }, u16>;
type SerdeCodec = formats::postcard::Postcard<Request, Response>;

type Usb = embassy_rp::usb::Driver<'static, USB>;

type UsbRead<'a> = UsbReadStream<'a, 'static, Usb>;
type BufUsbRead<'a> = BufReader<768, 64, UsbRead<'a>>;
type UsbFramedRead<'a, const SIZE: usize> = Framed<SIZE, BufUsbRead<'a>, LengthCodec, ThingError>;
type UsbSerdeRead<'a, const SIZE: usize> =
    SerdeFramed<SIZE, 0, UsbFramedRead<'a, SIZE>, Request, (), SerdeCodec, ThingError>;

type UsbWrite<'a> = UsbWriteStream<'a, 'static, Usb>;
type UsbFramedWrite<'a> =
    Framed<{ LBA_SIZE + core::mem::size_of::<u16>() }, UsbWrite<'a>, LengthCodec, ThingError>;
type UsbSerdeWrite<'a> = SerdeFramed<
    0,
    { Response::POSTCARD_MAX_SIZE },
    UsbFramedWrite<'a>,
    (),
    Response,
    SerdeCodec,
    ThingError,
>;

#[embassy_executor::main]
async fn main(spawner: Spawner) {
    let peripherals = embassy_rp::init(embassy_rp::config::Config::default());

    static USB_WRITE_CHANNEL_BUF: StaticCell<[[u8; LBA_SIZE]; 8]> = StaticCell::new();
    let buf = USB_WRITE_CHANNEL_BUF.init([[0u8; LBA_SIZE]; 8]);

    static USB_WRITE_CHANNEL: StaticCell<
        embassy_sync::zerocopy_channel::Channel<'_, RawMtx, [u8; LBA_SIZE]>,
    > = StaticCell::new();
    let channel = USB_WRITE_CHANNEL.init(embassy_sync::zerocopy_channel::Channel::new(buf));

    let (usb_write_tx, usb_write_rx) = channel.split();

    static USB_READ_CHANNEL_BUF: StaticCell<[[u8; LBA_SIZE]; 8]> = StaticCell::new();
    let buf = USB_READ_CHANNEL_BUF.init([[0u8; LBA_SIZE]; 8]);

    static USB_READ_CHANNEL: StaticCell<
        embassy_sync::zerocopy_channel::Channel<'_, RawMtx, [u8; LBA_SIZE]>,
    > = StaticCell::new();
    let channel = USB_READ_CHANNEL.init(embassy_sync::zerocopy_channel::Channel::new(buf));

    let (usb_read_tx, usb_read_rx) = channel.split();

    let driver = embassy_rp::usb::Driver::new(peripherals.USB, Irqs);
    let config = {
        let mut config = embassy_usb::Config::new(0xc0de, 0xcafe);
        config.manufacturer = Some("localcc & ambraglow");
        config.product = Some("flasher");
        config.serial_number = Some("12345678");
        config.max_power = 0; // powered by the xbox
        config.max_packet_size_0 = MAX_FULL_SPEED_PACKET_SIZE as u8;

        // Required for windows support
        config.composite_with_iads = true;
        config
    };

    let mut builder = {
        static CONFIG_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
        static BOS_DESCRIPTOR: StaticCell<[u8; 256]> = StaticCell::new();
        static CONTROL_BUF: StaticCell<[u8; 64]> = StaticCell::new();

        let builder = embassy_usb::Builder::new(
            driver,
            config,
            CONFIG_DESCRIPTOR.init([0; 256]),
            BOS_DESCRIPTOR.init([0; 256]),
            &mut [], // no msos descriptors
            CONTROL_BUF.init([0; 64]),
        );
        builder
    };

    let class = {
        static STATE: StaticCell<embassy_usb::class::cdc_acm::State> = StaticCell::new();
        let state = STATE.init(embassy_usb::class::cdc_acm::State::new());
        CdcAcmClass::new(&mut builder, state, MAX_FULL_SPEED_PACKET_SIZE as u16)
    };

    let device = builder.build();

    let spiex_peripherals = SpiexPeripherals {
        pio0: peripherals.PIO0,
        spiex_dma0: peripherals.DMA_CH1,
        spiex_dma1: peripherals.DMA_CH2,
        write_buf_dma0: peripherals.DMA_CH3,
        write_buf_dma1: peripherals.DMA_CH4,
        spi_tx: peripherals.PIN_19,
        spi_rx: peripherals.PIN_16,
        spi_clk: peripherals.PIN_18,
        spi_cs: peripherals.PIN_17,
        dbg_en: peripherals.PIN_20,
        rst_xdk: peripherals.PIN_21,
    };

    spawner.spawn_named("usb_task", usb_task(device)).unwrap();
    spawner
        .spawn_named(
            "usb_processing",
            usb_processing(class, usb_read_tx, usb_write_rx),
        )
        .unwrap();
    spawner
        .spawn_named(
            "spiex_task",
            spiex_task(spiex_peripherals, usb_write_tx, usb_read_rx),
        )
        .unwrap();
}

type UsbDevice = embassy_usb::UsbDevice<'static, embassy_rp::usb::Driver<'static, USB>>;

#[embassy_executor::task]
async fn usb_processing(
    class: CdcAcmClass<'static, embassy_rp::usb::Driver<'static, USB>>,
    usb_read_tx: embassy_sync::zerocopy_channel::Sender<'static, RawMtx, [u8; LBA_SIZE]>,
    usb_write_rx: embassy_sync::zerocopy_channel::Receiver<'static, RawMtx, [u8; LBA_SIZE]>,
) {
    let codec = LengthDelimitedCodec::new(768u16);
    let serde_codec = formats::postcard::Postcard::<Request, Response>::new();

    let (mut tx, mut rx) = class.split();

    let read_task = usb_tasks::read_task(&mut rx, codec.clone(), serde_codec.clone(), usb_read_tx);
    let write_task =
        usb_tasks::write_task(&mut tx, codec.clone(), serde_codec.clone(), usb_write_rx);

    join(read_task, write_task).await;
}

#[embassy_executor::task]
async fn usb_task(mut usb: UsbDevice) -> ! {
    usb.run().await
}

static SPIEX_TASK_CHANNEL: embassy_sync::channel::Channel<RawMtx, SpiexTaskRequest, 8> =
    embassy_sync::channel::Channel::new();
static USB_WRITE_TASK_CHANNEL: embassy_sync::channel::Channel<RawMtx, command::Response, 8> =
    embassy_sync::channel::Channel::new();

static USB_READ_LBA_CANCELLATION: AtomicBool = AtomicBool::new(false);
static SPIEX_READ_LBA_CANCELLATION: AtomicBool = AtomicBool::new(false);

mod usb_tasks {

    use defmt::debug;
    use embassy_futures::select::Either;

    pub use super::*;

    pub(super) async fn read_task(
        rx: &mut Receiver<'static, Usb>,
        codec: LengthCodec,
        serde_codec: SerdeCodec,
        mut usb_read_tx: embassy_sync::zerocopy_channel::Sender<'static, RawMtx, [u8; LBA_SIZE]>,
    ) {
        loop {
            rx.wait_connection().await;

            let read_stream = UsbRead::new(rx);
            let read_stream = BufUsbRead::new(read_stream);
            let framed =
                UsbFramedRead::<{ LBA_SIZE + size_of::<u16>() }>::new(read_stream, codec.clone());
            let mut serde = UsbSerdeRead::new(framed, serde_codec.clone());

            let mut current_mode = UsbReadMode::Command;

            debug!("serial connected");

            loop {
                match current_mode {
                    UsbReadMode::Command => {
                        current_mode = read_command(&mut serde).await;
                    }
                    UsbReadMode::LbaRange { start, count } => {
                        current_mode = read_lba_range(
                            serde.get_transport_mut(),
                            &mut usb_read_tx,
                            start,
                            count,
                        )
                        .await;
                    }
                }
            }
        }
    }

    pub(super) async fn write_task(
        tx: &mut Sender<'static, Usb>,
        codec: LengthCodec,
        serde_codec: SerdeCodec,
        mut write_rx: embassy_sync::zerocopy_channel::Receiver<'static, RawMtx, [u8; LBA_SIZE]>,
    ) {
        loop {
            tx.wait_connection().await;

            let write_stream = UsbWrite::new(tx);
            let framed = UsbFramedWrite::new(write_stream, codec.clone());
            let mut serde = UsbSerdeWrite::new(framed, serde_codec.clone());

            loop {
                match select(USB_WRITE_TASK_CHANNEL.receive(), write_rx.receive()).await {
                    Either::First(response) => {
                        if let Err(e) = serde.send(response).await {
                            debug!("usb send error: {:?}", e);
                        }
                    }
                    Either::Second(lba) => {
                        let bytes = lba.as_slice();
                        if let Err(e) = serde.get_transport_mut().send(bytes).await {
                            SPIEX_READ_LBA_CANCELLATION.store(true, Ordering::Release);
                            debug!("usb send error: {:?}", e);
                        }
                        write_rx.receive_done();
                    }
                };
            }
        }
    }
    async fn read_command<'a>(
        serde: &mut UsbSerdeRead<'a, { LBA_SIZE + size_of::<u16>() }>,
    ) -> UsbReadMode {
        while let Some(e) = serde.next().await {
            info!("received command maybe");
            let Ok(e) = e.map_err(|e| {
                debug!("error: {:?}", e);
                ()
            }) else {
                continue;
            };

            match e {
                Request::StopSmc => {
                    info!("stop smc");
                    SPIEX_TASK_CHANNEL.send(SpiexTaskRequest::StopSmc).await;
                    info!("next");
                }
                Request::StartSmc => {
                    SPIEX_TASK_CHANNEL.send(SpiexTaskRequest::StartSmc).await;
                }
                Request::Cancel => {
                    USB_READ_LBA_CANCELLATION.store(true, Ordering::Release);
                    SPIEX_READ_LBA_CANCELLATION.store(true, Ordering::Release);
                    USB_WRITE_TASK_CHANNEL.send(Response::Ok).await;
                }
                Request::GetFlashConfig => {
                    info!("getting flash config");
                    SPIEX_TASK_CHANNEL
                        .send(SpiexTaskRequest::GetFlashConfig)
                        .await;
                }
                Request::ReadLbaRange { start, count } => {
                    info!("read lba range?");
                    SPIEX_TASK_CHANNEL
                        .send(SpiexTaskRequest::ReadLbaRange { start, count })
                        .await;
                }
                Request::WriteLbaRange { start, count } => {
                    return UsbReadMode::LbaRange { start, count };
                }
            }
        }

        UsbReadMode::Command
    }

    async fn read_lba_range<'a>(
        framed: &mut UsbFramedRead<'a, { LBA_SIZE + size_of::<u16>() }>,
        usb_read_tx: &mut embassy_sync::zerocopy_channel::Sender<'static, RawMtx, [u8; LBA_SIZE]>,
        start: u32,
        count: u32,
    ) -> UsbReadMode {
        USB_READ_LBA_CANCELLATION.store(false, Ordering::Release);

        SPIEX_TASK_CHANNEL
            .send(SpiexTaskRequest::WriteLbaRange { start, count })
            .await;

        for _ in 0..count {
            if USB_READ_LBA_CANCELLATION
                .compare_exchange(true, false, Ordering::SeqCst, Ordering::Acquire)
                .is_ok()
            {
                debug!("usb read lba cancelled");
                return UsbReadMode::Command;
            }

            let Some(Ok(mut frame)) = framed.next().await else {
                info!("failed to receive frame!!");
                // SAFETY: the message fits inside of the heapless string
                let error_message = unsafe {
                    "unexpected eof when reading lba range from usb"
                        .try_into()
                        .unwrap_unchecked()
                };
                USB_WRITE_TASK_CHANNEL
                    .send(Response::Error(error_message))
                    .await;
                return UsbReadMode::Command;
            };

            {
                let buf = usb_read_tx.send().await;
                frame.consume_to(buf);
            }
            usb_read_tx.send_done();
        }
        UsbReadMode::Command
    }

    #[derive(Format)]
    enum UsbReadMode {
        Command,
        LbaRange { start: u32, count: u32 },
    }
}

#[embassy_executor::task]
async fn spiex_task(
    peripherals: SpiexPeripherals,
    mut usb_write_tx: embassy_sync::zerocopy_channel::Sender<'static, RawMtx, [u8; LBA_SIZE]>,
    mut usb_read_rx: embassy_sync::zerocopy_channel::Receiver<'static, RawMtx, [u8; LBA_SIZE]>,
) {
    let mut write_dma_state = DMAState {
        dma0: peripherals.write_buf_dma0,
        dma1: peripherals.write_buf_dma1,
    };
    spiex::init(&write_dma_state);

    let mut spiex_dma_state = DMAState {
        dma0: peripherals.spiex_dma0,
        dma1: peripherals.spiex_dma1,
    };

    // SAFETY: LbaWriteData gets initialized before use
    let mut lba_write_data = core::pin::pin!(unsafe { LbaWriteData::uninit() });
    // SAFETY: LbaWriteData has not been initialized before
    unsafe {
        lba_write_data
            .as_mut()
            .init(spiex_dma_state.dma0.reborrow())
    }
    .await;

    let pio = Pio::new(peripherals.pio0, Irqs);
    let spiex_state = SpiexState::new(
        spiex_dma_state,
        pio,
        peripherals.spi_tx,
        peripherals.spi_rx,
        peripherals.spi_clk,
        peripherals.spi_cs,
    );

    let xbox_pins = XboxPins {
        dbg_en: peripherals.dbg_en,
        rst_xdk: peripherals.rst_xdk,
    };
    let mut xbox_state = XboxState::new(xbox_pins, spiex_state);

    let mut mode = SpiexMode::ReadRequest;

    loop {
        info!("spiex mode: {:?}", mode);
        match mode {
            SpiexMode::ReadRequest => {
                mode = spiex_read_request(&mut xbox_state).await;
            }
            SpiexMode::ReadLbaRange { start, count } => {
                mode = spiex_read_lba_range(&mut xbox_state, &mut usb_write_tx, start, count).await;
            }
            SpiexMode::WriteLbaRange {
                flash_config,
                start,
                count,
            } => {
                mode = spiex_write_lba_range(
                    &mut xbox_state,
                    lba_write_data.as_mut(),
                    &mut write_dma_state,
                    &mut usb_read_rx,
                    flash_config,
                    start,
                    count,
                )
                .await;
            }
        }
    }
}

enum SpiexTaskRequest {
    StopSmc,
    StartSmc,
    GetFlashConfig,
    ReadLbaRange { start: u32, count: u32 },
    WriteLbaRange { start: u32, count: u32 },
}

async fn spiex_read_request<'a, PIO>(xbox_state: &mut XboxState<'a, PIO>) -> SpiexMode
where
    PIO: Instance,
{
    let mut flash_config = 0u32;

    loop {
        let request = SPIEX_TASK_CHANNEL.receive().await;
        match request {
            SpiexTaskRequest::StopSmc => {
                info!("stopping smc");
                xbox_state.stop_smc().await;
                info!("stopped smc");
                USB_WRITE_TASK_CHANNEL.send(Response::Ok).await;
                info!("sent response");
            }
            SpiexTaskRequest::StartSmc => {
                xbox_state.start_smc().await;
                USB_WRITE_TASK_CHANNEL.send(Response::Ok).await;
            }
            SpiexTaskRequest::GetFlashConfig => {
                flash_config = xbox_state.get_flash_config().await;
                USB_WRITE_TASK_CHANNEL
                    .send(Response::FlashConfig(flash_config))
                    .await;
            }
            SpiexTaskRequest::ReadLbaRange { start, count } => {
                return SpiexMode::ReadLbaRange { start, count };
            }
            SpiexTaskRequest::WriteLbaRange { start, count } => {
                return SpiexMode::WriteLbaRange {
                    flash_config,
                    start,
                    count,
                };
            }
        }
    }
}

async fn spiex_read_lba_range<'a, PIO: Instance>(
    xbox_state: &mut XboxState<'a, PIO>,
    usb_write_tx: &mut embassy_sync::zerocopy_channel::Sender<'static, RawMtx, [u8; LBA_SIZE]>,
    start: u32,
    count: u32,
) -> SpiexMode {
    SPIEX_READ_LBA_CANCELLATION.store(false, Ordering::Release);

    let a = Instant::now();
    for i in 0..count {
        if SPIEX_READ_LBA_CANCELLATION
            .compare_exchange(true, false, Ordering::SeqCst, Ordering::Acquire)
            .is_ok()
        {
            return SpiexMode::ReadRequest;
        }

        let lba = start + i;

        {
            let data = usb_write_tx.send().await;
            if let Err(_) = xbox_state.read_lba(Duration::from_secs(1), lba, data).await {
                // SAFETY: the error string always fits inside the capacity
                let error_string = unsafe {
                    "timed out reading the lba data"
                        .try_into()
                        .unwrap_unchecked()
                };
                USB_WRITE_TASK_CHANNEL
                    .send(Response::Error(error_string))
                    .await;

                return SpiexMode::ReadRequest;
            }
        }

        usb_write_tx.send_done();
    }
    let b = Instant::now();
    info!("took: {}", b.duration_since(a).as_micros(),);

    SpiexMode::ReadRequest
}

async fn spiex_write_lba_range<'a, PIO, DMA0, DMA1>(
    xbox_state: &mut XboxState<'a, PIO>,
    mut lba_write_data: core::pin::Pin<&mut LbaWriteData>,
    write_dma_state: &mut DMAState<'a, DMA0, DMA1>,
    usb_read_rx: &mut embassy_sync::zerocopy_channel::Receiver<'static, RawMtx, [u8; LBA_SIZE]>,
    flash_config: u32,
    start: u32,
    count: u32,
) -> SpiexMode
where
    PIO: Instance,
    DMA0: embassy_rp::dma::Channel,
    DMA1: embassy_rp::dma::Channel,
{
    USB_READ_LBA_CANCELLATION.store(false, Ordering::Release);

    for i in 0..count {
        let lba = i + start;

        let major = (flash_config >> 17) & 3;
        let minor = (flash_config >> 4) & 3;
        let block_size = match (major, minor) {
            (major, minor) if major >= 1 && minor == 2 => 0x20000,
            (major, minor) if major >= 1 && minor == 3 => 0x40000,
            _ => 0x4000,
        };

        let sectors_in_block = block_size / 0x200;
        if (lba % sectors_in_block) == 0 {
            if let Err(_) = xbox_state
                .nand_erase_block(Duration::from_secs(1), lba)
                .await
            {
                USB_READ_LBA_CANCELLATION.store(true, Ordering::Release);
                // SAFETY: the error string always fits inside the capacity
                let error_string =
                    unsafe { "timed out erasing nand block".try_into().unwrap_unchecked() };
                USB_WRITE_TASK_CHANNEL
                    .send(Response::Error(error_string))
                    .await;
                continue;
            }
        }

        {
            let data = usb_read_rx.receive().await;
            spiex::byteswap::byte_swap(data.as_mut());

            // SAFETY: the returned future is polled to completion
            unsafe { lba_write_data.as_mut().prepare(write_dma_state, &data) }.await;
        }
        usb_read_rx.receive_done();

        if let Err(_) = xbox_state
            .write_lba(Duration::from_secs(1), lba, &lba_write_data)
            .await
        {
            // SAFETY: the error string always fits inside the capacity
            let error_string = unsafe {
                "timed out writing the lba data"
                    .try_into()
                    .unwrap_unchecked()
            };
            USB_WRITE_TASK_CHANNEL
                .send(Response::Error(error_string))
                .await;

            return SpiexMode::ReadRequest;
        }
    }

    SpiexMode::ReadRequest
}

#[derive(Format)]
enum SpiexMode {
    ReadRequest,
    ReadLbaRange {
        start: u32,
        count: u32,
    },
    WriteLbaRange {
        flash_config: u32,
        start: u32,
        count: u32,
    },
}

#[derive(Debug, Format)]
enum ThingError {
    Endpoint(EndpointError),
    Codec(CodecError),
    Postcard(postcard::Error),
}

impl From<EndpointError> for ThingError {
    fn from(value: EndpointError) -> Self {
        ThingError::Endpoint(value)
    }
}

impl From<CodecError> for ThingError {
    fn from(value: CodecError) -> Self {
        ThingError::Codec(value)
    }
}

impl From<postcard::Error> for ThingError {
    fn from(value: postcard::Error) -> Self {
        ThingError::Postcard(value)
    }
}
