//! Style entry → className + injectable CSS rule.
// parity: babel-plugin src/shared/utils/{convert-to-className,generate-css-rule}.js

use crate::errors::StylexError;
use crate::fxhash::FxHashMap;
use crate::hash::hash;
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;
use crate::shared::dashify::dashed_key;
use crate::shared::fallbacks::{has_var_fallback, variable_fallbacks};
use crate::shared::flatten::{PreRuleValue, StyleScalar};
use crate::shared::normalize_value::CssValueError;
use crate::shared::priorities::get_priority;
use crate::shared::pseudo_sort::{sort_at_rules, sort_pseudos};
use crate::shared::rtl::{RtlContext, generate_ltr, generate_rtl};
use crate::shared::transform_value::{transform_value_num, transform_value_str};
use std::cell::RefCell;
use std::rc::Rc;

fn css_value_error(e: CssValueError) -> StylexError {
    match e {
        CssValueError::UnclosedFunction => StylexError::unclosed_function(),
        CssValueError::UnclosedString => StylexError::unclosed_string(),
        CssValueError::EmptyValue => StylexError::empty_value(),
    }
}

fn transform_scalar(
    key: &str,
    scalar: &StyleScalar,
    font_size_px_to_rem: bool,
) -> Result<String, StylexError> {
    match scalar {
        StyleScalar::Str(s) => {
            transform_value_str(key, s.as_ref(), font_size_px_to_rem).map_err(css_value_error)
        }
        StyleScalar::Num(n) => {
            transform_value_num(key, *n, font_size_px_to_rem).map_err(css_value_error)
        }
        // normalizeValue short-circuits on nullish, so the declaration is empty.
        StyleScalar::Undefined => Ok(String::new()),
    }
}

/// One compiled declaration, memo-shared behind `Rc`: a hit is a refcount bump.
pub type CompiledDecl = Rc<StylexRule>;

struct ConvertMemo {
    key: String,
    map: FxHashMap<String, CompiledDecl>,
}

thread_local! {
    static CONVERT_MEMO: RefCell<ConvertMemo> = RefCell::new(ConvertMemo {
        key: String::new(),
        map: FxHashMap::default(),
    });
}

const CONVERT_MEMO_CAP: usize = 1 << 16;

