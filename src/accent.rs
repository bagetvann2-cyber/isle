//! Dominant vibrant colour of a cover, computed once when the cover is decoded.

const BINS: usize = 24; // 15 degrees of hue each

/// (hue 0..360, saturation 0..1, value 0..1) of an 8-bit RGB colour.
fn hsv(r: f32, g: f32, b: f32) -> (f32, f32, f32) {
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let d = mx - mn;
    let h = if d == 0.0 {
        0.0
    } else if mx == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if mx == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h, if mx == 0.0 { 0.0 } else { d / mx }, mx)
}

fn hsl_to_rgb(h: f32, s: f32, l: f32) -> [u8; 3] {
    let c = (1.0 - (2.0 * l - 1.0).abs()) * s;
    let x = c * (1.0 - ((h / 60.0) % 2.0 - 1.0).abs());
    let (r, g, b) = match (h / 60.0) as u32 % 6 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = l - c / 2.0;
    [r, g, b].map(|v| ((v + m) * 255.0).round().clamp(0.0, 255.0) as u8)
}

/// Most vibrant dominant hue of a premultiplied BGRA image, lifted so it reads on black.
/// `None` for (near) greyscale, black or white images.
pub fn pick(bgra: &[u8]) -> Option<[u8; 3]> {
    let mut w = [0.0f32; BINS];
    let mut sum = [[0.0f32; 3]; BINS];
    let (mut total, mut used) = (0usize, 0usize);
    for p in bgra.as_chunks::<4>().0 {
        total += 1;
        if p[3] < 128 {
            continue;
        }
        let a = p[3] as f32;
        let (r, g, b) = (p[2] as f32 * 255.0 / a, p[1] as f32 * 255.0 / a, p[0] as f32 * 255.0 / a);
        let (h, s, v) = hsv(r / 255.0, g / 255.0, b / 255.0);
        if v < 0.2 || s < 0.25 {
            continue; // near black, grey or white
        }
        let (bin, wt) = ((h / (360.0 / BINS as f32)) as usize % BINS, s * v);
        used += 1;
        w[bin] += wt;
        for (acc, c) in sum[bin].iter_mut().zip([r, g, b]) {
            *acc += c * wt;
        }
    }
    if used * 100 < total {
        return None; // under 1% of the image is colourful
    }
    // A hue family is a bin plus its neighbours: smooths over a gradient split between bins.
    let score = |i: usize| w[i] + 0.5 * (w[(i + BINS - 1) % BINS] + w[(i + 1) % BINS]);
    let best = (0..BINS).max_by(|&a, &b| score(a).total_cmp(&score(b)))?;
    let (mut acc, mut wt) = ([0.0f32; 3], 0.0f32);
    for i in [(best + BINS - 1) % BINS, best, (best + 1) % BINS] {
        wt += w[i];
        for k in 0..3 {
            acc[k] += sum[i][k];
        }
    }
    if wt <= 0.0 {
        return None;
    }
    let [r, g, b] = acc.map(|c| c / wt / 255.0);
    let h = hsv(r, g, b).0;
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let l = (mx + mn) / 2.0;
    let s = if mx == mn { 0.0 } else { (mx - mn) / (1.0 - (2.0 * l - 1.0).abs()) };
    Some(hsl_to_rgb(h, s.clamp(0.5, 1.0), l.clamp(0.6, 0.72)))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Premultiplied BGRA image from a per-pixel (r, g, b) function over a 40 x 40 grid.
    fn img(f: impl Fn(usize, usize) -> [u8; 3]) -> Vec<u8> {
        (0..40 * 40)
            .flat_map(|i| {
                let [r, g, b] = f(i % 40, i / 40);
                [b, g, r, 255]
            })
            .collect()
    }

    fn hue(c: [u8; 3]) -> f32 {
        hsv(c[0] as f32 / 255.0, c[1] as f32 / 255.0, c[2] as f32 / 255.0).0
    }

    #[test]
    fn solid_blue_is_blue_and_visible() {
        let c = pick(&img(|_, _| [20, 60, 200])).unwrap();
        assert!((200.0..240.0).contains(&hue(c)), "{c:?}");
        assert!(c.iter().map(|&v| v as u32).sum::<u32>() > 250, "lifted for black: {c:?}");
    }

    #[test]
    fn black_with_red_patch_is_red() {
        let c = pick(&img(|x, y| if x < 12 && y < 12 { [200, 20, 30] } else { [5, 5, 5] })).unwrap();
        let h = hue(c);
        assert!(!(15.0..345.0).contains(&h), "{c:?} hue {h}");
    }

    #[test]
    fn grey_black_white_have_no_accent() {
        assert_eq!(pick(&img(|x, _| [(x * 6) as u8; 3])), None);
        assert_eq!(pick(&img(|_, _| [0, 0, 0])), None);
        assert_eq!(pick(&img(|_, _| [255, 255, 255])), None);
    }

    #[test]
    fn vibrant_beats_larger_dull_area() {
        // 70% muted brownish-grey, 30% saturated green: green wins.
        let c = pick(&img(|x, _| if x < 12 { [30, 220, 60] } else { [120, 110, 100] })).unwrap();
        assert!((100.0..150.0).contains(&hue(c)), "{c:?}");
    }
}
