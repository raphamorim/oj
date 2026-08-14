use std::path::Path;
use std::time::Instant;

use oj_compiler::{compile, set_import_meta_env, CompileOptions};

const SRC: &str = r#"
import { useState, useEffect } from "react";

const API = import.meta.env.VITE_API_URL;
const MODE = import.meta.env.MODE;

interface Props { label: string; count?: number }

export function Widget({ label, count = 0 }: Props) {
  const [n, setN] = useState<number>(count);
  const dev = import.meta.env.DEV;
  useEffect(() => {
    if (dev) console.log("mounted", label, API, MODE);
  }, [label]);
  return (
    <button className="widget" onClick={() => setN((v) => v + 1)} data-api={API}>
      {label}: {n} {dev ? "(dev)" : "(prod)"}
    </button>
  );
}

export function List({ items }: { items: string[] }) {
  return <ul>{items.map((it, i) => <li key={i}><Widget label={it} /></li>)}</ul>;
}
"#;

fn main() {
    set_import_meta_env(vec![
        ("import.meta.env.VITE_API_URL".into(), "\"https://api.example.com\"".into()),
        ("import.meta.env.MODE".into(), "\"development\"".into()),
        ("import.meta.env.DEV".into(), "true".into()),
        (
            "import.meta.env".into(),
            "({\"VITE_API_URL\":\"https://api.example.com\",\"MODE\":\"development\",\"DEV\":true})".into(),
        ),
    ]);

    let opts = CompileOptions::dev();
    let path = Path::new("Widget.tsx");

    let out = compile(path, SRC, &opts).expect("compile");
    assert!(out.code.contains("\"development\""), "define must be replaced");
    assert!(!out.code.contains("import.meta.env"), "no bare import.meta.env left");

    let warmup = 2_000;
    let iters = 40_000;
    for _ in 0..warmup {
        let _ = compile(path, SRC, &opts).unwrap();
    }
    let t = Instant::now();
    for _ in 0..iters {
        let _ = compile(path, SRC, &opts).unwrap();
    }
    let el = t.elapsed();
    println!(
        "{iters} compiles in {el:?}  =  {:.2} us/compile  ({:.0} compiles/sec)",
        el.as_nanos() as f64 / iters as f64 / 1000.0,
        iters as f64 / el.as_secs_f64(),
    );
}
