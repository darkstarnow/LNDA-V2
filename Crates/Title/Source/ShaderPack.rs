pub const MAGIC: &[u8; 4] = b"PS\0\x09";
const HEADER: usize = 0x80;
const GRAPHICS: usize = 0x70;
const COMPUTE: usize = 0x18;
const NONE: u32 = u32::MAX;
pub const STAGES: [&str; 7] = ["PS", "VS", "GS", "ES", "HS", "LS", "fetch"];

fn u32_at(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], o: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(o..o + 8)?.try_into().ok()?))
}

fn text(b: &[u8]) -> &str {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    std::str::from_utf8(&b[..end]).unwrap_or("?")
}

pub struct Pack<'a> {
    file: &'a [u8],
    graphics: &'a [u8],
    compute: &'a [u8],
    blobs: Table<'a>,
    programs: Table<'a>,
}

#[derive(Clone, Copy)]
struct Table<'a> {
    entries: &'a [u8],
    data: &'a [u8],
}

impl<'a> Table<'a> {
    fn get(&self, index: u32) -> Option<&'a [u8]> {
        if index == NONE {
            return None;
        }
        let at = index as usize * 8;
        let off = u32_at(self.entries, at)? as usize;
        let len = u32_at(self.entries, at + 4)? as usize;
        self.data.get(off..off.checked_add(len)?)
    }

    fn len(&self) -> u32 {
        (self.entries.len() / 8) as u32
    }
}

#[derive(Debug, Clone, Copy)]
pub struct Stage<'a> {
    pub program: &'a [u8],
    pub metadata: Option<&'a [u8]>,
}

#[derive(Debug, Clone, Copy)]
pub struct Graphics<'a> {
    pub key: u64,
    pub shader_hash: u64,
    pub pipeline_hash: u64,
    pub depth_stencil_hash: u64,
    pub rasterizer_hash: u64,
    pub blend_hash: u64,
    pub packet: &'a [u8],
    pub metadata: &'a [u8],
    pub stages: [Option<Stage<'a>>; 7],
}

#[derive(Debug, Clone, Copy)]
pub struct Compute<'a> {
    pub shader_hash: u64,
    pub packet: &'a [u8],
    pub metadata: &'a [u8],
    pub stage: Stage<'a>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputElement<'a> {
    pub semantic: &'a str,
    pub index: u32,
    pub format: u32,
    pub slot: u32,
    pub offset: u32,
    pub class: u32,
    pub step_rate: u32,
}

impl<'a> Graphics<'a> {
    pub fn target_formats(&self) -> Option<[u8; 8]> {
        self.metadata.get(0x10..0x18)?.try_into().ok()
    }

    pub fn depth_targets_topology(&self) -> Option<(u8, u8, u8)> {
        let b = self.metadata.get(0x24..0x27)?;
        Some((b[0], b[1], b[2]))
    }

    pub fn samples(&self) -> Option<(u32, u32, u32)> {
        Some((
            u32_at(self.metadata, 0x1c)?,
            u32_at(self.metadata, 0x28)?,
            u32_at(self.metadata, 0x2c)?,
        ))
    }

    pub fn input_layout(&self) -> Option<Vec<InputElement<'a>>> {
        let m = self.metadata;
        let end = 0x4c + u32_at(m, 0x38)? as usize;
        let count = u32_at(m, 0x4c)?;
        let mut at = 0x50;
        let mut out = Vec::new();
        for _ in 0..count {
            let len = u32_at(m, at)? as usize;
            let name = m.get(at + 4..at + 4 + len)?;
            at += 4 + len;
            let f = |i: usize| u32_at(m, at + 4 * i);
            out.push(InputElement {
                semantic: text(name),
                index: f(0)?,
                format: f(1)?,
                slot: f(2)?,
                offset: f(3)?,
                class: f(4)?,
                step_rate: f(5)?,
            });
            at += 24;
        }
        (at == end && end <= m.len()).then_some(out)
    }
}

