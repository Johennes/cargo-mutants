// Copyright 2021-2023 Martin Pool

//! Visit the abstract syntax tree and discover things to mutate.
//!
//! Knowledge of the `syn` API is localized here.
//!
//! Walking the tree starts with some root files known to the build tool:
//! e.g. for cargo they are identified from the targets. The tree walker then
//! follows `mod` statements to recursively visit other referenced files.

use std::collections::VecDeque;
use std::sync::Arc;

use anyhow::Context;
use quote::ToTokens;
use syn::ext::IdentExt;
use syn::visit::Visit;
use syn::{Attribute, ItemFn};
use tracing::{debug, debug_span, trace, trace_span, warn};

use crate::path::TreeRelativePathBuf;
use crate::source::SourceFile;
use crate::*;

pub fn discover_mutants(
    tool: &dyn Tool,
    root: &Utf8Path,
    options: &Options,
) -> Result<Vec<Mutant>> {
    walk_tree(tool, root, options).map(|x| x.0)
}

pub fn discover_files(
    tool: &dyn Tool,
    root: &Utf8Path,
    options: &Options,
) -> Result<Vec<Arc<SourceFile>>> {
    walk_tree(tool, root, options).map(|x| x.1)
}

/// Discover all mutants and all source files.
///
/// The list of source files includes even those with no mutants.
fn walk_tree(
    tool: &dyn Tool,
    root: &Utf8Path,
    options: &Options,
) -> Result<(Vec<Mutant>, Vec<Arc<SourceFile>>)> {
    let mut mutants = Vec::new();
    let mut seen_files: Vec<Arc<SourceFile>> = Vec::new();

    let mut file_queue: VecDeque<Arc<SourceFile>> = tool.root_files(root)?.into();
    while let Some(source_file) = file_queue.pop_front() {
        check_interrupted()?;
        let package_name = source_file.package_name.clone();
        let (mut file_mutants, more_files) = walk_file(root, Arc::clone(&source_file))?;
        // We'll still walk down through files that don't match globs, so that
        // we have a chance to find modules underneath them. However, we won't
        // collect any mutants from them, and they don't count as "seen" for
        // `--list-files`.
        for path in more_files {
            file_queue.push_back(Arc::new(SourceFile::new(root, path, package_name.clone())?));
        }
        let path = &source_file.tree_relative_path;
        if let Some(examine_globset) = &options.examine_globset {
            if !examine_globset.is_match(path.as_ref()) {
                trace!("{path:?} does not match examine globset");
                continue;
            }
        }
        if let Some(exclude_globset) = &options.exclude_globset {
            if exclude_globset.is_match(path.as_ref()) {
                trace!("{path:?} excluded by globset");
                continue;
            }
        }
        if let Some(examine_names) = &options.examine_names {
            if !examine_names.is_empty() {
                file_mutants.retain(|m| examine_names.is_match(&m.to_string()));
            }
        }
        if let Some(exclude_names) = &options.exclude_names {
            if !exclude_names.is_empty() {
                file_mutants.retain(|m| !exclude_names.is_match(&m.to_string()));
            }
        }
        mutants.append(&mut file_mutants);
        seen_files.push(Arc::clone(&source_file));
    }
    Ok((mutants, seen_files))
}

/// Find all possible mutants in a source file.
///
/// Returns the mutants found, and more files discovered by `mod` statements to visit.
fn walk_file(
    root: &Utf8Path,
    source_file: Arc<SourceFile>,
) -> Result<(Vec<Mutant>, Vec<TreeRelativePathBuf>)> {
    let _span = debug_span!("source_file", path = source_file.tree_relative_slashes()).entered();
    debug!("visit source file");
    let syn_file = syn::parse_str::<syn::File>(&source_file.code)
        .with_context(|| format!("failed to parse {}", source_file.tree_relative_slashes()))?;
    let mut visitor = DiscoveryVisitor {
        root: root.to_owned(),
        source_file,
        more_files: Vec::new(),
        mutants: Vec::new(),
        namespace_stack: Vec::new(),
    };
    visitor.visit_file(&syn_file);
    Ok((visitor.mutants, visitor.more_files))
}

