use bytemuck::{Pod, Zeroable};
use wgpu::util::DeviceExt;

use crate::game::CarState;

const GAME_SHADER: &str = r#"
struct GameUniform {
    car_x: f32,
    car_y: f32,
    heading: f32,
    speed: f32,
    steering: f32,
    throttle: f32,
    brake: f32,
    time: f32,
    aspect: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
}

struct Hit {
    t: f32,
    color: vec3<f32>,
    normal: vec3<f32>,
    emissive: f32,
}

@group(0) @binding(0) var<uniform> game: GameUniform;

struct VsOut {
    @builtin(position) position: vec4<f32>,
    @location(0) uv: vec2<f32>,
}

@vertex
fn vs_main(@builtin(vertex_index) i: u32) -> VsOut {
    let positions = array<vec2<f32>, 3>(
        vec2<f32>(-1.0, -1.0),
        vec2<f32>(3.0, -1.0),
        vec2<f32>(-1.0, 3.0),
    );
    let p = positions[i];
    return VsOut(vec4<f32>(p, 0.0, 1.0), p * 0.5 + vec2<f32>(0.5, 0.5));
}

fn line_grid(v: vec2<f32>, scale: f32, width: f32) -> f32 {
    let g = abs(fract(v / scale - 0.5) - 0.5) / fwidth(v / scale);
    return 1.0 - min(min(g.x, g.y), 1.0 / width);
}

fn ray_box(ro: vec3<f32>, rd: vec3<f32>, half_size: vec3<f32>) -> vec2<f32> {
    let safe_rd = select(vec3<f32>(0.0001), rd, abs(rd) > vec3<f32>(0.0001));
    let inv = 1.0 / safe_rd;
    let t0 = (-half_size - ro) * inv;
    let t1 = (half_size - ro) * inv;
    let lo = min(t0, t1);
    let hi = max(t0, t1);
    return vec2<f32>(max(max(lo.x, lo.y), lo.z), min(min(hi.x, hi.y), hi.z));
}

fn box_normal(p: vec3<f32>, half_size: vec3<f32>) -> vec3<f32> {
    let d = abs(abs(p) - half_size);
    if (d.x < d.y && d.x < d.z) {
        return vec3<f32>(sign(p.x), 0.0, 0.0);
    }
    if (d.y < d.z) {
        return vec3<f32>(0.0, sign(p.y), 0.0);
    }
    return vec3<f32>(0.0, 0.0, sign(p.z));
}

fn merge_box(
    ro: vec3<f32>,
    rd: vec3<f32>,
    center: vec3<f32>,
    half_size: vec3<f32>,
    color: vec3<f32>,
    best: Hit,
) -> Hit {
    var out = best;
    let local_ro = ro - center;
    let span = ray_box(local_ro, rd, half_size);
    if (span.x <= span.y && span.y > 0.0) {
        let t = max(span.x, 0.0);
        if (t < out.t) {
            let p = local_ro + rd * t;
            out.t = t;
            out.color = color;
            out.normal = box_normal(p, half_size);
            out.emissive = 0.0;
        }
    }
    return out;
}

fn merge_emissive_box(
    ro: vec3<f32>,
    rd: vec3<f32>,
    center: vec3<f32>,
    half_size: vec3<f32>,
    color: vec3<f32>,
    emissive: f32,
    best: Hit,
) -> Hit {
    var out = best;
    let local_ro = ro - center;
    let span = ray_box(local_ro, rd, half_size);
    if (span.x <= span.y && span.y > 0.0) {
        let t = max(span.x, 0.0);
        if (t < out.t) {
            let p = local_ro + rd * t;
            out.t = t;
            out.color = color;
            out.normal = box_normal(p, half_size);
            out.emissive = emissive;
        }
    }
    return out;
}

fn merge_wheel(
    ro: vec3<f32>,
    rd: vec3<f32>,
    center: vec3<f32>,
    radius: f32,
    half_width: f32,
    color: vec3<f32>,
    best: Hit,
) -> Hit {
    var out = best;
    let p = ro - center;
    let a = rd.y * rd.y + rd.z * rd.z;
    let b = 2.0 * (p.y * rd.y + p.z * rd.z);
    let c = p.y * p.y + p.z * p.z - radius * radius;
    let disc = b * b - 4.0 * a * c;
    if (disc >= 0.0 && a > 0.00001) {
        let s = sqrt(disc);
        var t = (-b - s) / (2.0 * a);
        if (t <= 0.0) {
            t = (-b + s) / (2.0 * a);
        }
        let x = p.x + rd.x * t;
        if (t > 0.0 && abs(x) <= half_width && t < out.t) {
            let hit = p + rd * t;
            out.t = t;
            out.color = color;
            out.normal = normalize(vec3<f32>(0.0, hit.y, hit.z));
            out.emissive = 0.0;
        }
    }
    return out;
}

