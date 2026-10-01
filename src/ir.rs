//! The IR. See docs/ir-v0.md for the specification this implements.
//!
//! SSA with block parameters rather than phi nodes, three types, sixteen
//! instructions. Backends consume the POST-refcount form only, so no backend
//! ever reasons about ownership.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrTy {
    I64,
    F64,
    I1,
    Ref,
    /// A **value enum**: an enum whose every payload is a scalar or another
    /// value enum, so it is a tag and a union passed and returned by copy.
    /// `u32` is its index in `Module::types`.
    ///
    /// It is NOT managed: no header, no refcount, no allocation. `Ref` is
    /// still the only managed shape, which is what keeps the refcount pass
    /// mechanical (docs/ir-v0.md §2). See `docs/value-enums.md` for the rule
    /// and for why nothing observable changes.
    Val(u32),
}

impl IrTy {
    pub fn c_name(self) -> String {
        match self {
            IrTy::I64 => "int64_t".to_string(),
            IrTy::F64 => "double".to_string(),
            IrTy::I1 => "bool".to_string(),
            IrTy::Ref => "Obj *".to_string(),
            IrTy::Val(i) => format!("T{i}v"),
        }
    }

    /// Whether this shape is refcounted. Only `Ref` is.
    pub fn is_managed(self) -> bool {
        self == IrTy::Ref
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Value(pub u32);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct BlockId(pub u32);

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "v{}", self.0)
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "block{}", self.0)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Cmp {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

impl Cmp {
    pub fn c_op(self) -> &'static str {
        match self {
            Cmp::Eq => "==",
            Cmp::Ne => "!=",
            Cmp::Lt => "<",
            Cmp::Le => "<=",
            Cmp::Gt => ">",
            Cmp::Ge => ">=",
        }
    }

    /// The result when both operands are the same value.
    ///
    /// Comparing a value with itself is legal and the answer does not depend
    /// on what it holds, so the emitter writes the answer rather than a C
    /// comparison the C compiler would warn about.
    pub fn holds_for_equal_operands(self) -> bool {
        matches!(self, Cmp::Eq | Cmp::Le | Cmp::Ge)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    /// The bit operations. `&` `|` `^` cannot overflow; the shifts trap on a
    /// count outside 0..63, where C would be undefined. None of them has a
    /// float form -- the lowerer refuses a float operand.
    And,
    Or,
    Xor,
    Shl,
    Shr,
    /// Two's-complement wrapping `+` `-` `*`, for hashes and PRNGs, which
    /// are defined modulo 2^64 and would trap under the checked ops.
    WrapAdd,
    WrapSub,
    WrapMul,
}

impl ArithOp {
    /// Runtime helper implementing this op. Add/sub/mul are checked and trap
    /// on overflow (docs/ir-v0.md §3); div and rem trap on zero and on the
    /// one signed-overflow case, INT64_MIN / -1.
    /// The plain C operator, for floats -- where IEEE already defines every
    /// case and there is nothing to check.
    pub fn c_op(self) -> &'static str {
        match self {
            ArithOp::Add => "+",
            ArithOp::Sub => "-",
            ArithOp::Mul => "*",
            ArithOp::Div => "/",
            ArithOp::Rem => "%",
            ArithOp::And | ArithOp::Or | ArithOp::Xor | ArithOp::Shl | ArithOp::Shr => {
                unreachable!("bit operations have no float form")
            }
            ArithOp::WrapAdd | ArithOp::WrapSub | ArithOp::WrapMul => {
                unreachable!("wrapping operations have no float form")
            }
        }
    }

    pub fn rt_fn(self) -> &'static str {
        match self {
            ArithOp::Add => "rt_iadd",
            ArithOp::Sub => "rt_isub",
            ArithOp::Mul => "rt_imul",
            ArithOp::Div => "rt_idiv",
            ArithOp::Rem => "rt_irem",
            ArithOp::And => "rt_iand",
            ArithOp::Or => "rt_ior",
            ArithOp::Xor => "rt_ixor",
            ArithOp::Shl => "rt_ishl",
            ArithOp::Shr => "rt_ishr",
            ArithOp::WrapAdd => "rt_wrapping_add",
            ArithOp::WrapSub => "rt_wrapping_sub",
            ArithOp::WrapMul => "rt_wrapping_mul",
        }
    }
}

