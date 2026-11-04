use anyhow::{Context, Result};
use rfb_proto::mvs::{MvsDecoder, QuantTables, Rect};
use rfb_proto::{
    auto_framebuffer_update_custom, auto_pasteboard, client_cut_text, clipboard_fetch,
    framebuffer_update_request, gesture_begin_end, gesture_magnify, gesture_mouse_scroll,
    gesture_quicklook, gesture_rotate, gesture_scroll, gesture_smart_magnify, gesture_swipe,
    key_event, pasteboard_text, pointer_event, AppleSession, Update,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::time::Duration;

fn usage() -> &'static str {
    "usage:\n  ss-client probe    [--host mm.local] [--port 5900] [--user tai] [--password <password>]\n  ss-client snapshot --out artifacts/rust.png [--mvs] [connection opts]\n  ss-client stream   --frames 3 --out artifacts/stream.png [--mvs] [--dump-mvs PREFIX] [connection opts]\n  ss-client mvs-file --in artifacts/mvs-fixture.bin [--quant artifacts/mvs-fixture-quant.bin] [--out artifacts/mvs.png]\n  ss-client mvs-compare --out artifacts/cmp-mvs.png [--out-zlib artifacts/cmp-zlib.png] [--timeout-secs 30]\n  ss-client gesture  --kind swipe|scroll [gesture opts] --out artifacts/gesture.png\n  ss-client demo-input --x 1000 --y 800 --text hello --out artifacts/input.png\n  ss-client clipboard --text hello [--no-auto]\n\n--mvs advertises MultiVariant (0x3f3) only and decodes tiles into the framebuffer.\nmvs-compare decodes one MVS and one Zlib full frame (one session each) and reports\nlatency plus pixel agreement.\n\ngesture opts:\n  swipe:  --motion N --swipe-mask N --progress F --px F --py F --vx F --vy F --vz F --flags N --flavor N --series\n  scroll: --dx F --dy F --dz F --natural 0|1 --phase N --mask N --series\n  common: --delay-ms N"
}

/// Render a saved MVS type-0 body to PNG without touching the network.
fn mvs_file(opts: &HashMap<String, String>) -> Result<()> {
    let input = get(opts, "in").unwrap_or("artifacts/mvs-4.bin");
    let body = std::fs::read(input).with_context(|| format!("read {input}"))?;
    let width: u16 = num(opts, "width", 2560u16);
    let height: u16 = num(opts, "height", 1600u16);
    let mut dec = MvsDecoder::new(width, height);
    if let Some(path) = get(opts, "quant") {
        let q = std::fs::read(path).with_context(|| format!("read {path}"))?;
        dec.quant = QuantTables::from_type2(&q)?;
        println!("quant tables from {path}");
    }
    let mut fb = vec![0u8; width as usize * height as usize * 4];
    let stats = dec
        .apply(&body, Rect::new(0, 0, width, height), &mut fb)?
        .context("type-2 body: pass the type-0 frame instead")?;
    println!(
        "decoded {} tiles, {} commands, {} dct blocks ({} bits)",
        stats.tiles, stats.commands, stats.dct_blocks, stats.dct_bits
    );
    let out = get(opts, "out").unwrap_or("artifacts/mvs.png");
    if let Some(parent) = std::path::Path::new(out).parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    write_png(out, width, height, &fb)?;
    println!("wrote {out}");
    Ok(())
}

fn write_png(path: &str, width: u16, height: u16, bgrx: &[u8]) -> Result<()> {
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for px in bgrx.chunks_exact(4) {
        rgb.push(px[2]);
        rgb.push(px[1]);
        rgb.push(px[0]);
    }
    let file = File::create(path).with_context(|| format!("create {path}"))?;
    let w = BufWriter::new(file);
    let mut enc = png::Encoder::new(w, width as u32, height as u32);
    enc.set_color(png::ColorType::Rgb);
    enc.set_depth(png::BitDepth::Eight);
    let mut writer = enc.write_header()?;
    writer.write_image_data(&rgb)?;
    Ok(())
}

