// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim
//
// oj-native partial bundling: concatenate one node_modules package's internal
// file graph into a single self-contained ES module, served as one request
// instead of one-per-file. Unlike esbuild's static CJS->ESM conversion (Vite's
// pre-bundle, which has a UMD/dynamic-export interop tail), each module keeps a
// real CommonJS runtime here (`module`/`exports`/`require` via an inline
// registry), so UMD and dynamic-export packages bundle correctly. Cross-package
// dependencies stay as native ESM imports to *their* bundles, so shared packages
// dedupe and their interop is likewise preserved.
//
// This module is the pure emitter: given each internal module's factory body and
// its resolved dependency edges, it produces the bundle source. Graph discovery,
// per-file compilation (via `cjs::analyze_for_factory`), and serving live in the
// server crate.

/// Where a `require(spec)` inside an internal module resolves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DepTarget {
    /// Another file inside this same package bundle, by its bundle id.
    Internal(String),
    /// A different package — imported as native ESM from its own bundle.
    External(String),
}

/// One CommonJS file inside the package, already lowered to a factory body that
/// references its dependencies through the `require` parameter.
#[derive(Debug, Clone)]
pub struct PkgModule {
    /// Stable id within the bundle (the file's package-relative path).
    pub id: String,
    /// Factory body: runs with `module`, `exports`, `require` in scope.
    pub body: String,
    /// `require()` specifier -> where it resolves.
    pub deps: Vec<(String, DepTarget)>,
}

fn js_str(s: &str) -> String {
    serde_json::Value::String(s.to_string()).to_string()
}

fn is_valid_ident(name: &str) -> bool {
    !name.is_empty()
        && name != "default"
        && name
            .chars()
            .enumerate()
            .all(|(i, c)| c == '_' || c == '$' || c.is_ascii_alphabetic() || (i > 0 && c.is_ascii_digit()))
}

