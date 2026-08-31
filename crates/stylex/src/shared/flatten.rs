//! Raw style namespace → flat `[key, PreRule]` entries, plus basic validation.
// parity: babel-plugin src/shared/preprocess-rules/{flatten-raw-style-obj,basic-validation,PreRule}.js

use crate::errors::StylexError;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::options::ResolvedOptions;
use crate::shared::generate_rule::{CompiledDecl, convert_style_to_class_name};
use crate::shared::media_query::{MediaQueryError, is_media_key, last_media_query_wins_transform};
use crate::shared::resolution::flat_map_expanded_shorthands;
use std::borrow::Cow;
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq)]
pub enum StyleScalar<'a> {
    Str(Cow<'a, str>),
    Num(f64),
    /// A legacy shorthand whose split produced no part for this side; it
    /// renders as an empty declaration instead of throwing on an empty value.
    Undefined,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreRuleValue<'a> {
    Single(StyleScalar<'a>),
    Multi(Vec<StyleScalar<'a>>),
}

#[derive(Clone, Debug, PartialEq)]
pub enum PreRule<'a> {
    Null,
    Rule {
        property: Cow<'a, str>,
        value: PreRuleValue<'a>,
        key_path: Vec<Cow<'a, str>>,
    },
    Set(Vec<PreRule<'a>>),
}

/// One compiled slot: `None` mirrors upstream's `null` ComputedStyle; the
/// second element is the classesToOriginalPath keyPath for that class.
pub type ComputedStyle<'a, 'b> = Option<(CompiledDecl, &'b [Cow<'a, str>])>;

/// The property-value branch's per-property (condition, rule) groupings.
type ConditionGroups<'a> = Vec<(Cow<'a, str>, Vec<(&'a str, PreRule<'a>)>)>;

impl<'a> PreRule<'a> {
    // parity: PreRule.js PreRuleSet.create
    fn create_set(rules: Vec<PreRule<'a>>) -> PreRule<'a> {
        let mut flat: Vec<PreRule> = Vec::with_capacity(rules.len());
        for rule in rules {
            match rule {
                PreRule::Set(inner) => flat.extend(inner),
                other => flat.push(other),
            }
        }
        match flat.len() {
            0 => PreRule::Null,
            1 => flat.into_iter().next().expect("len checked"),
            _ => PreRule::Set(flat),
        }
    }

    pub fn compiled(
        &self,
        options: &ResolvedOptions,
    ) -> Result<Vec<ComputedStyle<'a, '_>>, StylexError> {
        match self {
            PreRule::Null => Ok(vec![None]),
            PreRule::Rule {
                property,
                value,
                key_path,
            } => {
                // Condition lists stay in keyPath order; the sorted orders are
                // derived inside the conversion, on its memo-miss path only.
                let pseudos: Vec<&str> = key_path
                    .iter()
                    .filter(|k| k.starts_with(':') || k.starts_with('['))
                    .map(Cow::as_ref)
                    .collect();
                let at_rules: Vec<&str> = key_path
                    .iter()
                    .filter(|k| k.starts_with('@'))
                    .map(Cow::as_ref)
                    .collect();
                let const_rules: Vec<&str> = key_path
                    .iter()
                    .filter(|k| k.starts_with("var(--"))
                    .map(Cow::as_ref)
                    .collect();
                let decl = convert_style_to_class_name(
                    property,
                    value,
                    &pseudos,
                    &at_rules,
                    &const_rules,
                    options,
                )?;
                Ok(vec![Some((decl, key_path.as_slice()))])
            }
            PreRule::Set(rules) => {
                let mut tuples: Vec<ComputedStyle<'a, '_>> = Vec::new();
                for rule in rules {
                    tuples.extend(rule.compiled(options)?.into_iter().flatten().map(Some));
                }
                if tuples.is_empty() {
                    Ok(vec![None])
                } else {
                    Ok(tuples)
                }
            }
        }
    }
}

// Upstream copies via `obj[key]=value`, so "__proto__" is a [[Set]]: primitives
// vanish, objects become the prototype, array/null protos break isPlainObject.
struct ProtoSplit<'a> {
    own: Vec<(&'a str, &'a EvalValue)>,
    proto: Option<&'a JsObjectMap>,
    non_plain: bool,
}