/// Capture one full frame over an already-split session.
///
/// Sends a single non-incremental request, then an *incremental* one after
/// every update that does not carry the full frame. MVS needs the extra
/// request: the first non-incremental one is answered with `0x451`/`0x453`
/// metadata and the type-2 quant body only, and the full tile body follows the
/// next incremental request.
fn capture_full(
    reader: &mut rfb_proto::SessionReader,
    writer: &mut rfb_proto::SessionWriter,
    mvs: &mut Option<MvsDecoder>,
) -> Result<(u16, u16, Vec<u8>)> {
    let width = reader.info.width;
    let height = reader.info.height;
    let mut fb = vec![0u8; width as usize * height as usize * 4];
    writer.send(&framebuffer_update_request(width, height, false))?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    for _ in 0..2000 {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!("no full framebuffer update within 30 s");
        }
        let update = reader.read_update()?;
        let full = update
            .rects
            .iter()
            .any(|r| r.w == width && r.h == height && !r.pixels.is_empty());
        apply_update(&mut fb, width, height, &update, mvs)?;
        if full {
            return Ok((width, height, fb));
        }
        writer.send(&framebuffer_update_request(width, height, true))?;
    }
    anyhow::bail!("no full framebuffer update received")
}

/// Copy one rectangle of BGRX pixels into the framebuffer.
fn copy_rect(fb: &mut [u8], fb_w: u16, fb_h: u16, r: &rfb_proto::RectUpdate) {
    if r.pixels.is_empty() || r.w == 0 || r.h == 0 {
        return;
    }
    let src_row = r.w as usize * 4;
    for row in 0..r.h as usize {
        let dy = r.y as usize + row;
        if dy >= fb_h as usize || r.x as usize + r.w as usize > fb_w as usize {
            continue;
        }
        let src = row * src_row;
        if src + src_row > r.pixels.len() {
            break;
        }
        let dst = (dy * fb_w as usize + r.x as usize) * 4;
        fb[dst..dst + src_row].copy_from_slice(&r.pixels[src..src + src_row]);
    }
}

/// What an update painted.
#[derive(Default, Clone, Copy)]
struct Applied {
    /// An MVS body painted tiles into the framebuffer.
    painted: bool,
    /// DCT blocks decoded by MVS bodies in this update.
    dct_blocks: usize,
}

/// Apply an update: copy pixel rectangles and decode MVS bodies.
fn apply_update(
    fb: &mut [u8],
    fb_w: u16,
    fb_h: u16,
    update: &Update,
    mvs: &mut Option<MvsDecoder>,
) -> Result<Applied> {
    let mut applied = Applied::default();
    for r in &update.rects {
        if r.encoding == 0x3f3 {
            let Some(dec) = mvs.as_mut() else {
                continue;
            };
            let rect = Rect::new(r.x, r.y, r.w, r.h);
            if let Some(stats) = dec.apply(&r.pixels, rect, fb)? {
                println!(
                    "  mvs rect {}x{}+{}+{}: {} tiles, {} commands, {} dct blocks ({} bits)",
                    r.w,
                    r.h,
                    r.x,
                    r.y,
                    stats.tiles,
                    stats.commands,
                    stats.dct_blocks,
                    stats.dct_bits
                );
                applied.painted = true;
                applied.dct_blocks += stats.dct_blocks;
            }
            continue;
        }
        copy_rect(fb, fb_w, fb_h, r);
    }
    Ok(applied)
}

/// One captured full frame plus its timings.
struct CapturedFrame {
    width: u16,
    height: u16,
    fb: Vec<u8>,
    /// SetEncodings -> request -> frame applied.
    end_to_end_ms: f64,
    /// Applying the target update to the framebuffer.
    decode_ms: f64,
    dct_blocks: usize,
}