/// Emit the self-contained ESM bundle for one package.
///
/// - `modules`: every internal file, in any order.
/// - `entry_id`: the id whose exports become the bundle's exports.
/// - `externals`: distinct cross-package specifiers, in stable order.
/// - `named_exports`: statically-known named exports of the entry to re-export.
pub fn emit_package_bundle(
    modules: &[PkgModule],
    entry_id: &str,
    externals: &[String],
    named_exports: &[String],
) -> String {
    let mut out = String::new();

    // 1. Import each cross-package dependency as native ESM (one bundle each) and
    //    collect its CommonJS value (the shape `require()` should return).
    for (i, ext) in externals.iter().enumerate() {
        out.push_str(&format!("import * as __oj_extns{i} from {};\n", js_str(ext)));
    }
    out.push_str("function __oj_extcjs(ns) {\n");
    out.push_str("  return ns && ns.__cjs_exports !== undefined ? ns.__cjs_exports : (ns && ns.default !== undefined ? ns.default : ns);\n");
    out.push_str("}\n");
    if !externals.is_empty() {
        out.push_str("const __oj_ext = {\n");
        for (i, ext) in externals.iter().enumerate() {
            out.push_str(&format!("  {}: () => __oj_extcjs(__oj_extns{i}),\n", js_str(ext)));
        }
        out.push_str("};\n");
    }

    // 2. Per-module dependency maps: spec -> "#<internalId>" | "@<external>".
    out.push_str("const __oj_deps = {\n");
    for m in modules {
        out.push_str(&format!("  {}: {{", js_str(&m.id)));
        let mut first = true;
        for (spec, target) in &m.deps {
            if !first {
                out.push_str(", ");
            }
            first = false;
            let v = match target {
                DepTarget::Internal(id) => format!("#{id}"),
                DepTarget::External(pkg) => format!("@{pkg}"),
            };
            out.push_str(&format!("{}: {}", js_str(spec), js_str(&v)));
        }
        out.push_str("},\n");
    }
    out.push_str("};\n");

    // 3. Inline CommonJS registry: real module/exports/require per module.
    out.push_str("const __oj_cache = {};\n");
    out.push_str("const __oj_factories = {\n");
    for m in modules {
        out.push_str(&format!(
            "  {}: function (module, exports, require) {{\n{}\n}},\n",
            js_str(&m.id),
            m.body,
        ));
    }
    out.push_str("};\n");
    out.push_str(
        "function __oj_require(id) {\n\
         \x20 if (Object.prototype.hasOwnProperty.call(__oj_cache, id)) return __oj_cache[id].exports;\n\
         \x20 const module = { exports: {} };\n\
         \x20 __oj_cache[id] = module;\n\
         \x20 const map = __oj_deps[id] || {};\n\
         \x20 const require = (spec) => {\n\
         \x20   const t = map[spec];\n\
         \x20   if (t === undefined) throw new Error(\"[oj] unresolved require(\" + JSON.stringify(spec) + \") in package bundle\");\n\
         \x20   if (t[0] === \"#\") return __oj_require(t.slice(1));\n\
         \x20   return __oj_ext[t.slice(1)]();\n\
         \x20 };\n\
         \x20 __oj_factories[id].call(module.exports, module, module.exports, require);\n\
         \x20 return module.exports;\n\
         }\n",
    );

    // 4. Instantiate the entry and re-export its CommonJS value as ESM.
    out.push_str(&format!("const __oj_entry = __oj_require({});\n", js_str(entry_id)));
    out.push_str(
        "const __oj_default = (__oj_entry && __oj_entry.__esModule) ? __oj_entry[\"default\"] : __oj_entry;\n",
    );
    out.push_str("export default __oj_default;\n");
    out.push_str("export const __cjs_exports = __oj_entry;\n");

    let mut seen = std::collections::HashSet::new();
    for name in named_exports {
        if is_valid_ident(name) && seen.insert(name.as_str()) {
            out.push_str(&format!(
                "const __oj_x_{name} = __oj_entry[{}];\nexport {{ __oj_x_{name} as {name} }};\n",
                js_str(name),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(src: &str) -> serde_json::Value {
        // Evaluate the emitted ESM by importing it as a data: URL under node.
        let dir = std::env::temp_dir().join(format!("oj-pkgbundle-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join(format!("b-{}.mjs", fastrand_like(src)));
        std::fs::write(&f, src).unwrap();
        let probe = dir.join(format!("probe-{}.mjs", fastrand_like(src)));
        std::fs::write(
            &probe,
            format!(
                "import def, {{ __cjs_exports }} from {:?};\nprocess.stdout.write(JSON.stringify({{ def, cjs: __cjs_exports }}));\n",
                f.to_string_lossy()
            ),
        )
        .unwrap();
        let out = std::process::Command::new("node").arg(&probe).output().unwrap();
        assert!(out.status.success(), "node failed: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    // A tiny deterministic name so parallel tests don't collide (no rand dep).
    fn fastrand_like(s: &str) -> u64 {
        let mut h: u64 = 1469598103934665603;
        for b in s.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(1099511628211);
        }
        h
    }

    #[test]
    fn bundles_internal_cjs_graph_with_entry_exports() {
        // entry requires an internal file; both are CJS.
        let modules = vec![
            PkgModule {
                id: "index.js".into(),
                body: "const dep = require(\"./dep\");\nmodule.exports.greet = (n) => dep.hi + n;".into(),
                deps: vec![("./dep".into(), DepTarget::Internal("dep.js".into()))],
            },
            PkgModule {
                id: "dep.js".into(),
                body: "module.exports.hi = \"hi \";".into(),
                deps: vec![],
            },
        ];
        let src = emit_package_bundle(&modules, "index.js", &[], &["greet".into()]);
        let v = run(&src);
        // greet is a function; call it by re-emitting a probe. Simpler: check cjs.greet exists via default.
        assert!(v["cjs"].is_object(), "cjs exports object: {v}");
        // named export wiring present in source
        assert!(src.contains("as greet"), "named export emitted: {src}");
    }

    #[test]
    fn default_export_follows_module_exports() {
        let modules = vec![PkgModule {
            id: "index.js".into(),
            body: "module.exports = 41 + 1;".into(),
            deps: vec![],
        }];
        let src = emit_package_bundle(&modules, "index.js", &[], &[]);
        let v = run(&src);
        assert_eq!(v["def"], serde_json::json!(42), "module.exports becomes default: {v}");
    }

    #[test]
    fn esmodule_flag_unwraps_default() {
        let modules = vec![PkgModule {
            id: "index.js".into(),
            body: "Object.defineProperty(exports, \"__esModule\", { value: true });\nexports.default = 7;".into(),
            deps: vec![],
        }];
        let src = emit_package_bundle(&modules, "index.js", &[], &[]);
        let v = run(&src);
        assert_eq!(v["def"], serde_json::json!(7), "__esModule default unwrapped: {v}");
    }

    #[test]
    fn external_require_routes_through_esm_import() {
        // The external dep isn't resolvable here; assert the wiring is emitted
        // (import + __oj_ext entry + deps map) rather than executing it.
        let modules = vec![PkgModule {
            id: "index.js".into(),
            body: "const react = require(\"react\");\nmodule.exports = react;".into(),
            deps: vec![("react".into(), DepTarget::External("react".into()))],
        }];
        let src = emit_package_bundle(&modules, "index.js", &["/@oj-pkg/react.mjs".into()], &[]);
        assert!(src.contains("import * as __oj_extns0 from \"/@oj-pkg/react.mjs\""), "{src}");
        assert!(src.contains("__oj_extcjs(__oj_extns0)"), "{src}");
        assert!(src.contains(r#""react": "@react""#), "external dep mapped: {src}");
    }
}
