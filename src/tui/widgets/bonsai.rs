//! Procedural pixel-art bonsai, ported from the site hero animation
//! (`site/index.html`, `buildTree`/`drawTree`). Geometry is generated once
//! per (size, seed) and painted progressively: the growth choreography lives
//! entirely in per-element reveal windows (`start_at`/`end_at`), mirroring
//! the JS original so terminal trees share the site's look. Rendering uses
//! half-block cells, so one terminal row holds two pixel rows.

use std::time::{Duration, Instant};

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// Total growth time, matching the site hero.
pub const GROW_DURATION: Duration = Duration::from_millis(2400);

/// Smallest terminal area worth drawing a tree in (cols, rows).
pub const MIN_AREA: (u16, u16) = (24, 12);

type Rgb = (u8, u8, u8);

const LEAF: [Rgb; 5] = [
    (0x29, 0x4b, 0x31),
    (0x41, 0x6b, 0x42),
    (0x66, 0x99, 0x57),
    (0x91, 0xc9, 0x66),
    (0xd0, 0xed, 0x8d),
];
const BARK: [Rgb; 3] = [(0x51, 0x38, 0x28), (0x72, 0x50, 0x39), (0x9b, 0x6c, 0x48)];
const POT: [Rgb; 3] = [(0x5e, 0x3e, 0x2c), (0x83, 0x58, 0x3b), (0xc2, 0x8a, 0x52)];

/// Design-space bounding box of the tree layout, in grid units.
const DESIGN_W: f64 = 56.0;
const DESIGN_H: f64 = 62.0;
/// Cap keeps large terminals on the chunky pixel aesthetic instead of a
/// high-resolution tree.
const MAX_SCALE: f64 = 1.3;

/// Mulberry32, the same PRNG as the site hero. The call order during
/// generation is part of the look — deliberate, don't reorder.
struct Mulberry32 {
    state: u32,
}

impl Mulberry32 {
    fn new(seed: u32) -> Self {
        Self { state: seed }
    }

    /// Uniform in `[0, 1)`.
    fn next(&mut self) -> f64 {
        self.state = self.state.wrapping_add(0x6D2B_79F5);
        let mut v = self.state;
        v = (v ^ (v >> 15)).wrapping_mul(v | 1);
        v ^= v.wrapping_add((v ^ (v >> 7)).wrapping_mul(v | 61));
        f64::from(v ^ (v >> 14)) / 4_294_967_296.0
    }
}

/// A fresh seed for a new tree; each call yields a different shape.
pub fn random_seed() -> u32 {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut hasher = RandomState::new().build_hasher();
    hasher.write_u128(
        std::time::SystemTime::UNIX_EPOCH
            .elapsed()
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    );
    hasher.finish() as u32
}

struct Branch {
    /// Spline cells in pixel space (y down), base to tip.
    points: Vec<(i32, i32)>,
    start_thickness: f64,
    end_thickness: f64,
    start_at: f64,
    end_at: f64,
    accent: bool,
}

struct Leaf {
    x: i32,
    y: i32,
    /// Size in design units (1, or 2 for rare highlight blobs).
    size: f64,
    color: Rgb,
    start_at: f64,
}

struct PotRect {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
    color: Rgb,
}

/// A generated tree: fixed geometry, painted at any growth progress.
pub struct BonsaiTree {
    cols: u16,
    rows: u16,
    /// Pixel grid dimensions (width = cols, height = rows * 2).
    px_w: i32,
    px_h: i32,
    scale: f64,
    pot: Vec<PotRect>,
    branches: Vec<Branch>,
    leaves: Vec<Leaf>,
}

