// name: Scope spectrum
// protocol: furumi-visualizer-v2
// bundle-version: 5
//
// This bundled script is both a preset and an API example. It demonstrates
// every drawing command currently understood by Furumi's visualizer runtime:
// clear, cell, hline, vline, rect, trace and text.
//
// Script entry point:
//   fn render(input) -> Array<Map>
//
// Important input fields:
//   width, height              terminal size in cells
//   time, position, progress   animation time and playback progress
//   volume, paused             player state
//   energy, bass, mid, treble  smoothed audio bands in the 0.0..1.0 range
//   beat                       transient pulse estimate in the 0.0..1.0 range
//   samples                    recent mono waveform samples in the -1.0..1.0 range
//   show_clock, clock          user setting and formatted clock text
//   track_title, track_artist  metadata for optional labels
//
// Useful numeric helpers available to scripts:
//   to_int, to_float           Rhai conversions
//   sin, cos, tan, sqrt, abs, pow
// Scripts cannot import modules; keep every visualization self-contained.
//
// Return value: an array of command maps. Colors are 0xRRGGBB integers.
//   #{ op: "clear", bg: 0x000000 }
//   #{ op: "cell", x: 10, y: 4, ch: "*", fg: 0x33ffee, bg: 0x000000 }
//   #{ op: "hline", x: 0, y: 4, w: 20, ch: "-", fg: 0x33ffee, bg: 0x000000 }
//   #{ op: "vline", x: 10, y: 0, h: 12, ch: "|", fg: 0x33ffee, bg: 0x000000 }
//   #{ op: "rect", x: 10, y: 6, w: 2, h: 4, ch: "#", fg: 0x33ffee, bg: 0x000000 }
//   #{ op: "trace", x: 0, ys: [4, 5, 3], ch: "*", line_ch: "|", fg: 0xffffff, line_fg: 0x558888, bg: 0x000000 }
//   #{ op: "text", x: 2, y: 1, text: "hello", fg: 0xffffff, bg: 0x000000 }
//
// Performance tip: prefer hline/vline/rect/trace for large shapes and reserve
// cell for isolated details. Crossing the Rhai -> Rust boundary once per cell
// is much slower than returning one bulk command.

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

fn draw_grid(width, height, progress, energy, bg) {
    let cmds = [];
    let grid = rgb(22, 48 + energy * 30, 54 + energy * 35);
    let center = height / 2;

    let x = 0;
    while x < width {
        cmds.push(vline(x, 0, height, ".", grid, bg));
        x += 12;
    }

    let y = 0;
    while y < height {
        cmds.push(hline(0, y, width, ".", grid, bg));
        y += 4;
    }

    cmds.push(hline(0, center, width, "-", grid, bg));

    let cross_x = 0;
    while cross_x < width {
        let cross_y = 0;
        while cross_y < height {
            cmds.push(cell(cross_x, cross_y, "+", grid, bg));
            cross_y += 4;
        }
        cmds.push(cell(cross_x, center, "+", grid, bg));
        cross_x += 12;
    }

    let scan_x = to_int(progress * to_float(width - 1));
    let scan = rgb(25, 120 + energy * 80, 130 + energy * 80);
    cmds.push(vline(scan_x, 0, height, "|", scan, bg));
    cmds
}

fn draw_scope(input, scope_height, bg) {
    let cmds = [];
    let width = input.width;
    if width <= 0 || scope_height <= 0 {
        return cmds;
    }
    let center = to_float(scope_height - 1) / 2.0;
    let half = center;
    let fg = rgb(55 + input.beat * 80, 230, 210 + input.treble * 45);
    let bridge = rgb(20, 120 + input.energy * 70, 115 + input.energy * 60);
    let amplitude = 0.74 + input.energy * 0.55 + input.beat * 0.10;
    let ys = [];
    let x = 0;
    while x < width {
        let nx = if width > 1 { to_float(x) / to_float(width - 1) } else { 0.0 };
        let sample = sample_at(input.samples, nx) * amplitude;
        let y = to_int(clamp(center - sample * half, 0.0, to_float(scope_height - 1)));
        ys.push(y);
        x += 1;
    }
    cmds.push(trace(0, ys, "*", "|", fg, bridge, bg));
    cmds
}

