//! Lowering of function bodies to Cranelift IR.
//!
//! Values: `F64` / `Bool` are plain Cranelift values. `Any` is a QuickJS
//! `JSValue` as a `(payload, tag)` pair of `I64`s, and is *owned*: every
//! `Any` expression evaluates to a new reference that the consumer must
//! either store, pass on to a callee that takes ownership, or free.
//!
//! Exceptions: helpers report them by returning `JS_EXCEPTION` (or a negative
//! int); native callees by setting `tsaot_unwinding`. Either way control goes
//! to an exception edge, which frees the owned temporaries that are live at
//! that point (`Lower::temps`) and jumps to the innermost handler: a `catch`
//! or `finally` block, or the function's exception exit, which frees all
//! `Any` locals and propagates.

use std::collections::HashMap;

use cranelift_codegen::ir::condcodes::{FloatCC, IntCC};
use cranelift_codegen::ir::types::{F32, F64, I8, I32, I64, I64X2};
use cranelift_codegen::ir::{
    Block, BlockArg, FuncRef, GlobalValue, InstBuilder, MemFlagsData, SigRef, StackSlotData, StackSlotKind, Type, Value,
};
use cranelift_frontend::{FunctionBuilder, Variable};
use tsaot_front::ir::{
    self, AnyBinOp, AnyCmpOp, AnyUnOp, ArrKind, BinOp, CmpOp, Expr, ExprKind, Key, LogicalOp, MAYBE_F64_UNDEFINED,
    MathFn, Stmt, Ty, UnOp,
};

use crate::jsvalue::*;
use crate::layout;

/// Typed array kinds (`JSTypedArrayEnum`), each with a pristine shape.
const TYPED_ARRAYS: i64 = 12;

/// `JSBinaryOperatorEnum` / `JSUnaryOperatorEnum` of the engine patch.
mod op {
    pub const ADD: i64 = 0;
    pub const SUB: i64 = 1;
    pub const MUL: i64 = 2;
    pub const DIV: i64 = 3;
    pub const MOD: i64 = 4;
    pub const POW: i64 = 5;
    pub const SHL: i64 = 6;
    pub const SAR: i64 = 7;
    pub const SHR: i64 = 8;
    pub const AND: i64 = 9;
    pub const OR: i64 = 10;
    pub const XOR: i64 = 11;
    pub const LT: i64 = 12;
    pub const LTE: i64 = 13;
    pub const GT: i64 = 14;
    pub const GTE: i64 = 15;
    pub const EQ: i64 = 16;
    pub const NEQ: i64 = 17;
    pub const STRICT_EQ: i64 = 18;
    pub const STRICT_NEQ: i64 = 19;
    pub const IN: i64 = 20;
    pub const INSTANCEOF: i64 = 21;
    pub const NEG: i64 = 0;
    pub const PLUS: i64 = 1;
    pub const BITNOT: i64 = 2;
    pub const INC: i64 = 3;
    pub const DEC: i64 = 4;
    pub const TONUMERIC: i64 = 5;
    pub const TYPEOF: i64 = 6;
}

/// `JS_PROP_THROW`.
const PROP_THROW: i64 = 1 << 14;

/// A lowered value.
#[derive(Clone, Copy, Debug)]
pub(crate) enum V {
    Num(Value),
    /// An `I32` value as a sign-extended `i64`: only where values cross a
    /// call, a return or a block boundary (see `Lower::abi`).
    Int(Value),
    Bool(Value),
    Any(Value, Value),
    Void,
}

impl V {
    fn num(self) -> Value {
        match self {
            V::Num(v) => v,
            other => panic!("expected a number, got {other:?}"),
        }
    }
    fn boolean(self) -> Value {
        match self {
            V::Bool(v) => v,
            other => panic!("expected a boolean, got {other:?}"),
        }
    }
    fn any(self) -> (Value, Value) {
        match self {
            V::Any(p, t) => (p, t),
            other => panic!("expected a JS value, got {other:?}"),
        }
    }
    /// As Cranelift values (block arguments, returns).
    pub(crate) fn values(self) -> Vec<Value> {
        match self {
            V::Num(v) | V::Bool(v) | V::Int(v) => vec![v],
            V::Any(p, t) => vec![p, t],
            V::Void => vec![],
        }
    }
}

/// Cranelift types of a value of type `t`.
pub(crate) fn clif_types(t: Ty) -> Vec<Type> {
    match t {
        Ty::I32 => vec![I64],
        Ty::F64 | Ty::MaybeF64 => vec![F64],
        Ty::Bool => vec![I8],
        Ty::Any | Ty::Arr(_) => vec![I64, I64],
        Ty::Void => vec![],
    }
}

/// Whether values of type `t` are `JSValue`s.
pub(crate) fn is_js(t: Ty) -> bool {
    matches!(t, Ty::Any | Ty::Arr(_))
}

/// Rebuild a `V` of type `t` from Cranelift values.
pub(crate) fn from_values(t: Ty, vs: &[Value]) -> V {
    match t {
        Ty::I32 => V::Int(vs[0]),
        Ty::F64 | Ty::MaybeF64 => V::Num(vs[0]),
        Ty::Bool => V::Bool(vs[0]),
        Ty::Any | Ty::Arr(_) => V::Any(vs[0], vs[1]),
        Ty::Void => V::Void,
    }
}

#[derive(Clone, Copy)]
enum LVar {
    One(Variable),
    /// An `F64` local that only ever holds integers (see `int_locals`), as
    /// an `i64`.
    Int(Variable),
    Two(Variable, Variable),
    /// Captured by closures: the box object (owned unless the box comes
    /// from the closure's own captures) and a pointer to its value slot.
    Boxed {
        obj: (Variable, Variable),
        slot: Variable,
        owned: bool,
    },
}

/// Where lowered code goes when it leaves the body.
#[derive(Clone, Copy)]
pub(crate) enum Exit {
    /// Return from the Cranelift function (on exception: set
    /// `tsaot_unwinding` and return placeholder values).
    Return,
    /// Jump to a block: for `ret`, with the return value as its parameters;
    /// for `unwind`, without parameters.
    Jump(Block),
}

#[derive(Clone, Copy)]
pub(crate) struct Exits {
    pub ret: Exit,
    pub unwind: Exit,
}

/// Function-level references declared in the Cranelift function.
pub(crate) struct Env {
    pub helpers: HashMap<&'static str, FuncRef>,
    pub funcs: Vec<FuncRef>,
    pub stack_limit: GlobalValue,
    pub unwinding: GlobalValue,
    pub cells: GlobalValue,
    pub strings: GlobalValue,
    pub atoms: GlobalValue,
    /// `tsaot_shapes`: the shapes of a pristine typed array (by kind), then
    /// of a pristine array.
    pub shapes: GlobalValue,
    /// `tsaot_ics`: inline caches, and the next free one.
    pub ics: GlobalValue,
    /// `tsaot_privates` / `tsaot_homes`: private name values and home objects
    /// of classes (see `ir::Module`).
    pub privates: GlobalValue,
    pub homes: GlobalValue,
    pub next_ic: std::rc::Rc<std::cell::Cell<usize>>,
    /// `FuncId` of closure 0.
    pub closure_base: usize,
    /// Parameter types, by `FuncId`.
    pub params: std::rc::Rc<Vec<Vec<Ty>>>,
    /// `value_captures`, by `ClosureId`.
    pub value_captures: std::rc::Rc<Vec<Vec<bool>>>,
    /// The signature of `JSCFunctionFast` entries.
    pub fast_sig: SigRef,
}

/// An exception edge: its block, the temporaries to free there, the handler.
type ExceptionEdge = (Block, Vec<(Value, Value)>, Block);

enum Scope {
    Jump(ir::LoopId),
    /// A `finally` block to run when a jump or return leaves its `try`, with
    /// the handler and scope depths in effect around the `try`.
    Finally {
        body: Vec<Stmt>,
        handlers: usize,
        scopes: usize,
    },
    /// An iterator to close when a jump or return leaves its guard (but not
    /// a `continue` of loop `loop_id`).
    CloseIter {
        iter: ir::LocalId,
        next: ir::LocalId,
        loop_id: Option<ir::LoopId>,
        handlers: usize,
        scopes: usize,
    },
}

pub(crate) struct Lower<'a> {
    pub b: FunctionBuilder<'a>,
    env: Env,
    ctx: Value,
    ptr: Type,
    ret: Ty,
    locals: Vec<Ty>,
    names: Vec<Option<ir::AtomId>>,
    vars: Vec<LVar>,
    /// loop id -> (continue target, break target)
    loops: HashMap<ir::LoopId, (Option<Block>, Block)>,
    /// Owned values to free if an exception occurs now.
    temps: Vec<(Value, Value)>,
    /// Innermost-last exception handler blocks.
    handlers: Vec<Block>,
    scopes: Vec<Scope>,
    /// Exception edges: (block, temporaries to free, handler).
    pending: Vec<ExceptionEdge>,
    exc_exit: Block,
    ret_block: Block,
    exits: Exits,
    /// Methods: the receiver (borrowed).
    pub(crate) this: Option<(Value, Value)>,
    async_cx: Option<AsyncCx>,
    /// An async generator: `await` suspends to the engine (kind 3).
    async_gen: bool,
    /// See `ir::Func::ret_slot`.
    ret_slot: Option<ir::LocalId>,
    /// `f64` values that are exact integers by construction (int locals,
    /// integral constants), and their `i64` value.
    int_of: HashMap<Value, Value>,
    /// Of those, the ones in the int32 range.
    i32s: std::collections::HashSet<Value>,
    /// Products of two int32s: their exact `i64` value (the `f64` may be
    /// rounded, which cannot change how it compares to an int32).
    wide_of: HashMap<Value, Value>,
    /// `%` results: their integer value (the `f64` may be -0 for 0), valid
    /// when the flag (the divisor was not 0) is set; else the `f64` is NaN.
    rem_of: HashMap<Value, (Value, Value)>,
    /// Int locals whose current update cannot pass 2^53: loop counters
    /// below a constant bound (see `bounded_counter`).
    bounded: Vec<ir::LocalId>,
    /// Int locals that need no range checks (see `small_ints`).
    small: Vec<bool>,
    /// Int locals always in the int32 range (see `i32_locals`).
    i32_range: Vec<bool>,
}

