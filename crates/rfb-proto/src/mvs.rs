//! Portable MultiVariant Scaled (MVS) frame decoder.
//!
//! Wire layout, verified 2026-11-04 against `mm.local` (see
//! `docs/findings/2026-11-04-codex-mvs-streams.md`):
//!
//! ```text
//! +0x00 u8    type_of_update (0 = partial; 1 = full uses a different header)
//! +0x01 u8    p1
//! +0x02 u8    p2
//! +0x03 u8[3] data_off, big-endian, offset from the body start
//! +0x06 ..    stream A: commands only
//! data_off .. stream B: every command payload
//! ```
//!
//! Stream A starts with one flag bit and then repeats `[3-bit command][run]`.
//! A run is one flag bit (0 = single tile), else four bits giving `v + 1`
//! (1..=15); `v == 15` escapes to `16 + value` where value is a base-128
//! integer (three bytes maximum, high bit = continuation). Each stream ends with
//! a **bit-aligned** `0x6d` marker followed by zero padding.
//!
//! Payload widths in stream B:
//!
//! | code | payload |
//! |------|---------|
//! | 0 fill, 1 copy-previous, 2 copy-above, 7 cache+1 | none |
//! | 3 two-colour bitmap | 8-bit row mask, one 8-bit row per clear bit |
//! | 4 colour/bitmap | 2 flags; see [`MvsDecoder`] |
//! | 5 DCT | variable, see [`PortableBlocks`] |
//! | 6 cache index | 16 bits (hi byte, lo byte) |
//!
//! Tiles are 8x8; a 2560x1600 frame is 320x200 = 64000 tiles.
//!
//! # Verified rendering rules (2026-11-04)
//!
//! * The DCT coefficient reference tile is **zeroed per update body** and then
//!   chained: every block uses the previous block's output. Fill/bitmap/solid
//!   commands do not touch it (`DecodeMVSPartialUpdate` memsets a 0x180 buffer
//!   before the body and `Handler5` copies `out` to `ref` after each block).
//! * The block buffer is three 64-word i16 planes: Y at +0, Cb at +0x80,
//!   Cr at +0x100. Only chroma DC is coded; chroma AC stays zero.
//! * `code 0` fills the tile with white. 57925 fill tiles of the reference
//!   capture are pure `(255,255,255)` and the running solid colour cannot be
//!   the source (the first fill follows a `(32,32,32)` solid).
//! * `code 1` copies the **previous** tile in raster order and `code 2` copies
//!   the tile **above**; 3272/3272 and 151/151 tiles of the reference capture
//!   match those sources and 0/151 match the other one.
//! * `code 4` keeps two independent colour states: the solid colour (used when
//!   `b1 == 0, b2 == 1`) is only written when a solid colour is actually read,
//!   and the two bitmap colours (used by `code 3` and reuse bitmaps) live in
//!   their own slots. Verified by tile 239 of the capture.
//!
//! Evidence: `artifacts/mvs-fixture-quant.bin` (type-2 quant tables) and
//! `artifacts/mvs-fixture.bin`, a 2560x1600 type-0 body captured from the
//! `mm.local` desktop. The original 2,352-block white-desktop body cited in
//! `docs/findings/2026-11-04-codex-mvs-streams.md` §5.1 has been replaced by
//! this 3,388-block capture (a live `stream --encoding 1011` run overwrote it);
//! the value path is still differential-tested against the framework on every
//! block of the new fixture (`tools/probe/mvs_block_values.py`).

use anyhow::{bail, Result};

/// End-of-stream marker, bit-aligned (not byte-aligned).
pub const STREAM_MARKER: u8 = 0x6d;
/// Tile edge in pixels.
pub const TILE: usize = 8;

/// Transposed JPEG zigzag, the framework table at `0x7ff9224c2340`.
///
/// Coded coefficient `index` is stored at plane offset `SCAN[index]`.
/// `out[8*u+v]` holds `F[v][u]`, i.e. the stored row index is the *horizontal*
/// frequency.
pub const SCAN: [usize; 64] = [
    0, 8, 1, 2, 9, 16, 24, 17, 10, 3, 4, 11, 18, 25, 32, 40, 33, 26, 19, 12, 5, 6, 13, 20, 27,
    34, 41, 48, 56, 49, 42, 35, 28, 21, 14, 7, 15, 22, 29, 36, 43, 50, 57, 58, 51, 44, 37, 30,
    23, 31, 38, 45, 52, 59, 60, 53, 46, 39, 47, 54, 61, 62, 55, 63,
];

/// Framework default luma quantisation table (`0x7ff9224c2390`).
pub const DEFAULT_QUANT_LUMA: [u8; 64] = [
    16, 11, 11, 14, 24, 22, 24, 33, 11, 12, 11, 11, 26, 18, 19, 23, 13, 13, 14, 24, 20, 21, 22,
    28, 13, 17, 14, 21, 23, 25, 34, 34, 17, 22, 18, 25, 31, 32, 34, 44, 24, 20, 17, 27, 35, 40,
    44, 54, 30, 35, 28, 30, 34, 44, 54, 64, 35, 30, 33, 38, 45, 55, 65, 75,
];

/// Framework default chroma quantisation table (`0x7ff9224c23d0`).
pub const DEFAULT_QUANT_CHROMA: [u8; 64] = [
    19, 19, 24, 47, 76, 99, 99, 99, 19, 21, 26, 66, 99, 99, 99, 99, 24, 26, 56, 99, 99, 99, 99,
    99, 47, 66, 99, 99, 99, 99, 99, 99, 76, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
    99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99, 99,
];

/// Parsed type-0 MVS rectangle body.
#[derive(Debug, Clone, Copy)]
pub struct Header {
    pub type_of_update: u8,
    pub p1: u8,
    pub p2: u8,
    pub data_off: usize,
}

impl Header {
    pub fn parse(body: &[u8]) -> Result<Self> {
        if body.len() < 6 {
            bail!("MVS body too short: {} bytes", body.len());
        }
        let data_off = ((body[3] as usize) << 16) | ((body[4] as usize) << 8) | body[5] as usize;
        if data_off < 6 || data_off > body.len() {
            bail!("MVS data_off {data_off} outside body of {}", body.len());
        }
        Ok(Self {
            type_of_update: body[0],
            p1: body[1],
            p2: body[2],
            data_off,
        })
    }
}

/// MSB-first bit reader over one stream.
pub struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    pub fn bit_pos(&self) -> usize {
        self.pos
    }

    pub fn remaining(&self) -> usize {
        self.data.len() * 8 - self.pos
    }

    pub fn read_bit(&mut self) -> Result<u32> {
        if self.pos >= self.data.len() * 8 {
            bail!("MVS stream exhausted at bit {}", self.pos);
        }
        let byte = self.data[self.pos >> 3];
        let bit = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Ok(bit as u32)
    }

    pub fn read(&mut self, bits: u32) -> Result<u32> {
        debug_assert!(bits <= 32);
        let mut v = 0u32;
        for _ in 0..bits {
            v = (v << 1) | self.read_bit()?;
        }
        Ok(v)
    }

    /// Consume the bit-aligned `0x6d` marker and verify zero padding.
    pub fn finish(&mut self) -> Result<()> {
        let marker = self.read(8)?;
        if marker != STREAM_MARKER as u32 {
            bail!(
                "MVS stream marker 0x{marker:02x} != 0x6d at bit {}",
                self.pos - 8
            );
        }
        while self.remaining() >= 8 {
            if self.read(8)? != 0 {
                bail!("non-zero MVS padding at bit {}", self.pos - 8);
            }
        }
        Ok(())
    }
}

/// One command from stream A.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Fill the tile with white.
    Fill,
    /// Copy the previous tile in raster order.
    CopyPrevious,
    /// Copy the tile above.
    CopyAbove,
    /// Two-colour 1-bit bitmap, reusing the last bitmap colours.
    Bitmap,
    /// Flagged colour or bitmap.
    Colour,
    /// DCT block, decoded by [`PortableBlocks`].
    Dct,
    /// Tile cache index, 16 bits.
    Cache16,
    /// Tile cache, previous index + 1.
    Cache17,
}

impl Command {
    pub fn from_code(code: u32) -> Result<Self> {
        Ok(match code {
            0 => Command::Fill,
            1 => Command::CopyPrevious,
            2 => Command::CopyAbove,
            3 => Command::Bitmap,
            4 => Command::Colour,
            5 => Command::Dct,
            6 => Command::Cache16,
            7 => Command::Cache17,
            _ => bail!("invalid MVS command {code}"),
        })
    }
}

/// A command plus the number of tiles it covers.
#[derive(Debug, Clone, Copy)]
pub struct TileRun {
    pub command: Command,
    pub tiles: u32,
}

/// Iterator over `[command][run]` pairs in stream A.
///
/// The command stream carries no length; the decoder stops after the tile
/// budget supplied by the rectangle. Stream A then holds the bit-aligned
/// `0x6d` marker, which [`CommandIter::finish`] verifies.
pub struct CommandIter<'a> {
    bits: BitReader<'a>,
    left: u32,
    done: bool,
}

impl<'a> CommandIter<'a> {
    /// Consume the leading flag bit and return the iterator.
    pub fn new(body_stream_a: &'a [u8], tiles: u32) -> Result<Self> {
        let mut bits = BitReader::new(body_stream_a);
        bits.read_bit()?; // leading flag, observed 0
        Ok(Self {
            bits,
            left: tiles,
            done: false,
        })
    }

    /// Consume the trailing marker and padding after the last command.
    pub fn finish(&mut self) -> Result<()> {
        self.bits.finish()
    }

    fn read_run(&mut self) -> Result<u32> {
        if self.bits.read_bit()? == 0 {
            return Ok(0);
        }
        let v = self.bits.read(4)?;
        if v < 15 {
            return Ok(v + 1);
        }
        // Escape: base-128 integer, high bit = continuation (three bytes max).
        let mut value = 0u32;
        for shift in [0, 7, 14] {
            let byte = self.bits.read(8)?;
            value |= (byte & 0x7f) << shift;
            if byte & 0x80 == 0 {
                return Ok(16 + value);
            }
        }
        bail!("MVS run escape exceeds three bytes")
    }
}

