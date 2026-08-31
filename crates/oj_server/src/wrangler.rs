// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;

#[derive(Default, Debug, PartialEq)]
pub struct WranglerConfig {
    pub compat_date: Option<String>,
    pub compat_flags: Vec<String>,
    pub vars: Vec<(String, String)>,
    pub service_bindings: Vec<(String, String)>,
}

pub fn load(root: &Path) -> WranglerConfig {
    let mut cfg = if let Some(text) = read_first(root, &["wrangler.jsonc", "wrangler.json"]) {
        parse_json(&text)
    } else if let Some(text) = read_first(root, &["wrangler.toml"]) {
        parse_toml(&text)
    } else {
        WranglerConfig::default()
    };
    for (k, v) in load_dev_vars(root) {
        upsert(&mut cfg.vars, k, v);
    }
    cfg
}

fn read_first(root: &Path, names: &[&str]) -> Option<String> {
    for n in names {
        let p = root.join(n);
        if p.is_file() {
            if let Ok(s) = std::fs::read_to_string(&p) {
                return Some(s);
            }
        }
    }
    None
}

fn upsert(vars: &mut Vec<(String, String)>, key: String, value: String) {
    if let Some(slot) = vars.iter_mut().find(|(k, _)| *k == key) {
        slot.1 = value;
    } else {
        vars.push((key, value));
    }
}

fn parse_json(text: &str) -> WranglerConfig {
    let mut cleaned = text.to_string();
    let _ = json_strip_comments::strip(&mut cleaned);
    let Ok(v) = serde_json::from_str::<serde_json::Value>(&cleaned) else {
        return WranglerConfig::default();
    };
    let mut cfg = WranglerConfig {
        compat_date: v
            .get("compatibility_date")
            .and_then(|x| x.as_str())
            .map(str::to_string),
        ..Default::default()
    };
    if let Some(flags) = v.get("compatibility_flags").and_then(|x| x.as_array()) {
        cfg.compat_flags = flags
            .iter()
            .filter_map(|f| f.as_str().map(str::to_string))
            .collect();
    }
    if let Some(vars) = v.get("vars").and_then(|x| x.as_object()) {
        for (k, val) in vars {
            cfg.vars.push((k.clone(), json_scalar(val)));
        }
    }
    if let Some(services) = v.get("services").and_then(|x| x.as_array()) {
        for s in services {
            let binding = s.get("binding").and_then(|x| x.as_str());
            let service = s.get("service").and_then(|x| x.as_str());
            if let (Some(b), Some(svc)) = (binding, service) {
                cfg.service_bindings.push((b.to_string(), svc.to_string()));
            }
        }
    }
    cfg
}

fn json_scalar(v: &serde_json::Value) -> String {
    match v {
        serde_json::Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn parse_toml(text: &str) -> WranglerConfig {
    let mut cfg = WranglerConfig::default();
    let mut in_vars = false;
    for line in text.lines() {
        let t = line.trim();
        if t.starts_with('[') {
            in_vars = t == "[vars]";
            continue;
        }
        if let Some((key, rest)) = t.split_once('=') {
            let key = key.trim();
            let val = rest.trim().trim_matches('"').to_string();
            if in_vars {
                cfg.vars.push((key.to_string(), val));
            } else if key == "compatibility_date" {
                cfg.compat_date = Some(val);
            } else if key == "compatibility_flags" {
                cfg.compat_flags = rest
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
    }
    cfg
}

fn load_dev_vars(root: &Path) -> Vec<(String, String)> {
    let p = root.join(".dev.vars");
    let Ok(text) = std::fs::read_to_string(&p) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for line in text.lines() {
        let t = line.trim();
        if t.is_empty() || t.starts_with('#') {
            continue;
        }
        if let Some((key, rest)) = t.split_once('=') {
            let mut v = rest.trim();
            if (v.starts_with('"') && v.ends_with('"')) || (v.starts_with('\'') && v.ends_with('\'')) {
                v = &v[1..v.len() - 1];
            }
            out.push((key.trim().to_string(), v.to_string()));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_jsonc_with_comments_and_trailing_commas() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("wrangler.jsonc"),
            r#"{
              // the worker
              "compatibility_date": "2026-08-01",
              "compatibility_flags": ["nodejs_compat"],
              "vars": { "EVENTS_API_URL": "https://x", "FLAG": true },
              "services": [
                { "binding": "CONFIDENCE_RESOLVER", "service": "resolver" },
              ],
            }"#,
        )
        .unwrap();
        let cfg = load(dir.path());
        assert_eq!(cfg.compat_date.as_deref(), Some("2026-08-01"));
        assert_eq!(cfg.compat_flags, vec!["nodejs_compat".to_string()]);
        assert!(cfg.vars.contains(&("EVENTS_API_URL".into(), "https://x".into())));
        assert!(cfg.vars.contains(&("FLAG".into(), "true".into())));
        assert_eq!(
            cfg.service_bindings,
            vec![("CONFIDENCE_RESOLVER".to_string(), "resolver".to_string())]
        );
    }

    #[test]
    fn dev_vars_override_wrangler_vars() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("wrangler.json"),
            r#"{"vars":{"TOKEN":"public"}}"#,
        )
        .unwrap();
        std::fs::write(dir.path().join(".dev.vars"), "TOKEN=\"secret\"\n# note\nEXTRA=1\n").unwrap();
        let cfg = load(dir.path());
        assert!(cfg.vars.contains(&("TOKEN".into(), "secret".into())), "{:?}", cfg.vars);
        assert!(cfg.vars.contains(&("EXTRA".into(), "1".into())));
    }

    #[test]
    fn parses_toml_vars_and_compat() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("wrangler.toml"),
            "compatibility_date = \"2026-08-01\"\ncompatibility_flags = [\"nodejs_compat\"]\n[vars]\nA = \"b\"\n",
        )
        .unwrap();
        let cfg = load(dir.path());
        assert_eq!(cfg.compat_date.as_deref(), Some("2026-08-01"));
        assert_eq!(cfg.compat_flags, vec!["nodejs_compat".to_string()]);
        assert!(cfg.vars.contains(&("A".into(), "b".into())));
    }

    #[test]
    fn missing_config_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()), WranglerConfig::default());
    }
}
