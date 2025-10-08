use defmt::info;
use embassy_executor::raw::{TaskRef, trace::TaskRefTrace};
use embassy_rp::uart::Blocking;
use embassy_sync::blocking_mutex::{Mutex, raw::CriticalSectionRawMutex};
use embassy_time::Instant;
use once_cell::sync::OnceCell;

use core::mem::size_of;

#[derive(Clone)]
enum EventType {
    ExecutorPollStart {
        id: u32,
    }, // 0
    ExecutorIdle {
        id: u32,
    }, // 1
    TaskNew {
        executor_id: u32,
        id: u32,
        name: heapless::String<32>,
    }, // 2
    TaskExecBegin {
        executor_id: u32,
        id: u32,
    }, // 3
    TaskExecEnd {
        executor_id: u32,
        id: u32,
    }, // 4
    // this only takes effect after TaskExecEnd happened if the task was executing
    TaskWaitingBegin {
        executor_id: u32,
        id: u32,
    }, // 5
    TaskEnd {
        executor_id: u32,
        id: u32,
    }, // 6
}

impl EventType {
    const fn max_size() -> usize {
        size_of::<u32>() + size_of::<u8>() + 32
    }

    fn serialized_size(&self) -> usize {
        match self {
            EventType::TaskNew { name, .. } => {
                size_of::<u32>() * 2 + size_of::<u8>() * 2 + name.len()
            }
            _ => size_of::<u32>() * 2 + size_of::<u8>(),
        }
    }

    fn variant_id(&self) -> u8 {
        match self {
            EventType::ExecutorPollStart { .. } => 0,
            EventType::ExecutorIdle { .. } => 1,
            EventType::TaskNew { .. } => 2,
            EventType::TaskExecBegin { .. } => 3,
            EventType::TaskExecEnd { .. } => 4,
            EventType::TaskWaitingBegin { .. } => 5,
            EventType::TaskEnd { .. } => 6,
        }
    }

    /// Panics: if the array is less in size than `serialized_size`
    fn serialize(&self, array: &mut [u8]) {
        array[..size_of::<u8>()].copy_from_slice(&self.variant_id().to_le_bytes());
        let array = &mut array[size_of::<u8>()..];

        match self {
            EventType::ExecutorPollStart { id } | EventType::ExecutorIdle { id } => {
                array[size_of::<u8>()..][..size_of::<u32>()].copy_from_slice(&id.to_le_bytes());
            }
            EventType::TaskExecBegin { executor_id, id }
            | EventType::TaskExecEnd { executor_id, id }
            | EventType::TaskWaitingBegin { executor_id, id }
            | EventType::TaskEnd { executor_id, id } => {
                let array = &mut array[size_of::<u8>()..];
                array[..size_of::<u32>()].copy_from_slice(&executor_id.to_le_bytes());

                let array = &mut array[size_of::<u32>()..];
                array[..size_of::<u32>()].copy_from_slice(&id.to_le_bytes());
            }
            EventType::TaskNew {
                executor_id,
                id,
                name,
            } => {
                let array = &mut array[size_of::<u8>()..];
                array[..size_of::<u32>()].copy_from_slice(&executor_id.to_le_bytes());

                let array = &mut array[size_of::<u32>()..];
                array[..size_of::<u32>()].copy_from_slice(&id.to_le_bytes());

                let array = &mut array[size_of::<u32>()..];
                array[0] = name.len() as u8;

                let array = &mut array[1..];
                array[..name.len()].copy_from_slice(name.as_bytes());
            }
        }
    }
}

#[derive(Clone)]
pub struct Event {
    ty: EventType,
    time_ticks: u64,
}

impl Event {
    pub const fn max_size() -> usize {
        EventType::max_size() + size_of::<u64>()
    }

    pub fn serialized_size(&self) -> usize {
        self.ty.serialized_size() + size_of::<u64>()
    }

    /// Panics: if the array is less in size than `serialized_size`
    pub fn serialize(&self, array: &mut [u8]) {
        let serialized_size = self.ty.serialized_size();
        self.ty.serialize(array);

        let array = &mut array[serialized_size..];
        array[..size_of::<u64>()].copy_from_slice(&self.time_ticks.to_le_bytes());
    }
}