impl Iterator for CommandIter<'_> {
    type Item = Result<TileRun>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done || self.left == 0 {
            return None;
        }
        let command = match self.bits.read(3).and_then(Command::from_code) {
            Ok(c) => c,
            Err(e) => {
                self.done = true;
                return Some(Err(e));
            }
        };
        match self.read_run() {
            Ok(run) => {
                let tiles = run + 1;
                if tiles > self.left {
                    self.done = true;
                    return Some(Err(anyhow::anyhow!(
                        "MVS run of {tiles} tiles exceeds the {} left",
                        self.left
                    )));
                }
                self.left -= tiles;
                Some(Ok(TileRun { command, tiles }))
            }
            Err(e) => {
                self.done = true;
                Some(Err(e))
            }
        }
    }
}

/// Owner of the per-frame tile canvas.
pub trait TileSink {
    /// Called for every tile in raster order as commands are applied.
    fn tile(&mut self, index: usize, kind: Command);
}

/// Decoder for DCT blocks (command 5).
pub trait BlockDecoder {
    fn decode_block(&mut self, payload: &mut BitReader<'_>, p1: u8, p2: u8) -> Result<()>;
}

/// A [`BlockDecoder`] that rejects DCT blocks.
pub struct NoDct;

impl BlockDecoder for NoDct {
    fn decode_block(&mut self, _payload: &mut BitReader<'_>, _p1: u8, _p2: u8) -> Result<()> {
        bail!("DCT block requires a BlockDecoder backend")
    }
}

/// Walk a whole type-0 frame, applying every tile to `sink`.
///
/// `tiles` is `tiles_x * tiles_y` for the rectangle. Returns the number of
/// tiles emitted and verifies both stream terminators.
pub fn decode_frame<S: TileSink, B: BlockDecoder>(
    body: &[u8],
    tiles: u32,
    sink: &mut S,
    blocks: &mut B,
) -> Result<usize> {
    let header = Header::parse(body)?;
    if header.type_of_update != 0 {
        bail!("only type-0 (partial) MVS bodies are decoded");
    }
    let mut commands = CommandIter::new(&body[6..header.data_off], tiles)?;
    let mut payload = BitReader::new(&body[header.data_off..]);

    let mut index = 0usize;
    for run in &mut commands {
        let run = run?;
        for _ in 0..run.tiles {
            decode_payload(run.command, &mut payload, header, blocks)?;
            sink.tile(index, run.command);
            index += 1;
        }
    }
    commands.finish()?;
    payload.finish()?;
    Ok(index)
}

/// Consume the payload for one tile of `command`.
pub fn decode_payload<B: BlockDecoder>(
    command: Command,
    payload: &mut BitReader<'_>,
    header: Header,
    blocks: &mut B,
) -> Result<()> {
    match command {
        Command::Fill
        | Command::CopyPrevious
        | Command::CopyAbove
        | Command::Cache17 => Ok(()),
        Command::Bitmap => {
            let mask = payload.read(8)? as u8;
            for row in 0..8 {
                if mask & (0x80 >> row) == 0 {
                    payload.read(8)?;
                }
            }
            Ok(())
        }
        Command::Colour => {
            let bitmap = payload.read(1)? == 1;
            let reuse = payload.read(1)? == 1;
            if !reuse {
                // (1 + bitmap) colours of Y8 Cb6 Cr6
                for _ in 0..(1 + bitmap as u32) {
                    payload.read(8)?;
                    payload.read(6)?;
                    payload.read(6)?;
                }
            }
            if bitmap {
                let mask = payload.read(8)? as u8;
                for row in 0..8 {
                    if mask & (0x80 >> row) == 0 {
                        payload.read(8)?;
                    }
                }
            }
            Ok(())
        }
        Command::Cache16 => {
            payload.read(16)?;
            Ok(())
        }
        Command::Dct => blocks.decode_block(payload, header.p1, header.p2),
    }
}

/// Tile edge in words of one coefficient plane.
const PLANE: usize = 64;

fn i16w(x: i32) -> i16 {
    x as i16 // truncation: native low-16 wrap
}

fn half_toward_zero(x: i16) -> i32 {
    ((x as i32) + i32::from(x < 0)) >> 1
}

/// Coefficient scale multiplier for `index` (native `0x480e12` / `0x481217`).
pub fn coefficient_scale(index: usize, param: u8) -> i32 {
    if param < 15 {
        if index < param as usize {
            8
        } else {
            16
        }
    } else if index < 6 || index < param as usize {
        // Native has two separate `2` arms (indices 1..6 and 6..param).
        2
    } else {
        8
    }
}

/// Portable DCT block decoder: coefficients and bit consumption.
///
/// Transcribed from `0x7ff922480b28`; the coefficient values are validated
/// against the framework on all 2,352 blocks of the reference capture
/// (`artifacts/mvs-4.bin`, 4,704/4,704 with zero and random references).
pub struct PortableBlocks {
    pub blocks: usize,
    pub bits: usize,
    /// Coefficients of the most recently decoded block, in plane order.
    pub coeff: [i16; 192],
    /// Chain reference for the next block: the previous block's output.
    /// Zeroed at the start of an update body.
    pub reference: [i16; 192],
    /// True when the block reused the reference (first header bit was 1).
    pub last_reuse: bool,
    /// Parameter actually used by the block (`p1` or `p2`), for the spatial
    /// record that type-1 refinement reads back.
    pub last_param: u8,
}

impl Default for PortableBlocks {
    fn default() -> Self {
        Self {
            blocks: 0,
            bits: 0,
            coeff: [0; 192],
            reference: [0; 192],
            last_reuse: false,
            last_param: 0,
        }
    }
}

impl PortableBlocks {
    /// Drop the prediction chain (called once per update body).
    pub fn reset_reference(&mut self) {
        self.reference = [0; 192];
    }

    /// Non-unit coefficient magnitude token; returns the signed value.
    fn magnitude(r: &mut BitReader<'_>, index: usize) -> Result<i32> {
        let k = read_unary(r, if index < 6 { 33 } else { 19 })?;
        let (suffix_bits, base) = if index < 6 {
            if k < 4 {
                (3, 4 * k + 2)
            } else {
                (4, 8 * k - 14)
            }
        } else {
            let base = if k == 0 {
                2
            } else if k == 1 {
                4
            } else {
                8 * k - 8
            };
            ((k + 2).min(4), base)
        };
        let suffix = r.read(suffix_bits)?;
        let magnitude = base as i32 + (suffix >> 1) as i32;
        Ok(if suffix & 1 == 1 {
            -magnitude
        } else {
            magnitude
        })
    }
}

impl BlockDecoder for PortableBlocks {
    fn decode_block(&mut self, r: &mut BitReader<'_>, p1: u8, p2: u8) -> Result<()> {
        let start = r.bit_pos();
        let mut out = [0i16; 192];
        let reference = self.reference;
        self.last_reuse = r.read_bit()? == 1;
        if self.last_reuse {
            // Reuse: copy the luma plane and the two chroma DC words.
            out[..PLANE].copy_from_slice(&reference[..PLANE]);
            out[64] = reference[64];
            out[128] = reference[128];
        } else {
            let copy_chroma = r.read_bit()? == 1;
            let param = if r.read_bit()? == 1 { p2 } else { p1 };
            self.last_param = param;
            if copy_chroma {
                out[64] = reference[64];
                out[128] = reference[128];
            } else {
                let cb = predictor_symbol(r)?;
                let cr = predictor_symbol(r)?;
                out[64] = i16w(2 * (half_toward_zero(reference[64]) - cb));
                out[128] = i16w(2 * (half_toward_zero(reference[128]) - cr));
            }
            let y = predictor_symbol(r)?;
            out[0] = i16w(reference[0] as i32 - y);

            let mut index = 1usize;
            while index < 64 {
                let scale = coefficient_scale(index, param);
                let value = if r.read_bit()? == 1 {
                    Self::magnitude(r, index)?
                } else {
                    let code = r.read(2)?;
                    if code == 1 {
                        if r.read_bit()? == 0 {
                            break; // end of block
                        }
                        let mut run = 3 + r.read(2)? as usize;
                        if run == 6 {
                            loop {
                                let part = r.read(3)? as usize;
                                run += part;
                                if index + run >= 64 {
                                    bail!("extended zero run outside block");
                                }
                                if part != 7 {
                                    break;
                                }
                            }
                        }
                        index += run;
                        continue;
                    }
                    match code {
                        0 => 0,
                        2 => 1,
                        _ => -1,
                    }
                };
                out[SCAN[index]] = i16w(scale * value);
                index += 1;
            }
        }
        self.coeff = out;
        self.reference = out;
        self.blocks += 1;
        self.bits += r.bit_pos() - start;
        Ok(())
    }
}

/// Per-session quantisation tables (type-2 update, else framework defaults).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantTables {
    pub luma: [u8; 64],
    pub chroma: [u8; 64],
}

impl Default for QuantTables {
    fn default() -> Self {
        Self {
            luma: DEFAULT_QUANT_LUMA,
            chroma: DEFAULT_QUANT_CHROMA,
        }
    }
}

impl QuantTables {
    /// Parse a type-2 update body: `[2][Y 64 bytes][Cb/Cr 64 bytes]`.
    pub fn from_type2(body: &[u8]) -> Result<Self> {
        if body.len() < 129 {
            bail!("MVS type-2 body is {} bytes, need 129", body.len());
        }
        if body[0] != 2 {
            bail!("MVS type-2 body starts with type {}", body[0]);
        }
        let mut luma = [0u8; 64];
        let mut chroma = [0u8; 64];
        luma.copy_from_slice(&body[1..65]);
        chroma.copy_from_slice(&body[65..129]);
        Ok(Self { luma, chroma })
    }
}

/// Separable 8x8 IDCT basis with the native fixed-point scale folded in.
///
/// The native transform (`0x7ff9223bc67d`) is a fixed-point SSE IDCT; this is
/// the mathematical equivalent at scale 64. It matches the framework wrapper
/// (`0x3ff9223bc594`) on every block of the reference capture (3,388/3,388
/// with a maximum channel difference of 1).
pub struct Idct {
    /// `cos[x][u] = cos((2x+1) u pi / 16)`
    cos: [[f64; 8]; 8],
}

