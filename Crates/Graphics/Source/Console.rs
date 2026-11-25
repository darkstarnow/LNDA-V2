use std::sync::atomic::{AtomicBool, Ordering};

const GIB: usize = 1 << 30;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Console {
    #[default]
    SeriesX,
    SeriesS,
}

impl Console {
    pub const fn title_memory(self) -> (usize, usize) {
        match self {
            Console::SeriesX => (10 * GIB, 7 * GIB / 2),
            Console::SeriesS => (0, 7_776 << 20),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Console::SeriesX => "Xbox Series X",
            Console::SeriesS => "Xbox Series S",
        }
    }
}

static SERIES_S: AtomicBool = AtomicBool::new(false);

impl Console {
    pub fn activate(self) {
        leonida_formats::texture::set_series_s_tiling(self == Console::SeriesS);
        SERIES_S.store(self == Console::SeriesS, Ordering::Relaxed);
    }

    pub fn current() -> Self {
        if SERIES_S.load(Ordering::Relaxed) {
            Console::SeriesS
        } else {
            Console::SeriesX
        }
    }
}
