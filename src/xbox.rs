use embassy_rp::{
    Peri, PeripheralType,
    gpio::Output,
    peripherals::{PIN_20, PIN_21},
};
use embassy_time::{Duration, Instant, Timer};

use crate::spiex::{self, LBA_SIZE, LbaWriteData, RegWriteRequest, SpiMode, SpiexState};

pub struct XboxPins<'a, DbgEn: PeripheralType = PIN_20, RstXdk: PeripheralType = PIN_21> {
    pub dbg_en: Peri<'a, DbgEn>,
    pub rst_xdk: Peri<'a, RstXdk>,
}

pub struct XboxState<'a, PIO: embassy_rp::pio::Instance> {
    dbg_en: Output<'a>,
    rst_xdk: Output<'a>,
    spiex: Option<SpiexState<'a, PIO, SpiMode>>,
}

#[derive(Debug, Copy, Clone)]
pub struct TimeoutError;

impl<'a, PIO: embassy_rp::pio::Instance> XboxState<'a, PIO> {
    pub fn new(pins: XboxPins<'a>, spiex: SpiexState<'a, PIO, SpiMode>) -> Self {
        let dbg_en = Output::new(pins.dbg_en, embassy_rp::gpio::Level::High);
        let rst_xdk = Output::new(pins.rst_xdk, embassy_rp::gpio::Level::High);

        XboxState {
            dbg_en,
            rst_xdk,
            spiex: Some(spiex),
        }
    }

    pub async fn read_lba(
        &mut self,
        timeout: Duration,
        lba: u32,
        out_data: &mut [u8; LBA_SIZE],
    ) -> Result<(), TimeoutError> {
        self.take_spiex(async move |mut spiex| {
            spiex.nand_clear_status().await;
            spiex.write_reg(0x0c, lba << 9).await;
            spiex.write_reg(0x08, 0x03).await;

            if let Err(e) = spiex.nand_wait_ready(timeout).await {
                return (spiex, Err(e));
            }

            spiex.write_reg(0x0c, 0x0).await;

            let mut spiex = spiex.into_lba();

            spiex.read_lba(out_data).await;
            spiex::byteswap::byte_swap(out_data);

            (spiex.into_spi(), Ok(()))
        })
        .await
    }

    pub async fn write_lba(
        &mut self,
        timeout: Duration,
        lba: u32,
        out_data: &LbaWriteData,
    ) -> Result<(), TimeoutError> {
        self.take_spiex(async move |mut spiex| {
            spiex.nand_clear_status().await;
            spiex.write_reg(0x0c, 0x00).await;

            let mut spiex = spiex.into_lba();

            spiex.write_lba(out_data).await;

            let mut spiex = spiex.into_spi();

            if let Err(e) = spiex.nand_wait_ready(timeout).await {
                return (spiex, Err(e));
            }

            spiex.write_reg(0x0c, lba << 9).await;

            if let Err(e) = spiex.nand_wait_ready(timeout).await {
                return (spiex, Err(e));
            }

            let mut spiex = spiex.into_reg_bulk();

            spiex
                .write_reg_bulk([
                    RegWriteRequest::new(0x08, 0x55),
                    RegWriteRequest::new(0x08, 0xaa),
                    RegWriteRequest::new(0x08, 0x04),
                ])
                .await;

            let mut spiex = spiex.into_spi();

            if let Err(e) = spiex.nand_wait_ready(timeout).await {
                return (spiex, Err(e));
            }

            (spiex, Ok(()))
        })
        .await
    }

    pub async fn get_flash_config(&mut self) -> u32 {
        self.spiex.as_mut().unwrap().get_flash_config().await
    }

    /// Erases blocksize bytes, not a singular lba
    pub async fn nand_erase_block(
        &mut self,
        timeout: Duration,
        lba: u32,
    ) -> Result<(), TimeoutError> {
        self.spiex
            .as_mut()
            .unwrap()
            .nand_erase_block(lba, timeout)
            .await
    }

    pub async fn start_smc(&mut self) {
        // todo: switch spiex into NoneMode?
        // can make panic much more likely in other functions on unwrapping though...
        self.dbg_en.set_low();
        self.rst_xdk.set_low();

        Timer::after(Duration::from_millis(50)).await;

        self.rst_xdk.set_high();
    }

    pub async fn stop_smc(&mut self) {
        let spiex = self.spiex.as_mut().unwrap();

        self.dbg_en.set_low();

        Timer::after_millis(50).await;

        spiex.set_cs(embassy_rp::gpio::Level::Low);
        self.rst_xdk.set_low();

        Timer::after_millis(50).await;

        self.dbg_en.set_high();
        self.rst_xdk.set_high();

        Timer::after_millis(50).await;

        spiex.set_cs(embassy_rp::gpio::Level::High);

        Timer::after_millis(50).await;
    }

    async fn take_spiex<
        ActualRet,
        Ret: Future<Output = (SpiexState<'a, PIO, SpiMode>, ActualRet)>,
        F: FnOnce(SpiexState<'a, PIO, SpiMode>) -> Ret,
    >(
        &mut self,
        callback: F,
    ) -> ActualRet {
        let (spiex, ret) = callback(self.spiex.take().unwrap()).await;
        self.spiex = Some(spiex);
        ret
    }
}

trait XboxSpiex<'a, PIO: embassy_rp::pio::Instance> {
    async fn get_flash_config(&mut self) -> u32;
    async fn nand_get_status(&mut self) -> u32;
    async fn nand_clear_status(&mut self);
    async fn nand_erase_block(&mut self, lba: u32, timeout: Duration) -> Result<(), TimeoutError>;
    async fn nand_wait_ready(&mut self, timeout: Duration) -> Result<(), TimeoutError>;
}

impl<'a, PIO: embassy_rp::pio::Instance> XboxSpiex<'a, PIO> for SpiexState<'a, PIO, SpiMode> {
    async fn get_flash_config(&mut self) -> u32 {
        self.read_reg(0x00).await
    }

    async fn nand_get_status(&mut self) -> u32 {
        self.read_reg(0x04).await
    }

    async fn nand_clear_status(&mut self) {
        let status = self.nand_get_status().await;
        self.write_reg(0x04, status).await;
    }

    async fn nand_erase_block(&mut self, lba: u32, timeout: Duration) -> Result<(), TimeoutError> {
        self.nand_clear_status().await;

        let zero_reg_value = self.read_reg(0x00).await;
        self.write_reg(0x00, zero_reg_value | 0x08).await;

        self.write_reg(0x0c, lba << 9).await;

        self.write_reg(0x08, 0xaa).await;
        self.write_reg(0x08, 0x55).await;
        self.write_reg(0x08, 0x05).await;

        self.nand_wait_ready(timeout).await
    }

    async fn nand_wait_ready(&mut self, timeout: Duration) -> Result<(), TimeoutError> {
        let start = Instant::now();
        loop {
            let status = self.nand_get_status().await;
            if (status & 0x1) == 0 {
                return Ok(());
            }

            if Instant::now().duration_since(start) > timeout {
                return Err(TimeoutError);
            }
        }
    }
}

// this wouldn't be needed if we had partial borrows guh
// struct XboxSpiex<'a, PIO: embassy_rp::pio::Instance> {
//     spiex: SpiexState<'a, PIO, SpiMode>,
// }

// impl<'a, PIO: embassy_rp::pio::Instance> XboxSpiex<'a, PIO> {}
