//! The logo, drawn in code at any size: a ring around a downward triangle, or (mounted) a
//! solid disc with the triangle cut out, as the Mac's menu-bar icon. Shared with
//! `build.rs`, which draws the app icon (`app.ico`) with it, so it uses only `std`.

/// Which form of the logo.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Glyph {
    /// A ring around the triangle: nothing mounted.
    Ring,
    /// A solid disc with the triangle cut out: mounted.
    Disc,
}

/// The inner circle's radius, and the triangle's, as a fraction of the outer circle's:
/// the logo's proportions (the Mac app icon's SVGs: 268.6 / 340).
const INNER: f64 = 0.79;
/// Samples per pixel along each axis, for smooth edges.
const SAMPLES: u32 = 8;

/// Whether the point (`x`, `y`), relative to the centre and in units of the outer radius
/// (y pointing down), is inside the glyph.
fn inside(glyph: Glyph, x: f64, y: f64) -> bool {
    let r2 = x * x + y * y;
    let in_outer = r2 <= 1.0;
    let in_inner = r2 <= INNER * INNER;
    // The triangle: its top edge on the centre line, its point at the bottom of the inner
    // circle.
    let in_triangle = y >= 0.0 && x.abs() <= INNER - y;
    match glyph {
        Glyph::Ring => (in_outer && !in_inner) || in_triangle,
        Glyph::Disc => in_outer && !in_triangle,
    }
}

/// Whether the point (`x`, `y`), relative to the centre and in units of half the plate's
/// side, is inside a rounded square with corners of radius `corner`.
fn inside_plate(x: f64, y: f64, corner: f64) -> bool {
    let (dx, dy) = (x.abs() - (1.0 - corner), y.abs() - (1.0 - corner));
    if dx <= 0.0 || dy <= 0.0 {
        return x.abs() <= 1.0 && y.abs() <= 1.0;
    }
    dx * dx + dy * dy <= corner * corner
}

/// Coverage (0 to 1) of each pixel of a `size` x `size` image by `test`, which takes
/// coordinates relative to the image's centre in pixels.
fn coverage(size: u32, test: impl Fn(f64, f64) -> bool) -> Vec<f64> {
    let half = size as f64 / 2.0;
    let mut out = Vec::with_capacity((size * size) as usize);
    for py in 0..size {
        for px in 0..size {
            let mut hits = 0;
            for sy in 0..SAMPLES {
                for sx in 0..SAMPLES {
                    let x = px as f64 + (sx as f64 + 0.5) / SAMPLES as f64 - half;
                    let y = py as f64 + (sy as f64 + 0.5) / SAMPLES as f64 - half;
                    if test(x, y) {
                        hits += 1;
                    }
                }
            }
            out.push(hits as f64 / (SAMPLES * SAMPLES) as f64);
        }
    }
    out
}

fn to_byte(v: f64) -> u8 {
    (v.clamp(0.0, 1.0) * 255.0).round() as u8
}

/// The notification-area icon: the glyph alone in `rgb` at `opacity`, on transparency.
/// Its outer circle is 7.5/18 of the image, as on the Mac. Top-down rows of BGRA, alpha
/// not premultiplied.
pub fn tray(size: u32, glyph: Glyph, rgb: [u8; 3], opacity: f64) -> Vec<u8> {
    let outer = size as f64 * 7.5 / 18.0;
    let cover = coverage(size, |x, y| inside(glyph, x / outer, y / outer));
    let mut out = Vec::with_capacity(cover.len() * 4);
    for c in cover {
        out.extend([rgb[2], rgb[1], rgb[0], to_byte(c * opacity)]);
    }
    out
}

/// The app icon: the ring on a light rounded square, as the Mac app icon's light
/// appearance (#1E1E1E on #F0F0F0). Top-down rows of BGRA, alpha not premultiplied.
pub fn app(size: u32) -> Vec<u8> {
    const PLATE: [f64; 3] = [240.0, 240.0, 240.0];
    const INK: [f64; 3] = [30.0, 30.0, 30.0];
    // A little margin, as Windows icons have; the ring's radius is the Mac icon's
    // 340/1024 of the plate.
    let half_plate = size as f64 * 15.0 / 32.0;
    let outer = half_plate * 2.0 * 340.0 / 1024.0;
    let plate = coverage(size, |x, y| {
        inside_plate(x / half_plate, y / half_plate, 0.2)
    });
    let ink = coverage(size, |x, y| inside(Glyph::Ring, x / outer, y / outer));
    let mut out = Vec::with_capacity(plate.len() * 4);
    for (p, i) in plate.into_iter().zip(ink) {
        // The ink lies wholly on the plate, so the colour is a plain mix of the two.
        let mix = |k: usize| to_byte((PLATE[k] * (1.0 - i) + INK[k] * i) / 255.0);
        out.extend([mix(2), mix(1), mix(0), to_byte(p)]);
    }
    out
}

/// An `.ico` file holding `images`: (size, top-down BGRA rows) each, stored as 32-bit
/// bitmaps.
pub fn ico(images: &[(u32, Vec<u8>)]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend(0u16.to_le_bytes()); // reserved
    out.extend(1u16.to_le_bytes()); // type: icon
    out.extend((images.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * images.len() as u32;
    let mut bodies = Vec::new();
    for (size, bgra) in images {
        let size = *size;
        let mask_row = size.div_ceil(32) * 4;
        let mut body = Vec::new();
        // BITMAPINFOHEADER; the height counts the colour image and the mask.
        body.extend(40u32.to_le_bytes());
        body.extend((size as i32).to_le_bytes());
        body.extend((2 * size as i32).to_le_bytes());
        body.extend(1u16.to_le_bytes());
        body.extend(32u16.to_le_bytes());
        body.extend(0u32.to_le_bytes()); // BI_RGB
        body.extend((size * size * 4 + mask_row * size).to_le_bytes());
        body.extend([0u8; 16]);
        // Rows bottom-up.
        for row in (0..size).rev() {
            let start = (row * size * 4) as usize;
            body.extend(&bgra[start..start + (size * 4) as usize]);
        }
        // An empty AND mask: the alpha channel decides.
        body.extend(std::iter::repeat_n(0u8, (mask_row * size) as usize));

        let dim = if size >= 256 { 0 } else { size as u8 };
        out.extend([dim, dim, 0, 0]);
        out.extend(1u16.to_le_bytes()); // planes
        out.extend(32u16.to_le_bytes()); // bits per pixel
        out.extend((body.len() as u32).to_le_bytes());
        out.extend(offset.to_le_bytes());
        offset += body.len() as u32;
        bodies.push(body);
    }
    for body in bodies {
        out.extend(body);
    }
    out
}

/// The app icon at the sizes Windows asks for, from 16 px in menus to 256 px in Explorer.
pub fn app_ico() -> Vec<u8> {
    let images: Vec<_> = [16, 20, 24, 32, 40, 48, 64, 96, 256]
        .into_iter()
        .map(|size| (size, app(size)))
        .collect();
    ico(&images)
}
