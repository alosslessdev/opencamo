//! Headless self-test: `opencamo-desktop --selftest <url> <n_frames>`.
//!
//! Connects to a `/v1/stream` URL, decodes `n_frames` JPEG frames, prints
//! stats to stdout, and exits 0 on success / nonzero on failure.
//! Never initializes eframe, so it works without a display.

use std::io::BufReader;
use std::time::{Duration, Instant};

use anyhow::Context;

use crate::protocol::ensure_stream_url;
use crate::stream::{decode_jpeg, read_part};

/// Run the self-test. Returns the process exit code.
pub fn run(args: &[String]) -> i32 {
    if args.len() != 2 {
        eprintln!("usage: opencamo-desktop --selftest <stream-url> <n_frames>");
        return 2;
    }
    let url = ensure_stream_url(&args[0]);
    let n_frames: usize = match args[1].parse() {
        Ok(n) if n > 0 => n,
        Ok(_) => {
            eprintln!("selftest: n_frames must be > 0");
            return 2;
        }
        Err(e) => {
            eprintln!("selftest: invalid n_frames {:?}: {}", args[1], e);
            return 2;
        }
    };

    match run_inner(&url, n_frames) {
        Ok(stats) => {
            println!(
                "selftest: OK — {} frames decoded from {}",
                stats.frames, url
            );
            println!("selftest: first frame: {}x{}", stats.width, stats.height);
            println!(
                "selftest: bytes read: {} ({:.1} KiB total, {:.1} KiB avg/frame)",
                stats.bytes,
                stats.bytes as f64 / 1024.0,
                (stats.bytes as f64 / stats.frames as f64) / 1024.0
            );
            println!(
                "selftest: elapsed: {:.2}s ({:.1} fps delivered)",
                stats.elapsed.as_secs_f64(),
                stats.frames as f64 / stats.elapsed.as_secs_f64()
            );
            println!(
                "selftest: decode time: {:.1}ms avg, {:.1}ms max",
                stats.avg_decode_ms, stats.max_decode_ms
            );
            0
        }
        Err(e) => {
            eprintln!("selftest: FAILED: {:#}", e);
            1
        }
    }
}

struct SelftestStats {
    frames: usize,
    bytes: usize,
    width: u32,
    height: u32,
    elapsed: Duration,
    avg_decode_ms: f64,
    max_decode_ms: f64,
}

fn run_inner(url: &str, n_frames: usize) -> anyhow::Result<SelftestStats> {
    let agent: ureq::Agent = ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(10))
        .timeout_read(Duration::from_secs(30))
        .build();

    println!("selftest: GET {}", url);
    let resp = agent
        .get(url)
        .call()
        .map_err(|e| anyhow::anyhow!("{}", crate::stream::describe_ureq_error(e)))?;

    let content_type = resp.header("content-type").unwrap_or("").to_string();
    if !content_type.contains("multipart/x-mixed-replace") {
        anyhow::bail!(
            "expected multipart/x-mixed-replace response, got {:?}",
            content_type
        );
    }
    if !content_type.contains("frame") {
        anyhow::bail!("unexpected multipart boundary in {:?}", content_type);
    }

    let mut reader = BufReader::new(resp.into_reader());
    let start = Instant::now();
    let mut bytes: usize = 0;
    let mut decode_total = Duration::ZERO;
    let mut decode_max = Duration::ZERO;
    let mut width = 0u32;
    let mut height = 0u32;

    for i in 0..n_frames {
        let part = read_part(&mut reader)
            .with_context(|| format!("reading part {} of {}", i + 1, n_frames))?
            .ok_or_else(|| anyhow::anyhow!("stream ended after {} frames", i))?;
        bytes += part.len();

        let t0 = Instant::now();
        let frame = decode_jpeg(i as u64, &part)
            .with_context(|| format!("decoding frame {}", i + 1))?;
        let dt = t0.elapsed();
        decode_total += dt;
        decode_max = decode_max.max(dt);

        if i == 0 {
            width = frame.width;
            height = frame.height;
        } else if frame.width != width || frame.height != height {
            anyhow::bail!(
                "frame {} changed size: {}x{} -> {}x{}",
                i,
                width,
                height,
                frame.width,
                frame.height
            );
        }
    }

    let elapsed = start.elapsed();
    Ok(SelftestStats {
        frames: n_frames,
        bytes,
        width,
        height,
        elapsed,
        avg_decode_ms: decode_total.as_secs_f64() * 1000.0 / n_frames as f64,
        max_decode_ms: decode_max.as_secs_f64() * 1000.0,
    })
}