/// `syn` visitor that recursively traverses the syntax tree, accumulating places
/// that could be mutated.
struct DiscoveryVisitor {
    /// All the mutants generated by visiting the file.
    mutants: Vec<Mutant>,

    /// The file being visited.
    source_file: Arc<SourceFile>,

    /// The root of the source tree.
    root: Utf8PathBuf,

    /// The stack of namespaces we're currently inside.
    namespace_stack: Vec<String>,

    /// Files discovered by `mod` statements.
    more_files: Vec<TreeRelativePathBuf>,
}

impl DiscoveryVisitor {
    fn collect_fn_mutants(&mut self, return_type: &syn::ReturnType, span: &proc_macro2::Span) {
        let full_function_name = Arc::new(self.namespace_stack.join("::"));
        let return_type_str = Arc::new(return_type_to_string(return_type));
        for op in ops_for_return_type(return_type) {
            self.mutants.push(Mutant::new(
                &self.source_file,
                op,
                &full_function_name,
                &return_type_str,
                span.into(),
            ))
        }
    }

    /// Call a function with a namespace pushed onto the stack.
    ///
    /// This is used when recursively descending into a namespace.
    fn in_namespace<F, T>(&mut self, name: &str, f: F) -> T
    where
        F: FnOnce(&mut Self) -> T,
    {
        let name = remove_excess_spaces(name);
        self.namespace_stack.push(name.clone());
        let r = f(self);
        assert_eq!(self.namespace_stack.pop().unwrap(), name);
        r
    }
}

impl<'ast> Visit<'ast> for DiscoveryVisitor {
    /// Visit top-level `fn foo()`.
    fn visit_item_fn(&mut self, i: &'ast ItemFn) {
        // TODO: Filter out more inapplicable fns.
        let function_name = remove_excess_spaces(&i.sig.ident.to_token_stream().to_string());
        let _span = trace_span!(
            "fn",
            line = i.sig.fn_token.span.start().line,
            name = function_name
        )
        .entered();
        if attrs_excluded(&i.attrs) {
            trace!("excluded by attrs");
            return; // don't look inside it either
        }
        if block_is_empty(&i.block) {
            trace!("function body is empty");
            return;
        }
        self.in_namespace(&function_name, |self_| {
            self_.collect_fn_mutants(&i.sig.output, &i.block.brace_token.span);
            syn::visit::visit_item_fn(self_, i);
        });
    }

    /// Visit `fn foo()` within an `impl`.
    fn visit_impl_item_method(&mut self, i: &'ast syn::ImplItemMethod) {
        // Don't look inside constructors (called "new") because there's often no good
        // alternative.
        if attrs_excluded(&i.attrs) || i.sig.ident == "new" || block_is_empty(&i.block) {
            return;
        }
        let function_name = remove_excess_spaces(&i.sig.ident.to_token_stream().to_string());
        self.in_namespace(&function_name, |self_| {
            self_.collect_fn_mutants(&i.sig.output, &i.block.brace_token.span);
            syn::visit::visit_impl_item_method(self_, i)
        });
    }