fn rotate_y(v: vec3<f32>, angle: f32) -> vec3<f32> {
    let s = sin(angle);
    let c = cos(angle);
    return vec3<f32>(v.x * c - v.z * s, v.y, v.x * s + v.z * c);
}

fn merge_steered_wheel(
    ro: vec3<f32>,
    rd: vec3<f32>,
    center: vec3<f32>,
    steer_angle: f32,
    radius: f32,
    half_width: f32,
    color: vec3<f32>,
    best: Hit,
) -> Hit {
    let local_ro = rotate_y(ro - center, -steer_angle) + center;
    let local_rd = rotate_y(rd, -steer_angle);
    var h = merge_wheel(local_ro, local_rd, center, radius, half_width, color, best);
    if (h.t < best.t) {
        h.normal = rotate_y(h.normal, steer_angle);
    }
    return h;
}

// Flat disc lying in a plane perpendicular to the wheel axis (local x), used to
// cap the open ends of the tire cylinders so they read as solid sidewalls.
fn merge_disc(
    ro: vec3<f32>,
    rd: vec3<f32>,
    center: vec3<f32>,
    axis: f32,
    radius: f32,
    color: vec3<f32>,
    best: Hit,
) -> Hit {
    var out = best;
    if (abs(rd.x) > 0.00001) {
        let t = (center.x - ro.x) / rd.x;
        if (t > 0.0 && t < out.t) {
            let p = ro + rd * t - center;
            if (p.y * p.y + p.z * p.z <= radius * radius) {
                out.t = t;
                out.color = color;
                out.normal = vec3<f32>(axis, 0.0, 0.0);
                out.emissive = 0.0;
            }
        }
    }
    return out;
}

fn merge_steered_disc(
    ro: vec3<f32>,
    rd: vec3<f32>,
    wheel_center: vec3<f32>,
    steer_angle: f32,
    offset_x: f32,
    axis: f32,
    radius: f32,
    color: vec3<f32>,
    best: Hit,
) -> Hit {
    let local_ro = rotate_y(ro - wheel_center, -steer_angle) + wheel_center;
    let local_rd = rotate_y(rd, -steer_angle);
    let disc_center = vec3<f32>(wheel_center.x + offset_x, wheel_center.y, wheel_center.z);
    var h = merge_disc(local_ro, local_rd, disc_center, axis, radius, color, best);
    if (h.t < best.t) {
        h.normal = rotate_y(h.normal, steer_angle);
    }
    return h;
}

