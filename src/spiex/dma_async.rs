use core::cell::Cell;
use core::task::{Context, Poll, Waker};

use embassy_rp::dma::Channel;
use embassy_rp::interrupt::InterruptExt;
use embassy_rp::{Peri, interrupt};
use embassy_sync::blocking_mutex::Mutex;
use embassy_sync::blocking_mutex::raw::{CriticalSectionRawMutex, RawMutex};

use crate::spiex::DMAState;

#[interrupt]
fn DMA_IRQ_1() {
    let ints1 = embassy_rp::pac::DMA.ints(1).read();

    for channel in 0..CHANNEL_COUNT {
        let ctrl_trig = embassy_rp::pac::DMA.ch(channel).ctrl_trig().read();
        if ctrl_trig.ahb_error() {
            panic!("DMA: error on DMA_0 channel {}", channel);
        }

        if ints1 & (1 << channel) == (1 << channel) {
            CHANNEL_WAKERS[channel].signal(());
        }
    }

    embassy_rp::pac::DMA.ints(1).write_value(ints1);
}

/// Initialize async dma
///
/// The channels that are passed in dma state here will not be available for use with embassy transfers
/// and should only be used with custom async transfers
pub fn init<DMA0: Channel, DMA1: Channel>(dma_state: &DMAState<DMA0, DMA1>) {
    interrupt::DMA_IRQ_1.disable();
    // setting priority higher than dma_irq_0 registered by embassy
    interrupt::DMA_IRQ_1.set_priority(interrupt::Priority::P2);

    // inte 0 is used by embassy, using inte 1 for custom wakers
    // registering only requested dma channels otherwise embassy transfers will break on all
    embassy_rp::pac::DMA
        .inte(1)
        .write_value(1 << dma_state.dma0.number() | 1 << dma_state.dma1.number());

    unsafe { interrupt::DMA_IRQ_1.enable() };
}

const CHANNEL_COUNT: usize = 12;
static CHANNEL_WAKERS: [Signal<CriticalSectionRawMutex, ()>; CHANNEL_COUNT] =
    [const { Signal::new() }; CHANNEL_COUNT];

pub struct CustomTransfer<'a, C: Channel> {
    channel: Peri<'a, C>,
}

impl<'a, C: Channel> CustomTransfer<'a, C> {
    pub fn new(channel: Peri<'a, C>) -> Self {
        Self { channel }
    }
}

impl<'a, C: Channel> Unpin for CustomTransfer<'a, C> {}
impl<'a, C: Channel> Future for CustomTransfer<'a, C> {
    type Output = ();

    fn poll(
        self: core::pin::Pin<&mut Self>,
        cx: &mut core::task::Context<'_>,
    ) -> core::task::Poll<Self::Output> {
        match CHANNEL_WAKERS[self.channel.number() as usize].poll_wait(cx) {
            Poll::Ready(_) => {
                CHANNEL_WAKERS[self.channel.number() as usize].reset();
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

struct Signal<M, T>
where
    M: RawMutex,
{
    state: Mutex<M, Cell<State<T>>>,
}

enum State<T> {
    None,
    Waiting(Waker),
    Signaled(T),
}

impl<M, T> Signal<M, T>
where
    M: RawMutex,
{
    /// Create a new `Signal`.
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(Cell::new(State::None)),
        }
    }
}

impl<M, T> Default for Signal<M, T>
where
    M: RawMutex,
{
    fn default() -> Self {
        Self::new()
    }
}

impl<M, T> Signal<M, T>
where
    M: RawMutex,
{
    /// Mark this Signal as signaled.
    pub fn signal(&self, val: T) {
        self.state.lock(|cell| {
            let state = cell.replace(State::Signaled(val));
            if let State::Waiting(waker) = state {
                waker.wake();
            }
        })
    }

    /// Remove the queued value in this `Signal`, if any.
    pub fn reset(&self) {
        self.state.lock(|cell| cell.set(State::None));
    }

    pub fn poll_wait(&self, cx: &mut Context<'_>) -> Poll<T> {
        self.state.lock(|cell| {
            let state = cell.replace(State::None);
            match state {
                State::None => {
                    cell.set(State::Waiting(cx.waker().clone()));
                    Poll::Pending
                }
                State::Waiting(w) if w.will_wake(cx.waker()) => {
                    cell.set(State::Waiting(w));
                    Poll::Pending
                }
                State::Waiting(w) => {
                    cell.set(State::Waiting(cx.waker().clone()));
                    w.wake();
                    Poll::Pending
                }
                State::Signaled(res) => Poll::Ready(res),
            }
        })
    }
}
