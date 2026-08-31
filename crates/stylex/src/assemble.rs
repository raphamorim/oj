// parity: stylex-0.19.0 packages/@stylexjs/babel-plugin/src/index.js processStylexRules

use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::hash::BuildHasherDefault;

use crate::jsrt::{locale_key, utf16_cmp};
use crate::rules::StylexRule;

const LOGICAL_FLOAT_START_VAR: &str = "--stylex-logical-start";
const LOGICAL_FLOAT_END_VAR: &str = "--stylex-logical-end";

pub type Comparator = fn(&str, &str) -> Ordering;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LayersConfig {
    Off,
    On {
        before: Vec<String>,
        after: Vec<String>,
        prefix: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssembleConfig {
    pub use_layers: LayersConfig,
    pub use_legacy_classnames_sort: bool,
    pub legacy_disable_layers: bool,
    pub enable_ltr_rtl_comments: bool,
}

impl Default for AssembleConfig {
    fn default() -> Self {
        AssembleConfig {
            use_layers: LayersConfig::Off,
            use_legacy_classnames_sort: false,
            legacy_disable_layers: false,
            enable_ltr_rtl_comments: false,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum AssembleError {
    // Message text mirrors the upstream throw byte for byte.
    #[error("circular reference detected for constant {0}")]
    CircularConst(String),
    #[error("unsupported processStylexRules config: {0}")]
    Unsupported(String),
    #[error("invalid processStylexRules config: {0}")]
    InvalidConfig(String),
}

impl AssembleConfig {
    // Mirrors the upstream raw-config parse: `boolean` is shorthand for `{useLayers}`,
    // `useLayers !== false` turns layers on, falsy prefix means no prefix.
    pub fn from_json(v: &serde_json::Value) -> Result<Self, AssembleError> {
        use serde_json::Value;
        let obj = match v {
            Value::Bool(b) => {
                return Ok(AssembleConfig {
                    use_layers: if *b {
                        LayersConfig::on_empty()
                    } else {
                        LayersConfig::Off
                    },
                    ..AssembleConfig::default()
                });
            }
            Value::Null => return Ok(AssembleConfig::default()),
            Value::Object(o) => o,
            other => {
                return Err(AssembleError::InvalidConfig(format!(
                    "expected boolean or object, got {other}"
                )));
            }
        };
        let use_layers = match obj.get("useLayers") {
            None | Some(Value::Null) | Some(Value::Bool(false)) => LayersConfig::Off,
            Some(Value::Bool(true)) => LayersConfig::on_empty(),
            Some(Value::Object(l)) => {
                let list = |key: &str| -> Result<Vec<String>, AssembleError> {
                    match l.get(key) {
                        None | Some(Value::Null) => Ok(vec![]),
                        Some(Value::Array(a)) => a
                            .iter()
                            .map(|s| {
                                s.as_str().map(str::to_string).ok_or_else(|| {
                                    AssembleError::InvalidConfig(format!(
                                        "useLayers.{key} entries must be strings"
                                    ))
                                })
                            })
                            .collect(),
                        Some(other) => Err(AssembleError::InvalidConfig(format!(
                            "useLayers.{key} must be an array, got {other}"
                        ))),
                    }
                };
                let prefix = match l.get("prefix") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(s)) => Some(s.clone()),
                    Some(other) => {
                        return Err(AssembleError::InvalidConfig(format!(
                            "useLayers.prefix must be a string, got {other}"
                        )));
                    }
                };
                LayersConfig::On {
                    before: list("before")?,
                    after: list("after")?,
                    prefix,
                }
            }
            Some(other) => {
                return Err(AssembleError::InvalidConfig(format!(
                    "useLayers must be boolean or object, got {other}"
                )));
            }
        };
        Ok(AssembleConfig {
            use_layers,
            use_legacy_classnames_sort: obj.get("useLegacyClassnamesSort").is_some_and(js_truthy),
            legacy_disable_layers: obj.get("legacyDisableLayers").is_some_and(js_truthy),
            enable_ltr_rtl_comments: obj.get("enableLTRRTLComments").is_some_and(js_truthy),
        })
    }
}

impl LayersConfig {
    fn on_empty() -> Self {
        LayersConfig::On {
            before: vec![],
            after: vec![],
            prefix: None,
        }
    }
}

fn js_truthy(v: &serde_json::Value) -> bool {
    use serde_json::Value;
    match v {
        Value::Null => false,
        Value::Bool(b) => *b,
        Value::Number(n) => n.as_f64().is_some_and(|f| f != 0.0 && !f.is_nan()),
        Value::String(s) => !s.is_empty(),
        Value::Array(_) | Value::Object(_) => true,
    }
}

// JS string|number const value; `Other` covers metadata that upstream would
// stringify via String().
#[derive(Clone, Debug, PartialEq)]
enum ConstVal {
    Str(String),
    Num(f64),
    Other(serde_json::Value),
}

impl ConstVal {
    fn from_json(v: &serde_json::Value) -> ConstVal {
        match v {
            serde_json::Value::String(s) => ConstVal::Str(s.clone()),
            serde_json::Value::Number(n) => ConstVal::Num(n.as_f64().unwrap_or(f64::NAN)),
            other => match crate::rules::non_finite_from_tag(other) {
                Some(x) => ConstVal::Num(x),
                None => ConstVal::Other(other.clone()),
            },
        }
    }

    fn to_js_string(&self) -> String {
        match self {
            ConstVal::Str(s) => s.clone(),
            ConstVal::Num(n) => crate::jsrt::js_number_to_string(*n),
            ConstVal::Other(v) => js_string_of_value(v),
        }
    }
}

fn js_string_of_value(v: &serde_json::Value) -> String {
    use serde_json::Value;
    match v {
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => crate::jsrt::js_number_to_string(n.as_f64().unwrap_or(f64::NAN)),
        Value::String(s) => s.clone(),
        // Array.prototype.toString: elements joined by ',', null → ''.
        Value::Array(a) => a
            .iter()
            .map(|e| {
                if e.is_null() {
                    String::new()
                } else {
                    js_string_of_value(e)
                }
            })
            .collect::<Vec<_>>()
            .join(","),
        Value::Object(_) => "[object Object]".to_string(),
    }
}

pub fn assemble(rules: &[StylexRule], cfg: &AssembleConfig) -> Result<String, AssembleError> {
    assemble_impl(rules, cfg, None)
}

pub fn assemble_with_comparator(
    rules: &[StylexRule],
    cfg: &AssembleConfig,
    cmp: Comparator,
) -> Result<String, AssembleError> {
    assemble_impl(rules, cfg, Some(cmp))
}

fn is_const_rule(r: &StylexRule) -> bool {
    r.const_key.is_some() && matches!(&r.const_val, Some(v) if !v.is_null())
}

fn const_decl(r: &StylexRule) -> (String, ConstVal) {
    (
        format!("var(--{})", r.class_name),
        ConstVal::from_json(r.const_val.as_ref().unwrap()),
    )
}

// Insertion-ordered "var(--keyhash)" → value map; duplicate keys keep their
// slot, value last-wins (JS Map.set). Each value is then resolved in place.
fn collect_resolved_consts(
    decls: impl Iterator<Item = (String, ConstVal)>,
) -> Result<Vec<(String, ConstVal)>, AssembleError> {
    let mut consts: Vec<(String, ConstVal)> = Vec::new();
    for (key, val) in decls {
        match consts.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = val,
            None => consts.push((key, val)),
        }
    }
    for i in 0..consts.len() {
        let value = consts[i].1.clone();
        let resolved = resolve_constant(&value, &consts, &mut Vec::new())?;
        consts[i].1 = resolved;
    }
    Ok(consts)
}

fn rule_has_logical_float(r: &StylexRule) -> bool {
    r.ltr.contains(LOGICAL_FLOAT_START_VAR)
        || r.ltr.contains(LOGICAL_FLOAT_END_VAR)
        || r.rtl.as_deref().is_some_and(|t| {
            t.contains(LOGICAL_FLOAT_START_VAR) || t.contains(LOGICAL_FLOAT_END_VAR)
        })
}

fn logical_float_block(has_logical_float: bool) -> String {
    if has_logical_float {
        format!(
            ":root, [dir=\"ltr\"] {{\n  {LOGICAL_FLOAT_START_VAR}: left;\n  \
             {LOGICAL_FLOAT_END_VAR}: right;\n}}\n[dir=\"rtl\"] {{\n  \
             {LOGICAL_FLOAT_START_VAR}: right;\n  {LOGICAL_FLOAT_END_VAR}: left;\n}}\n"
        )
    } else {
        String::new()
    }
}

fn layer_parts(cfg: &AssembleConfig) -> (bool, &[String], &[String], &str) {
    match &cfg.use_layers {
        LayersConfig::On {
            before,
            after,
            prefix,
        } => (true, before, after, prefix.as_deref().unwrap_or("")),
        LayersConfig::Off => (false, &[], &[], ""),
    }
}

fn layer_name(prefix: &str, index: usize) -> String {
    if prefix.is_empty() {
        format!("priority{}", index + 1)
    } else {
        format!("{prefix}.priority{}", index + 1)
    }
}

fn layers_header(cfg: &AssembleConfig, group_count: usize) -> String {
    let (_, before, after, prefix) = layer_parts(cfg);
    let names: Vec<String> = before
        .iter()
        .cloned()
        .chain((0..group_count).map(|i| layer_name(prefix, i)))
        .chain(after.iter().cloned())
        .collect();
    format!("\n@layer {};\n", names.join(", "))
}

// One deduped class's output: the ltr line, or the ltr/rtl pair joined by the
// same '\n' the group join would use.
fn rule_chunk(ltr: &str, rtl: Option<&str>, index: usize, cfg: &AssembleConfig) -> String {
    let use_layers_on = matches!(cfg.use_layers, LayersConfig::On { .. });
    let mut ltr = ltr.to_string();
    let mut rtl = rtl.map(str::to_string);
    if !use_layers_on && !cfg.legacy_disable_layers {
        ltr = add_specificity_level(&ltr, index);
        rtl = rtl.map(|r| add_specificity_level(&r, index));
    }
    ltr = double_theme_classes(&ltr);
    rtl = rtl.map(|r| double_theme_classes(&r));
    match rtl {
        Some(rtl) if cfg.enable_ltr_rtl_comments => {
            format!("/* @ltr begin */{ltr}/* @ltr end */\n/* @rtl begin */{rtl}/* @rtl end */")
        }
        Some(rtl) => format!(
            "{}\n{}",
            add_ancestor_selector(&ltr, "html:not([dir='rtl'])"),
            add_ancestor_selector(&rtl, "html[dir='rtl']")
        ),
        None => ltr,
    }
}

fn compare_rules(
    a: &StylexRule,
    b: &StylexRule,
    cfg: &AssembleConfig,
    cmp: Comparator,
) -> Ordering {
    let diff = a.priority - b.priority;
    if diff != 0.0 {
        return if diff < 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    if cfg.use_legacy_classnames_sort {
        cmp(&a.class_name, &b.class_name)
    } else {
        match cmp(decl_slice(&a.ltr), decl_slice(&b.ltr)) {
            Ordering::Equal => cmp(&a.ltr, &b.ltr),
            other => other,
        }
    }
}

fn assemble_impl(
    rules: &[StylexRule],
    cfg: &AssembleConfig,
    custom_cmp: Option<Comparator>,
) -> Result<String, AssembleError> {
    if rules.is_empty() {
        return Ok(String::new());
    }

    let non_const: Vec<&StylexRule> = rules.iter().filter(|r| !is_const_rule(r)).collect();
    let consts =
        collect_resolved_consts(rules.iter().filter(|r| is_const_rule(r)).map(const_decl))?;
    let prepared_consts = prepare_consts(&consts);

    let mut sorted: Vec<StylexRule> = match custom_cmp {
        Some(cmp) => {
            let mut cloned: Vec<StylexRule> = non_const.iter().map(|r| (*r).clone()).collect();
            cloned.sort_by(|a, b| compare_rules(a, b, cfg, cmp));
            cloned
        }
        None => keyed_sorted_clone(&non_const, cfg.use_legacy_classnames_sort),
    };

    // Logical-float detection reads the pre-substitution rule text.
    let has_logical_float = non_const.iter().any(|r| rule_has_logical_float(r));
    let logical_float_vars = logical_float_block(has_logical_float);

    for rule in &mut sorted {
        if let Some(ltr) = substitute_consts(&rule.ltr, &prepared_consts) {
            rule.ltr = ltr.into();
        }
        if let Some(rtl) = &rule.rtl
            && let Some(rtl_text) = substitute_consts(rtl, &prepared_consts)
        {
            rule.rtl = Some(rtl_text.into());
        }
    }

    // Consecutive runs of floor(priority/1000); input is priority-sorted so runs
    // are the priority bands.
    let mut groups: Vec<Vec<StylexRule>> = Vec::new();
    let mut last_level = -1.0f64;
    for rule in sorted {
        let level = (rule.priority / 1000.0).floor();
        match groups.last_mut() {
            Some(last) if level == last_level => last.push(rule),
            _ => {
                last_level = level;
                groups.push(vec![rule]);
            }
        }
    }

    let (use_layers_on, _, _, layer_prefix) = layer_parts(cfg);
    let header = if use_layers_on {
        layers_header(cfg, groups.len())
    } else {
        String::new()
    };

    let mut group_blocks: Vec<String> = Vec::with_capacity(groups.len());
    for (index, group) in groups.iter().enumerate() {
        let pri = group[0].priority;

        // Last-wins dedupe by className, first-occurrence order (JS Map semantics).
        let mut order: Vec<&str> = Vec::with_capacity(group.len());
        let mut latest: HashMap<&str, &StylexRule> = HashMap::with_capacity(group.len());
        for rule in group {
            if latest.insert(&rule.class_name, rule).is_none() {
                order.push(&rule.class_name);
            }
        }

        let mut chunks: Vec<String> = Vec::with_capacity(order.len());
        for class_name in order {
            let rule = latest[class_name];
            // Empty rtl is falsy upstream and behaves exactly like absent.
            let rtl = rule.rtl.as_deref().filter(|s| !s.is_empty());
            chunks.push(rule_chunk(&rule.ltr, rtl, index, cfg));
        }
        let collected = chunks.join("\n");
        group_blocks.push(if use_layers_on && pri > 0.0 {
            format!(
                "@layer {}{{\n{}\n}}",
                layer_name(layer_prefix, index),
                collected
            )
        } else {
            collected
        });
    }

    Ok(format!(
        "{logical_float_vars}{header}{}",
        group_blocks.join("\n")
    ))
}

// localeCompare levels of one string, laid out [primary n][secondary n]
// [tertiary n]; Fallback marks a char outside the pinned collation alphabet.
enum CollKey {
    Verified { n: usize, buf: Vec<u8> },
    Fallback,
}

impl CollKey {
    fn derive(s: &str) -> CollKey {
        let n = s.chars().count();
        let mut buf = vec![0u8; 3 * n];
        for (i, c) in s.chars().enumerate() {
            match locale_key(c) {
                Ok((p, sec, ter)) => {
                    buf[i] = p;
                    buf[n + i] = sec;
                    buf[2 * n + i] = ter;
                }
                Err(_) => return CollKey::Fallback,
            }
        }
        CollKey::Verified { n, buf }
    }

    fn verified(&self) -> bool {
        matches!(self, CollKey::Verified { .. })
    }

    // Equal primary sequences imply equal char counts, so the secondary and
    // tertiary segment comparisons are always aligned.
    fn cmp_verified(&self, other: &CollKey) -> Ordering {
        match (self, other) {
            (CollKey::Verified { n: na, buf: ba }, CollKey::Verified { n: nb, buf: bb }) => ba
                [..*na]
                .cmp(&bb[..*nb])
                .then_with(|| ba[*na..2 * na].cmp(&bb[*nb..2 * nb]))
                .then_with(|| ba[2 * na..].cmp(&bb[2 * nb..])),
            _ => unreachable!("cmp_verified on a fallback collation key"),
        }
    }

    // default_locale_cmp semantics: either side unverified → UTF-16 order.
    fn cmp_or<'x>(
        &self,
        other: &CollKey,
        fallback: impl FnOnce() -> (&'x str, &'x str),
    ) -> Ordering {
        if self.verified() && other.verified() {
            self.cmp_verified(other)
        } else {
            let (a, b) = fallback();
            utf16_cmp(a, b)
        }
    }
}

enum SortKeys {
    Legacy(CollKey),
    Standard { decl: CollKey, ltr: CollKey },
}

impl SortKeys {
    fn for_rule(r: &StylexRule, legacy: bool) -> SortKeys {
        if legacy {
            SortKeys::Legacy(CollKey::derive(&r.class_name))
        } else {
            SortKeys::Standard {
                decl: CollKey::derive(decl_slice(&r.ltr)),
                ltr: CollKey::derive(&r.ltr),
            }
        }
    }

    fn verified(&self) -> bool {
        match self {
            SortKeys::Legacy(k) => k.verified(),
            SortKeys::Standard { decl, ltr } => decl.verified() && ltr.verified(),
        }
    }

    fn cmp_with_fallback(&self, other: &SortKeys, a: &StylexRule, b: &StylexRule) -> Ordering {
        match (self, other) {
            (SortKeys::Legacy(ka), SortKeys::Legacy(kb)) => {
                ka.cmp_or(kb, || (&a.class_name, &b.class_name))
            }
            (
                SortKeys::Standard { decl: da, ltr: la },
                SortKeys::Standard { decl: db, ltr: lb },
            ) => match da.cmp_or(db, || (decl_slice(&a.ltr), decl_slice(&b.ltr))) {
                Ordering::Equal => la.cmp_or(lb, || (&a.ltr, &b.ltr)),
                other => other,
            },
            _ => unreachable!("mixed sort key kinds"),
        }
    }

    fn cmp_verified(&self, other: &SortKeys) -> Ordering {
        match (self, other) {
            (SortKeys::Legacy(ka), SortKeys::Legacy(kb)) => ka.cmp_verified(kb),
            (
                SortKeys::Standard { decl: da, ltr: la },
                SortKeys::Standard { decl: db, ltr: lb },
            ) => da.cmp_verified(db).then_with(|| la.cmp_verified(lb)),
            _ => unreachable!("mixed sort key kinds"),
        }
    }
}

// Collation keys derived once per rule instead of once per comparison; the
// stable sort and the per-pair fallback keep comparator semantics identical.
fn keyed_sorted_clone(non_const: &[&StylexRule], legacy: bool) -> Vec<StylexRule> {
    let mut entries: Vec<(SortKeys, &StylexRule)> = non_const
        .iter()
        .map(|r| (SortKeys::for_rule(r, legacy), *r))
        .collect();
    entries.sort_by(|(ka, a), (kb, b)| {
        let diff = a.priority - b.priority;
        if diff != 0.0 {
            return if diff < 0.0 {
                Ordering::Less
            } else {
                Ordering::Greater
            };
        }
        ka.cmp_with_fallback(kb, a, b)
    });
    entries.into_iter().map(|(_, r)| r.clone()).collect()
}

// rule.slice(rule.lastIndexOf('{')): lastIndexOf misses → -1 → JS slice(-1) is
// the final character.
fn decl_slice(rule: &str) -> &str {
    match rule.rfind('{') {
        Some(i) => &rule[i..],
        None => rule
            .char_indices()
            .last()
            .map(|(i, _)| &rule[i..])
            .unwrap_or(""),
    }
}

fn resolve_constant(
    value: &ConstVal,
    consts: &[(String, ConstVal)],
    visited: &mut Vec<String>,
) -> Result<ConstVal, AssembleError> {
    let ConstVal::Str(s) = value else {
        return Ok(value.clone());
    };
    let mut result = s.clone();
    let mut scan_from = 0usize;
    while let Some((start, end)) = find_var_ref(&result, scan_from) {
        let ref_name = result[start + 4..end - 1].to_string();
        if visited.contains(&ref_name) {
            return Err(AssembleError::CircularConst(ref_name));
        }
        let ref_key = format!("var({ref_name})");
        let Some((_, ref_val)) = consts.iter().find(|(k, _)| *k == ref_key) else {
            scan_from = end;
            continue;
        };
        visited.push(ref_name.clone());
        let replacement = resolve_constant(&ref_val.clone(), consts, visited)?;
        let needle = result[start..end].to_string();
        result = js_replace_first(&result, &needle, &replacement.to_js_string());
        visited.retain(|v| *v != ref_name);
        scan_from = 0;
    }
    Ok(ConstVal::Str(result))
}

// Scanner for /var\((--[A-Za-z0-9_-]+)\)/ from a byte offset; returns the byte
// span of the whole match.
fn find_var_ref(s: &str, from: usize) -> Option<(usize, usize)> {
    let bytes = s.as_bytes();
    let mut pos = from;
    loop {
        let off = s.get(pos..)?.find("var(--")?;
        let start = pos + off;
        let mut j = start + 6;
        while j < bytes.len() && is_var_ident_byte(bytes[j]) {
            j += 1;
        }
        if j > start + 6 && j < bytes.len() && bytes[j] == b')' {
            return Some((start, j + 1));
        }
        pos = start + 1;
    }
}

fn is_var_ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'-'
}

// The per-const strings substitution needs, derived once per assemble instead
// of once per rule × const.
struct PreparedConst {
    var_ref: String,
    replacement: String,
    key_rewrite: Option<(String, String)>,
}

fn prepare_consts(consts: &[(String, ConstVal)]) -> Vec<PreparedConst> {
    consts
        .iter()
        .map(|(var_ref, const_val)| {
            let replacement = const_val.to_js_string();
            // A const resolving to var(...) also rewrites `--constName:` declaration
            // keys so the target variable stays overridable. Trims are ES TrimString.
            let key_rewrite =
                (replacement.starts_with("var(") && replacement.ends_with(')')).then(|| {
                    let inside = crate::jsrt::js_trim(&replacement[4..replacement.len() - 1]);
                    let target_name = match inside.find(',') {
                        Some(ci) => crate::jsrt::js_trim(&inside[..ci]),
                        None => inside,
                    };
                    let const_name = &var_ref[4..var_ref.len() - 1];
                    (format!("{const_name}:"), format!("{target_name}:"))
                });
            PreparedConst {
                var_ref: var_ref.clone(),
                replacement,
                key_rewrite,
            }
        })
        .collect()
}

// The contains gates only skip js_replace_all calls that would be identity
// copies; a hit re-probes for var(-- because replacements can add/remove refs.
/// `None` when no const reference matched: callers keep sharing the rule's
/// Arc'd text instead of materializing an identical String.
fn substitute_consts(text: &str, consts: &[PreparedConst]) -> Option<String> {
    let mut out: Option<String> = None;
    let mut has_ref = text.contains("var(--");
    for c in consts {
        let cur = out.as_deref().unwrap_or(text);
        if has_ref && cur.contains(c.var_ref.as_str()) {
            let next = js_replace_all(cur, &c.var_ref, &c.replacement);
            has_ref = next.contains("var(--");
            out = Some(next);
        }
        let cur = out.as_deref().unwrap_or(text);
        if let Some((needle, target)) = &c.key_rewrite
            && cur.contains(needle.as_str())
        {
            let next = js_replace_all(cur, needle, target);
            has_ref = next.contains("var(--");
            out = Some(next);
        }
    }
    out
}

// JS GetSubstitution for string-pattern replace/replaceAll: only $$, $&, $`, $'
// are active (no capture groups exist).
fn js_substitution(out: &mut Vec<u8>, full: &str, pos: usize, matched: &str, replacement: &str) {
    let rep = replacement.as_bytes();
    let mut i = 0;
    while i < rep.len() {
        if rep[i] == b'$' && i + 1 < rep.len() {
            match rep[i + 1] {
                b'$' => {
                    out.push(b'$');
                    i += 2;
                    continue;
                }
                b'&' => {
                    out.extend_from_slice(matched.as_bytes());
                    i += 2;
                    continue;
                }
                b'`' => {
                    out.extend_from_slice(&full.as_bytes()[..pos]);
                    i += 2;
                    continue;
                }
                b'\'' => {
                    out.extend_from_slice(&full.as_bytes()[pos + matched.len()..]);
                    i += 2;
                    continue;
                }
                _ => {}
            }
        }
        out.push(rep[i]);
        i += 1;
    }
}

fn js_replace_first(s: &str, needle: &str, replacement: &str) -> String {
    let Some(pos) = s.find(needle) else {
        return s.to_string();
    };
    let mut out = Vec::with_capacity(s.len());
    out.extend_from_slice(&s.as_bytes()[..pos]);
    js_substitution(&mut out, s, pos, needle, replacement);
    out.extend_from_slice(&s.as_bytes()[pos + needle.len()..]);
    String::from_utf8(out).expect("byte-splices preserve UTF-8")
}

fn js_replace_all(s: &str, needle: &str, replacement: &str) -> String {
    debug_assert!(!needle.is_empty());
    let mut out = Vec::with_capacity(s.len());
    let mut pos = 0;
    while let Some(off) = s[pos..].find(needle) {
        let at = pos + off;
        out.extend_from_slice(&s.as_bytes()[pos..at]);
        js_substitution(&mut out, s, at, needle, replacement);
        pos = at + needle.len();
    }
    out.extend_from_slice(&s.as_bytes()[pos..]);
    String::from_utf8(out).expect("byte-splices preserve UTF-8")
}

// :not(#\#) polyfill; inserted before the first '::' when present, else before
// the last '{'; @keyframes exempt (only @keyframes — @property etc. are not).
fn add_specificity_level(selector: &str, index: usize) -> String {
    if selector.starts_with("@keyframes") {
        return selector.to_string();
    }
    let pseudo = ":not(#\\#)".repeat(index);
    let split_at = match selector.find("::") {
        Some(i) => i,
        None => match selector.rfind('{') {
            Some(i) => i,
            // lastIndexOf miss → -1 → JS slice(0,-1) / slice(-1) split before the
            // final character.
            None => selector.char_indices().last().map(|(i, _)| i).unwrap_or(0),
        },
    };
    format!(
        "{}{}{}",
        &selector[..split_at],
        pseudo,
        &selector[split_at..]
    )
}

fn add_ancestor_selector(selector: &str, ancestor_selector: &str) -> String {
    if selector.starts_with("@keyframes") {
        return selector.to_string();
    }
    if !selector.starts_with('@') {
        return format!("{ancestor_selector} {selector}");
    }
    let last_at_rule = selector.rfind('@').unwrap_or(0);
    match selector[last_at_rule..].find('{') {
        Some(off) => {
            let bracket = last_at_rule + off;
            format!(
                "{}{} {}",
                &selector[..bracket + 1],
                ancestor_selector,
                &selector[bracket + 1..]
            )
        }
        // indexOf miss → -1 → slice(0,0) prefix and the whole string as rest.
        None => format!("{ancestor_selector} {selector}"),
    }
}

// /\.([a-zA-Z0-9]+), \.([a-zA-Z0-9]+):root/g → '.$1.$1, .$1.$1:root' — the
// second class is intentionally overwritten by the first, as upstream does.
fn double_theme_classes(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(s.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'.'
            && let Some((end, class_end)) = match_theme_pair(bytes, i)
        {
            let class = &s[i + 1..class_end];
            out.extend_from_slice(format!(".{class}.{class}, .{class}.{class}:root").as_bytes());
            i = end;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8(out).expect("ASCII-anchored splices preserve UTF-8")
}

fn match_theme_pair(bytes: &[u8], start: usize) -> Option<(usize, usize)> {
    let mut j = start + 1;
    while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
        j += 1;
    }
    if j == start + 1 {
        return None;
    }
    let class_end = j;
    if !bytes[j..].starts_with(b", .") {
        return None;
    }
    j += 3;
    let second_start = j;
    while j < bytes.len() && bytes[j].is_ascii_alphanumeric() {
        j += 1;
    }
    if j == second_start || !bytes[j..].starts_with(b":root") {
        return None;
    }
    Some((j + 5, class_end))
}

// The one comparator shared with pseudo sorting (jsrt::default_locale_cmp);
// re-exported so existing callers keep their import path.
pub use crate::jsrt::default_locale_cmp;

fn class_hash64(s: &str) -> u64 {
    let mut h = 0xcbf29ce484222325u64;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// Hash values are precomputed 64-bit class hashes; pass them through verbatim.
#[derive(Default)]
struct IdentityHasher(u64);

impl std::hash::Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 = (self.0 << 8) | u64::from(b);
        }
    }
    fn write_u64(&mut self, x: u64) {
        self.0 = x;
    }
}

