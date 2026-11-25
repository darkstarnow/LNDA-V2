#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Swizzle(pub u32);

impl Swizzle {
    pub const LINEAR: Swizzle = Swizzle(0);
    pub const SW_256B_S: Swizzle = Swizzle(1);
    pub const SW_256B_D: Swizzle = Swizzle(2);
    pub const SW_4KB_S: Swizzle = Swizzle(5);
    pub const SW_4KB_D: Swizzle = Swizzle(6);
    pub const SW_64KB_S: Swizzle = Swizzle(9);
    pub const SW_64KB_D: Swizzle = Swizzle(10);
    pub const SW_64KB_S_T: Swizzle = Swizzle(17);
    pub const SW_64KB_D_T: Swizzle = Swizzle(18);
    pub const SW_4KB_S_X: Swizzle = Swizzle(21);
    pub const SW_4KB_D_X: Swizzle = Swizzle(22);
    pub const SW_4KB_R_X: Swizzle = Swizzle(23);
    pub const SW_64KB_Z_X: Swizzle = Swizzle(24);
    pub const SW_64KB_S_X: Swizzle = Swizzle(25);
    pub const SW_64KB_D_X: Swizzle = Swizzle(26);
    pub const SW_64KB_R_X: Swizzle = Swizzle(27);

    pub fn block_log2(self) -> Option<u32> {
        Some(match self.0 {
            0 => 0,
            1 | 2 => 8,
            5 | 6 | 21..=23 => 12,
            9 | 10 | 17 | 18 | 24..=27 => 16,
            _ => return None,
        })
    }

    fn standard(self) -> bool {
        matches!(self.0, 1 | 5 | 9 | 17 | 21 | 25)
    }

    fn display(self) -> bool {
        matches!(self.0, 2 | 6 | 10 | 18 | 22 | 26)
    }

    fn z_order(self) -> bool {
        self.0 == 24
    }