impl Default for Idct {
    fn default() -> Self {
        Self::new()
    }
}

impl Idct {
    pub fn new() -> Self {
        let mut cos = [[0f64; 8]; 8];
        for (x, row) in cos.iter_mut().enumerate() {
            for (u, c) in row.iter_mut().enumerate() {
                *c = (((2 * x + 1) as f64) * u as f64 * std::f64::consts::PI / 16.0).cos();
            }
        }
        Self { cos }
    }

    /// Dequantise `coeff` (native plane order) with `quant` and transform.
    ///
    /// Returns 64 row-major samples at scale 64 (centred: add 8192 in the
    /// colour helper). Quant entries are raw values, not the native `<< 4`
    /// expansion.
    pub fn plane(&self, coeff: &[i16], quant: &[u8; 64]) -> [i32; 64] {
        const C: [f64; 8] = [
            std::f64::consts::FRAC_1_SQRT_2,
            1.0,
            1.0,
            1.0,
            1.0,
            1.0,
            1.0,
            1.0,
        ];
        // f[v][u] = coeff[8u+v] * quant[v][u], wrapped to i16 like `pmullw`.
        let mut f = [[0f64; 8]; 8];
        for v in 0..8 {
            for u in 0..8 {
                let c = coeff[8 * u + v] as i32 * quant[8 * v + u] as i32;
                f[v][u] = i16w(c) as f64;
            }
        }
        // Vertical pass: tmp[y][u] = sum_v C(v) f[v][u] cos((2y+1)v pi/16)
        let mut tmp = [[0f64; 8]; 8];
        for (y, row) in tmp.iter_mut().enumerate() {
            for (u, slot) in row.iter_mut().enumerate() {
                let mut s = 0.0;
                for v in 0..8 {
                    s += C[v] * f[v][u] * self.cos[y][v];
                }
                *slot = s;
            }
        }
        // Horizontal pass, then scale: 64 * (1/4) = 16.
        let mut out = [0i32; 64];
        for y in 0..8 {
            for x in 0..8 {
                let mut s = 0.0;
                for u in 0..8 {
                    s += C[u] * tmp[y][u] * self.cos[x][u];
                }
                out[8 * y + x] = (16.0 * s).round() as i32;
            }
        }
        out
    }
}

fn sat16(x: i32) -> i32 {
    x.clamp(-32768, 32767)
}

fn clip8(x: i32) -> u8 {
    x.clamp(0, 255) as u8
}

/// Colour conversion helper `0x7ff9223bcb21`, transcribed exactly.
///
/// Inputs are scale-64 spatial samples: `y` is centred, `cb`/`cr` are
/// zero-centred. Returns BGR, the framebuffer byte order.
pub fn rgb_from_idct(y: i32, cb: i32, cr: i32) -> [u8; 3] {
    let y = i16w(y + 8192) as i32;
    // `pmulhw` semantics: wrap the 32-bit product, then take the high word.
    // Refined/cached tiles can drive the spatial samples far beyond the range a
    // plain type-0 frame produces, so these must not panic on overflow.
    let green = sat16(
        sat16(y + ((cb.wrapping_mul(-22554)) >> 16))
            + sat16(((cr.wrapping_mul(18734)) >> 16) - cr),
    );
    let blue = sat16(sat16(y + cb) + sat16(cb + ((cb.wrapping_mul(-14942)) >> 16)));
    let red = sat16(sat16(y + cr) + ((cr.wrapping_mul(26345)) >> 16));
    let scale = |v: i32| clip8(sat16(v + 32) >> 6);
    [scale(blue), scale(green), scale(red)]
}

fn round_div(num: i32, den: i32) -> i32 {
    if num >= 0 {
        (num + den / 2) / den
    } else {
        -((-num + den / 2) / den)
    }
}

/// Solid-colour conversion `0x7ff922481a85`, transcribed exactly.
///
/// `y` is the 8-bit luma, `cb`/`cr` are the 6-bit chroma already shifted left
/// by two (0..=252). Returns BGR.
pub fn solid_rgb(y: u8, cb: u8, cr: u8) -> [u8; 3] {
    let y = y as i32;
    let cb = cb as i32 - 128;
    let cr = cr as i32 - 128;
    let red = y + round_div(1402 * cr, 1000);
    let green = y + ((-46802 * cr - 22554 * cb + 32768) >> 16);
    let blue = y + round_div(1772 * cb, 1000);
    [clip8(blue), clip8(green), clip8(red)]
}

/// A rectangle within the framebuffer, in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

impl Rect {
    pub fn new(x: u16, y: u16, w: u16, h: u16) -> Self {
        Self { x, y, w, h }
    }
}

/// Counters from one decoded update body.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameStats {
    pub tiles: usize,
    pub commands: usize,
    pub dct_blocks: usize,
    pub dct_bits: usize,
}

// ---------------------------------------------------------------------------
// Type-1 refinement, spatial records and the coefficient ring cache
// (docs: artifacts/push-recon.md C1/C2/C3/C5).
// ---------------------------------------------------------------------------

/// Entries in the coefficient ring (`viewer+0xda0`, `calloc(99, 65000)`).
pub const CACHE_ENTRIES: usize = 65_000;
/// Bytes per ring entry: 64 luma + 15 Cb + 20 Cr signed coefficients.
pub const CACHE_STRIDE: usize = 99;

/// AAC-style counts of the framework's luma AC Huffman table (`0x4c34c0`).
/// The native table struct is `[unused][counts; 16][symbols]`, so the dump
/// tool must read 17 bytes and drop the first one.
pub const LUMA_AC_COUNTS: [u8; 16] =
    [0, 2, 1, 3, 3, 2, 4, 3, 5, 5, 4, 4, 0, 0, 1, 125];

/// Symbols of the luma AC table (`0x4c34e0`, 162 bytes).
pub const LUMA_AC_SYMBOLS: [u8; 162] = [
    0x01, 0x02, 0x03, 0x00, 0x04, 0x11, 0x05, 0x12, 0x21, 0x31, 0x41, 0x06, 0x13, 0x51, 0x61, 0x07,
    0x22, 0x71, 0x14, 0x32, 0x81, 0x91, 0xa1, 0x08, 0x23, 0x42, 0xb1, 0xc1, 0x15, 0x52, 0xd1, 0xf0,
    0x24, 0x33, 0x62, 0x72, 0x82, 0x09, 0x0a, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x25, 0x26, 0x27, 0x28,
    0x29, 0x2a, 0x34, 0x35, 0x36, 0x37, 0x38, 0x39, 0x3a, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49,
    0x4a, 0x53, 0x54, 0x55, 0x56, 0x57, 0x58, 0x59, 0x5a, 0x63, 0x64, 0x65, 0x66, 0x67, 0x68, 0x69,
    0x6a, 0x73, 0x74, 0x75, 0x76, 0x77, 0x78, 0x79, 0x7a, 0x83, 0x84, 0x85, 0x86, 0x87, 0x88, 0x89,
    0x8a, 0x92, 0x93, 0x94, 0x95, 0x96, 0x97, 0x98, 0x99, 0x9a, 0xa2, 0xa3, 0xa4, 0xa5, 0xa6, 0xa7,
    0xa8, 0xa9, 0xaa, 0xb2, 0xb3, 0xb4, 0xb5, 0xb6, 0xb7, 0xb8, 0xb9, 0xba, 0xc2, 0xc3, 0xc4, 0xc5,
    0xc6, 0xc7, 0xc8, 0xc9, 0xca, 0xd2, 0xd3, 0xd4, 0xd5, 0xd6, 0xd7, 0xd8, 0xd9, 0xda, 0xe1, 0xe2,
    0xe3, 0xe4, 0xe5, 0xe6, 0xe7, 0xe8, 0xe9, 0xea, 0xf1, 0xf2, 0xf3, 0xf4, 0xf5, 0xf6, 0xf7, 0xf8,
    0xf9, 0xfa,
];

/// Canonical Huffman decoder built from a JPEG-style counts/symbols pair.
pub struct Huffman {
    counts: [u8; 16],
    symbols: Vec<u8>,
    first_code: [i32; 17],
    first_index: [i32; 17],
}

impl Huffman {
    pub fn new(counts: [u8; 16], symbols: &[u8]) -> Self {
        let mut first_code = [0i32; 17];
        let mut first_index = [0i32; 17];
        let mut code = 0i32;
        let mut index = 0i32;
        for len in 1..=16 {
            first_code[len] = code;
            first_index[len] = index;
            code = (code + counts[len - 1] as i32) << 1;
            index += counts[len - 1] as i32;
        }
        Self {
            counts,
            symbols: symbols.to_vec(),
            first_code,
            first_index,
        }
    }

    pub fn luma_ac() -> Self {
        Self::new(LUMA_AC_COUNTS, &LUMA_AC_SYMBOLS)
    }

    pub fn decode(&self, r: &mut BitReader<'_>) -> Result<u8> {
        let mut code = 0i32;
        for len in 1..=16 {
            code = (code << 1) | r.read_bit()? as i32;
            let count = self.counts[len - 1] as i32;
            if code - self.first_code[len] < count {
                let i = self.first_index[len] + (code - self.first_code[len]);
                return Ok(self.symbols[i as usize]);
            }
        }
        bail!("invalid Huffman code")
    }
}

/// One 84-byte spatial record (`viewer+0xd80`, stride 0x54).
///
/// Stores the *base* coefficients of a tile; type-1 bodies refine them. It is
/// not pixel storage and not the code-6/7 ring.
#[derive(Clone, Copy)]
pub struct SpatialRecord {
    /// Selected DCT multiplier threshold (`p1` or `p2`).
    pub param: u8,
    /// Luma coefficients in coded order, low byte of the i16.
    pub y: [i8; 64],
    pub cr_count: u8,
    pub cr_dc: i8,
    pub cb_count: u8,
    pub cb_dc: i8,
    pub generation: u32,
    pub source_record: i32,
    pub source_framebuffer_offset: u32,
}

