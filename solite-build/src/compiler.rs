use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::Path;

use oxc_allocator::Allocator;
use oxc_ast::ast::Statement;
use oxc_codegen::{Codegen, CodegenOptions, IndentChar};
use oxc_parser::Parser;
use oxc_semantic::SemanticBuilder;
use oxc_span::SourceType;
use oxc_transformer::{TransformOptions, Transformer};

#[derive(Debug)]
pub enum CompileError {
    Io(io::Error),
    Parse(String),
    Transform(String),
    Jsx(String),
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Parse(err) => write!(f, "{err}"),
            Self::Transform(err) => write!(f, "{err}"),
            Self::Jsx(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for CompileError {}

impl From<io::Error> for CompileError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}

pub fn compile_component_file(path: &Path) -> Result<String, CompileError> {
    let source = fs::read_to_string(path)?;
    compile_component_source(path, &source)
}

pub fn compile_component_source(path: &Path, source: &str) -> Result<String, CompileError> {
    compile_module_source(path, source)
}

pub fn compile_module_source(path: &Path, source: &str) -> Result<String, CompileError> {
    if !needs_compile(path) {
        return Ok(source.to_string());
    }

    let source_type = source_type_for_path(path);
    let lowered = if source_type.is_jsx() || looks_like_jsx(source) {
        transform_jsx_source(source)?
    } else {
        source.to_string()
    };

    if source_type.is_typescript() {
        strip_typescript(path, &lowered, source_type)
    } else {
        Ok(lowered)
    }
}

pub fn is_compilable_module(path: &Path) -> bool {
    needs_compile(path)
}

/// A module specifier referenced by a static `import`/`export … from`/`export *`
/// statement, paired with the byte range of its source-string literal (quotes
/// included).
struct SpecifierSpan {
    value: String,
    start: usize,
    end: usize,
}

/// Parse `js_source` as an ES module, rewrite each static import/export
/// specifier through `rewrite`, and return the rewritten source alongside every
/// original specifier in source order.
///
/// `rewrite` is called once per specifier in source order; returning `Some(new)`
/// replaces the specifier (re-quoted), and `None` leaves it untouched. The
/// returned specifier list is useful for walking a module graph — the `solite`
/// CLI uses it to discover relative imports while rewriting `.tsx`/`.ts`/`.jsx`
/// extensions to the `.js` keys it emits.
pub fn map_module_specifiers(
    js_source: &str,
    mut rewrite: impl FnMut(&str) -> Option<String>,
) -> Result<(String, Vec<String>), CompileError> {
    let allocator = Allocator::default();
    let parsed = Parser::new(&allocator, js_source, SourceType::mjs()).parse();
    if !parsed.errors.is_empty() {
        return Err(CompileError::Parse(
            parsed
                .errors
                .iter()
                .map(|err| err.to_string())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }

    let mut specs: Vec<SpecifierSpan> = Vec::new();
    for stmt in &parsed.program.body {
        let source = match stmt {
            Statement::ImportDeclaration(decl) => Some(&decl.source),
            Statement::ExportAllDeclaration(decl) => Some(&decl.source),
            Statement::ExportNamedDeclaration(decl) => decl.source.as_ref(),
            _ => None,
        };
        if let Some(lit) = source {
            specs.push(SpecifierSpan {
                value: lit.value.as_str().to_string(),
                start: lit.span.start as usize,
                end: lit.span.end as usize,
            });
        }
    }

    let specifiers: Vec<String> = specs.iter().map(|spec| spec.value.clone()).collect();
    let replacements: Vec<Option<String>> = specs.iter().map(|spec| rewrite(&spec.value)).collect();

    // Apply replacements back-to-front so earlier byte offsets stay valid.
    let mut output = js_source.to_string();
    for (spec, replacement) in specs.iter().zip(replacements.iter()).rev() {
        if let Some(replacement) = replacement {
            output.replace_range(spec.start..spec.end, &format!("\"{replacement}\""));
        }
    }

    Ok((output, specifiers))
}

fn needs_compile(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| matches!(ext, "jsx" | "tsx" | "ts"))
}

fn source_type_for_path(path: &Path) -> SourceType {
    SourceType::from_path(path).unwrap_or_else(|_| {
        match path.extension().and_then(|ext| ext.to_str()) {
            Some("tsx") => SourceType::tsx(),
            Some("ts") => SourceType::ts(),
            Some("jsx") => SourceType::jsx(),
            _ => SourceType::mjs(),
        }
    })
}

fn strip_typescript(
    path: &Path,
    source: &str,
    original_source_type: SourceType,
) -> Result<String, CompileError> {
    let allocator = Allocator::default();
    let parse_source_type = if original_source_type.is_typescript() {
        original_source_type.with_jsx(false)
    } else {
        SourceType::mjs()
    };

    let parsed = Parser::new(&allocator, source, parse_source_type).parse();
    if !parsed.errors.is_empty() {
        return Err(CompileError::Parse(format_diagnostics(
            "parse",
            parsed.errors,
        )));
    }

    let mut program = parsed.program;
    let semantic = SemanticBuilder::new()
        .with_excess_capacity(2.0)
        .with_enum_eval(true)
        .build(&program);
    if !semantic.errors.is_empty() {
        return Err(CompileError::Parse(format_diagnostics(
            "semantic",
            semantic.errors,
        )));
    }

    let options = TransformOptions::default();
    let transformed = Transformer::new(&allocator, path, &options)
        .build_with_scoping(semantic.semantic.into_scoping(), &mut program);
    if !transformed.errors.is_empty() {
        return Err(CompileError::Transform(format_diagnostics(
            "transform",
            transformed.errors,
        )));
    }

    let options = CodegenOptions {
        indent_char: IndentChar::Space,
        indent_width: 2,
        ..CodegenOptions::default()
    };
    Ok(Codegen::new().with_options(options).build(&program).code)
}

fn format_diagnostics(label: &str, diagnostics: Vec<oxc_diagnostics::OxcDiagnostic>) -> String {
    diagnostics
        .into_iter()
        .map(|diagnostic| format!("{label} error: {diagnostic:?}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn looks_like_jsx(source: &str) -> bool {
    let bytes = source.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'<' && can_start_jsx(source, index) {
            return true;
        }
        index += 1;
    }
    false
}

fn transform_jsx_source(source: &str) -> Result<String, CompileError> {
    let mut parser = JsxParser::new(source);
    parser.transform_program()
}

fn transform_jsx_embedded(source: &str) -> Result<String, CompileError> {
    let output = transform_jsx_source(source)?;
    let import = format!("{}\n", universal_import());
    Ok(output
        .strip_prefix(&import)
        .map_or(output.clone(), ToString::to_string))
}

#[derive(Clone, Debug)]
enum JsxNode {
    Element(JsxElement),
    Fragment(Vec<JsxChild>),
}

#[derive(Clone, Debug)]
struct JsxElement {
    tag: String,
    attrs: Vec<JsxAttribute>,
    children: Vec<JsxChild>,
}

#[derive(Clone, Debug)]
enum JsxChild {
    Node(JsxNode),
    Text(String),
    Expr(String),
}

#[derive(Clone, Debug)]
enum JsxAttribute {
    Prop { name: String, value: JsxAttrValue },
    Spread(String),
}

#[derive(Clone, Debug)]
enum JsxAttrValue {
    Bool,
    String(String),
    Expr(String),
}

struct JsxParser<'a> {
    source: &'a str,
    bytes: &'a [u8],
    len: usize,
}

impl<'a> JsxParser<'a> {
    fn new(source: &'a str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            len: source.len(),
        }
    }

    fn transform_program(&mut self) -> Result<String, CompileError> {
        let mut out = String::with_capacity(self.source.len());
        let mut index = 0;
        let mut transformed = false;
        let mut generator = JsxGenerator::new(self.source);
        while index < self.len {
            if self.skip_string_or_comment(&mut index, &mut out) {
                continue;
            }
            if self.bytes[index] == b'<' && can_start_jsx(self.source, index) {
                let (node, next) = self.parse_node(index)?;
                let mut expr = generator.node_expr(&node);
                if jsx_node_contains_this(&node) && looks_like_method_body(&out) {
                    out.push_str("const _self$ = this; ");
                    expr = expr.replace("this.", "_self$.");
                }
                out.push_str(&expr);
                index = next;
                transformed = true;
            } else {
                out.push(self.bytes[index] as char);
                index += 1;
            }
        }

        if transformed {
            Ok(format!("{}\n{}", universal_import(), out))
        } else {
            Ok(out)
        }
    }

    fn skip_string_or_comment(&self, index: &mut usize, out: &mut String) -> bool {
        match self.bytes[*index] {
            b'\'' | b'"' | b'`' => {
                let end = self.scan_string(*index);
                out.push_str(&self.source[*index..end]);
                *index = end;
                true
            }
            b'/' if self.starts_with(*index, "//") => {
                let end = self.scan_line_comment(*index);
                out.push_str(&self.source[*index..end]);
                *index = end;
                true
            }
            b'/' if self.starts_with(*index, "/*") => {
                let end = self.scan_block_comment(*index);
                out.push_str(&self.source[*index..end]);
                *index = end;
                true
            }
            _ => false,
        }
    }

    fn parse_node(&self, index: usize) -> Result<(JsxNode, usize), CompileError> {
        if self.starts_with(index, "<>") {
            return self.parse_fragment(index);
        }
        self.parse_element(index)
    }

    fn parse_fragment(&self, index: usize) -> Result<(JsxNode, usize), CompileError> {
        let mut cursor = index + 2;
        let children = self.parse_children(&mut cursor, None)?;
        if !self.starts_with(cursor, "</>") {
            return Err(CompileError::Jsx("missing JSX fragment close".into()));
        }
        Ok((JsxNode::Fragment(children), cursor + 3))
    }

    fn parse_element(&self, index: usize) -> Result<(JsxNode, usize), CompileError> {
        let mut cursor = index + 1;
        let tag = self.read_tag_name(&mut cursor)?;
        let mut attrs = Vec::new();

        loop {
            self.skip_ws(&mut cursor);
            if cursor >= self.len {
                return Err(CompileError::Jsx(format!("unterminated <{tag}>")));
            }
            if self.starts_with(cursor, "/>") {
                cursor += 2;
                return Ok((
                    JsxNode::Element(JsxElement {
                        tag,
                        attrs,
                        children: Vec::new(),
                    }),
                    cursor,
                ));
            }
            if self.bytes[cursor] == b'>' {
                cursor += 1;
                break;
            }
            attrs.push(self.parse_attr(&mut cursor)?);
        }

        let children = self.parse_children(&mut cursor, Some(&tag))?;
        if !self.starts_with(cursor, "</") {
            return Err(CompileError::Jsx(format!("missing close tag for <{tag}>")));
        }
        cursor += 2;
        let close_tag = self.read_tag_name(&mut cursor)?;
        if close_tag != tag {
            return Err(CompileError::Jsx(format!(
                "mismatched JSX close tag: expected </{tag}>, got </{close_tag}>"
            )));
        }
        self.skip_ws(&mut cursor);
        if cursor >= self.len || self.bytes[cursor] != b'>' {
            return Err(CompileError::Jsx(format!("unterminated </{tag}>")));
        }
        cursor += 1;

        Ok((
            JsxNode::Element(JsxElement {
                tag,
                attrs,
                children,
            }),
            cursor,
        ))
    }

    fn parse_children(
        &self,
        cursor: &mut usize,
        closing_tag: Option<&str>,
    ) -> Result<Vec<JsxChild>, CompileError> {
        let mut children = Vec::new();
        loop {
            if *cursor >= self.len {
                return Err(CompileError::Jsx("unterminated JSX children".into()));
            }
            if closing_tag.is_none() && self.starts_with(*cursor, "</>") {
                break;
            }
            if closing_tag.is_some() && self.starts_with(*cursor, "</") {
                break;
            }
            match self.bytes[*cursor] {
                b'<' => {
                    let (node, next) = self.parse_node(*cursor)?;
                    children.push(JsxChild::Node(node));
                    *cursor = next;
                }
                b'{' => {
                    let expr = self.read_braced_expr(cursor)?;
                    let trimmed = expr
                        .trim()
                        .strip_prefix("...")
                        .map(str::trim)
                        .unwrap_or(expr.trim());
                    if !trimmed.is_empty() && !trimmed.starts_with("/*") {
                        children.push(JsxChild::Expr(transform_jsx_embedded(trimmed)?));
                    }
                }
                _ => {
                    let start = *cursor;
                    while *cursor < self.len
                        && self.bytes[*cursor] != b'<'
                        && self.bytes[*cursor] != b'{'
                    {
                        *cursor += 1;
                    }
                    if let Some(text) = normalize_jsx_text(&self.source[start..*cursor]) {
                        children.push(JsxChild::Text(text));
                    }
                }
            }
        }
        Ok(children)
    }

    fn parse_attr(&self, cursor: &mut usize) -> Result<JsxAttribute, CompileError> {
        if self.starts_with(*cursor, "{...") {
            *cursor += 1;
            let expr = self.read_spread_expr(cursor)?;
            return Ok(JsxAttribute::Spread(transform_jsx_embedded(expr.trim())?));
        }

        let name = self.read_attr_name(cursor)?;
        self.skip_ws(cursor);
        if *cursor >= self.len || self.bytes[*cursor] != b'=' {
            return Ok(JsxAttribute::Prop {
                name,
                value: JsxAttrValue::Bool,
            });
        }
        *cursor += 1;
        self.skip_ws(cursor);
        if *cursor >= self.len {
            return Err(CompileError::Jsx(format!(
                "missing value for JSX attr {name}"
            )));
        }
        let value = match self.bytes[*cursor] {
            b'\'' | b'"' => JsxAttrValue::String(self.read_quoted(cursor)?),
            b'{' => JsxAttrValue::Expr(transform_jsx_embedded(
                self.read_braced_expr(cursor)?.trim(),
            )?),
            _ => {
                return Err(CompileError::Jsx(format!(
                    "unsupported JSX attr value for {name}"
                )));
            }
        };
        Ok(JsxAttribute::Prop { name, value })
    }

    fn read_tag_name(&self, cursor: &mut usize) -> Result<String, CompileError> {
        let start = *cursor;
        while *cursor < self.len && is_tag_char(self.bytes[*cursor]) {
            *cursor += 1;
        }
        if *cursor == start {
            return Err(CompileError::Jsx("expected JSX tag name".into()));
        }
        Ok(self.source[start..*cursor].to_string())
    }

    fn read_attr_name(&self, cursor: &mut usize) -> Result<String, CompileError> {
        let start = *cursor;
        while *cursor < self.len && is_attr_char(self.bytes[*cursor]) {
            *cursor += 1;
        }
        if *cursor == start {
            return Err(CompileError::Jsx("expected JSX attr name".into()));
        }
        Ok(self.source[start..*cursor].to_string())
    }

    fn read_quoted(&self, cursor: &mut usize) -> Result<String, CompileError> {
        let quote = self.bytes[*cursor];
        *cursor += 1;
        let start = *cursor;
        while *cursor < self.len {
            if self.bytes[*cursor] == b'\\' {
                *cursor += 2;
                continue;
            }
            if self.bytes[*cursor] == quote {
                let value = self.source[start..*cursor].to_string();
                *cursor += 1;
                return Ok(value);
            }
            *cursor += 1;
        }
        Err(CompileError::Jsx(
            "unterminated JSX string attribute".into(),
        ))
    }

    fn read_braced_expr(&self, cursor: &mut usize) -> Result<String, CompileError> {
        if self.bytes[*cursor] != b'{' {
            return Err(CompileError::Jsx("expected `{`".into()));
        }
        let start = *cursor + 1;
        let end = self.scan_balanced(*cursor, b'{', b'}')?;
        let expr = self.source[start..end - 1].to_string();
        *cursor = end;
        Ok(expr)
    }

    fn read_spread_expr<'b>(&'b self, cursor: &mut usize) -> Result<&'b str, CompileError> {
        if !self.starts_with(*cursor, "...") {
            return Err(CompileError::Jsx("expected JSX spread".into()));
        }
        *cursor += 3;
        let start = *cursor;
        let mut depth = 0usize;
        while *cursor < self.len {
            match self.bytes[*cursor] {
                b'\'' | b'"' | b'`' => *cursor = self.scan_string(*cursor),
                b'/' if self.starts_with(*cursor, "//") => {
                    *cursor = self.scan_line_comment(*cursor)
                }
                b'/' if self.starts_with(*cursor, "/*") => {
                    *cursor = self.scan_block_comment(*cursor)
                }
                b'{' | b'(' | b'[' => {
                    depth += 1;
                    *cursor += 1;
                }
                b'}' if depth == 0 => {
                    let expr = &self.source[start..*cursor];
                    *cursor += 1;
                    return Ok(expr);
                }
                b'}' | b')' | b']' => {
                    depth = depth.saturating_sub(1);
                    *cursor += 1;
                }
                _ => *cursor += 1,
            }
        }
        Err(CompileError::Jsx("unterminated JSX spread".into()))
    }

    fn scan_balanced(&self, start: usize, open: u8, close: u8) -> Result<usize, CompileError> {
        let mut cursor = start;
        let mut depth = 0usize;
        while cursor < self.len {
            match self.bytes[cursor] {
                b'\'' | b'"' | b'`' => cursor = self.scan_string(cursor),
                b'/' if self.starts_with(cursor, "//") => cursor = self.scan_line_comment(cursor),
                b'/' if self.starts_with(cursor, "/*") => cursor = self.scan_block_comment(cursor),
                byte if byte == open => {
                    depth += 1;
                    cursor += 1;
                }
                byte if byte == close => {
                    depth -= 1;
                    cursor += 1;
                    if depth == 0 {
                        return Ok(cursor);
                    }
                }
                _ => cursor += 1,
            }
        }
        Err(CompileError::Jsx("unterminated JSX expression".into()))
    }

    fn scan_string(&self, start: usize) -> usize {
        let quote = self.bytes[start];
        let mut cursor = start + 1;
        while cursor < self.len {
            if self.bytes[cursor] == b'\\' {
                cursor += 2;
            } else if self.bytes[cursor] == quote {
                return cursor + 1;
            } else {
                cursor += 1;
            }
        }
        self.len
    }

    fn scan_line_comment(&self, start: usize) -> usize {
        let mut cursor = start + 2;
        while cursor < self.len && self.bytes[cursor] != b'\n' {
            cursor += 1;
        }
        cursor
    }

    fn scan_block_comment(&self, start: usize) -> usize {
        let mut cursor = start + 2;
        while cursor + 1 < self.len {
            if self.bytes[cursor] == b'*' && self.bytes[cursor + 1] == b'/' {
                return cursor + 2;
            }
            cursor += 1;
        }
        self.len
    }

    fn skip_ws(&self, cursor: &mut usize) {
        while *cursor < self.len && self.bytes[*cursor].is_ascii_whitespace() {
            *cursor += 1;
        }
    }

    fn starts_with(&self, index: usize, needle: &str) -> bool {
        self.source[index..].starts_with(needle)
    }
}

