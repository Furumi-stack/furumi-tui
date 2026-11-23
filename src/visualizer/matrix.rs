//! VFD dot-matrix glyphs encoded as two rows of Braille cells.
use rhai::{Array, Dynamic};
use unicode_width::UnicodeWidthChar;

pub(super) fn matrix_text(text: &str, width: i64) -> Array {
    let limit = (width.clamp(0, 512) / 3) as usize;
    let mut top = String::new();
    let mut bottom = String::new();
    for (index, ch) in text.chars().enumerate() {
        if index >= limit {
            return Vec::new();
        }
        let mut uppercase = ch.to_uppercase();
        let Some(first) = uppercase.next() else {
            return Vec::new();
        };
        if uppercase.next().is_some() {
            return Vec::new();
        }
        let Some(glyph) = glyph(first) else {
            return Vec::new();
        };
        let mut cells = [[0; 3]; 2];
        for (y, row) in glyph.into_iter().enumerate() {
            for x in 0..5 {
                if row & (16 >> x) != 0 {
                    cells[y / 4][x / 2] |= super::BRAILLE_BITS[y % 4][x % 2];
                }
            }
        }
        for (first, second) in cells[0].into_iter().zip(cells[1]) {
            top.push(char::from_u32(super::BRAILLE_BASE | first).unwrap_or(' '));
            bottom.push(char::from_u32(super::BRAILLE_BASE | second).unwrap_or(' '));
        }
    }
    vec![Dynamic::from(top), Dynamic::from(bottom)]
}

/// Fit the largest block glyphs that leave room for the surrounding UI.
pub(super) fn lyric_layout(text: &str, width: i64, height: i64) -> Array {
    let words: Vec<_> = text.split_whitespace().collect();
    if words.len() >= 3 && width >= 60 && height >= 16 {
        // Pick a substantial word that fits the headline scale. Keep reading order.
        let focus = words
            .iter()
            .enumerate()
            .filter(|(_, word)| word.chars().count() as i64 * 9 <= width - 8)
            .max_by_key(|(index, word)| (word.chars().count(), usize::MAX - index));
        if let Some((index, word)) = focus {
            let headline = lyric_blocks(word, width - 8, 11, 3);
            let before = words[..index].join(" ");
            let after = words[index + 1..].join(" ");
            let supporting = |phrase: &str| lyric_blocks(phrase, width - 8, 9, 1);
            let above = supporting(&before);
            let below = supporting(&after);
            let fits = !headline.is_empty()
                && (before.is_empty() || !above.is_empty())
                && (after.is_empty() || !below.is_empty())
                && (headline.len() + above.len() + below.len() + 2) as i64 <= height;
            if fits {
                return [above, headline, below]
                    .into_iter()
                    .filter(|rows| !rows.is_empty())
                    .map(Dynamic::from)
                    .collect();
            }
        }
    }
    let rows = lyric_blocks(text, width, height, 3);
    if rows.is_empty() {
        Vec::new()
    } else {
        vec![Dynamic::from(rows)]
    }
}

fn lyric_blocks(text: &str, width: i64, height: i64, max_scale: usize) -> Array {
    let width = width.clamp(0, 512) as usize;
    let height = height.clamp(0, 32) as usize;
    let Some(words) = text
        .split_whitespace()
        .map(|word| {
            word.chars()
                .map(|ch| {
                    let mut uppercase = ch.to_uppercase();
                    let first = uppercase.next()?;
                    if uppercase.next().is_some() {
                        return None;
                    }
                    glyph(first)
                })
                .collect::<Option<Vec<_>>>()
        })
        .collect::<Option<Vec<_>>>()
    else {
        return Vec::new();
    };
    if words.is_empty() {
        return Vec::new();
    }
    // 2×2 pixels per terminal cell, with a blank pixel column per letter.
    const BLOCKS: [char; 16] = [
        ' ', '▘', '▝', '▀', '▖', '▌', '▞', '▛', '▗', '▚', '▐', '▜', '▄', '▙', '▟', '█',
    ];
    for scale in (1..=max_scale).rev() {
        let columns = width / (3 * scale);
        let rows = (7 * scale).div_ceil(2);
        let max_lines = ((height + 1) / (rows + 1)).min(3);
        if max_lines == 0 {
            continue;
        }
        let mut lines: Vec<Vec<[u8; 7]>> = vec![Vec::new()];
        for word in &words {
            let line = lines.last_mut().unwrap();
            if word.len() > columns {
                lines.clear();
                break;
            }
            if line.is_empty() {
                line.extend(word);
            } else if line.len() + word.len() < columns {
                line.push([0; 7]);
                line.extend(word);
            } else {
                lines.push(word.clone());
            }
        }
        if lines.is_empty() || lines.len() > max_lines {
            continue;
        }

        let mut result = Vec::new();
        for line in lines {
            if !result.is_empty() {
                result.push(Dynamic::from(String::new()));
            }
            for y in 0..rows {
                let mut row = String::new();
                for letter in &line {
                    for x in 0..3 * scale {
                        let mut mask = 0;
                        for dy in 0..2 {
                            let pixel_y = (2 * y + dy) / scale;
                            for dx in 0..2 {
                                let pixel_x = (2 * x + dx) / scale;
                                if pixel_y < 7 && letter[pixel_y] & (32 >> (pixel_x + 1)) != 0 {
                                    mask |= 1 << (2 * dy + dx);
                                }
                            }
                        }
                        row.push(BLOCKS[mask]);
                    }
                }
                result.push(Dynamic::from(row));
            }
        }
        return result;
    }
    Vec::new()
}

pub(super) fn cut_cells(text: &str, skip: i64, width: i64) -> String {
    let start = skip.max(0) as usize;
    let end = start.saturating_add(width.clamp(0, 512) as usize);
    let mut position = 0_usize;
    let mut result = String::new();
    for ch in text.chars() {
        let width = ch.width().unwrap_or_default();
        let next = position.saturating_add(width);
        if next > end {
            break;
        }
        if position >= start && (width > 0 || !result.is_empty()) {
            result.push(ch);
        } else if next > start && position < start {
            result.extend(std::iter::repeat_n(' ', next - start));
        }
        position = next;
    }
    result
}

fn glyph(ch: char) -> Option<[u8; 7]> {
    Some(match ch {
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
        _ => return None,
    })
}
