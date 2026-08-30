//! Procedural character art.
//!
//! Every character is an explicit recipe — skin, hair colour and style, garment,
//! face — layered onto an 18×28 buffer. Nothing is a sprite sheet, which is the
//! fact the whole theme engine rests on: **a new cast is a data table, not
//! commissioned art.**
//!
//! Ported from `portraitArt.ts`. The layer order and the exact pixel
//! coordinates are kept, because they are what make a face read as a face at
//! this size; a "cleaner" rewrite would just be different art.
//!
//! Recipes arrive as data (see `theme.rs`), so the painter never names a
//! character. That is the difference between a themed renderer and one with a
//! second theme bolted on.

use serde::Deserialize;

pub const W: usize = 18;
pub const H: usize = 28;
/// Head skin columns. Every feature is placed relative to these.
const HX0: i32 = 4;
const HX1: i32 = 13;
const OUTLINE: Rgb = [38, 34, 46];

pub type Rgb = [u8; 3];

/// One character, as data. Unknown fields are rejected rather than ignored: a
/// typo in a theme file should be a visible error, not a silently missing hat.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub skin: String,
    pub hairc: Rgb,
    pub hair: String,
    #[serde(default)]
    pub part: Option<String>,
    #[serde(default)]
    pub recede: Option<i32>,
    #[serde(default)]
    pub length: Option<i32>,
    #[serde(default)]
    pub vol: Option<i32>,
    pub cloth: String,
    pub c1: Rgb,
    #[serde(default)]
    pub c2: Option<Rgb>,
    #[serde(default)]
    pub tie: Option<Rgb>,
    #[serde(default)]
    pub brow: Option<String>,
    #[serde(default)]
    pub mouth: Option<String>,
    #[serde(default)]
    pub blush: bool,
    #[serde(default)]
    pub facial: Option<String>,
    #[serde(default)]
    pub glasses: bool,
    /// Bigger, lashed eyes for a more expressive face.
    #[serde(default)]
    pub lashes: bool,
    /// Heavier build: a wider torso and a fuller face.
    #[serde(default)]
    pub heavy: bool,
}

struct SkinPal {
    hi: Rgb,
    base: Rgb,
    sh: Rgb,
    line: Rgb,
}

fn skin_pal(name: &str) -> SkinPal {
    match name {
        "tan" => SkinPal { hi: [232, 182, 136], base: [214, 162, 116], sh: [176, 126, 86], line: [138, 92, 60] },
        "brown" => SkinPal { hi: [180, 130, 94], base: [158, 112, 78], sh: [124, 86, 58], line: [90, 60, 40] },
        "dark" => SkinPal { hi: [142, 98, 70], base: [120, 80, 56], sh: [94, 62, 42], line: [64, 42, 28] },
        // Light is the fallback as well as a value: an unknown skin name paints
        // a person rather than nothing.
        _ => SkinPal { hi: [255, 221, 189], base: [247, 201, 170], sh: [212, 158, 126], line: [168, 112, 82] },
    }
}

