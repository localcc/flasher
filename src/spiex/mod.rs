use core::{
    cell::UnsafeCell,
    marker::{PhantomData, PhantomPinned},
    sync::atomic::{Ordering, compiler_fence},
};

use defmt::info;
use embassy_futures::join::join;
use embassy_rp::{
    Peri, PeripheralType,
    clocks::clk_sys_freq,
    dma::Channel,
    peripherals::{DMA_CH1, DMA_CH2},
    pio::{self, Pin, Pio, PioPin, StateMachine},
};
use fixed::{traits::ToFixed as _, types::extra::U8};
use zerocopy::{FromBytes, Immutable, IntoBytes, Unalign};

use crate::spiex::dma_async::CustomTransfer;

pub mod byteswap;
mod dma_async;

/// Initialize async dma
///
/// The channels that are passed in dma state here will not be available for use with embassy transfers
/// and should only be used with [`LbaWriteData`]
pub fn init<DMA0: Channel, DMA1: Channel>(dma_state: &DMAState<DMA0, DMA1>) {
    dma_async::init(dma_state);
}

pub struct DMAState<'a, DMA0: PeripheralType = DMA_CH1, DMA1: PeripheralType = DMA_CH2> {
    pub dma0: Peri<'a, DMA0>,
    pub dma1: Peri<'a, DMA1>,
}

pub struct SpiMode;
pub struct LbaMode;
pub struct RegBulkMode;

pub struct SpiexState<'a, PIO: embassy_rp::pio::Instance, Mode> {
    dma_state: DMAState<'a>,
    pio: Pio<'a, PIO>,

    tx_pin: Pin<'a, PIO>,
    rx_pin: Pin<'a, PIO>,
    clk_pin: Pin<'a, PIO>,
    cs_pin: Pin<'a, PIO>,

    _marker: PhantomData<Mode>,
}

impl<'a, PIO: embassy_rp::pio::Instance> SpiexState<'a, PIO, SpiMode> {
    pub fn new(
        dma_state: DMAState<'a>,
        mut pio: Pio<'a, PIO>,
        tx: Peri<'a, impl PioPin + 'a>,
        rx: Peri<'a, impl PioPin + 'a>,
        clk: Peri<'a, impl PioPin + 'a>,
        cs: Peri<'a, impl PioPin + 'a>,
    ) -> Self {
        let clock_divider = calculate_clock_divider(14_000_000);

        let mut tx_pin = pio.common.make_pio_pin(tx);
        let mut rx_pin = pio.common.make_pio_pin(rx);
        let mut clk_pin = pio.common.make_pio_pin(clk);
        let mut cs_pin = pio.common.make_pio_pin(cs);

        tx_pin.set_input_sync_bypass(true);
        rx_pin.set_input_sync_bypass(true);
        rx_pin.set_pull(embassy_rp::gpio::Pull::Up);
        clk_pin.set_input_sync_bypass(true);
        cs_pin.set_input_sync_bypass(true);

        tx_pin.set_drive_strength(embassy_rp::gpio::Drive::_12mA);
        rx_pin.set_drive_strength(embassy_rp::gpio::Drive::_12mA);
        clk_pin.set_drive_strength(embassy_rp::gpio::Drive::_12mA);
        cs_pin.set_drive_strength(embassy_rp::gpio::Drive::_12mA);

        // sm0/sm1 must be kept in sync with SpiexExt impl
        configure_spi_sm(
            &mut pio.common,
            &mut pio.sm0,
            clock_divider,
            &tx_pin,
            &rx_pin,
            &clk_pin,
        );
        configure_lba_sm(
            &mut pio.common,
            &mut pio.sm1,
            clock_divider,
            &tx_pin,
            &rx_pin,
            &clk_pin,
            &cs_pin,
        );
        configure_reg_bulk_sm(
            &mut pio.common,
            &mut pio.sm2,
            clock_divider,
            &tx_pin,
            &clk_pin,
            &cs_pin,
        );

        pio.spi_sm()
            .set_pins(embassy_rp::gpio::Level::High, &[&cs_pin]);
        pio.spi_sm().set_enable(true);

        SpiexState {
            dma_state,
            pio,
            tx_pin,
            rx_pin,
            clk_pin,
            cs_pin,
            _marker: PhantomData,
        }
    }

