// name: Pulsing sphere
// protocol: furumi-visualizer-v2
// bundle-version: 5
//
// A second bundled example with a very different structure from
// scope_spectrum.rhai: instead of a waveform-first visual, it renders a
// hollow terminal "sphere" whose radius, halo and palette respond to audio.
//
// Script entry point:
//   fn render(input) -> Array<Map>
//
// Input map fields supplied by Furumi:
//   width, height              terminal size in cells
//   time, position, progress   animation time and playback progress
//   volume, paused             player state
//   energy, bass, mid, treble  smoothed audio bands in the 0.0..1.0 range
//   beat                       transient pulse estimate in the 0.0..1.0 range
//   seed                       stable per-track number in the 0.0..1.0 range
//   samples                    recent mono waveform samples in the -1.0..1.0 range
//   show_clock, clock          user setting and formatted clock text
//   track_title, track_artist  metadata strings
//
// Useful numeric helpers available to scripts:
//   to_int, to_float           Rhai conversions
//   sin, cos, tan, sqrt, abs, pow
// Scripts cannot import modules; keep every visualization self-contained.
//
// Draw command maps returned from render(input):
//   #{ op: "clear", bg: 0x000000 }
//   #{ op: "cell", x: 10, y: 4, ch: "*", fg: 0x33ffee, bg: 0x000000 }
//   #{ op: "hline", x: 0, y: 4, w: 20, ch: "-", fg: 0x33ffee, bg: 0x000000 }
//   #{ op: "vline", x: 10, y: 0, h: 12, ch: "|", fg: 0x33ffee, bg: 0x000000 }
//   #{ op: "rect", x: 10, y: 6, w: 2, h: 4, ch: "#", fg: 0x33ffee, bg: 0x000000 }
//   #{ op: "trace", x: 0, ys: [4, 5, 3], ch: "*", line_ch: "|", fg: 0xffffff, line_fg: 0x558888, bg: 0x000000 }
//   #{ op: "text", x: 2, y: 1, text: "hello", fg: 0xffffff, bg: 0x000000 }
//
// This script intentionally uses every command type. The sphere itself stays
// hollow: trace commands draw its spectral outline and orbit, hline draws
// latitudes, rect/vline draw the small energy meter, cells create stars and
// burst nodes, and labels use text.

fn clamp(value, low, high) {
    if value < low {
        low
    } else if value > high {
        high
    } else {
        value
    }
}

fn min(left, right) {
    if left < right { left } else { right }
}

fn max(left, right) {
    if left > right { left } else { right }
}

fn rgb(r, g, b) {
    let rr = to_int(clamp(r, 0, 255));
    let gg = to_int(clamp(g, 0, 255));
    let bb = to_int(clamp(b, 0, 255));
    rr * 65536 + gg * 256 + bb
}

fn lerp(left, right, mix) {
    left * (1.0 - mix) + right * mix
}

fn sample_at(samples, nx) {
    let len = samples.len();
    if len <= 0 {
        0.0
    } else {
        let pos = clamp(nx, 0.0, 1.0) * (len - 1);
        let left = to_int(pos);
        let right = if left + 1 < len { left + 1 } else { left };
        let mix = pos - to_float(left);
        lerp(samples[left], samples[right], mix)
    }
}

fn cell(x, y, ch, fg, bg) {
    #{ op: "cell", x: x, y: y, ch: ch, fg: fg, bg: bg }
}

fn hline(x, y, w, ch, fg, bg) {
    #{ op: "hline", x: x, y: y, w: w, ch: ch, fg: fg, bg: bg }
}

fn vline(x, y, h, ch, fg, bg) {
    #{ op: "vline", x: x, y: y, h: h, ch: ch, fg: fg, bg: bg }
}

fn rect(x, y, w, h, ch, fg, bg) {
    #{ op: "rect", x: x, y: y, w: w, h: h, ch: ch, fg: fg, bg: bg }
}

fn trace(x, ys, ch, line_ch, fg, line_fg, bg) {
    #{ op: "trace", x: x, ys: ys, ch: ch, line_ch: line_ch, fg: fg, line_fg: line_fg, bg: bg }
}

fn text(x, y, value, fg, bg) {
    #{ op: "text", x: x, y: y, text: value, fg: fg, bg: bg }
}

