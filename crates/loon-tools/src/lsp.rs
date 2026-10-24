//! In-process language intelligence: symbol tables and syntax diagnostics computed with
//! parser crates compiled into this binary.
//!
//! The alternative — driving external language servers — needs the right binary on the
//! user's machine, a matching version, a spawn/stdio protocol and a health check: four
//! failure modes before any symbol is found. Parser crates compile in, are pinned by the
//! lockfile, and cannot be missing at runtime.
//!
//! Honest scope: this is syntax-level understanding — definitions, kinds, positions,
//! syntax errors. It does not do name resolution or type inference, so it cannot say which
//! `Foo::new` a bare `new()` calls. Every function here is fail-open: a file that does not
//! parse is a *finding*, never a panic, and callers fall back to the regex path in
//! `inspect.rs`.

use serde_json::json;
use syn::visit::Visit;
// `ToTokens` for rendering non-path self-types (`&Foo`, `[u8; 4]`) as written.
use quote::ToTokens as _;

/// What kind of declaration a symbol is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymbolKind {
    Function,
    Struct,
    Enum,
    Union,
    Trait,
    Impl,
    Module,
    TypeAlias,
    Const,
    Static,
    Macro,
    Field,
}

impl SymbolKind {
    pub fn as_str(self) -> &'static str {
        match self {
            SymbolKind::Function => "function",
            SymbolKind::Struct => "struct",
            SymbolKind::Enum => "enum",
            SymbolKind::Union => "union",
            SymbolKind::Trait => "trait",
            SymbolKind::Impl => "impl",
            SymbolKind::Module => "module",
            SymbolKind::TypeAlias => "type",
            SymbolKind::Const => "const",
            SymbolKind::Static => "static",
            SymbolKind::Macro => "macro",
            SymbolKind::Field => "field",
        }
    }
}

/// One declaration found in a source file.
///
/// Positions are 1-based line and 0-based column, matching the convention the rest of the
/// tool set reports (`read` numbers lines from 1; editors address columns from 0).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Symbol {
    pub name: String,
    pub kind: SymbolKind,
    pub line: usize,
    pub column: usize,
    pub end_line: usize,
    /// `pub`, `pub(crate)`, … — empty when the item is private.
    pub visibility: String,
    /// Attached doc comment, without the `///` markers. Empty when there is none.
    pub doc: String,
    /// The receiver type for an `impl` block (`impl Foo` → `Foo`); empty otherwise.
    pub container: String,
}

impl Symbol {
    fn to_json(&self) -> serde_json::Value {
        let mut v = json!({
            "name": self.name,
            "kind": self.kind.as_str(),
            "line": self.line,
            "column": self.column,
            "end_line": self.end_line,
            "visibility": self.visibility,
        });
        if !self.doc.is_empty() {
            v["doc"] = json!(self.doc);
        }
        if !self.container.is_empty() {
            v["container"] = json!(self.container);
        }
        v
    }
}

/// One syntax problem found in a source file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl Diagnostic {
    fn to_json(&self) -> serde_json::Value {
        json!({"line": self.line, "column": self.column, "message": self.message})
    }
}

/// Result of analysing one source text.
#[derive(Debug, Default)]
pub struct Analysis {
    pub symbols: Vec<Symbol>,
    pub diagnostics: Vec<Diagnostic>,
    /// True when a real parser handled the file; false means the caller should fall back to
    /// the regex heuristic in `inspect.rs`.
    pub parsed: bool,
}

fn visibility(vis: &syn::Visibility) -> String {
    match vis {
        syn::Visibility::Public(_) => "pub".to_string(),
        syn::Visibility::Restricted(r) => {
            let path = path_to_string(&r.path);
            format!("pub({path})")
        }
        syn::Visibility::Inherited => String::new(),
    }
}

fn path_to_string(path: &syn::Path) -> String {
    path.segments
        .iter()
        .map(|s| s.ident.to_string())
        .collect::<Vec<_>>()
        .join("::")
}

fn doc_of(attrs: &[syn::Attribute]) -> String {
    let mut lines = Vec::new();
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        if let Ok(meta) = &attr.meta.require_name_value() {
            if let syn::Expr::Lit(lit) = &meta.value {
                if let syn::Lit::Str(s) = &lit.lit {
                    lines.push(s.value().trim_start().to_string());
                }
            }
        }
    }
    lines.join("\n")
}

/// End line of the item's span, from proc-macro2's `LineColumn` (1-based line).
fn end_line_of(span: proc_macro2::Span) -> usize {
    span.end().line
}

