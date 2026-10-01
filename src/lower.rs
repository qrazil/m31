//! Typecheck and lower the AST to IR, in one pass.
//!
//! These are deliberately fused. A separate checker would have to hand its
//! results to the lowerer through a side table keyed by expression identity,
//! and the two traversals would then be free to disagree about types -- a
//! divergence that is invisible until it miscompiles. One pass cannot
//! disagree with itself. The cost is that only the first error is reported,
//! which is acceptable while `corpus/errors/` compares one diagnostic per
//! program.
//!
//! Refcount insertion happens here too, producing the post-refcount IR that
//! docs/ir-v0.md §4 says backends consume. The ownership protocol is §5:
//! arguments are borrowed, returns are owned (+1).

use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap};
use std::rc::Rc;

use crate::ast::*;
use crate::diag::{Diag, Span};
use crate::ir::{self, BlockId, Inst, IrTy, Term, TypeDef, Value};

mod builtins;
mod consts;
mod decl;
mod expr;
mod hold;
mod lambda;
mod privacy;
mod refcount;
mod scope;
mod stmt;
mod ty;
use consts::ConstInfo;

/// A local binding: its type, its current SSA value, and whether it was
/// declared `const`.
type Binding = (Ty, Value, bool);

/// A lowered expression, plus whether we are holding a +1 on it that somebody
/// must release. Literals are immortal and variables are borrowed from their
/// local, so only a call result arrives owned -- or a borrowed operand that
/// had to be held across a call (src/lower/hold.rs).
#[derive(Clone, Copy)]
struct Val {
    /// `None` for a void expression. A void call has no value, and inventing
    /// a dummy one put a dead `bconst` in every emitted function.
    v: Option<Value>,
    ty: Ty,
    owned: bool,
}

impl Val {
    fn new(v: Value, ty: Ty, owned: bool) -> Self {
        Val {
            v: Some(v),
            ty,
            owned,
        }
    }

    fn void() -> Self {
        Val {
            v: None,
            ty: Ty::Void,
            owned: false,
        }
    }

    /// The value. Every caller reaches this only after a type check that
    /// excludes `void`, so a `None` here is a compiler bug, not user error.
    fn val(&self) -> Value {
        self.v.expect("void expression used as a value")
    }
}

struct Sig {
    params: Vec<Param>,
    ret: Ty,
    /// Which module declared this, and whether it left the module. Builtins
    /// carry an empty module and are visible everywhere.
    module: String,
    is_pub: bool,
    /// The seam to C: no body to lower, and the call goes to a runtime
    /// symbol found by one name transform. See docs/stdlib-seam.md.
    is_prim: bool,
}

/// One entry per enclosing `while`, so `break` and `continue` know where to
/// jump and which values to carry.
struct LoopCtx {
    header: BlockId,
    exit: BlockId,
    /// Loop-carried variable names, in the same order as the header's and
    /// exit's block parameters.
    carried: Vec<String>,
    /// Scope depth at the top of the loop body. `break`/`continue` must
    /// release every scope inside this one before jumping.
    depth: usize,
    /// `freezes.len()` at the top of the loop body. `break`/`continue` must
    /// restore any `const` block frozen since -- but not one that encloses
    /// the whole loop -- for the same reason `depth` exists, one space over.
    freeze_depth: usize,
    /// Whether any `break` leaves this loop. A `while (true)` that nothing
    /// breaks out of never finishes, so what follows it is unreachable.
    broke: bool,
}

struct BlockBuf {
    id: BlockId,
    params: Vec<Value>,
    insts: Vec<Inst>,
    term: Option<Term>,
}

/// One `case V(T x):` binding of a match arm that is still being lowered.
///
/// The enum is held for the whole match by a synthetic local (`hold`), so the
/// payload can be taken out of it by name at any point inside the arm.
struct PayloadBind {
    /// The name the arm gave the payload.
    name: String,
    /// The synthetic local holding the scrutinee, so the enum is looked up by
    /// name rather than by a cached SSA value that a nested `if` may have
    /// rebound.
    hold: String,
    tid: u32,
    /// The variant this arm matched, which names the union arm the payload
    /// lives in (`ir::Inst::EnumTake`).
    tag: u32,
    idx: u32,
    /// `self.scopes.len()` inside the arm's own body scope. A move may only
    /// happen at exactly this depth, for the reason `mark_moved` gives: a
    /// move inside a nested loop or branch would run a different number of
    /// times than it was checked.
    depth: usize,
    /// Whether the `match` is the sole owner of the enum -- true when the
    /// scrutinee was a temporary, so no name outside the match reaches it.
    /// When it is false the payload cannot be taken out at all.
    sole_owner: bool,
}