impl Default for SpatialRecord {
    fn default() -> Self {
        // calloc semantics: every field starts at zero (source_record 0, not -1).
        Self {
            param: 0,
            y: [0; 64],
            cr_count: 0,
            cr_dc: 0,
            cb_count: 0,
            cb_dc: 0,
            generation: 0,
            source_record: 0,
            source_framebuffer_offset: 0,
        }
    }
}

/// The 99-byte coefficient ring. Entries are inserted only by type-1 code 1.
pub struct CacheRing {
    entries: Vec<[u8; CACHE_STRIDE]>,
    /// Last successfully *read* index; code 7 and type-1 code 3 use it.
    pub last_read: u16,
    write_cursor: u16,
    /// Number of insertions so far (also bounds valid read indices).
    pub total_insertions: u32,
}

impl Default for CacheRing {
    fn default() -> Self {
        Self::new()
    }
}

impl CacheRing {
    pub fn new() -> Self {
        Self {
            entries: vec![[0u8; CACHE_STRIDE]; CACHE_ENTRIES],
            last_read: 0,
            write_cursor: 0,
            total_insertions: 0,
        }
    }

    /// Native validity: `u16(index + 535) >= 536` (excludes 0 and >= 65001)
    /// and `index <= total_insertions`.
    fn valid(&self, index: u16) -> bool {
        let i = index as usize;
        i != 0 && i < CACHE_ENTRIES && (i as u32) <= self.total_insertions
    }

    /// Read an entry; sets `last_read` only on success, like `0x481ae3`.
    pub fn read(&mut self, index: u16) -> Option<[u8; CACHE_STRIDE]> {
        if !self.valid(index) {
            return None;
        }
        self.last_read = index;
        Some(self.entries[index as usize])
    }

    /// Ring write cursor (last inserted slot).
    pub fn write_cursor(&self) -> u16 {
        self.write_cursor
    }

    /// Append an entry (`0x481cd3`): cursor wraps 65000 -> 1.
    pub fn insert(&mut self, entry: [u8; CACHE_STRIDE]) {
        self.total_insertions += 1;
        self.write_cursor = self.write_cursor.wrapping_add(1);
        if self.write_cursor as usize == CACHE_ENTRIES {
            self.write_cursor = 1;
        }
        self.entries[self.write_cursor as usize] = entry;
    }
}

/// Expand a 99-byte ring entry into three coefficient planes.
pub fn expand_cache_entry(entry: &[u8; CACHE_STRIDE]) -> [i16; 192] {
    let mut coeff = [0i16; 192];
    for j in 0..64 {
        coeff[SCAN[j]] = entry[j] as i8 as i16;
    }
    // C8: ring order is Y64, Cr15, Cb20 (plane order is still Y, Cb, Cr).
    for j in 0..15 {
        coeff[128 + SCAN[j]] = entry[64 + j] as i8 as i16;
    }
    for j in 0..20 {
        coeff[64 + SCAN[j]] = entry[79 + j] as i8 as i16;
    }
    coeff
}

fn read_unary(r: &mut BitReader<'_>, limit: u32) -> Result<u32> {
    let mut ones = 0;
    while r.read_bit()? == 1 {
        ones += 1;
        if ones >= limit {
            bail!("invalid unary prefix");
        }
    }
    Ok(ones)
}

/// Signed predictor symbol (`0x7ff9223bc1bf`).
fn predictor_symbol(r: &mut BitReader<'_>) -> Result<i32> {
    let k = read_unary(r, 39)?;
    let suffix = r.read((k + 1).min(4))?;
    if k == 0 {
        if suffix == 0 {
            return Ok(0);
        }
        let negative = r.read_bit()? == 1;
        return Ok(if negative { -1 } else { 1 });
    }
    let base = if k < 3 { 1 << k } else { 8 * k - 16 };
    let magnitude = base + (suffix >> 1);
    Ok(if suffix & 1 == 1 {
        -(magnitude as i32)
    } else {
        magnitude as i32
    })
}

/// Type-1 refinement step for one coefficient (`3b95b9..3b9df9`).
fn refine_step(old: i8, r: &mut BitReader<'_>, width: u32) -> Result<i8> {
    if old != 0 {
        let v = r.read(width)? as i32;
        let delta = if old > 0 { v } else { -v };
        return Ok((old as i32 + delta) as i8);
    }
    if width != 1 {
        return Ok(predictor_symbol(r)? as i8);
    }
    if r.read(1)? == 0 {
        return Ok(0);
    }
    Ok(if r.read(1)? == 1 { -1 } else { 1 })
}

/// JPEG AC run/size decoding with the luma table (`3be6f9`).
///
/// Writes into `plane` in *plane* order: `plane[SCAN[1 + cursor]] = value`.
fn decode_ac(
    plane: &mut [i16; 64],
    count: u8,
    r: &mut BitReader<'_>,
    huff: &Huffman,
) -> Result<()> {
    let count = count as usize;
    let mut cursor = 0usize;
    while cursor < count {
        let symbol = huff.decode(r)?;
        let (run, size) = ((symbol >> 4) as usize, (symbol & 15) as u32);
        if size == 0 {
            if run != 15 {
                break;
            }
            cursor += 16;
        } else {
            cursor += run;
            let pos = 1 + cursor;
            if pos >= 64 {
                break;
            }
            let raw = r.read(size)? as i32;
            let value = if raw < (1 << (size - 1)) {
                raw + 1 - (1 << size)
            } else {
                raw
            };
            plane[SCAN[pos]] = value as i16;
            cursor += 1;
        }
    }
    Ok(())
}

/// Fill colour for `code 0` tiles (BGRX).
const BACKGROUND: [u8; 4] = [255, 255, 255, 0];

/// State machine for MVS updates on one display.
///
/// Feed each `0x3f3` rectangle body to [`MvsDecoder::apply`]; type-2 bodies
/// update the quantisation tables and type-0 bodies paint into the BGRX
/// framebuffer you pass in.
pub struct MvsDecoder {
    width: u16,
    height: u16,
    pub quant: QuantTables,
    idct: Idct,
    /// `ceil(width / 8)`; the global spatial-record index includes it.
    grid_width: usize,
    /// Incremented once per type-0 body (`viewer+0xd90`).
    generation: u32,
    /// Base coefficient records, one 84-byte entry per tile (`viewer+0xd80`).
    records: Vec<SpatialRecord>,
    /// Scratch record for the DCT reuse path.
    prev_record: SpatialRecord,
    /// 99-byte coefficient ring (`viewer+0xda0`); session scoped.
    pub cache: CacheRing,
}

impl MvsDecoder {
    pub fn new(width: u16, height: u16) -> Self {
        let grid_width = (width as usize).div_ceil(8);
        let grid_height = (height as usize).div_ceil(8);
        Self {
            width,
            height,
            quant: QuantTables::default(),
            idct: Idct::new(),
            grid_width,
            generation: 0,
            records: vec![SpatialRecord::default(); grid_width * grid_height],
            prev_record: SpatialRecord::default(),
            cache: CacheRing::new(),
        }
    }

    pub fn dimensions(&self) -> (u16, u16) {
        (self.width, self.height)
    }

    /// Change the framebuffer geometry (e.g. the server ignored a scale
    /// request). Keeps the current quantisation tables.
    pub fn resize(&mut self, width: u16, height: u16) {
        self.width = width;
        self.height = height;
        self.grid_width = (width as usize).div_ceil(8);
        let grid_height = (height as usize).div_ceil(8);
        self.records = vec![SpatialRecord::default(); self.grid_width * grid_height];
    }

    /// Apply one MVS body. Returns `None` for type-2 (quant table) bodies.
    pub fn apply(
        &mut self,
        body: &[u8],
        rect: Rect,
        fb: &mut [u8],
    ) -> Result<Option<FrameStats>> {
        match body.first().copied() {
            Some(0) => Ok(Some(self.decode_partial(body, rect, fb)?)),
            Some(1) => Ok(Some(self.decode_refinement(body, rect, fb)?)),
            Some(2) => {
                self.quant = QuantTables::from_type2(body)?;
                Ok(None)
            }
            Some(t) => bail!("unsupported MVS update type {t}"),
            None => bail!("empty MVS body"),
        }
    }

    fn check_rect(&self, rect: Rect, fb_len: usize) -> Result<()> {
        let stride = self.width as usize * 4;
        if fb_len < stride * self.height as usize {
            bail!(
                "framebuffer is {fb_len} bytes, need {}",
                stride * self.height as usize
            );
        }
        if rect.x as usize + rect.w as usize > self.width as usize
            || rect.y as usize + rect.h as usize > self.height as usize
        {
            bail!(
                "MVS rect {}x{}+{}+{} outside {}x{}",
                rect.w,
                rect.h,
                rect.x,
                rect.y,
                self.width,
                self.height
            );
        }
        if rect.w < TILE as u16 || rect.h < TILE as u16 {
            bail!("MVS rect {}x{} is smaller than one tile", rect.w, rect.h);
        }
        Ok(())
    }

    /// Render three coefficient planes into a BGRX tile.
    fn render_coeff(&self, coeff: &[i16; 192], tile: &mut [u8; TILE * TILE * 4]) {
        let y = self.idct.plane(&coeff[..64], &self.quant.luma);
        let cb = self.idct.plane(&coeff[64..128], &self.quant.chroma);
        let cr = self.idct.plane(&coeff[128..192], &self.quant.chroma);
        for j in 0..64 {
            let [b, g, r] = rgb_from_idct(y[j], cb[j], cr[j]);
            tile[j * 4..j * 4 + 4].copy_from_slice(&[b, g, r, 0]);
        }
    }

    /// Resolve a cache index (type-0 code 6/7, type-1 code 3) into `tile`.
    ///
    /// Returns false when the index is invalid; the caller then leaves the
    /// tile untouched, which is the closest behaviour to the native error path.
    fn render_cache(&mut self, index: u16, tile: &mut [u8; TILE * TILE * 4]) -> bool {
        let Some(entry) = self.cache.read(index) else {
            return false;
        };
        let coeff = expand_cache_entry(&entry);
        self.render_coeff(&coeff, tile);
        true
    }

