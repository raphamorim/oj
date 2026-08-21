//! A `.oj-cache` entry is a file on disk that another process, another oj
//! version, or a crash mid-write produced. Reading one must never panic, and
//! whatever it deserializes into must round trip byte for byte.
#![no_main]

use libfuzzer_sys::fuzz_target;
use oj_cache::CachedModule;

fuzz_target!(|data: &[u8]| {
    let Ok(entry) = serde_json::from_slice::<CachedModule>(data) else {
        return;
    };
    // Anything the cache accepts, it must be able to write back and re-read
    // unchanged: a lossy round trip would serve one module and cache another.
    let bytes = serde_json::to_vec(&entry).expect("an entry always serializes");
    let again: CachedModule = serde_json::from_slice(&bytes).expect("round trip");
    assert_eq!(entry, again, "entry changed across a round trip");
});