/// A vtable slot: an interface method's name and its IR-level shape.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub name: String,
    pub params: Vec<IrTy>,
    pub ret: Option<IrTy>,
}

/// One variant of an enum: its name and the IR types of its payload slots.
/// Its index in `TypeDef::variants` is the runtime tag.
#[derive(Debug, Clone)]
pub struct Variant {
    pub name: String,
    pub payload: Vec<IrTy>,
}

/// A user-defined type's layout. The emitter turns each of these into a C
/// struct and, if any field is a `ref`, a drop function that releases them.
#[derive(Debug, Clone)]
pub struct TypeDef {
    pub name: String,
    pub fields: Vec<(String, IrTy)>,
    /// An interface has no fields and is never allocated; it exists so a
    /// value can be typed by what it can do rather than what it is.
    pub is_interface: bool,
    /// A channel: the runtime owns its layout, so the emitter produces no
    /// struct, no drop function and no TypeInfo for it. Field 0 records the
    /// element type and is never stored.
    pub is_chan: bool,
    /// A distinct type: identical representation to its base, a different
    /// identity to the type checker, and no existence at all at runtime.
    pub is_distinct: bool,
    /// One entry per interface-method slot in the program: the IR name of
    /// this type's implementation, or `None` if it has none.
    pub vtable: Vec<Option<String>>,
    /// Non-empty only for an enum. An enum is laid out as a tag and a union
    /// of one struct per variant that carries a payload (src/emit_c.rs,
    /// `emit_enum_body`), and which members hold references depends on the
    /// tag -- so its drop and walk functions switch on it.
    pub variants: Vec<Variant>,
    pub is_enum: bool,
    /// A **value enum**: laid out as a tag and a union, passed and returned
    /// by copy, never allocated and never refcounted. Set only on an enum,
    /// and only when every payload of every variant is a scalar or another
    /// value enum -- see `docs/value-enums.md` for the rule, the proof that
    /// nothing observable changes, and the cases that are excluded.
    ///
    /// A value enum has no object: no header, no TypeInfo, no drop, walk or
    /// copy function, and no vtable. Its values are `IrTy::Val(tid)`.
    pub is_value: bool,
    /// The IR name of the type's destructor (`File.drop`), if it declares
    /// one. The drop function calls it before releasing any field, so the
    /// destructor still sees a whole object (docs/destructors-decision.md).
    pub destructor: Option<String>,
    /// The IR names of the three RESERVED methods the runtime calls on this
    /// type, each present only if the type declares it with exactly the one
    /// signature it may have: `int T.cmp(T)`, `int T.hash()`, `bool T.eq(T)`.
    /// They go into the TypeInfo beside the destructor, which is the same
    /// idea -- a method of a known name that nothing in the program calls by
    /// name -- and are what `sort` and a Map keyed on a user type reach for.
    pub cmp: Option<String>,
    pub hash: Option<String>,
    pub eq: Option<String>,
    /// Beside `destructor`: the type's name as the program spells it
    /// (`io.File`, `Wrap<Res>`). It goes into the TypeInfo so that the
    /// runtime can name the type when a `const` meets a value that owns a
    /// resource -- which only happens through an interface, where the
    /// compiler could not see the type (docs/destructors-decision.md).
    pub resource: Option<String>,
}

impl TypeDef {
    /// Whether the emitter produces no object for this type at all: no C
    /// struct with a header, no TypeInfo, no drop, walk or copy function.
    ///
    /// An interface has no layout of its own, a channel's and a collection's
    /// layout belongs to the runtime, a distinct type is erased to its base,
    /// and a value enum is a plain C struct passed by copy.
    pub fn has_no_object(&self) -> bool {
        self.is_interface || self.is_chan || self.is_distinct || self.is_value
    }

    /// Whether this type holds references, and therefore needs a drop
    /// function. Types that hold none pay no call when freed.
    pub fn needs_drop(&self) -> bool {
        if self.is_value {
            // Every payload is a scalar or another value enum: there is
            // nothing to release, and nothing holds this to release it.
            return false;
        }
        if self.is_enum {
            return self.variants.iter().any(|v| v.payload.contains(&IrTy::Ref));
        }
        self.fields.iter().any(|(_, t)| *t == IrTy::Ref)
    }

    /// Whether the type needs a drop function at all: to release fields,
    /// to run a destructor, or both. `needs_drop` alone decides the walk
    /// function, which only cares about references.
    pub fn has_drop_fn(&self) -> bool {
        self.needs_drop() || self.destructor.is_some()
    }