pub struct Lowerer {
    sigs: HashMap<String, Sig>,
    /// User-defined types, indexed by `Ty::User` id. Distinct from `types`
    /// below, which is the per-function map from Value to IrTy.
    typedefs: Vec<TypeDef>,
    /// Surface types of each type's fields, parallel to `typedefs`. The IR
    /// only records `Ref`, which cannot distinguish `str` from a user type.
    field_surface: Vec<Vec<Ty>>,
    /// Field declarations, parallel to `typedefs`, so construction can bind
    /// arguments by the same rule as a call.
    field_params: Vec<Vec<Param>>,
    /// Base type of each distinct type, parallel to `typedefs`.
    distinct_base: Vec<Option<Ty>>,
    /// Module and export flag per type, parallel to `typedefs`. Privacy has
    /// to survive the merge into one program, so it travels with the
    /// declaration rather than with the file.
    type_module: Vec<String>,
    type_pub: Vec<bool>,
    /// Surface payload types per variant, parallel to `typedefs`. The IR
    /// records only `Ref`, which cannot tell `str` from a user type, and a
    /// match arm has to bind the payload at its real type.
    variant_surface: Vec<Vec<Vec<Ty>>>,
    /// The enums this attempt is representing as values, by name, with their
    /// index in `typedefs`. Computed once by `value_enums`.
    value_enum: HashMap<String, u32>,
    /// Enums a previous attempt found could not be values after all.
    forced_boxed: BTreeSet<String>,
    /// Enums THIS attempt has just found the same thing about. Non-empty
    /// means the diagnostic it returned is a request to start again, not an
    /// error -- see `lower_program`.
    demote: Demotions,
    /// Every module in the program, so `greet.hello(..)` can be told from a
    /// field access on a variable called `greet`.
    modules: std::collections::HashSet<String>,
    /// What each module imports, so a local can be refused the name of a
    /// module its own file imports. See `Program::imports_by_module`.
    imports_by_module: HashMap<String, Vec<String>>,
    /// The module whose function is being lowered, for privacy checks.
    cur_module: String,
    /// Set while a collection literal is being built. The old constructor
    /// spelling is refused, and the literal lowering reaches the same
    /// allocation path through it.
    building_literal: bool,
    /// Keys of methods declared `static`. They are called on the type and
    /// take no receiver, so a call site has to know which kind it has.
    statics: std::collections::HashSet<String>,
    /// The monomorphised program's interned type expressions.
    ty_exprs: Vec<TyExpr>,
    /// Source spellings of instantiations, for diagnostics (`Program::shown`).
    shown: HashMap<String, (String, Vec<Ty>)>,
    /// Generic methods, by concrete receiver (`Program::generic_methods`).
    generic_methods: std::collections::HashSet<String>,
    /// Required methods per interface, parallel to `typedefs`; empty for a
    /// struct.
    iface_methods: Vec<Vec<Func>>,
    /// Interface method names, one per dispatch slot, assigned once for the
    /// whole program so a vtable index is a constant at every call site.
    iface_slots: Vec<ir::Slot>,
    strings: Vec<String>,
    /// Module constants by qualified name, with their computed values
    /// (src/lower/consts.rs).
    consts: HashMap<String, ConstInfo>,
    /// The same keys in declaration order, so constants are checked -- and
    /// their static objects numbered -- the same way on every run.
    const_order: Vec<String>,
    /// Constants being computed, innermost last: meeting one already here is
    /// a cycle.
    const_stack: Vec<String>,
    /// The collections constants hold, as static data (`ir::Module::statics`).
    static_objs: Vec<ir::StaticObj>,
    // per-function state
    types: Vec<IrTy>,
    blocks: Vec<BlockBuf>,
    cur: usize,
    scopes: Vec<HashMap<String, Binding>>,
    /// Names, innermost scope last, whose locals hold a +1 to release on exit.
    owned: Vec<Vec<String>>,
    /// Owned temporaries produced while lowering the current statement.
    stmt_temps: Vec<Value>,
    /// Objects currently frozen by an active `const` block, innermost last:
    /// the object and the bool `rt_freeze_enter` returned for it, which
    /// `rt_freeze_leave` needs to decide whether to actually clear the flag
    /// again (docs/const-decision.md, "Corrected 2026-09-27" -- restore,
    /// don't unconditionally clear). Popped back to its length on entry once
    /// `lower_const_block` returns, so it never outlives the Rust call that
    /// pushed it -- the same discipline `scopes` and `owned` already keep.
    /// `return`, `break`, `continue` and `?` leave one or more `const`
    /// blocks without unwinding this call stack, so each of those emits the
    /// restoring calls itself (`restore_freezes_to`) before jumping.
    freezes: Vec<(Value, Value)>,
    loops: Vec<LoopCtx>,
    /// The loop variables of the `for` loops currently being lowered,
    /// innermost last. They are bound `const`, like any other name that
    /// cannot be reassigned -- this is only so the diagnostic can say WHY,
    /// rather than reporting a `const` the reader never wrote.
    loop_vars: Vec<String>,
    /// Counter for synthetic names, so nested loops do not collide.
    synth: u32,
    /// Locals that have been moved out of. Any later use is refused.
    ///
    /// One bit per local, as docs/types.md §4a describes: this is the whole
    /// of the ownership discipline, and it applies only at the boundary
    /// where a value leaves the thread.
    moved: Vec<String>,
    /// The `case V(T x):` bindings of the match arms currently being lowered,
    /// innermost last. A payload binding is borrowed from the enum for
    /// reading, but it is also the arm's OWN name for that payload, so it may
    /// be taken out of the enum and moved across a thread boundary --
    /// docs/concurrency-decision.md, "Taking a payload out of a match".
    payload_binds: Vec<PayloadBind>,
    /// Inside a method: the receiver's type id and its SSA value. Fields are
    /// reached by bare name, which is safe only because nothing shadows
    /// anything -- see `check_shadow`.
    recv: Option<(u32, Value)>,
    /// Why the function being lowered has no receiver, for the diagnostic
    /// when `this` is written in it anyway: top-level code, a free function
    /// or a static method. Empty inside an instance method.
    no_recv: String,
    ret_ty: Ty,
    /// Whether any type declares a destructor. Without one, releasing a
    /// reference runs only the runtime, which decides whether a built-in
    /// method can run user code (src/lower/hold.rs, `releases`).
    has_destructors: bool,
    /// The synthesised wrapper type for each (function, interface) pair a
    /// function's name has been used as a value at, by (function key,
    /// interface type name). One per pair for the whole program, so writing
    /// the same name twice produces one type, one method and -- after the
    /// field-less type's static instance -- one object.
    fn_refs: HashMap<(String, String), u32>,
    /// The forwarding methods those wrappers need, waiting to be lowered.
    /// They are made while a body is being lowered, which is in the middle
    /// of the loop that lowers bodies, so they are lowered after it.
    synth_funcs: Vec<Func>,
    /// How many lambda types have been synthesised, so each gets a name of
    /// its own. A lambda is not cached the way a function's name is: two
    /// lambdas that look alike are still two sites with two capture sets.
    lambdas: u32,
    /// For each synthesised lambda method, by function key, the target it
    /// was checked against: the interface as a reader spells it and the
    /// method's name. Kept only so that a body whose type is wrong can be
    /// told about in the interface's words rather than the wrapper's.
    lambda_targets: HashMap<String, (String, String)>,
    /// Set while a lambda's synthesised method is the function being
    /// lowered, to the entry `lambda_targets` holds for it.
    cur_lambda: Option<(String, String)>,
    /// Names declared by the statements at the entry file's top level, which
    /// are the program's body and so are locals of it -- NOT globals a
    /// function can reach. Kept only to say that in the diagnostic, which is
    /// otherwise a bare "unknown variable" for a name the reader can see two
    /// lines above. Empty while the entry body itself is lowered, where the
    /// name is in scope and the hint would be a lie.
    entry_locals: std::collections::HashSet<String>,
    /// The entry file's module, so the hint above is not offered to an
    /// imported file, which cannot see those names under any reading.
    entry_module: String,
    /// Set while the entry body is the function being lowered.
    in_entry: bool,
}

/// Every builtin function, whether it lives in `sigs` (`concat`) or is
/// special-cased in the call path because its type depends on its argument
/// (`print`, `clone`, the channel operations).
///
/// A bare call to one of these always means the builtin, in every module.
/// Nothing may take the name, because that would be shadowing (§4.1): a
/// module-level `print` used to replace the builtin silently in its own
/// module, and to turn every `print` in an importing file into a privacy
/// error about a function that file never asked for.
const BUILTIN_FNS: &[&str] = &["print", "concat", "clone", "send", "recv", "close", "trap"];

/// The name of the synthesised function whose body is the program: the
/// statements at the entry file's top level. It starts with `$`, which no
/// identifier may, so no program can name it (src/emit_c.rs mangles it).
const ENTRY: &str = "$main";

