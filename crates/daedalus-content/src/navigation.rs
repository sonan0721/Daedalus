//! Bounded, cancellable occupancy-grid path queries.
//!
//! These queries cover 4-connected tile grids and 6-connected voxel grids. They are not a general
//! 3D navmesh, jump graph, crowd-avoidance, or agent-steering implementation.

use std::collections::VecDeque;
use std::fmt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use serde::{Deserialize, Serialize};

use crate::Revision;
use crate::tile::TileCoord;

const MAX_NAV_CELLS: usize = 4 * 1024 * 1024;
const UNVISITED: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoxelCoord {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TileGrid {
    origin: TileCoord,
    width: u16,
    height: u16,
    revision: Revision,
    blocked: Vec<bool>,
}

impl TileGrid {
    pub fn new(
        origin: TileCoord,
        width: u16,
        height: u16,
        revision: Revision,
        blocked: Vec<bool>,
    ) -> Result<Self, NavigationError> {
        let count = checked_cell_count(&[width, height])?;
        if blocked.len() != count {
            return Err(NavigationError::CellCountMismatch);
        }
        if origin.x.checked_add(i32::from(width) - 1).is_none()
            || origin.y.checked_add(i32::from(height) - 1).is_none()
        {
            return Err(NavigationError::CoordinateOverflow);
        }
        Ok(Self {
            origin,
            width,
            height,
            revision,
            blocked,
        })
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn is_blocked(&self, position: TileCoord) -> Option<bool> {
        self.validate().ok()?;
        self.index_of(&position).map(|index| self.blocked[index])
    }

    pub fn set_blocked(
        &mut self,
        position: TileCoord,
        blocked: bool,
    ) -> Result<(), NavigationError> {
        self.validate()?;
        let index = self
            .index_of(&position)
            .ok_or(NavigationError::OutOfBounds)?;
        if self.blocked[index] != blocked {
            self.revision = self
                .revision
                .checked_add(1)
                .ok_or(NavigationError::RevisionOverflow)?;
            self.blocked[index] = blocked;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoxelGrid {
    origin: VoxelCoord,
    width: u16,
    height: u16,
    depth: u16,
    revision: Revision,
    blocked: Vec<bool>,
}

impl VoxelGrid {
    pub fn new(
        origin: VoxelCoord,
        width: u16,
        height: u16,
        depth: u16,
        revision: Revision,
        blocked: Vec<bool>,
    ) -> Result<Self, NavigationError> {
        let count = checked_cell_count(&[width, height, depth])?;
        if blocked.len() != count {
            return Err(NavigationError::CellCountMismatch);
        }
        if origin.x.checked_add(i32::from(width) - 1).is_none()
            || origin.y.checked_add(i32::from(height) - 1).is_none()
            || origin.z.checked_add(i32::from(depth) - 1).is_none()
        {
            return Err(NavigationError::CoordinateOverflow);
        }
        Ok(Self {
            origin,
            width,
            height,
            depth,
            revision,
            blocked,
        })
    }

    pub fn revision(&self) -> Revision {
        self.revision
    }

    pub fn is_blocked(&self, position: VoxelCoord) -> Option<bool> {
        self.validate().ok()?;
        self.index_of(&position).map(|index| self.blocked[index])
    }

    pub fn set_blocked(
        &mut self,
        position: VoxelCoord,
        blocked: bool,
    ) -> Result<(), NavigationError> {
        self.validate()?;
        let index = self
            .index_of(&position)
            .ok_or(NavigationError::OutOfBounds)?;
        if self.blocked[index] != blocked {
            self.revision = self
                .revision
                .checked_add(1)
                .ok_or(NavigationError::RevisionOverflow)?;
            self.blocked[index] = blocked;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum NavigationError {
    InvalidDimensions,
    GridTooLarge,
    CellCountMismatch,
    CoordinateOverflow,
    OutOfBounds,
    RevisionOverflow,
}

impl fmt::Display for NavigationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions => f.write_str("navigation grid dimensions must be positive"),
            Self::GridTooLarge => f.write_str("navigation grid exceeds the configured cell limit"),
            Self::CellCountMismatch => {
                f.write_str("blocked cell count does not match grid dimensions")
            }
            Self::CoordinateOverflow => f.write_str("navigation grid coordinates overflow i32"),
            Self::OutOfBounds => f.write_str("navigation coordinate is outside the grid"),
            Self::RevisionOverflow => f.write_str("navigation grid revision overflow"),
        }
    }
}

impl std::error::Error for NavigationError {}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PathRequest<P> {
    pub start: P,
    pub goal: P,
    pub expected_revision: Option<Revision>,
    /// Maximum nodes expanded; this is the deterministic work bound and timeout result source.
    pub max_expansions: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct PathResult<P> {
    pub path: Vec<P>,
    pub source_revision: Revision,
    pub expanded_nodes: u32,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum PathSearchResult<P> {
    Found {
        path: Vec<P>,
        source_revision: Revision,
        expanded_nodes: u32,
    },
    NoPath {
        source_revision: Revision,
        expanded_nodes: u32,
    },
    BudgetExceeded {
        source_revision: Revision,
        expanded_nodes: u32,
    },
    Cancelled {
        source_revision: Revision,
        expanded_nodes: u32,
    },
    StaleRevision {
        expected: Revision,
        actual: Revision,
    },
    InvalidEndpoint {
        source_revision: Revision,
    },
    InvalidGrid {
        source_revision: Revision,
    },
}

impl<P> PathSearchResult<P> {
    pub fn source_revision(&self) -> Option<Revision> {
        match self {
            Self::Found {
                source_revision, ..
            }
            | Self::NoPath {
                source_revision, ..
            }
            | Self::BudgetExceeded {
                source_revision, ..
            }
            | Self::Cancelled {
                source_revision, ..
            }
            | Self::InvalidEndpoint { source_revision }
            | Self::InvalidGrid { source_revision } => Some(*source_revision),
            Self::StaleRevision { .. } => None,
        }
    }

    /// Use this after asynchronous work completes to reject a result from an older world revision.
    pub fn is_stale_at(&self, current_revision: Revision) -> bool {
        self.source_revision()
            .is_some_and(|source_revision| source_revision != current_revision)
    }
}

/// A clonable flag that may be set from another thread while a bounded search is running.
#[derive(Clone, Debug, Default)]
pub struct CancellationToken(Arc<AtomicBool>);

impl CancellationToken {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

pub trait OccupancyGrid {
    type Position: Clone + Eq;

    fn revision(&self) -> Revision;
    fn validate(&self) -> Result<(), NavigationError>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
    fn index_of(&self, position: &Self::Position) -> Option<usize>;
    fn position_of(&self, index: usize) -> Self::Position;
    fn is_blocked_index(&self, index: usize) -> bool;
    fn for_each_neighbor(&self, index: usize, callback: impl FnMut(usize));
}

impl OccupancyGrid for TileGrid {
    type Position = TileCoord;

    fn revision(&self) -> Revision {
        self.revision
    }

    fn validate(&self) -> Result<(), NavigationError> {
        let expected = checked_cell_count(&[self.width, self.height])?;
        if self.blocked.len() != expected {
            return Err(NavigationError::CellCountMismatch);
        }
        if self
            .origin
            .x
            .checked_add(i32::from(self.width) - 1)
            .is_none()
            || self
                .origin
                .y
                .checked_add(i32::from(self.height) - 1)
                .is_none()
        {
            return Err(NavigationError::CoordinateOverflow);
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.blocked.len()
    }

    fn index_of(&self, position: &TileCoord) -> Option<usize> {
        let x = position.x.checked_sub(self.origin.x)?;
        let y = position.y.checked_sub(self.origin.y)?;
        if x < 0 || y < 0 || x >= i32::from(self.width) || y >= i32::from(self.height) {
            return None;
        }
        Some(x as usize + usize::from(self.width) * y as usize)
    }

    fn position_of(&self, index: usize) -> TileCoord {
        let x = index % usize::from(self.width);
        let y = index / usize::from(self.width);
        TileCoord {
            x: self.origin.x + x as i32,
            y: self.origin.y + y as i32,
        }
    }

    fn is_blocked_index(&self, index: usize) -> bool {
        self.blocked[index]
    }

    fn for_each_neighbor(&self, index: usize, mut callback: impl FnMut(usize)) {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        let x = index % width;
        let y = index / width;
        if x > 0 {
            callback(index - 1);
        }
        if x + 1 < width {
            callback(index + 1);
        }
        if y > 0 {
            callback(index - width);
        }
        if y + 1 < height {
            callback(index + width);
        }
    }
}

impl OccupancyGrid for VoxelGrid {
    type Position = VoxelCoord;

    fn revision(&self) -> Revision {
        self.revision
    }

    fn validate(&self) -> Result<(), NavigationError> {
        let expected = checked_cell_count(&[self.width, self.height, self.depth])?;
        if self.blocked.len() != expected {
            return Err(NavigationError::CellCountMismatch);
        }
        if self
            .origin
            .x
            .checked_add(i32::from(self.width) - 1)
            .is_none()
            || self
                .origin
                .y
                .checked_add(i32::from(self.height) - 1)
                .is_none()
            || self
                .origin
                .z
                .checked_add(i32::from(self.depth) - 1)
                .is_none()
        {
            return Err(NavigationError::CoordinateOverflow);
        }
        Ok(())
    }

    fn len(&self) -> usize {
        self.blocked.len()
    }

    fn index_of(&self, position: &VoxelCoord) -> Option<usize> {
        let x = position.x.checked_sub(self.origin.x)?;
        let y = position.y.checked_sub(self.origin.y)?;
        let z = position.z.checked_sub(self.origin.z)?;
        if x < 0
            || y < 0
            || z < 0
            || x >= i32::from(self.width)
            || y >= i32::from(self.height)
            || z >= i32::from(self.depth)
        {
            return None;
        }
        Some(
            x as usize
                + usize::from(self.width) * (y as usize + usize::from(self.height) * z as usize),
        )
    }

    fn position_of(&self, index: usize) -> VoxelCoord {
        let width = usize::from(self.width);
        let plane = width * usize::from(self.height);
        let z = index / plane;
        let remainder = index % plane;
        let y = remainder / width;
        let x = remainder % width;
        VoxelCoord {
            x: self.origin.x + x as i32,
            y: self.origin.y + y as i32,
            z: self.origin.z + z as i32,
        }
    }

    fn is_blocked_index(&self, index: usize) -> bool {
        self.blocked[index]
    }

    fn for_each_neighbor(&self, index: usize, mut callback: impl FnMut(usize)) {
        let width = usize::from(self.width);
        let height = usize::from(self.height);
        let depth = usize::from(self.depth);
        let plane = width * height;
        let z = index / plane;
        let remainder = index % plane;
        let y = remainder / width;
        let x = remainder % width;
        if x > 0 {
            callback(index - 1);
        }
        if x + 1 < width {
            callback(index + 1);
        }
        if y > 0 {
            callback(index - width);
        }
        if y + 1 < height {
            callback(index + width);
        }
        if z > 0 {
            callback(index - plane);
        }
        if z + 1 < depth {
            callback(index + plane);
        }
    }
}

/// Finds a shortest path on an immutable occupancy snapshot using deterministic breadth-first search.
pub fn find_path<G>(
    grid: &G,
    request: PathRequest<G::Position>,
    cancellation: &CancellationToken,
) -> PathSearchResult<G::Position>
where
    G: OccupancyGrid,
{
    let source_revision = grid.revision();
    if grid.validate().is_err() {
        return PathSearchResult::InvalidGrid { source_revision };
    }
    if let Some(expected) = request.expected_revision
        && expected != source_revision
    {
        return PathSearchResult::StaleRevision {
            expected,
            actual: source_revision,
        };
    }
    let Some(start) = grid.index_of(&request.start) else {
        return PathSearchResult::InvalidEndpoint { source_revision };
    };
    let Some(goal) = grid.index_of(&request.goal) else {
        return PathSearchResult::InvalidEndpoint { source_revision };
    };
    if cancellation.is_cancelled() {
        return PathSearchResult::Cancelled {
            source_revision,
            expanded_nodes: 0,
        };
    }
    if grid.is_blocked_index(start) || grid.is_blocked_index(goal) {
        return PathSearchResult::NoPath {
            source_revision,
            expanded_nodes: 0,
        };
    }
    if start == goal {
        return PathSearchResult::Found {
            path: vec![request.start],
            source_revision,
            expanded_nodes: 0,
        };
    }

    let mut parents = vec![UNVISITED; grid.len()];
    parents[start] = start as u32;
    let mut queue = VecDeque::new();
    queue.push_back(start as u32);
    let mut expanded_nodes = 0_u32;
    while let Some(current) = queue.pop_front() {
        if cancellation.is_cancelled() {
            return PathSearchResult::Cancelled {
                source_revision,
                expanded_nodes,
            };
        }
        if expanded_nodes >= request.max_expansions {
            return PathSearchResult::BudgetExceeded {
                source_revision,
                expanded_nodes,
            };
        }
        let current = current as usize;
        expanded_nodes += 1;
        if current == goal {
            let mut indices = vec![goal];
            let mut cursor = goal;
            while cursor != start {
                cursor = parents[cursor] as usize;
                indices.push(cursor);
            }
            indices.reverse();
            let path = indices
                .into_iter()
                .map(|index| grid.position_of(index))
                .collect();
            return PathSearchResult::Found {
                path,
                source_revision,
                expanded_nodes,
            };
        }
        let mut cancelled_during_neighbor_scan = false;
        grid.for_each_neighbor(current, |neighbor| {
            if cancellation.is_cancelled() {
                cancelled_during_neighbor_scan = true;
            } else if parents[neighbor] == UNVISITED && !grid.is_blocked_index(neighbor) {
                parents[neighbor] = current as u32;
                queue.push_back(neighbor as u32);
            }
        });
        if cancelled_during_neighbor_scan {
            return PathSearchResult::Cancelled {
                source_revision,
                expanded_nodes,
            };
        }
    }
    PathSearchResult::NoPath {
        source_revision,
        expanded_nodes,
    }
}

fn checked_cell_count(dimensions: &[u16]) -> Result<usize, NavigationError> {
    if dimensions.contains(&0) {
        return Err(NavigationError::InvalidDimensions);
    }
    let count = dimensions.iter().try_fold(1_usize, |product, dimension| {
        product.checked_mul(usize::from(*dimension))
    });
    let count = count.ok_or(NavigationError::GridTooLarge)?;
    if count > MAX_NAV_CELLS || count > u32::MAX as usize {
        return Err(NavigationError::GridTooLarge);
    }
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tile_grid(width: u16, height: u16, blocked: Vec<bool>) -> TileGrid {
        TileGrid::new(TileCoord { x: -3, y: 5 }, width, height, 4, blocked).unwrap()
    }

    fn request(start: TileCoord, goal: TileCoord, max_expansions: u32) -> PathRequest<TileCoord> {
        PathRequest {
            start,
            goal,
            expected_revision: Some(4),
            max_expansions,
        }
    }

    #[test]
    fn blocked_path_reports_no_path_and_search_budget_is_bounded() {
        let grid = tile_grid(
            3,
            3,
            vec![false, true, false, false, true, false, false, true, false],
        );
        let result = find_path(
            &grid,
            request(TileCoord { x: -3, y: 5 }, TileCoord { x: -1, y: 5 }, 50),
            &CancellationToken::new(),
        );
        assert!(matches!(result, PathSearchResult::NoPath { .. }));

        let open = tile_grid(10, 10, vec![false; 100]);
        let result = find_path(
            &open,
            request(TileCoord { x: -3, y: 5 }, TileCoord { x: 6, y: 14 }, 1),
            &CancellationToken::new(),
        );
        assert!(matches!(result, PathSearchResult::BudgetExceeded { .. }));
    }

    #[test]
    fn cancellation_stale_revision_and_post_search_staleness_are_reported() {
        let mut grid = tile_grid(3, 1, vec![false; 3]);
        let token = CancellationToken::new();
        token.cancel();
        assert!(matches!(
            find_path(
                &grid,
                request(TileCoord { x: -3, y: 5 }, TileCoord { x: -1, y: 5 }, 10),
                &token
            ),
            PathSearchResult::Cancelled { .. }
        ));

        let stale = find_path(
            &grid,
            PathRequest {
                expected_revision: Some(3),
                ..request(TileCoord { x: -3, y: 5 }, TileCoord { x: -1, y: 5 }, 10)
            },
            &CancellationToken::new(),
        );
        assert!(matches!(
            stale,
            PathSearchResult::StaleRevision {
                expected: 3,
                actual: 4
            }
        ));

        let result = find_path(
            &grid,
            request(TileCoord { x: -3, y: 5 }, TileCoord { x: -1, y: 5 }, 10),
            &CancellationToken::new(),
        );
        assert!(matches!(result, PathSearchResult::Found { .. }));
        assert!(result.is_stale_at(5));
        grid.set_blocked(TileCoord { x: -2, y: 5 }, true).unwrap();
        assert_eq!(grid.revision(), 5);
    }

    #[test]
    fn voxel_grid_uses_face_adjacent_six_connected_steps() {
        let grid =
            VoxelGrid::new(VoxelCoord { x: 0, y: 0, z: 0 }, 2, 2, 2, 9, vec![false; 8]).unwrap();
        let result = find_path(
            &grid,
            PathRequest {
                start: VoxelCoord { x: 0, y: 0, z: 0 },
                goal: VoxelCoord { x: 1, y: 1, z: 1 },
                expected_revision: Some(9),
                max_expansions: 8,
            },
            &CancellationToken::new(),
        );
        let PathSearchResult::Found { path, .. } = result else {
            panic!("expected voxel grid path");
        };
        assert_eq!(path.len(), 4);
    }
}
