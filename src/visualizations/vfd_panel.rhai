// name: VFD panel
// protocol: furumi-visualizer-v3
// bundle-version: 3
//
// A hi-fi front panel in the style of a vacuum fluorescent display: thin
// segment digits, a peak level meter (bars or needles), a third-octave
// analyzer with peak hold, tone fans, a routing diagram, transport lamps and
// a dot-matrix title line. Everything is drawn thin (eighth blocks and
// braille dots); only level bars are thicker.
//
// Effects, most of them keeping a little state between frames:
//   power-on segment test, counter "search" on track changes, OVER lamp with
//   hold, blinking pause point, slightly uneven digit brightness (an aged tube), a slow filament "breath", a
//   brighter panel on loud passages and a dimmer at night, a short burst of
//   static when the track or source changes, LOUDNESS / MUTE / SEARCH lamps,
//   a running tape counter, and a standby clock with a sweep every minute
//   when nothing is loaded.
//
// Options: edit the three functions below.
fn PALETTE() { "classic" }        // "classic" | "green" | "amber" | "ice"
fn METER_STYLE() { "bars" }       // "bars" | "needles"
fn NIGHT_DIMMER() { true }        // dim the panel from 22:00 to 07:00
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
//   show_clock, clock             user setting and local clock text
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
//   clock_hour, clock_minute, clock_second, clock_millis   local time
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
fn frac(x) { x - x.floor() }

fn rgb(r, g, b) {
    to_int(clamp(r, 0, 255)) * 65536 + to_int(clamp(g, 0, 255)) * 256 + to_int(clamp(b, 0, 255))
}

fn scale(c, f) {
    rgb(to_float((c >> 16) & 255) * f, to_float((c >> 8) & 255) * f, to_float(c & 255) * f)
}

// Blend toward white by `t` (0..1).
fn whiten(c, t) {
    let r = to_float((c >> 16) & 255); let g = to_float((c >> 8) & 255); let b = to_float(c & 255);
    rgb(r + (255.0 - r) * t, g + (255.0 - g) * t, b + (255.0 - b) * t)
}

fn pad2(n) { if n < 10 { "0" + n } else { "" + n } }
fn pad4(n) { let s = "" + n; while s.len() < 4 { s = "0" + s; } s }

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
fn dots(points, fg) {
    if points.len() > 1 { this.push(#{ op: "dots", points: points, fg: fg, bg: 0 }); }
}

// ---------- palette and brightness ----------
// Four colour zones like the filters over a real VFD: text, analyzer,
// auxiliary (tone / routing / transport) and warnings. Each frame the zones
// get one brightness: night dimmer x filament breath x loudness.

fn zone_bases() {
    switch PALETTE() {
        "green" => [rgb(120, 255, 214), rgb(120, 255, 214), rgb(120, 255, 214), rgb(255, 72, 52)],
        "amber" => [rgb(255, 184, 52), rgb(255, 176, 36), rgb(255, 184, 52), rgb(255, 64, 40)],
        "ice" => [rgb(214, 236, 255), rgb(150, 206, 255), rgb(120, 196, 255), rgb(255, 86, 86)],
        _ => [rgb(232, 244, 240), rgb(255, 176, 36), rgb(84, 234, 216), rgb(255, 58, 38)]
    }
}

fn night(input) {
    let h = get(input, "clock_hour", 12);
    NIGHT_DIMMER() && (h >= 22 || h < 7)
}

fn make_palette(input, boost) {
    let z = zone_bases();
    let dim = if night(input) { 0.72 } else { 1.0 };
    let breath = 0.955 + 0.045 * sin(input.time * 0.9);
    let loud = 1.0 + 0.07 * input.energy;
    let b = clamp(dim * breath * loud + boost, 0.3, 1.15);
    #{
        white: scale(z[0], b), white_mid: scale(z[0], 0.6 * b), white_ghost: scale(z[0], 0.095),
        amber: scale(z[1], b), amber_hot: whiten(scale(z[1], b), 0.45),
        amber_ghost: scale(z[1], 0.16), amber_text: scale(z[1], 0.8 * b),
        red: scale(z[3], b), red_ghost: scale(z[3], 0.18),
        cyan: scale(z[2], b), cyan_mid: scale(z[2], 0.55 * b), cyan_ghost: scale(z[2], 0.13),
        // unlit segments of the thin digits: fainter than the analyzer's,
        // so a lit digit never reads as an 8
        white_seg: scale(z[0], 0.055), amber_seg: scale(z[1], 0.075), cyan_seg: scale(z[2], 0.07),
        noise: scale(z[0], 0.32 * b)
    }
}

// ---------- thin segment font ----------
// Segments are thin lines on cell edges, all about equally thick: horizontals
// are eighth blocks ("▁", 1/8 of the cell height) along the bottom of a row,
// verticals are quarter blocks ("▎", 1/4 of the narrower cell width) along
// the left edge of the first and the last column. A glyph is cw cells wide
// (cw - 1 between its verticals) and 2*rh+1 rows tall.

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
    let inner = cw - 1;
    this.hl(x, y, inner, "▁", pick(mask, 1, on, off));
    this.vl(x, y + 1, rh, "▎", pick(mask, 32, on, off));
    this.vl(x + cw - 1, y + 1, rh, "▎", pick(mask, 2, on, off));
    this.hl(x, y + rh, inner, "▁", pick(mask, 64, on, off));
    this.vl(x, y + rh + 1, rh, "▎", pick(mask, 16, on, off));
    this.vl(x + cw - 1, y + rh + 1, rh, "▎", pick(mask, 4, on, off));
    this.hl(x, y + 2 * rh, inner, "▁", pick(mask, 8, on, off));
}

// An aged tube: every digit position is a little brighter or dimmer.
fn wear(x) { 0.84 + 0.16 * frac(abs(sin(to_float(x) * 12.9898)) * 43758.5453) }