    /// Whether every value of this type can be one shared static object.
    ///
    /// A type with no fields has nothing to tell two of its values apart:
    /// the object is a bare header, and the header is the same for every
    /// instance. So the emitter writes ONE static instance with
    /// `RC_IMMORTAL` and every construction yields it, instead of a
    /// `rt_alloc` per construction (docs/closures-decision.md, "A callback
    /// with no captures is a static, immortal object"). Nothing observable
    /// changes: `==` on a user type is its `eq` method and never identity,
    /// and an `eq` on a field-less type has nothing to read.
    ///
    /// Four kinds of type are excluded because they are not really
    /// field-less:
    ///
    ///   - an interface, a channel and a distinct type have no object of
    ///     their own at all -- no struct, no TypeInfo, nothing to point at;
    ///   - an enum's `fields` is empty but its object carries a tag and
    ///     payload slots, so its values differ.
    ///
    /// And one because sharing would change behaviour: a type that declares
    /// a DESTRUCTOR keeps allocating. An immortal is never dropped, so its
    /// `drop` would never run -- silently, at every site -- and a `const`
    /// binding of one would stop trapping on the resource it owns. Refusing
    /// the combination outright was the alternative, but a field-less type
    /// with a destructor is a legitimate guard object whose whole content is
    /// its effect, and this optimisation is not a reason to outlaw it. It
    /// loses nothing that matters: a synthesised callback type never has a
    /// destructor, which is the case this exists for.
    pub fn is_immortal_singleton(&self) -> bool {
        !self.is_interface
            && !self.is_chan
            && !self.is_distinct
            && !self.is_enum
            && self.destructor.is_none()
            && self.fields.is_empty()
    }

    /// Whether variant `tag` of this **boxed** enum has no payload, and can
    /// therefore be one shared static instance rather than a fresh
    /// allocation per construction (docs/perf-board.md item 5).
    ///
    /// A value enum already pays nothing to construct -- it is a plain
    /// struct, not an object -- so this only matters for a boxed one, which
    /// otherwise calls `rt_alloc` for every construction regardless of
    /// whether the tag has anything to distinguish one value from another. A
    /// payload-free variant has none, by the same argument
    /// `is_immortal_singleton` makes for a field-less type: nothing about it
    /// can differ between two constructions, so one shared object serves
    /// every one of them. `Option.None` is the case this exists for -- it is
    /// constructed constantly, and `Option<T>` is boxed whenever `T` is a
    /// reference.
    ///
    /// Excluded exactly as `is_immortal_singleton` excludes a destructor:
    /// an immortal is never dropped, so a guard variant's effect would never
    /// run.
    pub fn is_immortal_variant(&self, tag: usize) -> bool {
        self.is_enum
            && !self.is_value
            && self.destructor.is_none()
            && self
                .variants
                .get(tag)
                .map(|v| v.payload.is_empty())
                .unwrap_or(false)
    }

    /// Whether variant `tag` is the OS-errno passthrough variant of one of
    /// the three stdlib error enums the compiler special-cases by name --
    /// `io#Error`, `net#Error`, `term#Error` -- each of which has exactly
    /// one `Other(int)` variant carrying a raw Linux errno
    /// (docs/errors-decision.md, "the OS-errno class needs a real
    /// mechanism").
    ///
    /// Every OTHER variant of these three types is payload-free and already
    /// gets the ordinary 8-byte `{ tag: int64 }` layout for free, same as
    /// any other payload-free value enum. `Other` cannot join them the same
    /// way, because its payload is not fixed at compile time -- but it CAN
    /// still avoid a union member, by packing the errno directly into the
    /// tag word instead of beside it: `OS_ERRNO_TAG_BASE | errno`. The
    /// reserved high bit can never collide with a real enum's small number
    /// of ordinary declaration-order tags (0, 1, 2, ...), so a plain
    /// `tag >= OS_ERRNO_TAG_BASE` test is exactly "this is `Other`" --
    /// `Inst::EnumPack` computes the tag this way instead of using the
    /// constant every other variant uses, `Inst::EnumPayload` reads the
    /// errno back out of it with a mask instead of a union read, and
    /// `lower::lower_match` emits the range test instead of an equality
    /// test when dispatching to this one arm. `emit_enum_body` skips a
    /// union member for it entirely, which is what takes these three types
    /// from 16 bytes to 8.
    ///
    /// Named explicitly rather than detected structurally -- "a value enum
    /// with every variant payload-free except one carrying a single `int`"
    /// -- because that shape says nothing about the RANGE of the int. A
    /// general one can be negative or exceed 32 bits, and either would be
    /// silently corrupted by this packing; it is sound only because a Linux
    /// errno is always a small non-negative number. The corpus has over a
    /// dozen fixtures with this exact structural shape for unrelated
    /// reasons (`corpus/core/1281`, `1283`, `1285`, ... -- deliberately
    /// exercising the ordinary value-enum rule on an int payload), and a
    /// structural rule would have reached into every one of them.
    pub fn os_errno_variant(&self, tag: usize) -> bool {
        self.is_enum
            && self.is_value
            && matches!(self.name.as_str(), "io#Error" | "net#Error" | "term#Error")
            && self
                .variants
                .get(tag)
                .is_some_and(|v| v.name == "Other" && v.payload == [IrTy::I64])
    }
}

