// name: Morphing cube
// protocol: furumi-visualizer-v2
// bundle-version: 6
//
// Audio-reactive neon wireframe, echo contours, core, shock rings, sparks
// and a bounded particle field. No native 3D API or retained state required.
// Echoes sample earlier rotation times with CURRENT audio; they are analytic
// trails, not stored frames. Particle lifetimes use time, rings also expand
// as the smoothed beat envelope decays. No beat history is available in v2.
//
// Fixed budgets: 8 mesh vertices, at most 48 particles + 24 short tails,
// 20 background stars, 3 rings, 2 echoes. Edges/rings use bulk trace commands.
// Smaller terminals omit echoes/rings and reduce particle counts.

fn clamp(v, lo, hi) {
    if v < lo { lo } else if v > hi { hi } else { v }
}

fn min(a, b) { if a < b { a } else { b } }

fn rgb(r, g, b) {
    to_int(clamp(r, 0.0, 255.0)) * 65536
        + to_int(clamp(g, 0.0, 255.0)) * 256
        + to_int(clamp(b, 0.0, 255.0))
}

// trace runs left-to-right. Swap endpoints for reversed edges and use
// vline for vertical ones (including an edge projected to a single cell).
fn edge(ax, ay, bx, by, fg, bg) {
    if ax > bx {
        let tx = ax; ax = bx; bx = tx;
        let ty = ay; ay = by; by = ty;
    }
    if ax == bx {
        return #{ op: "vline", x: ax, y: min(ay, by),
            h: abs(to_float(by - ay)).to_int() + 1, ch: "|", fg: fg, bg: bg };
    }
    let dx = bx - ax;
    let dy = by - ay;
    let ch = if abs(to_float(dy)) * 3.0 < to_float(dx) { "-" }
        else if abs(to_float(dy)) > to_float(dx) { "|" }
        else if dy < 0 { "/" } else { "\\" };
    let ys = [];
    let slope = to_float(dy) / to_float(dx);
    for x in 0..=dx {
        ys.push(to_int(to_float(ay) + to_float(x) * slope + 0.5));
    }
    #{ op: "trace", x: ax, ys: ys, ch: ch, line_ch: ch,
        fg: fg, line_fg: fg, bg: bg }
}

// Lift quieter passages without a hard gain/clipping threshold.
fn response(level) { 1.0 - pow(1.0 - clamp(level, 0.0, 1.0), 1.6) }

fn palette(t, heat, light) {
    rgb((95.0 + 125.0 * heat + 35.0 * sin(t * 0.43)) * light,
        (185.0 - 95.0 * heat + 55.0 * sin(t * 0.31 + 1.3)) * light,
        255.0 * light)
}

// A single projected mesh, fitted uniformly: never clamp individual corners.
fn mesh(t, energy, bass, mid, treble, beat, width, height) {
    let pulse = 0.70 + energy * 0.45 + beat * 0.85;
    let sx = pulse * (1.0 + bass * 0.55);
    let sy = pulse * (1.0 - bass * 0.42 + mid * 0.15);
    let sz = pulse * (1.0 + treble * 0.22);
    // A bounded beat kick, not time * instantaneous_energy (which jumps).
    let ax = t * 0.47 + 0.35 + beat * 0.22 * sin(t * 1.3);
    let ay = t * 0.63 + 0.55;
    let az = t * 0.23 + beat * 0.16 * cos(t * 0.7);
    let cx = cos(ax); let snx = sin(ax);
    let cy = cos(ay); let sny = sin(ay);
    let cz = cos(az); let snz = sin(az);
    let twist = mid * 0.95 * sin(t * 0.83);
    let ct = cos(twist); let st = sin(twist);
    let vertices = [
        [-1.0, -1.0, -1.0], [1.0, -1.0, -1.0],
        [1.0, 1.0, -1.0], [-1.0, 1.0, -1.0],
        [-1.0, -1.0, 1.0], [1.0, -1.0, 1.0],
        [1.0, 1.0, 1.0], [-1.0, 1.0, 1.0]
    ];
    let projected = [];
    let extent = 0.01;
    for i in 0..8 {
        let v = vertices[i];
        let pull = 1.0 + treble * 0.38 * sin(t * 2.3 + to_float(i) * 2.4)
            + beat * 0.14 * cos(to_float(i) * 1.7 + t);
        let x = v[0] * sx * pull;
        let y = v[1] * sy * pull;
        let z = v[2] * sz * pull;
        let xt = x * ct + z * st * v[1];
        let zt = z * ct - x * st * v[1];
        let y1 = y * cx - zt * snx;
        let z1 = y * snx + zt * cx;
        let x2 = xt * cy + z1 * sny;
        let z2 = z1 * cy - xt * sny;
        let x3 = x2 * cz - y1 * snz;
        let y3 = x2 * snz + y1 * cz;
        // Even the largest deformed vertex stays < 6.4 from the origin.
        let perspective = 10.0 / (10.0 + z2);
        let px = x3 * perspective;
        let py = y3 * perspective;
        if abs(px) > extent { extent = abs(px); }
        if abs(py) > extent { extent = abs(py); }
        projected.push([px, py, z2]);
    }
    // CELL_ASPECT is cell height / width; adjust for a different font.
    let CELL_ASPECT = 2.0;
    let span = min(to_float(width - 4) / CELL_ASPECT, to_float(height - 4));
    let scale = span * min(0.23, 0.46 / extent);
    let center_x = to_float(width - 1) * 0.5;
    let center_y = to_float(height - 1) * 0.5;
    let points = [];
    for p in projected {
        points.push([to_int(center_x + p[0] * scale * CELL_ASPECT + 0.5),
            to_int(center_y + p[1] * scale + 0.5), p[2]]);
    }
    points
}

