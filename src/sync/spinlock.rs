// Copyright 2015 The coio Developers.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! A simple Spinlock

use std::cell::UnsafeCell;
use std::fmt;
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

#[inline(always)]
fn cpu_relax() {
    if cfg!(any(target_arch = "x86", target_arch = "x86_64")) {
        unsafe {
            // "Modern" processors exiting a tight loop (like this one)
            // usually detect a _possible_ memory order violation.
            // The PAUSE instruction hints that this is a busy-waiting
            // loop and that no such violation will occur.
            // Furthermore it might also relax the loop and
            // efficiently "pause" the processor for a bit,
            // which reduces power consumption for some CPUs.
            asm!("pause" ::: "memory" : "volatile");
        }
    } else {
        unsafe {
            asm!("" ::: "memory" : "volatile");
        }
    }
}

// 1<<4 iterations (16) equals about 40ns on a i7 3770
const BACKOFF_BASE: usize = 1 << 4;
const BACKOFF_CEILING: usize = 1 << 12;

/// A simple, unfair spinlock.
///
/// This type of lock can grant one thread access more often than others,
/// but will be *at least* twice as fast as a Mutex and generally be fairer than one.
pub struct Spinlock<T: ?Sized> {
    lock: AtomicBool,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for Spinlock<T> {}
unsafe impl<T: ?Sized + Send> Sync for Spinlock<T> {}

impl<T> Spinlock<T> {
    pub fn new(data: T) -> Spinlock<T> {
        Spinlock {
            lock: AtomicBool::new(false),
            data: UnsafeCell::new(data),
        }
    }
}

impl<T: ?Sized> Spinlock<T> {
    pub fn try_lock(&self) -> Option<SpinlockGuard<T>> {
        const SUCCESS: Ordering = Ordering::Acquire;
        const FAILURE: Ordering = Ordering::Relaxed;

        match self.lock.compare_exchange_weak(false, true, SUCCESS, FAILURE) {
            Ok(_) => Some(SpinlockGuard(&self.lock, unsafe { &mut *self.data.get() })),
            Err(_) => None,
        }
    }

    pub fn lock(&self) -> SpinlockGuard<T> {
        const SUCCESS: Ordering = Ordering::Acquire;
        const FAILURE: Ordering = Ordering::Relaxed;

        let mut backoff = BACKOFF_BASE;

        // TODO: Use WFE and SEV instructions for ARM
        while self.lock.compare_exchange_weak(false, true, SUCCESS, FAILURE) != Ok(false) {
            // NOTE:
            //   Spinning here using `while self.lock.load(Relaxed) == true {}`
            //   is commonly done and would buy us about 10% more performance.
            //   But this would come with the cost of extreme unfairness under contention.
            for _ in 0..backoff {
                cpu_relax();
            }

            // exponential backoff
            backoff <<= (backoff != BACKOFF_CEILING) as usize;
        }

        SpinlockGuard(&self.lock, unsafe { &mut *self.data.get() })
    }
}

impl<T: ?Sized + Default> Default for Spinlock<T> {
    fn default() -> Spinlock<T> {
        Spinlock::new(Default::default())
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for Spinlock<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.try_lock() {
            Some(guard) => write!(f, "Spinlock {{ data: {:?} }}", &*guard),
            None => write!(f, "Spinlock {{ <locked> }}"),
        }
    }
}

pub struct SpinlockGuard<'a, T: ?Sized + 'a>(&'a AtomicBool, &'a mut T);

impl<'a, T: ?Sized> !Send for SpinlockGuard<'a, T> {}

impl<'a, T: ?Sized> Drop for SpinlockGuard<'a, T> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl<'a, T: ?Sized> Deref for SpinlockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.1
    }
}

impl<'a, T: ?Sized> DerefMut for SpinlockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.1
    }
}

/// This "fair" spinlock variant using the ticket lock algorithm.
///
/// This lock has a similiar performance to `std::sync::Mutex`, and thus gets slower about 5x
/// faster than `Spinlock`, but guarantees fairness which a `Mutex` surprisingly does not.
// TODO:
//   The CHL or MCS lock would theoretically be much faster the more cores a system has,
//   but initial tests showed a slow down instead.
pub struct TicketSpinlock<T: ?Sized> {
    tick: AtomicUsize,
    tock: AtomicUsize,
    data: UnsafeCell<T>,
}

unsafe impl<T: ?Sized + Send> Send for TicketSpinlock<T> {}
unsafe impl<T: ?Sized + Send> Sync for TicketSpinlock<T> {}

impl<T> TicketSpinlock<T> {
    pub fn new(data: T) -> TicketSpinlock<T> {
        TicketSpinlock {
            tick: AtomicUsize::new(0),
            tock: AtomicUsize::new(0),
            data: UnsafeCell::new(data),
        }
    }
}

impl<T: ?Sized> TicketSpinlock<T> {
    pub fn lock(&self) -> TicketSpinlockGuard<T> {
        let ticket = self.tick.fetch_add(1, Ordering::Relaxed);

        loop {
            let cur = self.tock.load(Ordering::Acquire);

            if cur == ticket {
                break;
            }

            // proportional backoff
            for _ in 0..((ticket - cur) << 2) {
                cpu_relax();
            }
        }

        TicketSpinlockGuard(&self.tock,
                            ticket.wrapping_add(1),
                            unsafe { &mut *self.data.get() })
    }
}

impl<T: ?Sized + Default> Default for TicketSpinlock<T> {
    fn default() -> TicketSpinlock<T> {
        TicketSpinlock::new(Default::default())
    }
}

impl<T: ?Sized + fmt::Debug> fmt::Debug for TicketSpinlock<T> {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "TicketSpinlock {{ <locked> }}")
    }
}

pub struct TicketSpinlockGuard<'a, T: ?Sized + 'a>(&'a AtomicUsize, usize, &'a mut T);

impl<'a, T: ?Sized> !Send for TicketSpinlockGuard<'a, T> {}

impl<'a, T: ?Sized> Drop for TicketSpinlockGuard<'a, T> {
    fn drop(&mut self) {
        self.0.store(self.1, Ordering::Release);
    }
}

impl<'a, T: ?Sized> Deref for TicketSpinlockGuard<'a, T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.2
    }
}

impl<'a, T: ?Sized> DerefMut for TicketSpinlockGuard<'a, T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.2
    }
}
