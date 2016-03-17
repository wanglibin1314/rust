// Copyright 2015 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

use self::ImportDirectiveSubclass::*;

use DefModifiers;
use Module;
use Namespace::{self, TypeNS, ValueNS};
use {NameBinding, NameBindingKind, PrivacyError};
use ResolveResult;
use ResolveResult::*;
use Resolver;
use UseLexicalScopeFlag::DontUseLexicalScope;
use {names_to_string, module_to_string};
use {resolve_error, ResolutionError};

use rustc::lint;
use rustc::middle::def::*;

use syntax::ast::{NodeId, Name};
use syntax::attr::AttrMetaMethods;
use syntax::codemap::Span;
use syntax::util::lev_distance::find_best_match_for_name;

use std::mem::replace;
use std::cell::Cell;

/// Contains data for specific types of import directives.
#[derive(Clone, Debug)]
pub enum ImportDirectiveSubclass {
    SingleImport {
        target: Name,
        source: Name,
        type_determined: Cell<bool>,
        value_determined: Cell<bool>,
    },
    GlobImport,
}

impl ImportDirectiveSubclass {
    pub fn single(target: Name, source: Name) -> Self {
        SingleImport {
            target: target,
            source: source,
            type_determined: Cell::new(false),
            value_determined: Cell::new(false),
        }
    }
}

/// One import directive.
#[derive(Debug,Clone)]
pub struct ImportDirective<'a> {
    module_path: Vec<Name>,
    target_module: Cell<Option<Module<'a>>>, // the resolution of `module_path`
    subclass: ImportDirectiveSubclass,
    span: Span,
    id: NodeId,
    is_public: bool, // see note in ImportResolutionPerNamespace about how to use this
    is_prelude: bool,
}

impl<'a> ImportDirective<'a> {
    pub fn new(module_path: Vec<Name>,
               subclass: ImportDirectiveSubclass,
               span: Span,
               id: NodeId,
               is_public: bool,
               is_prelude: bool)
               -> Self {
        ImportDirective {
            module_path: module_path,
            target_module: Cell::new(None),
            subclass: subclass,
            span: span,
            id: id,
            is_public: is_public,
            is_prelude: is_prelude,
        }
    }

    // Given the binding to which this directive resolves in a particular namespace,
    // this returns the binding for the name this directive defines in that namespace.
    fn import(&self, binding: &'a NameBinding<'a>, privacy_error: Option<Box<PrivacyError<'a>>>)
              -> NameBinding<'a> {
        let mut modifiers = match self.is_public {
            true => DefModifiers::PUBLIC | DefModifiers::IMPORTABLE,
            false => DefModifiers::empty(),
        };
        if let GlobImport = self.subclass {
            modifiers = modifiers | DefModifiers::GLOB_IMPORTED;
        }

        NameBinding {
            kind: NameBindingKind::Import {
                binding: binding,
                id: self.id,
                privacy_error: privacy_error,
            },
            span: Some(self.span),
            modifiers: modifiers,
        }
    }
}

#[derive(Clone, Default)]
/// Records information about the resolution of a name in a module.
pub struct NameResolution<'a> {
    /// The number of unresolved single imports of any visibility that could define the name.
    outstanding_references: u32,
    /// The number of unresolved `pub` single imports that could define the name.
    pub_outstanding_references: u32,
    /// The least shadowable known binding for this name, or None if there are no known bindings.
    pub binding: Option<&'a NameBinding<'a>>,
    duplicate_globs: Vec<&'a NameBinding<'a>>,
}

impl<'a> NameResolution<'a> {
    fn try_define(&mut self, binding: &'a NameBinding<'a>) -> Result<(), &'a NameBinding<'a>> {
        if let Some(old_binding) = self.binding {
            if binding.defined_with(DefModifiers::GLOB_IMPORTED) {
                self.duplicate_globs.push(binding);
            } else if old_binding.defined_with(DefModifiers::GLOB_IMPORTED) {
                self.duplicate_globs.push(old_binding);
                self.binding = Some(binding);
            } else {
                return Err(old_binding);
            }
        } else {
            self.binding = Some(binding);
        }

        Ok(())
    }

