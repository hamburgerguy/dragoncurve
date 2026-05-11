// Dragon Curve — nannou 0.19 + gif 0.14
//
// Cargo.toml dependencies:
//   nannou = "0.19"
//   gif = "0.14"
//
// Controls:
//   G      start recording (restarts animation, captures camera+zoom into GIF)
//   S      save dragon_curve.gif  (also auto-saves when animation finishes)
//   R      restart / clear recording
//   Esc    quit

use nannou::prelude::*;
use std::fs::File;

// ---------------------------------------------------------------------------
// Tunables
// ---------------------------------------------------------------------------

const ORDER: usize = 14;    // fold depth — 2^ORDER – 1 segments
const CANVAS: u32  = 1080;  // window + soft-render size (square)
const MARGIN: f32  = 90.0;  // padding when fully zoomed out

// Speed ramp: segments added per tick, lerped DRAW_MIN → DRAW_MAX.
// A power >1 on the ease keeps it pinned near DRAW_MIN for longer.
const DRAW_MIN: f32   = 1.0;    // start: one segment per frame
const DRAW_MAX: f32   = 300.0;  // end: blazing fast
const SPEED_POWER: i32 = 5;     // ease exponent — higher = slower longer

// Zoom ramp: camera scale ZOOM_IN → ZOOM_OUT (1.0 = full view)
const ZOOM_IN:  f32 = 12.0;
const ZOOM_OUT: f32 = 1.0;
const ZOOM_POWER: i32 = 3;     // ease exponent for zoom

// Fraction of total progress over which the ramp plays out
const RAMP_END: f32 = 0.90;

// Camera pan smoothing — scales with zoom so the head stays tight on screen
// when zoomed in and settles gently when zoomed out.
// alpha = clamp(CAM_PAN_BASE * cam_zoom, CAM_PAN_MIN, CAM_PAN_MAX)
const CAM_PAN_BASE: f32  = 0.08; // multiplied by current zoom each tick
const CAM_PAN_MIN:  f32  = 0.08; // floor — never too sluggish
const CAM_PAN_MAX:  f32  = 0.55; // ceiling — never teleports

// Zoom smoothing (separate, slower feels better visually)
const CAM_ZOOM_SMOOTH: f32 = 0.045;

// GIF output
const GIF_SIZE: u16        = 540;  // pixels — bigger to preserve zoom detail
const GIF_CAPTURE_EVERY: usize = 1; // capture every frame during recording
const GIF_FRAME_DELAY: u16 = 3;    // ×10 ms per frame (3 = 30 ms ≈ 33 fps)

// ---------------------------------------------------------------------------
// Colour gradient
// ---------------------------------------------------------------------------

fn hue_rgb(t: f32) -> (u8, u8, u8) {
    let r = ((0.5 + 0.5 * (t * TAU).cos())       * 255.0) as u8;
    let g = ((0.5 + 0.5 * (t * TAU + 2.1).cos()) * 255.0) as u8;
    let b = ((0.5 + 0.5 * (t * TAU + 4.2).cos()) * 255.0) as u8;
    (r, g, b)
}

// ---------------------------------------------------------------------------
// Easing — ease-out with configurable power
// ---------------------------------------------------------------------------

fn ease_out_n(t: f32, n: i32) -> f32 {
    1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(n)
}

fn ramp_t(progress: f32) -> f32 {
    (progress / RAMP_END).clamp(0.0, 1.0)
}

fn target_zoom(progress: f32) -> f32 {
    let e = ease_out_n(ramp_t(progress), ZOOM_POWER);
    ZOOM_IN + (ZOOM_OUT - ZOOM_IN) * e
}

fn draw_step(progress: f32) -> usize {
    let e = ease_out_n(ramp_t(progress), SPEED_POWER);
    (DRAW_MIN + (DRAW_MAX - DRAW_MIN) * e).round().max(1.0) as usize
}

// ---------------------------------------------------------------------------
// Dragon-curve construction
// ---------------------------------------------------------------------------

fn build_turns(order: usize) -> Vec<i8> {
    let mut turns: Vec<i8> = vec![1];
    for _ in 1..order {
        let n = turns.len();
        turns.push(1);
        for i in (0..n).rev() {
            let v = turns[i];
            turns.push(-v);
        }
    }
    turns
}

fn turns_to_points(turns: &[i8]) -> Vec<Vec2> {
    let mut pts = Vec::with_capacity(turns.len() + 1);
    let mut pos = Vec2::ZERO;
    let mut dir = vec2(1.0, 0.0);
    pts.push(pos);
    for &t in turns {
        pos += dir;
        pts.push(pos);
        dir = if t == 1 { vec2(-dir.y, dir.x) } else { vec2(dir.y, -dir.x) };
    }
    pts
}

