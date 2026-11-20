//! Text colours: the 16 named colours, `#rrggbb`, the nearest named colour
//! for clients without hex colours (before 1.16) and gradient maths.

/// The named colours in legacy code order (`§0` to `§f`), with their RGB.
pub const NAMED: [(&str, u32); 16] = [
    ("black", 0x000000),
    ("dark_blue", 0x0000aa),
    ("dark_green", 0x00aa00),
    ("dark_aqua", 0x00aaaa),
    ("dark_red", 0xaa0000),
    ("dark_purple", 0xaa00aa),
    ("gold", 0xffaa00),
    ("gray", 0xaaaaaa),
    ("dark_gray", 0x555555),
    ("blue", 0x5555ff),
    ("green", 0x55ff55),
    ("aqua", 0x55ffff),
    ("red", 0xff5555),
    ("light_purple", 0xff55ff),
    ("yellow", 0xffff55),
    ("white", 0xffffff),
];

/// The RGB of a `#rrggbb` colour (either case), as `TextColor` parses it.
pub fn hex(text: &str) -> Option<u32> {
    let digits = text.strip_prefix('#')?;
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    u32::from_str_radix(digits, 16).ok()
}

/// The RGB of a colour a component may name: one of the 16 names or `#rrggbb`.
pub fn parse(text: &str) -> Option<u32> {
    NAMED
        .iter()
        .find(|(name, _)| *name == text)
        .map(|(_, rgb)| *rgb)
        .or_else(|| hex(text))
}

/// The legacy code (`0`-`9`, `a`-`f`) of a named colour.
pub fn code(name: &str) -> Option<char> {
    let index = NAMED.iter().position(|(named, _)| *named == name)?;
    char::from_digit(index as u32, 16)
}

/// The named colour of a legacy code, in either case.
pub fn from_code(code: char) -> Option<&'static str> {
    let index = code.to_digit(16)?;
    Some(NAMED[index as usize].0)
}

/// The named colour closest to `rgb`, by a weighted RGB distance ("redmean")
/// that tracks perceived difference better than plain Euclidean distance.
pub fn nearest(rgb: u32) -> &'static str {
    let split = |rgb: u32| {
        (
            ((rgb >> 16) & 0xff) as i64,
            ((rgb >> 8) & 0xff) as i64,
            (rgb & 0xff) as i64,
        )
    };
    let (r, g, b) = split(rgb);
    NAMED
        .iter()
        .min_by_key(|(_, named)| {
            let (nr, ng, nb) = split(*named);
            let mean = (r + nr) / 2;
            let (dr, dg, db) = (r - nr, g - ng, b - nb);
            (((512 + mean) * dr * dr) >> 8) + 4 * dg * dg + (((767 - mean) * db * db) >> 8)
        })
        .map(|(name, _)| *name)
        .unwrap_or("white")
}

/// `#rrggbb` for `rgb`.
pub fn to_hex(rgb: u32) -> String {
    format!("#{:06x}", rgb & 0xff_ffff)
}

/// The colour `t` (0..=1) of the way along `stops`, interpolated linearly
/// in RGB between neighbouring stops.
pub fn gradient(stops: &[u32], t: f64) -> u32 {
    match stops {
        [] => 0xffffff,
        [only] => *only,
        _ => {
            let t = t.clamp(0.0, 1.0) * (stops.len() - 1) as f64;
            let index = (t.floor() as usize).min(stops.len() - 2);
            let local = t - index as f64;
            lerp(stops[index], stops[index + 1], local)
        }
    }
}

/// The colour `t` (0..=1) of the way along `stops`, shifted by `phase`
/// (0..=1), as MiniMessage's gradient does it: the position is
/// `(t + phase) * (stops - 1)` stops along, and past the last stop it
/// continues from the last stop back to the first, then on from the first.
/// With phase 0 this is exactly [`gradient`].
pub fn gradient_shifted(stops: &[u32], t: f64, phase: f64) -> u32 {
    if phase == 0.0 || stops.len() < 2 {
        return gradient(stops, t);
    }
    let count = stops.len();
    let position = (t.clamp(0.0, 1.0) + phase.clamp(0.0, 1.0)) * (count - 1) as f64;
    let low = position.floor();
    let high = position.ceil() as usize % count;
    lerp(stops[low as usize % count], stops[high], position - low)
}

