//! Experimental Cranelift-based JIT tier (work in progress).
//!
//! Staged plan: `planning/js-performance-roadmap/09-cranelift-jit.md`.
//!
//! Status: **narrow baseline tier**. The legacy per-opcode shim remains the
//! complete fallback, while eligible hot ordinary functions can execute
//! primitive arithmetic, dense numeric reads, monomorphic data loads, and
//! guarded ordinary calls as native Cranelift code.
//!
//! The tier is opt-in through [`Context::enable_jit`] and is gated behind the
//! `jit` feature. Unsupported operations and failed guards resume at an exact
//! interpreter bytecode boundary.

use crate::Context;
use crate::vm::CodeBlock;
use crate::vm::CompletionRecord;
use crate::vm::opcode::{Instruction, JIT_OP_SHIMS, Opcode};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{AbiParam, InstBuilder, types};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use rustc_hash::FxHashMap;
use std::time::Instant;

mod native;

/// High bit of a shim's `u64` return value: set means the op broke (a
/// [`CompletionRecord`] was stashed in `vm.jit_pending`); clear means continue,
/// with the low bits holding the new `frame.pc`.
pub(crate) const JIT_BREAK_BIT: u64 = 1 << 63;

/// Marker used by the native entry ABI. Legacy shim statuses are untagged
/// bytecode PCs, so a tagged status can be decoded without changing the shim
/// table while the native compiler is being introduced.
pub(crate) const JIT_EXIT_BIT: u64 = 1 << 62;
pub(crate) const JIT_GUARD_FAIL_BIT: u64 = 1 << 61;
const JIT_EXIT_KIND_MASK: u64 = 0xff;
const JIT_EXIT_PC_SHIFT: u32 = 8;

/// Kinds of exits from generated code.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JitExitKind {
    /// Resume the current frame in the interpreter at the encoded PC.
    Deopt = 1,
    /// Finish the current frame through the VM-owned return transition.
    Return = 2,
    /// Let the runtime perform a call/frame transition.
    Call = 3,
    /// A completion record has been stored in VM state.
    Completion = 4,
    /// A runtime budget or limit stopped native execution.
    Budget = 5,
}

impl JitExitKind {
    fn from_u8(kind: u8) -> Option<Self> {
        match kind {
            1 => Some(Self::Deopt),
            2 => Some(Self::Return),
            3 => Some(Self::Call),
            4 => Some(Self::Completion),
            5 => Some(Self::Budget),
            _ => None,
        }
    }
}

/// A decoded status returned by a native entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JitExit {
    pub(crate) kind: JitExitKind,
    pub(crate) pc: u32,
}

impl JitExit {
    /// Encode an explicit native exit. The PC is always the exact bytecode
    /// boundary at which the interpreter may resume.
    #[inline]
    pub(crate) const fn encode(kind: JitExitKind, pc: u32) -> u64 {
        JIT_EXIT_BIT | ((pc as u64) << JIT_EXIT_PC_SHIFT) | kind as u64
    }

    /// Decode an explicit native exit. Legacy shim statuses intentionally
    /// return `None` and continue to use their old PC/break protocol.
    #[inline]
    pub(crate) fn decode(status: u64) -> Option<Self> {
        if status & JIT_EXIT_BIT == 0 || status & JIT_BREAK_BIT != 0 {
            return None;
        }

        let kind = JitExitKind::from_u8((status & JIT_EXIT_KIND_MASK) as u8)?;
        let pc = (status >> JIT_EXIT_PC_SHIFT) as u32;
        Some(Self { kind, pc })
    }
}

/// Hotness thresholds used by the opt-in runtime tier.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct JitThresholds {
    /// Number of entries before an ordinary function is considered hot.
    pub function_entries: u32,
    /// Number of observed backward edges before a loop is considered hot.
    pub loop_backedges: u32,
}

impl Default for JitThresholds {
    fn default() -> Self {
        Self {
            function_entries: 64,
            loop_backedges: 256,
        }
    }
}