fn fit_to_canvas(pts: &[Vec2], canvas: f32, margin: f32) -> Vec<Vec2> {
    let mut mn_x = f32::INFINITY;
    let mut mx_x = f32::NEG_INFINITY;
    let mut mn_y = f32::INFINITY;
    let mut mx_y = f32::NEG_INFINITY;
    for p in pts {
        mn_x = mn_x.min(p.x); mx_x = mx_x.max(p.x);
        mn_y = mn_y.min(p.y); mx_y = mx_y.max(p.y);
    }
    let scale = (canvas - margin * 2.0) / (mx_x - mn_x).max(mx_y - mn_y);
    let cx = (mn_x + mx_x) * 0.5;
    let cy = (mn_y + mx_y) * 0.5;
    pts.iter().map(|p| vec2((p.x - cx) * scale, (p.y - cy) * scale)).collect()
}

// ---------------------------------------------------------------------------
// Software rasteriser  (used to bake camera + zoom into GIF frames)
// ---------------------------------------------------------------------------

struct SoftCanvas {
    w: usize,
    h: usize,
    buf: Vec<u8>, // RGBA row-major
}

impl SoftCanvas {
    fn new(w: usize, h: usize) -> Self {
        Self { w, h, buf: vec![0u8; w * h * 4] }
    }

    fn clear(&mut self) {
        self.buf.iter_mut().for_each(|b| *b = 0);
    }

    fn put(&mut self, x: i32, y: i32, r: u8, g: u8, b: u8) {
        if x < 0 || y < 0 || x >= self.w as i32 || y >= self.h as i32 { return; }
        let i = (y as usize * self.w + x as usize) * 4;
        self.buf[i] = r; self.buf[i+1] = g; self.buf[i+2] = b; self.buf[i+3] = 255;
    }

    fn line1(&mut self, mut x0: i32, mut y0: i32, x1: i32, y1: i32, r: u8, g: u8, b: u8) {
        let dx =  (x1 - x0).abs();
        let dy = -(y1 - y0).abs();
        let sx = if x0 < x1 { 1 } else { -1 };
        let sy = if y0 < y1 { 1 } else { -1 };
        let mut err = dx + dy;
        loop {
            self.put(x0, y0, r, g, b);
            if x0 == x1 && y0 == y1 { break; }
            let e2 = 2 * err;
            if e2 >= dy { err += dy; x0 += sx; }
            if e2 <= dx { err += dx; y0 += sy; }
        }
    }