impl<'a> Lower<'a> {
    /// Set up lowering of a body with locals typed `locals`, the first ones
    /// being the parameters (`params`, already in `b`'s current block).
    /// Closures also get `closure_data`, their function object's data slots.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        mut b: FunctionBuilder<'a>,
        env: Env,
        ctx: Value,
        ptr: Type,
        f: &ir::Func,
        params: &[V],
        closure_data: Option<Value>,
        exits: Exits,
    ) -> Self {
        let ints = int_locals(f);
        let small = small_ints(f, &ints);
        let i32_range = i32_locals(f, &ints);
        let vars: Vec<LVar> = f
            .locals
            .iter()
            .enumerate()
            .map(|(i, &t)| match t {
                _ if ints[i] || t == Ty::I32 => LVar::Int(b.declare_var(I64)),
                _ if f.boxed[i] => LVar::Boxed {
                    obj: (b.declare_var(I64), b.declare_var(I64)),
                    slot: b.declare_var(I64),
                    // Step functions own every box (captures are dup'ed
                    // into their frame).
                    owned: f.is_async || f.is_generator || !f.captures.contains(&i),
                },
                Ty::Any | Ty::Arr(_) => LVar::Two(b.declare_var(I64), b.declare_var(I64)),
                t => LVar::One(b.declare_var(clif_types(t)[0])),
            })
            .collect();
        let exc_exit = b.create_block();
        b.set_cold_block(exc_exit);
        let ret_block = b.create_block();
        for t in clif_types(f.ret) {
            b.append_block_param(ret_block, t);
        }
        let mut l = Lower {
            b,
            env,
            ctx,
            ptr,
            ret: f.ret,
            locals: f.locals.clone(),
            names: f.local_names.clone(),
            vars,
            loops: HashMap::new(),
            temps: Vec::new(),
            handlers: Vec::new(),
            scopes: Vec::new(),
            pending: Vec::new(),
            exc_exit,
            ret_block,
            exits,
            this: None,
            async_cx: None,
            async_gen: false,
            ret_slot: f.ret_slot,
            int_of: HashMap::new(),
            i32s: Default::default(),
            wide_of: HashMap::new(),
            rem_of: HashMap::new(),
            bounded: Vec::new(),
            small,
            i32_range,
        };
        for (i, &t) in f.locals.iter().enumerate() {
            if let LVar::Boxed { obj: (op, ot), slot, .. } = l.vars[i] {
                // Boxes are created when their scope is entered (or come
                // from the closure, below).
                let zero = l.b.ins().iconst(I64, 0);
                let undef = l.b.ins().iconst(I64, TAG_UNDEFINED);
                l.b.def_var(op, zero);
                l.b.def_var(ot, undef);
                l.b.def_var(slot, zero);
                continue;
            }
            let v = match params.get(i) {
                // A widened parameter is boxed on entry.
                Some(&p) => match (t, p) {
                    (Ty::Any, V::Num(d)) => {
                        let (p, t) = l.box_number(d);
                        V::Any(p, t)
                    }
                    (Ty::Any, V::Int(i)) => {
                        let d = l.int_f64(i);
                        let (p, t) = l.box_number(d);
                        V::Any(p, t)
                    }
                    (Ty::Any, V::Bool(x)) => {
                        let p = l.b.ins().uextend(I64, x);
                        let t = l.b.ins().iconst(I64, TAG_BOOL);
                        V::Any(p, t)
                    }
                    (_, p) => p,
                },
                None => l.zero(t),
            };
            l.def_local(i, v);
        }
        // A closure's data slots hold, per captured variable, its box object
        // and the address of the box's value (as a non-reference value).
        // A value capture holds the value itself, which its slot points at.
        if let Some(data) = closure_data {
            for (k, &cl) in f.captures.iter().enumerate() {
                let LVar::Boxed { obj: (op, ot), slot, .. } = l.vars[cl] else { unreachable!() };
                let at = (2 * k * SIZE) as i32;
                let p = l.b.ins().load(I64, Self::mem(), data, at);
                let t = l.b.ins().load(I64, Self::mem(), data, at + TAG_OFFSET);
                let s = if f.value_captures[k] {
                    l.b.ins().iadd_imm_s(data, at as i64)
                } else {
                    l.b.ins().load(I64, Self::mem(), data, at + SIZE as i32)
                };
                l.b.def_var(op, p);
                l.b.def_var(ot, t);
                l.b.def_var(slot, s);
            }
        }
        l
    }

    fn is_filled(&self) -> bool {
        let cur = self.b.current_block().unwrap();
        self.b.func.layout.last_inst(cur).is_some_and(|i| self.b.func.dfg.insts[i].opcode().is_terminator())
    }

    /// After a terminator, continue emitting into a fresh unreachable block.
    fn after_terminator(&mut self) {
        let dead = self.b.create_block();
        self.b.switch_to_block(dead);
    }

    fn zero(&mut self, t: Ty) -> V {
        match t {
            Ty::F64 | Ty::MaybeF64 => V::Num(self.b.ins().f64const(0.0)),
            Ty::I32 => V::Int(self.b.ins().iconst(I64, 0)),
            Ty::Bool => V::Bool(self.b.ins().iconst(I8, 0)),
            Ty::Any | Ty::Arr(_) => self.konst(0, TAG_UNDEFINED),
            Ty::Void => V::Void,
        }
    }

    fn konst(&mut self, payload: i64, tag: i64) -> V {
        let p = self.b.ins().iconst(I64, payload);
        let t = self.b.ins().iconst(I64, tag);
        V::Any(p, t)
    }

    fn helper(&mut self, name: &str, args: &[Value]) -> Vec<Value> {
        let f = *self.env.helpers.get(name).unwrap_or_else(|| panic!("helper {name} not declared"));
        let call = self.b.ins().call(f, args);
        self.b.inst_results(call).to_vec()
    }

    fn mem() -> MemFlagsData {
        MemFlagsData::trusted()
    }

    // ---- locals ----

    fn def_local(&mut self, l: ir::LocalId, v: V) {
        match (self.vars[l], v) {
            (LVar::One(var), V::Num(x) | V::Bool(x)) => self.b.def_var(var, x),
            // Only ever an integer (by `int_locals`).
            (LVar::Int(var), V::Num(x)) => {
                let i = self.int_of_num(x);
                self.b.def_var(var, i)
            }
            (LVar::Int(var), V::Int(i)) => self.b.def_var(var, i),
            (LVar::Two(p, t), V::Any(x, y)) => {
                self.b.def_var(p, x);
                self.b.def_var(t, y);
            }
            _ => panic!("local {l} type mismatch"),
        }
    }

    /// The local's value, borrowed. Boxed locals are checked for their TDZ.
    fn use_local(&mut self, l: ir::LocalId) -> V {
        match (self.vars[l], self.locals[l]) {
            (LVar::One(v), Ty::F64 | Ty::MaybeF64) => V::Num(self.b.use_var(v)),
            (LVar::One(v), Ty::Bool) => V::Bool(self.b.use_var(v)),
            (LVar::Int(v), t) => {
                let i = self.b.use_var(v);
                let d = self.int_f64(i);
                if t == Ty::I32 || self.i32_range[l] {
                    self.i32s.insert(d);
                }
                V::Num(d)
            }
            (LVar::Two(p, t), _) => V::Any(self.b.use_var(p), self.b.use_var(t)),
            (LVar::Boxed { .. }, _) => {
                let (p, t) = self.load_box(l);
                self.check_tdz(l, t);
                V::Any(p, t)
            }
            _ => unreachable!(),
        }
    }

    fn box_slot(&mut self, l: ir::LocalId) -> Value {
        let LVar::Boxed { slot, .. } = self.vars[l] else { unreachable!() };
        self.b.use_var(slot)
    }

    /// The value in a boxed local's box, borrowed, without a TDZ check.
    fn load_box(&mut self, l: ir::LocalId) -> (Value, Value) {
        let slot = self.box_slot(l);
        let p = self.b.ins().load(I64, Self::mem(), slot, 0);
        let t = self.b.ins().load(I64, Self::mem(), slot, TAG_OFFSET);
        (p, t)
    }

    fn store_box(&mut self, l: ir::LocalId, (p, t): (Value, Value)) {
        let slot = self.box_slot(l);
        self.b.ins().store(Self::mem(), p, slot, 0);
        self.b.ins().store(Self::mem(), t, slot, TAG_OFFSET);
    }

    /// ReferenceError if a boxed local is still uninitialized.
    fn check_tdz(&mut self, l: ir::LocalId, t: Value) {
        let uninit = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_UNINITIALIZED);
        let throw = self.b.create_block();
        let cont = self.b.create_block();
        self.b.set_cold_block(throw);
        self.b.ins().brif(uninit, throw, &[], cont, &[]);
        self.b.switch_to_block(throw);
        let atom = self.atom(self.names[l].expect("boxed locals are named"));
        let ctx = self.ctx;
        self.helper("tsaot_throw_uninitialized_atom", &[ctx, atom]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(cont);
    }

    /// Fresh, uninitialized boxes for boxed locals (their scope is entered).
    fn new_boxes(&mut self, ls: &[ir::LocalId], copy_values: bool) {
        for &l in ls {
            let LVar::Boxed { obj: (op, ot), slot, owned } = self.vars[l] else { unreachable!() };
            debug_assert!(owned);
            let old_obj = (self.b.use_var(op), self.b.use_var(ot));
            let old_val = if copy_values { Some(self.load_box(l)) } else { None };
            let out = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
            let out_addr = self.b.ins().stack_addr(self.ptr, out, 0);
            let ctx = self.ctx;
            let r = self.helper("tsaot_box_new", &[ctx, out_addr]);
            self.check_value(r[1]);
            let new_slot = self.b.ins().stack_load(self.ptr, I64, out, 0);
            self.b.def_var(op, r[0]);
            self.b.def_var(ot, r[1]);
            self.b.def_var(slot, new_slot);
            if let Some(v) = old_val {
                // A new binding for the next iteration, with the same value.
                self.dup(v);
                self.store_box(l, v);
            }
            self.free(old_obj);
        }
    }

    fn free_all_locals(&mut self) {
        for l in 0..self.locals.len() {
            match self.vars[l] {
                LVar::Boxed { obj: (op, ot), owned: true, .. } => {
                    let obj = (self.b.use_var(op), self.b.use_var(ot));
                    self.free(obj);
                }
                LVar::Boxed { .. } => {}
                _ if is_js(self.locals[l]) => {
                    let (p, t) = self.use_local(l).any();
                    self.free((p, t));
                }
                _ => {}
            }
        }
    }
    // ---- reference counting ----

    fn is_refcounted(&mut self, t: Value) -> Value {
        self.b.ins().icmp_imm_s(IntCC::SignedLessThan, t, 0)
    }

    /// A new reference to `v` (borrowed): the reference count, in the
    /// allocator header before the pointer, is incremented inline.
    fn dup(&mut self, (p, t): (Value, Value)) {
        let rc = self.is_refcounted(t);
        let yes = self.b.create_block();
        let join = self.b.create_block();
        self.b.ins().brif(rc, yes, &[], join, &[]);
        self.b.switch_to_block(yes);
        let n = self.b.ins().load(I32, Self::mem(), p, layout::REF_COUNT);
        let n = self.b.ins().iadd_imm_s(n, 1);
        self.b.ins().store(Self::mem(), n, p, layout::REF_COUNT);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
    }

    /// Release a reference: decremented inline unless it is the last one,
    /// which the engine frees.
    fn free(&mut self, (p, t): (Value, Value)) {
        let rc = self.is_refcounted(t);
        let yes = self.b.create_block();
        let dec = self.b.create_block();
        let last = self.b.create_block();
        let join = self.b.create_block();
        self.b.set_cold_block(last);
        self.b.ins().brif(rc, yes, &[], join, &[]);
        self.b.switch_to_block(yes);
        let n = self.b.ins().load(I32, Self::mem(), p, layout::REF_COUNT);
        let shared = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, n, 1);
        self.b.ins().brif(shared, dec, &[], last, &[]);
        self.b.switch_to_block(dec);
        let n = self.b.ins().iadd_imm_s(n, -1);
        self.b.ins().store(Self::mem(), n, p, layout::REF_COUNT);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(last);
        let ctx = self.ctx;
        self.helper("JS_FreeValue", &[ctx, p, t]);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
    }
    fn free_v(&mut self, v: V) {
        if let V::Any(p, t) = v {
            self.free((p, t));
        }
    }

    fn hold(&mut self, v: V) {
        if let V::Any(p, t) = v {
            self.temps.push((p, t));
        }
    }

    fn release(&mut self, v: V) {
        if let V::Any(p, t) = v {
            let i = self.temps.iter().rposition(|&x| x == (p, t)).expect("released value is held");
            self.temps.remove(i);
        }
    }

    // ---- exceptions ----

    /// A block to branch to when an exception is pending now.
    fn throw_edge(&mut self) -> Block {
        let blk = self.b.create_block();
        self.b.set_cold_block(blk);
        let target = self.handlers.last().copied().unwrap_or(self.exc_exit);
        self.pending.push((blk, self.temps.clone(), target));
        blk
    }

    fn branch_throw(&mut self, cond: Value) {
        let edge = self.throw_edge();
        let cont = self.b.create_block();
        self.b.ins().brif(cond, edge, &[], cont, &[]);
        self.b.switch_to_block(cont);
    }

    /// Check a `JSValue` returned by a helper for `JS_EXCEPTION`.
    fn check_value(&mut self, t: Value) {
        let exc = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_EXCEPTION);
        self.branch_throw(exc);
    }

    /// Check an int returned by a helper (negative: exception).
    fn check_int(&mut self, r: Value) {
        let exc = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, r, 0);
        self.branch_throw(exc);
    }

    fn check_flag(&mut self) {
        let addr = self.b.ins().symbol_value(self.ptr, self.env.unwinding);
        let flag = self.b.ins().load(I8, Self::mem(), addr, 0);
        self.branch_throw(flag);
    }

    fn clear_flag(&mut self) {
        let addr = self.b.ins().symbol_value(self.ptr, self.env.unwinding);
        let zero = self.b.ins().iconst(I8, 0);
        self.b.ins().store(Self::mem(), zero, addr, 0);
    }

    /// Entry check of a function called from native code.
    pub(crate) fn stack_check(&mut self) {
        let limit_addr = self.b.ins().symbol_value(self.ptr, self.env.stack_limit);
        let limit = self.b.ins().load(self.ptr, Self::mem(), limit_addr, 0);
        let sp = self.b.ins().get_stack_pointer(self.ptr);
        let overflow = self.b.ins().icmp(IntCC::UnsignedLessThan, sp, limit);
        let throw = self.b.create_block();
        let body = self.b.create_block();
        self.b.set_cold_block(throw);
        self.b.ins().brif(overflow, throw, &[], body, &[]);
        self.b.switch_to_block(throw);
        let ctx = self.ctx;
        self.helper("tsaot_stack_overflow", &[ctx]);
        let target = self.exc_exit;
        self.b.ins().jump(target, &[]);
        self.b.switch_to_block(body);
    }

    /// Lower a whole function body, including its exits.
    pub(crate) fn body(mut self, body: &[Stmt]) -> FunctionBuilder<'a> {
        self.stmts(body);
        if !self.is_filled() {
            // Falling off the end (or dead code after a terminator).
            let v = if self.ret == Ty::Any { self.konst(0, TAG_UNDEFINED) } else { self.zero(self.ret) };
            let args: Vec<BlockArg> = v.values().into_iter().map(BlockArg::Value).collect();
            let rb = self.ret_block;
            self.b.ins().jump(rb, &args);
        }

        let is_async = self.async_cx.is_some();

        // Normal exit: free the locals, then leave with the return value.
        let rb = self.ret_block;
        self.b.switch_to_block(rb);
        let rv: Vec<Value> = self.b.block_params(rb).to_vec();
        self.free_all_locals();
        if is_async {
            // The value the promise resolves to.
            let ret = self.ret;
            let (p, t) = match self.unabi(ret, &rv) {
                V::Num(d) => self.box_number(d),
                V::Bool(x) => (self.b.ins().uextend(I64, x), self.b.ins().iconst(I64, TAG_BOOL)),
                V::Any(p, t) => (p, t),
                V::Void => (self.b.ins().iconst(I64, 0), self.b.ins().iconst(I64, TAG_UNDEFINED)),
                V::Int(_) => unreachable!("converted by unabi"),
            };
            self.b.ins().return_(&[p, t]);
        } else {
            match self.exits.ret {
                Exit::Return => {
                    self.b.ins().return_(&rv);
                }
                Exit::Jump(blk) => {
                    let args: Vec<BlockArg> = rv.into_iter().map(BlockArg::Value).collect();
                    self.b.ins().jump(blk, &args);
                }
            }
        }

        // Exception exit.
        let ee = self.exc_exit;
        self.b.switch_to_block(ee);
        self.free_all_locals();
        if is_async {
            // The exception is pending; the runtime rejects the promise.
            let z = self.b.ins().iconst(I64, 0);
            let t = self.b.ins().iconst(I64, TAG_EXCEPTION);
            self.b.ins().return_(&[z, t]);
        } else {
            match self.exits.unwind {
                Exit::Return => {
                    let addr = self.b.ins().symbol_value(self.ptr, self.env.unwinding);
                    let one = self.b.ins().iconst(I8, 1);
                    self.b.ins().store(Self::mem(), one, addr, 0);
                    let z = self.zero(self.ret);
                    let vals = z.values();
                    self.b.ins().return_(&vals);
                }
                Exit::Jump(blk) => {
                    self.b.ins().jump(blk, &[]);
                }
            }
        }
        self.emit_dispatch();

        // Exception edges: free their live temporaries, then go to the handler.
        let pending = std::mem::take(&mut self.pending);
        for (blk, temps, target) in pending {
            self.b.switch_to_block(blk);
            for t in temps.into_iter().rev() {
                self.free(t);
            }
            self.b.ins().jump(target, &[]);
        }
        self.b
    }

    // ---- statements ----

    fn stmts(&mut self, list: &[Stmt]) {
        for s in list {
            self.stmt(s);
        }
    }

    fn stmt(&mut self, s: &Stmt) {
        match s {
            Stmt::Expr(e) => self.effect(e),
            Stmt::NewBoxes(ls) => self.new_boxes(ls, false),
            Stmt::Await { value, target } => self.await_stmt(value, *target),
            Stmt::Yield { value, kind, target } => self.yield_stmt(value, *kind, *target),
            Stmt::Block(b) => self.stmts(b),
            Stmt::If(c, then, els) => {
                let cv = self.expr(c).boolean();
                let tb = self.b.create_block();
                let eb = self.b.create_block();
                let join = self.b.create_block();
                self.b.ins().brif(cv, tb, &[], eb, &[]);
                self.b.switch_to_block(tb);
                self.stmts(then);
                if !self.is_filled() {
                    self.b.ins().jump(join, &[]);
                }
                self.b.switch_to_block(eb);
                self.stmts(els);
                if !self.is_filled() {
                    self.b.ins().jump(join, &[]);
                }
                self.b.switch_to_block(join);
            }
            Stmt::Loop { id, test, post_test, body, update, per_iteration } => {
                // Each iteration gets its own bindings for captured loop variables.
                self.new_boxes(per_iteration, true);
                let header = self.b.create_block();
                let body_b = self.b.create_block();
                let cont = self.b.create_block();
                let exit = self.b.create_block();
                self.loops.insert(*id, (Some(cont), exit));
                self.b.ins().jump(header, &[]);

                self.b.switch_to_block(header);
                match test {
                    Some(t) if !post_test => {
                        let v = self.expr(t).boolean();
                        self.b.ins().brif(v, body_b, &[], exit, &[]);
                    }
                    _ => {
                        self.b.ins().jump(body_b, &[]);
                    }
                }

                self.b.switch_to_block(body_b);
                self.scopes.push(Scope::Jump(*id));
                self.stmts(body);
                self.scopes.pop();
                if !self.is_filled() {
                    self.b.ins().jump(cont, &[]);
                }

                self.b.switch_to_block(cont);
                let counter = self.bounded_counter(test.as_ref().filter(|_| !post_test), body, update.as_ref());
                self.bounded.extend(counter);
                self.new_boxes(per_iteration, true);
                if let Some(u) = update {
                    self.effect(u);
                }
                if counter.is_some() {
                    self.bounded.pop();
                }
                match test {
                    Some(t) if *post_test => {
                        let v = self.expr(t).boolean();
                        self.b.ins().brif(v, header, &[], exit, &[]);
                    }
                    // Rotated: a copy of a cheap test at the bottom saves a
                    // jump per iteration.
                    Some(t) if is_cheap(t) => {
                        let v = self.expr(t).boolean();
                        self.b.ins().brif(v, body_b, &[], exit, &[]);
                    }
                    _ => {
                        self.b.ins().jump(header, &[]);
                    }
                }
                self.b.switch_to_block(exit);
            }
            Stmt::Switch { id, disc, cases } => self.switch(*id, *disc, cases),
            Stmt::Break(id) => {
                self.leave_to(*id, false);
                let (_, exit) = self.loops[id];
                self.b.ins().jump(exit, &[]);
                self.after_terminator();
            }
            Stmt::Continue(id) => {
                self.leave_to(*id, true);
                let (cont, _) = self.loops[id];
                self.b.ins().jump(cont.expect("continue targets a loop"), &[]);
                self.after_terminator();
            }
            Stmt::Return(e) => {
                let v = match e {
                    Some(e) => self.expr(e),
                    None => V::Void,
                };
                let v = if self.ret == Ty::Void {
                    self.free_v(v);
                    V::Void
                } else {
                    v
                };
                self.return_value(v);
            }
            Stmt::Throw(e) => {
                let (p, t) = self.expr(e).any();
                let ctx = self.ctx;
                self.helper("JS_Throw", &[ctx, p, t]);
                let edge = self.throw_edge();
                self.b.ins().jump(edge, &[]);
                self.after_terminator();
            }
            Stmt::Try { body, catch, finally, exc } => self.try_stmt(body, catch.as_ref(), finally.as_ref(), *exc),
            Stmt::IterGuard { iter, next, loop_id, body } => self.iter_guard(*iter, *next, *loop_id, body),
        }
    }

    /// Return `v` (owned), running the `finally` blocks on the way out.
    fn return_value(&mut self, v: V) {
        let v = if self.scopes.iter().any(|s| !matches!(s, Scope::Jump(_))) {
            match (self.ret_slot, v) {
                (_, V::Void) => {
                    self.run_finally_until(0);
                    V::Void
                }
                (None, v) => {
                    self.hold(v);
                    self.run_finally_until(0);
                    self.release(v);
                    v
                }
                (Some(slot), v) => {
                    // The finalizers may suspend: the value waits in a local.
                    let (p, t) = self.boxed(v);
                    self.set_local(slot, V::Any(p, t));
                    self.run_finally_until(0);
                    let (p, t) = self.take_local(slot);
                    match self.ret {
                        Ty::F64 | Ty::I32 => V::Num(self.number_of((p, t))),
                        Ty::Bool => {
                            let x = self.b.ins().ireduce(I8, p);
                            V::Bool(self.b.ins().icmp_imm_s(IntCC::NotEqual, x, 0))
                        }
                        _ => V::Any(p, t),
                    }
                }
            }
        } else {
            v
        };
        let ret = self.ret;
        let args: Vec<BlockArg> = self.abi(ret, v).into_iter().map(BlockArg::Value).collect();
        let rb = self.ret_block;
        self.b.ins().jump(rb, &args);
        self.after_terminator();
    }

    /// Move the value out of a JS-valued local, leaving `undefined`.
    fn take_local(&mut self, l: ir::LocalId) -> (Value, Value) {
        let v = self.use_local(l).any();
        let u = self.konst(0, TAG_UNDEFINED);
        self.def_local(l, u);
        v
    }

    /// Run the `finally` blocks between here and jump target `id`.
    fn leave_to(&mut self, id: ir::LoopId, is_continue: bool) {
        let depth = self.scopes.iter().rposition(|s| matches!(s, Scope::Jump(t) if *t == id)).expect("jump target");
        self.unwind(depth + 1, is_continue.then_some(id));
    }

    /// Run every `finally` in `scopes[depth..]`, innermost first, each with
    /// the handlers and scopes that surround its `try`.
    fn run_finally_until(&mut self, depth: usize) {
        self.unwind(depth, None);
    }

    /// Leave `scopes[depth..]`, innermost first: run the `finally` blocks
    /// and close the iterators (except for a `continue` of loop
    /// `continued`), each with the handlers and scopes around it.
    fn unwind(&mut self, depth: usize, continued: Option<ir::LoopId>) {
        let mut i = self.scopes.len();
        while i > depth {
            i -= 1;
            let (handlers, scopes) = match &self.scopes[i] {
                Scope::Jump(_) => continue,
                Scope::CloseIter { loop_id, .. } if continued.is_some() && *loop_id == continued => continue,
                Scope::Finally { handlers, scopes, .. } | Scope::CloseIter { handlers, scopes, .. } => {
                    (*handlers, *scopes)
                }
            };
            let saved_handlers = self.handlers.split_off(handlers);
            let saved_scopes = self.scopes.split_off(scopes);
            match &saved_scopes[i - scopes] {
                Scope::Finally { body, .. } => {
                    let body = body.clone();
                    self.stmts(&body);
                }
                Scope::CloseIter { iter, next, .. } => self.close_iter(*iter, *next),
                Scope::Jump(_) => unreachable!(),
            }
            self.handlers.extend(saved_handlers);
            self.scopes.extend(saved_scopes);
        }
    }

    fn try_stmt(
        &mut self,
        body: &[Stmt],
        catch: Option<&(Option<ir::LocalId>, Vec<Stmt>)>,
        finally: Option<&Vec<Stmt>>,
        exc_slot: Option<ir::LocalId>,
    ) {
        let join = self.b.create_block();
        let fin_handler = finally.map(|_| {
            let blk = self.b.create_block();
            self.b.set_cold_block(blk);
            blk
        });
        let catch_blk = catch.map(|_| self.b.create_block());
        let outer_handlers = self.handlers.len();
        let outer_scopes = self.scopes.len();
        if let Some(f) = finally {
            self.scopes.push(Scope::Finally { body: f.clone(), handlers: outer_handlers, scopes: outer_scopes });
        }
        if let Some(fh) = fin_handler {
            self.handlers.push(fh);
        }
        if let Some(cb) = catch_blk {
            self.handlers.push(cb);
        }

        // try { body }
        self.stmts(body);
        if catch_blk.is_some() {
            self.handlers.pop();
        }
        if !self.is_filled() {
            self.finally_then_jump(finally, join);
        }

        // catch (e) { handler }
        if let (Some(cb), Some((param, handler))) = (catch_blk, catch) {
            self.b.switch_to_block(cb);
            self.clear_flag();
            let ctx = self.ctx;
            let exc = self.helper("JS_GetException", &[ctx]);
            match param {
                Some(l) => self.set_local(*l, V::Any(exc[0], exc[1])),
                None => self.free((exc[0], exc[1])),
            }
            self.stmts(handler);
            if !self.is_filled() {
                self.finally_then_jump(finally, join);
            }
        }
        if fin_handler.is_some() {
            self.handlers.pop();
        }
        if finally.is_some() {
            self.scopes.pop();
        }

        // finally on the exception path: save the exception, run the
        // finalizer, rethrow.
        if let (Some(fh), Some(f)) = (fin_handler, finally) {
            self.b.switch_to_block(fh);
            self.clear_flag();
            let ctx = self.ctx;
            let exc = self.helper("JS_GetException", &[ctx]);
            let exc = V::Any(exc[0], exc[1]);
            let (p, t) = match exc_slot {
                // The finalizer may suspend: the exception waits in a local.
                Some(slot) => {
                    self.set_local(slot, exc);
                    self.stmts(f);
                    self.take_local(slot)
                }
                None => {
                    self.hold(exc);
                    self.stmts(f);
                    self.release(exc);
                    exc.any()
                }
            };
            self.helper("JS_Throw", &[ctx, p, t]);
            let edge = self.throw_edge();
            self.b.ins().jump(edge, &[]);
        }
        self.b.switch_to_block(join);
    }

    fn finally_then_jump(&mut self, finally: Option<&Vec<Stmt>>, join: Block) {
        if let Some(f) = finally {
            // The finalizer runs with the handlers outside the `try`.
            let n = self.scopes.iter().rposition(|s| matches!(s, Scope::Finally { .. })).unwrap();
            let Scope::Finally { handlers, scopes, .. } = self.scopes[n] else { unreachable!() };
            let saved_handlers = self.handlers.split_off(handlers);
            let saved_scopes = self.scopes.split_off(scopes);
            self.stmts(f);
            self.handlers.extend(saved_handlers);
            self.scopes.extend(saved_scopes);
        }
        if !self.is_filled() {
            self.b.ins().jump(join, &[]);
        }
    }

    fn switch(&mut self, id: ir::LoopId, disc: ir::LocalId, cases: &[(Option<Expr>, Vec<Stmt>)]) {
        let exit = self.b.create_block();
        let bodies: Vec<Block> = cases.iter().map(|_| self.b.create_block()).collect();
        self.loops.insert(id, (None, exit));
        // Tests, in order; then the default case (or the exit).
        for (i, (test, _)) in cases.iter().enumerate() {
            let Some(test) = test else { continue };
            let v = self.expr(test);
            self.hold(v);
            let d = self.use_local(disc);
            let eq = self.strict_eq(d.any(), v.any());
            self.release(v);
            self.free_v(v);
            let next = self.b.create_block();
            self.b.ins().brif(eq, bodies[i], &[], next, &[]);
            self.b.switch_to_block(next);
        }
        let default = cases.iter().position(|(t, _)| t.is_none()).map_or(exit, |i| bodies[i]);
        self.b.ins().jump(default, &[]);
        self.scopes.push(Scope::Jump(id));
        for (i, (_, body)) in cases.iter().enumerate() {
            self.b.switch_to_block(bodies[i]);
            self.stmts(body);
            if !self.is_filled() {
                let next = bodies.get(i + 1).copied().unwrap_or(exit);
                self.b.ins().jump(next, &[]);
            }
        }
        self.scopes.pop();
        self.b.switch_to_block(exit);
    }

    /// `a === b` for borrowed values.
    fn strict_eq(&mut self, a: (Value, Value), b: (Value, Value)) -> Value {
        self.compare_any(AnyCmpOp::StrictEq, a, b)
    }

    // ---- expressions ----

    /// Evaluate for side effects only.
    fn effect(&mut self, e: &Expr) {
        match &e.kind {
            ExprKind::Box(x) => self.effect(x),
            ExprKind::SetProp(o, a, x) if self.prop_step(o, *a, x).is_some() => {
                let step = self.prop_step(o, *a, x).unwrap();
                self.prop_update(o, *a, step, false);
            }
            ExprKind::SetLocal(l, _) | ExprKind::InitLocal(l, _) if matches!(self.vars[*l], LVar::Int(_)) => {
                self.expr(e);
            }
            ExprKind::SetLocal(l, x) => {
                let v = self.expr(x);
                self.set_local(*l, v);
            }
            ExprKind::InitLocal(l, x) => {
                let v = self.expr(x);
                self.init_local(*l, v);
            }
            _ => {
                let v = self.expr(e);
                self.free_v(v);
            }
        }
    }

    /// Store an owned value in a local, freeing its previous value. Boxed
    /// locals are checked for their TDZ first.
    fn set_local(&mut self, l: ir::LocalId, v: V) {
        if let LVar::Boxed { .. } = self.vars[l] {
            let old = self.load_box(l);
            self.hold(v);
            self.check_tdz(l, old.1);
            self.release(v);
            self.store_box(l, v.any());
            self.free(old);
        } else if is_js(self.locals[l]) {
            let old = self.use_local(l).any();
            self.def_local(l, v);
            self.free(old);
        } else {
            self.def_local(l, v);
        }
    }

    /// A local's initialization by its declaration (no TDZ check).
    fn init_local(&mut self, l: ir::LocalId, v: V) {
        if let LVar::Boxed { .. } = self.vars[l] {
            let old = self.load_box(l);
            self.store_box(l, v.any());
            self.free(old);
        } else {
            self.set_local(l, v);
        }
    }

    /// An `f64` that holds an integer as that `i64` (exact by construction,
    /// else converted).
    fn int_of_num(&mut self, d: Value) -> Value {
        match self.int_of.get(&d) {
            Some(&i) => i,
            None => self.b.ins().fcvt_to_sint_sat(I64, d),
        }
    }

    /// Values of type `ty` as they cross a block, call or return: `I32`
    /// ones as `i64`s.
    fn abi(&mut self, ty: Ty, v: V) -> Vec<Value> {
        match (ty, v) {
            (Ty::I32, V::Num(d)) => vec![self.int_of_num(d)],
            _ => v.values(),
        }
    }

    /// The inverse of `abi`.
    fn unabi(&mut self, ty: Ty, vs: &[Value]) -> V {
        match from_values(ty, vs) {
            V::Int(i) => {
                let d = self.int_f64(i);
                self.i32s.insert(d);
                V::Num(d)
            }
            v => v,
        }
    }

    /// Integers to compare `a` and `c` by, when that agrees with comparing
    /// the doubles: both exact, or an int32 against a rounded product of
    /// int32s (a rounded product is past 2^53, far from any int32). `%`
    /// results may be -0 for 0, which compares the same.
    /// Also the flags under which they are valid (else a `%` result is NaN).
    fn int_pair(&self, a: Value, c: Value) -> Option<(Value, Value, Vec<Value>)> {
        let exact = |v: &Value| match self.rem_of.get(v) {
            Some(&(i, ok)) => Some((i, Some(ok))),
            None => self.int_of.get(v).map(|&i| (i, None)),
        };
        let small = |v: &Value| self.i32s.contains(v) || self.rem_of.contains_key(v);
        let (x, y) = match (exact(&a), exact(&c)) {
            (Some(x), Some(y)) => (x, y),
            (None, Some(y)) if small(&c) => ((*self.wide_of.get(&a)?, None), y),
            (Some(x), None) if small(&a) => (x, (*self.wide_of.get(&c)?, None)),
            _ => return None,
        };
        Some((x.0, y.0, x.1.into_iter().chain(y.1).collect()))
    }

    /// `a % c` for int32s `a` (`x`) and `y`: the integer remainder,
    /// with the dividend's sign (`-4 % 2` is -0); NaN for `% 0`.
    fn int_rem(&mut self, x: Value, y: Value, a: Value) -> Value {
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, F64);
        let r_param = self.b.append_block_param(join, I64);
        let nonzero = self.b.ins().icmp_imm_s(IntCC::NotEqual, y, 0);
        self.b.ins().brif(nonzero, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        // 32-bit division is much faster than 64-bit (Cranelift defines
        // INT32_MIN % -1 as 0).
        let (x32, y32) = (self.b.ins().ireduce(I32, x), self.b.ins().ireduce(I32, y));
        let r = self.b.ins().srem(x32, y32);
        let r = self.b.ins().sextend(I64, r);
        let rf = self.b.ins().fcvt_from_sint(F64, r);
        let rf = self.b.ins().fcopysign(rf, a);
        self.b.ins().jump(join, &[BlockArg::Value(rf), BlockArg::Value(r)]);
        self.b.switch_to_block(slow);
        let nan = self.b.ins().f64const(f64::NAN);
        let zero = self.b.ins().iconst(I64, 0);
        self.b.ins().jump(join, &[BlockArg::Value(nan), BlockArg::Value(zero)]);
        self.b.switch_to_block(join);
        self.rem_of.insert(res, (r_param, nonzero));
        res
    }

    /// An int local's value as an `f64` (exact: see `int_locals`).
    fn int_f64(&mut self, i: Value) -> Value {
        let d = self.b.ins().fcvt_from_sint(F64, i);
        self.int_of.insert(d, i);
        d
    }

    /// `x + c` for an int local `x`, rounded as `f64` arithmetic would past
    /// 2^53.
    fn int_add(&mut self, x: Value, c: i64) -> Value {
        let r = self.b.ins().iadd_imm_s(x, c);
        if c == 0 {
            return r;
        }
        let over = if c > 0 {
            self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, r, INT_EXACT)
        } else {
            self.b.ins().icmp_imm_s(IntCC::SignedLessThan, r, -INT_EXACT)
        };
        let round = self.b.create_block();
        self.b.set_cold_block(round);
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, I64);
        self.b.ins().brif(over, round, &[], join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(round);
        let d = self.b.ins().fcvt_from_sint(F64, r);
        let i = self.b.ins().fcvt_to_sint_sat(I64, d);
        self.b.ins().jump(join, &[BlockArg::Value(i)]);
        self.b.switch_to_block(join);
        res
    }

    /// For a loop `for (...; i < K; i++)` (or `<=`, `--` with `>` / `>=`)
    /// over an int local `i` that the body does not assign, with `K` an
    /// integer literal: `i`. Its update then stays within K +/- 256, far
    /// from 2^53.
    fn bounded_counter(&self, test: Option<&Expr>, body: &[Stmt], update: Option<&Expr>) -> Option<ir::LocalId> {
        let ExprKind::Compare(op, x, y) = &test?.kind else { return None };
        let ExprKind::Local(l) = x.kind else { return None };
        let ExprKind::Update { local, delta, .. } = update?.kind else { return None };
        int_literal(y, (INT_EXACT >> 1) as f64)?;
        let up = matches!(op, CmpOp::Lt | CmpOp::Le) && delta > 0.0;
        let down = matches!(op, CmpOp::Gt | CmpOp::Ge) && delta < 0.0;
        if local != l || !(up || down) || !matches!(self.vars[l], LVar::Int(_)) {
            return None;
        }
        let mut assigned = false;
        ir::visit_stmts(body, &mut |e| assigned |= ir::assigns_local(e, l));
        (!assigned).then_some(l)
    }

    /// The new value of int local `l` assigned `x` (a form `int_locals`
    /// accepts).
    fn int_value(&mut self, l: ir::LocalId, x: &Expr) -> Value {
        if let Some(c) = int_literal(x, INT_EXACT as f64) {
            return self.b.ins().iconst(I64, c);
        }
        if let Some(c) = int_step(l, x) {
            let LVar::Int(var) = self.vars[l] else { unreachable!() };
            let cur = self.b.use_var(var);
            if self.small[l] || self.locals[l] == Ty::I32 {
                return self.b.ins().iadd_imm_s(cur, c);
            }
            return self.int_add(cur, c);
        }
        if let ExprKind::Local(m) = x.kind
            && let LVar::Int(var) = self.vars[m]
        {
            return self.b.use_var(var);
        }
        // An int by the checker (an `int` local's value) or by `int_locals`.
        let d = self.expr(x).num();
        self.int_of_num(d)
    }

    pub(crate) fn expr(&mut self, e: &Expr) -> V {
        match &e.kind {
            ExprKind::Num(v) => {
                let d = self.b.ins().f64const(*v);
                if let Some(c) = int_literal(e, INT_EXACT as f64) {
                    let i = self.b.ins().iconst(I64, c);
                    self.int_of.insert(d, i);
                    if i32::try_from(c).is_ok() {
                        self.i32s.insert(d);
                    }
                }
                V::Num(d)
            }
            ExprKind::Bool(v) => V::Bool(self.b.ins().iconst(I8, *v as i64)),
            ExprKind::Undefined => self.konst(0, TAG_UNDEFINED),
            ExprKind::Null => self.konst(0, TAG_NULL),
            ExprKind::Local(l) => {
                let v = self.use_local(*l);
                if let V::Any(p, t) = v {
                    self.dup((p, t));
                }
                v
            }
            ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x) if matches!(self.vars[*l], LVar::Int(_)) => {
                let LVar::Int(var) = self.vars[*l] else { unreachable!() };
                let i = self.int_value(*l, x);
                self.b.def_var(var, i);
                V::Num(self.int_f64(i))
            }
            ExprKind::SetLocal(l, x) => {
                let v = self.expr(x);
                if let V::Any(p, t) = v {
                    // One reference for the local, one for the expression.
                    self.dup((p, t));
                }
                self.set_local(*l, v);
                v
            }
            ExprKind::This => {
                let (p, t) = self.this.expect("`this` in a method");
                self.dup((p, t));
                V::Any(p, t)
            }
            ExprKind::InitLocal(l, x) => {
                let v = self.expr(x);
                if let V::Any(p, t) = v {
                    self.dup((p, t));
                }
                self.init_local(*l, v);
                v
            }
            ExprKind::Closure(cid, boxes) => {
                // Data slots: per captured variable, its box object and the
                // address of its value (borrowed: the function dups them).
                // A value capture: the value itself (the closure points
                // the second slot at it).
                let by_value = self.env.value_captures[*cid].clone();
                let mut vals = Vec::new();
                for (&bl, &value) in boxes.iter().zip(&by_value) {
                    let tag = self.b.ins().iconst(I64, TAG_FLOAT64);
                    if value {
                        let v = match self.vars[bl] {
                            LVar::Boxed { .. } => self.load_box(bl),
                            _ => self.use_local(bl).any(),
                        };
                        let zero = self.b.ins().iconst(I64, 0);
                        vals.push(v);
                        vals.push((zero, tag));
                        continue;
                    }
                    let LVar::Boxed { obj: (op, ot), .. } = self.vars[bl] else { unreachable!() };
                    let obj = (self.b.use_var(op), self.b.use_var(ot));
                    let slot = self.box_slot(bl);
                    vals.push(obj);
                    vals.push((slot, tag));
                }
                let data = self.values_slot(&vals);
                let index = self.b.ins().iconst(I32, (self.env.closure_base + cid) as i64);
                let n = self.b.ins().iconst(I32, vals.len() as i64);
                let ctx = self.ctx;
                let r = self.helper("tsaot_make_closure", &[ctx, index, n, data]);
                self.check_value(r[1]);
                V::Any(r[0], r[1])
            }
            ExprKind::Update { local, delta, prefix } => {
                if let LVar::Int(var) = self.vars[*local] {
                    let old = self.b.use_var(var);
                    let new = if self.small[*local] || self.bounded.contains(local) || self.locals[*local] == Ty::I32 {
                        self.b.ins().iadd_imm_s(old, *delta as i64)
                    } else {
                        self.int_add(old, *delta as i64)
                    };
                    self.b.def_var(var, new);
                    return V::Num(self.int_f64(if *prefix { new } else { old }));
                }
                let old = self.use_local(*local).num();
                let d = self.b.ins().f64const(*delta);
                let new = self.b.ins().fadd(old, d);
                self.def_local(*local, V::Num(new));
                V::Num(if *prefix { new } else { old })
            }
            ExprKind::Unary(op, x) => {
                let v = self.expr(x);
                match op {
                    UnOp::Neg => V::Num(self.b.ins().fneg(v.num())),
                    UnOp::Not => V::Bool(self.b.ins().bxor_imm_u(v.boolean(), 1)),
                    UnOp::BitNot => {
                        let i = self.toint32(v.num());
                        let r = self.b.ins().bnot(i);
                        let r = self.b.ins().sextend(I64, r);
                        let d = self.int_f64(r);
                        self.i32s.insert(d);
                        V::Num(d)
                    }
                }
            }
            ExprKind::Truthy(x) => {
                let v = self.expr(x);
                V::Bool(match v {
                    V::Bool(b) => b,
                    V::Num(d) => self.truthy_num(d),
                    V::Any(p, t) => {
                        let r = self.truthy_any((p, t));
                        self.free((p, t));
                        r
                    }
                    V::Void => self.b.ins().iconst(I8, 0),
                    V::Int(_) => unreachable!("an ABI value"),
                })
            }
            ExprKind::Binary(op, x, y) => {
                let a = self.expr(x).num();
                let c = self.expr(y).num();
                V::Num(self.binary(*op, a, c))
            }
            ExprKind::NumArith(op, x, y) => {
                let a = self.expr(x);
                self.hold(a);
                let c = self.expr(y);
                self.hold(c);
                let an = self.number_value(a);
                let cn = self.number_value(c);
                self.release(c);
                self.release(a);
                self.free_v(c);
                self.free_v(a);
                V::Num(self.binary(*op, an, cn))
            }
            ExprKind::Compare(op, x, y) => {
                let a = self.expr(x);
                let c = self.expr(y);
                V::Bool(match (a, c) {
                    (V::Num(a), V::Num(c)) if self.int_pair(a, c).is_some() => {
                        let (ia, ic, valid) = self.int_pair(a, c).unwrap();
                        let cc = match op {
                            CmpOp::Lt => IntCC::SignedLessThan,
                            CmpOp::Le => IntCC::SignedLessThanOrEqual,
                            CmpOp::Gt => IntCC::SignedGreaterThan,
                            CmpOp::Ge => IntCC::SignedGreaterThanOrEqual,
                            CmpOp::Eq => IntCC::Equal,
                            CmpOp::Ne => IntCC::NotEqual,
                        };
                        let r = self.b.ins().icmp(cc, ia, ic);
                        // A NaN `%` result: only `!=` holds.
                        valid.into_iter().fold(r, |r, ok| {
                            if *op == CmpOp::Ne {
                                let nan = self.b.ins().bxor_imm_u(ok, 1);
                                self.b.ins().bor(r, nan)
                            } else {
                                self.b.ins().band(r, ok)
                            }
                        })
                    }
                    (V::Num(a), V::Num(c)) => {
                        let cc = match op {
                            CmpOp::Lt => FloatCC::LessThan,
                            CmpOp::Le => FloatCC::LessThanOrEqual,
                            CmpOp::Gt => FloatCC::GreaterThan,
                            CmpOp::Ge => FloatCC::GreaterThanOrEqual,
                            CmpOp::Eq => FloatCC::Equal,
                            CmpOp::Ne => FloatCC::NotEqual,
                        };
                        self.b.ins().fcmp(cc, a, c)
                    }
                    (V::Bool(a), V::Bool(c)) => {
                        let cc = if *op == CmpOp::Eq { IntCC::Equal } else { IntCC::NotEqual };
                        self.b.ins().icmp(cc, a, c)
                    }
                    _ => unreachable!("compare operand types"),
                })
            }
            ExprKind::Logical(op, x, y) => self.logical(*op, e.ty, x, y),
            ExprKind::Cond(c, x, y) => {
                let cv = self.expr(c).boolean();
                let tb = self.b.create_block();
                let eb = self.b.create_block();
                let join = self.b.create_block();
                let tys = clif_types(e.ty);
                for &t in &tys {
                    self.b.append_block_param(join, t);
                }
                self.b.ins().brif(cv, tb, &[], eb, &[]);
                for (blk, arm) in [(tb, x), (eb, y)] {
                    self.b.switch_to_block(blk);
                    let v = self.expr(arm);
                    let args: Vec<BlockArg> = self.abi(e.ty, v).into_iter().map(BlockArg::Value).collect();
                    self.b.ins().jump(join, &args);
                }
                self.b.switch_to_block(join);
                let params = self.b.block_params(join).to_vec();
                self.unabi(e.ty, &params)
            }
            ExprKind::Call(id, args) => {
                let mut vals = Vec::new();
                let mut held = Vec::new();
                let tys = self.env.params.clone();
                for (a, &t) in args.iter().zip(&tys[*id]) {
                    let v = self.expr(a);
                    self.hold(v);
                    held.push(v);
                    vals.extend(self.abi(t, v));
                }
                // The callee takes ownership of its arguments.
                for v in held.into_iter().rev() {
                    self.release(v);
                }
                let mut call_args = vec![self.ctx];
                call_args.extend(vals);
                let call = self.b.ins().call(self.env.funcs[*id], &call_args);
                let results = self.b.inst_results(call).to_vec();
                self.check_flag();
                self.unabi(e.ty, &results)
            }
            ExprKind::Math(f, args) => {
                let vals: Vec<Value> = args.iter().map(|a| self.expr(a).num()).collect();
                V::Num(self.math(*f, &vals)).retype(f.ret(), self)
            }
            ExprKind::Seq(xs) => {
                let (last, rest) = xs.split_last().unwrap();
                for x in rest {
                    self.effect(x);
                }
                self.expr(last)
            }
            ExprKind::Void(x) => {
                self.effect(x);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::Str(id) => {
                let base = self.b.ins().symbol_value(self.ptr, self.env.strings);
                let off = (*id * SIZE) as i32;
                let p = self.b.ins().load(I64, Self::mem(), base, off);
                let t = self.b.ins().load(I64, Self::mem(), base, off + TAG_OFFSET);
                self.dup((p, t));
                V::Any(p, t)
            }
            ExprKind::Box(x) => match self.expr(x) {
                V::Num(d) if x.ty == Ty::MaybeF64 => {
                    let (p, t) = self.box_maybe(d);
                    V::Any(p, t)
                }
                V::Num(d) => {
                    let (p, t) = self.box_number(d);
                    V::Any(p, t)
                }
                V::Bool(b) => {
                    let p = self.b.ins().uextend(I64, b);
                    let t = self.b.ins().iconst(I64, TAG_BOOL);
                    V::Any(p, t)
                }
                other => other,
            },
            ExprKind::AsyncIterator(x) => {
                let v = self.expr(x);
                self.hold(v);
                let (p, t) = v.any();
                let ctx = self.ctx;
                let is_async = self.b.ins().iconst(I32, 1);
                let r = self.helper("JS_GetIteratorObject", &[ctx, p, t, is_async]);
                self.check_value(r[1]);
                self.release(v);
                self.free_v(v);
                V::Any(r[0], r[1])
            }
            ExprKind::AsyncIterResult(x, done) => {
                let v = self.expr(x);
                self.hold(v);
                let (p, t) = v.any();
                let ctx = self.ctx;
                let done_addr = self.done_slot();
                let r = self.helper("tsaot_for_await_result", &[ctx, p, t, done_addr]);
                self.check_value(r[1]);
                self.release(v);
                self.free_v(v);
                let d = self.b.ins().load(I32, Self::mem(), done_addr, 0);
                let d = self.b.ins().icmp_imm_s(IntCC::NotEqual, d, 0);
                self.set_local(*done, V::Bool(d));
                V::Any(r[0], r[1])
            }
            ExprKind::CheckObject(x) => {
                let v = self.expr(x);
                self.hold(v);
                let (p, t) = v.any();
                let ctx = self.ctx;
                let r = self.helper("tsaot_check_object", &[ctx, p, t]);
                self.check_int(r[0]);
                self.release(v);
                self.free_v(v);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::IterCloseQuiet(l) => {
                let (p, t) = self.use_local(*l).any();
                let ctx = self.ctx;
                self.helper("tsaot_iter_close_quiet", &[ctx, p, t]);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::IntOf(x) => {
                // An int32 is not reference-counted: nothing to free.
                let (p, _) = self.expr(x).any();
                let i = self.b.ins().ireduce(I32, p);
                let i = self.b.ins().sextend(I64, i);
                let d = self.int_f64(i);
                self.i32s.insert(d);
                V::Num(d)
            }
            ExprKind::AnyUnary(op, x) => {
                let v = self.expr(x).any();
                let (p, t) = self.unary_any(*op, v);
                V::Any(p, t)
            }
            ExprKind::AnyBinary(op, x, y) => {
                let a = self.expr(x);
                self.hold(a);
                let c = self.expr(y);
                self.hold(c);
                let (p, t) = self.binary_any(*op, a.any(), c.any());
                self.release(c);
                self.release(a);
                V::Any(p, t)
            }
            ExprKind::AnyCompare(op @ (AnyCmpOp::StrictEq | AnyCmpOp::StrictNe), x, y)
                if matches!(x.kind, ExprKind::Undefined | ExprKind::Null)
                    || matches!(y.kind, ExprKind::Undefined | ExprKind::Null) =>
            {
                // Strict equality with `undefined` / `null` is a tag test.
                let (other, konst) =
                    if matches!(y.kind, ExprKind::Undefined | ExprKind::Null) { (x, y) } else { (y, x) };
                let (v, owned) = self.operand(other, &[]);
                let tag = if let ExprKind::Null = konst.kind { TAG_NULL } else { TAG_UNDEFINED };
                let r = match v {
                    V::Any(_, t) => {
                        let cc = if *op == AnyCmpOp::StrictEq { IntCC::Equal } else { IntCC::NotEqual };
                        self.b.ins().icmp_imm_s(cc, t, tag)
                    }
                    _ => self.b.ins().iconst(I8, (*op == AnyCmpOp::StrictNe) as i64),
                };
                self.drop_operand(v, owned);
                V::Bool(r)
            }
            ExprKind::AnyCompare(op @ (AnyCmpOp::Eq | AnyCmpOp::Ne), x, y)
                if matches!(x.kind, ExprKind::Undefined | ExprKind::Null)
                    || matches!(y.kind, ExprKind::Undefined | ExprKind::Null) =>
            {
                // Loose equality with `undefined` / `null`: the value is one of
                // them (no object is "HTMLDDA": only a test harness API makes
                // one, which this runtime never calls).
                let other = if matches!(y.kind, ExprKind::Undefined | ExprKind::Null) { x } else { y };
                let (v, owned) = self.operand(other, &[]);
                let eq = *op == AnyCmpOp::Eq;
                let r = match v {
                    V::Any(_, t) => {
                        let u = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_UNDEFINED);
                        let n = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_NULL);
                        let nullish = self.b.ins().bor(u, n);
                        if eq { nullish } else { self.b.ins().bxor_imm_u(nullish, 1) }
                    }
                    _ => self.b.ins().iconst(I8, !eq as i64),
                };
                self.drop_operand(v, owned);
                V::Bool(r)
            }
            ExprKind::AnyCompare(op, x, y) => {
                // Boxed numbers are compared unboxed (boxed only to go slow).
                let a = self.unboxed_operand(x);
                self.hold(a);
                let c = self.unboxed_operand(y);
                self.hold(c);
                let r = self.compare_mixed(*op, a, c);
                self.release(c);
                self.release(a);
                self.free_v(c);
                self.free_v(a);
                V::Bool(r)
            }
            ExprKind::Cell(k) => {
                let (p, t) = self.load_cell(*k);
                self.dup((p, t));
                V::Any(p, t)
            }
            ExprKind::SetCell(k, x) | ExprKind::InitCell(k, x) => {
                let v = self.expr(x);
                self.hold(v);
                // A declaration replaces `uninitialized` (nothing to free).
                let old = if let ExprKind::InitCell(..) = e.kind { self.raw_cell(*k) } else { self.load_cell(*k) };
                let (p, t) = v.any();
                self.dup((p, t));
                let base = self.b.ins().symbol_value(self.ptr, self.env.cells);
                let off = (*k * SIZE) as i32;
                self.b.ins().store(Self::mem(), p, base, off);
                self.b.ins().store(Self::mem(), t, base, off + TAG_OFFSET);
                self.free(old);
                self.release(v);
                v
            }
            ExprKind::Global(a) => {
                let atom = self.atom(*a);
                let ctx = self.ctx;
                let r = self.helper("JS_GetGlobalVariable", &[ctx, atom]);
                self.check_value(r[1]);
                V::Any(r[0], r[1])
            }
            ExprKind::GlobalOrUndefined(a) => {
                let atom = self.atom(*a);
                let ctx = self.ctx;
                let r = self.helper("tsaot_global_or_undefined", &[ctx, atom]);
                self.check_value(r[1]);
                V::Any(r[0], r[1])
            }
            ExprKind::SetGlobal(a, x) => {
                let v = self.expr(x);
                self.hold(v);
                let (p, t) = v.any();
                self.dup((p, t));
                let atom = self.atom(*a);
                let ctx = self.ctx;
                let r = self.helper("JS_SetGlobalVariable", &[ctx, atom, p, t]);
                self.check_int(r[0]);
                self.release(v);
                v
            }
            ExprKind::GetProp(o, a) => {
                let (obj, owned) = self.operand(o, &[]);
                let (p, t) = obj.any();
                let (rp, rt) = self.prop_get(p, t, *a);
                self.drop_operand(obj, owned);
                V::Any(rp, rt)
            }
            ExprKind::SetProp(o, a, x) if self.prop_step(o, *a, x).is_some() => {
                let step = self.prop_step(o, *a, x).unwrap();
                self.prop_update(o, *a, step, true)
            }
            ExprKind::SetProp(o, a, x) => {
                let (obj, owned) = self.operand(o, &[x]);
                let v = self.expr(x);
                self.hold(v);
                let (vp, vt) = v.any();
                // One reference for the property, one for the expression.
                self.dup((vp, vt));
                let (p, t) = obj.any();
                let ic = self.new_ic();
                let join = self.b.create_block();
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                // An own writable data property of a cached shape, or a setter
                // (own or of the prototype).
                let (shape, kind) = self.ic_shape_hit(p, t, ic, slow);
                let own = self.b.create_block();
                let proto = self.b.create_block();
                let is_own = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 0);
                self.b.ins().brif(is_own, own, &[], proto, &[]);
                self.b.switch_to_block(proto);
                let pat = self.ic_accessor_slot(p, shape, kind, ic, slow);
                let setter = self.b.ins().load(I64, Self::mem(), pat, 8);
                self.nonnull(setter, slow);
                let ctx = self.ctx;
                // The property's reference is dropped after the call.
                let tag = self.b.ins().iconst(I64, TAG_OBJECT);
                let argv = self.values_slot(&[(vp, vt)]);
                let (sp, st) = self.call_linked((setter, tag), (p, t), 1, argv, true, |l| {
                    l.dup((vp, vt));
                    let r = l.helper("tsaot_call_setter", &[ctx, setter, p, t, vp, vt]);
                    let failed = l.b.ins().icmp_imm_s(IntCC::SignedLessThan, r[0], 0);
                    let exc = l.b.ins().iconst(I64, TAG_EXCEPTION);
                    let undef = l.b.ins().iconst(I64, TAG_UNDEFINED);
                    let zero = l.b.ins().iconst(I64, 0);
                    (zero, l.b.ins().select(failed, exc, undef))
                });
                self.free((vp, vt));
                self.free((sp, st));
                self.check_value(st);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(own);
                let at = self.ic_slot_addr(p, ic);
                let old_p = self.b.ins().load(I64, Self::mem(), at, 0);
                let old_t = self.b.ins().load(I64, Self::mem(), at, TAG_OFFSET);
                self.b.ins().store(Self::mem(), vp, at, 0);
                self.b.ins().store(Self::mem(), vt, at, TAG_OFFSET);
                self.free((old_p, old_t));
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(slow);
                let atom = self.atom(*a);
                let ctx = self.ctx;
                let r = self.helper("tsaot_ic_set_proto", &[ctx, p, t, atom, vp, vt, ic]);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                self.release(v);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::GetElem(o, k) => {
                let (obj, owned) = self.operand(o, &[k]);
                let key = self.expr(k);
                let (op_, ot) = obj.any();
                let join = self.b.create_block();
                let rp = self.b.append_block_param(join, I64);
                let rt = self.b.append_block_param(join, I64);
                let slow = self.b.create_block();
                // A fast array element.
                let (ok, idx) = self.elem_index(key);
                let count = self.array_part(op_, ot, ok, slow);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, count);
                let hit = self.b.create_block();
                self.b.ins().brif(inb, hit, &[], slow, &[]);
                self.b.switch_to_block(hit);
                let at = self.array_slot(op_, idx);
                let vp = self.b.ins().load(I64, Self::mem(), at, 0);
                let vt = self.b.ins().load(I64, Self::mem(), at, TAG_OFFSET);
                self.dup((vp, vt));
                self.b.ins().jump(join, &[BlockArg::Value(vp), BlockArg::Value(vt)]);
                // Anything else. The key is passed on (JS_GetPropertyValue
                // frees it).
                self.b.switch_to_block(slow);
                let (kp, kt) = self.boxed(key);
                let ctx = self.ctx;
                let r = self.helper("JS_GetPropertyValue", &[ctx, op_, ot, kp, kt]);
                self.check_value(r[1]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
                self.b.switch_to_block(join);
                self.drop_operand(obj, owned);
                V::Any(rp, rt)
            }
            ExprKind::SetElem(o, k, x) => {
                let (obj, owned) = self.operand(o, &[k, x]);
                let key = self.expr(k);
                self.hold(key);
                let v = self.expr(x);
                self.hold(v);
                let (vp, vt) = v.any();
                // One reference for the property, one for the expression.
                self.dup((vp, vt));
                let (op_, ot) = obj.any();
                let join = self.b.create_block();
                let slow = self.b.create_block();
                // Overwrite a fast array element (always a writable data
                // property).
                let (ok, idx) = self.elem_index(key);
                let count = self.array_part(op_, ot, ok, slow);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, count);
                let hit = self.b.create_block();
                let beyond = self.b.create_block();
                self.b.ins().brif(inb, hit, &[], beyond, &[]);
                self.b.switch_to_block(hit);
                let at = self.array_slot(op_, idx);
                let old_p = self.b.ins().load(I64, Self::mem(), at, 0);
                let old_t = self.b.ins().load(I64, Self::mem(), at, TAG_OFFSET);
                self.b.ins().store(Self::mem(), vp, at, 0);
                self.b.ins().store(Self::mem(), vt, at, TAG_OFFSET);
                self.free((old_p, old_t));
                self.b.ins().jump(join, &[]);
                // Appending (index == count): the engine's append fast path,
                // which takes the value unless it does not apply (-2).
                self.b.switch_to_block(beyond);
                let is_append = self.b.ins().icmp(IntCC::Equal, idx, count);
                let append = self.b.create_block();
                self.b.ins().brif(is_append, append, &[], slow, &[]);
                self.b.switch_to_block(append);
                let ctx = self.ctx;
                let r = self.helper("JS_ArrayAppendFast", &[ctx, op_, ot, vp, vt]);
                let na = self.b.ins().icmp_imm_s(IntCC::Equal, r[0], -2);
                let appended = self.b.create_block();
                self.b.ins().brif(na, slow, &[], appended, &[]);
                self.b.switch_to_block(appended);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                // Anything else: tsaot_set_elem takes the key and one
                // reference to the value.
                self.b.switch_to_block(slow);
                let kv = self.boxed(key);
                let slot = self.values_slot(&[(op_, ot), kv, (vp, vt)]);
                self.release(v);
                self.release(key);
                self.hold(v);
                let ctx = self.ctx;
                let r = self.helper("tsaot_set_elem", &[ctx, slot]);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                self.release(v);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::Length(o, atom) => {
                let (obj, owned) = self.operand(o, &[]);
                let (op_, ot) = obj.any();
                let join = self.b.create_block();
                let rp = self.b.append_block_param(join, I64);
                let rt = self.b.append_block_param(join, I64);
                let slow = self.b.create_block();
                let not_obj = self.b.create_block();
                let is_obj = self.b.ins().icmp_imm_s(IntCC::Equal, ot, TAG_OBJECT);
                let obj_blk = self.b.create_block();
                self.b.ins().brif(is_obj, obj_blk, &[], not_obj, &[]);
                // Arrays: `length` is always own property 0, a number.
                self.b.switch_to_block(obj_blk);
                let class = self.b.ins().uload16(I32, Self::mem(), op_, layout::OBJ_CLASS_ID);
                let is_array = self.b.ins().icmp_imm_s(IntCC::Equal, class, layout::CLASS_ARRAY);
                let array = self.b.create_block();
                let other = self.b.create_block();
                self.b.ins().brif(is_array, array, &[], other, &[]);
                self.b.switch_to_block(array);
                let prop = self.b.ins().load(I64, Self::mem(), op_, layout::OBJ_PROP);
                let lp = self.b.ins().load(I64, Self::mem(), prop, 0);
                let lt = self.b.ins().load(I64, Self::mem(), prop, TAG_OFFSET);
                self.b.ins().jump(join, &[BlockArg::Value(lp), BlockArg::Value(lt)]);
                // Typed arrays with a pristine shape: the element count.
                self.b.switch_to_block(other);
                let kind = self.b.ins().iadd_imm_s(class, -layout::CLASS_TYPED_ARRAY);
                let is_ta = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThan, kind, TYPED_ARRAYS);
                let ta = self.b.create_block();
                self.b.ins().brif(is_ta, ta, &[], slow, &[]);
                self.b.switch_to_block(ta);
                let pristine = self.pristine_shape(op_, kind);
                let ta_len = self.b.create_block();
                self.b.ins().brif(pristine, ta_len, &[], slow, &[]);
                self.b.switch_to_block(ta_len);
                let count = self.b.ins().uload32(Self::mem(), op_, layout::OBJ_ARRAY_COUNT);
                let int = self.b.ins().iconst(I64, TAG_INT);
                self.b.ins().jump(join, &[BlockArg::Value(count), BlockArg::Value(int)]);
                // Strings: the length in the header.
                self.b.switch_to_block(not_obj);
                let is_str = self.b.ins().icmp_imm_s(IntCC::Equal, ot, TAG_STRING);
                let string = self.b.create_block();
                self.b.ins().brif(is_str, string, &[], slow, &[]);
                self.b.switch_to_block(string);
                let w = self.b.ins().uload32(Self::mem(), op_, layout::STRING_LEN);
                let len = self.b.ins().band_imm_u(w, 0x7fff_ffff);
                let int = self.b.ins().iconst(I64, TAG_INT);
                self.b.ins().jump(join, &[BlockArg::Value(len), BlockArg::Value(int)]);
                self.b.switch_to_block(slow);
                let a = self.atom(*atom);
                let ctx = self.ctx;
                let r = self.helper("JS_GetProperty", &[ctx, op_, ot, a]);
                self.check_value(r[1]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
                self.b.switch_to_block(join);
                self.drop_operand(obj, owned);
                V::Any(rp, rt)
            }
            ExprKind::TypedGet(kind, a, i) => {
                let (arr, owned) = self.operand(a, &[i]);
                let index = self.expr(i);
                let (p, _) = arr.any();
                let join = self.b.create_block();
                let r = self.b.append_block_param(join, F64);
                let (ok, idx) = self.elem_index(index);
                let count = self.b.ins().uload32(Self::mem(), p, layout::OBJ_ARRAY_COUNT);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, count);
                let inb = self.b.ins().band(ok, inb);
                let hit = self.b.create_block();
                let undef = self.b.ins().f64const(f64::from_bits(MAYBE_F64_UNDEFINED));
                self.b.ins().brif(inb, hit, &[], join, &[BlockArg::Value(undef)]);
                self.b.switch_to_block(hit);
                let base = self.b.ins().load(I64, Self::mem(), p, layout::OBJ_ARRAY_VALUES);
                let at = self.b.ins().imul_imm_s(idx, kind.size() as i64);
                let at = self.b.ins().iadd(base, at);
                let d = self.load_element(*kind, at);
                self.b.ins().jump(join, &[BlockArg::Value(d)]);
                self.b.switch_to_block(join);
                self.drop_operand(arr, owned);
                V::Num(r)
            }
            ExprKind::TypedSet(kind, a, i, x) => {
                let (arr, owned) = self.operand(a, &[i, x]);
                let index = self.expr(i);
                let V::Num(d) = self.expr(x) else { unreachable!("TypedSet value") };
                let (p, t) = arr.any();
                let join = self.b.create_block();
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                // In bounds, and the buffer is not immutable.
                let (ok, idx) = self.elem_index(index);
                let count = self.b.ins().uload32(Self::mem(), p, layout::OBJ_ARRAY_COUNT);
                let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, count);
                let inb = self.b.ins().band(ok, inb);
                let check = self.b.create_block();
                self.b.ins().brif(inb, check, &[], slow, &[]);
                self.b.switch_to_block(check);
                let ta = self.b.ins().load(I64, Self::mem(), p, layout::OBJ_TYPED_ARRAY);
                let buf = self.b.ins().load(I64, Self::mem(), ta, layout::TA_BUFFER);
                let abuf = self.b.ins().load(I64, Self::mem(), buf, layout::OBJ_ARRAY_BUFFER);
                let immutable = self.b.ins().uload8(I32, Self::mem(), abuf, layout::ABUF_IMMUTABLE);
                let hit = self.b.create_block();
                self.b.ins().brif(immutable, slow, &[], hit, &[]);
                self.b.switch_to_block(hit);
                let base = self.b.ins().load(I64, Self::mem(), p, layout::OBJ_ARRAY_VALUES);
                let at = self.b.ins().imul_imm_s(idx, kind.size() as i64);
                let at = self.b.ins().iadd(base, at);
                self.store_element(*kind, at, d);
                self.b.ins().jump(join, &[]);
                // Out of bounds or immutable: whatever the engine does.
                self.b.switch_to_block(slow);
                let kv = self.boxed(index);
                let vv = self.box_number(d);
                let slot = self.values_slot(&[(p, t), kv, vv]);
                let ctx = self.ctx;
                let r = self.helper("tsaot_set_elem", &[ctx, slot]);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                self.drop_operand(arr, owned);
                V::Num(d)
            }
            ExprKind::DeleteProp(o, a) => {
                let obj = self.expr(o);
                self.hold(obj);
                let atom = self.atom(*a);
                let (p, t) = obj.any();
                let flags = self.b.ins().iconst(I32, PROP_THROW);
                let ctx = self.ctx;
                let r = self.helper("JS_DeleteProperty", &[ctx, p, t, atom, flags]);
                self.check_int(r[0]);
                self.release(obj);
                self.free_v(obj);
                V::Bool(self.b.ins().icmp_imm_s(IntCC::NotEqual, r[0], 0))
            }
            ExprKind::DeleteElem(o, k) => {
                let obj = self.expr(o);
                self.hold(obj);
                let key = self.expr(k);
                self.hold(key);
                let slot = self.values_slot(&[obj.any(), key.any()]);
                let ctx = self.ctx;
                let r = self.helper("tsaot_delete_elem", &[ctx, slot]);
                self.check_int(r[0]);
                self.release(key);
                self.release(obj);
                self.free_v(key);
                self.free_v(obj);
                V::Bool(self.b.ins().icmp_imm_s(IntCC::NotEqual, r[0], 0))
            }
            ExprKind::CallValue(f, args) => {
                let fv = self.expr(f);
                self.hold(fv);
                let undef = self.konst(0, TAG_UNDEFINED);
                self.call_values(fv, undef, args)
            }
            ExprKind::CallThis(f, this, args) => {
                let fv = self.expr(f);
                self.hold(fv);
                let tv = self.expr(this);
                self.hold(tv);
                let r = self.call_values(fv, tv, args);
                self.release(tv);
                self.free_v(tv);
                r
            }
            ExprKind::CallMethod(o, key, args) => {
                // The method is read before the arguments are evaluated.
                let mut later: Vec<&Expr> = args.iter().collect();
                if let Key::Value(k) = key {
                    later.push(k);
                }
                let (obj, owned) = self.operand(o, &later);
                let (op_, ot) = obj.any();
                let ctx = self.ctx;
                let m = match key {
                    Key::Atom(a) => {
                        let (mp, mt) = self.prop_get(op_, ot, *a);
                        vec![mp, mt]
                    }
                    Key::Value(k) => {
                        let (kp, kt) = self.expr(k).any();
                        self.helper("JS_GetPropertyValue", &[ctx, op_, ot, kp, kt])
                    }
                };
                self.check_value(m[1]);
                let fv = V::Any(m[0], m[1]);
                self.hold(fv);
                let r = self.call_values(fv, obj, args);
                self.drop_operand(obj, owned);
                r
            }
            ExprKind::New(f, args) => {
                let fv = self.expr(f);
                self.hold(fv);
                let held: Vec<V> = args
                    .iter()
                    .map(|a| {
                        let v = self.expr(a);
                        self.hold(v);
                        v
                    })
                    .collect();
                let vals: Vec<(Value, Value)> = held.iter().map(|v| v.any()).collect();
                let argv = self.values_slot(&vals);
                let argc = self.b.ins().iconst(I32, args.len() as i64);
                let (fp, ft) = fv.any();
                let ctx = self.ctx;
                let r = self.helper("JS_CallConstructor", &[ctx, fp, ft, argc, argv]);
                self.check_value(r[1]);
                for v in held.into_iter().rev() {
                    self.release(v);
                    self.free_v(v);
                }
                self.release(fv);
                self.free_v(fv);
                V::Any(r[0], r[1])
            }
            ExprKind::Object(props) => {
                // The values first (the object is unobservable until made),
                // then the object: an allocation site caches the literal's
                // final shape, and later objects are made with it directly,
                // as JSC's allocation profiles do.
                let held: Vec<V> = props
                    .iter()
                    .map(|(_, x)| {
                        let (p, t) = self.expr(x).any();
                        let v = V::Any(p, t);
                        self.hold(v);
                        v
                    })
                    .collect();
                let vals: Vec<(Value, Value)> = held.iter().map(|v| v.any()).collect();
                // Both paths take the values.
                for &v in held.iter().rev() {
                    self.release(v);
                }
                let values = self.values_slot(&vals);
                let ic = self.new_ic();
                let ctx = self.ctx;
                let join = self.b.create_block();
                let rp = self.b.append_block_param(join, I64);
                let rt = self.b.append_block_param(join, I64);
                let fast = self.b.create_block();
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                let shape = self.b.ins().load(I64, Self::mem(), ic, 0);
                self.b.ins().brif(shape, fast, &[], slow, &[]);
                self.b.switch_to_block(fast);
                let r = self.helper("JS_NewObjectWithCachedShape", &[ctx, shape, values]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
                self.b.switch_to_block(slow);
                let n = props.len();
                let atoms = self.b.create_sized_stack_slot(StackSlotData::new(
                    StackSlotKind::ExplicitSlot,
                    (n.max(1) * 4) as u32,
                    2,
                ));
                for (i, (a, _)) in props.iter().enumerate() {
                    let atom = self.atom(*a);
                    self.b.ins().stack_store(self.ptr, atom, atoms, (i * 4) as i32);
                }
                let atoms = self.b.ins().stack_addr(self.ptr, atoms, 0);
                let count = self.b.ins().iconst(I32, n as i64);
                let r = self.helper("tsaot_new_object", &[ctx, ic, count, atoms, values]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
                self.b.switch_to_block(join);
                self.check_value(rt);
                V::Any(rp, rt)
            }
            ExprKind::Array(elems) => {
                let held: Vec<V> = elems
                    .iter()
                    .map(|a| {
                        let v = self.expr(a);
                        self.hold(v);
                        v
                    })
                    .collect();
                let vals: Vec<(Value, Value)> = held.iter().map(|v| v.any()).collect();
                let slot = self.values_slot(&vals);
                // tsaot_new_array takes ownership of the elements.
                for v in held.into_iter().rev() {
                    self.release(v);
                }
                let n = self.b.ins().iconst(I32, elems.len() as i64);
                let ctx = self.ctx;
                let r = self.helper("tsaot_new_array", &[ctx, n, slot]);
                self.check_value(r[1]);
                V::Any(r[0], r[1])
            }
            ExprKind::ArrayParts(parts) => {
                let ctx = self.ctx;
                let a = self.helper("JS_NewArray", &[ctx]);
                self.check_value(a[1]);
                let arr = V::Any(a[0], a[1]);
                self.hold(arr);
                for part in parts {
                    let r = match part {
                        ir::ArrPart::Value(x) => {
                            // Takes ownership of the value.
                            let (p, t) = self.expr(x).any();
                            self.helper("tsaot_array_push", &[ctx, a[0], a[1], p, t])
                        }
                        ir::ArrPart::Spread(x) => {
                            let v = self.expr(x);
                            self.hold(v);
                            let (p, t) = v.any();
                            let r = self.helper("tsaot_array_spread", &[ctx, a[0], a[1], p, t]);
                            self.release(v);
                            self.free_v(v);
                            r
                        }
                        ir::ArrPart::Hole => self.helper("tsaot_array_hole", &[ctx, a[0], a[1]]),
                    };
                    self.check_int(r[0]);
                }
                self.release(arr);
                arr
            }
            ExprKind::ObjectParts(parts) => {
                let ctx = self.ctx;
                let o = self.helper("JS_NewObject", &[ctx]);
                self.check_value(o[1]);
                let obj = V::Any(o[0], o[1]);
                self.hold(obj);
                for part in parts {
                    match part {
                        ir::ObjPart::Field(a, x) => {
                            let (p, t) = self.expr(x).any();
                            let slot = self.values_slot(&[(o[0], o[1])]);
                            let atom = self.atom(*a);
                            let r = self.helper("tsaot_define_field", &[ctx, slot, atom, p, t]);
                            self.check_int(r[0]);
                        }
                        ir::ObjPart::Computed(k, x) => {
                            // The key is converted before the value is evaluated.
                            let (kp, kt) = self.expr(k).any();
                            self.hold(V::Any(kp, kt));
                            let key = self.helper("JS_ToPropertyKey", &[ctx, kp, kt]);
                            self.check_value(key[1]);
                            self.release(V::Any(kp, kt));
                            self.free((kp, kt));
                            let key = V::Any(key[0], key[1]);
                            self.hold(key);
                            let (p, t) = self.expr(x).any();
                            let slot = self.values_slot(&[(o[0], o[1]), key.any()]);
                            let r = self.helper("tsaot_define_computed", &[ctx, slot, p, t]);
                            self.check_int(r[0]);
                            self.release(key);
                            self.free_v(key);
                        }
                        ir::ObjPart::Accessor(a, x, setter) => {
                            let (p, t) = self.expr(x).any();
                            let slot = self.values_slot(&[(o[0], o[1])]);
                            let atom = self.atom(*a);
                            let s = self.b.ins().iconst(I32, *setter as i64);
                            let r = self.helper("tsaot_define_accessor", &[ctx, slot, atom, p, t, s]);
                            self.check_int(r[0]);
                        }
                        ir::ObjPart::Spread(x) => {
                            let v = self.expr(x);
                            self.hold(v);
                            let (p, t) = v.any();
                            let r = self.helper("tsaot_object_spread", &[ctx, o[0], o[1], p, t]);
                            self.check_int(r[0]);
                            self.release(v);
                            self.free_v(v);
                        }
                    }
                }
                self.release(obj);
                obj
            }
            ExprKind::CallSpread(f, this, args) => {
                let fv = self.expr(f);
                self.hold(fv);
                let tv = self.expr(this);
                self.hold(tv);
                let av = self.expr(args);
                self.hold(av);
                let slot = self.values_slot(&[fv.any(), tv.any(), av.any()]);
                let ctx = self.ctx;
                let r = self.helper("tsaot_apply", &[ctx, slot]);
                self.check_value(r[1]);
                for v in [av, tv, fv] {
                    self.release(v);
                    self.free_v(v);
                }
                V::Any(r[0], r[1])
            }
            ExprKind::NewSpread(f, args) => {
                let fv = self.expr(f);
                self.hold(fv);
                let av = self.expr(args);
                self.hold(av);
                let ((fp, ft), (ap, at)) = (fv.any(), av.any());
                let ctx = self.ctx;
                let r = self.helper("tsaot_construct", &[ctx, fp, ft, ap, at]);
                self.check_value(r[1]);
                for v in [av, fv] {
                    self.release(v);
                    self.free_v(v);
                }
                V::Any(r[0], r[1])
            }
            ExprKind::IterOpen(x, iter, next, atom) => {
                self.iter_open(x, *iter, *next, *atom);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::ForInIterator(x) => self.convert(x, "JS_ForInIteratorObject"),
            ExprKind::ToObject(x) => self.convert(x, "JS_ToObject"),
            ExprKind::PrivateField(o, slot) => {
                let (obj, owned) = self.operand(o, &[]);
                let (op_, ot) = obj.any();
                let ic = self.new_ic();
                let join = self.b.create_block();
                let rp = self.b.append_block_param(join, I64);
                let rt = self.b.append_block_param(join, I64);
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                // A field of a cached shape (private fields are properties
                // keyed by their private symbol).
                let at = self.ic_hit(op_, ot, ic, slow);
                let vp = self.b.ins().load(I64, Self::mem(), at, 0);
                let vt = self.b.ins().load(I64, Self::mem(), at, TAG_OFFSET);
                self.dup((vp, vt));
                self.b.ins().jump(join, &[BlockArg::Value(vp), BlockArg::Value(vt)]);
                self.b.switch_to_block(slow);
                let (np, nt) = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let r = self.helper("tsaot_private_get_ic", &[ctx, op_, ot, np, nt, ic]);
                self.check_value(r[1]);
                self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
                self.b.switch_to_block(join);
                self.drop_operand(obj, owned);
                V::Any(rp, rt)
            }
            ExprKind::PrivateSetField(o, slot, x) => {
                let (obj, owned) = self.operand(o, &[x]);
                let v = self.expr(x);
                self.hold(v);
                let (vp, vt) = v.any();
                // One reference for the field (taken), one for the expression.
                self.dup((vp, vt));
                let (op_, ot) = obj.any();
                let ic = self.new_ic();
                let join = self.b.create_block();
                let slow = self.b.create_block();
                self.b.set_cold_block(slow);
                let at = self.ic_hit(op_, ot, ic, slow);
                let old_p = self.b.ins().load(I64, Self::mem(), at, 0);
                let old_t = self.b.ins().load(I64, Self::mem(), at, TAG_OFFSET);
                self.b.ins().store(Self::mem(), vp, at, 0);
                self.b.ins().store(Self::mem(), vt, at, TAG_OFFSET);
                self.free((old_p, old_t));
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(slow);
                let (np, nt) = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let slot = self.values_slot(&[(op_, ot), (np, nt)]);
                let r = self.helper("tsaot_private_set_ic", &[ctx, slot, vp, vt, ic]);
                self.check_int(r[0]);
                self.b.ins().jump(join, &[]);
                self.b.switch_to_block(join);
                self.release(v);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::PrivateMethod(o, slot) | ExprKind::PrivateGetter(o, slot) => {
                let (obj, owned) = self.operand(o, &[]);
                let (op_, ot) = obj.any();
                let (fp, ft) = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let r = self.helper("JS_PrivateCheckBrand", &[ctx, op_, ot, fp, ft]);
                self.check_int(r[0]);
                let v = if let ExprKind::PrivateGetter(..) = e.kind {
                    let slot = self.values_slot(&[(fp, ft), (op_, ot)]);
                    let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
                    let argc = self.b.ins().iconst(I32, 0);
                    let r = self.helper("tsaot_call", &[ctx, slot, argc, argv]);
                    self.check_value(r[1]);
                    V::Any(r[0], r[1])
                } else {
                    self.dup((fp, ft));
                    V::Any(fp, ft)
                };
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::PrivateSetter(o, slot, x) => {
                let (obj, owned) = self.operand(o, &[x]);
                let v = self.expr(x);
                self.hold(v);
                let (vp, vt) = v.any();
                let (op_, ot) = obj.any();
                let (fp, ft) = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let r = self.helper("JS_PrivateCheckBrand", &[ctx, op_, ot, fp, ft]);
                self.check_int(r[0]);
                let slot = self.values_slot(&[(fp, ft), (op_, ot), (vp, vt)]);
                let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
                let argc = self.b.ins().iconst(I32, 1);
                let r = self.helper("tsaot_call", &[ctx, slot, argc, argv]);
                self.check_value(r[1]);
                self.free((r[0], r[1]));
                self.release(v);
                self.drop_operand(obj, owned);
                v
            }
            ExprKind::PrivateIn(o, slot) => {
                let (obj, owned) = self.operand(o, &[]);
                let (op_, ot) = obj.any();
                let (np, nt) = self.table_value(self.env.privates, *slot);
                let ctx = self.ctx;
                let r = self.helper("JS_PrivateIn", &[ctx, op_, ot, np, nt]);
                self.check_int(r[0]);
                self.drop_operand(obj, owned);
                V::Bool(self.b.ins().icmp_imm_s(IntCC::NotEqual, r[0], 0))
            }
            ExprKind::SuperGetAtom(home, a) => {
                let (hp, ht) = self.table_value(self.env.homes, *home);
                let this = self.this.expect("`super` in a method");
                let (rp, rt) = self.prop_get_from(hp, ht, Some(this), *a);
                V::Any(rp, rt)
            }
            ExprKind::SuperGet(home, k) => {
                let key = self.expr(k);
                self.hold(key);
                let h = self.table_value(self.env.homes, *home);
                let this = self.this.expect("`super` in a method");
                let slot = self.values_slot(&[h, this, key.any()]);
                let ctx = self.ctx;
                let r = self.helper("tsaot_super_get", &[ctx, slot]);
                self.check_value(r[1]);
                self.release(key);
                self.free_v(key);
                V::Any(r[0], r[1])
            }
            ExprKind::SuperSet(home, k, x) => {
                let key = self.expr(k);
                self.hold(key);
                let v = self.expr(x);
                self.hold(v);
                let (vp, vt) = v.any();
                // One reference for the property (taken), one for the expression.
                self.dup((vp, vt));
                let h = self.table_value(self.env.homes, *home);
                let this = self.this.expect("`super` in a method");
                let slot = self.values_slot(&[h, this, key.any()]);
                let ctx = self.ctx;
                let r = self.helper("tsaot_super_set", &[ctx, slot, vp, vt]);
                self.check_int(r[0]);
                self.release(v);
                self.release(key);
                self.free_v(key);
                v
            }
            ExprKind::ForInGet(iter, o, k) => {
                let (obj, owned) = self.operand(o, &[k]);
                let (key, key_owned) = self.operand(k, &[]);
                let it = self.use_local(*iter).any();
                let slot = self.values_slot(&[it, obj.any(), key.any()]);
                let ctx = self.ctx;
                let r = self.helper("tsaot_for_in_get", &[ctx, slot]);
                self.check_value(r[1]);
                self.drop_operand(key, key_owned);
                self.drop_operand(obj, owned);
                V::Any(r[0], r[1])
            }
            ExprKind::ToPropKey(x) => self.convert(x, "JS_ToPropertyKey"),
            ExprKind::ArrayPush(arr, x) => {
                // Takes ownership of the value.
                let (p, t) = self.expr(x).any();
                let (ap, at) = self.use_local(*arr).any();
                let ctx = self.ctx;
                let r = self.helper("tsaot_array_push", &[ctx, ap, at, p, t]);
                self.check_int(r[0]);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::IterStep(iter, next) => self.iter_step(*iter, *next),
            ExprKind::IterClose(iter, next) => {
                self.close_iter(*iter, *next);
                self.konst(0, TAG_UNDEFINED)
            }
            ExprKind::ObjectRest(src, keys) => {
                let sv = self.expr(src);
                self.hold(sv);
                let mut held = vec![sv];
                for k in keys {
                    let v = self.expr(k);
                    self.hold(v);
                    held.push(v);
                }
                let vals: Vec<(Value, Value)> = held.iter().map(|v| v.any()).collect();
                let slot = self.values_slot(&vals);
                let n = self.b.ins().iconst(I32, keys.len() as i64);
                let ctx = self.ctx;
                let r = self.helper("tsaot_object_rest", &[ctx, slot, n]);
                self.check_value(r[1]);
                for v in held.into_iter().rev() {
                    self.release(v);
                    self.free_v(v);
                }
                V::Any(r[0], r[1])
            }
        }
    }

    /// Call `f` (held) with `this` (borrowed) and `args`; frees `f`.
    fn call_values(&mut self, f: V, this: V, args: &[Expr]) -> V {
        let held: Vec<V> = args
            .iter()
            .map(|a| {
                let v = self.expr(a);
                self.hold(v);
                v
            })
            .collect();
        let mut vals = vec![f.any(), this.any()];
        vals.extend(held.iter().map(|v| v.any()));
        let slot = self.values_slot(&vals);
        let argv = self.b.ins().iadd_imm_s(slot, 2 * SIZE as i64);
        let argc = self.b.ins().iconst(I32, args.len() as i64);
        let ctx = self.ctx;
        let (rp, rt) = self.call_linked(f.any(), this.any(), args.len(), argv, false, |l| {
            let r = l.helper("tsaot_call", &[ctx, slot, argc, argv]);
            (r[0], r[1])
        });
        self.check_value(rt);
        for v in held.into_iter().rev() {
            self.release(v);
            self.free_v(v);
        }
        self.release(f);
        self.free_v(f);
        V::Any(rp, rt)
    }

    /// Call `f` with `this` and `argc` arguments at `argv` (all borrowed;
    /// `f` held by the caller unless `borrowed`, then held here during the
    /// call). A native function (C function data with a fast entry) is
    /// entered directly through its entry, as linked calls in JSC; other
    /// callees, or a declining entry, take `generic`. A new reference.
    fn call_linked(
        &mut self,
        (fp, ft): (Value, Value),
        (thp, tht): (Value, Value),
        argc: usize,
        argv: Value,
        borrowed: bool,
        generic: impl FnOnce(&mut Self) -> (Value, Value),
    ) -> (Value, Value) {
        let join = self.b.create_block();
        let rp = self.b.append_block_param(join, I64);
        let rt = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        let is_obj = self.b.ins().icmp_imm_s(IntCC::Equal, ft, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let class = self.b.ins().uload16(I32, Self::mem(), fp, layout::OBJ_CLASS_ID);
        let is_data = self.b.ins().icmp_imm_s(IntCC::Equal, class, layout::CLASS_C_FUNCTION_DATA);
        let data = self.b.create_block();
        self.b.ins().brif(is_data, data, &[], slow, &[]);
        self.b.switch_to_block(data);
        let rec = self.b.ins().load(I64, Self::mem(), fp, layout::OBJ_FUNC_DATA);
        let entry = self.b.ins().load(I64, Self::mem(), rec, layout::FDATA_FAST);
        let has = self.b.create_block();
        self.b.ins().brif(entry, has, &[], slow, &[]);
        self.b.switch_to_block(has);
        // Entries skip the stack check of the engine's call path: so here.
        let limit_addr = self.b.ins().symbol_value(self.ptr, self.env.stack_limit);
        let limit = self.b.ins().load(self.ptr, Self::mem(), limit_addr, 0);
        let sp = self.b.ins().get_stack_pointer(self.ptr);
        let deep = self.b.ins().icmp(IntCC::UnsignedLessThan, sp, limit);
        let call = self.b.create_block();
        self.b.ins().brif(deep, slow, &[], call, &[]);
        self.b.switch_to_block(call);
        if borrowed {
            self.dup((fp, ft));
        }
        let fdata = self.b.ins().iadd_imm_s(rec, layout::FDATA_DATA as i64);
        let ctx = self.ctx;
        let n = self.b.ins().iconst(I32, argc as i64);
        let sig: SigRef = self.env.fast_sig;
        let inst = self.b.ins().call_indirect(sig, entry, &[ctx, thp, tht, n, argv, fdata]);
        let r = self.b.inst_results(inst).to_vec();
        if borrowed {
            self.free((fp, ft));
        }
        let declined = self.b.ins().icmp_imm_s(IntCC::Equal, r[1], TAG_UNINITIALIZED);
        self.b.ins().brif(declined, slow, &[], join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
        self.b.switch_to_block(slow);
        let (gp, gt) = generic(self);
        self.b.ins().jump(join, &[BlockArg::Value(gp), BlockArg::Value(gt)]);
        self.b.switch_to_block(join);
        (rp, rt)
    }

    /// Spill JS values into a fresh stack slot; returns its address.
    fn values_slot(&mut self, vals: &[(Value, Value)]) -> Value {
        let size = (vals.len().max(1) * SIZE) as u32;
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 4));
        for (i, &(p, t)) in vals.iter().enumerate() {
            // One 16-byte store per value: C code copies JSValues with
            // 16-byte loads, which two 8-byte stores could not forward to.
            let off = (i * SIZE) as i32;
            let v = self.b.ins().scalar_to_vector(I64X2, p);
            let v = self.b.ins().insertlane(v, t, 1);
            self.b.ins().stack_store(self.ptr, v, slot, off);
        }
        self.b.ins().stack_addr(self.ptr, slot, 0)
    }

    fn atom(&mut self, a: ir::AtomId) -> Value {
        let base = self.b.ins().symbol_value(self.ptr, self.env.atoms);
        self.b.ins().load(I32, Self::mem(), base, (a * 4) as i32)
    }

    /// A cell's value (borrowed); throws if it is uninitialized.
    /// A cell's value, borrowed, without a TDZ check.
    fn raw_cell(&mut self, k: ir::CellId) -> (Value, Value) {
        let base = self.b.ins().symbol_value(self.ptr, self.env.cells);
        let off = (k * SIZE) as i32;
        let p = self.b.ins().load(I64, Self::mem(), base, off);
        let t = self.b.ins().load(I64, Self::mem(), base, off + TAG_OFFSET);
        (p, t)
    }

    fn load_cell(&mut self, k: ir::CellId) -> (Value, Value) {
        let base = self.b.ins().symbol_value(self.ptr, self.env.cells);
        let off = (k * SIZE) as i32;
        let p = self.b.ins().load(I64, Self::mem(), base, off);
        let t = self.b.ins().load(I64, Self::mem(), base, off + TAG_OFFSET);
        let uninit = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_UNINITIALIZED);
        let throw = self.b.create_block();
        let cont = self.b.create_block();
        self.b.set_cold_block(throw);
        self.b.ins().brif(uninit, throw, &[], cont, &[]);
        self.b.switch_to_block(throw);
        let kv = self.b.ins().iconst(I32, k as i64);
        let ctx = self.ctx;
        self.helper("tsaot_throw_uninitialized", &[ctx, kv]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(cont);
        (p, t)
    }

    fn logical(&mut self, op: LogicalOp, ty: Ty, x: &Expr, y: &Expr) -> V {
        let a = self.expr(x);
        let truth = match (op, a) {
            (LogicalOp::Nullish, V::Any(_, t)) => {
                // "Truthy" here means: not nullish, keep the left value.
                let u = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_UNDEFINED);
                let n = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_NULL);
                let nullish = self.b.ins().bor(u, n);
                self.b.ins().bxor_imm_u(nullish, 1)
            }
            (_, V::Bool(b)) => b,
            (_, V::Num(d)) => self.truthy_num(d),
            (_, V::Any(p, t)) => self.truthy_any((p, t)),
            _ => unreachable!(),
        };
        let rhs = self.b.create_block();
        let join = self.b.create_block();
        for t in clif_types(ty) {
            self.b.append_block_param(join, t);
        }
        let keep: Vec<BlockArg> = self.abi(ty, a).into_iter().map(BlockArg::Value).collect();
        match op {
            LogicalOp::And => self.b.ins().brif(truth, rhs, &[], join, &keep),
            LogicalOp::Or | LogicalOp::Nullish => self.b.ins().brif(truth, join, &keep, rhs, &[]),
        };
        self.b.switch_to_block(rhs);
        self.free_v(a);
        let c = self.expr(y);
        let args: Vec<BlockArg> = self.abi(ty, c).into_iter().map(BlockArg::Value).collect();
        self.b.ins().jump(join, &args);
        self.b.switch_to_block(join);
        let params = self.b.block_params(join).to_vec();
        self.unabi(ty, &params)
    }

    // ---- dynamic values ----

    /// ToNumber of an operand (borrowed): inline for numbers, else
    /// `JS_ToFloat64`, which may throw.
    fn number_value(&mut self, v: V) -> Value {
        let V::Any(p, t) = v else { return v.num() };
        let join = self.b.create_block();
        let r = self.b.append_block_param(join, F64);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let num = self.is_number(t);
        self.b.ins().brif(num, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let d = self.number_of((p, t));
        self.b.ins().jump(join, &[BlockArg::Value(d)]);
        self.b.switch_to_block(slow);
        let out = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 8, 3));
        let addr = self.b.ins().stack_addr(self.ptr, out, 0);
        let ctx = self.ctx;
        let s = self.helper("JS_ToFloat64", &[ctx, addr, p, t]);
        self.check_int(s[0]);
        let d = self.b.ins().stack_load(self.ptr, F64, out, 0);
        self.b.ins().jump(join, &[BlockArg::Value(d)]);
        self.b.switch_to_block(join);
        r
    }

    fn is_number(&mut self, t: Value) -> Value {
        let i = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_INT);
        let f = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_FLOAT64);
        self.b.ins().bor(i, f)
    }

    /// The number in a value known to hold one.
    fn number_of(&mut self, (p, t): (Value, Value)) -> Value {
        let i = self.b.ins().ireduce(I32, p);
        let fi = self.b.ins().fcvt_from_sint(F64, i);
        let ff = self.b.ins().bitcast(F64, MemFlagsData::new(), p);
        let is_int = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_INT);
        self.b.ins().select(is_int, fi, ff)
    }

    fn truthy_num(&mut self, d: Value) -> Value {
        let z = self.b.ins().f64const(0.0);
        // Ordered: NaN is falsy.
        self.b.ins().fcmp(FloatCC::OrderedNotEqual, d, z)
    }

    /// JS truthiness of a borrowed value.
    fn truthy_any(&mut self, (p, t): (Value, Value)) -> Value {
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, I8);
        // int / bool: payload != 0
        let small = self.b.ins().icmp_imm_u(IntCC::UnsignedLessThanOrEqual, t, TAG_BOOL);
        let int_blk = self.b.create_block();
        let other = self.b.create_block();
        self.b.ins().brif(small, int_blk, &[], other, &[]);
        self.b.switch_to_block(int_blk);
        let i = self.b.ins().ireduce(I32, p);
        let r = self.b.ins().icmp_imm_s(IntCC::NotEqual, i, 0);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        // float
        self.b.switch_to_block(other);
        let is_f = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_FLOAT64);
        let f_blk = self.b.create_block();
        let other2 = self.b.create_block();
        self.b.ins().brif(is_f, f_blk, &[], other2, &[]);
        self.b.switch_to_block(f_blk);
        let d = self.b.ins().bitcast(F64, MemFlagsData::new(), p);
        let r = self.truthy_num(d);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        // null / undefined: false
        self.b.switch_to_block(other2);
        let u = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_UNDEFINED);
        let n = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_NULL);
        let nullish = self.b.ins().bor(u, n);
        let slow = self.b.create_block();
        let zero = self.b.ins().iconst(I8, 0);
        self.b.ins().brif(nullish, join, &[BlockArg::Value(zero)], slow, &[]);
        // everything else (strings, objects, ...)
        self.b.switch_to_block(slow);
        let ctx = self.ctx;
        let r = self.helper("JS_ToBool", &[ctx, p, t]);
        let r = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, r[0], 0);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        res
    }

    /// If all of `vals` are int32s, try `f` on them: it gives a result and an
    /// overflow flag. Without overflow the result, an int32, goes to `join`.
    /// Otherwise code continues in the (new) current block.
    fn int_fast_path(
        &mut self,
        vals: &[(Value, Value)],
        join: Block,
        f: impl FnOnce(&mut FunctionBuilder, &[Value]) -> Option<(Value, Value)>,
    ) {
        let mut all = None;
        for &(_, t) in vals {
            let is_int = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_INT);
            all = Some(match all {
                None => is_int,
                Some(a) => self.b.ins().band(a, is_int),
            });
        }
        let int_blk = self.b.create_block();
        let rest = self.b.create_block();
        self.b.ins().brif(all.expect("operands"), int_blk, &[], rest, &[]);
        self.b.switch_to_block(int_blk);
        let xs: Vec<Value> = vals.iter().map(|&(p, _)| self.b.ins().ireduce(I32, p)).collect();
        match f(&mut self.b, &xs) {
            Some((r, overflow)) => {
                let ok = self.b.create_block();
                self.b.ins().brif(overflow, rest, &[], ok, &[]);
                self.b.switch_to_block(ok);
                let p = self.b.ins().uextend(I64, r);
                let t = self.b.ins().iconst(I64, TAG_INT);
                self.b.ins().jump(join, &[BlockArg::Value(p), BlockArg::Value(t)]);
            }
            None => {
                self.b.ins().jump(rest, &[]);
            }
        }
        self.b.switch_to_block(rest);
    }

    /// `op a b` on owned values (consumed). Fast path for two numbers.
    fn binary_any(&mut self, op: AnyBinOp, a: (Value, Value), c: (Value, Value)) -> (Value, Value) {
        let na = self.is_number(a.1);
        let nc = self.is_number(c.1);
        let both = self.b.ins().band(na, nc);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        let rp = self.b.append_block_param(join, I64);
        let rt = self.b.append_block_param(join, I64);
        // Two int32s: integer ops, unless the result overflows.
        let int_op = |b: &mut FunctionBuilder, x: Value, y: Value| -> Option<(Value, Value)> {
            let no = b.ins().iconst(I8, 0);
            Some(match op {
                AnyBinOp::Add => b.ins().sadd_overflow(x, y),
                AnyBinOp::Sub => b.ins().ssub_overflow(x, y),
                AnyBinOp::BitAnd => (b.ins().band(x, y), no),
                AnyBinOp::BitOr => (b.ins().bor(x, y), no),
                AnyBinOp::BitXor => (b.ins().bxor(x, y), no),
                // The shift count is taken modulo 32, as in JS.
                AnyBinOp::Shl => (b.ins().ishl(x, y), no),
                AnyBinOp::Sar => (b.ins().sshr(x, y), no),
                _ => return None,
            })
        };
        if matches!(
            op,
            AnyBinOp::Add
                | AnyBinOp::Sub
                | AnyBinOp::BitAnd
                | AnyBinOp::BitOr
                | AnyBinOp::BitXor
                | AnyBinOp::Shl
                | AnyBinOp::Sar
        ) {
            self.int_fast_path(&[a, c], join, |b, xs| int_op(b, xs[0], xs[1]));
        }
        self.b.ins().brif(both, fast, &[], slow, &[]);

        self.b.switch_to_block(fast);
        let x = self.number_of(a);
        let y = self.number_of(c);
        let bop = match op {
            AnyBinOp::Add => BinOp::Add,
            AnyBinOp::Sub => BinOp::Sub,
            AnyBinOp::Mul => BinOp::Mul,
            AnyBinOp::Div => BinOp::Div,
            AnyBinOp::Rem => BinOp::Rem,
            AnyBinOp::Pow => BinOp::Pow,
            AnyBinOp::Shl => BinOp::Shl,
            AnyBinOp::Sar => BinOp::Sar,
            AnyBinOp::Shr => BinOp::Shr,
            AnyBinOp::BitAnd => BinOp::BitAnd,
            AnyBinOp::BitOr => BinOp::BitOr,
            AnyBinOp::BitXor => BinOp::BitXor,
        };
        let r = self.binary(bop, x, y);
        let (p, t) = self.box_number(r);
        self.b.ins().jump(join, &[BlockArg::Value(p), BlockArg::Value(t)]);

        self.b.switch_to_block(slow);
        self.b.set_cold_block(slow);
        let code = match op {
            AnyBinOp::Add => op::ADD,
            AnyBinOp::Sub => op::SUB,
            AnyBinOp::Mul => op::MUL,
            AnyBinOp::Div => op::DIV,
            AnyBinOp::Rem => op::MOD,
            AnyBinOp::Pow => op::POW,
            AnyBinOp::Shl => op::SHL,
            AnyBinOp::Sar => op::SAR,
            AnyBinOp::Shr => op::SHR,
            AnyBinOp::BitAnd => op::AND,
            AnyBinOp::BitOr => op::OR,
            AnyBinOp::BitXor => op::XOR,
        };
        let code = self.b.ins().iconst(I32, code);
        let ctx = self.ctx;
        let r = self.helper("JS_BinaryOperator", &[ctx, code, a.0, a.1, c.0, c.1]);
        self.check_value(r[1]);
        self.free(c);
        self.free(a);
        self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
        self.b.switch_to_block(join);
        (rp, rt)
    }

    /// A number from a number, or from a JS value known to be one.
    fn num_value(&mut self, v: V) -> Value {
        match v {
            V::Num(d) => d,
            other => self.number_of(other.any()),
        }
    }

    /// `x`, but a number for a boxed number.
    fn unboxed_operand(&mut self, x: &Expr) -> V {
        match &x.kind {
            ExprKind::Box(inner) if inner.ty == Ty::F64 => self.expr(inner),
            _ => self.expr(x),
        }
    }

    /// Comparison of borrowed values. Fast path for two numbers.
    fn compare_any(&mut self, op: AnyCmpOp, a: (Value, Value), c: (Value, Value)) -> Value {
        self.compare_mixed(op, V::Any(a.0, a.1), V::Any(c.0, c.1))
    }

    /// A comparison of JS values or numbers (borrowed).
    fn compare_mixed(&mut self, op: AnyCmpOp, a: V, c: V) -> Value {
        let code = match op {
            AnyCmpOp::Lt => op::LT,
            AnyCmpOp::Le => op::LTE,
            AnyCmpOp::Gt => op::GT,
            AnyCmpOp::Ge => op::GTE,
            AnyCmpOp::Eq => op::EQ,
            AnyCmpOp::Ne => op::NEQ,
            AnyCmpOp::StrictEq => op::STRICT_EQ,
            AnyCmpOp::StrictNe => op::STRICT_NEQ,
            AnyCmpOp::In => op::IN,
            AnyCmpOp::InstanceOf => op::INSTANCEOF,
        };
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, I8);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        if !matches!(op, AnyCmpOp::In | AnyCmpOp::InstanceOf) {
            let fast = self.b.create_block();
            match (a, c) {
                (V::Any(_, at), V::Any(_, ct)) => {
                    let na = self.is_number(at);
                    let nc = self.is_number(ct);
                    let both = self.b.ins().band(na, nc);
                    self.b.ins().brif(both, fast, &[], slow, &[]);
                }
                (V::Any(_, t), _) | (_, V::Any(_, t)) => {
                    let n = self.is_number(t);
                    self.b.ins().brif(n, fast, &[], slow, &[]);
                }
                _ => {
                    self.b.ins().jump(fast, &[]);
                }
            }
            self.b.switch_to_block(fast);
            let x = self.num_value(a);
            let y = self.num_value(c);
            let cc = match op {
                AnyCmpOp::Lt => FloatCC::LessThan,
                AnyCmpOp::Le => FloatCC::LessThanOrEqual,
                AnyCmpOp::Gt => FloatCC::GreaterThan,
                AnyCmpOp::Ge => FloatCC::GreaterThanOrEqual,
                AnyCmpOp::Eq | AnyCmpOp::StrictEq => FloatCC::Equal,
                _ => FloatCC::NotEqual,
            };
            let r = self.b.ins().fcmp(cc, x, y);
            self.b.ins().jump(join, &[BlockArg::Value(r)]);
        } else {
            self.b.ins().jump(slow, &[]);
        }
        self.b.switch_to_block(slow);
        let code = self.b.ins().iconst(I32, code);
        let ctx = self.ctx;
        let (ap, at) = self.boxed(a);
        let (cp, ct) = self.boxed(c);
        let r = self.helper("JS_BinaryOperator", &[ctx, code, ap, at, cp, ct]);
        self.check_value(r[1]);
        let r = self.b.ins().ireduce(I8, r[0]);
        let r = self.b.ins().icmp_imm_s(IntCC::NotEqual, r, 0);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        res
    }

    /// Unary operator on an owned value (consumed).
    fn unary_any(&mut self, op: AnyUnOp, v: (Value, Value)) -> (Value, Value) {
        let join = self.b.create_block();
        let rp = self.b.append_block_param(join, I64);
        let rt = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        match op {
            AnyUnOp::ToStr => {
                let is_str = self.b.ins().icmp_imm_s(IntCC::Equal, v.1, TAG_STRING);
                let same = [BlockArg::Value(v.0), BlockArg::Value(v.1)];
                self.b.ins().brif(is_str, join, &same, slow, &[]);
                self.b.switch_to_block(slow);
                let ctx = self.ctx;
                self.hold(V::Any(v.0, v.1));
                let r = self.helper("JS_ToString", &[ctx, v.0, v.1]);
                self.check_value(r[1]);
                self.release(V::Any(v.0, v.1));
                self.free(v);
                self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
                self.b.switch_to_block(join);
                return (rp, rt);
            }
            AnyUnOp::Typeof => {
                self.b.ins().jump(slow, &[]);
            }
            _ => {
                if matches!(op, AnyUnOp::Inc | AnyUnOp::Dec) {
                    // An int32 that does not overflow stays one.
                    let delta = if op == AnyUnOp::Inc { 1 } else { -1 };
                    self.int_fast_path(&[v], join, |b, xs| {
                        let one = b.ins().iconst(I32, delta);
                        Some(b.ins().sadd_overflow(xs[0], one))
                    });
                }
                let num = self.is_number(v.1);
                let fast = self.b.create_block();
                self.b.ins().brif(num, fast, &[], slow, &[]);
                self.b.switch_to_block(fast);
                let (p, t) = match op {
                    AnyUnOp::Plus | AnyUnOp::ToNumeric => v,
                    AnyUnOp::Neg | AnyUnOp::Inc | AnyUnOp::Dec | AnyUnOp::BitNot => {
                        let d = self.number_of(v);
                        let r = match op {
                            AnyUnOp::Neg => self.b.ins().fneg(d),
                            AnyUnOp::Inc | AnyUnOp::Dec => {
                                let one = self.b.ins().f64const(if op == AnyUnOp::Inc { 1.0 } else { -1.0 });
                                self.b.ins().fadd(d, one)
                            }
                            _ => {
                                let i = self.toint32(d);
                                let r = self.b.ins().bnot(i);
                                self.b.ins().fcvt_from_sint(F64, r)
                            }
                        };
                        self.box_number(r)
                    }
                    _ => unreachable!(),
                };
                self.b.ins().jump(join, &[BlockArg::Value(p), BlockArg::Value(t)]);
            }
        }
        self.b.switch_to_block(slow);
        let code = match op {
            AnyUnOp::Neg => op::NEG,
            AnyUnOp::Plus => op::PLUS,
            AnyUnOp::BitNot => op::BITNOT,
            AnyUnOp::Inc => op::INC,
            AnyUnOp::Dec => op::DEC,
            AnyUnOp::ToNumeric => op::TONUMERIC,
            AnyUnOp::Typeof => op::TYPEOF,
            AnyUnOp::ToStr => unreachable!(),
        };
        let code = self.b.ins().iconst(I32, code);
        let ctx = self.ctx;
        self.hold(V::Any(v.0, v.1));
        let r = self.helper("JS_UnaryOperator", &[ctx, code, v.0, v.1]);
        self.check_value(r[1]);
        self.release(V::Any(v.0, v.1));
        self.free(v);
        self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
        self.b.switch_to_block(join);
        (rp, rt)
    }

    /// Box a `MaybeF64`: its reserved NaN is `undefined`.
    fn box_maybe(&mut self, d: Value) -> (Value, Value) {
        let (p, t) = self.box_number(d);
        let u = self.b.ins().bitcast(I64, MemFlagsData::new(), d);
        let undef = self.b.ins().icmp_imm_s(IntCC::Equal, u, MAYBE_F64_UNDEFINED as i64);
        let zero = self.b.ins().iconst(I64, 0);
        let undef_tag = self.b.ins().iconst(I64, TAG_UNDEFINED);
        let p = self.b.ins().select(undef, zero, p);
        let t = self.b.ins().select(undef, undef_tag, t);
        (p, t)
    }

    /// Boxes a number like the interpreter's `js_number()`: int32-valued
    /// doubles (except -0) become `JS_TAG_INT`, everything else
    /// `JS_TAG_FLOAT64`.
    ///
    /// The int32 test is QuickJS's `double_is_int32`, done on the bit pattern
    /// with integer ops only. The obvious test (convert to int and back, then
    /// compare) is a ~20-cycle chain of dependent float conversions that
    /// sits right on the latency path back into the interpreter; here the
    /// conversion that produces the int payload runs in parallel instead.
    /// Returns `(payload, tag)`.
    pub(crate) fn box_number(&mut self, d: Value) -> (Value, Value) {
        if let Some(&i) = self.int_of.get(&d) {
            let i32v = self.b.ins().ireduce(I32, i);
            let back = self.b.ins().sextend(I64, i32v);
            let is_int = self.b.ins().icmp(IntCC::Equal, back, i);
            let int_payload = self.b.ins().uextend(I64, i32v);
            let u = self.b.ins().bitcast(I64, MemFlagsData::new(), d);
            let payload = self.b.ins().select(is_int, int_payload, u);
            let int_tag = self.b.ins().iconst(I64, TAG_INT);
            let float_tag = self.b.ins().iconst(I64, TAG_FLOAT64);
            return (payload, self.b.ins().select(is_int, int_tag, float_tag));
        }
        let b = &mut self.b;
        let u = b.ins().bitcast(I64, MemFlagsData::new(), d);
        // Unbiased exponent; negative exponents wrap to huge unsigned values.
        let e = b.ins().ushr_imm_u(u, 52);
        let e = b.ins().band_imm_u(e, 0x7ff);
        let e = b.ins().iadd_imm_s(e, -1023);
        // |d| in [1, 2^31): integral iff no mantissa bits below the binary point.
        let small = b.ins().icmp_imm_u(IntCC::UnsignedLessThanOrEqual, e, 30);
        let frac = b.ins().ishl_imm_u(u, 12);
        let frac = b.ins().ishl(frac, e);
        let small_int = b.ins().icmp_imm_s(IntCC::Equal, frac, 0);
        // Otherwise only +0 and INT32_MIN are int32 (this rejects -0).
        let is_zero = b.ins().icmp_imm_s(IntCC::Equal, u, 0);
        let is_min = b.ins().icmp_imm_s(IntCC::Equal, u, 0xc1e0_0000_0000_0000_u64 as i64);
        let big_int = b.ins().bor(is_zero, is_min);
        let is_int = b.ins().select(small, small_int, big_int);
        let i = b.ins().fcvt_to_sint_sat(I32, d);
        let int_payload = b.ins().uextend(I64, i);
        let payload = b.ins().select(is_int, int_payload, u);
        let int_tag = b.ins().iconst(I64, TAG_INT);
        let float_tag = b.ins().iconst(I64, TAG_FLOAT64);
        (payload, b.ins().select(is_int, int_tag, float_tag))
    }

    // ---- numbers ----

    fn call_helper(&mut self, name: &str, args: &[Value]) -> Value {
        self.helper(name, args)[0]
    }

    fn binary(&mut self, op: BinOp, a: Value, c: Value) -> Value {
        let ints = (self.i32s.contains(&a) && self.i32s.contains(&c)).then(|| (self.int_of[&a], self.int_of[&c]));
        match (op, ints) {
            // Exact: |a + b| <= 2^32.
            (BinOp::Add, Some((x, y))) => {
                let r = self.b.ins().iadd(x, y);
                return self.int_f64(r);
            }
            (BinOp::Sub, Some((x, y))) => {
                let r = self.b.ins().isub(x, y);
                return self.int_f64(r);
            }
            // The `f64` product is the exact one rounded (past 2^53).
            (BinOp::Mul, Some((x, y))) => {
                let p = self.b.ins().imul(x, y);
                let zero = self.b.ins().icmp_imm_s(IntCC::Equal, p, 0);
                let neg = self.b.ins().bxor(x, y);
                let neg = self.b.ins().icmp_imm_s(IntCC::SignedLessThan, neg, 0);
                let negzero = self.b.ins().band(zero, neg);
                let d = self.b.ins().fcvt_from_sint(F64, p);
                // `-3 * 0` is -0.
                let mz = self.b.ins().f64const(-0.0);
                let d = self.b.ins().select(negzero, mz, d);
                self.wide_of.insert(d, p);
                return d;
            }
            (BinOp::Rem, Some((x, y))) => return self.int_rem(x, y, a),
            _ => {}
        }
        match op {
            BinOp::Add => self.b.ins().fadd(a, c),
            BinOp::Sub => self.b.ins().fsub(a, c),
            BinOp::Mul => self.b.ins().fmul(a, c),
            BinOp::Div => self.b.ins().fdiv(a, c),
            BinOp::Rem => self.rem(a, c),
            BinOp::Pow => self.call_helper("tsaot_pow", &[a, c]),
            BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor | BinOp::Shl | BinOp::Sar | BinOp::Shr => {
                let x = self.toint32(a);
                let y = self.toint32(c);
                // Cranelift shifts take the amount modulo 32, as JS does.
                let r = match op {
                    BinOp::BitAnd => self.b.ins().band(x, y),
                    BinOp::BitOr => self.b.ins().bor(x, y),
                    BinOp::BitXor => self.b.ins().bxor(x, y),
                    BinOp::Shl => self.b.ins().ishl(x, y),
                    BinOp::Sar => self.b.ins().sshr(x, y),
                    BinOp::Shr => {
                        let r = self.b.ins().ushr(x, y);
                        let r = self.b.ins().uextend(I64, r);
                        return self.int_f64(r);
                    }
                    _ => unreachable!(),
                };
                let r = self.b.ins().sextend(I64, r);
                let d = self.int_f64(r);
                self.i32s.insert(d);
                d
            }
        }
    }

    /// JS `%`. When both operands are integer-valued (and the divisor is not 0
    /// or -1) an integer remainder is exact; `copysign` restores JS's rule
    /// that the result has the dividend's sign, including `-4 % 2 === -0`.
    /// Everything else uses C `fmod`, which matches JS exactly.
    fn rem(&mut self, a: Value, c: Value) -> Value {
        let ai = self.b.ins().fcvt_to_sint_sat(I64, a);
        let ci = self.b.ins().fcvt_to_sint_sat(I64, c);
        let af = self.b.ins().fcvt_from_sint(F64, ai);
        let cf = self.b.ins().fcvt_from_sint(F64, ci);
        let a_int = self.b.ins().fcmp(FloatCC::Equal, af, a);
        let c_int = self.b.ins().fcmp(FloatCC::Equal, cf, c);
        let c_abs = self.b.ins().iabs(ci);
        // |c| > 1 excludes 0 (NaN result) and -1 (srem overflow on INT64_MIN).
        let c_ok = self.b.ins().icmp_imm_s(IntCC::SignedGreaterThan, c_abs, 1);
        // Values >= 2^63 saturate to i64::MAX, which converts back to 2^63
        // and would pass the round-trip check.
        let a_unsat = self.b.ins().icmp_imm_s(IntCC::NotEqual, ai, i64::MAX);
        let c_unsat = self.b.ins().icmp_imm_s(IntCC::NotEqual, ci, i64::MAX);
        let ok = self.b.ins().band(a_int, c_int);
        let ok = self.b.ins().band(ok, c_ok);
        let unsat = self.b.ins().band(a_unsat, c_unsat);
        let ok = self.b.ins().band(ok, unsat);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, F64);
        self.b.ins().brif(ok, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let r = self.b.ins().srem(ai, ci);
        let rf = self.b.ins().fcvt_from_sint(F64, r);
        let rf = self.b.ins().fcopysign(rf, a);
        self.b.ins().jump(join, &[BlockArg::Value(rf)]);
        self.b.switch_to_block(slow);
        self.b.set_cold_block(slow);
        let r = self.call_helper("fmod", &[a, c]);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        res
    }

    /// An object operand, read while the `later` operands are evaluated. A
    /// plain JS-valued local is borrowed instead of referenced when none of
    /// them assigns it (nothing else can: it is not captured). Returns the
    /// value and whether it is owned (then also held).
    fn operand(&mut self, e: &Expr, later: &[&Expr]) -> (V, bool) {
        if let ExprKind::This = e.kind {
            let (p, t) = self.this.expect("`this` in a method");
            return (V::Any(p, t), false);
        }
        if let ExprKind::Local(l) = e.kind
            && let LVar::Two(..) = self.vars[l]
            && !later.iter().any(|x| ir::assigns_local(x, l))
        {
            return (self.use_local(l), false);
        }
        let v = self.expr(e);
        self.hold(v);
        (v, true)
    }

    /// Done with an `operand`.
    fn drop_operand(&mut self, v: V, owned: bool) {
        if owned {
            self.release(v);
            self.free_v(v);
        }
    }

    // ---- iteration ----

    /// `helper(ctx, x)` on an owned value, which it borrows: a new value.
    fn convert(&mut self, x: &Expr, helper: &str) -> V {
        let v = self.expr(x);
        self.hold(v);
        let (p, t) = v.any();
        let ctx = self.ctx;
        let r = self.helper(helper, &[ctx, p, t]);
        self.check_value(r[1]);
        self.release(v);
        self.free_v(v);
        V::Any(r[0], r[1])
    }

    /// See `ExprKind::IterOpen`.
    fn iter_open(&mut self, x: &Expr, iter: ir::LocalId, next: ir::LocalId, atom: ir::AtomId) {
        let (p, t) = self.expr(x).any();
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        // An array with the shape of a fresh one: iterated directly.
        let is_obj = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let kind = self.b.ins().iconst(I32, TYPED_ARRAYS);
        let pristine = self.pristine_shape(p, kind);
        let check = self.b.create_block();
        self.b.ins().brif(pristine, check, &[], slow, &[]);
        // ...while array iteration is the original one.
        self.b.switch_to_block(check);
        let ctx = self.ctx;
        let ok = self.helper("tsaot_array_iter_ok", &[ctx]);
        self.b.ins().brif(ok[0], fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        self.set_local(iter, V::Any(p, t));
        let zero = self.konst(0, TAG_INT);
        self.set_local(next, zero);
        self.b.ins().jump(join, &[]);
        // Anything else: the iterator protocol.
        self.b.switch_to_block(slow);
        let v = V::Any(p, t);
        self.hold(v);
        let sync = self.b.ins().iconst(I32, 0);
        let it = self.helper("JS_GetIteratorObject", &[ctx, p, t, sync]);
        self.check_value(it[1]);
        self.release(v);
        self.free_v(v);
        self.set_local(iter, V::Any(it[0], it[1]));
        let a = self.atom(atom);
        let m = self.helper("JS_GetProperty", &[ctx, it[0], it[1], a]);
        self.check_value(m[1]);
        self.set_local(next, V::Any(m[0], m[1]));
        // A native generator with the original `next`: resumed directly
        // (`next` becomes `true`).
        let ok = self.helper("tsaot_gen_iter_ok", &[ctx, it[0], it[1], m[0], m[1]]);
        let generator = self.b.create_block();
        self.b.ins().brif(ok[0], generator, &[], join, &[]);
        self.b.switch_to_block(generator);
        let one = self.b.ins().iconst(I64, 1);
        let bool_tag = self.b.ins().iconst(I64, TAG_BOOL);
        self.set_local(next, V::Any(one, bool_tag));
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
    }

    /// See `ExprKind::IterStep`.
    fn iter_step(&mut self, iter: ir::LocalId, next: Option<ir::LocalId>) -> V {
        let (ip, it) = self.use_local(iter).any();
        let ctx = self.ctx;
        let join = self.b.create_block();
        let rp = self.b.append_block_param(join, I64);
        let rt = self.b.append_block_param(join, I64);
        let finished = self.b.create_block();
        let failed = self.b.create_block();
        self.b.set_cold_block(failed);
        // Done already: no more calls, `undefined`.
        let live = self.b.ins().icmp_imm_s(IntCC::NotEqual, it, TAG_UNDEFINED);
        let step = self.b.create_block();
        let u = self.konst(0, TAG_UNDEFINED);
        let (up, ut) = u.any();
        self.b.ins().brif(live, step, &[], join, &[BlockArg::Value(up), BlockArg::Value(ut)]);
        self.b.switch_to_block(step);
        let r = match next {
            Some(n) => {
                let (np, nt) = self.use_local(n).any();
                let direct = self.b.ins().icmp_imm_s(IntCC::Equal, nt, TAG_INT);
                let array = self.b.create_block();
                let protocol = self.b.create_block();
                self.b.ins().brif(direct, array, &[], protocol, &[]);
                // An array iterated directly: index `next` until its length
                // (read every time, as the array iterator does).
                self.b.switch_to_block(array);
                let idx = self.b.ins().ireduce(I32, np);
                let idx = self.b.ins().uextend(I64, idx);
                let len = self.array_length(ip);
                let more = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, len);
                let get = self.b.create_block();
                self.b.ins().brif(more, get, &[], finished, &[]);
                self.b.switch_to_block(get);
                let v = self.array_element(ip, it, idx);
                let exc = self.b.ins().icmp_imm_s(IntCC::Equal, v.1, TAG_EXCEPTION);
                let got = self.b.create_block();
                self.b.ins().brif(exc, failed, &[], got, &[]);
                self.b.switch_to_block(got);
                let n1 = self.b.ins().iadd_imm_s(idx, 1);
                let n1 = self.b.ins().ireduce(I32, n1);
                let n1 = self.b.ins().uextend(I64, n1);
                let int = self.b.ins().iconst(I64, TAG_INT);
                self.def_local(n, V::Any(n1, int));
                self.b.ins().jump(join, &[BlockArg::Value(v.0), BlockArg::Value(v.1)]);
                // A native generator resumed directly, or the iterator protocol.
                self.b.switch_to_block(protocol);
                let done_addr = self.done_slot();
                let resume = self.b.create_block();
                let call = self.b.create_block();
                let after = self.b.create_block();
                let rp2 = self.b.append_block_param(after, I64);
                let rt2 = self.b.append_block_param(after, I64);
                let is_gen = self.b.ins().icmp_imm_s(IntCC::Equal, nt, TAG_BOOL);
                self.b.ins().brif(is_gen, resume, &[], call, &[]);
                self.b.switch_to_block(resume);
                let r = self.helper("JS_NativeGeneratorNext", &[ctx, ip, it, done_addr]);
                self.b.ins().jump(after, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
                self.b.switch_to_block(call);
                let zero = self.b.ins().iconst(I32, 0);
                let null = self.b.ins().iconst(self.ptr, 0);
                let r = self.helper("JS_IteratorStep", &[ctx, ip, it, np, nt, zero, null, done_addr]);
                self.b.ins().jump(after, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
                self.b.switch_to_block(after);
                (vec![rp2, rt2], done_addr)
            }
            None => {
                let done_addr = self.done_slot();
                (self.helper("JS_ForInStep", &[ctx, ip, it, done_addr]), done_addr)
            }
        };
        let (r, done_addr) = r;
        let exc = self.b.ins().icmp_imm_s(IntCC::Equal, r[1], TAG_EXCEPTION);
        let ok = self.b.create_block();
        self.b.ins().brif(exc, failed, &[], ok, &[]);
        self.b.switch_to_block(ok);
        let done = self.b.ins().load(I32, Self::mem(), done_addr, 0);
        self.b.ins().brif(done, finished, &[], join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
        // Done, or thrown: the iterator is not closed after either.
        self.b.switch_to_block(finished);
        let u = self.konst(0, TAG_UNDEFINED);
        self.set_local(iter, u);
        let (up, ut) = u.any();
        self.b.ins().jump(join, &[BlockArg::Value(up), BlockArg::Value(ut)]);
        self.b.switch_to_block(failed);
        let u = self.konst(0, TAG_UNDEFINED);
        self.set_local(iter, u);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(join);
        V::Any(rp, rt)
    }

    /// A stack slot for an `int` out-parameter; its address.
    fn done_slot(&mut self) -> Value {
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, 4, 2));
        self.b.ins().stack_addr(self.ptr, slot, 0)
    }

    /// The length of array `p` (its first property), as a `u32` in an `I64`.
    fn array_length(&mut self, p: Value) -> Value {
        let prop = self.b.ins().load(I64, Self::mem(), p, layout::OBJ_PROP);
        let lp = self.b.ins().load(I64, Self::mem(), prop, 0);
        let lt = self.b.ins().load(I64, Self::mem(), prop, TAG_OFFSET);
        let is_int = self.b.ins().icmp_imm_s(IntCC::Equal, lt, TAG_INT);
        let i = self.b.ins().ireduce(I32, lp);
        let i = self.b.ins().uextend(I64, i);
        let d = self.b.ins().bitcast(F64, MemFlagsData::new(), lp);
        let f = self.b.ins().fcvt_to_uint_sat(I64, d);
        self.b.ins().select(is_int, i, f)
    }

    /// Element `idx` (an `I64` below 2^32) of array `(p, t)`: a new value,
    /// or `JS_EXCEPTION`.
    fn array_element(&mut self, p: Value, t: Value, idx: Value) -> (Value, Value) {
        let join = self.b.create_block();
        let rp = self.b.append_block_param(join, I64);
        let rt = self.b.append_block_param(join, I64);
        let count = self.b.ins().uload32(Self::mem(), p, layout::OBJ_ARRAY_COUNT);
        let inb = self.b.ins().icmp(IntCC::UnsignedLessThan, idx, count);
        let hit = self.b.create_block();
        let slow = self.b.create_block();
        self.b.ins().brif(inb, hit, &[], slow, &[]);
        self.b.switch_to_block(hit);
        let at = self.array_slot(p, idx);
        let vp = self.b.ins().load(I64, Self::mem(), at, 0);
        let vt = self.b.ins().load(I64, Self::mem(), at, TAG_OFFSET);
        self.dup((vp, vt));
        self.b.ins().jump(join, &[BlockArg::Value(vp), BlockArg::Value(vt)]);
        // Holes and slow arrays: a property read.
        self.b.switch_to_block(slow);
        let d = self.b.ins().fcvt_from_uint(F64, idx);
        let (kp, kt) = self.box_number(d);
        let ctx = self.ctx;
        let r = self.helper("JS_GetPropertyValue", &[ctx, p, t, kp, kt]);
        self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
        self.b.switch_to_block(join);
        (rp, rt)
    }

    /// Close the iteration in `iter` unless it is done, leaving `undefined`
    /// (`pending`: with an exception pending, which closing keeps; errors of
    /// the closing itself are then ignored). Returns the closing's result.
    fn close_raw(&mut self, iter: ir::LocalId, next: ir::LocalId, pending: bool) -> Value {
        let (p, t) = self.use_local(iter).any();
        let (np, nt) = self.use_local(next).any();
        let join = self.b.create_block();
        let r = self.b.append_block_param(join, I32);
        let zero = self.b.ins().iconst(I32, 0);
        let live = self.b.ins().icmp_imm_s(IntCC::NotEqual, t, TAG_UNDEFINED);
        let close = self.b.create_block();
        self.b.ins().brif(live, close, &[], join, &[BlockArg::Value(zero)]);
        self.b.switch_to_block(close);
        let ctx = self.ctx;
        let pending = self.b.ins().iconst(I32, pending as i64);
        let direct = self.b.ins().icmp_imm_s(IntCC::Equal, nt, TAG_INT);
        let array = self.b.create_block();
        let protocol = self.b.create_block();
        let closed = self.b.create_block();
        let res = self.b.append_block_param(closed, I32);
        self.b.ins().brif(direct, array, &[], protocol, &[]);
        self.b.switch_to_block(array);
        let idx = self.b.ins().ireduce(I32, np);
        let a = self.helper("tsaot_array_iter_close", &[ctx, p, t, idx, pending]);
        self.b.ins().jump(closed, &[BlockArg::Value(a[0])]);
        self.b.switch_to_block(protocol);
        let c = self.helper("JS_IteratorCloseCompletion", &[ctx, p, t, pending]);
        self.b.ins().jump(closed, &[BlockArg::Value(c[0])]);
        self.b.switch_to_block(closed);
        let u = self.konst(0, TAG_UNDEFINED);
        self.set_local(iter, u);
        self.b.ins().jump(join, &[BlockArg::Value(res)]);
        self.b.switch_to_block(join);
        r
    }

    /// Close the iteration (normal completion) unless it is done.
    fn close_iter(&mut self, iter: ir::LocalId, next: ir::LocalId) {
        let r = self.close_raw(iter, next, false);
        self.check_int(r);
    }

    /// `IterGuard`: `body`, closing the iterator when it completes abruptly.
    fn iter_guard(&mut self, iter: ir::LocalId, next: ir::LocalId, loop_id: Option<ir::LoopId>, body: &[Stmt]) {
        let handler = self.b.create_block();
        self.b.set_cold_block(handler);
        let outer = self.handlers.last().copied().unwrap_or(self.exc_exit);
        let (handlers, scopes) = (self.handlers.len(), self.scopes.len());
        self.scopes.push(Scope::CloseIter { iter, next, loop_id, handlers, scopes });
        self.handlers.push(handler);
        self.stmts(body);
        self.handlers.pop();
        self.scopes.pop();
        let after = self.b.create_block();
        if !self.is_filled() {
            self.b.ins().jump(after, &[]);
        }
        // An exception: close with it pending, then propagate it.
        self.b.switch_to_block(handler);
        self.clear_flag();
        self.close_raw(iter, next, true);
        self.b.ins().jump(outer, &[]);
        self.b.switch_to_block(after);
    }

    // ---- inline caches ----

    /// `o.k = o.k + y` / `o.k - y` / `o.k++` / `o.k--` (`x` is the value
    /// assigned to `o.k`), with `o` a plain local or `this` and `y` a cheap
    /// number: updated in place by `prop_update`.
    fn prop_step<'e>(&self, o: &Expr, a: ir::AtomId, x: &'e Expr) -> Option<PropStep<'e>> {
        let plain = match o.kind {
            ExprKind::Local(l) => matches!(self.vars[l], LVar::Two(..)),
            ExprKind::This => true,
            _ => false,
        };
        let same = |g: &Expr| match &g.kind {
            ExprKind::GetProp(o2, a2) => {
                *a2 == a
                    && std::mem::discriminant(&o.kind) == std::mem::discriminant(&o2.kind)
                    && match (&o.kind, &o2.kind) {
                        (ExprKind::Local(l), ExprKind::Local(m)) => l == m,
                        _ => true,
                    }
            }
            _ => false,
        };
        match &x.kind {
            _ if !plain => None,
            ExprKind::AnyUnary(op @ (AnyUnOp::Inc | AnyUnOp::Dec), g) if same(g) => Some(PropStep::Unary(*op)),
            ExprKind::AnyBinary(op @ (AnyBinOp::Add | AnyBinOp::Sub), g, y) if same(g) => match &y.kind {
                ExprKind::Box(n) if n.ty == Ty::F64 && is_cheap(n) => Some(PropStep::Binary(*op, y, n)),
                _ => None,
            },
            _ => None,
        }
    }

    /// A `prop_step` update. An int or double data property of a cached
    /// shape is updated in place (an int result that fits stays an int);
    /// everything else goes through the engine. Returns the new value if
    /// `want`ed, else `V::Void`.
    fn prop_update(&mut self, o: &Expr, a: ir::AtomId, step: PropStep, want: bool) -> V {
        let (p, t) = match o.kind {
            ExprKind::Local(l) => self.use_local(l).any(),
            _ => self.this.expect("`this` in a method"),
        };
        // Filled by the slow path's set: an own writable data property.
        let ic = self.new_ic();
        let get_ic = self.new_ic();
        let join = self.b.create_block();
        let (rp, rt) = if want {
            (self.b.append_block_param(join, I64), self.b.append_block_param(join, I64))
        } else {
            let u = self.b.ins().iconst(I64, 0);
            (u, u)
        };
        let result = |np: Value, nt: Value| if want { vec![BlockArg::Value(np), BlockArg::Value(nt)] } else { vec![] };
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let num = self.b.create_block();
        let old_d = self.b.append_block_param(num, F64);

        let at = self.ic_hit(p, t, ic, slow);
        let vp = self.b.ins().load(I64, Self::mem(), at, 0);
        let vt = self.b.ins().load(I64, Self::mem(), at, TAG_OFFSET);
        // The step, as a double and (when it is one) as an integer.
        let (cd, ci) = match step {
            PropStep::Unary(op) => {
                let c = if op == AnyUnOp::Inc { 1 } else { -1 };
                (self.b.ins().f64const(c as f64), Some(self.b.ins().iconst(I64, c)))
            }
            PropStep::Binary(op, _, n) => {
                let d = self.expr(n).num();
                let i = self.int_of.get(&d).copied();
                if op == AnyBinOp::Sub { (self.b.ins().fneg(d), i.map(|i| self.b.ins().ineg(i))) } else { (d, i) }
            }
        };
        let is_int = self.b.ins().icmp_imm_s(IntCC::Equal, vt, TAG_INT);
        let int_b = self.b.create_block();
        let not_int = self.b.create_block();
        self.b.ins().brif(is_int, int_b, &[], not_int, &[]);

        self.b.switch_to_block(int_b);
        let old_i = self.b.ins().ireduce(I32, vp);
        let old_i = self.b.ins().sextend(I64, old_i);
        // An int result that fits is stored as is; otherwise compute in double.
        if let Some(ci) = ci {
            let r = self.b.ins().iadd(old_i, ci);
            let r32 = self.b.ins().ireduce(I32, r);
            let back = self.b.ins().sextend(I64, r32);
            let fits = self.b.ins().icmp(IntCC::Equal, back, r);
            let store = self.b.create_block();
            let wide = self.b.create_block();
            self.b.ins().brif(fits, store, &[], wide, &[]);
            self.b.switch_to_block(store);
            let np = self.b.ins().uextend(I64, r32);
            self.b.ins().store(Self::mem(), np, at, 0);
            let it = self.b.ins().iconst(I64, TAG_INT);
            self.b.ins().jump(join, &result(np, it));
            self.b.switch_to_block(wide);
        }
        let as_d = self.b.ins().fcvt_from_sint(F64, old_i);
        self.b.ins().jump(num, &[BlockArg::Value(as_d)]);

        self.b.switch_to_block(not_int);
        let is_f = self.b.ins().icmp_imm_s(IntCC::Equal, vt, TAG_FLOAT64);
        let fd = self.b.ins().bitcast(F64, MemFlagsData::new(), vp);
        self.b.ins().brif(is_f, num, &[BlockArg::Value(fd)], slow, &[]);

        // Numbers: the old value is not a reference, nothing to free.
        self.b.switch_to_block(num);
        let nd = self.b.ins().fadd(old_d, cd);
        let (np, nt) = self.box_number(nd);
        self.b.ins().store(Self::mem(), np, at, 0);
        self.b.ins().store(Self::mem(), nt, at, TAG_OFFSET);
        self.b.ins().jump(join, &result(np, nt));

        self.b.switch_to_block(slow);
        let atom = self.atom(a);
        let ctx = self.ctx;
        let r = self.helper("tsaot_ic_get", &[ctx, p, t, atom, get_ic]);
        self.check_value(r[1]);
        let (np, nt) = match step {
            PropStep::Unary(op) => self.unary_any(op, (r[0], r[1])),
            PropStep::Binary(op, y, _) => {
                let g = V::Any(r[0], r[1]);
                self.hold(g);
                let c = self.expr(y);
                self.hold(c);
                let v = self.binary_any(op, (r[0], r[1]), c.any());
                self.release(c);
                self.release(g);
                v
            }
        };
        let v = V::Any(np, nt);
        if want {
            self.hold(v);
            // One reference for the property, one for the expression.
            self.dup((np, nt));
        }
        let atom = self.atom(a);
        let r = self.helper("tsaot_ic_set", &[ctx, p, t, atom, np, nt, ic]);
        self.check_int(r[0]);
        if want {
            self.release(v);
        }
        self.b.ins().jump(join, &result(np, nt));

        self.b.switch_to_block(join);
        if want { V::Any(rp, rt) } else { V::Void }
    }

    /// `obj.a` (borrowed object) through a new inline cache: an own data
    /// property, or a data property (a method) or getter of the direct
    /// prototype. A new reference.
    fn prop_get(&mut self, p: Value, t: Value, a: ir::AtomId) -> (Value, Value) {
        self.prop_get_from(p, t, None, a)
    }

    /// `prop_get`, or with a receiver: `super.a` (`p`, `t`: the home object;
    /// only its prototypes are searched, and getters get the receiver).
    fn prop_get_from(&mut self, p: Value, t: Value, receiver: Option<(Value, Value)>, a: ir::AtomId) -> (Value, Value) {
        let ic = self.new_ic();
        let join = self.b.create_block();
        let rp = self.b.append_block_param(join, I64);
        let rt = self.b.append_block_param(join, I64);
        let slow = self.b.create_block();
        self.b.set_cold_block(slow);
        let (shape, kind) = self.ic_shape_hit(p, t, ic, slow);
        let own = self.b.create_block();
        let proto = self.b.create_block();
        let is_own = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 0);
        self.b.ins().brif(is_own, own, &[], proto, &[]);
        self.b.switch_to_block(own);
        if receiver.is_some() {
            // (Never filled for `super`.)
            self.b.ins().jump(slow, &[]);
            let dead = self.b.create_block();
            self.b.switch_to_block(dead);
        }
        let at = self.ic_slot_addr(p, ic);
        let vp = self.b.ins().load(I64, Self::mem(), at, 0);
        let vt = self.b.ins().load(I64, Self::mem(), at, TAG_OFFSET);
        self.dup((vp, vt));
        self.b.ins().jump(join, &[BlockArg::Value(vp), BlockArg::Value(vt)]);
        // On the prototype: its value; or a call of a getter (own or on the
        // prototype).
        self.b.switch_to_block(proto);
        let data = self.b.create_block();
        let getter = self.b.create_block();
        let is_data = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 1);
        self.b.ins().brif(is_data, data, &[], getter, &[]);
        self.b.switch_to_block(data);
        let pat = self.ic_proto_slot(shape, ic, slow);
        let vp = self.b.ins().load(I64, Self::mem(), pat, 0);
        let vt = self.b.ins().load(I64, Self::mem(), pat, TAG_OFFSET);
        self.dup((vp, vt));
        self.b.ins().jump(join, &[BlockArg::Value(vp), BlockArg::Value(vt)]);
        self.b.switch_to_block(getter);
        let pat = self.ic_accessor_slot(p, shape, kind, ic, slow);
        let g = self.b.ins().load(I64, Self::mem(), pat, 0);
        self.nonnull(g, slow);
        let ctx = self.ctx;
        let (thp, tht) = receiver.unwrap_or((p, t));
        let tag = self.b.ins().iconst(I64, TAG_OBJECT);
        let none = self.b.ins().iconst(I64, 0);
        let (gp, gt) = self.call_linked((g, tag), (thp, tht), 0, none, true, |l| {
            let r = l.helper("tsaot_call_getter", &[ctx, g, thp, tht]);
            (r[0], r[1])
        });
        self.check_value(gt);
        self.b.ins().jump(join, &[BlockArg::Value(gp), BlockArg::Value(gt)]);
        self.b.switch_to_block(slow);
        let atom = self.atom(a);
        let r = match receiver {
            Some((rp_, rt_)) => self.helper("tsaot_super_get_ic", &[ctx, p, t, rp_, rt_, atom, ic]),
            None => self.helper("tsaot_ic_get_proto", &[ctx, p, t, atom, ic]),
        };
        self.check_value(r[1]);
        self.b.ins().jump(join, &[BlockArg::Value(r[0]), BlockArg::Value(r[1])]);
        self.b.switch_to_block(join);
        (rp, rt)
    }

    /// Continue if `(p, t)` is an object with the shape cached in `ic`, else
    /// go to `slow`. Returns the shape and the cache's kind.
    fn ic_shape_hit(&mut self, p: Value, t: Value, ic: Value, slow: Block) -> (Value, Value) {
        let is_obj = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let shape = self.b.ins().load(I64, Self::mem(), p, layout::OBJ_SHAPE);
        let cached = self.b.ins().load(I64, Self::mem(), ic, 0);
        let same = self.b.ins().icmp(IntCC::Equal, shape, cached);
        let hit = self.b.create_block();
        self.b.ins().brif(same, hit, &[], slow, &[]);
        self.b.switch_to_block(hit);
        let kind = self.b.ins().uload32(Self::mem(), ic, 12);
        (shape, kind)
    }

    /// The address of the cached own property of `p`.
    fn ic_slot_addr(&mut self, p: Value, ic: Value) -> Value {
        let slot = self.b.ins().uload32(Self::mem(), ic, 8);
        let props = self.b.ins().load(I64, Self::mem(), p, layout::OBJ_PROP);
        let off = self.b.ins().imul_imm_s(slot, SIZE as i64);
        self.b.ins().iadd(props, off)
    }

    /// For a cache of a prototype property: continue if the prototype (of
    /// an object of `shape`) holding it, and the one before it if it is two
    /// levels up, have the cached shapes, else go to `slow`. Returns the
    /// address of the property in the holder.
    fn ic_proto_slot(&mut self, shape: Value, ic: Value, slow: Block) -> Value {
        let first = self.b.ins().load(I64, Self::mem(), shape, layout::SHAPE_PROTO);
        let mid = self.b.ins().load(I64, Self::mem(), ic, 24);
        let holder = self.b.create_block();
        let proto = self.b.append_block_param(holder, I64);
        let second = self.b.create_block();
        self.b.ins().brif(mid, second, &[], holder, &[BlockArg::Value(first)]);
        self.b.switch_to_block(second);
        let first_shape = self.b.ins().load(I64, Self::mem(), first, layout::OBJ_SHAPE);
        let same = self.b.ins().icmp(IntCC::Equal, first_shape, mid);
        let up = self.b.create_block();
        self.b.ins().brif(same, up, &[], slow, &[]);
        self.b.switch_to_block(up);
        let p2 = self.b.ins().load(I64, Self::mem(), first_shape, layout::SHAPE_PROTO);
        self.b.ins().jump(holder, &[BlockArg::Value(p2)]);
        self.b.switch_to_block(holder);
        let pshape = self.b.ins().load(I64, Self::mem(), proto, layout::OBJ_SHAPE);
        let cached = self.b.ins().load(I64, Self::mem(), ic, 16);
        let same = self.b.ins().icmp(IntCC::Equal, pshape, cached);
        let hit = self.b.create_block();
        self.b.ins().brif(same, hit, &[], slow, &[]);
        self.b.switch_to_block(hit);
        self.ic_slot_addr(proto, ic)
    }

    /// For a cache of a getter or setter: the address of the property
    /// holding it, own (kinds 4 and 5) or on a prototype (`ic_proto_slot`).
    fn ic_accessor_slot(&mut self, p: Value, shape: Value, kind: Value, ic: Value, slow: Block) -> Value {
        let holder = self.b.create_block();
        let at = self.b.append_block_param(holder, I64);
        let own = self.b.create_block();
        let proto = self.b.create_block();
        let is_own = self.b.ins().icmp_imm_s(IntCC::UnsignedGreaterThanOrEqual, kind, 4);
        self.b.ins().brif(is_own, own, &[], proto, &[]);
        self.b.switch_to_block(own);
        let own_at = self.ic_slot_addr(p, ic);
        self.b.ins().jump(holder, &[BlockArg::Value(own_at)]);
        self.b.switch_to_block(proto);
        let proto_at = self.ic_proto_slot(shape, ic, slow);
        self.b.ins().jump(holder, &[BlockArg::Value(proto_at)]);
        self.b.switch_to_block(holder);
        at
    }

    /// Continue if `v` (a cached getter or setter) is not NULL, else go to
    /// `slow`: an accessor half can be removed without a shape change.
    fn nonnull(&mut self, v: Value, slow: Block) {
        let ok = self.b.create_block();
        self.b.ins().brif(v, ok, &[], slow, &[]);
        self.b.switch_to_block(ok);
    }

    /// The address of a new inline cache (a `TsaotIC`).
    fn new_ic(&mut self) -> Value {
        let i = self.env.next_ic.get();
        self.env.next_ic.set(i + 1);
        let base = self.b.ins().symbol_value(self.ptr, self.env.ics);
        self.b.ins().iadd_imm_s(base, (i * IC_SIZE) as i64)
    }

    /// Continue if `(p, t)` is an object with the shape cached in `ic`, else
    /// go to `slow`. Returns the address of the cached property's value.
    fn ic_hit(&mut self, p: Value, t: Value, ic: Value, slow: Block) -> Value {
        let is_obj = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_OBJECT);
        let obj = self.b.create_block();
        self.b.ins().brif(is_obj, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let shape = self.b.ins().load(I64, Self::mem(), p, layout::OBJ_SHAPE);
        let cached = self.b.ins().load(I64, Self::mem(), ic, 0);
        let same = self.b.ins().icmp(IntCC::Equal, shape, cached);
        let hit = self.b.create_block();
        self.b.ins().brif(same, hit, &[], slow, &[]);
        self.b.switch_to_block(hit);
        let slot = self.b.ins().uload32(Self::mem(), ic, 8);
        let props = self.b.ins().load(I64, Self::mem(), p, layout::OBJ_PROP);
        let off = self.b.ins().imul_imm_s(slot, SIZE as i64);
        self.b.ins().iadd(props, off)
    }

    // ---- arrays ----

    /// An element index from a key: whether it is an integer (`I8`) and, if
    /// so, its value (`I64`; negative ones fail unsigned bounds checks).
    fn elem_index(&mut self, key: V) -> (Value, Value) {
        match key {
            V::Num(d) if self.int_of.contains_key(&d) => (self.b.ins().iconst(I8, 1), self.int_of[&d]),
            V::Num(d) => {
                let i = self.b.ins().fcvt_to_sint_sat(I64, d);
                let back = self.b.ins().fcvt_from_sint(F64, i);
                (self.b.ins().fcmp(FloatCC::Equal, back, d), i)
            }
            V::Any(p, t) => {
                let ok = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_INT);
                let i = self.b.ins().ireduce(I32, p);
                (ok, self.b.ins().sextend(I64, i))
            }
            _ => {
                let zero = self.b.ins().iconst(I64, 0);
                (self.b.ins().iconst(I8, 0), zero)
            }
        }
    }

    /// Continue only if `ok` and `(p, t)` is an array, else go to `slow`.
    /// Returns its fast element count (0 unless a fast array).
    fn array_part(&mut self, p: Value, t: Value, ok: Value, slow: Block) -> Value {
        let is_obj = self.b.ins().icmp_imm_s(IntCC::Equal, t, TAG_OBJECT);
        let go = self.b.ins().band(is_obj, ok);
        let obj = self.b.create_block();
        self.b.ins().brif(go, obj, &[], slow, &[]);
        self.b.switch_to_block(obj);
        let class = self.b.ins().uload16(I32, Self::mem(), p, layout::OBJ_CLASS_ID);
        let is_array = self.b.ins().icmp_imm_s(IntCC::Equal, class, layout::CLASS_ARRAY);
        let array = self.b.create_block();
        self.b.ins().brif(is_array, array, &[], slow, &[]);
        self.b.switch_to_block(array);
        self.b.ins().uload32(Self::mem(), p, layout::OBJ_ARRAY_COUNT)
    }

    /// Entry `slot` of a table of JS values (borrowed).
    fn table_value(&mut self, table: GlobalValue, slot: usize) -> (Value, Value) {
        let base = self.b.ins().symbol_value(self.ptr, table);
        let at = (slot * SIZE) as i32;
        let p = self.b.ins().load(I64, Self::mem(), base, at);
        let t = self.b.ins().load(I64, Self::mem(), base, at + TAG_OFFSET);
        (p, t)
    }

    /// Address of fast array element `idx`.
    fn array_slot(&mut self, p: Value, idx: Value) -> Value {
        let values = self.b.ins().load(I64, Self::mem(), p, layout::OBJ_ARRAY_VALUES);
        let off = self.b.ins().imul_imm_s(idx, SIZE as i64);
        self.b.ins().iadd(values, off)
    }

    /// Whether typed array `p` (of `JSTypedArrayEnum` `kind`, an `I32`) has
    /// the shape of a fresh one, so its `length` is its element count.
    fn pristine_shape(&mut self, p: Value, kind: Value) -> Value {
        let shape = self.b.ins().load(I64, Self::mem(), p, layout::OBJ_SHAPE);
        let table = self.b.ins().symbol_value(self.ptr, self.env.shapes);
        let k = self.b.ins().uextend(I64, kind);
        let off = self.b.ins().imul_imm_s(k, 8);
        let at = self.b.ins().iadd(table, off);
        let want = self.b.ins().load(I64, Self::mem(), at, 0);
        self.b.ins().icmp(IntCC::Equal, shape, want)
    }

    /// Read the element at `at` as a number.
    fn load_element(&mut self, kind: ArrKind, at: Value) -> Value {
        let m = Self::mem();
        match kind {
            ArrKind::Uint8Clamped | ArrKind::Uint8 => {
                let x = self.b.ins().uload8(I32, m, at, 0);
                self.b.ins().fcvt_from_uint(F64, x)
            }
            ArrKind::Int8 => {
                let x = self.b.ins().sload8(I32, m, at, 0);
                self.b.ins().fcvt_from_sint(F64, x)
            }
            ArrKind::Uint16 => {
                let x = self.b.ins().uload16(I32, m, at, 0);
                self.b.ins().fcvt_from_uint(F64, x)
            }
            ArrKind::Int16 => {
                let x = self.b.ins().sload16(I32, m, at, 0);
                self.b.ins().fcvt_from_sint(F64, x)
            }
            ArrKind::Uint32 => {
                let x = self.b.ins().load(I32, m, at, 0);
                self.b.ins().fcvt_from_uint(F64, x)
            }
            ArrKind::Int32 => {
                let x = self.b.ins().load(I32, m, at, 0);
                self.b.ins().fcvt_from_sint(F64, x)
            }
            // A promoted float32 NaN never has the low bits of the reserved
            // `undefined` NaN.
            ArrKind::Float32 => {
                let x = self.b.ins().load(F32, m, at, 0);
                self.b.ins().fpromote(F64, x)
            }
            ArrKind::Float64 => {
                // Any NaN may be stored; never the reserved one.
                let bits = self.b.ins().load(I64, m, at, 0);
                let reserved = self.b.ins().icmp_imm_s(IntCC::Equal, bits, MAYBE_F64_UNDEFINED as i64);
                let nan = self.b.ins().iconst(I64, f64::NAN.to_bits() as i64);
                let bits = self.b.ins().select(reserved, nan, bits);
                self.b.ins().bitcast(F64, MemFlagsData::new(), bits)
            }
        }
    }

    /// Store number `d` at `at`, converted as the engine does.
    fn store_element(&mut self, kind: ArrKind, at: Value, d: Value) {
        let m = Self::mem();
        match kind {
            ArrKind::Float64 => {
                self.b.ins().store(m, d, at, 0);
            }
            ArrKind::Float32 => {
                let x = self.b.ins().fdemote(F32, d);
                self.b.ins().store(m, x, at, 0);
            }
            ArrKind::Uint8Clamped => {
                // NaN -> 0, clamped to [0, 255], rounded half to even.
                let lo = self.b.ins().f64const(0.0);
                let hi = self.b.ins().f64const(255.0);
                let x = self.b.ins().fmax(d, lo);
                let x = self.b.ins().fmin(x, hi);
                let x = self.b.ins().nearest(x);
                let is_nan = self.b.ins().fcmp(FloatCC::Unordered, d, d);
                let x = self.b.ins().select(is_nan, lo, x);
                let x = self.b.ins().fcvt_to_uint_sat(I32, x);
                self.b.ins().istore8(m, x, at, 0);
            }
            _ => {
                let x = self.toint32(d);
                match kind.size() {
                    1 => self.b.ins().istore8(m, x, at, 0),
                    2 => self.b.ins().istore16(m, x, at, 0),
                    _ => self.b.ins().store(m, x, at, 0),
                };
            }
        }
    }

    /// `v` as a JS value (a new reference only if it is a number or boolean).
    fn boxed(&mut self, v: V) -> (Value, Value) {
        match v {
            V::Num(d) => self.box_number(d),
            V::Int(i) => {
                let d = self.int_f64(i);
                self.box_number(d)
            }
            V::Bool(x) => {
                let p = self.b.ins().uextend(I64, x);
                (p, self.b.ins().iconst(I64, TAG_BOOL))
            }
            V::Any(p, t) => (p, t),
            V::Void => {
                let V::Any(p, t) = self.konst(0, TAG_UNDEFINED) else { unreachable!() };
                (p, t)
            }
        }
    }

    /// JS ToInt32. Exact for |x| < 2^63 via a saturating i64 conversion and
    /// truncation; everything else (huge values, infinities, NaN) goes to the
    /// runtime.
    fn toint32(&mut self, x: Value) -> Value {
        if let Some(&i) = self.int_of.get(&x) {
            return self.b.ins().ireduce(I32, i);
        }
        let ax = self.b.ins().fabs(x);
        let lim = self.b.ins().f64const(9_223_372_036_854_775_808.0);
        let small = self.b.ins().fcmp(FloatCC::LessThan, ax, lim);
        let fast = self.b.create_block();
        let slow = self.b.create_block();
        let join = self.b.create_block();
        let res = self.b.append_block_param(join, I32);
        self.b.ins().brif(small, fast, &[], slow, &[]);
        self.b.switch_to_block(fast);
        let i = self.b.ins().fcvt_to_sint_sat(I64, x);
        let r = self.b.ins().ireduce(I32, i);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(slow);
        self.b.set_cold_block(slow);
        let r = self.call_helper("tsaot_toint32_slow", &[x]);
        self.b.ins().jump(join, &[BlockArg::Value(r)]);
        self.b.switch_to_block(join);
        res
    }

    fn math(&mut self, f: MathFn, a: &[Value]) -> Value {
        if let Some(sym) = f.symbol() {
            return self.call_helper(sym, a);
        }
        fn ins<'s, 'b>(l: &'s mut Lower<'b>) -> cranelift_frontend::FuncInstBuilder<'s, 'b> {
            l.b.ins()
        }
        match f {
            MathFn::Abs => ins(self).fabs(a[0]),
            MathFn::Sqrt => ins(self).sqrt(a[0]),
            MathFn::Floor => ins(self).floor(a[0]),
            MathFn::Ceil => ins(self).ceil(a[0]),
            MathFn::Trunc => ins(self).trunc(a[0]),
            MathFn::Fround => {
                let s = ins(self).fdemote(F32, a[0]);
                ins(self).fpromote(F64, s)
            }
            // Cranelift fmin/fmax propagate NaN and order -0 < +0, like JS.
            MathFn::Min => ins(self).fmin(a[0], a[1]),
            MathFn::Max => ins(self).fmax(a[0], a[1]),
            MathFn::Imul => {
                let x = self.toint32(a[0]);
                let y = self.toint32(a[1]);
                let r = ins(self).imul(x, y);
                ins(self).fcvt_from_sint(F64, r)
            }
            MathFn::Clz32 => {
                let x = self.toint32(a[0]);
                let r = ins(self).clz(x);
                ins(self).fcvt_from_uint(F64, r)
            }
            MathFn::Sign => {
                // Returns x itself for ±0 and NaN.
                let z = ins(self).f64const(0.0);
                let one = ins(self).f64const(1.0);
                let neg = ins(self).f64const(-1.0);
                let lt = ins(self).fcmp(FloatCC::LessThan, a[0], z);
                let gt = ins(self).fcmp(FloatCC::GreaterThan, a[0], z);
                let r = ins(self).select(lt, neg, a[0]);
                ins(self).select(gt, one, r)
            }
            MathFn::IsNaN => ins(self).fcmp(FloatCC::Unordered, a[0], a[0]),
            MathFn::IsFinite => self.is_finite(a[0]),
            MathFn::IsInteger => self.is_integer(a[0]),
            MathFn::IsSafeInteger => {
                let int = self.is_integer(a[0]);
                let ax = ins(self).fabs(a[0]);
                let max = ins(self).f64const(9007199254740991.0);
                let safe = ins(self).fcmp(FloatCC::LessThanOrEqual, ax, max);
                ins(self).band(int, safe)
            }
            _ => unreachable!("{f:?} has a symbol"),
        }
    }

    fn is_finite(&mut self, x: Value) -> Value {
        let ax = self.b.ins().fabs(x);
        let inf = self.b.ins().f64const(f64::INFINITY);
        self.b.ins().fcmp(FloatCC::LessThan, ax, inf)
    }

    fn is_integer(&mut self, x: Value) -> Value {
        let fin = self.is_finite(x);
        let t = self.b.ins().trunc(x);
        let eq = self.b.ins().fcmp(FloatCC::Equal, t, x);
        self.b.ins().band(fin, eq)
    }
}

