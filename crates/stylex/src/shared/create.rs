//! `stylex.create` namespace compilation over already-evaluated style objects.
// parity: babel-plugin src/shared/stylex-create.js + visitors/stylex-create.js:252-283

use crate::errors::StylexError;
use crate::eval::value::{EvalValue, JsObjectMap};
use crate::fxhash::FxHashMap;
use crate::fxhash::FxHashSet;
use crate::hash::create_short_hash;
use crate::options::{ModuleResolutionType, ResolvedOptions};
use crate::rules::StylexRule;
use crate::shared::dev_naming::{
    DebugPathInfo, add_source_map_data, convert_to_test_styles, inject_dev_class_names,
};
use crate::shared::flatten::{
    PreRule, flatten_raw_style_object, media_order_transform, validate_namespace,
};
use std::borrow::Cow;
use std::collections::BTreeMap;
use std::sync::Arc;

/// Per-call inputs the babel visitor reads from the AST / process state.
/// Wave 3 (AST front end) fills these; pins feed them synthetically.
#[derive(Debug, Clone)]
pub struct CreateContext<'a> {
    pub options: &'a ResolvedOptions,
    /// Absolute source filename ('/'-separated); `None` only in AST-less tests.
    pub filename: Option<String>,
    /// process.cwd() equivalent; unused until wave 3 resolves packages itself.
    pub cwd: String,
    /// Name of the variable the create() result is assigned to, if any.
    pub var_name: Option<String>,
    /// 1-based source line of each namespace property (babel `loc.start.line`).
    pub namespace_lines: Option<BTreeMap<String, u32>>,
    /// Nearest package.json walking up from dirname(filename): (name, dir).
    pub file_package: Option<(String, String)>,
    /// Name of the nearest package.json walking up from dirname(cwd).
    pub cwd_package_name: Option<String>,
}

impl<'a> CreateContext<'a> {
    pub fn new(options: &'a ResolvedOptions) -> Self {
        CreateContext {
            options,
            filename: None,
            cwd: String::new(),
            var_name: None,
            namespace_lines: None,
            file_package: None,
            cwd_package_name: None,
        }
    }
}

/// className → keyPath, first-seen order with last-write-wins values.
pub type ClassPathsInNamespace = Vec<(String, Vec<String>)>;

#[derive(Debug, Clone, PartialEq)]
pub struct CreateOutput {
    /// namespace name → compiled object (className strings / null, `$$css`).
    /// Rc-shared with the namespace memo: prod-mode consumers never clone.
    pub compiled: Arc<JsObjectMap>,
    /// Injected rules in traversal order, deduped by className (first wins).
    pub rules: Vec<StylexRule>,
    /// namespace → className → keyPath (classPathsPerNamespace upstream);
    /// built only when the caller has dynamic-fn namespaces to rewrite.
    pub class_paths: Vec<(String, ClassPathsInNamespace)>,
}

/// The namespace an atom compiles under (`@stylexjs/atoms` styleInput key).
pub const INLINE_NAMESPACE: &str = "__inline__";

/// parity: atoms babel-transform compileStaticStyle — styleXCreateSet plus the
/// dev-class step only, never source-map data and never test styles.
pub fn compile_atom(
    property: &str,
    value: &str,
    ctx: &CreateContext<'_>,
) -> Result<CreateOutput, StylexError> {
    let mut raw = JsObjectMap::new();
    raw.insert(property, EvalValue::Str(value.to_string()));
    let mut namespaces = JsObjectMap::new();
    namespaces.insert(INLINE_NAMESPACE, EvalValue::Obj(raw.into()));
    let mut out = compile_namespaces_core(&namespaces, ctx, false)?;
    if ctx.options.dev && ctx.options.enable_dev_class_names {
        out.compiled = inject_dev_class_names(out.compiled, None, ctx.filename.as_deref());
    }
    Ok(out)
}