// Text in the segment font. "." is a point, " " a blank digit. `point_on`
// lets the caller blink points. Returns the x after the last glyph.
fn seg_text(x, y, s, cw, rh, on, off, point_on) {
    let cx = x;
    for ch in s.chars() {
        if ch == '.' {
            this.txt(cx, y + 2 * rh, "▖", if point_on { scale(on, wear(cx)) } else { off });
            cx += 2;
        } else {
            if ch != ' ' { this.seg(cx, y, seg_mask("" + ch), cw, rh, scale(on, wear(cx)), off); }
            cx += cw + 1;
        }
    }
    cx
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

// Deck peak meter scale: 0 on the scale is -10 dBFS; the first mark is the
// host floor (-60 dBFS).
fn meter_pos(db) {
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

// Tape counter: counts take-up reel turns, so it runs a little slower as
// the reel fills.
fn tape_counter(position) {
    to_int(sqrt(mx(position, 0.0) * 30.0 + 400.0) * 20.0 - 400.0) % 10000
}

// ---------- panels ----------

fn draw_counter(input, st, x, y) {
    let p = st.pal;
    let secs = clamp(to_int(input.position), 0, 99 * 60 + 59);
    let text = "" + (secs / 60) + "." + pad2(secs % 60);
    if st.spin > 0.0 {
        // track change: digits "search" like an AMS counter
        let f = input.frame;
        text = "" + (f % 10) + "." + ((f * 7) % 10) + "" + ((f * 3) % 10);
    }
    let lit = if input.paused { p.white_mid } else { p.white };
    let off = if st.boot > 0.0 { lit } else { p.white_seg };
    let blink = !input.paused || (to_int(input.time * 2.0) % 2 == 0);
    let end = this.seg_text(x, y, text, 6, 2, lit, off, blink);
    let n = get(input, "track_number", 0);
    let q = get(input, "queue_index", 0);
    let tn = if n > 0 { n } else { q };
    let ts = if tn <= 0 { "-" } else if tn > 99 { "--" } else { "" + tn };
    let te = this.seg_text(end + 3, y, ts, 6, 2, lit, off, false);
    this.txt(x, y + 6, "TIME", p.white_mid);
    this.txt(end + 3, y + 6, "TRACK", p.white_mid);
    te
}

// Width kept for the counter so the meter never moves: "88.88" and a
// two-digit track number.
fn counter_width() { 29 + 3 + 13 }

fn draw_meter(input, st, x, y, w) {
    let p = st.pal;
    let labels = ["-∞", "-20", "-15", "-10", "-7", "-5", "-3", "-1", "0", "+1", "+3", "+5", "+8"];
    let n = 12.0;
    this.txt(x + 4, y, "PEAK LEVEL METER", p.white);
    this.txt(x + w - 4, y, "0VU", p.white);
    this.txt(x + w - 4, y + 1, "▔▔▔", p.white_mid);
    let bx = x + 3;
    let mw = w - 4;
    let hot = to_int(8.0 / n * to_float(mw));
    let rows = [["L", get(input, "level_l", 0.0), get(input, "peak_l", 0.0), y + 2],
                ["R", get(input, "level_r", 0.0), get(input, "peak_r", 0.0), y + 4]];
    for r in rows {
        this.txt(x, r[3], r[0], p.white);
        this.hl(bx, r[3], hot, "▃", if st.boot > 0.0 { p.white_mid } else { p.white_ghost });
        this.hl(bx + hot, r[3], mw - hot, "▃", if st.boot > 0.0 { p.red } else { p.red_ghost });
        let cells = if r[1] > 0.0 { clamp(to_int(meter_pos(dbfs(input, r[1])) / n * to_float(mw) + 0.5), 0, mw) } else { 0 };
        this.hl(bx, r[3], mn(cells, hot), "▃", p.white);
        if cells > hot { this.hl(bx + hot, r[3], cells - hot, "▃", p.red); }
        if r[2] > 0.0 {
            let pk = clamp(to_int(meter_pos(dbfs(input, r[2])) / n * to_float(mw)), 0, mw - 1);
            this.txt(bx + pk, r[3], "▃", if pk >= hot { p.red } else { p.white });
        }
    }
    this.txt(x, y + 3, "dB", p.white);
    // labels that would touch the previous one are left out on narrow meters
    let free = bx;
    for k in 0..13 {
        let lx = bx + to_int(to_float(k) / n * to_float(mw - 2)) - if k == 0 { 0 } else { 1 };
        if lx >= free && (k == 12 || lx + labels[k].len() < bx + mw) {
            this.txt(lx, y + 3, labels[k], if k >= 8 { p.red } else { p.white });
            free = lx + labels[k].len() + 1;
        }
    }
}

// Two needle VU meters drawn with braille: an arc scale with a red zone,
// ticks at the deck marks and a needle on the RMS level.
fn draw_needles(input, st, x, y, w) {
    let p = st.pal;
    this.txt(x + 4, y, "VU METER", p.white);
    this.txt(x + w - 4, y, "0VU", p.white);
    let mw = (w - 4) / 2;
    let chans = [["L", get(input, "level_l", 0.0), get(input, "peak_l", 0.0)],
                 ["R", get(input, "level_r", 0.0), get(input, "peak_r", 0.0)]];
    for k in 0..2 {
        let left = x + k * (mw + 4);
        let wd = mw * 2;                                // meter width in dots
        let top = (y + 1) * 4 + 1;                      // arc top in dots
        let bottom = (y + 5) * 4 + 3;                   // last dot row of the face
        let r = to_float(wd / 2 - 2) / 0.7071;          // radius for a +/-45 degree arc
        let cx = to_float(left * 2 + wd / 2);
        let cy = to_float(top) + r;                     // pivot below the face
        let arc = []; let red_arc = []; let ticks = [];
        let steps = mx(wd, 8);
        for i in 0..=steps {
            let pos = to_float(i) / to_float(steps) * 12.0;
            let a = (pos / 12.0 * 90.0 - 45.0) * 0.0174533;
            let px = to_int(cx + r * sin(a)); let py = to_int(cy - r * cos(a));
            if pos >= 8.0 { red_arc.push(px); red_arc.push(py); } else { arc.push(px); arc.push(py); }
        }
        for m in [0.0, 1.0, 3.0, 5.0, 8.0, 10.0, 12.0] {
            let a = (m / 12.0 * 90.0 - 45.0) * 0.0174533;
            let tx = to_int(cx + (r + 2.0) * sin(a)); let ty = to_int(cy - (r + 2.0) * cos(a));
            // keep the label row free
            if ty >= (y + 1) * 4 { ticks.push(tx); ticks.push(ty); }
        }
        this.dots(arc, if st.boot > 0.0 { p.white } else { p.white_mid });
        this.dots(red_arc, p.red);
        this.dots(ticks, p.white);
        let level = if chans[k][1] > 0.0 { meter_pos(dbfs(input, chans[k][1])) } else { 0.0 };
        if st.boot > 0.0 { level = 12.0 * (1.0 - st.boot / 0.9); }
        let a = (level / 12.0 * 90.0 - 45.0) * 0.0174533;
        let tip_x = cx + (r - 1.0) * sin(a); let tip_y = cy - (r - 1.0) * cos(a);
        // stop the needle at the bottom of the face instead of the pivot
        let rb = (cy - to_float(bottom)) / cos(a);
        let foot_x = cx + rb * sin(a);
        this.ln(to_int(tip_x), to_int(tip_y), to_int(foot_x), bottom, if level >= 8.0 { p.red } else { p.white });
        this.txt(left, y + 5, chans[k][0], p.white);
        let peak_on = chans[k][2] > 0.0 && meter_pos(dbfs(input, chans[k][2])) >= 8.0;
        this.txt(left + mw - 4, y + 5, "PEAK", if peak_on || st.boot > 0.0 { p.red } else { p.red_ghost });
    }
}

// Red lamps like a deck's tape-type indicators, here for the loudest range;
// below them the OVER lamp and the tape counter.
fn draw_lamps(input, st, bands, x, y) {
    let p = st.pal;
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
        this.txt(cx, y, names[i], if on { p.red } else { p.red_ghost });
        this.hl(cx, y + 1, names[i].len(), "▔", if on { p.red } else { p.red_ghost });
        cx += names[i].len() + 3;
    }
    let over = st.boot > 0.0 || st.over > 0.0;
    this.txt(x, y + 3, "OVER", if over { p.red } else { p.red_ghost });
    this.txt(x, y + 5, "CNT", p.white_mid);
    this.txt(x + 4, y + 5, if st.boot > 0.0 { "8888" } else { pad4(tape_counter(input.position)) }, p.white);
}

fn lamp(x, y, label, on, lit, ghost) {
    this.txt(x, y, label, if on { lit } else { ghost });
    x + label.len() + 3
}

fn draw_status_row(input, st, x, y, w) {
    let p = st.pal;
    let boot = st.boot > 0.0;
    let repeat = get(input, "repeat", "off");
    let source = get(input, "source", "");
    let vol = to_int(input.volume * 100.0 + 0.5);
    let cx = x;
    cx = this.lamp(cx, y, "REPEAT", boot || repeat != "off", p.white, p.white_ghost);
    if repeat == "one" || boot { this.txt(cx - 2, y, "1", p.white); }
    cx = this.lamp(cx, y, "SHUFFLE", boot || get(input, "shuffle", false), p.white, p.white_ghost);
    cx = this.lamp(cx, y, "LOCAL", boot || source == "local", p.white, p.white_ghost);
    cx = this.lamp(cx, y, "FED", boot || source == "federated", p.white, p.white_ghost);
    cx = this.lamp(cx, y, "PODCAST", boot || source == "podcast", p.white, p.white_ghost);
    cx += 2;
    // automatic modes: loudness at low volume, mute at zero, search on seeks
    cx = this.lamp(cx, y, "LOUDNESS", boot || (vol > 0 && vol <= 30), p.white, p.white_ghost);
    cx = this.lamp(cx, y, "MUTE", boot || vol == 0, p.red, p.red_ghost);
    let blink = to_int(input.time * 6.0) % 2 == 0;
    this.lamp(cx, y, "SEARCH", boot || (st.search > 0.0 && blink), p.white, p.white_ghost);

    // format badge, like a noise-reduction lamp with its boxed letter
    let fmt = get(input, "audio_format", "").to_upper();
    let depth = get(input, "audio_bit_depth", 0);
    let badge = if fmt == "" { "PCM" } else { fmt };
    let kbps = get(input, "audio_bitrate", 0);
    let box_ = if depth > 0 { "" + depth + "BIT" } else if kbps > 0 { "" + kbps + "K" } else { "--" };
    let bx = x + w - badge.len() - box_.len() - 4;
    if bx > cx + 6 {
        this.txt(bx, y, badge, if fmt != "" || boot { p.white } else { p.white_ghost });
        this.txt(bx + badge.len() + 2, y, "[" + box_ + "]", p.white);
    }
}

// Dot-matrix title line: 5x7 glyphs in braille (3 cells x 2 rows each),
// scrolling smoothly by dots when it is too long.
fn draw_matrix_title(input, st, x, y, w, label) {
    let p = st.pal;
    if w <= 3 { return; }
    let text = label.to_upper();
    let chars = [];
    for c in text.chars() {
        // combining marks, zero-width characters and variation selectors
        // have no cell of their own
        let u = to_int(c);
        if (u >= 0x300 && u <= 0x36F) || (u >= 0x1AB0 && u <= 0x1AFF) || (u >= 0x1DC0 && u <= 0x1DFF)
            || (u >= 0x20D0 && u <= 0x20FF) || (u >= 0x200B && u <= 0x200F) || (u >= 0xFE00 && u <= 0xFE0F) {
            continue;
        }
        chars.push(c);
    }
    let count = chars.len();
    let x0 = x * 2; let wd = w * 2; let y0 = y * 4;
    let total = count * 6;
    let gap = 24;
    let off = if total <= wd { 0 } else { to_int(input.time * 14.0) % (total + gap) };
    let pts = [];
    // during the gap between loops `i` stays at the end of the text
    let i = mn(off / 6, count);
    let px = x0 + i * 6 - off;
    // two passes over the text so the loop joins seamlessly
    let pass = 0;
    while px < x0 + wd && pass < 2 {
        if i >= count {
            if total <= wd { break; }
            px += gap; i = 0; pass += 1;
            continue;
        }
        let glyph = dm_glyph(chars[i]);
        for row in 0..7 {
            let bits = glyph[row];
            if bits == 0 { continue; }
            for col in 0..5 {
                if (bits & (16 >> col)) != 0 {
                    let dx = px + col;
                    if dx >= x0 && dx < x0 + wd { pts.push(dx); pts.push(y0 + row); }
                }
            }
        }
        px += 6; i += 1;
    }
    this.dots(pts, p.white);
}

fn title_label(input) {
    if input.track_artist != "" && input.track_title != "" {
        input.track_artist + " — " + input.track_title
    } else if input.track_title != "" { input.track_title } else { "NO DISC" }
}

// Segment analyzer: thin dashes stacked per band, a dark gap between bands.
fn draw_analyzer(input, st, bands, x, y, bw, h) {
    let p = st.pal;
    let n = bands.len();
    let bar = mx(bw - 1, 1);
    let right = x + 2 + n * bw;
    let scale_fg = p.amber_ghost + rgb(30, 22, 3);
    let red_row = y + h / 5;
    let free = x + 1;
    let r = 0;
    while r < h {
        let fg = if y + r == red_row { p.red } else { scale_fg };
        this.txt(x, y + r, "·", fg);
        this.txt(right, y + r, "·", fg);
        r += 1;
    }
    for i in 0..n {
        let bx = x + 2 + i * bw;
        let v = if st.boot > 0.0 { 1.0 } else { bands[i][0] };
        let lit = clamp(to_int(v * to_float(h) + 0.5), 0, h);
        if lit < h { this.fill(bx, y, bar, h - lit, "▂", p.amber_ghost); }
        if lit > 1 { this.fill(bx, y + h - lit + 1, bar, lit - 1, "▂", p.amber); }
        if lit > 0 { this.hl(bx, y + h - lit, bar, "▂", p.amber_hot); }
        let cap = clamp(to_int(bands[i][1] * to_float(h) + 0.5), 0, h);
        if cap > lit && st.boot <= 0.0 { this.hl(bx, y + h - cap, bar, "▂", p.amber_hot); }
        let label = band_label(i);
        let lx = mx(bx + (bar - label.len()) / 2, x + 1);
        if lx >= free {
            this.txt(lx, y + h + 1, label, p.amber_text);
            free = lx + label.len() + 1;
        }
    }
    right
}

fn draw_readout(input, st, bands, x, y) {
    let p = st.pal;
    let best = 0;
    for i in 1..bands.len() { if bands[i][0] > bands[best][0] { best = i; } }
    let live = st.boot > 0.0 || bands[best][0] > 0.08;
    let hz = band_hz(best);
    let digits = ""; let unit = "";
    if hz < 1000 { digits = "" + hz; unit = "HZ"; }
    else if hz < 10000 { digits = "" + (hz / 1000) + "." + ((hz % 1000) / 100); unit = "KHZ"; }
    else { digits = "" + (hz / 1000); unit = "KHZ"; }
    if !live { digits = "--"; }
    let off = if st.boot > 0.0 { p.amber } else { p.amber_seg };
    let end = this.seg_text(x, y, digits, 6, 2, p.amber, off, true);
    this.txt(end + 1, y + 4, unit, if live { p.amber } else { p.amber_ghost });
    this.txt(x, y + 7, "PEAK BAND", p.amber_text);
    this.txt(x + 12, y + 7, "HOLD", p.amber);
    let sr = get(input, "sample_rate", 0);
    let srt = if sr > 0 { "" + (sr / 1000) + "." + ((sr % 1000) / 100) + "k" } else { "--" };
    this.txt(x, y + 9, "RATE", p.amber_text);
    this.txt(x + 5, y + 9, "[" + srt + "]", p.amber);
}

// Bass / treble fans as thin stepped strokes: each range's line runs from its
// level at the edge (+10 .. -10) into the flat 0 line in the middle; only the
// line that matches the current balance against the mids is drawn.
fn draw_fans(input, st, bands, x, y, w) {
    let p = st.pal;
    let lo = (bands[1][0] + bands[2][0] + bands[3][0] + bands[4][0]) / 4.0;
    let mid = (bands[10][0] + bands[11][0] + bands[12][0] + bands[13][0]) / 4.0;
    let hi = (bands[19][0] + bands[20][0] + bands[21][0] + bands[22][0]) / 4.0;
    let bass_line = clamp(to_int(2.0 - (lo - mid) * 6.0 + 0.5), 0, 4);
    let treble_line = clamp(to_int(2.0 - (hi - mid) * 6.0 + 0.5), 0, 4);

    this.txt(x, y, "BASS", p.cyan);
    this.txt(x + w - 6, y, "TREBLE", p.cyan);
    let gy = y + 1;
    let gh = 4;                          // rows for +10 .. -10
    this.txt(x, gy, "+10", p.cyan_mid);
    this.txt(x, gy + gh / 2, "  0", p.cyan_mid);
    this.txt(x, gy + gh - 1, "-10", p.cyan_mid);
    let gx = x + 4;
    let gw = w - 4;
    // three thin strokes per row: top "▔", middle "─", bottom "▁"
    let levels = 3 * gh - 1;
    let zero = levels / 2;
    let ramp = gw * 3 / 10;
    let lines = [[bass_line, 0, 1], [treble_line, gw - 1, -1]];
    for l in lines {
        let start = l[0] * levels / 4;
        for c in 0..ramp {
            let t = to_float(c) / to_float(mx(ramp - 1, 1));
            let lv = to_int(to_float(start) + (to_float(zero) - to_float(start)) * t + 0.5);
            let ch = switch lv % 3 { 0 => "▔", 1 => "─", _ => "▁" };
            this.txt(gx + l[1] + l[2] * c, gy + lv / 3, ch, p.cyan);
        }
    }
    let flat = switch zero % 3 { 0 => "▔", 1 => "─", _ => "▁" };
    this.hl(gx + ramp, gy + zero / 3, gw - 2 * ramp, flat, p.cyan_mid);
}

fn draw_balance(input, st, x, y, w) {
    let p = st.pal;
    let l = get(input, "level_l", 0.0);
    let r = get(input, "level_r", 0.0);
    let bal = if l + r > 0.01 { (r - l) / (l + r) } else { 0.0 };
    this.txt(x, y, "L", p.cyan);
    this.txt(x + w - 1, y, "R", p.cyan);
    let mid = x + w / 2;
    this.txt(mid - 3, y, "BALANCE", p.cyan);
    let d = x + 2;
    while d < mid - 4 { this.txt(d, y, "·", p.cyan_mid); d += 2; }
    d = mid + 5;
    while d < x + w - 2 { this.txt(d, y, "·", p.cyan_mid); d += 2; }
    let pos = clamp(mid + to_int(bal * to_float(w / 2 - 2) * 4.0), x + 2, x + w - 3);
    this.txt(pos, y + 1, "▮", p.cyan);
}

// A label plate: a thin line above and below the label, no sides.
// Returns the x after the plate (3 rows tall, the label on row y+1).
fn plate(x, y, label, fg) {
    let w = label.len() + 2;
    this.hl(x, y, w, "▁", fg);
    this.txt(x + 1, y + 1, label, fg);
    this.hl(x, y + 2, w, "▔", fg);
    x + w
}

// Signal path like a receiver's routing display. The top row runs source ->
// decoder -> volume; a wire leaves the volume plate along its bottom edge,
// turns down at its right end and comes back along the top edge of the tone
// plate, so every corner sits on a cell edge and joins cleanly. The bottom
// row runs tone -> amplifier -> speakers.
fn draw_routing(input, st, x, y) {
    let p = st.pal;
    let playing = get(input, "playing", !input.paused && input.track_title != "");
    let lit = playing || st.boot > 0.0;
    let on = if lit { p.cyan } else { p.cyan_mid };
    let dim = if lit { p.cyan_mid } else { p.cyan_ghost };
    let fmt = get(input, "audio_format", "").to_upper();
    let vol = to_int(input.volume * 100.0 + 0.5);
    let src = switch get(input, "source", "") { "federated" => "NETWORK", "podcast" => "PODCAST", _ => "LIBRARY" };

    let x1 = this.plate(x, y, src, on);
    this.hl(x1 + 1, y + 1, 2, "─", dim);
    let x2 = this.plate(x1 + 4, y, if fmt == "" { "DECODE" } else { fmt }, on);
    this.hl(x2 + 1, y + 1, 2, "─", dim);
    let x3 = this.plate(x2 + 4, y, "VOL " + vol, on);

    // bottom row, right-aligned so the tone plate ends under the wire
    let ty = y + 4;
    let tone_x = x3 - 6;
    let amp_x = tone_x - 4 - 5;
    let speakers = if amp_x - 4 - 12 >= x { "SPEAKERS A" } else { "SPK A" };
    let sp_x = amp_x - 4 - (speakers.len() + 2);
    this.plate(sp_x, ty, speakers, on);
    this.hl(amp_x - 3, ty + 1, 2, "─", dim);
    this.plate(amp_x, ty, "AMP", on);
    this.hl(tone_x - 3, ty + 1, 2, "─", dim);
    this.plate(tone_x, ty, "TONE", on);

    // the wire: from the volume plate's bottom-right corner straight down to
    // the tone plate's top-right corner (both on the left edge of column x3)
    this.vl(x3, y + 2, ty - y - 1, "▎", dim);
    x3
}

fn draw_transport(input, st, x, y) {
    let p = st.pal;
    let word = if input.track_title == "" { "StoP" } else if input.paused { "PAUSE" } else { "PLAY" };
    let off = if st.boot > 0.0 { p.cyan } else { p.cyan_seg };
    this.seg_text(x, y, word, 6, 2, p.cyan, off, false);
    let ly = y + 6;
    let stereo = get(input, "level_l", 0.0) > 0.0 && (get(input, "level_l", 0.0) - get(input, "level_r", 0.0)).abs() > 0.001;
    this.txt(x, ly, "TRACK", p.cyan_mid);
    this.txt(x + 11, ly, "STEREO", if stereo || st.boot > 0.0 { p.cyan } else { p.cyan_ghost });
    this.txt(x + transport_width() - 5, ly, "VOL", p.cyan_mid);
    let q = get(input, "queue_index", 0);
    let qn = get(input, "queue_length", 0);
    let qs = if q <= 0 { "-" } else if q > 99 { "--" } else { "" + q };
    let end = this.seg_text(x, ly + 1, qs, 4, 1, p.cyan, off, false);
    this.txt(end + 1, ly + 2, "/" + (if qn > 99 { "99+" } else { "" + qn }), p.cyan_mid);
    // volume right-aligned to the block's edge, "%" in the last column
    let vs = "" + to_int(input.volume * 100.0 + 0.5);
    let vx = x + transport_width() - 2 - (vs.len() * 5 - 1) - 1;
    this.seg_text(vx, ly + 1, vs, 4, 1, p.cyan, off, false);
    this.txt(x + transport_width() - 2, ly + 2, "%", p.cyan);
}

// "PAUSE" in the segment font is the widest transport word.
fn transport_width() { 35 }

// Static on the display for a moment after the track or source changes.
fn draw_noise(input, st) {
    if st.noise <= 0.0 { return; }
    let W = input.width; let H = input.height;
    // capped so a huge terminal stays far below the operation limit
    let count = mn(to_int(to_float(W * H) * 0.35 * clamp(st.noise / 0.35, 0.0, 1.0)), 6000);
    let f = input.frame % 9973;
    let pts = [];
    for i in 0..count {
        let a = (i * 7919 + f * 104729 + 13) % 1000003;
        let b = (i * 15485863 % 999983 + f * 32452843 % 999979) % 1000033;
        pts.push(a % (W * 2)); pts.push(b % (H * 4));
    }
    this.dots(pts, st.pal.noise);
}

// ---------- state ----------

fn advance_state(input) {
    let st = input.state;
    // a track is its title, queue slot and seed together, so two songs with
    // the same title still count as a change
    let key = input.track_title + "|" + get(input, "queue_index", 0) + "|" + input.seed;
    if !("boot" in st) || !("key" in st) {
        st = #{ boot: 0.9, spin: 0.0, over: 0.0, noise: 0.0, search: 0.0,
                key: key, source: get(input, "source", ""), last_pos: input.position };
    }
    let dt = get(input, "dt", 0.05);
    st.boot -= dt; st.spin -= dt; st.over -= dt; st.noise -= dt; st.search -= dt;
    let source = get(input, "source", "");
    if st.key != key {
        st.key = key;
        if input.track_title != "" { st.spin = 0.6; }
        st.noise = 0.35;
    } else if input.track_title != "" {
        let expected = st.last_pos + if input.paused { 0.0 } else { dt };
        if input.position < 1.0 && st.last_pos > 1.5 {
            // the same track again (repeat one, "previous" restart)
            st.spin = 0.6;
        } else if abs(input.position - expected) > 1.5 {
            // a jump that playback alone cannot explain is a seek
            st.search = 0.8;
        }
    }
    if st.source != source { st.source = source; st.noise = 0.35; }
    st.last_pos = input.position;
    let hot = mx(get(input, "peak_l", 0.0), get(input, "peak_r", 0.0));
    if dbfs(input, hot) > -0.5 && hot > 0.0 { st.over = 1.0; }
    // the panel brightens while the power-on test runs
    st.pal = make_palette(input, if st.boot > 0.0 { 0.1 } else { 0.0 });
    st
}

