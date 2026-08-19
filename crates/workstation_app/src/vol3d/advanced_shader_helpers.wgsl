// Second-generation traversal helpers, appended to the base raymarch prelude
// by `advanced::compose_shader`. Everything the base shader already declares -
// `Uniforms`/`u`, `t_volume`, `s_volume`, `t_lut`, `s_lut`, `t_floor`,
// `s_floor`, `t_color`, `t_light`, `VsOut`, `box_intersect`, `shaded_rgb`,
// `column_max`, `threshold_strength` - is used from here and never redeclared.
//
// The advanced parameters live in their own uniform block rather than in the
// base `Uniforms`, so the base shader text is not edited by this port and the
// two can be reviewed independently.

@group(0) @binding(9) var t_support: texture_3d<f32>;
@group(0) @binding(10) var t_hierarchy_fine: texture_3d<f32>;
@group(0) @binding(11) var t_hierarchy_coarse: texture_3d<f32>;
@group(0) @binding(12) var t_preintegrated: texture_2d<f32>;

// Field order is pinned by `advanced::ADVANCED_UNIFORM_FIELDS` and checked
// against this declaration by a Naga-parsing test. Adding a field here without
// adding it there is a compile-time-silent, render-time-catastrophic mistake.
struct AdvancedUniforms {
    render_mode: f32,
    support_mode: f32,
    support_floor: f32,
    support_fade: f32,

    iso_value: f32,
    iso_width: f32,
    jitter_strength: f32,
    preintegration: f32,

    crop_x_min: f32,
    crop_x_max: f32,
    crop_y_min: f32,
    crop_y_max: f32,

    slice_x: f32,
    slice_y: f32,
    slice_z: f32,
    adaptive_strength: f32,

    // > 0.5 selects the fixed-step, no-hierarchy reference path kept for A/B
    // verification of the accelerated traversal.
    reference_path: f32,
    _advanced_pad_0: f32,
    _advanced_pad_1: f32,
    _advanced_pad_2: f32,
};

@group(0) @binding(13) var<uniform> ua: AdvancedUniforms;

const MAX_TRAVERSAL_STEPS: i32 = 1024;
const FINE_DIMS: vec3<i32> = vec3<i32>(24, 24, 6);
const COARSE_DIMS: vec3<i32> = vec3<i32>(6, 6, 2);
const HUGE_T: f32 = 1.0e20;

// Stable per-pixel offset. Deterministic in screen space, so a static camera
// produces a static image and the A/B capture is repeatable.
fn hash12(p: vec2<f32>) -> f32 {
    let h = dot(p, vec2<f32>(127.1, 311.7));
    return fract(sin(h) * 43758.5453123);
}

fn point_to_uvw(point: vec3<f32>) -> vec3<f32> {
    return vec3<f32>(
        (point.x + 1.0) * 0.5,
        (point.y + 1.0) * 0.5,
        point.z / max(u.zspan, 0.0001)
    );
}

fn hierarchy_coord(uvw: vec3<f32>, dims: vec3<i32>) -> vec3<i32> {
    let scaled = vec3<i32>(floor(clamp(uvw, vec3<f32>(0.0), vec3<f32>(0.999999)) * vec3<f32>(dims)));
    return clamp(scaled, vec3<i32>(0), dims - vec3<i32>(1));
}

// r = minimum field, g = maximum field, b = minimum support, a = maximum
// support, all conservative over the cell PLUS a one-voxel apron so trilinear
// samples taken inside the cell cannot leave the reported interval.
fn fine_range(uvw: vec3<f32>) -> vec4<f32> {
    return textureLoad(t_hierarchy_fine, hierarchy_coord(uvw, FINE_DIMS), 0);
}

fn coarse_range(uvw: vec3<f32>) -> vec4<f32> {
    return textureLoad(t_hierarchy_coarse, hierarchy_coord(uvw, COARSE_DIMS), 0);
}

// The one test that decides whether the optimisation can hide a storm. It must
// answer true whenever ANY point of the cell could paint a pixel under the
// active mode; answering true too often only costs time.
fn range_can_contribute(range: vec4<f32>) -> bool {
    // Maximum support 0 means every voxel the sampler can reach inside this
    // cell is no-data, and no-data is transparent in every threshold mode.
    if (range.a <= 0.0001) {
        return false;
    }
    // Isosurface: the surface can only exist where the maximum reaches it.
    if (ua.render_mode > 1.5 && ua.render_mode < 2.5) {
        return range.g >= ua.iso_value - max(ua.iso_width, 0.002);
    }
    // Hybrid draws the shell AND the interior, so a cell that fails the shell
    // test still has to face the direct-volume tests below.
    if (ua.render_mode > 0.5 && ua.render_mode < 1.5) {
        if (range.g >= ua.iso_value - max(ua.iso_width, 0.002)) {
            return true;
        }
    }
    // Velocity two-box: geometry, opacity and this test all come from the
    // reflectivity structure plane. `smoothstep(ref_gate, ref_gate + 0.08, v)`
    // is zero for every v at or below the gate.
    if (u.velocity_mode > 0.5) {
        return range.g > u.ref_gate;
    }
    if (u.threshold_mode > 1.5) {
        return range.r < u.threshold || range.g > u.threshold_high;
    }
    if (u.threshold_mode > 0.5) {
        return range.r < u.threshold;
    }
    return range.g > u.threshold;
}