impl BonsaiTree {
    /// Generate a tree filling a `cols` x `rows` terminal area.
    pub fn generate(cols: u16, rows: u16, seed: u32) -> Self {
        let px_w = i32::from(cols);
        let px_h = i32::from(rows) * 2;
        let scale = (f64::from(px_w) / DESIGN_W)
            .min(f64::from(px_h) / DESIGN_H)
            .clamp(0.25, MAX_SCALE);
        let mut builder = TreeBuilder {
            rng: Mulberry32::new(seed),
            scale,
            // Layout extends one unit further left than right; nudge right to
            // center the visual mass.
            root_x: (f64::from(px_w) / 2.0 + scale).round(),
            root_y: f64::from(px_h) - (10.0 * scale).round(),
            branches: Vec::new(),
            leaves: Vec::new(),
        };
        builder.build();
        let pot = builder.pot_rects();
        Self {
            cols,
            rows,
            px_w,
            px_h,
            scale,
            pot,
            branches: builder.branches,
            leaves: builder.leaves,
        }
    }

    /// Paint the tree at `progress` in `[0, 1]` into `area`, centered.
    /// Untouched cells keep their existing content, so the tree composes
    /// over whatever background the caller drew.
    pub fn render(&self, area: Rect, buf: &mut Buffer, progress: f64) {
        let grid = self.paint(progress.clamp(0.0, 1.0));
        let cols = self.cols.min(area.width);
        let rows = self.rows.min(area.height);
        let dx = area.x + (area.width - cols) / 2;
        let dy = area.y + (area.height - rows) / 2;
        for row in 0..rows {
            for col in 0..cols {
                let upper = grid.get(col, row * 2);
                let lower = grid.get(col, row * 2 + 1);
                let Some(cell) = buf.cell_mut((dx + col, dy + row)) else {
                    continue;
                };
                match (upper, lower) {
                    (Some(u), Some(l)) => {
                        cell.set_symbol("▀")
                            .set_fg(Color::Rgb(u.0, u.1, u.2))
                            .set_bg(Color::Rgb(l.0, l.1, l.2));
                    }
                    (Some(u), None) => {
                        cell.set_symbol("▀").set_fg(Color::Rgb(u.0, u.1, u.2));
                    }
                    (None, Some(l)) => {
                        cell.set_symbol("▄").set_fg(Color::Rgb(l.0, l.1, l.2));
                    }
                    (None, None) => {}
                }
            }
        }
    }

    fn paint(&self, progress: f64) -> PixelGrid {
        let mut grid = PixelGrid::new(self.px_w, self.px_h);

        if progress > 0.01 {
            for rect in &self.pot {
                grid.fill_rect(rect.x0, rect.y0, rect.x1, rect.y1, rect.color);
            }
        }

        for branch in &self.branches {
            let window = branch.end_at - branch.start_at;
            let visible = ((progress - branch.start_at) / window).clamp(0.0, 1.0);
            let count = (branch.points.len() as f64 * visible).ceil() as usize;
            let last = branch.points.len().saturating_sub(1).max(1) as f64;
            for (index, &(x, y)) in branch.points[..count.min(branch.points.len())]
                .iter()
                .enumerate()
            {
                let along = index as f64 / last;
                let thickness = branch.start_thickness
                    + (branch.end_thickness - branch.start_thickness) * along;
                let color = if branch.accent {
                    BARK[2]
                } else {
                    BARK[((along * BARK.len() as f64) as usize).min(BARK.len() - 1)]
                };
                grid.fill_square(x, y, self.px_size(thickness), color);
            }
        }

        for leaf in &self.leaves {
            if progress >= leaf.start_at {
                grid.fill_square(leaf.x, leaf.y, self.px_size(leaf.size), leaf.color);
            }
        }

        grid
    }

    /// Design-unit extent to pixels, never vanishing entirely.
    fn px_size(&self, units: f64) -> i32 {
        ((units * self.scale).round() as i32).max(1)
    }
}

/// Generation state; mirrors the JS `buildTree` closure.
struct TreeBuilder {
    rng: Mulberry32,
    scale: f64,
    root_x: f64,
    root_y: f64,
    branches: Vec<Branch>,
    leaves: Vec<Leaf>,
}

impl TreeBuilder {
    /// Design coordinates (x right, y up, in units) to pixel space (y down).
    fn at(&self, x: f64, y: f64) -> (f64, f64) {
        (self.root_x + x * self.scale, self.root_y - y * self.scale)
    }

