// name: VFD panel
// protocol: furumi-visualizer-v3
// bundle-version: 7
//
// A hi-fi front panel in the style of a vacuum fluorescent display: thin
// segment digits, a peak level meter (bars or needles), a third-octave
// analyzer with three levels of glow and peak hold, a peak frequency readout
// with hold, tone fans, a signal path schematic, the transport word, mode
// lamps and a dot-matrix title line. Everything is drawn thin (eighth blocks,
// braille dots and box-drawing slashes for the 14-segment diagonals); only
// level bars are thicker.
//
// Typography: numbers (time, frequency, volume) in 7-segment digits, the
// transport word (PLAY / PAUSE / STOP) in 14-segment letters with faint unlit
// segments,
// the title in a 5x7 dot matrix, service labels in plain thin text, mode lamps
// as a label over a short bar.
//
// Effects, most of them keeping a little state between frames:
//   power-on segment test, counter "search" on track changes, OVER lamp with
//   hold, blinking pause point, slightly uneven digit brightness (an aged
//   tube), a slow filament "breath", phosphor that glows brighter only where a
//   column is loud, a dimmer at night, colours that fade into unlit phosphor
//   on pause, a source block that goes dark and relights with its new value,
//   a short burst of static when the track or source changes, a pulse that
//   now and then travels the signal path, LOUDNESS / MUTE / SEARCH lamps, a
//   running tape counter, and a standby clock with a sweep every minute when
//   nothing is loaded.
//
// Options: edit the functions below.
fn PALETTE() { "classic" }        // "classic" | "green" | "amber" | "ice"
fn METER_STYLE() { "bars" }       // "bars" | "needles"
fn NIGHT_DIMMER() { true }        // dim the panel from 22:00 to 07:00
fn DISPLAY_MODE() { "auto" }      // "auto" | "full" | "spectrum" | "minimal" | "deck"
//   full      everything: meters, analyzer, tone, signal path, transport
//   spectrum  a taller analyzer; the lower modules shrink to one line
//   minimal   time, title, L/R meter and the analyzer, no signal path
//   deck      a cassette deck: large counter, tape counter, wide L/R meters
//   auto      Shift+M in the visualizer steps through full, spectrum,
//             minimal and deck; a fixed value pins one of them
// A display mode that does not fit the window steps down: full -> spectrum
// -> minimal -> compact. On a change the panel switches like hardware: the
// time and L/R meters stay lit, the other sections go dark and light up one
// after another.
fn DECK_LAMPS() { true }          // AMS / NR / AUTO REVERSE / REW-FF lamps in deck mode
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
//   mode                          display mode counter (Shift+M), use modulo
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