struct JsxGenerator {
    next_id: usize,
    next_ref: usize,
    static_bindings: HashSet<String>,
    static_text_values: HashMap<String, String>,
}

#[derive(Clone)]
struct NativeResult {
    id: String,
    declarations: Vec<String>,
    exprs: Vec<String>,
    dynamics: Vec<DynamicProp>,
}

#[derive(Clone)]
struct DynamicProp {
    elem: String,
    key: String,
    value: String,
}

#[derive(Clone)]
struct ChildBuild {
    id: Option<String>,
    expr: Option<String>,
    declarations: Vec<String>,
    exprs: Vec<String>,
    dynamics: Vec<DynamicProp>,
}

impl JsxGenerator {
    fn new(source: &str) -> Self {
        Self {
            next_id: 0,
            next_ref: 0,
            static_bindings: collect_static_bindings(source),
            static_text_values: collect_static_text_values(source),
        }
    }

    fn node_expr(&mut self, node: &JsxNode) -> String {
        match node {
            JsxNode::Element(element) if is_component_tag(&element.tag) => {
                self.component_expr(element)
            }
            JsxNode::Element(element) => {
                let result = self.native_result(element);
                self.create_template(result)
            }
            JsxNode::Fragment(children) => self.children_value(children),
        }
    }

    fn native_result(&mut self, element: &JsxElement) -> NativeResult {
        let el = self.el_var();
        let mut result = NativeResult {
            id: el.clone(),
            declarations: vec![format!(
                "{el} = _sol_createElement({})",
                js_string(&element.tag)
            )],
            exprs: Vec::new(),
            dynamics: Vec::new(),
        };

        let mut children = element.children.clone();
        let has_children = !children.is_empty();
        let (attrs, spread) = self.process_spreads(&element.attrs, &el, has_children);

        for attr in attrs {
            match attr {
                JsxAttribute::Spread(_) => {}
                JsxAttribute::Prop { name, value } if name == "children" => {
                    if !has_children {
                        children.push(JsxChild::Expr(attr_value_expr(&value)));
                    }
                }
                JsxAttribute::Prop {
                    name,
                    value: JsxAttrValue::Expr(expr),
                } if name == "ref" => result
                    .exprs
                    .insert(0, self.native_ref_statement(&el, &expr)),
                JsxAttribute::Prop { name, value } if name.starts_with("use:") => {
                    let directive = &name[4..];
                    let value = match value {
                        JsxAttrValue::Bool => "() => true".to_string(),
                        JsxAttrValue::String(value) => format!("() => {}", js_string(&value)),
                        JsxAttrValue::Expr(expr) => {
                            format!("() => {}", clean_once_expr(&prop_expr(&expr)))
                        }
                    };
                    result
                        .exprs
                        .insert(0, format!("_sol_use({directive}, {el}, {value});"));
                }
                JsxAttribute::Prop { name, value } => {
                    let expr = attr_value_expr(&value);
                    if matches!(value, JsxAttrValue::Expr(_)) && is_dynamic_expr(&expr) {
                        result.dynamics.push(DynamicProp {
                            elem: el.clone(),
                            key: name,
                            value: clean_once_expr(&expr),
                        });
                    } else {
                        result.exprs.push(format!(
                            "_sol_setProp({el}, {}, {});",
                            js_string(&name),
                            clean_once_expr(&expr)
                        ));
                    }
                }
            }
        }

        if let Some(spread) = spread {
            result.exprs.push(spread);
        }

        self.transform_native_children(&el, &children, &mut result);
        result
    }

