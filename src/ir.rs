//! The IR. See docs/ir-v0.md for the specification this implements.
//!
//! SSA with block parameters rather than phi nodes, three types, sixteen
//! instructions. Backends consume the POST-refcount form only, so no backend
//! ever reasons about ownership.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrTy {
    I64,
    I1,
    Ref,
}

impl IrTy {
    pub fn c_name(self) -> &'static str {
        match self {
            IrTy::I64 => "int64_t",
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArithOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
}

impl ArithOp {
    /// Runtime helper implementing this op. Add/sub/mul are checked and trap
    /// on overflow (docs/ir-v0.md §3); div and rem trap on zero and on the
    /// one signed-overflow case, INT64_MIN / -1.
    pub fn rt_fn(self) -> &'static str {
        match self {
            ArithOp::Add => "rt_iadd",
            ArithOp::Sub => "rt_isub",
            ArithOp::Mul => "rt_imul",
            ArithOp::Div => "rt_idiv",
            ArithOp::Rem => "rt_irem",
        }
    }
}

#[derive(Debug, Clone)]
pub enum Inst {
    /// `v = <n>`
    IConst { dst: Value, val: i64 },
    /// `v = <true|false>`
    BConst { dst: Value, val: bool },
    /// `v = <string literal>`; immortal, see docs/ir-v0.md §5.4
    SConst { dst: Value, idx: u32 },
    /// `v = <op> a, b`  -- traps rather than wrapping
    Arith { dst: Value, op: ArithOp, lhs: Value, rhs: Value },
    /// `v = icmp <cond> a, b`
    ICmp { dst: Value, cmp: Cmp, lhs: Value, rhs: Value },
    /// `v = not a`
    Not { dst: Value, src: Value },
    /// `v? = call f(args)`
    Call { dst: Option<Value>, func: String, args: Vec<Value> },
    /// `rc_inc v`
    RcInc { val: Value },
    /// `rc_dec v`
    RcDec { val: Value },
}

#[derive(Debug, Clone)]
pub enum Term {
    Jump { to: BlockId, args: Vec<Value> },
    Brif { cond: Value, then: BlockId, then_args: Vec<Value>, els: BlockId, els_args: Vec<Value> },
    Ret { val: Option<Value> },
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
        self.blocks.iter().find(|b| b.id == id).expect("dangling block id")
    }
}

#[derive(Debug, Clone)]
pub struct Module {
    pub funcs: Vec<Func>,
    /// Interned string literals; `SConst.idx` indexes this.
    pub strings: Vec<String>,
}

// ---- textual form, for --emit-ir and for debugging -----------------------

impl fmt::Display for Module {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (i, s) in self.strings.iter().enumerate() {
            writeln!(f, "str{i} = {s:?}")?;
        }
        if !self.strings.is_empty() {
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
        writeln!(f, "func {}({}){} {{", self.name, params.join(", "), ret)?;
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
    vs.iter().map(|v| v.to_string()).collect::<Vec<_>>().join(", ")
}

fn show_inst(i: &Inst) -> String {
    match i {
        Inst::IConst { dst, val } => format!("{dst} = iconst {val}"),
        Inst::BConst { dst, val } => format!("{dst} = bconst {val}"),
        Inst::SConst { dst, idx } => format!("{dst} = sconst str{idx}"),
        Inst::Arith { dst, op, lhs, rhs } => {
            let name = match op {
                ArithOp::Add => "iadd",
                ArithOp::Sub => "isub",
                ArithOp::Mul => "imul",
                ArithOp::Div => "idiv",
                ArithOp::Rem => "irem",
            };
            format!("{dst} = {name} {lhs}, {rhs}")
        }
        Inst::ICmp { dst, cmp, lhs, rhs } => {
            format!("{dst} = icmp {} {lhs}, {rhs}", cmp.c_op())
        }
        Inst::Not { dst, src } => format!("{dst} = not {src}"),
        Inst::Call { dst, func, args: a } => match dst {
            Some(d) => format!("{d} = call {func}({})", args(a)),
            None => format!("call {func}({})", args(a)),
        },
        Inst::RcInc { val } => format!("rc_inc {val}"),
        Inst::RcDec { val } => format!("rc_dec {val}"),
    }
}

fn show_term(t: &Term) -> String {
    match t {
        Term::Jump { to, args: a } if a.is_empty() => format!("jump {to}"),
        Term::Jump { to, args: a } => format!("jump {to}({})", args(a)),
        Term::Brif { cond, then, then_args, els, els_args } => {
            let ta = if then_args.is_empty() { String::new() } else { format!("({})", args(then_args)) };
            let ea = if els_args.is_empty() { String::new() } else { format!("({})", args(els_args)) };
            format!("brif {cond}, {then}{ta}, {els}{ea}")
        }
        Term::Ret { val: Some(v) } => format!("ret {v}"),
        Term::Ret { val: None } => "ret".to_string(),
    }
}
