// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! Vite-compatible `.env` loading and `import.meta.env` define construction.
//!
//! Loads `.env`, `.env.local`, `.env.<mode>`, `.env.<mode>.local` in Vite's
//! precedence order (later wins), parses dotenv syntax with `${VAR}`
//! expansion, then exposes only `envPrefix`-prefixed vars (default `VITE_`)
//! to client code, alongside the built-ins MODE/BASE_URL/DEV/PROD/SSR.

use std::collections::BTreeMap;
use std::path::Path;

/// Parse one `.env` file's contents into ordered key/value pairs, resolving
/// `${VAR}` / `$VAR` against `base` (earlier vars + process env) as it goes.
pub fn parse(contents: &str, base: &BTreeMap<String, String>) -> Vec<(String, String)> {
    let mut acc = base.clone();
    let mut out = Vec::new();
    for raw in contents.lines() {
        let line = raw.trim_start();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let line = line.strip_prefix("export ").unwrap_or(line);
        let Some((key, rest)) = line.split_once('=') else { continue };
        let key = key.trim();
        if key.is_empty() {
            continue;
        }
        let value = parse_value(rest.trim(), &acc);
        acc.insert(key.to_string(), value.clone());
        out.push((key.to_string(), value));
    }
    out
}

fn parse_value(raw: &str, vars: &BTreeMap<String, String>) -> String {
    let bytes = raw.as_bytes();
    if bytes.first() == Some(&b'\'') {
        // Single-quoted: literal, no expansion. Take up to the closing quote.
        let inner = &raw[1..];
        return inner.split_once('\'').map(|(v, _)| v).unwrap_or(inner).to_string();
    }
    if bytes.first() == Some(&b'"') {
        // Double-quoted: interpret escapes, expand vars.
        let inner = &raw[1..];
        let inner = inner.split_once('"').map(|(v, _)| v).unwrap_or(inner);
        let unescaped = inner.replace("\\n", "\n").replace("\\t", "\t").replace("\\\"", "\"");
        return expand(&unescaped, vars);
    }
    // Unquoted: strip an inline `#` comment, trim, expand.
    let end = raw.find(" #").unwrap_or(raw.len());
    expand(raw[..end].trim(), vars)
}

/// Expand `${VAR}` and `$VAR`; `\$` is a literal `$`.
fn expand(input: &str, vars: &BTreeMap<String, String>) -> String {
    let mut out = String::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if c == b'\\' && bytes.get(i + 1) == Some(&b'$') {
            out.push('$');
            i += 2;
            continue;
        }
        if c == b'$' {
            let (name, next) = if bytes.get(i + 1) == Some(&b'{') {
                match input[i + 2..].find('}') {
                    Some(rel) => (&input[i + 2..i + 2 + rel], i + 2 + rel + 1),
                    None => (&input[i + 1..i + 1], i + 1),
                }
            } else {
                let start = i + 1;
                let mut end = start;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric() || bytes[end] == b'_')
                {
                    end += 1;
                }
                (&input[start..end], end)
            };
            if !name.is_empty() {
                out.push_str(vars.get(name).map(String::as_str).unwrap_or(""));
                i = next;
                continue;
            }
        }
        out.push(c as char);
        i += 1;
    }
    out
}

/// Load and merge the four env files for `mode` from `dir`. Process env is
/// the expansion base but is not emitted (only file-declared vars are).
pub fn load(dir: &Path, mode: &str) -> Vec<(String, String)> {
    let mut base: BTreeMap<String, String> = std::env::vars().collect();
    let mut merged: BTreeMap<String, String> = BTreeMap::new();
    // Lowest to highest precedence; later overrides earlier.
    for name in [".env", ".env.local", &format!(".env.{mode}"), &format!(".env.{mode}.local")] {
        let Ok(contents) = std::fs::read_to_string(dir.join(name)) else { continue };
        for (k, v) in parse(&contents, &base) {
            base.insert(k.clone(), v.clone());
            merged.insert(k, v);
        }
    }
    merged.into_iter().collect()
}

