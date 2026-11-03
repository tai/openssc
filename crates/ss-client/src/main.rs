use anyhow::{Context, Result};
use rfb_proto::{
    auto_framebuffer_update_custom, auto_pasteboard, client_cut_text, framebuffer_update_request,
    gesture_begin_end, gesture_magnify, gesture_mouse_scroll, gesture_quicklook, gesture_rotate,
    gesture_scroll, gesture_smart_magnify, gesture_swipe, key_event, pointer_event, AppleSession,
    Update,
};
use std::collections::HashMap;
use std::fs::File;
use std::io::BufWriter;
use std::time::Duration;

fn usage() -> &'static str {
    "usage:\n  ss-client probe    [--host mm.local] [--port 5900] [--user tai] [--password <password>]\n  ss-client snapshot --out artifacts/rust.png [connection opts]\n  ss-client stream   --frames 3 --out artifacts/stream.png [connection opts]\n  ss-client gesture  --kind swipe|scroll [gesture opts] --out artifacts/gesture.png\n  ss-client demo-input --x 1000 --y 800 --text hello --out artifacts/input.png\n  ss-client clipboard --text hello [--no-auto]\n\ngesture opts:\n  swipe:  --motion N --swipe-mask N --progress F --px F --py F --vx F --vy F --vz F --flags N --flavor N --series\n  scroll: --dx F --dy F --dz F --natural 0|1 --phase N --mask N --series\n  common: --delay-ms N"
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

fn capture_full(
    reader: &mut rfb_proto::SessionReader,
    writer: &mut rfb_proto::SessionWriter,
) -> Result<(u16, u16, Vec<u8>)> {
    let width = reader.info.width;
    let height = reader.info.height;
    let mut fb = vec![0u8; width as usize * height as usize * 4];
    writer.send(&framebuffer_update_request(width, height, false))?;
    for _ in 0..100 {
        let update = reader.read_update()?;
        apply_update(&mut fb, width, height, &update);
        if update
            .rects
            .iter()
            .any(|r| r.w == width && r.h == height && !r.pixels.is_empty())
        {
            return Ok((width, height, fb));
        }
    }
    anyhow::bail!("no full framebuffer update received")
}

fn apply_update(fb: &mut [u8], fb_w: u16, fb_h: u16, update: &Update) {
    for r in &update.rects {
        if r.pixels.is_empty() || r.w == 0 || r.h == 0 {
            continue;
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

    let mut session = AppleSession::connect(&host, port, &user, &password)?;
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
            std::thread::sleep(Duration::from_millis(1000));
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
            let width = reader.info.width;
            let height = reader.info.height;
            let logical = opts.contains_key("logical");
            let auto = opts.contains_key("auto");
            let auto_version: u16 = num(&opts, "auto-version", 1u16);
            let auto_screen: u32 = num(&opts, "auto-screen", 0xffff_ffffu32);
            let auto_w = if logical { width / 2 } else { width };
            let auto_h = if logical { height / 2 } else { height };
            let mut fb = vec![0u8; width as usize * height as usize * 4];
            if auto {
                writer.send(&auto_framebuffer_update_custom(
                    auto_version,
                    auto_screen,
                    auto_w,
                    auto_h,
                ))?;
            }
            writer.send(&framebuffer_update_request(width, height, false))?;
            let frames: u32 = num(&opts, "frames", 3u32);
            for i in 0..frames {
                let update = reader.read_update()?;
                apply_update(&mut fb, width, height, &update);
                let desc: Vec<_> = update
                    .rects
                    .iter()
                    .map(|r| (r.x, r.y, r.w, r.h, r.encoding, r.pixels.len()))
                    .collect();
                println!("frame {}/{}: {} rect(s) {:?}", i + 1, frames, update.rects.len(), desc);
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
        "snapshot" | "gesture" | "demo-input" => {
            session.start_streaming()?;
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
                        let body = gesture_mouse_scroll(
                            num(&opts, "dx", 0i16),
                            dy,
                            num(&opts, "dz", 0i16),
                            (0, 0, 0),
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
            let (width, height, pixels) = capture_full(&mut reader, &mut writer)?;
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