    /// Decode a type-0 body into `fb` (BGRX, `self.width * 4` bytes per row).
    pub fn decode_partial(&mut self, body: &[u8], rect: Rect, fb: &mut [u8]) -> Result<FrameStats> {
        let stride = self.width as usize * 4;
        self.check_rect(rect, fb.len())?;
        let header = Header::parse(body)?;
        if header.type_of_update != 0 {
            bail!("only type-0 (partial) MVS bodies are decoded here");
        }
        let tiles_x = (rect.w as usize).div_ceil(8);
        let tiles_y = (rect.h as usize).div_ceil(8);
        let total = tiles_x * tiles_y;
        // One generation per body; every expanded tile records it.
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let grid_width = self.grid_width;

        let mut commands = CommandIter::new(&body[6..header.data_off], total as u32)?;
        let mut payload = BitReader::new(&body[header.data_off..]);
        let mut blocks = PortableBlocks::default();
        let mut solid = BACKGROUND;
        let mut bitmap = [BACKGROUND; 2];
        let mut tile = [0u8; TILE * TILE * 4];
        let mut stats = FrameStats::default();

        let mut index = 0usize;
        for run in &mut commands {
            let run = run?;
            stats.commands += 1;
            for _ in 0..run.tiles {
                let tx = index % tiles_x;
                let ty = index / tiles_x;
                let gi = (rect.y as usize / TILE + ty) * grid_width
                    + rect.x as usize / TILE
                    + tx;
                if let Some(record) = self.records.get_mut(gi) {
                    record.generation = generation;
                    record.source_record = -1;
                }
                match run.command {
                    Command::Fill => fill_tile(&mut tile, BACKGROUND),
                    Command::CopyPrevious => {
                        let source = if index > 0 {
                            Some(tile_offset(rect, tiles_x, index - 1))
                        } else if rect.x as usize >= TILE {
                            Some((rect.x as usize - TILE, rect.y as usize))
                        } else {
                            None
                        };
                        let copied = match source {
                            Some((sx, sy)) => copy_tile_at(fb, stride, sx, sy, &mut tile),
                            None => false,
                        };
                        if !copied {
                            fill_tile(&mut tile, BACKGROUND);
                        }
                        if let Some(record) = self.records.get_mut(gi) {
                            record.source_record = gi as i32 - 1;
                            if let Some((sx, sy)) = source {
                                record.source_framebuffer_offset =
                                    (sy * stride + sx * 4) as u32;
                            }
                        }
                    }
                    Command::CopyAbove => {
                        let above_in_rect = index >= tiles_x;
                        let source = if above_in_rect {
                            Some(tile_offset(rect, tiles_x, index - tiles_x))
                        } else if rect.y as usize >= TILE {
                            Some((
                                rect.x as usize + (index % tiles_x) * TILE,
                                rect.y as usize - TILE,
                            ))
                        } else {
                            None
                        };
                        let copied = match source {
                            Some((sx, sy)) => copy_tile_at(fb, stride, sx, sy, &mut tile),
                            None => false,
                        };
                        if !copied {
                            fill_tile(&mut tile, BACKGROUND);
                        }
                        if let Some(record) = self.records.get_mut(gi) {
                            record.source_record = gi as i32 - grid_width as i32;
                            if let Some((sx, sy)) = source {
                                record.source_framebuffer_offset =
                                    (sy * stride + sx * 4) as u32;
                            }
                        }
                    }
                    Command::Colour => {
                        let is_bitmap = payload.read(1)? == 1;
                        let reuse = payload.read(1)? == 1;
                        if !is_bitmap {
                            if !reuse {
                                let y = payload.read(8)? as u8;
                                let cb = (payload.read(6)? as u8) << 2;
                                let cr = (payload.read(6)? as u8) << 2;
                                let [b, g, r] = solid_rgb(y, cb, cr);
                                solid = [b, g, r, 0];
                            }
                            fill_tile(&mut tile, solid);
                        } else {
                            if !reuse {
                                for slot in bitmap.iter_mut() {
                                    let y = payload.read(8)? as u8;
                                    let cb = (payload.read(6)? as u8) << 2;
                                    let cr = (payload.read(6)? as u8) << 2;
                                    let [b, g, r] = solid_rgb(y, cb, cr);
                                    *slot = [b, g, r, 0];
                                }
                            }
                            decode_bitmap(&mut payload, &bitmap, &mut tile)?;
                        }
                    }
                    Command::Bitmap => decode_bitmap(&mut payload, &bitmap, &mut tile)?,
                    Command::Dct => {
                        blocks.decode_block(&mut payload, header.p1, header.p2)?;
                        self.render_coeff(&blocks.coeff, &mut tile);
                        // Spatial record: base coefficients for type-1
                        // refinement. A reused block copies the previous record.
                        let record = if blocks.last_reuse {
                            self.prev_record
                        } else {
                            let mut rec = SpatialRecord {
                                generation,
                                source_record: -1,
                                param: blocks.last_param,
                                cr_count: 1,
                                cr_dc: blocks.coeff[128] as i8,
                                cb_count: 1,
                                cb_dc: blocks.coeff[64] as i8,
                                ..SpatialRecord::default()
                            };
                            for (j, &scan) in SCAN.iter().enumerate() {
                                rec.y[j] = blocks.coeff[scan] as i8;
                            }
                            rec
                        };
                        if let Some(slot) = self.records.get_mut(gi) {
                            *slot = record;
                        }
                        self.prev_record = record;
                        stats.dct_blocks += 1;
                    }
                    Command::Cache16 => {
                        let cache_index = payload.read(16)? as u16;
                        self.render_cache(cache_index, &mut tile);
                    }
                    Command::Cache17 => {
                        let cache_index = self.cache.last_read.wrapping_add(1);
                        self.render_cache(cache_index, &mut tile);
                    }
                }
                write_tile(fb, stride, rect, tx, ty, &tile);
                index += 1;
                stats.tiles += 1;
            }
        }
        if index != total {
            bail!("MVS body painted {index} tiles, expected {total}");
        }
        commands.finish()?;
        payload.finish()?;
        stats.dct_bits = blocks.bits;
        Ok(stats)
    }

    /// Decode a type-1 body: refinement of the type-0 spatial records.
    ///
    /// No command stream and no leading flag; one MSB-first stream from
    /// `body[3]` carrying 2-bit per-tile codes plus their payloads, terminated
    /// by the three bytes `mvs`.
    pub fn decode_refinement(
        &mut self,
        body: &[u8],
        rect: Rect,
        fb: &mut [u8],
    ) -> Result<FrameStats> {
        let stride = self.width as usize * 4;
        self.check_rect(rect, fb.len())?;
        if body.len() < 4 {
            bail!("MVS type-1 body is {} bytes", body.len());
        }
        let cr_count = body[1].min(64);
        let cb_count = body[2].min(64);
        let huff = Huffman::luma_ac();
        let mut bits = BitReader::new(&body[3..]);
        let grid_width = self.grid_width;
        let tiles_x = (rect.w as usize).div_ceil(8);
        let tiles_y = (rect.h as usize).div_ceil(8);
        let mut tile = [0u8; TILE * TILE * 4];
        let mut stats = FrameStats::default();

        for ly in 0..tiles_y {
            for lx in 0..tiles_x {
                let gi = (rect.y as usize / TILE + ly) * grid_width
                    + rect.x as usize / TILE
                    + lx;
                let x = rect.x as usize + lx * TILE;
                let y = rect.y as usize + ly * TILE;
                let code = bits.read(2)?;
                match code {
                    0 => {}
                    3 => {
                        let cache_index = if bits.read_bit()? == 1 {
                            self.cache.last_read.wrapping_add(1)
                        } else {
                            bits.read(16)? as u16
                        };
                        if self.render_cache(cache_index, &mut tile) {
                            write_tile_at(fb, stride, x, y, &tile);
                        }
                    }
                    2 => {
                        let record = self.records.get(gi).copied().unwrap_or_default();
                        if record.source_record >= 0 {
                            let source = self
                                .records
                                .get(record.source_record as usize)
                                .copied()
                                .unwrap_or_default();
                            if source.generation == record.generation {
                                copy_tile_from_offset(
                                    fb,
                                    stride,
                                    record.source_framebuffer_offset as usize,
                                    x,
                                    y,
                                );
                            }
                        }
                    }
                    _ => {
                        let record = self.records.get(gi).copied().unwrap_or_default();
                        let (coeff, ring) =
                            refine(&record, &mut bits, cr_count, cb_count, &huff)?;
                        self.render_coeff(&coeff, &mut tile);
                        write_tile_at(fb, stride, x, y, &tile);
                        self.cache.insert(ring);
                        stats.dct_blocks += 1;
                    }
                }
                stats.tiles += 1;
            }
        }
        let m = bits.read(8)?;
        let v = bits.read(8)?;
        let s = bits.read(8)?;
        if [m as u8, v as u8, s as u8] != *b"mvs" {
            bail!("type-1 terminator missing (read {m:02x} {v:02x} {s:02x})");
        }
        Ok(stats)
    }
}

/// Refine one spatial record with the type-1 payload (`3b95b9..3b9df9`).
fn refine(
    record: &SpatialRecord,
    bits: &mut BitReader<'_>,
    cr_count: u8,
    cb_count: u8,
    huff: &Huffman,
) -> Result<([i16; 192], [u8; CACHE_STRIDE])> {
    let mut out = [0i16; 192];
    let mut ring = [0u8; CACHE_STRIDE];
    out[0] = record.y[0] as i16;
    ring[0] = out[0] as u8;
    let param = record.param as usize;
    let n = ((bits.read(6)? as usize) + 1).min(64);
    for j in 1..n {
        let width = if param >= 15 {
            if j < param {
                1
            } else {
                3
            }
        } else if j < param {
            3
        } else {
            4
        };
        let value = refine_step(record.y[j], bits, width)?;
        out[SCAN[j]] = value as i16;
        ring[j] = value as u8;
    }
    // Chroma: Cr first, then Cb, both with the *luma* AC table.
    out[128] = refine_step(record.cr_dc, bits, 1)? as i16;
    let mut cr_plane = [0i16; 64];
    decode_ac(&mut cr_plane, cr_count, bits, huff)?;
    out[129..192].copy_from_slice(&cr_plane[1..64]);
    out[64] = refine_step(record.cb_dc, bits, 1)? as i16;
    let mut cb_plane = [0i16; 64];
    decode_ac(&mut cb_plane, cb_count, bits, huff)?;
    out[65..128].copy_from_slice(&cb_plane[1..64]);
    // C8: ring order is Y64, Cr15, Cb20.
    for j in 0..15 {
        ring[64 + j] = out[128 + SCAN[j]] as u8;
    }
    for j in 0..20 {
        ring[79 + j] = out[64 + SCAN[j]] as u8;
    }
    Ok((out, ring))
}

