// name: Status braille histogram
// protocol: furumi-visualizer-v2
// surface: status
//
// Always-on, one-row visualization. Edit this file to customize it; saved
// changes reload during playback. The host never overwrites an existing file.
// Same render(input) and draw commands as fullscreen scripts; height is 1.
// Audio bands and waveform amplitudes drive the bars (not a full FFT).
// Each Braille character packs two independent four-dot vertical columns.
// "default" background retains the terminal's background color.

// A method reads the waveform through `this`, avoiding an array copy for
// every column in this continuously running visualization.
fn level(index, columns, band, energy, beat) {
    let wave = 0.0;
    let count = this.len();
    if count > 0 {
        let start = index * count / columns;
        let end = (index + 1) * count / columns;
        let total = 0;
        for i in start..end {
            wave += abs(this[i]);
            total += 1;
        }
        if total > 0 { wave /= to_float(total); }
        else { wave = abs(this[start]); }
    }
    let value = band * 0.55 + wave * 0.8 + energy * 0.12 + beat * 0.18;
    if value <= 0.01 { 0 }
    else if value < 0.20 { 1 }
    else if value < 0.40 { 2 }
    else if value < 0.65 { 3 }
    else { 4 }
}

fn render(input) {
    let commands = [#{ op: "clear", bg: "default" }];
    if input.width <= 0 || input.height <= 0 || input.paused { return commands; }
    // Lookup table indexed by left/right bar heights, from the bottom up.
    let dots = ["⠀⢀⢠⢰⢸", "⡀⣀⣠⣰⣸", "⡄⣄⣤⣴⣼", "⡆⣆⣦⣶⣾", "⡇⣇⣧⣷⣿"];
    let columns = input.width * 2;
    let samples = input.samples;
    for x in 0..input.width {
        let levels = [];
        for side in 0..2 {
            let index = x * 2 + side;
            let phase = to_float(index) / to_float(columns);
            let band = if phase < 0.33 { input.bass }
                else if phase < 0.67 { input.mid } else { input.treble };
            levels.push(samples.level(index, columns, band, input.energy, input.beat));
        }
        let glyph = dots[levels[0]].sub_string(levels[1], 1);
        commands.push(#{ op: "cell", x: x, y: 0, ch: glyph,
            fg: 0x48c9c0, bg: "default" });
    }
    commands
}