/// Parse Rust source and collect every top-level and nested declaration.
///
/// `syn` is fail-open by construction: on a syntax error this returns no symbols and one
/// diagnostic, and the caller falls back. Struct/enum fields and impl methods are included,
/// because "which methods does this impl have" is most of what symbol navigation is for.
pub fn analyse_rust(src: &str) -> Analysis {
    struct Collector {
        out: Vec<Symbol>,
        /// Receiver of the innermost `impl` being walked, so a method reports `container`.
        impl_stack: Vec<String>,
    }
    impl Collector {
        fn push(
            &mut self,
            name: &str,
            kind: SymbolKind,
            span: proc_macro2::Span,
            vis: &syn::Visibility,
            attrs: &[syn::Attribute],
            container: &str,
        ) {
            // proc-macro2's span-locations reports 1-based line and 0-based column directly,
            // which is exactly the convention documented on `Symbol`.
            let line = span.start().line;
            let column = span.start().column;
            self.out.push(Symbol {
                name: name.to_string(),
                kind,
                line,
                column,
                end_line: end_line_of(span),
                visibility: visibility(vis),
                doc: doc_of(attrs),
                container: container.to_string(),
            });
        }
    }
    impl Visit<'_> for Collector {
        fn visit_item_fn(&mut self, item: &syn::ItemFn) {
            self.push(
                &item.sig.ident.to_string(),
                SymbolKind::Function,
                item.sig.ident.span(),
                &item.vis,
                &item.attrs,
                "",
            );
            syn::visit::visit_item_fn(self, item);
        }
        fn visit_item_struct(&mut self, item: &syn::ItemStruct) {
            self.push(
                &item.ident.to_string(),
                SymbolKind::Struct,
                item.ident.span(),
                &item.vis,
                &item.attrs,
                "",
            );
            // Named fields are symbols too: a struct's shape is usually what the caller
            // wants to know, and it is one walk away.
            if let syn::Fields::Named(named) = &item.fields {
                for field in &named.named {
                    if let Some(ident) = &field.ident {
                        self.push(
                            &ident.to_string(),
                            SymbolKind::Field,
                            ident.span(),
                            &field.vis,
                            &field.attrs,
                            &item.ident.to_string(),
                        );
                    }
                }
            }
            syn::visit::visit_item_struct(self, item);
        }
        fn visit_item_enum(&mut self, item: &syn::ItemEnum) {
            self.push(
                &item.ident.to_string(),
                SymbolKind::Enum,
                item.ident.span(),
                &item.vis,
                &item.attrs,
                "",
            );
            for variant in &item.variants {
                self.push(
                    &variant.ident.to_string(),
                    SymbolKind::Field,
                    variant.ident.span(),
                    &syn::Visibility::Inherited,
                    &variant.attrs,
                    &item.ident.to_string(),
                );
            }
            syn::visit::visit_item_enum(self, item);
        }
        fn visit_item_union(&mut self, item: &syn::ItemUnion) {
            self.push(
                &item.ident.to_string(),
                SymbolKind::Union,
                item.ident.span(),
                &item.vis,
                &item.attrs,
                "",
            );
            syn::visit::visit_item_union(self, item);
        }
        fn visit_item_trait(&mut self, item: &syn::ItemTrait) {
            self.push(
                &item.ident.to_string(),
                SymbolKind::Trait,
                item.ident.span(),
                &item.vis,
                &item.attrs,
                "",
            );
            syn::visit::visit_item_trait(self, item);
        }
        fn visit_item_impl(&mut self, item: &syn::ItemImpl) {
            // The receiver type as written, so `impl fmt::Display for Config` reports
            // `Config` as the container and `fmt::Display` as the trait.
            let container = match &*item.self_ty {
                syn::Type::Path(tp) => path_to_string(&tp.path),
                other => other.to_token_stream().to_string(),
            };
            // The impl's own "name" is its trait or its type, so searching for either finds it.
            let name = match &item.trait_ {
                Some((_, path, _)) => path_to_string(path),
                None => container.clone(),
            };
            self.push(
                &name,
                SymbolKind::Impl,
                item.impl_token.span,
                &syn::Visibility::Inherited,
                &item.attrs,
                &container,
            );
            self.impl_stack.push(container);
            syn::visit::visit_item_impl(self, item);
            self.impl_stack.pop();
        }
        fn visit_impl_item_fn(&mut self, item: &syn::ImplItemFn) {
            // The receiver is whatever impl encloses this method: `impl A { fn go }`
            // reports container `A`, so two types with the same method name are told apart.
            let container = self.impl_stack.last().cloned().unwrap_or_default();
            self.push(
                &item.sig.ident.to_string(),
                SymbolKind::Function,
                item.sig.ident.span(),
                &item.vis,
                &item.attrs,
                &container,
            );
            syn::visit::visit_impl_item_fn(self, item);
        }
        fn visit_item_mod(&mut self, item: &syn::ItemMod) {
            self.push(
                &item.ident.to_string(),
                SymbolKind::Module,
                item.ident.span(),
                &item.vis,
                &item.attrs,
                "",
            );
            syn::visit::visit_item_mod(self, item);
        }
        fn visit_item_type(&mut self, item: &syn::ItemType) {
            self.push(
                &item.ident.to_string(),
                SymbolKind::TypeAlias,
                item.ident.span(),
                &item.vis,
                &item.attrs,
                "",
            );
            syn::visit::visit_item_type(self, item);
        }
        fn visit_item_const(&mut self, item: &syn::ItemConst) {
            self.push(
                &item.ident.to_string(),
                SymbolKind::Const,
                item.ident.span(),
                &item.vis,
                &item.attrs,
                "",
            );
            syn::visit::visit_item_const(self, item);
        }
        fn visit_item_static(&mut self, item: &syn::ItemStatic) {
            self.push(
                &item.ident.to_string(),
                SymbolKind::Static,
                item.ident.span(),
                &item.vis,
                &item.attrs,
                "",
            );
            syn::visit::visit_item_static(self, item);
        }
        fn visit_item_macro(&mut self, item: &syn::ItemMacro) {
            // `macro_rules! name` — the ident carries the name.
            let name = item
                .ident
                .as_ref()
                .map(|i| i.to_string())
                .unwrap_or_else(|| path_to_string(&item.mac.path));
            self.push(
                &name,
                SymbolKind::Macro,
                item.mac
                    .path
                    .get_ident()
                    .map(|i| i.span())
                    .unwrap_or_else(proc_macro2::Span::call_site),
                &syn::Visibility::Inherited,
                &item.attrs,
                "",
            );
            syn::visit::visit_item_macro(self, item);
        }
    }

    let file: syn::File = match syn::parse_str(src) {
        Ok(f) => f,
        Err(e) => {
            let start = e.span().start();
            return Analysis {
                symbols: Vec::new(),
                diagnostics: vec![Diagnostic {
                    line: start.line,
                    column: start.column,
                    message: e.to_string(),
                }],
                parsed: false,
            };
        }
    };
    let mut collector = Collector {
        out: Vec::new(),
        impl_stack: Vec::new(),
    };
    collector.visit_file(&file);
    Analysis {
        symbols: collector.out,
        diagnostics: Vec::new(),
        parsed: true,
    }
}