    pub async fn read_reg(&mut self, reg: u8) -> u32 {
        let mut out_data = [(reg << 2) | 1, 0xff, 0, 0, 0, 0];
        byteswap::byte_swap(&mut out_data);

        let mut in_data = [0u8; 6];

        self.set_cs(embassy_rp::gpio::Level::Low);

        let (spi_rx, spi_tx) = self.pio.spi_sm().rx_tx();

        let rx = spi_rx.dma_pull(self.dma_state.dma0.reborrow(), &mut in_data, false);
        let tx = spi_tx.dma_push(self.dma_state.dma1.reborrow(), &out_data, false);

        join(rx, tx).await;

        self.set_cs(embassy_rp::gpio::Level::High);

        byteswap::byte_swap(&mut in_data);
        u32::from_le_bytes(in_data[2..].try_into().unwrap())
    }

    pub async fn write_reg(&mut self, reg: u8, value: u32) {
        let mut out_data = [(reg << 2) | 2, 0, 0, 0, 0];
        out_data[1..].copy_from_slice(&value.to_le_bytes());
        byteswap::byte_swap(&mut out_data);

        let mut in_data = [0u8; 5];

        self.set_cs(embassy_rp::gpio::Level::Low);

        let (spi_rx, spi_tx) = self.pio.spi_sm().rx_tx();

        let rx = spi_rx.dma_pull(self.dma_state.dma0.reborrow(), &mut in_data, false);
        let tx = spi_tx.dma_push(self.dma_state.dma1.reborrow(), &out_data, false);

        join(rx, tx).await;

        self.set_cs(embassy_rp::gpio::Level::High);
    }

    pub fn set_cs(&mut self, level: embassy_rp::gpio::Level) {
        self.pio.spi_sm().set_pins(level, &[&self.cs_pin]);
    }

    pub fn into_lba(mut self) -> SpiexState<'a, PIO, LbaMode> {
        self.pio.spi_sm().set_enable(false);
        self.pio.lba_sm().set_enable(true);

        SpiexState {
            dma_state: self.dma_state,
            pio: self.pio,
            tx_pin: self.tx_pin,
            rx_pin: self.rx_pin,
            clk_pin: self.clk_pin,
            cs_pin: self.cs_pin,
            _marker: PhantomData,
        }
    }

    pub fn into_reg_bulk(mut self) -> SpiexState<'a, PIO, RegBulkMode> {
        self.pio.spi_sm().set_enable(false);
        self.pio.reg_bulk_sm().set_enable(true);

        SpiexState {
            dma_state: self.dma_state,
            pio: self.pio,
            tx_pin: self.tx_pin,
            rx_pin: self.rx_pin,
            clk_pin: self.clk_pin,
            cs_pin: self.cs_pin,
            _marker: PhantomData,
        }
    }
}

impl<'a, PIO: embassy_rp::pio::Instance> SpiexState<'a, PIO, RegBulkMode> {
    pub async fn write_reg_bulk<const T: usize>(&mut self, requests: [RegWriteRequest; T]) {
        let (_, spi_tx) = self.pio.reg_bulk_sm().rx_tx();

        spi_tx
            .dma_push(self.dma_state.dma1.reborrow(), requests.as_bytes(), false)
            .await;
    }

    pub fn into_spi(mut self) -> SpiexState<'a, PIO, SpiMode> {
        self.pio.reg_bulk_sm().set_enable(false);
        self.pio.spi_sm().set_enable(true);

