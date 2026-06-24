//! vyer-index — real parsing for Vyer via tree-sitter.
//!
//! tree-sitter is syntactic only (it cannot resolve cross-file references — that
//! is the graph layer's job), but it gives *correct* node boundaries where the
//! heuristic scanner guesses: strings/comments containing braces, nested items,
//! methods inside `impl`/`class` blocks. This crate produces exactly the same
//! [`ParseTree`] the incremental core already memoizes, and is installed through
//! `vyer-incr`'s parser-injection hook — so the incremental spine, memoization,
//! and read-after-write freshness are completely untouched (Phase 2 of the build).
//!
//! Robustness (Rule §8: degrade, don't crash): an unknown language, a grammar
//! load failure, or an unparseable/huge/binary blob all fall back to the
//! zero-dependency heuristic parser rather than erroring.

use std::sync::Arc;

use tree_sitter::{Language, Node, Parser as TsParser};
use vyer_incr::{Db, Lang, ParseTree, Parser};

/// Build the injectable parser. Install with `db.set_parser(tree_sitter_parser())`.
pub fn tree_sitter_parser() -> Parser {
    Arc::new(|text: &str, lang: Lang| parse(text, lang))
}

/// Parse `text` for `lang` into the structural [`ParseTree`]. Runs the language
/// pack's tree-sitter tags query (one generic code path for every language);
/// falls back to the heuristic parser for unregistered languages or any failure.
pub fn parse(text: &str, lang: Lang) -> ParseTree {
    let pack = match langpack::pack(lang) {
        Some(p) => p,
        None => return Db::heuristic_parse(text, lang),
    };
    match langpack::extract(text, pack) {
        Some(items) if !items.is_empty() => ParseTree { items },
        _ => {
            // Empty / failed → don't regress: try the heuristic scanner.
            let h = Db::heuristic_parse(text, lang);
            if h.items.is_empty() {
                ParseTree::default()
            } else {
                h
            }
        }
    }
}

/// Real syntax validation for the apply path (SCRY-001). Parses `text` and
/// reports whether tree-sitter found any `ERROR`/`MISSING` node — i.e. the code
/// does NOT parse. Returns `false` (can't judge → don't block) for languages
/// without a grammar, matching the "degrade, don't crash" rule. This closes the
/// hole where the heuristic brace-checker passed syntactically-invalid Python
/// (it had no check at all) and reported a false `parse=ok`.
pub fn has_parse_error(text: &str, lang: Lang) -> bool {
    let language = match ts_language(lang) {
        Some(l) => l,
        None => return false, // no grammar → cannot validate; don't block the write
    };
    let mut parser = TsParser::new();
    if parser.set_language(&language).is_err() {
        return false;
    }
    match parser.parse(text, None) {
        // SCRY-078: `has_error()` flags ERROR nodes (e.g. an unbalanced brace) but
        // NOT always MISSING nodes — and an invalid Python edit (a `def` whose body
        // is dedented away) yields a MISSING block, not an ERROR. Walk for either,
        // so the apply re-parse gate (Rule §4) refuses a structurally-broken write
        // in indentation-significant languages too, not just brace ones.
        Some(tree) => {
            let root = tree.root_node();
            node_has_error_or_missing(root)
                // SCRY-078: tree-sitter-python accepts a dedented `def`/`class` body
                // as an EMPTY block (no ERROR/MISSING) — but an empty body is invalid
                // Python. Catch it so the gate refuses the write. (Empty `{}` blocks
                // ARE valid in brace languages, so this is Python-only.)
                || (matches!(lang, Lang::Python) && python_empty_def_body(root))
        }
        None => true, // parse failed outright → definitely broken
    }
}

