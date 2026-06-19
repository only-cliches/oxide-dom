#![cfg(feature = "jsx-compiler")]

use std::path::Path;

use oxc_allocator::Allocator;
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_parser::Parser;
use oxc_span::SourceType;

const FIXTURES: &[&str] = &[
    "attributeExpressions",
    "components",
    "conditionalExpressions",
    "fragments",
    "insertChildren",
    "simpleElements",
    "textInterpolation",
];

const SOLID_HELPERS: &[&str] = &[
    "createComponent",
    "createElement",
    "createTextNode",
    "effect",
    "For",
    "insert",
    "insertNode",
    "memo",
    "mergeProps",
    "setProp",
    "spread",
    "use",
];

#[test]
#[ignore = "strict parity oracle: run when changing the JSX transform"]
fn solid_universal_fixtures_match_babel_goldens() {
    let root = Path::new(
        "vendor/solid-jsx-oxc/packages/babel-plugin-jsx-dom-expressions/test/__universal_fixtures__",
    );
    for name in FIXTURES {
        let path = root.join(name).join("code.js");
        let expected_path = root.join(name).join("output.js");
        let source = std::fs::read_to_string(&path).expect("read fixture source");
        let expected = std::fs::read_to_string(&expected_path).expect("read Babel golden output");
        let compile_path = path.with_extension("jsx");
        let actual = solite::compile_component_source(&compile_path, &source)
            .unwrap_or_else(|err| panic!("{name} failed to compile: {err}"));

        let expected = canonicalize_for_parity(&expected_path, &expected);
        let actual = canonicalize_for_parity(&path, &actual);
        if actual != expected {
            panic!(
                "{name} diverged from Solid Babel universal output\n{}",
                first_difference(&expected, &actual)
            );
        }
    }
}

#[test]
fn solid_universal_fixtures_compile_to_plain_js() {
    let root = Path::new(
        "vendor/solid-jsx-oxc/packages/babel-plugin-jsx-dom-expressions/test/__universal_fixtures__",
    );
    for name in FIXTURES {
        let path = root.join(name).join("code.js");
        let source = std::fs::read_to_string(&path).expect("read fixture source");
        let compile_path = path.with_extension("jsx");
        let actual = solite::compile_component_source(&compile_path, &source)
            .unwrap_or_else(|err| panic!("{name} failed to compile: {err}"));
        canonicalize_for_parity(&compile_path, &actual);
    }
}

fn canonicalize_for_parity(path: &Path, source: &str) -> String {
    let normalized = normalize_allowed_integration_differences(source);
    canonicalize_js(path, &normalized)
}

fn normalize_allowed_integration_differences(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    for line in source.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with("import {")
            && (trimmed.contains("from \"r-custom\"") || trimmed.contains("from \"solite-runtime\""))
        {
            continue;
        }
        output.push_str(line);
        output.push('\n');
    }

    for helper in SOLID_HELPERS {
        output = output.replace(&format!("_${helper}"), &format!("__solid_{helper}"));
        output = output.replace(&format!("_sol_{helper}"), &format!("__solid_{helper}"));
    }

    normalize_generated_identifiers(&output)
}

fn normalize_generated_identifiers(source: &str) -> String {
    let mut output = String::with_capacity(source.len());
    let mut mappings: Vec<(String, String)> = Vec::new();
    let mut index = 0;
    while index < source.len() {
        let rest = &source[index..];
        if let Some(raw) = read_generated_identifier(rest) {
            let normalized = generated_identifier_mapping(&mut mappings, raw);
            output.push_str(&normalized);
            index += raw.len();
        } else {
            let ch = rest.chars().next().expect("non-empty source suffix");
            output.push(ch);
            index += ch.len_utf8();
        }
    }
    output
}

fn generated_identifier_mapping(mappings: &mut Vec<(String, String)>, raw: &str) -> String {
    if raw.starts_with("_el$") {
        return "__tmp_el".to_string();
    }
    if raw.starts_with("_ref$") {
        return "__tmp_ref".to_string();
    }
    if raw.starts_with("_c$") {
        return "__tmp_cond".to_string();
    }
    if raw.starts_with("_v$") {
        return "__tmp_value".to_string();
    }
    if let Some((_, normalized)) = mappings.iter().find(|(seen, _)| seen == raw) {
        return normalized.clone();
    }

    let prefix = if raw == "_$p" || raw == "_sol_p" || raw == "_p$" {
        "__tmp_prev"
    } else {
        "__tmp"
    };
    let normalized = format!("{prefix}{}", mappings.len() + 1);
    mappings.push((raw.to_string(), normalized.clone()));
    normalized
}

fn read_generated_identifier(source: &str) -> Option<&str> {
    for prefix in ["_el$", "_ref$", "_v$", "_c$"] {
        if let Some(rest) = source.strip_prefix(prefix) {
            let digits = rest.bytes().take_while(u8::is_ascii_digit).count();
            return Some(&source[..prefix.len() + digits]);
        }
    }
    for exact in ["_$p", "_sol_p", "_p$"] {
        if source.starts_with(exact) {
            return Some(&source[..exact.len()]);
        }
    }
    None
}

fn canonicalize_js(path: &Path, source: &str) -> String {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, source, SourceType::mjs()).parse();
    assert!(
        parsed.errors.is_empty(),
        "{} failed to parse normalized JavaScript: {:?}\n{}",
        path.display(),
        parsed.errors,
        source
    );

    let options = CodegenOptions {
        indent_char: IndentChar::Space,
        indent_width: 2,
        ..CodegenOptions::default()
    };
    Codegen::new()
        .with_options(options)
        .build(&parsed.program)
        .code
}

fn first_difference(expected: &str, actual: &str) -> String {
    for (index, (expected_line, actual_line)) in expected.lines().zip(actual.lines()).enumerate() {
        if expected_line != actual_line {
            return format!(
                "first differing line {}:\nexpected: {}\nactual:   {}",
                index + 1,
                expected_line,
                actual_line
            );
        }
    }

    let expected_lines = expected.lines().count();
    let actual_lines = actual.lines().count();
    format!(
        "line counts differ after common prefix: expected {expected_lines}, actual {actual_lines}"
    )
}