    fn process_spreads(
        &mut self,
        attrs: &[JsxAttribute],
        el: &str,
        has_children: bool,
    ) -> (Vec<JsxAttribute>, Option<String>) {
        if !attrs
            .iter()
            .any(|attr| matches!(attr, JsxAttribute::Spread(_)))
        {
            return (attrs.to_vec(), None);
        }

        let mut filtered = Vec::new();
        let mut spread_args = Vec::new();
        let mut running_object = Vec::new();
        let mut first_spread = false;
        let mut dynamic_spread = false;

        for attr in attrs {
            match attr {
                JsxAttribute::Spread(expr) => {
                    first_spread = true;
                    if !running_object.is_empty() {
                        spread_args.push(format!("{{ {} }}", running_object.join(", ")));
                        running_object.clear();
                    }
                    let expr = prop_expr(expr);
                    if is_dynamic_expr(&expr) {
                        dynamic_spread = true;
                        spread_args.push(
                            zero_arg_call_callee(&expr)
                                .unwrap_or_else(|| format!("() => {}", expression_body(&expr))),
                        );
                    } else {
                        spread_args.push(expr);
                    }
                }
                JsxAttribute::Prop { name, value }
                    if (first_spread
                        || matches!(value, JsxAttrValue::Expr(expr) if is_dynamic_expr(expr)))
                        && can_native_spread(name) =>
                {
                    let expr = attr_value_expr(value);
                    if matches!(value, JsxAttrValue::Expr(_)) && is_dynamic_expr(&expr) {
                        running_object.push(format!(
                            "get {}() {{ return {}; }}",
                            object_key(name),
                            clean_once_expr(&expr)
                        ));
                    } else {
                        running_object.push(format!(
                            "{}: {}",
                            object_key(name),
                            clean_once_expr(&expr)
                        ));
                    }
                }
                _ => filtered.push(attr.clone()),
            }
        }

        if !running_object.is_empty() {
            spread_args.push(format!("{{ {} }}", running_object.join(", ")));
        }

        let props = if spread_args.len() == 1 && !dynamic_spread {
            spread_args.pop().unwrap_or_else(String::new)
        } else {
            format!("_sol_mergeProps({})", spread_args.join(", "))
        };

        (
            filtered,
            Some(format!("_sol_spread({el}, {props}, {has_children});")),
        )
    }