/// The reserved method name of a destructor: `void File.drop() { .. }` runs
/// when a `File`'s count reaches zero, before its fields are released
/// (docs/destructors-decision.md). Rust's name, because it is the operation
/// the refcount already performs; Swift's `deinit` would be a new keyword for
/// the same thing.
pub const DESTRUCTOR: &str = "drop";

/// Why a resource-owning type cannot be cloned, worded the same way `clone`
/// itself words it -- a thread-boundary diagnostic that advised `clone` on a
/// `net.Conn` would only send the reader to that second error.
const NO_CLONE: &str = "it owns a resource (it has a destructor), and a copy would \
                        release it a second time";

/// What DOES work when the value cannot be cloned. A parameter, a field and
/// `this` are all held by somebody else, and no local alias changes that --
/// the alias is a second reference to the same resource, which is what the
/// move rule refuses. So the only answer is to move it where it is made.
const OWNER_MOVES: &str = "A resource can only be moved on by whoever made it: bind it \
                           with `case` in a `match` on the call that produced it -- \
                           that binding may cross -- and send it from there";

/// The one shape a destructor may have, checked on the program as written --
/// BEFORE monomorphisation, because a method of a generic type that is never
/// instantiated, or a method with type parameters of its own that is never
/// called, never reaches the lowering at all, and a malformed destructor
/// should not be accepted just because nothing uses its type yet.
///
/// Each refusal is a rule with a reason, not a missing feature:
///   - no parameters and a `void` result: nobody calls it, so there is no one
///     to pass an argument to or to hand a value or an error back to. Rust's
///     `Drop::drop` cannot fail either; a failure the program must see is a
///     `close()` that returns a `Result` and that the destructor also calls.
///   - not `static`: it exists to act on the dying object.
///   - no type parameters of its own: there is no call site to infer them
///     from. (A destructor of a generic TYPE is fine: it is instantiated with
///     each instantiation of the type, like every other method of it.)
///   - not `pub`: it is never called by name, from anywhere, so exporting it
///     would mean nothing -- and one spelling beats two that do the same.
///   - only on a struct: a distinct type is erased and has no object of its
///     own to die; an interface has no objects at all; an enum is refused for
///     now because nothing needs it, and allowing it later breaks nothing
///     where taking it away would.
///   - an interface may not require one: that would be a way to call it.
pub fn check_destructor_decls(p: &Program) -> Result<(), Diag> {
    for t in p.types.iter().filter(|t| t.is_interface) {
        if let Some(m) = t.methods.iter().find(|m| m.name == DESTRUCTOR) {
            return Err(Diag::new(
                m.span,
                format!(
                    "an interface cannot require `{DESTRUCTOR}`: it is the name of a \
                     destructor, which is never called, so it cannot be dispatched to"
                ),
            )
            .in_module(&t.module));
        }
    }
    for f in &p.funcs {
        let Some(recv) = &f.recv else { continue };
        if f.name != DESTRUCTOR {
            continue;
        }
        let tname = bare(recv);
        let refuse = |msg: String| Err(Diag::new(f.span, msg).in_module(&f.module));
        if f.is_static {
            return refuse(format!(
                "`{tname}.{DESTRUCTOR}` is a destructor, which acts on the dying object, \
                 so it cannot be static"
            ));
        }
        if !f.tparams.is_empty() {
            return refuse(format!(
                "`{tname}.{DESTRUCTOR}` is a destructor and cannot have type parameters: \
                 it is never called, so there is nothing to infer them from"
            ));
        }
        if !f.params.is_empty() {
            return refuse(format!(
                "`{tname}.{DESTRUCTOR}` is a destructor and takes no arguments: it runs \
                 when the object dies, and nobody calls it to pass any"
            ));
        }
        if f.ret != Ty::Void {
            return refuse(format!(
                "`{tname}.{DESTRUCTOR}` is a destructor and must return `void`: nobody \
                 receives its result, so it cannot report an error either -- give the \
                 type an ordinary method that returns a Result, and call that from \
                 `{DESTRUCTOR}` too"
            ));
        }
        if f.is_pub {
            return refuse(format!(
                "`{tname}.{DESTRUCTOR}` is a destructor, which is never called by name, \
                 so it cannot be `pub`; remove the `pub`"
            ));
        }
        let decl = p
            .types
            .iter()
            .chain(p.prelude.iter())
            .find(|t| t.name == *recv);
        if let Some(t) = decl {
            let what = if t.distinct_base.is_some() {
                Some("a distinct type, which is erased and has no object of its own to destroy")
            } else if t.is_interface {
                Some("an interface, which has no objects of its own")
            } else if t.is_enum {
                Some(
                    "an enum; a destructor belongs to a struct, whose fields hold what it releases",
                )
            } else {
                None
            };
            if let Some(what) = what {
                return refuse(format!("`{tname}` cannot have a destructor: it is {what}"));
            }
        }
    }
    Ok(())
}

/// The reserved method names beside `drop`: the ones the language itself
/// gives a meaning to, so a type may not use them for anything else.
///
/// Each is a method the program does not call BY NAME. `cmp` and `eq` are
/// what the comparison operators desugar to (§6.2), and all three are what
/// the runtime reaches for when it is holding an object and needs to order
/// it, hash it or compare it: `sort` on a list of a user type, and a `Map`
/// keyed on one. Because nothing at the use site writes the call, nothing at
/// the use site can be told its signature is wrong -- so the signature is
/// fixed here, and checked where the method is DECLARED.
///
/// `(name, the one signature, what the result means)`, with `T` standing for
/// the receiver type.
const RESERVED_METHODS: &[(&str, &str)] = &[
    ("cmp", "int T.cmp(T other)"),
    ("eq", "bool T.eq(T other)"),
    ("hash", "int T.hash()"),
];

/// A reserved method may have exactly one shape, on the program as written
/// -- before monomorphisation, for the reason `check_destructor_decls` gives.
///
/// Why refuse a wrongly-shaped `cmp` at its declaration rather than at the
/// use that wanted it, which is how `to_str` works: `to_str` is called by
/// name in the source that `print(v)` stands for, so the use site has
/// somewhere to put the message. These three are called from the RUNTIME,
/// through a pointer in the TypeInfo, and the program never writes the call
/// at all. A `bool P.cmp(int)` that is only ever wrong when someone sorts a
/// `List<P>` in another module, months later, is a worse error than one on
/// the line that wrote it.
///
/// The shape rules, and why each:
///   - not `static`: all three act on a receiver; a static one has none.
///   - no type parameters of its own: the runtime calls it through one
///     pointer, so there is no call site to infer them from.
///   - `cmp` and `eq` take exactly one parameter, not optional, of the
///     receiver's own type -- or of an interface, which is the other honest
///     reading and is spelled out at the check itself.
///   - `int` from `cmp` (negative, zero, positive), `bool` from `eq`, `int`
///     from `hash`.
///
/// An interface may require them: `interface Ord { int cmp(Ord other); }` is
/// an ordinary one-method interface, and the same shape rule applies with
/// the interface standing in for `T`. Unlike `drop`, these are dispatched
/// to, so there is no reason to refuse.
pub fn check_reserved_decls(p: &Program) -> Result<(), Diag> {
    // Interface requirements: the receiver is the interface itself.
    for t in p.types.iter().chain(p.prelude.iter()) {
        for m in &t.methods {
            check_reserved_shape(p, m, &t.name, &t.module)?;
        }
    }
    for f in &p.funcs {
        let Some(recv) = &f.recv else { continue };
        check_reserved_shape(p, f, recv, &f.module)?;
    }
    Ok(())
}

