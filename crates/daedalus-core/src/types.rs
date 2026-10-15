use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

static NEXT_SESSION_ID: AtomicU64 = AtomicU64::new(1);

/// A session-local simulation tick. Tick zero is the initial state.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Tick(pub u64);

impl Tick {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

/// A session identifier encoded as a decimal JSON string to preserve all 64 bits.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct SessionId(u64);

impl SessionId {
    #[must_use]
    pub fn fresh() -> Self {
        let id = NEXT_SESSION_ID.fetch_add(1, Ordering::Relaxed);
        Self(id)
    }

    #[must_use]
    pub const fn from_raw(value: u64) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }
}

impl Serialize for SessionId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for SessionId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value
            .parse::<u64>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// A de-duplication key for one command issuer, encoded as a JSON string.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct CommandId(u128);

impl CommandId {
    #[must_use]
    pub const fn from_raw(value: u128) -> Self {
        Self(value)
    }

    #[must_use]
    pub const fn get(self) -> u128 {
        self.0
    }
}

impl Serialize for CommandId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for CommandId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        value
            .parse::<u128>()
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

/// A revision of authoritative state used to reject stale work or commands.
#[derive(
    Clone, Copy, Debug, Default, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize,
)]
#[serde(transparent)]
pub struct Revision(pub u64);

impl Revision {
    #[must_use]
    pub const fn get(self) -> u64 {
        self.0
    }

    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.saturating_add(1))
    }
}

/// An entity identifier scoped to a session and an ECS generation.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
pub struct EntityHandle {
    pub session: SessionId,
    pub index: u32,
    pub generation: u32,
}

/// Minimal transform data shared by simulation and presentation consumers.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Transform {
    pub translation: [f32; 3],
    pub rotation: [f32; 4],
    pub scale: [f32; 3],
}

impl Transform {
    pub const IDENTITY: Self = Self {
        translation: [0.0, 0.0, 0.0],
        rotation: [0.0, 0.0, 0.0, 1.0],
        scale: [1.0, 1.0, 1.0],
    };

    #[must_use]
    pub fn is_finite(self) -> bool {
        self.translation.into_iter().all(f32::is_finite)
            && self.rotation.into_iter().all(f32::is_finite)
            && self.scale.into_iter().all(f32::is_finite)
    }
}

impl Default for Transform {
    fn default() -> Self {
        Self::IDENTITY
    }
}

#[cfg(test)]
mod tests {
    use super::{CommandId, SessionId};

    #[test]
    fn large_ids_round_trip_as_json_strings() {
        let session = SessionId::from_raw(u64::MAX);
        let command = CommandId::from_raw(u128::MAX);
        let session_json = serde_json::to_string(&session).expect("serialize session");
        let command_json = serde_json::to_string(&command).expect("serialize command");
        assert_eq!(session_json, format!("\"{}\"", u64::MAX));
        assert_eq!(command_json, format!("\"{}\"", u128::MAX));
        assert_eq!(
            serde_json::from_str::<SessionId>(&session_json).unwrap(),
            session
        );
        assert_eq!(
            serde_json::from_str::<CommandId>(&command_json).unwrap(),
            command
        );
    }
}