    fn transform_native_children(
        &mut self,
        el: &str,
        children: &[JsxChild],
        result: &mut NativeResult,
    ) {
        let multi = children.len() > 1;
        let children = self.merge_native_text_children(children);
        let mut child_results = children
            .iter()
            .map(|child| self.build_child(child, multi))
            .collect::<Vec<_>>();

        let mut appends = Vec::new();
        for index in 0..child_results.len() {
            let child = child_results[index].clone();
            if let Some(id) = &child.id {
                appends.push(format!("_sol_insertNode({el}, {id});"));
                result.declarations.extend(child.declarations);
                result.exprs.extend(child.exprs);
                result.dynamics.extend(child.dynamics);
            } else if let Some(expr) = &child.expr {
                let anchor = next_child_id(&child_results, index)
                    .map_or("null".to_string(), ToString::to_string);
                if multi {
                    result
                        .exprs
                        .push(format!("_sol_insert({el}, {expr}, {anchor});"));
                } else {
                    result.exprs.push(format!("_sol_insert({el}, {expr});"));
                }
            }
        }

        child_results.clear();
        if !appends.is_empty() {
            result.exprs.splice(0..0, appends);
        }
    }

    fn build_child(&mut self, child: &JsxChild, multi: bool) -> ChildBuild {
        match child {
            JsxChild::Text(text) if multi => self.text_child(text, true),
            JsxChild::Text(text) => self.text_child(text, false),
            JsxChild::Expr(expr) if literal_text_value(&prop_expr(expr)).is_some() => {
                let text = match literal_text_value(&prop_expr(expr)) {
                    Some(text) => text,
                    None => {
                        return ChildBuild {
                            id: None,
                            expr: Some(child_expr(expr)),
                            declarations: Vec::new(),
                            exprs: Vec::new(),
                            dynamics: Vec::new(),
                        };
                    }
                };
                self.text_child(&text, multi)
            }
            JsxChild::Expr(expr) => ChildBuild {
                id: None,
                expr: Some(child_expr(expr)),
                declarations: Vec::new(),
                exprs: Vec::new(),
                dynamics: Vec::new(),
            },
            JsxChild::Node(JsxNode::Element(element)) if !is_component_tag(&element.tag) => {
                let native = self.native_result(element);
                ChildBuild {
                    id: Some(native.id),
                    expr: None,
                    declarations: native.declarations,
                    exprs: native.exprs,
                    dynamics: native.dynamics,
                }
            }
            JsxChild::Node(node) => ChildBuild {
                id: None,
                expr: Some(self.node_expr(node)),
                declarations: Vec::new(),
                exprs: Vec::new(),
                dynamics: Vec::new(),
            },
        }
    }

    fn text_child(&mut self, text: &str, multi: bool) -> ChildBuild {
        if multi {
            let id = self.el_var();
            ChildBuild {
                id: Some(id.clone()),
                expr: None,
                declarations: vec![format!(
                    "{id} = _sol_createTextNode(`{}`)",
                    escape_template_text(text)
                )],
                exprs: Vec::new(),
                dynamics: Vec::new(),
            }
        } else {
            ChildBuild {
                id: Some(format!(
                    "_sol_createTextNode(`{}`)",
                    escape_template_text(text)
                )),
                expr: None,
                declarations: Vec::new(),
                exprs: Vec::new(),
                dynamics: Vec::new(),
            }
        }
    }

    fn merge_native_text_children(&self, children: &[JsxChild]) -> Vec<JsxChild> {
        let mut merged = Vec::new();
        for child in children {
            if let Some(text) = self.child_text_value(child) {
                if let Some(JsxChild::Text(previous)) = merged.last_mut() {
                    previous.push_str(&text);
                } else {
                    merged.push(JsxChild::Text(text));
                }
            } else {
                merged.push(child.clone());
            }
        }
        merged
    }

    fn child_text_value(&self, child: &JsxChild) -> Option<String> {
        match child {
            JsxChild::Text(text) => Some(text.clone()),
            JsxChild::Expr(expr) => self
                .eval_static_text_expr(&prop_expr(expr))
                .map(|text| escape_native_expression_text(&text)),
            _ => None,
        }
    }

    fn eval_static_text_expr(&self, expr: &str) -> Option<String> {
        let expr = expr.trim();
        if let Some(value) = literal_text_value(expr) {
            return Some(value);
        }
        if let Some(value) = self.static_text_values.get(expr) {
            return Some(value.clone());
        }
        let parts = split_top_level_plus(expr)?;
        let mut output = String::new();
        for part in parts {
            output.push_str(&self.eval_static_text_expr(part)?);
        }
        Some(output)
    }