fn clamp(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// Highlight, base and shadow from one colour. Shading is derived rather than
/// authored so a theme supplies one colour per garment, not three.
fn shades(c: Rgb) -> [Rgb; 3] {
    shades_with(c, 1.22, 0.68)
}

fn shades_with(c: Rgb, dl: f32, dd: f32) -> [Rgb; 3] {
    [
        [clamp(c[0] as f32 * dl), clamp(c[1] as f32 * dl), clamp(c[2] as f32 * dl)],
        c,
        [clamp(c[0] as f32 * dd), clamp(c[1] as f32 * dd), clamp(c[2] as f32 * dd)],
    ]
}

/// An RGBA buffer with the drawing primitives the recipes are written against.
pub struct Canvas {
    pub buf: Vec<u8>,
    w: i32,
    h: i32,
}

impl Canvas {
    pub fn new(w: usize, h: usize) -> Self {
        Self { buf: vec![0; w * h * 4], w: w as i32, h: h as i32 }
    }

    /// Out-of-bounds writes are dropped rather than clamped. Recipes routinely
    /// draw past an edge (hair at the temples), and clamping would smear the
    /// last column instead of letting the stroke end.
    fn set(&mut self, x: i32, y: i32, c: Rgb) {
        self.set_a(x, y, c, 255)
    }

    fn set_a(&mut self, x: i32, y: i32, c: Rgb, a: u8) {
        if x < 0 || x >= self.w || y < 0 || y >= self.h {
            return;
        }
        let i = ((y * self.w + x) * 4) as usize;
        self.buf[i] = c[0];
        self.buf[i + 1] = c[1];
        self.buf[i + 2] = c[2];
        self.buf[i + 3] = a;
    }

    fn alpha_at(&self, x: i32, y: i32) -> u8 {
        if x < 0 || x >= self.w || y < 0 || y >= self.h {
            return 0;
        }
        self.buf[((y * self.w + x) * 4 + 3) as usize]
    }

    fn rgb_at(&self, x: i32, y: i32) -> Rgb {
        if x < 0 || x >= self.w || y < 0 || y >= self.h {
            return [0, 0, 0];
        }
        let i = ((y * self.w + x) * 4) as usize;
        [self.buf[i], self.buf[i + 1], self.buf[i + 2]]
    }

    fn rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, c: Rgb) {
        for y in y0..=y1 {
            for x in x0..=x1 {
                self.set(x, y, c);
            }
        }
    }
}

fn draw_head(cv: &mut Canvas, skin: &str) {
    let s = skin_pal(skin);
    for y in 4..=16 {
        for x in HX0..=HX1 {
            // Corners are cut so the skull reads as rounded rather than a block.
            if ((x == HX0 || x == HX1) && (y == 4 || y == 5 || y == 16)) || ((x == 5 || x == 12) && y == 4) {
                continue;
            }
            cv.set(x, y, s.base);
        }
    }
    for y in 6..12 {
        cv.set(5, y, s.hi);
    }
    cv.set(6, 5, s.hi);
    cv.set(7, 5, s.hi);
    for y in 6..15 {
        cv.set(12, y, s.sh);
    }
    for x in [7, 8, 9, 10, 11] {
        cv.set(x, 16, s.sh);
    }
    for ex in [HX0 - 1, HX1 + 1] {
        cv.set(ex, 9, s.base);
        cv.set(ex, 10, s.base);
        cv.set(ex, 11, s.sh);
    }
    cv.rect(7, 17, 10, 18, s.sh);
    cv.rect(7, 17, 9, 17, s.base);
}

fn draw_face(cv: &mut Canvas, r: &Recipe) {
    let s = skin_pal(&r.skin);
    let white: Rgb = [250, 248, 244];
    let pup: Rgb = [46, 38, 42];
    for (a, b, p) in [(5, 6, 6), (10, 11, 10)] {
        cv.set(a, 9, white);
        cv.set(b, 9, white);
        cv.set(p, 9, pup);
    }
    if r.lashes {
        let lash: Rgb = [54, 40, 48];
        let glint: Rgb = [252, 250, 248];
        for x in [5, 6, 10, 11] {
            cv.set(x, 8, lash);
        }
        cv.set(4, 8, lash);
        cv.set(12, 8, lash);
        cv.set(5, 9, glint);
        cv.set(10, 9, glint);
    }
    match r.brow.as_deref().unwrap_or("flat") {
        "angry" => {
            cv.set(5, 8, s.line);
            cv.set(6, 7, s.line);
            cv.set(10, 7, s.line);
            cv.set(11, 8, s.line);
        }
        "raised" => {
            for x in [5, 6, 10, 11] {
                cv.set(x, 6, s.line);
            }
        }
        "soft" => {
            for x in [5, 11] {
                cv.set(x, 7, s.line);
            }
            for x in [6, 10] {
                cv.set(x, 7, s.sh);
            }
        }
        _ => {
            for x in [5, 6, 10, 11] {
                cv.set(x, 7, s.line);
            }
        }
    }
    // Nose.
    cv.set(8, 11, s.sh);
    cv.set(8, 12, s.sh);
    cv.set(7, 12, s.sh);

    let mc: Rgb = [158, 86, 80];
    let mouth: &[(i32, i32)] = match r.mouth.as_deref().unwrap_or("neutral") {
        "smile" => &[(7, 14), (8, 14), (9, 14), (10, 14), (6, 13), (11, 13)],
        "frown" => &[(7, 15), (8, 15), (9, 15), (10, 15), (6, 14), (11, 14)],
        "grin" => &[(7, 14), (8, 14), (9, 14), (10, 14), (7, 13), (8, 13), (9, 13), (10, 13), (6, 13), (11, 13)],
        _ => &[(7, 14), (8, 14), (9, 14), (10, 14)],
    };
    for (x, y) in mouth {
        cv.set(*x, *y, mc);
    }
    if r.blush {
        for x in [5, 12] {
            cv.set_a(x, 12, [235, 150, 140], 140);
        }
    }
}

