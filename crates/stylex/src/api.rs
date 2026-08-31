//! Public transform surface (design-core.md §3): the memchr pre-gate, the
//! splice compile (CLI/harness/rolldown), and the AST compile (oj dev path).

use std::path::Path;

use memchr::memmem;
use oxc_allocator::Allocator;
use oxc_ast::ast::Program;
use oxc_parser::Parser;
use oxc_span::SourceType;

use crate::errors::StylexError;
use crate::eval::value::JsObjectMap;
use crate::imports::scan_imports;
use crate::module_resolution::{FsProvider, THEME_FILE_EXTENSION};
use crate::options::ResolvedOptions;
use crate::rules::StylexRule;
use crate::state::CompileState;
use crate::transform::ast_backend::apply_plan;
use crate::transform::visitor::transform_module;

pub struct FileContext<'s> {
    /// Absolute path the source pretends to live at.
    pub filename: &'s Path,
    pub source_text: &'s str,
    /// process.cwd() equivalent; pins `$$css` debug-string derivation.
    pub cwd: &'s Path,
}

#[derive(Debug)]
pub struct SourceCompileResult {
    pub code: String,
    pub map: Option<String>,
    pub rules: Vec<StylexRule>,
    pub modified: bool,
    /// (var name, compiled namespaces) per create call, pre-DCE, in order.
    pub create_objects: Vec<(Option<String>, std::sync::Arc<JsObjectMap>)>,
}

pub fn might_contain_stylex(source: &str, options: &ResolvedOptions) -> bool {
    options
        .import_sources
        .iter()
        .map(crate::options::ImportSource::from_specifier)
        .any(|needle| memmem::find(source.as_bytes(), needle.as_bytes()).is_some())
        // Atoms compile off a hardcoded source no importSources setting covers.
        || memmem::find(source.as_bytes(), crate::imports::ATOMS_SOURCE.as_bytes()).is_some()
        // The sx prop compiles with no stylex import in the file at all, so
        // it needs the same needle `is_dormant` already carries.
        || options
            .sx_prop_name
            .as_deref()
            .is_some_and(|sx| memmem::find(source.as_bytes(), sx.as_bytes()).is_some())
        // A rewritable import source must carry the hardcoded `.stylex`
        // suffix, so the literal is a sound pre-gate for that pass too.
        || (options.rewrite_aliases
            && memmem::find(source.as_bytes(), THEME_FILE_EXTENSION.as_bytes()).is_some())
        || has_string_escapes(source)
}

// A `\u`/`\x` escape can cook into an import-source match the raw needles
// miss (`"@stylexjs/stylex"`); parse and decide post-parse.
fn has_string_escapes(source: &str) -> bool {
    memmem::find(source.as_bytes(), b"\\u").is_some()
        || memmem::find(source.as_bytes(), b"\\x").is_some()
}