fn check_reserved_shape(p: &Program, f: &Func, recv: &str, module: &str) -> Result<(), Diag> {
    let Some((_, want)) = RESERVED_METHODS.iter().find(|(n, _)| *n == f.name) else {
        return Ok(());
    };
    let tname = bare(recv);
    let want = want.replace('T', tname);
    let refuse = |why: String| {
        Err(Diag::new(
            f.span,
            format!(
                "`{tname}.{}` is a reserved method and must be declared `{want}`: \
                 {why}. The language calls it itself -- {} -- so its signature is \
                 not the type's to choose; if this method means something else, \
                 give it another name.",
                f.name,
                match f.name.as_str() {
                    "cmp" => "`<`, `<=`, `>`, `>=` and `sort`",
                    "eq" => "`==`, `!=` and a `Map` keyed on this type",
                    _ => "a `Map` keyed on this type",
                }
            ),
        )
        .in_module(module))
    };
    if f.is_static {
        return refuse("this one is static, and a reserved method acts on a receiver".to_string());
    }
    if !f.tparams.is_empty() {
        return refuse(
            "this one has type parameters, and nothing would infer them: the call \
             is made by the runtime, through one pointer"
                .to_string(),
        );
    }
    // A comparison CALLBACK is a different method that happens to share the
    // name: `interface Less { int cmp(Point a, Point b); }` compares two
    // values given to it, where the reserved one compares the receiver with
    // one other. Two parameters is the difference, and it is visible in the
    // declaration -- the runtime only ever installs the receiver-plus-one
    // shape in the type's TypeInfo (`reserved_method`), so the two cannot be
    // confused for each other. Refusing this would ban the callback
    // interface the standard library's `sort.by` is built on
    // (docs/closures-decision.md).
    if f.name != "hash" && f.params.len() == 2 {
        return Ok(());
    }
    if f.params.len() != usize::from(f.name != "hash") {
        return refuse(match f.params.len() {
            0 => "this one takes none".to_string(),
            1 => "this one takes a parameter".to_string(),
            n => format!("this one takes {n} parameters"),
        });
    }
    if let Some(prm) = f.params.first() {
        if prm.is_optional() {
            return refuse("this one's parameter is optional".to_string());
        }
        // The receiver's own type, written as the declaration writes it:
        // `Wrap<T>.cmp(Wrap<T> other)` interns its parameter under the base
        // name, which is the one the receiver carries too.
        //
        // Or an INTERFACE, which is the one other thing it can honestly be:
        // `interface Ord { int cmp(Ord other); }` is an ordinary one-method
        // interface, and a type satisfies it by declaring `int C.cmp(Ord)`.
        // That method means "compare me with any Ord", which is a different
        // promise from "compare me with another C" -- so it is allowed here
        // and it is NOT what `sort` or a map key accepts (`reserved_method`
        // asks for the receiver's own type). One name, and the signature
        // says which of the two it is.
        let named = match prm.ty {
            Ty::User(i) => p.ty_exprs[i as usize].name.clone(),
            _ => String::new(),
        };
        let is_iface = p
            .types
            .iter()
            .chain(p.prelude.iter())
            .any(|t| t.name == named && t.is_interface);
        if named != *recv && !is_iface {
            return refuse(format!("this one takes {}", quoted(&ty_shown(p, prm.ty))));
        }
    }
    let want_ret = if f.name == "eq" { Ty::Bool } else { Ty::Int };
    if f.ret != want_ret {
        return refuse(format!("this one returns `{}`", ty_shown(p, f.ret)));
    }
    Ok(())
}

/// A surface type's name for a diagnostic raised BEFORE the lowerer exists,
/// so `Lowerer::tyname` is not available yet.
fn ty_shown(p: &Program, t: Ty) -> String {
    match t {
        Ty::User(i) => bare(&p.ty_exprs[i as usize].name).to_string(),
        other => other.name().to_string(),
    }
}

/// The representation of a surface type, WITHOUT resolving distinct types
/// and WITHOUT the value-enum rule. Use `Lowerer::irty` instead wherever
/// either can appear, which is everywhere outside this file's own helpers.
fn ir_ty(t: Ty) -> IrTy {
    match t {
        Ty::Int => IrTy::I64,
        Ty::Float => IrTy::F64,
        Ty::Bool => IrTy::I1,
        Ty::Str | Ty::Bytes => IrTy::Ref,
        Ty::User(_) => IrTy::Ref,
        Ty::Void => unreachable!("void is not a value type"),
    }
}

/// Lower a monomorphised program to IR, choosing the representation of each
/// enum as it goes.
///
/// An enum whose payloads are all scalars can be a **value** -- a tag and a
/// union, passed and returned by copy, never allocated and never refcounted.
/// `value_enums` works out which enums those are from the type table alone.
/// One thing it cannot see from there takes a type back to being a heap
/// object: crossing a thread boundary (`Lowerer::transfer`), which needs an
/// object to check for uniqueness. Rather than predict where that happens,
/// this lowers the program, and lowers it again with that type boxed if it
/// turns up. The boxed set only ever grows and is bounded by the number of
/// types, so this terminates; in practice it runs once.
///
/// Whatever comes out is checked by `ir::Module::verify`, which is the reason
/// a missed case here is a loud compiler bug and not a silent miscompile.
pub fn lower_program(p: &Program) -> Result<ir::Module, Diag> {
    let mut boxed: BTreeSet<String> = BTreeSet::new();
    loop {
        // Shared with the lowerer, which is consumed by the attempt, so the
        // demotion it discovered outlives it.
        let asked: Demotions = Rc::new(RefCell::new(BTreeSet::new()));
        let mut lw = Lowerer::new();
        lw.forced_boxed = boxed.clone();
        lw.demote = Rc::clone(&asked);
        match lw.lower_program(p) {
            Ok(mut m) => {
                // docs/perf-board.md item 1, second half: hoist an
                // Array/List/bytes' data pointer and length out of a loop
                // that does nothing opaque to it. Pure IR-to-IR, after
                // lowering and before `verify`, so `verify` checks this
                // pass's output too.
                crate::hoist::hoist_seq_metadata(&mut m);
                m.verify();
                return Ok(m);
            }
            Err(d) => {
                let names = asked.borrow().clone();
                if names.is_empty() {
                    return Err(d); // a real diagnostic, for the person
                }
                let before = boxed.len();
                boxed.extend(names);
                assert!(
                    boxed.len() > before,
                    "internal error: lowering asked to box an enum that is already boxed"
                );
            }
        }
    }
}

