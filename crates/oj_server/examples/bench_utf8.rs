// Measures source-file UTF-8 validation: std::str::from_utf8 (scalar-ish, old)
// vs simdutf8::basic::from_utf8 (SIMD, new), over a real TSX source corpus.
// Every module read on the cold-start crawl validates its bytes as UTF-8.
//   cargo run --release --example bench_utf8 -p oj_server
use std::path::Path;
use std::time::Instant;

fn collect(dir: &Path, out: &mut Vec<Vec<u8>>) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "tsx" || x == "ts") {
            if let Ok(b) = std::fs::read(&p) {
                out.push(b);
            }
        }
    }
}

fn main() {
    let mut files = Vec::new();
    for d in ["bench/apps/app-1000/src", "www/src", "playground/src"] {
        collect(Path::new(d), &mut files);
    }
    if files.is_empty() {
        eprintln!("run from the repo root; no sources found");
        std::process::exit(1);
    }
    let total: usize = files.iter().map(|b| b.len()).sum();
    let iters = 300;

    // OLD: standard-library validation
    let mut ok_old = 0usize;
    let t = Instant::now();
    for _ in 0..iters {
        for b in &files {
            ok_old += std::str::from_utf8(b).is_ok() as usize;
        }
    }
    let old = t.elapsed();

    // NEW: SIMD validation
    let mut ok_new = 0usize;
    let t = Instant::now();
    for _ in 0..iters {
        for b in &files {
            ok_new += simdutf8::basic::from_utf8(b).is_ok() as usize;
        }
    }
    let new = t.elapsed();

    assert_eq!(ok_old, ok_new, "both must validate the same files");
    let mb = (total * iters) as f64 / (1024.0 * 1024.0);
    println!("validate {} files ({} KB) x {iters} iters:", files.len(), total / 1024);
    println!("  OLD (std::str::from_utf8):     {old:?}  = {:.2} GB/s", mb / 1024.0 / old.as_secs_f64());
    println!("  NEW (simdutf8::basic):         {new:?}  = {:.2} GB/s", mb / 1024.0 / new.as_secs_f64());
    println!("  speedup: {:.1}x", old.as_secs_f64() / new.as_secs_f64());
}