    /// Informal-upright bonsai: one S-curved trunk and a few asymmetric pads.
    /// Knots and reveal windows are verbatim from the site hero.
    fn build(&mut self) {
        let trunk = [
            self.at(0.0, 0.0),
            self.at(-2.4, 6.0),
            self.at(2.2, 13.0),
            self.at(-1.3, 20.0),
            self.at(3.1, 27.0),
            self.at(0.2, 34.0),
            self.at(3.5, 41.0),
        ];
        self.add_path(&trunk, 6.0, 2.0, 0.05, 0.46, false);

        // Exposed roots anchor the trunk into the shallow pot.
        let left_root = [self.at(0.0, 0.5), self.at(-3.5, 1.4), self.at(-8.0, 0.2)];
        self.add_path(&left_root, 4.0, 1.0, 0.03, 0.14, false);
        let right_root = [self.at(0.5, 0.3), self.at(4.0, 1.2), self.at(8.5, 0.1)];
        self.add_path(&right_root, 4.0, 1.0, 0.04, 0.15, false);

        let accent = [
            self.at(-0.8, 1.0),
            self.at(-2.6, 7.0),
            self.at(1.1, 13.0),
            self.at(-2.2, 20.0),
            self.at(2.0, 27.0),
            self.at(-0.8, 34.0),
            self.at(2.7, 40.0),
        ];
        self.add_path(&accent, 1.2, 1.0, 0.09, 0.47, true);

        // Alternating pads overlap inward while preserving negative space.
        let b1 = [
            self.at(1.7, 11.0),
            self.at(-5.0, 12.0),
            self.at(-13.0, 14.0),
            self.at(-20.0, 14.5),
        ];
        self.add_path(&b1, 3.5, 1.0, 0.28, 0.43, false);
        self.pad(-14.5, 16.0, 0.41, 9.8, 3.5);

        let b2 = [
            self.at(-1.4, 20.0),
            self.at(5.5, 20.5),
            self.at(12.0, 22.5),
            self.at(18.0, 23.0),
        ];
        self.add_path(&b2, 3.0, 1.0, 0.4, 0.55, false);
        self.pad(12.5, 25.0, 0.53, 9.4, 3.5);

        let b3 = [
            self.at(3.0, 27.0),
            self.at(-3.0, 27.5),
            self.at(-9.0, 29.5),
            self.at(-15.0, 30.0),
        ];
        self.add_path(&b3, 2.7, 1.0, 0.52, 0.66, false);
        self.pad(-10.5, 32.0, 0.64, 8.3, 3.2);

        let b4 = [
            self.at(0.4, 34.0),
            self.at(5.0, 34.5),
            self.at(9.0, 36.0),
            self.at(13.0, 37.0),
        ];
        self.add_path(&b4, 2.3, 1.0, 0.63, 0.76, false);
        self.pad(8.5, 39.0, 0.74, 7.4, 3.1);

        let b5 = [self.at(3.5, 41.0), self.at(1.5, 43.0), self.at(2.4, 45.0)];
        self.add_path(&b5, 2.0, 1.0, 0.72, 0.83, false);
        self.pad(1.5, 45.5, 0.81, 8.2, 4.2);
    }

    /// Uniform Catmull-Rom spline through `knots`, rasterized cell by cell so
    /// the branch can grow point-by-point from base to tip.
    fn add_path(
        &mut self,
        knots: &[(f64, f64)],
        start_thickness: f64,
        end_thickness: f64,
        start_at: f64,
        end_at: f64,
        accent: bool,
    ) {
        let mut points: Vec<(i32, i32)> = Vec::new();
        for segment in 0..knots.len() - 1 {
            let p0 = knots[segment.saturating_sub(1)];
            let p1 = knots[segment];
            let p2 = knots[segment + 1];
            let p3 = knots[(segment + 2).min(knots.len() - 1)];
            let distance = (p2.0 - p1.0).hypot(p2.1 - p1.1);
            let steps = ((distance * 1.5).round() as i32).max(3);
            let first = if segment == 0 { 0 } else { 1 };
            for step in first..=steps {
                let t = f64::from(step) / f64::from(steps);
                let t2 = t * t;
                let t3 = t2 * t;
                let interp = |a: f64, b: f64, c: f64, d: f64| {
                    0.5 * (2.0 * b
                        + (-a + c) * t
                        + (2.0 * a - 5.0 * b + 4.0 * c - d) * t2
                        + (-a + 3.0 * b - 3.0 * c + d) * t3)
                };
                let point = (
                    interp(p0.0, p1.0, p2.0, p3.0).round() as i32,
                    interp(p0.1, p1.1, p2.1, p3.1).round() as i32,
                );
                if points.last() != Some(&point) {
                    points.push(point);
                }
            }
        }
        self.branches.push(Branch {
            points,
            start_thickness,
            end_thickness,
            start_at,
            end_at,
            accent,
        });
    }

