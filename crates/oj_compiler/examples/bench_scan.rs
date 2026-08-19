use std::path::Path;
use std::time::Instant;

use memchr::memmem::Finder;

fn collect(dir: &Path, out: &mut Vec<String>) {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return;
    };
    for e in rd.flatten() {
        let p = e.path();
        if p.is_dir() {
            collect(&p, out);
        } else if p.extension().is_some_and(|x| x == "tsx" || x == "ts") {
            if let Ok(s) = std::fs::read_to_string(&p) {
                out.push(s);
            }
        }
    }
}

fn main() {
    let mut sources = Vec::new();
    for d in ["bench/apps/app-1000/src", "www/src", "playground/src"] {
        collect(Path::new(d), &mut sources);
    }
    if sources.is_empty() {
        eprintln!("run from the repo root; no sources found");
        std::process::exit(1);
    }
    let bytes: usize = sources.iter().map(|s| s.len()).sum();
    let iters = 200;

    let mut hits_old = 0usize;
    let t = Instant::now();
    for _ in 0..iters {
        for s in &sources {
            hits_old += s.contains("import.meta.env") as usize;
            hits_old += s.contains("import.meta.glob") as usize;
            hits_old += s.contains("$RefreshReg$(") as usize;
        }
    }
    let old = t.elapsed();

    let f_env = Finder::new("import.meta.env");
    let f_glob = Finder::new("import.meta.glob");
    let f_reg = Finder::new("$RefreshReg$(");
    let mut hits_new = 0usize;
    let t = Instant::now();
    for _ in 0..iters {
        for s in &sources {
            hits_new += f_env.find(s.as_bytes()).is_some() as usize;
            hits_new += f_glob.find(s.as_bytes()).is_some() as usize;
            hits_new += f_reg.find(s.as_bytes()).is_some() as usize;
        }
    }
    let new = t.elapsed();

    assert_eq!(hits_old, hits_new, "both must report the same matches");
    let scans = (sources.len() * 3 * iters) as f64;
    println!(
        "prescan {} modules ({} KB) x {iters} iters, 3 scans each:",
        sources.len(),
        bytes / 1024
    );
    println!(
        "  OLD (str::contains):  {old:?}  = {:.1} ns/scan",
        old.as_nanos() as f64 / scans
    );
    println!(
        "  NEW (memmem::Finder): {new:?}  = {:.1} ns/scan",
        new.as_nanos() as f64 / scans
    );
    println!("  speedup: {:.1}x", old.as_secs_f64() / new.as_secs_f64());
}