impl V {
    /// Math results that are booleans (`Number.isNaN` & co.).
    fn retype(self, t: Ty, _l: &mut Lower) -> V {
        match (self, t) {
            (V::Num(v), Ty::Bool) => V::Bool(v),
            (v, _) => v,
        }
    }
}

/// Size of a `TsaotIC` (an inline cache entry).
pub(crate) const IC_SIZE: usize = 32;

/// Frame layout of an async function: per local, its first slot (boxed
/// locals use two: the box object and the address of its value), then the
/// slot of `this` (methods), and the total number of slots.
/// See `Lower::prop_step`.
#[derive(Clone, Copy)]
enum PropStep<'e> {
    Unary(AnyUnOp),
    /// The operator, its boxed right operand and that operand's number.
    Binary(AnyBinOp, &'e Expr, &'e Expr),
}

/// A small expression without calls or side effects, cheap to lower twice.
fn is_cheap(e: &Expr) -> bool {
    match &e.kind {
        ExprKind::Num(_) | ExprKind::Bool(_) | ExprKind::Local(_) => true,
        ExprKind::Compare(_, x, y) | ExprKind::Binary(BinOp::Add | BinOp::Sub, x, y) => is_cheap(x) && is_cheap(y),
        ExprKind::Unary(UnOp::Not, x) | ExprKind::Truthy(x) => is_cheap(x),
        _ => false,
    }
}