/// Enums a lowering attempt found it could not keep as values, shared with
/// the driver above because the attempt consumes the `Lowerer`.
type Demotions = Rc<RefCell<BTreeSet<String>>>;

/// Which enums may be values, by name, with their index in the type table.
///
/// **The rule.** An enum is a value when every payload of every variant is
/// `int`, `float` or `bool` -- or a `distinct` type over one of those, which
/// is erased to it -- or another value enum. Everything else is a heap
/// object, exactly as before.
///
/// Excluded, and why (docs/value-enums.md has the full argument):
///
///   - a `str`, a `bytes`, a collection or any other reference payload: the
///     enum would own a reference, and a copy of it would have to retain --
///     which is refcount traffic in a value, the thing this removes;
///   - an enum that reaches itself through a payload: it would have no
///     finite size. The fixpoint below starts from nothing and only adds, so
///     a recursive enum is never added;
///   - a value enum used as a collection's element, a map's key or value, or
///     a channel's element: those are one machine word each, and a tag plus
///     a union is not. Seeded into `forced` below from the container
///     declarations monomorphisation left in the type table;
///   - an enum that satisfies an interface the program declares: dispatch
///     reads a vtable out of an object header, and a value has none. Checked
///     by name and arity, which over-approximates: an enum that merely looks
///     like it satisfies an interface stays boxed, and is only slower.
///
/// `forced` is what a previous lowering attempt discovered (`lower_program`).
fn value_enums(p: &Program, forced: &BTreeSet<String>) -> HashMap<String, u32> {
    let by_name: HashMap<&str, usize> = p
        .types
        .iter()
        .enumerate()
        .map(|(i, t)| (t.name.as_str(), i))
        .collect();
    let decl_of = |t: Ty| -> Option<usize> {
        match t {
            Ty::User(i) => by_name.get(p.ty_exprs[i as usize].name.as_str()).copied(),
            _ => None,
        }
    };

    // An enum that satisfies an interface can be assigned to an
    // interface-typed slot, and dispatch needs an object header. Name and
    // arity only: a near-miss would be refused by `assignable` anyway, so
    // over-approximating here costs a boxed enum and never correctness.
    let mut forced: BTreeSet<String> = forced.clone();
    for iface in p.types.iter().filter(|t| t.is_interface) {
        if iface.methods.is_empty() {
            continue;
        }
        for e in p.types.iter().filter(|t| t.is_enum) {
            let satisfies = iface.methods.iter().all(|m| {
                p.funcs.iter().any(|f| {
                    f.recv.as_deref() == Some(e.name.as_str())
                        && bare(&f.name) == bare(&m.name)
                        && f.params.len() == m.params.len()
                })
            });
            if satisfies {
                forced.insert(e.name.clone());
            }
        }
    }

    // A container's element, key or value type rides in one machine word.
    // Monomorphisation planted the type arguments as the declaration's
    // `$t0`/`$t1` fields (src/mono.rs, `builtin_decl`).
    for t in &p.types {
        let container = ["Array$", "List$", "Map$", "Chan$"]
            .iter()
            .any(|k| t.name.starts_with(k));
        if container {
            for f in &t.fields {
                if let Some(j) = decl_of(f.ty) {
                    forced.insert(p.types[j].name.clone());
                }
            }
        }
    }

    // Least fixpoint: start with nothing and add an enum once every payload
    // it carries is already known to be fine. Starting from nothing is what
    // excludes a recursive enum -- it can never be the first one added, so it
    // is never added at all, and nor is a mutually recursive group.
    //
    // Nothing takes an enum back OUT of this set. A boxed enum carrying a
    // value enum needs no fixing up, because both have the same
    // tag-and-union layout and the value one sits inline at its own size.
    // That matters more than it sounds: `Result<bytes, Error>` is a heap
    // object because of the `bytes`, and if its payload were a machine-word
    // slot it would have dragged `Error` onto the heap with it -- and with
    // `Error` every `Result<int, Error>` in the program, which is exactly the
    // type `apps/git/zlib.src` wanted in its inner loop.
    let mut ok = vec![false; p.types.len()];
    loop {
        let mut grew = false;
        for (i, t) in p.types.iter().enumerate() {
            if ok[i] || !t.is_enum || forced.contains(&t.name) {
                continue;
            }
            let fine = t.variants.iter().all(|v| {
                v.payload.iter().all(|pt| {
                    // A distinct type is erased to its base, so follow the
                    // chain before deciding.
                    let mut pt = *pt;
                    while let Some(j) = decl_of(pt) {
                        match p.types[j].distinct_base {
                            Some(b) => pt = b,
                            None => break,
                        }
                    }
                    match pt {
                        Ty::Int | Ty::Float | Ty::Bool => true,
                        Ty::User(_) => decl_of(pt).is_some_and(|j| ok[j]),
                        _ => false,
                    }
                })
            });
            if fine {
                ok[i] = true;
                grew = true;
            }
        }
        if !grew {
            break;
        }
    }

    p.types
        .iter()
        .enumerate()
        .filter(|(i, _)| ok[*i])
        .map(|(i, t)| (t.name.clone(), i as u32))
        .collect()
}

impl Lowerer {
    pub fn new() -> Self {
        Lowerer {
            sigs: HashMap::new(),
            typedefs: Vec::new(),
            field_surface: Vec::new(),
            field_params: Vec::new(),
            distinct_base: Vec::new(),
            type_module: Vec::new(),
            type_pub: Vec::new(),
            variant_surface: Vec::new(),
            value_enum: HashMap::new(),
            forced_boxed: BTreeSet::new(),
            demote: Rc::new(RefCell::new(BTreeSet::new())),
            modules: std::collections::HashSet::new(),
            imports_by_module: HashMap::new(),
            cur_module: String::new(),
            building_literal: false,
            statics: std::collections::HashSet::new(),
            ty_exprs: Vec::new(),
            shown: HashMap::new(),
            generic_methods: std::collections::HashSet::new(),
            iface_methods: Vec::new(),
            iface_slots: Vec::new(),
            strings: Vec::new(),
            consts: HashMap::new(),
            const_order: Vec::new(),
            const_stack: Vec::new(),
            static_objs: Vec::new(),
            types: Vec::new(),
            blocks: Vec::new(),
            cur: 0,
            scopes: Vec::new(),
            owned: Vec::new(),
            stmt_temps: Vec::new(),
            freezes: Vec::new(),
            loops: Vec::new(),
            loop_vars: Vec::new(),
            synth: 0,
            moved: Vec::new(),
            payload_binds: Vec::new(),
            recv: None,
            no_recv: String::new(),
            ret_ty: Ty::Void,
            has_destructors: false,
            fn_refs: HashMap::new(),
            synth_funcs: Vec::new(),
            lambdas: 0,
            lambda_targets: HashMap::new(),
            cur_lambda: None,
            entry_locals: std::collections::HashSet::new(),
            entry_module: String::new(),
            in_entry: false,
        }
    }

