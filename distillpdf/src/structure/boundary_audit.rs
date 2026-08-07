//! Syntax-aware L2 production-boundary audit.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use quote::ToTokens;
use sha2::{Digest, Sha256};
use syn::visit::{self, Visit};
use syn::{
    ExprCall, ExprField, ExprMethodCall, ExprUnsafe, ImplItem, Item, ItemFn, ItemImpl, Member,
    TypePath,
};

fn workspace() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .to_path_buf()
}

fn has_test_cfg(item: &Item) -> bool {
    let attrs = match item {
        Item::Const(item) => &item.attrs,
        Item::Enum(item) => &item.attrs,
        Item::ExternCrate(item) => &item.attrs,
        Item::Fn(item) => &item.attrs,
        Item::ForeignMod(item) => &item.attrs,
        Item::Impl(item) => &item.attrs,
        Item::Macro(item) => &item.attrs,
        Item::Mod(item) => &item.attrs,
        Item::Static(item) => &item.attrs,
        Item::Struct(item) => &item.attrs,
        Item::Trait(item) => &item.attrs,
        Item::TraitAlias(item) => &item.attrs,
        Item::Type(item) => &item.attrs,
        Item::Union(item) => &item.attrs,
        Item::Use(item) => &item.attrs,
        _ => return false,
    };
    attrs.iter().any(|attribute| {
        attribute.path().is_ident("cfg")
            && attribute
                .meta
                .to_token_stream()
                .to_string()
                .contains("test")
    })
}

#[derive(Default)]
struct BoundaryVisitor {
    findings: Vec<String>,
    document_types: BTreeSet<String>,
    backend_functions: BTreeSet<String>,
    clone_functions: BTreeSet<String>,
}

impl BoundaryVisitor {
    fn record(&mut self, kind: &str, value: impl ToTokens) {
        let normalized = value.to_token_stream().to_string();
        let digest = format!("{:x}", Sha256::digest(normalized.as_bytes()));
        let snippet: String = normalized.chars().take(120).collect();
        self.findings.push(format!("{kind}|{digest}|{snippet}"));
    }
}

const BACKEND_METHODS: &[&str] = &[
    "catalog",
    "deref",
    "get_and_decode_page_content",
    "get_dictionary",
    "get_object",
    "get_page_content",
    "get_page_fonts",
    "get_page_resources",
    "get_pages",
    "load_mem",
    "num_deref",
    "sub_dict",
    "test_adapter",
];

impl<'ast> Visit<'ast> for BoundaryVisitor {
    fn visit_type_path(&mut self, node: &'ast TypePath) {
        if node
            .path
            .segments
            .last()
            .is_some_and(|part| self.document_types.contains(&part.ident.to_string()))
        {
            self.record("concrete-document-type", node);
        }
        visit::visit_type_path(self, node);
    }

    fn visit_item_fn(&mut self, node: &'ast ItemFn) {
        if node.sig.ident == "test_adapter" {
            self.record("production-test-adapter", node);
        }
        visit::visit_item_fn(self, node);
    }

    fn visit_item_impl(&mut self, node: &'ast ItemImpl) {
        if node.unsafety.is_some() {
            self.record("unsafe-impl", node);
        }
        visit::visit_item_impl(self, node);
    }

    fn visit_expr_method_call(&mut self, node: &'ast ExprMethodCall) {
        let method = node.method.to_string();
        if BACKEND_METHODS.contains(&method.as_str()) {
            self.record("direct-backend-method", node);
        }
        if method == "clone" || method == "cloned" || method == "clone_from" {
            // Exact fingerprints make all current clones reviewable. L2 subphases remove the
            // source-proportional subset; adding or swapping any clone changes this allowlist.
            self.record("clone-call", node);
        }
        visit::visit_expr_method_call(self, node);
    }

    fn visit_expr_call(&mut self, node: &'ast ExprCall) {
        if let syn::Expr::Path(path) = node.func.as_ref() {
            if let Some(function) = path.path.segments.last() {
                let function = function.ident.to_string();
                if self.backend_functions.contains(&function) {
                    self.record("direct-backend-function", node);
                }
                if self.clone_functions.contains(&function) {
                    self.record("clone-call", node);
                }
            }
        }
        visit::visit_expr_call(self, node);
    }

    fn visit_expr_field(&mut self, node: &'ast ExprField) {
        if let Member::Named(field) = &node.member {
            if field == "objects" || field == "trailer" {
                self.record("direct-backend-field", node);
            }
        }
        visit::visit_expr_field(self, node);
    }

    fn visit_expr_unsafe(&mut self, node: &'ast ExprUnsafe) {
        self.record("unsafe-block", node);
        visit::visit_expr_unsafe(self, node);
    }
}

