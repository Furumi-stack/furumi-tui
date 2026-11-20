// name: VFD panel
// protocol: furumi-visualizer-v3
// bundle-version: 1
//
// A hi-fi front panel in the style of a vacuum fluorescent display: thin
// segment digits, a stereo peak level meter, a third-octave analyzer with
// peak hold, tone fans, a routing diagram and transport lamps. Everything is
// drawn thin (eighth blocks and braille dots); only level bars are thicker.
//
// It uses the v3 inputs (see below) and keeps a little state between frames
// for its effects: a power-on segment test, a "search" spin of the counter on
// track changes, an OVER lamp that holds and a blinking pause point.
//
// Script entry point:
//   fn render(input) -> Array<Map>                  (stateless)
//   fn render(input) -> #{ commands: Array<Map>, state: Map }
//
// Input fields (v2):
//   width, height                 terminal size in cells
//   time, position, progress      animation time and playback progress
//   volume, paused                player state
//   energy, bass, mid, treble     smoothed bands 0.0..1.0 (scaled by volume)
//   beat                          transient pulse estimate 0.0..1.0
//   seed                          stable per-track number 0.0..1.0
//   samples                       recent mono waveform -1.0..1.0
//   show_clock, clock             user setting and clock text
//   track_title, track_artist     metadata strings
// Added in v3:
//   spectrum, spectrum_peaks      31 third-octave bands (spectrum_hz: 20 Hz ..
//                                 20 kHz), 0.0..1.0 over spectrum_floor_db..0
//                                 dBFS, with analyzer fall and peak hold
//   level_l, level_r              channel RMS on the same scale
//   peak_l, peak_r                held channel sample peaks on the same scale
//   sample_rate                   analyzed sample rate in Hz (0 = none)
//   duration, track_number, disc_number, year, release_title
//   audio_format, audio_bitrate, audio_sample_rate, audio_bit_depth
//   source                        "local", "federated", "podcast" or ""
//   playing                       a track is loaded and not paused
//   queue_index, queue_length     1-based position (0 = none) and length
//   shuffle, repeat               bool and "off" / "one" / "all"
//   dt, frame                     seconds since the last frame, frame count
//   state                         what the last frame returned as `state`
//                                 (empty when the visualizer opens or the
//                                 script changes)
//
// Draw commands (v3 adds dots, line and text attributes):
//   #{ op: "clear", bg }
//   #{ op: "cell" | "text" | "hline" | "vline" | "rect" | "trace", ... }
//   #{ op: "dots", points: [x0, y0, x1, y1, ...], fg }   braille dots; a cell
//                                                        is 2 x 4 dots
//   #{ op: "line", x0, y0, x1, y1, fg }                  thin braille line
//   bold / dim / italic / underline / reversed: true     on any command
//
// The file is yours to edit; a newer Furumi only replaces it while it still
// carries an older "bundle-version" line.

// ---------- helpers ----------

fn clamp(v, lo, hi) { if v < lo { lo } else if v > hi { hi } else { v } }
fn mn(a, b) { if a < b { a } else { b } }
fn mx(a, b) { if a > b { a } else { b } }

fn rgb(r, g, b) {
    to_int(clamp(r, 0, 255)) * 65536 + to_int(clamp(g, 0, 255)) * 256 + to_int(clamp(b, 0, 255))
}

fn pad2(n) { if n < 10 { "0" + n } else { "" + n } }

fn get(input, key, fallback) { if key in input { input[key] } else { fallback } }