/// Largest integer step `int_step` takes on an int local: smaller than half
/// the spacing of doubles near 2^62, so rounding stops an int local from
/// growing well before it could overflow an `i64`.
const INT_STEP_MAX: f64 = 256.0;
/// 2^53: integers up to this magnitude are exact doubles.
const INT_EXACT: i64 = 1 << 53;

/// An integer literal that `int_locals` accepts (an exact double; not -0).
fn int_literal(e: &Expr, limit: f64) -> Option<i64> {
    match e.kind {
        ExprKind::Num(c) if c.fract() == 0.0 && c.abs() <= limit && !(c == 0.0 && c.is_sign_negative()) => {
            Some(c as i64)
        }
        _ => None,
    }
}

/// `x + c` / `x - c` / `c + x` on local `l`, with `c` a small integer: the
/// step `c` (negated for `-`).
fn int_step(l: ir::LocalId, e: &Expr) -> Option<i64> {
    let ExprKind::Binary(op, x, y) = &e.kind else { return None };
    let local = |x: &Expr| matches!(x.kind, ExprKind::Local(m) if m == l);
    match op {
        BinOp::Add if local(x) => int_literal(y, INT_STEP_MAX),
        BinOp::Add if local(y) => int_literal(x, INT_STEP_MAX),
        BinOp::Sub if local(x) => int_literal(y, INT_STEP_MAX).map(|c| -c),
        _ => None,
    }
}