fn spectrum_value(freq, input) {
    let band = if freq < 0.28 {
        input.bass
    } else if freq < 0.68 {
        input.mid
    } else {
        input.treble
    };
    let wobble = abs(sin(input.time * (1.7 + freq * 5.0) + input.seed * 17.0 + freq * 31.0));
    let harmonic = abs(cos(input.time * 0.63 - freq * 23.0));
    clamp(0.06 + band * (0.30 + wobble * 0.48 + harmonic * 0.18) + input.beat * (1.0 - freq) * 0.35, 0.0, 1.0)
}

fn draw_bars(input, top, height, bg) {
    let cmds = [];
    if height <= 0 {
        return cmds;
    }
    let width = input.width;
    let bar_width = if width >= 80 { 2 } else { 1 };
    let bars = width / bar_width;
    let bar = 0;
    while bar < bars {
        let freq = to_float(bar) / to_float(bars);
        let value = spectrum_value(freq, input);
        let bar_height = to_int(clamp(value * to_float(height), 1.0, to_float(height)));
        let fg = rgb(60 + (1.0 - freq) * 80, 160 + value * 95, 210 - freq * 110);
        let x = bar * bar_width;
        let y = top + height - bar_height;
        let w = if bar_width > 1 && x + 1 < width { 2 } else { 1 };
        if bar_height > 0 {
            cmds.push(rect(x, y, w, bar_height, "#", fg, bg));
        }
        bar += 1;
    }
    cmds
}

fn append_all(cmds, more) {
    for cmd in more {
        cmds.push(cmd);
    }
    cmds
}

fn track_label(input) {
    if input.track_artist != "" && input.track_title != "" {
        input.track_artist + " - " + input.track_title
    } else if input.track_title != "" {
        input.track_title
    } else {
        input.track_artist
    }
}

fn draw_track_badge(input, bg) {
    let cmds = [];
    let label = track_label(input);
    if label == "" || input.width <= 8 || input.height <= 4 {
        return cmds;
    }

    let max_text = input.width - 6;
    let value = if label.len() > max_text {
        label.sub_string(0, max_text)
    } else {
        label
    };
    let badge_width = min(value.len() + 4, input.width);
    let x = (input.width - badge_width) / 2;
    let y = input.height - 1;
    let badge_bg = rgb(0, 30 + input.energy * 36, 38 + input.beat * 42);
    let badge_fg = rgb(190 + input.treble * 45, 255, 235);

    cmds.push(rect(x, y, badge_width, 1, " ", badge_fg, badge_bg));
    cmds.push(text(x + 2, y, value, badge_fg, badge_bg));
    cmds
}

fn render(input) {
    let cmds = [];
    let bg = rgb(0, 0, 0);
    cmds.push(#{ op: "clear", bg: bg });

    if input.width <= 0 || input.height <= 2 {
        return cmds;
    }

    let content_height = input.height - 2;
    let bars_height = if content_height > 12 { 8 } else { content_height / 3 };
    let separator = if bars_height > 0 { 1 } else { 0 };
    let scope_height = content_height - bars_height - separator;

    cmds = append_all(cmds, draw_grid(input.width, scope_height, input.progress, input.energy, bg));
    cmds = append_all(cmds, draw_scope(input, scope_height, bg));

    if bars_height > 0 {
        let sep_y = scope_height;
        let sep = rgb(20, 68, 74);
        cmds.push(hline(0, sep_y, input.width, "-", sep, bg));
        cmds = append_all(cmds, draw_bars(input, sep_y + separator, bars_height, bg));
    }

    if input.show_clock {
        let clock_bg = rgb(0, 26, 30);
        let clock_fg = rgb(180, 255, 245);
        cmds.push(text(1, 0, " " + input.clock + " ", clock_fg, clock_bg));
    }

    cmds = append_all(cmds, draw_track_badge(input, bg));

    cmds
}
