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
}

impl Default for PortableBlocks {
    fn default() -> Self {
        Self {
            blocks: 0,
            bits: 0,
            coeff: [0; 192],
            reference: [0; 192],
        }
    }
}

impl PortableBlocks {
    /// Drop the prediction chain (called once per update body).
    pub fn reset_reference(&mut self) {
        self.reference = [0; 192];
    }

    fn unary(r: &mut BitReader<'_>, limit: u32) -> Result<u32> {
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
    fn predictor(r: &mut BitReader<'_>) -> Result<i32> {
        let k = Self::unary(r, 39)?;
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

    /// Non-unit coefficient magnitude token; returns the signed value.
    fn magnitude(r: &mut BitReader<'_>, index: usize) -> Result<i32> {
        let k = Self::unary(r, if index < 6 { 33 } else { 19 })?;
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
        if r.read_bit()? == 1 {
            // Reuse: copy the luma plane and the two chroma DC words.
            out[..PLANE].copy_from_slice(&reference[..PLANE]);
            out[64] = reference[64];
            out[128] = reference[128];
        } else {
            let copy_chroma = r.read_bit()? == 1;
            let param = if r.read_bit()? == 1 { p2 } else { p1 };
            if copy_chroma {
                out[64] = reference[64];
                out[128] = reference[128];
            } else {
                let cb = Self::predictor(r)?;
                let cr = Self::predictor(r)?;
                out[64] = i16w(2 * (half_toward_zero(reference[64]) - cb));
                out[128] = i16w(2 * (half_toward_zero(reference[128]) - cr));
            }
            let y = Self::predictor(r)?;
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
    let green = sat16(
        sat16(y + ((cb * -22554) >> 16)) + sat16(((cr * 18734) >> 16) - cr),
    );
    let blue = sat16(sat16(y + cb) + sat16(cb + ((cb * -14942) >> 16)));
    let red = sat16(sat16(y + cr) + ((cr * 26345) >> 16));
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
}

impl MvsDecoder {
    pub fn new(width: u16, height: u16) -> Self {
        Self {
            width,
            height,
            quant: QuantTables::default(),
            idct: Idct::new(),
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
            Some(2) => {
                self.quant = QuantTables::from_type2(body)?;
                Ok(None)
            }
            Some(t) => bail!("unsupported MVS update type {t}"),
            None => bail!("empty MVS body"),
        }
    }

    /// Decode a type-0 body into `fb` (BGRX, `self.width * 4` bytes per row).
    pub fn decode_partial(&mut self, body: &[u8], rect: Rect, fb: &mut [u8]) -> Result<FrameStats> {
        let stride = self.width as usize * 4;
        let fb_h = self.height as usize;
        if fb.len() < stride * fb_h {
            bail!(
                "framebuffer is {} bytes, need {}",
                fb.len(),
                stride * fb_h
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
        let header = Header::parse(body)?;
        if header.type_of_update != 0 {
            bail!("only type-0 (partial) MVS bodies are decoded");
        }
        let tiles_x = rect.w as usize / TILE;
        let tiles_y = rect.h as usize / TILE;
        if tiles_x == 0 || tiles_y == 0 {
            bail!("MVS rect {}x{} is smaller than one tile", rect.w, rect.h);
        }
        let total = tiles_x * tiles_y;

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
                debug_assert!(index < total);
                match run.command {
                    Command::Fill => fill_tile(&mut tile, BACKGROUND),
                    Command::CopyPrevious => {
                        // Source is the previous tile in raster order. The
                        // framework keeps a precomputed framebuffer offset, so
                        // at a rect's left edge it reads the tile just left of
                        // the rect instead of wrapping.
                        let copied = if index > 0 {
                            let (sx, sy) = tile_offset(rect, tiles_x, index - 1);
                            copy_tile_at(fb, stride, sx, sy, &mut tile)
                        } else if rect.x as usize >= TILE {
                            copy_tile_at(
                                fb,
                                stride,
                                rect.x as usize - TILE,
                                rect.y as usize,
                                &mut tile,
                            )
                        } else {
                            false
                        };
                        if !copied {
                            fill_tile(&mut tile, BACKGROUND);
                        }
                    }
                    Command::CopyAbove => {
                        let copied = if index >= tiles_x {
                            let (sx, sy) = tile_offset(rect, tiles_x, index - tiles_x);
                            copy_tile_at(fb, stride, sx, sy, &mut tile)
                        } else if rect.y as usize >= TILE {
                            let sx = rect.x as usize + (index % tiles_x) * TILE;
                            copy_tile_at(fb, stride, sx, rect.y as usize - TILE, &mut tile)
                        } else {
                            false
                        };
                        if !copied {
                            fill_tile(&mut tile, BACKGROUND);
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
                        let y = self.idct.plane(&blocks.coeff[..64], &self.quant.luma);
                        let cb = self.idct.plane(&blocks.coeff[64..128], &self.quant.chroma);
                        let cr = self.idct.plane(&blocks.coeff[128..192], &self.quant.chroma);
                        for j in 0..64 {
                            let [b, g, r] = rgb_from_idct(y[j], cb[j], cr[j]);
                            tile[j * 4..j * 4 + 4].copy_from_slice(&[b, g, r, 0]);
                        }
                        stats.dct_blocks += 1;
                    }
                    Command::Cache16 => {
                        // Tile cache entries are not exercised by the reference
                        // capture; consume the index and leave the tile alone.
                        let _ = payload.read(16)?;
                    }
                    Command::Cache17 => {}
                }
                let tx = index % tiles_x;
                let ty = index / tiles_x;
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

    fn artifact(name: &str) -> Option<Vec<u8>> {
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
        let q = quant().expect("artifacts/mvs-fixture-quant.bin");
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
}