    fn thick_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32,
                  r: u8, g: u8, b: u8, hw: i32) {
        for dy in -hw..=hw {
            for dx in -hw..=hw {
                if dx*dx + dy*dy <= hw*hw + hw {
                    self.line1(x0 as i32+dx, y0 as i32+dy,
                               x1 as i32+dx, y1 as i32+dy, r, g, b);
                }
            }
        }
    }

    /// Render up_to curve segments with the given camera transform baked in.
    ///
    /// The world → pixel mapping mirrors what the nannou view does:
    ///   screen_pos = (world_pos - cam_pan) * cam_zoom
    /// then mapped from nannou screen-space [-half, +half] to pixel [0, w].
    ///
    /// `cam_zoom`  – current zoom scale (matches Model::cam_zoom)
    /// `cam_pan`   – current pan in world coords (matches Model::cam_pos)
    fn draw_curve_with_camera(
        &mut self,
        pts: &[Vec2],
        up_to: usize,
        cam_zoom: f32,
        cam_pan: Vec2,
    ) {
        let half = self.w as f32 / 2.0; // pixel-space centre
        let n = up_to.min(pts.len().saturating_sub(1));

        for i in 0..n {
            let t = i as f32 / (pts.len() - 1) as f32;
            let (r, g, b) = hue_rgb(t);

            // Apply camera: translate by -pan, then scale by zoom
            let a = (pts[i]     - cam_pan) * cam_zoom;
            let b_ = (pts[i + 1] - cam_pan) * cam_zoom;

            // nannou screen Y is up; pixel Y is down — flip Y
            // nannou origin = screen centre; pixel origin = top-left
            let px0 = half + a.x;
            let py0 = half - a.y;
            let px1 = half + b_.x;
            let py1 = half - b_.y;

            // Line weight scales with zoom so it stays visually consistent
            let hw = ((cam_zoom * 0.7).round() as i32).clamp(0, 4);
            self.thick_line(px0, py0, px1, py1, r, g, b, hw);
        }
    }

    // Box-filter downsample + nearest-palette quantise → indexed bytes
    fn to_indexed(&self, tw: usize, th: usize, pal: &[(u8, u8, u8)]) -> Vec<u8> {
        let bx = (self.w / tw).max(1);
        let by = (self.h / th).max(1);
        let mut out = Vec::with_capacity(tw * th);
        for ty in 0..th {
            for tx in 0..tw {
                let (mut sr, mut sg, mut sb, mut cnt) = (0u32, 0u32, 0u32, 0u32);
                for dy in 0..by {
                    for dx in 0..bx {
                        let sx = (tx * bx + dx).min(self.w - 1);
                        let sy = (ty * by + dy).min(self.h - 1);
                        let i = (sy * self.w + sx) * 4;
                        sr += self.buf[i]   as u32;
                        sg += self.buf[i+1] as u32;
                        sb += self.buf[i+2] as u32;
                        cnt += 1;
                    }
                }
                let ar = (sr / cnt) as u8;
                let ag = (sg / cnt) as u8;
                let ab = (sb / cnt) as u8;
                let idx = pal.iter().enumerate()
                    .min_by_key(|(_, c)| {
                        let dr = ar as i32 - c.0 as i32;
                        let dg = ag as i32 - c.1 as i32;
                        let db = ab as i32 - c.2 as i32;
                        dr*dr + dg*dg + db*db
                    })
                    .map(|(i, _)| i)
                    .unwrap_or(0) as u8;
                out.push(idx);
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------

fn build_palette() -> Vec<(u8, u8, u8)> {
    let mut pal = vec![(0u8, 0u8, 0u8)]; // index 0 = black background
    for i in 1usize..256 {
        pal.push(hue_rgb((i - 1) as f32 / 254.0));
    }
    pal
}

fn palette_bytes(pal: &[(u8, u8, u8)]) -> Vec<u8> {
    pal.iter().flat_map(|&(r, g, b)| [r, g, b]).collect()
}

// ---------------------------------------------------------------------------
// GIF writer
// ---------------------------------------------------------------------------

fn save_gif(frames: &[Vec<u8>], pal: &[(u8, u8, u8)], size: u16, path: &str) {
    println!("Writing {} frames -> {} ...", frames.len(), path);
    let flat_pal = palette_bytes(pal);
    let file = File::create(path).expect("could not create GIF file");
    let mut encoder = gif::Encoder::new(file, size, size, &flat_pal)
        .expect("GIF encoder error");
    encoder.set_repeat(gif::Repeat::Infinite).unwrap();
    for pixels in frames {
        let mut frame = gif::Frame::from_indexed_pixels(
            size, size, pixels.clone(), Some(0),
        );
        frame.delay = GIF_FRAME_DELAY;
        encoder.write_frame(&frame).expect("GIF write error");
    }
    println!("Saved {}", path);
}

// ---------------------------------------------------------------------------
// App model
// ---------------------------------------------------------------------------

struct Model {
    pts:       Vec<Vec2>,
    total:     usize,
    head:      usize,     // segments drawn so far
    tick:      usize,
    cam_zoom:  f32,       // current (smoothed) camera zoom
    cam_pos:   Vec2,      // current (smoothed) camera pan, world-space
    recording: bool,
    pal:       Vec<(u8, u8, u8)>,
    soft:      SoftCanvas,
    frames:    Vec<Vec<u8>>,
    done:      bool,
}

fn make_model(app: &App) -> Model {
    app.new_window()
        .size(CANVAS, CANVAS)
        .title("Dragon Curve  |  [G] record  [S] save  [R] restart  [Esc] quit")
        .key_pressed(key_pressed)
        .view(view)
        .build()
        .unwrap();

    let pts = fit_to_canvas(
        &turns_to_points(&build_turns(ORDER)),
        CANVAS as f32,
        MARGIN,
    );
    let total = pts.len().saturating_sub(1);
    let pal  = build_palette();
    let soft = SoftCanvas::new(CANVAS as usize, CANVAS as usize);

    Model {
        pts, total,
        head: 0, tick: 0,
        cam_zoom: ZOOM_IN,
        cam_pos: Vec2::ZERO,
        recording: false,
        pal, soft,
        frames: Vec::new(),
        done: false,
    }
}

fn reset(m: &mut Model) {
    m.head = 0;
    m.tick = 0;
    m.cam_zoom = ZOOM_IN;
    m.cam_pos  = Vec2::ZERO;
    m.frames.clear();
    m.done = false;
}

// ---------------------------------------------------------------------------
// Input
// ---------------------------------------------------------------------------

fn key_pressed(app: &App, m: &mut Model, key: Key) {
    match key {
        Key::R => {
            reset(m);
            m.recording = false;
            println!("Restarted.");
        }
        Key::G => {
            reset(m);
            m.recording = true;
            println!("Recording started — camera + zoom will be baked into GIF.");
        }
        Key::S => {
            if m.frames.is_empty() {
                println!("Nothing recorded yet. Press G to start.");
            } else {
                save_gif(&m.frames, &m.pal, GIF_SIZE, "dragon_curve.gif");
                m.done = true;
            }
        }
        Key::Escape => app.quit(),
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Update
// ---------------------------------------------------------------------------

fn update(_app: &App, m: &mut Model, _u: Update) {
    if m.head >= m.total { return; }

    let progress = m.head as f32 / m.total as f32;

    // ── Advance drawing head ──────────────────────────────────────────────
    let step = draw_step(progress);
    m.head = (m.head + step).min(m.total);
    m.tick += 1;

    // ── Camera zoom ───────────────────────────────────────────────────────
    let tz = target_zoom(progress);
    m.cam_zoom += (tz - m.cam_zoom) * CAM_ZOOM_SMOOTH;

    // ── Camera pan ────────────────────────────────────────────────────────
    // Always target the drawing head directly. The pan alpha scales with the
    // current zoom so the camera keeps up when zoomed in tight (fast drawing)
    // and glides smoothly as it pulls back. Once nearly zoomed out, the target
    // quietly converges to the origin (centre of the full curve).
    let head_world = m.pts[m.head.min(m.pts.len() - 1)];
    let zoom_frac  = ((m.cam_zoom - ZOOM_OUT) / (ZOOM_IN - ZOOM_OUT)).clamp(0.0, 1.0);
    // Target: head position while zoomed in, lerp to origin as zoom out
    let target_pan = head_world * zoom_frac;
    // Pan alpha: faster when zoomed in so the head never escapes the frame
    let pan_alpha  = (CAM_PAN_BASE * m.cam_zoom).clamp(CAM_PAN_MIN, CAM_PAN_MAX);
    m.cam_pos += (target_pan - m.cam_pos) * pan_alpha;

    // ── GIF capture — bake the live camera into the soft rasteriser ───────
    if m.recording && m.tick % GIF_CAPTURE_EVERY == 0 {
        m.soft.clear();
        m.soft.draw_curve_with_camera(&m.pts, m.head, m.cam_zoom, m.cam_pos);
        let indexed = m.soft.to_indexed(GIF_SIZE as usize, GIF_SIZE as usize, &m.pal);
        m.frames.push(indexed);
    }

    // Auto-save on animation completion
    if m.recording && m.head >= m.total && !m.done {
        if let Some(last) = m.frames.last().cloned() {
            for _ in 0..40 { m.frames.push(last.clone()); }
        }
        save_gif(&m.frames, &m.pal, GIF_SIZE, "dragon_curve.gif");
        m.done      = true;
        m.recording = false;
        println!("Auto-saved!");
    }
}

// ---------------------------------------------------------------------------
// View (GPU / live window)
// ---------------------------------------------------------------------------

fn view(app: &App, m: &Model, frame: Frame) {
    let draw = app.draw();
    draw.background().color(BLACK);

    let total = m.total as f32;

    // Camera transform: pan then zoom (matches the soft-rasteriser logic)
    let cam = draw
        .scale(m.cam_zoom)
        .translate(vec3(-m.cam_pos.x, -m.cam_pos.y, 0.0));

    for i in 0..m.head {
        let t = i as f32 / total;
        let (r, g, b) = hue_rgb(t);
        let col  = srgba(r as f32/255.0, g as f32/255.0, b as f32/255.0, 1.00);
        let glow = srgba(r as f32/255.0, g as f32/255.0, b as f32/255.0, 0.14);
        let w  = 1.4 / m.cam_zoom;
        let wg = 6.0 / m.cam_zoom;
        cam.line().start(m.pts[i]).end(m.pts[i+1]).weight(w ).color(col);
        cam.line().start(m.pts[i]).end(m.pts[i+1]).weight(wg).color(glow);
    }

    // Drawing-head dot
    if m.head < m.total {
        let t = m.head as f32 / total;
        let (r, g, b) = hue_rgb(t);
        cam.ellipse()
            .xy(m.pts[m.head])
            .radius(5.0 / m.cam_zoom)
            .color(srgba(r as f32/255.0, g as f32/255.0, b as f32/255.0, 1.0));
    }

    // HUD (screen-space — outside camera)
    let hud: String = if m.done {
        "Saved dragon_curve.gif".into()
    } else if m.recording {
        format!("REC  {} frames  zoom {:.1}x  |  [S] save  [R] restart", m.frames.len(), m.cam_zoom)
    } else {
        format!(
            "{}/{} segs  zoom {:.1}x  |  [G] record  [R] restart  [Esc] quit",
            m.head, m.total, m.cam_zoom
        )
    };
    let hud_color = if m.recording { srgba(1.0, 0.25, 0.25, 1.0) }
                    else           { srgba(0.55, 0.55, 0.75, 0.85) };
    draw.text(&hud)
        .xy(vec2(0.0, -(CANVAS as f32/2.0) + 22.0))
        .font_size(15)
        .color(hud_color);

    draw.to_frame(app, &frame).unwrap();
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    nannou::app(make_model).update(update).run();
}