fn audit_source(source: &str) -> Vec<String> {
    let file = syn::parse_file(source).expect("production Rust source must parse");
    let mut document_types = BTreeSet::from(["Document".to_string()]);
    let mut backend_functions = BACKEND_METHODS
        .iter()
        .map(|name| (*name).to_string())
        .collect::<BTreeSet<_>>();
    let mut clone_functions = BTreeSet::from([
        "clone".to_string(),
        "clone_from".to_string(),
        "cloned".to_string(),
    ]);
    for item in &file.items {
        if let Item::Use(item) = item {
            collect_document_use_aliases(&item.tree, &mut document_types);
            collect_function_aliases(&item.tree, &mut backend_functions);
            collect_function_aliases(&item.tree, &mut clone_functions);
        }
    }
    loop {
        let before = document_types.len();
        for item in &file.items {
            if let Item::Type(item) = item {
                if let syn::Type::Path(path) = item.ty.as_ref() {
                    if path
                        .path
                        .segments
                        .last()
                        .is_some_and(|part| document_types.contains(&part.ident.to_string()))
                    {
                        document_types.insert(item.ident.to_string());
                    }
                }
            }
        }
        if document_types.len() == before {
            break;
        }
    }
    let mut visitor = BoundaryVisitor {
        document_types,
        backend_functions,
        clone_functions,
        ..BoundaryVisitor::default()
    };
    for item in &file.items {
        if !has_test_cfg(item) {
            visitor.visit_item(item);
        }
    }
    visitor.findings.sort();
    visitor.findings
}

fn collect_function_aliases(tree: &syn::UseTree, functions: &mut BTreeSet<String>) {
    match tree {
        syn::UseTree::Path(path) => collect_function_aliases(&path.tree, functions),
        syn::UseTree::Rename(rename) if functions.contains(&rename.ident.to_string()) => {
            functions.insert(rename.rename.to_string());
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_function_aliases(item, functions);
            }
        }
        _ => {}
    }
}

fn collect_document_use_aliases(tree: &syn::UseTree, aliases: &mut BTreeSet<String>) {
    match tree {
        syn::UseTree::Path(path) => collect_document_use_aliases(&path.tree, aliases),
        syn::UseTree::Name(name) if name.ident == "Document" => {
            aliases.insert("Document".to_string());
        }
        syn::UseTree::Rename(rename) if rename.ident == "Document" => {
            aliases.insert(rename.rename.to_string());
        }
        syn::UseTree::Group(group) => {
            for item in &group.items {
                collect_document_use_aliases(item, aliases);
            }
        }
        _ => {}
    }
}

fn audit_tree(root: &Path) -> BTreeMap<String, Vec<String>> {
    let mut paths = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(directory) = stack.pop() {
        for entry in std::fs::read_dir(directory).expect("source directory") {
            let path = entry.expect("source entry").path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|extension| extension == "rs") {
                paths.push(path);
            }
        }
    }
    paths.sort();
    paths
        .into_iter()
        .filter_map(|path| {
            let source = std::fs::read_to_string(&path).expect("UTF-8 Rust source");
            let findings = audit_source(&source);
            (!findings.is_empty()).then(|| {
                (
                    path.strip_prefix(root).unwrap().display().to_string(),
                    findings,
                )
            })
        })
        .collect()
}

#[test]
fn production_boundary_matches_exact_syntax_allowlist() {
    let actual = audit_tree(&Path::new(env!("CARGO_MANIFEST_DIR")).join("src"));
    let snapshot = workspace().join("tests/oracles/l2-boundary-ast.json");
    if std::env::var_os("DISTILLPDF_UPDATE_L2_BOUNDARY_AUDIT").is_some() {
        std::fs::write(
            &snapshot,
            serde_json::to_string_pretty(&actual).unwrap() + "\n",
        )
        .unwrap();
    }
    let expected: BTreeMap<String, Vec<String>> =
        serde_json::from_slice(&std::fs::read(&snapshot).expect("L2 AST allowlist"))
            .expect("valid L2 AST allowlist");
    assert_eq!(actual, expected, "production boundary syntax changed");
}

#[test]
fn audit_detects_aliases_whitespace_ufcs_clones_and_changed_unsafe() {
    let source = r#"
        use elsewhere::Document as Eager;
        type Hidden = Box<Document>;
        fn bypass(doc: &Document, alias: Eager, object: Object) {
            let _ = doc . get_object ( (1, 0) );
            let _ = Document::get_pages(doc);
            let _ = object.clone();
            let _ = Clone::clone(&object);
            let _ = pdfobj::deref(doc, &object);
            let _ = pdfobj::sub_dict(doc, &object);
            unsafe { first_legacy_call(); }
        }
        unsafe impl Send for Eager {}
        #[cfg(test)]
        fn ignored(doc: Document) { let _ = doc.get_object((2, 0)); }
    "#;
    let findings = audit_source(source);
    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.starts_with("concrete-document-type|"))
            .count(),
        4
    );
    assert!(findings
        .iter()
        .any(|finding| finding.starts_with("direct-backend-method|")));
    assert!(findings
        .iter()
        .any(|finding| finding.starts_with("direct-backend-function|")));
    assert!(findings
        .iter()
        .any(|finding| finding.starts_with("clone-call|")));
    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.starts_with("clone-call|"))
            .count(),
        2
    );
    assert_eq!(
        findings
            .iter()
            .filter(|finding| finding.starts_with("direct-backend-function|"))
            .count(),
        3
    );
    assert!(findings
        .iter()
        .any(|finding| finding.starts_with("unsafe-impl|")));
    let unsafe_fingerprint = findings
        .iter()
        .find(|finding| finding.starts_with("unsafe-block|"))
        .unwrap();
    let changed = audit_source("fn f() { unsafe { different_call(); } }");
    assert!(!changed.contains(unsafe_fingerprint));
    let chained = audit_source("type Later = Alias; type Alias = Document; fn f(_: Later) {}");
    assert_eq!(
        chained
            .iter()
            .filter(|finding| finding.starts_with("concrete-document-type|"))
            .count(),
        3
    );
}