/// The reserved high bit that marks a computed tag -- see
/// `TypeDef::os_errno_variant`. `1 << 32` keeps the errno itself in the
/// tag's low 32 bits untouched by the marker, so recovering it is a plain
/// mask (`tag & 0xFFFFFFFF`) and no ordinary declaration-order tag (always
/// a handful of small integers starting at 0) can ever reach it.
pub const OS_ERRNO_TAG_BASE: i64 = 1i64 << 32;

#[derive(Debug, Clone)]
pub enum Inst {
    /// `v = <n>`
    IConst { dst: Value, val: i64 },
    /// `v = <x>` -- a float constant, emitted with enough digits to round
    /// trip exactly.
    FConst { dst: Value, val: f64 },
    /// `v = <true|false>`
    BConst { dst: Value, val: bool },
    /// `v = <string literal>`; immortal, see docs/ir-v0.md §5.4
    SConst { dst: Value, idx: u32 },
    /// `v = <static object>` -- a module constant's collection, built by the
    /// compiler and emitted as static data. Immortal and borrowed, exactly
    /// like a string literal; `idx` indexes `Module::statics`.
    KConst { dst: Value, idx: u32 },
    /// `v = <op> a, b`  -- traps rather than wrapping, except for the
    /// explicitly wrapping ops
    Arith {
        dst: Value,
        op: ArithOp,
        lhs: Value,
        rhs: Value,
    },
    /// `v = icmp <cond> a, b`
    ICmp {
        dst: Value,
        cmp: Cmp,
        lhs: Value,
        rhs: Value,
    },
    /// `v = not a`
    Not { dst: Value, src: Value },
    /// `v? = call f(args)`
    Call {
        dst: Option<Value>,
        func: String,
        args: Vec<Value>,
    },
    /// `v? = call_iface obj.<slot>(args)` -- dynamic dispatch through the
    /// receiver's type header. The first argument is the receiver.
    CallIface {
        dst: Option<Value>,
        slot: u32,
        name: String,
        args: Vec<Value>,
        ret: Option<IrTy>,
    },
    /// `v = alloc <type>`; refcount 1, fields uninitialised. The lowering
    /// always follows this with a store to every field.
    Alloc { dst: Value, tid: u32 },
    /// `v = enum <type>.<tag>(args)` -- allocate, write the tag, write the
    /// payload. One instruction rather than three because a half-built enum
    /// has a tag that does not describe its slots, and nothing should be able
    /// to observe that state.
    EnumPack {
        dst: Value,
        tid: u32,
        tag: u32,
        args: Vec<Value>,
    },
    /// `ok, v = parse f(src)` -- a runtime call that reports success
    /// separately from the value it produces, writing the value through an
    /// out-parameter. Parsing needs it: "did it work" and "what is it" are
    /// two answers, and a sentinel would have to be a value the input could
    /// not produce, which for a float does not exist.
    ParseInto {
        ok: Value,
        dst: Value,
        func: String,
        src: Value,
    },
    /// `v = tag obj` -- which variant this is, as its declaration index.
    EnumTag { dst: Value, obj: Value, tid: u32 },
    /// `v = payload obj.<tag>.<idx>` -- one payload slot. For a boxed enum
    /// the slot is a machine word and the destination's type says how to read
    /// it; for a value enum (`IrTy::Val`) the payload is a typed member of
    /// the variant's union arm, which is why `tag` is carried here.
    ///
    /// The lowering only emits this where the tag is already known -- every
    /// site is a `match` arm, a `?`, or `Option.or`, and each of those has
    /// just tested the tag.
    EnumPayload {
        dst: Value,
        obj: Value,
        tid: u32,
        tag: u32,
        idx: u32,
    },
    /// `v = enum_clone obj` -- a second object with the same tag and the
    /// same payload, each reference in it retained once more.
    ///
    /// An enum has no fields, so the field-by-field copy `clone` uses for a
    /// struct would copy nothing at all; and which payload slots hold
    /// references depends on the tag, so the retains have to switch on it.
    /// Only ever emitted for a BOXED enum -- cloning a value enum is the
    /// assignment that binds the result.
    EnumClone { dst: Value, src: Value, tid: u32 },
    /// `take obj.<idx>` -- clear one payload slot, without releasing what it
    /// held. The +1 the enum was holding now belongs to whoever read the slot
    /// (with `EnumPayload`) just before: ownership has moved OUT of the enum.
    ///
    /// Emitted only where the enum is known to be the sole owner, so that
    /// nothing else can observe the hole. A cleared slot reads as null, and
    /// the drop function generated for an enum skips a null slot.
    EnumTake {
        obj: Value,
        tid: u32,
        tag: u32,
        idx: u32,
    },
    /// `v = load obj.<field>`
    LoadField {
        dst: Value,
        obj: Value,
        tid: u32,
        idx: u32,
    },
    /// `store obj.<field>, v`
    ///
    /// Deliberately dumb: any retain or release the store implies is emitted
    /// by the lowering as separate rc_inc/rc_dec instructions, so no backend
    /// has to reason about ownership (docs/ir-v0.md §4).
    StoreField {
        obj: Value,
        tid: u32,
        idx: u32,
        val: Value,
    },
    /// `spawn f(args)` -- run `f` on its own thread. The emitter generates
    /// one argument struct and one trampoline per spawned function.
    Spawn { func: String, args: Vec<Value> },
    /// `rc_inc v`
    RcInc { val: Value },
    /// `rc_dec v`
    RcDec { val: Value },
}