// Method calls mutate the command array in place; don't copy it into helpers.
fn dot(x, y, ch, fg, bg, width, height) {
    let px = to_int(x + 0.5); let py = to_int(y + 0.5);
    if x >= 0.0 && y >= 1.0 && px < width && py < height - 1 {
        this.push(#{ op: "cell", x: px, y: py, ch: ch, fg: fg, bg: bg });
    }
}

fn render(input) {
    let bg = 0x020610;
    let cmds = [#{ op: "clear", bg: bg }];
    if input.width < 12 || input.height < 8 { return cmds; }
    let w = input.width; let h = input.height;
    let t = input.time;
    let energy = response(input.energy);
    let bass = response(input.bass);
    let mid = response(input.mid);
    let treble = response(input.treble);
    let beat = response(input.beat);
    let drive = clamp(energy * 0.4 + treble * 0.3 + beat * 0.8, 0.0, 1.0);
    let small = w < 40 || h < 16;
    let center_x = to_float(w - 1) * 0.5;
    let center_y = to_float(h - 1) * 0.5;
    let span = min(to_float(w - 4) * 0.5, to_float(h - 4));
    let points = mesh(t, energy, bass, mid, treble, beat, w, h);
    let edges = [[0,1], [1,2], [2,3], [3,0], [4,5], [5,6],
        [6,7], [7,4], [0,4], [1,5], [2,6], [3,7]];

    // Sparse drifting starfield; deterministic, independent of audio hardware.
    let stars = if small { 8 } else { 20 };
    for i in 0..stars {
        let phase = to_float(i);
        let x = (phase * 37.71 + t * (0.3 + to_float(i % 3) * 0.2)) % to_float(w - 1);
        let y = 1.0 + (phase * 13.37) % to_float(h - 3);
        let light = 0.12 + treble * 0.16 * (0.5 + 0.5 * sin(t * 2.0 + phase));
        cmds.dot(x, y, ".", palette(t, 0.0, light), bg, w, h);
    }

    // Three elliptical shock fronts. On a transient they brighten and
    // spread; release of the beat envelope carries them farther outward.
    if !small && drive > 0.04 {
        for ring in 0..3 {
            let phase = (t * 0.45 + to_float(ring) / 3.0) % 1.0;
            let radius = 0.22 + phase * 0.55 + (1.0 - beat) * 0.18;
            let rx = span * radius;
            let ry = span * radius * (0.22 + mid * 0.10);
            let left = to_int(center_x - rx);
            let right = to_int(center_x + rx);
            let top = []; let bottom = [];
            for x in left..=right {
                let nx = clamp((to_float(x) - center_x) / rx, -1.0, 1.0);
                let dy = sqrt(1.0 - nx * nx) * ry;
                top.push(to_int(center_y - dy + 0.5));
                bottom.push(to_int(center_y + dy + 0.5));
            }
            let fg = palette(t + to_float(ring), beat, (1.0 - phase) * (0.12 + beat * 0.45 + bass * 0.12));
            for ys in [top, bottom] {
                cmds.push(#{ op: "trace", x: left, ys: ys, ch: ".", line_ch: ".",
                    fg: fg, line_fg: fg, bg: bg });
            }
        }
    }

    // Particles launch from the eight corners with staggered lifetimes.
    // Speed/age depend on time; audio changes reach and brightness instead
    // of rescaling time and teleporting particles on each audio update.
    if drive > 0.04 {
        let particles = if small { 16 } else { 48 };
        for i in 0..particles {
            let fi = to_float(i);
            let age = (t * (0.27 + to_float(i % 5) * 0.025) + fi * 0.618034) % 1.0;
            let angle = fi * 2.39996 + t * 0.16;
            let dx = cos(angle); let dy = sin(angle);
            let origin = points[i % 8];
            let travel = span * (0.35 + energy * 0.55 + beat * 0.8);
            let distance = age * travel;
            let light = sin(age * 3.14159) * (0.15 + drive * 0.85);
            let fg = palette(t + fi * 0.13, beat, light);
            let ch = if beat > 0.45 && i % 3 == 0 { "*" } else if treble > 0.35 { "+" } else { "." };
            if !small && i % 2 == 0 {
                let tail = distance - span * 0.04;
                cmds.dot(to_float(origin[0]) + dx * tail * 2.0,
                    to_float(origin[1]) + dy * tail, ".", palette(t, beat, light * 0.3), bg, w, h);
            }
            cmds.dot(to_float(origin[0]) + dx * distance * 2.0,
                to_float(origin[1]) + dy * distance, ch, fg, bg, w, h);
        }
    }

    // Analytic motion echoes. These are recomputed poses, not frame history.
    if !small && drive > 0.04 {
        for echo in [2, 1] {
            let lag = to_float(echo) * (0.14 + beat * 0.10);
            let ghost = mesh(t - lag, energy, bass, mid, treble, beat, w, h);
            let fg = palette(t + to_float(echo), 1.0 - beat, (0.08 + drive * 0.16) / to_float(echo));
            for e in edges {
                let a = ghost[e[0]]; let b = ghost[e[1]];
                cmds.push(edge(a[0], a[1], b[0], b[1], fg, bg));
            }
        }
    }

    // A warm inner cage gives depth and a second pulse inside the main mesh.
    if !small {
        let inner = [];
        let size = 0.35 + beat * 0.25;
        for p in points {
            inner.push([to_int(center_x + (to_float(p[0]) - center_x) * size + 0.5),
                to_int(center_y + (to_float(p[1]) - center_y) * size + 0.5)]);
        }
        let fg = rgb(100.0 + beat * 155.0, 45.0 + energy * 75.0, 65.0 + treble * 70.0);
        for e in edges {
            let a = inner[e[0]]; let b = inner[e[1]];
            cmds.push(edge(a[0], a[1], b[0], b[1], fg, bg));
        }
    }

    let ordered = [];
    for e in edges {
        let depth = (points[e[0]][2] + points[e[1]][2]) * 0.5;
        let at = 0;
        while at < ordered.len() && ordered[at][2] > depth { at += 1; }
        ordered.insert(at, [e[0], e[1], depth]);
    }
    for e in ordered {
        let a = points[e[0]]; let b = points[e[1]];
        let light = clamp(0.78 - e[2] * 0.14 + beat * 0.12, 0.3, 1.0);
        cmds.push(edge(a[0], a[1], b[0], b[1], palette(t, beat, light), bg));
    }
    // Sparks travel along edges; no more than twelve extra cells.
    if treble > 0.08 || beat > 0.08 {
        for i in 0..12 {
            let e = edges[i]; let a = points[e[0]]; let b = points[e[1]];
            let phase = (t * 0.8 + to_float(i) * 0.137) % 1.0;
            cmds.dot(to_float(a[0]) + to_float(b[0] - a[0]) * phase,
                to_float(a[1]) + to_float(b[1] - a[1]) * phase,
                "*", 0xffe8c0, bg, w, h);
        }
    }
    for p in points {
        if !small && beat > 0.45 {
            let glow = palette(t, beat, beat * 0.45);
            for offset in [[-1,0], [1,0], [0,-1], [0,1]] {
                cmds.dot(to_float(p[0] + offset[0]), to_float(p[1] + offset[1]), "+", glow, bg, w, h);
            }
        }
        cmds.push(#{ op: "cell", x: p[0], y: p[1], ch: "o", fg: 0xe8f6ff, bg: bg });
    }
    if input.show_clock {
        cmds.push(#{ op: "text", x: 1, y: 0, text: input.clock, fg: 0xa6cde0, bg: bg });
    }
    if input.paused {
        cmds.push(#{ op: "text", x: w - 7, y: 0, text: "paused", fg: 0xa6cde0, bg: bg });
    }
    if w >= 16 {
        cmds.push(#{ op: "text", x: 1, y: h - 1, text: "MORPHING CUBE", fg: 0x527b98, bg: bg });
    }
    cmds
}
