//! `%KEY%` substitution runs over an `index.html` nobody validated, against a
//! map derived from a `.env` nobody validated. It must be total, must never
//! substitute a key it was not given, and must never re-scan what it produced.
#![no_main]

use std::collections::BTreeMap;

use libfuzzer_sys::fuzz_target;
use oj_env::replace_html_env;

fuzz_target!(|data: &[u8]| {
    let Ok(html) = std::str::from_utf8(data) else {
        return;
    };

    // With nothing to substitute, substitution is the identity.
    assert_eq!(replace_html_env(html, &BTreeMap::new()), html);

    let mut env = BTreeMap::new();
    env.insert("KNOWN".to_string(), "value".to_string());
    env.insert("RECURSIVE".to_string(), "%KNOWN%".to_string());
    env.insert("EMPTY".to_string(), String::new());
    let out = replace_html_env(html, &env);

    // Only the keys in the map may have been consumed.
    let substitutions = html.matches("%KNOWN%").count()
        + html.matches("%RECURSIVE%").count()
        + html.matches("%EMPTY%").count();
    if substitutions == 0 {
        assert_eq!(out, html, "substituted something that was not in the map");
    }
    // A substituted value is never scanned again.
    if html.contains("%RECURSIVE%") {
        assert!(
            out.contains("%KNOWN%"),
            "a substituted value was expanded again:\n{out}"
        );
    }
    // Substitution is a fixed point of itself once the placeholders are gone.
    let twice = replace_html_env(&out, &env);
    if !out.contains("%KNOWN%") && !out.contains("%RECURSIVE%") && !out.contains("%EMPTY%") {
        assert_eq!(twice, out, "not idempotent");
    }
});
