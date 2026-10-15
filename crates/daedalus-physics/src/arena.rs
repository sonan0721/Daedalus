use std::fmt;

use daedalus_core::{Revision, SessionId};

/// Opaque body identity scoped to one physics world and one slot generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PhysicsBodyHandle {
    session: SessionId,
    index: u32,
    generation: u32,
}

impl PhysicsBodyHandle {
    pub(crate) const fn new(session: SessionId, index: u32, generation: u32) -> Self {
        Self {
            session,
            index,
            generation,
        }
    }

    pub(crate) const fn index(self) -> usize {
        self.index as usize
    }

    pub(crate) const fn generation(self) -> u32 {
        self.generation
    }

    pub(crate) const fn session(self) -> SessionId {
        self.session
    }
}

/// Failure returned by a physics adapter.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PhysicsError {
    /// A world setting or body input was not finite or outside supported bounds.
    InvalidInput(&'static str),
    /// A body handle belongs to another world, was removed, or has been superseded.
    StaleBodyHandle,
    /// The configured body limit or representable handle space has been reached.
    BodyCapacityReached,
    /// An allocation request failed before changing world state.
    AllocationFailed,
    /// The world revision counter cannot advance without wrapping.
    RevisionOverflow,
    /// The native world may have partially advanced and has been quarantined from further use.
    WorldPoisoned,
    /// A native or Rust backend rejected the operation.
    Backend(String),
}

impl fmt::Display for PhysicsError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(reason) => write!(f, "invalid physics input: {reason}"),
            Self::StaleBodyHandle => {
                f.write_str("physics body handle is stale or belongs to another world")
            }
            Self::BodyCapacityReached => f.write_str("physics world body capacity reached"),
            Self::AllocationFailed => {
                f.write_str("physics world could not reserve bounded storage")
            }
            Self::RevisionOverflow => f.write_str("physics world revision overflowed"),
            Self::WorldPoisoned => {
                f.write_str("physics world is poisoned after an incomplete backend step")
            }
            Self::Backend(message) => write!(f, "physics backend error: {message}"),
        }
    }
}

impl std::error::Error for PhysicsError {}

pub(crate) struct HandleArena<T> {
    session: SessionId,
    slots: Vec<Slot<T>>,
    free: Vec<u32>,
    live_count: usize,
}

struct Slot<T> {
    generation: u32,
    value: Option<T>,
    retired: bool,
}

impl<T> HandleArena<T> {
    pub(crate) fn new(session: SessionId) -> Self {
        Self {
            session,
            slots: Vec::new(),
            free: Vec::new(),
            live_count: 0,
        }
    }

    pub(crate) fn len(&self) -> usize {
        self.live_count
    }

    /// Reserve enough room for a following insert without changing handle state.
    pub(crate) fn prepare_insert(&mut self) -> Result<(), PhysicsError> {
        if self.free.is_empty() {
            if self.slots.len() >= u32::MAX as usize {
                return Err(PhysicsError::BodyCapacityReached);
            }
            self.slots
                .try_reserve(1)
                .map_err(|_| PhysicsError::AllocationFailed)?;
        }
        Ok(())
    }

    pub(crate) fn insert_prepared(&mut self, value: T) -> PhysicsBodyHandle {
        let index = if let Some(index) = self.free.pop() {
            index
        } else {
            let index = self.slots.len() as u32;
            self.slots.push(Slot {
                generation: 0,
                value: None,
                retired: false,
            });
            index
        };
        let slot = &mut self.slots[index as usize];
        debug_assert!(slot.value.is_none() && !slot.retired);
        slot.value = Some(value);
        self.live_count += 1;
        PhysicsBodyHandle::new(self.session, index, slot.generation)
    }

    pub(crate) fn get(&self, handle: PhysicsBodyHandle) -> Result<&T, PhysicsError> {
        self.checked_slot(handle)?
            .value
            .as_ref()
            .ok_or(PhysicsError::StaleBodyHandle)
    }

    pub(crate) fn prepare_remove(&mut self, handle: PhysicsBodyHandle) -> Result<(), PhysicsError> {
        let slot = self.checked_slot(handle)?;
        if slot.value.is_none() {
            return Err(PhysicsError::StaleBodyHandle);
        }
        if slot.generation != u32::MAX {
            self.free
                .try_reserve(1)
                .map_err(|_| PhysicsError::AllocationFailed)?;
        }
        Ok(())
    }

    pub(crate) fn remove_prepared(&mut self, handle: PhysicsBodyHandle) -> Result<T, PhysicsError> {
        let (value, reusable) = {
            let slot = self.checked_slot_mut(handle)?;
            let value = slot.value.take().ok_or(PhysicsError::StaleBodyHandle)?;
            let reusable = if let Some(generation) = slot.generation.checked_add(1) {
                slot.generation = generation;
                true
            } else {
                slot.retired = true;
                false
            };
            (value, reusable)
        };
        self.live_count -= 1;
        if reusable {
            self.free.push(handle.index);
        }
        Ok(value)
    }

    fn checked_slot(&self, handle: PhysicsBodyHandle) -> Result<&Slot<T>, PhysicsError> {
        if handle.session() != self.session {
            return Err(PhysicsError::StaleBodyHandle);
        }
        let slot = self
            .slots
            .get(handle.index())
            .ok_or(PhysicsError::StaleBodyHandle)?;
        if slot.generation != handle.generation() || slot.retired {
            return Err(PhysicsError::StaleBodyHandle);
        }
        Ok(slot)
    }

    fn checked_slot_mut(
        &mut self,
        handle: PhysicsBodyHandle,
    ) -> Result<&mut Slot<T>, PhysicsError> {
        if handle.session() != self.session {
            return Err(PhysicsError::StaleBodyHandle);
        }
        let slot = self
            .slots
            .get_mut(handle.index())
            .ok_or(PhysicsError::StaleBodyHandle)?;
        if slot.generation != handle.generation() || slot.retired {
            return Err(PhysicsError::StaleBodyHandle);
        }
        Ok(slot)
    }
}

pub(crate) fn next_revision(current: Revision) -> Result<Revision, PhysicsError> {
    current
        .0
        .checked_add(1)
        .map(Revision)
        .ok_or(PhysicsError::RevisionOverflow)
}

#[cfg(test)]
mod tests {
    use daedalus_core::{Revision, SessionId};

    use super::{HandleArena, PhysicsError, next_revision};

    #[test]
    fn removed_and_cross_session_handles_are_rejected() {
        let mut a = HandleArena::<&str>::new(SessionId::fresh());
        let b = HandleArena::<&str>::new(SessionId::fresh());
        a.prepare_insert().unwrap();
        let first = a.insert_prepared("body");
        assert_eq!(b.get(first), Err(PhysicsError::StaleBodyHandle));
        a.prepare_remove(first).unwrap();
        assert_eq!(a.remove_prepared(first).unwrap(), "body");
        assert_eq!(a.get(first), Err(PhysicsError::StaleBodyHandle));
        a.prepare_insert().unwrap();
        let second = a.insert_prepared("replacement");
        assert_eq!(first.index, second.index);
        assert_ne!(first.generation, second.generation);
        assert_eq!(a.get(first), Err(PhysicsError::StaleBodyHandle));
    }

    #[test]
    fn revision_increment_is_checked() {
        assert_eq!(
            next_revision(Revision(u64::MAX)),
            Err(PhysicsError::RevisionOverflow)
        );
    }
}