    // Returns Some(the resolution of the name), or None if the resolution depends
    // on whether more globs can define the name.
    fn try_result(&self, allow_private_imports: bool)
                  -> Option<ResolveResult<&'a NameBinding<'a>>> {
        match self.binding {
            Some(binding) if !binding.defined_with(DefModifiers::GLOB_IMPORTED) =>
                Some(Success(binding)),
            // If (1) we don't allow private imports, (2) no public single import can define the
            // name, and (3) no public glob has defined the name, the resolution depends on globs.
            _ if !allow_private_imports && self.pub_outstanding_references == 0 &&
                 !self.binding.map(NameBinding::is_public).unwrap_or(false) => None,
            _ if self.outstanding_references > 0 => Some(Indeterminate),
            Some(binding) => Some(Success(binding)),
            None => None,
        }
    }

    fn increment_outstanding_references(&mut self, is_public: bool) {
        self.outstanding_references += 1;
        if is_public {
            self.pub_outstanding_references += 1;
        }
    }

    fn decrement_outstanding_references(&mut self, is_public: bool) {
        let decrement_references = |count: &mut _| {
            assert!(*count > 0);
            *count -= 1;
        };

        decrement_references(&mut self.outstanding_references);
        if is_public {
            decrement_references(&mut self.pub_outstanding_references);
        }
    }

    fn report_conflicts<F: FnMut(&NameBinding, &NameBinding)>(&self, mut report: F) {
        let binding = match self.binding {
            Some(binding) => binding,
            None => return,
        };

        for duplicate_glob in self.duplicate_globs.iter() {
            // FIXME #31337: We currently allow items to shadow glob-imported re-exports.
            if !binding.is_import() {
                if let NameBindingKind::Import { binding, .. } = duplicate_glob.kind {
                    if binding.is_import() { continue }
                }
            }

            report(duplicate_glob, binding);
        }
    }
}

impl<'a> ::ModuleS<'a> {
    pub fn resolve_name(&self, name: Name, ns: Namespace, allow_private_imports: bool)
                        -> ResolveResult<&'a NameBinding<'a>> {
        let resolutions = match self.resolutions.borrow_state() {
            ::std::cell::BorrowState::Unused => self.resolutions.borrow(),
            _ => return Failed(None), // This happens when there is a cycle of glob imports
        };

        let resolution = resolutions.get(&(name, ns)).cloned().unwrap_or_default();
        if let Some(result) = resolution.try_result(allow_private_imports) {
            // If the resolution doesn't depend on glob definability, check privacy and return.
            return result.and_then(|binding| {
                let allowed = allow_private_imports || !binding.is_import() || binding.is_public();
                if allowed { Success(binding) } else { Failed(None) }
            });
        }

        // Check if the globs are determined
        for directive in self.globs.borrow().iter() {
            if !allow_private_imports && !directive.is_public { continue }
            match directive.target_module.get() {
                None => return Indeterminate,
                Some(target_module) => match target_module.resolve_name(name, ns, false) {
                    Indeterminate => return Indeterminate,
                    _ => {}
                }
            }
        }

        Failed(None)
    }

    // Invariant: this may not be called until import resolution is complete.
    pub fn resolve_name_in_lexical_scope(&self, name: Name, ns: Namespace)
                                         -> Option<&'a NameBinding<'a>> {
        self.resolutions.borrow().get(&(name, ns)).and_then(|resolution| resolution.binding)
            .or_else(|| self.prelude.borrow().and_then(|prelude| {
                prelude.resolve_name(name, ns, false).success()
            }))
    }