/// One machine-word slot of a static collection, as the compiler computed
/// it. A float is already its bit pattern and a bool is 0 or 1 -- the same
/// word the runtime would have stored -- so the emitter writes numbers and
/// addresses and never has to know what they meant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum StaticSlot {
    Word(i64),
    /// A string literal, by its index in `Module::strings`.
    Str(u32),
    /// Another static collection, by its index in `Module::statics`.
    Obj(u32),
}

/// A module constant's collection, laid out exactly as the runtime lays out
/// the same collection built at run time (runtime/rt.h), with a count of
/// RC_IMMORTAL. The runtime cannot tell one from the other except by that
/// count, which is the point: every read path is the ordinary one.
#[derive(Debug, Clone)]
pub enum StaticObj {
    /// An `Arr`. `refs` picks the TypeInfo, as `rt_array_new` would.
    Array { refs: bool, slots: Vec<StaticSlot> },
    /// A `Lst`, whose slots live in a buffer of their own -- a static array
    /// here, never reallocated, because nothing may push to it.
    List { refs: bool, slots: Vec<StaticSlot> },
    /// A `Bytes`: one octet per element, not a slot.
    Bytes(Vec<u8>),
    /// A `Map`'s open-addressed table, already hashed: `table.len()` is the
    /// capacity, a power of two, and `None` is an empty slot. There are no
    /// tombstones, because nothing was ever removed.
    Map {
        key_is_str: bool,
        val_is_ref: bool,
        len: usize,
        table: Vec<Option<(StaticSlot, StaticSlot)>>,
    },
}

#[derive(Debug, Clone)]
pub enum Term {
    Jump {
        to: BlockId,
        args: Vec<Value>,
    },
    Brif {
        cond: Value,
        then: BlockId,
        then_args: Vec<Value>,
        els: BlockId,
        els_args: Vec<Value>,
    },
    Ret {
        val: Option<Value>,
    },
}

#[derive(Debug, Clone)]
pub struct Block {
    pub id: BlockId,
    pub params: Vec<Value>,
    pub insts: Vec<Inst>,
    pub term: Term,
}