/// The int locals that provably stay below 2^52 in magnitude, so their
/// steps need no range check: assigned only integer literals, and stepped
/// only in code that runs a statically bounded number of times (loops
/// `for (i = a; i < K; i++)` with literal `a` and `K`, and their nests).
fn small_ints(f: &ir::Func, ints: &[bool]) -> Vec<bool> {
    let n = f.locals.len();
    let mut lit = vec![0.0f64; n];
    let mut steps = vec![0.0f64; n];
    bound_stmts(&f.body, 1.0, &mut lit, &mut steps);
    (0..n).map(|l| ints[l] && lit[l] + steps[l] <= (1u64 << 52) as f64).collect()
}

/// For `small_ints`: the largest literal assigned to each local, and how far
/// the steps executed `times` times (at most) can take it.
fn bound_stmts(stmts: &[Stmt], times: f64, lit: &mut [f64], steps: &mut [f64]) {
    let count = |e: &Expr, times: f64, lit: &mut [f64], steps: &mut [f64]| {
        ir::visit(e, &mut |x| match &x.kind {
            ExprKind::Update { local, delta, .. } => steps[*local] += delta.abs() * times,
            ExprKind::SetLocal(l, v) | ExprKind::InitLocal(l, v) => {
                if let Some(c) = int_literal(v, INT_EXACT as f64) {
                    lit[*l] = lit[*l].max(c.unsigned_abs() as f64);
                } else if v.ty == Ty::I32 {
                    lit[*l] = lit[*l].max(2_147_483_648.0);
                } else if let Some(c) = int_step(*l, v) {
                    steps[*l] += c.unsigned_abs() as f64 * times;
                } else {
                    steps[*l] = f64::INFINITY;
                }
            }
            _ => {}
        })
    };
    for (k, s) in stmts.iter().enumerate() {
        match s {
            Stmt::Expr(e) | Stmt::Throw(e) | Stmt::Return(Some(e)) => count(e, times, lit, steps),
            Stmt::Await { value, .. } | Stmt::Yield { value, .. } => count(value, times, lit, steps),
            Stmt::Return(None) | Stmt::Break(_) | Stmt::Continue(_) | Stmt::NewBoxes(_) => {}
            Stmt::If(c, a, b) => {
                count(c, times, lit, steps);
                bound_stmts(a, times, lit, steps);
                bound_stmts(b, times, lit, steps);
            }
            Stmt::Loop { test, post_test, body, update, .. } => {
                let test_pre = test.as_ref().filter(|_| !post_test);
                let trips = static_trips(&stmts[..k], test_pre, body, update.as_ref());
                // The test runs once more than the body.
                let inner = times * trips.map_or(f64::INFINITY, |t| t + 1.0);
                test.iter().chain(update).for_each(|e| count(e, inner, lit, steps));
                bound_stmts(body, inner, lit, steps);
            }
            Stmt::Switch { cases, .. } => {
                for (c, body) in cases {
                    c.iter().for_each(|e| count(e, times, lit, steps));
                    bound_stmts(body, times, lit, steps);
                }
            }
            Stmt::Block(body) | Stmt::IterGuard { body, .. } => bound_stmts(body, times, lit, steps),
            Stmt::Try { body, catch, finally, .. } => {
                bound_stmts(body, times, lit, steps);
                catch.iter().for_each(|(_, c)| bound_stmts(c, times, lit, steps));
                finally.iter().for_each(|c| bound_stmts(c, times, lit, steps));
            }
        }
    }
}