// ---------- layouts ----------

fn render_full(input, st) {
    let cmds = [];
    let p = st.pal;
    let W = input.width;
    let H = input.height;
    let cw = mn(W - 4, 170);
    let x0 = (W - cw) / 2;
    let bands = panel_bands(input);

    let spec_h = clamp(H - 28, 8, 14);
    let used = 7 + 1 + 2 + spec_h + 2 + 9;
    let gap = clamp((H - used) / 5, 1, 2);
    // row 0 stays free for the optional clock
    let y = clamp((H - used - gap * 4) / 2, if input.show_clock { 1 } else { 0 }, 3);

    let lamps_w = 18;
    cmds.draw_counter(input, st, x0, y);
    let meter_x = x0 + counter_width() + 4;
    let meter_w = x0 + cw - lamps_w - 4 - meter_x;
    if METER_STYLE() == "needles" {
        cmds.draw_needles(input, st, meter_x, y, meter_w);
    } else {
        cmds.draw_meter(input, st, meter_x, y, meter_w);
    }
    cmds.draw_lamps(input, st, bands, x0 + cw - lamps_w + 2, y);

    y += 7 + gap;
    cmds.draw_status_row(input, st, x0, y, cw);
    y += 1;
    cmds.txt(x0, y, "▸", p.white_mid);
    cmds.draw_matrix_title(input, st, x0 + 2, y, cw - 2, title_label(input));

    y += 2 + gap;
    let readout_w = 30;
    let bw = clamp((cw - readout_w - 6) / 25, 3, 5);
    let right = cmds.draw_analyzer(input, st, bands, x0, y, bw, spec_h);
    let rx = right + mx((x0 + cw - right - 22) / 2, 3);
    cmds.draw_readout(input, st, bands, rx, y + mx((spec_h - 10) / 2, 0));

    y += spec_h + 2 + gap;
    cmds.draw_balance(input, st, x0, y, 28);
    cmds.draw_fans(input, st, bands, x0, y + 3, 28);
    let routing_w = 40;
    let tr_w = transport_width();
    let rx2 = x0 + 32 + mx((cw - 32 - routing_w - tr_w) / 2, 0);
    cmds.draw_routing(input, st, rx2, y);
    cmds.draw_transport(input, st, x0 + cw - tr_w, y);

    if input.show_clock { cmds.txt(W - input.clock.len() - 1, 0, input.clock, p.white_mid); }
    cmds
}