/// Dynamic-fn namespaces are wave 3: callers reject them with `unsupported_api`
/// before this point (`EvalValue` cannot represent functions).
pub fn compile_namespaces(
    namespaces: &JsObjectMap,
    ctx: &CreateContext<'_>,
    need_class_paths: bool,
) -> Result<CreateOutput, StylexError> {
    let mut out = compile_namespaces_core(namespaces, ctx, need_class_paths)?;
    let options = ctx.options;
    if options.debug && options.enable_debug_data_prop {
        let debug_paths = DebugPathInfo {
            file_package: ctx.file_package.clone(),
            cwd_package_name: ctx.cwd_package_name.clone(),
            root_dir: options
                .unstable_module_resolution
                .as_ref()
                .and_then(|m| m.root_dir.as_ref())
                .map(|r| r.to_string_lossy().into_owned()),
            is_haste: options
                .unstable_module_resolution
                .as_ref()
                .is_some_and(|m| m.kind == ModuleResolutionType::Haste),
        };
        out.compiled = add_source_map_data(
            out.compiled,
            ctx.namespace_lines.as_ref(),
            ctx.filename.as_deref(),
            &debug_paths,
        );
    }
    if options.dev && options.enable_dev_class_names {
        out.compiled = inject_dev_class_names(
            out.compiled,
            ctx.var_name.as_deref(),
            ctx.filename.as_deref(),
        );
    }
    if options.test {
        out.compiled = convert_to_test_styles(
            &out.compiled,
            ctx.var_name.as_deref(),
            ctx.filename.as_deref(),
        );
    }
    Ok(out)
}

fn compile_namespaces_core(
    namespaces: &JsObjectMap,
    ctx: &CreateContext<'_>,
    need_class_paths: bool,
) -> Result<CreateOutput, StylexError> {
    let _t = crate::timings::start(crate::timings::Stage::Create);
    let options = ctx.options;
    let mut resolved_namespaces = JsObjectMap::new();
    let mut rules: Vec<StylexRule> = Vec::new();
    let mut class_paths: Vec<(String, ClassPathsInNamespace)> = Vec::new();
    let mut seen_class_names: FxHashSet<Arc<str>> = FxHashSet::default();

    for (namespace_name, namespace_value) in namespaces.entries() {
        // The evaluator's `value[key] =` [[Set]] never creates a "__proto__"
        // namespace, and Object.keys(namespaces) cannot see prototype entries.
        if namespace_name == "__proto__" {
            continue;
        }
        validate_namespace(namespace_value, &[])?;
        let EvalValue::Obj(namespace) = namespace_value else {
            unreachable!("validate_namespace only accepts objects");
        };

        let transformed;
        let namespace_ref = match media_order_transform(namespace, options)? {
            Some(rebuilt) => {
                transformed = rebuilt;
                &transformed
            }
            None => namespace,
        };
        let flattened = flatten_raw_style_object(namespace_ref, options)?;
        let deduped = dedupe_last_wins(flattened);

        let mut namespace_obj = JsObjectMap::new();
        let mut paths_in_namespace: Vec<(String, Vec<String>)> = Vec::new();
        let mut namespace_rules: Vec<StylexRule> = Vec::new();
        for (key, pre_rule) in &deduped {
            // Variables-as-keys skip minification to avoid dynamic-style regressions.
            let display_key = if options.enable_minified_keys && !key.starts_with("--") {
                let hashed = minified_key_hash(key.as_ref());
                if options.debug {
                    format!("{key}-k{hashed}")
                } else {
                    format!("k{hashed}")
                }
            } else {
                key.to_string()
            };

            let computed = pre_rule.compiled(options)?;
            let mut class_names: Vec<&str> = Vec::new();
            for entry in computed.iter().flatten() {
                let name: &str = &entry.0.class_name;
                if !class_names.contains(&name) {
                    class_names.push(name);
                }
            }
            let joined = class_names.join(" ");
            namespace_obj.insert(
                display_key,
                if joined.is_empty() {
                    EvalValue::Null
                } else {
                    EvalValue::Str(joined)
                },
            );
            for (decl, key_path) in computed.into_iter().flatten() {
                if need_class_paths {
                    match paths_in_namespace
                        .iter_mut()
                        .find(|(name, _)| name.as_str() == &*decl.class_name)
                    {
                        Some(slot) => {
                            slot.1 = key_path.iter().map(|c| c.to_string()).collect();
                        }
                        None => paths_in_namespace.push((
                            decl.class_name.to_string(),
                            key_path.iter().map(|c| c.to_string()).collect(),
                        )),
                    }
                }
                namespace_rules.push((*decl).clone());
            }
        }
        namespace_obj.insert("$$css", EvalValue::Bool(true));
        resolved_namespaces.insert(
            namespace_name.to_string(),
            EvalValue::Obj(Arc::new(namespace_obj)),
        );
        for rule in namespace_rules {
            if seen_class_names.insert(rule.class_name.clone()) {
                rules.push(rule);
            }
        }
        if need_class_paths {
            class_paths.push((namespace_name.to_string(), paths_in_namespace));
        }
    }

    Ok(CreateOutput {
        compiled: Arc::new(resolved_namespaces),
        rules,
        class_paths,
    })
}

