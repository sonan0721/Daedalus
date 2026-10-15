struct OverlayGlobals {
    time: f32,
    width: f32,
    height: f32,
    manual_srgb: f32,
};

@group(0) @binding(0) var<uniform> overlay_globals: OverlayGlobals;

@vertex
fn vs_overlay(@builtin(vertex_index) vertex_index: u32) -> @builtin(position) vec4<f32> {
    var positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    return vec4<f32>(positions[vertex_index], 0.0, 1.0);
}

fn linear_to_srgb(color: vec3<f32>) -> vec3<f32> {
    let low = color * 12.92;
    let high = 1.055 * pow(max(color, vec3<f32>(0.0)), vec3<f32>(1.0 / 2.4)) - vec3<f32>(0.055);
    return select(high, low, color <= vec3<f32>(0.0031308));
}

@fragment
fn fs_overlay(@builtin(position) position: vec4<f32>) -> @location(0) vec4<f32> {
    let uv = position.xy / vec2<f32>(overlay_globals.width, overlay_globals.height);
    let bar_center = vec2<f32>(0.5, 0.88);
    let half_size = vec2<f32>(0.20, 0.012);
    let relative = abs(uv - bar_center);
    let in_bar = relative.x < half_size.x && relative.y < half_size.y;
    let background = vec4<f32>(0.015, 0.022, 0.035, 0.18);
    var output = background;
    if in_bar {
        let along = (uv.x - (bar_center.x - half_size.x)) / (2.0 * half_size.x);
        let phase = fract(along - overlay_globals.time * 0.42);
        let pulse = 0.30 + 0.70 * smoothstep(0.0, 0.16, 1.0 - abs(phase - 0.5) * 2.0);
        output = vec4<f32>(0.16, 0.66, 0.96, pulse * 0.94);
    }
    if overlay_globals.manual_srgb > 0.5 {
        output = vec4<f32>(linear_to_srgb(output.rgb), output.a);
    }
    return output;
}