fn proto_split(obj: &JsObjectMap) -> ProtoSplit<'_> {
    let mut split = ProtoSplit {
        own: Vec::with_capacity(obj.len()),
        proto: None,
        non_plain: false,
    };
    for (key, val) in obj.entries() {
        if key == "__proto__" {
            match val {
                EvalValue::Obj(p) => split.proto = Some(p),
                EvalValue::Null | EvalValue::Arr(_) => split.non_plain = true,
                _ => {}
            }
        } else {
            split.own.push((key, val));
        }
    }
    // a non-plain link anywhere up the chain breaks the constructor lookup
    if let Some(p) = split.proto
        && proto_split(p).non_plain
    {
        split.non_plain = true;
    }
    split
}

// for..in order: own keys, then unshadowed prototype-chain keys.
fn for_in_entries(obj: &JsObjectMap) -> Vec<(&str, &EvalValue)> {
    let mut entries = Vec::new();
    let mut cursor = Some(obj);
    while let Some(map) = cursor {
        let split = proto_split(map);
        for (key, val) in split.own {
            if !entries.iter().any(|(k, _)| *k == key) {
                entries.push((key, val));
            }
        }
        cursor = split.proto;
    }
    entries
}

fn is_plain(obj: &JsObjectMap) -> bool {
    !proto_split(obj).non_plain
}

// parity: basic-validation.js validateNamespace
pub fn validate_namespace(namespace: &EvalValue, conditions: &[String]) -> Result<(), StylexError> {
    let EvalValue::Obj(ns) = namespace else {
        return Err(StylexError::illegal_namespace_value());
    };
    if !is_plain(ns) {
        return Err(StylexError::illegal_namespace_value());
    }
    for (key, val) in for_in_entries(ns) {
        match val {
            EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {}
            EvalValue::Arr(items) => {
                for item in items {
                    match item {
                        EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {}
                        _ => return Err(StylexError::illegal_prop_array_value()),
                    }
                }
            }
            // isPlainObject gates the object branches; non-plain values fall
            // through to ILLEGAL_PROP_VALUE exactly as upstream's last throw.
            EvalValue::Obj(inner) if is_plain(inner) => {
                if key.starts_with('@') || key.starts_with(':') || key.starts_with('[') {
                    if conditions.iter().any(|c| c == key) {
                        return Err(StylexError::duplicate_conditional());
                    }
                    let mut nested = conditions.to_vec();
                    nested.push(key.to_string());
                    validate_namespace(val, &nested)?;
                } else {
                    validate_conditional_styles(val, &[])?;
                }
            }
            EvalValue::Obj(_) | EvalValue::Undefined | EvalValue::Bool(_) => {
                return Err(StylexError::illegal_prop_value());
            }
        }
    }
    Ok(())
}

// parity: basic-validation.js validateConditionalStyles
fn validate_conditional_styles(val: &EvalValue, conditions: &[String]) -> Result<(), StylexError> {
    let EvalValue::Obj(obj) = val else {
        unreachable!("callers only pass objects");
    };
    for (key, v) in for_in_entries(obj) {
        if !(key.starts_with('@')
            || key.starts_with(':')
            || key.starts_with('[')
            || key.starts_with("var(--")
            || key == "default")
        {
            return Err(StylexError::invalid_pseudo_or_at_rule());
        }
        if conditions.iter().any(|c| c == key) {
            return Err(StylexError::duplicate_conditional());
        }
        match v {
            EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {}
            EvalValue::Arr(items) => {
                // parity quirk: arrays inside conditions report ILLEGAL_PROP_VALUE,
                // not ILLEGAL_PROP_ARRAY_VALUE.
                for item in items {
                    match item {
                        EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {}
                        _ => return Err(StylexError::illegal_prop_value()),
                    }
                }
            }
            EvalValue::Obj(inner) if is_plain(inner) => {
                let mut nested = conditions.to_vec();
                nested.push(key.to_string());
                validate_conditional_styles(v, &nested)?;
            }
            EvalValue::Obj(_) | EvalValue::Undefined | EvalValue::Bool(_) => {
                return Err(StylexError::illegal_prop_value());
            }
        }
    }
    Ok(())
}