    /// Jittered leaf pad; the rng call order matches the JS `pad` helper.
    fn pad(&mut self, x: f64, y: f64, start_at: f64, radius_x: f64, radius_y: f64) {
        let jx = x + (self.rng.next() - 0.5) * 0.8;
        let jy = y + (self.rng.next() - 0.5) * 0.4;
        let center = self.at(jx, jy);
        let rx = radius_x * (0.96 + self.rng.next() * 0.08) * self.scale;
        let ry = radius_y * (0.96 + self.rng.next() * 0.08) * self.scale;
        self.add_leaf_pad(center, start_at, rx, ry);
    }

    /// Three overlapping elliptical lobes with ragged edges and holes; the
    /// bottom half is squashed so pads read as foliage shelves. Each cell
    /// carries its own reveal time so pads unfurl outward.
    fn add_leaf_pad(&mut self, center: (f64, f64), start_at: f64, rx: f64, ry: f64) {
        let lobes = [
            (-rx * 0.56, 0.25, rx * 0.56, ry * 0.78),
            (0.0, -0.38, rx * 0.72, ry),
            (rx * 0.58, 0.18, rx * 0.54, ry * 0.76),
        ];
        for (lobe_index, &(lx, ly, lrx, lry)) in lobes.iter().enumerate() {
            let columns = (lrx * (0.97 + self.rng.next() * 0.06)).ceil().max(1.0);
            let rows = (lry * (0.97 + self.rng.next() * 0.06)).ceil().max(1.0);
            for row in -(rows as i32)..=(rows as i32) {
                for column in -(columns as i32)..=(columns as i32) {
                    let row_f = f64::from(row);
                    let vertical = if row < 0 {
                        row_f / rows
                    } else {
                        row_f / (rows * 0.58)
                    };
                    let distance = (f64::from(column) / columns).powi(2) + vertical.powi(2);
                    let edge = 1.0 + (self.rng.next() - 0.5) * 0.18;
                    // Short-circuits preserved: the hole rng only fires past
                    // the 0.72 ring, like the JS `||` chain.
                    if distance > edge || (distance > 0.72 && self.rng.next() < 0.1) {
                        continue;
                    }
                    let tone = self.rng.next() + if row < 0 { 0.2 } else { -0.1 };
                    let color_index = if tone > 0.92 {
                        4
                    } else if tone > 0.66 {
                        3
                    } else if tone > 0.38 {
                        2
                    } else if tone > 0.14 {
                        1
                    } else {
                        0
                    };
                    let size = if distance < 0.36 && self.rng.next() > 0.97 {
                        2.0
                    } else {
                        1.0
                    };
                    self.leaves.push(Leaf {
                        x: (center.0 + lx + f64::from(column)).round() as i32,
                        y: (center.1 + ly + row_f).round() as i32,
                        size,
                        color: LEAF[color_index],
                        start_at: start_at
                            + lobe_index as f64 * 0.012
                            + distance.max(0.0) * 0.025
                            + self.rng.next() * 0.018,
                    });
                }
            }
        }
    }