/// Build the `import.meta.env.*` define pairs for the compiler: the built-ins
/// plus every `prefix`-prefixed loaded var, plus a whole-object replacement
/// for bare `import.meta.env`. Values are JS expressions (JSON-encoded).
pub fn import_meta_env_defines(
    loaded: &[(String, String)],
    mode: &str,
    dev: bool,
    base_url: &str,
    prefix: &str,
) -> Vec<(String, String)> {
    let mut obj = serde_json::Map::new();
    obj.insert("MODE".into(), mode.into());
    obj.insert("BASE_URL".into(), base_url.into());
    obj.insert("DEV".into(), dev.into());
    obj.insert("PROD".into(), (!dev).into());
    obj.insert("SSR".into(), false.into());
    for (k, v) in loaded {
        if k.starts_with(prefix) {
            obj.insert(k.clone(), serde_json::Value::String(v.clone()));
        }
    }

    let mut defines = Vec::new();
    for (k, v) in &obj {
        defines.push((format!("import.meta.env.{k}"), v.to_string()));
    }
    // Bare `import.meta.env`: the whole object.
    defines.push(("import.meta.env".into(), serde_json::Value::Object(obj).to_string()));
    defines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> BTreeMap<String, String> {
        BTreeMap::new()
    }

    #[test]
    fn parses_basic_quotes_and_comments() {
        let src = "# comment\nexport A=1\nB=\"two words\"\nC='literal $A'\nD=trailing # note\n";
        let v = parse(src, &base());
        assert_eq!(v, vec![
            ("A".into(), "1".into()),
            ("B".into(), "two words".into()),
            ("C".into(), "literal $A".into()),
            ("D".into(), "trailing".into()),
        ]);
    }

    #[test]
    fn expands_prior_vars_but_not_in_single_quotes() {
        let v = parse("HOST=example.com\nURL=https://${HOST}/api\nRAW='${HOST}'\n", &base());
        assert_eq!(v[1], ("URL".into(), "https://example.com/api".into()));
        assert_eq!(v[2], ("RAW".into(), "${HOST}".into()));
    }

    #[test]
    fn escaped_dollar_is_literal() {
        let v = parse("PRICE=\"\\$5\"\n", &base());
        assert_eq!(v[0], ("PRICE".into(), "$5".into()));
    }

    #[test]
    fn defines_include_builtins_and_only_prefixed_vars() {
        let loaded = vec![
            ("VITE_API".into(), "https://api.test".into()),
            ("SECRET".into(), "nope".into()),
        ];
        let d = import_meta_env_defines(&loaded, "development", true, "/", "VITE_");
        let map: std::collections::HashMap<_, _> = d.iter().cloned().collect();
        assert_eq!(map["import.meta.env.MODE"], "\"development\"");
        assert_eq!(map["import.meta.env.DEV"], "true");
        assert_eq!(map["import.meta.env.PROD"], "false");
        assert_eq!(map["import.meta.env.VITE_API"], "\"https://api.test\"");
        assert!(!map.contains_key("import.meta.env.SECRET"), "unprefixed var must not leak");
        // The whole-object replacement carries the prefixed var, not the secret.
        assert!(map["import.meta.env"].contains("VITE_API"));
        assert!(!map["import.meta.env"].contains("SECRET"));
    }

    #[test]
    fn expansion_boundary_undefined_base_and_unclosed_brace() {
        let mut b = base();
        b.insert("FROM_ENV".into(), "envval".into()); // stands in for a process-env var
        let src = "A=first\n\
                   UNBRACED=$A/x\n\
                   MISSING=[$NOPE]\n\
                   FROMBASE=${FROM_ENV}\n\
                   UNCLOSED=${OOPS\n";
        let map: std::collections::HashMap<_, _> = parse(src, &b).into_iter().collect();
        // unbraced $A expands and stops at the non-identifier '/'
        assert_eq!(map["UNBRACED"], "first/x");
        // an undefined variable expands to the empty string
        assert_eq!(map["MISSING"], "[]");
        // a base (process env) variable is available to expansion
        assert_eq!(map["FROMBASE"], "envval");
        // an unterminated ${ is emitted literally rather than swallowing the rest
        assert_eq!(map["UNCLOSED"], "${OOPS");
    }

    #[test]
    fn missing_files_yield_empty() {
        let d = std::env::temp_dir().join("oj-env-none");
        assert!(load(&d, "development").is_empty());
    }
}