/// The `enableMediaQueryOrder` pre-pass: `Some(rebuilt)` when anything can
/// change, so the caller can hold the rebuild and flatten borrows either way.
pub fn media_order_transform(
    style: &JsObjectMap,
    options: &ResolvedOptions,
) -> Result<Option<JsObjectMap>, StylexError> {
    if options.enable_media_query_order && dfs_can_change(style, 0) {
        Ok(Some(dfs_process_map(style, 0)?))
    } else {
        Ok(None)
    }
}

pub fn flatten_raw_style_object<'a>(
    style: &'a JsObjectMap,
    options: &ResolvedOptions,
) -> Result<Vec<(Cow<'a, str>, PreRule<'a>)>, StylexError> {
    flatten_inner(style, &[], options)
}

/// Anything that would make `dfs_process_queries` differ from its input: a
/// rewritable media key, a "__proto__" entry, or a CSSType brand it would drop.
fn dfs_can_change(obj: &JsObjectMap, depth: usize) -> bool {
    obj.css_type().is_some()
        || obj.entries().any(|(key, val)| {
            key == "__proto__"
                || (depth >= 1 && is_media_key(key))
                || matches!(val, EvalValue::Obj(inner) if dfs_can_change(inner, depth + 1))
        })
}

// parity: style-value-parser media-query-transform.js dfsProcessQueries; the
// delete+reinsert moves every rewritten @media key to the end of its siblings.
// Map-in skips a redundant top-level deep clone the EvalValue wrapper needed.
fn dfs_process_map(obj: &JsObjectMap, depth: usize) -> Result<JsObjectMap, StylexError> {
    let mut result = JsObjectMap::new();
    // Object.entries-style clone: own keys only — a "__proto__"-carried
    // prototype (and its inherited entries) is stripped here, before flatten.
    for (key, val) in proto_split(obj).own {
        let processed = match val {
            EvalValue::Obj(inner) => EvalValue::Obj(Arc::new(dfs_process_map(inner, depth + 1)?)),
            other => other.clone(),
        };
        result.insert(key, processed);
    }
    if depth >= 1 {
        let media_keys: Vec<String> = result
            .keys()
            .filter(|k| is_media_key(k))
            .map(str::to_string)
            .collect();
        if !media_keys.is_empty() {
            let rewritten = last_media_query_wins_transform(&media_keys).map_err(|e| match e {
                MediaQueryError::Syntax(err) => err,
                // Never claim upstream's syntax error for a form we cannot verify.
                MediaQueryError::Unverified { input } => StylexError::unsupported_api(&format!(
                    "media query `{input}` (unverified tokenizer form)"
                )),
            })?;
            for (old_key, new_key) in media_keys.iter().zip(rewritten) {
                let value = result.remove(old_key).expect("key collected from map");
                result.insert(new_key, value);
            }
        }
    }
    Ok(result)
}