impl<'a> Pack<'a> {
    pub fn parse(file: &'a [u8]) -> Result<Self, &'static str> {
        if !file.starts_with(MAGIC) {
            return Err("not a shader pack");
        }
        let n = |o| {
            u32_at(file, o)
                .map(|v| v as usize)
                .ok_or("truncated header")
        };
        let (ng, nc, nb, np, blob_bytes, prog_bytes) =
            (n(0x50)?, n(0x54)?, n(0x58)?, n(0x5c)?, n(0x60)?, n(0x64)?);
        let mut at = HEADER;
        let mut take = |len: usize| {
            let part = file.get(at..at.checked_add(len)?)?;
            at += len;
            Some(part)
        };
        let graphics = take(ng * GRAPHICS).ok_or("truncated graphics records")?;
        let compute = take(nc * COMPUTE).ok_or("truncated compute records")?;
        let blob_entries = take(nb * 8).ok_or("truncated blob table")?;
        let prog_entries = take(np * 8).ok_or("truncated program table")?;
        let blob_data = take(blob_bytes).ok_or("truncated blob data")?;
        let prog_data = take(prog_bytes).ok_or("truncated program data")?;
        let pack = Pack {
            file,
            graphics,
            compute,
            blobs: Table {
                entries: blob_entries,
                data: blob_data,
            },
            programs: Table {
                entries: prog_entries,
                data: prog_data,
            },
        };
        for i in 0..pack.blobs.len() {
            pack.blobs.get(i).ok_or("blob outside its data")?;
        }
        for i in 0..pack.programs.len() {
            pack.programs.get(i).ok_or("program outside its data")?;
        }
        let blob = |i: u32| i < pack.blobs.len();
        let prog = |i: u32| i == NONE || i < pack.programs.len();
        let meta = |i: u32| i == NONE || i < pack.blobs.len();
        for r in graphics.as_chunks::<GRAPHICS>().0 {
            let w = |o: usize| u32_at(r, o).unwrap_or(NONE);
            let ok = blob(w(8))
                && blob(w(12))
                && (0..7).all(|s| prog(w(0x38 + 8 * s)) && meta(w(0x3c + 8 * s)));
            if !ok {
                return Err("graphics record refers outside the tables");
            }
        }
        for r in compute.as_chunks::<COMPUTE>().0 {
            let w = |o| u32_at(r, o).unwrap_or(NONE);
            if !(blob(w(8)) && blob(w(12)) && w(16) < pack.programs.len() && meta(w(20))) {
                return Err("compute record refers outside the tables");
            }
        }
        Ok(pack)
    }

    pub fn driver(&self) -> &'a str {
        text(&self.file[4..0x24])
    }

    pub fn compiler(&self) -> &'a str {
        text(&self.file[0x24..0x48])
    }

    pub fn fingerprint(&self) -> u64 {
        u64_at(self.file, 0x48).unwrap_or(0)
    }

    pub fn graphics(&self) -> impl Iterator<Item = Graphics<'a>> + '_ {
        self.graphics.as_chunks::<GRAPHICS>().0.iter().map(|r| {
            let q = |o| u64_at(r, o).unwrap_or(0);
            let w = |o| u32_at(r, o).unwrap_or(NONE);
            let stages = std::array::from_fn(|s| {
                Some(Stage {
                    program: self.programs.get(w(0x38 + 8 * s))?,
                    metadata: self.blobs.get(w(0x3c + 8 * s)),
                })
            });
            Graphics {
                key: q(0),
                shader_hash: q(0x10),
                pipeline_hash: q(0x18),
                depth_stencil_hash: q(0x20),
                rasterizer_hash: q(0x28),
                blend_hash: q(0x30),
                packet: self.blobs.get(w(8)).unwrap_or_default(),
                metadata: self.blobs.get(w(12)).unwrap_or_default(),
                stages,
            }
        })
    }

    pub fn compute(&self) -> impl Iterator<Item = Compute<'a>> + '_ {
        self.compute.as_chunks::<COMPUTE>().0.iter().map(|r| {
            let w = |o| u32_at(r, o).unwrap_or(NONE);
            Compute {
                shader_hash: u64_at(r, 0).unwrap_or(0),
                packet: self.blobs.get(w(8)).unwrap_or_default(),
                metadata: self.blobs.get(w(12)).unwrap_or_default(),
                stage: Stage {
                    program: self.programs.get(w(16)).unwrap_or_default(),
                    metadata: self.blobs.get(w(20)),
                },
            }
        })
    }
}

