//! Small bounded primitives shared by project control and the legacy live fixture.

use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, TryLockError, Weak};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct CreditLimits {
    pub tickets: usize,
    pub bytes: usize,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct CreditUsage {
    pub tickets: usize,
    pub bytes: usize,
}

/// A bounded count-and-byte budget. A reservation lives until its final lease is dropped.
pub(crate) struct CreditPool {
    inner: Arc<CreditPoolInner>,
}

struct CreditPoolInner {
    limits: CreditLimits,
    tickets: AtomicUsize,
    bytes: AtomicUsize,
}

#[derive(Clone)]
pub(crate) struct CreditLease {
    _reservation: Arc<CreditReservation>,
}

struct CreditReservation {
    pool: Arc<CreditPoolInner>,
    bytes: usize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum CreditError {
    TicketCapacity,
    ByteCapacity,
    InvalidReservation,
}

impl CreditPool {
    pub fn new(limits: CreditLimits) -> Self {
        Self {
            inner: Arc::new(CreditPoolInner {
                limits,
                tickets: AtomicUsize::new(0),
                bytes: AtomicUsize::new(0),
            }),
        }
    }

    pub fn try_reserve(&self, bytes: usize) -> Result<CreditLease, CreditError> {
        if self.inner.limits.tickets == 0 || bytes == 0 {
            return Err(CreditError::InvalidReservation);
        }
        if bytes > self.inner.limits.bytes {
            return Err(CreditError::ByteCapacity);
        }
        if !try_reserve_atomic(&self.inner.tickets, self.inner.limits.tickets, 1) {
            return Err(CreditError::TicketCapacity);
        }
        if !try_reserve_atomic(&self.inner.bytes, self.inner.limits.bytes, bytes) {
            self.inner.tickets.fetch_sub(1, Ordering::Release);
            return Err(CreditError::ByteCapacity);
        }

        Ok(CreditLease {
            _reservation: Arc::new(CreditReservation {
                pool: Arc::clone(&self.inner),
                bytes,
            }),
        })
    }

    pub fn usage(&self) -> CreditUsage {
        // Count and byte snapshots are intentionally best-effort: reservations use separate
        // atomics so public admission and lease release never wait on a contended credit lock.
        CreditUsage {
            tickets: self.inner.tickets.load(Ordering::Acquire),
            bytes: self.inner.bytes.load(Ordering::Acquire),
        }
    }
}

fn try_reserve_atomic(counter: &AtomicUsize, limit: usize, amount: usize) -> bool {
    let mut current = counter.load(Ordering::Acquire);
    loop {
        let Some(next) = current.checked_add(amount) else {
            return false;
        };
        if next > limit {
            return false;
        }
        match counter.compare_exchange_weak(current, next, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => return true,
            Err(observed) => current = observed,
        }
    }
}

impl Drop for CreditReservation {
    fn drop(&mut self) {
        self.pool.tickets.fetch_sub(1, Ordering::AcqRel);
        self.pool.bytes.fetch_sub(self.bytes, Ordering::AcqRel);
    }
}

#[derive(Default)]
pub(crate) struct StopSignal {
    requested: AtomicBool,
}

impl StopSignal {
    pub fn request(&self) {
        self.requested.store(true, Ordering::Release);
    }

    pub fn is_requested(&self) -> bool {
        self.requested.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum MailboxError {
    Busy,
    Poisoned,
    AtCapacity,
}

struct MailboxState<T> {
    latest: Option<Arc<T>>,
    retained: Vec<RetainedMailboxValue<T>>,
}

struct RetainedMailboxValue<T> {
    value: Weak<T>,
    backing_probe: Option<Box<dyn Fn() -> bool + Send + Sync>>,
}

/// Latest-value mailbox that coalesces frames while bounding consumer-held generations.
pub(crate) struct LatestValueMailbox<T> {
    state: Mutex<MailboxState<T>>,
    max_retained: usize,
}

impl<T> LatestValueMailbox<T> {
    pub fn new(max_retained: usize) -> Self {
        Self {
            state: Mutex::new(MailboxState {
                latest: None,
                retained: Vec::new(),
            }),
            max_retained,
        }
    }

    pub fn try_latest(&self) -> Result<Option<Arc<T>>, MailboxError> {
        match self.state.try_lock() {
            Ok(state) => Ok(state.latest.clone()),
            Err(TryLockError::WouldBlock) => Err(MailboxError::Busy),
            Err(TryLockError::Poisoned(_)) => Err(MailboxError::Poisoned),
        }
    }

    pub fn try_publish(&self, value: T) -> Result<(), MailboxError> {
        self.try_publish_inner(value, None)
    }

    /// Publish with a nonwaiting probe for separately retained data owned by each value.
    /// The probe runs without locks beyond this mailbox's existing try-lock and should use
    /// weak Arc upgrades/strong counts only.
    pub fn try_publish_with_backing_probe<F>(
        &self,
        value: T,
        backing_probe: F,
    ) -> Result<(), MailboxError>
    where
        F: Fn() -> bool + Send + Sync + 'static,
    {
        self.try_publish_inner(value, Some(Box::new(backing_probe)))
    }

    fn try_publish_inner(
        &self,
        value: T,
        backing_probe: Option<Box<dyn Fn() -> bool + Send + Sync>>,
    ) -> Result<(), MailboxError> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(MailboxError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(MailboxError::Poisoned),
        };
        let mut externally_retained = 0_usize;
        let latest_ptr = state.latest.as_ref().map(Arc::as_ptr);
        state.retained.retain(|generation| {
            let value = generation.value.upgrade();
            let value_alive = value.is_some();
            let value_retained = value.as_ref().is_some_and(|value| {
                let is_latest =
                    latest_ptr.is_some_and(|latest| std::ptr::eq(latest, Arc::as_ptr(value)));
                let mailbox_refs = if is_latest { 2 } else { 1 };
                Arc::strong_count(value) > mailbox_refs
            });
            let backing_retained = generation
                .backing_probe
                .as_ref()
                .is_some_and(|probe| probe());
            if value_retained || backing_retained {
                externally_retained += 1;
            }
            value_alive || backing_retained
        });
        if externally_retained.saturating_add(1) > self.max_retained {
            return Err(MailboxError::AtCapacity);
        }
        let value = Arc::new(value);
        state.retained.push(RetainedMailboxValue {
            value: Arc::downgrade(&value),
            backing_probe,
        });
        state.latest = Some(value);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::thread;

    use super::{CreditLimits, CreditPool, LatestValueMailbox, MailboxError};

    #[test]
    fn frame_mailbox_counts_client_held_generations_and_retains_last_complete_value() {
        let mailbox = LatestValueMailbox::new(2);
        mailbox.try_publish(1_u32).unwrap();
        let first = mailbox.try_latest().unwrap().unwrap();
        mailbox.try_publish(2).unwrap();
        let second = mailbox.try_latest().unwrap().unwrap();

        assert_eq!(mailbox.try_publish(3), Err(MailboxError::AtCapacity));
        assert_eq!(*mailbox.try_latest().unwrap().unwrap(), 2);

        drop(first);
        mailbox.try_publish(3).unwrap();
        assert_eq!(*mailbox.try_latest().unwrap().unwrap(), 3);
        assert_eq!(*second, 2);
    }

    #[test]
    fn result_credits_are_bounded_thread_safe_and_released_by_last_lease() {
        let pool = Arc::new(CreditPool::new(CreditLimits {
            tickets: 4,
            bytes: 16,
        }));
        let mut workers = Vec::new();
        for _ in 0..8 {
            let pool = Arc::clone(&pool);
            workers.push(thread::spawn(move || {
                for _ in 0..100 {
                    if let Ok(lease) = pool.try_reserve(4) {
                        let _shared = lease.clone();
                        let usage = pool.usage();
                        assert!(usage.tickets <= 4);
                        assert!(usage.bytes <= 16);
                    }
                }
            }));
        }
        for worker in workers {
            worker.join().unwrap();
        }
        assert_eq!(pool.usage().tickets, 0);
        assert_eq!(pool.usage().bytes, 0);

        let lease = pool.try_reserve(8).unwrap();
        let last_reader = lease.clone();
        drop(lease);
        assert_eq!(pool.usage().tickets, 1);
        drop(last_reader);
        assert_eq!(pool.usage().tickets, 0);
    }
}
