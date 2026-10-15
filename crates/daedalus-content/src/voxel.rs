//! Deterministic voxel chunk generation and revision-tagged greedy surface meshing.

use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::Revision;

pub const VOXEL_GENERATOR_VERSION: u32 = 1;
const MAX_CHUNK_CELLS: usize = 4 * 1024 * 1024;
const DEFAULT_MAX_MESH_QUADS: usize = 1_000_000;

#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkCoord {
    pub x: i32,
    pub y: i32,
    pub z: i32,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkDimensions {
    pub x: u16,
    pub y: u16,
    pub z: u16,
}

impl ChunkDimensions {
    pub fn cell_count(self) -> Result<usize, VoxelError> {
        if self.x == 0 || self.y == 0 || self.z == 0 {
            return Err(VoxelError::InvalidDimensions);
        }
        let count = usize::from(self.x)
            .checked_mul(usize::from(self.y))
            .and_then(|count| count.checked_mul(usize::from(self.z)))
            .ok_or(VoxelError::InvalidDimensions)?;
        if count > MAX_CHUNK_CELLS {
            return Err(VoxelError::ChunkTooLarge);
        }
        Ok(count)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoxelCell {
    pub material: Option<u16>,
}

impl VoxelCell {
    pub const AIR: Self = Self { material: None };

    pub const fn solid(material: u16) -> Self {
        Self {
            material: Some(material),
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoxelChunk {
    pub coord: ChunkCoord,
    pub dimensions: ChunkDimensions,
    pub generator_version: u32,
    pub lifetime_generation: u64,
    pub data_revision: Revision,
    /// X-major, then Y, then Z: `x + dimensions.x * (y + dimensions.y * z)`.
    pub cells: Vec<VoxelCell>,
}

impl VoxelChunk {
    pub fn new(
        coord: ChunkCoord,
        dimensions: ChunkDimensions,
        generator_version: u32,
        lifetime_generation: u64,
        data_revision: Revision,
        cells: Vec<VoxelCell>,
    ) -> Result<Self, VoxelError> {
        if cells.len() != dimensions.cell_count()? {
            return Err(VoxelError::CellCountMismatch);
        }
        Ok(Self {
            coord,
            dimensions,
            generator_version,
            lifetime_generation,
            data_revision,
            cells,
        })
    }

    pub fn cell(&self, x: u16, y: u16, z: u16) -> Option<VoxelCell> {
        if x >= self.dimensions.x || y >= self.dimensions.y || z >= self.dimensions.z {
            return None;
        }
        self.cells
            .get(cell_index(self.dimensions, x, y, z))
            .copied()
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MaterialSurface {
    Opaque,
    Transparent,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VoxelMaterial {
    pub id: u16,
    pub surface: MaterialSurface,
    /// Materials with different merge groups never share a greedy quad.
    pub merge_group: u16,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FaceDirection {
    NegativeX,
    PositiveX,
    NegativeY,
    PositiveY,
    NegativeZ,
    PositiveZ,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TerrainGenerator {
    pub version: u32,
    pub base_height: i32,
    pub height_amplitude: u16,
    /// Horizontal spacing between deterministic value-noise lattice points.
    pub noise_scale: u16,
    pub surface_material: u16,
    pub subsurface_material: u16,
}

impl Default for TerrainGenerator {
    fn default() -> Self {
        Self {
            version: VOXEL_GENERATOR_VERSION,
            base_height: 8,
            height_amplitude: 4,
            noise_scale: 8,
            surface_material: 1,
            subsurface_material: 2,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoxelError {
    InvalidDimensions,
    ChunkTooLarge,
    CellCountMismatch,
    UnsupportedGeneratorVersion(u32),
    InvalidGeneratorConfiguration,
    CoordinateBoundary,
    NeighborCoordinateMismatch,
    NeighborDimensionsMismatch,
    UnknownMaterial(u16),
    InvalidChunkData,
    MeshQuadBudgetExceeded { limit: usize },
}

impl fmt::Display for VoxelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidDimensions => f.write_str("voxel chunk dimensions must be positive"),
            Self::ChunkTooLarge => f.write_str("voxel chunk exceeds the configured cell limit"),
            Self::CellCountMismatch => f.write_str("voxel cell count does not match dimensions"),
            Self::UnsupportedGeneratorVersion(version) => {
                write!(f, "unsupported terrain generator version {version}")
            }
            Self::InvalidGeneratorConfiguration => {
                f.write_str("invalid terrain generator configuration")
            }
            Self::CoordinateBoundary => {
                f.write_str("chunk coordinate has no representable adjacent chunk")
            }
            Self::NeighborCoordinateMismatch => {
                f.write_str("neighbor map key does not match chunk coordinate")
            }
            Self::NeighborDimensionsMismatch => {
                f.write_str("adjacent chunks have different dimensions")
            }
            Self::UnknownMaterial(id) => write!(f, "voxel material {id} has no definition"),
            Self::InvalidChunkData => f.write_str("voxel chunk contains inconsistent data"),
            Self::MeshQuadBudgetExceeded { limit } => {
                write!(f, "greedy mesh exceeded the {limit}-quad output budget")
            }
        }
    }
}

impl std::error::Error for VoxelError {}

/// Produces heightfield terrain from a fixed integer value-noise algorithm.
///
/// The generator is intentionally a small baseline, not a complete world-gen stack. Its output
/// is deterministic for a given seed, generator version, coordinate, dimensions, and settings.
pub fn generate_chunk(
    seed: u64,
    coord: ChunkCoord,
    dimensions: ChunkDimensions,
    generator: TerrainGenerator,
    lifetime_generation: u64,
    data_revision: Revision,
) -> Result<VoxelChunk, VoxelError> {
    let count = dimensions.cell_count()?;
    if generator.version != VOXEL_GENERATOR_VERSION {
        return Err(VoxelError::UnsupportedGeneratorVersion(generator.version));
    }
    if generator.noise_scale == 0 {
        return Err(VoxelError::InvalidGeneratorConfiguration);
    }
    let mut cells = vec![VoxelCell::AIR; count];
    let origin_x = i64::from(coord.x) * i64::from(dimensions.x);
    let origin_y = i64::from(coord.y) * i64::from(dimensions.y);
    let origin_z = i64::from(coord.z) * i64::from(dimensions.z);
    for z in 0..dimensions.z {
        for x in 0..dimensions.x {
            let world_x = origin_x + i64::from(x);
            let world_z = origin_z + i64::from(z);
            let height = i64::from(generator.base_height)
                + terrain_noise(seed, world_x, world_z, generator.noise_scale)
                    * i64::from(generator.height_amplitude)
                    / 127;
            for y in 0..dimensions.y {
                let world_y = origin_y + i64::from(y);
                if world_y <= height {
                    let material = if world_y == height {
                        generator.surface_material
                    } else {
                        generator.subsurface_material
                    };
                    let index = cell_index(dimensions, x, y, z);
                    cells[index] = VoxelCell::solid(material);
                }
            }
        }
    }
    VoxelChunk::new(
        coord,
        dimensions,
        generator.version,
        lifetime_generation,
        data_revision,
        cells,
    )
}

fn terrain_noise(seed: u64, x: i64, z: i64, scale: u16) -> i64 {
    let scale = i64::from(scale);
    let gx = x.div_euclid(scale);
    let gz = z.div_euclid(scale);
    let fx = x.rem_euclid(scale) * 65_536 / scale;
    let fz = z.rem_euclid(scale) * 65_536 / scale;
    let sx = smooth_fixed(fx);
    let sz = smooth_fixed(fz);
    let a = lattice_value(seed, gx, gz);
    let b = lattice_value(seed, gx + 1, gz);
    let c = lattice_value(seed, gx, gz + 1);
    let d = lattice_value(seed, gx + 1, gz + 1);
    let top = lerp_fixed(a, b, sx);
    let bottom = lerp_fixed(c, d, sx);
    lerp_fixed(top, bottom, sz)
}

fn lattice_value(seed: u64, x: i64, z: i64) -> i64 {
    let mut value = seed ^ (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    value ^= (z as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^= value >> 31;
    (value & 0xff) as i64 - 127
}

fn smooth_fixed(t: i64) -> i64 {
    // 3t² - 2t³ in 16.16 fixed point.
    let t2 = t * t / 65_536;
    (t2 * (3 * 65_536 - 2 * t) / 65_536).clamp(0, 65_536)
}

fn lerp_fixed(a: i64, b: i64, t: i64) -> i64 {
    a + (b - a) * t / 65_536
}

fn cell_index(dimensions: ChunkDimensions, x: u16, y: u16, z: u16) -> usize {
    usize::from(x)
        + usize::from(dimensions.x) * (usize::from(y) + usize::from(dimensions.y) * usize::from(z))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChunkStamp {
    pub lifetime_generation: u64,
    pub data_revision: Revision,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshRevisionTag {
    pub coord: ChunkCoord,
    pub source: ChunkStamp,
    /// All six adjacent coordinates are represented; `None` means air was sampled there.
    pub neighbors: Vec<NeighborRevision>,
    pub job_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NeighborRevision {
    pub coord: ChunkCoord,
    pub stamp: Option<ChunkStamp>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MeshQuad {
    /// Lowest local grid coordinate of the quad; plane axis is selected by `face`.
    pub origin: [u16; 3],
    /// Extents along the two axes perpendicular to the face normal.
    pub extent: [u16; 2],
    pub face: FaceDirection,
    pub material_id: u16,
    pub merge_group: u16,
    pub surface: MaterialSurface,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GreedyMesh {
    pub tag: MeshRevisionTag,
    pub quads: Vec<MeshQuad>,
}

impl GreedyMesh {
    /// Returns false after source edits, neighbor load/unload/edit, or job supersession.
    pub fn is_current(
        &self,
        chunk: &VoxelChunk,
        neighbors: &BTreeMap<ChunkCoord, VoxelChunk>,
        job_generation: u64,
    ) -> bool {
        if self.tag.coord != chunk.coord
            || self.tag.source
                != (ChunkStamp {
                    lifetime_generation: chunk.lifetime_generation,
                    data_revision: chunk.data_revision,
                })
            || self.tag.job_generation != job_generation
        {
            return false;
        }
        self.tag.neighbors.iter().all(|entry| {
            let expected = entry.stamp;
            let actual = neighbors.get(&entry.coord).map(|neighbor| ChunkStamp {
                lifetime_generation: neighbor.lifetime_generation,
                data_revision: neighbor.data_revision,
            });
            expected == actual
        })
    }
}

/// Builds merged exposed faces from an immutable chunk and its currently resident neighbors.
///
/// Interface policy is explicit: air exposes any solid face; same-material interfaces are
/// hidden; opaque faces bordering transparent material remain visible; transparent-to-opaque
/// and transparent-to-transparent interfaces are hidden to avoid duplicate coplanar surfaces.
pub fn mesh_chunk(
    chunk: &VoxelChunk,
    neighbors: &BTreeMap<ChunkCoord, VoxelChunk>,
    materials: &BTreeMap<u16, VoxelMaterial>,
    job_generation: u64,
) -> Result<GreedyMesh, VoxelError> {
    mesh_chunk_with_budget(
        chunk,
        neighbors,
        materials,
        job_generation,
        DEFAULT_MAX_MESH_QUADS,
    )
}

/// Greedy meshing with an explicit maximum quad count. A budget breach is an error, never a
/// partially successful/truncated mesh.
pub fn mesh_chunk_with_budget(
    chunk: &VoxelChunk,
    neighbors: &BTreeMap<ChunkCoord, VoxelChunk>,
    materials: &BTreeMap<u16, VoxelMaterial>,
    job_generation: u64,
    max_quads: usize,
) -> Result<GreedyMesh, VoxelError> {
    let expected_count = chunk.dimensions.cell_count()?;
    if chunk.cells.len() != expected_count {
        return Err(VoxelError::CellCountMismatch);
    }
    if let Some(id) = chunk.cells.iter().find_map(|cell| {
        cell.material
            .filter(|material| !materials.contains_key(material))
    }) {
        return Err(VoxelError::UnknownMaterial(id));
    }
    for (coord, neighbor) in neighbors {
        if *coord != neighbor.coord {
            return Err(VoxelError::NeighborCoordinateMismatch);
        }
        if neighbor.dimensions != chunk.dimensions {
            return Err(VoxelError::NeighborDimensionsMismatch);
        }
        if neighbor.cells.len() != neighbor.dimensions.cell_count()? {
            return Err(VoxelError::CellCountMismatch);
        }
    }
    let mut neighbor_tags = Vec::with_capacity(6);
    for coord in adjacent_coords(chunk.coord)? {
        neighbor_tags.push(NeighborRevision {
            coord,
            stamp: neighbors.get(&coord).map(|neighbor| ChunkStamp {
                lifetime_generation: neighbor.lifetime_generation,
                data_revision: neighbor.data_revision,
            }),
        });
    }
    let tag = MeshRevisionTag {
        coord: chunk.coord,
        source: ChunkStamp {
            lifetime_generation: chunk.lifetime_generation,
            data_revision: chunk.data_revision,
        },
        neighbors: neighbor_tags,
        job_generation,
    };
    let mut quads = Vec::new();
    let dims = [chunk.dimensions.x, chunk.dimensions.y, chunk.dimensions.z];
    for axis in 0..3 {
        let u_axis = (axis + 1) % 3;
        let v_axis = (axis + 2) % 3;
        let plane_width = dims[u_axis];
        let plane_height = dims[v_axis];
        let mut mask = vec![None; usize::from(plane_width) * usize::from(plane_height)];
        for slice in 0..=dims[axis] {
            for positive in [false, true] {
                mask.fill(None);
                for v in 0..plane_height {
                    for u in 0..plane_width {
                        let mut low = [0_i32; 3];
                        let mut high = [0_i32; 3];
                        low[axis] = i32::from(slice) - 1;
                        high[axis] = i32::from(slice);
                        low[u_axis] = i32::from(u);
                        high[u_axis] = i32::from(u);
                        low[v_axis] = i32::from(v);
                        high[v_axis] = i32::from(v);
                        let (source_pos, neighbor_pos) =
                            if positive { (low, high) } else { (high, low) };
                        if !position_inside(source_pos, dims) {
                            continue;
                        }
                        let source = sample(chunk, neighbors, source_pos);
                        let neighbor = sample(chunk, neighbors, neighbor_pos);
                        let Some(source_material) = source.material else {
                            continue;
                        };
                        let source_properties = materials
                            .get(&source_material)
                            .ok_or(VoxelError::UnknownMaterial(source_material))?;
                        let neighbor_properties = neighbor
                            .material
                            .map(|id| materials.get(&id).ok_or(VoxelError::UnknownMaterial(id)))
                            .transpose()?;
                        if !face_is_visible(source_material, source_properties, neighbor_properties)
                        {
                            continue;
                        }
                        mask[usize::from(u) + usize::from(plane_width) * usize::from(v)] =
                            Some(FaceKey {
                                material_id: source_material,
                                surface: source_properties.surface,
                                merge_group: source_properties.merge_group,
                            });
                    }
                }
                consume_mask(
                    &mut mask,
                    MaskPlane {
                        width: plane_width,
                        height: plane_height,
                        axis,
                        u_axis,
                        v_axis,
                        slice,
                        face: face_for(axis, positive),
                    },
                    max_quads,
                    &mut quads,
                )?;
            }
        }
    }
    Ok(GreedyMesh { tag, quads })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct FaceKey {
    material_id: u16,
    surface: MaterialSurface,
    merge_group: u16,
}

fn face_is_visible(
    source_id: u16,
    source: &VoxelMaterial,
    neighbor: Option<&VoxelMaterial>,
) -> bool {
    match neighbor {
        None => true,
        Some(neighbor) if source_id == neighbor.id => false,
        Some(neighbor)
            if source.surface == MaterialSurface::Opaque
                && neighbor.surface == MaterialSurface::Transparent =>
        {
            true
        }
        Some(_) => false,
    }
}

fn sample(
    chunk: &VoxelChunk,
    neighbors: &BTreeMap<ChunkCoord, VoxelChunk>,
    position: [i32; 3],
) -> VoxelCell {
    let dims = [
        i32::from(chunk.dimensions.x),
        i32::from(chunk.dimensions.y),
        i32::from(chunk.dimensions.z),
    ];
    if position
        .iter()
        .zip(dims)
        .all(|(coordinate, length)| *coordinate >= 0 && *coordinate < length)
    {
        return chunk
            .cell(position[0] as u16, position[1] as u16, position[2] as u16)
            .unwrap_or(VoxelCell::AIR);
    }
    let mut delta = [0_i32; 3];
    let mut local = position;
    for axis in 0..3 {
        if position[axis] < 0 {
            delta[axis] = -1;
            local[axis] += dims[axis];
        } else if position[axis] >= dims[axis] {
            delta[axis] = 1;
            local[axis] -= dims[axis];
        }
    }
    if delta.iter().filter(|delta| **delta != 0).count() != 1 {
        return VoxelCell::AIR;
    }
    let Some(coord) = chunk
        .coord
        .x
        .checked_add(delta[0])
        .zip(chunk.coord.y.checked_add(delta[1]))
        .zip(chunk.coord.z.checked_add(delta[2]))
        .map(|((x, y), z)| ChunkCoord { x, y, z })
    else {
        return VoxelCell::AIR;
    };
    neighbors
        .get(&coord)
        .and_then(|neighbor| neighbor.cell(local[0] as u16, local[1] as u16, local[2] as u16))
        .unwrap_or(VoxelCell::AIR)
}

struct MaskPlane {
    width: u16,
    height: u16,
    axis: usize,
    u_axis: usize,
    v_axis: usize,
    slice: u16,
    face: FaceDirection,
}

fn consume_mask(
    mask: &mut [Option<FaceKey>],
    plane: MaskPlane,
    max_quads: usize,
    quads: &mut Vec<MeshQuad>,
) -> Result<(), VoxelError> {
    let width_usize = usize::from(plane.width);
    let height_usize = usize::from(plane.height);
    for v in 0..height_usize {
        for u in 0..width_usize {
            let index = u + width_usize * v;
            let Some(key) = mask[index] else {
                continue;
            };
            let mut quad_width = 1;
            while u + quad_width < width_usize && mask[index + quad_width] == Some(key) {
                quad_width += 1;
            }
            let mut quad_height = 1;
            'grow: while v + quad_height < height_usize {
                for offset in 0..quad_width {
                    if mask[u + offset + width_usize * (v + quad_height)] != Some(key) {
                        break 'grow;
                    }
                }
                quad_height += 1;
            }
            for row in v..v + quad_height {
                for column in u..u + quad_width {
                    mask[column + width_usize * row] = None;
                }
            }
            let mut origin = [0_u16; 3];
            origin[plane.axis] = plane.slice;
            origin[plane.u_axis] = u as u16;
            origin[plane.v_axis] = v as u16;
            if quads.len() >= max_quads {
                return Err(VoxelError::MeshQuadBudgetExceeded { limit: max_quads });
            }
            quads.push(MeshQuad {
                origin,
                extent: [quad_width as u16, quad_height as u16],
                face: plane.face,
                material_id: key.material_id,
                merge_group: key.merge_group,
                surface: key.surface,
            });
        }
    }
    Ok(())
}

fn position_inside(position: [i32; 3], dimensions: [u16; 3]) -> bool {
    position
        .iter()
        .zip(dimensions)
        .all(|(coordinate, length)| *coordinate >= 0 && *coordinate < i32::from(length))
}

fn face_for(axis: usize, positive: bool) -> FaceDirection {
    match (axis, positive) {
        (0, false) => FaceDirection::NegativeX,
        (0, true) => FaceDirection::PositiveX,
        (1, false) => FaceDirection::NegativeY,
        (1, true) => FaceDirection::PositiveY,
        (2, false) => FaceDirection::NegativeZ,
        (2, true) => FaceDirection::PositiveZ,
        _ => unreachable!("axis is in 0..3"),
    }
}

fn adjacent_coords(coord: ChunkCoord) -> Result<[ChunkCoord; 6], VoxelError> {
    let xm = coord
        .x
        .checked_sub(1)
        .ok_or(VoxelError::CoordinateBoundary)?;
    let xp = coord
        .x
        .checked_add(1)
        .ok_or(VoxelError::CoordinateBoundary)?;
    let ym = coord
        .y
        .checked_sub(1)
        .ok_or(VoxelError::CoordinateBoundary)?;
    let yp = coord
        .y
        .checked_add(1)
        .ok_or(VoxelError::CoordinateBoundary)?;
    let zm = coord
        .z
        .checked_sub(1)
        .ok_or(VoxelError::CoordinateBoundary)?;
    let zp = coord
        .z
        .checked_add(1)
        .ok_or(VoxelError::CoordinateBoundary)?;
    Ok([
        ChunkCoord { x: xm, ..coord },
        ChunkCoord { x: xp, ..coord },
        ChunkCoord { y: ym, ..coord },
        ChunkCoord { y: yp, ..coord },
        ChunkCoord { z: zm, ..coord },
        ChunkCoord { z: zp, ..coord },
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    fn materials() -> BTreeMap<u16, VoxelMaterial> {
        [
            VoxelMaterial {
                id: 1,
                surface: MaterialSurface::Opaque,
                merge_group: 0,
            },
            VoxelMaterial {
                id: 2,
                surface: MaterialSurface::Transparent,
                merge_group: 0,
            },
        ]
        .into_iter()
        .map(|material| (material.id, material))
        .collect()
    }

    fn chunk(dimensions: ChunkDimensions, cells: Vec<VoxelCell>, revision: u64) -> VoxelChunk {
        VoxelChunk::new(
            ChunkCoord { x: 0, y: 0, z: 0 },
            dimensions,
            1,
            1,
            revision,
            cells,
        )
        .unwrap()
    }

    #[test]
    fn solid_cube_exposes_six_merged_faces() {
        let dims = ChunkDimensions { x: 2, y: 2, z: 2 };
        let chunk = chunk(dims, vec![VoxelCell::solid(1); 8], 4);
        let mesh = mesh_chunk(&chunk, &BTreeMap::new(), &materials(), 9).unwrap();
        assert_eq!(mesh.quads.len(), 6);
        assert!(mesh.quads.iter().all(|quad| quad.extent == [2, 2]));
        assert!(mesh.quads.iter().all(|quad| quad.material_id == 1));
    }

    #[test]
    fn internal_material_boundaries_do_not_merge_or_create_hidden_faces() {
        let dims = ChunkDimensions { x: 2, y: 1, z: 1 };
        let same_material = chunk(dims, vec![VoxelCell::solid(1), VoxelCell::solid(1)], 0);
        let mesh = mesh_chunk(&same_material, &BTreeMap::new(), &materials(), 0).unwrap();
        assert_eq!(mesh.quads.len(), 6);

        let different_material = chunk(dims, vec![VoxelCell::solid(1), VoxelCell::solid(2)], 0);
        let mesh = mesh_chunk(&different_material, &BTreeMap::new(), &materials(), 0).unwrap();
        assert_eq!(mesh.quads.len(), 11);
        assert!(
            mesh.quads
                .iter()
                .any(|quad| { quad.face == FaceDirection::PositiveX && quad.material_id == 1 })
        );
    }

    #[test]
    fn neighbor_occupancy_hides_chunk_boundary_and_neighbor_change_stales_mesh() {
        let dims = ChunkDimensions { x: 1, y: 1, z: 1 };
        let source = chunk(dims, vec![VoxelCell::solid(1)], 3);
        let neighbor_coord = ChunkCoord { x: 1, y: 0, z: 0 };
        let neighbor =
            VoxelChunk::new(neighbor_coord, dims, 1, 2, 8, vec![VoxelCell::solid(1)]).unwrap();
        let neighbors = BTreeMap::from([(neighbor_coord, neighbor.clone())]);
        let mesh = mesh_chunk(&source, &neighbors, &materials(), 5).unwrap();
        assert_eq!(mesh.quads.len(), 5);
        assert!(mesh.is_current(&source, &neighbors, 5));
        let edited = VoxelChunk {
            data_revision: 9,
            ..neighbor
        };
        let changed = BTreeMap::from([(neighbor_coord, edited)]);
        assert!(!mesh.is_current(&source, &changed, 5));
        assert!(!mesh.is_current(&source, &neighbors, 6));
    }

    #[test]
    fn empty_chunk_never_emits_faces_owned_by_a_solid_neighbor() {
        let dims = ChunkDimensions { x: 1, y: 1, z: 1 };
        let empty = chunk(dims, vec![VoxelCell::AIR], 1);
        let neighbor_coord = ChunkCoord { x: 1, y: 0, z: 0 };
        let neighbor =
            VoxelChunk::new(neighbor_coord, dims, 1, 4, 6, vec![VoxelCell::solid(1)]).unwrap();
        let neighbors = BTreeMap::from([(neighbor_coord, neighbor)]);
        let mesh = mesh_chunk(&empty, &neighbors, &materials(), 2).unwrap();
        assert!(mesh.quads.is_empty());
        assert!(!mesh.is_current(&empty, &BTreeMap::new(), 2));
    }

    #[test]
    fn opaque_transparent_boundary_is_owned_by_the_opaque_source_only() {
        let dims = ChunkDimensions { x: 1, y: 1, z: 1 };
        let opaque = chunk(dims, vec![VoxelCell::solid(1)], 1);
        let neighbor_coord = ChunkCoord { x: 1, y: 0, z: 0 };
        let transparent =
            VoxelChunk::new(neighbor_coord, dims, 1, 2, 3, vec![VoxelCell::solid(2)]).unwrap();
        let neighbors = BTreeMap::from([(neighbor_coord, transparent)]);
        let mesh = mesh_chunk(&opaque, &neighbors, &materials(), 0).unwrap();
        assert_eq!(mesh.quads.len(), 6);
        assert!(
            mesh.quads
                .iter()
                .any(|quad| { quad.face == FaceDirection::PositiveX && quad.material_id == 1 })
        );

        let transparent_source = chunk(dims, vec![VoxelCell::solid(2)], 1);
        let opaque_neighbor =
            VoxelChunk::new(neighbor_coord, dims, 1, 2, 3, vec![VoxelCell::solid(1)]).unwrap();
        let neighbors = BTreeMap::from([(neighbor_coord, opaque_neighbor)]);
        let mesh = mesh_chunk(&transparent_source, &neighbors, &materials(), 0).unwrap();
        assert_eq!(mesh.quads.len(), 5);
        assert!(
            !mesh
                .quads
                .iter()
                .any(|quad| quad.face == FaceDirection::PositiveX)
        );
    }

    #[test]
    fn greedy_mesh_returns_error_when_quad_budget_is_exceeded() {
        let dims = ChunkDimensions { x: 1, y: 1, z: 1 };
        let source = chunk(dims, vec![VoxelCell::solid(1)], 0);
        assert_eq!(
            mesh_chunk_with_budget(&source, &BTreeMap::new(), &materials(), 0, 5),
            Err(VoxelError::MeshQuadBudgetExceeded { limit: 5 })
        );
    }

    #[test]
    fn coordinate_limit_is_rejected_instead_of_aliasing_a_neighbor() {
        let dims = ChunkDimensions { x: 1, y: 1, z: 1 };
        let source = VoxelChunk::new(
            ChunkCoord {
                x: i32::MAX,
                y: 0,
                z: 0,
            },
            dims,
            1,
            1,
            0,
            vec![VoxelCell::solid(1)],
        )
        .unwrap();
        assert_eq!(
            mesh_chunk(&source, &BTreeMap::new(), &materials(), 0),
            Err(VoxelError::CoordinateBoundary)
        );
    }

    #[test]
    fn chunk_generation_is_seeded_and_revision_tags_are_preserved() {
        let dims = ChunkDimensions { x: 8, y: 16, z: 8 };
        let config = TerrainGenerator::default();
        let first =
            generate_chunk(42, ChunkCoord { x: -2, y: 0, z: 7 }, dims, config, 3, 11).unwrap();
        let second =
            generate_chunk(42, ChunkCoord { x: -2, y: 0, z: 7 }, dims, config, 3, 11).unwrap();
        let other_seed =
            generate_chunk(43, ChunkCoord { x: -2, y: 0, z: 7 }, dims, config, 3, 11).unwrap();
        assert_eq!(first, second);
        assert_ne!(first.cells, other_seed.cells);
        assert_eq!(first.generator_version, 1);
        assert_eq!(first.data_revision, 11);
        assert_eq!(first.lifetime_generation, 3);
    }
}
