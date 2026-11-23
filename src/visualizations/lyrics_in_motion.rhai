// name: Lyrics in motion
// protocol: furumi-visualizer-v3
// bundle-version: 3
//
// Playback timestamps drive the entrance and highlight. The active glyphs
// are cached across frames and rebuilt when the terminal changes size.

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

fn stage(input, w, h, tint, arrival) {
    if w < 20 || h < 12 { return; }
    let band = max(1, h / 40);
    for y in range(3, h - 3, band) {
        let distance = abs(to_float(2 * y - h)) / to_float(h);
        let level = 0.04 + pow(1.0 - distance, 3.0) * (0.12 + input.bass * 0.12 + arrival * 0.08);
        this.push(#{ op: "rect", x: 0, y: y, w: w, h: min(band, h - 3 - y), bg: glow(tint, level) });
    }
    let dots = [];
    let shapes = ["✦", "✧", "⋄", "⟡", "+", "∙"];
    for i in 0..64 {
        let angle = to_float(i) * 2.399963 + input.time * 0.09;
        let radius = 0.48 + 0.46 * abs(sin(to_float(i) * 8.17 + input.time * 0.24));
        let x = w + to_int(cos(angle) * to_float(w - 2) * radius);
        let y = h * 2 + to_int(sin(angle) * to_float(h * 2 - 12) * radius);
        if abs(2 * x - 2 * w) > w || abs(y - h * 2) > h {
            if i % 5 == 0 {
                let twinkle = 0.5 + 0.5 * sin(input.time * 2.4 + to_float(i) * 1.83);
                let ch = shapes[(i / 5) % shapes.len()];
                let fg = glow(tint, 0.28 + twinkle * 0.38 + input.treble * 0.25);
                this.push(#{ op: "cell", x: x / 2, y: y / 4, ch: ch, fg: fg, preserve_bg: true });
            } else {
                dots.push(x); dots.push(y);
            }
        }
    }
    this.push(#{ op: "dots", points: dots, fg: glow(tint, 0.48 + input.treble * 0.40), preserve_bg: true });

    let samples = min(24, w / 4);
    for i in 0..samples {
        let wave = 0.5 + 0.5 * sin(input.time * 5.0 + to_float(i) * 0.67);
        let rise = min(max(1, h / 5 - 3), 1 + to_int(wave * (2.0 + 4.0 * input.bass + 3.0 * input.beat)));
        let x = 2 + i * (w / 2 - 3) / samples;
        let fg = glow(tint, 0.27 + wave * 0.25 + input.energy * 0.26);
        this.push(#{ op: "vline", x: x, y: h - 3 - rise, h: rise, ch: "▌", fg: fg, bg: 0x080b18 });
        this.push(#{ op: "vline", x: w - 1 - x, y: h - 3 - rise, h: rise, ch: "▐", fg: fg, bg: 0x080b18 });
    }
}

fn phrase(glyph, slot, total, mode, p, progress, y, w, h, tint, beat) {
    let drift = pow(1.0 - p, 3.0);
    let side = if slot % 2 == 0 { -1 } else { 1 };
    let height = glyph.len();
    let headline = height >= 10 || total == 1;
    for i in 0..height {
        let row = glyph[i];
        if row == "" { continue; }
        let width = cell_width(row);
        let space = w - width;
        let anchor = if total == 1 { space / 2 }
                     else if (slot + mode) % 2 == 0 { space / 5 }
                     else { space * 4 / 5 };
        let dx = if mode == 0 { side * to_int(drift * to_float(min(22, w / 5))) }
                 else if mode == 2 { to_int(drift * to_float(i * 2 - height)) }
                 else { 0 };
        let dy = if mode == 1 { to_int(drift * to_float(min(5, h / 6))) } else { 0 };
        let x = anchor + dx;
        let brightness = if headline { 1.0 } else { 0.72 };
        let shade = brightness * (1.0 - 0.27 * to_float(i) / to_float(height));
        // Reveal from opposing edges in the third entrance.
        let shown = if mode == 2 { max(1, to_int(to_float(width) * (1.0 - drift))) } else { width };
        let start = if mode == 2 && side > 0 { width - shown } else { 0 };
        let visible = cut_cells(row, start, shown);
        let lift = y + i + dy;
        this.text_at(visible, x + start + 1, lift, w, h, glow(tint, 0.40 + beat * 0.22));
        this.text_at(visible, x + start, lift, w, h, glow(tint, shade * (0.76 + 0.24 * p)));
        let sweep = to_int(to_float(width) * progress);
        this.text_at(cut_cells(row, start, max(0, min(shown, sweep - start))), x + start, lift,
                     w, h, glow(0xfff6ff, shade + 0.20));
    }
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
    let tint = if l.index % 2 == 0 { 0x93c5ff } else { 0xffa5da };
    let p = clamp(l.elapsed / 0.40, 0.0, 1.0);
    let arrival = if l.index >= 0 { pow(1.0 - p, 3.0) } else { 0.0 };
    commands.stage(input, w, h, tint, arrival);

    if h > 7 {
        let title_width = if input.show_clock && w >= 32 { w - 24 } else { w };
        let title = if cell_width(input.track_title) <= title_width { input.track_title }
                    else { cut_cells(input.track_title, 0, title_width - 1) + "…" };
        commands.center(title, 0, 1, w, h, glow(tint, 0.78));
        if input.show_clock && w >= 32 {
            commands.text_at(input.clock, w - 10, 1, w, h, glow(tint, 0.72));
        }
    }

    if l.index >= 0 {
        let font_width = w - 8;
        let font_height = if h >= 12 { min(32, h * 3 / 5) } else { 0 };
        if !("active" in st) || st.active != l.active || st.width != font_width || st.height != font_height {
            st.active = l.active;
            st.width = font_width;
            st.height = font_height;
            st.phrases = lyric_layout(l.active, font_width, font_height);
        }
        let total = st.phrases.len();
        let rows = max(1, total - 1);
        if total > 0 {
            rows = total - 1;
            for glyph in st.phrases { rows += glyph.len(); }
        }
        let top = (h - rows) / 2;
        let y = top;
        if total == 0 {
            commands.center(l.active, 0, y, w, h, 0xf6f3ff);
        }
        let mode = l.index % 3;
        let duration = if l.until_next >= 0.0 { l.elapsed + l.until_next } else { 2.0 };
        let entrance = clamp(duration * 0.28, 0.08, 0.60);
        for slot in 0..total {
            let glyph = st.phrases[slot];
            let phase = clamp(l.elapsed / entrance * 1.5 - 0.5 * to_float(slot), 0.0, 1.0);
            let progress = clamp(l.progress * to_float(total) - to_float(slot), 0.0, 1.0);
            commands.phrase(glyph, slot, total, mode, phase, progress, y, w, h, tint, input.beat);
            y += glyph.len() + 1;
        }
        if h >= 12 {
            let exit = 1.0 - arrival;
            let side = if mode == 1 { -1 } else { 1 };
            commands.center(l.previous, side * to_int(exit * to_float(w / 12)),
                            top - 2, w, h, glow(tint, 0.32 + arrival * 0.38));
            commands.center(l.next, 0, min(h - 3, top + rows + 3), w, h, glow(tint, 0.66));
        }
    } else {
        let message = if l.instrumental { "Instrumental" }
                      else if l.timed { "Waiting for the first line" }
                      else if l.available { "Plain lyrics · Ctrl+L to read" }
                      else if l.loading { "Looking for lyrics…" }
                      else { "No lyrics found" };
        commands.center(message, 0, h / 2, w, h, glow(tint, 0.82));
    }

    if h >= 10 && l.index >= 0 && l.until_next >= 0.0 && w >= 14 {
        let count = clamp(to_int(to_float(w - 8) * l.progress), 0, w - 8);
        commands.push(#{ op: "hline", x: 4, y: h - 2, w: w - 8, ch: "─", fg: 0x394568, bg: 0x080b18 });
        if count > 0 {
            commands.push(#{ op: "hline", x: 4, y: h - 2, w: count, ch: "━", fg: tint, bg: 0x080b18 });
            commands.text_at("◆", min(w - 5, 4 + count), h - 2, w, h, 0xfff6ff);
        }
    }
    #{ commands: commands, state: st }
}