/// Connect a private session, advertise one encoding, and capture one full
/// frame. Every wait is bounded by `budget_secs` and every read is bounded by
/// the session's socket timeout, so a stalled server reports an error instead
/// of hanging.
#[allow(clippy::too_many_arguments)]
fn capture_encoded_frame(
    host: &str,
    port: u16,
    user: &str,
    password: &str,
    encoding: i32,
    mvs: bool,
    budget_secs: u64,
    opts: &HashMap<String, String>,
) -> Result<CapturedFrame> {
    let mut session = AppleSession::connect(host, port, user, password)?;
    session.debug_control = opts.contains_key("debug-controls");
    session.start_streaming()?;
    let (mut reader, mut writer) = session.split()?;
    // A stalled server must report an error, not hang: bound every read.
    let read_timeout = Duration::from_secs(budget_secs.min(5).max(1));
    reader.set_read_timeout(read_timeout)?;
    let (width, height) = (reader.info.width, reader.info.height);
    let mut dec = mvs.then(|| MvsDecoder::new(width, height));
    let mut fb = vec![0u8; width as usize * height as usize * 4];

    writer.send(&rfb_proto::set_encodings(&[encoding]))?;
    let started = std::time::Instant::now();
    let deadline = started + Duration::from_secs(budget_secs);
    writer.send(&framebuffer_update_request(width, height, false))?;

    let mut settled = 0usize;
    for _ in 0..2000 {
        if std::time::Instant::now() >= deadline {
            anyhow::bail!(
                "no full 0x{encoding:x} frame within {budget_secs} s ({settled} updates seen)"
            );
        }
        let update = reader
            .read_update()
            .with_context(|| format!("0x{encoding:x}: no update within {read_timeout:?}"))?;
        settled += 1;
        if opts.contains_key("debug-controls") {
            let d: Vec<_> = update
                .rects
                .iter()
                .map(|r| (r.x, r.y, r.w, r.h, r.encoding, r.pixels.len()))
                .collect();
            eprintln!("0x{encoding:x} update {settled}: {d:?}");
        }
        let full = update
            .rects
            .iter()
            .any(|r| r.encoding == encoding as u32 && r.w == width && r.h == height);
        let t = std::time::Instant::now();
        let applied = apply_update(&mut fb, width, height, &update, &mut dec)?;
        let decode_ms = t.elapsed().as_secs_f64() * 1000.0;
        if full {
            let dct_blocks = if mvs { applied.dct_blocks } else { 0 };
            return Ok(CapturedFrame {
                width,
                height,
                fb,
                end_to_end_ms: started.elapsed().as_secs_f64() * 1000.0,
                decode_ms,
                dct_blocks,
            });
        }
        // MVS answers the first non-incremental request with metadata and the
        // type-2 quant body only; incremental requests pull the tile body.
        writer.send(&framebuffer_update_request(width, height, true))?;
    }
    anyhow::bail!("no full 0x{encoding:x} frame after 2000 updates")
}

fn get<'a>(m: &'a HashMap<String, String>, key: &str) -> Option<&'a str> {
    m.get(key).map(|s| s.as_str())
}

fn num<T: std::str::FromStr>(m: &HashMap<String, String>, key: &str, default: T) -> T {
    get(m, key).and_then(|s| s.parse().ok()).unwrap_or(default)
}