/// True if `node` or any descendant is an ERROR or a MISSING node.
fn node_has_error_or_missing(root: Node) -> bool {
    // SCRY-091: ITERATIVE (explicit heap stack), not recursive — a deeply-nested
    // (≥~100k) malicious file would otherwise overflow the call stack and crash the
    // daemon on apply (DoS). The frontier lives on the heap, so depth is free.
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.is_error() || node.is_missing() {
            return true;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

/// True if any Python `def`/`class` has an EMPTY block body — invalid Python that
/// tree-sitter accepts without an error node (a dedented body parses this way).
fn python_empty_def_body(root: Node) -> bool {
    // SCRY-091: iterative, same stack-overflow defense as node_has_error_or_missing.
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let kind = node.kind();
        if (kind == "function_definition" || kind == "class_definition")
            && node
                .child_by_field_name("body")
                .is_some_and(|b| b.kind() == "block" && b.named_child_count() == 0)
        {
            return true;
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

/// A match from an AST-pattern query (SP-7): the captured node's span (1-based,
/// inclusive), the capture name, and the node kind.
pub struct AstMatch {
    pub start: u32,
    pub end: u32,
    pub capture: String,
    pub kind: String,
}

/// Run a tree-sitter S-expression query against `text` and return the captured
/// node spans (SP-7 AST-pattern search — the real "AST-ish pattern" the schema
/// advertised). Returns an error string for an unsupported language or a query
/// that fails to compile (surfaced to the agent, never a panic).
pub fn ast_query(text: &str, lang: Lang, pattern: &str) -> Result<Vec<AstMatch>, String> {
    use streaming_iterator::StreamingIterator;
    let language = ts_language(lang).ok_or("no tree-sitter grammar for this language")?;
    let query = tree_sitter::Query::new(&language, pattern)
        .map_err(|e| format!("invalid AST query: {e}"))?;
    let mut parser = TsParser::new();
    parser
        .set_language(&language)
        .map_err(|_| "failed to load grammar".to_string())?;
    let tree = parser.parse(text, None).ok_or("parse failed")?;
    let names = query.capture_names();
    let mut cursor = tree_sitter::QueryCursor::new();
    let mut out = Vec::new();
    let mut it = cursor.matches(&query, tree.root_node(), text.as_bytes());
    while let Some(m) = it.next() {
        for cap in m.captures {
            let n = cap.node;
            out.push(AstMatch {
                start: n.start_position().row as u32 + 1,
                end: n.end_position().row as u32 + 1,
                capture: names
                    .get(cap.index as usize)
                    .map(|s| s.to_string())
                    .unwrap_or_default(),
                kind: n.kind().to_string(),
            });
        }
    }
    Ok(out)
}

/// Dump the tree-sitter AST as an indented list of NAMED node kinds with line
/// ranges — the affordance for authoring `mode=ast` S-expression queries (you
/// can't write `(class_definition …)` without knowing the node kinds). Anonymous
/// nodes (punctuation/keywords) are skipped; output is capped at `cap` nodes.
pub fn dump_ast(
    text: &str,
    lang: Lang,
    cap: usize,
    range: Option<(u32, u32)>,
) -> Result<String, String> {
    let language = ts_language(lang).ok_or("no tree-sitter grammar for this language")?;
    let mut parser = TsParser::new();
    parser
        .set_language(&language)
        .map_err(|_| "failed to load grammar".to_string())?;
    let tree = parser.parse(text, None).ok_or("parse failed")?;
    let mut out = String::new();
    let mut count = 0usize;
    walk_ast(tree.root_node(), 0, &mut out, &mut count, cap, None, range);
    if count >= cap {
        out.push_str(&format!(
            "… (truncated at {cap} nodes; dump a smaller file or symbol)\n"
        ));
    }
    Ok(out)
}

fn walk_ast(
    node: tree_sitter::Node,
    depth: usize,
    out: &mut String,
    count: &mut usize,
    cap: usize,
    field: Option<&str>,
    range: Option<(u32, u32)>,
) {
    if *count >= cap {
        return;
    }
    let (ns, ne) = (
        node.start_position().row as u32 + 1,
        node.end_position().row as u32 + 1,
    );
    // Range filter (SP-13b): a node whose span doesn't overlap the requested
    // lines can't contain an overlapping descendant either — prune the subtree.
    if let Some((lo, hi)) = range {
        if ne < lo || ns > hi {
            return;
        }
    }
    let mut child_depth = depth;
    if node.is_named() {
        let indent = "  ".repeat(depth.min(16));
        // A field name (`name:`, `body:`, …) is the parent→child edge label —
        // emitting it lets an agent author field-qualified queries like
        // `(class_definition name: (identifier) @c)`.
        let fld = field.map(|f| format!("{f}: ")).unwrap_or_default();
        out.push_str(&format!(
            "{indent}{fld}({}) @L{}-{}\n",
            node.kind(),
            node.start_position().row + 1,
            node.end_position().row + 1
        ));
        *count += 1;
        child_depth = depth + 1;
    }
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            walk_ast(
                c.node(),
                child_depth,
                out,
                count,
                cap,
                c.field_name(),
                range,
            );
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
}

fn ts_language(lang: Lang) -> Option<Language> {
    langpack::pack(lang).map(|p| p.language())
}

/// The node's first source line, trimmed — the symbol's display header.
fn node_first_line(node: Node, src: &[u8]) -> String {
    let start = node.start_byte();
    let end = node.end_byte().min(src.len());
    let slice = &src[start..end];
    let line_len = slice
        .iter()
        .position(|&b| b == b'\n')
        .unwrap_or(slice.len());
    String::from_utf8_lossy(&slice[..line_len])
        .trim()
        .to_string()
}

/// The pluggable language registry. Adding a language = add a grammar dependency
/// and ONE `LangPack` (a grammar fn + a tree-sitter tags query). Everything else
/// — symbol names, kinds, spans, and all nine superpowers — flows from the one
/// generic [`extract`] path below. No per-language code anywhere else.
///
/// Isolation: a file routes to exactly one pack by extension, and each pack's
/// grammar + compiled query are built **lazily, once** (OnceLock) — so a Dart
/// project never constructs the Python/Rust grammar (no cold-start cost for
/// languages it doesn't use), and a mixed repo just routes each file to its own
/// pack.
mod langpack {
    use super::node_first_line;
    use std::sync::OnceLock;
    use streaming_iterator::StreamingIterator;
    use tree_sitter::{Language, Node, Parser as TsParser, Query, QueryCursor};
    use vyer_incr::Item;
    use vyer_incr::Lang;

    pub struct LangPack {
        grammar: fn() -> Language,
        /// A tree-sitter query whose captures are `@name` (the symbol name) and
        /// `@def.<kind>` (the definition node + its vyer kind). This is the
        /// *only* per-language logic — declarative, like each grammar's own
        /// `tags.scm`.
        tags: &'static str,
        lang_cell: OnceLock<Language>,
        query_cell: OnceLock<Option<Query>>,
    }

    impl LangPack {
        pub fn language(&self) -> Language {
            self.lang_cell.get_or_init(|| (self.grammar)()).clone()
        }
        fn query(&self) -> Option<&Query> {
            self.query_cell
                .get_or_init(|| Query::new(&self.language(), self.tags).ok())
                .as_ref()
        }
    }

    macro_rules! pack {
        ($g:expr, $t:expr) => {
            LangPack {
                grammar: $g,
                tags: $t,
                lang_cell: OnceLock::new(),
                query_cell: OnceLock::new(),
            }
        };
    }

    fn rust_lang() -> Language {
        tree_sitter_rust::LANGUAGE.into()
    }
    fn python_lang() -> Language {
        tree_sitter_python::LANGUAGE.into()
    }
    fn js_lang() -> Language {
        tree_sitter_javascript::LANGUAGE.into()
    }
    fn ts_lang() -> Language {
        tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()
    }
    fn tsx_lang() -> Language {
        // SCRY-099: the JSX-aware grammar for `.tsx` (React). A superset of the
        // `typescript` grammar, so the same TS_TAGS query extracts the same symbols
        // while JSX parses correctly (no mis-bounded/dropped components).
        tree_sitter_typescript::LANGUAGE_TSX.into()
    }
    fn go_lang() -> Language {
        tree_sitter_go::LANGUAGE.into()
    }
    fn dart_lang() -> Language {
        tree_sitter_dart::LANGUAGE.into()
    }
    fn java_lang() -> Language {
        tree_sitter_java::LANGUAGE.into()
    }
    fn ruby_lang() -> Language {
        tree_sitter_ruby::LANGUAGE.into()
    }
    fn swift_lang() -> Language {
        tree_sitter_swift::LANGUAGE.into()
    }
    fn kotlin_lang() -> Language {
        tree_sitter_kotlin_ng::LANGUAGE.into()
    }
    fn c_lang() -> Language {
        tree_sitter_c::LANGUAGE.into()
    }
    fn cpp_lang() -> Language {
        tree_sitter_cpp::LANGUAGE.into()
    }
    fn csharp_lang() -> Language {
        tree_sitter_c_sharp::LANGUAGE.into()
    }
    fn php_lang() -> Language {
        tree_sitter_php::LANGUAGE_PHP.into()
    }

    // --- per-language tags queries (the whole "spec" for a language) ---
    // `name: (_) @name` captures the name field whatever its node type, so we
    // never hard-code identifier-vs-type_identifier. `@def.<kind>` marks the
    // definition node and carries vyer's kind. Methods/receivers/arrows are
    // handled by the grammar's own structure — no special cases.
    const RUST_TAGS: &str = r#"
(function_item name: (_) @name) @def.fn
(struct_item name: (_) @name) @def.struct
(enum_item name: (_) @name) @def.enum
(trait_item name: (_) @name) @def.trait
(mod_item name: (_) @name) @def.mod
(macro_definition name: (_) @name) @def.macro
(impl_item) @def.impl
(field_declaration name: (field_identifier) @name) @def.field
(enum_variant name: (identifier) @name) @def.variant
(const_item name: (identifier) @name) @def.const
(static_item name: (identifier) @name) @def.const
"#;
    // Class-body and module-level assignments only (via nesting) so we index
    // class attributes + module constants WITHOUT capturing every function-local
    // variable (that would replace the god-class problem with index noise).
    const PYTHON_TAGS: &str = r#"
(function_definition name: (_) @name) @def.def
(class_definition name: (_) @name) @def.class
(class_definition body: (block (expression_statement (assignment left: (identifier) @name)) @def.field))
(module (expression_statement (assignment left: (identifier) @name)) @def.const)
"#;
    const JS_TAGS: &str = r#"
(function_declaration name: (_) @name) @def.function
(generator_function_declaration name: (_) @name) @def.function
(class_declaration name: (_) @name) @def.class
(method_definition name: (_) @name) @def.function
(lexical_declaration (variable_declarator name: (_) @name value: (arrow_function))) @def.function
(lexical_declaration (variable_declarator name: (_) @name value: (function_expression))) @def.function
(field_definition property: (_) @name) @def.field
(export_statement (lexical_declaration (variable_declarator name: (identifier) @name))) @def.const
(program (lexical_declaration (variable_declarator name: (identifier) @name)) @def.const)
"#;
    const TS_TAGS: &str = r#"
(function_declaration name: (_) @name) @def.function
(class_declaration name: (_) @name) @def.class
(abstract_class_declaration name: (_) @name) @def.class
(interface_declaration name: (_) @name) @def.interface
(enum_declaration name: (_) @name) @def.enum
(type_alias_declaration name: (_) @name) @def.type
(method_definition name: (_) @name) @def.function
(lexical_declaration (variable_declarator name: (_) @name value: (arrow_function))) @def.function
(public_field_definition name: (property_identifier) @name) @def.field
(property_signature name: (property_identifier) @name) @def.field
(enum_body (property_identifier) @name @def.variant)
(export_statement (lexical_declaration (variable_declarator name: (identifier) @name))) @def.const
(program (lexical_declaration (variable_declarator name: (identifier) @name)) @def.const)
"#;
    const GO_TAGS: &str = r#"
(function_declaration name: (_) @name) @def.func
(method_declaration name: (_) @name) @def.func
(type_spec name: (_) @name) @def.type
(field_declaration name: (field_identifier) @name) @def.field
(const_spec name: (identifier) @name) @def.const
"#;
    // tree-sitter-dart 0.2.x exposes `name:` fields and Dart 3 node kinds.
    // Methods nest a function/getter signature inside `method_declaration`.
    const DART_TAGS: &str = r#"
(class_declaration name: (identifier) @name) @def.class
(enum_declaration name: (identifier) @name) @def.enum
(mixin_declaration name: (identifier) @name) @def.class
(extension_declaration name: (identifier) @name) @def.class
(function_declaration signature: (function_signature name: (identifier) @name)) @def.function
(method_declaration signature: (method_signature (function_signature name: (identifier) @name))) @def.method
(method_declaration signature: (method_signature (getter_signature name: (identifier) @name))) @def.method
(static_final_declaration name: (identifier) @name) @def.field
(initialized_identifier name: (identifier) @name) @def.field
"#;
    const JAVA_TAGS: &str = r#"
(class_declaration name: (_) @name) @def.class
(interface_declaration name: (_) @name) @def.interface
(enum_declaration name: (_) @name) @def.enum
(method_declaration name: (_) @name) @def.function
(constructor_declaration name: (_) @name) @def.function
(field_declaration declarator: (variable_declarator name: (identifier) @name)) @def.field
(enum_constant name: (identifier) @name) @def.variant
"#;
    const RUBY_TAGS: &str = r#"
(class name: (_) @name) @def.class
(module name: (_) @name) @def.mod
(method name: (_) @name) @def.def
(singleton_method name: (_) @name) @def.def
"#;
    // Swift's grammar models struct/enum/extension as `class_declaration` too.
    // Properties are class-body-scoped (nesting) to skip function-local let/var.
    const SWIFT_TAGS: &str = r#"
(class_declaration name: (_) @name) @def.class
(protocol_declaration name: (_) @name) @def.interface
(function_declaration name: (_) @name) @def.function
(class_body (property_declaration name: (pattern bound_identifier: (simple_identifier) @name)) @def.field)
(enum_entry name: (simple_identifier) @name) @def.variant
"#;
    // Kotlin models interface/object via class/object declarations. Properties are
    // class-body / top-level scoped (nesting) to skip function-local val/var.
    const KOTLIN_TAGS: &str = r#"
(class_declaration name: (_) @name) @def.class
(object_declaration name: (_) @name) @def.class
(function_declaration name: (_) @name) @def.function
(class_body (property_declaration (variable_declaration (identifier) @name)) @def.field)
(source_file (property_declaration (variable_declaration (identifier) @name)) @def.const)
(enum_entry (identifier) @name) @def.variant
"#;
    // C names are nested inside `function_declarator`; struct/enum/union/typedef
    // carry a name/declarator field.
    const C_TAGS: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @def.function
(struct_specifier name: (type_identifier) @name) @def.struct
(union_specifier name: (type_identifier) @name) @def.struct
(enum_specifier name: (type_identifier) @name) @def.enum
(type_definition declarator: (type_identifier) @name) @def.type
(field_declaration declarator: (field_identifier) @name) @def.field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @def.field
(enumerator name: (identifier) @name) @def.variant
(preproc_def name: (identifier) @name) @def.const
"#;
    const CPP_TAGS: &str = r#"
(function_definition declarator: (function_declarator declarator: (identifier) @name)) @def.function
(function_definition declarator: (function_declarator declarator: (field_identifier) @name)) @def.function
(class_specifier name: (type_identifier) @name) @def.class
(struct_specifier name: (type_identifier) @name) @def.struct
(enum_specifier name: (type_identifier) @name) @def.enum
(namespace_definition name: (_) @name) @def.mod
(field_declaration declarator: (field_identifier) @name) @def.field
(field_declaration declarator: (pointer_declarator declarator: (field_identifier) @name)) @def.field
(enumerator name: (identifier) @name) @def.variant
"#;
    const CSHARP_TAGS: &str = r#"
(class_declaration name: (_) @name) @def.class
(interface_declaration name: (_) @name) @def.interface
(struct_declaration name: (_) @name) @def.struct
(enum_declaration name: (_) @name) @def.enum
(method_declaration name: (_) @name) @def.function
(constructor_declaration name: (_) @name) @def.function
(namespace_declaration name: (_) @name) @def.mod
(property_declaration name: (identifier) @name) @def.field
(field_declaration (variable_declaration (variable_declarator name: (identifier) @name))) @def.field
(enum_member_declaration name: (identifier) @name) @def.variant
"#;
    const PHP_TAGS: &str = r#"
(class_declaration name: (_) @name) @def.class
(interface_declaration name: (_) @name) @def.interface
(trait_declaration name: (_) @name) @def.class
(enum_declaration name: (_) @name) @def.enum
(function_definition name: (_) @name) @def.function
(method_declaration name: (_) @name) @def.function
(property_declaration (property_element name: (variable_name (name) @name))) @def.field
(const_declaration (const_element (name) @name)) @def.const
(enum_case name: (name) @name) @def.variant
"#;

    static RUST: LangPack = pack!(rust_lang, RUST_TAGS);
    static PYTHON: LangPack = pack!(python_lang, PYTHON_TAGS);
    static JS: LangPack = pack!(js_lang, JS_TAGS);
    static TS: LangPack = pack!(ts_lang, TS_TAGS);
    static TSX: LangPack = pack!(tsx_lang, TS_TAGS); // SCRY-099: JSX-aware, same tags
    static GO: LangPack = pack!(go_lang, GO_TAGS);
    static DART: LangPack = pack!(dart_lang, DART_TAGS);
    static JAVA: LangPack = pack!(java_lang, JAVA_TAGS);
    static RUBY: LangPack = pack!(ruby_lang, RUBY_TAGS);
    static SWIFT: LangPack = pack!(swift_lang, SWIFT_TAGS);
    static KOTLIN: LangPack = pack!(kotlin_lang, KOTLIN_TAGS);
    static C: LangPack = pack!(c_lang, C_TAGS);
    static CPP: LangPack = pack!(cpp_lang, CPP_TAGS);
    static CSHARP: LangPack = pack!(csharp_lang, CSHARP_TAGS);
    static PHP: LangPack = pack!(php_lang, PHP_TAGS);

    /// The registry — the single place a language is wired in.
    pub fn pack(lang: Lang) -> Option<&'static LangPack> {
        Some(match lang {
            Lang::Rust => &RUST,
            Lang::Python => &PYTHON,
            Lang::JavaScript => &JS,
            Lang::TypeScript => &TS,
            Lang::Tsx => &TSX,
            Lang::Go => &GO,
            Lang::Dart => &DART,
            Lang::Java => &JAVA,
            Lang::Ruby => &RUBY,
            Lang::Swift => &SWIFT,
            Lang::Kotlin => &KOTLIN,
            Lang::C => &C,
            Lang::Cpp => &CPP,
            Lang::CSharp => &CSHARP,
            Lang::Php => &PHP,
            Lang::Generic => return None,
        })
    }

    /// Intern a tags-query kind suffix to vyer's `&'static str` kind vocabulary.
    fn kind(k: &str) -> &'static str {
        match k {
            "fn" => "fn",
            "def" => "def",
            "function" => "function",
            "func" => "func",
            "struct" => "struct",
            "enum" => "enum",
            "trait" => "trait",
            "impl" => "impl",
            "mod" => "mod",
            "macro" => "macro",
            "class" => "class",
            "interface" => "interface",
            "type" => "type",
            "const" => "const",
            "field" => "field",
            "variant" => "variant",
            _ => "symbol",
        }
    }

    /// The ONE generic extractor: run the pack's tags query, emit an [`Item`] per
    /// `@def.*` match with its `@name`. Replaces all the old per-language code.
    pub fn extract(text: &str, pack: &LangPack) -> Option<Vec<Item>> {
        let language = pack.language();
        let query = pack.query()?;
        let mut parser = TsParser::new();
        parser.set_language(&language).ok()?;
        let tree = parser.parse(text, None)?;
        let names = query.capture_names();
        let src = text.as_bytes();
        let mut items: Vec<Item> = Vec::new();
        let mut cursor = QueryCursor::new();
        let mut it = cursor.matches(query, tree.root_node(), src);
        while let Some(m) = it.next() {
            let mut def_node: Option<Node> = None;
            let mut k: &'static str = "symbol";
            let mut name: Option<String> = None;
            for cap in m.captures {
                let cn = names[cap.index as usize];
                if let Some(suffix) = cn.strip_prefix("def.") {
                    def_node = Some(cap.node);
                    k = kind(suffix);
                } else if cn == "name" {
                    name = Some(String::from_utf8_lossy(&src[cap.node.byte_range()]).to_string());
                }
            }
            if let Some(node) = def_node {
                items.push(Item {
                    kind: k,
                    header: node_first_line(node, src),
                    start: node.start_position().row as u32 + 1,
                    end: node.end_position().row as u32 + 1,
                    name,
                });
            }
        }
        // Deterministic order; drop exact-duplicate spans (a node can satisfy
        // more than one pattern).
        items.sort_by(|a, b| a.start.cmp(&b.start).then(a.end.cmp(&b.end)));
        items.dedup_by(|a, b| a.start == b.start && a.end == b.end && a.kind == b.kind);
        Some(items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use vyer_incr::Db;

    #[test]
    fn rust_methods_inside_impl_are_found() {
        let src = "\
pub struct S { x: i32 }
impl S {
    pub fn get(&self) -> i32 { self.x }
    fn set(&mut self, v: i32) { self.x = v; }
}
";
        let tree = parse(src, Lang::Rust);
        let kinds: Vec<&str> = tree.items.iter().map(|i| i.kind).collect();
        assert!(kinds.contains(&"struct"));
        assert!(kinds.contains(&"impl"));
        // the two methods nested in the impl block — the heuristic scanner
        // cannot see these reliably; tree-sitter does.
        assert_eq!(
            kinds.iter().filter(|k| **k == "fn").count(),
            2,
            "items: {:?}",
            tree.items
        );
    }

    #[test]
    fn dart_fields_and_consts_are_first_class_symbols() {
        // SCRY-128: a class-level const/field must be its OWN small symbol so a
        // symbol query lands on the declaration line, not the enclosing god-class.
        let src = "\
class Config {
  static const int stardustValue = 2;
  final String name;
  int count = 0;
  void doThing() {}
}
";
        let tree = parse(src, Lang::Dart);
        let field = tree
            .items
            .iter()
            .find(|i| i.name.as_deref() == Some("stardustValue"))
            .unwrap_or_else(|| panic!("stardustValue not indexed: {:?}", tree.items));
        assert_eq!(field.kind, "field");
        assert_eq!(
            (field.start, field.end),
            (2, 2),
            "field span is the decl line"
        );
        // the other field-likes are present too
        for n in ["name", "count"] {
            assert!(
                tree.items.iter().any(|i| i.name.as_deref() == Some(n)),
                "{n} not indexed: {:?}",
                tree.items
            );
        }
        // the class and method still index (no regression).
        assert!(tree.items.iter().any(|i| i.kind == "class"));
        assert!(tree
            .items
            .iter()
            .any(|i| i.name.as_deref() == Some("doThing")));
    }

    #[test]
    fn members_are_first_class_symbols_across_languages() {
        // SCRY-130: a class field / property / enum-member / top-level constant must
        // be its OWN small symbol so a name lookup lands on the declaration, not the
        // enclosing god-class. This ALSO guards against a malformed tags query: a
        // bad query fails to compile, query() returns None, and parse() silently
        // falls back to the heuristic scanner — which does NOT find members — so a
        // missing member here means the query broke (a severe per-language regress).
        // Each case: (lang, source, member-name, expected-kind).
        let cases: &[(Lang, &str, &str, &str)] = &[
            (
                Lang::Rust,
                "struct S { stardust: i32 }\n",
                "stardust",
                "field",
            ),
            (Lang::Rust, "enum E { Alpha, Beta }\n", "Alpha", "variant"),
            (
                Lang::Rust,
                "const STARDUST: i32 = 2;\n",
                "STARDUST",
                "const",
            ),
            (
                Lang::Python,
                "class C:\n    STARDUST = 2\n",
                "STARDUST",
                "field",
            ),
            (Lang::Python, "STARDUST = 2\n", "STARDUST", "const"),
            (
                Lang::TypeScript,
                "class C {\n  stardust: number = 2;\n}\n",
                "stardust",
                "field",
            ),
            (
                Lang::TypeScript,
                "enum E { Alpha, Beta }\n",
                "Alpha",
                "variant",
            ),
            (
                Lang::TypeScript,
                "export const STARDUST = 2;\n",
                "STARDUST",
                "const",
            ),
            (
                Lang::JavaScript,
                "class C {\n  stardust = 2;\n}\n",
                "stardust",
                "field",
            ),
            (
                Lang::JavaScript,
                "export const STARDUST = 2;\n",
                "STARDUST",
                "const",
            ),
            (
                Lang::Go,
                "type S struct {\n  Stardust int\n}\n",
                "Stardust",
                "field",
            ),
            (Lang::Go, "const Stardust = 2\n", "Stardust", "const"),
            (
                Lang::Java,
                "class C {\n  private int stardust;\n}\n",
                "stardust",
                "field",
            ),
            (Lang::Java, "enum E { ALPHA, BETA }\n", "ALPHA", "variant"),
            (
                Lang::CSharp,
                "class C {\n  public int Stardust { get; set; }\n}\n",
                "Stardust",
                "field",
            ),
            (Lang::CSharp, "enum E { Alpha, Beta }\n", "Alpha", "variant"),
            (
                Lang::Kotlin,
                "class C {\n  val stardust = 2\n}\n",
                "stardust",
                "field",
            ),
            (
                Lang::Kotlin,
                "enum class E { ALPHA, BETA }\n",
                "ALPHA",
                "variant",
            ),
            (
                Lang::Swift,
                "class C {\n  let stardust = 2\n}\n",
                "stardust",
                "field",
            ),
            (
                Lang::Php,
                "<?php\nclass C {\n  public int $stardust = 2;\n}\n",
                "stardust",
                "field",
            ),
            (
                Lang::Php,
                "<?php\nenum E { case Alpha; }\n",
                "Alpha",
                "variant",
            ),
            (
                Lang::C,
                "struct S { int stardust; };\n",
                "stardust",
                "field",
            ),
            (Lang::C, "enum E { ALPHA, BETA };\n", "ALPHA", "variant"),
            (
                Lang::Cpp,
                "class C { public: int stardust; };\n",
                "stardust",
                "field",
            ),
        ];
        for (lang, src, member, kind) in cases {
            let tree = parse(src, *lang);
            let found = tree
                .items
                .iter()
                .find(|i| i.name.as_deref() == Some(member));
            match found {
                Some(it) => assert_eq!(
                    it.kind, *kind,
                    "{lang:?}: `{member}` has wrong kind; items: {:?}",
                    tree.items
                ),
                None => panic!(
                    "{lang:?}: member `{member}` NOT indexed (tags query may have failed to \
                     compile → heuristic fallback). items: {:?}",
                    tree.items
                ),
            }
        }
    }

    #[test]
    fn braces_in_strings_do_not_fool_the_parser() {
        // The heuristic brace-matcher mis-ends this fn at the `}` in the string.
        let src = "fn f() {\n    let s = \"a } b\";\n    println!(\"{}\", s);\n}\nfn g() {}\n";
        let tree = parse(src, Lang::Rust);
        let f = tree
            .items
            .iter()
            .find(|i| i.header.contains("fn f"))
            .unwrap();
        assert_eq!(
            (f.start, f.end),
            (1, 4),
            "fn f spans lines 1..=4: {:?}",
            tree.items
        );
        assert!(tree.items.iter().any(|i| i.header.contains("fn g")));
    }

    #[test]
    fn python_and_go_and_js_parse() {
        let py = parse(
            "class A:\n    def m(self):\n        return 1\n",
            Lang::Python,
        );
        assert!(py.items.iter().any(|i| i.kind == "class"));
        assert!(py.items.iter().any(|i| i.kind == "def"));

        let go = parse(
            "package p\nfunc Hello() string { return \"hi\" }\ntype T struct{}\n",
            Lang::Go,
        );
        assert!(go.items.iter().any(|i| i.kind == "func"));
        assert!(go.items.iter().any(|i| i.kind == "type"));

        let js = parse(
            "class C { greet() { return 1; } }\nfunction top() {}\n",
            Lang::JavaScript,
        );
        assert!(js.items.iter().any(|i| i.kind == "class"));
        assert!(js.items.iter().any(|i| i.kind == "function"));
    }

    #[test]
    fn has_parse_error_catches_invalid_python_that_brace_check_missed() {
        // SCRY-001 regression: the apply path used to report parse=ok for this.
        assert!(has_parse_error(
            "def is_over(self):\n    return self.over (((  # broken\n",
            Lang::Python
        ));
        assert!(!has_parse_error(
            "def is_over(self):\n    return self.over  # fine\n",
            Lang::Python
        ));
        // SCRY-078: a DEDENTED body parses as an EMPTY block (no ERROR/MISSING
        // node) — tree-sitter accepts it, but it is invalid Python.
        assert!(
            has_parse_error("def foo():\nreturn 2\n", Lang::Python),
            "a dedented (empty) def body must be flagged"
        );
        assert!(
            !has_parse_error("def foo():\n    return 2\n", Lang::Python),
            "a properly-indented body must be accepted"
        );
        // an empty `{}` block is VALID in brace languages — must NOT be flagged.
        assert!(!has_parse_error("fn foo() {}\n", Lang::Rust));
        // No grammar → cannot judge → do not block (false).
        assert!(!has_parse_error("anything at all", Lang::Generic));
    }

    #[test]
    fn deep_nesting_does_not_overflow_the_stack() {
        // SCRY-091: the error/empty-body tree walks must be ITERATIVE — a
        // pathologically deep tree (≥~100k) crashed the recursive version with a
        // stack overflow (DoS on apply). If this regresses, the test runner ABORTS
        // (not a soft failure), so reaching the assert at all proves no overflow.
        let depth = 120_000;
        let src = format!(
            "fn t() -> i32 {{ {}1{} }}\n",
            "(".repeat(depth),
            ")".repeat(depth)
        );
        let _ = has_parse_error(&src, Lang::Rust);
        // a normal file is of course still fine.
        assert!(!has_parse_error("fn ok() {}\n", Lang::Rust));
    }

    #[test]
    fn js_arrow_function_is_extracted() {
        // SCRY-020: `const NAME = (..) => ..` must become a searchable symbol.
        let js = parse(
            "const add = (a, b) => a + b;\nexport const mul = function (a, b) { return a * b; };\n",
            Lang::JavaScript,
        );
        let mut db = Db::new();
        db.set_parser(tree_sitter_parser());
        db.set_text("a.js", "const add = (a, b) => a + b;\n");
        let names: Vec<String> = db
            .symbols("a.js")
            .symbols
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"add".to_string()), "names: {names:?}");
        assert!(
            js.items.iter().any(|i| i.header.contains("add")),
            "items: {:?}",
            js.items
        );
    }

    #[test]
    fn tsx_jsx_components_are_extracted_with_correct_bounds() {
        // SCRY-099: `.tsx` MUST use the JSX-aware grammar. With the plain `typescript`
        // grammar, JSX (`<Tag>`) misparses and a one-line arrow component swallows the
        // next symbol (Card swallowing App). All three components must extract.
        let src = concat!(
            "export function Button() {\n  return <button>x</button>;\n}\n",
            "const Card = () => <div className=\"c\">hi</div>;\n",
            "export default function App() {\n  return <Button />;\n}\n",
        );
        let mut db = Db::new();
        db.set_parser(tree_sitter_parser());
        db.set_text("App.tsx", src);
        let names: Vec<String> = db
            .symbols("App.tsx")
            .symbols
            .iter()
            .map(|s| s.name.clone())
            .collect();
        for want in ["Button", "Card", "App"] {
            assert!(
                names.contains(&want.to_string()),
                "`.tsx` dropped/misparsed component {want}: {names:?}"
            );
        }
    }

    #[test]
    fn dart_pack_extracts_classes_methods_and_functions() {
        let mut db = Db::new();
        db.set_parser(tree_sitter_parser());
        db.set_text(
            "a.dart",
            "class Counter {\n  void inc() { value += 1; }\n}\n\nint add(int a, int b) {\n  return a + b;\n}\n",
        );
        let names: Vec<String> = db
            .symbols("a.dart")
            .symbols
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"Counter".to_string()), "class: {names:?}");
        assert!(names.contains(&"inc".to_string()), "method: {names:?}");
        assert!(
            names.contains(&"add".to_string()),
            "top-level fn: {names:?}"
        );
    }

    #[test]
    fn dart3_modern_syntax_parses_without_error() {
        // The constructs vyer's old (0.0.4) grammar rejected: enhanced enums,
        // records, record/destructuring patterns, null-aware elements, cascades.
        let src = r#"
enum Status {
  active,
  inactive;
  String get label => name;
}

(int, String) pair() => (1, name);

class Repo {
  final List<int> items = [];
  void build(List<Object> pairs, Object? maybe) {
    final ws = [const Icon(), ?maybe];
    for (final (a, b) in pairs) {
      handle(a, b);
    }
    items..add(1)..add(2);
  }
}

void main() {
  final x = (1, 2);
  switch (x) {
    case (var a, var b):
      use(a, b);
    case _:
      done();
  }
}
"#;
        assert!(
            !has_parse_error(src, Lang::Dart),
            "Dart 3 syntax must parse with the upgraded grammar"
        );
        // symbol extraction (tags query) must still work against the new grammar
        let pt = parse(src, Lang::Dart);
        assert!(
            pt.items.iter().any(|i| i.kind == "class"),
            "should still extract a class: {:?}",
            pt.items
        );
    }

    #[test]
    fn pack_handles_go_receivers_and_js_arrows_declaratively() {
        // The old hand-written Go-receiver fix and JS-arrow special case are gone;
        // the grammar's own structure (via the tags query) handles both now.
        let mut db = Db::new();
        db.set_parser(tree_sitter_parser());
        db.set_text(
            "g.go",
            "package p\nfunc (s Shape) Area() int { return 1 }\nfunc Free() int { return 2 }\n",
        );
        let gn: Vec<String> = db
            .symbols("g.go")
            .symbols
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(
            gn.contains(&"Area".to_string()),
            "go receiver method: {gn:?}"
        );
        assert!(gn.contains(&"Free".to_string()), "go fn: {gn:?}");

        db.set_text(
            "a.js",
            "const add = (a, b) => a + b;\nclass C { m() { return 1; } }\n",
        );
        let jn: Vec<String> = db
            .symbols("a.js")
            .symbols
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(jn.contains(&"add".to_string()), "js arrow: {jn:?}");
        assert!(
            jn.contains(&"C".to_string()) && jn.contains(&"m".to_string()),
            "js class/method: {jn:?}"
        );
    }

    #[test]
    fn new_language_packs_extract_classes_and_functions() {
        let mut db = Db::new();
        db.set_parser(tree_sitter_parser());
        let names = |db: &Db, p: &str| -> Vec<String> {
            db.symbols(p)
                .symbols
                .iter()
                .map(|s| s.name.clone())
                .collect()
        };

        db.set_text(
            "A.java",
            "class Foo {\n  int bar(int x) { return x; }\n}\ninterface I { void m(); }\nenum E { A, B }\n",
        );
        let j = names(&db, "A.java");
        assert!(
            j.contains(&"Foo".into())
                && j.contains(&"bar".into())
                && j.contains(&"I".into())
                && j.contains(&"E".into()),
            "java: {j:?}"
        );

        db.set_text(
            "a.rb",
            "class Foo\n  def bar(x)\n    x\n  end\nend\nmodule M\nend\n",
        );
        let r = names(&db, "a.rb");
        assert!(
            r.contains(&"Foo".into()) && r.contains(&"bar".into()) && r.contains(&"M".into()),
            "ruby: {r:?}"
        );

        db.set_text("a.swift", "class Foo {\n  func bar(x: Int) -> Int { return x }\n}\nfunc top() {}\nprotocol P {}\n");
        let s = names(&db, "a.swift");
        assert!(
            s.contains(&"Foo".into())
                && s.contains(&"bar".into())
                && s.contains(&"top".into())
                && s.contains(&"P".into()),
            "swift: {s:?}"
        );

        db.set_text(
            "a.kt",
            "class Foo {\n  fun bar(x: Int): Int { return x }\n}\nfun top() {}\nobject O {}\n",
        );
        let k = names(&db, "a.kt");
        assert!(
            k.contains(&"Foo".into())
                && k.contains(&"bar".into())
                && k.contains(&"top".into())
                && k.contains(&"O".into()),
            "kotlin: {k:?}"
        );
    }

    #[test]
    fn c_pack_extracts_functions_structs_enums() {
        let mut db = Db::new();
        db.set_parser(tree_sitter_parser());
        db.set_text(
            "a.c",
            "int add(int a, int b) {\n  return a + b;\n}\nstruct Point { int x; };\nenum Color { RED, GREEN };\ntypedef int MyInt;\n",
        );
        let names: Vec<String> = db
            .symbols("a.c")
            .symbols
            .iter()
            .map(|s| s.name.clone())
            .collect();
        assert!(names.contains(&"add".into()), "fn: {names:?}");
        assert!(names.contains(&"Point".into()), "struct: {names:?}");
        assert!(names.contains(&"Color".into()), "enum: {names:?}");
        assert!(names.contains(&"MyInt".into()), "typedef: {names:?}");
    }

    #[test]
    fn cpp_csharp_php_packs_extract() {
        let mut db = Db::new();
        db.set_parser(tree_sitter_parser());
        let names = |db: &Db, p: &str| -> Vec<String> {
            db.symbols(p)
                .symbols
                .iter()
                .map(|s| s.name.clone())
                .collect()
        };
        db.set_text("a.cpp", "class Widget {\npublic:\n  int draw(int x) { return x; }\n};\nnamespace ui {}\nint main() { return 0; }\n");
        let cpp = names(&db, "a.cpp");
        assert!(
            cpp.contains(&"Widget".into())
                && cpp.contains(&"draw".into())
                && cpp.contains(&"main".into()),
            "cpp: {cpp:?}"
        );
        db.set_text("a.cs", "namespace App {\n  class Svc {\n    public int Run() { return 1; }\n  }\n  interface I {}\n}\n");
        let cs = names(&db, "a.cs");
        assert!(
            cs.contains(&"Svc".into()) && cs.contains(&"Run".into()) && cs.contains(&"I".into()),
            "cs: {cs:?}"
        );
        db.set_text(
            "a.php",
            "<?php\nclass User {\n  function name() { return 1; }\n}\nfunction helper() {}\n",
        );
        let php = names(&db, "a.php");
        assert!(
            php.contains(&"User".into())
                && php.contains(&"name".into())
                && php.contains(&"helper".into()),
            "php: {php:?}"
        );
    }

    #[test]
    fn unparseable_or_generic_degrades_without_panicking() {
        // Garbage Rust still returns *something* or empty — never panics.
        let _ = parse("fn fn fn ((( {{{ )))", Lang::Rust);
        // Generic falls back to the heuristic (which yields nothing here).
        let g = parse("just some prose, no code.", Lang::Generic);
        assert!(g.items.is_empty());
    }

    #[test]
    fn injected_into_db_symbols_resolve_with_real_names() {
        let mut db = Db::new();
        db.set_parser(tree_sitter_parser());
        db.set_text("s.rs", "impl Foo {\n    pub fn bar(&self) -> u8 { 1 }\n}\n");
        let syms = db.symbols("s.rs");
        let names: Vec<&str> = syms.symbols.iter().map(|s| s.name.as_str()).collect();
        assert!(
            names.contains(&"bar"),
            "tree-sitter method name should resolve: {names:?}"
        );
        assert!(names.contains(&"Foo"));
    }
}
