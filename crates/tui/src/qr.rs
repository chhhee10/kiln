//! QR codes rendered with half-block characters: each terminal cell shows two
//! vertically stacked modules (`▀` with fg = top, bg = bottom).

use qrcode::{EcLevel, QrCode};

/// Quiet zone around the code, in modules.
const QUIET: usize = 2;

/// A QR code as a grid of modules (`true` = dark), including the quiet zone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct QrGrid {
    pub size: usize,
    pub dark: Vec<bool>,
}

impl QrGrid {
    pub fn encode(data: &str) -> Option<Self> {
        let code = QrCode::with_error_correction_level(data.as_bytes(), EcLevel::L)
            .or_else(|_| QrCode::new(data.as_bytes()))
            .ok()?;
        let w = code.width();
        let modules: Vec<bool> = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
        let size = w + QUIET * 2;
        let mut dark = vec![false; size * size];
        for y in 0..w {
            for x in 0..w {
                dark[(y + QUIET) * size + x + QUIET] = modules[y * w + x];
            }
        }
        Some(Self { size, dark })
    }

    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        x < self.size && y < self.size && self.dark[y * self.size + x]
    }

    /// Terminal rows needed (two modules per row).
    pub fn rows(&self) -> usize {
        self.size.div_ceil(2)
    }

    /// Row `r` as (top_dark, bottom_dark) pairs.
    pub fn row(&self, r: usize) -> Vec<(bool, bool)> {
        (0..self.size).map(|x| (self.is_dark(x, r * 2), self.is_dark(x, r * 2 + 1))).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_pair_uri() {
        let g = QrGrid::encode("kiln://pair?url=https://mac-mini.tailnet.ts.net&code=K7Q29XMA").unwrap();
        assert!(g.size >= 21 + 4);
        assert_eq!(g.rows(), g.size.div_ceil(2));
        // Quiet zone is light; finder pattern corner is dark.
        assert!(!g.is_dark(0, 0));
        assert!(g.is_dark(QUIET, QUIET));
        assert_eq!(g.row(0).len(), g.size);
    }
}