// Called in method style (cmds.txt(...)) so commands append in place.
// Negative coordinates are dropped: Furumi rejects them.
fn txt(x, y, s, fg) {
    if x >= 0 && y >= 0 && s != "" {
        this.push(#{ op: "text", x: x, y: y, text: s, fg: fg, bg: 0 });
    }
}
fn hl(x, y, w, ch, fg) {
    if x >= 0 && y >= 0 && w > 0 { this.push(#{ op: "hline", x: x, y: y, w: w, ch: ch, fg: fg, bg: 0 }); }
}
fn vl(x, y, h, ch, fg) {
    if x >= 0 && y >= 0 && h > 0 { this.push(#{ op: "vline", x: x, y: y, h: h, ch: ch, fg: fg, bg: 0 }); }
}
fn fill(x, y, w, h, ch, fg) {
    if x >= 0 && y >= 0 && w > 0 && h > 0 {
        this.push(#{ op: "rect", x: x, y: y, w: w, h: h, ch: ch, fg: fg, bg: 0 });
    }
}
// Braille line in dot coordinates (2 dots per cell across, 4 down).
fn ln(x0, y0, x1, y1, fg) {
    if x0 >= 0 && y0 >= 0 && x1 >= 0 && y1 >= 0 {
        this.push(#{ op: "line", x0: x0, y0: y0, x1: x1, y1: y1, fg: fg, bg: 0 });
    }
}

// ---------- palette (lit / ghost of unlit segments) ----------

fn white() { rgb(232, 244, 240) }
fn white_mid() { rgb(140, 152, 150) }
fn white_ghost() { rgb(22, 27, 27) }
fn amber() { rgb(255, 176, 36) }
fn amber_hot() { rgb(255, 224, 130) }
fn amber_ghost() { rgb(40, 25, 6) }
fn amber_text() { rgb(208, 138, 28) }
fn red() { rgb(255, 58, 38) }
fn red_ghost() { rgb(46, 11, 8) }
fn cyan() { rgb(84, 234, 216) }
fn cyan_mid() { rgb(44, 146, 136) }
fn cyan_ghost() { rgb(8, 32, 30) }

// ---------- thin segment font ----------
// Segments are eighth-block lines on cell edges: horizontals "▁" at the
// bottom of a row, verticals "▕" / "▏" on the inner edges of the outer
// columns. A glyph is cw cells wide and 2*rh+1 rows tall.

fn seg_mask(ch) {
    let s = switch ch {
        "0" => "abcdef", "1" => "bc", "2" => "abged", "3" => "abgcd",
        "4" => "fgbc", "5" => "afgcd", "S" => "afgcd", "6" => "afgedc",
        "7" => "abc", "8" => "abcdefg", "9" => "abcdfg", "P" => "abefg",
        "L" => "def", "A" => "abcefg", "Y" => "bcdfg", "U" => "bcdef",
        "E" => "adefg", "t" => "defg", "o" => "cdeg", "H" => "bcefg",
        "C" => "adef", "r" => "eg", "n" => "ceg", "d" => "bcdeg",
        "-" => "g", _ => ""
    };
    let m = 0;
    for c in s.chars() {
        m = m | switch c { 'a' => 1, 'b' => 2, 'c' => 4, 'd' => 8, 'e' => 16, 'f' => 32, 'g' => 64, _ => 0 };
    }
    m
}

fn pick(mask, bit, on, off) { if (mask & bit) != 0 { on } else { off } }

fn seg(x, y, mask, cw, rh, on, off) {
    let inner = cw - 2;
    this.hl(x + 1, y, inner, "▁", pick(mask, 1, on, off));
    this.vl(x, y + 1, rh, "▕", pick(mask, 32, on, off));
    this.vl(x + cw - 1, y + 1, rh, "▏", pick(mask, 2, on, off));
    this.hl(x + 1, y + rh, inner, "▁", pick(mask, 64, on, off));
    this.vl(x, y + rh + 1, rh, "▕", pick(mask, 16, on, off));
    this.vl(x + cw - 1, y + rh + 1, rh, "▏", pick(mask, 4, on, off));
    this.hl(x + 1, y + 2 * rh, inner, "▁", pick(mask, 8, on, off));
}

// Text in the segment font. "." is a point, " " a blank digit. `point_on`
// lets the caller blink points. Returns the x after the last glyph.
fn seg_text(x, y, s, cw, rh, on, off, point_on) {
    let cx = x;
    for ch in s.chars() {
        if ch == '.' {
            this.txt(cx, y + 2 * rh, "▖", if point_on { on } else { off });
            cx += 2;
        } else {
            if ch != ' ' { this.seg(cx, y, seg_mask("" + ch), cw, rh, on, off); }
            cx += cw + 1;
        }
    }
    cx
}

fn seg_width(s, cw) {
    let w = 0;
    for ch in s.chars() { if ch == '.' { w += 2; } else { w += cw + 1; } }
    w - 1
}

// ---------- measurements ----------

// Normalized meter value -> dBFS.
fn dbfs(input, v) { get(input, "spectrum_floor_db", -60.0) * (1.0 - v) }

// Display range of the analyzer: ordinary music sits around -45..-15 dBFS
// per third-octave band; the panel shows -50..-10 dBFS.
fn band_display(input, v) { clamp((dbfs(input, v) + 50.0) / 40.0, 0.0, 1.0) }

// The 25 panel bands 63 Hz .. 16 kHz are spectrum entries 5..29.
fn panel_bands(input) {
    let s = get(input, "spectrum", []);
    let p = get(input, "spectrum_peaks", []);
    let out = [];
    for i in 5..30 {
        let v = if i < s.len() { s[i] } else { 0.0 };
        let pk = if i < p.len() { p[i] } else { 0.0 };
        out.push([band_display(input, v), band_display(input, pk)]);
    }
    out
}

fn band_label(i) {
    switch i { 0 => "63", 4 => "160", 8 => "400", 12 => "1k", 16 => "2.5k", 20 => "6.3k", 24 => "16k", _ => "·" }
}

fn band_hz(i) {
    let hz = [63, 80, 100, 125, 160, 200, 250, 315, 400, 500, 630, 800, 1000, 1250, 1600,
              2000, 2500, 3150, 4000, 5000, 6300, 8000, 10000, 12500, 16000];
    hz[clamp(i, 0, 24)]
}

// ---------- panels ----------

fn draw_counter(input, st, x, y) {
    let secs = clamp(to_int(input.position), 0, 99 * 60 + 59);
    let text = "" + (secs / 60) + "." + pad2(secs % 60);
    if st.spin > 0.0 {
        // track change: digits "search" like an AMS counter
        let f = input.frame;
        text = "" + (f % 10) + "." + ((f * 7) % 10) + "" + ((f * 3) % 10);
    }
    let lit = if input.paused { white_mid() } else { white() };
    let off = if st.boot > 0.0 { lit } else { white_ghost() };
    let blink = !input.paused || (to_int(input.time * 2.0) % 2 == 0);
    let end = this.seg_text(x, y, text, 6, 2, lit, off, blink);
    let n = get(input, "track_number", 0);
    let q = get(input, "queue_index", 0);
    let tn = if n > 0 { n } else { q };
    let ts = if tn <= 0 { "-" } else if tn > 99 { "--" } else { "" + tn };
    let te = this.seg_text(end + 3, y, ts, 6, 2, lit, off, false);
    this.txt(x, y + 6, "TIME", white_mid());
    this.txt(end + 3, y + 6, "TRACK", white_mid());
    te
}

// Width kept for the counter so the meter never moves: "88.88" and a
// two-digit track number.
fn counter_width() { 29 + 3 + 13 }

// Deck peak meter scale: 0 on the scale is -10 dBFS.
fn meter_pos(db) {
    // The first mark is the host floor (-60 dBFS) on this scale.
    let marks = [-50.0, -20.0, -15.0, -10.0, -7.0, -5.0, -3.0, -1.0, 0.0, 1.0, 3.0, 5.0, 8.0];
    let d = db + 10.0;
    if d <= marks[0] { return 0.0; }
    for i in 1..marks.len() {
        if d <= marks[i] {
            return to_float(i - 1) + (d - marks[i - 1]) / (marks[i] - marks[i - 1]);
        }
    }
    to_float(marks.len() - 1)
}

fn draw_meter(input, st, x, y, w) {
    let labels = ["-∞", "-20", "-15", "-10", "-7", "-5", "-3", "-1", "0", "+1", "+3", "+5", "+8"];
    let n = 12.0;
    this.txt(x + 4, y, "PEAK LEVEL METER", white());
    this.txt(x + w - 4, y, "0VU", white());
    this.txt(x + w - 4, y + 1, "▔▔▔", white_mid());
    let bx = x + 3;
    let mw = w - 4;
    let hot = to_int(8.0 / n * to_float(mw));
    let rows = [["L", get(input, "level_l", 0.0), get(input, "peak_l", 0.0), y + 2],
                ["R", get(input, "level_r", 0.0), get(input, "peak_r", 0.0), y + 4]];
    for r in rows {
        this.txt(x, r[3], r[0], white());
        let ghost = if st.boot > 0.0 { white_mid() } else { white_ghost() };
        this.hl(bx, r[3], hot, "▃", ghost);
        this.hl(bx + hot, r[3], mw - hot, "▃", if st.boot > 0.0 { red() } else { red_ghost() });
        let cells = if r[1] > 0.0 { clamp(to_int(meter_pos(dbfs(input, r[1])) / n * to_float(mw) + 0.5), 0, mw) } else { 0 };
        this.hl(bx, r[3], mn(cells, hot), "▃", white());
        if cells > hot { this.hl(bx + hot, r[3], cells - hot, "▃", red()); }
        if r[2] > 0.0 {
            let pk = clamp(to_int(meter_pos(dbfs(input, r[2])) / n * to_float(mw)), 0, mw - 1);
            this.txt(bx + pk, r[3], "▃", if pk >= hot { red() } else { white() });
        }
    }
    this.txt(x, y + 3, "dB", white());
    // labels that would touch the previous one are left out on narrow meters
    let free = bx;
    for k in 0..13 {
        let lx = bx + to_int(to_float(k) / n * to_float(mw - 2)) - if k == 0 { 0 } else { 1 };
        if lx >= free && (k == 12 || lx + labels[k].len() < bx + mw) {
            this.txt(lx, y + 3, labels[k], if k >= 8 { red() } else { white() });
            free = lx + labels[k].len() + 1;
        }
    }
}

// Red lamps like a deck's tape-type indicators, here for the loudest range.
fn draw_lamps(input, st, bands, x, y) {
    let sums = [0.0, 0.0, 0.0];
    for i in 0..bands.len() {
        let g = if i < 7 { 0 } else if i < 17 { 1 } else { 2 };
        sums[g] += bands[i][0];
    }
    let avg = [sums[0] / 7.0, sums[1] / 10.0, sums[2] / 8.0];
    let best = 0;
    if avg[1] > avg[best] { best = 1; }
    if avg[2] > avg[best] { best = 2; }
    let names = ["LOW", "MID", "HIGH"];
    let cx = x;
    for i in 0..3 {
        let on = st.boot > 0.0 || (i == best && avg[best] > 0.05);
        this.txt(cx, y, names[i], if on { red() } else { red_ghost() });
        this.hl(cx, y + 1, names[i].len(), "▔", if on { red() } else { red_ghost() });
        cx += names[i].len() + 3;
    }
    let over = st.boot > 0.0 || st.over > 0.0;
    this.txt(x, y + 3, "OVER", if over { red() } else { red_ghost() });
}

fn lamp(x, y, label, on, lit, ghost) {
    this.txt(x, y, label, if on { lit } else { ghost });
    x + label.len() + 3
}

fn draw_status_row(input, st, x, y, w) {
    let boot = st.boot > 0.0;
    let repeat = get(input, "repeat", "off");
    let source = get(input, "source", "");
    let cx = x;
    cx = this.lamp(cx, y, "REPEAT", boot || repeat != "off", white(), white_ghost());
    if repeat == "one" || boot { this.txt(cx - 2, y, "1", white()); }
    cx = this.lamp(cx, y, "SHUFFLE", boot || get(input, "shuffle", false), white(), white_ghost());
    cx = this.lamp(cx, y, "LOCAL", boot || source == "local", white(), white_ghost());
    cx = this.lamp(cx, y, "FED", boot || source == "federated", white(), white_ghost());
    cx = this.lamp(cx, y, "PODCAST", boot || source == "podcast", white(), white_ghost());

    // format badge, like a noise-reduction lamp with its boxed letter
    let fmt = get(input, "audio_format", "").to_upper();
    let depth = get(input, "audio_bit_depth", 0);
    let badge = if fmt == "" { "PCM" } else { fmt };
    let bx = x + w - badge.len() - 8;
    this.txt(bx, y, badge, if fmt != "" || boot { white() } else { white_ghost() });
    let kbps = get(input, "audio_bitrate", 0);
    let box_ = if depth > 0 { "" + depth + "BIT" } else if kbps > 0 { "" + kbps + "K" } else { "--" };
    this.txt(bx + badge.len() + 2, y, "[" + box_ + "]", white());
}

fn draw_title(input, x, y, w) {
    if w <= 4 { return; }
    let label = if input.track_artist != "" && input.track_title != "" {
        input.track_artist + " — " + input.track_title
    } else if input.track_title != "" { input.track_title } else { "NO DISC" };
    label = label.to_upper();
    if label.len() <= w {
        this.txt(x, y, label, white());
    } else {
        let loop_ = label + "   ·   ";
        let off = to_int(input.time * 4.0) % loop_.len();
        this.txt(x, y, (loop_ + loop_).sub_string(off, w), white());
    }
}

// Segment analyzer: thin dashes stacked per band, a dark gap between bands.
fn draw_analyzer(input, st, bands, x, y, bw, h) {
    let n = bands.len();
    let bar = mx(bw - 1, 1);
    let right = x + 2 + n * bw;
    let scale = amber_ghost() + rgb(40, 30, 4);
    let red_row = y + h / 5;
    let free = x + 1;
    let r = 0;
    while r < h {
        let fg = if y + r == red_row { red() } else { scale };
        this.txt(x, y + r, "·", fg);
        this.txt(right, y + r, "·", fg);
        r += 1;
    }
    for i in 0..n {
        let bx = x + 2 + i * bw;
        let v = if st.boot > 0.0 { 1.0 } else { bands[i][0] };
        let lit = clamp(to_int(v * to_float(h) + 0.5), 0, h);
        if lit < h { this.fill(bx, y, bar, h - lit, "▂", amber_ghost()); }
        if lit > 1 { this.fill(bx, y + h - lit + 1, bar, lit - 1, "▂", amber()); }
        if lit > 0 { this.hl(bx, y + h - lit, bar, "▂", amber_hot()); }
        let cap = clamp(to_int(bands[i][1] * to_float(h) + 0.5), 0, h);
        if cap > lit && st.boot <= 0.0 { this.hl(bx, y + h - cap, bar, "▂", amber_hot()); }
        let label = band_label(i);
        let lx = mx(bx + (bar - label.len()) / 2, x + 1);
        if lx >= free {
            this.txt(lx, y + h + 1, label, amber_text());
            free = lx + label.len() + 1;
        }
    }
    right
}

fn draw_readout(input, st, bands, x, y) {
    let best = 0;
    for i in 1..bands.len() { if bands[i][0] > bands[best][0] { best = i; } }
    let live = st.boot > 0.0 || bands[best][0] > 0.08;
    let hz = band_hz(best);
    let digits = ""; let unit = "";
    if hz < 1000 { digits = "" + hz; unit = "HZ"; }
    else if hz < 10000 { digits = "" + (hz / 1000) + "." + ((hz % 1000) / 100); unit = "KHZ"; }
    else { digits = "" + (hz / 1000); unit = "KHZ"; }
    if !live { digits = "--"; }
    let on = amber();
    let off = if st.boot > 0.0 { amber() } else { amber_ghost() };
    let end = this.seg_text(x, y, digits, 6, 2, on, off, true);
    this.txt(end + 1, y + 4, unit, if live { amber() } else { amber_ghost() });
    this.txt(x, y + 7, "PEAK BAND", amber_text());
    this.txt(x + 12, y + 7, "HOLD", amber());
    let sr = get(input, "sample_rate", 0);
    let srt = if sr > 0 { "" + (sr / 1000) + "." + ((sr % 1000) / 100) + "k" } else { "--" };
    this.txt(x, y + 9, "RATE", amber_text());
    this.txt(x + 5, y + 9, "[" + srt + "]", amber());
}

// Bass / treble fans: thin lines converging on the 0 line; the line closest
// to the current balance of that range against the mids is lit.
fn draw_fans(input, bands, x, y, w) {
    let lo = (bands[1][0] + bands[2][0] + bands[3][0] + bands[4][0]) / 4.0;
    let mid = (bands[10][0] + bands[11][0] + bands[12][0] + bands[13][0]) / 4.0;
    let hi = (bands[19][0] + bands[20][0] + bands[21][0] + bands[22][0]) / 4.0;
    let bass_line = clamp(to_int(2.0 - (lo - mid) * 6.0 + 0.5), 0, 4);
    let treble_line = clamp(to_int(2.0 - (hi - mid) * 6.0 + 0.5), 0, 4);

    this.txt(x, y, "BASS", cyan());
    this.txt(x + w - 6, y, "TREBLE", cyan());
    let gy = y + 1;
    let gh = 4;                         // rows: +10 .. -10
    this.txt(x, gy, "+10", cyan_mid());
    this.txt(x, gy + 2, "  0", cyan_mid());
    this.txt(x, gy + gh - 1, "-10", cyan_mid());
    let X0 = (x + 4) * 2;
    let W = (w - 4) * 2;
    let Y0 = gy * 4 + 1;
    let H = gh * 4 - 3;
    let ymid = Y0 + H / 2;
    let half = W / 2;
    for i in 0..5 {
        let yy = Y0 + i * H / 4;
        let bass_fg = if i == bass_line { cyan() } else { cyan_ghost() };
        let treble_fg = if i == treble_line { cyan() } else { cyan_ghost() };
        this.ln(X0, yy, X0 + half * 6 / 10, ymid, bass_fg);
        this.ln(X0 + W - 1, yy, X0 + W - 1 - half * 6 / 10, ymid, treble_fg);
    }
    this.ln(X0 + half * 6 / 10, ymid, X0 + W - 1 - half * 6 / 10, ymid, cyan());
}

fn draw_balance(input, x, y, w) {
    let l = get(input, "level_l", 0.0);
    let r = get(input, "level_r", 0.0);
    let bal = if l + r > 0.01 { (r - l) / (l + r) } else { 0.0 };
    this.txt(x, y, "L", cyan());
    this.txt(x + w - 1, y, "R", cyan());
    let mid = x + w / 2;
    this.txt(mid - 3, y, "BALANCE", cyan());
    let d = x + 2;
    while d < mid - 4 { this.txt(d, y, "·", cyan_mid()); d += 2; }
    d = mid + 5;
    while d < x + w - 2 { this.txt(d, y, "·", cyan_mid()); d += 2; }
    let pos = clamp(mid + to_int(bal * to_float(w / 2 - 2) * 4.0), x + 2, x + w - 3);
    this.txt(pos, y + 1, "▮", cyan());
}

// Small speaker icon (box and cone) in braille, one cell wide plus the cone.
fn speaker(x, y, fg) {
    let X = x * 2; let Y = y * 4;
    this.ln(X, Y + 1, X, Y + 3, fg);
    this.ln(X + 1, Y + 1, X + 1, Y + 3, fg);
    this.ln(X + 1, Y + 1, X + 3, Y, fg);
    this.ln(X + 1, Y + 3, X + 3, Y + 4, fg);
    this.ln(X + 3, Y, X + 3, Y + 4, fg);
}

// Thin box around a label: "▁" above, "▔" below, "▕" / "▏" at the sides.
// Returns the x after the box. The box is 3 rows tall, the label on row y+1.
fn tbox(x, y, label, fg) {
    let w = label.len() + 4;
    this.hl(x + 1, y, w - 2, "▁", fg);
    this.txt(x, y + 1, "▕", fg);
    this.txt(x + 2, y + 1, label, fg);
    this.txt(x + w - 1, y + 1, "▏", fg);
    this.hl(x + 1, y + 2, w - 2, "▔", fg);
    x + w
}

fn draw_routing(input, st, x, y) {
    let playing = get(input, "playing", !input.paused && input.track_title != "");
    let lit = playing || st.boot > 0.0;
    let on = if lit { cyan() } else { cyan_mid() };
    let dim = if lit { cyan_mid() } else { cyan_ghost() };
    let fmt = get(input, "audio_format", "").to_upper();
    let vol = to_int(input.volume * 100.0 + 0.5);
    let src = switch get(input, "source", "") { "federated" => "NETWORK", "podcast" => "PODCAST", _ => "LIBRARY" };

    // source -> decoder -> volume
    let x1 = this.tbox(x, y, src, on);
    this.hl(x1, y + 1, 3, "─", dim);
    let x2 = this.tbox(x1 + 3, y, if fmt == "" { "DECODE" } else { fmt }, on);
    this.hl(x2, y + 1, 3, "─", dim);
    let x3 = this.tbox(x2 + 3, y, "VOL " + vol, on);

    // volume -> tone -> amplifier -> speakers A / B
    let ty = y + 4;
    let drop_x = x3 - 3;
    this.vl(drop_x, y + 3, ty - y - 2, "▏", dim);
    this.hl(x + 24, ty + 1, drop_x - x - 24, "─", dim);
    this.txt(drop_x, ty + 1, "┘", dim);
    let tone_end = this.tbox(x + 16, ty, "TONE", on);
    this.hl(tone_end, ty + 1, x + 24 - tone_end, "─", dim);

    // amplifier: a thin braille triangle pointing at the speakers
    let AX = (x + 6) * 2; let AY = (ty + 1) * 4 + 2;
    this.ln(AX, AY, AX + 16, AY - 6, on);
    this.ln(AX, AY, AX + 16, AY + 6, on);
    this.ln(AX + 16, AY - 6, AX + 16, AY + 6, on);
    this.txt(x + 10, ty + 1, "AMP", on);
    this.hl(x + 15, ty + 1, 1, "─", dim);
    this.speaker(x, ty, on);
    this.txt(x + 3, ty, "A", on);
    this.speaker(x, ty + 2, dim);
    this.txt(x + 3, ty + 2, "B", dim);
    this.ln((x + 2) * 2, ty * 4 + 2, AX, AY, dim);
    this.ln((x + 2) * 2, (ty + 2) * 4 + 2, AX, AY, dim);
    x3
}

fn draw_transport(input, st, x, y) {
    let word = if input.track_title == "" { "StoP" } else if input.paused { "PAUSE" } else { "PLAY" };
    let off = if st.boot > 0.0 { cyan() } else { cyan_ghost() };
    this.seg_text(x, y, word, 6, 2, cyan(), off, false);
    let ly = y + 6;
    let stereo = get(input, "level_l", 0.0) > 0.0 && (get(input, "level_l", 0.0) - get(input, "level_r", 0.0)).abs() > 0.001;
    this.txt(x, ly, "TRACK", cyan_mid());
    this.txt(x + 11, ly, "STEREO", if stereo || st.boot > 0.0 { cyan() } else { cyan_ghost() });
    this.txt(x + transport_width() - 5, ly, "VOL", cyan_mid());
    let q = get(input, "queue_index", 0);
    let qn = get(input, "queue_length", 0);
    let qs = if q <= 0 { "-" } else if q > 99 { "--" } else { "" + q };
    let end = this.seg_text(x, ly + 1, qs, 4, 1, cyan(), off, false);
    this.txt(end + 1, ly + 2, "/" + (if qn > 99 { "99+" } else { "" + qn }), cyan_mid());
    // volume right-aligned to the block's edge, "%" in the last column
    let vs = "" + to_int(input.volume * 100.0 + 0.5);
    let vx = x + transport_width() - 2 - (vs.len() * 5 - 1) - 1;
    this.seg_text(vx, ly + 1, vs, 4, 1, cyan(), off, false);
    this.txt(x + transport_width() - 2, ly + 2, "%", cyan());
}

// "PAUSE" in the segment font is the widest transport word.
fn transport_width() { 35 }

// ---------- state ----------

fn advance_state(input) {
    let st = input.state;
    if !("boot" in st) { st = #{ boot: 0.9, spin: 0.0, over: 0.0, title: input.track_title }; }
    let dt = get(input, "dt", 0.05);
    st.boot = st.boot - dt;
    st.spin = st.spin - dt;
    st.over = st.over - dt;
    if st.title != input.track_title {
        st.title = input.track_title;
        if input.track_title != "" { st.spin = 0.6; }
    }
    let hot = mx(get(input, "peak_l", 0.0), get(input, "peak_r", 0.0));
    if dbfs(input, hot) > -0.5 && hot > 0.0 { st.over = 1.0; }
    st
}

// ---------- layouts ----------

fn render_full(input, st) {
    let cmds = [];
    let W = input.width;
    let H = input.height;
    let cw = mn(W - 4, 170);
    let x0 = (W - cw) / 2;
    let bands = panel_bands(input);

    let spec_h = clamp(H - 27, 8, 14);
    let used = 7 + 1 + spec_h + 2 + 9;
    let gap = clamp((H - used) / 5, 1, 2);
    let y = clamp((H - used - gap * 4) / 2, 0, 3);

    let lamps_w = 18;
    cmds.draw_counter(input, st, x0, y);
    let meter_x = x0 + counter_width() + 4;
    cmds.draw_meter(input, st, meter_x, y, x0 + cw - lamps_w - 4 - meter_x);
    cmds.draw_lamps(input, st, bands, x0 + cw - lamps_w + 2, y);

    y += 7 + gap;
    cmds.draw_status_row(input, st, x0, y, cw);
    y += 1;
    cmds.txt(x0, y, "▸", white_mid());
    cmds.draw_title(input, x0 + 2, y, cw - 2);

    y += 1 + gap;
    let readout_w = 30;
    let bw = clamp((cw - readout_w - 6) / 25, 3, 5);
    let right = cmds.draw_analyzer(input, st, bands, x0, y, bw, spec_h);
    let rx = right + mx((x0 + cw - right - 22) / 2, 3);
    cmds.draw_readout(input, st, bands, rx, y + mx((spec_h - 10) / 2, 0));

    y += spec_h + 2 + gap;
    cmds.draw_balance(input, x0, y, 28);
    cmds.draw_fans(input, bands, x0, y + 3, 28);
    let routing_w = 40;
    let tr_w = transport_width();
    let rx2 = x0 + 32 + mx((cw - 32 - routing_w - tr_w) / 2, 0);
    cmds.draw_routing(input, st, rx2, y);
    cmds.draw_transport(input, st, x0 + cw - tr_w, y);

    if input.show_clock { cmds.txt(W - input.clock.len() - 1, 0, input.clock, white_mid()); }
    cmds
}

fn render_compact(input, st) {
    let cmds = [];
    let W = input.width;
    let H = input.height;
    let bands = panel_bands(input);
    let top = 0;
    let clock_w = if input.show_clock { input.clock.len() + 2 } else { 0 };
    if H >= 18 {
        let end = cmds.draw_counter(input, st, 1, 0);
        cmds.draw_title(input, end + 3, 2, W - end - 4);
        top = 8;
        if input.show_clock {
            // top right when the counter leaves room, else the free row
            // above the analyzer
            let cy = if end + clock_w <= W - 1 { 0 } else { 7 };
            cmds.txt(W - input.clock.len() - 1, cy, input.clock, white_mid());
        }
    } else {
        cmds.draw_title(input, 1, 0, W - 2 - clock_w);
        top = 2;
        if input.show_clock { cmds.txt(W - input.clock.len() - 1, 0, input.clock, white_mid()); }
    }
    let spec_h = mn(H - top - 2, 14);
    let bw = clamp((W - 4) / 25, 2, 5);
    if spec_h >= 3 { cmds.draw_analyzer(input, st, bands, 0, top, bw, spec_h); }
    cmds
}

fn render(input) {
    let st = advance_state(input);
    let cmds = [#{ op: "clear", bg: 0 }];
    if input.width >= 20 && input.height >= 6 {
        let body = if input.width >= 120 && input.height >= 34 { render_full(input, st) } else { render_compact(input, st) };
        for c in body { cmds.push(c); }
    }
    #{ commands: cmds, state: st }
}
