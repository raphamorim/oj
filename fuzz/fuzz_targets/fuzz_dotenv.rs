//! `.env` files are untrusted text that a build reads without asking, and the
//! defines derived from them are shipped to the browser. Parsing must be total,
//! and confinement -- no unprefixed variable in anything the client sees -- must
//! hold for every input.
#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use oj_env::{html_env_map, import_meta_env_defines, parse};

fuzz_target!(|data: &[u8]| {
    let Ok(contents) = std::str::from_utf8(data) else {
        return;
    };
    let mut base = BTreeMap::new();
    base.insert("PRESET".to_string(), "preset-value".to_string());

    let loaded = parse(contents, &base);
    for (key, _) in &loaded {
        assert!(!key.is_empty(), "empty key");
        assert_eq!(key.trim(), key, "untrimmed key: {key:?}");
        assert!(!key.contains('='), "key holds an '=': {key:?}");
        assert!(!key.contains('\n'), "key holds a newline: {key:?}");
    }

    let prefix = "VITE_";
    let defines = import_meta_env_defines(&loaded, "production", false, "/", &[prefix]);

    // The aggregate object is always valid JSON, and its keys are exactly the
    // individual defines.
    let aggregate = defines
        .iter()
        .find(|(k, _)| k == "import.meta.env")
        .map(|(_, v)| v.clone())
        .expect("aggregate define");
    let json: serde_json::Value = serde_json::from_str(&aggregate).expect("valid JSON object");
    let object = json.as_object().expect("object");
    let mut individual: Vec<&str> = defines
        .iter()
        .filter_map(|(k, _)| k.strip_prefix("import.meta.env."))
        .collect();
    individual.sort_unstable();
    let mut aggregated: Vec<&str> = object.keys().map(String::as_str).collect();
    aggregated.sort_unstable();
    assert_eq!(individual, aggregated, "defines disagree with the object");

    // Confinement: only builtins and prefixed variables may appear.
    const BUILTINS: [&str; 5] = ["MODE", "BASE_URL", "DEV", "PROD", "SSR"];
    for key in object.keys() {
        assert!(
            key.starts_with(prefix) || BUILTINS.contains(&key.as_str()),
            "unprefixed variable exposed: {key:?}"
        );
    }
    // A variable in the file but not in the defines must not appear by value
    // either, unless some prefixed variable legitimately holds the same text.
    let public: Vec<&String> = loaded
        .iter()
        .filter(|(k, _)| k.starts_with(prefix))
        .map(|(_, v)| v)
        .collect();
    for (key, value) in &loaded {
        if key.starts_with(prefix) || value.is_empty() {
            continue;
        }
        if public.iter().any(|v| *v == value) {
            continue;
        }
        assert!(
            !aggregate.contains(value.as_str()),
            "value of {key:?} leaked into the defines"
        );
    }

    // The html substitution map round trips the string values.
    let env = html_env_map(&defines);
    for (key, value) in &loaded {
        if key.starts_with(prefix) {
            assert_eq!(env.get(key.as_str()), Some(value), "{key:?} not recoverable");
        }
    }
});