    // Define the name or return the existing binding if there is a collision.
    pub fn try_define_child(&self, name: Name, ns: Namespace, binding: NameBinding<'a>)
                            -> Result<(), &'a NameBinding<'a>> {
        if self.resolutions.borrow_state() != ::std::cell::BorrowState::Unused { return Ok(()); }
        self.update_resolution(name, ns, |resolution| {
            resolution.try_define(self.arenas.alloc_name_binding(binding))
        })
    }

    pub fn add_import_directive(&self, directive: ImportDirective<'a>) {
        let directive = self.arenas.alloc_import_directive(directive);
        self.unresolved_imports.borrow_mut().push(directive);
        if let GlobImport = directive.subclass {
            // We don't add prelude imports to the globs since they only affect lexical scopes,
            // which are not relevant to import resolution.
            if !directive.is_prelude {
                self.globs.borrow_mut().push(directive);
            }
        }
    }

    pub fn increment_outstanding_references_for(&self, name: Name, ns: Namespace, is_public: bool) {
        self.resolutions.borrow_mut().entry((name, ns)).or_insert_with(Default::default)
            .increment_outstanding_references(is_public);
    }

    // Use `update` to mutate the resolution for the name.
    // If the resolution becomes a success, define it in the module's glob importers.
    fn update_resolution<T, F>(&self, name: Name, ns: Namespace, update: F) -> T
        where F: FnOnce(&mut NameResolution<'a>) -> T
    {
        let mut resolutions = self.resolutions.borrow_mut();
        let resolution = resolutions.entry((name, ns)).or_insert_with(Default::default);
        let was_success = resolution.try_result(false).and_then(ResolveResult::success).is_some();

        let t = update(resolution);
        if !was_success {
            if let Some(Success(binding)) = resolution.try_result(false) {
                self.define_in_glob_importers(name, ns, binding);
            }
        }
        t
    }

    fn define_in_glob_importers(&self, name: Name, ns: Namespace, binding: &'a NameBinding<'a>) {
        if !binding.defined_with(DefModifiers::PUBLIC | DefModifiers::IMPORTABLE) { return }
        for &(importer, directive) in self.glob_importers.borrow_mut().iter() {
            let _ = importer.try_define_child(name, ns, directive.import(binding, None));
        }
    }
}

struct ImportResolvingError<'a> {
    /// Module where the error happened
    source_module: Module<'a>,
    import_directive: &'a ImportDirective<'a>,
    span: Span,
    help: String,
}

struct ImportResolver<'a, 'b: 'a, 'tcx: 'b> {
    resolver: &'a mut Resolver<'b, 'tcx>,
}

impl<'a, 'b:'a, 'tcx:'b> ImportResolver<'a, 'b, 'tcx> {
    // Import resolution
    //
    // This is a fixed-point algorithm. We resolve imports until our efforts
    // are stymied by an unresolved import; then we bail out of the current
    // module and continue. We terminate successfully once no more imports
    // remain or unsuccessfully when no forward progress in resolving imports
    // is made.

    /// Resolves all imports for the crate. This method performs the fixed-
    /// point iteration.
    fn resolve_imports(&mut self) {
        let mut i = 0;
        let mut prev_unresolved_imports = 0;
        let mut errors = Vec::new();

        loop {
            debug!("(resolving imports) iteration {}, {} imports left",
                   i,
                   self.resolver.unresolved_imports);

            self.resolve_imports_for_module_subtree(self.resolver.graph_root, &mut errors);

            if self.resolver.unresolved_imports == 0 {
                debug!("(resolving imports) success");
                self.finalize_resolutions(self.resolver.graph_root, false);
                break;
            }

            if self.resolver.unresolved_imports == prev_unresolved_imports {
                // resolving failed
                // Report unresolved imports only if no hard error was already reported
                // to avoid generating multiple errors on the same import.
                // Imports that are still indeterminate at this point are actually blocked
                // by errored imports, so there is no point reporting them.
                self.finalize_resolutions(self.resolver.graph_root, errors.len() == 0);
                for e in errors {
                    self.import_resolving_error(e)
                }
                break;
            }

            i += 1;
            prev_unresolved_imports = self.resolver.unresolved_imports;
        }
    }

    /// Resolves an `ImportResolvingError` into the correct enum discriminant
    /// and passes that on to `resolve_error`.
    fn import_resolving_error(&self, e: ImportResolvingError<'b>) {
        // If it's a single failed import then create a "fake" import
        // resolution for it so that later resolve stages won't complain.
        if let SingleImport { target, .. } = e.import_directive.subclass {
            let dummy_binding = self.resolver.arenas.alloc_name_binding(NameBinding {
                modifiers: DefModifiers::GLOB_IMPORTED,
                kind: NameBindingKind::Def(Def::Err),
                span: None,
            });
            let dummy_binding = e.import_directive.import(dummy_binding, None);

            let _ = e.source_module.try_define_child(target, ValueNS, dummy_binding.clone());
            let _ = e.source_module.try_define_child(target, TypeNS, dummy_binding);
        }

        let path = import_path_to_string(&e.import_directive.module_path,
                                         &e.import_directive.subclass);

        resolve_error(self.resolver,
                      e.span,
                      ResolutionError::UnresolvedImport(Some((&path, &e.help))));
    }