    /// Visit `impl Foo { ...}` or `impl Debug for Foo { ... }`.
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if attrs_excluded(&i.attrs) {
            return;
        }
        let type_name = type_name_string(&i.self_ty);
        let name = if let Some((_, trait_path, _)) = &i.trait_ {
            let trait_name = &trait_path.segments.last().unwrap().ident;
            if trait_name == "Default" {
                // We don't know (yet) how to generate an interestingly-broken
                // Default::default.
                return;
            }
            format!(
                "<impl {} for {}>",
                trait_name,
                remove_excess_spaces(&type_name)
            )
        } else {
            type_name
        };
        // Make an approximately-right namespace.
        // TODO: For `impl X for Y` get both X and Y onto the namespace
        // stack so that we can show a more descriptive name.
        self.in_namespace(&name, |v| syn::visit::visit_item_impl(v, i));
    }

    /// Visit `mod foo { ... }` or `mod foo;`.
    fn visit_item_mod(&mut self, node: &'ast syn::ItemMod) {
        let mod_name = &node.ident.unraw().to_string();
        let _span = trace_span!(
            "mod",
            line = node.mod_token.span.start().line,
            name = mod_name
        )
        .entered();
        if attrs_excluded(&node.attrs) {
            trace!("mod {:?} excluded by attrs", node.ident,);
            return;
        }
        // If there's no content in braces, then this is a `mod foo;`
        // statement referring to an external file. We find the file name
        // then remember to visit it later.
        //
        // Both the current module and the included sub-module can be in
        // either style: `.../foo.rs` or `.../foo/mod.rs`.
        //
        // If the current file ends with `/mod.rs`, then sub-modules
        // will be in the same directory as this file. Otherwise, this is
        // `/foo.rs` and sub-modules will be in `foo/`.
        //
        // Having determined the directory then we can look for either
        // `foo.rs` or `foo/mod.rs`.
        if node.content.is_none() {
            let my_path: &Utf8Path = self.source_file.tree_relative_path().as_ref();
            // Maybe matching on the name here is no the right approach and
            // we should instead remember how this file was found?
            let dir = if my_path.ends_with("mod.rs")
                || my_path.ends_with("lib.rs")
                || my_path.ends_with("main.rs")
            {
                my_path.parent().expect("mod path has no parent").to_owned()
            } else {
                my_path.with_extension("")
            };
            let mut found = false;
            let mut tried_paths = Vec::new();
            for &ext in &[".rs", "/mod.rs"] {
                let relative_path = TreeRelativePathBuf::new(dir.join(format!("{mod_name}{ext}")));
                let full_path = relative_path.within(&self.root);
                if full_path.is_file() {
                    trace!("found submodule in {full_path}");
                    self.more_files.push(relative_path);
                    found = true;
                    break;
                } else {
                    tried_paths.push(full_path);
                }
            }
            if !found {
                warn!(
                    "{path}:{line}: referent of mod {mod_name:#?} not found: tried {tried_paths:?}",
                    path = self.source_file.tree_relative_path,
                    line = node.mod_token.span.start().line,
                );
            }
        }
        self.in_namespace(mod_name, |v| syn::visit::visit_item_mod(v, node));
    }
}

fn ops_for_return_type(return_type: &syn::ReturnType) -> Vec<MutationOp> {
    let mut ops: Vec<MutationOp> = Vec::new();
    match return_type {
        syn::ReturnType::Default => ops.push(MutationOp::Unit),
        syn::ReturnType::Type(_rarrow, box_typ) => match &**box_typ {
            syn::Type::Path(syn::TypePath { path, .. }) => {
                // dbg!(&path);
                if path.is_ident("bool") {
                    ops.push(MutationOp::True);
                    ops.push(MutationOp::False);
                } else if path.is_ident("String") {
                    // TODO: Detect &str etc.
                    ops.push(MutationOp::EmptyString);
                    ops.push(MutationOp::Xyzzy);
                } else if path_is_result(path) {
                    // TODO: Try this for any path ending in "Result".
                    // TODO: Recursively generate for types inside the Ok side of the Result.
                    ops.push(MutationOp::OkDefault);
                } else {
                    ops.push(MutationOp::Default)
                }
            }
            _ => ops.push(MutationOp::Default),
        },
    }
    ops
}

fn type_name_string(ty: &syn::Type) -> String {
    ty.to_token_stream().to_string()
}

fn return_type_to_string(return_type: &syn::ReturnType) -> String {
    match return_type {
        syn::ReturnType::Default => String::new(),
        syn::ReturnType::Type(arrow, typ) => {
            format!(
                "{} {}",
                arrow.to_token_stream(),
                remove_excess_spaces(&typ.to_token_stream().to_string())
            )
        }
    }
}