/// The number of iterations of a loop `for (i = a; i < K; i += d)` (or
/// `<=`, or counting down with `>` / `>=`) whose body does not assign `i`,
/// with `i = a` (`a` an integer literal) the last assignment to `i` among
/// the statements `before` the loop.
fn static_trips(before: &[Stmt], test: Option<&Expr>, body: &[Stmt], update: Option<&Expr>) -> Option<f64> {
    let ExprKind::Compare(op, x, y) = &test?.kind else { return None };
    let ExprKind::Local(i) = x.kind else { return None };
    let ExprKind::Update { local, delta, .. } = update?.kind else { return None };
    let k = int_literal(y, (INT_EXACT >> 1) as f64)? as f64;
    if local != i || delta == 0.0 {
        return None;
    }
    let assigns = |s: &Stmt| {
        let mut found = false;
        ir::visit_stmts(std::slice::from_ref(s), &mut |e| found |= ir::assigns_local(e, i));
        found
    };
    if body.iter().any(assigns) {
        return None;
    }
    let a = match before.iter().rev().find(|s| assigns(s))? {
        Stmt::Expr(Expr { kind: ExprKind::SetLocal(l, v) | ExprKind::InitLocal(l, v), .. }) if *l == i => {
            int_literal(v, (INT_EXACT >> 1) as f64)? as f64
        }
        _ => return None,
    };
    let d = delta.abs();
    let span = match op {
        CmpOp::Lt if delta > 0.0 => k - a,
        CmpOp::Le if delta > 0.0 => k - a + 1.0,
        CmpOp::Gt if delta < 0.0 => a - k,
        CmpOp::Ge if delta < 0.0 => a - k + 1.0,
        _ => return None,
    };
    Some((span / d).ceil().max(0.0))
}