/// `None` = pre-gate skip (the file cannot reference any stylex import source).
pub fn transform_source(
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<Option<SourceCompileResult>, StylexError> {
    let allocator = Allocator::default();
    transform_source_in(&allocator, ctx, options, fs)
}

/// [`transform_source`] parsing into the caller's arena (reusable across jobs
/// via `reset()` — nothing in the result borrows it).
pub fn transform_source_in(
    allocator: &Allocator,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<Option<SourceCompileResult>, StylexError> {
    transform_source_mapped_in(allocator, ctx, options, fs, false)
}

/// [`transform_source_in`] that also builds a v3 sourcemap; emitted text is
/// byte-identical either way, so the map stays opt-in.
pub fn transform_source_mapped_in(
    allocator: &Allocator,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
    want_map: bool,
) -> Result<Option<SourceCompileResult>, StylexError> {
    if !might_contain_stylex(ctx.source_text, options) {
        return Ok(None);
    }
    let filename = ctx.filename.to_string_lossy().replace('\\', "/");
    let program = {
        let _t = crate::timings::start(crate::timings::Stage::Parse);
        parse_program(allocator, ctx.source_text, &filename)?
    };
    if is_dormant(&program, ctx.source_text, options)? {
        return Ok(Some(SourceCompileResult {
            code: ctx.source_text.to_string(),
            map: None,
            rules: Vec::new(),
            modified: false,
            create_objects: Vec::new(),
        }));
    }
    let filename_for_map = want_map.then(|| filename.clone());
    let mut state = CompileState::build(
        &program,
        options,
        Some(filename),
        ctx.cwd.to_string_lossy().replace('\\', "/"),
    )?;
    let out = {
        let _t = crate::timings::start(crate::timings::Stage::Transform);
        transform_module(&program, ctx.source_text, &mut state, fs, false, want_map)?
    };
    let map = out.splice_map.as_ref().map(|m| {
        render_sourcemap(
            m,
            filename_for_map.as_deref().unwrap_or_default(),
            ctx.source_text,
        )
    });
    Ok(Some(SourceCompileResult {
        code: out.code,
        map,
        rules: state.rules,
        modified: out.modified,
        create_objects: out.create_objects,
    }))
}

/// [`transform_source`] that also returns the fs-dependency log the cache
/// needs (crate::cache); recording costs one package.json read per hit dir.
pub fn transform_source_with_dep_log(
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<(Option<SourceCompileResult>, crate::cache::DepLog), StylexError> {
    let allocator = Allocator::default();
    transform_source_with_dep_log_in(&allocator, ctx, options, fs)
}

/// [`transform_source_with_dep_log`] in the caller's arena (see
/// [`transform_source_in`]).
pub fn transform_source_with_dep_log_in(
    allocator: &Allocator,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<(Option<SourceCompileResult>, crate::cache::DepLog), StylexError> {
    let recorder = crate::cache::RecordingFs::new(fs, ctx.cwd);
    let result = transform_source_in(allocator, ctx, options, &recorder)?;
    Ok((result, recorder.into_log()))
}

/// AST-backend result: rules plus whether the caller's program was mutated.
#[derive(Debug)]
pub struct CompileResult {
    pub modified: bool,
    pub rules: Vec<StylexRule>,
    /// (var name, compiled namespaces) per create call, pre-DCE, in order.
    pub create_objects: Vec<(Option<String>, std::sync::Arc<JsObjectMap>)>,
}

/// oj dev path: mutates the caller's AST in place; the program must be the
/// parse of `ctx.source_text` (synthesized-node spans point into it).
pub fn transform_program<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<CompileResult, StylexError> {
    if !might_contain_stylex(ctx.source_text, options) {
        return Ok(CompileResult {
            modified: false,
            rules: Vec::new(),
            create_objects: Vec::new(),
        });
    }
    if is_dormant(program, ctx.source_text, options)? {
        return Ok(CompileResult {
            modified: false,
            rules: Vec::new(),
            create_objects: Vec::new(),
        });
    }
    let filename = ctx.filename.to_string_lossy().replace('\\', "/");
    let cwd = ctx.cwd.to_string_lossy().replace('\\', "/");
    // Read-only analysis at a shorter reborrow (the AST is covariant over
    // the arena lifetime) yields an owned plan; the mutation follows.
    let (plan, modified, rules, create_objects) = {
        let program_ref: &Program<'_> = &*program;
        let mut state = CompileState::build(program_ref, options, Some(filename), cwd)?;
        let out = {
            let _t = crate::timings::start(crate::timings::Stage::Transform);
            transform_module(program_ref, ctx.source_text, &mut state, fs, true, false)?
        };
        (out.plan, out.modified, state.rules, out.create_objects)
    };
    apply_plan(allocator, program, &plan)?;
    Ok(CompileResult {
        modified,
        rules,
        create_objects,
    })
}

/// [`transform_program`] with the cache's fs-dependency log, mirroring
/// [`transform_source_with_dep_log`].
pub fn transform_program_with_dep_log<'a>(
    allocator: &'a Allocator,
    program: &mut Program<'a>,
    ctx: &FileContext<'_>,
    options: &ResolvedOptions,
    fs: &dyn FsProvider,
) -> Result<(CompileResult, crate::cache::DepLog), StylexError> {
    let recorder = crate::cache::RecordingFs::new(fs, ctx.cwd);
    let result = transform_program(allocator, program, ctx, options, &recorder)?;
    Ok((result, recorder.into_log()))
}

// Dormant needs no stylex binding AND no possible sx prop: that transform
// fires with no import at all and synthesizes one.
fn is_dormant(
    program: &Program<'_>,
    source: &str,
    options: &ResolvedOptions,
) -> Result<bool, StylexError> {
    // `rewriteAliases` runs in Program.exit over every import declaration, with
    // no stylex binding required anywhere in the file.
    if options.rewrite_aliases {
        return Ok(false);
    }
    if let Some(sx_prop) = &options.sx_prop_name
        && memmem::find(source.as_bytes(), sx_prop.as_bytes()).is_some()
    {
        return Ok(false);
    }
    let _t = crate::timings::start(crate::timings::Stage::ImportScan);
    Ok(scan_imports(program, options)?.is_dormant())
}

/// Serializes splice positions as a v3 sourcemap with the original inlined.
fn render_sourcemap(
    map: &crate::transform::js_out::SpliceMap,
    filename: &str,
    source_text: &str,
) -> String {
    let mut builder = oxc_sourcemap::SourceMapBuilder::default();
    let src_id = builder.set_source_and_content(filename, source_text);
    builder.set_file(filename);
    for &(dst_line, dst_col, src_line, src_col) in &map.tokens {
        builder.add_token(dst_line, dst_col, src_line, src_col, Some(src_id), None);
    }
    builder.into_sourcemap().to_json_string()
}

// The oracle parses every file with the typescript+jsx plugins regardless of
// extension (`input.js` with `f<T>(0)` is a generic call, not comparisons).
pub fn parse_program<'a>(
    allocator: &'a Allocator,
    source: &'a str,
    _filename: &str,
) -> Result<Program<'a>, StylexError> {
    let ret = Parser::new(allocator, source, SourceType::tsx()).parse();
    if !ret.panicked && !ret.diagnostics.has_errors() {
        return Ok(ret.program);
    }
    let detail = ret
        .diagnostics
        .errors()
        .next()
        .map(|d| d.message.to_string())
        .unwrap_or_else(|| "unknown parse error".to_string());
    Err(StylexError::parse_error(&detail))
}