fn render_compact(input, st) {
    let cmds = [];
    let p = st.pal;
    let W = input.width;
    let H = input.height;
    let bands = panel_bands(input);
    let top = 0;
    let clock_w = if input.show_clock { input.clock.len() + 2 } else { 0 };
    if H >= 18 {
        let end = cmds.draw_counter(input, st, 1, 0);
        cmds.draw_matrix_title(input, st, end + 3, 2, W - end - 4, title_label(input));
        top = 8;
        if input.show_clock {
            // top right when the counter leaves room, else the free row
            // above the analyzer
            let cy = if end + clock_w <= W - 1 { 0 } else { 7 };
            cmds.txt(W - input.clock.len() - 1, cy, input.clock, p.white_mid);
        }
    } else {
        cmds.draw_matrix_title(input, st, 1, 0, W - 2 - clock_w, title_label(input));
        top = 3;
        if input.show_clock { cmds.txt(W - input.clock.len() - 1, 0, input.clock, p.white_mid); }
    }
    let spec_h = mn(H - top - 2, 14);
    let bw = clamp((W - 4) / 25, 2, 5);
    if spec_h >= 3 { cmds.draw_analyzer(input, st, bands, 0, top, bw, spec_h); }
    cmds
}

// Nothing loaded: a large clock with a blinking colon; at the top of every
// minute a brightness sweep runs across the digits.
fn render_standby(input, st) {
    let cmds = [];
    let p = st.pal;
    let W = input.width;
    let H = input.height;
    let h = get(input, "clock_hour", 0);
    let m = get(input, "clock_minute", 0);
    let s = get(input, "clock_second", 0);
    if W < 42 || H < 10 { return render_compact(input, st); }
    let big = W >= 70 && H >= 20;
    let cw = if big { 9 } else { 6 };
    let rh = if big { 4 } else { 2 };
    let digits = pad2(h) + pad2(m);
    let width = 4 * (cw + 1) + 3;
    let x = mx((W - width - 10) / 2, 0);
    let y = mx((H - (2 * rh + 1) - 6) / 2, 0);
    let ms = get(input, "clock_millis", 0);
    let sweep = if s < 2 { (to_float(s) + to_float(ms) / 1000.0) * 3.0 - 0.5 } else { -10.0 };
    let cx = x;
    for i in 0..4 {
        if i == 2 {
            let colon = if s % 2 == 0 { p.white } else { p.white_ghost };
            cmds.colon(cx, y, rh, colon);
            cx += 3;
        }
        let boost = clamp(1.0 - abs(to_float(i) - sweep), 0.0, 1.0);
        let on = whiten(p.white, 0.55 * boost);
        cmds.seg_text(cx, y, digits.sub_string(i, 1), cw, rh, on, p.white_seg, false);
        cx += cw + 1;
    }
    cmds.seg_text(cx + 1, y + 2 * rh - 2, pad2(s), 4, 1, p.white_mid, p.white_seg, false);
    let ly = y + 2 * rh + 2;
    cmds.txt(x, ly, "STANDBY", p.red);
    cmds.txt(x + 10, ly, if night(input) { "DIMMER" } else { "" }, p.white_mid);
    if W >= 40 {
        cmds.draw_matrix_title(input, st, x, ly + 2, mn(W - x - 2, width + 10), "FURUMI · NO DISC");
    }
    cmds
}