#[derive(Debug, Clone)]
pub struct Func {
    pub name: String,
    pub params: Vec<Value>,
    pub ret: Option<IrTy>,
    pub blocks: Vec<Block>,
    /// Type of every value the function defines, indexed by `Value.0`.
    pub types: Vec<IrTy>,
    pub entry: BlockId,
}

impl Func {
    pub fn ty_of(&self, v: Value) -> IrTy {
        self.types[v.0 as usize]
    }

    pub fn block(&self, id: BlockId) -> &Block {
        self.blocks
            .iter()
            .find(|b| b.id == id)
            .expect("dangling block id")
    }
}

#[derive(Debug, Clone)]
pub struct Module {
    pub funcs: Vec<Func>,
    /// Interned string literals; `SConst.idx` indexes this.
    pub strings: Vec<String>,
    /// The collections module constants hold, already built; `KConst.idx`
    /// indexes this. An entry only ever refers to entries before it, so
    /// emitting them in order defines everything before it is named.
    pub statics: Vec<StaticObj>,
    /// User-defined types; `Alloc.tid` and the field instructions index this.
    pub types: Vec<TypeDef>,
    /// Interface method names, one per dispatch slot. Assigned once for the
    /// whole program, so a vtable index is a constant at every call site.
    /// One dispatch slot per distinct interface method NAME AND SHAPE.
    ///
    /// Keyed by the shape as well as the name because the call site casts
    /// the stored pointer to the signature it computed: two interfaces
    /// declaring `m` with different parameters must not share a slot, or one
    /// call would invoke the other's function through the wrong type. The
    /// shape is IR-level, so `int area()` and `Price area()` still share --
    /// the cast is identical and a distinct type is erased.
    pub iface_slots: Vec<Slot>,
    /// The exact set of runtime symbol names (`rt_open`, `rt_read`, ...) that
    /// a `prim` call site's name-transform rule produces (docs/stdlib-seam.md
    /// §2: strip the module qualifier, strip the reserved `__`, prefix
    /// `rt_`). `Inst::Call.func` cannot itself distinguish a genuine foreign
    /// call crossing the C seam from an ordinary internal runtime helper
    /// call the lowering emits directly (`rt_concat`, `rt_chan_send`, ...) --
    /// both are plain `rt_`-prefixed strings by the time they reach this IR.
    /// This set is precomputed once, program-wide, by the lowerer (which
    /// alone knows which signatures came from an actual `prim` declaration),
    /// so `src/emit_c.rs` can tell the two apart at the one place it matters:
    /// deciding which call sites need `rt_enter_blocking`/`rt_exit_blocking`
    /// around them (docs/concurrency-decision.md, "Blocking FFI", Phase 3).
    /// A plain `HashSet`, not iterated for emission order -- only ever
    /// queried with `.contains`, so it cannot be the source of the
    /// HashMap-iteration nondeterminism this project has a gate against.
    pub prim_targets: std::collections::HashSet<String>,
}