/// Runtime counters for the opt-in JIT tier.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct JitStats {
    /// Number of cache lookup requests.
    pub cache_requests: u64,
    /// Number of compiled-entry cache hits.
    pub cache_hits: u64,
    /// Number of compiled-entry cache misses.
    pub cache_misses: u64,
    /// Number of successful compilations.
    pub compilations: u64,
    /// Number of successful native baseline compilations.
    pub native_compilations: u64,
    /// Number of successful shim-fallback compilations.
    pub shim_compilations: u64,
    /// Number of compilation failures/rejections.
    pub compilation_failures: u64,
    /// Number of function entries observed by the tiering loop.
    pub function_entries: u64,
    /// Number of backward edges observed by the tiering loop.
    pub loop_backedges: u64,
    /// Number of native baseline entries invoked.
    pub native_entries: u64,
    /// Number of native entries that returned to the interpreter.
    pub deopts: u64,
    /// Nanoseconds spent compiling generated entries.
    pub compile_time_ns: u128,
}

#[derive(Clone, Copy, Debug, Default)]
struct Hotness {
    function_entries: u32,
    loop_backedges: u32,
}

#[derive(Clone, Copy)]
struct CachedEntry {
    entry: extern "C" fn(*mut Context) -> u64,
    native: bool,
}

/// If `instr` is a **same-frame** branch (no frame push), return its target
/// `pc`. The JIT can then lower it to a native edge to that target's block.
///
/// Safe-by-construction allowlist: only opcodes that set `frame.pc` within the
/// current frame and never push a new frame. Anything not listed (calls, `new`,
/// returns, `JumpTable`, generators, …) returns `None` and is handled by the
/// generic deopt-on-pc-change path — so a missing entry just costs a deopt, it
/// can never miscompile.
fn same_frame_jump_target(instr: &Instruction) -> Option<u32> {
    match instr {
        Instruction::Jump { address }
        | Instruction::JumpIfTrue { address, .. }
        | Instruction::JumpIfFalse { address, .. }
        | Instruction::JumpIfNotUndefined { address, .. }
        | Instruction::JumpIfNullOrUndefined { address, .. }
        | Instruction::JumpIfNotLessThan { address, .. }
        | Instruction::JumpIfNotLessThanOrEqual { address, .. }
        | Instruction::JumpIfNotGreaterThan { address, .. }
        | Instruction::JumpIfNotGreaterThanOrEqual { address, .. }
        | Instruction::JumpIfNotEqual { address, .. }
        | Instruction::LogicalAnd { address, .. }
        | Instruction::LogicalOr { address, .. }
        | Instruction::Coalesce { address, .. } => Some(address.as_u32()),
        _ => None,
    }
}

/// A JIT backend bound to the host machine.
///
/// Owns the [`JITModule`]; dropping it frees the emitted code, so callers must
/// keep it alive for as long as any compiled function pointer is in use. The
/// real tier will hold one of these per realm.
pub struct JitBackend {
    pub(super) module: JITModule,
    /// Monotonic counter for unique symbol names. `JITModule::declare_function`
    /// deduplicates by name, so reusing a fixed name (e.g. "`jit_codeblock`")
    /// across compilations makes the second `define_function` fail with
    /// `DuplicateDefinition`. Each compile gets a fresh name from this counter.
    pub(super) next_fn_id: u64,
    /// Compiled entries are scoped to this backend. The code block's debug ID
    /// is unique for the lifetime of the current thread, which is sufficient
    /// because a backend is not shared across threads or realms.
    cache: FxHashMap<u64, CachedEntry>,
    hotness: FxHashMap<u64, Hotness>,
    thresholds: JitThresholds,
    stats: JitStats,
}

impl std::fmt::Debug for JitBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitBackend").finish_non_exhaustive()
    }
}

impl JitBackend {
    /// Build a JIT backend configured for the host ISA.
    ///
    /// # Panics
    /// Panics if the host platform is not supported by Cranelift.
    #[must_use]
    pub fn new() -> Self {
        let mut flags = settings::builder();
        flags
            .set("use_colocated_libcalls", "false")
            .expect("valid flag");
        flags.set("is_pic", "false").expect("valid flag");
        let isa_builder = cranelift_native::builder().expect("host ISA must be supported");
        let isa = isa_builder
            .finish(settings::Flags::new(flags))
            .expect("valid ISA flags");
        let builder = JITBuilder::with_isa(isa, cranelift_module::default_libcall_names());
        Self {
            module: JITModule::new(builder),
            next_fn_id: 0,
            cache: FxHashMap::default(),
            hotness: FxHashMap::default(),
            thresholds: JitThresholds::default(),
            stats: JitStats::default(),
        }
    }

    /// Return a snapshot of the counters collected by this backend.
    #[must_use]
    pub const fn stats(&self) -> JitStats {
        self.stats
    }

    /// Configure the thresholds used by the opt-in tiering loop.
    pub fn set_thresholds(&mut self, thresholds: JitThresholds) {
        self.thresholds = thresholds;
    }