#[test]
fn audit_ignores_comments_strings_and_test_only_items() {
    let source = r#"
        // Document::get_object and unsafe { fake(); }
        const TEXT: &str = "Document . get_pages() object.clone()";
        #[cfg(test)]
        fn test_adapter(document: Document) { let _ = document.clone(); }
    "#;
    assert!(audit_source(source).is_empty());
}

fn read_signature(source: &str, owner: &str) -> String {
    let file = syn::parse_file(source).unwrap();
    file.items
        .iter()
        .find_map(|item| {
            let Item::Impl(item) = item else { return None };
            let syn::Type::Path(path) = item.self_ty.as_ref() else {
                return None;
            };
            (path.path.segments.last()?.ident == owner).then_some(item)
        })
        .and_then(|item| {
            item.items.iter().find_map(|item| match item {
                ImplItem::Fn(method) if method.sig.ident == "read" => {
                    Some(method.sig.to_token_stream().to_string())
                }
                _ => None,
            })
        })
        .unwrap_or_else(|| panic!("missing {owner}::read"))
}

fn compile(source: &str, name: &str) -> std::process::Output {
    let directory = std::env::temp_dir().join(format!(
        "distillpdf-l2-compile-fail-{}-{}",
        std::process::id(),
        name
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let source_path = directory.join("case.rs");
    std::fs::write(&source_path, source).unwrap();
    let output = std::process::Command::new("rustc")
        .args(["--crate-type=lib", "--edition=2021"])
        .arg(&source_path)
        .arg("--out-dir")
        .arg(&directory)
        .output()
        .expect("rustc is available beside cargo");
    std::fs::remove_dir_all(directory).unwrap();
    output
}

#[test]
fn all_handle_reads_have_compile_failed_borrow_escape_proof() {
    let access_source =
        std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/access.rs"))
            .unwrap();
    for (owner, value, actual_value, actual_wrapper, compile_wrapper) in [
        (
            "ObjectHandle",
            "Object",
            "Object",
            "Result < R , AccessError >",
            "Result<R, ()>",
        ),
        (
            "DictionaryHandle",
            "Dictionary",
            "Dictionary",
            "Result < R , AccessError >",
            "Result<R, ()>",
        ),
        (
            "StreamHandle",
            "Stream",
            "lopdf :: Stream",
            "Option < R >",
            "Option<R>",
        ),
    ] {
        let actual = read_signature(&access_source, owner);
        assert!(
            actual.contains("< R >"),
            "{owner}::read has no fixed R: {actual}"
        );
        assert!(
            actual.contains(&format!("FnOnce (& {actual_value}) -> R")),
            "{owner}::read no longer closure-scopes {value}: {actual}"
        );
        assert!(
            actual.contains(actual_wrapper),
            "{owner}::read return shape changed: {actual}"
        );
        let result_expression = if owner == "StreamHandle" {
            "Some(inspect(self.value))"
        } else {
            "Ok(inspect(self.value))"
        };
        let case = format!(
            r#"
                struct {value};
                struct Handle<'a> {{ value: &'a {value} }}
                impl Handle<'_> {{
                    fn read<R>(&self, inspect: impl FnOnce(&{value}) -> R) -> {compile_wrapper} {{
                        {result_expression}
                    }}
                }}
                fn escape<'a>(handle: &'a Handle<'a>) -> &'a {value} {{
                    handle.read(|value| value).unwrap()
                }}
            "#
        );
        let output = compile(&case, owner);
        assert!(
            !output.status.success(),
            "{owner} borrow unexpectedly escaped"
        );
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("lifetime"),
            "{owner} failed for the wrong reason: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let owned = compile(
        r#"
            struct Value(Vec<u8>);
            struct Handle<'a> { value: &'a Value }
            impl Handle<'_> {
                fn read<R>(&self, inspect: impl FnOnce(&Value) -> R) -> R {
                    inspect(self.value)
                }
            }
            fn copy(handle: &Handle<'_>) -> Vec<u8> {
                handle.read(|value| value.0.clone())
            }
        "#,
        "owned-copy",
    );
    assert!(
        owned.status.success(),
        "owned copy did not compile: {}",
        String::from_utf8_lossy(&owned.stderr)
    );
}
