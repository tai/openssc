//! Replay a captured full-resolution MVS scroll burst through the decoder.
//!
//! The fixture is an untracked analysis artifact produced by
//! `SS_DUMP_MVS=<path> ss-gui ...` (see `dump_mvs` in `crates/ss-gui`); it is
//! the length-prefixed stream of every `0x3f3` body the client received, in
//! arrival order:
//!
//! ```text
//! b"SSMVS1\0\0", u16 width, u16 height,
//! then repeated: u32 len, u32 ms, u16 x, u16 y, u16 w, u16 h, u32 encoding,
//!                [len] body
//! ```
//!
//! The test silently skips when the fixture is absent (fixtures live under the
//! gitignored `artifacts/` tree) and never asserts on wall time: it prints ms,
//! Mpx/s, tiles and DCT blocks so a change can be compared against the recorded
//! baseline in `docs/findings/`.

use rfb_proto::mvs::{MvsDecoder, Rect};
use std::time::Instant;

struct Record {
    rect: Rect,
    /// Arrival time, ms since the client started (0 when replaying old files).
    ms: u32,
    body: Vec<u8>,
}

fn fixture_path() -> std::path::PathBuf {
    if let Some(path) = std::env::var_os("SS_MVS_BURST") {
        return path.into();
    }
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../artifacts/mvs-scroll-burst.bin")
}

fn load(path: &std::path::Path) -> Option<(u16, u16, Vec<Record>)> {
    let data = std::fs::read(path).ok()?;
    assert!(data.len() >= 12, "fixture header truncated");
    assert_eq!(&data[..8], b"SSMVS1\0\0", "not an SS_DUMP_MVS fixture");
    let width = u16::from_le_bytes([data[8], data[9]]);
    let height = u16::from_le_bytes([data[10], data[11]]);
    let mut at = 12;
    let mut records = Vec::new();
    while at < data.len() {
        assert!(at + 20 <= data.len(), "record header truncated at {at}");
        let len = u32::from_le_bytes(data[at..at + 4].try_into().unwrap()) as usize;
        let ms = u32::from_le_bytes(data[at + 4..at + 8].try_into().unwrap());
        let u16_at = |o: usize| u16::from_le_bytes([data[o], data[o + 1]]);
        let (x, y, w, h) = (
            u16_at(at + 8),
            u16_at(at + 10),
            u16_at(at + 12),
            u16_at(at + 14),
        );
        let encoding = u32::from_le_bytes(data[at + 16..at + 20].try_into().unwrap());
        at += 20;
        assert!(at + len <= data.len(), "record body truncated at {at}");
        assert_eq!(encoding, 0x3f3, "only MVS bodies belong in this fixture");
        records.push(Record {
            rect: Rect::new(x, y, w, h),
            ms,
            body: data[at..at + len].to_vec(),
        });
        at += len;
    }
    Some((width, height, records))
}

#[test]
fn replay_scroll_burst() {
    let path = fixture_path();
    let Some((width, height, records)) = load(&path) else {
        eprintln!("mvs replay: no fixture at {} (skipped)", path.display());
        return;
    };
    let mut fb = vec![0u8; width as usize * height as usize * 4];
    let mut tiles = 0u64;
    let mut blocks = 0u64;
    let mut px = 0u64;

    // Two passes: the first warms the code and tables, the second is reported
    // so a cold instruction cache does not masquerade as a regression.
    let mut best_ms = f64::MAX;
    let span = match (records.first(), records.last()) {
        (Some(a), Some(b)) => b.ms.saturating_sub(a.ms),
        _ => 0,
    };
    for pass in 0..2 {
        let mut decoder = MvsDecoder::new(width, height);
        for r in &mut fb {
            *r = 0;
        }
        tiles = 0;
        blocks = 0;
        px = 0;
        let mut bytes = 0u64;
        let started = Instant::now();
        for record in &records {
            let stats = decoder
                .apply(&record.body, record.rect, &mut fb)
                .expect("captured body must decode");
            if let Some(stats) = stats {
                tiles += stats.tiles as u64;
                blocks += stats.dct_blocks as u64;
                px += record.rect.w as u64 * record.rect.h as u64;
            }
            bytes += record.body.len() as u64;
        }
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        best_ms = best_ms.min(ms);
        eprintln!(
            "mvs replay pass {pass}: {ms:.1} ms, {:.1} Mpx/s, {} bodies over {span} ms, \
             {tiles} tiles, {blocks} dct blocks, {:.1} KiB bodies",
            (px as f64 / 1e6) / (ms / 1000.0),
            records.len(),
            bytes as f64 / 1024.0,
        );
    }
    eprintln!(
        "mvs replay: best {best_ms:.1} ms for {}x{} ({:.1} Mpx/s)",
        width,
        height,
        (px as f64 / 1e6) / (best_ms / 1000.0)
    );
    assert!(tiles > 0, "fixture carried no tiles");
}