/// Write a tile at absolute pixel coordinates.
fn write_tile_at(fb: &mut [u8], stride: usize, x: usize, y: usize, tile: &[u8; TILE * TILE * 4]) {
    for row in 0..TILE {
        let src = row * TILE * 4;
        let dst = (y + row) * stride + x * 4;
        fb[dst..dst + TILE * 4].copy_from_slice(&tile[src..src + TILE * 4]);
    }
}

/// Copy an 8x8 tile from a byte offset inside the framebuffer to `(x, y)`.
fn copy_tile_from_offset(fb: &mut [u8], stride: usize, offset: usize, x: usize, y: usize) {
    if offset + 7 * stride + TILE * 4 > fb.len() {
        return;
    }
    for row in 0..TILE {
        let src = offset + row * stride;
        let dst = (y + row) * stride + x * 4;
        fb.copy_within(src..src + TILE * 4, dst);
    }
}

fn fill_tile(tile: &mut [u8; TILE * TILE * 4], pixel: [u8; 4]) {
    for chunk in tile.chunks_exact_mut(4) {
        chunk.copy_from_slice(&pixel);
    }
}

fn decode_bitmap(
    payload: &mut BitReader<'_>,
    colours: &[[u8; 4]; 2],
    tile: &mut [u8; TILE * TILE * 4],
) -> Result<()> {
    let mask = payload.read(8)? as u8;
    for row in 0..TILE {
        let base = row * TILE * 4;
        if mask & (0x80 >> row) != 0 {
            for col in 0..TILE {
                tile[base + col * 4..base + col * 4 + 4].copy_from_slice(&colours[0]);
            }
        } else {
            let bits = payload.read(8)? as u8;
            for col in 0..TILE {
                let pixel = if bits & (0x80 >> col) != 0 {
                    colours[0]
                } else {
                    colours[1]
                };
                tile[base + col * 4..base + col * 4 + 4].copy_from_slice(&pixel);
            }
        }
    }
    Ok(())
}

fn tile_offset(rect: Rect, tiles_x: usize, index: usize) -> (usize, usize) {
    let tx = index % tiles_x;
    let ty = index / tiles_x;
    (rect.x as usize + tx * TILE, rect.y as usize + ty * TILE)
}

fn write_tile(fb: &mut [u8], stride: usize, rect: Rect, tx: usize, ty: usize, tile: &[u8]) {
    let x = rect.x as usize + tx * TILE;
    let y = rect.y as usize + ty * TILE;
    for row in 0..TILE {
        let src = row * TILE * 4;
        let dst = (y + row) * stride + x * 4;
        fb[dst..dst + TILE * 4].copy_from_slice(&tile[src..src + TILE * 4]);
    }
}