fn draw_hair(cv: &mut Canvas, r: &Recipe) {
    let color = r.hairc;
    let [hi, base, sh] = shades(color);
    let skin_base = skin_pal(&r.skin).base;

    match r.hair.as_str() {
        "styleFloppy" => {
            cv.rect(HX0, 2, HX1, 4, base);
            for x in HX0 - 1..=HX1 + 1 {
                cv.set(x, 3, base);
            }
            cv.rect(HX0 - 1, 4, HX1 + 1, 5, base);
            for x in HX0..=HX1 {
                cv.set(x, 5, base);
            }
            for x in 6..=12 {
                cv.set(x, 6, base);
            }
            for x in [9, 10, 11] {
                cv.set(x, 7, base);
            }
            for y in 6..9 {
                for x in [HX0 - 1, HX0, HX1, HX1 + 1] {
                    cv.set(x, y, base);
                }
            }
            for x in HX0..=HX1 {
                if cv.alpha_at(x, 2) > 0 {
                    cv.set(x, 2, hi);
                }
            }
            for x in [7, 8, 9] {
                cv.set(x, 6, hi);
            }
        }
        "styleFrame" => {
            let length = r.length.unwrap_or(17);
            let vol = r.vol.unwrap_or(1);
            cv.rect(HX0 - 1, 2, HX1 + 1, 5, base);
            for x in HX0 - 1..=HX1 + 1 {
                cv.set(x, 3, base);
            }
            for x in HX0..=HX1 {
                cv.set(x, 5, base);
            }
            for x in 6..12 {
                cv.set(x, 6, base);
            }
            cv.set(8, 6, skin_base);
            cv.set(9, 6, skin_base);
            for y in 6..=length {
                for dx in 0..vol {
                    cv.set(HX0 - 1 - dx, y, base);
                    cv.set(HX1 + 1 + dx, y, base);
                }
                cv.set(HX0, y, base);
                cv.set(HX1, y, base);
            }
            for x in HX0 - 1..HX0 + 1 {
                cv.set(x, length + 1, base);
            }
            for x in HX1..HX1 + 2 {
                cv.set(x, length + 1, base);
            }
            for y in 2..6 {
                if cv.alpha_at(HX1, y) > 0 {
                    cv.set(HX1, y, sh);
                }
            }
            for x in HX0..9 {
                if cv.alpha_at(x, 2) > 0 {
                    cv.set(x, 2, hi);
                }
            }
        }
        "styleBun" => {
            cv.rect(HX0, 3, HX1, 5, base);
            for x in HX0 - 1..=HX1 + 1 {
                cv.set(x, 4, base);
            }
            for x in HX0..=HX1 {
                cv.set(x, 5, base);
            }
            for x in 6..12 {
                cv.set(x, 6, base);
            }
            cv.set(8, 6, skin_base);
            cv.set(9, 6, skin_base);
            for y in 6..9 {
                cv.set(HX0, y, base);
                cv.set(HX1, y, base);
            }
            cv.rect(7, 1, 10, 2, base);
            for x in HX0..=HX1 {
                if cv.alpha_at(x, 3) > 0 {
                    cv.set(x, 3, hi);
                }
            }
        }
        "styleCurly" => {
            let pts: [(i32, i32); 25] = [
                (4, 3), (5, 2), (6, 3), (7, 2), (8, 3), (9, 2), (10, 3), (11, 2), (12, 3), (13, 3),
                (3, 4), (4, 4), (13, 4), (14, 4), (3, 5), (4, 5), (13, 5), (14, 5), (3, 6), (13, 6),
                (4, 6), (12, 6), (3, 7), (13, 7), (4, 7),
            ];
            cv.rect(HX0, 3, HX1, 5, base);
            for x in HX0 - 1..=HX1 + 1 {
                cv.set(x, 4, base);
            }
            for (x, y) in pts {
                cv.set(x, y, base);
            }
            for x in 6..12 {
                cv.set(x, 6, base);
            }
            cv.set(8, 6, skin_base);
            cv.set(9, 6, skin_base);
            for (x, y) in [(5, 2), (7, 2), (9, 2), (11, 2)] {
                cv.set(x, y, hi);
            }
        }
        "styleMessy" => {
            let length = r.length.unwrap_or(8);
            cv.rect(HX0 - 1, 2, HX1 + 1, 5, base);
            let spikes: [(i32, i32); 9] =
                [(3, 2), (5, 1), (7, 2), (9, 1), (11, 2), (13, 1), (14, 2), (4, 2), (12, 2)];
            for (x, y) in spikes {
                cv.set(x, y, base);
            }
            for x in HX0..=HX1 {
                cv.set(x, 5, base);
            }
            for x in 6..12 {
                cv.set(x, 6, base);
            }
            cv.set(8, 6, skin_base);
            cv.set(9, 6, skin_base);
            for y in 6..=length {
                for x in [HX0 - 1, HX0, HX1, HX1 + 1] {
                    cv.set(x, y, base);
                }
            }
            for (x, y) in spikes {
                cv.set(x, y, hi);
            }
        }
        "styleRecede" => {
            for y in 4..10 {
                for x in [HX0 - 1, HX0, HX1, HX1 + 1] {
                    cv.set(x, y, base);
                }
            }
            for x in HX0..=HX1 {
                cv.set(x, 4, base);
            }
            for x in HX0 + 1..HX1 {
                cv.set(x, 5, base);
            }
            // Carve the forehead back out of the hair mass.
            for y in 5..9 {
                for x in 6..12 {
                    if cv.rgb_at(x, y) == base {
                        cv.set(x, y, skin_base);
                    }
                }
            }
            for x in HX0..=HX1 {
                if cv.alpha_at(x, 4) > 0 {
                    cv.set(x, 4, sh);
                }
            }
        }
        "styleSpiky" => {
            cv.rect(HX0, 3, HX1, 5, base);
            for x in HX0 - 1..=HX1 + 1 {
                cv.set(x, 4, base);
            }
            for x in HX0..=HX1 {
                cv.set(x, 5, base);
            }
            let spikes: [(i32, i32); 8] =
                [(5, 2), (7, 1), (9, 2), (11, 1), (6, 2), (8, 2), (10, 2), (12, 2)];
            for (x, y) in spikes {
                cv.set(x, y, base);
            }
            for x in 6..12 {
                cv.set(x, 6, base);
            }
            cv.set(8, 6, skin_base);
            cv.set(9, 6, skin_base);
            for y in 6..8 {
                cv.set(HX0, y, base);
                cv.set(HX1, y, base);
            }
            for (x, y) in spikes {
                cv.set(x, y, hi);
            }
        }
        "styleBald" => {
            // A rounded skin crown with a sheen, and hair only as a low
            // horseshoe — a bald head is a shape, not an absence.
            let [shi, sbase, ssh] = shades_with(skin_base, 1.1, 0.82);
            for x in 6..=11 {
                cv.set(x, 2, sbase);
            }
            for x in 5..=12 {
                cv.set(x, 3, sbase);
            }
            for x in HX0..=HX1 {
                cv.set(x, 4, sbase);
            }
            for x in [7, 8, 9] {
                cv.set(x, 2, shi);
            }
            cv.set(6, 3, shi);
            cv.set(7, 3, shi);
            cv.set(5, 3, ssh);
            cv.set(12, 3, ssh);
            cv.set(HX1, 4, ssh);

            let top = if r.recede.unwrap_or(0) != 0 { 8 } else { 6 };
            for y in top..=10 {
                cv.set(HX0 - 1, y, sh);
                cv.set(HX0, y, base);
                cv.set(HX1, y, base);
                cv.set(HX1 + 1, y, sh);
            }
        }
        // styleShort is the default: an unknown style still paints hair.
        _ => {
            let part_left = r.part.as_deref() != Some("R");
            cv.rect(HX0, 2, HX1, 4, base);
            for x in HX0 - 1..=HX1 + 1 {
                cv.set(x, 3, base);
            }
            cv.rect(HX0 - 1, 4, HX1 + 1, 5, base);
            for y in 6..9 {
                for x in [HX0 - 1, HX0, HX1, HX1 + 1] {
                    cv.set(x, y, base);
                }
            }
            for x in HX0..=HX1 {
                cv.set(x, 5, base);
            }
            if r.recede.unwrap_or(0) != 0 {
                for y in 3..6 {
                    for x in 6..12 {
                        if cv.rgb_at(x, y) == base {
                            cv.set(x, y, skin_base);
                        }
                    }
                }
                cv.set(8, 5, base); // widow's peak
            }
            let hx = if part_left { 6 } else { 11 };
            for y in 2..6 {
                cv.set(hx, y, sh);
            }
            for x in HX0..hx {
                if cv.alpha_at(x, 3) > 0 {
                    cv.set(x, 3, hi);
                }
            }
            for x in HX0..=HX1 {
                if cv.alpha_at(x, 2) > 0 {
                    cv.set(x, 2, hi);
                }
            }
        }
    }
}