/// The unanchored `/var\(--[a-z0-9]+\)/` key unwrap in flatten-raw-style-obj.js:
/// any key CONTAINING that pattern is sliced `[4..len-1]` wholesale.
fn unwrap_var_key(key: &str) -> Cow<'_, str> {
    let bytes = key.as_bytes();
    let mut search = 0;
    while let Some(pos) = key[search..].find("var(--") {
        let start = search + pos;
        let mut i = start + 6;
        while i < bytes.len() && (bytes[i].is_ascii_lowercase() || bytes[i].is_ascii_digit()) {
            i += 1;
        }
        if i > start + 6 && i < bytes.len() && bytes[i] == b')' {
            // Upstream does key.slice(4, -1) in UTF-16 units regardless of
            // where the match sits; byte slicing panics on multibyte keys.
            return Cow::Owned(crate::jsrt::js_slice_utf16(key, 4, -1));
        }
        search = start + 1;
    }
    Cow::Borrowed(key)
}

fn js_truthy(scalar: &StyleScalar) -> bool {
    match scalar {
        StyleScalar::Str(s) => !s.is_empty(),
        StyleScalar::Num(n) => *n != 0.0 && !n.is_nan(),
        StyleScalar::Undefined => false,
    }
}

fn same_value_zero(a: &StyleScalar, b: &StyleScalar) -> bool {
    match (a, b) {
        (StyleScalar::Str(x), StyleScalar::Str(y)) => x == y,
        (StyleScalar::Num(x), StyleScalar::Num(y)) => x == y || (x.is_nan() && y.is_nan()),
        (StyleScalar::Undefined, StyleScalar::Undefined) => true,
        _ => false,
    }
}

/// `None` is JS `null`; validation rejected every other non-scalar already.
fn scalar_of<'a>(value: &'a EvalValue) -> Option<StyleScalar<'a>> {
    match value {
        EvalValue::Str(s) => Some(StyleScalar::Str(Cow::Borrowed(s))),
        EvalValue::Num(n) => Some(StyleScalar::Num(*n)),
        _ => None,
    }
}

fn rule_key_path<'a>(
    key_path: &[Cow<'a, str>],
    includes_key: &str,
    property: Cow<'a, str>,
) -> Vec<Cow<'a, str>> {
    if key_path.iter().any(|k| k == includes_key) {
        key_path
            .iter()
            .map(|k| {
                if k == includes_key {
                    property.clone()
                } else {
                    k.clone()
                }
            })
            .collect()
    } else {
        let mut path = key_path.to_vec();
        path.push(property);
        path
    }
}