    /// Attempts to resolve imports for the given module and all of its
    /// submodules.
    fn resolve_imports_for_module_subtree(&mut self,
                                          module_: Module<'b>,
                                          errors: &mut Vec<ImportResolvingError<'b>>) {
        debug!("(resolving imports for module subtree) resolving {}",
               module_to_string(&module_));
        let orig_module = replace(&mut self.resolver.current_module, module_);
        self.resolve_imports_in_current_module(errors);
        self.resolver.current_module = orig_module;

        for (_, child_module) in module_.module_children.borrow().iter() {
            self.resolve_imports_for_module_subtree(child_module, errors);
        }
    }

    /// Attempts to resolve imports for the given module only.
    fn resolve_imports_in_current_module(&mut self, errors: &mut Vec<ImportResolvingError<'b>>) {
        let mut imports = Vec::new();
        let mut unresolved_imports = self.resolver.current_module.unresolved_imports.borrow_mut();
        ::std::mem::swap(&mut imports, &mut unresolved_imports);

        for import_directive in imports {
            match self.resolve_import(&import_directive) {
                Failed(err) => {
                    let (span, help) = match err {
                        Some((span, msg)) => (span, format!(". {}", msg)),
                        None => (import_directive.span, String::new()),
                    };
                    errors.push(ImportResolvingError {
                        source_module: self.resolver.current_module,
                        import_directive: import_directive,
                        span: span,
                        help: help,
                    });
                }
                Indeterminate => unresolved_imports.push(import_directive),
                Success(()) => {
                    // Decrement the count of unresolved imports.
                    assert!(self.resolver.unresolved_imports >= 1);
                    self.resolver.unresolved_imports -= 1;
                }
            }
        }
    }