    fn create_template(&self, result: NativeResult) -> String {
        if result.exprs.is_empty() && result.dynamics.is_empty() && result.declarations.len() == 1 {
            return result.declarations[0]
                .split_once(" = ")
                .map(|(_, init)| init.to_string())
                .unwrap_or_else(|| result.id);
        }

        let mut statements = Vec::new();
        statements.push(format!("var {};", result.declarations.join(", ")));
        statements.extend(result.exprs);
        statements.extend(wrap_dynamics(&result.dynamics));
        statements.push(format!("return {};", result.id));
        format!("(() => {{ {} }})()", statements.join(" "))
    }

    fn component_expr(&mut self, element: &JsxElement) -> String {
        let mut chunks = Vec::new();
        let mut current_props = Vec::new();
        let mut force_merge = false;

        for attr in &element.attrs {
            match attr {
                JsxAttribute::Spread(expr) => {
                    if !current_props.is_empty() {
                        chunks.push(format!("{{ {} }}", current_props.join(", ")));
                        current_props.clear();
                    }
                    let expr = prop_expr(expr);
                    if is_dynamic_expr(&expr) {
                        force_merge = true;
                        chunks.push(
                            zero_arg_call_callee(&expr)
                                .unwrap_or_else(|| format!("() => {}", expression_body(&expr))),
                        );
                    } else {
                        chunks.push(expr);
                    }
                }
                JsxAttribute::Prop { name, value } => {
                    let prop = self.component_prop(name, value);
                    current_props.push(prop);
                }
            }
        }

        if !element.children.is_empty() {
            current_props.push(self.component_children_prop(&element.children));
        }
        if !current_props.is_empty() {
            chunks.push(format!("{{ {} }}", current_props.join(", ")));
        }

        let props = match chunks.len() {
            0 => "{}".to_string(),
            1 if !force_merge => chunks.pop().unwrap_or_else(String::new),
            _ => format!("_sol_mergeProps({})", chunks.join(", ")),
        };
        format!(
            "_sol_createComponent({}, {props})",
            self.component_tag_expr(&element.tag)
        )
    }

    fn children_value(&mut self, children: &[JsxChild]) -> String {
        let values = children
            .iter()
            .map(|child| match child {
                JsxChild::Text(text) => js_string(&decode_component_text(text)),
                JsxChild::Expr(expr) => child_value_expr(expr),
                JsxChild::Node(node) => self.node_expr(node),
            })
            .collect::<Vec<_>>();
        match values.len() {
            0 => "[]".to_string(),
            1 => values[0].clone(),
            _ => format!("[{}]", values.join(", ")),
        }
    }

    fn el_var(&mut self) -> String {
        self.next_id += 1;
        if self.next_id == 1 {
            "_el$".to_string()
        } else {
            format!("_el${}", self.next_id)
        }
    }

    fn ref_var(&mut self) -> String {
        self.next_ref += 1;
        if self.next_ref == 1 {
            "_ref$".to_string()
        } else {
            format!("_ref${}", self.next_ref)
        }
    }

    fn native_ref_statement(&mut self, element: &str, expr: &str) -> String {
        let expr = clean_once_expr(&prop_expr(expr));
        if is_static_ref(&expr, &self.static_bindings) || is_function_expr(&expr) {
            format!("_sol_use({expr}, {element});")
        } else {
            let ref_id = self.ref_var();
            if is_assignable_ref(&expr) {
                format!(
                    "var {ref_id} = {expr}; typeof {ref_id} === \"function\" ? _sol_use({ref_id}, {element}) : {expr} = {element};"
                )
            } else {
                format!(
                    "var {ref_id} = {expr}; typeof {ref_id} === \"function\" && _sol_use({ref_id}, {element});"
                )
            }
        }
    }

    fn component_prop(&mut self, name: &str, value: &JsxAttrValue) -> String {
        if name == "ref" {
            if let JsxAttrValue::Expr(expr) = value {
                let expr = clean_once_expr(&prop_expr(expr));
                if is_static_ref(&expr, &self.static_bindings) || is_function_expr(&expr) {
                    return format!("ref: {expr}");
                }

                let ref_id = self.ref_var();
                if is_assignable_ref(&expr) {
                    return format!(
                        "ref(r$) {{ var {ref_id} = {expr}; typeof {ref_id} === \"function\" ? {ref_id}(r$) : {expr} = r$; }}"
                    );
                }
                return format!(
                    "ref(r$) {{ var {ref_id} = {expr}; typeof {ref_id} === \"function\" && {ref_id}(r$); }}"
                );
            }
        }

        let key = object_key(name);
        match value {
            JsxAttrValue::Bool => format!("{key}: true"),
            JsxAttrValue::String(value) => {
                format!("{key}: {}", js_string(&decode_component_attr(value)))
            }
            JsxAttrValue::Expr(expr) => {
                let raw = prop_expr(expr);
                let once = is_once_expr(&raw);
                let expr = clean_once_expr(&raw);
                if !once && is_dynamic_expr(&expr) {
                    format!("get {key}() {{ return {}; }}", component_getter_expr(&expr))
                } else {
                    format!("{key}: {expr}")
                }
            }
        }
    }

    fn component_children_prop(&mut self, children: &[JsxChild]) -> String {
        if let [JsxChild::Text(text)] = children {
            return format!("children: {}", js_string(&decode_component_text(text)));
        }
        if let [JsxChild::Expr(expr)] = children {
            let raw = prop_expr(expr);
            let once = is_once_expr(&raw);
            let expr = clean_once_expr(&raw);
            if once || !is_dynamic_expr(&expr) || is_function_expr(&expr) {
                return format!("children: {expr}");
            }
            return format!(
                "get children() {{ return {}; }}",
                component_getter_expr(&expr)
            );
        }
        if let [JsxChild::Node(JsxNode::Element(element))] = children {
            if !is_component_tag(&element.tag) {
                let result = self.native_result(element);
                let mut statements = Vec::new();
                statements.push(format!("var {};", result.declarations.join(", ")));
                statements.extend(result.exprs);
                statements.extend(wrap_dynamics(&result.dynamics));
                statements.push(format!("return {};", result.id));
                return format!("get children() {{ {} }}", statements.join(" "));
            }
        }
        format!(
            "get children() {{ return {}; }}",
            self.children_value(children)
        )
    }

    fn component_tag_expr(&self, tag: &str) -> String {
        if is_builtin_component(tag) && !self.static_bindings.contains(tag) {
            format!("_sol_{tag}")
        } else {
            tag.to_string()
        }
    }
}

fn is_assignable_ref(expr: &str) -> bool {
    if expr.contains("=>")
        || expr.contains('?')
        || expr.contains(':')
        || expr.contains("??")
        || expr.contains("||")
        || expr.contains("&&")
    {
        return false;
    }
    let trimmed = expr.trim();
    if is_identifier(trimmed) {
        return true;
    }
    if !trimmed.contains('.') {
        return false;
    }
    let parts = trimmed.split('.').collect::<Vec<_>>();
    for (index, part) in parts.iter().enumerate() {
        let part = part.trim();
        if part.is_empty() {
            return false;
        }
        if is_identifier(part) {
            continue;
        }
        if index + 1 < parts.len() && part.ends_with(')') && !part.contains("=>") {
            continue;
        }
        return false;
    }
    true
}

