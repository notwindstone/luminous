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
use cranelift_codegen::ir::types::{F32, F64, I8, I32, I64};
use cranelift_codegen::ir::{
    Block, BlockArg, FuncRef, GlobalValue, InstBuilder, MemFlagsData, StackSlotData, StackSlotKind, Type, Value,
};
use cranelift_frontend::{FunctionBuilder, Variable};
use tsaot_front::ir::{
    self, AnyBinOp, AnyCmpOp, AnyUnOp, BinOp, CmpOp, Expr, ExprKind, Key, LogicalOp, MathFn, Stmt, Ty, UnOp,
};

use crate::jsvalue::*;

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
            V::Num(v) | V::Bool(v) => vec![v],
            V::Any(p, t) => vec![p, t],
            V::Void => vec![],
        }
    }
}

/// Cranelift types of a value of type `t`.
pub(crate) fn clif_types(t: Ty) -> Vec<Type> {
    match t {
        Ty::F64 => vec![F64],
        Ty::Bool => vec![I8],
        Ty::Any => vec![I64, I64],
        Ty::Void => vec![],
    }
}

/// Rebuild a `V` of type `t` from Cranelift values.
pub(crate) fn from_values(t: Ty, vs: &[Value]) -> V {
    match t {
        Ty::F64 => V::Num(vs[0]),
        Ty::Bool => V::Bool(vs[0]),
        Ty::Any => V::Any(vs[0], vs[1]),
        Ty::Void => V::Void,
    }
}