fn draw_facial(cv: &mut Canvas, kind: &str, color: Rgb) {
    let [_, base, sh] = shades(color);
    match kind {
        "mustacheSm" => {
            for x in [7, 8, 9] {
                cv.set(x, 13, base);
            }
        }
        "stubble" => {
            for (x, y) in [(5, 14), (6, 15), (7, 15), (8, 15), (9, 15), (10, 15), (11, 14), (12, 13), (4, 13), (5, 15)] {
                cv.set_a(x, y, sh, 150);
            }
        }
        "goatee" => {
            for x in [8, 9] {
                cv.set(x, 15, base);
                cv.set(x, 14, base);
            }
            for x in [7, 8, 9, 10] {
                cv.set(x, 13, base);
            }
        }
        _ => {
            for x in [6, 7, 8, 9, 10] {
                cv.set(x, 13, base);
            }
            cv.set(6, 12, base);
            cv.set(10, 12, base);
        }
    }
}

/// Clear prescription glasses, not sunglasses: a rim that frames each eye
/// without covering it, plus a glint so the lens reads as glass.
fn draw_glasses(cv: &mut Canvas) {
    let frame: Rgb = [60, 54, 62];
    let glint: Rgb = [236, 240, 246];
    for x in [5, 6] {
        cv.set(x, 8, frame);
        cv.set(x, 10, frame);
    }
    cv.set(4, 9, frame);
    cv.set(7, 9, frame);
    cv.set(7, 8, frame);
    for x in [10, 11] {
        cv.set(x, 8, frame);
        cv.set(x, 10, frame);
    }
    cv.set(9, 9, frame);
    cv.set(12, 9, frame);
    cv.set(12, 8, frame);
    cv.set(8, 8, frame); // bridge
    cv.set(3, 9, frame);
    cv.set(13, 9, frame); // temple arms
    cv.set(4, 8, glint);
    cv.set(9, 8, glint);
}

