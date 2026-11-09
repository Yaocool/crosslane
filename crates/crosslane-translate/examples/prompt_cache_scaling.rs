//! Growth benchmark for prompt-cache source projection and repeated fragments.
//! Run with `cargo run -p crosslane-translate --release --example prompt_cache_scaling`.
//! Allocation bytes are cumulative requested bytes (including realloc), not RSS.
//! Input construction and final payload destruction are outside the timing window.
//! Timing is informational; the allocation budget and exact token totals are asserted.

use crosslane_translate::{claude_to_kiro, ClaudeRequest, TokenCountCache, TranslationOptions};
use serde_json::json;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
static TRACK: AtomicBool = AtomicBool::new(false);
static BYTES: AtomicU64 = AtomicU64::new(0);
struct Alloc;
unsafe impl GlobalAlloc for Alloc {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        if TRACK.load(Ordering::Relaxed) {
            BYTES.fetch_add(l.size() as u64, Ordering::Relaxed);
        }
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        if TRACK.load(Ordering::Relaxed) {
            BYTES.fetch_add(n as u64, Ordering::Relaxed);
        }
        System.realloc(p, l, n)
    }
}
#[global_allocator]
static ALLOC: Alloc = Alloc;
fn options(cache: bool) -> TranslationOptions {
    let mut o = TranslationOptions::new("claude-opus-5.5", "AI_EDITOR");
    o.enable_prompt_cache = cache;
    o.enhance_system_prompt = false;
    o.enable_request_redaction = false;
    o
}
fn request(n: usize, total: usize, repeated: bool) -> ClaudeRequest {
    let mut blocks:Vec<_>=(0..n).map(|i| json!({"type":"text", "text":if repeated { "same short fragment".to_owned() } else { format!("{:08} {}", i, "x".repeat((total/n).saturating_sub(9))) }})).collect();
    blocks.last_mut().unwrap()["cache_control"] = json!({"type":"ephemeral"});
    let r: ClaudeRequest = serde_json::from_value(json!({"model":"claude-opus-5.5","max_tokens":64,"messages":[{"role":"user","content":blocks}]})).unwrap();
    crosslane_translate::validate::validate_claude(&r).unwrap();
    crosslane_translate::validate::validate_claude_generation(&r).unwrap();
    r
}
fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    if std::env::args().any(|s| s == "--validate-only") {
        for n in [1, 16, 64, 256, 1024, 4096] {
            request(n, 1024 * 1024, false);
        }
        for n in [128, 256, 512, 1024, 2048] {
            request(n, 0, true);
        }
        println!("11 fixtures passed validate_claude and validate_claude_generation");
        return;
    }
    for n in [1, 16, 64, 256, 1024, 4096] {
        let r = request(n, 1024 * 1024, false);
        for cache in [false, true] {
            let o = options(cache);
            let mut times = Vec::new();
            for _ in 0..5 {
                let t = Instant::now();
                let p = std::hint::black_box(claude_to_kiro(&r, &o));
                times.push(t.elapsed().as_micros());
                drop(p);
            }
            BYTES.store(0, Ordering::Relaxed);
            TRACK.store(true, Ordering::Relaxed);
            let p = claude_to_kiro(&r, &o);
            TRACK.store(false, Ordering::Relaxed);
            let bytes = BYTES.load(Ordering::Relaxed);
            drop(p);
            assert!(
                bytes < 64 * 1024 * 1024,
                "1 MiB fixture must not copy the full message per block: {bytes} requested bytes"
            );
            println!(
                "TRANSLATE blocks={n} cache={cache} median_us={} allocated_bytes={bytes}",
                median(times)
            );
        }
    }
    for n in [128, 256, 512, 1024, 2048] {
        let r = request(n, 0, true);
        let p = claude_to_kiro(&r, &options(true));
        let tokenizer = TokenCountCache::new(4096).unwrap();
        let full = tokenizer.estimate_kiro_payload(&p).await.unwrap() as u64;
        tokenizer.prefix_counts(&p).await.unwrap();
        let mut baseline = Vec::new();
        let mut profile = Vec::new();
        for _ in 0..3 {
            let t = Instant::now();
            assert_eq!(
                tokenizer.estimate_kiro_payload(&p).await.unwrap() as u64,
                full
            );
            baseline.push(t.elapsed().as_micros());
            let t = Instant::now();
            let (counts, overhead) = tokenizer.prefix_counts(&p).await.unwrap();
            profile.push(t.elapsed().as_micros());
            assert_eq!(counts.iter().sum::<u64>() + overhead, full);
        }
        println!(
            "REPEATED blocks={n} body_bytes={} baseline_median_us={} prefix_median_us={}",
            serde_json::to_vec(&r).unwrap().len(),
            median(baseline),
            median(profile)
        );
    }
}