struct PreparedRule {
    class_name: std::sync::Arc<str>,
    class_hash: u64,
    priority: f64,
    keys: SortKeys,
    // ltr/rtl carry the const substitutions; chunk memoizes the emitted text
    // per group index.
    ltr: String,
    rtl: Option<String>,
    chunk: Option<(usize, String)>,
}

struct FilePrep {
    rules: Vec<PreparedRule>,
    const_decls: Vec<(String, ConstVal)>,
    // safe = every sort key verified and every priority finite: the exact
    // preconditions under which sorted-by-key order equals the stable sort.
    safe: bool,
    logical_float: bool,
}

fn prepare_file(rules: &[StylexRule], legacy: bool, consts: &[PreparedConst]) -> FilePrep {
    let mut prep = FilePrep {
        rules: Vec::with_capacity(rules.len()),
        const_decls: Vec::new(),
        safe: true,
        logical_float: false,
    };
    for r in rules {
        if is_const_rule(r) {
            prep.const_decls.push(const_decl(r));
            continue;
        }
        prep.logical_float |= rule_has_logical_float(r);
        let keys = SortKeys::for_rule(r, legacy);
        prep.safe &= r.priority.is_finite() && keys.verified();
        let ltr = substitute_consts(&r.ltr, consts).unwrap_or_else(|| r.ltr.to_string());
        let rtl = r
            .rtl
            .as_ref()
            .map(|t| substitute_consts(t, consts).unwrap_or_else(|| t.to_string()));
        prep.rules.push(PreparedRule {
            class_name: r.class_name.clone(),
            class_hash: class_hash64(&r.class_name),
            priority: r.priority,
            keys,
            ltr,
            rtl,
            chunk: None,
        });
    }
    prep
}