    /// The shallow pot, soil line, and feet, as edge-scaled rects. Shared
    /// edges round identically, so no seams open at fractional scales.
    fn pot_rects(&self) -> Vec<PotRect> {
        let rect = |x0: f64, y0: f64, x1: f64, y1: f64, color: Rgb| PotRect {
            x0: (self.root_x + x0 * self.scale).round() as i32,
            y0: (self.root_y + y0 * self.scale).round() as i32,
            x1: (self.root_x + x1 * self.scale).round() as i32,
            y1: (self.root_y + y1 * self.scale).round() as i32,
            color,
        };
        vec![
            rect(-9.0, -1.0, 9.0, 1.0, LEAF[0]),
            rect(-12.0, 1.0, 12.0, 2.0, POT[0]),
            rect(-13.0, 2.0, 13.0, 4.0, POT[2]),
            rect(-11.0, 4.0, 11.0, 8.0, POT[1]),
            rect(-10.0, 8.0, 10.0, 9.0, POT[0]),
            rect(-8.0, 9.0, -4.0, 10.0, POT[0]),
            rect(4.0, 9.0, 8.0, 10.0, POT[0]),
        ]
    }
}

/// Sparse pixel canvas; `None` cells are transparent.
struct PixelGrid {
    width: i32,
    height: i32,
    cells: Vec<Option<Rgb>>,
}

impl PixelGrid {
    fn new(width: i32, height: i32) -> Self {
        Self {
            width,
            height,
            cells: vec![None; (width * height) as usize],
        }
    }

    fn get(&self, col: u16, row: u16) -> Option<Rgb> {
        let (x, y) = (i32::from(col), i32::from(row));
        if x < self.width && y < self.height {
            self.cells[(y * self.width + x) as usize]
        } else {
            None
        }
    }

    fn set(&mut self, x: i32, y: i32, color: Rgb) {
        if (0..self.width).contains(&x) && (0..self.height).contains(&y) {
            self.cells[(y * self.width + x) as usize] = Some(color);
        }
    }

    fn fill_rect(&mut self, x0: i32, y0: i32, x1: i32, y1: i32, color: Rgb) {
        for y in y0..y1 {
            for x in x0..x1 {
                self.set(x, y, color);
            }
        }
    }

    /// A `size`-wide square centered on the cell, like the JS `drawPixel`.
    fn fill_square(&mut self, cx: i32, cy: i32, size: i32, color: Rgb) {
        let offset = size / 2;
        self.fill_rect(
            cx - offset,
            cy - offset,
            cx - offset + size,
            cy - offset + size,
            color,
        );
    }
}

/// One growth cycle: a seed plus the moment it was planted. Geometry is
/// regenerated from the seed on each render (sub-millisecond), which keeps
/// this value plain state — widgets render it through `&AppState` and window
/// resizes need no special casing.
pub struct BonsaiGrowth {
    seed: u32,
    /// Armed on first observation rather than at construction: the tree
    /// sprouted in `AppState::new` only becomes visible after the rest of
    /// startup, which would silently eat most of the growth window.
    planted: std::cell::Cell<Option<Instant>>,
}

impl BonsaiGrowth {
    /// Plant a fresh randomly-seeded tree that grows over [`GROW_DURATION`],
    /// starting the moment it is first observed.
    pub fn sprout() -> Self {
        Self {
            seed: random_seed(),
            planted: std::cell::Cell::new(None),
        }
    }

    /// New seed, restarted clock — the site hero's "[ grow again ]".
    pub fn replant(&mut self) {
        *self = Self::sprout();
    }

    /// A tree that is already fully grown, for tests that assert the grown-state
    /// UI (e.g. the sidebar's replant hint). Backdates planting past
    /// [`GROW_DURATION`] so `progress()` reports 1.0 without waiting. Test-only,
    /// so it never trips the unused-code lint in release builds.
    #[cfg(test)]
    pub fn grown() -> Self {
        let growth = Self::sprout();
        growth
            .planted
            .set(Instant::now().checked_sub(GROW_DURATION));
        growth
    }

