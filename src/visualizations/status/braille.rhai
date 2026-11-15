// name: Status braille histogram
// protocol: furumi-visualizer-v2
// surface: status
// revision: 2
//
// One-row waveform histogram. Input is decoded audio BEFORE output volume.
// The host freezes the last frame on pause, including custom scripts.
// Edit this file to customize it; only untouched bundled defaults upgrade.
// Each Braille character packs two independent four-dot vertical columns.
// "default" background retains the terminal's background color.

fn clamp(value, low, high) {
    if value < low { low } else if value > high { high } else { value }
}

// Local RMS for one slice of the recent waveform. Method syntax avoids
// copying the complete sample array for every column.
fn level(index, columns, reference) {
    let count = this.len();
    if count == 0 { return 0; }
    let start = index * count / columns;
    let end = (index + 1) * count / columns;
    let square = 0.0;
    let total = 0;
    for i in start..end {
        square += this[i] * this[i];
        total += 1;
    }
    let rms = if total > 0 { sqrt(square / to_float(total)) }
        else { abs(this[start]) };
    // Remove the near-silent baseline and spread useful levels over 0..4.
    // No additive bass/energy/beat floor: quiet columns really become empty.
    let normalized = clamp((rms / reference - 0.12) / 0.72, 0.0, 1.0);
    to_int(normalized * 4.0 + 0.5)
}

fn render(input) {
    let commands = [#{ op: "clear", bg: "default" }];
    if input.width <= 0 || input.height <= 0 { return commands; }
    let samples = input.samples;
    let square = 0.0;
    for sample in samples { square += sample * sample; }
    let rms = if samples.len() > 0 { sqrt(square / to_float(samples.len())) } else { 0.0 };
    // Adapt headroom to the current recording, with an absolute noise floor
    // so silence/very quiet passages are never normalized into a full row.
    let reference = if rms * 1.8 > 0.10 { rms * 1.8 } else { 0.10 };
    let dots = ["⠀⢀⢠⢰⢸", "⡀⣀⣠⣰⣸", "⡄⣄⣤⣴⣼", "⡆⣆⣦⣶⣾", "⡇⣇⣧⣷⣿"];
    let columns = input.width * 2;
    for x in 0..input.width {
        let left = samples.level(x * 2, columns, reference);
        let right = samples.level(x * 2 + 1, columns, reference);
        let glyph = dots[left].sub_string(right, 1);
        commands.push(#{ op: "cell", x: x, y: 0, ch: glyph,
            fg: 0x48c9c0, bg: "default" });
    }
    commands
}