    /// Return the currently configured tiering thresholds.
    #[must_use]
    pub const fn thresholds(&self) -> JitThresholds {
        self.thresholds
    }

    /// Record an ordinary function entry for tiering.
    pub(crate) fn record_function_entry(&mut self, code: &CodeBlock) {
        self.stats.function_entries = self.stats.function_entries.saturating_add(1);
        let hotness = self.hotness.entry(code.debug_id).or_default();
        hotness.function_entries = hotness.function_entries.saturating_add(1);
    }

    /// Record a backward edge for tiering.
    pub(crate) fn record_loop_backedge(&mut self, code: &CodeBlock) {
        self.stats.loop_backedges = self.stats.loop_backedges.saturating_add(1);
        let hotness = self.hotness.entry(code.debug_id).or_default();
        hotness.loop_backedges = hotness.loop_backedges.saturating_add(1);
    }

    /// Record a native entry that returned to the interpreter.
    pub(crate) fn record_deopt(&mut self) {
        self.stats.deopts = self.stats.deopts.saturating_add(1);
    }

    /// Whether this code block has enough observed activity for compilation.
    #[must_use]
    pub(crate) fn is_hot(&self, code: &CodeBlock) -> bool {
        let Some(hotness) = self.hotness.get(&code.debug_id) else {
            return false;
        };

        hotness.function_entries >= self.thresholds.function_entries
            || hotness.loop_backedges >= self.thresholds.loop_backedges
    }

    /// Return a cached entry if one exists, compiling and caching it otherwise.
    fn cached_entry(&mut self, code: &CodeBlock) -> CachedEntry {
        self.stats.cache_requests = self.stats.cache_requests.saturating_add(1);

        if let Some(cached) = self.cache.get(&code.debug_id) {
            self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
            return *cached;
        }

        self.stats.cache_misses = self.stats.cache_misses.saturating_add(1);
        let started = Instant::now();
        let (entry, native) = self.compile_codeblock_with_kind(code);
        self.stats.compile_time_ns = self
            .stats
            .compile_time_ns
            .saturating_add(started.elapsed().as_nanos());
        self.stats.compilations = self.stats.compilations.saturating_add(1);
        if native {
            self.stats.native_compilations = self.stats.native_compilations.saturating_add(1);
        } else {
            self.stats.shim_compilations = self.stats.shim_compilations.saturating_add(1);
        }
        let cached = CachedEntry { entry, native };
        self.cache.insert(code.debug_id, cached);
        cached
    }

    /// Invoke a cached entry for the current frame. This is the shared runtime
    /// hook used by both the explicit API and the context-owned tier.
    pub(crate) fn invoke_cached_entry(&mut self, code: &CodeBlock, context: &mut Context) -> u64 {
        let cached = self.cached_entry(code);
        if cached.native {
            self.stats.native_entries = self.stats.native_entries.saturating_add(1);
        }
        // SAFETY: `context` is exclusively borrowed for the duration of the
        // native call, and the backend owns the generated code pointer.
        (cached.entry)(std::ptr::from_mut(context))
    }

    /// Invoke a cached entry and finish it through the existing interpreter
    /// transition machinery. The context-owned tier uses
    /// [`Self::invoke_cached_entry`] directly so it can continue its one-step
    /// scheduling loop after a deopt.
    pub(crate) fn run_cached_entry(
        &mut self,
        code: &CodeBlock,
        context: &mut Context,
    ) -> CompletionRecord {
        let status = self.invoke_cached_entry(code, context);

        if status & JIT_BREAK_BIT != 0 {
            return context
                .vm
                .jit_pending
                .take()
                .expect("a break status must have stashed a completion record");
        }

        if let Some(exit) = JitExit::decode(status) {
            if matches!(exit.kind, JitExitKind::Deopt) {
                self.record_deopt();
            }
        }

        // Legacy shim entries and explicit deopt entries both leave the VM at
        // an interpreter-visible program counter. The interpreter owns all
        // frame, exception, and return transitions.
        context.run_interpreter()
    }

    /// Allocate a process-unique-within-this-backend symbol name for a freshly
    /// compiled function. Prevents `DuplicateDefinition` when the same backend
    /// compiles more than one function (or the same `CodeBlock` twice).
    pub(super) fn next_fn_name(&mut self, prefix: &str) -> String {
        let id = self.next_fn_id;
        self.next_fn_id += 1;
        format!("{prefix}_{id}")
    }

