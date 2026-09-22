//! Abstract syntax tree.
//!
//! Deliberately thin: the AST exists to be typechecked and lowered, and every
//! interesting transformation happens on the IR instead. See docs/ir-v0.md §4.

use crate::diag::Span;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ty {
    Int,
    Float,
    Bool,
    Str,
    /// A mutable, growable sequence of octets. A builtin like `str` rather
    /// than a predeclared generic like `List`, because it has no type
    /// argument: its element is always a byte, stored as one, not as an
    /// int64 slot. See docs/reference.md §3.10.
    Bytes,
    Void,
    /// A named type -- user-defined, generic instance, or type parameter --
    /// indexed into the program's `ty_exprs` arena.
    ///
    /// Interning keeps `Ty` `Copy` and one word wide even though a type
    /// expression like `Box<Pair<int, str>>` is a tree. Monomorphisation
    /// resolves every one of these to a concrete declaration.
    User(u32),
}

impl Ty {
    pub fn name(self) -> &'static str {
        match self {
            Ty::Int => "int",
            Ty::Float => "float",
            Ty::Bool => "bool",
            Ty::Str => "str",
            Ty::Bytes => "bytes",
            Ty::Void => "void",
            // Named through the type table by the caller where a real name
            // is needed; this fallback only appears in internal messages.
            Ty::User(_) => "<type>",
        }
    }

    /// Whether values of this type are reference counted: `str`, `bytes`
    /// and every named type. This is the single predicate the refcount pass
    /// consults.
    pub fn is_ref(self) -> bool {
        matches!(self, Ty::Str | Ty::Bytes | Ty::User(_))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
}