    /// Attempts to resolve the given import. The return value indicates
    /// failure if we're certain the name does not exist, indeterminate if we
    /// don't know whether the name exists at the moment due to other
    /// currently-unresolved imports, or success if we know the name exists.
    /// If successful, the resolved bindings are written into the module.
    fn resolve_import(&mut self, directive: &'b ImportDirective<'b>) -> ResolveResult<()> {
        debug!("(resolving import for module) resolving import `{}::...` in `{}`",
               names_to_string(&directive.module_path),
               module_to_string(self.resolver.current_module));

        let target_module = match directive.target_module.get() {
            Some(module) => module,
            _ => match self.resolver.resolve_module_path(&directive.module_path,
                                                         DontUseLexicalScope,
                                                         directive.span) {
                Success(module) => module,
                Indeterminate => return Indeterminate,
                Failed(err) => return Failed(err),
            },
        };

        directive.target_module.set(Some(target_module));
        let (source, target, value_determined, type_determined) = match directive.subclass {
            SingleImport { source, target, ref value_determined, ref type_determined } =>
                (source, target, value_determined, type_determined),
            GlobImport => return self.resolve_glob_import(target_module, directive),
        };

        // We need to resolve both namespaces for this to succeed.
        let module_ = self.resolver.current_module;
        let (value_result, type_result) = {
            let mut resolve_in_ns = |ns, determined: bool| {
                // Temporarily count the directive as determined so that the resolution fails
                // (as opposed to being indeterminate) when it can only be defined by the directive.
                if !determined {
                    module_.resolutions.borrow_mut().get_mut(&(target, ns)).unwrap()
                           .decrement_outstanding_references(directive.is_public);
                }
                let result =
                    self.resolver.resolve_name_in_module(target_module, source, ns, false, true);
                if !determined {
                    module_.increment_outstanding_references_for(target, ns, directive.is_public)
                }
                result
            };
            (resolve_in_ns(ValueNS, value_determined.get()),
             resolve_in_ns(TypeNS, type_determined.get()))
        };

        for &(ns, result, determined) in &[(ValueNS, &value_result, value_determined),
                                           (TypeNS, &type_result, type_determined)] {
            if determined.get() { continue }
            if let Indeterminate = *result { continue }

            determined.set(true);
            if let Success(binding) = *result {
                if !binding.defined_with(DefModifiers::IMPORTABLE) {
                    let msg = format!("`{}` is not directly importable", target);
                    span_err!(self.resolver.session, directive.span, E0253, "{}", &msg);
                }

                let privacy_error = if !self.resolver.is_visible(binding, target_module) {
                    Some(Box::new(PrivacyError(directive.span, source, binding)))
                } else {
                    None
                };

                let imported_binding = directive.import(binding, privacy_error);
                let conflict = module_.try_define_child(target, ns, imported_binding);
                if let Err(old_binding) = conflict {
                    let binding = &directive.import(binding, None);
                    self.resolver.report_conflict(module_, target, ns, binding, old_binding);
                }
            }

            module_.update_resolution(target, ns, |resolution| {
                resolution.decrement_outstanding_references(directive.is_public);
            })
        }

        match (&value_result, &type_result) {
            (&Indeterminate, _) | (_, &Indeterminate) => return Indeterminate,
            (&Failed(_), &Failed(_)) => {
                let children = target_module.resolutions.borrow();
                let names = children.keys().map(|&(ref name, _)| name);
                let lev_suggestion = match find_best_match_for_name(names, &source.as_str(), None) {
                    Some(name) => format!(". Did you mean to use `{}`?", name),
                    None => "".to_owned(),
                };
                let msg = format!("There is no `{}` in `{}`{}",
                                  source,
                                  module_to_string(target_module), lev_suggestion);
                return Failed(Some((directive.span, msg)));
            }
            _ => (),
        }

        match (&value_result, &type_result) {
            (&Success(name_binding), _) if !name_binding.is_import() &&
                                           directive.is_public &&
                                           !name_binding.is_public() => {
                let msg = format!("`{}` is private, and cannot be reexported", source);
                let note_msg = format!("consider marking `{}` as `pub` in the imported module",
                                        source);
                struct_span_err!(self.resolver.session, directive.span, E0364, "{}", &msg)
                    .span_note(directive.span, &note_msg)
                    .emit();
            }

            (_, &Success(name_binding)) if !name_binding.is_import() &&
                                           directive.is_public &&
                                           !name_binding.is_public() => {
                if name_binding.is_extern_crate() {
                    let msg = format!("extern crate `{}` is private, and cannot be reexported \
                                       (error E0364), consider declaring with `pub`",
                                       source);
                    self.resolver.session.add_lint(lint::builtin::PRIVATE_IN_PUBLIC,
                                                   directive.id,
                                                   directive.span,
                                                   msg);
                } else {
                    let msg = format!("`{}` is private, and cannot be reexported", source);
                    let note_msg =
                        format!("consider declaring type or module `{}` with `pub`", source);
                    struct_span_err!(self.resolver.session, directive.span, E0365, "{}", &msg)
                        .span_note(directive.span, &note_msg)
                        .emit();
                }
            }

            _ => {}
        }

        // Report a privacy error here if all successful namespaces are privacy errors.
        let mut privacy_error = None;
        for &ns in &[ValueNS, TypeNS] {
            privacy_error = match module_.resolve_name(target, ns, true) {
                Success(&NameBinding {
                    kind: NameBindingKind::Import { ref privacy_error, .. }, ..
                }) => privacy_error.as_ref().map(|error| (**error).clone()),
                _ => continue,
            };
            if privacy_error.is_none() { break }
        }
        privacy_error.map(|error| self.resolver.privacy_errors.push(error));

        // Record what this import resolves to for later uses in documentation,
        // this may resolve to either a value or a type, but for documentation
        // purposes it's good enough to just favor one over the other.
        let def = match type_result.success().and_then(NameBinding::def) {
            Some(def) => def,
            None => value_result.success().and_then(NameBinding::def).unwrap(),
        };
        let path_resolution = PathResolution { base_def: def, depth: 0 };
        self.resolver.def_map.borrow_mut().insert(directive.id, path_resolution);

        debug!("(resolving single import) successfully resolved import");
        return Success(());
    }