// Strict total order = comparator order + the (file, seq) input position; with
// safe files this reproduces the one-shot stable sort exactly.
fn entry_cmp(files: &[FilePrep], a: (u32, u32), b: (u32, u32)) -> Ordering {
    let pa = &files[a.0 as usize].rules[a.1 as usize];
    let pb = &files[b.0 as usize].rules[b.1 as usize];
    let diff = pa.priority - pb.priority;
    if diff != 0.0 {
        return if diff < 0.0 {
            Ordering::Less
        } else {
            Ordering::Greater
        };
    }
    pa.keys.cmp_verified(&pb.keys).then_with(|| a.cmp(&b))
}

// Per-config incremental state: prepared rules, resolved consts, and the
// maintained global sort order that make a single-file swap re-emit O(rules).
struct IncrState {
    cfg: AssembleConfig,
    dirty: BTreeSet<String>,
    structural: bool,
    consts: Vec<(String, ConstVal)>,
    prepared_consts: Vec<PreparedConst>,
    paths: Vec<String>,
    files: Vec<FilePrep>,
    sorted: Vec<(u32, u32)>,
}

const MAX_INCR_STATES: usize = 4;

// Incremental assembly for the HMR path (design-core.md §7): file-keyed rule
// sets, canonical emission order = BTreeMap file order, memoized full assemble.
#[derive(Default)]
pub struct RuleRegistry {
    files: BTreeMap<String, Vec<StylexRule>>,
    generation: u64,
    cache: Option<(u64, AssembleConfig, Result<String, AssembleError>)>,
    incr: Vec<IncrState>,
}