    pub fn xor(self) -> bool {
        self.0 >= 16
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dimension {
    Tex1d,
    Tex2d,
    Tex3d,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    Invalid,
    Unsupported,
    TooSmall,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Desc {
    pub dimension: Dimension,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub mips: u32,
    pub element_bytes: u32,
    pub block: u32,
    pub samples: u32,
    pub swizzle: Swizzle,
}

fn dim(texels: u32, mip: u32, block: u32) -> u32 {
    (texels >> mip).max(1).div_ceil(block)
}

fn padded_dim(texels: u32, mip: u32, block: u32) -> u32 {
    texels.max(1).div_ceil(block).div_ceil(1 << mip)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mip {
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub pitch: u32,
    pub rows: u32,
    pub depth_pitch: u32,
    pub offset: u64,
    pub tail: Option<Tail>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tail {
    pub offset: u32,
    pub x: u32,
    pub y: u32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Layout {
    pub desc: Desc,
    pub block: [u32; 3],
    pub thick: bool,
    pub slices: u32,
    pub slice_size: u64,
    pub size: u64,
    pub alignment: u64,
    pub first_tail_mip: u32,
    pub mips: Vec<Mip>,
    pattern: [Vec<u32>; 3],
}

const MICRO: [[&[u8; 16]; 5]; 3] = [
    [
        b"x0x1x2x3y0y1y2y3",
        b"--x0x1x2y0y1y2x3",
        b"----x0x1y0y1y2x2",
        b"------x0y0y1x1x2",
        b"--------y0y1x0x1",
    ],
    [
        b"x0x1x2y1y0y2x3y3",
        b"--x0x1x2y0y1y2x3",
        b"----x0x1y0y1x2y2",
        b"------x0y0x1x2y1",
        b"--------x0y0x1y1",
    ],
    [
        b"x0x1z0y0z1y1x2z2",
        b"--x0z0y0z1y1x1z2",
        b"----x0y0z0y1x1z1",
        b"------x0z0y0x1z1",
        b"--------z0y0x0z1",
    ],
];

fn pattern(kind: usize, element_log2: u32, block_log2: u32) -> [Vec<u32>; 3] {
    let mut axes: [Vec<u32>; 3] = Default::default();
    let micro = MICRO[kind][element_log2 as usize];
    for bit in 0..8u32 {
        let (axis, index) = (micro[2 * bit as usize], micro[2 * bit as usize + 1]);
        let axis = match axis {
            b'x' => 0,
            b'y' => 1,
            b'z' => 2,
            _ => continue,
        };
        let index = usize::from(index - b'0');
        let to = &mut axes[axis];
        if to.len() <= index {
            to.resize(index + 1, 0);
        }
        to[index] = bit;
    }
    let order: &[usize] = if kind == 2 { &[1, 0, 2] } else { &[1, 0] };
    for (bit, axis) in (8..block_log2).zip(order.iter().cycle()) {
        axes[*axis].push(bit);
    }
    axes
}

fn spread(bits: &[u32]) -> Vec<u32> {
    (0..1u32 << bits.len())
        .map(|c| {
            bits.iter()
                .enumerate()
                .map(|(i, &to)| ((c >> i) & 1) << to)
                .sum()
        })
        .collect()
}

const MAX_MIPS: u32 = 16;

const MAX_EXTENT: u32 = 1 << 14;

const BLOCK_256_2D: [[u32; 2]; 5] = [[16, 16], [16, 8], [8, 8], [8, 4], [4, 4]];
const BLOCK_256_3D: [[u32; 3]; 5] = [[8, 4, 8], [4, 4, 8], [4, 4, 4], [4, 2, 4], [2, 2, 4]];
const BLOCK_1K_3D: [[u32; 3]; 5] = [[16, 8, 8], [8, 8, 8], [8, 8, 4], [8, 4, 4], [4, 4, 4]];

static SERIES_S_TILING: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub fn set_series_s_tiling(series_s: bool) {
    SERIES_S_TILING.store(series_s, std::sync::atomic::Ordering::Relaxed);
}

fn render_xor_pattern(series_s: bool, element_bytes: u32) -> [Vec<u32>; 3] {
    let (x, y): (&[u16], &[u16]) = match (series_s, element_bytes) {
        (false, 2) => (
            &[2, 4, 8, 0x80, 0x200, 0x800, 0x1400, 0x8100, 0x1000],
            &[
                0x10, 0x20, 0x40, 0x2000, 0x300, 0x400, 0x4800, 0x8100, 0x1000,
            ],
        ),
        (false, 4) => (
            &[4, 8, 0x40, 0x2000, 0x200, 0x800, 0x1400, 0x8100, 0x1000],
            &[
                0x10, 0x20, 0x80, 0x4000, 0x300, 0x400, 0x8800, 0x100, 0x1000,
            ],
        ),
        (false, 8) => (
            &[8, 0x20, 0x40, 0x4000, 0x200, 0x800, 0x1400, 0x8100, 0x1000],
            &[
                0x10, 0x80, 0x2000, 0x8000, 0x300, 0x400, 0x800, 0x100, 0x1000,
            ],
        ),
        (true, 2) => (
            &[2, 4, 8, 0x80, 0x200, 0x1400, 0x2100, 0x8000, 0x4000],
            &[
                0x10, 0x20, 0x40, 0x800, 0x300, 0x400, 0x4100, 0x8000, 0x2000,
            ],
        ),
        (true, 4) => (
            &[4, 8, 0x40, 0x800, 0x200, 0x2400, 0x4100, 0x8000],
            &[
                0x10, 0x20, 0x80, 0x1000, 0x300, 0x400, 0x8100, 0x4000, 0x2000,
            ],
        ),
        (true, 8) => (
            &[8, 0x20, 0x40, 0x1000, 0x200, 0x4400, 0x8100, 0x2000],
            &[0x10, 0x80, 0x800, 0x2000, 0x300, 0x400, 0x8100, 0x4000],
        ),
        _ => return Default::default(),
    };
    fn contributions(basis: &[u16]) -> Vec<u32> {
        let mut offsets = vec![0; 1 << basis.len()];
        for (bit, &value) in basis.iter().enumerate() {
            let n = 1 << bit;
            for i in 0..n {
                offsets[n + i] = offsets[i] ^ u32::from(value);
            }
        }
        offsets
    }
    [contributions(x), contributions(y), Vec::new()]
}

impl Layout {
    pub fn new(desc: &Desc) -> Result<Layout, Error> {
        Self::new_for_model(
            desc,
            SERIES_S_TILING.load(std::sync::atomic::Ordering::Relaxed),
        )
    }

    fn new_for_model(desc: &Desc, series_s: bool) -> Result<Layout, Error> {
        let d = *desc;
        let block_log2 = d.swizzle.block_log2().ok_or(Error::Invalid)?;
        let valid = (1..=MAX_EXTENT).contains(&d.width)
            && (1..=MAX_EXTENT).contains(&d.height)
            && (1..=MAX_EXTENT).contains(&d.depth)
            && (1..=MAX_MIPS).contains(&d.mips)
            && d.element_bytes.is_power_of_two()
            && d.element_bytes <= 16
            && matches!(d.block, 1 | 4)
            && d.samples.is_power_of_two()
            && d.samples <= 8
            && (d.samples == 1 || (d.dimension == Dimension::Tex2d && d.mips == 1))
            && (d.dimension != Dimension::Tex1d || d.height == 1)
            && (d.dimension != Dimension::Tex3d
                || matches!(d.swizzle.0, 0 | 5 | 9 | 17 | 21 | 24..=27));
        if !valid {
            return Err(Error::Invalid);
        }
        if block_log2 == 0 {
            Ok(Layout::linear(d))
        } else {
            Ok(Layout::tiled(d, block_log2, series_s))
        }
    }

    fn linear(d: Desc) -> Layout {
        let bpe = u64::from(d.element_bytes);
        let align = 256 / d.element_bytes;
        let three_d = d.dimension == Dimension::Tex3d;
        let mut mips = vec![];
        let mut offset = 0;
        for i in (0..d.mips).rev() {
            let h = padded_dim(d.height, i, d.block);
            let pitch = padded_dim(d.width, i, d.block).next_multiple_of(align);
            mips.push(Mip {
                width: dim(d.width, i, d.block),
                height: dim(d.height, i, d.block),
                depth: if three_d { dim(d.depth, i, 1) } else { 1 },
                pitch,
                rows: h,
                depth_pitch: if three_d { d.depth } else { 1 },
                offset,
                tail: None,
            });
            offset += u64::from(pitch) * u64::from(h) * bpe;
        }
        mips.reverse();
        Layout {
            desc: d,
            block: [align, 1, 1],
            thick: false,
            slices: d.depth,
            slice_size: offset,
            size: offset * u64::from(d.depth),
            alignment: 256,
            first_tail_mip: d.mips,
            mips,
            pattern: Default::default(),
        }
    }

    fn tiled(d: Desc, block_log2: u32, series_s: bool) -> Layout {
        let element_log2 = d.element_bytes.trailing_zeros();
        let bpe = u64::from(d.element_bytes);
        let block_bytes = 1u64 << block_log2;
        let thick =
            d.dimension == Dimension::Tex3d && (d.swizzle.standard() || d.swizzle.display());
        let block = if thick {
            let amp = (block_log2 - 10) / 3;
            let rest = (block_log2 - 10) % 3;
            let b = BLOCK_1K_3D[element_log2 as usize];
            [
                b[0] << amp,
                b[1] << (amp + rest / 2),
                b[2] << (amp + u32::from(rest != 0)),
            ]
        } else {
            let samples_log2 = d.samples.trailing_zeros();
            let elements_log2 = block_log2 - element_log2 - samples_log2;
            let wide = samples_log2 & 1 == 0 || block_log2 & 1 != 0;
            let w = (elements_log2 + u32::from(wide)) / 2;
            [1 << w, 1 << (elements_log2 - w), 1]
        };
        let slices = d.depth.next_multiple_of(block[2]);
        let three_d = d.dimension == Dimension::Tex3d;
        let mip_depth = |i: u32| if thick { padded_dim(d.depth, i, 1) } else { 1 };
        let depth_of = |i: u32| if three_d { dim(d.depth, i, 1) } else { 1 };
        let (width, height) = (|i| dim(d.width, i, d.block), |i| dim(d.height, i, d.block));
        let padded = |i| {
            (
                padded_dim(d.width, i, d.block),
                padded_dim(d.height, i, d.block),
            )
        };
        let mut mips = Vec::with_capacity(d.mips as usize);
        let mut first_tail_mip = d.mips;
        let mut slice_size = 0u64;
        if d.mips == 1 {
            mips.push(Mip {
                width: width(0),
                height: height(0),
                depth: depth_of(0),
                pitch: width(0).next_multiple_of(block[0]),
                rows: height(0).next_multiple_of(block[1]),
                depth_pitch: if three_d { slices } else { 1 },
                offset: 0,
                tail: None,
            });
            slice_size =
                u64::from(mips[0].pitch) * u64::from(mips[0].rows) * bpe * u64::from(d.samples);
        } else {
            let mut tail_max = block;
            if thick {
                match block_log2 % 3 {
                    0 => tail_max[1] >>= 1,
                    1 => tail_max[0] >>= 1,
                    _ => tail_max[2] >>= 1,
                }
            } else {
                tail_max[0] >>= 1;
            }
            let effective = if thick {
                block_log2 - (block_log2 - 8) / 3
            } else {
                block_log2
            };
            let max_in_tail = match effective {
                8 => 0,
                9..=11 => 1 + (1 << (effective - 9)),
                _ => effective - 4,
            };
            let mut fixed = tail_max;
            if d.swizzle.z_order() && element_log2 <= 1 {
                let b = BLOCK_256_2D[element_log2 as usize];
                fixed[0] /= b[0] / BLOCK_256_2D[2][0];
                fixed[1] /= b[1] / BLOCK_256_2D[2][1];
            }
            let mut sizes = vec![];
            for i in 0..d.mips {
                let (w, h) = padded(i);
                if w <= fixed[0] && h <= fixed[1] && d.mips - i <= max_in_tail {
                    first_tail_mip = i;
                    slice_size += block_bytes / u64::from(block[2]);
                    break;
                }
                let pitch = w.next_multiple_of(block[0]);
                let rows = h.next_multiple_of(block[1]);
                let depth = mip_depth(i).next_multiple_of(block[2]);
                let one = u64::from(pitch) * u64::from(rows) * bpe;
                slice_size += one;
                sizes.push(one * u64::from(block[2]));
                mips.push(Mip {
                    width: width(i),
                    height: height(i),
                    depth: depth_of(i),
                    pitch,
                    rows,
                    depth_pitch: if three_d { depth } else { 1 },
                    offset: 0,
                    tail: None,
                });
            }
            let mut offset = if first_tail_mip < d.mips {
                block_bytes
            } else {
                0
            };
            for i in (0..first_tail_mip as usize).rev() {
                mips[i].offset = offset;
                offset += sizes[i];
            }
            let micro = if thick {
                let b = BLOCK_256_3D[element_log2 as usize];
                [b[0], b[1]]
            } else {
                BLOCK_256_2D[element_log2 as usize]
            };
            let (mut pitch, mut rows) = (tail_max[0], tail_max[1]);
            for i in first_tail_mip..d.mips {
                let m = max_in_tail - 1 - (i - first_tail_mip);
                let offset = if m > 6 { 16 << m } else { m << 8 };
                let bits = |from: u32| (0..6).map(|b| ((offset >> (from + 2 * b)) & 1) << b).sum();
                let (mut x, mut y): (u32, u32) = (bits(9), bits(8));
                if block_log2 & 1 != 0 {
                    std::mem::swap(&mut x, &mut y);
                    if element_log2 & 1 != 0 {
                        y = (y << 1) | (x & 1);
                        x >>= 1;
                    }
                }
                mips.push(Mip {
                    width: width(i),
                    height: height(i),
                    depth: depth_of(i),
                    pitch,
                    rows,
                    depth_pitch: if thick {
                        mip_depth(i).next_multiple_of(BLOCK_256_3D[element_log2 as usize][2])
                    } else {
                        1
                    },
                    offset: 0,
                    tail: Some(Tail {
                        offset,
                        x: x * micro[0],
                        y: y * micro[1],
                    }),
                });
                pitch = (pitch >> 1).max(micro[0]);
                rows = (rows >> 1).max(micro[1]);
            }
        }
        let slab = if thick { u64::from(block[2]) } else { 1 };
        let slice_size = slice_size * slab;
        let slices = slices / block[2];
        let addressable = !d.swizzle.xor() && d.samples == 1;
        let pattern = if addressable {
            let kind = if thick {
                2
            } else {
                usize::from(d.swizzle.display())
            };
            pattern(kind, element_log2, block_log2).map(|bits| spread(&bits))
        } else if d.swizzle == Swizzle::SW_64KB_R_X
            && d.dimension == Dimension::Tex2d
            && d.depth == 1
            && d.mips == 1
            && d.samples == 1
            && d.block == 1
        {
            render_xor_pattern(series_s, d.element_bytes)
        } else {
            Default::default()
        };
        Layout {
            desc: d,
            block,
            thick,
            slices,
            slice_size,
            size: slice_size * u64::from(slices),
            alignment: block_bytes,
            first_tail_mip,
            mips,
            pattern,
        }
    }

    pub fn addressable(&self) -> bool {
        self.desc.swizzle == Swizzle::LINEAR || !self.pattern[0].is_empty()
    }

    fn split(&self, z: u32) -> (u32, u32) {
        if self.thick {
            (z / self.block[2], z % self.block[2])
        } else {
            (z, 0)
        }
    }

    pub fn offset(&self, mip: u32, x: u32, y: u32, z: u32) -> Result<u64, Error> {
        let m = self.mips.get(mip as usize).ok_or(Error::Invalid)?;
        let depth = if self.desc.dimension == Dimension::Tex3d {
            m.depth
        } else {
            self.slices
        };
        let inside = x < m.width && y < m.height && z < depth;
        if !inside || !self.addressable() {
            return Err(if inside {
                Error::Unsupported
            } else {
                Error::Invalid
            });
        }
        let (slice, z) = self.split(z);
        let base = u64::from(slice) * self.slice_size + m.offset;
        if self.desc.swizzle == Swizzle::LINEAR {
            let row = u64::from(y) * u64::from(m.pitch) + u64::from(x);
            return Ok(base + row * u64::from(self.desc.element_bytes));
        }
        if self.desc.swizzle == Swizzle::SW_64KB_R_X {
            return Ok(self.render_xor_offset(m, x, y));
        }
        Ok(base + self.row(m, y, z) + self.column(m, x))
    }

    fn render_xor_offset(&self, mip: &Mip, x: u32, y: u32) -> u64 {
        let [w, h, _] = self.block;
        let block = u64::from(y / h) * u64::from(mip.pitch / w) + u64::from(x / w);
        let ox = &self.pattern[0];
        let oy = &self.pattern[1];
        block * self.alignment
            + u64::from(ox[x as usize & (ox.len() - 1)] ^ oy[y as usize & (oy.len() - 1)])
    }

    fn row(&self, m: &Mip, y: u32, z: u32) -> u64 {
        let [bw, bh, _] = self.block;
        let log2 = self.alignment.trailing_zeros();
        let y = y + m.tail.map_or(0, |t| t.y);
        let blocks = u64::from(y / bh) * u64::from(m.pitch / bw);
        (blocks << log2)
            + u64::from(self.pattern[1][(y % bh) as usize] | self.pattern[2][z as usize])
    }

    fn column(&self, m: &Mip, x: u32) -> u64 {
        let bw = self.block[0];
        let x = x + m.tail.map_or(0, |t| t.x);
        (u64::from(x / bw) << self.alignment.trailing_zeros())
            + u64::from(self.pattern[0][(x % bw) as usize])
    }

    pub fn subresource_depth(&self, mip: u32) -> u32 {
        self.mips.get(mip as usize).map_or(0, |m| m.depth)
    }

    pub fn bytes_from(&self, first: u32) -> u64 {
        let whole = first == 0
            || self.slices != 1
            || self.thick
            || self.desc.dimension == Dimension::Tex3d
            || self.desc.samples != 1
            || first >= self.desc.mips;
        if whole {
            return self.size;
        }
        self.mips[first as usize..]
            .iter()
            .map(|m| match m.tail {
                Some(_) => self.alignment,
                None => {
                    m.offset
                        + u64::from(m.pitch)
                            * u64::from(m.rows)
                            * u64::from(self.desc.element_bytes)
                }
            })
            .max()
            .unwrap_or(self.size)
            .min(self.size)
    }

    pub fn detile(
        &self,
        tiled: &[u8],
        mip: u32,
        slice: u32,
        linear: &mut [u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        self.walk(
            tiled.len(),
            mip,
            slice,
            linear.len(),
            row_pitch,
            depth_pitch,
            |t, l, len| {
                linear[l..][..len].copy_from_slice(&tiled[t..][..len]);
            },
        )
    }

    pub fn tile(
        &self,
        tiled: &mut [u8],
        mip: u32,
        slice: u32,
        linear: &[u8],
        row_pitch: usize,
        depth_pitch: usize,
    ) -> Result<(), Error> {
        self.walk(
            tiled.len(),
            mip,
            slice,
            linear.len(),
            row_pitch,
            depth_pitch,
            |t, l, len| {
                tiled[t..][..len].copy_from_slice(&linear[l..][..len]);
            },
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn walk(
        &self,
        tiled_len: usize,
        mip: u32,
        slice: u32,
        linear_len: usize,
        row_pitch: usize,
        depth_pitch: usize,
        mut copy: impl FnMut(usize, usize, usize),
    ) -> Result<(), Error> {
        let m = self.mips.get(mip as usize).ok_or(Error::Invalid)?;
        let depth = m.depth;
        let first = if self.desc.dimension == Dimension::Tex3d {
            if slice != 0 {
                return Err(Error::Invalid);
            }
            0
        } else {
            slice
        };
        if first >= self.slices {
            return Err(Error::Invalid);
        }
        if !self.addressable() {
            return Err(Error::Unsupported);
        }
        let n = self.desc.element_bytes as usize;
        let bytes = m.width as usize * n;
        let need = (depth as usize - 1)
            .checked_mul(depth_pitch)
            .zip((m.height as usize - 1).checked_mul(row_pitch))
            .and_then(|(depth_offset, row_offset)| depth_offset.checked_add(row_offset))
            .and_then(|offset| offset.checked_add(bytes))
            .ok_or(Error::TooSmall)?;
        if tiled_len < self.bytes_from(mip) as usize || linear_len < need || row_pitch < bytes {
            return Err(Error::TooSmall);
        }
        if self.desc.swizzle == Swizzle::SW_64KB_R_X {
            for y in 0..m.height {
                for x in (0..m.width).step_by(16 / n) {
                    let lx = x as usize * n;
                    let at = self.render_xor_offset(m, x, y) as usize;
                    copy(at, y as usize * row_pitch + lx, 16.min(bytes - lx));
                }
            }
            return Ok(());
        }
        let linear = self.desc.swizzle == Swizzle::LINEAR;
        let mut runs = Vec::new();
        if !linear {
            let mut first_linear = 0;
            let mut first_tiled = self.column(m, 0) as usize;
            for x in 1..m.width {
                let offset = self.column(m, x) as usize;
                let at = x as usize * n;
                if offset != first_tiled + (at - first_linear) {
                    runs.push((first_tiled, first_linear, at - first_linear));
                    first_tiled = offset;
                    first_linear = at;
                }
            }
            runs.push((first_tiled, first_linear, bytes - first_linear));
        }
        let (body, last) = runs.split_at(runs.len().saturating_sub(1));
        let run_len = body
            .first()
            .map(|run| run.2)
            .filter(|&len| body.iter().all(|run| run.2 == len));
        for z in 0..depth {
            let (s, inner) = self.split(first + z);
            let base = u64::from(s) * self.slice_size + m.offset;
            for y in 0..m.height {
                let linear_row = z as usize * depth_pitch + y as usize * row_pitch;
                if linear {
                    let tiled_row = base + u64::from(y) * u64::from(m.pitch) * n as u64;
                    copy(tiled_row as usize, linear_row, bytes);
                } else {
                    let tiled_row = (base + self.row(m, y, inner)) as usize;
                    match run_len {
                        Some(8) => copy_runs::<8>(body, tiled_row, linear_row, &mut copy),
                        Some(16) => copy_runs::<16>(body, tiled_row, linear_row, &mut copy),
                        Some(32) => copy_runs::<32>(body, tiled_row, linear_row, &mut copy),
                        _ => copy_runs::<0>(body, tiled_row, linear_row, &mut copy),
                    }
                    copy_runs::<0>(last, tiled_row, linear_row, &mut copy);
                }
            }
        }
        Ok(())
    }
}

fn copy_runs<const N: usize>(
    runs: &[(usize, usize, usize)],
    tiled_row: usize,
    linear_row: usize,
    copy: &mut impl FnMut(usize, usize, usize),
) {
    for &(tiled, linear, len) in runs {
        let len = if N == 0 { len } else { N };
        copy(tiled_row + tiled, linear_row + linear, len);
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub bytes: u32,
    pub block: u32,
    pub stencil: bool,
}

pub fn format(dxgi: u32) -> Option<Format> {
    let (bytes, block, stencil) = match dxgi {
        1..=4 => (16, 1, false),
        9..=18 => (8, 1, false),
        19..=22 | 44..=47 => (4, 1, true),
        23..=43 | 67 | 87..=93 | 116 | 117 => (4, 1, false),
        48..=59 | 85 | 86 | 115 => (2, 1, false),
        118..=120 => (2, 1, true),
        60..=65 => (1, 1, false),
        70..=72 | 79..=81 => (8, 4, false),
        73..=78 | 82..=84 | 94..=99 => (16, 4, false),
        _ => return None,
    };
    Some(Format {
        bytes,
        block,
        stencil,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureDesc {
    pub dimension: Dimension,
    pub width: u32,
    pub height: u32,
    pub depth_or_array: u32,
    pub mips: u32,
    pub format: u32,
    pub samples: u32,
    pub swizzle: Swizzle,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plane {
    pub offset: u64,
    pub stencil: bool,
    pub layout: Layout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Texture {
    pub planes: Vec<Plane>,
    pub size: u64,
    pub alignment: u64,
}

impl Texture {
    pub fn new(desc: &TextureDesc) -> Result<Texture, Error> {
        let f = format(desc.format).ok_or(Error::Invalid)?;
        let depth3d = if desc.dimension == Dimension::Tex3d {
            desc.depth_or_array
        } else {
            1
        };
        let mips = match desc.mips {
            0 => {
                32 - desc
                    .width
                    .max(desc.height)
                    .max(depth3d)
                    .max(1)
                    .leading_zeros()
            }
            n => n,
        };
        let plane = |bytes| Desc {
            dimension: desc.dimension,
            width: desc.width,
            height: desc.height,
            depth: desc.depth_or_array,
            mips,
            element_bytes: bytes,
            block: f.block,
            samples: desc.samples.max(1),
            swizzle: desc.swizzle,
        };
        let mut planes = vec![Plane {
            offset: 0,
            stencil: false,
            layout: Layout::new(&plane(f.bytes))?,
        }];
        if f.stencil {
            let layout = Layout::new(&plane(1))?;
            let offset = planes[0].layout.size.next_multiple_of(layout.alignment);
            planes.push(Plane {
                offset,
                stencil: true,
                layout,
            });
        }
        let last = &planes[planes.len() - 1];
        let size = last.offset + last.layout.size;
        let alignment = planes.iter().map(|p| p.layout.alignment).max().unwrap_or(1);
        Ok(Texture {
            planes,
            size,
            alignment,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desc(w: u32, h: u32, mips: u32, bpe: u32, swizzle: Swizzle) -> Desc {
        Desc {
            dimension: Dimension::Tex2d,
            width: w,
            height: h,
            depth: 1,
            mips,
            element_bytes: bpe,
            block: 1,
            samples: 1,
            swizzle,
        }
    }

    #[test]
    fn extents_past_the_hardware_limit_are_invalid() {
        for swizzle in [Swizzle::LINEAR, Swizzle::SW_64KB_D_X] {
            let largest = Layout::new(&desc(MAX_EXTENT, MAX_EXTENT, 15, 16, swizzle)).unwrap();
            assert!(largest.size >= u64::from(MAX_EXTENT) * u64::from(MAX_EXTENT) * 16);
            for (w, h) in [
                (MAX_EXTENT + 1, 1),
                (1, MAX_EXTENT + 1),
                (u32::MAX, u32::MAX),
            ] {
                let result = Layout::new(&desc(w, h, 1, 16, swizzle));
                assert_eq!(result, Err(Error::Invalid), "{w}x{h}");
            }
            let mut deep = desc(1, 1, 1, 16, swizzle);
            deep.depth = u32::MAX;
            assert_eq!(Layout::new(&deep), Err(Error::Invalid));
        }
    }

    #[test]
    fn render_xor_matches_independent_addrlib_offsets_and_padded_rows() {
        for (series_s, n, hashes) in [
            (false, 2, [0x10514417061b8725, 0x11a55308ad39a7b9]),
            (false, 4, [0xd4556429b544c925, 0x7a2302116719e58f]),
            (false, 8, [0x30aa25589f795b25, 0xb5f483c6ddf11404]),
            (true, 8, [0xe107baa89506c525, 0x7b7f2a812bdc611b]),
            (true, 2, [0x67cde4ff6e728f25, 0x2eb48d52f015da36]),
            (true, 4, [0xce798e86f5ab1d25, 0x6cddf048fc97e768]),
        ] {
            for ((w, h), expected) in [(256, 256), (513, 357)].into_iter().zip(hashes) {
                let l = Layout::new_for_model(&desc(w, h, 1, n, Swizzle::SW_64KB_R_X), series_s)
                    .unwrap();
                let pitch = w as usize * n as usize + 32;
                let mut linear = vec![0x55; pitch * h as usize];
                let mut tiled = vec![0xee; l.size as usize];
                let mut reference = tiled.clone();
                let mut hash = 0xcbf29ce484222325u64;
                for y in 0..h {
                    for x in 0..w {
                        let at = l.offset(0, x, y, 0).unwrap() as u32;
                        for b in at.to_le_bytes() {
                            hash = (hash ^ u64::from(b)).wrapping_mul(0x100000001b3);
                        }
                        let seed = u64::from(x + w * y)
                            .wrapping_mul(0x9e3779b97f4a7c15)
                            .to_le_bytes();
                        let src = y as usize * pitch + x as usize * n as usize;
                        linear[src..][..n as usize].copy_from_slice(&seed[..n as usize]);
                        reference[at as usize..][..n as usize].copy_from_slice(&seed[..n as usize]);
                    }
                }
                assert_eq!(hash, expected);
                l.tile(&mut tiled, 0, 0, &linear, pitch, 0).unwrap();
                assert_eq!(tiled, reference);
                let mut output = vec![0x55; linear.len()];
                l.detile(&tiled, 0, 0, &mut output, pitch, 0).unwrap();
                assert_eq!(linear, output);
            }
        }
    }

    #[test]
    fn blocks_hold_their_size_in_elements() {
        for log2 in [8u32, 12, 16] {
            let swizzle = match log2 {
                8 => Swizzle::SW_256B_S,
                12 => Swizzle::SW_4KB_S,
                _ => Swizzle::SW_64KB_D,
            };
            for e in 0..5 {
                let l = Layout::new(&desc(1, 1, 1, 1 << e, swizzle)).unwrap();
                let [w, h, d] = l.block;
                assert_eq!(u64::from(w * h * d) << e, 1 << log2, "{log2} {e}");
                assert_eq!(l.pattern[0].len() as u32, w);
                assert_eq!(l.pattern[1].len() as u32, h);
            }
        }
        let l = Layout::new(&desc(64, 64, 1, 16, Swizzle::SW_64KB_D)).unwrap();
        assert_eq!(l.block, [64, 64, 1]);
        assert_eq!((l.size, l.alignment), (65536, 65536));
    }

    #[test]
    fn addresses_are_a_bijection_and_copies_round_trip() {
        let cases = [
            desc(64, 64, 7, 16, Swizzle::SW_64KB_D),
            desc(100, 60, 7, 4, Swizzle::SW_64KB_D),
            desc(250, 175, 8, 8, Swizzle::SW_4KB_S),
            desc(33, 17, 6, 1, Swizzle::SW_4KB_D),
            desc(40, 9, 3, 2, Swizzle::SW_64KB_S),
            desc(30, 20, 5, 4, Swizzle::LINEAR),
            desc(83, 31, 6, 1, Swizzle::SW_256B_S),
            Desc {
                block: 4,
                ..desc(109, 71, 6, 16, Swizzle::SW_256B_D)
            },
            Desc {
                depth: 3,
                ..desc(70, 70, 4, 16, Swizzle::SW_4KB_S)
            },
            Desc {
                dimension: Dimension::Tex3d,
                depth: 16,
                ..desc(64, 32, 7, 4, Swizzle::SW_4KB_S)
            },
        ];
        for d in cases {
            let l = Layout::new(&d).unwrap();
            let mut seen = vec![false; l.size as usize];
            let mut tiled = vec![0u8; l.size as usize];
            let n = d.element_bytes as usize;
            let slices = if d.dimension == Dimension::Tex3d {
                1
            } else {
                d.depth
            };
            for slice in 0..slices {
                for mip in 0..d.mips {
                    let m = l.mips[mip as usize];
                    let active = m.width as usize * n;
                    let row = active + 13;
                    let plane = row * m.height as usize + 11;
                    let linear: Vec<u8> = (0..plane * m.depth as usize)
                        .map(|i| (i * 7 + mip as usize * 13 + slice as usize) as u8)
                        .collect();
                    l.tile(&mut tiled, mip, slice, &linear, row, plane).unwrap();
                    let mut back = vec![0xcc; linear.len()];
                    l.detile(&tiled, mip, slice, &mut back, row, plane).unwrap();
                    for z in 0..m.depth {
                        let base = z as usize * plane;
                        for y in 0..m.height {
                            let start = base + y as usize * row;
                            assert_eq!(back[start..start + active], linear[start..start + active]);
                            assert!(back[start + active..start + row].iter().all(|&b| b == 0xcc));
                            for x in 0..m.width {
                                let at = l.offset(mip, x, y, slice + z).unwrap() as usize;
                                let source = start + x as usize * n;
                                assert_eq!(tiled[at..at + n], linear[source..source + n]);
                                assert!(at + n <= seen.len(), "{d:?} mip {mip} outside");
                                assert!(at.is_multiple_of(n));
                                assert!(!seen[at], "{d:?} mip {mip} ({x},{y},{z}) twice");
                                seen[at] = true;
                            }
                        }
                        assert!(
                            back[base + plane - 11..base + plane]
                                .iter()
                                .all(|&b| b == 0xcc)
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn hand_checked_addresses() {
        let l = Layout::new(&desc(128, 128, 1, 4, Swizzle::SW_64KB_D)).unwrap();
        assert_eq!(l.offset(0, 1, 0, 0), Ok(1 << 2));
        assert_eq!(l.offset(0, 4, 0, 0), Ok(1 << 6));
        assert_eq!(l.offset(0, 0, 4, 0), Ok(1 << 7));
        assert_eq!(l.offset(0, 0, 8, 0), Ok(1 << 8));
        assert_eq!(l.offset(0, 64, 64, 0), Ok((1 << 15) | (1 << 14)));
        let l = Layout::new(&desc(32, 32, 1, 16, Swizzle::SW_4KB_S)).unwrap();
        assert_eq!(l.block, [16, 16, 1]);
        assert_eq!(l.offset(0, 0, 1, 0), Ok(1 << 4));
        assert_eq!(l.offset(0, 1, 0, 0), Ok(1 << 6));
        assert_eq!(l.offset(0, 16, 0, 0), Ok(4096));
        assert_eq!(l.offset(0, 0, 16, 0), Ok(8192));
    }

    #[test]
    fn mip_tail_comes_first() {
        let l = Layout::new(&desc(64, 64, 9, 16, Swizzle::SW_64KB_D)).unwrap();
        assert_eq!(l.first_tail_mip, 1);
        assert_eq!(l.size, 2 * 65536);
        assert_eq!(l.mips[0].offset, 65536);
        assert_eq!(
            l.mips[1].tail,
            Some(Tail {
                offset: 32768,
                x: 32,
                y: 0
            })
        );
        assert_eq!(l.offset(1, 0, 0, 0), Ok(32768));
        let l = Layout::new(&desc(64, 64, 9, 16, Swizzle::SW_4KB_S)).unwrap();
        assert_eq!(l.first_tail_mip, 3);
        assert_eq!(l.mips[2].offset, 4096);
        assert_eq!(l.mips[1].offset, 8192);
        assert_eq!(l.mips[0].offset, 8192 + 16384);
    }

    #[test]
    fn smaller_mips_need_only_the_start_of_the_texture() {
        let l = Layout::new(&Desc {
            block: 4,
            ..desc(1024, 1024, 9, 8, Swizzle::SW_4KB_S)
        })
        .unwrap();
        assert_eq!(l.bytes_from(0), l.size);
        let needs: Vec<u64> = (0..9).map(|mip| l.bytes_from(mip)).collect();
        assert!(needs.windows(2).all(|w| w[0] >= w[1]), "{needs:?}");
        assert_eq!((l.bytes_from(4), l.bytes_from(8)), (4096, 4096));
        let tiled: Vec<u8> = (0..l.size).map(|i| ((i * 7) >> 3) as u8).collect();
        for mip in 1..9 {
            let m = l.mips[mip as usize];
            let pitch = m.width as usize * 8;
            let read = |tiled: &[u8]| {
                let mut linear = vec![0; pitch * m.height as usize];
                l.detile(tiled, mip, 0, &mut linear, pitch, pitch * m.height as usize)
                    .map(|()| linear)
            };
            let len = l.bytes_from(mip) as usize;
            assert_eq!(read(&tiled[..len]), read(&tiled));
            assert_eq!(read(&tiled[..len - 1]), Err(Error::TooSmall));
        }
        let l = Layout::new(&Desc {
            depth: 2,
            block: 4,
            ..desc(1024, 1024, 9, 8, Swizzle::SW_4KB_S)
        })
        .unwrap();
        assert_eq!(l.bytes_from(4), l.size);
    }

    #[test]
    fn depth_stencil_is_two_planes() {
        let t = Texture::new(&TextureDesc {
            dimension: Dimension::Tex2d,
            width: 1920,
            height: 1080,
            depth_or_array: 1,
            mips: 1,
            format: 20,
            samples: 1,
            swizzle: Swizzle::SW_64KB_Z_X,
        })
        .unwrap();
        assert_eq!(t.planes.len(), 2);
        assert_eq!(t.planes[0].layout.size, 1920 * 1152 * 4);
        assert_eq!(t.planes[1].offset, t.planes[0].layout.size);
        assert_eq!(t.planes[1].layout.size, 2048 * 1280);
        assert!(!t.planes[0].layout.addressable());
    }

    #[test]
    fn texture_copies_reject_overflowing_pitches() {
        let layout = Layout::new(&desc(4, 2, 1, 4, Swizzle::LINEAR)).unwrap();
        let mut tiled = vec![0; layout.size as usize];
        let mut linear = [0; 32];
        assert_eq!(
            layout.detile(&tiled, 0, 0, &mut linear, usize::MAX, 0),
            Err(Error::TooSmall)
        );
        assert_eq!(
            layout.tile(&mut tiled, 0, 0, &linear, usize::MAX, 0),
            Err(Error::TooSmall)
        );
    }

    #[test]
    fn volume_copy_rejects_overflowing_depth_pitch() {
        let layout = Layout::new(&Desc {
            dimension: Dimension::Tex3d,
            depth: 3,
            ..desc(4, 2, 1, 4, Swizzle::LINEAR)
        })
        .unwrap();
        let tiled = vec![0; layout.size as usize];
        let mut linear = [0; 96];
        assert_eq!(
            layout.detile(&tiled, 0, 0, &mut linear, 16, usize::MAX),
            Err(Error::TooSmall)
        );
    }

    #[test]
    fn bad_descriptions_and_buffers_are_refused() {
        assert_eq!(
            Layout::new(&desc(0, 4, 1, 4, Swizzle::SW_4KB_S)),
            Err(Error::Invalid)
        );
        assert_eq!(
            Layout::new(&desc(4, 4, 1, 12, Swizzle::SW_4KB_S)),
            Err(Error::Invalid)
        );
        assert_eq!(
            Layout::new(&desc(4, 4, 1, 4, Swizzle(3))),
            Err(Error::Invalid)
        );
        let l = Layout::new(&desc(64, 64, 1, 4, Swizzle::SW_4KB_S)).unwrap();
        let mut out = vec![0; 64 * 64 * 4];
        assert_eq!(
            l.detile(&[0; 100], 0, 0, &mut out, 256, 0),
            Err(Error::TooSmall)
        );
        assert_eq!(l.offset(0, 64, 0, 0), Err(Error::Invalid));
    }
}