fn prop_expr(expr: &str) -> String {
    let trimmed = expr.trim();
    if trimmed.is_empty() {
        "undefined".to_string()
    } else {
        trimmed.to_string()
    }
}

fn child_expr(expr: &str) -> String {
    let trimmed = expr.trim();
    let once = is_once_expr(trimmed);
    let cleaned = clean_once_expr(trimmed);
    if once {
        cleaned
    } else if let Some(callee) = zero_arg_call_callee(&cleaned) {
        callee
    } else if let Some(wrapped) = wrapped_dynamic_condition_child(&cleaned) {
        wrapped
    } else if let Some(wrapped) = wrapped_dynamic_or_child(&cleaned) {
        wrapped
    } else if let Some(wrapped) = wrapped_dynamic_nullish_child(&cleaned) {
        wrapped
    } else if let Some(wrapped) = wrapped_dynamic_and_child(&cleaned) {
        wrapped
    } else if cleaned.starts_with("function")
        || is_arrow_function(&cleaned)
        || !is_dynamic_expr(&cleaned)
    {
        cleaned
    } else if cleaned.contains(',') {
        format!("() => ({cleaned})")
    } else {
        format!("() => {cleaned}")
    }
}

fn wrapped_dynamic_and_child(expr: &str) -> Option<String> {
    let (predicate, value) = split_top_level_last(expr, "&&")?;
    if !is_dynamic_expr(predicate) {
        return None;
    }
    let condition = memo_condition_expr(predicate);
    Some(format!(
        "(() => {{ var _c$ = _sol_memo(() => {condition}); return () => _c$() && {value}; }})()"
    ))
}

fn wrapped_dynamic_or_child(expr: &str) -> Option<String> {
    let (left, fallback) = split_top_level_last(expr, "||")?;
    let (predicate, value) = split_top_level_last(trim_wrapping_parens(left), "&&")?;
    if !is_dynamic_expr(predicate) {
        return None;
    }
    let condition = memo_condition_expr(predicate);
    Some(format!(
        "(() => {{ var _c$ = _sol_memo(() => {condition}); return () => (_c$() && {value}) || {fallback}; }})()"
    ))
}

fn wrapped_dynamic_nullish_child(expr: &str) -> Option<String> {
    let (left, fallback) = split_top_level_first(expr, "??")?;
    let (predicate, value) = split_top_level_last(trim_wrapping_parens(left), "&&")?;
    if !is_dynamic_expr(predicate) {
        return None;
    }
    let condition = memo_condition_expr(predicate);
    Some(format!(
        "(() => {{ var _c$ = _sol_memo(() => {condition}); return () => (_c$() && {value}) ?? {fallback}; }})()"
    ))
}

fn wrapped_dynamic_condition_child(expr: &str) -> Option<String> {
    let (condition, consequent, alternate) = split_top_level_conditional(expr)?;
    if condition.contains("?.") || !is_dynamic_expr(condition) {
        return None;
    }
    let condition = memo_condition_expr(condition);
    let consequent = inline_nested_condition(consequent);
    let alternate = inline_nested_condition(alternate);
    Some(format!(
        "(() => {{ var _c$ = _sol_memo(() => {condition}); return () => (_c$() ? {consequent} : {alternate}); }})()"
    ))
}

fn split_top_level_last<'a>(expr: &'a str, operator: &str) -> Option<(&'a str, &'a str)> {
    let bytes = expr.as_bytes();
    let op = operator.as_bytes();
    let mut depth = 0usize;
    let mut cursor = 0;
    let mut found = None;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = scan_js_string(bytes, cursor),
            b'(' | b'[' | b'{' => {
                depth += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            _ if depth == 0 && bytes[cursor..].starts_with(op) => {
                found = Some(cursor);
                cursor += op.len();
            }
            _ => cursor += 1,
        }
    }
    let index = found?;
    Some((expr[..index].trim(), expr[index + operator.len()..].trim()))
}

fn split_top_level_first<'a>(expr: &'a str, operator: &str) -> Option<(&'a str, &'a str)> {
    let bytes = expr.as_bytes();
    let op = operator.as_bytes();
    let mut depth = 0usize;
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = scan_js_string(bytes, cursor),
            b'(' | b'[' | b'{' => {
                depth += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            _ if depth == 0 && bytes[cursor..].starts_with(op) => {
                return Some((
                    expr[..cursor].trim(),
                    expr[cursor + operator.len()..].trim(),
                ));
            }
            _ => cursor += 1,
        }
    }
    None
}

fn split_top_level_conditional(expr: &str) -> Option<(&str, &str, &str)> {
    let bytes = expr.as_bytes();
    let mut depth = 0usize;
    let mut question = None;
    let mut cursor = 0;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = scan_js_string(bytes, cursor),
            b'(' | b'[' | b'{' => {
                depth += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            b'?' if depth == 0
                && !bytes
                    .get(cursor + 1)
                    .is_some_and(|byte| *byte == b'.' || *byte == b'?') =>
            {
                question = Some(cursor);
                cursor += 1;
                break;
            }
            _ => cursor += 1,
        }
    }
    let question = question?;
    depth = 0;
    let mut nested = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = scan_js_string(bytes, cursor),
            b'(' | b'[' | b'{' => {
                depth += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            b'?' if depth == 0
                && !bytes
                    .get(cursor + 1)
                    .is_some_and(|byte| *byte == b'.' || *byte == b'?') =>
            {
                nested += 1;
                cursor += 1;
            }
            b':' if depth == 0 && nested > 0 => {
                nested -= 1;
                cursor += 1;
            }
            b':' if depth == 0 => {
                return Some((
                    expr[..question].trim(),
                    expr[question + 1..cursor].trim(),
                    expr[cursor + 1..].trim(),
                ));
            }
            _ => cursor += 1,
        }
    }
    None
}

fn inline_nested_condition(expr: &str) -> String {
    let Some((condition, consequent, alternate)) =
        split_top_level_conditional(trim_wrapping_parens(expr))
    else {
        return expr.to_string();
    };
    if is_dynamic_expr(condition) {
        format!(
            "(_sol_memo(() => {})() ? {} : {})",
            memo_condition_expr(condition),
            consequent,
            alternate
        )
    } else {
        expr.to_string()
    }
}

fn memo_condition_expr(condition: &str) -> String {
    let condition = condition.trim();
    if condition.contains('>') || condition.contains('<') || condition.contains("==") {
        condition.to_string()
    } else if condition.contains("&&") || condition.contains("||") {
        format!("!!({condition})")
    } else {
        format!("!!{condition}")
    }
}

fn trim_wrapping_parens(expr: &str) -> &str {
    let trimmed = expr.trim();
    if trimmed.starts_with('(') && trimmed.ends_with(')') {
        &trimmed[1..trimmed.len() - 1]
    } else {
        trimmed
    }
}

fn scan_js_string(bytes: &[u8], start: usize) -> usize {
    let quote = bytes[start];
    let mut cursor = start + 1;
    while cursor < bytes.len() {
        if bytes[cursor] == b'\\' {
            cursor += 2;
        } else if bytes[cursor] == quote {
            return cursor + 1;
        } else {
            cursor += 1;
        }
    }
    bytes.len()
}