#[derive(Clone, Copy)]
enum LVar {
    One(Variable),
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
    /// `FuncId` of closure 0.
    pub closure_base: usize,
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
        let vars: Vec<LVar> = f
            .locals
            .iter()
            .enumerate()
            .map(|(i, &t)| match t {
                _ if f.boxed[i] => LVar::Boxed {
                    obj: (b.declare_var(I64), b.declare_var(I64)),
                    slot: b.declare_var(I64),
                    owned: !f.captures.contains(&i),
                },
                Ty::Any => LVar::Two(b.declare_var(I64), b.declare_var(I64)),
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
        if let Some(data) = closure_data {
            for (k, &cl) in f.captures.iter().enumerate() {
                let LVar::Boxed { obj: (op, ot), slot, .. } = l.vars[cl] else { unreachable!() };
                let at = (2 * k * SIZE) as i32;
                let p = l.b.ins().load(I64, Self::mem(), data, at);
                let t = l.b.ins().load(I64, Self::mem(), data, at + TAG_OFFSET);
                let s = l.b.ins().load(I64, Self::mem(), data, at + SIZE as i32);
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
            Ty::F64 => V::Num(self.b.ins().f64const(0.0)),
            Ty::Bool => V::Bool(self.b.ins().iconst(I8, 0)),
            Ty::Any => self.konst(0, TAG_UNDEFINED),
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
            (LVar::One(v), Ty::F64) => V::Num(self.b.use_var(v)),
            (LVar::One(v), Ty::Bool) => V::Bool(self.b.use_var(v)),
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
                _ if self.locals[l] == Ty::Any => {
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

    /// A new reference to `v` (borrowed).
    fn dup(&mut self, (p, t): (Value, Value)) {
        let rc = self.is_refcounted(t);
        let yes = self.b.create_block();
        let join = self.b.create_block();
        self.b.ins().brif(rc, yes, &[], join, &[]);
        self.b.switch_to_block(yes);
        let ctx = self.ctx;
        self.helper("JS_DupValue", &[ctx, p, t]);
        self.b.ins().jump(join, &[]);
        self.b.switch_to_block(join);
    }

    fn free(&mut self, (p, t): (Value, Value)) {
        let rc = self.is_refcounted(t);
        let yes = self.b.create_block();
        let join = self.b.create_block();
        self.b.ins().brif(rc, yes, &[], join, &[]);
        self.b.switch_to_block(yes);
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

        // Normal exit: free the locals, then leave with the return value.
        let rb = self.ret_block;
        self.b.switch_to_block(rb);
        let rv: Vec<Value> = self.b.block_params(rb).to_vec();
        self.free_all_locals();
        match self.exits.ret {
            Exit::Return => {
                self.b.ins().return_(&rv);
            }
            Exit::Jump(blk) => {
                let args: Vec<BlockArg> = rv.into_iter().map(BlockArg::Value).collect();
                self.b.ins().jump(blk, &args);
            }
        }

        // Exception exit.
        let ee = self.exc_exit;
        self.b.switch_to_block(ee);
        self.free_all_locals();
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
                self.new_boxes(per_iteration, true);
                if let Some(u) = update {
                    self.effect(u);
                }
                match test {
                    Some(t) if *post_test => {
                        let v = self.expr(t).boolean();
                        self.b.ins().brif(v, header, &[], exit, &[]);
                    }
                    _ => {
                        self.b.ins().jump(header, &[]);
                    }
                }
                self.b.switch_to_block(exit);
            }
            Stmt::Switch { id, disc, cases } => self.switch(*id, *disc, cases),
            Stmt::Break(id) => {
                self.leave_to(*id);
                let (_, exit) = self.loops[id];
                self.b.ins().jump(exit, &[]);
                self.after_terminator();
            }
            Stmt::Continue(id) => {
                self.leave_to(*id);
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
                if self.scopes.iter().any(|s| matches!(s, Scope::Finally { .. })) {
                    self.hold(v);
                    self.run_finally_until(0);
                    self.release(v);
                }
                let args: Vec<BlockArg> = v.values().into_iter().map(BlockArg::Value).collect();
                let rb = self.ret_block;
                self.b.ins().jump(rb, &args);
                self.after_terminator();
            }
            Stmt::Throw(e) => {
                let (p, t) = self.expr(e).any();
                let ctx = self.ctx;
                self.helper("JS_Throw", &[ctx, p, t]);
                let edge = self.throw_edge();
                self.b.ins().jump(edge, &[]);
                self.after_terminator();
            }
            Stmt::Try { body, catch, finally } => self.try_stmt(body, catch.as_ref(), finally.as_ref()),
        }
    }

    /// Run the `finally` blocks between here and jump target `id`.
    fn leave_to(&mut self, id: ir::LoopId) {
        let depth = self.scopes.iter().rposition(|s| matches!(s, Scope::Jump(t) if *t == id)).expect("jump target");
        self.run_finally_until(depth + 1);
    }

    /// Run every `finally` in `scopes[depth..]`, innermost first, each with
    /// the handlers and scopes that surround its `try`.
    fn run_finally_until(&mut self, depth: usize) {
        let mut i = self.scopes.len();
        while i > depth {
            i -= 1;
            if let Scope::Finally { body, handlers, scopes } = &self.scopes[i] {
                let (body, handlers, scopes) = (body.clone(), *handlers, *scopes);
                let saved_handlers = self.handlers.split_off(handlers);
                let saved_scopes = self.scopes.split_off(scopes);
                self.stmts(&body);
                self.handlers.extend(saved_handlers);
                self.scopes.extend(saved_scopes);
            }
        }
    }

    fn try_stmt(
        &mut self,
        body: &[Stmt],
        catch: Option<&(Option<ir::LocalId>, Vec<Stmt>)>,
        finally: Option<&Vec<Stmt>>,
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
            self.hold(exc);
            self.stmts(f);
            self.release(exc);
            let (p, t) = exc.any();
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
        } else if self.locals[l] == Ty::Any {
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

    pub(crate) fn expr(&mut self, e: &Expr) -> V {
        match &e.kind {
            ExprKind::Num(v) => V::Num(self.b.ins().f64const(*v)),
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
            ExprKind::SetLocal(l, x) => {
                let v = self.expr(x);
                if let V::Any(p, t) = v {
                    // One reference for the local, one for the expression.
                    self.dup((p, t));
                }
                self.set_local(*l, v);
                v
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
                let mut vals = Vec::new();
                for &bl in boxes {
                    let LVar::Boxed { obj: (op, ot), .. } = self.vars[bl] else { unreachable!() };
                    let obj = (self.b.use_var(op), self.b.use_var(ot));
                    let slot = self.box_slot(bl);
                    let tag = self.b.ins().iconst(I64, TAG_FLOAT64);
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
                        V::Num(self.b.ins().fcvt_from_sint(F64, r))
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
                })
            }
            ExprKind::Binary(op, x, y) => {
                let a = self.expr(x).num();
                let c = self.expr(y).num();
                V::Num(self.binary(*op, a, c))
            }
            ExprKind::Compare(op, x, y) => {
                let a = self.expr(x);
                let c = self.expr(y);
                V::Bool(match (a, c) {
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
                    let args: Vec<BlockArg> = v.values().into_iter().map(BlockArg::Value).collect();
                    self.b.ins().jump(join, &args);
                }
                self.b.switch_to_block(join);
                let params = self.b.block_params(join).to_vec();
                from_values(e.ty, &params)
            }
            ExprKind::Call(id, args) => {
                let mut vals = Vec::new();
                let mut held = Vec::new();
                for a in args {
                    let v = self.expr(a);
                    self.hold(v);
                    held.push(v);
                    vals.extend(v.values());
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
                from_values(e.ty, &results)
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
            ExprKind::AnyCompare(op, x, y) => {
                let a = self.expr(x);
                self.hold(a);
                let c = self.expr(y);
                self.hold(c);
                let r = self.compare_any(*op, a.any(), c.any());
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
            ExprKind::SetCell(k, x) => {
                let v = self.expr(x);
                self.hold(v);
                let old = self.load_cell(*k);
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
                let obj = self.expr(o);
                self.hold(obj);
                let atom = self.atom(*a);
                let (p, t) = obj.any();
                let ctx = self.ctx;
                let r = self.helper("JS_GetProperty", &[ctx, p, t, atom]);
                self.check_value(r[1]);
                self.release(obj);
                self.free_v(obj);
                V::Any(r[0], r[1])
            }
            ExprKind::SetProp(o, a, x) => {
                let obj = self.expr(o);
                self.hold(obj);
                let v = self.expr(x);
                self.hold(v);
                let (vp, vt) = v.any();
                // One reference for the property, one for the expression.
                self.dup((vp, vt));
                let atom = self.atom(*a);
                let (op_, ot) = obj.any();
                let ctx = self.ctx;
                let r = self.helper("JS_SetProperty", &[ctx, op_, ot, atom, vp, vt]);
                self.check_int(r[0]);
                self.release(v);
                self.release(obj);
                self.free_v(obj);
                v
            }
            ExprKind::GetElem(o, k) => {
                let obj = self.expr(o);
                self.hold(obj);
                let key = self.expr(k);
                // The key is passed on (JS_GetPropertyValue frees it).
                let (op_, ot) = obj.any();
                let (kp, kt) = key.any();
                let ctx = self.ctx;
                let r = self.helper("JS_GetPropertyValue", &[ctx, op_, ot, kp, kt]);
                self.check_value(r[1]);
                self.release(obj);
                self.free_v(obj);
                V::Any(r[0], r[1])
            }
            ExprKind::SetElem(o, k, x) => {
                let obj = self.expr(o);
                self.hold(obj);
                let key = self.expr(k);
                self.hold(key);
                let v = self.expr(x);
                self.hold(v);
                let (vp, vt) = v.any();
                self.dup((vp, vt));
                let slot = self.values_slot(&[obj.any(), key.any(), (vp, vt)]);
                // tsaot_set_elem takes the key and one reference to the value.
                self.release(v);
                self.release(key);
                self.hold(v);
                let ctx = self.ctx;
                let r = self.helper("tsaot_set_elem", &[ctx, slot]);
                self.check_int(r[0]);
                self.release(v);
                self.release(obj);
                self.free_v(obj);
                v
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
            ExprKind::CallMethod(o, key, args) => {
                // The method is read before the arguments are evaluated.
                let obj = self.expr(o);
                self.hold(obj);
                let (op_, ot) = obj.any();
                let ctx = self.ctx;
                let m = match key {
                    Key::Atom(a) => {
                        let atom = self.atom(*a);
                        self.helper("JS_GetProperty", &[ctx, op_, ot, atom])
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
                self.release(obj);
                self.free_v(obj);
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
                let ctx = self.ctx;
                let o = self.helper("JS_NewObject", &[ctx]);
                self.check_value(o[1]);
                let obj = V::Any(o[0], o[1]);
                self.hold(obj);
                let slot = self.values_slot(&[(o[0], o[1])]);
                for (a, x) in props {
                    let (p, t) = self.expr(x).any();
                    let atom = self.atom(*a);
                    // Takes ownership of the value.
                    let r = self.helper("tsaot_define_field", &[ctx, slot, atom, p, t]);
                    self.check_int(r[0]);
                }
                self.release(obj);
                obj
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
        let r = self.helper("tsaot_call", &[ctx, slot, argc, argv]);
        self.check_value(r[1]);
        for v in held.into_iter().rev() {
            self.release(v);
            self.free_v(v);
        }
        self.release(f);
        self.free_v(f);
        V::Any(r[0], r[1])
    }

    /// Spill JS values into a fresh stack slot; returns its address.
    fn values_slot(&mut self, vals: &[(Value, Value)]) -> Value {
        let size = (vals.len().max(1) * SIZE) as u32;
        let slot = self.b.create_sized_stack_slot(StackSlotData::new(StackSlotKind::ExplicitSlot, size, 4));
        for (i, &(p, t)) in vals.iter().enumerate() {
            let off = (i * SIZE) as i32;
            self.b.ins().stack_store(self.ptr, p, slot, off);
            self.b.ins().stack_store(self.ptr, t, slot, off + TAG_OFFSET);
        }
        self.b.ins().stack_addr(self.ptr, slot, 0)
    }

    fn atom(&mut self, a: ir::AtomId) -> Value {
        let base = self.b.ins().symbol_value(self.ptr, self.env.atoms);
        self.b.ins().load(I32, Self::mem(), base, (a * 4) as i32)
    }

    /// A cell's value (borrowed); throws if it is uninitialized.
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
        let keep: Vec<BlockArg> = a.values().into_iter().map(BlockArg::Value).collect();
        match op {
            LogicalOp::And => self.b.ins().brif(truth, rhs, &[], join, &keep),
            LogicalOp::Or | LogicalOp::Nullish => self.b.ins().brif(truth, join, &keep, rhs, &[]),
        };
        self.b.switch_to_block(rhs);
        self.free_v(a);
        let c = self.expr(y);
        let args: Vec<BlockArg> = c.values().into_iter().map(BlockArg::Value).collect();
        self.b.ins().jump(join, &args);
        self.b.switch_to_block(join);
        let params = self.b.block_params(join).to_vec();
        from_values(ty, &params)
    }

    // ---- dynamic values ----

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

    /// Comparison of borrowed values. Fast path for two numbers.
    fn compare_any(&mut self, op: AnyCmpOp, a: (Value, Value), c: (Value, Value)) -> Value {
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
            let na = self.is_number(a.1);
            let nc = self.is_number(c.1);
            let both = self.b.ins().band(na, nc);
            let fast = self.b.create_block();
            self.b.ins().brif(both, fast, &[], slow, &[]);
            self.b.switch_to_block(fast);
            let x = self.number_of(a);
            let y = self.number_of(c);
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
        let r = self.helper("JS_BinaryOperator", &[ctx, code, a.0, a.1, c.0, c.1]);
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
                        return self.b.ins().fcvt_from_uint(F64, r);
                    }
                    _ => unreachable!(),
                };
                self.b.ins().fcvt_from_sint(F64, r)
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

    /// JS ToInt32. Exact for |x| < 2^63 via a saturating i64 conversion and
    /// truncation; everything else (huge values, infinities, NaN) goes to the
    /// runtime.
    fn toint32(&mut self, x: Value) -> Value {
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
