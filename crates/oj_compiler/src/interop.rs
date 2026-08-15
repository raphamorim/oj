// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::{ImportDeclarationSpecifier, ModuleExportName, Statement};
use oxc_parser::Parser;
use oxc_span::SourceType;

pub fn rewrite_cjs_interop(
    source: &str,
    path: &Path,
    interop: &dyn Fn(&str) -> Option<String>,
) -> Option<String> {
    let source_type = SourceType::from_path(path).unwrap_or_default();
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, source_type).parse();
    if parsed.panicked {
        return None;
    }

    let mut edits: Vec<(usize, usize, String)> = Vec::new();
    let mut idx = 0usize;

    for stmt in &parsed.program.body {
        let Statement::ImportDeclaration(decl) = stmt else { continue };
        if decl.import_kind.is_type() {
            continue;
        }
        let Some(url) = interop(decl.source.value.as_str()) else { continue };
        let ns = format!("__ojcjs{idx}");
        idx += 1;

        let mut out = format!("import {ns} from {};", json_str(&url));
        match &decl.specifiers {
            None => {
                out = format!("import {};", json_str(&url));
            }
            Some(specs) => {
                let mut names: Vec<String> = Vec::new();
                for spec in specs {
                    match spec {
                        ImportDeclarationSpecifier::ImportDefaultSpecifier(s) => {
                            out.push_str(&format!(
                                "const {} = {ns} && {ns}.__esModule ? {ns}.default : {ns};",
                                s.local.name
                            ));
                        }
                        ImportDeclarationSpecifier::ImportNamespaceSpecifier(s) => {
                            out.push_str(&format!("const {} = {ns};", s.local.name));
                        }
                        ImportDeclarationSpecifier::ImportSpecifier(s) => {
                            let imported = match &s.imported {
                                ModuleExportName::IdentifierName(i) => i.name.to_string(),
                                ModuleExportName::IdentifierReference(i) => i.name.to_string(),
                                ModuleExportName::StringLiteral(l) => l.value.to_string(),
                            };
                            names.push(format!("{}: {}", json_key(&imported), s.local.name));
                        }
                    }
                }
                if !names.is_empty() {
                    out.push_str(&format!("const {{ {} }} = {ns};", names.join(", ")));
                }
            }
        }
        edits.push((decl.span.start as usize, decl.span.end as usize, out));
    }

    if edits.is_empty() {
        return None;
    }
    edits.sort_by_key(|e| std::cmp::Reverse(e.0));
    let mut result = source.to_string();
    for (start, end, text) in edits {
        result.replace_range(start..end, &text);
    }
    Some(result)
}

fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            _ => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_key(s: &str) -> String {
    if !s.is_empty()
        && s.chars().next().map(|c| c.is_ascii_alphabetic() || c == '_' || c == '$').unwrap_or(false)
        && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
    {
        s.to_string()
    } else {
        json_str(s)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn interop_all(url: &str) -> impl Fn(&str) -> Option<String> + '_ {
        move |spec: &str| (spec == "cjs-dep").then(|| url.to_string())
    }

    fn run(src: &str) -> String {
        rewrite_cjs_interop(src, Path::new("m.js"), &interop_all("/@oj-deps/cjs-dep.mjs")).unwrap()
    }

    #[test]
    fn default_import_unwraps_esmodule() {
        let out = run(r#"import Foo from "cjs-dep";"#);
        assert!(out.contains(r#"import __ojcjs0 from "/@oj-deps/cjs-dep.mjs";"#), "{out}");
        assert!(out.contains("const Foo = __ojcjs0 && __ojcjs0.__esModule ? __ojcjs0.default : __ojcjs0;"), "{out}");
    }

    #[test]
    fn named_imports_destructure_from_module_exports() {
        let out = run(r#"import { a, b as c } from "cjs-dep";"#);
        assert!(out.contains("const { a: a, b: c } = __ojcjs0;"), "{out}");
    }

    #[test]
    fn mixed_default_and_named() {
        let out = run(r#"import D, { x } from "cjs-dep";"#);
        assert!(out.contains("const D = __ojcjs0 && __ojcjs0.__esModule ? __ojcjs0.default : __ojcjs0;"), "{out}");
        assert!(out.contains("const { x: x } = __ojcjs0;"), "{out}");
    }

    #[test]
    fn namespace_import_binds_module_exports() {
        let out = run(r#"import * as ns from "cjs-dep";"#);
        assert!(out.contains("const ns = __ojcjs0;"), "{out}");
    }

    #[test]
    fn side_effect_import_just_rewrites_specifier() {
        let out = run(r#"import "cjs-dep";"#);
        assert_eq!(out.trim(), r#"import "/@oj-deps/cjs-dep.mjs";"#);
    }

    #[test]
    fn non_interop_and_type_imports_untouched() {
        assert!(rewrite_cjs_interop(r#"import x from "other";"#, Path::new("m.ts"), &interop_all("/u")).is_none());
        assert!(rewrite_cjs_interop(r#"import type T from "cjs-dep";"#, Path::new("m.ts"), &interop_all("/u")).is_none());
    }

    #[test]
    fn string_named_import() {
        let out = run(r#"import { "weird-name" as w } from "cjs-dep";"#);
        assert!(out.contains(r#"const { "weird-name": w } = __ojcjs0;"#), "{out}");
    }
}