// A time-varying RGB palette. "shade" is usually 0.0..1.0; larger values are
// allowed and clamped by rgb(). Audio bands shift the hue without Rust knowing
// anything about the visual.
fn palette(input, shade, phase) {
    let t = input.time + input.seed * 8.0 + phase;
    let bass_push = input.bass * 70.0;
    let treble_push = input.treble * 65.0;
    rgb(
        50 + shade * 130 + sin(t * 0.90) * 55 + bass_push,
        70 + shade * 115 + sin(t * 0.63 + 2.1) * 55 + input.mid * 55,
        95 + shade * 145 + cos(t * 0.72 + 0.7) * 55 + treble_push
    )
}

fn wrap01(value) {
    let out = value;
    while out < 0.0 {
        out += 1.0;
    }
    while out > 1.0 {
        out -= 1.0;
    }
    out
}

fn spectral_band(input, phase) {
    if phase < 0.34 {
        input.bass
    } else if phase < 0.68 {
        input.mid
    } else {
        input.treble
    }
}

// Treat the waveform as if it was wrapped around the sphere. The returned
// value is a radial spike amount: low frequencies push broad parts of the
// contour, raw samples add sharp teeth, and beat makes the edge jump outward.
fn edge_spectrum(input, phase) {
    let p = wrap01(phase);
    let wave = abs(sample_at(input.samples, p));
    let neighbor = abs(sample_at(input.samples, wrap01(p + 0.021)));
    let band = spectral_band(input, p);
    let flutter = abs(sin(input.time * (5.0 + p * 7.0) + p * 38.0 + input.seed * 9.0));
    let fine = abs(cos(input.time * (8.0 + p * 4.0) - p * 71.0));
    clamp(
        wave * (0.62 + input.energy * 0.36)
            + neighbor * 0.24
            + band * 0.46
            + input.beat * (0.30 + flutter * 0.64)
            + fine * input.energy * 0.18,
        0.0,
        1.55
    )
}

fn push_starfield(cmds, input, bg) {
    let stars = 34;
    let index = 0;
    while index < stars {
        let x = (index * 37 + to_int(input.seed * 1000.0)) % max(input.width, 1);
        let y = (index * 17 + to_int(input.time * 2.0)) % max(input.height, 1);
        let speed = 0.7 + to_float(index % 5) * 0.15;
        let twinkle = abs(sin(input.time * speed + to_float(index)));
        let fg = rgb(
            45 + twinkle * 120 + input.treble * 80,
            75 + twinkle * 120,
            105 + twinkle * 130
        );
        let ch = if twinkle + input.beat > 1.25 { "*" } else { "." };
        cmds.push(cell(x, y, ch, fg, bg));
        index += 1;
    }
    cmds
}

fn push_orbit(cmds, input, cx, cy, rx, ry, bg) {
    let width = max(rx * 2 + 9, 3);
    let start_x = max(cx - width / 2, 0);
    let max_w = input.width - start_x;
    let actual_w = min(width, max_w);
    let back = [];
    let front = [];
    let i = 0;
    while i < actual_w {
        let nx = to_float(i) / to_float(max(actual_w - 1, 1));
        let angle = nx * 6.28318 + input.time * 0.85;
        let sample = sample_at(input.samples, nx);
        let wobble = (sin(angle + input.bass * 2.0) + sample * 0.28)
            * to_float(max(ry, 1))
            * (0.28 + input.energy * 0.10);
        let y_front = to_int(clamp(
            to_float(cy) + wobble + input.beat * 1.5,
            0.0,
            to_float(input.height - 1)
        ));
        let y_back = to_int(clamp(
            to_float(cy) - wobble - input.beat * 1.5,
            0.0,
            to_float(input.height - 1)
        ));
        front.push(y_front);
        back.push(y_back);
        i += 1;
    }

    let back_fg = palette(input, 0.18, 3.4);
    let front_fg = palette(input, 0.65 + input.beat * 0.25, 0.5);
    cmds.push(trace(start_x, back, ".", ".", back_fg, back_fg, bg));
    cmds.push(trace(start_x, front, "*", ".", front_fg, front_fg, bg));
    cmds
}

