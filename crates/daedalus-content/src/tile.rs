//! Bounded 2D tile fluid and full-recompute light reference algorithms.

use std::collections::{BTreeMap, VecDeque};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Revision;

const MAX_REGION_CELLS: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_LIGHT_WORK_ITEMS: u64 = 16 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TileCoord {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fluid {
    pub kind: Option<u16>,
    pub amount: u8,
}

impl Fluid {
    pub const EMPTY: Self = Self {
        kind: None,
        amount: 0,
    };
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TileCell {
    pub solid: bool,
    /// 0 lets light pass with base attenuation; 255 completely blocks propagation.
    pub opacity: u8,
    /// Scalar emitted light in the 0..=255 range.
    pub emission: u8,
    pub light: u8,
    pub fluid: Fluid,
}

impl Default for TileCell {
    fn default() -> Self {
        Self {
            solid: false,
            opacity: 0,
            emission: 0,
            light: 0,
            fluid: Fluid::EMPTY,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TileRegion {
    pub origin: TileCoord,
    pub width: u16,
    pub height: u16,
    pub revision: Revision,
    pub light_revision: Revision,
    /// Row-major with increasing y: `x + width * y`.
    pub cells: Vec<TileCell>,
}

impl TileRegion {
    pub fn new(
        origin: TileCoord,
        width: u16,
        height: u16,
        revision: Revision,
        cells: Vec<TileCell>,
    ) -> Result<Self, TileError> {
        let count = region_cell_count(width, height)?;
        if cells.len() != count {
            return Err(TileError::CellCountMismatch);
        }
        if origin.x.checked_add(i32::from(width) - 1).is_none()
            || origin.y.checked_add(i32::from(height) - 1).is_none()
        {
            return Err(TileError::CoordinateOverflow);
        }
        for cell in &cells {
            validate_cell(*cell)?;
        }
        Ok(Self {
            origin,
            width,
            height,
            revision,
            light_revision: 0,
            cells,
        })
    }

    pub fn cell(&self, x: u16, y: u16) -> Option<&TileCell> {
        if x >= self.width || y >= self.height {
            return None;
        }
        self.cells.get(tile_index(self.width, x, y))
    }

    /// Replaces a cell and advances the source revision when the value changes.
    pub fn set_cell(&mut self, x: u16, y: u16, cell: TileCell) -> Result<(), TileError> {
        validate_region(self)?;
        validate_cell(cell)?;
        if x >= self.width || y >= self.height {
            return Err(TileError::OutOfBounds);
        }
        let index = tile_index(self.width, x, y);
        if self.cells[index] != cell {
            let next_revision = self
                .revision
                .checked_add(1)
                .ok_or(TileError::RevisionOverflow)?;
            self.cells[index] = cell;
            self.revision = next_revision;
        }
        Ok(())
    }

    pub fn total_fluid_by_kind(&self) -> Result<BTreeMap<u16, u64>, TileError> {
        validate_region(self)?;
        let mut totals = BTreeMap::new();
        for cell in &self.cells {
            if let Some(kind) = cell.fluid.kind {
                *totals.entry(kind).or_insert(0) += u64::from(cell.fluid.amount);
            }
        }
        Ok(totals)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FluidStepConfig {
    /// Maximum fluid amount in every non-solid cell.
    pub capacity: u8,
    /// Maximum amount transferred down by one cell during this step.
    pub downward_rate: u8,
    /// Maximum amount transferred across one horizontal edge during this step.
    pub lateral_rate: u8,
}

impl Default for FluidStepConfig {
    fn default() -> Self {
        Self {
            capacity: u8::MAX,
            downward_rate: u8::MAX,
            lateral_rate: 8,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TileError {
    InvalidDimensions,
    RegionTooLarge,
    CellCountMismatch,
    CoordinateOverflow,
    OutOfBounds,
    InvalidFluidCell,
    FluidExceedsCapacity,
    ZeroCapacity,
    LightWorkBudgetExceeded { limit: u64 },
    RevisionOverflow,
}

impl fmt::Display for TileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions => f.write_str("tile region dimensions must be positive"),
            Self::RegionTooLarge => f.write_str("tile region exceeds the configured cell limit"),
            Self::CellCountMismatch => f.write_str("tile cell count does not match dimensions"),
            Self::CoordinateOverflow => f.write_str("tile region coordinates overflow i32"),
            Self::OutOfBounds => f.write_str("tile coordinate is outside the region"),
            Self::InvalidFluidCell => f.write_str("fluid kind and amount are inconsistent"),
            Self::FluidExceedsCapacity => f.write_str("fluid amount exceeds cell capacity"),
            Self::ZeroCapacity => f.write_str("fluid capacity must be greater than zero"),
            Self::LightWorkBudgetExceeded { limit } => {
                write!(
                    f,
                    "light recompute exceeded the {limit}-neighbor work budget"
                )
            }
            Self::RevisionOverflow => f.write_str("tile region revision overflow"),
        }
    }
}

impl std::error::Error for TileError {}

/// Advances fluid with a deterministic two-pass pairwise relaxation.
///
/// Region edges are closed: transfers outside the supplied region are not proposed. Each
/// accepted transfer subtracts and adds the same bounded integer amount, preserving total mass.
pub fn step_fluid(region: &TileRegion, config: FluidStepConfig) -> Result<TileRegion, TileError> {
    validate_region(region)?;
    if config.capacity == 0 {
        return Err(TileError::ZeroCapacity);
    }
    let mut next = region.clone();
    for cell in &next.cells {
        if cell.fluid.amount > config.capacity {
            return Err(TileError::FluidExceedsCapacity);
        }
        if cell.solid && cell.fluid.amount > 0 {
            return Err(TileError::InvalidFluidCell);
        }
    }

    // Downward transfer runs bottom-up, so newly received mass cannot fall twice in one step.
    if config.downward_rate > 0 {
        for y in (0..next.height.saturating_sub(1)).rev() {
            for x in 0..next.width {
                let source_index = tile_index(next.width, x, y);
                let destination_index = tile_index(next.width, x, y + 1);
                let source = next.cells[source_index];
                if source.fluid.amount == 0 {
                    continue;
                }
                let destination = next.cells[destination_index];
                let free_capacity = config.capacity.saturating_sub(destination.fluid.amount);
                let compatible = !destination.solid
                    && (destination.fluid.amount == 0
                        || destination.fluid.kind == source.fluid.kind);
                if !compatible || free_capacity == 0 {
                    continue;
                }
                let moved = source
                    .fluid
                    .amount
                    .min(config.downward_rate)
                    .min(free_capacity);
                transfer(&mut next.cells, source_index, destination_index, moved);
            }
        }
    }

    // Horizontal equalization uses a fixed left-to-right edge order and never crosses region edges.
    if config.lateral_rate > 0 && next.width > 1 {
        for y in 0..next.height {
            for x in 0..next.width - 1 {
                let left_index = tile_index(next.width, x, y);
                let right_index = tile_index(next.width, x + 1, y);
                let left = next.cells[left_index];
                let right = next.cells[right_index];
                if left.solid || right.solid || !fluid_compatible(left.fluid, right.fluid) {
                    continue;
                }
                let (source_index, destination_index, source, destination) =
                    if left.fluid.amount > right.fluid.amount {
                        (left_index, right_index, left, right)
                    } else if right.fluid.amount > left.fluid.amount {
                        (right_index, left_index, right, left)
                    } else {
                        continue;
                    };
                let difference = source.fluid.amount - destination.fluid.amount;
                let amount_to_balance = difference / 2;
                let free_capacity = config.capacity.saturating_sub(destination.fluid.amount);
                let moved = amount_to_balance
                    .min(config.lateral_rate)
                    .min(free_capacity);
                transfer(&mut next.cells, source_index, destination_index, moved);
            }
        }
    }

    if next.cells != region.cells {
        next.revision = region
            .revision
            .checked_add(1)
            .ok_or(TileError::RevisionOverflow)?;
    }
    validate_region(&next)?;
    Ok(next)
}

fn transfer(cells: &mut [TileCell], source_index: usize, destination_index: usize, amount: u8) {
    if amount == 0 {
        return;
    }
    let kind = cells[source_index].fluid.kind;
    cells[source_index].fluid.amount -= amount;
    if cells[source_index].fluid.amount == 0 {
        cells[source_index].fluid = Fluid::EMPTY;
    }
    if cells[destination_index].fluid.amount == 0 {
        cells[destination_index].fluid.kind = kind;
    }
    cells[destination_index].fluid.amount += amount;
}

fn fluid_compatible(left: Fluid, right: Fluid) -> bool {
    left.amount == 0 || right.amount == 0 || left.kind == right.kind
}

fn validate_cell(cell: TileCell) -> Result<(), TileError> {
    if cell.solid && cell.fluid.amount > 0 {
        return Err(TileError::InvalidFluidCell);
    }
    match (cell.fluid.kind, cell.fluid.amount) {
        (None, 0) | (Some(_), 1..=u8::MAX) => Ok(()),
        _ => Err(TileError::InvalidFluidCell),
    }
}

fn validate_region(region: &TileRegion) -> Result<(), TileError> {
    let count = region_cell_count(region.width, region.height)?;
    if region.cells.len() != count {
        return Err(TileError::CellCountMismatch);
    }
    if region
        .origin
        .x
        .checked_add(i32::from(region.width) - 1)
        .is_none()
        || region
            .origin
            .y
            .checked_add(i32::from(region.height) - 1)
            .is_none()
    {
        return Err(TileError::CoordinateOverflow);
    }
    for cell in &region.cells {
        validate_cell(*cell)?;
    }
    Ok(())
}

fn region_cell_count(width: u16, height: u16) -> Result<usize, TileError> {
    if width == 0 || height == 0 {
        return Err(TileError::InvalidDimensions);
    }
    let count = usize::from(width)
        .checked_mul(usize::from(height))
        .ok_or(TileError::InvalidDimensions)?;
    if count > MAX_REGION_CELLS {
        return Err(TileError::RegionTooLarge);
    }
    Ok(count)
}

fn tile_index(width: u16, x: u16, y: u16) -> usize {
    usize::from(x) + usize::from(width) * usize::from(y)
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LightField {
    pub source_revision: Revision,
    pub light_revision: Revision,
    pub work_items: u64,
    pub values: Vec<u8>,
}

impl LightField {
    pub fn is_current(&self, region: &TileRegion) -> bool {
        self.source_revision == region.revision && self.light_revision == region.light_revision
    }
}

/// Recomputes light from all emitters, which correctly removes old contributions after edits.
///
/// Propagation is four-connected, loses one unit plus destination opacity per step, and stops
/// before cells with opacity 255. The algorithm is a correctness baseline; it does not claim an
/// incremental frontier or cross-region propagation policy.
pub fn recompute_light(region: &TileRegion) -> Result<(TileRegion, LightField), TileError> {
    recompute_light_with_budget(region, DEFAULT_MAX_LIGHT_WORK_ITEMS)
}

/// Full reference recompute with a hard neighbor-visit budget. On budget exhaustion, no new light
/// field is returned, so the caller can keep the last committed region and report the deferral.
pub fn recompute_light_with_budget(
    region: &TileRegion,
    max_work_items: u64,
) -> Result<(TileRegion, LightField), TileError> {
    validate_region(region)?;
    let mut values = vec![0_u8; region.cells.len()];
    let mut queue = VecDeque::new();
    for (index, cell) in region.cells.iter().enumerate() {
        if cell.emission > 0 {
            values[index] = cell.emission;
            queue.push_back(index);
        }
    }
    let mut work_items = 0_u64;
    while let Some(source_index) = queue.pop_front() {
        let source_light = values[source_index];
        if source_light <= 1 {
            continue;
        }
        let x = (source_index % usize::from(region.width)) as u16;
        let y = (source_index / usize::from(region.width)) as u16;
        let mut neighbors = [None; 4];
        if x > 0 {
            neighbors[0] = Some((x - 1, y));
        }
        if x + 1 < region.width {
            neighbors[1] = Some((x + 1, y));
        }
        if y > 0 {
            neighbors[2] = Some((x, y - 1));
        }
        if y + 1 < region.height {
            neighbors[3] = Some((x, y + 1));
        }
        for (nx, ny) in neighbors.into_iter().flatten() {
            if work_items >= max_work_items {
                return Err(TileError::LightWorkBudgetExceeded {
                    limit: max_work_items,
                });
            }
            work_items += 1;
            let neighbor_index = tile_index(region.width, nx, ny);
            let opacity = region.cells[neighbor_index].opacity;
            if opacity == u8::MAX {
                continue;
            }
            let attenuation = u16::from(opacity) + 1;
            let candidate = u16::from(source_light).saturating_sub(attenuation) as u8;
            if candidate > values[neighbor_index] {
                values[neighbor_index] = candidate;
                queue.push_back(neighbor_index);
            }
        }
    }

    let mut next = region.clone();
    let changed = next
        .cells
        .iter()
        .zip(&values)
        .any(|(cell, light)| cell.light != *light);
    for (cell, light) in next.cells.iter_mut().zip(&values) {
        cell.light = *light;
    }
    if changed {
        next.light_revision = region
            .light_revision
            .checked_add(1)
            .ok_or(TileError::RevisionOverflow)?;
    }
    let field = LightField {
        source_revision: next.revision,
        light_revision: next.light_revision,
        work_items,
        values,
    };
    Ok((next, field))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn region(width: u16, height: u16, cells: Vec<TileCell>) -> TileRegion {
        TileRegion::new(TileCoord { x: -2, y: 9 }, width, height, 7, cells).unwrap()
    }

    #[test]
    fn fluid_step_preserves_each_kind_and_respects_capacity_and_solids() {
        let mut cells = vec![TileCell::default(); 12];
        cells[1] = TileCell {
            fluid: Fluid {
                kind: Some(4),
                amount: 9,
            },
            ..TileCell::default()
        };
        cells[2] = TileCell {
            fluid: Fluid {
                kind: Some(4),
                amount: 3,
            },
            ..TileCell::default()
        };
        cells[5].solid = true;
        let before = region(4, 3, cells);
        let totals = before.total_fluid_by_kind().unwrap();
        let after = step_fluid(
            &before,
            FluidStepConfig {
                capacity: 10,
                downward_rate: 7,
                lateral_rate: 4,
            },
        )
        .unwrap();
        assert_eq!(after.total_fluid_by_kind().unwrap(), totals);
        assert!(after.cells.iter().all(|cell| cell.fluid.amount <= 10));
        assert!(
            after
                .cells
                .iter()
                .filter(|cell| cell.solid)
                .all(|cell| cell.fluid.amount == 0)
        );
        assert!(after.revision > before.revision);
    }

    #[test]
    fn fluid_at_region_boundary_is_retained_without_double_or_external_transfer() {
        let mut cells = vec![TileCell::default(); 4];
        cells[3] = TileCell {
            fluid: Fluid {
                kind: Some(1),
                amount: 200,
            },
            ..TileCell::default()
        };
        let before = region(2, 2, cells);
        let after = step_fluid(
            &before,
            FluidStepConfig {
                capacity: u8::MAX,
                downward_rate: u8::MAX,
                lateral_rate: 0,
            },
        )
        .unwrap();
        assert_eq!(
            before.total_fluid_by_kind().unwrap(),
            after.total_fluid_by_kind().unwrap()
        );
        assert_eq!(after.cells[3].fluid.amount, 200);
    }

    #[test]
    fn light_removal_recomputes_without_a_stale_glow_and_matches_reference() {
        let mut cells = vec![TileCell::default(); 7];
        cells[0].emission = 12;
        cells[3].opacity = 255;
        let mut lit = region(7, 1, cells);
        let (with_light, field) = recompute_light(&lit).unwrap();
        assert_eq!(field.values, reference_light(&lit));
        assert!(with_light.cells[2].light > 0);
        assert_eq!(with_light.cells[4].light, 0);
        assert!(field.is_current(&with_light));

        lit = with_light;
        let mut source_removed = *lit.cell(0, 0).unwrap();
        source_removed.emission = 0;
        lit.set_cell(0, 0, source_removed).unwrap();
        let (dark, dark_field) = recompute_light(&lit).unwrap();
        assert_eq!(dark_field.values, reference_light(&lit));
        assert!(dark.cells.iter().all(|cell| cell.light == 0));
        assert!(!field.is_current(&dark));
    }

    #[test]
    fn light_budget_exhaustion_does_not_publish_partial_values() {
        let mut cells = vec![TileCell::default(); 4];
        cells[0].emission = 10;
        let source = region(2, 2, cells);
        assert_eq!(
            recompute_light_with_budget(&source, 0),
            Err(TileError::LightWorkBudgetExceeded { limit: 0 })
        );
        assert_eq!(source.cells[0].light, 0);
        assert_eq!(source.light_revision, 0);
    }

    fn reference_light(region: &TileRegion) -> Vec<u8> {
        let mut result = vec![0_u8; region.cells.len()];
        let mut frontier: Vec<usize> = region
            .cells
            .iter()
            .enumerate()
            .filter_map(|(index, cell)| {
                if cell.emission > 0 {
                    result[index] = cell.emission;
                    Some(index)
                } else {
                    None
                }
            })
            .collect();
        while let Some(source) = frontier.pop() {
            let x = source % usize::from(region.width);
            let y = source / usize::from(region.width);
            let mut destinations = Vec::with_capacity(4);
            if x > 0 {
                destinations.push(source - 1);
            }
            if x + 1 < usize::from(region.width) {
                destinations.push(source + 1);
            }
            if y > 0 {
                destinations.push(source - usize::from(region.width));
            }
            if y + 1 < usize::from(region.height) {
                destinations.push(source + usize::from(region.width));
            }
            for destination in destinations {
                let opacity = region.cells[destination].opacity;
                if opacity == u8::MAX {
                    continue;
                }
                let next = result[source].saturating_sub(opacity.saturating_add(1));
                if next > result[destination] {
                    result[destination] = next;
                    frontier.push(destination);
                }
            }
        }
        result
    }
}