/// Copy the tile whose top-left pixel is `(x, y)`. Returns false when that
/// tile is not fully inside the framebuffer.
fn copy_tile_at(fb: &[u8], stride: usize, x: usize, y: usize, tile: &mut [u8; TILE * TILE * 4]) -> bool {
    let fb_h = fb.len() / stride;
    if x + TILE > stride / 4 || y + TILE > fb_h {
        return false;
    }
    for row in 0..TILE {
        let src = (y + row) * stride + x * 4;
        tile[row * TILE * 4..row * TILE * 4 + TILE * 4]
            .copy_from_slice(&fb[src..src + TILE * 4]);
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured fixtures live under the gitignored `artifacts/` tree. Set
    /// `SS_MVS_ARTIFACTS_OFF=1` to simulate a clean checkout: those tests then
    /// return early and only the synthetic fixtures run.
    fn artifact(name: &str) -> Option<Vec<u8>> {
        if std::env::var_os("SS_MVS_ARTIFACTS_OFF").is_some() {
            return None;
        }
        let path = format!("{}/../../artifacts/{name}", env!("CARGO_MANIFEST_DIR"));
        std::fs::read(path).ok()
    }

    /// Stable copy of a live 2560x1600 capture. Kept out of `mvs-4.bin`,
    /// which `ss-client stream` overwrites on every run.
    fn capture() -> Option<Vec<u8>> {
        artifact("mvs-fixture.bin")
    }

    fn quant() -> Option<QuantTables> {
        let body = artifact("mvs-fixture-quant.bin")?;
        Some(QuantTables::from_type2(&body).unwrap())
    }

    #[test]
    fn header_parses_reference_capture() {
        let Some(body) = capture() else { return };
        let h = Header::parse(&body).unwrap();
        assert_eq!(h.type_of_update, 0);
        assert_eq!((h.p1, h.p2), (15, 25));
        assert!(h.data_off > 6 && h.data_off <= body.len());
    }

    #[test]
    fn command_stream_totals_are_exact() {
        let Some(body) = capture() else { return };
        let h = Header::parse(&body).unwrap();
        let mut commands = CommandIter::new(&body[6..h.data_off], 64_000).unwrap();
        let mut tiles = 0u64;
        let mut count = 0u64;
        for run in &mut commands {
            let run = run.unwrap();
            tiles += run.tiles as u64;
            count += 1;
        }
        assert_eq!(tiles, 64_000);
        assert_eq!(count, 1_754);
        commands.finish().unwrap();
    }

    #[test]
    fn recorded_quant_tables_parse() {
        let Some(q) = quant() else { return };
        assert_eq!(&q.luma[..6], &[12, 9, 9, 11, 18, 17]);
        assert_eq!(&q.chroma[..6], &[15, 15, 18, 36, 57, 75]);
        assert_ne!(q.luma, DEFAULT_QUANT_LUMA);
    }

    #[test]
    fn portable_blocks_match_recorded_sizes() {
        let Some(body) = capture() else { return };
        let Some(sizes) = artifact("mvs-block-sizes.txt") else {
            return;
        };
        let h = Header::parse(&body).unwrap();
        let mut payload = BitReader::new(&body[h.data_off..]);
        let mut commands = CommandIter::new(&body[6..h.data_off], 64_000).unwrap();
        let mut blocks = PortableBlocks::default();
        let mut tiles = 0usize;
        for run in &mut commands {
            let run = run.unwrap();
            for _ in 0..run.tiles {
                decode_payload(run.command, &mut payload, h, &mut blocks).unwrap();
                tiles += 1;
            }
        }
        assert_eq!(tiles, 64_000, "tile count");
        let expected: Vec<usize> = String::from_utf8_lossy(&sizes)
            .lines()
            .map(|l| l.split('\t').nth(2).unwrap().parse().unwrap())
            .collect();
        assert_eq!(blocks.blocks, expected.len(), "block count");
        assert_eq!(blocks.bits, expected.iter().sum::<usize>(), "block bits");
        payload.finish().unwrap();
    }

    /// Value-path acceptance fixture from the MVS findings (§5.1).
    #[test]
    fn portable_block_values_match_fixture() {
        let Some(body) = capture() else { return };
        let h = Header::parse(&body).unwrap();
        let mut commands = CommandIter::new(&body[6..h.data_off], 64_000).unwrap();
        let mut payload = BitReader::new(&body[h.data_off..]);
        let mut blocks = PortableBlocks::default();
        // FNV-1a over every coefficient word (16-bit values, low byte first).
        let mut hash: u32 = 0x811c9dc5;
        for run in &mut commands {
            let run = run.unwrap();
            for _ in 0..run.tiles {
                let before = blocks.bits;
                decode_payload(run.command, &mut payload, h, &mut blocks).unwrap();
                if run.command != Command::Dct {
                    continue;
                }
                debug_assert!(blocks.bits > before);
                for word in blocks.coeff {
                    hash = (hash ^ (word as u16) as u32).wrapping_mul(16_777_619);
                }
            }
        }
        assert_eq!(blocks.blocks, 3_388, "dct block count");
        assert_eq!(blocks.bits, 148_670, "dct bits");
        assert_eq!(hash, 0x1135_7da4, "coefficient FNV-1a");
    }

    #[test]
    fn rgb_helper_matches_native_probes() {
        // Recorded in tools/probe/mvs_block_values.py NOTES §6.
        assert_eq!(rgb_from_idct(0, 0, 0), [128, 128, 128]);
        assert_eq!(rgb_from_idct(0, 640, 0), [146, 125, 128]);
        assert_eq!(rgb_from_idct(0, 0, 640), [128, 121, 142]);
    }

    #[test]
    fn solid_helper_matches_native_tables() {
        assert_eq!(solid_rgb(218, 128, 128), [218, 218, 218]);
        assert_eq!(solid_rgb(32, 128, 128), [32, 32, 32]);
        // T0[x] = round(1402 * (x-128) / 1000), T1[x] = round(1772 * (x-128) / 1000)
        assert_eq!(solid_rgb(128, 128, 0), [128, 219, 0]); // red clips to 0
        assert_eq!(solid_rgb(200, 128, 252), [200, 111, 255]);
    }

    #[test]
    fn idct_dc_only_is_scale64() {
        let idct = Idct::new();
        let mut coeff = [0i16; 64];
        coeff[0] = 1;
        let mut quant = [16u8; 64];
        quant[0] = 16;
        let plane = idct.plane(&coeff, &quant);
        // Ideal DC gain: 128 (native fixed point gives 127).
        assert!(plane.iter().all(|&v| (v - 128).abs() <= 1), "{plane:?}");
    }

    #[test]
    fn reference_capture_renders_desktop() {
        let Some(body) = capture() else { return };
        let Some(quant) = quant() else { return };
        let mut dec = MvsDecoder::new(2560, 1600);
        dec.quant = quant;
        let mut fb = vec![0u8; 2560 * 1600 * 4];
        let stats = dec
            .apply(&body, Rect::new(0, 0, 2560, 1600), &mut fb)
            .unwrap()
            .expect("type-0 body");
        assert_eq!(stats.tiles, 64_000);
        assert_eq!(stats.commands, 1_754);
        assert_eq!(stats.dct_blocks, 3_388);
        assert_eq!(stats.dct_bits, 148_670);

        let px = |x: usize, y: usize| {
            let o = (y * 2560 + x) * 4;
            [fb[o], fb[o + 1], fb[o + 2]]
        };
        // Menu bar grey: the first command is a solid colour with Y=219.
        assert_eq!(px(0, 0), [219, 219, 219]);
        // Document body is white everywhere below the toolbar.
        assert_eq!(px(1280, 800), [255, 255, 255]);
        assert_eq!(px(10, 1590), [255, 255, 255]);
        let white = fb
            .chunks_exact(4)
            .filter(|p| p[0] == 255 && p[1] == 255 && p[2] == 255)
            .count();
        assert!(white > 3_500_000, "white pixels: {white}");
    }

    /// A small incremental rectangle (56x24 at +1648+0): the decoder must
    /// honour the rect origin and its own tile budget.
    /// Replay a captured push sequence in manifest order.
    ///
    /// Returns `(bodies, insertions, last_read, write_cursor, fnv)`.
    fn replay_manifest(manifest: &str) -> Option<(usize, u32, u16, u16, u32)> {
        let text = artifact(manifest)?;
        let mut dec = MvsDecoder::new(2560, 1600);
        let mut fb = vec![0u8; 2560 * 1600 * 4];
        let mut bodies = 0usize;
        for line in String::from_utf8_lossy(&text).lines() {
            let f: Vec<&str> = line.split_whitespace().collect();
            if f.len() < 8 {
                continue;
            }
            let path = format!("{}/../../{}", env!("CARGO_MANIFEST_DIR"), f[7]);
            let body = std::fs::read(&path).ok()?;
            let rect = Rect::new(
                f[2].parse().unwrap(),
                f[3].parse().unwrap(),
                f[4].parse().unwrap(),
                f[5].parse().unwrap(),
            );
            dec.apply(&body, rect, &mut fb)
                .unwrap_or_else(|e| panic!("body {} ({}): {e:#}", f[0], f[1]));
            bodies += 1;
        }
        let mut fnv: u32 = 0x811c9dc5;
        for b in &fb {
            fnv = (fnv ^ *b as u32).wrapping_mul(16_777_619);
        }
        Some((
            bodies,
            dec.cache.total_insertions,
            dec.cache.last_read,
            dec.cache.write_cursor(),
            fnv,
        ))
    }

    /// Acceptance fixtures for the cache + type-1 path. Two independent
    /// captures; the insertion counts are cross-checked against the
    /// independent Python parser in `artifacts/push-codex-parse.py`.
    #[test]
    fn push_sequence_applies_with_cache_and_type1() {
        let Some((bodies, inserts, last_read, cursor, fnv)) =
            replay_manifest("push-pC/manifest.txt")
        else {
            return;
        };
        assert_eq!(bodies, 89, "manifest body count");
        assert_eq!(inserts, 20_082, "ring insertions");
        assert_eq!(last_read, 20_073);
        assert_eq!(cursor, 20_082);
        assert_eq!(fnv, 0xe065_e9ab, "final framebuffer FNV");
    }

    /// codex's own 12-second capture (131 type-0, 90 type-1, 1 quant).
    #[test]
    fn codex_capture_sequence_applies() {
        let Some((bodies, inserts, last_read, cursor, fnv)) =
            replay_manifest("push-codex-manifest.txt")
        else {
            return;
        };
        assert_eq!(bodies, 222, "manifest body count");
        assert_eq!(inserts, 34_608, "ring insertions");
        assert_eq!(last_read, 526);
        assert_eq!(cursor, 34_608);
        assert_eq!(fnv, 0xfc24_7744, "final framebuffer FNV");
    }

    #[test]
    fn partial_rect_decodes() {
        let Some(body) = artifact("mvs-partial.bin") else {
            return;
        };
        let mut dec = MvsDecoder::new(2560, 1600);
        let mut fb = vec![0u8; 2560 * 1600 * 4];
        let stats = dec
            .apply(&body, Rect::new(1648, 0, 56, 24), &mut fb)
            .unwrap()
            .expect("type-0 body");
        assert_eq!(stats.tiles, 7 * 3);
        assert_eq!(stats.commands, 9);
        // Only the rect is written; the rest of the canvas stays zero.
        assert!(fb[..1648 * 4].iter().all(|&b| b == 0));
    }

    #[test]
    fn payload_widths_consume_stream_b_exactly() {
        struct Sink;
        impl TileSink for Sink {
            fn tile(&mut self, _index: usize, _kind: Command) {}
        }
        let Some(body) = capture() else { return };
        let h = Header::parse(&body).unwrap();
        let mut payload = BitReader::new(&body[h.data_off..]);
        let mut commands = CommandIter::new(&body[6..h.data_off], 64_000).unwrap();
        let mut tiles = 0usize;
        for run in &mut commands {
            let run = run.unwrap();
            for _ in 0..run.tiles {
                if run.command == Command::Dct {
                    // First DCT block of the reference capture (tile 6); the
                    // payload grammar is covered by the block tests.
                    assert_eq!(tiles, 6);
                    return;
                }
                decode_payload(run.command, &mut payload, h, &mut NoDct).unwrap();
                tiles += 1;
            }
        }
    }
    // ---------------------------------------------------------------------
    // Synthetic fixtures. Deterministic, content-free, no `artifacts/`.
    // ---------------------------------------------------------------------

    /// MSB-first bit writer with the bit-aligned `0x6d` marker helper.
    struct BitWriter {
        bytes: Vec<u8>,
        pos: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                pos: 0,
            }
        }

        fn bit(&mut self, one: bool) {
            if self.pos % 8 == 0 {
                self.bytes.push(0);
            }
            if one {
                self.bytes[self.pos / 8] |= 0x80 >> (self.pos % 8);
            }
            self.pos += 1;
        }

        fn bits(&mut self, value: u32, n: u32) {
            for i in (0..n).rev() {
                self.bit((value >> i) & 1 == 1);
            }
        }

        /// Bit-aligned end-of-stream marker plus zero padding.
        fn marker(&mut self) {
            self.bits(STREAM_MARKER as u32, 8);
            while self.pos % 8 != 0 {
                self.bit(false);
            }
        }

        fn take(&mut self) -> Vec<u8> {
            std::mem::take(&mut self.bytes)
        }
    }

    /// Builder for a type-0 body: stream A (commands) plus stream B (payloads).
    struct Type0Body {
        commands: BitWriter,
        payload: BitWriter,
    }

    impl Type0Body {
        fn new() -> Self {
            let mut commands = BitWriter::new();
            commands.bit(false); // leading flag bit
            Self {
                commands,
                payload: BitWriter::new(),
            }
        }

        /// One single-tile command (run flag 0).
        fn command(&mut self, code: u32) {
            self.commands.bits(code, 3);
            self.commands.bit(false);
        }

        /// Command 4, solid colour (not a bitmap, no reuse).
        fn solid(&mut self, y: u8, cb6: u8, cr6: u8) {
            self.command(4);
            self.payload.bit(false);
            self.payload.bit(false);
            self.payload.bits(y as u32, 8);
            self.payload.bits(cb6 as u32, 6);
            self.payload.bits(cr6 as u32, 6);
        }

        /// Command 5: reuse 0, chroma copied from the reference, luma DC delta 0,
        /// optional AC coefficient 1 with magnitude `m` (2..=5, sign positive;
        /// at param 15 the stored coefficient is `2 * m`), then an
        /// end-of-block token.
        fn dct(&mut self, ac1_magnitude: Option<u32>) {
            self.command(5);
            self.payload.bit(false); // not a reuse
            self.payload.bit(true); // copy chroma DC
            self.payload.bit(false); // param = p1
            self.payload.bit(false); // predictor k = 0
            self.payload.bit(false); // suffix 0 -> delta 0
            if let Some(m) = ac1_magnitude {
                assert!((2..=5).contains(&m), "magnitude 2..=5");
                self.payload.bit(true); // magnitude token
                self.payload.bit(false); // k = 0
                self.payload.bits(2 * (m - 2), 3); // suffix, positive sign
            }
            self.payload.bit(false); // not a magnitude token
            self.payload.bits(0b01, 2); // run/EOB code
            self.payload.bit(false); // end of block
        }

        /// Command 6 with a 16-bit cache index.
        fn cache16(&mut self, index: u16) {
            self.command(6);
            self.payload.bits(index as u32, 16);
        }

        /// Any command with no payload (0, 1, 2, 7).
        fn bare(&mut self, code: u32) {
            self.command(code);
        }

        fn body(mut self, p1: u8, p2: u8) -> Vec<u8> {
            self.commands.marker();
            self.payload.marker();
            let commands = self.commands.take();
            let payload = self.payload.take();
            let data_off = 6 + commands.len();
            let mut body = vec![
                0u8,
                p1,
                p2,
                (data_off >> 16) as u8,
                ((data_off >> 8) & 0xff) as u8,
                (data_off & 0xff) as u8,
            ];
            body.extend_from_slice(&commands);
            body.extend_from_slice(&payload);
            body
        }
    }

    /// Builder for a type-1 (refinement) body: one interleaved bit stream.
    struct Type1Body {
        bits: BitWriter,
    }

    impl Type1Body {
        fn new() -> Self {
            Self {
                bits: BitWriter::new(),
            }
        }

        fn code(&mut self, code: u32) {
            self.bits.bits(code, 2);
        }

        /// Code 1. The fixtures only refine luma index 1 on a record whose
        /// `y[1] == 4` at `param >= 15` (width 1) and whose chroma DCs are 0,
        /// with zero AC counts in the header.
        fn refine(&mut self, n: u32, ac1_bit: bool) {
            self.code(1);
            self.bits.bits(n - 1, 6);
            for j in 1..n {
                if j == 1 {
                    self.bits.bit(ac1_bit);
                } else {
                    self.bits.bit(false);
                }
            }
            self.bits.bit(false); // Cr DC step: old 0 -> 0
            self.bits.bit(false); // Cb DC step: old 0 -> 0
        }

        /// Code 3 with an absolute 16-bit ring index.
        fn cache_index(&mut self, index: u16) {
            self.code(3);
            self.bits.bit(false);
            self.bits.bits(index as u32, 16);
        }

        /// Code 3 reading `last_read + 1`.
        fn cache_next(&mut self) {
            self.code(3);
            self.bits.bit(true);
        }

        /// Code 2: copy from the spatial record's source tile.
        fn copy(&mut self) {
            self.code(2);
        }

        fn skip(&mut self) {
            self.code(0);
        }

        fn body(mut self, cr_ac: u8, cb_ac: u8) -> Vec<u8> {
            self.bits.bits(b'm' as u32, 8);
            self.bits.bits(b'v' as u32, 8);
            self.bits.bits(b's' as u32, 8);
            while self.bits.pos % 8 != 0 {
                self.bits.bit(false);
            }
            let stream = self.bits.take();
            let mut body = vec![1u8, cr_ac, cb_ac];
            body.extend_from_slice(&stream);
            body
        }
    }

    fn solid_tile(rgb: [u8; 3]) -> [u8; 256] {
        let mut tile = [0u8; 256];
        for px in tile.chunks_exact_mut(4) {
            px[0] = rgb[2];
            px[1] = rgb[1];
            px[2] = rgb[0];
            px[3] = 0;
        }
        tile
    }

    fn tile_of(fb: &[u8], width: usize, tx: usize, ty: usize) -> [u8; 256] {
        let stride = width * 4;
        let mut tile = [0u8; 256];
        for row in 0..8 {
            let src = (ty * 8 + row) * stride + tx * 8 * 4;
            tile[row * 32..row * 32 + 32].copy_from_slice(&fb[src..src + 32]);
        }
        tile
    }

    /// Command set of a type-0 body: solid, copy-previous, copy-above, DCT,
    /// cache16, cache17 and fill all parse and paint.
    #[test]
    fn synthetic_type0_command_set() {
        let mut dec = MvsDecoder::new(64, 32);
        let mut fb = vec![0u8; 64 * 32 * 4];
        let mut b = Type0Body::new();
        b.solid(255, 32, 32); // tile 0: white
        b.bare(1); // tile 1: copy previous -> tile 0
        b.dct(Some(2)); // tile 2: DCT with AC 1 (+4)
        b.cache16(9999); // tile 3: cache read with an invalid index
        b.bare(2); // tile 4: copy above -> tile 0
        b.bare(7); // tile 5: cache17 (invalid)
        b.bare(0); // tile 6: fill -> white
        b.solid(128, 32, 32); // tile 7: grey 128
        let stats = dec
            .apply(&b.body(15, 25), Rect::new(0, 0, 32, 16), &mut fb)
            .unwrap()
            .expect("type-0 body");
        assert_eq!(stats.tiles, 8);
        assert_eq!(stats.commands, 8);

        let white = solid_tile([255, 255, 255]);
        assert_eq!(tile_of(&fb, 64, 0, 0), white, "solid white");
        assert_eq!(tile_of(&fb, 64, 1, 0), white, "copy previous");
        assert_eq!(tile_of(&fb, 64, 0, 1), white, "copy above");
        assert_eq!(tile_of(&fb, 64, 2, 1), white, "fill");
        assert_eq!(tile_of(&fb, 64, 3, 1), solid_tile([128, 128, 128]), "solid grey");

        // The DCT tile carries AC coefficient 1, so it is not a flat colour.
        let dct = tile_of(&fb, 64, 2, 0);
        assert!(
            dct.chunks_exact(4).any(|px| px != &dct[0..4]),
            "DCT tile is not uniform"
        );
    }

    /// Refinement, same-generation copy and both ring read paths.
    #[test]
    fn synthetic_refine_copy_and_ring_reads() {
        let mut dec = MvsDecoder::new(64, 32);
        let mut fb = vec![0u8; 64 * 32 * 4];

        // Body A (type-0, generation 1): two DCT tiles, each copied to its right.
        let mut a = Type0Body::new();
        a.dct(Some(2)); // t0: record y[1] = 4
        a.bare(1); // t1: copy previous, source 0
        a.dct(Some(4)); // t2: record y[1] = 8 (distinct from t0)
        a.bare(1); // t3: copy previous, source 2
        for _ in 0..4 {
            a.bare(0); // t4..t7 fill
        }
        dec.apply(&a.body(15, 25), Rect::new(0, 0, 32, 16), &mut fb)
            .unwrap();
        let t0_base = tile_of(&fb, 64, 0, 0);
        assert_eq!(tile_of(&fb, 64, 1, 0), t0_base, "type-0 copy");

        // Body B (type-1): refine t0 (ring entry 1), then copy it to t1.
        let mut b = Type1Body::new();
        b.refine(2, true); // y[1]: 4 -> 5, insert entry 1
        b.copy(); // t1: source 0, same generation
        dec.apply(&b.body(0, 0), Rect::new(0, 0, 16, 8), &mut fb)
            .unwrap();
        let t0_refined = tile_of(&fb, 64, 0, 0);
        assert_ne!(t0_refined, t0_base, "refined tile differs from its base");
        assert_eq!(
            tile_of(&fb, 64, 1, 0),
            t0_refined,
            "same-generation copy equals the refined source"
        );
        assert_eq!(dec.cache.total_insertions, 1);

        // Body C (type-1): refine t2 (entry 2), then read entry 1 via code 3.
        let mut c = Type1Body::new();
        c.refine(2, false); // y[1] stays 8; inserts ring entry 2
        c.cache_index(1);
        dec.apply(&c.body(0, 0), Rect::new(16, 0, 16, 8), &mut fb)
            .unwrap();
        let t2_refined = tile_of(&fb, 64, 2, 0);
        assert_ne!(t2_refined, t0_refined);
        assert_eq!(
            tile_of(&fb, 64, 3, 0),
            t0_refined,
            "cache read equals the insert-time render"
        );
        assert_eq!(dec.cache.total_insertions, 2);

        // Body D (type-1): last_read is 1, so code 3 + flag 1 reads entry 2.
        let mut d = Type1Body::new();
        d.skip();
        d.cache_next();
        dec.apply(&d.body(0, 0), Rect::new(16, 0, 16, 8), &mut fb)
            .unwrap();
        assert_eq!(
            tile_of(&fb, 64, 3, 0),
            t2_refined,
            "last_read+1 resolves the second entry"
        );

        // Body E (type-0): the same two entries read through commands 6 and 7.
        let mut e = Type0Body::new();
        e.cache16(1);
        e.bare(7);
        dec.apply(&e.body(15, 25), Rect::new(0, 0, 16, 8), &mut fb)
            .unwrap();
        assert_eq!(
            tile_of(&fb, 64, 0, 0),
            t0_refined,
            "type-0 code 6 renders ring entry 1"
        );
        assert_eq!(
            tile_of(&fb, 64, 1, 0),
            t2_refined,
            "type-0 code 7 renders last_read+1"
        );
    }

    /// A type-1 copy only fires when the destination and source records share
    /// a generation; otherwise the destination keeps its pixels.
    #[test]
    fn synthetic_type1_copy_requires_matching_generation() {
        let mut dec = MvsDecoder::new(64, 32);
        let mut fb = vec![0u8; 64 * 32 * 4];

        // Generation 1: tile 0 gets a DCT record.
        let mut a = Type0Body::new();
        a.dct(Some(2));
        dec.apply(&a.body(15, 25), Rect::new(0, 0, 8, 8), &mut fb)
            .unwrap();
        let t0_base = tile_of(&fb, 64, 0, 0);

        // Generation 2: tile 1 copies the tile to its left (generation 1), so
        // its record points at an older generation.
        let mut b = Type0Body::new();
        b.bare(1); // rect starts at x = 8: "previous" is the left neighbour
        dec.apply(&b.body(15, 25), Rect::new(8, 0, 8, 8), &mut fb)
            .unwrap();
        assert_eq!(
            tile_of(&fb, 64, 1, 0),
            t0_base,
            "type-0 copy previous reads the left neighbour"
        );

        // Type-1 refines tile 0; type-1 never bumps the record generation.
        let mut c = Type1Body::new();
        c.refine(2, true);
        dec.apply(&c.body(0, 0), Rect::new(0, 0, 8, 8), &mut fb)
            .unwrap();
        let t0_refined = tile_of(&fb, 64, 0, 0);
        assert_ne!(t0_refined, t0_base);

        // Type-1 copy at tile 1: generations differ (2 vs 1), so it is skipped.
        let mut d = Type1Body::new();
        d.copy();
        dec.apply(&d.body(0, 0), Rect::new(8, 0, 8, 8), &mut fb)
            .unwrap();
        assert_eq!(
            tile_of(&fb, 64, 1, 0),
            t0_base,
            "mismatched generation leaves the destination untouched"
        );
        assert_ne!(tile_of(&fb, 64, 1, 0), t0_refined);
    }

    /// The ring write cursor wraps 65000 -> 1 and entry 0 is never valid.
    #[test]
    fn synthetic_cache_ring_wraps_at_capacity() {
        let mut dec = MvsDecoder::new(8, 8);
        for i in 0..CACHE_ENTRIES {
            let mut entry = [0u8; CACHE_STRIDE];
            entry[0] = (i & 0xff) as u8;
            dec.cache.insert(entry);
        }
        assert_eq!(dec.cache.total_insertions, CACHE_ENTRIES as u32);
        assert_eq!(dec.cache.write_cursor(), 1, "cursor wraps 65000 -> 1");
        assert!(dec.cache.read(0).is_none(), "index 0 is never valid");
        assert!(
            dec.cache.read(CACHE_ENTRIES as u16).is_none(),
            "index 65000 is outside the allocation"
        );
        let last = dec.cache.read(1).expect("slot 1 holds the last insert");
        assert_eq!(last[0], ((CACHE_ENTRIES - 1) & 0xff) as u8);
    }

}