        SpiexState {
            dma_state: self.dma_state,
            pio: self.pio,
            tx_pin: self.tx_pin,
            rx_pin: self.rx_pin,
            clk_pin: self.clk_pin,
            cs_pin: self.cs_pin,
            _marker: PhantomData,
        }
    }
}

#[derive(IntoBytes, FromBytes, Immutable)]
#[repr(C)]
pub struct RegWriteRequest {
    bit_count: u8,
    reg: u8,
    value: Unalign<u32>,
}

impl RegWriteRequest {
    pub const fn new(reg: u8, value: u32) -> Self {
        let request = RegWriteRequest {
            bit_count: 0,
            reg: (reg << 2) | 2,
            value: Unalign::new(value),
        };

        // ugly dance to byteswap in const fn
        let mut bytes: [u8; core::mem::size_of::<RegWriteRequest>()] =
            zerocopy::transmute!(request);
        byteswap::byte_swap(&mut bytes);

        let mut request: RegWriteRequest = zerocopy::transmute!(bytes);
        request.bit_count =
            (core::mem::size_of::<u8>() * 8 + core::mem::size_of::<u32>() * 8) as u8 - 1;

        request
    }
}

#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
struct LbaReadCommand {
    first_reg: u8,
    first_reg_value: Unalign<u32>,
    second_reg: u8,
    output_read_marker: u8,
}

impl LbaReadCommand {
    const fn default() -> Self {
        LbaReadCommand {
            first_reg: 0,
            first_reg_value: Unalign::new(0),
            second_reg: 0,
            output_read_marker: 0,
        }
    }
}

#[derive(Clone, Copy, FromBytes, IntoBytes, Immutable)]
#[repr(C)]
struct LbaWriteCommand {
    first_reg: u8,
    first_reg_value: Unalign<u32>,
    second_reg: u8,
    output_read_marker: u8,
    second_reg_value: u8,
}

impl LbaWriteCommand {
    const fn default() -> Self {
        LbaWriteCommand {
            first_reg: 0,
            first_reg_value: Unalign::new(0),
            second_reg: 0,
            output_read_marker: 0,
            second_reg_value: 0,
        }
    }
}

pub const LBA_SIZE: usize = 0x200 + 0x10;
const LBA_COMMAND_BUFFER_SIZE: usize = LBA_SIZE / core::mem::size_of::<u32>();
const LBA_READ_COMMAND_BUFFER: [LbaReadCommand; LBA_COMMAND_BUFFER_SIZE] = {
    let mut command_buffer = [LbaReadCommand::default(); LBA_COMMAND_BUFFER_SIZE];
    let mut i = 0;
    while i < command_buffer.len() {
        let command = LbaReadCommand {
            first_reg: (0x08 << 2) | 2,
            first_reg_value: Unalign::new(0),
            second_reg: (0x10 << 2) | 1,
            output_read_marker: 0,
        };

        // ugly dance to byteswap in const fn
        let mut bytes: [u8; core::mem::size_of::<LbaReadCommand>()] = zerocopy::transmute!(command);
        byteswap::byte_swap(&mut bytes);

        let mut command: LbaReadCommand = zerocopy::transmute!(bytes);

        // doing this after byteswap as this is a command interpreted by the pio
        // specify that we want to output the read marker (0xff)
        command.output_read_marker = 7;

        command_buffer[i] = command;

        i += 1;
    }

    command_buffer
};
const LBA_WRITE_COMMAND_BUFFER: [LbaWriteCommand; LBA_COMMAND_BUFFER_SIZE] = {
    let mut command_buffer = [LbaWriteCommand::default(); LBA_COMMAND_BUFFER_SIZE];
    let mut i = 0;
    while i < command_buffer.len() {
        let command = LbaWriteCommand {
            first_reg: (0x10 << 2) | 2,
            first_reg_value: Unalign::new(0),
            second_reg: (0x08 << 2) | 2,
            output_read_marker: 0,
            second_reg_value: 0x01,
        };

        // ugly dance to byteswap in const fn
        let mut bytes: [u8; core::mem::size_of::<LbaWriteCommand>()] =
            zerocopy::transmute!(command);
        byteswap::byte_swap(&mut bytes);

        let command: LbaWriteCommand = zerocopy::transmute!(bytes);
        command_buffer[i] = command;

        i += 1;
    }

    command_buffer
};