// The colon of the standby clock: two small squares between the hour and
// minute digits.
fn colon(x, y, rh, fg) {
    this.txt(x + 1, y + rh - 1, "▪", fg);
    this.txt(x + 1, y + rh + 1, "▪", fg);
}

fn render(input) {
    let st = advance_state(input);
    let cmds = [#{ op: "clear", bg: 0 }];
    if input.width >= 20 && input.height >= 6 {
        cmds.draw_noise(input, st);
        let body = if input.track_title == "" && st.boot <= 0.0 {
            render_standby(input, st)
        } else if input.width >= 120 && input.height >= 34 {
            render_full(input, st)
        } else {
            render_compact(input, st)
        };
        for c in body { cmds.push(c); }
    }
    #{ commands: cmds, state: st }
}

// ---------- dot-matrix font ----------

// 5x7 dot-matrix glyphs, one int per row with bit 4 as the leftmost dot.
// Latin, digits, common punctuation and Cyrillic; anything else is a box.
fn dm_glyph(c) {
    switch c {
        'A' => [14, 17, 17, 31, 17, 17, 17],
        'B' => [30, 17, 17, 30, 17, 17, 30],
        'C' => [14, 17, 16, 16, 16, 17, 14],
        'D' => [30, 17, 17, 17, 17, 17, 30],
        'E' => [31, 16, 16, 30, 16, 16, 31],
        'F' => [31, 16, 16, 30, 16, 16, 16],
        'G' => [14, 17, 16, 23, 17, 17, 15],
        'H' => [17, 17, 17, 31, 17, 17, 17],
        'I' => [14, 4, 4, 4, 4, 4, 14],
        'J' => [7, 2, 2, 2, 2, 18, 12],
        'K' => [17, 18, 20, 24, 20, 18, 17],
        'L' => [16, 16, 16, 16, 16, 16, 31],
        'M' => [17, 27, 21, 21, 17, 17, 17],
        'N' => [17, 17, 25, 21, 19, 17, 17],
        'O' => [14, 17, 17, 17, 17, 17, 14],
        'P' => [30, 17, 17, 30, 16, 16, 16],
        'Q' => [14, 17, 17, 17, 21, 18, 13],
        'R' => [30, 17, 17, 30, 20, 18, 17],
        'S' => [15, 16, 16, 14, 1, 1, 30],
        'T' => [31, 4, 4, 4, 4, 4, 4],
        'U' => [17, 17, 17, 17, 17, 17, 14],
        'V' => [17, 17, 17, 17, 17, 10, 4],
        'W' => [17, 17, 17, 21, 21, 21, 10],
        'X' => [17, 17, 10, 4, 10, 17, 17],
        'Y' => [17, 17, 10, 4, 4, 4, 4],
        'Z' => [31, 1, 2, 4, 8, 16, 31],
        '0' => [14, 17, 19, 21, 25, 17, 14],
        '1' => [4, 12, 4, 4, 4, 4, 14],
        '2' => [14, 17, 1, 2, 4, 8, 31],
        '3' => [31, 2, 4, 2, 1, 17, 14],
        '4' => [2, 6, 10, 18, 31, 2, 2],
        '5' => [31, 16, 30, 1, 1, 17, 14],
        '6' => [6, 8, 16, 30, 17, 17, 14],
        '7' => [31, 1, 2, 4, 8, 8, 8],
        '8' => [14, 17, 17, 14, 17, 17, 14],
        '9' => [14, 17, 17, 15, 1, 2, 12],
        ' ' => [0, 0, 0, 0, 0, 0, 0],
        '.' => [0, 0, 0, 0, 0, 12, 12],
        ',' => [0, 0, 0, 0, 12, 4, 8],
        ':' => [0, 12, 12, 0, 12, 12, 0],
        ';' => [0, 12, 12, 0, 12, 4, 8],
        '-' => [0, 0, 0, 31, 0, 0, 0],
        '—' => [0, 0, 0, 31, 0, 0, 0],
        '–' => [0, 0, 0, 31, 0, 0, 0],
        '\'' => [12, 4, 8, 0, 0, 0, 0],
        '’' => [12, 4, 8, 0, 0, 0, 0],
        '`' => [12, 4, 8, 0, 0, 0, 0],
        '"' => [10, 10, 10, 0, 0, 0, 0],
        '«' => [10, 10, 10, 0, 0, 0, 0],
        '»' => [10, 10, 10, 0, 0, 0, 0],
        '“' => [10, 10, 10, 0, 0, 0, 0],
        '”' => [10, 10, 10, 0, 0, 0, 0],
        '!' => [4, 4, 4, 4, 4, 0, 4],
        '?' => [14, 17, 1, 2, 4, 0, 4],
        '&' => [12, 18, 20, 8, 21, 18, 13],
        '/' => [0, 1, 2, 4, 8, 16, 0],
        '(' => [2, 4, 8, 8, 8, 4, 2],
        ')' => [8, 4, 2, 2, 2, 4, 8],
        '[' => [14, 8, 8, 8, 8, 8, 14],
        ']' => [14, 2, 2, 2, 2, 2, 14],
        '+' => [0, 4, 4, 31, 4, 4, 0],
        '#' => [10, 10, 31, 10, 31, 10, 10],
        '*' => [0, 4, 21, 14, 21, 4, 0],
        '_' => [0, 0, 0, 0, 0, 0, 31],
        '·' => [0, 0, 0, 4, 0, 0, 0],
        '•' => [0, 0, 0, 4, 0, 0, 0],
        '…' => [0, 0, 0, 0, 0, 0, 21],
        '%' => [24, 25, 2, 4, 8, 19, 3],
        '=' => [0, 0, 31, 0, 31, 0, 0],
        '@' => [14, 17, 1, 13, 21, 21, 14],
        '$' => [4, 15, 20, 14, 5, 30, 4],
        'А' => [14, 17, 17, 31, 17, 17, 17],
        'В' => [30, 17, 17, 30, 17, 17, 30],
        'Е' => [31, 16, 16, 30, 16, 16, 31],
        'К' => [17, 18, 20, 24, 20, 18, 17],
        'М' => [17, 27, 21, 21, 17, 17, 17],
        'Н' => [17, 17, 17, 31, 17, 17, 17],
        'О' => [14, 17, 17, 17, 17, 17, 14],
        'Р' => [30, 17, 17, 30, 16, 16, 16],
        'С' => [14, 17, 16, 16, 16, 17, 14],
        'Т' => [31, 4, 4, 4, 4, 4, 4],
        'Х' => [17, 17, 10, 4, 10, 17, 17],
        'З' => [31, 2, 4, 2, 1, 17, 14],
        'Ё' => [10, 0, 31, 16, 30, 16, 31],
        'Б' => [31, 16, 16, 30, 17, 17, 30],
        'Г' => [31, 16, 16, 16, 16, 16, 16],
        'Д' => [6, 10, 10, 10, 10, 31, 17],
        'Ж' => [21, 21, 14, 4, 14, 21, 21],
        'И' => [17, 17, 19, 21, 25, 17, 17],
        'Й' => [10, 4, 17, 19, 21, 25, 17],
        'Л' => [7, 9, 9, 9, 9, 9, 17],
        'П' => [31, 17, 17, 17, 17, 17, 17],
        'У' => [17, 17, 17, 15, 1, 17, 14],
        'Ф' => [4, 14, 21, 21, 21, 14, 4],
        'Ц' => [18, 18, 18, 18, 18, 31, 1],
        'Ч' => [17, 17, 17, 15, 1, 1, 1],
        'Ш' => [21, 21, 21, 21, 21, 21, 31],
        'Щ' => [21, 21, 21, 21, 21, 31, 1],
        'Ъ' => [24, 8, 8, 14, 9, 9, 14],
        'Ы' => [17, 17, 17, 25, 21, 21, 25],
        'Ь' => [16, 16, 16, 30, 17, 17, 30],
        'Э' => [14, 17, 1, 7, 1, 17, 14],
        'Ю' => [18, 21, 21, 29, 21, 21, 18],
        'Я' => [15, 17, 17, 15, 5, 9, 17],
        'І' => [14, 4, 4, 4, 4, 4, 14],
        'Ї' => [10, 0, 14, 4, 4, 4, 14],
        'Є' => [14, 17, 16, 30, 16, 17, 14],
        'Ґ' => [1, 31, 16, 16, 16, 16, 16],
        'Ў' => [10, 4, 17, 15, 1, 17, 14],
        'À' => [14, 17, 17, 31, 17, 17, 17],
        'Á' => [14, 17, 17, 31, 17, 17, 17],
        'Â' => [14, 17, 17, 31, 17, 17, 17],
        'Ä' => [14, 17, 17, 31, 17, 17, 17],
        'Ã' => [14, 17, 17, 31, 17, 17, 17],
        'Å' => [14, 17, 17, 31, 17, 17, 17],
        'Ç' => [14, 17, 16, 16, 16, 17, 14],
        'È' => [31, 16, 16, 30, 16, 16, 31],
        'É' => [31, 16, 16, 30, 16, 16, 31],
        'Ê' => [31, 16, 16, 30, 16, 16, 31],
        'Ë' => [31, 16, 16, 30, 16, 16, 31],
        'Ì' => [14, 4, 4, 4, 4, 4, 14],
        'Í' => [14, 4, 4, 4, 4, 4, 14],
        'Î' => [14, 4, 4, 4, 4, 4, 14],
        'Ï' => [14, 4, 4, 4, 4, 4, 14],
        'Ñ' => [17, 17, 25, 21, 19, 17, 17],
        'Ò' => [14, 17, 17, 17, 17, 17, 14],
        'Ó' => [14, 17, 17, 17, 17, 17, 14],
        'Ô' => [14, 17, 17, 17, 17, 17, 14],
        'Ö' => [14, 17, 17, 17, 17, 17, 14],
        'Õ' => [14, 17, 17, 17, 17, 17, 14],
        'Ø' => [14, 17, 17, 17, 17, 17, 14],
        'Ù' => [17, 17, 17, 17, 17, 17, 14],
        'Ú' => [17, 17, 17, 17, 17, 17, 14],
        'Û' => [17, 17, 17, 17, 17, 17, 14],
        'Ü' => [17, 17, 17, 17, 17, 17, 14],
        'Ý' => [17, 17, 10, 4, 4, 4, 4],
        'Ÿ' => [17, 17, 10, 4, 4, 4, 4],
        'Œ' => [14, 17, 17, 17, 17, 17, 14],
        'Æ' => [14, 17, 17, 31, 17, 17, 17],
        'ß' => [15, 16, 16, 14, 1, 1, 30],
        _ => [31, 17, 17, 17, 17, 17, 31]
    }
}