    /// Compile a function `extern "C" fn(*mut Context) -> i64` whose body is a
    /// single indirect call to the given host `helper`, threading the context
    /// pointer through.
    ///
    /// This is the in-engine analogue of the spike's `compile_call_helper`, but
    /// the helper now operates on a real [`Context`]. It is the minimal proof
    /// that JIT-emitted native code can invoke `boa_engine` runtime routines —
    /// exactly how every lowered bytecode op will reach VM state in JIT-1.
    ///
    /// # Panics
    /// Panics if Cranelift codegen fails.
    #[must_use]
    pub fn compile_ctx_thunk(
        &mut self,
        helper: extern "C" fn(*mut Context) -> i64,
    ) -> extern "C" fn(*mut Context) -> i64 {
        let ptr = self.module.target_config().pointer_type();
        let mut ctx = self.module.make_context();
        let mut fctx = FunctionBuilderContext::new();

        ctx.func.signature.params.push(AbiParam::new(ptr));
        ctx.func.signature.returns.push(AbiParam::new(types::I64));

        {
            let mut bcx = FunctionBuilder::new(&mut ctx.func, &mut fctx);
            let block = bcx.create_block();
            bcx.append_block_params_for_function_params(block);
            bcx.switch_to_block(block);
            bcx.seal_block(block);
            let ctx_arg = bcx.block_params(block)[0];

            let mut sig = self.module.make_signature();
            sig.params.push(AbiParam::new(ptr));
            sig.returns.push(AbiParam::new(types::I64));
            let sigref = bcx.import_signature(sig);

            let helper_addr = bcx.ins().iconst(ptr, helper as usize as i64);
            let call = bcx.ins().call_indirect(sigref, helper_addr, &[ctx_arg]);
            let result = bcx.inst_results(call)[0];
            bcx.ins().return_(&[result]);
            bcx.finalize();
        }

        let name = self.next_fn_name("ctx_thunk");
        let id = self
            .module
            .declare_function(&name, Linkage::Export, &ctx.func.signature)
            .expect("declare");
        self.module.define_function(id, &mut ctx).expect("define");
        self.module.clear_context(&mut ctx);
        self.module.finalize_definitions().expect("finalize");

        let code = self.module.get_finalized_function(id);
        // SAFETY: the compiled function matches this signature, and `self`
        // (which owns the code) outlives the returned pointer by contract.
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(*mut Context) -> i64>(code) }
    }

    /// Compile a [`CodeBlock`] using the narrow native baseline lowering when
    /// its bytecode and frame metadata satisfy the native allowlist. The
    /// complete shim bridge remains the semantics-preserving fallback for
    /// unsupported or malformed blocks.
    ///
    /// # Panics
    /// Panics if Cranelift codegen fails.
    #[must_use]
    pub fn compile_codeblock(&mut self, code: &CodeBlock) -> extern "C" fn(*mut Context) -> u64 {
        self.compile_codeblock_with_kind(code).0
    }

    fn compile_codeblock_with_kind(
        &mut self,
        code: &CodeBlock,
    ) -> (extern "C" fn(*mut Context) -> u64, bool) {
        if let Some(native) = native::compile(self, code) {
            return (native, true);
        }

        (self.compile_shim_codeblock(code), false)
    }

    /// Compile a code block using the legacy shim bridge. This remains the
    /// complete-semantics fallback while the native allowlist grows.
    #[must_use]
    fn compile_shim_codeblock(&mut self, code: &CodeBlock) -> extern "C" fn(*mut Context) -> u64 {
        let ptr = self.module.target_config().pointer_type();

        // Walk the bytecode into (pc, opcode index, linear-next pc, jump target)
        // tuples, and map each instruction's pc to its index for jump edges.
        let bytes = &code.bytecode.bytes;
        let mut ops: Vec<(usize, usize, usize, Option<u32>)> = Vec::new();
        let mut pc_to_index: std::collections::HashMap<usize, usize> =
            std::collections::HashMap::new();
        let mut pc = 0usize;
        while pc < bytes.len() {
            let opcode = Opcode::decode(bytes[pc]);
            let (instruction, next) = code.bytecode.next_instruction(pc);
            let target = same_frame_jump_target(&instruction);
            pc_to_index.insert(pc, ops.len());
            ops.push((pc, opcode as usize, next, target));
            pc = next;
        }

        let mut cctx = self.module.make_context();
        let mut fctx = FunctionBuilderContext::new();
        cctx.func.signature.params.push(AbiParam::new(ptr));
        cctx.func.signature.returns.push(AbiParam::new(types::I64));

        {
            let mut bcx = FunctionBuilder::new(&mut cctx.func, &mut fctx);

            // The shared shim signature: extern "C" fn(*mut Context, u32) -> u64.
            let mut shim_sig = self.module.make_signature();
            shim_sig.params.push(AbiParam::new(ptr));
            shim_sig.params.push(AbiParam::new(types::I32));
            shim_sig.returns.push(AbiParam::new(types::I64));
            let shim_sigref = bcx.import_signature(shim_sig);

            let entry = bcx.create_block();
            bcx.append_block_params_for_function_params(entry);
            let op_blocks: Vec<_> = ops.iter().map(|_| bcx.create_block()).collect();
            let break_block = bcx.create_block();
            let deopt_block = bcx.create_block();
            bcx.append_block_param(deopt_block, types::I64);

            bcx.switch_to_block(entry);
            let ctx_val = bcx.block_params(entry)[0];
            if let Some(first) = op_blocks.first() {
                bcx.ins().jump(*first, &[]);
            } else {
                let zero = bcx.ins().iconst(types::I64, 0);
                bcx.ins().jump(deopt_block, &[zero.into()]);
            }

            for (i, &(op_pc, op_idx, linear_next, jump_target)) in ops.iter().enumerate() {
                bcx.switch_to_block(op_blocks[i]);

                // Bake the specific shim's address and call it directly.
                let shim_addr = JIT_OP_SHIMS[op_idx] as usize as i64;
                let shim_addr_val = bcx.ins().iconst(ptr, shim_addr);
                let pc_arg = bcx.ins().iconst(types::I32, op_pc as i64);
                let call = bcx
                    .ins()
                    .call_indirect(shim_sigref, shim_addr_val, &[ctx_val, pc_arg]);
                let status = bcx.inst_results(call)[0];

                // Break? (high bit set)
                #[allow(clippy::cast_possible_wrap)]
                let break_bit = bcx.ins().iconst(types::I64, JIT_BREAK_BIT as i64);
                let masked = bcx.ins().band(status, break_bit);
                let cont = bcx.create_block();
                bcx.ins().brif(masked, break_block, &[], cont, &[]);

                // Continue: where did `frame.pc` go?
                bcx.switch_to_block(cont);
                let fall_block = op_blocks.get(i + 1).copied();
                let lin = bcx.ins().iconst(types::I64, linear_next as i64);
                let is_linear = bcx.ins().icmp(IntCC::Equal, status, lin);

                // If this is a same-frame jump whose target is an instruction in
                // this CodeBlock, give it a native edge: pc == linear-next →
                // fall through; pc == target → branch to the target's block;
                // anything else → deopt. (Backward targets make loops run in
                // native code.) For non-jumps, only linear-next is native.
                // Carry the jump target's pc alongside its block so the branch
                // arms can use it without re-unwrapping `jump_target`.
                let target_block = jump_target.and_then(|t| {
                    pc_to_index
                        .get(&(t as usize))
                        .map(|&idx| (t, op_blocks[idx]))
                });

                match (fall_block, target_block) {
                    (Some(fall), Some((target_pc, tgt))) => {
                        let check_target = bcx.create_block();
                        bcx.ins().brif(is_linear, fall, &[], check_target, &[]);
                        bcx.switch_to_block(check_target);
                        let tpc = bcx.ins().iconst(types::I64, i64::from(target_pc));
                        let is_target = bcx.ins().icmp(IntCC::Equal, status, tpc);
                        bcx.ins()
                            .brif(is_target, tgt, &[], deopt_block, &[status.into()]);
                    }
                    (Some(fall), None) => {
                        bcx.ins()
                            .brif(is_linear, fall, &[], deopt_block, &[status.into()]);
                    }
                    (None, Some((target_pc, tgt))) => {
                        // Last instruction is a jump (e.g. a loop's trailing back-edge).
                        let tpc = bcx.ins().iconst(types::I64, i64::from(target_pc));
                        let is_target = bcx.ins().icmp(IntCC::Equal, status, tpc);
                        bcx.ins()
                            .brif(is_target, tgt, &[], deopt_block, &[status.into()]);
                    }
                    (None, None) => {
                        bcx.ins().jump(deopt_block, &[status.into()]);
                    }
                }
            }

            // break_block → return the break sentinel.
            bcx.switch_to_block(break_block);
            #[allow(clippy::cast_possible_wrap)]
            let sentinel = bcx.ins().iconst(types::I64, JIT_BREAK_BIT as i64);
            bcx.ins().return_(&[sentinel]);

            // deopt_block → return the pc-carrying status.
            bcx.switch_to_block(deopt_block);
            let status = bcx.block_params(deopt_block)[0];
            bcx.ins().return_(&[status]);

            bcx.seal_all_blocks();
            bcx.finalize();
        }

        let name = self.next_fn_name("jit_codeblock");
        let id = self
            .module
            .declare_function(&name, Linkage::Export, &cctx.func.signature)
            .expect("declare");
        self.module.define_function(id, &mut cctx).expect("define");
        self.module.clear_context(&mut cctx);
        self.module.finalize_definitions().expect("finalize");

        let code_ptr = self.module.get_finalized_function(id);
        // SAFETY: the compiled function matches this signature, and `self` owns
        // the code for as long as the returned pointer is used.
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(*mut Context) -> u64>(code_ptr) }
    }

    /// Compile `code` and run it against the current (already-entered) frame on
    /// `context`, returning the resulting [`CompletionRecord`].
    ///
    /// The caller must have pushed the frame for `code` (as the interpreter does
    /// before [`Context::run`]). If the JIT-compiled code deopts (hits any control
    /// flow), execution transparently continues in the interpreter from the
    /// current `frame.pc`, so the result is always correct.
    ///
    /// # Panics
    /// Panics if Cranelift codegen fails.
    #[must_use]
    pub(crate) fn run_codeblock(
        &mut self,
        code: &CodeBlock,
        context: &mut Context,
    ) -> CompletionRecord {
        self.run_cached_entry(code, context)
    }
}