impl BinOp {
    pub fn spelling(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::Rem => "%",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnOp {
    Neg,
    Not,
    /// `~x`, bitwise complement. Spelled apart from `!` because `bool` is not
    /// an integer here: `!` is logical and `~` flips bits, and neither
    /// applies to the other's type.
    BitNot,
}

/// The part of a declaration's name a reader wrote.
///
/// Declarations are interned module-qualified (`lib#Point`) so two modules
/// may each declare a `Point`. Diagnostics carry a file and a line, so the
/// bare name is unambiguous in context and is what the reader typed.
pub fn bare(name: &str) -> &str {
    match name.split_once('#') {
        Some((_, n)) => n,
        None => name,
    }
}

/// One interned type expression: a name plus type arguments.
/// `Point` is `("Point", [])`; `Box<int>` is `("Box", [Int])`; the `T` inside
/// a generic declaration is `("T", [])` and is resolved by substitution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TyExpr {
    pub name: String,
    pub args: Vec<Ty>,
}

/// One variant of an enum: a name and the types it carries.
///
/// The payload is positional and unnamed. A variant is not a struct -- if
/// there is enough in it to want field names, the payload should be a struct.
#[derive(Debug, Clone)]
pub struct EnumVariant {
    pub name: String,
    pub payload: Vec<Ty>,
    pub span: Span,
}

/// A user-defined type declaration, possibly generic.
///
/// Three shapes: a struct (`fields`), an interface (`methods`, signatures
/// only) and an enum (`variants`).
#[derive(Debug, Clone)]
pub struct TypeDecl {
    pub name: String,
    /// The module that declared this. Set by the loader, so a merged program
    /// can still tell whose declaration is whose -- which is what makes
    /// privacy enforceable once every file is in one `Program`.
    pub module: String,
    /// Exported from its module. Private is the default: forgetting to mark
    /// something private would export it permanently, while forgetting to
    /// mark something public is a one-word fix.
    pub is_pub: bool,
    /// Type parameter names, empty for a non-generic type.
    pub tparams: Vec<String>,
    pub fields: Vec<Param>,
    /// Required method signatures; non-empty only for an interface. Bodies
    /// are empty.
    pub methods: Vec<Func>,
    pub is_interface: bool,
    /// Non-empty only for an enum: the variants, in declaration order. The
    /// index in this list is the runtime tag.
    pub variants: Vec<EnumVariant>,
    pub is_enum: bool,
    /// `distinct int Price;` -- same representation as the base type, a
    /// different identity to the type checker, and nothing at all at
    /// runtime. Erased before the IR, like generics.
    pub distinct_base: Option<Ty>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Expr {
    Int(i64, Span),
    Float(f64, Span),
    Bool(bool, Span),
    Str(String, Span),
    Var(String, Span),
    /// `this` -- the whole receiver of an instance method, borrowed like a
    /// parameter. Its own node rather than `Var("this")` so no pass can
    /// mistake it for a local: it cannot be declared, assigned or moved,
    /// and what it denotes depends on the enclosing method, not on a scope.
    This(Span),
    Bin(BinOp, Box<Expr>, Box<Expr>, Span),
    Un(UnOp, Box<Expr>, Span),
    Call(String, Args, Span),
    /// `obj.method(args)`
    MethodCall(Box<Expr>, String, Args, Span),
    /// `expr.field`
    Field(Box<Expr>, String, Span),
    /// `expr[index]`
    Index(Box<Expr>, Box<Expr>, Span),
    /// `[]`, `[a, b, c]` -- the elements of a List or an Array.
    ///
    /// Typed by its context, so it is only legal where the expected type is
    /// known: a declaration, an argument, a return. There is nothing to
    /// infer from `[]` on its own.
    SeqLit(Vec<Expr>, Span),
    /// `[x; n]` -- n copies of x. Rust's spelling. This replaces the old
    /// `Array<int>(n, fill)` rather than joining it: one way to say a thing.
    RepeatLit(Box<Expr>, Box<Expr>, Span),
    /// `{}`, `{k: v, ..}` -- the entries of a Map.
    MapLit(Vec<(Expr, Expr)>, Span),
    /// `f()?` -- give me the value, or return the failure from here.
    ///
    /// Sugar for a `match` that returns the `Err` or `None` arm unchanged.
    /// It hides a return, which is a fair thing to dislike -- but it hides
    /// one specific return, always in the same place, and the signature
    /// still says the function can fail.
    Try(Box<Expr>, Span),
    /// `Option<int>.Some(1)` -- the enum type, the variant name, the
    /// payload. The type is written in full rather than inferred: a variant
    /// with no payload has nothing to infer from, and one rule beats a rule
    /// with an exception.
    EnumNew(Ty, String, Args, Span),
    /// `Point(x: 1, y: 2)` / `Box<int>(value: 5)` -- construction is always
    /// by field name, so reordering fields in a declaration cannot silently
    /// transpose values. Carries the interned type, so type arguments survive
    /// to monomorphisation.
    New(Ty, Args, Span),
}

impl Expr {
    pub fn span(&self) -> Span {
        match self {
            Expr::Int(_, s)
            | Expr::Float(_, s)
            | Expr::Bool(_, s)
            | Expr::Str(_, s)
            | Expr::Var(_, s)
            | Expr::This(s)
            | Expr::Bin(_, _, _, s)
            | Expr::Un(_, _, s)
            | Expr::Call(_, _, s)
            | Expr::MethodCall(_, _, _, s)
            | Expr::Field(_, _, s)
            | Expr::Index(_, _, s)
            | Expr::New(_, _, s)
            | Expr::EnumNew(_, _, _, s)
            | Expr::Try(_, s)
            | Expr::SeqLit(_, s)
            | Expr::RepeatLit(_, _, s)
            | Expr::MapLit(_, s) => *s,
        }
    }
}

#[derive(Debug, Clone)]
pub enum Stmt {
    /// `int x = expr;` / `const int x = expr;`
    Decl {
        ty: Ty,
        name: String,
        init: Expr,
        is_const: bool,
        span: Span,
    },
    /// `x = expr;`
    Assign {
        name: String,
        value: Expr,
        span: Span,
    },
    /// `obj[i] = expr;`
    SetIndex {
        obj: Expr,
        index: Expr,
        value: Expr,
        span: Span,
    },
    /// `obj.field = expr;`
    SetField {
        obj: Expr,
        field: String,
        value: Expr,
        span: Span,
    },
    /// `return expr;` / `return;`
    Return { value: Option<Expr>, span: Span },
    /// A call evaluated for effect.
    Eval { expr: Expr, span: Span },
    /// `spawn f(args);` -- run `f` on its own thread.
    Spawn {
        name: String,
        args: Args,
        span: Span,
    },
    /// `break;`
    Break { span: Span },
    /// `continue;`
    Continue { span: Span },
    /// `for (int x in xs) { .. }`
    ForIn {
        ty: Ty,
        name: String,
        iter: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    /// `while (cond) { .. }`
    While {
        cond: Expr,
        body: Vec<Stmt>,
        span: Span,
    },
    /// `match (e) { case V(int x): { .. } .. }`
    ///
    /// Exhaustive, and with no fallthrough. There is no `default`: leaving it
    /// out means adding a variant is a compile error at every match that
    /// needs updating, which is the entire reason to have enums checked.
    Match {
        scrutinee: Expr,
        arms: Vec<MatchArm>,
        span: Span,
    },
    /// `if (cond) { .. } else { .. }`
    If {
        cond: Expr,
        then: Vec<Stmt>,
        els: Option<Vec<Stmt>>,
        span: Span,
    },
}

/// One arm of a `match`: a variant name, the bindings for its payload, and
/// the body. Bindings are type-first like every other binding in the
/// language -- `case Some(int v)`.
#[derive(Debug, Clone)]
pub struct MatchArm {
    pub variant: String,
    pub binds: Vec<Param>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

/// A parameter or a field.
///
/// Oro's rule, applied to both: **a parameter with no default is positional,
/// one with a default is named.** Never both. That removes the question of
/// whether to pass something positionally and the question of what order
/// optional arguments come in.
#[derive(Debug, Clone)]
pub struct Param {
    pub ty: Ty,
    pub name: String,
    pub default: Option<Expr>,
    /// An embedded field: written as a bare type with no name, and named
    /// after its type. Its fields and methods are promoted onto the outer
    /// type -- composition in place of inheritance.
    pub embedded: bool,
    /// A field marked `pub`: readable, writable and constructible from
    /// other modules. Fields follow the rule every other declaration does --
    /// private by default -- so a type's invariants are its module's to keep
    /// (docs/modules-decision.md §2). Always false for a parameter or a
    /// match binding, which have no visibility of their own.
    pub is_pub: bool,
    pub span: Span,
}

impl Param {
    pub fn is_optional(&self) -> bool {
        self.default.is_some()
    }
}

/// A call's or construction's arguments: positional ones fill the mandatory
/// parameters in order, named ones fill the optional parameters in any order.
#[derive(Debug, Clone, Default)]
pub struct Args {
    pub pos: Vec<Expr>,
    pub named: Vec<(String, Expr)>,
}

#[derive(Debug, Clone)]
pub struct Func {
    /// The module that declared this -- see `TypeDecl::module`.
    pub module: String,
    /// Exported from its module -- see `TypeDecl::is_pub`.
    pub is_pub: bool,
    pub ret: Ty,
    /// A method on the TYPE rather than on a value: `static Point
    /// Point.origin()`. It has no receiver, so a bare field name means
    /// nothing inside it, and it is called as `Point.origin()`.
    ///
    /// This is what lets a conversion dispatch on its TARGET -- `Price.parse`
    /// reads text and produces a Price, which single dispatch on a receiver
    /// cannot express because there is no Price yet to dispatch on.
    pub is_static: bool,
    /// The seam to C: a declaration with no body, whose implementation is a
    /// runtime function found by one name transform (`__open` ->
    /// `rt_open`). Only source the compiler ships may declare one --
    /// see docs/stdlib-seam.md. `body` is empty and never read.
    pub is_prim: bool,
    /// For a method, the receiver type's name: `int Rect.area()` has
    /// `recv = Some("Rect")`. Methods are declared outside the type body so
    /// they can be added to any type, and so a type declaration stays a list
    /// of fields.
    pub recv: Option<String>,
    pub name: String,
    /// Type parameter names, empty for a non-generic function.
    pub tparams: Vec<String>,
    /// For a method on a generic type, the names its receiver's type
    /// parameters take inside it: `T Box<T>.get()` has `["T"]`. Written at
    /// the method rather than borrowed silently from the type declaration,
    /// so a method's signature can be read without finding its type.
    /// Monomorphisation instantiates the method once per instantiation of
    /// the type, and empties this.
    pub recv_tparams: Vec<String>,
    pub params: Vec<Param>,
    pub body: Vec<Stmt>,
    pub span: Span,
}

impl Func {
    /// The name this function is known by: `area` for a plain function,
    /// `Rect.area` for a method. `.` cannot appear in a source identifier, so
    /// the two namespaces cannot collide.
    pub fn key(&self) -> String {
        match &self.recv {
            Some(r) => format!("{r}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

/// `[pub] const <type> NAME = <constant expression>;` at the top level of a
/// module -- docs/reference.md §4.4.
///
/// Its own item rather than a `Stmt::Decl` with a flag, because it is not a
/// statement: it does not run, it has no place in the program's order, and
/// an imported module may declare one. The compiler computes its value, and
/// the emitted C holds that value as static, immortal data.
#[derive(Debug, Clone)]
pub struct ConstDecl {
    /// The module that declared this -- see `TypeDecl::module`.
    pub module: String,
    /// Exported from its module -- see `TypeDecl::is_pub`.
    pub is_pub: bool,
    pub ty: Ty,
    /// Interned module-qualified, as a free function's name is: `lib#MAX`.
    pub name: String,
    pub init: Expr,
    pub span: Span,
}

/// One `import name;` -- the module this file depends on.
#[derive(Debug, Clone)]
pub struct Import {
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Program {
    /// The module this program came from: the source file's basename. The
    /// entry file's is unused; every other module's qualifies its exports.
    pub module: String,
    pub imports: Vec<Import>,
    /// What every module in the merged program imports, by module name.
    /// `imports` is one file's list and does not survive the merge, but
    /// shadowing is checked per file: a local may not take the name of a
    /// module its own file imports, and nothing another file imports is
    /// that file's business.
    pub imports_by_module: std::collections::HashMap<String, Vec<String>>,
    pub types: Vec<TypeDecl>,
    /// Declarations the compiler supplies rather than the program: `Option`
    /// and `Result`. Kept apart from `types` because they are not part of
    /// the source text -- the formatter must not print them back out, and a
    /// program that redeclares one is redeclaring, not shadowing.
    pub prelude: Vec<TypeDecl>,
    pub funcs: Vec<Func>,
    /// Module-level constants, from every module, in source order.
    pub consts: Vec<ConstDecl>,
    /// Statements written at the top level, in source order. They become the
    /// program's body -- there is no `main`.
    pub toplevel: Vec<Stmt>,
    /// Interned type expressions; `Ty::User` indexes this.
    pub ty_exprs: Vec<TyExpr>,
    /// What each instantiation -- of a type or a function -- was written as,
    /// by mangled name: `List$int` was `("List", [Int])`. Monomorphisation fills it in; a diagnostic
    /// reads it back, because the mangled name is for C and the person
    /// reading the error wrote `List<int>`. Empty before monomorphisation.
    pub shown: std::collections::HashMap<String, (String, Vec<Ty>)>,
    /// Every generic method a concrete receiver has, as `Box$int.first`.
    /// Monomorphisation instantiates one at each call whose receiver's type
    /// it can see written down; a call it could not see through reaches the
    /// lowering uninstantiated, and this lets that say why instead of
    /// claiming the method does not exist.
    pub generic_methods: std::collections::HashSet<String>,
}
