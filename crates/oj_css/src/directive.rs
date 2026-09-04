// SPDX-License-Identifier: MIT
// Copyright (c) 2026 Raphael Amorim

//! A plugin-owned CSS directive (`@marker;`) carried through the stylesheet
//! pipeline as an oj-owned sentinel. The directive is masked to an unknown
//! at-rule before the sidecars run, survives PostCSS, Tailwind and Lightning
//! CSS as an opaque rule, and is unmasked to the plugin's CSS at the end. One
//! mechanism, tested here, instead of one per plugin.

/// The at-rule name the pipeline sees in place of a plugin directive.
pub const SENTINEL_AT_RULE: &str = "@oj-directive";

/// The token a directive is masked as: `@marker;` becomes `marker`.
pub fn directive_token(directive: &str) -> String {
    directive
        .trim()
        .trim_start_matches('@')
        .trim_end_matches(';')
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '-' })
        .collect()
}

fn sentinel(token: &str) -> String {
    format!("{SENTINEL_AT_RULE} \"{token}\";")
}

/// Replace every `directive` in `css` with its sentinel. `None` when the
/// directive does not occur, so callers can skip the unmask.
pub fn mask(css: &str, directive: &str) -> Option<String> {
    if !css.contains(directive) {
        return None;
    }
    Some(css.replace(directive, &sentinel(&directive_token(directive))))
}

/// Replace every sentinel for `directive` with `replacement`, tolerant of the
/// whitespace and quote normalization the pipeline may have applied.
pub fn unmask(css: &str, directive: &str, replacement: &str) -> String {
    let token = directive_token(directive);
    let pattern = format!(
        r#"{}\s*["']?{}["']?\s*;?"#,
        regex::escape(SENTINEL_AT_RULE),
        regex::escape(&token)
    );
    match regex::Regex::new(&pattern) {
        Ok(re) => re.replace_all(css, replacement).into_owned(),
        Err(_) => css.replace(&sentinel(&token), replacement),
    }
}

/// True when a masked directive is still in the output (the pipeline kept
/// the sentinel, as it must).
pub fn has_sentinel(css: &str) -> bool {
    css.contains(SENTINEL_AT_RULE)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_strips_at_and_semicolon() {
        assert_eq!(directive_token("@marker;"), "marker");
        assert_eq!(directive_token("@my-plugin;"), "my-plugin");
        assert_eq!(directive_token("  @a.b; "), "a-b");
    }

    #[test]
    fn mask_is_none_when_absent_and_replaces_every_occurrence() {
        assert!(mask(".a{}", "@marker;").is_none());
        let masked = mask("@marker;\n.a{}\n@marker;", "@marker;").unwrap();
        assert_eq!(masked.matches(SENTINEL_AT_RULE).count(), 2);
        assert!(!masked.contains("@marker;"));
    }

    #[test]
    fn unmask_restores_the_plugin_css() {
        let masked = mask(".a{}\n@marker;\n.b{}", "@marker;").unwrap();
        let out = unmask(&masked, "@marker;", ".m{color:red}");
        assert_eq!(out, ".a{}\n.m{color:red}\n.b{}");
        assert!(!has_sentinel(&out));
    }

    #[test]
    fn sentinel_survives_lightningcss_dev_and_minified_build() {
        let src = ".a { color: red }\n@marker;\n.b { color: blue }";
        let masked = mask(src, "@marker;").unwrap();
        let dev = crate::compile_css_dev("/src/a.css", &masked, false, &crate::CssResolve::default()).unwrap();
        assert!(has_sentinel(&dev.css), "dev output kept the sentinel: {}", dev.css);
        let out = unmask(&dev.css, "@marker;", ".m{color:green}");
        assert!(out.contains(".m{color:green}"), "{out}");
        assert!(!has_sentinel(&out));
        let a = out.find(".a").unwrap();
        let m = out.find(".m").unwrap();
        let b = out.find(".b").unwrap();
        assert!(a < m && m < b, "the directive keeps its position: {out}");

        let prod = crate::compile_css("/src/a.css", &masked, true).unwrap();
        assert!(has_sentinel(&prod.css), "minified output kept the sentinel: {}", prod.css);
        let out = unmask(&prod.css, "@marker;", ".m{color:green}");
        assert!(out.contains(".m{color:green}"), "{out}");
        assert!(!has_sentinel(&out));
    }
}