fn body_shape(cv: &mut Canvas, col: Rgb, heavy: bool) {
    let [_, base, sh] = shades(col);
    let rows: &[(i32, i32, i32)] = if heavy {
        &[(19, 5, 12), (20, 3, 14), (21, 2, 15), (22, 1, 16), (23, 1, 16), (24, 0, 17), (25, 0, 17), (26, 0, 17), (27, 0, 17)]
    } else {
        &[(19, 6, 11), (20, 4, 13), (21, 3, 14), (22, 2, 15), (23, 2, 15), (24, 1, 16), (25, 1, 16), (26, 1, 16), (27, 1, 16)]
    };
    for (y, a, b) in rows {
        cv.rect(*a, *y, *b, *y, base);
    }
    let (lo, hi) = if heavy { (1, 16) } else { (2, 15) };
    for y in 22..28 {
        cv.set(lo, y, sh);
        cv.set(hi, y, sh);
    }
}

fn draw_clothing(cv: &mut Canvas, r: &Recipe) {
    let [hi, base, sh] = shades(r.c1);
    body_shape(cv, r.c1, r.heavy);
    let white: Rgb = [238, 238, 236];

    match r.cloth.as_str() {
        "suit" => {
            for (x, y) in [(8, 19), (9, 19), (7, 20), (8, 20), (9, 20), (10, 20), (8, 21), (9, 21)] {
                cv.set(x, y, white);
            }
            for (x, y) in [(6, 20), (7, 21), (11, 20), (10, 21), (6, 21), (11, 21)] {
                cv.set(x, y, sh);
            }
            match r.tie {
                Some(tie) => {
                    for y in 20..26 {
                        cv.set(8, y, tie);
                        cv.set(9, y, tie);
                    }
                    cv.set(8, 20, shades(tie)[0]);
                }
                None => {
                    for y in 22..26 {
                        cv.set(8, y, white);
                        cv.set(9, y, white);
                    }
                }
            }
        }
        "dressshirt" => {
            for (x, y) in [(6, 19), (7, 19), (10, 19), (11, 19), (7, 20), (10, 20)] {
                cv.set(x, y, sh);
            }
            let mut y = 20;
            while y < 27 {
                cv.set(8, y, sh);
                y += 2;
            }
            if let Some(tie) = r.tie {
                for y in 19..26 {
                    cv.set(8, y, tie);
                    cv.set(9, y, tie);
                }
            }
        }
        "polo" => {
            for (x, y) in [(6, 19), (7, 19), (10, 19), (11, 19)] {
                cv.set(x, y, hi);
            }
            cv.set(8, 20, sh);
            cv.set(8, 22, sh);
            let accent = r.c2.map(|c| shades(c)[1]).unwrap_or(hi);
            for (x, y) in [(7, 20), (9, 20)] {
                cv.set(x, y, accent);
            }
        }
        "blouse" => {
            let s = skin_pal(&r.skin);
            for (x, y) in [(7, 19), (8, 19), (9, 19), (10, 19), (8, 20), (9, 20)] {
                cv.set(x, y, s.sh);
            }
            for x in 5..13 {
                if cv.rgb_at(x, 20) == base {
                    cv.set(x, 20, hi);
                }
            }
        }
        "cardigan" => {
            let inner = r.c2.map(|c| shades(c)[1]).unwrap_or([235, 233, 226]);
            for y in 19..27 {
                cv.set(8, y, inner);
                cv.set(9, y, inner);
            }
            for (x, y) in [(6, 19), (7, 19), (10, 19), (11, 19)] {
                cv.set(x, y, sh);
            }
        }
        // sweater, and anything a theme invents: a plain shoulder line.
        _ => {
            for x in 6..=11 {
                cv.set(x, 19, sh);
            }
        }
    }
}