impl RuleRegistry {
    pub fn new() -> Self {
        RuleRegistry::default()
    }

    pub fn set_file_rules(&mut self, file: &str, rules: Vec<StylexRule>) {
        if self.files.get(file).is_some_and(|old| *old == rules) {
            return;
        }
        let existed = self.files.insert(file.to_string(), rules).is_some();
        for state in &mut self.incr {
            if existed {
                state.dirty.insert(file.to_string());
            } else {
                state.structural = true;
            }
        }
        self.generation += 1;
    }

    pub fn remove_file(&mut self, file: &str) -> bool {
        let removed = self.files.remove(file).is_some();
        if removed {
            for state in &mut self.incr {
                state.structural = true;
            }
            self.generation += 1;
        }
        removed
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    // The one-shot-equivalent rule list: files in path order, rules in file order.
    pub fn all_rules(&self) -> Vec<StylexRule> {
        self.files.values().flatten().cloned().collect()
    }

    pub fn emit(&mut self, cfg: &AssembleConfig) -> Result<String, AssembleError> {
        if let Some((generation, cached_cfg, result)) = &self.cache
            && *generation == self.generation
            && cached_cfg == cfg
        {
            return result.clone();
        }
        let result = match self.try_incremental(cfg) {
            Some(css) => {
                // Debug cross-check: the incremental path must match one-shot output.
                #[cfg(debug_assertions)]
                {
                    let full = assemble(&self.all_rules(), cfg);
                    debug_assert_eq!(
                        full.as_deref(),
                        Ok(css.as_str()),
                        "incremental emit diverged from one-shot assemble"
                    );
                }
                Ok(css)
            }
            None => {
                let result = assemble(&self.all_rules(), cfg);
                self.rebuild_incr_state(cfg);
                result
            }
        };
        self.cache = Some((self.generation, cfg.clone(), result.clone()));
        result
    }

    // The fast path: re-prepare only the swapped files and splice them into the
    // cached sort order. None = fall back to the one-shot path.
    fn try_incremental(&mut self, cfg: &AssembleConfig) -> Option<String> {
        let Self { files, incr, .. } = self;
        let state = incr.iter_mut().find(|s| s.cfg == *cfg)?;
        if state.structural {
            return None;
        }

        let mut raw_consts: Vec<(String, ConstVal)> = Vec::new();
        for (rank, path) in state.paths.iter().enumerate() {
            if state.dirty.contains(path) {
                let rules = files.get(path)?;
                raw_consts.extend(rules.iter().filter(|r| is_const_rule(r)).map(const_decl));
            } else {
                raw_consts.extend(state.files[rank].const_decls.iter().cloned());
            }
        }
        let resolved = collect_resolved_consts(raw_consts.into_iter()).ok()?;
        if resolved != state.consts {
            return None;
        }

        let dirty: Vec<String> = std::mem::take(&mut state.dirty).into_iter().collect();
        for path in &dirty {
            let rank = state.paths.binary_search(path).ok()? as u32;
            let prep = prepare_file(
                files.get(path)?,
                state.cfg.use_legacy_classnames_sort,
                &state.prepared_consts,
            );
            if !prep.safe {
                return None;
            }
            state.files[rank as usize] = prep;
            let mut fresh: Vec<(u32, u32)> = (0..state.files[rank as usize].rules.len())
                .map(|seq| (rank, seq as u32))
                .collect();
            fresh.sort_unstable_by(|&a, &b| entry_cmp(&state.files, a, b));
            state.sorted = merge_sorted(&state.files, &state.sorted, rank, fresh);
        }

        if files.values().all(|rules| rules.is_empty()) {
            return Some(String::new());
        }
        Some(render(state))
    }

    // Full rebuild of the per-config state; drops it instead when the rule set
    // is outside the safe preconditions (or consts fail to resolve).
    fn rebuild_incr_state(&mut self, cfg: &AssembleConfig) {
        self.incr.retain(|s| s.cfg != *cfg);
        let raw = self
            .files
            .values()
            .flat_map(|rules| rules.iter().filter(|r| is_const_rule(r)).map(const_decl));
        let Ok(consts) = collect_resolved_consts(raw) else {
            return;
        };
        let prepared_consts = prepare_consts(&consts);
        let mut paths = Vec::with_capacity(self.files.len());
        let mut fps = Vec::with_capacity(self.files.len());
        for (path, rules) in &self.files {
            let prep = prepare_file(rules, cfg.use_legacy_classnames_sort, &prepared_consts);
            if !prep.safe {
                return;
            }
            paths.push(path.clone());
            fps.push(prep);
        }
        let mut sorted: Vec<(u32, u32)> = fps
            .iter()
            .enumerate()
            .flat_map(|(rank, f)| (0..f.rules.len()).map(move |seq| (rank as u32, seq as u32)))
            .collect();
        sorted.sort_unstable_by(|&a, &b| entry_cmp(&fps, a, b));
        if self.incr.len() >= MAX_INCR_STATES {
            self.incr.remove(0);
        }
        self.incr.push(IncrState {
            cfg: cfg.clone(),
            dirty: BTreeSet::new(),
            structural: false,
            consts,
            prepared_consts,
            paths,
            files: fps,
            sorted,
        });
    }
}

// old minus `rank`'s stale entries, merged with that file's fresh pre-sorted
// entries by binary-searched insertion points (strict entry_cmp total order).
fn merge_sorted(
    files: &[FilePrep],
    old: &[(u32, u32)],
    rank: u32,
    fresh: Vec<(u32, u32)>,
) -> Vec<(u32, u32)> {
    let kept: Vec<(u32, u32)> = old.iter().copied().filter(|e| e.0 != rank).collect();
    let mut merged = Vec::with_capacity(kept.len() + fresh.len());
    let mut prev = 0usize;
    for &candidate in &fresh {
        let at = prev
            + kept[prev..].partition_point(|&e| entry_cmp(files, e, candidate) == Ordering::Less);
        merged.extend_from_slice(&kept[prev..at]);
        merged.push(candidate);
        prev = at;
    }
    merged.extend_from_slice(&kept[prev..]);
    merged
}

// First slot inline: an empty Vec never allocates, so only a real 64-bit hash
// collision costs a heap allocation.
type SlotMap = HashMap<u64, (u32, Vec<u32>), BuildHasherDefault<IdentityHasher>>;

fn render(state: &mut IncrState) -> String {
    let IncrState {
        cfg, files, sorted, ..
    } = state;

    struct GroupMeta {
        first_priority: f64,
        emit: Vec<(u32, u32)>,
    }
    let mut groups: Vec<GroupMeta> = Vec::new();
    let mut slots: Vec<(u32, u32)> = Vec::new();
    let mut slot_map: SlotMap = SlotMap::default();
    let mut last_band = 0.0f64;
    let mut first_priority = 0.0f64;
    let mut open = false;
    for &(rank, seq) in sorted.iter() {
        let p = &files[rank as usize].rules[seq as usize];
        let band = (p.priority / 1000.0).floor();
        if !open || band != last_band {
            if open {
                groups.push(GroupMeta {
                    first_priority,
                    emit: std::mem::take(&mut slots),
                });
            }
            slot_map.clear();
            open = true;
            last_band = band;
            first_priority = p.priority;
        }
        // JS Map semantics: a repeated class keeps its first slot, latest value wins.
        match slot_map.entry(p.class_hash) {
            std::collections::hash_map::Entry::Vacant(vacant) => {
                vacant.insert((slots.len() as u32, Vec::new()));
                slots.push((rank, seq));
            }
            std::collections::hash_map::Entry::Occupied(mut occupied) => {
                let same_class = |&si: &u32| {
                    let (er, es) = slots[si as usize];
                    files[er as usize].rules[es as usize].class_name == p.class_name
                };
                let (first, rest) = occupied.get_mut();
                let existing = std::iter::once(*first).find(|si| same_class(si));
                let existing = existing.or_else(|| rest.iter().copied().find(|si| same_class(si)));
                match existing {
                    Some(si) => slots[si as usize] = (rank, seq),
                    None => {
                        rest.push(slots.len() as u32);
                        slots.push((rank, seq));
                    }
                }
            }
        }
    }
    if open {
        groups.push(GroupMeta {
            first_priority,
            emit: slots,
        });
    }

    let (use_layers_on, _, _, layer_prefix) = layer_parts(cfg);
    let header = if use_layers_on {
        layers_header(cfg, groups.len())
    } else {
        String::new()
    };
    let logical = logical_float_block(files.iter().any(|f| f.logical_float));

    let mut out = String::with_capacity(logical.len() + header.len() + 64 * sorted.len().min(8192));
    out.push_str(&logical);
    out.push_str(&header);
    for (index, group) in groups.iter().enumerate() {
        if index > 0 {
            out.push('\n');
        }
        let wrap = use_layers_on && group.first_priority > 0.0;
        if wrap {
            out.push_str("@layer ");
            out.push_str(&layer_name(layer_prefix, index));
            out.push_str("{\n");
        }
        for (j, &(rank, seq)) in group.emit.iter().enumerate() {
            if j > 0 {
                out.push('\n');
            }
            let p = &mut files[rank as usize].rules[seq as usize];
            if !matches!(&p.chunk, Some((i, _)) if *i == index) {
                let rtl = p.rtl.as_deref().filter(|s| !s.is_empty());
                let chunk = rule_chunk(&p.ltr, rtl, index, cfg);
                p.chunk = Some((index, chunk));
            }
            out.push_str(&p.chunk.as_ref().expect("chunk just memoized").1);
        }
        if wrap {
            out.push_str("\n}");
        }
    }
    out
}
