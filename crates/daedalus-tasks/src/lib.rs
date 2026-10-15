//! Bounded CPU work admission with cooperative cancellation and owned results.

use std::any::Any;
use std::cell::Cell;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Condvar, Mutex, TryLockError};
use std::time::Duration;

use rayon::{ThreadPool, ThreadPoolBuilder};
use thiserror::Error;

static NEXT_POOL_ID: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CURRENT_POOL_ID: Cell<Option<u64>> = const { Cell::new(None) };
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct TaskId(pub u64);

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskFailure {
    pub message: String,
}

impl TaskFailure {
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum TaskPoolError {
    #[error("worker count must be positive")]
    NoWorkers,
    #[error("admission capacity must be positive")]
    NoCapacity,
    #[error("failed to create the Rayon worker pool: {0}")]
    WorkerPool(String),
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SubmitError {
    #[error("task admission capacity is full")]
    Full,
    #[error("task pool is shutting down")]
    Closed,
    #[error("task admission is busy; retry without waiting")]
    Busy,
    #[error("task identifier space is exhausted")]
    IdExhausted,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ReceiveError {
    #[error("another consumer is currently polling task results")]
    Busy,
    #[error("task result state was poisoned")]
    Poisoned,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum ShutdownError {
    #[error("a task cannot wait for shutdown of its own pool")]
    CalledFromWorker,
}

#[derive(Clone, Debug)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    fn cancel(&self) -> bool {
        !self.0.swap(true, Ordering::AcqRel)
    }
}

pub struct TaskHandle<T> {
    id: TaskId,
    cancellation: CancellationToken,
    marker: PhantomData<fn() -> T>,
}

impl<T> Clone for TaskHandle<T> {
    fn clone(&self) -> Self {
        Self {
            id: self.id,
            cancellation: self.cancellation.clone(),
            marker: PhantomData,
        }
    }
}

impl<T> std::fmt::Debug for TaskHandle<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("TaskHandle")
            .field("id", &self.id)
            .field("cancelled", &self.cancellation.is_cancelled())
            .finish()
    }
}

impl<T> TaskHandle<T> {
    #[must_use]
    pub const fn id(&self) -> TaskId {
        self.id
    }

    /// Request cooperative cancellation. Running code must check its token.
    pub fn cancel(&self) -> bool {
        self.cancellation.cancel()
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }
}

pub enum TaskOutcome<T> {
    Completed(T),
    Cancelled,
    Failed(TaskFailure),
    Panicked(String),
}

impl<T: std::fmt::Debug> std::fmt::Debug for TaskOutcome<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Completed(value) => formatter.debug_tuple("Completed").field(value).finish(),
            Self::Cancelled => formatter.write_str("Cancelled"),
            Self::Failed(error) => formatter.debug_tuple("Failed").field(error).finish(),
            Self::Panicked(message) => formatter.debug_tuple("Panicked").field(message).finish(),
        }
    }
}

pub struct TaskCompletion {
    pub id: TaskId,
    outcome: TaskOutcome<Box<dyn Any + Send>>,
}

impl std::fmt::Debug for TaskCompletion {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = match &self.outcome {
            TaskOutcome::Completed(_) => "Completed",
            TaskOutcome::Cancelled => "Cancelled",
            TaskOutcome::Failed(_) => "Failed",
            TaskOutcome::Panicked(_) => "Panicked",
        };
        formatter
            .debug_struct("TaskCompletion")
            .field("id", &self.id)
            .field("outcome", &name)
            .finish()
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
#[error("task result type does not match the requested type")]
pub struct ResultTypeMismatch;

impl TaskCompletion {
    /// Recover the statically typed result associated with a submitted handle.
    pub fn into_outcome<T: Send + 'static>(self) -> Result<TaskOutcome<T>, ResultTypeMismatch> {
        match self.outcome {
            TaskOutcome::Completed(value) => value
                .downcast::<T>()
                .map(|value| TaskOutcome::Completed(*value))
                .map_err(|_| ResultTypeMismatch),
            TaskOutcome::Cancelled => Ok(TaskOutcome::Cancelled),
            TaskOutcome::Failed(error) => Ok(TaskOutcome::Failed(error)),
            TaskOutcome::Panicked(message) => Ok(TaskOutcome::Panicked(message)),
        }
    }
}