fn child_value_expr(expr: &str) -> String {
    let raw = prop_expr(expr);
    let once = is_once_expr(&raw);
    let cleaned = clean_once_expr(&raw);
    if !once && is_dynamic_expr(&cleaned) && !is_function_expr(&cleaned) {
        if let Some(callee) = zero_arg_call_callee(&cleaned) {
            format!("_sol_memo({callee})")
        } else {
            format!("_sol_memo(() => {})", component_getter_expr(&cleaned))
        }
    } else {
        cleaned
    }
}

fn component_getter_expr(expr: &str) -> String {
    if let Some((condition, consequent, alternate)) = split_top_level_conditional(expr) {
        if !condition.contains("?.")
            && is_dynamic_expr(condition)
            && (is_dynamic_expr(consequent) || is_dynamic_expr(alternate))
        {
            return format!(
                "_sol_memo(() => {})() ? {} : {}",
                memo_condition_expr(condition),
                inline_nested_condition(consequent),
                inline_nested_condition(alternate)
            );
        }
    }
    if let Some((predicate, value)) = split_top_level_last(expr, "&&") {
        if is_dynamic_expr(predicate) && is_dynamic_expr(value) {
            return format!(
                "_sol_memo(() => {})() && {value}",
                memo_condition_expr(predicate)
            );
        }
    }
    if let Some((left, fallback)) = split_top_level_last(expr, "||") {
        if let Some((predicate, value)) = split_top_level_last(trim_wrapping_parens(left), "&&") {
            if is_dynamic_expr(predicate) {
                return format!(
                    "_sol_memo(() => {})() && {value} || {fallback}",
                    memo_condition_expr(predicate)
                );
            }
        }
    }
    expr.to_string()
}

fn attr_value_expr(value: &JsxAttrValue) -> String {
    match value {
        JsxAttrValue::Bool => "true".to_string(),
        JsxAttrValue::String(value) => js_string(value),
        JsxAttrValue::Expr(expr) => prop_expr(expr),
    }
}

fn clean_once_expr(expr: &str) -> String {
    expr.trim()
        .strip_prefix("/*@once*/")
        .map(str::trim)
        .unwrap_or_else(|| expr.trim())
        .to_string()
}

fn is_once_expr(expr: &str) -> bool {
    expr.trim().starts_with("/*@once*/")
}

fn is_dynamic_expr(expr: &str) -> bool {
    let expr = expr.trim();
    if expr.is_empty() || expr.starts_with("/*@once*/") {
        return false;
    }
    if is_identifier(expr)
        || matches!(expr, "true" | "false" | "null" | "undefined")
        || expr.starts_with('"')
        || expr.starts_with('\'')
        || expr.starts_with('`')
        || expr.chars().all(|c| c.is_ascii_digit())
        || is_function_expr(expr)
        || is_static_control_expression(expr)
    {
        return false;
    }
    expr.contains('.')
        || expr.contains("()")
        || expr.contains('?')
        || expr.contains("&&")
        || expr.contains("||")
        || expr.contains("...")
        || expr.contains('(')
}

fn is_static_control_expression(expr: &str) -> bool {
    (expr.contains('?') || expr.contains("&&") || expr.contains("||"))
        && !expr.contains('.')
        && !expr.contains('(')
        && !expr.contains("=>")
}

fn zero_arg_call_callee(expr: &str) -> Option<String> {
    let trimmed = expr.trim();
    let callee = trimmed.strip_suffix("()")?.trim();
    if is_identifier(callee) {
        Some(callee.to_string())
    } else {
        None
    }
}

fn expression_body(expr: &str) -> String {
    let trimmed = expr.trim();
    if trimmed.starts_with('{') || trimmed.contains(',') {
        format!("({expr})")
    } else {
        trimmed.to_string()
    }
}

fn object_key(name: &str) -> String {
    if is_identifier(name) && name != "class" {
        name.to_string()
    } else {
        format!("[{}]", js_string(name))
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first == '$' || first.is_ascii_alphabetic())
        && chars.all(|c| c == '_' || c == '$' || c.is_ascii_alphanumeric())
}

fn is_arrow_function(expr: &str) -> bool {
    let expr = expr.trim_start();
    if expr.starts_with('(') || expr.starts_with("async ") {
        return expr.contains("=>");
    }
    let mut end = 0;
    for (index, ch) in expr.char_indices() {
        if index == 0 {
            if !(ch == '_' || ch == '$' || ch.is_ascii_alphabetic()) {
                return false;
            }
            end = ch.len_utf8();
            continue;
        }
        if ch == '_' || ch == '$' || ch.is_ascii_alphanumeric() {
            end = index + ch.len_utf8();
        } else {
            break;
        }
    }
    end > 0 && expr[end..].trim_start().starts_with("=>")
}

fn is_function_expr(expr: &str) -> bool {
    expr.trim_start().starts_with("function") || is_arrow_function(expr)
}

fn is_static_ref(expr: &str, bindings: &HashSet<String>) -> bool {
    is_identifier(expr) && bindings.contains(expr)
}

fn can_native_spread(name: &str) -> bool {
    name != "ref" && name != "children" && !name.starts_with("use:")
}

fn next_child_id(children: &[ChildBuild], index: usize) -> Option<&str> {
    children
        .iter()
        .skip(index + 1)
        .find_map(|child| child.id.as_deref())
}

fn wrap_dynamics(dynamics: &[DynamicProp]) -> Vec<String> {
    match dynamics {
        [] => Vec::new(),
        [dynamic] => vec![format!(
            "_sol_effect(_$p => _sol_setProp({}, {}, {}, _$p));",
            dynamic.elem,
            js_string(&dynamic.key),
            dynamic.value
        )],
        _ => {
            let names = ["e", "t", "a", "o", "i", "n", "s", "r", "l", "c"];
            let mut declarations = Vec::new();
            let mut statements = Vec::new();
            let mut initial = Vec::new();
            for (index, dynamic) in dynamics.iter().enumerate() {
                let value_id = if index == 0 {
                    "_v$".to_string()
                } else {
                    format!("_v${}", index + 1)
                };
                let prop = names.get(index).copied().unwrap_or("x");
                declarations.push(format!("{value_id} = {}", dynamic.value));
                statements.push(format!(
                    "{value_id} !== _p$.{prop} && (_p$.{prop} = _sol_setProp({}, {}, {value_id}, _p$.{prop}));",
                    dynamic.elem,
                    js_string(&dynamic.key)
                ));
                initial.push(format!("{prop}: undefined"));
            }
            vec![format!(
                "_sol_effect(_p$ => {{ var {}; {} return _p$; }}, {{ {} }});",
                declarations.join(", "),
                statements.join(" "),
                initial.join(", ")
            )]
        }
    }
}

fn escape_template_text(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('\n', "\\n")
        .replace('\r', "\\r")
        .replace('`', "\\`")
        .replace('{', "\\{")
        .replace("${", "\\${")
}

fn string_literal_value(expr: &str) -> Option<String> {
    let trimmed = expr.trim();
    if trimmed.starts_with('"') && trimmed.ends_with('"') {
        serde_json::from_str(trimmed).ok()
    } else if trimmed.starts_with('\'') && trimmed.ends_with('\'') {
        Some(trimmed[1..trimmed.len() - 1].replace("\\'", "'"))
    } else {
        None
    }
}

fn literal_text_value(expr: &str) -> Option<String> {
    let trimmed = expr.trim();
    string_literal_value(trimmed).or_else(|| {
        (!trimmed.is_empty() && trimmed.chars().all(|c| c.is_ascii_digit()))
            .then(|| trimmed.to_string())
    })
}