fn car_hit(ro: vec3<f32>, rd: vec3<f32>) -> Hit {
    let red = vec3<f32>(0.86, 0.06, 0.035);
    let red_hi = vec3<f32>(1.0, 0.16, 0.08);
    let carbon = vec3<f32>(0.025, 0.025, 0.028);
    let rubber = vec3<f32>(0.012, 0.011, 0.010);
    let halo = vec3<f32>(0.08, 0.08, 0.075);
    var h = Hit(1.0e9, vec3<f32>(0.0), vec3<f32>(0.0, 1.0, 0.0), 0.0);

    // Long F1-style chassis and sidepods.
    h = merge_box(ro, rd, vec3<f32>(0.0, 0.38, -0.08), vec3<f32>(0.34, 0.18, 0.96), red, h);
    h = merge_box(ro, rd, vec3<f32>(-0.43, 0.31, -0.24), vec3<f32>(0.18, 0.12, 0.58), red * 0.74, h);
    h = merge_box(ro, rd, vec3<f32>(0.43, 0.31, -0.24), vec3<f32>(0.18, 0.12, 0.58), red * 0.74, h);

    // Slender nose and front wing.
    h = merge_box(ro, rd, vec3<f32>(0.0, 0.29, 1.15), vec3<f32>(0.14, 0.10, 1.10), red_hi, h);
    h = merge_box(ro, rd, vec3<f32>(0.0, 0.18, 2.14), vec3<f32>(1.05, 0.055, 0.18), carbon, h);
    h = merge_box(ro, rd, vec3<f32>(-0.84, 0.26, 2.03), vec3<f32>(0.06, 0.20, 0.24), carbon, h);
    h = merge_box(ro, rd, vec3<f32>(0.84, 0.26, 2.03), vec3<f32>(0.06, 0.20, 0.24), carbon, h);

    // Cockpit, engine cover, halo, and rear wing.
    h = merge_box(ro, rd, vec3<f32>(0.0, 0.68, -0.35), vec3<f32>(0.22, 0.16, 0.24), vec3<f32>(0.05, 0.09, 0.11), h);
    h = merge_box(ro, rd, vec3<f32>(0.0, 0.66, -0.92), vec3<f32>(0.18, 0.30, 0.42), red * 0.82, h);
    h = merge_box(ro, rd, vec3<f32>(0.0, 0.93, -0.33), vec3<f32>(0.31, 0.035, 0.28), halo, h);
    h = merge_box(ro, rd, vec3<f32>(0.0, 0.58, -1.74), vec3<f32>(0.94, 0.08, 0.22), carbon, h);

    // Four cylindrical tires, axis left-right in car-local space. Front tires
    // yaw with steering input.
    let steer_angle = -game.steering * 0.46;
    h = merge_steered_wheel(ro, rd, vec3<f32>(-0.74, 0.29, 1.28), steer_angle, 0.34, 0.18, rubber, h);
    h = merge_steered_wheel(ro, rd, vec3<f32>(0.74, 0.29, 1.28), steer_angle, 0.34, 0.18, rubber, h);
    h = merge_wheel(ro, rd, vec3<f32>(-0.78, 0.31, -1.12), 0.39, 0.20, rubber, h);
    h = merge_wheel(ro, rd, vec3<f32>(0.78, 0.31, -1.12), 0.39, 0.20, rubber, h);

    // Wheel hubs.
    h = merge_steered_wheel(ro, rd, vec3<f32>(-0.745, 0.29, 1.28), steer_angle, 0.18, 0.205, vec3<f32>(0.75, 0.72, 0.62), h);
    h = merge_steered_wheel(ro, rd, vec3<f32>(0.745, 0.29, 1.28), steer_angle, 0.18, 0.205, vec3<f32>(0.75, 0.72, 0.62), h);
    h = merge_wheel(ro, rd, vec3<f32>(-0.785, 0.31, -1.12), 0.20, 0.225, vec3<f32>(0.75, 0.72, 0.62), h);
    h = merge_wheel(ro, rd, vec3<f32>(0.785, 0.31, -1.12), 0.20, 0.225, vec3<f32>(0.75, 0.72, 0.62), h);

    // Front-tire sidewalls: cap the open cylinder ends with flat rubber discs so
    // the steered front wheels read as solid tires, plus a metallic hub cap.
    let sidewall = vec3<f32>(0.05, 0.047, 0.043);
    let hub = vec3<f32>(0.75, 0.72, 0.62);
    h = merge_steered_disc(ro, rd, vec3<f32>(-0.74, 0.29, 1.28), steer_angle, -0.18, -1.0, 0.34, sidewall, h);
    h = merge_steered_disc(ro, rd, vec3<f32>(-0.74, 0.29, 1.28), steer_angle, 0.18, 1.0, 0.34, sidewall, h);
    h = merge_steered_disc(ro, rd, vec3<f32>(-0.74, 0.29, 1.28), steer_angle, -0.207, -1.0, 0.185, hub, h);
    h = merge_steered_disc(ro, rd, vec3<f32>(0.74, 0.29, 1.28), steer_angle, 0.18, 1.0, 0.34, sidewall, h);
    h = merge_steered_disc(ro, rd, vec3<f32>(0.74, 0.29, 1.28), steer_angle, -0.18, -1.0, 0.34, sidewall, h);
    h = merge_steered_disc(ro, rd, vec3<f32>(0.74, 0.29, 1.28), steer_angle, 0.207, 1.0, 0.185, hub, h);

    // Rear brake/rain lights. Glow softly while coasting and flare under braking.
    let tail = vec3<f32>(1.0, 0.08, 0.05);
    let brake_glow = 0.30 + game.brake * 1.7;
    h = merge_emissive_box(ro, rd, vec3<f32>(0.0, 0.62, -1.90), vec3<f32>(0.09, 0.07, 0.05), tail, brake_glow, h);
    h = merge_emissive_box(ro, rd, vec3<f32>(-0.55, 0.30, -1.86), vec3<f32>(0.07, 0.05, 0.04), tail, brake_glow * 0.8, h);
    h = merge_emissive_box(ro, rd, vec3<f32>(0.55, 0.30, -1.86), vec3<f32>(0.07, 0.05, 0.04), tail, brake_glow * 0.8, h);

    return h;
}

