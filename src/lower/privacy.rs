//! Privacy: which types, fields, methods and imported names a module
//! may see, and the diagnostics for reaching past that boundary.

use super::{Lowerer, Sig, DESTRUCTOR};
use crate::diag::{Diag, Span};

impl Lowerer {
    /// Does `module` import a module called `name`?
    pub(super) fn module_imports(&self, module: &str, name: &str) -> bool {
        module == name
            || self
                .imports_by_module
                .get(module)
                .is_some_and(|v| v.iter().any(|m| m == name))
    }

    /// The rest of the advice, when the module a diagnostic points at is one
    /// this file has not imported: writing `mod.name` is not enough on its
    /// own, because the qualifier is out of scope too (§2.1).
    pub(super) fn and_import(&self, module: &str) -> String {
        if self.module_imports(&self.cur_module, module) {
            return String::new();
        }
        format!(", and `import {module};` at the top")
    }

    /// Is `name` a module whose name is in scope in the file being lowered?
    ///
    /// A module's name is in scope only in the file that wrote `import name;`
    /// (§2.1), which is already how the parser reads `mod.Type`. Being
    /// somewhere in the program is not enough: a module another file imports
    /// took the name of a field away from a method in a file that never
    /// mentioned it, and the breakage arrived when an unrelated file added an
    /// unrelated import.
    pub(super) fn module_in_scope(&self, name: &str) -> bool {
        self.modules.contains(name) && self.module_imports(&self.cur_module, name)
    }

    /// Refuse a declaration in `module` that takes the name of a module
    /// that file imports -- a local, a parameter, a function, a type or a
    /// field of one.
    ///
    /// `lib.f()` with a local `lib` in scope used to call a method on the
    /// local, so the import was silently shadowed for the rest of the
    /// function and a reader could not tell which `lib` was meant. Only this
    /// file's imports count: a module some other file imports is not in
    /// scope here, and adding an import deep in a library must not break a
    /// name in a file that never mentions it.
    pub(super) fn check_not_import(
        &self,
        module: &str,
        name: &str,
        span: Span,
    ) -> Result<(), Diag> {
        if self.module_imports(module, name) && module != name {
            return Err(Diag::new(
                span,
                format!("`{name}` is an imported module; shadowing is not allowed, rename one"),
            ));
        }
        Ok(())
    }

    /// `this.f`, read or assigned. A field of the receiver already has one
    /// spelling -- its bare name -- and a second would let the same read be
    /// written two ways in one method. Not a field at all falls through to
    /// the ordinary "no field" error.
    pub(super) fn refuse_this_field(
        &mut self,
        field: &str,
        this_span: Span,
        span: Span,
    ) -> Result<(), Diag> {
        self.this_val(this_span)?;
        let (tid, _) = self.recv.expect("this_val checked the receiver");
        // A private promoted field falls through too, to the privacy error:
        // advising the bare spelling would only lead to the same refusal.
        let visible = self
            .field_path(tid, field)
            .is_some_and(|p| self.check_field_access(tid, &p, field, span).is_ok());
        if visible {
            return Err(Diag::new(
                span,
                format!(
                    "write `{field}`, not `this.{field}`: a field of the receiver is \
                     reached by its bare name"
                ),
            ));
        }
        Ok(())
    }

    /// A destructor is run by the runtime, exactly once, when the count
    /// reaches zero -- never by the program. Calling it by hand would run it
    /// on a live object and then again when that object dies, so every
    /// spelling of a call is refused: `f.drop()`, `this.drop()`, a bare
    /// `drop()` inside a method, and `File.drop()`. The work a program wants
    /// to do early belongs in an ordinary method (`close()`) that the
    /// destructor calls too.
    pub(super) fn refuse_destructor_call(&self, tid: u32, m: &str, span: Span) -> Result<(), Diag> {
        let tname = &self.typedefs[tid as usize].name;
        if m == DESTRUCTOR && self.sigs.contains_key(&format!("{tname}.{m}")) {
            return Err(Diag::new(
                span,
                format!(
                    "`{}.{DESTRUCTOR}` is a destructor and cannot be called: it runs by \
                     itself when the last reference goes. Put what you want to do early \
                     in an ordinary method, and call that from `{DESTRUCTOR}` too",
                    self.show_name(tname)
                ),
            ));
        }
        Ok(())
    }

    /// May the module being lowered name or reach into this type?
    ///
    /// A builtin carries no module and is visible everywhere. Anything else
    /// is visible inside its own module, and outside only if it is `pub`.
    pub(super) fn type_visible(&self, tid: u32) -> bool {
        let m = &self.type_module[tid as usize];
        m.is_empty() || *m == self.cur_module || self.type_pub[tid as usize]
    }

