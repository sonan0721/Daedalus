use std::sync::{Arc, Mutex, TryLockError, Weak};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{EntityHandle, Tick, Transform};

/// One immutable simulation entity prepared for presentation-side extraction.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderEntitySnapshot {
    pub entity: EntityHandle,
    pub transform: Transform,
}

/// An owned immutable view of the simulation state needed by presentation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RenderSnapshot {
    pub sequence: u64,
    pub tick: Tick,
    pub entities: Arc<[RenderEntitySnapshot]>,
}

#[derive(Debug)]
struct SnapshotState {
    next_sequence: u64,
    max_retained: usize,
    latest: Option<Arc<RenderSnapshot>>,
    retained: Vec<Weak<RenderSnapshot>>,
}

#[derive(Clone, Debug)]
pub struct SnapshotPublisher {
    state: Arc<Mutex<SnapshotState>>,
}

#[derive(Clone, Debug)]
pub struct SnapshotConsumer {
    state: Arc<Mutex<SnapshotState>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PublicationStatus {
    Published { sequence: u64 },
    SkippedAtCapacity,
    SkippedAtEntityLimit { entity_count: usize, limit: usize },
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SnapshotPublishError {
    #[error("snapshot publication was skipped because the mailbox is busy")]
    Busy,
    #[error("snapshot mailbox lock was poisoned")]
    Poisoned,
    #[error("snapshot sequence space is exhausted")]
    SequenceExhausted,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SnapshotReadError {
    #[error("snapshot mailbox is busy")]
    Busy,
    #[error("snapshot mailbox lock was poisoned")]
    Poisoned,
}

/// Create a one-slot latest-value mailbox with a cap on simultaneously retained snapshots.
///
/// The consumer can keep snapshots alive after a later publication. When those retained
/// snapshots consume the cap, publication returns `SkippedAtCapacity` rather than waiting.
#[must_use]
pub fn snapshot_channel(max_retained: usize) -> (SnapshotPublisher, SnapshotConsumer) {
    assert!(
        max_retained > 0,
        "snapshot retention capacity must be positive"
    );
    let state = Arc::new(Mutex::new(SnapshotState {
        next_sequence: 1,
        max_retained,
        latest: None,
        retained: Vec::new(),
    }));
    (
        SnapshotPublisher {
            state: Arc::clone(&state),
        },
        SnapshotConsumer { state },
    )
}

impl SnapshotPublisher {
    /// Publish owned snapshot data without waiting for a slow consumer.
    pub fn try_publish(
        &self,
        tick: Tick,
        entities: Vec<RenderEntitySnapshot>,
    ) -> Result<PublicationStatus, SnapshotPublishError> {
        let entity_count = entities.len();
        self.try_publish_with(tick, entity_count, usize::MAX, || entities)
    }

    /// Publish using bounded, lazy entity extraction. The extractor is called only after
    /// the entity limit and retained-snapshot capacity checks pass. Extraction runs while
    /// the mailbox mutex is held; readers remain nonblocking and report `Busy` until the
    /// short, entity-bounded copy completes.
    pub fn try_publish_with<F>(
        &self,
        tick: Tick,
        entity_count: usize,
        entity_limit: usize,
        extract: F,
    ) -> Result<PublicationStatus, SnapshotPublishError>
    where
        F: FnOnce() -> Vec<RenderEntitySnapshot>,
    {
        if entity_count > entity_limit {
            return Ok(PublicationStatus::SkippedAtEntityLimit {
                entity_count,
                limit: entity_limit,
            });
        }
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => return Err(SnapshotPublishError::Busy),
            Err(TryLockError::Poisoned(_)) => return Err(SnapshotPublishError::Poisoned),
        };

        state
            .retained
            .retain(|snapshot| snapshot.strong_count() > 0);
        let externally_retained = state
            .retained
            .iter()
            .filter_map(Weak::upgrade)
            .filter(|snapshot| {
                let is_current = state
                    .latest
                    .as_ref()
                    .is_some_and(|latest| Arc::ptr_eq(latest, snapshot));
                let mailbox_and_upgrade = if is_current { 2 } else { 1 };
                Arc::strong_count(snapshot) > mailbox_and_upgrade
            })
            .count();
        if externally_retained.saturating_add(1) > state.max_retained {
            return Ok(PublicationStatus::SkippedAtCapacity);
        }

        let entities = extract();
        if entities.len() > entity_limit {
            return Ok(PublicationStatus::SkippedAtEntityLimit {
                entity_count: entities.len(),
                limit: entity_limit,
            });
        }
        let sequence = state.next_sequence;
        state.next_sequence = sequence
            .checked_add(1)
            .ok_or(SnapshotPublishError::SequenceExhausted)?;
        let snapshot = Arc::new(RenderSnapshot {
            sequence,
            tick,
            entities: entities.into(),
        });
        state.retained.push(Arc::downgrade(&snapshot));
        state.latest = Some(snapshot);
        Ok(PublicationStatus::Published { sequence })
    }
}

impl SnapshotConsumer {
    /// Read the most recent complete snapshot without waiting for the publisher.
    pub fn try_latest(&self) -> Result<Option<Arc<RenderSnapshot>>, SnapshotReadError> {
        match self.state.try_lock() {
            Ok(state) => Ok(state.latest.clone()),
            Err(TryLockError::WouldBlock) => Err(SnapshotReadError::Busy),
            Err(TryLockError::Poisoned(_)) => Err(SnapshotReadError::Poisoned),
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{RenderEntitySnapshot, SnapshotPublishError, SnapshotReadError, Tick, Transform};

    use super::{PublicationStatus, snapshot_channel};

    #[test]
    fn retained_snapshot_exhausts_capacity_without_blocking_publication() {
        let (publisher, consumer) = snapshot_channel(1);
        assert_eq!(
            publisher.try_publish(Tick(1), vec![]).unwrap(),
            PublicationStatus::Published { sequence: 1 }
        );
        let retained = consumer.try_latest().unwrap().unwrap();
        assert_eq!(
            publisher.try_publish(Tick(2), vec![]).unwrap(),
            PublicationStatus::SkippedAtCapacity
        );
        drop(retained);
        assert_eq!(
            publisher.try_publish(Tick(2), vec![]).unwrap(),
            PublicationStatus::Published { sequence: 2 }
        );
    }

    #[test]
    fn capacity_one_can_advance_when_the_consumer_did_not_retain_old_data() {
        let (publisher, consumer) = snapshot_channel(1);
        for tick in 1..=5 {
            assert!(matches!(
                publisher.try_publish(Tick(tick), vec![]).unwrap(),
                PublicationStatus::Published { .. }
            ));
            assert_eq!(consumer.try_latest().unwrap().unwrap().tick, Tick(tick));
        }
    }

    #[test]
    fn consumer_gets_an_owned_immutable_snapshot() {
        let (publisher, consumer) = snapshot_channel(2);
        let entity = RenderEntitySnapshot {
            entity: crate::EntityHandle {
                session: crate::SessionId::from_raw(4),
                index: 3,
                generation: 2,
            },
            transform: Transform::IDENTITY,
        };
        publisher.try_publish(Tick(7), vec![entity]).unwrap();
        let snapshot = consumer.try_latest().unwrap().unwrap();
        publisher.try_publish(Tick(8), vec![]).unwrap();
        assert_eq!(snapshot.tick, Tick(7));
        assert_eq!(snapshot.entities.as_ref(), &[entity]);
        assert_eq!(consumer.try_latest().unwrap().unwrap().tick, Tick(8));
    }

    #[test]
    fn mutex_contention_is_reported_without_waiting() {
        let (publisher, consumer) = snapshot_channel(2);
        let _guard = publisher.state.lock().unwrap();
        assert_eq!(
            publisher.try_publish(Tick(1), vec![]),
            Err(SnapshotPublishError::Busy)
        );
        assert_eq!(consumer.try_latest(), Err(SnapshotReadError::Busy));
    }

    #[test]
    fn lazy_extraction_is_skipped_for_entity_budget_and_retention_capacity() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (publisher, consumer) = snapshot_channel(1);
        publisher.try_publish(Tick(1), vec![]).unwrap();
        let retained = consumer.try_latest().unwrap().unwrap();
        let calls = AtomicUsize::new(0);

        assert_eq!(
            publisher
                .try_publish_with(Tick(2), 0, 1, || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    vec![]
                })
                .unwrap(),
            PublicationStatus::SkippedAtCapacity
        );
        assert_eq!(
            publisher
                .try_publish_with(Tick(3), 2, 1, || {
                    calls.fetch_add(1, Ordering::Relaxed);
                    vec![]
                })
                .unwrap(),
            PublicationStatus::SkippedAtEntityLimit {
                entity_count: 2,
                limit: 1,
            }
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(consumer.try_latest().unwrap().unwrap().tick, Tick(1));
        drop(retained);
    }

    #[test]
    fn lazy_extraction_is_skipped_when_mailbox_is_busy() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let (publisher, consumer) = snapshot_channel(2);
        let calls = AtomicUsize::new(0);
        let _guard = publisher.state.lock().unwrap();
        assert_eq!(
            publisher.try_publish_with(Tick(1), 0, 1, || {
                calls.fetch_add(1, Ordering::Relaxed);
                vec![]
            }),
            Err(SnapshotPublishError::Busy)
        );
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        assert_eq!(consumer.try_latest(), Err(SnapshotReadError::Busy));
    }
}
