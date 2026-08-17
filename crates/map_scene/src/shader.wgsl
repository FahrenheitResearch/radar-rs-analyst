// Retained map line shader.
//
// Vertices are world kilometres plus a world-space perpendicular. The camera
// matrix is applied to the position, while the normal is transformed by the
// matrix's linear part and then renormalised, which strips the zoom and leaves
// a pure screen direction. Offsetting along that direction by a pixel width
// gives strokes that stay the same thickness at every scale without ever
// rewriting a vertex.

struct MapUniform {
    world_to_clip: mat4x4<f32>,
    viewport_px: vec2<f32>,
    pixels_per_point: f32,
    _pad: f32,
};

@group(0) @binding(0) var<uniform> uniforms: MapUniform;

struct VertexInput {
    @location(0) position_km: vec2<f32>,
    @location(1) normal: vec2<f32>,
    @location(2) half_width_px: f32,
    @location(3) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

@vertex
fn vs_main(input: VertexInput) -> VertexOutput {
    var output: VertexOutput;

    let clip = uniforms.world_to_clip * vec4<f32>(input.position_km, 0.0, 1.0);

    // Linear part of the camera transform applied to the perpendicular.
    let rotated = (uniforms.world_to_clip * vec4<f32>(input.normal, 0.0, 0.0)).xy;
    let length = max(length(rotated), 1e-8);
    let direction = rotated / length;

    // Pixels -> clip units. The clip cube spans 2 units across the viewport.
    let half_width = max(input.half_width_px * uniforms.pixels_per_point, 0.5);
    let offset = direction * half_width * 2.0 / max(uniforms.viewport_px, vec2<f32>(1.0, 1.0));

    output.clip_position = vec4<f32>(clip.xy + offset * clip.w, clip.z, clip.w);
    output.color = input.color;
    return output;
}

@fragment
fn fs_main(input: VertexOutput) -> @location(0) vec4<f32> {
    // Straight alpha in, premultiplied out, matching the blend state.
    return vec4<f32>(input.color.rgb * input.color.a, input.color.a);
}