#[derive(Debug)]
struct PoolState {
    closed: bool,
    outstanding: usize,
    active: usize,
    cancellation: Vec<CancellationToken>,
}

struct Inner {
    id: u64,
    pool: ThreadPool,
    sender: Sender<TaskCompletion>,
    receiver: Mutex<Receiver<TaskCompletion>>,
    capacity: usize,
    state: Mutex<PoolState>,
    finished: Condvar,
    next_id: AtomicU64,
}

/// Rayon-backed CPU executor. Admission credits bound queued, running, and uncollected work.
#[derive(Clone)]
pub struct TaskPool {
    inner: Arc<Inner>,
}

impl TaskPool {
    pub fn new(workers: usize, capacity: usize) -> Result<Self, TaskPoolError> {
        if workers == 0 {
            return Err(TaskPoolError::NoWorkers);
        }
        if capacity == 0 {
            return Err(TaskPoolError::NoCapacity);
        }
        let pool = ThreadPoolBuilder::new()
            .num_threads(workers)
            .thread_name(|index| format!("daedalus-cpu-{index}"))
            .build()
            .map_err(|error| TaskPoolError::WorkerPool(error.to_string()))?;
        let (sender, receiver) = mpsc::channel();
        Ok(Self {
            inner: Arc::new(Inner {
                id: NEXT_POOL_ID.fetch_add(1, Ordering::Relaxed),
                pool,
                sender,
                receiver: Mutex::new(receiver),
                capacity,
                state: Mutex::new(PoolState {
                    closed: false,
                    outstanding: 0,
                    active: 0,
                    cancellation: Vec::new(),
                }),
                finished: Condvar::new(),
                next_id: AtomicU64::new(1),
            }),
        })
    }

    /// Attempt admission without waiting for capacity or another submitter.
    pub fn try_submit<T, F>(&self, job: F) -> Result<TaskHandle<T>, SubmitError>
    where
        T: Send + 'static,
        F: FnOnce(CancellationToken) -> Result<T, TaskFailure> + Send + 'static,
    {
        let mut state = match self.inner.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(SubmitError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(SubmitError::Closed),
        };
        if state.closed {
            return Err(SubmitError::Closed);
        }
        if state.outstanding >= self.inner.capacity {
            return Err(SubmitError::Full);
        }
        let id = self
            .inner
            .next_id
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |id| id.checked_add(1))
            .map_err(|_| SubmitError::IdExhausted)?;
        let cancellation = CancellationToken(Arc::new(AtomicBool::new(false)));
        state.outstanding += 1;
        state.active += 1;
        state.cancellation.push(cancellation.clone());
        drop(state);