// Blend two colours: t = 0 gives a, 1 gives b.
fn mixc(a, b, t) {
    let ar = to_float((a >> 16) & 255); let ag = to_float((a >> 8) & 255); let ab = to_float(a & 255);
    let br = to_float((b >> 16) & 255); let bg = to_float((b >> 8) & 255); let bb = to_float(b & 255);
    rgb(ar + (br - ar) * t, ag + (bg - ag) * t, ab + (bb - ab) * t)
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
    if x >= 0 && y >= 0 && w > 0 && fg >= 0 { this.push(#{ op: "hline", x: x, y: y, w: w, ch: ch, fg: fg, bg: 0 }); }
}
fn vl(x, y, h, ch, fg) {
    if x >= 0 && y >= 0 && h > 0 && fg >= 0 { this.push(#{ op: "vline", x: x, y: y, h: h, ch: ch, fg: fg, bg: 0 }); }
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
// auxiliary (tone / signal path / transport) and warnings. Each frame the
// zones get one brightness: night dimmer x filament breath (plus a boost during
// the power-on test). Loud analyzer columns glow brighter on their own, and
// pause fades lit colours into unlit phosphor.

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

// An unlit, desaturated phosphor tone of `c`: what a segment looks like when
// the grid voltage drops. `t` blends towards it (0 = lit colour).
fn phosphor(c, t) {
    let r = to_float((c >> 16) & 255); let g = to_float((c >> 8) & 255); let b = to_float(c & 255);
    let l = (0.3 * r + 0.59 * g + 0.11 * b) * 0.45;
    rgb(r + (l * 0.92 - r) * t, g + (l - g) * t, b + (l * 0.96 - b) * t)
}

fn glow(c, b, fade) { phosphor(scale(c, b), fade) }

// `boost` brightens the whole panel (power-on test); `fade` (0..1) moves the
// lit colours towards unlit phosphor (pause, stop).
fn make_palette(input, boost, fade) {
    let z = zone_bases();
    let dim = if night(input) { 0.72 } else { 1.0 };
    let breath = 0.955 + 0.045 * sin(input.time * 0.9);
    let b = clamp(dim * breath + boost, 0.3, 1.15);
    let amber = glow(z[1], b, fade);
    // the analyzer's tint runs from warm amber in the bass to light yellow
    // in the treble, a small shift that keeps the panel one colour
    let warm_base = if PALETTE() == "classic" || PALETTE() == "amber" { mixc(z[1], rgb(255, 120, 16), 0.3) } else { scale(z[1], 0.9) };
    #{
        white: glow(z[0], b, fade), white_mid: glow(z[0], 0.6 * b, fade), white_ghost: scale(z[0], 0.095),
        amber: amber, amber_low: glow(z[1], 0.68 * b, fade), amber_hot: whiten(amber, 0.5),
        peak: whiten(amber, 0.8), amber_ghost: scale(z[1], 0.17), amber_text: glow(z[1], 0.8 * b, fade),
        amber_rule: scale(z[1], 0.3),
        amber_warm: glow(warm_base, b, fade), amber_light: glow(whiten(z[1], 0.32), b, fade),
        red: glow(z[3], b, fade * 0.6), red_mid: glow(z[3], 0.34 * b, fade * 0.6), red_ghost: scale(z[3], 0.18),
        cyan: glow(z[2], b, fade), cyan_mid: glow(z[2], 0.55 * b, fade), cyan_hot: whiten(glow(z[2], b, fade), 0.6),
        cyan_line: glow(z[2], 0.4 * b, fade),
        cyan_ghost: scale(z[2], 0.13),
        // unlit segments of the thin digits: fainter than the analyzer's,
        // so a lit digit never reads as an 8
        white_seg: scale(z[0], 0.055), amber_seg: scale(z[1], 0.075), cyan_seg: scale(z[2], 0.07),
        // unlit lamps: visible without being read as lit, the same at night
        lamp_off: scale(z[0], 0.17), red_lamp_off: scale(z[3], 0.26),
        amber_lamp_off: scale(z[1], 0.22), cyan_lamp_off: scale(z[2], 0.2),
        noise: scale(z[0], 0.32 * b)
    }
}

// ---------- thin segment font ----------
// Segments are thin lines on cell edges, all about equally thick: horizontals
// are eighth blocks ("▁", 1/8 of the cell height) along the bottom of a row,
// verticals are quarter blocks ("▎", 1/4 of the narrower cell width) along
// the left edge of the first and the last column. A cell holds one stroke,
// so where a horizontal would share the left column with a vertical (middle
// and bottom bars) the vertical keeps the cell and the bar starts one column
// later: a small joint gap like on a real segment display. A glyph is cw cells
// wide and 2*rh+1 rows tall. It covers digits and the few letters in
// seg_mask; the transport word uses the 14-segment letters below (seg14_*).

fn seg_mask(ch) {
    let s = switch ch {
        "0" => "abcdef", "1" => "bc", "2" => "abged", "3" => "abgcd",
        "4" => "fgbc", "5" => "afgcd", "S" => "afgcd", "6" => "afgedc",
        "7" => "abc", "8" => "abcdefg", "9" => "abcdfg", "P" => "abefg",
        "L" => "def", "A" => "abcefg", "U" => "bcdef", "E" => "adefg",
        "O" => "abcdef", "H" => "bcefg", "C" => "adef", "r" => "eg", "n" => "ceg",
        "d" => "bcdeg", "-" => "g", _ => ""
    };
    let m = 0;
    for c in s.chars() {
        m = m | switch c { 'a' => 1, 'b' => 2, 'c' => 4, 'd' => 8, 'e' => 16, 'f' => 32, 'g' => 64, _ => 0 };
    }
    m
}

fn pick(mask, bit, on, off) { if (mask & bit) != 0 { on } else { off } }

fn seg(x, y, mask, cw, rh, on, off) {
    let inner = mx(cw - 2, 1);
    this.hl(x, y, cw - 1, "▁", pick(mask, 1, on, off));
    this.vl(x, y + 1, rh, "▎", pick(mask, 32, on, off));
    this.vl(x + cw - 1, y + 1, rh, "▎", pick(mask, 2, on, off));
    this.hl(x + 1, y + rh, inner, "▁", pick(mask, 64, on, off));
    this.vl(x, y + rh + 1, rh, "▎", pick(mask, 16, on, off));
    this.vl(x + cw - 1, y + rh + 1, rh, "▎", pick(mask, 4, on, off));
    this.hl(x + 1, y + 2 * rh, inner, "▁", pick(mask, 8, on, off));
}

// ---------- 14-segment letters (transport word) ----------
// A fixed 7 x 5 cell glyph: the outline as in the 7-segment font, a middle
// bar split in two halves (g1, g2), centre strokes above and below (i, l) and
// four diagonals drawn with box-drawing slashes, two cells each. Unlit
// segments glow faintly, like the counter's. Lit strokes are drawn last, so a
// lit stroke wins a cell it shares with an unlit one.

fn seg14_mask(ch) {
    let s = switch ch {
        "P" => "a b f e g1 g2", "L" => "f e d", "A" => "a b c e f g1 g2",
        "Y" => "h j l", "U" => "b c d e f", "S" => "a f g1 g2 c d",
        "E" => "a f e d g1", "T" => "a i l", "O" => "a b c d e f", _ => ""
    };
    let m = 0;
    for part in s.split(" ") {
        m = m | switch part {
            "a" => 1, "b" => 2, "c" => 4, "d" => 8, "e" => 16, "f" => 32,
            "g1" => 64, "g2" => 128, "h" => 256, "i" => 512, "j" => 1024,
            "k" => 2048, "l" => 4096, "m" => 8192, _ => 0
        };
    }
    m
}

fn seg14_stroke(x, y, bit, fg) {
    switch bit {
        1 => this.hl(x, y, 6, "▁", fg),
        2 => this.vl(x + 6, y + 1, 2, "▎", fg),
        4 => this.vl(x + 6, y + 3, 2, "▎", fg),
        8 => this.hl(x + 1, y + 4, 5, "▁", fg),
        16 => this.vl(x, y + 3, 2, "▎", fg),
        32 => this.vl(x, y + 1, 2, "▎", fg),
        64 => this.hl(x + 1, y + 2, 2, "▁", fg),
        128 => this.hl(x + 4, y + 2, 2, "▁", fg),
        256 => { this.txt(x + 1, y + 1, "╲", fg); this.txt(x + 2, y + 2, "╲", fg); },
        512 => this.vl(x + 3, y + 1, 2, "▎", fg),
        1024 => { this.txt(x + 4, y + 1, "╱", fg); this.txt(x + 3, y + 2, "╱", fg); },
        2048 => { this.txt(x + 2, y + 3, "╱", fg); this.txt(x + 1, y + 4, "╱", fg); },
        4096 => this.vl(x + 3, y + 3, 2, "▎", fg),
        8192 => { this.txt(x + 3, y + 3, "╲", fg); this.txt(x + 4, y + 4, "╲", fg); },
        _ => ()
    }
}

// Draws a word in 14-segment letters; returns the x after it.
fn seg14_text(x, y, s, on, off) {
    let cx = x;
    // unlit diagonals last, so the faint X of a 14-segment display stays whole
    // (unlit straight strokes give up the cells they share with it)
    let off_bits = [1, 2, 4, 8, 16, 32, 64, 128, 512, 4096, 256, 1024, 2048, 8192];
    let bits = [256, 1024, 2048, 8192, 1, 2, 4, 8, 16, 32, 64, 128, 512, 4096];
    for ch in s.chars() {
        let mask = seg14_mask("" + ch);
        for b in off_bits { if (mask & b) == 0 { this.seg14_stroke(cx, y, b, off); } }
        for b in bits { if (mask & b) != 0 { this.seg14_stroke(cx, y, b, on); } }
        cx += 8;
    }
    cx
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

// Display curve of the analyzer: a 42 dB window per third-octave band that
// ends at `top` dBFS, raised to a power so quiet passages sit lower and loud
// ones reach the top. `top` follows the music (see update_range): the window
// moves, the measurements do not.
fn band_display(input, v, top) {
    // the window never reaches below the analyzer floor: silence stays dark
    let lo = mx(top - 42.0, get(input, "spectrum_floor_db", -60.0));
    let lin = clamp((dbfs(input, v) - lo) / (top - lo), 0.0, 1.0);
    if lin <= 0.0 { 0.0 } else { pow(lin, 1.3) }
}

// Top of the analyzer window: rises at once with the loudest band, falls
// slowly (4 dB/s), and stays within -26..-6 dBFS, so quiet music fills more
// of the analyzer and loud music does not pin it to the top.
fn update_range(st, input, dt) {
    let s = get(input, "spectrum", []);
    let loudest = -60.0;
    for i in 5..30 { if i < s.len() { loudest = mx(loudest, dbfs(input, s[i])); } }
    if !("agc" in st) { st.agc = -14.0; }
    st.agc = if loudest > st.agc { loudest } else { st.agc - 4.0 * dt };
    st.top_db = clamp(st.agc + 4.0, -26.0, -6.0);
    st
}

// The 25 panel bands 63 Hz .. 16 kHz are spectrum entries 5..29.
fn panel_bands(input, top) {
    let s = get(input, "spectrum", []);
    let p = get(input, "spectrum_peaks", []);
    let out = [];
    for i in 5..30 {
        let v = if i < s.len() { s[i] } else { 0.0 };
        let pk = if i < p.len() { p[i] } else { 0.0 };
        out.push([band_display(input, v, top), band_display(input, pk, top)]);
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

fn draw_counter(input, st, x, y, cw, rh) {
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
    let end = this.seg_text(x, y, text, cw, rh, lit, off, blink);
    let n = get(input, "track_number", 0);
    let q = get(input, "queue_index", 0);
    let tn = if n > 0 { n } else { q };
    let ts = if tn <= 0 { "-" } else if tn > 99 { "--" } else { "" + tn };
    let te = this.seg_text(end + 3, y, ts, cw, rh, lit, off, false);
    this.txt(x, y + 2 * rh + 2, "TIME", p.white_mid);
    this.txt(end + 3, y + 2 * rh + 2, "TRACK", p.white_mid);
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
        // the red zone glows at a middle level even when unlit, so it reads as a zone
        this.hl(bx + hot, r[3], mw - hot, "▃", if st.boot > 0.0 { p.red } else { p.red_mid });
        let cells = if r[1] > 0.0 { clamp(to_int(meter_pos(dbfs(input, r[1])) / n * to_float(mw) + 0.5), 0, mw) } else { 0 };
        this.hl(bx, r[3], mn(cells, hot), "▃", p.white);
        if cells > hot { this.hl(bx + hot, r[3], cells - hot, "▃", p.red); }
        if r[2] > 0.0 {
            // a two-cell peak mark, brighter than the bar
            let pk = clamp(to_int(meter_pos(dbfs(input, r[2])) / n * to_float(mw)), 1, mw - 1);
            this.hl(bx + pk - 1, r[3], 2, "▃", if pk >= hot { whiten(p.red, 0.25) } else { whiten(p.white, 0.3) });
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
        this.txt(left + mw - 4, y + 5, "PEAK", if peak_on || st.boot > 0.0 { p.red } else { p.red_lamp_off });
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
        this.txt(cx, y, names[i], if on { p.red } else { p.red_lamp_off });
        this.hl(cx, y + 1, names[i].len(), "▔", if on { p.red } else { p.red_lamp_off });
        cx += names[i].len() + 3;
    }
    let over = st.boot > 0.0 || st.over > 0.0;
    this.txt(x, y + 3, "OVER", if over { p.red } else { p.red_lamp_off });
    this.txt(x, y + 5, "CNT", p.white_mid);
    this.txt(x + 4, y + 5, if st.boot > 0.0 { "8888" } else { pad4(tape_counter(input.position)) }, p.white);
}

// A mode lamp: the label with a short indicator bar under it.
fn lamp(x, y, label, on, lit, ghost) {
    let fg = if on { lit } else { ghost };
    this.txt(x, y, label, fg);
    this.hl(x, y + 1, label.len(), "▔", fg);
    x + label.len() + 3
}

fn draw_status_row(input, st, x, y, w) {
    let p = st.pal;
    let boot = st.boot > 0.0;
    let repeat = get(input, "repeat", "off");
    let source = get(input, "source", "");
    let vol = to_int(input.volume * 100.0 + 0.5);
    let cx = x;
    cx = this.lamp(cx, y, "REPEAT", boot || repeat != "off", p.white, p.lamp_off);
    if repeat == "one" || boot { this.txt(cx - 2, y, "1", p.white); }
    cx = this.lamp(cx, y, "SHUFFLE", boot || get(input, "shuffle", false), p.white, p.lamp_off);
    cx = this.lamp(cx, y, "LOCAL", boot || source == "local", p.white, p.lamp_off);
    cx = this.lamp(cx, y, "FED", boot || source == "federated", p.white, p.lamp_off);
    cx = this.lamp(cx, y, "PODCAST", boot || source == "podcast", p.white, p.lamp_off);
    cx += 2;
    // automatic modes: loudness at low volume, mute at zero, search on seeks
    cx = this.lamp(cx, y, "LOUDNESS", boot || (vol > 0 && vol <= 30), p.white, p.lamp_off);
    cx = this.lamp(cx, y, "MUTE", boot || vol == 0, p.red, p.red_lamp_off);
    let blink = to_int(input.time * 6.0) % 2 == 0;
    this.lamp(cx, y, "SEARCH", boot || (st.search > 0.0 && blink), p.white, p.lamp_off);

    // format badge, like a noise-reduction lamp with its boxed letter
    let fmt = get(input, "audio_format", "").to_upper();
    let depth = get(input, "audio_bit_depth", 0);
    let badge = if fmt == "" { "PCM" } else { fmt };
    let kbps = get(input, "audio_bitrate", 0);
    let box_ = if depth > 0 { "" + depth + "BIT" } else if kbps > 0 { "" + kbps + "K" } else { "--" };
    let bx = x + w - badge.len() - box_.len() - 4;
    if bx > cx + 6 {
        this.txt(bx, y, badge, if fmt != "" || boot { p.white } else { p.lamp_off });
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
// Three levels of glow: lit segments (dimmer at the foot of a column, the top
// one nearly white), unlit segments that stay visible but faint, and a light
// peak-hold mark above the column. Louder columns burn brighter on their own
// instead of the whole panel brightening.
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
    let boot = st.boot > 0.0;
    for i in 0..n {
        let bx = x + 2 + i * bw;
        let v = if boot { 1.0 } else { bands[i][0] };
        let lit = clamp(to_int(v * to_float(h) + 0.5), 0, h);
        let g = if boot { 1.0 } else { 0.78 + 0.34 * v };
        let t = to_float(i) / to_float(mx(n - 1, 1));
        let col = if t < 0.5 { mixc(p.amber_warm, p.amber, t * 2.0) } else { mixc(p.amber, p.amber_light, (t - 0.5) * 2.0) };
        if lit < h { this.fill(bx, y, bar, h - lit, "▂", p.amber_ghost); }
        if lit > 0 {
            let low = to_int(to_float(lit) * 0.45);
            if low > 0 { this.fill(bx, y + h - low, bar, low, "▂", scale(col, 0.68 * g)); }
            let body = lit - low - 1;
            if body > 0 { this.fill(bx, y + h - lit + 1, bar, body, "▂", scale(col, g)); }
            this.hl(bx, y + h - lit, bar, "▂", p.amber_hot);
        }
        let cap = clamp(to_int(bands[i][1] * to_float(h) + 0.5), 0, h);
        if cap > lit && !boot { this.hl(bx, y + h - cap, bar, "▂", p.peak); }
        let label = band_label(i);
        // narrow bars: keep only the dot halfway between two frequencies, so
        // dots never push the frequencies out
        if label != "·" || bar >= 2 || i % 4 == 2 {
            let lx = mx(bx + (bar - label.len()) / 2, x + 1);
            if lx >= free {
                this.txt(lx, y + h + 1, label, p.amber_text);
                free = lx + label.len() + 1;
            }
        }
    }
    right
}

// Peak frequency as one measuring module with a fixed vertical structure:
// caption and HOLD lamp, the value, a rule, then the sample rate. The value
// is held (HOLD lit) while the loudest band is weaker than the held one.
fn draw_measure(input, st, x, y, w) {
    let p = st.pal;
    let boot = st.boot > 0.0;
    let live = boot || st.hold_v > 0.05;
    let holding = !boot && live && st.hold_i != st.best_i;
    this.txt(x, y, "PEAK FREQUENCY", p.amber_text);
    this.txt(x + w - 4, y, "HOLD", if holding || boot { p.amber } else { p.amber_lamp_off });
    let hz = band_hz(st.hold_i);
    let digits = ""; let unit = "";
    if hz < 1000 { digits = "" + hz; unit = "Hz"; }
    else if hz < 10000 { digits = "" + (hz / 1000) + "." + ((hz % 1000) / 100); unit = "kHz"; }
    else { digits = "" + (hz / 1000); unit = "kHz"; }
    if !live { digits = "--"; }
    let off = if boot { p.amber } else { p.amber_seg };
    let end = this.seg_text(x, y + 2, digits, 6, 2, p.amber, off, true);
    this.txt(end + 1, y + 6, unit, if live { p.amber_text } else { p.amber_ghost });
    this.hl(x, y + 8, w, "▁", p.amber_rule);
    let sr = get(input, "sample_rate", 0);
    let srt = if sr > 0 { "" + (sr / 1000) + "." + ((sr % 1000) / 100) + " kHz" } else { "--" };
    this.txt(x, y + 10, "RATE", p.amber_text);
    this.txt(x + w - srt.len(), y + 10, srt, p.amber);
}

// Height of the measuring module.
fn measure_height() { 11 }

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
    // a bright centre mark, and the marker as two thin vertical segments
    this.txt(mid, y + 1, "▎", p.cyan_mid);
    let pos = clamp(mid + to_int(bal * to_float(w / 2 - 2) * 4.0), x + 2, x + w - 4);
    this.txt(pos, y + 1, "▎", p.cyan_hot);
    this.txt(pos + 1, y + 1, "▎", p.cyan_hot);
}

// A framed box: "▁" above, "▔" below, "▎" sides on the left edges of the
// first column and of the column after the label, so the frame lines are all
// equally thin and every corner meets. Returns the column after its right
// side. 3 rows tall, the label on row y+1.
fn sbox(x, y, label, fg) {
    let w = label.len() + 4;
    this.hl(x, y, w, "▁", fg);
    this.txt(x, y + 1, "▎", fg);
    this.txt(x + 2, y + 1, label, fg);
    this.txt(x + w, y + 1, "▎", fg);
    this.hl(x, y + 2, w, "▔", fg);
    x + w + 1
}

fn sbox_width(label) { label.len() + 5 }

// Signal path drawn as a schematic: boxes on a bus line that runs along their
// bottom edges, nodes between them, and a wire from the end of the upper row
// down to the lower row. The active path is lit; now and then a bright pulse
// travels along it from the source to the speakers.
fn draw_schematic(input, st, x, y, w) {
    let p = st.pal;
    let playing = get(input, "playing", !input.paused && input.track_title != "");
    let lit = playing || st.boot > 0.0;
    let on = if lit { p.cyan } else { p.cyan_mid };
    // lines and wires stay quiet; boxes and nodes carry the light
    let bus = if lit { p.cyan_line } else { p.cyan_ghost };
    let fmt = get(input, "audio_format", "").to_upper();
    let vol = to_int(input.volume * 100.0 + 0.5);
    let src = switch get(input, "source", "") { "federated" => "NETWORK", "podcast" => "PODCAST", _ => "LIBRARY" };
    let mid = if fmt == "" { "DECODE" } else { fmt };
    let vol_label = "VOL " + vol;

    // upper row: source, decoder, volume; the wire leaves at column xr
    let top = [src, mid, vol_label];
    let tw = 0;
    for l in top { tw += sbox_width(l); }
    let xr = x + w - 2;
    let gap = clamp((w - 2 - tw) / 2, 2, 12);
    let bus_y = y + 2;
    this.hl(x, bus_y, xr - x, "▔", bus);
    let cx = x;
    for k in 0..3 {
        let fg = on;
        // a new source goes dark for a moment, then lights with its value
        if k == 0 && st.src_flash > 0.25 { fg = p.cyan_ghost; }
        let next = this.sbox(cx, y, top[k], fg);
        if k < 2 { this.txt(next + (gap - 1) / 2, bus_y, "▀", on); }
        cx = next + gap - 1;
    }

    // the wire down to the lower bus
    let ty = y + 4;
    let low_y = ty + 2;
    this.vl(xr, bus_y, low_y - bus_y, "▎", bus);

    // lower row, right-aligned: tone, amplifier, speakers
    let speakers = "SPEAKERS A";
    let bottom = [speakers, "AMP", "TONE"];
    let bw_ = sbox_width(speakers) + sbox_width("AMP") + sbox_width("TONE");
    if bw_ + 4 > xr - x { speakers = "SPK A"; bottom = [speakers, "AMP", "TONE"]; bw_ = sbox_width(speakers) + sbox_width("AMP") + sbox_width("TONE"); }
    let gap2 = clamp((xr - x - bw_) / 3, 2, 12);
    let start = xr - 1 - bw_ - 2 * (gap2 - 1);
    this.hl(start, low_y, xr - start, "▔", bus);
    let cx2 = start;
    for k in 0..3 {
        let next = this.sbox(cx2, ty, bottom[k], on);
        if k < 2 { this.txt(next + (gap2 - 1) / 2, low_y, "▀", on); }
        cx2 = next + gap2 - 1;
    }

    // the pulse: every 7 s it runs once along bus, wire and lower bus
    if playing && st.boot <= 0.0 {
        let phase = (input.time % 7.0) / 2.2;
        if phase < 1.0 {
            let n1 = xr - x; let n2 = low_y - bus_y; let n3 = xr - start;
            let total = n1 + n2 + n3;
            let at = to_int(phase * to_float(total));
            for tail in 0..3 {
                let k = at - tail;
                if k < 0 { continue; }
                let fg = if tail == 0 { p.cyan_hot } else { p.cyan };
                if k < n1 { this.txt(x + k, bus_y, "▔", fg); }
                else if k < n1 + n2 { this.txt(xr, bus_y + (k - n1), "▎", fg); }
                else if k < total { this.txt(xr - 1 - (k - n1 - n2), low_y, "▔", fg); }
            }
        }
    }

    let state = if !playing { if input.track_title == "" { "STANDBY" } else { "PAUSED" } }
                else if vol == 0 { "MUTE" } else { "ACTIVE" };
    this.txt(x, y + 8, "SIGNAL PATH / " + state, p.cyan_mid);
}

// The transport module: the word in 14-segment letters, then one row of
// captions and one row of equally tall digits under them, aligned to the
// module's edges: track number (with the queue length) on the left, the
// STEREO lamp in the middle, volume on the right.
fn draw_transport(input, st, x, y) {
    let p = st.pal;
    let word = if input.track_title == "" { "STOP" } else if input.paused { "PAUSE" } else { "PLAY" };
    let boot = st.boot > 0.0;
    let off = if boot { p.cyan } else { p.cyan_seg };
    this.seg14_text(x, y, word, p.cyan, off);
    let w = transport_width();
    let ly = y + 6;
    let caption = whiten(p.cyan_mid, 0.25);
    let stereo = get(input, "level_l", 0.0) > 0.0 && (get(input, "level_l", 0.0) - get(input, "level_r", 0.0)).abs() > 0.001;
    this.txt(x, ly, "TRACK", caption);
    this.txt(x + 15, ly, "STEREO", if stereo || boot { p.cyan } else { p.cyan_lamp_off });
    this.txt(x + w - 7, ly, "VOLUME", caption);
    let q = get(input, "queue_index", 0);
    let qn = get(input, "queue_length", 0);
    let qs = if q <= 0 || q > 99 { "--" } else { pad2(q) };
    let end = this.seg_text(x, ly + 1, qs, 5, 1, p.cyan, off, false);
    this.txt(end + 1, ly + 3, "OF " + (if qn > 999 { "1K+" } else { "" + qn }), caption);
    // volume digits end where the module ends, "%" on their baseline
    let vs = "" + to_int(input.volume * 100.0 + 0.5);
    let vx = x + w - 3 - (vs.len() * 6 - 1);
    this.seg_text(vx, ly + 1, vs, 5, 1, p.cyan, off, false);
    this.txt(x + w - 2, ly + 3, "%", caption);
}

// "PAUSE" in the segment letters is the widest transport word.
fn transport_width() { 40 }

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
        st = #{ boot: 0.9, spin: 0.0, over: 0.0, noise: 0.0, search: 0.0, src_flash: 0.0,
                fade: 0.0, hold_i: 0, hold_v: 0.0, hold_t: 0.0, best_i: 0,
                key: key, source: get(input, "source", ""), last_pos: input.position };
    }
    if !("fade" in st) { st.fade = 0.0; st.src_flash = 0.0; st.hold_i = 0; st.hold_v = 0.0; st.hold_t = 0.0; st.best_i = 0; }
    let dt = get(input, "dt", 0.05);
    st.boot -= dt; st.spin -= dt; st.over -= dt; st.noise -= dt; st.search -= dt; st.src_flash -= dt;
    // lit colours fade into unlit phosphor while paused (the standby clock
    // stays lit)
    let target = if input.paused && input.track_title != "" { 1.0 } else { 0.0 };
    st.fade = if target > st.fade { mn(st.fade + dt / 0.8, target) } else { mx(st.fade - dt / 0.3, target) };
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
            st.seek_dir = if input.position > expected { 1 } else { -1 };
        }
    }
    if st.source != source { st.source = source; st.noise = 0.35; st.src_flash = 0.5; }
    st.last_pos = input.position;
    let hot = mx(get(input, "peak_l", 0.0), get(input, "peak_r", 0.0));
    if dbfs(input, hot) > -0.5 && hot > 0.0 { st.over = 1.0; }
    // the panel brightens while the power-on test runs
    st.pal = make_palette(input, if st.boot > 0.0 { 0.1 } else { 0.0 }, if st.boot > 0.0 { 0.0 } else { st.fade });
    st
}

// Peak-frequency hold: the readout keeps the strongest band for a moment, and
// a new band takes over when it is at least as loud as the held one has
// decayed to.
fn update_hold(st, bands, dt) {
    let best = 0;
    for i in 1..bands.len() { if bands[i][0] > bands[best][0] { best = i; } }
    st.best_i = best;
    st.hold_t -= dt;
    st.hold_v -= 0.35 * dt;
    if bands[best][0] >= st.hold_v || st.hold_t <= 0.0 {
        st.hold_i = best;
        st.hold_v = bands[best][0];
        st.hold_t = 1.2;
    }
    st
}

// ---------- layouts ----------

fn draw_top_band(input, st, bands, x0, y, cw) {
    let lamps_w = 18;
    this.draw_counter(input, st, x0, y, 6, 2);
    let meter_x = x0 + counter_width() + 4;
    let meter_w = x0 + cw - lamps_w - 4 - meter_x;
    if METER_STYLE() == "needles" {
        this.draw_needles(input, st, meter_x, y, meter_w);
    } else {
        this.draw_meter(input, st, meter_x, y, meter_w);
    }
    // the lamp block is not shared between modes: it relights with section 1
    this.draw_lamps(input, section(st, 1), bands, x0 + cw - lamps_w + 2, y);
}

fn draw_title_rows(input, st, x0, y, cw) {
    this.draw_status_row(input, st, x0, y, cw);
    this.txt(x0, y + 2, "▸", st.pal.white_mid);
    this.draw_matrix_title(input, st, x0 + 2, y + 2, cw - 2, title_label(input));
}

// FULL PANEL: meters, analyzer with its measuring module, tone, signal path
// and transport.
fn render_full(input, st, bands) {
    let cmds = [];
    let p = st.pal;
    let W = input.width;
    let H = input.height;
    let cw = mn(W - 4, 170);
    let x0 = (W - cw) / 2;

    // row 0 stays free for the optional clock
    let top = if input.show_clock { 1 } else { 0 };
    let spec_h = clamp(H - top - 30, 8, 16);
    // top band 7, status and title 4, analyzer spec_h + 2 labels, lower block 10
    let used = 7 + 4 + spec_h + 2 + 10;
    let gap = clamp((H - top - used) / 3, 0, 2);
    let y = clamp((H - used - gap * 3 - top) / 2 + top, top, 3);

    cmds.draw_top_band(input, st, bands, x0, y, cw);
    y += 7 + gap;
    cmds.draw_title_rows(input, section(st, 1), x0, y, cw);

    y += 4 + gap;
    let module_w = 26;
    let bw = clamp((cw - module_w - 8) / 25, 3, 5);
    let right = cmds.draw_analyzer(input, section(st, 2), bands, x0, y, bw, spec_h);
    let mx_ = right + mx((x0 + cw - right - module_w) / 2, 3);
    cmds.draw_measure(input, section(st, 3), mx_, y + mx((spec_h + 2 - measure_height()) / 2, 0), mn(module_w, x0 + cw - mx_));

    y += spec_h + 2 + gap;
    cmds.draw_balance(input, section(st, 4), x0, y, 28);
    cmds.draw_fans(input, section(st, 4), bands, x0, y + 3, 28);
    let tr_w = transport_width();
    let sx = x0 + 32;
    let sw = cw - 32 - tr_w - 4;
    cmds.draw_schematic(input, section(st, 5), sx, y, sw);
    cmds.draw_transport(input, section(st, 6), x0 + cw - tr_w, y);

    if input.show_clock { cmds.txt(W - input.clock.len() - 1, 0, input.clock, p.white_mid); }
    cmds
}

// One-line summary of transport and signal path for the compact modes.
fn draw_strip(input, st, x, y, w) {
    let p = st.pal;
    let word = if input.track_title == "" { "STOP" } else if input.paused { "PAUSE" } else { "PLAY" };
    let q = get(input, "queue_index", 0);
    let qn = get(input, "queue_length", 0);
    let vol = to_int(input.volume * 100.0 + 0.5);
    let fmt = get(input, "audio_format", "").to_upper();
    let src = switch get(input, "source", "") { "federated" => "NETWORK", "podcast" => "PODCAST", _ => "LIBRARY" };
    let cx = x;
    this.txt(cx, y, word, p.cyan); cx += word.len() + 3;
    let ts = "TRACK " + (if q > 999 { "999+" } else { "" + q }) + "/" + (if qn > 999 { "999+" } else { "" + qn });
    this.txt(cx, y, ts, p.cyan_mid); cx += ts.len() + 3;
    let vs = "VOL " + vol + "%";
    this.txt(cx, y, vs, p.cyan_mid); cx += vs.len() + 3;
    let path = src + " › " + (if fmt == "" { "DECODE" } else { fmt }) + " › VOL › TONE › AMP › A";
    if cx + path.len() < x + w { this.txt(cx, y, path, p.cyan_mid); }
}

// SPECTRUM FOCUS: the analyzer takes the height; the lower modules shrink to
// one line.
fn render_spectrum(input, st, bands) {
    let cmds = [];
    let p = st.pal;
    let W = input.width;
    let H = input.height;
    let cw = mn(W - 4, 170);
    let x0 = (W - cw) / 2;
    let top = if input.show_clock { 1 } else { 0 };
    // the analyzer is tall but capped, and the panel is centred vertically
    let spec_h = clamp(H - top - 17, 6, 18);
    let y = top + mx((H - top - (13 + spec_h + 4)) / 2, 0);
    cmds.draw_top_band(input, st, bands, x0, y, cw);
    y += 8;
    cmds.draw_title_rows(input, section(st, 1), x0, y, cw);
    y += 5;
    let module_w = 26;
    let bw = clamp((cw - module_w - 8) / 25, 2, 5);
    let right = cmds.draw_analyzer(input, section(st, 2), bands, x0, y, bw, spec_h);
    let mx_ = right + mx((x0 + cw - right - module_w) / 2, 3);
    if mx_ + module_w <= x0 + cw && spec_h + 2 >= measure_height() {
        cmds.draw_measure(input, section(st, 3), mx_, y + (spec_h + 2 - measure_height()) / 2, module_w);
    }
    cmds.draw_strip(input, section(st, 4), x0, y + spec_h + 3, cw);
    if input.show_clock { cmds.txt(W - input.clock.len() - 1, 0, input.clock, p.white_mid); }
    cmds
}

// TAPE DECK: a large counter, the tape counter and wide L/R meters, with a
// small analyzer underneath.
fn render_tape(input, st, bands) {
    let cmds = [];
    let p = st.pal;
    let W = input.width;
    let H = input.height;
    let cw = mn(W - 4, 170);
    let x0 = (W - cw) / 2;
    let cw0 = mn(W - 4, 170);
    // big digits only where the widest counter, track and tape counter still
    // end before the lamps
    let big = H >= 36 && cw0 >= 116;
    let rh = if big { 4 } else { 2 };
    let dw = if big { 9 } else { 6 };
    // centre the whole deck vertically
    let spec_h = clamp(H - (2 * rh + 4) - 6 - 5 - 6, 3, 10);
    let content = (2 * rh + 4) + 6 + 5 + spec_h + 4;
    let y = clamp((H - content) / 2, if input.show_clock { 1 } else { 0 }, 6);
    cmds.draw_counter(input, st, x0, y, dw, rh);
    // tape counter in segment digits at a fixed place after the widest
    // counter ("88.88" and a two-digit track), and the lamps on the right
    let cx = x0 + (6 * dw + 11) + 6;
    let off = if st.boot > 0.0 { p.white } else { p.white_seg };
    // during a seek the counter races like a deck winding the tape
    let counter = if st.boot > 0.0 { "8888" }
                  else if st.search > 0.0 { pad4((tape_counter(input.position) + input.frame * 37) % 10000) }
                  else { pad4(tape_counter(input.position)) };
    // the tape counter belongs to the deck section and relights with it
    let tp = section(st, 1).pal;
    cmds.seg_text(cx, y, counter, 6, 2, tp.white, off, false);
    cmds.txt(cx, y + 6, "TAPE COUNTER", tp.white_mid);
    let lamps_x = x0 + cw - 16;
    cmds.draw_lamps(input, section(st, 1), bands, lamps_x, y);
    if DECK_LAMPS() && lamps_x - (cx + 28) >= 18 {
        cmds.draw_deck_lamps(input, section(st, 1), cx + 31, y);
    }
    y += 2 * rh + 4;
    if METER_STYLE() == "needles" { cmds.draw_needles(input, st, x0, y, cw); } else { cmds.draw_meter(input, st, x0, y, cw); }
    y += 6;
    cmds.draw_title_rows(input, section(st, 2), x0, y, cw);
    y += 5;
    let bw = clamp((cw - 4) / 25, 2, 6);
    cmds.draw_analyzer(input, section(st, 3), bands, x0, y, bw, spec_h);
    if y + spec_h + 3 < H { cmds.draw_strip(input, section(st, 4), x0, y + spec_h + 3, cw); }
    if input.show_clock { cmds.txt(W - input.clock.len() - 1, 0, input.clock, p.white_mid); }
    cmds
}

// Cassette deck lamps: AMS lights while the counter searches on a track
// change, NR [B] while playing (decorative), AUTO REVERSE with repeat all,
// and REW / FF in the direction of a seek.
fn draw_deck_lamps(input, st, x, y) {
    let p = st.pal;
    let boot = st.boot > 0.0;
    let playing = get(input, "playing", false);
    let blink = to_int(input.time * 6.0) % 2 == 0;
    this.lamp(x, y, "AMS", boot || (st.spin > 0.0 && blink), p.white, p.lamp_off);
    let nx = this.lamp(x + 6, y, "NR", boot || playing, p.white, p.lamp_off);
    this.txt(nx - 2, y, "[B]", if boot || playing { p.white } else { p.lamp_off });
    this.lamp(x, y + 2, "AUTO REVERSE", boot || get(input, "repeat", "off") == "all", p.white, p.lamp_off);
    let dir = if "seek_dir" in st { st.seek_dir } else { 0 };
    let seeking = st.search > 0.0;
    this.lamp(x, y + 4, "<< REW", boot || (seeking && dir < 0), p.white, p.lamp_off);
    this.lamp(x + 9, y + 4, "FF >>", boot || (seeking && dir > 0), p.white, p.lamp_off);
}

// MINIMAL: time, the L/R meter, the title and a tall analyzer with its
// measuring module; no lamps, tone, signal path or transport.
fn render_minimal(input, st, bands) {
    let cmds = [];
    let p = st.pal;
    let W = input.width;
    let H = input.height;
    // a narrower composition keeps its balance on wide screens
    let cw = mn(W - 4, 132);
    let x0 = (W - cw) / 2;
    let top = if input.show_clock { 1 } else { 0 };
    // the analyzer is capped, so a tall window centres the panel instead
    let spec_h = clamp(H - top - 14, 6, 22);
    let y = top + mx((H - top - (13 + spec_h)) / 2, 0);
    cmds.draw_counter(input, st, x0, y, 6, 2);
    let meter_x = x0 + counter_width() + 4;
    let meter_w = x0 + cw - meter_x;
    if METER_STYLE() == "needles" {
        cmds.draw_needles(input, st, meter_x, y, meter_w);
    } else {
        cmds.draw_meter(input, st, meter_x, y, meter_w);
    }
    y += 8;
    let ts = section(st, 1);
    cmds.txt(x0, y, "▸", ts.pal.white_mid);
    cmds.draw_matrix_title(input, ts, x0 + 2, y, cw - 2, title_label(input));
    y += 3;
    let module_w = 26;
    let bw = clamp((cw - module_w - 8) / 25, 2, 5);
    let right = cmds.draw_analyzer(input, section(st, 2), bands, x0, y, bw, spec_h);
    let mx_ = right + mx((x0 + cw - right - module_w) / 2, 3);
    if mx_ + module_w <= x0 + cw && spec_h + 2 >= measure_height() {
        cmds.draw_measure(input, section(st, 3), mx_, y + (spec_h + 2 - measure_height()) / 2, module_w);
    }
    if input.show_clock { cmds.txt(W - input.clock.len() - 1, 0, input.clock, p.white_mid); }
    cmds
}

fn render_compact(input, st, bands) {
    let cmds = [];
    let p = st.pal;
    let W = input.width;
    let H = input.height;
    let top = 0;
    let clock_w = if input.show_clock { input.clock.len() + 2 } else { 0 };
    if H >= 18 {
        let end = cmds.draw_counter(input, st, 1, 0, 6, 2);
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
    if W < 42 || H < 10 { return render_compact(input, st, panel_bands(input, -8.0)); }
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
    cmds.seg_text(cx + 1, y + 2 * rh - 2, pad2(s), if big { 5 } else { 4 }, 1, p.white_mid, p.white_seg, false);
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

// Mode switch like a hardware display: section 0 (time, L/R meters) stays
// lit; the other sections are dark for a moment, then light up one after
// another with a brief overshoot as each comes on.
fn section(st, k) {
    if k == 0 || !("mode_t" in st) { return st; }
    let t = st.mode_t;
    let on_at = 0.12 + to_float(k) * 0.11;
    if t >= on_at + 0.08 { return st; }
    let s2 = st;
    let p = st.pal;
    let keys = ["white", "white_mid", "amber", "amber_low", "amber_hot", "peak", "amber_text",
                "amber_warm", "amber_light", "red", "red_mid", "cyan", "cyan_mid", "cyan_hot", "cyan_line"];
    if t < on_at {
        let ghost = #{ white: p.white_ghost, white_mid: p.white_ghost, amber: p.amber_ghost,
                       amber_low: p.amber_ghost, amber_hot: p.amber_ghost, peak: p.amber_ghost,
                       amber_text: p.amber_ghost, amber_warm: p.amber_ghost, amber_light: p.amber_ghost,
                       red: p.red_ghost, red_mid: p.red_ghost, cyan: p.cyan_ghost, cyan_mid: p.cyan_ghost,
                       cyan_hot: p.cyan_ghost, cyan_line: p.cyan_ghost };
        for key in keys { p[key] = ghost[key]; }
        // unlit lamps and rules go down with them, so nothing reads as lit
        p.lamp_off = p.white_ghost;
        p.red_lamp_off = p.red_ghost;
        p.amber_lamp_off = p.amber_ghost;
        p.cyan_lamp_off = p.cyan_ghost;
        p.amber_rule = p.amber_ghost;
    } else {
        for key in keys { p[key] = whiten(p[key], 0.35); }
    }
    s2.pal = p;
    s2
}

// Smallest screens for each composition (full needs a row more with the
// clock).
fn full_height(input) { if input.show_clock { 35 } else { 34 } }

fn mode_fits(mode, input) {
    let W = input.width; let H = input.height;
    switch mode {
        "full" => W >= 120 && H >= full_height(input),
        "spectrum" => W >= 100 && H >= 26,
        "minimal" => W >= 80 && H >= 20,
        "deck" => W >= 100 && H >= 30,
        _ => true
    }
}

fn mode_names() { ["full", "spectrum", "minimal", "deck"] }

// The display mode: Shift+M picks it unless DISPLAY_MODE() pins one; a mode
// that does not fit the window steps down. Earlier names still work.
fn pick_mode(input) {
    let mode = DISPLAY_MODE();
    mode = switch mode { "classic" => "full", "focus" => "spectrum", "tape" => "deck", _ => mode };
    if mode == "auto" {
        let names = mode_names();
        mode = names[get(input, "mode", 0) % names.len()];
    }
    if mode == "deck" && !mode_fits("deck", input) { mode = "spectrum"; }
    if mode == "full" && !mode_fits("full", input) { mode = "spectrum"; }
    if mode == "spectrum" && !mode_fits("spectrum", input) { mode = "minimal"; }
    if mode == "minimal" && !mode_fits("minimal", input) { mode = "compact"; }
    mode
}

// Bar inertia: a column follows a rise at once, but when the energy drops it
// starts falling slowly and gathers speed, like a real display settling. The
// peak marks have their own physics: a hold, then a fall that accelerates.
fn update_inertia(st, bands, dt) {
    let n = bands.len();
    let bars = if "bars" in st { st.bars } else { [] };
    let vel = if "vel" in st { st.vel } else { [] };
    if bars.len() != n { bars = []; vel = []; for i in 0..n { bars.push(0.0); vel.push(0.0); } }
    // peak marks: held for a moment, then they start falling slowly and
    // speed up, like a light object under gravity
    let caps = if "caps" in st { st.caps } else { [] };
    let cap_vel = if "cap_vel" in st { st.cap_vel } else { [] };
    let cap_hold = if "cap_hold" in st { st.cap_hold } else { [] };
    if caps.len() != n {
        caps = []; cap_vel = []; cap_hold = [];
        for i in 0..n { caps.push(0.0); cap_vel.push(0.0); cap_hold.push(0.0); }
    }
    let out = [];
    for i in 0..n {
        let v = bands[i][0];
        if v >= bars[i] {
            bars[i] = v; vel[i] = 0.0;
        } else {
            vel[i] = vel[i] + 2.4 * dt;
            bars[i] = mx(v, bars[i] - vel[i] * dt);
        }
        if bars[i] >= caps[i] {
            caps[i] = bars[i]; cap_vel[i] = 0.0; cap_hold[i] = 0.9;
        } else if cap_hold[i] > 0.0 {
            cap_hold[i] = cap_hold[i] - dt;
        } else {
            cap_vel[i] = cap_vel[i] + 1.6 * dt;
            caps[i] = mx(bars[i], caps[i] - cap_vel[i] * dt);
        }
        out.push([bars[i], caps[i]]);
    }
    st.bars = bars; st.vel = vel;
    st.caps = caps; st.cap_vel = cap_vel; st.cap_hold = cap_hold;
    st.shown = out;
    st
}

// For two seconds after Shift+M, a row of mode lamps along the bottom edge.
fn draw_mode_lamps(input, st, mode) {
    let p = st.pal;
    let names = ["FULL", "SPECTRUM", "MINIMAL", "DECK"];
    let keys = mode_names();
    let total = 9;
    for n in names { total += n.len() + 3; }
    let x = mx((input.width - total) / 2, 0);
    let y = input.height - 2;
    this.fill(x - 1, y, total + 2, 2, " ", 0);
    this.txt(x, y, "DISPLAY", p.white_mid);
    let cx = x + 9;
    for k in 0..names.len() {
        cx = this.lamp(cx, y, names[k], keys[k] == mode, p.white, p.lamp_off);
    }
}

fn render(input) {
    let st = advance_state(input);
    let dt = get(input, "dt", 0.05);
    st = update_range(st, input, dt);
    let raw = panel_bands(input, st.top_db);
    st = update_hold(st, raw, dt);
    st = update_inertia(st, raw, dt);
    let bands = st.shown;
    // the mode lamps light up for a moment on every Shift+M; the sections
    // switch over (mode_t, see section) only when the layout really changes,
    // not for a pinned DISPLAY_MODE() or a fallback to the same layout
    let mode = pick_mode(input);
    let mode_in = get(input, "mode", 0);
    if !("mode_seen" in st) { st.mode_seen = mode_in; st.mode_flash = 0.0; st.mode_shown = mode; }
    st.mode_flash -= dt;
    if "mode_t" in st {
        st.mode_t += dt;
        if st.mode_t > 1.5 { st.remove("mode_t"); }
    }
    if st.mode_seen != mode_in {
        st.mode_seen = mode_in;
        st.mode_flash = 2.0;
        if st.mode_shown != mode { st.mode_t = 0.0; }
    }
    st.mode_shown = mode;
    let cmds = [#{ op: "clear", bg: 0 }];
    let W = input.width; let H = input.height;
    if W >= 20 && H >= 6 {
        cmds.draw_noise(input, st);
        let body = if input.track_title == "" && st.boot <= 0.0 {
            render_standby(input, st)
        } else {
            switch mode {
                "full" => render_full(input, st, bands),
                "spectrum" => render_spectrum(input, st, bands),
                "minimal" => render_minimal(input, st, bands),
                "deck" => render_tape(input, st, bands),
                _ => render_compact(input, st, bands)
            }
        };
        for c in body { cmds.push(c); }
        if st.mode_flash > 0.0 && mode != "compact" && W >= 60 && H >= 8 { cmds.draw_mode_lamps(input, st, mode); }
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