impl<'a, PIO: embassy_rp::pio::Instance> SpiexState<'a, PIO, LbaMode> {
    pub async fn read_lba(&mut self, out_data: &mut [u8; LBA_SIZE]) {
        let (lba_rx, lba_tx) = self.pio.lba_sm().rx_tx();

        let rx = lba_rx.dma_pull(self.dma_state.dma0.reborrow(), out_data, false);
        let tx = lba_tx.dma_push(
            self.dma_state.dma1.reborrow(),
            LBA_READ_COMMAND_BUFFER.as_bytes(),
            false,
        );

        join(rx, tx).await;
    }

    pub async fn write_lba(&mut self, data: &LbaWriteData) {
        let (_, lba_tx) = self.pio.lba_sm().rx_tx();

        lba_tx
            .dma_push(
                self.dma_state.dma1.reborrow(),
                data.get_pio_buffer().as_bytes(),
                false,
            )
            .await;
    }

    pub fn into_spi(mut self) -> SpiexState<'a, PIO, SpiMode> {
        self.pio.lba_sm().set_enable(false);
        self.pio.spi_sm().set_enable(true);

        SpiexState {
            dma_state: self.dma_state,
            pio: self.pio,
            tx_pin: self.tx_pin,
            rx_pin: self.rx_pin,
            clk_pin: self.clk_pin,
            cs_pin: self.cs_pin,
            _marker: PhantomData,
        }
    }
}

#[derive(Default, Clone, Copy, FromBytes, Immutable)]
#[repr(C)]
struct LbaWriteDmaCommand {
    write_addr: u32,
    write_count: u32,
}

pub struct LbaWriteData {
    pio_buffer: UnsafeCell<[LbaWriteCommand; LBA_COMMAND_BUFFER_SIZE]>,
    // dma commands write 1 word into the pio command buffer, so the amount of them is the same
    // +1 for the last empty command to halt transfers
    dma_commands: [LbaWriteDmaCommand; LBA_COMMAND_BUFFER_SIZE + 1],
    _marker: PhantomPinned,
}

impl LbaWriteData {
    // SAFETY: must initialize later with `init`
    pub unsafe fn uninit() -> Self {
        LbaWriteData {
            pio_buffer: UnsafeCell::new([LbaWriteCommand::default(); LBA_COMMAND_BUFFER_SIZE]),
            dma_commands: [LbaWriteDmaCommand::default(); LBA_COMMAND_BUFFER_SIZE + 1],
            _marker: PhantomPinned,
        }
    }

    // SAFETY: must not have been initialized previously
    pub async unsafe fn init<DMA: embassy_rp::dma::Channel>(
        self: core::pin::Pin<&mut Self>,
        dma: Peri<'_, DMA>,
    ) {
        // SAFETY: self is never moved out
        let this = unsafe { self.get_unchecked_mut() };

        // SAFETY: from/to are in dma accessible buffers
        let transfer = unsafe {
            embassy_rp::dma::copy(
                dma,
                LBA_WRITE_COMMAND_BUFFER.as_bytes(),
                this.pio_buffer.as_mut_bytes(),
            )
        };
        transfer.await;

        for (index, dma_command) in this
            .dma_commands
            .iter_mut()
            .take(LBA_COMMAND_BUFFER_SIZE)
            .enumerate()
        {
            let lba_command_ptr = &this.pio_buffer.get_mut()[index] as *const LbaWriteCommand
                as *const core::ffi::c_void;
            // SAFETY: the offset cannot be out of bounds or wrapping on the end of the address space as it's always inside LbaCommand
            let dma_command_dst = unsafe {
                lba_command_ptr.byte_add(core::mem::offset_of!(LbaWriteCommand, first_reg_value))
            };

            dma_command.write_addr = dma_command_dst as u32;
            dma_command.write_count = 4;
        }

        this.dma_commands[LBA_COMMAND_BUFFER_SIZE] = LbaWriteDmaCommand {
            write_addr: 0,
            write_count: 0,
        };
    }