    pub fn progress(&self) -> f64 {
        let planted = self.planted.get().unwrap_or_else(|| {
            let now = Instant::now();
            self.planted.set(Some(now));
            now
        });
        (planted.elapsed().as_secs_f64() / GROW_DURATION.as_secs_f64()).min(1.0)
    }

    pub fn is_growing(&self) -> bool {
        self.progress() < 1.0
    }

    /// Render into `area` at `progress` (pass 1.0 to skip the animation when
    /// motion is reduced). Skips areas too small for the tree to read.
    pub fn render(&self, area: Rect, buf: &mut Buffer, progress: f64) {
        if area.width < MIN_AREA.0 || area.height < MIN_AREA.1 {
            return;
        }
        BonsaiTree::generate(area.width, area.height, self.seed).render(area, buf, progress);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bonsai_same_seed_is_deterministic() {
        let a = BonsaiTree::generate(60, 32, 0xDEAD_BEEF);
        let b = BonsaiTree::generate(60, 32, 0xDEAD_BEEF);
        assert_eq!(a.leaves.len(), b.leaves.len());
        assert_eq!(a.branches.len(), b.branches.len());
        let pixels =
            |tree: &BonsaiTree| tree.paint(1.0).cells.iter().filter(|c| c.is_some()).count();
        assert_eq!(pixels(&a), pixels(&b));
    }

    #[test]
    fn bonsai_different_seeds_differ() {
        let a = BonsaiTree::generate(60, 32, 1);
        let b = BonsaiTree::generate(60, 32, 2);
        assert_ne!(a.paint(1.0).cells, b.paint(1.0).cells);
    }

    #[test]
    fn bonsai_growth_is_monotonic() {
        let tree = BonsaiTree::generate(60, 32, 42);
        let lit = |p: f64| tree.paint(p).cells.iter().filter(|c| c.is_some()).count();
        assert_eq!(lit(0.0), 0);
        let (early, mid, full) = (lit(0.2), lit(0.6), lit(1.0));
        assert!(early > 0, "pot and roots should appear early");
        assert!(early < mid && mid < full, "{early} < {mid} < {full}");
        assert!(full > 400, "a full tree should be substantial: {full}");
    }

    /// Eyeball the tree in a real terminal:
    /// `cargo test bonsai_visual_dump -- --ignored --nocapture`
    #[test]
    #[ignore = "visual inspection helper, not an assertion"]
    fn bonsai_visual_dump() {
        for seed in [random_seed(), random_seed()] {
            let tree = BonsaiTree::generate(64, 32, seed);
            let grid = tree.paint(1.0);
            let mut out = format!("seed {seed:#010x}\n");
            for row in 0..32u16 {
                for col in 0..64u16 {
                    match (grid.get(col, row * 2), grid.get(col, row * 2 + 1)) {
                        (Some(u), Some(l)) => out.push_str(&format!(
                            "\x1b[38;2;{};{};{}m\x1b[48;2;{};{};{}m▀\x1b[0m",
                            u.0, u.1, u.2, l.0, l.1, l.2
                        )),
                        (Some(u), None) => {
                            out.push_str(&format!("\x1b[38;2;{};{};{}m▀\x1b[0m", u.0, u.1, u.2))
                        }
                        (None, Some(l)) => {
                            out.push_str(&format!("\x1b[38;2;{};{};{}m▄\x1b[0m", l.0, l.1, l.2))
                        }
                        (None, None) => out.push(' '),
                    }
                }
                out.push('\n');
            }
            println!("{out}");
        }
    }

    #[test]
    fn bonsai_renders_within_area() {
        let tree = BonsaiTree::generate(40, 20, 7);
        let area = Rect::new(2, 1, 40, 20);
        let mut buf = Buffer::empty(Rect::new(0, 0, 50, 25));
        tree.render(area, &mut buf, 1.0);
        let mut outside = 0;
        for y in 0..25u16 {
            for x in 0..50u16 {
                let cell = &buf[(x, y)];
                if cell.symbol() != " " && !area.contains(ratatui::layout::Position::new(x, y)) {
                    outside += 1;
                }
            }
        }
        assert_eq!(outside, 0, "tree must not paint outside its area");
    }
}