pub fn find(file: &[u8]) -> Option<std::borrow::Cow<'_, [u8]>> {
    if file.starts_with(MAGIC) {
        return Some(std::borrow::Cow::Borrowed(file));
    }
    let mut chunk = vec![0u8; 0x20000];
    let mut inflate = |at: usize| {
        use zlib_rs::{Inflate, InflateFlush, Status};
        let mut stream = Inflate::new(true, 15);
        let status = stream
            .decompress(file.get(at..)?, &mut chunk, InflateFlush::NoFlush)
            .ok()?;
        (status == Status::StreamEnd).then(|| {
            (
                chunk[..stream.total_out() as usize].to_vec(),
                stream.total_in() as usize,
            )
        })
    };
    let zlib = |at: usize| {
        file[at] == 0x78 && u16::from_be_bytes([file[at], file[at + 1]]).is_multiple_of(31)
    };
    let start = (0..file.len().saturating_sub(1))
        .step_by(16)
        .filter(|&at| zlib(at))
        .find_map(|at| {
            inflate(at)
                .filter(|(out, _)| out.starts_with(MAGIC))
                .map(|o| (at, o))
        })?;
    let (mut at, (mut pack, used)) = start;
    at += used;
    let size = |p: &[u8]| -> Option<usize> {
        let n = |o: usize| u32_at(p, o).map(|v| v as usize);
        let tables = n(0x50)? * GRAPHICS + n(0x54)? * COMPUTE + (n(0x58)? + n(0x5c)?) * 8;
        Some(HEADER + tables + n(0x60)? + n(0x64)?)
    };
    loop {
        if let Some(total) = size(&pack).filter(|&t| pack.len() >= t) {
            return (pack.len() == total).then_some(std::borrow::Cow::Owned(pack));
        }
        at = at.next_multiple_of(16);
        let (out, used) = inflate(at)?;
        pack.extend(out);
        at += used;
    }
}