    // Prepare the write data
    //
    // # Invariants
    //
    // dma_state DMA channels should differ from the ones used in SpiexState
    // this is because we reconfigure them into a state which embassy-rp might not reconfigure them back from
    // causing future transfers to fail
    //
    // # Safety
    //
    // Returned future must be polled to completion
    pub async unsafe fn prepare<DMA0: Channel, DMA1: Channel>(
        self: core::pin::Pin<&mut Self>,
        dma_state: &mut DMAState<'_, DMA0, DMA1>,
        lba_data: &[u8; LBA_SIZE],
    ) {
        // SAFETY: we never move out of this
        let this = unsafe { self.get_unchecked_mut() };

        {
            let dma0 = dma_state.dma0.regs();
            dma0.read_addr()
                .write_value(lba_data.as_ptr().addr() as u32);
            // we don't need a write address/transfer count as they will be populated by the control channel

            compiler_fence(Ordering::SeqCst);

            let mut ctrl = dma0.ctrl_trig().read();
            ctrl.set_data_size(embassy_rp::pac::dma::vals::DataSize::SIZE_BYTE);
            ctrl.set_chain_to(dma_state.dma1.number());
            ctrl.set_incr_read(true);
            ctrl.set_incr_write(true);
            ctrl.set_bswap(false);
            ctrl.set_irq_quiet(true);
            ctrl.set_en(true);

            dma0.al3_ctrl().write(|w| *w = ctrl.0);

            compiler_fence(Ordering::SeqCst);
        }

        // control channel
        {
            let dma1 = dma_state.dma1.regs();
            dma1.read_addr()
                .write_value(this.dma_commands.as_ptr().addr() as u32);

            let dma0_al1_write_trans_ptr = dma_state.dma0.regs().al1_write_addr().as_ptr();
            dma1.write_addr()
                .write_value(dma0_al1_write_trans_ptr.addr() as u32);

            dma1.trans_count().write_value(
                (core::mem::size_of::<LbaWriteDmaCommand>() / core::mem::size_of::<u32>()) as u32,
            );

            compiler_fence(Ordering::SeqCst);

            dma1.ctrl_trig().write(|w| {
                w.set_data_size(embassy_rp::pac::dma::vals::DataSize::SIZE_WORD);
                w.set_chain_to(dma_state.dma1.number());
                w.set_incr_read(true);
                w.set_incr_write(true);
                w.set_ring_sel(true);
                w.set_ring_size(3);
                w.set_bswap(false);
                w.set_en(true);
            });

            compiler_fence(Ordering::SeqCst);
        }

        CustomTransfer::new(dma_state.dma0.reborrow()).await;
    }

    fn get_pio_buffer(&self) -> &[LbaWriteCommand; LBA_COMMAND_BUFFER_SIZE] {
        // SAFETY: no mutable references exist to the pio buffer at this time as we are behind an immutable reference
        // SAFETY: data of the underlying buffer cannot change as all functions that change it require a mut reference
        unsafe { &*(self.pio_buffer.get() as *const _) }
    }
}

trait SpiexExt<'a, PIO: embassy_rp::pio::Instance> {
    fn spi_sm(&mut self) -> &mut StateMachine<'a, PIO, 0>;
    fn lba_sm(&mut self) -> &mut StateMachine<'a, PIO, 1>;
    fn reg_bulk_sm(&mut self) -> &mut StateMachine<'a, PIO, 2>;
}