impl Module {
    /// Check that no `IrTy::Val` has reached a position that needs an object.
    ///
    /// A value enum is the one shape in the IR that is neither a machine word
    /// nor an `Obj *` (docs/value-enums.md). Everywhere that *is* one of
    /// those -- a refcount operation, a runtime slot, an interface receiver --
    /// the lowering has to have demoted the type back to a boxed enum first.
    /// It does, by re-lowering (`lower::lower_program`); this is the check
    /// that says so, because a missed case here would be a silent miscompile
    /// -- a struct handed to something that will read it as a pointer -- and
    /// the C compiler would not catch all of them.
    ///
    /// It is a defect in the compiler if this ever fires, so it reports the
    /// instruction and panics rather than raising a diagnostic.
    pub fn verify(&self) {
        let bad = |f: &Func, v: Value| matches!(f.ty_of(v), IrTy::Val(_));
        let ret_of: std::collections::HashMap<&str, Option<IrTy>> = self
            .funcs
            .iter()
            .map(|g| (g.name.as_str(), g.ret))
            .collect();
        for f in &self.funcs {
            let ice = |what: &str, i: &Inst| -> ! {
                panic!(
                    "internal error: {what}, in `{}`: {}\n\
                     (see docs/value-enums.md; lower::lower_program decides \
                     which enums are values)",
                    f.name.replace('#', "."),
                    show_inst(i)
                )
            };
            for b in &f.blocks {
                for i in &b.insts {
                    match i {
                        // The refcount pass only ever touches `Ref`.
                        Inst::RcInc { val } | Inst::RcDec { val } if bad(f, *val) => {
                            ice("a value enum reached a refcount operation", i)
                        }
                        // The runtime's generic slot is one machine word, and
                        // every collection and channel argument rides in one.
                        Inst::Call { func, args, dst } if func.starts_with("rt_") => {
                            if args.iter().any(|a| bad(f, *a)) {
                                ice("a value enum reached a runtime call's argument", i);
                            }
                            if dst.is_some_and(|d| bad(f, d)) {
                                ice("a value enum reached a runtime call's result", i);
                            }
                        }
                        // Dispatch reads a vtable out of an object header.
                        Inst::CallIface { args, .. }
                            if args.first().is_some_and(|r| bad(f, *r)) =>
                        {
                            ice("a value enum reached an interface receiver", i)
                        }
                        // A field of `Ref` shape holds a pointer. (An enum's
                        // payload is NOT in this list: a boxed enum has the
                        // same tag-and-union layout as a value one, so it can
                        // carry a value enum inline.)
                        Inst::StoreField { tid, idx, val, .. }
                            if self.types[*tid as usize].fields[*idx as usize].1 == IrTy::Ref
                                && bad(f, *val) =>
                        {
                            ice("a value enum reached a reference-typed field", i)
                        }
                        // A call to one of the program's own functions has
                        // to produce a value of that function's return shape.
                        // Getting this wrong was impossible while every user
                        // type was a `Ref` and every call site could assume
                        // so; now it is a real question, and a wrong answer
                        // is a C type error a long way from its cause.
                        Inst::Call { dst, func, .. } => {
                            if let Some(want) = ret_of.get(func.as_str()) {
                                if dst.map(|d| f.ty_of(d)) != *want {
                                    ice("a call's result has the wrong shape", i);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}

// ---- textual form, for --emit-ir and for debugging -----------------------

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for t in &self.types {
            let fs: Vec<String> = t
                .fields
                .iter()
                .map(|(n, ty)| format!("{n}: {ty:?}"))
                .collect();
            // Module-qualified names are interned with `#`, which cannot
            // appear in source. The dump shows the spelling a reader would
            // write instead.
            // An enum's shape is not in its fields -- it has none -- and
            // which representation it got is the one thing a reader of this
            // dump cannot work out for themselves, so say it.
            let variants: Vec<String> = t
                .variants
                .iter()
                .map(|v| {
                    if v.payload.is_empty() {
                        v.name.clone()
                    } else {
                        let ps: Vec<String> = v.payload.iter().map(|p| format!("{p:?}")).collect();
                        format!("{}({})", v.name, ps.join(", "))
                    }
                })
                .collect();
            let body = if t.is_enum {
                variants.join("; ")
            } else {
                fs.join(", ")
            };
            let kind = if t.is_value { "value enum" } else { "type" };
            writeln!(f, "{kind} {} {{ {body} }}", t.name.replace('#', "."))?;
        }
        if !self.types.is_empty() {
            writeln!(f)?;
        }
        for (i, s) in self.strings.iter().enumerate() {
            writeln!(f, "str{i} = {s:?}")?;
        }
        if !self.strings.is_empty() {
            writeln!(f)?;
        }
        for (i, k) in self.statics.iter().enumerate() {
            writeln!(f, "k{i} = {k:?}")?;
        }
        if !self.statics.is_empty() {
            writeln!(f)?;
        }
        for func in &self.funcs {
            write!(f, "{func}")?;
        }
        Ok(())
    }
}

impl fmt::Display for Func {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let params: Vec<String> = self
            .params
            .iter()
            .map(|p| format!("{p}: {:?}", self.ty_of(*p)))
            .collect();
        let ret = match self.ret {
            Some(t) => format!(" -> {t:?}"),
            None => String::new(),
        };
        writeln!(
            f,
            "func {}({}){} {{",
            self.name.replace('#', "."),
            params.join(", "),
            ret
        )?;
        for b in &self.blocks {
            let bp: Vec<String> = b
                .params
                .iter()
                .map(|p| format!("{p}: {:?}", self.ty_of(*p)))
                .collect();
            if bp.is_empty() {
                writeln!(f, "{}:", b.id)?;
            } else {
                writeln!(f, "{}({}):", b.id, bp.join(", "))?;
            }
            for i in &b.insts {
                writeln!(f, "    {}", show_inst(i))?;
            }
            writeln!(f, "    {}", show_term(&b.term))?;
        }
        writeln!(f, "}}")
    }
}

fn args(vs: &[Value]) -> String {
    vs.iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

fn show_inst(i: &Inst) -> String {
    match i {
        Inst::IConst { dst, val } => format!("{dst} = iconst {val}"),
        Inst::FConst { dst, val } => format!("{dst} = fconst {val:?}"),
        Inst::BConst { dst, val } => format!("{dst} = bconst {val}"),
        Inst::SConst { dst, idx } => format!("{dst} = sconst str{idx}"),
        Inst::KConst { dst, idx } => format!("{dst} = kconst k{idx}"),
        Inst::Arith { dst, op, lhs, rhs } => {
            let name = match op {
                ArithOp::Add => "iadd",
                ArithOp::Sub => "isub",
                ArithOp::Mul => "imul",
                ArithOp::Div => "idiv",
                ArithOp::Rem => "irem",
                ArithOp::And => "iand",
                ArithOp::Or => "ior",
                ArithOp::Xor => "ixor",
                ArithOp::Shl => "ishl",
                ArithOp::Shr => "ishr",
                ArithOp::WrapAdd => "wadd",
                ArithOp::WrapSub => "wsub",
                ArithOp::WrapMul => "wmul",
            };
            format!("{dst} = {name} {lhs}, {rhs}")
        }
        Inst::ICmp { dst, cmp, lhs, rhs } => {
            format!("{dst} = icmp {} {lhs}, {rhs}", cmp.c_op())
        }
        Inst::EnumPack {
            dst,
            tid,
            tag,
            args,
        } => {
            let a: Vec<String> = args.iter().map(|v| v.to_string()).collect();
            format!("{dst} = enum T{tid}.{tag}({})", a.join(", "))
        }
        Inst::ParseInto { ok, dst, func, src } => {
            format!("{ok}, {dst} = parse {func}({src})")
        }
        Inst::EnumTag { dst, obj, tid } => format!("{dst} = tag T{tid} {obj}"),
        Inst::EnumPayload {
            dst,
            obj,
            tid,
            tag,
            idx,
        } => format!("{dst} = payload T{tid} {obj}.{tag}.{idx}"),
        Inst::EnumTake { obj, tid, tag, idx } => format!("take T{tid} {obj}.{tag}.{idx}"),
        Inst::EnumClone { dst, src, tid } => format!("{dst} = enum_clone T{tid} {src}"),
        Inst::Not { dst, src } => format!("{dst} = not {src}"),
        Inst::Call { dst, func, args: a } => match dst {
            Some(d) => format!("{d} = call {func}({})", args(a)),
            None => format!("call {func}({})", args(a)),
        },
        Inst::Alloc { dst, tid } => format!("{dst} = alloc type{tid}"),
        Inst::LoadField { dst, obj, tid, idx } => {
            format!("{dst} = load {obj}.type{tid}[{idx}]")
        }
        Inst::StoreField { obj, tid, idx, val } => {
            format!("store {obj}.type{tid}[{idx}], {val}")
        }
        Inst::CallIface {
            dst,
            slot,
            name,
            args: a,
            ..
        } => match dst {
            Some(d) => format!("{d} = call_iface [{slot}]{name}({})", args(a)),
            None => format!("call_iface [{slot}]{name}({})", args(a)),
        },
        Inst::Spawn { func, args: a } => format!("spawn {func}({})", args(a)),
        Inst::RcInc { val } => format!("rc_inc {val}"),
        Inst::RcDec { val } => format!("rc_dec {val}"),
    }
}

fn show_term(t: &Term) -> String {
    match t {
        Term::Jump { to, args: a } if a.is_empty() => format!("jump {to}"),
        Term::Jump { to, args: a } => format!("jump {to}({})", args(a)),
        Term::Brif {
            cond,
            then,
            then_args,
            els,
            els_args,
        } => {
            let ta = if then_args.is_empty() {
                String::new()
            } else {
                format!("({})", args(then_args))
            };
            let ea = if els_args.is_empty() {
                String::new()
            } else {
                format!("({})", args(els_args))
            };
            format!("brif {cond}, {then}{ta}, {els}{ea}")
        }
        Term::Ret { val: Some(v) } => format!("ret {v}"),
        Term::Ret { val: None } => "ret".to_string(),
    }
}
