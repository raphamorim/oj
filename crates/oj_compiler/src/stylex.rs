// SPDX-License-Identifier: MIT
// StyleX pass plumbing: gate, config, and the dev/build compile seams.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use fru::api::{transform_program, transform_source_mapped_in, FileContext};
use fru::module_resolution::StdFs;
use fru::options::{CompilerOptions, ResolvedOptions};
use memchr::memmem::Finder;
use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use serde_json::json;

pub use fru::rules::StylexRule;

/// Bump when the pass's output semantics change: it is folded into cache keys
/// so modules persisted by an older pass are never replayed.
pub const STYLEX_PASS_VERSION: u32 = 2;

static F_STYLEX: LazyLock<Finder<'static>> = LazyLock::new(|| Finder::new("stylex"));

#[derive(Debug, Clone)]
pub struct StylexPassConfig {
    /// The app root: the compiler's cwd (pins `$$css` debug-string derivation).
    pub app_root: PathBuf,
    /// Base directory the include/exclude globs are relative to, and the
    /// `unstable_moduleResolution` rootDir for cross-file theme references.
    pub root_dir: PathBuf,
    pub include: Vec<glob::Pattern>,
    pub exclude: Vec<glob::Pattern>,
    pub dev: bool,
    pub use_css_layers: bool,
    pub class_name_prefix: Option<String>,
    options: ResolvedOptions,
}

impl StylexPassConfig {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        app_root: PathBuf,
        root_dir: PathBuf,
        include: &[String],
        exclude: &[String],
        dev: bool,
        use_css_layers: bool,
        class_name_prefix: Option<String>,
    ) -> Result<Self, String> {
        let compile = |globs: &[String]| -> Result<Vec<glob::Pattern>, String> {
            globs
                .iter()
                .map(|g| {
                    glob::Pattern::new(g).map_err(|e| format!("invalid stylex glob {g:?}: {e}"))
                })
                .collect()
        };
        let raw = CompilerOptions {
            dev: Some(json!(dev)),
            class_name_prefix: class_name_prefix.as_ref().map(|prefix| json!(prefix)),
            // Always on: oj's dev graph is unbundled and oxc's TS transform elides
            // imports the pass leaves unused, so without the compensation imports a
            // theme/const module would drop out of the graph and its rules with it.
            treeshake_compensation: Some(json!(true)),
            unstable_module_resolution: Some(json!({
                "type": "commonJS",
                "rootDir": root_dir.to_string_lossy().replace('\\', "/"),
            })),
            ..CompilerOptions::default()
        };
        let options = raw
            .resolve()
            .map_err(|e| format!("invalid stylex options: {e}"))?;
        Ok(Self {
            app_root,
            root_dir,
            include: compile(include)?,
            exclude: compile(exclude)?,
            dev,
            use_css_layers,
            class_name_prefix,
            options,
        })
    }

    pub fn matches_path(&self, file: &Path) -> bool {
        let rel = file.strip_prefix(&self.root_dir).unwrap_or(file);
        let rel = rel.to_string_lossy().replace('\\', "/");
        self.include.iter().any(|p| p.matches(&rel))
            && !self.exclude.iter().any(|p| p.matches(&rel))
    }

    /// Cheap per-module gate: the path must be included and the source must
    /// mention "stylex" (SIMD scan) before the pass pays for anything.
    pub fn is_candidate(&self, file: &Path, source: &str) -> bool {
        crate::scan(&F_STYLEX, source) && self.matches_path(file)
    }

    /// Canonical config identity for cache salting (callers hash it).
    pub fn salt_input(&self) -> String {
        let globs =
            |v: &[glob::Pattern]| v.iter().map(|p| p.as_str()).collect::<Vec<_>>().join(",");
        format!(
            "stylex-pass-v{STYLEX_PASS_VERSION};appRoot={};root={};include={};exclude={};dev={};layers={};prefix={}",
            self.app_root.display(),
            self.root_dir.display(),
            globs(&self.include),
            globs(&self.exclude),
            self.dev,
            self.use_css_layers,
            self.class_name_prefix.as_deref().unwrap_or("x"),
        )
    }
}

#[derive(Debug, Default)]
pub struct StylexPassOutput {
    /// `Some` = the transform changed the module; feed this to the pipeline.
    pub code: Option<String>,
    /// v3 map for `code`; the splice shifts lines, so it must be forwarded.
    pub map: Option<String>,
    pub rules: Vec<StylexRule>,
}

#[derive(Debug, Default)]
pub struct StylexAstOutput {
    pub modified: bool,
    pub rules: Vec<StylexRule>,
}