fn axis_cell_exit(
    p: f32,
    d: f32,
    cell: i32,
    dim: i32,
    world_min: f32,
    world_max: f32
) -> f32 {
    if (abs(d) < 0.0000001) {
        return HUGE_T;
    }
    let width = (world_max - world_min) / f32(dim);
    var boundary = world_min + f32(cell) * width;
    if (d > 0.0) {
        boundary = boundary + width;
    }
    let distance = (boundary - p) / d;
    if (distance <= 0.000001) {
        return HUGE_T;
    }
    return distance;
}

fn next_cell_exit(point: vec3<f32>, rd: vec3<f32>, dims: vec3<i32>) -> f32 {
    let uvw = point_to_uvw(point);
    let cell = hierarchy_coord(uvw, dims);
    let tx = axis_cell_exit(point.x, rd.x, cell.x, dims.x, -1.0, 1.0);
    let ty = axis_cell_exit(point.y, rd.y, cell.y, dims.y, -1.0, 1.0);
    let tz = axis_cell_exit(point.z, rd.z, cell.z, dims.z, 0.0, u.zspan);
    return max(min(tx, min(ty, tz)), 0.00001);
}

// The authoritative no-data mask. 0 means the reconstruction produced nothing
// here, in every threshold mode including Below and Outside, where the stored
// value of a no-data voxel is 0 and would otherwise paint a solid slab.
fn support_value(uvw: vec3<f32>) -> f32 {
    return textureSampleLevel(t_support, s_volume, uvw, 0.0).r;
}

// Display weighting only. This fades reconstruction the beams barely
// constrain; it is not a confidence, a QC flag, or an error bar.
//
// Both non-fading modes - `Show full reconstruction` and `Color by support` -
// return uniform weight. Fading the inspection mode by the very quantity it
// paints would erase the cone of silence, the wide tilt gaps and the top
// extrapolation at the default floor, which is precisely the anatomy
// VERIFY.md's meteorological-honesty gate asks an operator to go and look at.
fn support_weight(value: f32) -> f32 {
    if (value <= 0.0001) {
        return 0.0;
    }
    if (ua.support_mode > 0.5) {
        return 1.0;
    }
    let floor_value = clamp(ua.support_floor, 0.0, 0.95);
    let normalized = smoothstep(floor_value, 1.0, value);
    return pow(max(normalized, 0.0001), max(ua.support_fade, 0.05));
}

fn support_color(value: f32) -> vec3<f32> {
    let low = vec3<f32>(0.78, 0.16, 0.15);
    let middle = vec3<f32>(0.95, 0.69, 0.16);
    let high = vec3<f32>(0.20, 0.82, 0.90);
    if (value < 0.5) {
        return mix(low, middle, value * 2.0);
    }
    return mix(middle, high, (value - 0.5) * 2.0);
}

// Six bisection steps place the crossing inside 1/64 of one ray step, which is
// well under a source voxel at every quality level.
fn refined_iso_t(ro: vec3<f32>, rd: vec3<f32>, ta_in: f32, tb_in: f32) -> f32 {
    var ta = ta_in;
    var tb = tb_in;
    var va = textureSampleLevel(t_volume, s_volume, point_to_uvw(ro + rd * ta), 0.0).r;
    let vb0 = textureSampleLevel(t_volume, s_volume, point_to_uvw(ro + rd * tb), 0.0).r;
    let rising = vb0 >= va;
    for (var iteration = 0; iteration < 6; iteration = iteration + 1) {
        let tm = 0.5 * (ta + tb);
        let vm = textureSampleLevel(t_volume, s_volume, point_to_uvw(ro + rd * tm), 0.0).r;
        if (rising) {
            if (vm < ua.iso_value) {
                ta = tm;
                va = vm;
            } else {
                tb = tm;
            }
        } else {
            if (vm > ua.iso_value) {
                ta = tm;
                va = vm;
            } else {
                tb = tm;
            }
        }
    }
    return 0.5 * (ta + tb);
}

fn crop_contains(point: vec3<f32>, low_z: f32, high_z: f32) -> bool {
    let x0 = mix(-1.0, 1.0, clamp(ua.crop_x_min, 0.0, 0.99));
    let x1 = mix(-1.0, 1.0, clamp(ua.crop_x_max, ua.crop_x_min + 0.01, 1.0));
    let y0 = mix(-1.0, 1.0, clamp(ua.crop_y_min, 0.0, 0.99));
    let y1 = mix(-1.0, 1.0, clamp(ua.crop_y_max, ua.crop_y_min + 0.01, 1.0));
    return point.x >= x0 && point.x <= x1 && point.y >= y0 && point.y <= y1
        && point.z >= low_z && point.z <= high_z;
}