fn collect_static_bindings(source: &str) -> HashSet<String> {
    let mut bindings = HashSet::new();
    for line in source.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("const ") {
            if let Some(name) = read_binding_name(rest) {
                bindings.insert(name);
            }
        }
        if trimmed.starts_with("import ") {
            if let Some((_, named)) = trimmed.split_once('{') {
                if let Some((named, _)) = named.split_once('}') {
                    for part in named.split(',') {
                        let name = part.trim().split_whitespace().last().unwrap_or("").trim();
                        if is_identifier(name) {
                            bindings.insert(name.to_string());
                        }
                    }
                }
            }
        }
    }
    bindings
}

fn collect_static_text_values(source: &str) -> HashMap<String, String> {
    let mut values = HashMap::new();
    for line in source.lines() {
        let trimmed = line.trim().trim_end_matches(';').trim();
        let Some(rest) = trimmed
            .strip_prefix("let ")
            .or_else(|| trimmed.strip_prefix("const "))
        else {
            continue;
        };
        let Some((name, expr)) = rest.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if !is_identifier(name) {
            continue;
        }
        if let Some(value) = eval_static_text_literal_expr(expr.trim(), &values) {
            values.insert(name.to_string(), value);
        }
    }
    values
}

fn eval_static_text_literal_expr(expr: &str, values: &HashMap<String, String>) -> Option<String> {
    let expr = expr.trim();
    if let Some(value) = literal_text_value(expr) {
        return Some(value);
    }
    if let Some(value) = values.get(expr) {
        return Some(value.clone());
    }
    let parts = split_top_level_plus(expr)?;
    if parts
        .iter()
        .all(|part| part.trim().chars().all(|c| c.is_ascii_digit()))
    {
        let sum = parts
            .iter()
            .map(|part| part.trim().parse::<i64>().ok())
            .collect::<Option<Vec<_>>>()?
            .into_iter()
            .sum::<i64>();
        return Some(sum.to_string());
    }
    let mut output = String::new();
    for part in parts {
        output.push_str(&eval_static_text_literal_expr(part, values)?);
    }
    Some(output)
}

fn split_top_level_plus(expr: &str) -> Option<Vec<&str>> {
    let bytes = expr.as_bytes();
    let mut depth = 0usize;
    let mut start = 0usize;
    let mut parts = Vec::new();
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\'' | b'"' | b'`' => cursor = scan_js_string(bytes, cursor),
            b'(' | b'[' | b'{' => {
                depth += 1;
                cursor += 1;
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
                cursor += 1;
            }
            b'+' if depth == 0 => {
                parts.push(expr[start..cursor].trim());
                start = cursor + 1;
                cursor += 1;
            }
            _ => cursor += 1,
        }
    }
    if parts.is_empty() {
        None
    } else {
        parts.push(expr[start..].trim());
        Some(parts)
    }
}

fn read_binding_name(rest: &str) -> Option<String> {
    let name = rest
        .trim_start()
        .split(|c: char| !(c == '_' || c == '$' || c.is_ascii_alphanumeric()))
        .next()?;
    is_identifier(name).then(|| name.to_string())
}

fn is_component_tag(tag: &str) -> bool {
    tag.contains('.')
        || tag
            .chars()
            .next()
            .is_some_and(|c| c == '_' || c == '$' || c.is_ascii_uppercase())
}

fn is_builtin_component(tag: &str) -> bool {
    tag == "For"
}

fn jsx_node_contains_this(node: &JsxNode) -> bool {
    match node {
        JsxNode::Element(element) => {
            element.attrs.iter().any(|attr| match attr {
                JsxAttribute::Prop {
                    value: JsxAttrValue::Expr(expr),
                    ..
                }
                | JsxAttribute::Spread(expr) => expr.contains("this."),
                _ => false,
            }) || element.children.iter().any(|child| match child {
                JsxChild::Expr(expr) => expr.contains("this."),
                JsxChild::Node(node) => jsx_node_contains_this(node),
                JsxChild::Text(_) => false,
            })
        }
        JsxNode::Fragment(children) => children.iter().any(|child| match child {
            JsxChild::Expr(expr) => expr.contains("this."),
            JsxChild::Node(node) => jsx_node_contains_this(node),
            JsxChild::Text(_) => false,
        }),
    }
}

fn looks_like_method_body(output_prefix: &str) -> bool {
    let Some(open_brace) = output_prefix.rfind('{') else {
        return false;
    };
    let prefix = &output_prefix[..open_brace];
    prefix.trim_end().ends_with("render()")
}

fn normalize_jsx_text(text: &str) -> Option<String> {
    if !text.contains('\n') && !text.contains('\r') {
        return (!text.is_empty()).then(|| collapse_whitespace_runs(text));
    }
    if text.trim().is_empty() {
        return None;
    }
    let normalized = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ");
    if normalized.is_empty() {
        None
    } else {
        Some(normalized)
    }
}

fn collapse_whitespace_runs(text: &str) -> String {
    let mut output = String::with_capacity(text.len());
    let mut previous_ws = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            if !previous_ws {
                output.push(' ');
            }
            previous_ws = true;
        } else {
            output.push(ch);
            previous_ws = false;
        }
    }
    output
}

fn decode_component_text(text: &str) -> String {
    text.replace("&nbsp;", "\u{a0}")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn decode_component_attr(text: &str) -> String {
    decode_component_text(text).replace("&hellip;", "\u{2026}")
}

fn escape_native_expression_text(text: &str) -> String {
    text.replace('<', "&lt;")
}

fn can_start_jsx(source: &str, index: usize) -> bool {
    let bytes = source.as_bytes();
    if index + 1 >= bytes.len() {
        return false;
    }
    let next = bytes[index + 1];
    if !(next == b'>' || is_tag_start(next)) {
        return false;
    }
    let before = previous_non_ws(source, index);
    match before {
        None => true,
        Some((_, b'(' | b'[' | b'{' | b'=' | b':' | b',' | b'?' | b'!' | b'&' | b'|' | b';')) => {
            true
        }
        Some((pos, b'>')) if pos > 0 && bytes[pos - 1] == b'=' => true,
        Some((pos, _)) => source[..=pos].trim_end().ends_with("return"),
    }
}

fn previous_non_ws(source: &str, index: usize) -> Option<(usize, u8)> {
    let bytes = source.as_bytes();
    let mut cursor = index;
    while cursor > 0 {
        cursor -= 1;
        if !bytes[cursor].is_ascii_whitespace() {
            return Some((cursor, bytes[cursor]));
        }
    }
    None
}

fn is_tag_start(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || byte.is_ascii_alphabetic()
}

fn is_tag_char(byte: u8) -> bool {
    is_attr_char(byte) || byte == b'.'
}

fn is_attr_char(byte: u8) -> bool {
    byte == b'_' || byte == b'$' || byte == b'-' || byte == b':' || byte.is_ascii_alphanumeric()
}

fn js_string(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| format!("\"{}\"", value))
}

fn universal_import() -> &'static str {
    r#"import { createElement as _sol_createElement, createTextNode as _sol_createTextNode, insertNode as _sol_insertNode, insert as _sol_insert, setProp as _sol_setProp, spread as _sol_spread, effect as _sol_effect, createComponent as _sol_createComponent, mergeProps as _sol_mergeProps, use as _sol_use, memo as _sol_memo, For as _sol_For } from "solite-runtime";"#
}