fn push_pulse_ring(cmds, input, cx, cy, rx, ry, bg) {
    let ring_rx = rx + 2 + to_int(input.beat * 5.0 + input.energy * 2.0);
    let ring_ry = ry + 1 + to_int(input.beat * 3.0 + input.bass * 2.0);
    let left = max(cx - ring_rx, 0);
    let right = min(cx + ring_rx, input.width - 1);
    let width = right - left + 1;
    if width <= 2 {
        return cmds;
    }

    let top = [];
    let bottom = [];
    let i = 0;
    while i < width {
        let nx = if width > 1 { to_float(i) / to_float(width - 1) * 2.0 - 1.0 } else { 0.0 };
        let phase = to_float(i) / to_float(max(width - 1, 1));
        let inside = max(1.0 - nx * nx, 0.0);
        let spike = edge_spectrum(input, phase) * (1.0 + input.energy * 3.0 + input.beat * 2.0);
        let y = sqrt(inside) * (to_float(ring_ry) + spike);
        top.push(to_int(clamp(to_float(cy) - y, 0.0, to_float(input.height - 1))));
        bottom.push(to_int(clamp(to_float(cy) + y, 0.0, to_float(input.height - 1))));
        i += 1;
    }

    let color = palette(input, 0.14 + input.beat * 0.35, 4.8);
    cmds.push(trace(left, top, ".", ".", color, color, bg));
    cmds.push(trace(left, bottom, ".", ".", color, color, bg));
    cmds
}

fn push_sphere(cmds, input, cx, cy, rx, ry, bg) {
    let left = max(cx - rx, 0);
    let right = min(cx + rx, input.width - 1);
    let width = right - left + 1;
    if width <= 2 {
        return cmds;
    }

    let top = [];
    let bottom = [];
    let glow_top = [];
    let glow_bottom = [];
    let i = 0;
    while i < width {
        let nx = if width > 1 { to_float(i) / to_float(width - 1) * 2.0 - 1.0 } else { 0.0 };
        let inside = max(1.0 - nx * nx, 0.0);
        let phase = to_float(i) / to_float(max(width - 1, 1));
        let top_spike = edge_spectrum(input, phase) * (1.0 + input.energy * 3.4 + input.beat * 3.0);
        let bottom_spike = edge_spectrum(input, 1.0 - phase) * (1.0 + input.energy * 3.4 + input.beat * 3.0);
        let edge = sqrt(inside) * to_float(ry);
        let top_y = to_int(clamp(to_float(cy) - edge - top_spike, 0.0, to_float(input.height - 1)));
        let bottom_y = to_int(clamp(to_float(cy) + edge + bottom_spike, 0.0, to_float(input.height - 1)));
        let glow = 1 + to_int(max(top_spike, bottom_spike) * 0.35 + input.beat * 2.0);

        top.push(top_y);
        bottom.push(bottom_y);
        glow_top.push(clamp(top_y - glow, 0, input.height - 1));
        glow_bottom.push(clamp(bottom_y + glow, 0, input.height - 1));
        i += 1;
    }

    let glow = palette(input, 0.22 + input.energy * 0.22, 3.1);
    let outline = palette(input, 0.82 + input.beat * 0.45, 0.3);
    let bridge = palette(input, 0.42 + input.mid * 0.25, 2.2);
    cmds.push(trace(left, glow_top, ".", ".", glow, glow, bg));
    cmds.push(trace(left, glow_bottom, ".", ".", glow, glow, bg));
    cmds.push(trace(left, top, "*", ".", outline, bridge, bg));
    cmds.push(trace(left, bottom, "*", ".", outline, bridge, bg));

    let equator = palette(input, 0.50 + input.bass * 0.25, 5.6);
    cmds.push(hline(left, cy, width, "-", equator, bg));
    if ry > 4 {
        let latitude = palette(input, 0.35 + input.treble * 0.20, 1.8);
        let span = max(width - width / 3, 3);
        let lat_x = left + (width - span) / 2;
        let lat_y = max(cy - ry / 2, 0);
        cmds.push(hline(lat_x, lat_y, span, ".", latitude, bg));
        cmds.push(hline(lat_x, min(cy + ry / 2, input.height - 1), span, ".", latitude, bg));
    }

    let nodes = 42;
    let node = 0;
    while node < nodes {
        let phase = to_float(node) / to_float(nodes);
        let angle = phase * 6.28318 + input.time * (0.10 + input.treble * 0.08);
        let spectrum = edge_spectrum(input, phase);
        let burst = spectrum * (1.4 + input.energy * 4.2 + input.beat * 3.5);
        let x = to_int(clamp(to_float(cx) + cos(angle) * (to_float(rx) + burst), 0.0, to_float(input.width - 1)));
        let y = to_int(clamp(to_float(cy) + sin(angle) * (to_float(ry) + burst * 0.58), 0.0, to_float(input.height - 1)));
        let fg = palette(input, 0.48 + spectrum * 0.36 + input.beat * 0.30, angle);
        let ch = if spectrum > 1.05 { "*" } else if spectrum > 0.72 { "+" } else { "." };
        cmds.push(cell(x, y, ch, fg, bg));
        if spectrum > 1.18 {
            let spike_x = to_int(clamp(to_float(cx) + cos(angle) * (to_float(rx) + burst + 1.5), 0.0, to_float(input.width - 1)));
            let spike_y = to_int(clamp(to_float(cy) + sin(angle) * (to_float(ry) + (burst + 1.5) * 0.58), 0.0, to_float(input.height - 1)));
            cmds.push(cell(spike_x, spike_y, ".", fg, bg));
        }
        node += 1;
    }

    cmds
}