/// The colour `local` (0..=1) of the way from `from` to `to`, per channel in
/// RGB, rounded.
fn lerp(from: u32, to: u32, local: f64) -> u32 {
    let channel = |shift: u32| {
        let a = f64::from((from >> shift) & 0xff);
        let b = f64::from((to >> shift) & 0xff);
        ((a + (b - a) * local).round() as u32) << shift
    };
    channel(16) | channel(8) | channel(0)
}

/// The colour at `t` (0..=1) of a rainbow starting at hue `phase` (-1..=1;
/// a negative phase is the same hue as `1 + phase`).
pub fn rainbow(t: f64, phase: f64) -> u32 {
    let hue = (t + phase).rem_euclid(1.0) * 6.0;
    let x = 1.0 - (hue % 2.0 - 1.0).abs();
    let (r, g, b) = match hue as u32 {
        0 => (1.0, x, 0.0),
        1 => (x, 1.0, 0.0),
        2 => (0.0, 1.0, x),
        3 => (0.0, x, 1.0),
        4 => (x, 0.0, 1.0),
        _ => (1.0, 0.0, x),
    };
    let byte = |value: f64| (value * 255.0).round() as u32;
    byte(r) << 16 | byte(g) << 8 | byte(b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_hex_parse_and_others_do_not() {
        assert_eq!(parse("gold"), Some(0xffaa00));
        assert_eq!(parse("#FFaa00"), Some(0xffaa00));
        for invalid in [
            "orange", "Gold", "#fff", "#gggggg", "ffaa00", "#ffaa000", "#+faa00",
        ] {
            assert_eq!(parse(invalid), None, "{invalid}");
        }
    }

    #[test]
    fn codes_map_both_ways() {
        assert_eq!(code("gold"), Some('6'));
        assert_eq!(code("white"), Some('f'));
        assert_eq!(from_code('F'), Some("white"));
        assert_eq!(from_code('g'), None);
    }

    #[test]
    fn nearest_picks_the_closest_named_colour() {
        for (name, rgb) in NAMED {
            assert_eq!(nearest(rgb), name);
        }
        assert_eq!(nearest(0xff8800), "gold");
        assert_eq!(nearest(0xfe5050), "red");
        assert_eq!(nearest(0x101010), "black");
        assert_eq!(nearest(0x7a7a7a), "dark_gray");
    }

    #[test]
    fn gradients_interpolate_between_stops() {
        let stops = [0xff0000, 0x0000ff];
        assert_eq!(gradient(&stops, 0.0), 0xff0000);
        assert_eq!(gradient(&stops, 1.0), 0x0000ff);
        assert_eq!(gradient(&stops, 0.5), 0x800080);
        assert_eq!(gradient(&[0xff0000, 0x00ff00, 0x0000ff], 0.5), 0x00ff00);
        assert_eq!(gradient(&[0x123456], 0.7), 0x123456);
        assert_eq!(gradient_shifted(&stops, 0.5, 0.0), gradient(&stops, 0.5));
        // Shifted past the last stop, a gradient runs back to the first.
        assert_eq!(gradient_shifted(&stops, 0.0, 0.5), 0x800080);
        assert_eq!(gradient_shifted(&stops, 0.5, 0.5), 0x0000ff);
        assert_eq!(gradient_shifted(&stops, 1.0, 0.5), 0x800080);
        assert_eq!(gradient_shifted(&stops, 1.0, 1.0), 0xff0000);
        assert_eq!(rainbow(0.0, 0.0), 0xff0000);
        assert_eq!(rainbow(1.0 / 3.0, 0.0), 0x00ff00);
    }
}