static EVENT_SENDER: OnceCell<
    Mutex<
        CriticalSectionRawMutex,
        embassy_sync::zerocopy_channel::Sender<'static, CriticalSectionRawMutex, Event>,
    >,
> = OnceCell::new();

static BUFFERED_EVENTS: Mutex<CriticalSectionRawMutex, heapless::Vec<Event, 128>> =
    Mutex::new(heapless::Vec::new());

fn try_send(event: Event) -> Option<Event> {
    if let Some(event_sender) = EVENT_SENDER.get() {
        // SAFETY: not reentrant
        unsafe {
            event_sender.lock_mut(|event_sender| {
                if let Some(value) = event_sender.try_send() {
                    *value = event;
                    event_sender.send_done();
                    None
                } else {
                    Some(event)
                }
            })
        }
    } else {
        Some(event)
    }
}

fn write_event(event: Event) {
    // SAFETY: not reentrant
    unsafe {
        BUFFERED_EVENTS.lock_mut(|e| {
            let len = e.len();
            let mut removed = 0;
            for i in 0..len {
                if try_send(e[i].clone()).is_none() {
                    removed += 1;
                } else {
                    break;
                }
            }

            // clear removed elements
            let _ = e.drain(removed..);

            if removed != len {
                // couldn't write all, buffering current event
                e.push(event);
            } else {
                // fit all but last event still didn't fit, buffering...
                if let Some(event) = try_send(event) {
                    e.push(event);
                }
            }
        })
    }
}

#[unsafe(no_mangle)]
unsafe extern "Rust" fn _embassy_trace_poll_start(executor_id: u32) {
    let time_ticks = Instant::now().as_ticks();

    write_event(Event {
        ty: EventType::ExecutorPollStart { id: executor_id },
        time_ticks,
    });
}
#[unsafe(no_mangle)]
unsafe extern "Rust" fn _embassy_trace_executor_idle(executor_id: u32) {
    let time_ticks = Instant::now().as_ticks();

    write_event(Event {
        ty: EventType::ExecutorIdle { id: executor_id },
        time_ticks,
    });
}

#[unsafe(no_mangle)]
unsafe extern "Rust" fn _embassy_trace_task_new(executor_id: u32, task_id: u32) {
    let time_ticks = Instant::now().as_ticks();
    let task_ref: TaskRef = unsafe { core::mem::transmute::<u32, TaskRef>(task_id) };

    write_event(Event {
        ty: EventType::TaskNew {
            executor_id,
            id: task_id,
            name: task_ref
                .name()
                .and_then(|e| e.try_into().ok())
                .unwrap_or_else(|| {
                    // SAFETY: the string always fits inside the capacity
                    unsafe { "unknown".try_into().unwrap_unchecked() }
                }),
        },
        time_ticks,
    });
}
#[unsafe(no_mangle)]
unsafe extern "Rust" fn _embassy_trace_task_end(executor_id: u32, task_id: u32) {
    let time_ticks = Instant::now().as_ticks();

    write_event(Event {
        ty: EventType::TaskEnd {
            executor_id,
            id: task_id,
        },
        time_ticks,
    });
}
#[unsafe(no_mangle)]
unsafe extern "Rust" fn _embassy_trace_task_exec_begin(executor_id: u32, task_id: u32) {
    let time_ticks = Instant::now().as_ticks();

    write_event(Event {
        ty: EventType::TaskExecBegin {
            executor_id,
            id: task_id,
        },
        time_ticks,
    });
}
#[unsafe(no_mangle)]
unsafe extern "Rust" fn _embassy_trace_task_exec_end(executor_id: u32, task_id: u32) {
    let time_ticks = Instant::now().as_ticks();

    write_event(Event {
        ty: EventType::TaskExecEnd {
            executor_id,
            id: task_id,
        },
        time_ticks,
    });
}
#[unsafe(no_mangle)]
unsafe extern "Rust" fn _embassy_trace_task_ready_begin(executor_id: u32, task_id: u32) {
    let time_ticks = Instant::now().as_ticks();

    write_event(Event {
        ty: EventType::TaskWaitingBegin {
            executor_id,
            id: task_id,
        },
        time_ticks,
    });
}