/// Trace a dark edge around everything drawn.
///
/// Last, and over the whole figure: at this size the outline is what separates a
/// character from the floor behind it, and outlining each layer as it is drawn
/// would leave seams where layers meet.
fn outline_pass(cv: &mut Canvas) {
    let (w, h) = (cv.w, cv.h);
    let mut edges = Vec::new();
    for y in 0..h {
        for x in 0..w {
            if cv.alpha_at(x, y) != 0 {
                continue;
            }
            let neighbour = [(1, 0), (-1, 0), (0, 1), (0, -1)]
                .iter()
                .any(|(dx, dy)| cv.alpha_at(x + dx, y + dy) == 255);
            if neighbour {
                edges.push((x, y));
            }
        }
    }
    for (x, y) in edges {
        cv.set(x, y, OUTLINE);
    }
}

/// Paint one portrait. The layer order is the recipe: clothing, then neck, then
/// head, then face, then hair over the hairline, then accessories.
pub fn portrait(r: &Recipe) -> Canvas {
    let mut cv = Canvas::new(W, H);
    draw_clothing(&mut cv, r);
    // Collar/neck shadow, so the head sits ON the body rather than beside it.
    let s = skin_pal(&r.skin);
    cv.rect(7, 18, 10, 19, s.sh);
    draw_head(&mut cv, &r.skin);
    draw_face(&mut cv, r);
    if let Some(f) = &r.facial {
        draw_facial(&mut cv, f, r.hairc);
    }
    draw_hair(&mut cv, r);
    if r.glasses {
        draw_glasses(&mut cv);
    }
    outline_pass(&mut cv);
    cv
}