impl<'a, PIO: embassy_rp::pio::Instance + 'a> SpiexExt<'a, PIO> for Pio<'a, PIO> {
    fn spi_sm(&mut self) -> &mut StateMachine<'a, PIO, 0> {
        &mut self.sm0
    }

    fn lba_sm(&mut self) -> &mut StateMachine<'a, PIO, 1> {
        &mut self.sm1
    }

    fn reg_bulk_sm(&mut self) -> &mut StateMachine<'a, PIO, 2> {
        &mut self.sm2
    }
}

fn calculate_clock_divider(frequency_hz: u32) -> fixed::FixedU32<U8> {
    // we multiply by 4 since each clock period is equal to 4 instructions

    info!("{}", clk_sys_freq());
    let sys_freq = clk_sys_freq().to_fixed::<fixed::FixedU64<U8>>();
    let target_freq = (frequency_hz * 4).to_fixed::<fixed::FixedU64<U8>>();
    (sys_freq / target_freq).to_fixed()
}

fn configure_spi_sm<'d, const SM: usize, PIO: embassy_rp::pio::Instance + 'd>(
    common: &mut embassy_rp::pio::Common<'d, PIO>,
    sm: &mut StateMachine<'d, PIO, SM>,
    clock_divider: fixed::FixedU32<U8>,
    tx_pin: &Pin<'d, PIO>,
    rx_pin: &Pin<'d, PIO>,
    clk_pin: &Pin<'d, PIO>,
) {
    let rw_spi_program = pio::program::pio_asm!(
        r#"
        .side_set 1
        .wrap_target
            out pins, 1 side 0b0 [1]
            in pins, 1 side 0b1 [1]
        .wrap
        "#
    );
    let rw_spi_program = common.load_program(&rw_spi_program.program);

    let mut config = embassy_rp::pio::Config::default();
    config.use_program(&rw_spi_program, &[clk_pin]);

    config.set_out_pins(&[tx_pin]);
    config.set_in_pins(&[rx_pin]);

    config.shift_in = pio::ShiftConfig {
        threshold: 8,
        direction: pio::ShiftDirection::Left,
        auto_fill: true,
    };
    config.shift_out = pio::ShiftConfig {
        threshold: 8,
        direction: pio::ShiftDirection::Left,
        auto_fill: true,
    };
    config.clock_divider = clock_divider;

    sm.set_config(&config);

    sm.set_pins(embassy_rp::gpio::Level::Low, &[&tx_pin, &clk_pin]);
    sm.set_pin_dirs(embassy_rp::pio::Direction::Out, &[&tx_pin, &clk_pin]);
}

