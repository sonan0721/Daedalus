struct Globals {
    view_projection: mat4x4<f32>,
    camera_position: vec4<f32>,
    light_direction_intensity: vec4<f32>,
    light_color: vec4<f32>,
    ambient_and_manual_srgb: vec4<f32>,
};

struct Material {
    base_color: vec4<f32>,
    metallic_roughness: vec4<f32>,
    emissive: vec4<f32>,
};

struct Materials {
    values: array<Material>,
};

@group(0) @binding(0) var<uniform> globals: Globals;
@group(1) @binding(0) var<storage, read> materials: Materials;

struct MeshVertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
};

struct PacketVertexInput {
    @location(0) position: vec3<f32>,
    @location(1) normal: vec3<f32>,
    @location(2) model_0: vec4<f32>,
    @location(3) model_1: vec4<f32>,
    @location(4) model_2: vec4<f32>,
    @location(5) model_3: vec4<f32>,
    @location(6) normal_0: vec4<f32>,
    @location(7) normal_1: vec4<f32>,
    @location(8) normal_2: vec4<f32>,
    @location(9) material_index: u32,
};

struct MeshVertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) world_position: vec3<f32>,
    @location(1) world_normal: vec3<f32>,
    @location(2) @interpolate(flat) material_index: u32,
};

@vertex
fn vs_main(input: MeshVertexInput, @builtin(instance_index) material_index: u32) -> MeshVertexOutput {
    var output: MeshVertexOutput;
    output.world_position = input.position;
    output.world_normal = normalize(input.normal);
    output.clip_position = globals.view_projection * vec4<f32>(input.position, 1.0);
    output.material_index = material_index;
    return output;
}

@vertex
fn vs_packet(input: PacketVertexInput) -> MeshVertexOutput {
    var output: MeshVertexOutput;
    let model = mat4x4<f32>(input.model_0, input.model_1, input.model_2, input.model_3);
    let normal_matrix = mat3x3<f32>(input.normal_0.xyz, input.normal_1.xyz, input.normal_2.xyz);
    let world_position = model * vec4<f32>(input.position, 1.0);
    output.world_position = world_position.xyz;
    output.world_normal = normalize(normal_matrix * input.normal);
    output.clip_position = globals.view_projection * world_position;
    output.material_index = input.material_index;
    return output;
}

fn fresnel_schlick(cos_theta: f32, f0: vec3<f32>) -> vec3<f32> {
    return f0 + (vec3<f32>(1.0) - f0) * pow(1.0 - clamp(cos_theta, 0.0, 1.0), 5.0);
}

fn distribution_ggx(normal: vec3<f32>, half_vector: vec3<f32>, roughness: f32) -> f32 {
    let alpha = max(roughness * roughness, 0.002);
    let alpha_squared = alpha * alpha;
    let n_dot_h = max(dot(normal, half_vector), 0.0);
    let denominator = n_dot_h * n_dot_h * (alpha_squared - 1.0) + 1.0;
    return alpha_squared / max(3.14159265 * denominator * denominator, 0.000001);
}

fn geometry_schlick_ggx(n_dot_v: f32, roughness: f32) -> f32 {
    let r = roughness + 1.0;
    let k = r * r / 8.0;
    return n_dot_v / max(n_dot_v * (1.0 - k) + k, 0.000001);
}

fn tone_map_aces(color: vec3<f32>) -> vec3<f32> {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((color * (a * color + vec3<f32>(b))) / (color * (c * color + vec3<f32>(d)) + vec3<f32>(e)), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn linear_to_srgb(color: vec3<f32>) -> vec3<f32> {
    let low = color * 12.92;
    let high = 1.055 * pow(max(color, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(high, low, color <= vec3<f32>(0.0031308));
}

@fragment
fn fs_main(input: MeshVertexOutput) -> @location(0) vec4<f32> {
    let material = materials.values[input.material_index];
    let base_color = material.base_color.rgb;
    let metallic = clamp(material.metallic_roughness.x, 0.0, 1.0);
    let roughness = clamp(material.metallic_roughness.y, 0.045, 1.0);
    let normal = normalize(input.world_normal);
    let view_direction = normalize(globals.camera_position.xyz - input.world_position);
    let light_direction = normalize(-globals.light_direction_intensity.xyz);
    let half_vector = normalize(view_direction + light_direction);

    let n_dot_l = max(dot(normal, light_direction), 0.0);
    let n_dot_v = max(dot(normal, view_direction), 0.001);
    let h_dot_v = max(dot(half_vector, view_direction), 0.0);
    let f0 = mix(vec3<f32>(0.04), base_color, metallic);
    let fresnel = fresnel_schlick(h_dot_v, f0);
    let distribution = distribution_ggx(normal, half_vector, roughness);
    let geometry = geometry_schlick_ggx(n_dot_v, roughness) * geometry_schlick_ggx(n_dot_l, roughness);
    let specular = distribution * geometry * fresnel / max(4.0 * n_dot_v * n_dot_l, 0.001);
    let diffuse_weight = (vec3<f32>(1.0) - fresnel) * (1.0 - metallic);
    let diffuse = diffuse_weight * base_color / 3.14159265;
    let direct = (diffuse + specular) * globals.light_color.rgb * globals.light_direction_intensity.w * n_dot_l;
    // A constant environment approximation keeps metal surfaces visible until
    // the renderer gains a measured image-based lighting path.
    let ambient = globals.ambient_and_manual_srgb.rgb
        * (base_color * diffuse_weight + fresnel * (1.0 - roughness * 0.5));
    let linear_color = tone_map_aces(direct + ambient + material.emissive.rgb);
    let output_color = select(linear_color, linear_to_srgb(linear_color), globals.ambient_and_manual_srgb.w > 0.5);
    return vec4<f32>(output_color, 1.0);
}