fn push_meter(cmds, input, bg) {
    if input.height < 5 || input.width < 20 {
        return cmds;
    }
    let y = input.height - 2;
    let w = min(input.width - 4, 48);
    let x = 2;
    let base = rgb(18, 34, 42);
    let fill = palette(input, 0.65 + input.energy * 0.35, 5.2);
    let filled = to_int(clamp(input.energy * to_float(w), 1.0, to_float(w)));
    cmds.push(hline(x, y, w, "-", base, bg));
    cmds.push(rect(x, y, filled, 1, " ", fill, fill));
    let progress = to_int(clamp(input.progress * to_float(w), 0.0, to_float(w)));
    if progress > 0 {
        cmds.push(hline(x, y - 1, progress, ".", fill, bg));
        cmds.push(vline(min(x + progress, x + w - 1), y - 1, 2, "|", fill, bg));
    }
    cmds
}

fn push_labels(cmds, input, bg) {
    let fg = rgb(180 + input.treble * 60, 230, 245);
    let dim = rgb(70, 120 + input.energy * 80, 130 + input.energy * 80);
    if input.show_clock {
        cmds.push(text(1, 0, " " + input.clock + " ", fg, rgb(2, 18, 24)));
    }
    if input.track_title != "" && input.height > 4 {
        cmds.push(text(2, input.height - 1, input.track_title, fg, bg));
    }
    if input.track_artist != "" && input.height > 5 {
        cmds.push(text(2, input.height - 3, input.track_artist, dim, bg));
    }
    if input.paused {
        cmds.push(text(2, 1, "paused", dim, bg));
    }
    cmds
}

fn render(input) {
    let cmds = [];
    let bg = rgb(1, 4, 10);
    cmds.push(#{ op: "clear", bg: bg });

    if input.width <= 8 || input.height <= 6 {
        return cmds;
    }

    cmds = push_starfield(cmds, input, bg);

    let cx = input.width / 2;
    let cy = input.height / 2;
    let max_ry = max((input.height - 6) / 2, 2);
    let max_rx = max(input.width / 3, 4);
    let base = min(to_float(max_ry), to_float(max_rx) / 2.0) * 0.78;
    let activity = clamp(input.energy * 0.72 + input.bass * 0.50 + input.beat * 1.00, 0.0, 1.18);
    let pulse = 0.34
        + activity * 0.58
        + input.volume * 0.04
        + input.beat * 0.18
        + abs(sin(input.time * 2.8 + input.position * 0.07)) * (0.03 + input.energy * 0.07);
    let ry = max(to_int(base * pulse), 2);
    let rx = max(to_int(to_float(ry) * (1.85 + input.treble * 0.28)), 4);

    cmds = push_pulse_ring(cmds, input, cx, cy, rx, ry, bg);
    cmds = push_orbit(cmds, input, cx, cy, rx, ry, bg);
    cmds = push_sphere(cmds, input, cx, cy, rx, ry, bg);
    cmds = push_meter(cmds, input, bg);
    cmds = push_labels(cmds, input, bg);
    cmds
}