fn configure_lba_sm<'d, const SM: usize, PIO: embassy_rp::pio::Instance + 'd>(
    common: &mut embassy_rp::pio::Common<'d, PIO>,
    sm: &mut StateMachine<'d, PIO, SM>,
    clock_divider: fixed::FixedU32<U8>,
    tx_pin: &Pin<'d, PIO>,
    rx_pin: &Pin<'d, PIO>,
    clk_pin: &Pin<'d, PIO>,
    cs_pin: &Pin<'d, PIO>,
) {
    let lba_program = pio::program::pio_asm!(
        r#"
            .side_set 2
            .wrap_target
            start:
            // sideset layout: [ spi_clk, spi_cs ]
            set x, 7 side 0b01
            first_reg_loop:
                out pins, 1 side 0b00 [1]
                jmp x-- first_reg_loop side 0b10 [1]
            set x, 31 side 0b00
            first_reg_value_loop:
                out pins, 1 side 0b00 [1]
                jmp x-- first_reg_value_loop side 0b10 [1]
            set x, 7 side 0b01
            second_reg_loop:
                out pins, 1 side 0b00 [1]
                jmp x-- second_reg_loop side 0b10 [1]
            out x, 8 side 0b00
            jmp !x write_start side 0b00
            second_reg_read_marker_loop:
                set pins, 1 side 0b00 [1]
                jmp x-- second_reg_read_marker_loop side 0b10 [1]
            set x, 31 side 0b00
            read_loop:
                set pins, 0 side 0b00 [1]
                in pins, 1 side 0b10
                jmp x-- read_loop side 0b10
            jmp start side 0b00
            write_start:
            set x, 7 side 0b00
            write_loop_firstbyte:
                out pins, 1 side 0b00 [1]
                jmp x-- write_loop_firstbyte side 0b10 [1]
            set x, 23 side 0b00
            write_loop:
                set pins, 0 side 0b00 [1]
                jmp x-- write_loop side 0b10 [1]
            .wrap
        "#
    );
    let lba_program = common.load_program(&lba_program.program);

    let mut config = embassy_rp::pio::Config::default();

    config.use_program(&lba_program, &[&cs_pin, &clk_pin]);

    config.set_set_pins(&[&tx_pin]);
    config.set_out_pins(&[&tx_pin]);
    config.set_in_pins(&[&rx_pin]);

    config.shift_in = pio::ShiftConfig {
        threshold: 8,
        direction: pio::ShiftDirection::Left,
        auto_fill: true,
    };
    config.shift_out = pio::ShiftConfig {
        threshold: 8,
        direction: pio::ShiftDirection::Left,
        auto_fill: true,
    };

    config.clock_divider = clock_divider;

    sm.set_config(&config);

    sm.set_pins(embassy_rp::gpio::Level::Low, &[&tx_pin, &clk_pin]);
    sm.set_pins(embassy_rp::gpio::Level::High, &[&cs_pin]);
    sm.set_pin_dirs(
        embassy_rp::pio::Direction::Out,
        &[&tx_pin, &clk_pin, &cs_pin],
    );
    sm.set_pin_dirs(embassy_rp::pio::Direction::In, &[&rx_pin]);
}

fn configure_reg_bulk_sm<'d, const SM: usize, PIO: embassy_rp::pio::Instance + 'd>(
    common: &mut embassy_rp::pio::Common<'d, PIO>,
    sm: &mut StateMachine<'d, PIO, SM>,
    clock_divider: fixed::FixedU32<U8>,
    tx_pin: &Pin<'d, PIO>,
    clk_pin: &Pin<'d, PIO>,
    cs_pin: &Pin<'d, PIO>,
) {
    let reg_bulk_program = pio::program::pio_asm!(
        r#"
            .side_set 2
            .wrap_target
            // sideset layout: [ spi_clk, spi_cs ]
            out x, 8 side 0b01
            write_loop:
                out pins, 1 side 0b00 [1]
                jmp x-- write_loop side 0b10 [1]
            .wrap
        "#
    );

    let reg_bulk_program = common.load_program(&reg_bulk_program.program);

    let mut config = embassy_rp::pio::Config::default();

    config.use_program(&reg_bulk_program, &[&cs_pin, &clk_pin]);

    config.set_out_pins(&[&tx_pin]);

    config.shift_in = pio::ShiftConfig {
        threshold: 8,
        direction: pio::ShiftDirection::Left,
        auto_fill: true,
    };
    config.shift_out = pio::ShiftConfig {
        threshold: 8,
        direction: pio::ShiftDirection::Left,
        auto_fill: true,
    };

    config.clock_divider = clock_divider;

    sm.set_config(&config);

    sm.set_pins(embassy_rp::gpio::Level::Low, &[&tx_pin, &clk_pin]);
    sm.set_pins(embassy_rp::gpio::Level::High, &[&cs_pin]);
    sm.set_pin_dirs(
        embassy_rp::pio::Direction::Out,
        &[&tx_pin, &clk_pin, &cs_pin],
    );
}