        let sender = self.inner.sender.clone();
        let inner = Arc::clone(&self.inner);
        let pool_id = self.inner.id;
        let worker_token = cancellation.clone();
        self.inner.pool.spawn(move || {
            let previous_pool = CURRENT_POOL_ID.with(|current| current.replace(Some(pool_id)));
            let outcome = if worker_token.is_cancelled() {
                TaskOutcome::Cancelled
            } else {
                match catch_unwind(AssertUnwindSafe(|| job(worker_token.clone()))) {
                    Ok(Ok(_value)) if worker_token.is_cancelled() => TaskOutcome::Cancelled,
                    Ok(Ok(value)) => TaskOutcome::Completed(Box::new(value) as Box<dyn Any + Send>),
                    Ok(Err(error)) => TaskOutcome::Failed(error),
                    Err(payload) => TaskOutcome::Panicked(panic_message(payload)),
                }
            };
            CURRENT_POOL_ID.with(|current| current.set(previous_pool));
            let _ = sender.send(TaskCompletion {
                id: TaskId(id),
                outcome,
            });
            let mut state = inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.active = state.active.saturating_sub(1);
            state
                .cancellation
                .retain(|token| !Arc::ptr_eq(&token.0, &worker_token.0));
            drop(state);
            inner.finished.notify_all();
        });
        Ok(TaskHandle {
            id: TaskId(id),
            cancellation,
            marker: PhantomData,
        })
    }

    /// Poll a completed task result without waiting for worker completion.
    pub fn try_recv(&self) -> Result<Option<TaskCompletion>, ReceiveError> {
        let mut state = match self.inner.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(ReceiveError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(ReceiveError::Poisoned),
        };
        let receiver = match self.inner.receiver.try_lock() {
            Ok(receiver) => receiver,
            Err(TryLockError::WouldBlock) => return Err(ReceiveError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(ReceiveError::Poisoned),
        };
        let completion = match receiver.try_recv() {
            Ok(completion) => completion,
            Err(TryRecvError::Empty | TryRecvError::Disconnected) => return Ok(None),
        };
        state.outstanding = state.outstanding.saturating_sub(1);
        Ok(Some(completion))
    }

    /// Stop accepting tasks, request cancellation for queued/running work, and wait for workers.
    ///
    /// This waits for accepted jobs to return. A running job that ignores its cancellation token
    /// can therefore delay shutdown indefinitely.
    pub fn shutdown(&self) -> Result<(), ShutdownError> {
        if self.is_current_worker() {
            return Err(ShutdownError::CalledFromWorker);
        }
        let tokens = {
            let mut state = self
                .inner
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            state.closed = true;
            state.cancellation.clone()
        };
        for token in tokens {
            token.cancel();
        }
        let mut state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while state.active > 0 {
            state = self
                .inner
                .finished
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        Ok(())
    }

    #[must_use]
    pub fn outstanding(&self) -> usize {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .outstanding
    }

    #[must_use]
    pub fn capacity(&self) -> usize {
        self.inner.capacity
    }

    #[must_use]
    pub fn is_shutdown(&self) -> bool {
        self.inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .closed
    }

    /// Wait for all active work with a timeout, mainly useful to coordinate controlled shutdown.
    pub fn wait_idle(&self, timeout: Duration) -> bool {
        if self.is_current_worker() {
            return false;
        }
        let state = self
            .inner
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (state, _) = self
            .inner
            .finished
            .wait_timeout_while(state, timeout, |state| state.active > 0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active == 0
    }

    fn is_current_worker(&self) -> bool {
        CURRENT_POOL_ID.with(|current| current.get() == Some(self.inner.id))
    }
}

fn panic_message(payload: Box<dyn Any + Send>) -> String {
    if let Some(message) = payload.downcast_ref::<String>() {
        message.clone()
    } else if let Some(message) = payload.downcast_ref::<&'static str>() {
        (*message).to_owned()
    } else {
        "task panicked with a non-string payload".to_owned()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    use super::{ReceiveError, ShutdownError, SubmitError, TaskFailure, TaskOutcome, TaskPool};

    fn receive_one<T: Send + 'static>(pool: &TaskPool) -> TaskOutcome<T> {
        assert!(
            pool.wait_idle(Duration::from_secs(3)),
            "task did not finish"
        );
        let completion = pool.try_recv().unwrap().expect("one completed task");
        completion.into_outcome().unwrap()
    }

    #[test]
    fn admission_credit_covers_queued_running_and_uncollected_results() {
        let pool = TaskPool::new(1, 1).unwrap();
        let (started_send, started_receive) = mpsc::channel();
        let (release_send, release_receive) = mpsc::channel();
        let handle = pool
            .try_submit(move |_| {
                started_send.send(()).unwrap();
                release_receive.recv().unwrap();
                Ok::<_, TaskFailure>(17_u32)
            })
            .unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        assert!(matches!(
            pool.try_submit(|_| Ok::<_, TaskFailure>(99_u32)),
            Err(SubmitError::Full)
        ));
        release_send.send(()).unwrap();
        assert_eq!(receive_one::<u32>(&pool).unwrap_completed(), 17);
        assert!(pool.try_submit(|_| Ok::<_, TaskFailure>(23_u32)).is_ok());
        assert_eq!(receive_one::<u32>(&pool).unwrap_completed(), 23);
        assert_eq!(pool.outstanding(), 0);
        assert!(!handle.is_cancelled());
        pool.shutdown().unwrap();
    }

    #[test]
    fn cancellation_failure_and_panic_are_delivered_as_distinct_results() {
        let pool = TaskPool::new(3, 3).unwrap();
        let (started_send, started_receive) = mpsc::channel();
        let cancelled = pool
            .try_submit(move |token| {
                started_send.send(()).unwrap();
                while !token.is_cancelled() {
                    thread::yield_now();
                }
                Ok::<_, TaskFailure>(0_u8)
            })
            .unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        let failed = pool
            .try_submit(|_| Err::<u8, _>(TaskFailure::new("input could not be decoded")))
            .unwrap();
        let panicked = pool
            .try_submit(|_| -> Result<u8, TaskFailure> { panic!("worker failure") })
            .unwrap();
        assert!(cancelled.cancel());
        assert!(pool.wait_idle(Duration::from_secs(3)));

        let mut outcomes = std::collections::HashMap::new();
        while let Some(completion) = pool.try_recv().unwrap() {
            match completion.id {
                id if id == cancelled.id() => {
                    outcomes.insert(id, completion.into_outcome::<u8>().unwrap());
                }
                id if id == failed.id() => {
                    outcomes.insert(id, completion.into_outcome::<u8>().unwrap());
                }
                id if id == panicked.id() => {
                    outcomes.insert(id, completion.into_outcome::<u8>().unwrap());
                }
                id => panic!("unexpected task id: {id:?}"),
            }
        }
        assert!(matches!(
            outcomes.remove(&cancelled.id()),
            Some(TaskOutcome::Cancelled)
        ));
        assert!(matches!(
            outcomes.remove(&failed.id()),
            Some(TaskOutcome::Failed(error)) if error.message == "input could not be decoded"
        ));
        assert!(matches!(
            outcomes.remove(&panicked.id()),
            Some(TaskOutcome::Panicked(message)) if message == "worker failure"
        ));
        pool.shutdown().unwrap();
    }

    #[test]
    fn shutdown_cancels_cooperative_work_and_rejects_new_admission() {
        let pool = TaskPool::new(1, 2).unwrap();
        let (started_send, started_receive) = mpsc::channel();
        let task = pool
            .try_submit(move |token| {
                started_send.send(()).unwrap();
                while !token.is_cancelled() {
                    thread::yield_now();
                }
                Ok::<_, TaskFailure>(())
            })
            .unwrap();
        started_receive
            .recv_timeout(Duration::from_secs(2))
            .unwrap();
        pool.shutdown().unwrap();
        assert!(task.is_cancelled());
        assert!(pool.is_shutdown());
        assert!(matches!(
            pool.try_submit(|_| Ok::<_, TaskFailure>(())),
            Err(SubmitError::Closed)
        ));
        assert!(matches!(receive_one::<()>(&pool), TaskOutcome::Cancelled));
    }

    #[test]
    fn polling_returns_busy_before_consuming_a_result_when_state_is_contended() {
        let pool = TaskPool::new(1, 1).unwrap();
        pool.try_submit(|_| Ok::<_, TaskFailure>(5_u32)).unwrap();
        assert!(pool.wait_idle(Duration::from_secs(3)));
        let state = pool.inner.state.lock().unwrap();
        assert_eq!(pool.try_recv().err(), Some(ReceiveError::Busy));
        drop(state);
        assert_eq!(
            pool.try_recv()
                .unwrap()
                .unwrap()
                .into_outcome::<u32>()
                .unwrap()
                .unwrap_completed(),
            5
        );
        pool.shutdown().unwrap();
    }

    #[test]
    fn pool_shutdown_and_idle_wait_cannot_join_their_current_worker() {
        let pool = TaskPool::new(1, 1).unwrap();
        let worker_pool = pool.clone();
        let handle = pool
            .try_submit(move |_| {
                assert_eq!(worker_pool.shutdown(), Err(ShutdownError::CalledFromWorker));
                assert!(!worker_pool.wait_idle(Duration::from_secs(1)));
                Ok::<_, TaskFailure>(1_u32)
            })
            .unwrap();
        assert_eq!(receive_one::<u32>(&pool).unwrap_completed(), 1);
        assert!(!handle.is_cancelled());
        pool.shutdown().unwrap();
    }

    trait UnwrapCompleted<T> {
        fn unwrap_completed(self) -> T;
    }

    impl<T: std::fmt::Debug> UnwrapCompleted<T> for TaskOutcome<T> {
        fn unwrap_completed(self) -> T {
            match self {
                TaskOutcome::Completed(value) => value,
                outcome => panic!("expected task completion, got {outcome:?}"),
            }
        }
    }
}