fn flatten_inner<'a>(
    style: &'a JsObjectMap,
    key_path: &[Cow<'a, str>],
    options: &ResolvedOptions,
) -> Result<Vec<(Cow<'a, str>, PreRule<'a>)>, StylexError> {
    let mut flattened: Vec<(Cow<'a, str>, PreRule<'a>)> = Vec::new();
    // for..in reach: prototype entries planted by "__proto__" keys stay
    // visible here unless the media-query clone above already stripped them.
    for (raw_key, value) in for_in_entries(style) {
        let key: Cow<'a, str> = unwrap_var_key(raw_key);

        match value {
            EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {
                let scalar = scalar_of(value);
                for (property, expanded) in
                    flat_map_expanded_shorthands(key.clone(), scalar, false, options)?
                {
                    match expanded {
                        None => flattened.push((property, PreRule::Null)),
                        Some(expanded) => {
                            let path = rule_key_path(key_path, &key, property.clone());
                            flattened.push((
                                property.clone(),
                                PreRule::Rule {
                                    property,
                                    value: PreRuleValue::Single(expanded),
                                    key_path: path,
                                },
                            ));
                        }
                    }
                }
            }
            EvalValue::Arr(items) => {
                // Fallback arrays: each element expands on its own, then the
                // per-property value lists are merged in first-seen order.
                let mut equivalent: Vec<(Cow<'a, str>, Vec<StyleScalar<'a>>)> = Vec::new();
                for item in items {
                    let scalar = scalar_of(item);
                    for (property, expanded) in
                        flat_map_expanded_shorthands(key.clone(), scalar, false, options)?
                    {
                        let slot = match equivalent.iter_mut().find(|(p, _)| *p == property) {
                            Some(slot) => &mut slot.1,
                            None => {
                                equivalent.push((property, Vec::new()));
                                &mut equivalent.last_mut().expect("just pushed").1
                            }
                        };
                        if let Some(value) = expanded {
                            slot.push(value);
                        }
                    }
                }
                for (property, values) in equivalent {
                    let mut deduped: Vec<StyleScalar> = Vec::new();
                    for value in values {
                        if js_truthy(&value) && !deduped.iter().any(|d| same_value_zero(d, &value))
                        {
                            deduped.push(value);
                        }
                    }
                    let pre_rule = match deduped.len() {
                        0 => PreRule::Null,
                        1 => PreRule::Rule {
                            property: property.clone(),
                            value: PreRuleValue::Single(deduped.into_iter().next().expect("len 1")),
                            key_path: rule_key_path(key_path, raw_key, property.clone()),
                        },
                        _ => PreRule::Rule {
                            property: property.clone(),
                            value: PreRuleValue::Multi(deduped),
                            key_path: rule_key_path(key_path, raw_key, property.clone()),
                        },
                    };
                    flattened.push((property, pre_rule));
                }
            }
            EvalValue::Obj(obj)
                if !key.starts_with(':') && !key.starts_with('@') && !key.starts_with('[') =>
            {
                // Property-value objects, e.g. color: { default, ':hover' }.
                let mut equivalent: ConditionGroups<'a> = Vec::new();
                for (condition, inner_value) in for_in_entries(obj) {
                    let nested_path: Vec<Cow<'a, str>> = if key_path.is_empty() {
                        vec![key.clone(), Cow::Borrowed(condition)]
                    } else {
                        let mut p = key_path.to_vec();
                        p.push(Cow::Borrowed(condition));
                        p
                    };
                    for (property, pre_rule) in
                        flatten_value_as_property(&key, inner_value, &nested_path, options)?
                    {
                        match equivalent.iter_mut().find(|(p, _)| *p == property) {
                            Some((_, conds)) => {
                                match conds.iter_mut().find(|(c, _)| *c == condition) {
                                    Some((_, slot)) => *slot = pre_rule,
                                    None => conds.push((condition, pre_rule)),
                                }
                            }
                            None => equivalent.push((property, vec![(condition, pre_rule)])),
                        }
                    }
                }
                for (property, conds) in equivalent {
                    let rules: Vec<PreRule> = conds.into_iter().map(|(_, r)| r).collect();
                    flattened.push((property, PreRule::create_set(rules)));
                }
            }
            EvalValue::Obj(obj) => {
                // Pseudo / at-rule / attribute objects, e.g. ':hover': { … }.
                let mut nested_path = key_path.to_vec();
                nested_path.push(Cow::Borrowed(raw_key));
                for (property, pre_rule) in flatten_inner(obj, &nested_path, options)? {
                    flattened.push((Cow::Owned(format!("{key}_{property}")), pre_rule));
                }
            }
            // Upstream's flatten silently skips other types (validation ran first).
            EvalValue::Undefined | EvalValue::Bool(_) => {}
        }
    }
    Ok(flattened)
}