/// Dev-path seam: mutates oj's own parsed AST in place — one parse per module.
/// Synthesized nodes carry the span of the node they replace (or an empty
/// span), so the pipeline's sourcemap stays valid against the ORIGINAL source.
/// `program` must be the parse of `source_text`.
pub fn stylex_pass_ast<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    path: &Path,
    source_text: &str,
    config: &StylexPassConfig,
) -> Result<StylexAstOutput, String> {
    let ctx = FileContext {
        filename: path,
        source_text,
        cwd: &config.app_root,
    };
    // ProgramScoping::Rebuild (inside transform_program): Prebuilt would need
    // a Semantic aliasing the &mut program (unsafe here) to save the ~2-25us
    // internal SemanticBuilder pass measured on the showcase fixture modules.
    match transform_program(allocator, program, &ctx, &config.options, &StdFs) {
        Ok(result) => Ok(StylexAstOutput {
            modified: result.modified,
            rules: result.rules,
        }),
        Err(e) => Err(e.to_string()),
    }
}

/// String-level pass: kept for the BUILD path only (the rolldown transform
/// plugin is string-in/string-out by nature); the dev pipeline mutates its
/// own AST via `stylex_pass_ast` instead.
pub fn stylex_pass(
    path: &Path,
    source_text: &str,
    config: &StylexPassConfig,
) -> Result<StylexPassOutput, String> {
    let ctx = FileContext {
        filename: path,
        source_text,
        cwd: &config.app_root,
    };
    // The splice moves line positions, so the map is not optional here: a
    // transform that reports none makes rolldown keep the pre-splice mapping.
    let allocator = oxc_allocator::Allocator::default();
    match transform_source_mapped_in(&allocator, &ctx, &config.options, &StdFs, true) {
        Ok(Some(result)) => Ok(StylexPassOutput {
            map: result.modified.then_some(result.map).flatten(),
            code: result.modified.then_some(result.code),
            rules: result.rules,
        }),
        Ok(None) => Ok(StylexPassOutput::default()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxc_parser::Parser;

    fn config(include: &[&str], exclude: &[&str]) -> StylexPassConfig {
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        StylexPassConfig::new(
            PathBuf::from("/app"),
            PathBuf::from("/app"),
            &s(include),
            &s(exclude),
            true,
            false,
            None,
        )
        .unwrap()
    }

    #[test]
    fn globs_match_root_relative_paths() {
        let cfg = config(&["src/**"], &[]);
        assert!(cfg.matches_path(Path::new("/app/src/main.ts")));
        assert!(cfg.matches_path(Path::new("/app/src/deep/Button.tsx")));
        assert!(!cfg.matches_path(Path::new("/app/lib/main.ts")));
    }

    #[test]
    fn exclude_wins_over_include() {
        let cfg = config(&["src/**"], &["src/vendor/**"]);
        assert!(cfg.matches_path(Path::new("/app/src/a.ts")));
        assert!(!cfg.matches_path(Path::new("/app/src/vendor/b.ts")));
    }

    #[test]
    fn candidate_needs_both_path_and_source_mention() {
        let cfg = config(&["src/**"], &[]);
        let file = Path::new("/app/src/main.ts");
        assert!(cfg.is_candidate(file, "import * as stylex from '@stylexjs/stylex';"));
        assert!(!cfg.is_candidate(file, "export const x = 1;"));
        assert!(!cfg.is_candidate(Path::new("/app/lib/x.ts"), "stylex"));
    }

    #[test]
    fn invalid_glob_is_a_config_error() {
        let err = StylexPassConfig::new(
            PathBuf::from("/app"),
            PathBuf::from("/app"),
            &["src/[".into()],
            &[],
            true,
            false,
            None,
        );
        assert!(err.is_err());
    }

    #[test]
    fn salt_input_changes_with_config_and_is_stable() {
        let a = config(&["src/**"], &[]).salt_input();
        let b = config(&["src/**"], &[]).salt_input();
        let c = config(&["app/**"], &[]).salt_input();
        assert_eq!(a, b);
        assert_ne!(a, c);
    }

    #[test]
    fn salt_input_changes_with_class_name_prefix() {
        let base = config(&["src/**"], &[]);
        let prefixed = StylexPassConfig::new(
            PathBuf::from("/app"),
            PathBuf::from("/app"),
            &["src/**".into()],
            &[],
            true,
            false,
            Some("oj".into()),
        )
        .unwrap();
        assert_ne!(base.salt_input(), prefixed.salt_input());
    }

    #[test]
    fn pass_compiles_a_create_call_and_extracts_rules() {
        let cfg = config(&["**"], &[]);
        let src = "import * as stylex from '@stylexjs/stylex';\n\
                   export const styles = stylex.create({ root: { color: 'red' } });\n";
        let out = stylex_pass(Path::new("/app/src/a.ts"), src, &cfg).unwrap();
        let code = out.code.expect("create call must modify the module");
        assert!(!code.contains("stylex.create"), "create must compile away");
        assert_eq!(out.rules.len(), 1);
        assert!(out.rules[0].ltr.contains("color:red"));
        assert!(code.contains(&*out.rules[0].class_name));
    }

    #[test]
    fn pass_skips_non_stylex_modules_via_pre_gate() {
        // Path gate passed, but no import source in the text ("stylex" alone
        // is not an import source match inside the compiler's pre-gate).
        let cfg = config(&["**"], &[]);
        let out = stylex_pass(Path::new("/app/src/a.ts"), "const stylexish = 1;", &cfg).unwrap();
        assert!(out.code.is_none());
        assert!(out.rules.is_empty());
    }

    #[test]
    fn pass_surfaces_authoring_errors() {
        let cfg = config(&["**"], &[]);
        let src = "import * as stylex from '@stylexjs/stylex';\n\
                   const dyn = Math.random();\n\
                   export const styles = stylex.create({ root: { color: dyn } });\n";
        let err = stylex_pass(Path::new("/app/src/a.ts"), src, &cfg).unwrap_err();
        assert!(!err.is_empty());
    }

    fn reprint(source: &str, path: &Path) -> String {
        let allocator = Allocator::default();
        let parsed = Parser::new(
            &allocator,
            source,
            oxc_span::SourceType::from_path(path).unwrap(),
        )
        .parse();
        assert!(!parsed.panicked, "reprint parse failed");
        oxc_codegen::Codegen::new().build(&parsed.program).code
    }

    #[test]
    fn ast_and_string_seams_agree_on_rules_and_reprint() {
        let cfg = config(&["**"], &[]);
        let path = Path::new("/app/src/a.ts");
        let src = "import * as stylex from '@stylexjs/stylex';\n\
                   const fade = stylex.keyframes({ from: { opacity: 0 }, to: { opacity: 1 } });\n\
                   export const styles = stylex.create({\n\
                     root: { color: 'red', ':hover': { color: 'blue' } },\n\
                     anim: { animationName: fade },\n\
                   });\n\
                   export const attrs = stylex.props(styles.root, styles.anim);\n";

        let string_out = stylex_pass(path, src, &cfg).unwrap();
        let string_code = string_out.code.expect("string seam must modify");

        let allocator = Allocator::default();
        let parsed = Parser::new(
            &allocator,
            src,
            oxc_span::SourceType::from_path(path).unwrap(),
        )
        .parse();
        assert!(!parsed.panicked && parsed.diagnostics.is_empty());
        let mut program = parsed.program;
        let ast_out = stylex_pass_ast(&allocator, &mut program, path, src, &cfg).unwrap();

        assert!(ast_out.modified);
        assert_eq!(string_out.rules, ast_out.rules, "identical rules");
        let ast_code = oxc_codegen::Codegen::new().build(&program).code;
        assert_eq!(
            reprint(&string_code, path),
            ast_code,
            "reprints byte-equal between the two seams"
        );
    }

    #[test]
    fn ast_pass_skips_stylex_free_modules_without_mutating() {
        let cfg = config(&["**"], &[]);
        let path = Path::new("/app/src/plain.ts");
        let src = "export const stylexish = 1;\n";
        let allocator = Allocator::default();
        let parsed = Parser::new(
            &allocator,
            src,
            oxc_span::SourceType::from_path(path).unwrap(),
        )
        .parse();
        let mut program = parsed.program;
        let out = stylex_pass_ast(&allocator, &mut program, path, src, &cfg).unwrap();
        assert!(!out.modified);
        assert!(out.rules.is_empty());
        let code = oxc_codegen::Codegen::new().build(&program).code;
        assert_eq!(code, reprint(src, path), "program left untouched");
    }

    #[test]
    fn class_name_prefix_changes_generated_classnames() {
        let src = "import * as stylex from '@stylexjs/stylex';\n\
                   export const styles = stylex.create({ root: { color: 'red' } });\n";
        let default_cfg = config(&["**"], &[]);
        let prefixed = StylexPassConfig::new(
            PathBuf::from("/app"),
            PathBuf::from("/app"),
            &["**".into()],
            &[],
            true,
            false,
            Some("oj".into()),
        )
        .unwrap();
        let a = stylex_pass(Path::new("/app/src/a.ts"), src, &default_cfg).unwrap();
        let b = stylex_pass(Path::new("/app/src/a.ts"), src, &prefixed).unwrap();
        assert!(b.rules[0].class_name.starts_with("oj"));
        assert_ne!(a.rules[0].class_name, b.rules[0].class_name);
    }
}