fn main() -> Result<()> {
    let mut it = std::env::args().skip(1);
    let cmd = it.next().unwrap_or_else(|| "probe".to_string());
    if cmd == "-h" || cmd == "--help" {
        println!("{}", usage());
        return Ok(());
    }
    let mut opts = HashMap::new();
    let mut pending: Option<String> = None;
    for arg in it {
        if let Some(k) = pending.take() {
            if let Some(next) = arg.strip_prefix("--") {
                // Boolean flag followed by another option.
                opts.insert(k, "true".to_string());
                pending = Some(next.to_string());
            } else {
                opts.insert(k, arg);
            }
        } else if let Some(k) = arg.strip_prefix("--") {
            pending = Some(k.to_string());
        }
    }
    if let Some(k) = pending {
        opts.insert(k, "true".to_string());
    }

    let host = get(&opts, "host").unwrap_or("mm.local").to_string();
    let port: u16 = num(&opts, "port", 5900);
    let user = get(&opts, "user").unwrap_or("tai").to_string();
    let password = get(&opts, "password").map(str::to_string).or_else(|| std::env::var("SS_PASSWORD").ok()).unwrap_or_default();

    // Offline modes do not open a connection.
    if cmd == "mvs-file" {
        return mvs_file(&opts);
    }

    let mut session = AppleSession::connect(&host, port, &user, &password)?;
    session.debug_control = opts.contains_key("debug-controls");
    println!(
        "connected: {}x{} name={:?}",
        session.info.width,
        session.info.height,
        String::from_utf8_lossy(&session.info.name)
    );

    match cmd.as_str() {
        "probe" => Ok(()),
        "clipboard" => {
            session.start_streaming()?;
            let (mut _reader, mut writer) = session.split()?;
            if !opts.contains_key("no-auto") {
                writer.send(&auto_pasteboard(1))?;
                std::thread::sleep(Duration::from_millis(200));
            }
            let text = get(&opts, "text").unwrap_or("ss-clone clipboard test");
            writer.send(&client_cut_text(text.as_bytes()))?;
            println!("sent clipboard text: {text:?}");
            // The server installs the text as a *promise* on the remote
            // pasteboard, so the data is only readable while we stay
            // connected. --hold-ms keeps the session open for inspection.
            let hold: u64 = num(&opts, "hold-ms", 1000u64);
            std::thread::sleep(Duration::from_millis(hold));
            Ok(())
        }
        "clipboard-sync" => {
            // Send text with AutoPasteboard enabled, then disable it and read
            // the resulting remote pasteboard back.
            session.start_streaming()?;
            let (mut reader, mut writer) = session.split()?;
            let text = get(&opts, "text").unwrap_or("ss-clone sync test");
            writer.send(&auto_pasteboard(1))?;
            std::thread::sleep(Duration::from_millis(300));
            writer.send(&client_cut_text(text.as_bytes()))?;
            std::thread::sleep(Duration::from_millis(600));
            writer.send(&auto_pasteboard(2))?;
            std::thread::sleep(Duration::from_millis(300));
            writer.send(&clipboard_fetch())?;
            let deadline = std::time::Instant::now()
                + Duration::from_millis(num(&opts, "wait-ms", 3000u64));
            while std::time::Instant::now() < deadline {
                writer.send(&framebuffer_update_request(
                    reader.info.width,
                    reader.info.height,
                    true,
                ))?;
                match reader.read_update() {
                    Ok(update) => {
                        for send in update.clipboard {
                            let archive = send.archive()?;
                            println!(
                                "readback promise={} uncompressed={}",
                                send.promise, send.uncompressed_size
                            );
                            if let Some(t) = pasteboard_text(&archive)? {
                                println!("remote text: {t:?}");
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("read: {e:#}");
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            Ok(())
        }
        "clipboard-read" => {
            session.start_streaming()?;
            let (mut reader, mut writer) = session.split()?;
            if !opts.contains_key("no-auto") {
                // selector 1 = start monitoring the remote pasteboard.
                writer.send(&auto_pasteboard(1))?;
                std::thread::sleep(Duration::from_millis(200));
            }
            writer.send(&clipboard_fetch())?;
            let deadline = std::time::Instant::now()
                + Duration::from_millis(num(&opts, "wait-ms", 4000u64));
            let (fw, fh) = (reader.info.width, reader.info.height);
            let poll_fetch = opts.contains_key("poll-fetch");
            let mut next_fetch = std::time::Instant::now();
            while std::time::Instant::now() < deadline {
                if poll_fetch && std::time::Instant::now() >= next_fetch {
                    let mut f = clipboard_fetch();
                    let want: usize = num(&opts, "fetch-len", 1usize);
                    f.resize(want, 0);
                    writer.send(&f)?;
                    next_fetch = std::time::Instant::now() + Duration::from_millis(250);
                }
                if !opts.contains_key("quiet") {
                    writer.send(&framebuffer_update_request(fw, fh, true))?;
                } else {
                    std::thread::sleep(Duration::from_millis(20));
                }
                match reader.read_update() {
                    Ok(update) => {
                        if update.clipboard_changed {
                            println!("pasteboard changed; fetching");
                            writer.send(&clipboard_fetch())?;
                        }
                        for send in update.clipboard {
                            let archive = send.archive()?;
                            println!(
                                "clipboard promise={} uncompressed={} archive={} bytes",
                                send.promise,
                                send.uncompressed_size,
                                archive.len()
                            );
                            if opts.contains_key("dump") {
                                let _ = std::fs::write("artifacts/clipboard-archive.bin", &archive);
                            }
                            match pasteboard_text(&archive)? {
                                Some(text) => println!("text: {text:?}"),
                                None => println!("(no text flavour)"),
                            }
                        }
                    }
                    Err(e) => {
                        eprintln!("read: {e:#}");
                        break;
                    }
                }
                std::thread::sleep(Duration::from_millis(4));
            }
            Ok(())
        }
        "stream" => {
            session.start_streaming()?;
            if let Some(scale) = get(&opts, "scale") {
                let scale: f64 = scale.parse()?;
                session.set_scale(scale)?;
                println!("requested scale {scale}");
            }
            let (mut reader, mut writer) = session.split()?;
            if let Some(enc) = get(&opts, "encoding") {
                let enc: i32 = enc.parse()?;
                writer.send(&rfb_proto::set_encodings(&[enc]))?;
                println!("advertised encoding {enc} only");
            } else if opts.contains_key("mvs") {
                writer.send(&rfb_proto::set_encodings(&[0x3f3]))?;
                println!("advertised MVS (0x3f3) only");
            }
            let width = reader.info.width;
            let height = reader.info.height;
            let logical = opts.contains_key("logical");
            let auto = opts.contains_key("auto");
            let auto_version: u16 = num(&opts, "auto-version", 1u16);
            let auto_screen: u32 = num(&opts, "auto-screen", 0xffff_ffffu32);
            let auto_w = if logical { width / 2 } else { width };
            let auto_h = if logical { height / 2 } else { height };
            let mut fb = vec![0u8; width as usize * height as usize * 4];
            let mut mvs = opts
                .contains_key("mvs")
                .then(|| MvsDecoder::new(width, height));
            if let Some(nb) = get(&opts, "bands") {
                let nb: usize = nb.parse()?;
                let band_h = (height as usize + nb - 1) / nb;
                let mut total = 0f64;
                for i in 0..nb {
                    let y = (i * band_h) as u16;
                    let h = ((height as usize - y as usize).min(band_h)) as u16;
                    let t0 = std::time::Instant::now();
                    writer.send(&rfb_proto::framebuffer_update_request_rect(
                        0, y, width, h, false,
                    ))?;
                    let upd = reader.read_update()?;
                    let dt = t0.elapsed().as_secs_f64() * 1000.0;
                    let bytes: usize = upd.rects.iter().map(|r| r.pixels.len()).sum();
                    println!("band {i}/{nb} y={y} h={h}: {dt:.1} ms, {} rect(s), {bytes} bytes", upd.rects.len());
                    total += dt;
                }
                println!("bands pass1 total {total:.1} ms");
                let passes: usize = get(&opts, "passes").map(|v| v.parse()).transpose()?.unwrap_or(2);
                for pass in 2..=passes {
                    let mut tot = 0f64;
                    for i in 0..nb {
                        let y = (i * band_h) as u16;
                        let h = ((height as usize - y as usize).min(band_h)) as u16;
                        let t0 = std::time::Instant::now();
                        writer.send(&rfb_proto::framebuffer_update_request_rect(0, y, width, h, false))?;
                        let upd = reader.read_update()?;
                        let dt = t0.elapsed().as_secs_f64() * 1000.0;
                        let bytes: usize = upd.rects.iter().map(|r| r.pixels.len()).sum();
                        println!("pass{pass} band {i}: {dt:.1} ms {bytes} bytes");
                        tot += dt;
                    }
                    println!("pass{pass} total {tot:.1} ms");
                }
                return Ok(());
            }
            if auto {
                writer.send(&auto_framebuffer_update_custom(
                    auto_version,
                    auto_screen,
                    auto_w,
                    auto_h,
                ))?;
            }
            writer.send(&framebuffer_update_request(width, height, false))?;
            if opts.contains_key("move") {
                writer.send(&pointer_event(0, 300, 300))?;
            }
            let frames: u32 = num(&opts, "frames", 3u32);
            for i in 0..frames {
                let update = reader.read_update()?;
                apply_update(&mut fb, width, height, &update, &mut mvs)?;
                let desc: Vec<_> = update
                    .rects
                    .iter()
                    .map(|r| (r.x, r.y, r.w, r.h, r.encoding, r.pixels.len()))
                    .collect();
                println!("frame {}/{}: {} rect(s) {:?}", i + 1, frames, update.rects.len(), desc);
                if let Some(r) = update.rects.iter().find(|r| matches!(r.encoding, 0x3e8 | 0x3e9 | 0x3ea)) {
                    let path = format!("artifacts/tier-{}-{}.bin", i + 1, r.encoding);
                    let _ = std::fs::write(&path, &r.pixels);
                    println!("  saved {path} ({} bytes, {}x{})", r.pixels.len(), r.w, r.h);
                }
                if let Some(prefix) = get(&opts, "dump-mvs") {
                    if let Some(r) = update.rects.iter().find(|r| r.encoding == 0x3f3) {
                        let path = format!("{prefix}-{}.bin", i + 1);
                        let _ = std::fs::write(&path, &r.pixels);
                        println!("  wrote {path} ({} bytes, {}x{})", r.pixels.len(), r.w, r.h);
                    }
                }
                let layout = update.rects.iter().any(|r| r.encoding == 0x451);
                if auto && layout {
                    writer.send(&auto_framebuffer_update_custom(
                        auto_version,
                        auto_screen,
                        auto_w,
                        auto_h,
                    ))?;
                    writer.send(&framebuffer_update_request(width, height, false))?;
                } else if i + 1 < frames {
                    writer.send(&framebuffer_update_request(width, height, true))?;
                }
            }
            let out = get(&opts, "out").unwrap_or("artifacts/stream.png");
            if let Some(parent) = std::path::Path::new(out).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            write_png(out, width, height, &fb)?;
            println!("wrote {out}");
            Ok(())
        }
        "mvs-compare" => {
            // One frame per session. The server will not switch encodings
            // back to Zlib after a SetEncodings that removed it, so each
            // encoding gets its own freshly connected session; the MVS frame
            // (fast) is captured immediately after the Zlib one so both see
            // almost the same screen.
            let budget: u64 = num(&opts, "timeout-secs", 30u64);
            let zlib = capture_encoded_frame(
                &host, port, &user, &password, 6, false, budget, &opts,
            )?;
            let mvs = capture_encoded_frame(
                &host,
                port,
                &user,
                &password,
                0x3f3,
                true,
                budget,
                &opts,
            )?;
            if (zlib.width, zlib.height) != (mvs.width, mvs.height) {
                anyhow::bail!(
                    "geometry changed between sessions: zlib {}x{} vs mvs {}x{}",
                    zlib.width,
                    zlib.height,
                    mvs.width,
                    mvs.height
                );
            }
            let n = zlib.width as usize * zlib.height as usize;
            let (mut exact, mut close) = (0usize, 0usize);
            let mut worst = 0u8;
            let mut sum = [0f64; 3];
            for (a, b) in zlib.fb.chunks_exact(4).zip(mvs.fb.chunks_exact(4)) {
                let mut px_worst = 0u8;
                for k in 0..3 {
                    let d = a[k].abs_diff(b[k]);
                    sum[k] += d as f64;
                    px_worst = px_worst.max(d);
                }
                worst = worst.max(px_worst);
                if px_worst == 0 {
                    exact += 1;
                } else if px_worst <= 2 {
                    close += 1;
                }
            }
            println!(
                "mvs:  {:.1} ms end to end ({:.1} ms decode, {} dct blocks)",
                mvs.end_to_end_ms, mvs.decode_ms, mvs.dct_blocks
            );
            println!(
                "zlib: {:.1} ms end to end ({:.1} ms decode)",
                zlib.end_to_end_ms, zlib.decode_ms
            );
            println!(
                "pixels: exact {:.2}%, <=2 {:.2}%, >2 {:.2}% (worst {worst})",
                100.0 * exact as f64 / n as f64,
                100.0 * close as f64 / n as f64,
                100.0 * (n - exact - close) as f64 / n as f64
            );
            println!(
                "mean abs error: B {:.3}, G {:.3}, R {:.3} (0..255)",
                sum[0] / n as f64,
                sum[1] / n as f64,
                sum[2] / n as f64
            );
            if let Some(out) = get(&opts, "out") {
                if let Some(parent) = std::path::Path::new(out).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                write_png(out, mvs.width, mvs.height, &mvs.fb)?;
                println!("wrote {out} (MVS frame)");
            }
            if let Some(out) = get(&opts, "out-zlib") {
                if let Some(parent) = std::path::Path::new(out).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                write_png(out, zlib.width, zlib.height, &zlib.fb)?;
                println!("wrote {out} (Zlib frame)");
            }
            Ok(())
        }
        "snapshot" | "gesture" | "demo-input" => {
            session.start_streaming()?;
            if opts.contains_key("mvs") && cmd == "snapshot" {
                session.send_message(&rfb_proto::set_encodings(&[0x3f3]))?;
                println!("advertised MVS (0x3f3) only");
            }
            if cmd == "demo-input" {
                let x: u16 = num(&opts, "x", 1000u16);
                let y: u16 = num(&opts, "y", 800u16);
                session.send_message(&pointer_event(0, x, y))?;
                std::thread::sleep(Duration::from_millis(100));
                session.send_message(&pointer_event(1, x, y))?;
                std::thread::sleep(Duration::from_millis(80));
                session.send_message(&pointer_event(0, x, y))?;
                std::thread::sleep(Duration::from_millis(200));
                let text = get(&opts, "text").unwrap_or("hello").to_string();
                for ch in text.chars() {
                    let keysym = match ch {
                        '\n' => 0xff0d,
                        c if c.is_ascii() => c as u32,
                        _ => continue,
                    };
                    session.send_message(&key_event(keysym, true))?;
                    std::thread::sleep(Duration::from_millis(20));
                    session.send_message(&key_event(keysym, false))?;
                    std::thread::sleep(Duration::from_millis(20));
                }
                std::thread::sleep(Duration::from_millis(400));
            }
            if cmd == "gesture" {
                let width = session.info.width;
                let height = session.info.height;
                let kind = get(&opts, "kind").unwrap_or("swipe");
                let series = opts.contains_key("series");
                let cx: u16 = num(&opts, "x", width / 2);
                let cy: u16 = num(&opts, "y", height / 2);
                if kind == "magnify" || kind == "rotate" {
                    let value = if kind == "magnify" {
                        num(&opts, "value", 0.2f64)
                    } else {
                        num(&opts, "value", 0.3f64)
                    };
                    let mask = num(&opts, "mask", 31u64);
                    let phases: Vec<u64> = if series {
                        vec![1, 2, 2, 4]
                    } else {
                        vec![num(&opts, "phase", 2u64)]
                    };
                    for p in phases {
                        let body = if kind == "magnify" {
                            gesture_magnify(value, p, mask, cx, cy)
                        } else {
                            gesture_rotate(value, p, mask, cx, cy)
                        };
                        session.send_gesture(&body)?;
                        std::thread::sleep(Duration::from_millis(30));
                    }
                } else if kind == "smart" {
                    session.send_gesture(&gesture_smart_magnify(cx, cy))?;
                } else if kind == "quicklook" {
                    session.send_gesture(&gesture_quicklook(cx, cy))?;
                } else if kind == "buttonscroll" {
                    // Standard VNC scroll-wheel convention: buttons 4/5.
                    let notches = num(&opts, "notches", 3i32);
                    let up = num(&opts, "dy", -1i32) < 0;
                    let bit: u8 = if up { 8 } else { 16 };
                    session.send_message(&pointer_event(0, cx, cy))?;
                    for _ in 0..notches.abs() {
                        session.send_message(&pointer_event(bit, cx, cy))?;
                        std::thread::sleep(Duration::from_millis(30));
                        session.send_message(&pointer_event(0, cx, cy))?;
                        std::thread::sleep(Duration::from_millis(30));
                    }
                } else if kind == "wheel" {
                    // Subtype 11 (MouseScroll). macOS only acts on a complete
                    // gesture: began(1) -> changed(2)... -> ended(4).
                    let phases: &[u32] = if series { &[1, 2, 2, 4] } else { &[num(&opts, "phase", 2u32)] };
                    let point_y: i32 = num(&opts, "point-y", -10i32);
                    let dy: i16 = num(&opts, "dy", -10i16);
                    for &phase in phases {
                        let (dy, point_y) = if phase == 2 { (dy, point_y) } else { (0, 0) };
                        let fixed_y: i32 = num(&opts, "fixed-y", 0i32);
                        let body = gesture_mouse_scroll(
                            num(&opts, "dx", 0i16),
                            dy,
                            num(&opts, "dz", 0i16),
                            (0, if phase == 2 { fixed_y } else { 0 }, 0),
                            (0, point_y, 0),
                            phase,
                            0,
                            cx,
                            cy,
                        );
                        session.send_gesture(&body)?;
                        std::thread::sleep(Duration::from_millis(16));
                    }
                } else if kind == "swipe" {
                    let body = gesture_swipe(
                        num(&opts, "motion", 1u32),
                        num(&opts, "swipe-mask", 1u32),
                        num(&opts, "progress", 1.0f32),
                        num(&opts, "px", 0.5f32),
                        num(&opts, "py", 0.5f32),
                        num(&opts, "vx", 0.0f32),
                        num(&opts, "vy", 0.0f32),
                        num(&opts, "vz", 0.0f32),
                        num(&opts, "flags", 0u32),
                        num(&opts, "flavor", 0u32),
                        cx,
                        cy,
                    );
                    if series {
                        session.send_gesture(&gesture_begin_end(1, 0, cx, cy))?;
                        std::thread::sleep(Duration::from_millis(50));
                        session.send_gesture(&body)?;
                        std::thread::sleep(Duration::from_millis(50));
                        session.send_gesture(&gesture_begin_end(2, 0, cx, cy))?;
                    } else {
                        session.send_gesture(&body)?;
                    }
                } else {
                    let phases: &[(u32, f32)] = if series {
                        &[(1, 0.0), (2, num(&opts, "dy", -10.0f32)), (4, 0.0)]
                    } else {
                        &[(num(&opts, "phase", 2u32), num(&opts, "dy", -10.0f32))]
                    };
                    for (phase, dy) in phases {
                        let body = gesture_scroll(
                            num(&opts, "dx", 0.0f32),
                            *dy,
                            num(&opts, "dz", 0.0f32),
                            num(&opts, "natural", 1u32),
                            *phase,
                            num(&opts, "mask", 31u32),
                            cx,
                            cy,
                        );
                        session.send_gesture(&body)?;
                        std::thread::sleep(Duration::from_millis(50));
                    }
                }
                let delay: u64 = num(&opts, "delay-ms", 700u64);
                std::thread::sleep(Duration::from_millis(delay));
            }

            let (mut reader, mut writer) = session.split()?;
            let mut mvs = opts
                .contains_key("mvs")
                .then(|| MvsDecoder::new(reader.info.width, reader.info.height));
            let (width, height, pixels) = capture_full(&mut reader, &mut writer, &mut mvs)?;
            let out = get(&opts, "out").unwrap_or("artifacts/rust.png");
            if let Some(parent) = std::path::Path::new(out).parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            write_png(out, width, height, &pixels)?;
            println!("wrote {out}");
            Ok(())
        }
        other => {
            eprintln!("unknown command {other}\n{}", usage());
            std::process::exit(2);
        }
    }
}