/// The property-value-object recursion, without upstream's intermediate
/// `{key: value}` object: identical to flatten_inner on that single-key map.
fn flatten_value_as_property<'a>(
    key: &Cow<'a, str>,
    value: &'a EvalValue,
    nested_path: &[Cow<'a, str>],
    options: &ResolvedOptions,
) -> Result<Vec<(Cow<'a, str>, PreRule<'a>)>, StylexError> {
    match value {
        EvalValue::Null | EvalValue::Str(_) | EvalValue::Num(_) => {
            let scalar = scalar_of(value);
            let mut out = Vec::new();
            for (property, expanded) in
                flat_map_expanded_shorthands(key.clone(), scalar, false, options)?
            {
                match expanded {
                    None => out.push((property, PreRule::Null)),
                    Some(expanded) => {
                        let path = rule_key_path(nested_path, key, property.clone());
                        out.push((
                            property.clone(),
                            PreRule::Rule {
                                property,
                                value: PreRuleValue::Single(expanded),
                                key_path: path,
                            },
                        ));
                    }
                }
            }
            Ok(out)
        }
        EvalValue::Arr(items) => {
            let mut equivalent: Vec<(Cow<'a, str>, Vec<StyleScalar<'a>>)> = Vec::new();
            for item in items {
                let scalar = scalar_of(item);
                for (property, expanded) in
                    flat_map_expanded_shorthands(key.clone(), scalar, false, options)?
                {
                    let slot = match equivalent.iter_mut().find(|(p, _)| *p == property) {
                        Some(slot) => &mut slot.1,
                        None => {
                            equivalent.push((property, Vec::new()));
                            &mut equivalent.last_mut().expect("just pushed").1
                        }
                    };
                    if let Some(value) = expanded {
                        slot.push(value);
                    }
                }
            }
            let mut out = Vec::new();
            for (property, values) in equivalent {
                let mut deduped: Vec<StyleScalar> = Vec::new();
                for value in values {
                    if js_truthy(&value) && !deduped.iter().any(|d| same_value_zero(d, &value)) {
                        deduped.push(value);
                    }
                }
                let pre_rule = match deduped.len() {
                    0 => PreRule::Null,
                    1 => PreRule::Rule {
                        property: property.clone(),
                        value: PreRuleValue::Single(deduped.into_iter().next().expect("len 1")),
                        key_path: rule_key_path(nested_path, key, property.clone()),
                    },
                    _ => PreRule::Rule {
                        property: property.clone(),
                        value: PreRuleValue::Multi(deduped),
                        key_path: rule_key_path(nested_path, key, property.clone()),
                    },
                };
                out.push((property, pre_rule));
            }
            Ok(out)
        }
        // Deeper nesting (condition inside a property-value object): group by
        // property and condition into per-property sets, exactly the
        // property-value branch's shape one level down.
        EvalValue::Obj(obj) => {
            let mut out = Vec::new();
            let mut equivalent: ConditionGroups<'a> = Vec::new();
            for (condition, inner) in for_in_entries(obj) {
                let mut p = nested_path.to_vec();
                p.push(Cow::Borrowed(condition));
                for (property, pre_rule) in flatten_value_as_property(key, inner, &p, options)? {
                    match equivalent.iter_mut().find(|(pr, _)| *pr == property) {
                        Some((_, conds)) => match conds.iter_mut().find(|(c, _)| *c == condition) {
                            Some((_, slot)) => *slot = pre_rule,
                            None => conds.push((condition, pre_rule)),
                        },
                        None => equivalent.push((property, vec![(condition, pre_rule)])),
                    }
                }
            }
            for (property, conds) in equivalent {
                let rules: Vec<PreRule> = conds.into_iter().map(|(_, r)| r).collect();
                out.push((property, PreRule::create_set(rules)));
            }
            Ok(out)
        }
        EvalValue::Undefined | EvalValue::Bool(_) => Ok(Vec::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwrap_var_key_matches_upstream_regex() {
        assert_eq!(unwrap_var_key("var(--abc)"), Cow::Borrowed("--abc"));
        assert_eq!(unwrap_var_key("var(--abc123)"), Cow::Borrowed("--abc123"));
        // Uppercase var names do not match the flatten-level regex.
        assert_eq!(
            unwrap_var_key("var(--myVar)"),
            Cow::Borrowed("var(--myVar)")
        );
        assert_eq!(unwrap_var_key("var(--abc"), Cow::Borrowed("var(--abc"));
        assert_eq!(unwrap_var_key("var(--)"), Cow::Borrowed("var(--)"));
        // Unanchored match slices the whole key.
        assert_eq!(
            unwrap_var_key("xxvar(--ab)yy"),
            Cow::<str>::Owned("r(--ab)y".to_string())
        );
        assert_eq!(unwrap_var_key("color"), Cow::Borrowed("color"));
    }
}
