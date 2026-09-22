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
}

impl IrTy {
    pub fn c_name(self) -> &'static str {
        match self {
            IrTy::I64 => "int64_t",
            IrTy::F64 => "double",
            IrTy::I1 => "bool",
            IrTy::Ref => "Obj *",
        }
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
    /// Non-empty only for an enum. An enum is laid out as a tag followed by
    /// `payload_slots()` generic slots, and which slots hold references
    /// depends on the tag -- so its drop and walk functions switch on it.
    pub variants: Vec<Variant>,
    pub is_enum: bool,
    /// The IR name of the type's destructor (`File.drop`), if it declares
    /// one. The drop function calls it before releasing any field, so the
    /// destructor still sees a whole object (docs/destructors-decision.md).
    pub destructor: Option<String>,
    /// Beside `destructor`: the type's name as the program spells it
    /// (`io.File`, `Holder<Res>`). It goes into the TypeInfo so that the
    /// runtime can name the type when a `const` meets a value that owns a
    /// resource -- which only happens through an interface, where the
    /// compiler could not see the type (docs/destructors-decision.md).
    pub resource: Option<String>,
}

impl TypeDef {
    /// Whether this type holds references, and therefore needs a drop
    /// function. Types that hold none pay no call when freed.
    pub fn needs_drop(&self) -> bool {
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

    /// How many payload slots an enum's object needs: the widest variant.
    /// Every variant shares the slots, so a slot's C type cannot depend on
    /// the variant -- they are all machine words, and a reference rides as
    /// its pointer, the same way a collection's elements do.
    pub fn payload_slots(&self) -> usize {
        self.variants
            .iter()
            .map(|v| v.payload.len())
            .max()
            .unwrap_or(0)
    }
}

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
    /// `v = payload obj.<idx>` -- one payload slot. The slot is a machine
    /// word; the destination's type says how to read it, and the lowering
    /// only emits this where the tag is already known.
    EnumPayload {
        dst: Value,
        obj: Value,
        tid: u32,
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
            writeln!(
                f,
                "type {} {{ {} }}",
                t.name.replace('#', "."),
                fs.join(", ")
            )?;
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
        Inst::EnumPayload { dst, obj, tid, idx } => format!("{dst} = payload T{tid} {obj}.{idx}"),
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