// parity: stylex-create.js reduceRight dedupe — the LAST occurrence of a key
// wins and keeps its (last) position.
fn dedupe_last_wins<'a>(
    flattened: Vec<(Cow<'a, str>, PreRule<'a>)>,
) -> Vec<(Cow<'a, str>, PreRule<'a>)> {
    let mut seen: FxHashSet<&str> = FxHashSet::default();
    let mut keep = vec![false; flattened.len()];
    for (i, (key, _)) in flattened.iter().enumerate().rev() {
        if seen.insert(key) {
            keep[i] = true;
        }
    }
    flattened
        .into_iter()
        .zip(keep)
        .filter_map(|(entry, keep)| keep.then_some(entry))
        .collect()
}

thread_local! {
    // createShortHash('<'+'>'+key) is pure; flattened keys repeat across every
    // file in a design-system corpus, so the hash is computed once per process.
    static MINIFIED_KEYS: std::cell::RefCell<FxHashMap<String, String>> =
        std::cell::RefCell::new(FxHashMap::default());
}

fn minified_key_hash(key: &str) -> String {
    MINIFIED_KEYS.with(|memo| {
        let mut memo = memo.borrow_mut();
        match memo.get(key) {
            Some(hashed) => hashed.clone(),
            None => {
                let hashed = create_short_hash(&format!("<>{key}"));
                memo.insert(key.to_string(), hashed.clone());
                hashed
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(entries: &[(&str, EvalValue)]) -> EvalValue {
        EvalValue::Obj(
            entries
                .iter()
                .map(|(k, v)| ((*k).to_string(), v.clone()))
                .collect::<JsObjectMap>()
                .into(),
        )
    }

    fn s(v: &str) -> EvalValue {
        EvalValue::Str(v.to_string())
    }

    #[test]
    fn minimal_namespace_compiles_to_known_answers() {
        let namespaces: JsObjectMap = [(
            "a".to_string(),
            obj(&[("color", s("blue")), ("backgroundColor", s("red"))]),
        )]
        .into_iter()
        .collect();
        let options = ResolvedOptions::default();
        let ctx = CreateContext::new(&options);
        let out = compile_namespaces(&namespaces, &ctx, false).unwrap();
        assert_eq!(
            out.compiled.to_json(),
            serde_json::json!({
                "a": { "kMwMTN": "xju2f9n", "kWkggS": "xrkmrrc", "$$css": true }
            })
        );
        assert_eq!(out.rules.len(), 2);
        assert_eq!(&*out.rules[0].ltr, ".xju2f9n{color:blue}");
        assert_eq!(&*out.rules[1].ltr, ".xrkmrrc{background-color:red}");
    }

    #[test]
    fn non_object_namespace_errors() {
        let namespaces: JsObjectMap = [("a".to_string(), EvalValue::Null)].into_iter().collect();
        let options = ResolvedOptions::default();
        let ctx = CreateContext::new(&options);
        let err = compile_namespaces(&namespaces, &ctx, false).unwrap_err();
        assert_eq!(err.message, "A StyleX namespace must be an object.");
    }
}
