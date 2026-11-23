// name: Lyrics in motion
// protocol: furumi-visualizer-v3
// bundle-version: 1
//
// Incoming and outgoing text are driven by playback timestamps. Rhai caches
// the two dot-matrix rasters across frames and only draws sparse accents.

fn clamp(value, low, high) {
    if value < low { low } else if value > high { high } else { value }
}

fn rgb(r, g, b) { (r << 16) | (g << 8) | b }

fn glow(color, gain) {
    let amount = clamp(gain, 0.0, 1.0);
    rgb(to_int(to_float((color >> 16) & 255) * amount),
        to_int(to_float((color >> 8) & 255) * amount),
        to_int(to_float(color & 255) * amount))
}

fn text_at(text, x, y, width, height, fg) {
    if text == "" || y < 0 || y >= height || x >= width { return; }
    let left = if x < 0 { 0 } else { x };
    let text = cut_cells(text, left - x, width - left);
    if text != "" {
        this.push(#{ op: "text", x: left, y: y, text: text, fg: fg, bg: 0x080b18, bold: true });
    }
}

fn center(text, dx, y, w, h, fg) {
    let text = if cell_width(text) <= w { text } else { cut_cells(text, 0, w - 1) + "…" };
    this.text_at(text, (w - cell_width(text)) / 2 + dx, y, w, h, fg);
}

fn verse(text, glyph, dx, y, w, h, fg) {
    if glyph.len() == 2 {
        this.center(glyph[0], dx, y, w, h, fg);
        this.center(glyph[1], dx, y + 1, w, h, fg);
    } else {
        this.center(text, dx, y, w, h, fg);
    }
}

fn atmosphere(input, w, h, phase, tint) {
    if w < 20 || h < 8 { return; }
    let points = [];
    let stride = if w < 65 { 3 } else { 2 };
    let cx = w;
    let cy = h * 2;
    for i in 0..22 {
        let base = to_float(i) * 2.399963 + input.time * (0.13 + input.treble * 0.08);
        let spread = 0.66 + 0.26 * sin(to_float(i) * 18.67 + phase * 1.8);
        let x = cx + to_int(cos(base) * to_float(w - 6) * spread);
        let y = cy + to_int(sin(base) * to_float(h * 2 - 4) * spread);
        if abs(to_float(y - cy)) > 8.0 || abs(to_float(x - cx)) > to_float(w / 2) {
            points.push(x); points.push(y);
        }
    }
    this.push(#{ op: "dots", points: points, fg: glow(tint, 0.25 + input.beat * 0.28), bg: 0x080b18 });

    let rails = [];
    for i in 0..18 {
        let x = i * stride;
        if x + 4 < w {
            let wave = sin(input.time * 3.0 + to_float(i) * 0.76);
            let rise = to_int((1.0 + wave) * (2.0 + 6.0 * input.bass));
            rails.push(4 + x); rails.push(h * 4 - 8 - rise);
            rails.push(w * 2 - 5 - x); rails.push(h * 4 - 8 - rise);
        }
    }
    this.push(#{ op: "dots", points: rails, fg: glow(tint, 0.34 + input.energy * 0.30), bg: 0x080b18 });
}

fn render(input) {
    let w = input.width;
    let h = input.height;
    let commands = [#{ op: "clear", bg: 0x080b18 }];
    if w < 4 || h < 3 { return commands; }
    let l = if "lyrics" in input { input.lyrics } else {
        #{ available: false, timed: false, instrumental: false, loading: false,
           active: "", previous: "", next: "", index: -1, elapsed: 0.0, progress: 0.0, until_next: -1.0 }
    };
    let st = if type_of(input.state) == "map" { input.state } else { #{} };
    let tint = if l.index % 2 == 0 { 0xa8baff } else { 0xffb795 };
    let y = h / 2 - 1;
    let p = clamp(l.elapsed / 0.48, 0.0, 1.0);
    commands.atmosphere(input, w, h, p, tint);

    if h > 7 {
        commands.center(input.track_title, 0, 1, w, h, glow(tint, 0.62));
        if input.show_clock && w >= 24 {
            commands.text_at(input.clock, w - 10, 1, w, h, glow(tint, 0.55));
        }
    }
    if l.index >= 0 {
        let font_width = if h >= 10 { w - 6 } else { 0 };
        if !("active" in st) || st.active != l.active || st.width != font_width {
            st.active = l.active;
            st.width = font_width;
            st.glyph = matrix_text(l.active, font_width);
        }
        if p < 1.0 && l.previous != "" {
            if !("previous" in st) || st.previous != l.previous || st.prev_width != font_width {
                st.previous = l.previous;
                st.prev_width = font_width;
                st.prev_glyph = matrix_text(l.previous, font_width);
            }
            let exit = p * p;
            let side = if l.index % 2 == 0 { 1 } else { -1 };
            commands.verse(l.previous, st.prev_glyph, to_int(exit * to_float(w / 2)) * side,
                  y - 2 - to_int(exit * to_float(h / 2)), w, h, glow(tint, (1.0 - p) * 0.55));
        }

        // Alternate diagonal entries, a horizontal slide and a cut.
        let remaining = pow(1.0 - p, 3.0);
        let mode = l.index % 5;
        let dx = if mode == 0 || mode == 2 { -to_int(remaining * to_float(w / 3)) }
                 else if mode == 1 || mode == 4 { to_int(remaining * to_float(w / 3)) }
                 else { 0 };
        let dy = if mode == 0 { -to_int(remaining * to_float(h / 3)) }
                 else if mode == 1 { to_int(remaining * to_float(h / 3)) }
                 else if mode == 4 { -to_int(remaining * to_float(h / 4)) }
                 else { 0 };
        let intensity = if mode == 3 { 1.0 } else { 0.72 + p * 0.28 };
        commands.verse(l.active, st.glyph, dx, y + dy, w, h, glow(tint, intensity));
    } else {
        let message = if l.instrumental { "Instrumental" }
                      else if l.timed { "Waiting for the first line" }
                      else if l.available { "Plain lyrics · l to read" }
                      else if l.loading { "Looking for lyrics…" }
                      else { "No lyrics found" };
        commands.center(message, 0, y, w, h, glow(tint, 0.72));
    }

    if h >= 10 {
        if l.next != "" { commands.center(l.next, 0, y + 4, w, h, glow(tint, 0.38)); }
        if l.index >= 0 && l.until_next >= 0.0 && w >= 14 {
            let count = clamp(to_int(to_float(w - 8) * l.progress), 0, w - 8);
            commands.push(#{ op: "hline", x: 4, y: h - 2, w: w - 8, ch: "─", fg: 0x26304b, bg: 0x080b18 });
            if count > 0 {
                commands.push(#{ op: "hline", x: 4, y: h - 2, w: count, ch: "━", fg: glow(tint, 0.70), bg: 0x080b18 });
            }
        }
    }
    #{ commands: commands, state: st }
}