    /// May the module being lowered call this function or method? The
    /// same rule as for a type: a builtin everywhere, anything else in its
    /// own module, and outside it only if it is `pub`.
    pub(super) fn sig_visible(&self, sig: &Sig) -> bool {
        sig.module.is_empty() || sig.module == self.cur_module || sig.is_pub
    }

    /// Refuse a method call the module being lowered may not make: the type
    /// has to be visible here, and the method callable from here. A method
    /// found BY NAME -- `to_str` for `print` and `str(..)`, `add`, `eq` and
    /// `cmp` for an operator -- goes through this too. The spelling hides the
    /// call, not the rule: `print(v)` is `v.to_str()`, so it gets exactly
    /// `v.to_str()`'s checks and diagnostics.
    pub(super) fn check_method_access(&self, tid: u32, m: &str, span: Span) -> Result<(), Diag> {
        if !self.type_visible(tid) {
            return Err(Diag::new(
                span,
                self.not_visible(tid, "its methods cannot be called from here"),
            ));
        }
        let key = format!("{}.{m}", self.typedefs[tid as usize].name);
        if let Some(sig) = self.sigs.get(&key) {
            if !self.sig_visible(sig) {
                return Err(Diag::new(
                    span,
                    format!("`{m}` is private to `{}`", sig.module),
                ));
            }
        }
        Ok(())
    }

    /// May the module being lowered read, write or name field `idx` of
    /// `tid`? The rule every other declaration follows: a builtin's fields
    /// everywhere, anything else inside its own module, and outside it only
    /// if the field is `pub`. It is judged on the type that DECLARES the
    /// field, so a field promoted through embedding keeps its own
    /// visibility wherever it surfaces.
    pub(super) fn field_visible(&self, tid: u32, idx: u32) -> bool {
        let m = &self.type_module[tid as usize];
        m.is_empty()
            || *m == self.cur_module
            || self.field_params[tid as usize][idx as usize].is_pub
    }

    /// Refuse reaching field `name`, found at `path` from `tid`, when it is
    /// private to another module.
    pub(super) fn check_field_access(
        &self,
        tid: u32,
        path: &[u32],
        name: &str,
        span: Span,
    ) -> Result<(), Diag> {
        let (owner, idx) = self.path_owner(tid, path);
        if self.field_visible(owner, idx) {
            return Ok(());
        }
        Err(Diag::new(
            span,
            format!(
                "field `{name}` of `{}` is private to `{}`; only a `pub` field can be \
                 used from another module",
                self.show_name(&self.typedefs[owner as usize].name),
                self.type_module[owner as usize]
            ),
        ))
    }

    /// The diagnostic for reaching into a type that is not visible here.
    pub(super) fn not_visible(&self, tid: u32, what: &str) -> String {
        format!(
            "`{}` is private to `{}`; {what}",
            self.bare_name(&self.typedefs[tid as usize].name),
            self.type_module[tid as usize]
        )
    }

    /// May the module being lowered name this function here?
    ///
    /// The same rule, and the same words, as calling it: a bare name means
    /// this module's declaration, and another module's is reached by
    /// qualifying it and only if it is `pub`. Taking a function's name as a
    /// value must not be a way around either half -- an interface made of a
    /// private function would export it to everyone holding the interface.
    pub(super) fn check_fn_ref_access(
        &self,
        key: &str,
        shown: &str,
        span: Span,
    ) -> Result<(), Diag> {
        let sig = self.sigs.get(key).expect("fn_ref_name found it");
        let owner = &sig.module;
        let bare = crate::ast::bare(key);
        let ours = owner.is_empty() || *owner == self.cur_module;
        // `shown` carries the spelling, and the two spellings have different
        // rules. A BARE name means this module's declaration and nothing
        // else, whether or not the other module exported it -- that is what
        // makes `pub` mean something. A QUALIFIED one has named the module
        // and needs only the export.
        let qualified = shown.contains('.');
        if ours || (qualified && sig.is_pub) {
            return Ok(());
        }
        // Word for word what a CALL of the same name says, so that a reader
        // who has met one has met the other.
        Err(Diag::new(
            span,
            match (sig.is_pub, qualified) {
                (true, false) => {
                    format!("`{shown}` is declared in `{owner}`; write `{owner}.{bare}`")
                }
                (false, true) => {
                    format!("`{bare}` is private to `{owner}`; mark it `pub` to export it")
                }
                (false, false) => format!("`{shown}` is private to `{owner}`"),
                (true, true) => unreachable!("a qualified pub function was accepted above"),
            },
        ))
    }
}