fn aces(x: vec3<f32>) -> vec3<f32> {
    let a = 2.51;
    let b = 0.03;
    let c = 2.43;
    let d = 0.59;
    let e = 0.14;
    return clamp((x * (a * x + b)) / (x * (c * x + d) + e), vec3<f32>(0.0), vec3<f32>(1.0));
}

fn sky_color(rd: vec3<f32>, light_dir: vec3<f32>) -> vec3<f32> {
    let t = clamp(rd.y * 0.5 + 0.5, 0.0, 1.0);
    var col = mix(vec3<f32>(0.020, 0.045, 0.075), vec3<f32>(0.045, 0.135, 0.225), t);
    col = mix(col, vec3<f32>(0.10, 0.26, 0.40), pow(t, 3.0));
    // Warm band hugging the horizon.
    let horizon = 1.0 - smoothstep(0.0, 0.26, abs(rd.y));
    col += horizon * vec3<f32>(0.22, 0.12, 0.05);
    // Sun disk and broad glow in the key-light direction.
    let s = max(dot(rd, light_dir), 0.0);
    col += pow(s, 320.0) * vec3<f32>(1.9, 1.7, 1.3);
    col += pow(s, 8.0) * vec3<f32>(0.42, 0.30, 0.18);
    return col;
}

fn ground_color(
    world: vec3<f32>,
    dist: f32,
    car_origin: vec3<f32>,
    forward: vec3<f32>,
    right: vec3<f32>,
) -> vec3<f32> {
    let g = vec2<f32>(world.x, world.z);
    let major = line_grid(g, 10.0, 1.3);
    let minor = line_grid(g, 2.0, 0.75) * 0.4;
    let fade = clamp(26.0 / dist, 0.0, 1.0);
    var col = mix(vec3<f32>(0.006, 0.011, 0.014), vec3<f32>(0.018, 0.040, 0.044), fade);
    col += major * vec3<f32>(0.10, 0.52, 0.46) * (0.55 + 0.6 * fade);
    col += minor * vec3<f32>(0.05, 0.18, 0.17);
    // Soft contact shadow shaped to the car footprint, oriented with heading.
    let rel = world - car_origin;
    let lx = dot(rel, right);
    let lz = dot(rel, forward);
    let d = length(vec2<f32>(lx / 1.25, lz / 2.45));
    let shadow = smoothstep(1.0, 0.30, d);
    col *= 1.0 - shadow * 0.74;
    return col;
}

fn shade_car(
    hit: Hit,
    right: vec3<f32>,
    up: vec3<f32>,
    forward: vec3<f32>,
    light_dir: vec3<f32>,
    eye_dir: vec3<f32>,
) -> vec3<f32> {
    let world_normal = normalize(right * hit.normal.x + up * hit.normal.y + forward * hit.normal.z);
    let diffuse = max(dot(world_normal, light_dir), 0.0);
    let half_v = normalize(light_dir + eye_dir);
    let spec = pow(max(dot(world_normal, half_v), 0.0), 56.0);
    let rim = pow(max(1.0 - dot(world_normal, eye_dir), 0.0), 3.0);
    let fill = max(dot(world_normal, vec3<f32>(0.30, 0.20, 0.93)), 0.0);
    var col = hit.color * (0.20 + diffuse * 0.85 + fill * 0.14);
    col += spec * vec3<f32>(1.0, 0.96, 0.86) * 0.7 * (1.0 - hit.emissive);
    col += rim * vec3<f32>(0.18, 0.40, 0.46);
    // Emissive parts (brake lights) ignore shading and bloom past white.
    col = mix(col, hit.color * (0.6 + hit.emissive), hit.emissive);
    return col;
}