    fn builtin(&mut self, name: &str, params: Vec<Ty>, ret: Ty) {
        let params = params
            .into_iter()
            .enumerate()
            .map(|(i, ty)| Param {
                ty,
                name: format!("a{i}"),
                default: None,
                embedded: false,
                is_pub: false,
                span: Span::new(0, 0),
            })
            .collect();
        self.sigs.insert(
            name.to_string(),
            Sig {
                params,
                ret,
                module: String::new(),
                is_pub: true,
                is_prim: false,
            },
        );
    }

    pub fn lower_program(mut self, p: &Program) -> Result<ir::Module, Diag> {
        // Builtins. `print` is special-cased in the call path because it
        // accepts int, bool or str and picks the runtime helper statically;
        // that is not user-visible overloading, which does not exist.
        self.builtin("concat", vec![Ty::Str, Ty::Str], Ty::Str);

        self.imports_by_module = p.imports_by_module.clone();
        self.generic_methods = p.generic_methods.clone();
        for f in &p.funcs {
            if !f.module.is_empty() {
                self.modules.insert(f.module.clone());
            }
        }
        for t in &p.types {
            if !t.module.is_empty() {
                self.modules.insert(t.module.clone());
            }
        }
        // A module may declare nothing but constants.
        for c in &p.consts {
            if !c.module.is_empty() {
                self.modules.insert(c.module.clone());
            }
        }

        // After monomorphisation every Ty::User names a concrete declaration
        // with no arguments, so resolution is a name lookup.
        self.ty_exprs = p.ty_exprs.clone();
        self.shown = p.shown.clone();

        // Which enums are values rather than heap objects. Decided from the
        // type table alone, before anything is lowered, because `irty` is
        // asked the moment the first field type is resolved below -- and the
        // answer has to be the same everywhere in the program or two spellings
        // of one type would disagree about its shape.
        self.value_enum = value_enums(p, &self.forced_boxed);

        // Type table first: signatures and field types may refer to any type,
        // including one declared later in the file.
        for t in &p.types {
            self.check_not_import(&t.module, crate::ast::bare(&t.name), t.span)
                .map_err(|d| d.in_module(&t.module))?;
            if self.typedefs.iter().any(|d| d.name == t.name) {
                return Err(Diag::new(
                    t.span,
                    format!("type `{}` is already defined", self.show_name(&t.name)),
                ));
            }
            let mut fields = Vec::new();
            for f in &t.fields {
                if fields.iter().any(|(n, _): &(String, IrTy)| *n == f.name) {
                    return Err(Diag::new(
                        f.span,
                        format!(
                            "duplicate field `{}` in type `{}`",
                            f.name,
                            self.show_name(&t.name)
                        ),
                    ));
                }
                fields.push((f.name.clone(), self.irty(f.ty)));
            }
            let mut variants = Vec::new();
            let mut vsurface = Vec::new();
            for v in &t.variants {
                variants.push(ir::Variant {
                    name: v.name.clone(),
                    payload: v.payload.iter().map(|p| self.irty(*p)).collect(),
                });
                vsurface.push(v.payload.clone());
            }
            self.variant_surface.push(vsurface);
            self.type_module.push(t.module.clone());
            self.type_pub.push(t.is_pub);
            self.typedefs.push(TypeDef {
                name: t.name.clone(),
                fields,
                variants,
                is_enum: t.is_enum,
                is_value: self.value_enum.contains_key(&t.name),
                is_interface: t.is_interface,
                is_chan: t.name.starts_with("Chan$")
                    || t.name.starts_with("Array$")
                    || t.name.starts_with("List$")
                    || t.name.starts_with("Map$"),
                is_distinct: t.distinct_base.is_some(),
                vtable: Vec::new(),
                // Filled in once every method is known, beside the vtable.
                destructor: None,
                cmp: None,
                hash: None,
                eq: None,
                resource: None,
            });
            self.distinct_base.push(t.distinct_base);
            self.iface_methods.push(t.methods.clone());
            // One dispatch slot per distinct interface method name, for the
            // whole program.
            for m in &t.methods {
                let slot = self.slot_of(m);
                if !self.iface_slots.contains(&slot) {
                    self.iface_slots.push(slot);
                }
            }
            self.field_surface
                .push(t.fields.iter().map(|f| f.ty).collect());
            self.field_params.push(t.fields.clone());
        }

        // A FIELD may not take the name of a module its own file imports
        // either. Inside a method a field is read by its bare name (§4.3),
        // so `text.split(..)` is both a method call on the field and a call
        // into the module `text`, and one of the two would silently win --
        // the thing §4.1 exists to prevent. It is checked after the loop
        // above rather than inside it because a field PROMOTED through
        // embedding (§3.5) claims the name exactly as an own field does,
        // and that cannot be asked until every type is registered.
        for t in &p.types {
            let Some(tid) = self.typedefs.iter().position(|d| d.name == t.name) else {
                continue;
            };
            let tid = tid as u32;
            let mut names = Vec::new();
            self.reachable_field_names(tid, &mut names);
            // Visibility is judged from the type's own module, which is the
            // only one that may declare a method on it (§2.1).
            let saved = std::mem::replace(&mut self.cur_module, t.module.clone());
            let clash = names
                .into_iter()
                .find(|n| {
                    *n != t.module
                        && self.module_imports(&t.module, n)
                        && self
                            .field_path(tid, n)
                            .is_some_and(|p| self.check_field_access(tid, &p, n, t.span).is_ok())
                })
                .map(|n| {
                    let span = t
                        .fields
                        .iter()
                        .find(|f| f.name == n)
                        .map_or(t.span, |f| f.span);
                    Diag::new(
                        span,
                        format!(
                            "`{n}` is a field of `{}` and also the module `{n}` this file \
                             imports; shadowing is not allowed, rename the field or drop \
                             the import",
                            self.show_name(&t.name)
                        ),
                    )
                    .in_module(&t.module)
                });
            self.cur_module = saved;
            if let Some(d) = clash {
                return Err(d);
            }
        }

        for f in &p.funcs {
            let _tag = f.module.clone();
            // Habit from C, Java and Go. Without this it declares an ordinary
            // function nothing calls, and the program silently does nothing --
            // the worst failure mode for someone who has written C before.
            // Names are interned module-qualified by now, so compare the bare
            // one; comparing `f.name` itself had quietly stopped matching.
            if f.recv.is_none() && crate::ast::bare(&f.name) == "main" {
                return Err(Diag::new(
                    f.span,
                    "there is no `main`: statements at the top level are the program",
                )
                .in_module(&f.module));
            }
            // In every module, not only the entry file. Allowing it in an
            // imported module would give `lib.print(..)` and bare `print(..)`
            // two meanings a reader has to keep apart, for no gain.
            if f.recv.is_none() && BUILTIN_FNS.contains(&crate::ast::bare(&f.name)) {
                return Err(Diag::new(
                    f.span,
                    format!(
                        "`{}` is a builtin function; shadowing is not allowed, rename this one",
                        crate::ast::bare(&f.name)
                    ),
                )
                .in_module(&f.module));
            }
            if f.recv.is_none() {
                self.check_not_import(&f.module, crate::ast::bare(&f.name), f.span)
                    .map_err(|d| d.in_module(&f.module))?;
            }
            if self.sigs.contains_key(&f.key()) {
                return Err(Diag::new(
                    f.span,
                    format!("`{}` is already defined", f.key()),
                ));
            }
            if f.is_static {
                // `Type.name(..)` is also how an enum variant is written, so
                // a static method may not take a variant's name -- there
                // would be no way to say which was meant.
                if let Some(r) = &f.recv {
                    if let Some(d) = self.typedefs.iter().find(|d| d.name == *r) {
                        if d.variants.iter().any(|v| v.name == f.name) {
                            return Err(Diag::new(
                                f.span,
                                format!(
                                    "`{}` already has a variant `{}`, and \
                                     `{}.{}` would be both",
                                    self.bare_name(r),
                                    f.name,
                                    self.bare_name(r),
                                    f.name
                                ),
                            ));
                        }
                    }
                }
                self.statics.insert(f.key());
            }
            if let Some(r) = &f.recv {
                // A module may only add methods to types it declared itself.
                // Otherwise one module could reach into another's private
                // type by declaring a method on it -- and a method written
                // far from its type is hard to find even when it is allowed.
                if let Some(i) = self.typedefs.iter().position(|d| d.name == *r) {
                    let owner = self.type_module[i].clone();
                    if !owner.is_empty() && owner != f.module {
                        return Err(Diag::new(
                            f.span,
                            format!(
                                "`{r}` is declared in `{owner}`; a method may only \
                                 be added to a type its own module declared"
                            ),
                        ));
                    }
                }
                if !self.typedefs.iter().any(|d| d.name == *r) {
                    // A bare receiver that another module declares is the
                    // common mistake here, and "unknown type" does not say
                    // what to do about it.
                    let suffix = format!("#{r}");
                    if let Some(i) = self.typedefs.iter().position(|d| d.name.ends_with(&suffix)) {
                        return Err(Diag::new(
                            f.span,
                            format!(
                                "`{}` is declared in `{}`; a method may only be \
                                 added to a type its own module declared",
                                self.bare_name(r),
                                self.type_module[i]
                            ),
                        ));
                    }
                    return Err(Diag::new(f.span, format!("unknown type `{r}`")));
                }
            }
            // A type name wins in construction position, so a function
            // sharing one is silently unreachable. Names are case-blind here
            // -- nothing requires a type to be capitalised -- which makes the
            // collision easy to hit by accident.
            if self.typedefs.iter().any(|d| d.name == f.name) {
                return Err(Diag::new(
                    f.span,
                    format!("`{}` is already a type", crate::ast::bare(&f.name)),
                ));
            }
            self.sigs.insert(
                f.key(),
                Sig {
                    params: f.params.clone(),
                    ret: f.ret,
                    module: f.module.clone(),
                    is_pub: f.is_pub,
                    is_prim: f.is_prim,
                },
            );
        }

        // Embedding promotes methods by SYNTHESISING FORWARDERS, one per
        // promoted method, rather than by teaching every call site about
        // embedding. Everything downstream -- direct calls, vtables,
        // interface satisfaction -- then works unchanged, and transitivity
        // falls out for free, because an inner type's own forwarders are
        // already methods by the time the outer one looks.
        let forwarders = self.embed_forwarders(p)?;
        Self::check_c_name_collisions(p, &forwarders)?;
        for f in &forwarders {
            self.sigs.insert(
                f.key(),
                Sig {
                    params: f.params.clone(),
                    ret: f.ret,
                    module: f.module.clone(),
                    is_pub: f.is_pub,
                    is_prim: f.is_prim,
                },
            );
        }

        // Constants after every function and type is known, so a constant
        // cannot take a name one of them has, and before any body is
        // lowered, so every use finds its value already computed.
        self.register_consts(p)?;

        // There is no `main`. The statements written at the top level are
        // the program, in source order, and they are lowered as the body of
        // one synthesised function. Declarations are order-independent, so a
        // function may be called above its own definition.
        let entry = Func {
            module: p.module.clone(),
            is_pub: false,
            ret: Ty::Void,
            is_static: false,
            is_prim: false,
            recv: None,
            name: ENTRY.to_string(),
            tparams: Vec::new(),
            recv_tparams: Vec::new(),
            params: Vec::new(),
            body: p.toplevel.clone(),
            span: Span::new(1, 1),
        };

        self.has_destructors = self.any_destructor();
        // Only the entry file may hold statements (src/modules.rs), so these
        // names belong to exactly one body: this one.
        self.entry_module = p.module.clone();
        self.entry_locals = entry
            .body
            .iter()
            .filter_map(|s| match s {
                Stmt::Decl { name, .. } => Some(name.clone()),
                _ => None,
            })
            .collect();
        let mut funcs = Vec::new();
        for f in &p.funcs {
            // A primitive has no body to lower: its implementation is the
            // runtime function the call site names. Emitting a definition
            // here would collide with the real one at link time.
            if f.is_prim {
                continue;
            }
            funcs.push(self.lower_func(f)?);
        }
        for f in &forwarders {
            funcs.push(self.lower_func(f)?);
        }
        funcs.push(self.lower_func(&entry)?);
        // A function's name used as a value synthesised a wrapper type and a
        // forwarding method while the body above was being lowered
        // (`fn_ref_wrapper`). Lower those now: a forwarder's body is one call
        // whose arguments are its own parameters, so it cannot synthesise
        // another -- the loop is for the invariant, not for a real cycle.
        while let Some(f) = self.synth_funcs.pop() {
            funcs.push(self.lower_func(&f)?);
        }
        // Fill each concrete type's vtable now that every method is known.
        let slots = self.iface_slots.clone();
        for i in 0..self.typedefs.len() {
            if self.typedefs[i].is_interface
                || self.typedefs[i].is_chan
                || self.typedefs[i].is_distinct
            {
                continue;
            }
            let tname = self.typedefs[i].name.clone();
            // The fill CHECKS rather than trusts. A bare name lookup put a
            // static -- whose signature is one argument short -- into a slot
            // that the call site then cast to the interface's shape.
            let vt: Vec<Option<String>> = slots
                .iter()
                .map(|slot| {
                    let key = format!("{tname}.{}", slot.name);
                    if self.statics.contains(&key) {
                        return None;
                    }
                    // The SHAPE has to match too. A name lookup alone would
                    // put a method into a slot the call site casts to some
                    // other signature.
                    let sig = self.sigs.get(&key)?;
                    let shape = ir::Slot {
                        name: slot.name.clone(),
                        params: sig.params.iter().map(|p| self.irty(p.ty)).collect(),
                        ret: (sig.ret != Ty::Void).then(|| self.irty(sig.ret)),
                    };
                    (shape == *slot).then_some(key)
                })
                .collect();
            self.typedefs[i].vtable = vt;
            // The reserved methods go beside the destructor, for the same
            // reason: the RUNTIME calls them, so it needs the pointer in the
            // TypeInfo. Recorded for every type that has one, whether or not
            // this program sorts or keys on it -- one `(CmpFn)` in a static
            // initialiser costs nothing, and making it conditional would
            // mean the TypeInfo depended on the program's uses of the type.
            self.typedefs[i].cmp = self.reserved_method(i as u32, "cmp");
            self.typedefs[i].hash = self.reserved_method(i as u32, "hash");
            self.typedefs[i].eq = self.reserved_method(i as u32, "eq");
            // `check_destructor_decls` has already refused every other shape
            // and every other kind of type, and forwarders never carry the
            // name, so a method of this name here is the type's own
            // destructor.
            let dkey = format!("{tname}.{DESTRUCTOR}");
            if self.sigs.contains_key(&dkey) {
                self.typedefs[i].destructor = Some(dkey);
                // Spelled as the entry file would spell it: bare for its
                // own types, `io.File` for an imported one.
                let saved = std::mem::replace(&mut self.cur_module, p.module.clone());
                self.typedefs[i].resource = Some(self.show_name(&tname));
                self.cur_module = saved;
            }
        }

        // Every `prim`'s runtime symbol, by the same name-transform rule
        // `lower_call` applies at its own call sites (docs/stdlib-seam.md
        // §2) -- computed once, here, rather than re-derived ad hoc wherever
        // it is needed later (src/emit_c.rs, for the blocking-FFI wrap; see
        // `ir::Module::prim_targets`'s own doc comment for why this can't be
        // recovered from an `Inst::Call`'s name alone).
        let prim_targets: std::collections::HashSet<String> = self
            .sigs
            .iter()
            .filter(|(_, sig)| sig.is_prim)
            .map(|(name, _)| format!("rt_{}", crate::ast::bare(name).trim_start_matches('_')))
            .collect();

        Ok(ir::Module {
            funcs,
            strings: self.strings,
            statics: self.static_objs,
            types: self.typedefs,
            iface_slots: self.iface_slots,
            prim_targets,
        })
    }