/// JSON shape shared with `inspect.rs`'s `find_symbol` output.
pub fn symbols_to_json(symbols: &[Symbol]) -> serde_json::Value {
    json!(symbols.iter().map(Symbol::to_json).collect::<Vec<_>>())
}

/// JSON shape for the `diagnostics` tool.
pub fn diagnostics_to_json(diags: &[Diagnostic]) -> serde_json::Value {
    json!(diags.iter().map(Diagnostic::to_json).collect::<Vec<_>>())
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"//! Module docs.
use std::fmt;

/// Does a thing.
pub struct Config {
    /// How many tries.
    pub retries: u32,
    secret: String,
}

#[derive(Debug)]
pub enum Mode {
    /// Fast path.
    Fast,
    Slow,
}

pub trait Run {
    fn run(&self);
}

impl Run for Config {
    /// Runs it.
    fn run(&self) {}
}

impl fmt::Display for Config {}

mod inner {
    pub fn helper() {}
}

pub type Alias = Config;
pub const LIMIT: u32 = 3;
static NAME: &str = "x";

macro_rules! shout {
    () => {};
}
"#;

    #[test]
    fn finds_declarations_with_positions_and_kinds() {
        let a = analyse_rust(SAMPLE);
        assert!(a.parsed, "the sample must parse: {:?}", a.diagnostics);
        let named = |n: &str, k: SymbolKind| {
            a.symbols
                .iter()
                .find(|s| s.name == n && s.kind == k)
                .unwrap_or_else(|| panic!("missing {n:?} {k:?}; have {:?}", a.symbols))
        };
        let cfg = named("Config", SymbolKind::Struct);
        assert_eq!(cfg.line, 5, "1-based line");
        assert_eq!(cfg.visibility, "pub");
        assert_eq!(cfg.doc, "Does a thing.");

        let run = named("run", SymbolKind::Function);
        // A method reports its receiver, so `run` on two types is not ambiguous.
        assert_eq!(run.container, "Config");
        assert_eq!(run.doc, "Runs it.");

        assert!(named("retries", SymbolKind::Field).doc.contains("How many"));
        assert!(named("Fast", SymbolKind::Field).doc.contains("Fast path"));
        // Existence checks: each of these must be present with the right kind.
        for (name, kind) in [
            ("Run", SymbolKind::Trait),
            ("inner", SymbolKind::Module),
            ("helper", SymbolKind::Function),
            ("Alias", SymbolKind::TypeAlias),
            ("LIMIT", SymbolKind::Const),
            ("NAME", SymbolKind::Static),
            ("shout", SymbolKind::Macro),
        ] {
            let _ = named(name, kind);
        }
    }

    /// Impl blocks are reported with their trait/type, so `impl Display for Config` is
    /// findable by searching for either name.
    #[test]
    fn impls_carry_trait_and_type() {
        let a = analyse_rust(SAMPLE);
        let impls: Vec<&Symbol> = a
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Impl)
            .collect();
        assert_eq!(impls.len(), 2, "{impls:?}");
        assert!(impls
            .iter()
            .any(|s| s.name == "Run" && s.container == "Config"));
        assert!(impls
            .iter()
            .any(|s| s.name == "fmt::Display" && s.container == "Config"));
    }

    /// A private item reports no visibility, so callers can filter on exported surface.
    #[test]
    fn private_items_have_no_visibility() {
        let a = analyse_rust(SAMPLE);
        let secret = a
            .symbols
            .iter()
            .find(|s| s.name == "secret")
            .expect("field symbol");
        assert_eq!(secret.visibility, "");
        assert_eq!(secret.container, "Config");
    }

    /// A syntax error is a finding with a position, not a crash — and the caller is told the
    /// file was not parsed so it can fall back.
    #[test]
    fn a_syntax_error_is_a_diagnostic_not_a_panic() {
        // The error must sit inside a well-formed file: syn backtracks an *unterminated*
        // brace to the enclosing block's opening line (verified by probe), so that input
        // would make the test assert on backtracking rather than on position reporting.
        let broken = "struct A {\n  x: u8,\n}\n\nfn ok() {}\nlet = 5;\n";
        let a = analyse_rust(broken);
        assert!(
            !a.parsed,
            "the file has a real error (let = 5; at top level)"
        );
        assert!(a.symbols.is_empty());
        assert_eq!(a.diagnostics.len(), 1);
        let d = &a.diagnostics[0];
        assert_eq!(d.line, 6, "points at the broken line, got {}", d.line);
        assert_eq!(d.column, 0);
        assert!(!d.message.is_empty());
    }

    /// An unterminated brace still yields a usable diagnostic (syn backtracks it to the
    /// opening line), and must not panic or hang.
    #[test]
    fn an_unterminated_brace_stays_fail_open() {
        let a = analyse_rust("fn main() {\n    let x = ;\n");
        assert!(!a.parsed);
        assert_eq!(a.diagnostics.len(), 1, "{:?}", a.diagnostics);
        assert!(a.diagnostics[0].line >= 1);
    }

    /// Empty and empty-ish inputs are valid, just empty.
    #[test]
    fn empty_input_is_empty_not_an_error() {
        let a = analyse_rust("");
        assert!(a.parsed && a.symbols.is_empty() && a.diagnostics.is_empty());
    }

    /// JSON shapes stay additive and present.
    #[test]
    fn json_output_has_the_documented_fields() {
        let a = analyse_rust(SAMPLE);
        let v = symbols_to_json(&a.symbols);
        let first = v.as_array().expect("array").first().expect("non-empty");
        for key in ["name", "kind", "line", "column", "end_line", "visibility"] {
            assert!(first.get(key).is_some(), "missing {key} in {first}");
        }
        let d = diagnostics_to_json(&a.diagnostics);
        assert!(d.is_array());
    }

    /// A file with CRLF line endings and unicode identifiers still positions correctly.
    #[test]
    fn positions_hold_across_line_endings_and_unicode() {
        let src = "struct Caf\u{e9} {\r\n    field: u8,\r\n}\r\n";
        let a = analyse_rust(src);
        assert!(a.parsed);
        let sym = a.symbols.iter().find(|s| s.name == "Café").expect("symbol");
        assert_eq!(sym.line, 1);
        // `struct ` is seven characters, so the identifier starts at column 7.
        assert_eq!(sym.column, 7);
    }
}