/// Convert a TokenStream representing a type to a String with typical Rust
/// spacing between tokens.
///
/// This shrinks for example "& 'static" to just "&'static".
fn remove_excess_spaces(s: &str) -> String {
    // Walk through looking at space characters, and consider whether we can drop them
    // without it being ambiguous.
    //
    // This is a bit hacky but seems to give reasonably legible results on
    // typical trees...
    //
    // We could instead perhaps do this on the type enum.
    let mut r = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' if r.ends_with("->") => {}
            ' ' => {
                // drop spaces following any of these chars
                if let Some(a) = r.chars().next_back() {
                    match a {
                        ':' | '&' | '<' | '>' => continue, // drop the space
                        _ => {}
                    }
                }
            }
            ':' | ',' | '<' | '>' if r.ends_with(' ') => {
                // drop spaces preceding these chars
                r.pop();
            }
            _ => {}
        }
        r.push(c)
    }
    r
}

fn path_is_result(path: &syn::Path) -> bool {
    path.segments
        .last()
        .map(|segment| segment.ident == "Result")
        .unwrap_or_default()
}

/// True if any of the attrs indicate that we should skip this node and everything inside it.
fn attrs_excluded(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr_is_cfg_test(attr) || attr_is_test(attr) || attr_is_mutants_skip(attr))
}

/// True if the block (e.g. the contents of a function) is empty.
fn block_is_empty(block: &syn::Block) -> bool {
    block.stmts.is_empty()
}

/// True if the attribute is `#[cfg(test)]`.
fn attr_is_cfg_test(attr: &Attribute) -> bool {
    if !attr.path.is_ident("cfg") {
        return false;
    }
    if let syn::Meta::List(meta_list) = attr.parse_meta().unwrap() {
        // We should have already checked this above, but to make sure:
        assert!(meta_list.path.is_ident("cfg"));
        for nested_meta in meta_list.nested {
            if let syn::NestedMeta::Meta(syn::Meta::Path(cfg_path)) = nested_meta {
                if cfg_path.is_ident("test") {
                    return true;
                }
            }
        }
    }
    false
}

/// True if the attribute is `#[test]`.
fn attr_is_test(attr: &Attribute) -> bool {
    attr.path.is_ident("test")
}

/// True if the attribute contains `mutants::skip`.
///
/// This for example returns true for `#[mutants::skip] or `#[cfg_attr(test, mutants::skip)]`.
fn attr_is_mutants_skip(attr: &Attribute) -> bool {
    fn path_is_mutants_skip(path: &syn::Path) -> bool {
        path.segments
            .iter()
            .map(|ps| &ps.ident)
            .eq(["mutants", "skip"].iter())
    }

    fn list_is_mutants_skip(meta_list: &syn::MetaList) -> bool {
        return meta_list.nested.iter().any(|n| match n {
            syn::NestedMeta::Meta(syn::Meta::Path(path)) => path_is_mutants_skip(path),
            syn::NestedMeta::Meta(syn::Meta::List(list)) => list_is_mutants_skip(list),
            _ => false,
        });
    }

    if path_is_mutants_skip(&attr.path) {
        return true;
    }

    if let Ok(syn::Meta::List(meta_list)) = attr.parse_meta() {
        return list_is_mutants_skip(&meta_list);
    }

    false
}

#[cfg(test)]
mod test {
    #[test]
    fn path_is_result() {
        let path: syn::Path = syn::parse_quote! { Result<(), ()> };
        assert!(super::path_is_result(&path));
    }

    #[test]
    fn remove_excess_spaces() {
        use super::remove_excess_spaces as rem;

        assert_eq!(rem("<impl Iterator for MergeTrees < AE , BE , AIT , BIT > > :: next -> Option < Self ::  Item >"),
    "<impl Iterator for MergeTrees<AE, BE, AIT, BIT>>::next -> Option<Self::Item>");
        assert_eq!(rem("Lex < 'buf >::take"), "Lex<'buf>::take");
    }
}
