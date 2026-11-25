#![allow(clippy::not_unsafe_ptr_arg_deref)]

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

use crate::object::{
    self, ComObject, E_NOTIMPL, FENCE, HRESULT, Iid, Object, S_OK, Table, This, guid,
};
use crate::resource::PAGEABLE_IIDS;

pub(crate) const FENCE_IIDS: [Iid; 4] = [
    guid(
        0x0a753dcf,
        0xc4d8,
        0x4b91,
        [0xad, 0xf6, 0xbe, 0x5a, 0x60, 0xd9, 0x5a, 0x76],
    ),
    PAGEABLE_IIDS[0],
    PAGEABLE_IIDS[1],
    PAGEABLE_IIDS[2],
];

#[derive(Clone, Copy)]
struct Waiter {
    value: u64,
    event: usize,
}

impl Waiter {
    fn wake(self) {
        #[cfg(windows)]
        unsafe {
            windows_sys::Win32::System::Threading::SetEvent(self.event as *mut c_void)
        };
    }
}

#[repr(C)]
pub(crate) struct Fence {
    object: Object,
    value: AtomicU64,
    flags: u32,
    waiters: Mutex<Vec<Waiter>>,
}

unsafe impl ComObject for Fence {
    const KIND: usize = FENCE;
    const INTERFACES: &'static [Iid] = &FENCE_IIDS;
}

impl Fence {
    fn new(initial: u64, flags: u32) -> Box<Self> {
        Box::new(Self {
            object: Object::new(&FUNCTIONS),
            value: AtomicU64::new(initial),
            flags,
            waiters: Mutex::new(Vec::new()),
        })
    }

    fn waiters(&self) -> MutexGuard<'_, Vec<Waiter>> {
        self.waiters.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn reached(&self, value: u64) -> bool {
        self.value.load(Ordering::Acquire) >= value
    }

    pub(crate) fn completed(this: This) -> u64 {
        Self::from_this(this).value.load(Ordering::Acquire)
    }

    pub(crate) fn advance(this: This, value: u64) {
        let fence = Self::from_this(this);
        let mut waiters = fence.waiters();
        fence.value.store(value, Ordering::Release);
        waiters.retain(|&waiter| {
            let reached = waiter.value <= value;
            if reached {
                waiter.wake();
            }
            !reached
        });
    }

    fn notify(&self, value: u64, event: *mut c_void) -> HRESULT {
        let mut waiters = self.waiters();
        if self.reached(value) {
            if !event.is_null() {
                Waiter {
                    value,
                    event: event as usize,
                }
                .wake();
            }
            return S_OK;
        }
        if event.is_null() {
            drop(waiters);
            return self.block_until(value);
        }
        waiters.push(Waiter {
            value,
            event: event as usize,
        });
        S_OK
    }

    fn block_until(&self, value: u64) -> HRESULT {
        #[cfg(windows)]
        if crate::host::LateRun::on() {
            crate::host::LateRun::settle_for_fence();
            if self.reached(value) {
                return S_OK;
            }
        }
        tracing::warn!(value, "blocking fence wait not implemented");
        E_NOTIMPL
    }

    pub(crate) extern "system" fn create(
        _: This,
        initial: u64,
        flags: u32,
        iid: *const Iid,
        out: *mut This,
    ) -> HRESULT {
        tracing::debug!(initial, flags, "fence");
        Self::new(initial, flags).hand_out(iid, out)
    }

    pub(crate) extern "system" fn get_completed_value(this: This) -> u64 {
        Self::completed(this)
    }

    extern "system" fn set_event_on_completion(
        this: This,
        value: u64,
        event: *mut c_void,
    ) -> HRESULT {
        Self::from_this(this).notify(value, event)
    }

    pub(crate) extern "system" fn signal(this: This, value: u64) -> HRESULT {
        Self::advance(this, value);
        #[cfg(windows)]
        crate::queue::Scheduler::release_held(false);
        S_OK
    }

    extern "system" fn get_creation_flags(this: This) -> u32 {
        Self::from_this(this).flags
    }

    extern "system" fn gpu_address(this: This) -> u64 {
        (&raw const Self::from_this(this).value) as u64
    }
}

static FUNCTIONS: Table<24> = {
    let mut f = object::pageable_table::<Fence, FENCE, 24>();
    f[8] = Fence::get_completed_value as *const ();
    f[9] = Fence::set_event_on_completion as *const ();
    f[10] = Fence::signal as *const ();
    f[11] = Fence::get_creation_flags as *const ();
    f[12] = Fence::gpu_address as *const ();
    Table(f)
};

#[cfg(test)]
mod tests {
    use super::*;
    use crate::object::slot;

    #[cfg(windows)]
    #[test]
    fn signals_set_waiting_events() {
        use windows_sys::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
        use windows_sys::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

        let mut f = std::ptr::null();
        assert_eq!(
            Fence::create(std::ptr::null(), 1, 0, &FENCE_IIDS[0], &mut f),
            S_OK
        );
        assert_eq!(slot::<extern "system" fn(This) -> u64>(f, 0x40)(f), 1);
        let event = unsafe { CreateEventW(std::ptr::null(), 0, 0, std::ptr::null()) };
        assert!(!event.is_null());
        let wait = slot::<extern "system" fn(This, u64, *mut c_void) -> HRESULT>(f, 0x48);
        let signal = slot::<extern "system" fn(This, u64) -> HRESULT>(f, 0x50);
        assert_eq!(wait(f, 5, event), S_OK);
        assert_ne!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(signal(f, 5), S_OK);
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(wait(f, 5, event), S_OK, "already reached");
        assert_eq!(unsafe { WaitForSingleObject(event, 0) }, WAIT_OBJECT_0);
        assert_eq!(wait(f, 9, std::ptr::null_mut()), E_NOTIMPL);
        let at = slot::<extern "system" fn(This) -> u64>(f, 0x60)(f);
        assert_eq!(unsafe { *(at as *const u64) }, 5);
        unsafe { CloseHandle(event) };
        assert_eq!(slot::<extern "system" fn(This) -> u32>(f, 0x10)(f), 0);
    }
}