fn render_scene(
    cam_pos: vec3<f32>,
    ray: vec3<f32>,
    car_origin: vec3<f32>,
    forward: vec3<f32>,
    right: vec3<f32>,
    up: vec3<f32>,
    light_dir: vec3<f32>,
) -> vec3<f32> {
    let local_ro = vec3<f32>(dot(cam_pos - car_origin, right), cam_pos.y, dot(cam_pos - car_origin, forward));
    let local_rd = vec3<f32>(dot(ray, right), ray.y, dot(ray, forward));
    let hit = car_hit(local_ro, local_rd);

    var ground_t = 1.0e9;
    if (ray.y < -0.0001) {
        ground_t = -cam_pos.y / ray.y;
    }

    if (hit.t < ground_t) {
        return shade_car(hit, right, up, forward, light_dir, -ray);
    }

    if (ground_t < 1.0e8) {
        let world = cam_pos + ray * ground_t;
        var col = ground_color(world, ground_t, car_origin, forward, right);

        // Glossy floor: reflect the car by mirroring the ray about y = 0.
        let refl = vec3<f32>(ray.x, -ray.y, ray.z);
        let rl_ro = vec3<f32>(dot(world - car_origin, right), 0.0, dot(world - car_origin, forward));
        let rl_rd = vec3<f32>(dot(refl, right), refl.y, dot(refl, forward));
        let rhit = car_hit(rl_ro, rl_rd);
        if (rhit.t < 50.0) {
            let rcol = shade_car(rhit, right, up, forward, light_dir, -refl);
            let fres = 0.18 + 0.55 * pow(1.0 - max(-ray.y, 0.0), 4.0);
            let atten = clamp(1.0 - rhit.t * 0.16, 0.0, 1.0);
            col = mix(col, rcol * vec3<f32>(0.72, 0.90, 0.95), fres * atten);
        }

        // Fade distant ground into the sky.
        let fog = 1.0 - clamp(26.0 / ground_t, 0.0, 1.0);
        col = mix(col, sky_color(ray, light_dir), fog * 0.7);
        return col;
    }

    return sky_color(ray, light_dir);
}

@fragment
fn fs_main(in: VsOut) -> @location(0) vec4<f32> {
    let car_origin = vec3<f32>(game.car_x, 0.0, game.car_y);
    let forward = vec3<f32>(sin(game.heading), 0.0, cos(game.heading));
    let right = vec3<f32>(forward.z, 0.0, -forward.x);
    let up = vec3<f32>(0.0, 1.0, 0.0);

    // Chase camera trailing the car, easing back slightly with speed.
    let pull = 6.7 + clamp(game.speed, 0.0, 42.0) * 0.018;
    let cam_pos = car_origin - forward * pull + up * 2.35;
    let look_at = car_origin + forward * 0.72 + up * 0.48;
    let cam_f = normalize(look_at - cam_pos);
    let cam_r = normalize(cross(cam_f, up));
    let cam_u = normalize(cross(cam_r, cam_f));

    let light_dir = normalize(vec3<f32>(-0.42, 0.58, -0.50));

    // Rotated-grid 4x supersampling to soften the ray-traced edges.
    let texel = fwidth(in.uv);
    var offsets = array<vec2<f32>, 4>(
        vec2<f32>(-0.125, -0.375),
        vec2<f32>(0.375, -0.125),
        vec2<f32>(-0.375, 0.125),
        vec2<f32>(0.125, 0.375),
    );

    var acc = vec3<f32>(0.0);
    for (var i = 0; i < 4; i = i + 1) {
        let uv = in.uv + offsets[i] * texel;
        let ndc = uv * 2.0 - vec2<f32>(1.0, 1.0);
        let ray = normalize(cam_f + cam_r * ndc.x * game.aspect * 0.78 + cam_u * ndc.y * 0.78);
        var c = render_scene(cam_pos, ray, car_origin, forward, right, up, light_dir);
        c = aces(c);
        let vignette = smoothstep(1.55, 0.30, length(ndc));
        acc += c * vignette;
    }
    acc = acc * 0.25;
    // Gentle filmic lift.
    acc = pow(acc, vec3<f32>(0.92));
    return vec4<f32>(acc, 1.0);
}
"#;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct GameUniform {
    car_x: f32,
    car_y: f32,
    heading: f32,
    speed: f32,
    steering: f32,
    throttle: f32,
    brake: f32,
    time: f32,
    aspect: f32,
    pad0: f32,
    pad1: f32,
    pad2: f32,
}