    fn new_val(&mut self, t: IrTy) -> Value {
        let v = Value(self.types.len() as u32);
        self.types.push(t);
        v
    }

    fn new_block(&mut self) -> BlockId {
        let id = BlockId(self.blocks.len() as u32);
        self.blocks.push(BlockBuf {
            id,
            params: Vec::new(),
            insts: Vec::new(),
            term: None,
        });
        id
    }

    fn switch_to(&mut self, b: BlockId) {
        self.cur = self
            .blocks
            .iter()
            .position(|x| x.id == b)
            .expect("unknown block");
    }

    fn push(&mut self, i: Inst) {
        debug_assert!(
            self.blocks[self.cur].term.is_none(),
            "instruction after terminator"
        );
        self.blocks[self.cur].insts.push(i);
    }

    fn terminate(&mut self, t: Term) {
        if self.blocks[self.cur].term.is_none() {
            self.blocks[self.cur].term = Some(t);
        }
    }

    fn terminated(&self) -> bool {
        self.blocks[self.cur].term.is_some()
    }

    /// A fresh IR value holding an enum of type `tid`, in whichever shape
    /// that enum has: a `Ref` to a heap object, or the value enum's own
    /// struct. Every place that produces or joins an enum goes through here,
    /// so the choice is made once.
    fn enum_val(&mut self, tid: u32) -> Value {
        let t = if self.typedefs[tid as usize].is_value {
            IrTy::Val(tid)
        } else {
            IrTy::Ref
        };
        self.new_val(t)
    }

