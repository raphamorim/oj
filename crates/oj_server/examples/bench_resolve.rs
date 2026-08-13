// Isolates the relative-import resolution phase: resolving N extensionless
// sibling imports (`./Comp<i>`) from one directory, the exact stat pattern
// rewrite_specifier uses, with vs without the per-directory listing cache.
//   cargo run --release --example bench_resolve -p oj_server
use std::ffi::OsString;
use std::path::Path;
use std::time::Instant;

const COMPILABLE: &[&str] = &["tsx", "ts", "jsx", "js", "mjs"];

// The old path: rewrite_specifier stats `joined` twice (its 1547 + 1560 checks),
// then probes each candidate extension until one hits.
fn resolve_stats(dir: &Path, name: &str) -> bool {
    let joined = dir.join(name); // extensionless
    let _ = joined.is_file(); // stat #1 (line 1547)
    if joined.is_file() {
        // stat #2 (line 1560)
        return true;
    }
    COMPILABLE.iter().any(|ext| joined.with_extension(ext).is_file()) // probe
}

// The new path: one read_dir per directory, then in-memory membership.
fn resolve_cached(entries: &std::collections::HashSet<OsString>, name: &str) -> bool {
    if entries.contains(OsString::from(name).as_os_str()) {
        return true;
    }
    COMPILABLE
        .iter()
        .any(|ext| entries.contains(OsString::from(format!("{name}.{ext}")).as_os_str()))
}

fn main() {
    let dir = Path::new("bench/apps/app-1000/src/components");
    if !dir.is_dir() {
        eprintln!("run from the repo root; missing {}", dir.display());
        std::process::exit(1);
    }
    let names: Vec<String> = (0..1000).map(|i| format!("Comp{i}")).collect();
    let crawls = 30; // each iteration ~ one cold-start crawl's resolution of this dir

    // OLD: per-import stat probe (no cache)
    let mut hits_old = 0usize;
    let t = Instant::now();
    for _ in 0..crawls {
        for n in &names {
            if resolve_stats(dir, n) {
                hits_old += 1;
            }
        }
    }
    let old = t.elapsed();

    // NEW: one read_dir per crawl, then in-memory lookups
    let mut hits_new = 0usize;
    let t = Instant::now();
    for _ in 0..crawls {
        let mut set = std::collections::HashSet::new();
        if let Ok(rd) = std::fs::read_dir(dir) {
            for e in rd.flatten() {
                set.insert(e.file_name());
            }
        }
        for n in &names {
            if resolve_cached(&set, n) {
                hits_new += 1;
            }
        }
    }
    let new = t.elapsed();

    assert_eq!(hits_old, hits_new, "both paths must resolve the same imports");
    let per = (names.len() * crawls) as f64;
    println!("resolve {} sibling imports x {crawls} crawls:", names.len());
    println!("  OLD (per-import stats):   {old:?}  = {:.2} us/import", old.as_nanos() as f64 / per / 1000.0);
    println!("  NEW (read_dir + lookup):  {new:?}  = {:.2} us/import", new.as_nanos() as f64 / per / 1000.0);
    println!("  speedup: {:.1}x", old.as_secs_f64() / new.as_secs_f64());
}