    // Resolves a glob import. Note that this function cannot fail; it either
    // succeeds or bails out (as importing * from an empty module or a module
    // that exports nothing is valid). target_module is the module we are
    // actually importing, i.e., `foo` in `use foo::*`.
    fn resolve_glob_import(&mut self, target_module: Module<'b>, directive: &'b ImportDirective<'b>)
                           -> ResolveResult<()> {
        if let Some(Def::Trait(_)) = target_module.def {
            self.resolver.session.span_err(directive.span, "items in traits are not importable.");
        }

        let module_ = self.resolver.current_module;
        if module_.def_id() == target_module.def_id() {
            // This means we are trying to glob import a module into itself, and it is a no-go
            let msg = "Cannot glob-import a module into itself.".into();
            return Failed(Some((directive.span, msg)));
        }
        self.resolver.populate_module_if_necessary(target_module);

        if directive.is_prelude {
            *module_.prelude.borrow_mut() = Some(target_module);
            return Success(());
        }

        // Add to target_module's glob_importers
        target_module.glob_importers.borrow_mut().push((module_, directive));

        for (&(name, ns), resolution) in target_module.resolutions.borrow().iter() {
            if let Some(Success(binding)) = resolution.try_result(false) {
                if binding.defined_with(DefModifiers::IMPORTABLE | DefModifiers::PUBLIC) {
                    let _ = module_.try_define_child(name, ns, directive.import(binding, None));
                }
            }
        }

        // Record the destination of this import
        if let Some(did) = target_module.def_id() {
            self.resolver.def_map.borrow_mut().insert(directive.id,
                                                      PathResolution {
                                                          base_def: Def::Mod(did),
                                                          depth: 0,
                                                      });
        }

        debug!("(resolving glob import) successfully resolved import");
        return Success(());
    }

    // Miscellaneous post-processing, including recording reexports, recording shadowed traits,
    // reporting conflicts, reporting the PRIVATE_IN_PUBLIC lint, and reporting unresolved imports.
    fn finalize_resolutions(&mut self, module: Module<'b>, report_unresolved_imports: bool) {
        // Since import resolution is finished, globs will not define any more names.
        *module.globs.borrow_mut() = Vec::new();

        let mut reexports = Vec::new();
        for (&(name, ns), resolution) in module.resolutions.borrow().iter() {
            resolution.report_conflicts(|b1, b2| {
                self.resolver.report_conflict(module, name, ns, b1, b2)
            });

            let binding = match resolution.binding {
                Some(binding) => binding,
                None => continue,
            };

            if binding.is_public() && (binding.is_import() || binding.is_extern_crate()) {
                if let Some(def) = binding.def() {
                    reexports.push(Export { name: name, def_id: def.def_id() });
                }
            }

            if let NameBindingKind::Import { binding: orig_binding, id, .. } = binding.kind {
                if ns == TypeNS && binding.is_public() &&
                   orig_binding.defined_with(DefModifiers::PRIVATE_VARIANT) {
                    let msg = format!("variant `{}` is private, and cannot be reexported \
                                       (error E0364), consider declaring its enum as `pub`",
                                      name);
                    let lint = lint::builtin::PRIVATE_IN_PUBLIC;
                    self.resolver.session.add_lint(lint, id, binding.span.unwrap(), msg);
                }
            }
        }

        if reexports.len() > 0 {
            if let Some(def_id) = module.def_id() {
                let node_id = self.resolver.ast_map.as_local_node_id(def_id).unwrap();
                self.resolver.export_map.insert(node_id, reexports);
            }
        }

        if report_unresolved_imports {
            for import in module.unresolved_imports.borrow().iter() {
                resolve_error(self.resolver, import.span, ResolutionError::UnresolvedImport(None));
                break;
            }
        }

        for (_, child) in module.module_children.borrow().iter() {
            self.finalize_resolutions(child, report_unresolved_imports);
        }
    }
}

fn import_path_to_string(names: &[Name], subclass: &ImportDirectiveSubclass) -> String {
    if names.is_empty() {
        import_directive_subclass_to_string(subclass)
    } else {
        (format!("{}::{}",
                 names_to_string(names),
                 import_directive_subclass_to_string(subclass)))
            .to_string()
    }
}

fn import_directive_subclass_to_string(subclass: &ImportDirectiveSubclass) -> String {
    match *subclass {
        SingleImport { source, .. } => source.to_string(),
        GlobImport => "*".to_string(),
    }
}

pub fn resolve_imports(resolver: &mut Resolver) {
    let mut import_resolver = ImportResolver { resolver: resolver };
    import_resolver.resolve_imports();
}