pub fn program_id(code: &[u8]) -> Option<[u8; 24]> {
    let id: [u8; 24] = code.get(..24)?.try_into().ok()?;
    let nop = |o: usize| u32::from_le_bytes(id[o..o + 4].try_into().expect("4")) >> 16 == 0xD580;
    (nop(0) && nop(12)).then_some(id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn le(out: &mut Vec<u8>, words: &[u32]) {
        for w in words {
            out.extend_from_slice(&w.to_le_bytes());
        }
    }

    fn metadata() -> Vec<u8> {
        let mut m = vec![0; 0x4c];
        m[0x10..0x18].copy_from_slice(&[28, 10, 0, 0, 0, 0, 0, 0]);
        m[0x1c..0x20].copy_from_slice(&u32::MAX.to_le_bytes());
        m[0x24..0x27].copy_from_slice(&[40, 2, 3]);
        m[0x28] = 1;
        let mut layout = Vec::new();
        le(&mut layout, &[2]);
        for (name, words) in [
            ("POSITION", [0, 6, 0, 0, 0, 0]),
            ("COLOR", [1, 28, 1, 12, 1, 1]),
        ] {
            le(&mut layout, &[name.len() as u32 + 1]);
            layout.extend_from_slice(name.as_bytes());
            layout.push(0);
            le(&mut layout, &words);
        }
        m[0x38..0x3c].copy_from_slice(&(layout.len() as u32).to_le_bytes());
        m.extend(layout);
        m.resize(m.len().next_multiple_of(4), 0);
        m
    }

    fn pack_bytes() -> Vec<u8> {
        let nop = |h: u32| [0xD580_0000 | (h & 0xFFFF), 0x03FF_0000, h];
        let mut code0 = Vec::new();
        le(&mut code0, &nop(0x1234));
        le(&mut code0, &nop(0x5678));
        le(&mut code0, &[0xBF81_0000]);
        let programs = [code0, vec![1; 8], vec![2; 12]];
        let blobs = [
            b"packet".to_vec(),
            metadata(),
            b"ps meta".to_vec(),
            vec![3; 28],
            vec![4; 20],
        ];
        let mut out = MAGIC.to_vec();
        out.extend(b"0x00000001".iter().chain(&[0; 22]));
        out.extend(b"1.2.3".iter().chain(&[0; 31]));
        out.extend(0xABCD_u64.to_le_bytes());
        let blob_bytes: usize = blobs.iter().map(Vec::len).sum();
        le(
            &mut out,
            &[
                1,
                1,
                blobs.len() as u32,
                programs.len() as u32,
                blob_bytes as u32,
                0x300,
            ],
        );
        out.resize(HEADER, 0);
        out.extend(0x1111_u64.to_le_bytes());
        le(&mut out, &[0, 1]);
        for h in 1..=5_u64 {
            out.extend(h.to_le_bytes());
        }
        le(&mut out, &[0, 2, 1, NONE]);
        le(&mut out, &[NONE; 10]);
        out.extend(0x2222_u64.to_le_bytes());
        le(&mut out, &[3, 4, 2, NONE]);
        let mut at = 0;
        for b in &blobs {
            le(&mut out, &[at, b.len() as u32]);
            at += b.len() as u32;
        }
        for (i, p) in programs.iter().enumerate() {
            le(&mut out, &[i as u32 * 0x100, p.len() as u32]);
        }
        for b in &blobs {
            out.extend(b);
        }
        let base = out.len();
        for (i, p) in programs.iter().enumerate() {
            out.resize(base + i * 0x100, 0);
            out.extend(p);
        }
        out.resize(base + 0x300, 0);
        out
    }

    #[test]
    fn reads_entries() {
        let file = pack_bytes();
        let pack = Pack::parse(&file).unwrap();
        assert_eq!(pack.driver(), "0x00000001");
        assert_eq!(pack.compiler(), "1.2.3");
        assert_eq!(pack.fingerprint(), 0xABCD);

        let g: Vec<_> = pack.graphics().collect();
        assert_eq!(g.len(), 1);
        let g = g[0];
        assert_eq!(g.key, 0x1111);
        let hashes = [
            g.shader_hash,
            g.pipeline_hash,
            g.depth_stencil_hash,
            g.rasterizer_hash,
        ];
        assert_eq!((hashes, g.blend_hash), ([1, 2, 3, 4], 5));
        assert_eq!(g.packet, b"packet");
        let ps = g.stages[0].unwrap();
        assert_eq!(ps.metadata, Some(&b"ps meta"[..]));
        assert_eq!(
            program_id(ps.program).unwrap()[..4],
            [0x34, 0x12, 0x80, 0xD5]
        );
        let vs = g.stages[1].unwrap();
        assert_eq!((vs.program, vs.metadata), (&[1; 8][..], None));
        assert!(g.stages[2..].iter().all(Option::is_none));
        assert_eq!(program_id(vs.program), None);

        assert_eq!(g.target_formats(), Some([28, 10, 0, 0, 0, 0, 0, 0]));
        assert_eq!(g.depth_targets_topology(), Some((40, 2, 3)));
        assert_eq!(g.samples(), Some((u32::MAX, 1, 0)));
        let layout = g.input_layout().unwrap();
        assert_eq!(layout.len(), 2);
        assert_eq!((layout[0].semantic, layout[0].format), ("POSITION", 6));
        let color = &layout[1];
        assert_eq!(
            (color.semantic, color.index, color.slot, color.offset),
            ("COLOR", 1, 1, 12)
        );
        assert_eq!((color.class, color.step_rate), (1, 1));

        let c: Vec<_> = pack.compute().collect();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].shader_hash, 0x2222);
        assert_eq!((c[0].packet.len(), c[0].metadata.len()), (28, 20));
        assert_eq!(
            (c[0].stage.program, c[0].stage.metadata),
            (&[2; 12][..], None)
        );
    }

    #[test]
    fn finds_a_pack_inside_an_archive() {
        let pack = pack_bytes();
        let zlib = |bytes: &[u8]| {
            let config = zlib_rs::DeflateConfig::new(6);
            let mut out = vec![0; zlib_rs::compress_bound(bytes.len())];
            let (out, status) = zlib_rs::compress_slice(&mut out, bytes, config);
            assert_eq!(status, zlib_rs::ReturnCode::Ok);
            out.to_vec()
        };
        let mut archive = vec![0x5A; 0x103];
        archive.extend(zlib(b"not a pack"));
        for part in pack.chunks(100) {
            archive.resize(archive.len().next_multiple_of(16), 0);
            archive.extend(zlib(part));
        }
        archive.extend([0; 40]);
        assert_eq!(find(&archive).as_deref(), Some(&pack[..]));
        assert_eq!(find(&pack).as_deref(), Some(&pack[..]));
        assert!(find(&archive[..archive.len() - 200]).is_none());
    }

    #[test]
    fn rejects_bad_files() {
        let file = pack_bytes();
        assert_eq!(Pack::parse(b"SGD5").err(), Some("not a shader pack"));
        assert!(Pack::parse(&file[..file.len() - 1]).is_err());
        let mut bad = file.clone();
        bad[HEADER + 0x38..HEADER + 0x3c].copy_from_slice(&9_u32.to_le_bytes());
        assert_eq!(
            Pack::parse(&bad).err(),
            Some("graphics record refers outside the tables")
        );
        let mut bad = file;
        let table = HEADER + GRAPHICS + COMPUTE;
        bad[table + 4..table + 8].copy_from_slice(&0xFFFF_u32.to_le_bytes());
        assert_eq!(Pack::parse(&bad).err(), Some("blob outside its data"));
    }
}