/// Length-prefixed segments with fixed `|` section breaks: injective, so equal
/// keys mean equal inputs.
fn push_memo_part(key: &mut String, part: &str) {
    let mut buf = [0u8; 20];
    let mut n = part.len();
    let mut i = buf.len();
    loop {
        i -= 1;
        buf[i] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    key.push_str(str::from_utf8(&buf[i..]).expect("ascii digits"));
    key.push(':');
    key.push_str(part);
}

fn push_memo_scalar(key: &mut String, scalar: &StyleScalar) {
    match scalar {
        StyleScalar::Str(s) => {
            key.push('s');
            push_memo_part(key, s);
        }
        StyleScalar::Num(n) => {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            let bits = n.to_bits();
            let mut buf = [0u8; 16];
            for (j, b) in buf.iter_mut().enumerate() {
                *b = HEX[((bits >> (60 - j * 4)) & 0xf) as usize];
            }
            key.push('n');
            key.push_str(str::from_utf8(&buf).expect("ascii digits"));
        }
        StyleScalar::Undefined => key.push('u'),
    }
}

// The fingerprint covers every option the uncached path reads: px-to-rem,
// debug class names + prefix, and the three RtlContext fields.
fn encode_memo_key(
    key: &mut String,
    property: &str,
    value: &PreRuleValue<'_>,
    pseudos: &[&str],
    at_rules: &[&str],
    const_rules: &[&str],
    options: &ResolvedOptions,
) {
    key.push(if options.enable_font_size_px_to_rem {
        'R'
    } else {
        'r'
    });
    key.push(if options.debug && options.enable_debug_class_names {
        'D'
    } else {
        'd'
    });
    key.push(match options.style_resolution {
        crate::options::StyleResolution::PropertySpecificity => 'p',
        crate::options::StyleResolution::ApplicationOrder => 'a',
        crate::options::StyleResolution::LegacyExpandShorthands => 'l',
    });
    key.push(if options.enable_logical_styles_polyfill {
        'L'
    } else {
        'g'
    });
    key.push(if options.enable_legacy_value_flipping {
        'F'
    } else {
        'f'
    });
    push_memo_part(key, &options.class_name_prefix);
    key.push('|');
    push_memo_part(key, property);
    key.push('|');
    match value {
        PreRuleValue::Single(scalar) => {
            key.push('S');
            push_memo_scalar(key, scalar);
        }
        PreRuleValue::Multi(scalars) => {
            key.push('M');
            for scalar in scalars {
                push_memo_scalar(key, scalar);
            }
        }
    }
    for list in [pseudos, at_rules, const_rules] {
        key.push('|');
        for item in list {
            push_memo_part(key, item);
        }
    }
}

/// `pseudos`/`at_rules`/`const_rules` arrive in keyPath order; sorting for the
/// hash input and selector happens here, on the miss path only.
pub fn convert_style_to_class_name(
    property: &str,
    value: &PreRuleValue<'_>,
    pseudos: &[&str],
    at_rules: &[&str],
    const_rules: &[&str],
    options: &ResolvedOptions,
) -> Result<CompiledDecl, StylexError> {
    CONVERT_MEMO.with(|memo| {
        let mut memo = memo.borrow_mut();
        let ConvertMemo { key, map } = &mut *memo;
        key.clear();
        encode_memo_key(
            key,
            property,
            value,
            pseudos,
            at_rules,
            const_rules,
            options,
        );
        if let Some(hit) = map.get(key.as_str()) {
            return Ok(Rc::clone(hit));
        }
        let computed = Rc::new(convert_style_to_class_name_uncached(
            property,
            value,
            pseudos,
            at_rules,
            const_rules,
            options,
        )?);
        if map.len() >= CONVERT_MEMO_CAP {
            map.clear();
        }
        map.insert(key.clone(), Rc::clone(&computed));
        Ok(computed)
    })
}

fn convert_style_to_class_name_uncached(
    property: &str,
    value: &PreRuleValue<'_>,
    pseudos: &[&str],
    at_rules: &[&str],
    const_rules: &[&str],
    options: &ResolvedOptions,
) -> Result<StylexRule, StylexError> {
    // Upstream evaluates the sorting PreRule.pseudos/atRules getters as call
    // arguments, so a collation error precedes any value-normalization error.
    let sorted_pseudos = sort_pseudos(pseudos)?;
    let sorted_at_rules = sort_at_rules(at_rules);
    let mut at_and_const = sorted_at_rules.clone();
    at_and_const.extend(const_rules.iter().copied());
    let sorted_at_and_const = sort_at_rules(&at_and_const);

    let dashed = dashed_key(property);

    let px_to_rem = options.enable_font_size_px_to_rem;
    let (values, is_array) = match value {
        PreRuleValue::Single(scalar) => {
            (vec![transform_scalar(property, scalar, px_to_rem)?], false)
        }
        PreRuleValue::Multi(scalars) => (
            scalars
                .iter()
                .map(|s| transform_scalar(property, s, px_to_rem))
                .collect::<Result<Vec<_>, _>>()?,
            true,
        ),
    };
    let values = if is_array && has_var_fallback(&values) {
        variable_fallbacks(&values)?
    } else {
        values
    };

    // The hash concatenates with `+` (so a JS `undefined` stringifies) while
    // the declaration is built with `Array.join`, which renders it empty.
    let hash_values: Vec<&str> = match value {
        PreRuleValue::Single(StyleScalar::Undefined) => vec!["undefined"],
        _ => values.iter().map(String::as_str).collect(),
    };

    // parity: the '<>' prefix and 'null' modifier keep upstream hashes stable;
    // built in place, byte-for-byte `<>{dashed}{values.join(", ")}{modifier}`.
    let modifier_len: usize = sorted_pseudos.iter().map(|s| s.len()).sum::<usize>()
        + sorted_at_and_const.iter().map(|s| s.len()).sum::<usize>();
    let mut hash_input = String::with_capacity(
        2 + dashed.len()
            + hash_values.iter().map(|v| v.len() + 2).sum::<usize>()
            + modifier_len.max(4),
    );
    hash_input.push_str("<>");
    hash_input.push_str(&dashed);
    for (i, v) in hash_values.iter().enumerate() {
        if i > 0 {
            hash_input.push_str(", ");
        }
        hash_input.push_str(v);
    }
    if modifier_len == 0 {
        hash_input.push_str("null");
    } else {
        for p in &sorted_pseudos {
            hash_input.push_str(p);
        }
        for a in &sorted_at_and_const {
            hash_input.push_str(a);
        }
    }
    let hashed = hash(&hash_input);
    let class_name: std::sync::Arc<str> = if options.debug && options.enable_debug_class_names {
        format!("{property}-{}{hashed}", options.class_name_prefix).into()
    } else {
        format!("{}{hashed}", options.class_name_prefix).into()
    };

    let rule = generate_css_rule(
        &class_name,
        &dashed,
        &values,
        &sorted_pseudos,
        &sorted_at_rules,
        const_rules,
        RtlContext::of(options),
    );
    Ok(rule)
}

pub fn generate_css_rule(
    class_name: &str,
    key: &str,
    values: &[String],
    pseudos: &[&str],
    at_rules: &[&str],
    const_rules: &[&str],
    ctx: RtlContext,
) -> StylexRule {
    let mut ltr_decls = String::with_capacity(
        values
            .iter()
            .map(|v| key.len() + v.len() + 2)
            .sum::<usize>(),
    );
    for (i, v) in values.iter().enumerate() {
        if i > 0 {
            ltr_decls.push(';');
        }
        let (k, v) = generate_ltr(key, v, ctx);
        ltr_decls.push_str(&k);
        ltr_decls.push(':');
        ltr_decls.push_str(&v);
    }
    let mut rtl_decls = String::new();
    for v in values {
        if let Some((k, v)) = generate_rtl(key, v, ctx) {
            if !rtl_decls.is_empty() {
                rtl_decls.push(';');
            }
            rtl_decls.push_str(&k);
            rtl_decls.push(':');
            rtl_decls.push_str(&v);
        }
    }

    let ltr = build_nested_css_rule(class_name, &ltr_decls, pseudos, at_rules, const_rules);
    let rtl = if rtl_decls.is_empty() {
        None
    } else {
        Some(build_nested_css_rule(
            class_name,
            &rtl_decls,
            pseudos,
            at_rules,
            const_rules,
        ))
    };

    let priority = get_priority(key)
        + pseudos.iter().map(|p| get_priority(p)).sum::<f64>()
        + at_rules.iter().map(|a| get_priority(a)).sum::<f64>()
        + const_rules.iter().map(|c| get_priority(c)).sum::<f64>();

    StylexRule {
        class_name: class_name.into(),
        ltr: ltr.into(),
        rtl: rtl.map(Into::into),
        const_key: None,
        const_val: None,
        priority,
    }
}

const THUMB_VARIANTS: [&str; 3] = [
    "::-webkit-slider-thumb",
    "::-moz-range-thumb",
    "::-ms-thumb",
];

// parity: generate-css-rule.js buildNestedCSSRule
fn build_nested_css_rule(
    class_name: &str,
    decls: &str,
    pseudos: &[&str],
    at_rules: &[&str],
    const_rules: &[&str],
) -> String {
    // Pseudo-classes before pseudo-elements, insertion order within each —
    // the two-buffer concat of upstream, built as one string in two passes.
    let mut pseudo = String::new();
    for p in pseudos {
        if *p != "::thumb" && !p.starts_with("::") {
            pseudo.push_str(p);
        }
    }
    for p in pseudos {
        if *p != "::thumb" && p.starts_with("::") {
            pseudo.push_str(p);
        }
    }
    let combined_len = at_rules.len() + const_rules.len();

    let class_copies = 1 + usize::from(pseudo.contains(":where(")) + combined_len;
    let mut selector = String::with_capacity(class_copies * (class_name.len() + 1) + pseudo.len());
    for _ in 0..class_copies {
        selector.push('.');
        selector.push_str(class_name);
    }
    selector.push_str(&pseudo);
    if pseudos.contains(&"::thumb") {
        selector = THUMB_VARIANTS
            .iter()
            .map(|suffix| format!("{selector}{suffix}"))
            .collect::<Vec<_>>()
            .join(", ");
    }

    // Nesting parity: iterating at_rules then const_rules and wrapping each
    // time leaves the last-iterated outermost; emit them reversed in one pass.
    let wrappers = at_rules.iter().chain(const_rules.iter());
    let mut rule = String::with_capacity(
        selector.len() + decls.len() + 2 + wrappers.clone().map(|w| w.len() + 2).sum::<usize>(),
    );
    for at_rule in wrappers.rev() {
        rule.push_str(at_rule);
        rule.push('{');
    }
    rule.push_str(&selector);
    rule.push('{');
    rule.push_str(decls);
    rule.push('}');
    for _ in 0..combined_len {
        rule.push('}');
    }
    rule
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defaults() -> ResolvedOptions {
        ResolvedOptions::default()
    }

    #[test]
    fn known_answer_color_red() {
        let decl = convert_style_to_class_name(
            "color",
            &PreRuleValue::Single(StyleScalar::Str(std::borrow::Cow::Borrowed("red"))),
            &[],
            &[],
            &[],
            &defaults(),
        )
        .unwrap();
        assert_eq!(&*decl.class_name, "x1e2nbdu");
        assert_eq!(&*decl.ltr, ".x1e2nbdu{color:red}");
        assert_eq!(decl.rtl, None);
        assert_eq!(decl.priority, 3000.0);
    }

    #[test]
    fn thumb_and_where_selector_shapes() {
        let decl = convert_style_to_class_name(
            "width",
            &PreRuleValue::Single(StyleScalar::Num(16.0)),
            &[":hover", "::thumb"],
            &[],
            &[],
            &defaults(),
        )
        .unwrap();
        assert_eq!(
            &*decl.ltr,
            format!(
                ".{c}:hover::-webkit-slider-thumb, .{c}:hover::-moz-range-thumb, .{c}:hover::-ms-thumb{{width:16px}}",
                c = decl.class_name
            )
        );
        assert_eq!(decl.priority, 9130.0);

        let decl = convert_style_to_class_name(
            "color",
            &PreRuleValue::Single(StyleScalar::Str(std::borrow::Cow::Borrowed("blue"))),
            &[":where(.x-default-marker:hover *)"],
            &[],
            &[],
            &defaults(),
        )
        .unwrap();
        assert_eq!(&*decl.class_name, "xobp4yc");
        assert_eq!(
            &*decl.ltr,
            ".xobp4yc.xobp4yc:where(.x-default-marker:hover *){color:blue}"
        );
        assert_eq!(decl.priority, 3011.3);
    }
}