/// The `F64` locals that only ever hold integers, which are kept in `i64`s:
/// locals (not parameters, not boxed) only assigned integer literals, `int`
/// values (`I32`: bitwise results, `int` locals and calls), other
/// such locals, and themselves plus or minus small integers (`x++`,
/// `x += 2`). A step past 2^53 rounds the result as `f64` arithmetic would,
/// so the value is always exactly the double JS computes; with small steps
/// rounding keeps it far from the `i64` limits.
fn int_locals(f: &ir::Func) -> Vec<bool> {
    let n = f.locals.len();
    let mut ints: Vec<bool> = (0..n).map(|i| i >= f.params.len() && !f.boxed[i] && f.locals[i] == Ty::F64).collect();
    loop {
        let mut changed = false;
        ir::visit_stmts(&f.body, &mut |e| {
            let (l, ok) = match &e.kind {
                ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x) => (
                    *l,
                    x.ty == Ty::I32
                        || int_literal(x, INT_EXACT as f64).is_some()
                        || int_step(*l, x).is_some()
                        || matches!(x.kind, ExprKind::Local(m) if ints[m]),
                ),
                ExprKind::Update { local, delta, .. } => (*local, delta.fract() == 0.0 && delta.abs() <= INT_STEP_MAX),
                _ => return,
            };
            if ints[l] && !ok {
                ints[l] = false;
                changed = true;
            }
        });
        if !changed {
            return ints;
        }
    }
}