pub struct GameRenderer {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    uniform_buffer: wgpu::Buffer,
    start: std::time::Instant,
}

impl GameRenderer {
    pub fn new(device: &wgpu::Device, format: wgpu::TextureFormat) -> Self {
        let uniform_buffer = device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
            label: Some("drive game uniforms"),
            contents: bytemuck::bytes_of(&GameUniform::zeroed()),
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
        });
        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("drive game bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::FRAGMENT,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("drive game bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: uniform_buffer.as_entire_binding(),
            }],
        });
        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("drive game pipeline layout"),
            bind_group_layouts: &[Some(&bind_group_layout)],
            immediate_size: 0,
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("drive game shader"),
            source: wgpu::ShaderSource::Wgsl(GAME_SHADER.into()),
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("drive game pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::REPLACE),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            multiview_mask: None,
            cache: None,
        });

        Self {
            pipeline,
            bind_group,
            uniform_buffer,
            start: std::time::Instant::now(),
        }
    }

    pub fn render(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        target: &wgpu::TextureView,
        width: u32,
        height: u32,
        car: CarState,
    ) {
        let uniform = GameUniform {
            car_x: car.x,
            car_y: car.y,
            heading: car.heading,
            speed: car.speed,
            steering: car.steering,
            throttle: car.throttle,
            brake: car.brake,
            time: self.start.elapsed().as_secs_f32(),
            aspect: width as f32 / height.max(1) as f32,
            pad0: 0.0,
            pad1: 0.0,
            pad2: 0.0,
        };
        queue.write_buffer(&self.uniform_buffer, 0, bytemuck::bytes_of(&uniform));

        let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("drive game render pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: target,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: 0.01,
                        g: 0.015,
                        b: 0.018,
                        a: 1.0,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use solite::gpu::GpuContext;

    #[test]
    fn shader_compiles_on_headless_device() {
        let gpu = pollster::block_on(GpuContext::headless()).expect("headless gpu");
        // Constructing the renderer forces naga to validate the WGSL.
        let _renderer = GameRenderer::new(&gpu.device, wgpu::TextureFormat::Rgba8UnormSrgb);
    }

    // Render a single frame to a PNG for eyeballing the visuals. Ignored by
    // default; run with: cargo test -p solite-drive-game render_frame -- --ignored --nocapture
    #[test]
    #[ignore]
    fn render_frame_to_png() {
        let gpu = pollster::block_on(GpuContext::headless()).expect("headless gpu");
        let device = &gpu.device;
        let queue = &gpu.queue;
        let format = wgpu::TextureFormat::Rgba8UnormSrgb;
        let renderer = GameRenderer::new(device, format);

        let width: u32 = 960;
        let height: u32 = 540;
        let texture = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("frame"),
            size: wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let view = texture.create_view(&wgpu::TextureViewDescriptor::default());

        let car = CarState {
            x: 2.0,
            y: -1.0,
            heading: 0.35,
            speed: 28.0,
            steering: -0.6,
            throttle: 0.0,
            brake: 1.0,
        };

        let bpp = 4u32;
        let unpadded = width * bpp;
        let align = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT;
        let padded = unpadded.div_ceil(align) * align;
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("readback"),
            size: (padded * height) as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        renderer.render(queue, &mut encoder, &view, width, height, car);
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &texture,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buffer,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(padded),
                    rows_per_image: Some(height),
                },
            },
            wgpu::Extent3d {
                width,
                height,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        let slice = buffer.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map"));
        device.poll(wgpu::PollType::wait_indefinitely()).expect("poll");
        let data = slice.get_mapped_range();

        let mut pixels = Vec::with_capacity((width * height * 4) as usize);
        for row in 0..height {
            let start = (row * padded) as usize;
            pixels.extend_from_slice(&data[start..start + unpadded as usize]);
        }
        let img: image::RgbaImage =
            image::ImageBuffer::from_raw(width, height, pixels).expect("image buffer");
        let out = std::path::Path::new("/tmp/drive_game_frame.png");
        img.save(out).expect("save png");
        println!("wrote {}", out.display());

        // Zoomed crop of the car for model inspection.
        let car_crop = image::imageops::crop_imm(&img, 360, 210, 240, 150).to_image();
        let car_crop =
            image::imageops::resize(&car_crop, 960, 600, image::imageops::FilterType::Nearest);
        car_crop.save("/tmp/drive_game_car.png").expect("save car");
    }
}