    /// The dispatch slot an interface method occupies: its name and its
    /// IR-level shape. IR-level rather than surface, because the call site's
    /// cast is built from IR types -- so `int area()` and `Price area()`
    /// share a slot, which is right, and `int m()` and `str m(str)` do not.
    fn slot_of(&self, m: &Func) -> ir::Slot {
        ir::Slot {
            name: m.name.clone(),
            params: m.params.iter().map(|p| self.irty(p.ty)).collect(),
            ret: (m.ret != Ty::Void).then(|| self.irty(m.ret)),
        }
    }
}

/// A name with its indefinite article, for a diagnostic: `an Array<int>`,
/// `a List<int>`. By the first letter, which is right for every type name
/// the language has.
/// `a_or_an`, with the name in backticks: the article is chosen from the
/// name, not from the quote in front of it.
fn quoted(name: &str) -> String {
    let vowel = name
        .chars()
        .next()
        .is_some_and(|c| "aeiouAEIOU".contains(c));
    format!("{} `{name}`", if vowel { "an" } else { "a" })
}

fn a_or_an(name: &str) -> String {
    let vowel = name
        .chars()
        .next()
        .is_some_and(|c| "aeiouAEIOU".contains(c));
    format!("{} {name}", if vowel { "an" } else { "a" })
}

fn stmt_span(s: &Stmt) -> Span {
    match s {
        Stmt::Decl { span, .. }
        | Stmt::Assign { span, .. }
        | Stmt::Return { span, .. }
        | Stmt::Eval { span, .. }
        | Stmt::While { span, .. }
        | Stmt::Break { span, .. }
        | Stmt::Continue { span, .. }
        | Stmt::ForIn { span, .. }
        | Stmt::ForRange { span, .. }
        | Stmt::Spawn { span, .. }
        | Stmt::SetIndex { span, .. }
        | Stmt::SetField { span, .. }
        | Stmt::Match { span, .. }
        | Stmt::If { span, .. }
        | Stmt::ConstBlock { span, .. } => *span,
    }
}