/// Of the int locals, those always in the int32 range: assigned only int32
/// literals and int32 values (bitwise results, `int`s), never stepped.
fn i32_locals(f: &ir::Func, ints: &[bool]) -> Vec<bool> {
    let mut i32s = ints.to_vec();
    ir::visit_stmts(&f.body, &mut |e| match &e.kind {
        ExprKind::SetLocal(l, x) | ExprKind::InitLocal(l, x) => {
            let lit = int_literal(x, INT_EXACT as f64).is_some_and(|c| i32::try_from(c).is_ok());
            if !(x.ty == Ty::I32 || lit) {
                i32s[*l] = false;
            }
        }
        ExprKind::Update { local, .. } => i32s[*local] = false,
        _ => {}
    });
    i32s
}

pub(crate) fn frame_layout(f: &ir::Func) -> (Vec<usize>, Option<usize>, usize) {
    let mut at = Vec::with_capacity(f.locals.len());
    let mut n = 0;
    for i in 0..f.locals.len() {
        at.push(n);
        n += if f.boxed[i] { 2 } else { 1 };
    }
    let this = f.has_this.then(|| {
        n += 1;
        n - 1
    });
    (at, this, n)
}

/// State of an async step function being lowered.
pub(crate) struct AsyncCx {
    /// The frame's slots (`JSValue *`).
    pub slots: Value,
    pub state: Value,
    /// The value delivered on resumption (`JSValue *`, borrowed).
    pub value: Value,
    pub is_throw: Value,
    pub layout: Vec<usize>,
    /// Resume points, by state - 1.
    resumes: Vec<Block>,
    dispatch: Block,
}

impl Lower<'_> {
    /// Start lowering an async step function: move the locals out of the
    /// frame, then dispatch on the state (0: start of the body).
    pub(crate) fn enter_async(&mut self, slots: Value, state: Value, value: Value, is_throw: Value, f: &ir::Func) {
        let (layout, this_slot, _) = frame_layout(f);
        for (l, &s) in layout.iter().enumerate() {
            let at = (s * SIZE) as i32;
            let p = self.b.ins().load(I64, Self::mem(), slots, at);
            let t = self.b.ins().load(I64, Self::mem(), slots, at + TAG_OFFSET);
            match self.vars[l] {
                LVar::Boxed { obj: (op, ot), slot, .. } => {
                    let sp = self.b.ins().load(I64, Self::mem(), slots, at + SIZE as i32);
                    self.b.def_var(op, p);
                    self.b.def_var(ot, t);
                    self.b.def_var(slot, sp);
                }
                LVar::Two(vp, vt) => {
                    self.b.def_var(vp, p);
                    self.b.def_var(vt, t);
                }
                LVar::Int(v) => self.b.def_var(v, p),
                LVar::One(v) => {
                    let x = match self.locals[l] {
                        Ty::F64 | Ty::MaybeF64 => self.b.ins().bitcast(F64, MemFlagsData::new(), p),
                        _ => self.b.ins().ireduce(I8, p),
                    };
                    self.b.def_var(v, x);
                }
            }
            // The frame no longer owns these values while the step runs.
            let zero = self.b.ins().iconst(I64, 0);
            let undef = self.b.ins().iconst(I64, TAG_UNDEFINED);
            self.b.ins().store(Self::mem(), zero, slots, at);
            self.b.ins().store(Self::mem(), undef, slots, at + TAG_OFFSET);
        }
        if let Some(s) = this_slot {
            // `this` stays owned by the frame; borrowed here.
            let at = (s * SIZE) as i32;
            let p = self.b.ins().load(I64, Self::mem(), slots, at);
            let t = self.b.ins().load(I64, Self::mem(), slots, at + TAG_OFFSET);
            self.this = Some((p, t));
        }
        let dispatch = self.b.create_block();
        self.b.ins().jump(dispatch, &[]);
        let start = self.b.create_block();
        self.b.switch_to_block(start);
        self.async_cx = Some(AsyncCx { slots, state, value, is_throw, layout, resumes: vec![start], dispatch });
        self.async_gen = f.is_async && f.is_generator;
    }

    /// Move every local into the frame (suspending).
    fn spill_locals(&mut self) {
        let cx = self.async_cx.as_ref().unwrap();
        let (slots, layout) = (cx.slots, cx.layout.clone());
        for (l, &s) in layout.iter().enumerate() {
            let at = (s * SIZE) as i32;
            let (p, t) = match self.vars[l] {
                LVar::Boxed { obj: (op, ot), slot, .. } => {
                    let sp = self.b.use_var(slot);
                    let ft = self.b.ins().iconst(I64, TAG_FLOAT64);
                    self.b.ins().store(Self::mem(), sp, slots, at + SIZE as i32);
                    self.b.ins().store(Self::mem(), ft, slots, at + SIZE as i32 + TAG_OFFSET);
                    (self.b.use_var(op), self.b.use_var(ot))
                }
                // Not a JS value: only this function reads it back.
                LVar::Int(v) => (self.b.use_var(v), self.b.ins().iconst(I64, TAG_FLOAT64)),
                LVar::Two(vp, vt) => (self.b.use_var(vp), self.b.use_var(vt)),
                LVar::One(v) => {
                    let x = self.b.use_var(v);
                    match self.locals[l] {
                        Ty::F64 | Ty::MaybeF64 => {
                            let p = self.b.ins().bitcast(I64, MemFlagsData::new(), x);
                            (p, self.b.ins().iconst(I64, TAG_FLOAT64))
                        }
                        _ => {
                            let p = self.b.ins().uextend(I64, x);
                            (p, self.b.ins().iconst(I64, TAG_BOOL))
                        }
                    }
                }
            };
            self.b.ins().store(Self::mem(), p, slots, at);
            self.b.ins().store(Self::mem(), t, slots, at + TAG_OFFSET);
        }
    }

    /// `await value`: register the continuation, spill, return "suspended";
    /// lowering continues at the resume point.
    fn await_stmt(&mut self, value: &Expr, target: ir::AwaitTarget) {
        let (vp, vt) = if self.async_gen {
            let v = self.expr(value).any();
            self.await_value(v)
        } else {
            self.await_promise(value)
        };
        self.awaited(vp, vt, target);
    }

    /// `await` in an async function: register the continuation, spill,
    /// return "suspended". The awaited value (a new reference).
    fn await_promise(&mut self, value: &Expr) -> (Value, Value) {
        let resume = self.b.create_block();
        let state = {
            let cx = self.async_cx.as_mut().expect("await in an async function");
            cx.resumes.push(resume);
            cx.resumes.len() - 1
        };
        let (p, t) = self.expr(value).any();
        let slots = self.async_cx.as_ref().unwrap().slots;
        let st = self.b.ins().iconst(I32, state as i64);
        let ctx = self.ctx;
        // Only locals survive a suspension.
        debug_assert!(self.temps.is_empty(), "values held across an await");
        // Takes ownership of the value.
        let r = self.helper("tsaot_await", &[ctx, slots, p, t, st]);
        self.check_int(r[0]);
        self.spill_locals();
        let z = self.b.ins().iconst(I64, 0);
        let suspended = self.b.ins().iconst(I64, TAG_UNINITIALIZED);
        self.b.ins().return_(&[z, suspended]);

        // Resumed: the locals come back from the frame (see enter_async).
        self.b.switch_to_block(resume);
        let (vptr, is_throw) = {
            let cx = self.async_cx.as_ref().unwrap();
            (cx.value, cx.is_throw)
        };
        let vp = self.b.ins().load(I64, Self::mem(), vptr, 0);
        let vt = self.b.ins().load(I64, Self::mem(), vptr, TAG_OFFSET);
        self.dup((vp, vt));
        let throw = self.b.create_block();
        let ok = self.b.create_block();
        self.b.set_cold_block(throw);
        self.b.ins().brif(is_throw, throw, &[], ok, &[]);
        self.b.switch_to_block(throw);
        // A rejection is thrown at the await.
        self.helper("JS_Throw", &[ctx, vp, vt]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(ok);
        (vp, vt)
    }

    /// Deliver an awaited value (a new reference) to `target`.
    fn awaited(&mut self, vp: Value, vt: Value, target: ir::AwaitTarget) {
        let v = V::Any(vp, vt);
        match target {
            ir::AwaitTarget::Discard => self.free((vp, vt)),
            ir::AwaitTarget::Set(l) => self.set_local(l, v),
            ir::AwaitTarget::Init(l) => self.init_local(l, v),
            ir::AwaitTarget::Return => {
                let v = if self.ret == Ty::Void {
                    self.free((vp, vt));
                    V::Void
                } else {
                    v
                };
                self.return_value(v);
            }
        }
    }

    /// `await` in an async generator: suspend for the engine to await `v`
    /// (taken). The settled value (a new reference); a rejection is thrown.
    fn await_value(&mut self, v: (Value, Value)) -> (Value, Value) {
        self.suspend(Some(v), 3);
        let ((vp, vt), mode) = self.resumption();
        let throw = self.b.create_block();
        let ok = self.b.create_block();
        self.b.set_cold_block(throw);
        let is_throw = self.b.ins().icmp_imm_s(IntCC::Equal, mode, 2);
        self.b.ins().brif(is_throw, throw, &[], ok, &[]);
        self.b.switch_to_block(throw);
        let ctx = self.ctx;
        self.helper("JS_Throw", &[ctx, vp, vt]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(ok);
        (vp, vt)
    }

    /// Suspend the generator: spill the locals and return to the runtime
    /// what it yields (`kind` 0: a value, 1: an iterator result as is, 3: a
    /// value to await, in an async generator). It
    /// continues at the returned block, with the resumption in the step
    /// function's parameters.
    fn suspend(&mut self, value: Option<(Value, Value)>, kind: i64) -> Block {
        let resume = self.b.create_block();
        let (state, vptr) = {
            let cx = self.async_cx.as_mut().expect("yield in a generator");
            cx.resumes.push(resume);
            (cx.resumes.len() - 1, cx.value)
        };
        debug_assert!(self.temps.is_empty(), "values held across a yield");
        self.spill_locals();
        if let Some((p, t)) = value {
            self.b.ins().store(Self::mem(), p, vptr, 0);
            self.b.ins().store(Self::mem(), t, vptr, TAG_OFFSET);
        }
        let payload = self.b.ins().iconst(I64, 4 * state as i64 + kind);
        let suspended = self.b.ins().iconst(I64, TAG_UNINITIALIZED);
        self.b.ins().return_(&[payload, suspended]);
        self.b.switch_to_block(resume);
        resume
    }

    /// The value the generator was resumed with (a new reference), and how
    /// (next / return / throw: 0 / 1 / 2).
    fn resumption(&mut self) -> ((Value, Value), Value) {
        let (vptr, mode) = {
            let cx = self.async_cx.as_ref().unwrap();
            (cx.value, cx.is_throw)
        };
        let vp = self.b.ins().load(I64, Self::mem(), vptr, 0);
        let vt = self.b.ins().load(I64, Self::mem(), vptr, TAG_OFFSET);
        self.dup((vp, vt));
        ((vp, vt), mode)
    }

    fn yield_stmt(&mut self, value: &Expr, kind: ir::YieldKind, target: ir::AwaitTarget) {
        match kind {
            ir::YieldKind::Initial => {
                self.suspend(None, 0);
            }
            ir::YieldKind::Value => {
                let v = self.expr(value).any();
                // An async generator yields the awaited value.
                let v = if self.async_gen { self.await_value(v) } else { v };
                self.suspend(Some(v), 0);
                let (v, mode) = self.resumption();
                self.resumed(v, mode, target);
            }
            ir::YieldKind::Delegate { iter, next, next_atom } if self.async_gen => {
                self.async_yield_star(value, iter, next, next_atom, target)
            }
            ir::YieldKind::Delegate { iter, next, next_atom } => self.yield_star(value, iter, next, next_atom, target),
        }
    }

    /// After a suspension: throw or return the value delivered, or deliver
    /// it to `target`.
    fn resumed(&mut self, (vp, vt): (Value, Value), mode: Value, target: ir::AwaitTarget) {
        let throw = self.b.create_block();
        let not_throw = self.b.create_block();
        let ret = self.b.create_block();
        let next = self.b.create_block();
        self.b.set_cold_block(throw);
        let is_throw = self.b.ins().icmp_imm_s(IntCC::Equal, mode, 2);
        self.b.ins().brif(is_throw, throw, &[], not_throw, &[]);
        self.b.switch_to_block(throw);
        let ctx = self.ctx;
        self.helper("JS_Throw", &[ctx, vp, vt]);
        let edge = self.throw_edge();
        self.b.ins().jump(edge, &[]);
        self.b.switch_to_block(not_throw);
        let is_return = self.b.ins().icmp_imm_s(IntCC::Equal, mode, 1);
        self.b.ins().brif(is_return, ret, &[], next, &[]);
        // `return(v)`: as a `return v` here (running `finally` blocks; an
        // async generator awaits the value first).
        self.b.switch_to_block(ret);
        let (rp, rt) = if self.async_gen { self.await_value((vp, vt)) } else { (vp, vt) };
        self.return_value(V::Any(rp, rt));
        self.b.switch_to_block(next);
        let v = V::Any(vp, vt);
        match target {
            ir::AwaitTarget::Discard => self.free((vp, vt)),
            ir::AwaitTarget::Set(l) => self.set_local(l, v),
            ir::AwaitTarget::Init(l) => self.init_local(l, v),
            ir::AwaitTarget::Return => unreachable!("`return yield` is hoisted"),
        }
    }

    /// `yield* value`: resume the inner iterator as the generator is
    /// resumed, yielding its results, until it is done.
    fn yield_star(
        &mut self,
        value: &Expr,
        iter: ir::LocalId,
        next: ir::LocalId,
        next_atom: ir::AtomId,
        target: ir::AwaitTarget,
    ) {
        let (p, t) = self.expr(value).any();
        let v = V::Any(p, t);
        self.hold(v);
        let ctx = self.ctx;
        let sync = self.b.ins().iconst(I32, 0);
        let it = self.helper("JS_GetIteratorObject", &[ctx, p, t, sync]);
        self.check_value(it[1]);
        self.release(v);
        self.free_v(v);
        self.set_local(iter, V::Any(it[0], it[1]));
        let a = self.atom(next_atom);
        let m = self.helper("JS_GetProperty", &[ctx, it[0], it[1], a]);
        self.check_value(m[1]);
        self.set_local(next, V::Any(m[0], m[1]));
        // The loop: (mode, received value).
        let head = self.b.create_block();
        let mode = self.b.append_block_param(head, I32);
        let rp = self.b.append_block_param(head, I64);
        let rt = self.b.append_block_param(head, I64);
        let zero = self.b.ins().iconst(I32, 0);
        let u = self.konst(0, TAG_UNDEFINED);
        let (up, ut) = u.any();
        self.b.ins().jump(head, &[BlockArg::Value(zero), BlockArg::Value(up), BlockArg::Value(ut)]);
        self.b.switch_to_block(head);
        let (ip, itag) = self.use_local(iter).any();
        let (np, nt) = self.use_local(next).any();
        let slot = self.values_slot(&[(ip, itag), (np, nt), (rp, rt)]);
        let kind_addr = self.done_slot();
        let r = self.helper("tsaot_yield_star", &[ctx, slot, mode, kind_addr]);
        self.check_value(r[1]);
        let kind = self.b.ins().load(I32, Self::mem(), kind_addr, 0);
        let yield_blk = self.b.create_block();
        let finished = self.b.create_block();
        let ret = self.b.create_block();
        let deliver = self.b.create_block();
        let not_yield = self.b.ins().icmp_imm_s(IntCC::NotEqual, kind, 0);
        self.b.ins().brif(not_yield, finished, &[], yield_blk, &[]);
        // An inner result: yielded as is, then resumed with the next mode.
        self.b.switch_to_block(yield_blk);
        self.suspend(Some((r[0], r[1])), 1);
        let ((vp, vt), mode) = self.resumption();
        self.b.ins().jump(head, &[BlockArg::Value(mode), BlockArg::Value(vp), BlockArg::Value(vt)]);
        self.b.switch_to_block(finished);
        let is_return = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 2);
        self.b.ins().brif(is_return, ret, &[], deliver, &[]);
        self.b.switch_to_block(ret);
        self.return_value(V::Any(r[0], r[1]));
        // Done: the value of the `yield*`.
        self.b.switch_to_block(deliver);
        let v = V::Any(r[0], r[1]);
        match target {
            ir::AwaitTarget::Discard => self.free((r[0], r[1])),
            ir::AwaitTarget::Set(l) => self.set_local(l, v),
            ir::AwaitTarget::Init(l) => self.init_local(l, v),
            ir::AwaitTarget::Return => unreachable!("`return yield*` is hoisted"),
        }
    }
    /// `yield*` in an async generator, as the interpreter's bytecode does it:
    /// the inner (async) iterator is resumed as the generator is, each
    /// result is awaited, and the values of the results not done are yielded
    /// (without awaiting them again). A `return` awaits the value it gets
    /// before passing it on, and again before returning.
    fn async_yield_star(
        &mut self,
        value: &Expr,
        iter: ir::LocalId,
        next: ir::LocalId,
        next_atom: ir::AtomId,
        target: ir::AwaitTarget,
    ) {
        let (p, t) = self.expr(value).any();
        let v = V::Any(p, t);
        self.hold(v);
        let ctx = self.ctx;
        let is_async = self.b.ins().iconst(I32, 1);
        let it = self.helper("JS_GetIteratorObject", &[ctx, p, t, is_async]);
        self.check_value(it[1]);
        self.release(v);
        self.free_v(v);
        self.set_local(iter, V::Any(it[0], it[1]));
        let a = self.atom(next_atom);
        let m = self.helper("JS_GetProperty", &[ctx, it[0], it[1], a]);
        self.check_value(m[1]);
        self.set_local(next, V::Any(m[0], m[1]));

        // The loop: (mode, received value).
        let head = self.b.create_block();
        let mode = self.b.append_block_param(head, I32);
        let rp = self.b.append_block_param(head, I64);
        let rt = self.b.append_block_param(head, I64);
        // A value to yield; the value of the `yield*`.
        let yield_part = self.b.create_block();
        let yp = self.b.append_block_param(yield_part, I64);
        let yt = self.b.append_block_param(yield_part, I64);
        let finish = self.b.create_block();
        let fp = self.b.append_block_param(finish, I64);
        let ft = self.b.append_block_param(finish, I64);
        let zero = self.b.ins().iconst(I32, 0);
        let (up, ut) = self.konst(0, TAG_UNDEFINED).any();
        self.b.ins().jump(head, &[BlockArg::Value(zero), BlockArg::Value(up), BlockArg::Value(ut)]);

        self.b.switch_to_block(head);
        let on_next = self.b.create_block();
        let not_next = self.b.create_block();
        let on_return = self.b.create_block();
        let on_throw = self.b.create_block();
        let is_next = self.b.ins().icmp_imm_s(IntCC::Equal, mode, 0);
        self.b.ins().brif(is_next, on_next, &[], not_next, &[]);
        self.b.switch_to_block(not_next);
        let is_return = self.b.ins().icmp_imm_s(IntCC::Equal, mode, 1);
        self.b.ins().brif(is_return, on_return, &[], on_throw, &[]);

        // Each mode separately: a mode does not survive the awaits.
        for (blk, m) in [(on_next, 0), (on_return, 1), (on_throw, 2)] {
            self.b.switch_to_block(blk);
            let received = if m == 1 { self.await_value((rp, rt)) } else { (rp, rt) };
            let (ip, itag) = self.use_local(iter).any();
            let (np, nt) = self.use_local(next).any();
            let slot = self.values_slot(&[(ip, itag), (np, nt), received]);
            let kind_addr = self.done_slot();
            let mv = self.b.ins().iconst(I32, m);
            let r = self.helper("tsaot_async_yield_star", &[ctx, slot, mv, kind_addr]);
            self.check_value(r[1]);
            let kind = self.b.ins().load(I32, Self::mem(), kind_addr, 0);
            let call_result = self.b.create_block();
            let special = self.b.create_block();
            let is_call = self.b.ins().icmp_imm_s(IntCC::Equal, kind, 0);
            self.b.ins().brif(is_call, call_result, &[], special, &[]);

            self.b.switch_to_block(special);
            if m == 1 {
                // No `return` method: return the (awaited) value.
                let (vp, vt) = self.await_value((r[0], r[1]));
                self.return_value(V::Any(vp, vt));
            } else if m == 2 {
                // No `throw` method: the iterator was closed; await that,
                // then throw.
                let (cp, ct) = self.await_value((r[0], r[1]));
                self.free((cp, ct));
                let e = self.helper("tsaot_throw_iterator_throw", &[ctx]);
                self.check_int(e[0]);
                self.b.ins().trap(cranelift_codegen::ir::TrapCode::user(1).unwrap());
            } else {
                self.b.ins().trap(cranelift_codegen::ir::TrapCode::user(1).unwrap());
            }

            self.b.switch_to_block(call_result);
            let (ap, at) = self.await_value((r[0], r[1]));
            let done_addr = self.done_slot();
            let val = self.helper("tsaot_async_yield_star_result", &[ctx, ap, at, done_addr]);
            self.check_value(val[1]);
            let done = self.b.ins().load(I32, Self::mem(), done_addr, 0);
            let finished = self.b.create_block();
            let args = [BlockArg::Value(val[0]), BlockArg::Value(val[1])];
            self.b.ins().brif(done, finished, &[], yield_part, &args);
            self.b.switch_to_block(finished);
            if m == 1 {
                let (vp, vt) = self.await_value((val[0], val[1]));
                self.return_value(V::Any(vp, vt));
            } else {
                self.b.ins().jump(finish, &args);
            }
        }

        // A result not done: its value is yielded, then the generator's
        // resumption goes to the inner iterator.
        self.b.switch_to_block(yield_part);
        self.suspend(Some((yp, yt)), 0);
        let ((vp, vt), mode) = self.resumption();
        self.b.ins().jump(head, &[BlockArg::Value(mode), BlockArg::Value(vp), BlockArg::Value(vt)]);

        self.b.switch_to_block(finish);
        let v = V::Any(fp, ft);
        match target {
            ir::AwaitTarget::Discard => self.free((fp, ft)),
            ir::AwaitTarget::Set(l) => self.set_local(l, v),
            ir::AwaitTarget::Init(l) => self.init_local(l, v),
            ir::AwaitTarget::Return => unreachable!("`return yield*` is hoisted"),
        }
    }

    /// The dispatch on the resume state.
    fn emit_dispatch(&mut self) {
        let Some(cx) = self.async_cx.take() else { return };
        self.b.switch_to_block(cx.dispatch);
        for (k, &blk) in cx.resumes.iter().enumerate().skip(1) {
            let is_k = self.b.ins().icmp_imm_s(IntCC::Equal, cx.state, k as i64);
            let next = self.b.create_block();
            self.b.ins().brif(is_k, blk, &[], next, &[]);
            self.b.switch_to_block(next);
        }
        self.b.ins().jump(cx.resumes[0], &[]);
    }
}