#[cfg(test)]
mod tests {
    use super::*;

    fn recipe(hair: &str, cloth: &str) -> Recipe {
        Recipe {
            skin: "light".into(),
            hairc: [58, 42, 28],
            hair: hair.into(),
            part: None,
            recede: None,
            length: None,
            vol: None,
            cloth: cloth.into(),
            c1: [58, 63, 74],
            c2: None,
            tie: Some([170, 58, 58]),
            brow: Some("flat".into()),
            mouth: Some("smile".into()),
            blush: false,
            facial: None,
            glasses: false,
            lashes: false,
            heavy: false,
        }
    }

    fn opaque(cv: &Canvas) -> usize {
        cv.buf.chunks(4).filter(|p| p[3] > 0).count()
    }

    #[test]
    fn a_portrait_fills_the_canvas_and_nothing_more() {
        let cv = portrait(&recipe("styleShort", "suit"));
        assert_eq!(cv.buf.len(), W * H * 4);
        let painted = opaque(&cv);
        assert!(painted > 200, "a character should cover most of the frame, got {painted}");
        assert!(painted < W * H, "and should not be a solid block");
    }

    /// Every style has to paint SOMETHING above the head, or a theme naming one
    /// gets a bald character with no indication why.
    #[test]
    fn every_hair_style_paints_hair() {
        for style in ["styleShort", "styleFloppy", "styleFrame", "styleBun", "styleCurly",
                      "styleMessy", "styleRecede", "styleSpiky", "styleBald"] {
            let cv = portrait(&recipe(style, "suit"));
            let above_head = (0..4)
                .flat_map(|y| (0..W as i32).map(move |x| (x, y)))
                .filter(|(x, y)| cv.alpha_at(*x, *y) > 0)
                .count();
            assert!(above_head > 0, "{style} painted nothing above the head");
        }
    }

    /// An unknown value must still produce a person — a theme file typo should
    /// not blank a character out of the floor.
    #[test]
    fn unknown_style_and_garment_still_paint_a_character() {
        let cv = portrait(&recipe("styleFromAnotherTheme", "browncoat"));
        assert!(opaque(&cv) > 200);
    }

    #[test]
    fn every_garment_paints_a_torso() {
        for cloth in ["suit", "dressshirt", "polo", "blouse", "cardigan", "sweater"] {
            let cv = portrait(&recipe("styleShort", cloth));
            let torso = (19..H as i32)
                .flat_map(|y| (0..W as i32).map(move |x| (x, y)))
                .filter(|(x, y)| cv.alpha_at(*x, *y) > 0)
                .count();
            assert!(torso > 60, "{cloth} painted only {torso} torso pixels");
        }
    }

    /// The outline is what separates a character from the floor behind it.
    #[test]
    fn the_outline_traces_the_silhouette() {
        let cv = portrait(&recipe("styleShort", "suit"));
        let outlined = cv.buf.chunks(4).filter(|p| [p[0], p[1], p[2]] == OUTLINE && p[3] > 0).count();
        assert!(outlined > 30, "expected a traced edge, got {outlined} pixels");
    }

    /// A heavier build is a different silhouette, not a recolour.
    #[test]
    fn the_heavy_build_is_wider() {
        let slim = portrait(&recipe("styleShort", "suit"));
        let mut r = recipe("styleShort", "suit");
        r.heavy = true;
        let heavy = portrait(&r);
        assert!(opaque(&heavy) > opaque(&slim));
    }

    /// A theme file typo should be a visible error, not a silently missing hat.
    #[test]
    fn an_unknown_recipe_field_is_rejected() {
        let ok: Result<Recipe, _> = serde_json::from_str(
            r#"{"skin":"light","hairc":[1,2,3],"hair":"styleShort","cloth":"suit","c1":[1,2,3]}"#,
        );
        assert!(ok.is_ok());
        let typo: Result<Recipe, _> = serde_json::from_str(
            r#"{"skin":"light","hairc":[1,2,3],"hair":"styleShort","cloth":"suit","c1":[1,2,3],"glases":true}"#,
        );
        assert!(typo.is_err(), "a misspelled field must not be silently dropped");
    }
}