impl Default for JitBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::JsValue;

    /// A host helper that drives real VM state: it pushes a value onto the VM
    /// stack and returns a sentinel. Reaching `context.vm.stack` proves the
    /// JIT-threaded pointer is a usable, real `Context`.
    extern "C" fn probe_push(ctx: *mut Context) -> i64 {
        // SAFETY: the test passes a pointer to a live `Context` and does not
        // alias it for the duration of this call.
        let context = unsafe { &mut *ctx };
        context.vm.stack.push(JsValue::new(7i32));
        42
    }

    #[test]
    fn jit_compiles_real_codeblock() {
        // Lower a real function's bytecode end-to-end. Reaching the end without
        // panicking proves the safe baseline compiler handles real opcode shapes
        // (operands, control flow, calls) — control flow simply lowers to deopt
        // edges. This does not execute the code (that needs frame setup / tiering,
        // the next step); it exercises the bytecode → Cranelift lowering.
        let mut context = Context::default();
        let src = "function add(a, b) { return a + b; }";
        let script = crate::Script::parse(crate::Source::from_bytes(src), None, &mut context)
            .expect("parse");
        let code = script.codeblock(&mut context).expect("codeblock");
        let mut backend = JitBackend::new();
        let _compiled = backend.compile_codeblock(&code);
    }

    #[test]
    fn jit_executes_script_matches_interpreter() {
        // End-to-end: run a real script through the JIT trampoline and confirm
        // the result matches the interpreter exactly. The JIT runs native code
        // for the prologue, deopts on the first control flow (the `add` call),
        // and the interpreter finishes — so this exercises the compiled code,
        // the break/deopt status protocol, and the trampoline's interpreter
        // hand-off, all producing the correct value.
        let src = "function add(a, b) { return a + b; } let r = add(2, 3) + 10; r";

        let mut c1 = Context::default();
        let s1 =
            crate::Script::parse(crate::Source::from_bytes(src), None, &mut c1).expect("parse");
        let interp = s1.evaluate(&mut c1).expect("interpret");

        let mut c2 = Context::default();
        let s2 =
            crate::Script::parse(crate::Source::from_bytes(src), None, &mut c2).expect("parse");
        let mut backend = JitBackend::new();
        let jit = s2.evaluate_jit(&mut c2, &mut backend).expect("jit");

        assert_eq!(jit.as_i32(), Some(15));
        assert_eq!(interp.as_i32(), jit.as_i32());
    }

    #[test]
    fn jit_backend_can_be_reused_across_compilations() {
        // Regression: `compile_codeblock` previously declared every emitted
        // function with the fixed name "jit_codeblock". Because
        // `JITModule::declare_function` dedups by name, the second compilation
        // on the same backend panicked in `define_function` with
        // `DuplicateDefinition`. A backend must compile many scripts.
        let mut backend = JitBackend::new();

        let mut c1 = Context::default();
        let s1 =
            crate::Script::parse(crate::Source::from_bytes("1 + 1"), None, &mut c1).expect("parse");
        let r1 = s1.evaluate_jit(&mut c1, &mut backend).expect("jit #1");
        assert_eq!(r1.as_i32(), Some(2));

        let mut c2 = Context::default();
        let s2 = crate::Script::parse(crate::Source::from_bytes("20 + 22"), None, &mut c2)
            .expect("parse");
        let r2 = s2
            .evaluate_jit(&mut c2, &mut backend)
            .expect("jit #2 (must not panic)");
        assert_eq!(r2.as_i32(), Some(42));
    }

    #[test]
    fn jit_backend_caches_codeblock_entries() {
        let mut context = Context::default();
        let script = crate::Script::parse(crate::Source::from_bytes("1 + 1"), None, &mut context)
            .expect("parse");
        let mut backend = JitBackend::new();

        assert_eq!(
            script
                .evaluate_jit(&mut context, &mut backend)
                .unwrap()
                .as_i32(),
            Some(2)
        );
        assert_eq!(
            script
                .evaluate_jit(&mut context, &mut backend)
                .unwrap()
                .as_i32(),
            Some(2)
        );

        let stats = backend.stats();
        assert_eq!(stats.cache_requests, 2);
        assert_eq!(stats.cache_misses, 1);
        assert_eq!(stats.cache_hits, 1);
        assert_eq!(stats.compilations, 1);
        assert_eq!(stats.native_entries, 2);
    }

    #[test]
    fn context_owned_jit_tiers_hot_function_entries() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function add(a, b) { return a + b; } let total = 0; for (let i = 0; i < 80; i++) { total = add(total, i); } total",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(3160));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.function_entries >= 2, "stats: {stats:?}");
        assert!(stats.compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_runs_native_integer_loop() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(n) { let total = 0; for (let i = 0; i < n; i++) { total = total + i; } return total; } let answer = 0; for (let j = 0; j < 80; j++) { answer = sum(10); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(45));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_runs_native_floating_point_loop() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(n) { let total = 0.5; for (let i = 0; i < n; i++) { total = total + 0.25; } return total; } let answer = 0; for (let j = 0; j < 80; j++) { answer = sum(10); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_number(), Some(3.0));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_runs_native_dense_integer_array_load() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(values, n) { let total = 0; for (let i = 0; i < n; i++) { total = total + values[i]; } return total; } let values = [1, 2, 3]; let answer = 0; for (let j = 0; j < 80; j++) { answer = sum(values, 3); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(6));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_runs_native_dense_floating_array_load() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(values, n) { let total = 0.5; for (let i = 0; i < n; i++) { total = total + values[i]; } return total; } let values = [1.25, 2.5, 3.75]; let answer = 0; for (let j = 0; j < 80; j++) { answer = sum(values, 3); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_number(), Some(8.0));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_runs_native_monomorphic_property_load() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(object, n) { let total = 0; for (let i = 0; i < n; i++) { total = total + object.value; } return total; } let object = { value: 3 }; let answer = 0; for (let j = 0; j < 80; j++) { answer = sum(object, 10); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(30));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_dispatches_native_ordinary_function_call() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function add(left, right) { return left + right; } function apply(function_value, left, right) { return function_value(left, right); } let answer = 0; for (let i = 0; i < 80; i++) { answer = apply(add, 20, 22); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(42));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 2, "stats: {stats:?}");
        assert!(stats.native_entries >= 2, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_deopts_non_ordinary_call_to_interpreter() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function add(left, right) { return left + right; } function apply(function_value, left, right) { return function_value(left, right); } let answer = 0; for (let i = 0; i < 80; i++) { answer = answer + apply(add, 20, 22); } answer = answer + apply(Math.max, 5, 4); answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(3365));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 2, "stats: {stats:?}");
        assert!(stats.deopts >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_deopts_property_shape_mismatch_to_interpreter() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(object, n) { let total = 0; for (let i = 0; i < n; i++) { total = total + object.value; } return total; } let first = { value: 3 }; let second = { value: 4, extra: 1 }; let answer = 0; for (let i = 0; i < 40; i++) { answer = answer + sum(first, 10); } for (let i = 0; i < 40; i++) { answer = answer + sum(second, 10); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(2800));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.deopts >= 1, "stats: {stats:?}");
    }

    #[test]
    fn jit_exit_round_trip() {
        for (kind, pc) in [
            (JitExitKind::Deopt, 0),
            (JitExitKind::Return, 17),
            (JitExitKind::Call, u32::MAX),
            (JitExitKind::Completion, 42),
            (JitExitKind::Budget, 99),
        ] {
            let status = JitExit::encode(kind, pc);
            assert_eq!(JitExit::decode(status), Some(JitExit { kind, pc }));
        }

        assert_eq!(JitExit::decode(7), None);
        assert_eq!(JitExit::decode(JIT_BREAK_BIT), None);
    }

    /// Run `src` through both the interpreter and the JIT and assert identical
    /// `i32` results. Exercises the JIT execution + deopt-to-interpreter hand-off
    /// across program shapes (the JIT deopts on control flow today, so these
    /// confirm the hand-off is correct before native loops/calls are added).
    fn assert_jit_matches_interp(src: &str, expected: i32) {
        let mut c1 = Context::default();
        let s1 =
            crate::Script::parse(crate::Source::from_bytes(src), None, &mut c1).expect("parse");
        let interp = s1.evaluate(&mut c1).expect("interpret");

        let mut c2 = Context::default();
        let s2 =
            crate::Script::parse(crate::Source::from_bytes(src), None, &mut c2).expect("parse");
        let mut backend = JitBackend::new();
        let jit = s2.evaluate_jit(&mut c2, &mut backend).expect("jit");

        assert_eq!(
            interp.as_i32(),
            Some(expected),
            "interpreter result for: {src}"
        );
        assert_eq!(jit.as_i32(), Some(expected), "jit result for: {src}");
    }

    #[test]
    fn jit_deopt_handoff_across_shapes() {
        // Loop (backward jumps).
        assert_jit_matches_interp("let s = 0; for (let i = 0; i < 10; i++) { s += i; } s", 45);
        // Conditional (forward jumps).
        assert_jit_matches_interp("let x = 7; let y = x > 5 ? 100 : 1; y", 100);
        // Nested calls + recursion.
        assert_jit_matches_interp(
            "function fib(n){ return n < 2 ? n : fib(n-1) + fib(n-2); } fib(10)",
            55,
        );
        // While loop accumulating.
        assert_jit_matches_interp("let n = 0, t = 0; while (n < 100) { t += n; n++; } t", 4950);
    }

    /// Honest first JIT perf measurement on a hot ordinary-function loop. Run with:
    /// `cargo test -p boa_engine --features jit --release jit_loop_perf -- --ignored --nocapture`
    #[test]
    #[ignore = "perf measurement; run manually with --release --nocapture"]
    fn jit_loop_perf() {
        let src = "function sum(n) { var total = 0; for (var i = 0; i < n; i++) { total = total + i; } return total; } var answer = 0; for (var j = 0; j < 1000; j++) { answer = answer + sum(1000); } answer";

        let time = |jit: bool| -> (i32, std::time::Duration, Option<JitStats>) {
            let mut c = Context::default();
            let script =
                crate::Script::parse(crate::Source::from_bytes(src), None, &mut c).unwrap();
            if jit {
                c.enable_jit();
            }
            // Warm up compilation/caches by evaluating once via the chosen path.
            drop(script.evaluate(&mut c).unwrap());
            let start = Instant::now();
            let v = script.evaluate(&mut c).unwrap();
            (v.as_i32().unwrap_or(0), start.elapsed(), c.jit_stats())
        };

        let (vi, ti, _) = time(false);
        let (vj, tj, stats) = time(true);
        assert_eq!(vi, vj, "jit and interpreter must agree");
        eprintln!(
            "jit_loop_perf: interpreter={:?} jit={:?} ratio={:.3} stats={stats:?} (result={vi})",
            ti,
            tj,
            tj.as_secs_f64() / ti.as_secs_f64()
        );
    }

    #[test]
    fn jit_drives_real_context() {
        let mut context = Context::default();
        let mut backend = JitBackend::new();
        let thunk = backend.compile_ctx_thunk(probe_push);

        // Run the JIT-compiled native code against the real Context.
        let reported = thunk(std::ptr::from_mut(&mut context));

        // The JIT'd code called our helper (returns the sentinel)...
        assert_eq!(reported, 42);
        // ...and the helper mutated the real VM stack (value is observable).
        let top = context.vm.stack.pop();
        assert_eq!(top.as_i32(), Some(7));
    }
}
