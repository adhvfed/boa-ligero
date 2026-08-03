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
use crate::builtins::function::OrdinaryFunction;
use crate::vm::CodeBlock;
use crate::vm::CompletionRecord;
use crate::vm::opcode::{Instruction, InstructionIterator, JIT_OP_SHIMS, Opcode};

use cranelift_codegen::ir::condcodes::IntCC;
use cranelift_codegen::ir::{AbiParam, InstBuilder, types};
use cranelift_codegen::settings::{self, Configurable};
use cranelift_frontend::{FunctionBuilder, FunctionBuilderContext};
use cranelift_jit::{JITBuilder, JITModule};
use cranelift_module::{Linkage, Module};
use rustc_hash::FxHashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

mod native;

static NEXT_BACKEND_ID: AtomicU64 = AtomicU64::new(1);

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
const JIT_EXIT_REASON_SHIFT: u32 = 8;
const JIT_EXIT_PC_SHIFT: u32 = 16;

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
    /// A loop artifact rejected the current frame before charging or changing
    /// any interpreter-visible state.
    EntryRejected = 6,
    /// A loop artifact completed a taken external branch and materialized the
    /// exact post-effect interpreter continuation.
    Continuation = 7,
}

impl JitExitKind {
    fn from_u8(kind: u8) -> Option<Self> {
        match kind {
            1 => Some(Self::Deopt),
            2 => Some(Self::Return),
            3 => Some(Self::Call),
            4 => Some(Self::Completion),
            5 => Some(Self::Budget),
            6 => Some(Self::EntryRejected),
            7 => Some(Self::Continuation),
            _ => None,
        }
    }
}

/// A decoded status returned by a native entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JitExit {
    pub(crate) kind: JitExitKind,
    pub(crate) reason: JitExitReason,
    pub(crate) pc: u32,
}

impl JitExit {
    /// Encode an explicit native exit with a diagnostic reason.
    #[inline]
    pub(crate) const fn encode_with_reason(
        kind: JitExitKind,
        reason: JitExitReason,
        pc: u32,
    ) -> u64 {
        JIT_EXIT_BIT
            | ((pc as u64) << JIT_EXIT_PC_SHIFT)
            | ((reason as u64) << JIT_EXIT_REASON_SHIFT)
            | kind as u64
    }

    /// Decode an explicit native exit. Legacy shim statuses intentionally
    /// return `None` and continue to use their old PC/break protocol.
    #[inline]
    pub(crate) fn decode(status: u64) -> Option<Self> {
        if status & JIT_EXIT_BIT == 0 || status & JIT_BREAK_BIT != 0 {
            return None;
        }

        let kind = JitExitKind::from_u8((status & JIT_EXIT_KIND_MASK) as u8)?;
        let reason = JitExitReason::from_u8(((status >> JIT_EXIT_REASON_SHIFT) & 0xff) as u8)?;
        let pc = (status >> JIT_EXIT_PC_SHIFT) as u32;
        Some(Self { kind, reason, pc })
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
    /// Reserved count of fatal compilation failures. Native eligibility
    /// rejections that successfully select the shim fallback are not failures.
    /// The current compiler reports fatal code-generation failures by panic,
    /// so this counter remains zero.
    pub compilation_failures: u64,
    /// Number of function entries observed by the tiering loop.
    pub function_entries: u64,
    /// Number of backward edges observed by the tiering loop.
    pub loop_backedges: u64,
    /// Number of code blocks that crossed either configured hotness threshold.
    pub hotness_threshold_crossings: u64,
    /// Number of loop backedges that bypassed code-block hotness updates after
    /// their frame had already observed a hot code block.
    pub saturated_loop_backedges: u64,
    /// Number of hot nonzero-PC frames handed to dormant interpreter dispatch
    /// after proving that they cannot branch back to PC zero.
    pub dormant_loop_frames: u64,
    /// Number of native baseline entries invoked.
    pub native_entries: u64,
    /// Number of native entries that returned to the interpreter.
    pub deopts: u64,
    /// Number of native call exits handed to the general VM scheduler.
    pub scheduler_call_exits: u64,
    /// Number of static context-tier admission decisions that kept a code
    /// block on the interpreter path.
    pub admission_denials: u64,
    /// Nanoseconds spent compiling generated entries.
    pub compile_time_ns: u128,
    /// Fixed-size counters for the loop-OSR tier. These remain zero until the
    /// scheduler slice begins observing typed loop regions.
    pub osr: JitOsrCounters,
}

/// Uniform numeric representation selected for one loop-OSR artifact.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JitOsrRepresentation {
    /// Every live value is guarded as an exact Boa integer.
    I32,
    /// Every live value is guarded as a JavaScript Number and represented as
    /// an IEEE-754 double, including integer-valued Numbers.
    F64,
}

/// Why a loop region was permanently rejected before code generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JitOsrRejectionReason {
    /// The code-block kind, handlers, or register bound is ineligible.
    IneligibleCodeBlock,
    /// Header or latch is not a decoded ordered instruction boundary.
    InvalidBoundary,
    /// The candidate exceeds the hard decoded-instruction bound.
    RegionTooLarge,
    /// The region contains an opcode outside the first numeric allowlist.
    UnsupportedRegionOpcode,
    /// The region does not have the single canonical-latch CFG shape.
    InvalidControlFlow,
    /// The external edge does not reach the bounded return continuation.
    UnsupportedContinuation,
    /// An I32 key was requested for a region that statically requires F64.
    RepresentationMismatch,
    /// Entry or path-specific exit liveness could not prove a value source.
    UnprovenValue,
    /// Native IR construction rejected an otherwise planned region.
    Lowering,
}

/// Why new loop-OSR compilation was suppressed by a backend-wide bound.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JitOsrSuppressionReason {
    /// The backend already retains its maximum 64 exact region keys.
    RegionCapacity,
    /// Accounted emitted loop code reached the backend circuit breaker.
    CodeBytes,
    /// A prior synchronous loop compilation exceeded the time breaker.
    CompileTime,
}

/// Fixed rejection counts, split by a bounded source-free reason taxonomy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct JitOsrRejectionCounters {
    /// Rejections for function kind, handler metadata, or register bounds.
    pub ineligible_code_block: u64,
    /// Rejections for invalid header or latch boundaries.
    pub invalid_boundary: u64,
    /// Rejections for regions over the instruction bound.
    pub region_too_large: u64,
    /// Rejections for opcodes outside the first numeric allowlist.
    pub unsupported_region_opcode: u64,
    /// Rejections for CFGs outside the canonical single-latch shape.
    pub invalid_control_flow: u64,
    /// Rejections for external continuations outside the bounded epilogue.
    pub unsupported_continuation: u64,
    /// Rejections for a representation incompatible with static constants.
    pub representation_mismatch: u64,
    /// Rejections for entry or exit values without a proven source.
    pub unproven_value: u64,
    /// Rejections from native IR construction.
    pub lowering: u64,
}

impl JitOsrRejectionCounters {
    fn record(&mut self, reason: JitOsrRejectionReason) {
        let counter = match reason {
            JitOsrRejectionReason::IneligibleCodeBlock => &mut self.ineligible_code_block,
            JitOsrRejectionReason::InvalidBoundary => &mut self.invalid_boundary,
            JitOsrRejectionReason::RegionTooLarge => &mut self.region_too_large,
            JitOsrRejectionReason::UnsupportedRegionOpcode => &mut self.unsupported_region_opcode,
            JitOsrRejectionReason::InvalidControlFlow => &mut self.invalid_control_flow,
            JitOsrRejectionReason::UnsupportedContinuation => &mut self.unsupported_continuation,
            JitOsrRejectionReason::RepresentationMismatch => &mut self.representation_mismatch,
            JitOsrRejectionReason::UnprovenValue => &mut self.unproven_value,
            JitOsrRejectionReason::Lowering => &mut self.lowering,
        };
        *counter = counter.saturating_add(1);
    }
}

/// Fixed suppression counts for the three backend-wide OSR circuit breakers.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct JitOsrSuppressionCounters {
    /// Unseen observations suppressed after the 64-key table filled.
    pub region_capacity: u64,
    /// New compilations suppressed by accounted emitted loop code.
    pub code_bytes: u64,
    /// New compilations suppressed after a slow synchronous compilation.
    pub compile_time: u64,
}

impl JitOsrSuppressionCounters {
    fn record(&mut self, reason: JitOsrSuppressionReason) {
        let counter = match reason {
            JitOsrSuppressionReason::RegionCapacity => &mut self.region_capacity,
            JitOsrSuppressionReason::CodeBytes => &mut self.code_bytes,
            JitOsrSuppressionReason::CompileTime => &mut self.compile_time,
        };
        *counter = counter.saturating_add(1);
    }
}

/// Fixed-size source-free loop-OSR counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct JitOsrCounters {
    /// Exact typed-region lookup requests.
    pub cache_requests: u64,
    /// Lookups for already retained exact keys.
    pub cache_hits: u64,
    /// Lookups for previously unseen exact keys.
    pub cache_misses: u64,
    /// Region keys that reached their independent backedge threshold.
    pub hotness_crossings: u64,
    /// Synchronous native loop compilation attempts.
    pub compile_attempts: u64,
    /// Successful native loop compilations.
    pub compilations: u64,
    /// Native loop entries invoked by the scheduler.
    pub entries: u64,
    /// Guarded loop entries rejected before any native effect or charge.
    pub entry_rejections: u64,
    /// Post-effect native loop exits resumed through a proven continuation.
    pub continuations: u64,
    /// Pre-effect native loop guards replayed in the interpreter.
    pub deopts: u64,
    /// Aggregate synchronous native loop compilation time.
    pub compile_time_ns: u128,
    /// Accounted emitted native loop code bytes.
    pub code_bytes: usize,
    /// Permanent planner/codegen rejections by fixed reason.
    pub rejections: JitOsrRejectionCounters,
    /// Backend-wide suppression observations by fixed circuit breaker.
    pub suppressions: JitOsrSuppressionCounters,
}

/// Schema version for [`JitDiagnosticSnapshot`].
pub const JIT_DIAGNOSTIC_SCHEMA_VERSION: u32 = 8;

/// Hard retention cap for each detailed JIT diagnostic record class.
///
/// Runtime callers may request a lower bound, but cannot turn diagnostics into
/// an unbounded page-controlled allocation.
pub const MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND: usize = 4_096;

/// Bounded record limits for opt-in detailed JIT diagnostics.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct JitDiagnosticLimits {
    /// Maximum number of compilation records retained by a context.
    pub compile_records: usize,
    /// Maximum number of function-entry admission records retained by a
    /// context.
    pub admission_records: usize,
    /// Maximum number of distinct exit records retained by a context.
    pub exit_records: usize,
    /// Maximum number of distinct interpreted call-site records retained by a
    /// context.
    pub call_records: usize,
    /// Maximum number of distinct interpreted loop-backedge records retained
    /// by a context.
    pub loop_records: usize,
    /// Maximum number of distinct interpreted storage-read records retained
    /// by a context.
    pub storage_records: usize,
}

impl JitDiagnosticLimits {
    fn bounded(self) -> Self {
        Self {
            compile_records: self
                .compile_records
                .min(MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND),
            admission_records: self
                .admission_records
                .min(MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND),
            exit_records: self.exit_records.min(MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND),
            call_records: self.call_records.min(MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND),
            loop_records: self.loop_records.min(MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND),
            storage_records: self
                .storage_records
                .min(MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND),
        }
    }
}

impl Default for JitDiagnosticLimits {
    fn default() -> Self {
        Self {
            compile_records: 256,
            admission_records: 256,
            exit_records: 256,
            call_records: 256,
            loop_records: 256,
            storage_records: 256,
        }
    }
}

/// Why context-tier function-entry admission allowed or denied a code block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JitAdmissionReason {
    /// A fully native body contains a validated backward branch.
    AllowedBackwardBranch,
    /// A fully native straight-line body meets the measured work floor.
    AllowedStraightLineWork,
    /// Static native eligibility rejected the body before code generation.
    DeniedNativeIneligible,
    /// The body contains a call, but compiled callers cannot yet resume after
    /// the scheduler transition.
    DeniedCallBoundary,
    /// A fully native straight-line body is below the measured work floor.
    DeniedStraightLineTooSmall,
}

/// Artifact selected for a JIT compilation request.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JitCompileOutcome {
    /// The narrow native compiler accepted the complete code block.
    Native,
    /// The complete-semantics opcode-shim fallback was emitted.
    Shim,
}

/// Why the narrow native compiler could not accept a code block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JitCompileBlockerKind {
    /// The function kind is outside the ordinary-function baseline contract.
    FunctionKind,
    /// The code block contains exception-handler metadata.
    ExceptionHandlers,
    /// The register file exceeds the current native compiler bound.
    RegisterLimit,
    /// The code block contains no instructions.
    EmptyCodeBlock,
    /// Decoding produced the same bytecode boundary more than once.
    DuplicateInstructionBoundary,
    /// The first opcode outside the native allowlist was encountered.
    UnsupportedOpcode,
    /// A branch target was not a decoded instruction boundary.
    InvalidBranchTarget,
    /// Register representation analysis could not produce a safe map.
    RegisterAnalysis,
    /// Native IR construction or code generation rejected the otherwise
    /// eligible block.
    Lowering,
}

/// Why generated code returned to the VM.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JitExitReason {
    /// The exit did not yet carry a more specific reason.
    #[default]
    Unknown = 0,
    /// The generated entry's frame or budget-mode guard failed.
    EntryGuard = 1,
    /// A function argument did not match the numeric specialization.
    ArgumentType = 2,
    /// A VM operand-stack value did not match the numeric specialization.
    StackType = 3,
    /// Dense indexed storage, bounds, hole, or element representation changed.
    DenseElement = 4,
    /// Named-property shape, slot, or representation changed.
    NamedProperty = 5,
    /// The ordinary call target did not match its monomorphic feedback.
    CallTarget = 6,
    /// An integer result left the native `i32` representation.
    IntegerOverflow = 7,
    /// The VM scheduler owns the next call/frame transition.
    Scheduler = 8,
    /// The shared VM return transition completed the native frame.
    Return = 9,
    /// Native instruction or loop accounting exhausted a runtime limit.
    RuntimeLimit = 10,
    /// A helper produced a JavaScript exception.
    Exception = 11,
    /// A declarative binding could not be read with the compiled entry's
    /// stable locator and value representation.
    BindingRead = 12,
    /// A native loop completed its single path-proven external edge.
    LoopExit = 13,
}

impl JitExitReason {
    const fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Unknown),
            1 => Some(Self::EntryGuard),
            2 => Some(Self::ArgumentType),
            3 => Some(Self::StackType),
            4 => Some(Self::DenseElement),
            5 => Some(Self::NamedProperty),
            6 => Some(Self::CallTarget),
            7 => Some(Self::IntegerOverflow),
            8 => Some(Self::Scheduler),
            9 => Some(Self::Return),
            10 => Some(Self::RuntimeLimit),
            11 => Some(Self::Exception),
            12 => Some(Self::BindingRead),
            13 => Some(Self::LoopExit),
            _ => None,
        }
    }
}

/// VM-facing category of a detailed native exit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JitDiagnosticExitKind {
    /// Resume the current frame in the interpreter.
    Deopt,
    /// Let the VM perform a call/frame transition.
    Call,
    /// The generated frame returned normally.
    Return,
    /// A pending completion record ended native execution.
    Completion,
    /// A finite runtime budget ended native execution.
    Budget,
    /// A loop entry guard rejected the current frame before native effects.
    EntryRejected,
    /// A native loop materialized a post-effect interpreter continuation.
    Continuation,
}

impl From<JitExitKind> for JitDiagnosticExitKind {
    fn from(value: JitExitKind) -> Self {
        match value {
            JitExitKind::Deopt => Self::Deopt,
            JitExitKind::Call => Self::Call,
            JitExitKind::Return => Self::Return,
            JitExitKind::Completion => Self::Completion,
            JitExitKind::Budget => Self::Budget,
            JitExitKind::EntryRejected => Self::EntryRejected,
            JitExitKind::Continuation => Self::Continuation,
        }
    }
}

/// One aggregated, source-free native exit site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct JitExitRecord {
    /// Runtime-local code-block identity.
    pub code_id: u64,
    /// Entry PC of the compiled artifact. Phase 1 uses zero.
    pub entry_pc: u32,
    /// Exact interpreter-visible exit PC.
    pub pc: u32,
    /// VM-facing exit category.
    pub kind: JitDiagnosticExitKind,
    /// Guard, transition, or completion reason.
    pub reason: JitExitReason,
    /// Number of exits observed at this site.
    pub count: u64,
    /// Aggregate wall time spent inside native entries ending at this site.
    pub native_ns: u128,
}

/// One bounded, source-free compilation diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct JitCompileRecord {
    /// Runtime-local code-block identity. This is not stable across processes.
    pub code_id: u64,
    /// Entry bytecode PC. Phase 1 entries always begin at zero.
    pub entry_pc: u32,
    /// Whether the selected artifact charges a finite instruction budget.
    pub budgeted: bool,
    /// Artifact selected for the cache entry.
    pub outcome: JitCompileOutcome,
    /// First native-eligibility blocker, if the shim was selected.
    pub blocker: Option<JitCompileBlockerKind>,
    /// Debug name of the first blocking opcode. This is a static opcode name,
    /// never source text or a property value.
    pub first_blocking_opcode: Option<String>,
    /// PC of the first blocking opcode or malformed edge.
    pub first_blocking_pc: Option<u32>,
    /// Instructions preceding the first blocker that are individually in the
    /// native allowlist. This is eligibility coverage, not executed coverage.
    pub supported_prefix_instructions: u32,
    /// Instructions in the emitted native artifact, or zero for a shim.
    pub native_instructions: u32,
    /// Static backward branches in a fully accepted native code block.
    pub native_backward_branches: u32,
    /// Static call instructions in a fully accepted native code block.
    pub native_call_instructions: u32,
    /// Static property-read instructions in a fully accepted native code block.
    pub native_property_instructions: u32,
    /// Total decoded bytecode instructions in the code block.
    pub bytecode_instructions: u32,
    /// Wall-clock nanoseconds spent selecting and compiling the artifact.
    pub compile_ns: u128,
    /// Machine-code bytes emitted for the selected artifact.
    pub code_bytes: usize,
}

/// One bounded, source-free context-tier admission decision.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct JitAdmissionRecord {
    /// Runtime-local code-block identity. This is not stable across processes.
    pub code_id: u64,
    /// Whether the context tier may compile this function entry.
    pub allowed: bool,
    /// Static reason for the decision.
    pub reason: JitAdmissionReason,
    /// Whether denied calls may use the frame-change interpreter fast path.
    pub leaf_fast_path: bool,
    /// First native-eligibility blocker for an ineligible body.
    pub blocker: Option<JitCompileBlockerKind>,
    /// Debug name of the first blocking opcode, never source or property data.
    pub first_blocking_opcode: Option<String>,
    /// PC of the first blocking opcode or malformed edge.
    pub first_blocking_pc: Option<u32>,
    /// Instructions preceding the first blocker that are individually in the
    /// native allowlist.
    pub supported_prefix_instructions: u32,
    /// Total decoded bytecode instructions available to admission.
    pub bytecode_instructions: u32,
    /// Static backward branches in a fully native body.
    pub native_backward_branches: u32,
    /// Static call instructions in a fully native body.
    pub native_call_instructions: u32,
    /// Static property-read instructions in a fully native body.
    pub native_property_instructions: u32,
}

/// One bounded, source-free interpreted call-site record.
///
/// The target identity used to classify first/same/changed observations is
/// retained only inside the context's bounded diagnostic state. It is never
/// included in this public snapshot.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct JitCallSiteRecord {
    /// Runtime-local caller code-block identity.
    pub caller_code_id: u64,
    /// Bytecode PC of the interpreted `Call` instruction.
    pub pc: u32,
    /// Total calls observed at this site.
    pub calls: u64,
    /// Calls whose current target satisfies the narrow ordinary-function
    /// predicate used by the existing native call lowering.
    pub ordinary_calls: u64,
    /// Calls to every other target, including values that later throw as
    /// non-callable and native, proxy, bound, or constructor targets.
    pub non_ordinary_calls: u64,
    /// First retained ordinary-target observation at this site.
    pub first_ordinary_target_calls: u64,
    /// Ordinary calls matching the last retained target at this site.
    pub same_ordinary_target_calls: u64,
    /// Ordinary calls differing from the last retained target at this site.
    pub changed_ordinary_target_calls: u64,
    /// Ordinary calls whose target already had a cached native entry for the
    /// current instruction-budget mode.
    pub cached_native_target_calls: u64,
    /// Ordinary calls whose target already had a cached shim entry for the
    /// current instruction-budget mode.
    pub cached_shim_target_calls: u64,
}

/// One bounded, source-free interpreted loop-backedge record.
///
/// Static candidacy means only that the observed loop range satisfies the
/// deliberately narrow Phase 2 opcode/metadata screen. It is not an OSR
/// compilation promise: live-state materialization, a typed nonzero-PC cache
/// key, and the entry/deopt ABI remain separate reviewed work.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct JitLoopSiteRecord {
    /// Runtime-local code-block identity.
    pub code_id: u64,
    /// Bytecode PC reached by the backward edge.
    pub header_pc: u32,
    /// Bytecode PC of the branch that produced the backward edge.
    pub backedge_pc: u32,
    /// Total interpreted backedges observed at this site.
    pub backedges: u64,
    /// Backedges that made the code block cross a configured hotness
    /// threshold. A code block can cross at most once per backend generation.
    pub hotness_crossings: u64,
    /// Backedges observed after the frame's native-entry decision was closed.
    pub closed_entry_backedges: u64,
    /// Whether the observed loop range passes the conservative static OSR
    /// screen described above.
    pub static_osr_candidate: bool,
    /// Static blocker when the observed loop range is not a candidate.
    pub static_osr_blocker: Option<JitCompileBlockerKind>,
    /// Static opcode name for the first blocker, never source text.
    pub first_blocking_opcode: Option<String>,
    /// Bytecode PC of the first blocker.
    pub first_blocking_pc: Option<u32>,
    /// Number of decoded instructions in the observed loop range.
    pub region_instructions: u32,
}

/// Interpreted storage-read shape observed at one bytecode site.
#[derive(Clone, Copy, Debug, Default, Hash, PartialEq, Eq, PartialOrd, Ord, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JitStorageSiteKind {
    /// Static-name property read backed by Boa's named inline cache.
    Named,
    /// Numeric indexed read eligible for Boa's dense-element inline cache.
    Dense,
    /// Computed property read outside the narrow dense numeric shape.
    #[default]
    Computed,
    /// Specialized `length` read, which has no named/dense cache record.
    Length,
}

/// One bounded, source-free interpreted storage-read record.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct JitStorageSiteRecord {
    /// Runtime-local code-block identity.
    pub code_id: u64,
    /// Bytecode PC of the interpreted read.
    pub pc: u32,
    /// Coarse operation/receiver shape; never a property name or key value.
    pub kind: JitStorageSiteKind,
    /// Total reads observed at this site.
    pub executions: u64,
    /// Reads for which the existing named/dense inline cache matched before
    /// the opcode executed.
    pub inline_cache_hits: u64,
    /// Reads for which the applicable inline cache did not match.
    pub inline_cache_misses: u64,
    /// Reads whose coarse shape has no applicable named/dense cache.
    pub inline_cache_not_applicable: u64,
}

/// Fixed aggregate counters produced only by diagnostic native artifacts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct JitNativeStorageRecord {
    /// Successful named-property guards.
    pub named_guard_hits: u64,
    /// Failed named-property guards that returned to the interpreter.
    pub named_guard_misses: u64,
    /// Named-property helper loads following successful guards.
    pub named_loads: u64,
    /// Successful dense-element guards.
    pub dense_guard_hits: u64,
    /// Failed dense-element guards that returned to the interpreter.
    pub dense_guard_misses: u64,
    /// Dense-element helper loads following successful guards.
    pub dense_loads: u64,
}

impl JitNativeStorageRecord {
    fn merge(&mut self, other: Self) {
        self.named_guard_hits = self.named_guard_hits.saturating_add(other.named_guard_hits);
        self.named_guard_misses = self
            .named_guard_misses
            .saturating_add(other.named_guard_misses);
        self.named_loads = self.named_loads.saturating_add(other.named_loads);
        self.dense_guard_hits = self.dense_guard_hits.saturating_add(other.dense_guard_hits);
        self.dense_guard_misses = self
            .dense_guard_misses
            .saturating_add(other.dense_guard_misses);
        self.dense_loads = self.dense_loads.saturating_add(other.dense_loads);
    }
}

/// Stable snapshot of opt-in detailed JIT diagnostics.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct JitDiagnosticSnapshot {
    /// Version of the public record schema.
    pub schema_version: u32,
    /// Effective hard-bounded retention limits for this snapshot.
    pub limits: JitDiagnosticLimits,
    /// Compilation records in deterministic cache-key order.
    pub compile_records: Vec<JitCompileRecord>,
    /// Function-entry admission decisions in runtime-local code-ID order.
    pub admission_records: Vec<JitAdmissionRecord>,
    /// Aggregated native exits in deterministic key order.
    pub exit_records: Vec<JitExitRecord>,
    /// Aggregated interpreted calls in deterministic caller/PC order.
    pub call_records: Vec<JitCallSiteRecord>,
    /// Aggregated interpreted loop backedges in deterministic code/header/
    /// backedge order.
    pub loop_records: Vec<JitLoopSiteRecord>,
    /// Aggregated interpreted storage reads in deterministic code/PC/kind
    /// order.
    pub storage_records: Vec<JitStorageSiteRecord>,
    /// Fixed native guard/load aggregates from diagnostic artifacts.
    pub native_storage: JitNativeStorageRecord,
    /// Fixed source-free loop-OSR aggregates collected while diagnostics were
    /// enabled. No per-site value or source information is retained here.
    pub osr: JitOsrCounters,
    /// Compilation records omitted after reaching the configured bound.
    pub dropped_compile_records: u64,
    /// Admission records omitted after reaching the configured bound.
    pub dropped_admission_records: u64,
    /// Exit records omitted after reaching the configured bound.
    pub dropped_exit_records: u64,
    /// Call observations omitted because their site was not retained after
    /// reaching the configured bound.
    pub dropped_call_observations: u64,
    /// Loop observations omitted because their site was not retained after
    /// reaching the configured bound.
    pub dropped_loop_observations: u64,
    /// Storage observations omitted because their site was not retained after
    /// reaching the configured bound.
    pub dropped_storage_observations: u64,
}

#[derive(Debug)]
struct JitCallSiteDiagnosticState {
    record: JitCallSiteRecord,
    last_ordinary_target_code_id: Option<u64>,
}

#[derive(Clone, Copy, Debug)]
enum JitCallTargetObservation {
    NonOrdinary,
    Ordinary {
        code_id: u64,
        cached_native: Option<bool>,
    },
}

#[derive(Debug)]
struct JitDiagnosticState {
    limits: JitDiagnosticLimits,
    compile_records: Vec<JitCompileRecord>,
    admission_records: Vec<JitAdmissionRecord>,
    exit_records: Vec<JitExitRecord>,
    call_records: Vec<JitCallSiteDiagnosticState>,
    call_record_indices: FxHashMap<(u64, u32), usize>,
    loop_records: Vec<JitLoopSiteRecord>,
    loop_record_indices: FxHashMap<(u64, u32, u32), usize>,
    storage_records: Vec<JitStorageSiteRecord>,
    storage_record_indices: FxHashMap<(u64, u32, JitStorageSiteKind), usize>,
    native_storage: JitNativeStorageRecord,
    osr: JitOsrCounters,
    dropped_compile_records: u64,
    dropped_admission_records: u64,
    dropped_exit_records: u64,
    dropped_call_observations: u64,
    dropped_loop_observations: u64,
    dropped_storage_observations: u64,
}

impl JitDiagnosticState {
    fn new(limits: JitDiagnosticLimits) -> Self {
        let limits = limits.bounded();
        Self {
            limits,
            compile_records: Vec::with_capacity(limits.compile_records.min(64)),
            admission_records: Vec::with_capacity(limits.admission_records.min(64)),
            exit_records: Vec::with_capacity(limits.exit_records.min(64)),
            call_records: Vec::with_capacity(limits.call_records.min(64)),
            call_record_indices: FxHashMap::default(),
            loop_records: Vec::with_capacity(limits.loop_records.min(64)),
            loop_record_indices: FxHashMap::default(),
            storage_records: Vec::with_capacity(limits.storage_records.min(64)),
            storage_record_indices: FxHashMap::default(),
            native_storage: JitNativeStorageRecord::default(),
            osr: JitOsrCounters::default(),
            dropped_compile_records: 0,
            dropped_admission_records: 0,
            dropped_exit_records: 0,
            dropped_call_observations: 0,
            dropped_loop_observations: 0,
            dropped_storage_observations: 0,
        }
    }

    fn record_compile(&mut self, record: JitCompileRecord) {
        if self.compile_records.len() < self.limits.compile_records {
            self.compile_records.push(record);
        } else {
            self.dropped_compile_records = self.dropped_compile_records.saturating_add(1);
        }
    }

    fn record_admission(&mut self, record: JitAdmissionRecord) {
        if self.admission_records.len() < self.limits.admission_records {
            self.admission_records.push(record);
        } else {
            self.dropped_admission_records = self.dropped_admission_records.saturating_add(1);
        }
    }

    fn record_exit(&mut self, code_id: u64, exit: JitExit, native_ns: u128) {
        let kind = JitDiagnosticExitKind::from(exit.kind);
        if let Some(record) = self.exit_records.iter_mut().find(|record| {
            record.code_id == code_id
                && record.entry_pc == 0
                && record.pc == exit.pc
                && record.kind == kind
                && record.reason == exit.reason
        }) {
            record.count = record.count.saturating_add(1);
            record.native_ns = record.native_ns.saturating_add(native_ns);
            return;
        }

        if self.exit_records.len() < self.limits.exit_records {
            self.exit_records.push(JitExitRecord {
                code_id,
                entry_pc: 0,
                pc: exit.pc,
                kind,
                reason: exit.reason,
                count: 1,
                native_ns,
            });
        } else {
            self.dropped_exit_records = self.dropped_exit_records.saturating_add(1);
        }
    }

    fn record_call_site(&mut self, caller_code_id: u64, pc: u32, target: JitCallTargetObservation) {
        let key = (caller_code_id, pc);
        let state = if let Some(index) = self.call_record_indices.get(&key).copied() {
            &mut self.call_records[index]
        } else if self.call_records.len() < self.limits.call_records {
            let index = self.call_records.len();
            self.call_records.push(JitCallSiteDiagnosticState {
                record: JitCallSiteRecord {
                    caller_code_id,
                    pc,
                    ..JitCallSiteRecord::default()
                },
                last_ordinary_target_code_id: None,
            });
            self.call_record_indices.insert(key, index);
            &mut self.call_records[index]
        } else {
            self.dropped_call_observations = self.dropped_call_observations.saturating_add(1);
            return;
        };

        state.record.calls = state.record.calls.saturating_add(1);
        match target {
            JitCallTargetObservation::NonOrdinary => {
                state.record.non_ordinary_calls = state.record.non_ordinary_calls.saturating_add(1);
            }
            JitCallTargetObservation::Ordinary {
                code_id,
                cached_native,
            } => {
                state.record.ordinary_calls = state.record.ordinary_calls.saturating_add(1);
                match state.last_ordinary_target_code_id {
                    None => {
                        state.record.first_ordinary_target_calls =
                            state.record.first_ordinary_target_calls.saturating_add(1);
                    }
                    Some(previous) if previous == code_id => {
                        state.record.same_ordinary_target_calls =
                            state.record.same_ordinary_target_calls.saturating_add(1);
                    }
                    Some(_) => {
                        state.record.changed_ordinary_target_calls =
                            state.record.changed_ordinary_target_calls.saturating_add(1);
                    }
                }
                state.last_ordinary_target_code_id = Some(code_id);
                match cached_native {
                    Some(true) => {
                        state.record.cached_native_target_calls =
                            state.record.cached_native_target_calls.saturating_add(1);
                    }
                    Some(false) => {
                        state.record.cached_shim_target_calls =
                            state.record.cached_shim_target_calls.saturating_add(1);
                    }
                    None => {}
                }
            }
        }
    }

    fn has_loop_site(&self, code_id: u64, header_pc: u32, backedge_pc: u32) -> bool {
        self.loop_record_indices
            .contains_key(&(code_id, header_pc, backedge_pc))
    }

    fn record_loop_site(
        &mut self,
        mut new_record: JitLoopSiteRecord,
        crossed_hotness: bool,
        entry_decision_closed: bool,
    ) {
        let key = (
            new_record.code_id,
            new_record.header_pc,
            new_record.backedge_pc,
        );
        let record = if let Some(index) = self.loop_record_indices.get(&key).copied() {
            &mut self.loop_records[index]
        } else if self.loop_records.len() < self.limits.loop_records {
            let index = self.loop_records.len();
            new_record.backedges = 0;
            new_record.hotness_crossings = 0;
            new_record.closed_entry_backedges = 0;
            self.loop_records.push(new_record);
            self.loop_record_indices.insert(key, index);
            &mut self.loop_records[index]
        } else {
            self.dropped_loop_observations = self.dropped_loop_observations.saturating_add(1);
            return;
        };

        record.backedges = record.backedges.saturating_add(1);
        if crossed_hotness {
            record.hotness_crossings = record.hotness_crossings.saturating_add(1);
        }
        if entry_decision_closed {
            record.closed_entry_backedges = record.closed_entry_backedges.saturating_add(1);
        }
    }

    fn record_storage_site(
        &mut self,
        code_id: u64,
        pc: u32,
        kind: JitStorageSiteKind,
        inline_cache_hit: Option<bool>,
    ) {
        let key = (code_id, pc, kind);
        let record = if let Some(index) = self.storage_record_indices.get(&key).copied() {
            &mut self.storage_records[index]
        } else if self.storage_records.len() < self.limits.storage_records {
            let index = self.storage_records.len();
            self.storage_records.push(JitStorageSiteRecord {
                code_id,
                pc,
                kind,
                ..JitStorageSiteRecord::default()
            });
            self.storage_record_indices.insert(key, index);
            &mut self.storage_records[index]
        } else {
            self.dropped_storage_observations = self.dropped_storage_observations.saturating_add(1);
            return;
        };

        record.executions = record.executions.saturating_add(1);
        match inline_cache_hit {
            Some(true) => record.inline_cache_hits = record.inline_cache_hits.saturating_add(1),
            Some(false) => {
                record.inline_cache_misses = record.inline_cache_misses.saturating_add(1);
            }
            None => {
                record.inline_cache_not_applicable =
                    record.inline_cache_not_applicable.saturating_add(1);
            }
        }
    }

    fn record_native_storage(&mut self, record: JitNativeStorageRecord) {
        self.native_storage.merge(record);
    }

    fn snapshot(&self) -> JitDiagnosticSnapshot {
        let mut compile_records = self.compile_records.clone();
        compile_records.sort_by_key(|record| (record.code_id, record.entry_pc, record.budgeted));
        let mut admission_records = self.admission_records.clone();
        admission_records.sort_by_key(|record| record.code_id);
        let mut exit_records = self.exit_records.clone();
        exit_records.sort_by_key(|record| {
            (
                record.code_id,
                record.entry_pc,
                record.pc,
                record.kind,
                record.reason,
            )
        });
        let mut call_records = self
            .call_records
            .iter()
            .map(|state| state.record)
            .collect::<Vec<_>>();
        call_records.sort_by_key(|record| (record.caller_code_id, record.pc));
        let mut loop_records = self.loop_records.clone();
        loop_records.sort_by_key(|record| (record.code_id, record.header_pc, record.backedge_pc));
        let mut storage_records = self.storage_records.clone();
        storage_records.sort_by_key(|record| (record.code_id, record.pc, record.kind));
        JitDiagnosticSnapshot {
            schema_version: JIT_DIAGNOSTIC_SCHEMA_VERSION,
            limits: self.limits,
            compile_records,
            admission_records,
            exit_records,
            call_records,
            loop_records,
            storage_records,
            native_storage: self.native_storage,
            osr: self.osr,
            dropped_compile_records: self.dropped_compile_records,
            dropped_admission_records: self.dropped_admission_records,
            dropped_exit_records: self.dropped_exit_records,
            dropped_call_observations: self.dropped_call_observations,
            dropped_loop_observations: self.dropped_loop_observations,
            dropped_storage_observations: self.dropped_storage_observations,
        }
    }
}

#[derive(Clone, Copy)]
struct CachedEntry {
    entry: extern "C" fn(*mut Context) -> u64,
    native: bool,
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
enum JitEntryPoint {
    Function,
    #[allow(
        dead_code,
        reason = "constructed by the loop-OSR planner before scheduler wiring"
    )]
    Loop {
        header_pc: u32,
        backedge_pc: u32,
        representation: JitOsrRepresentation,
    },
}

#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct JitCacheKey {
    code_id: u64,
    entry_point: JitEntryPoint,
    budgeted: bool,
    diagnostic: bool,
}

impl JitCacheKey {
    const fn function(code_id: u64, budgeted: bool, diagnostic: bool) -> Self {
        Self {
            code_id,
            entry_point: JitEntryPoint::Function,
            budgeted,
            diagnostic,
        }
    }

    const fn loop_region(
        code_id: u64,
        header_pc: u32,
        backedge_pc: u32,
        representation: JitOsrRepresentation,
        budgeted: bool,
        diagnostic: bool,
    ) -> Self {
        Self {
            code_id,
            entry_point: JitEntryPoint::Loop {
                header_pc,
                backedge_pc,
                representation,
            },
            budgeted,
            diagnostic,
        }
    }
}

const MAX_LOOP_REGION_STATES: usize = 64;
const MAX_LOOP_CODE_BYTES: usize = 1024 * 1024;
const MAX_LOOP_COMPILE_NS: u128 = 10_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopRegionStateKind {
    Observed,
    Rejected { reason: JitOsrRejectionReason },
    Suppressed { reason: JitOsrSuppressionReason },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LoopRegionState {
    kind: LoopRegionStateKind,
    backedges: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LoopRegionAction {
    Cold,
    Compile,
    Closed,
    Suppressed(JitOsrSuppressionReason),
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
    id: u64,
    pub(super) module: JITModule,
    /// Monotonic counter for unique symbol names. `JITModule::declare_function`
    /// deduplicates by name, so reusing a fixed name (e.g. "`jit_codeblock`")
    /// across compilations makes the second `define_function` fail with
    /// `DuplicateDefinition`. Each compile gets a fresh name from this counter.
    pub(super) next_fn_id: u64,
    /// Compiled entries are scoped to this backend. The code block's debug ID
    /// is unique for the lifetime of the current thread, which is sufficient
    /// because a backend is not shared across threads or realms. Budgeted and
    /// unbudgeted entries are distinct so the latter keep their fast path.
    cache: FxHashMap<JitCacheKey, CachedEntry>,
    /// Bounded per-key loop hotness and terminal admission state. No generated
    /// loop entry can be stored or invoked until the following compiler slice.
    loop_regions: FxHashMap<JitCacheKey, LoopRegionState>,
    loop_code_bytes: usize,
    loop_code_bytes_exhausted: bool,
    loop_compile_time_exhausted: bool,
    /// The last ordinary-function target observed at each bytecode call site.
    /// Native call lowering specializes against this identity and deopts when
    /// a later value is a different function, including another ordinary
    /// function with the same calling convention.
    call_targets: FxHashMap<(u64, u32), u64>,
    thresholds: JitThresholds,
    admission_min_straight_line_instructions: u32,
    admission_allow_call_boundaries: bool,
    stats: JitStats,
    diagnostics: Option<JitDiagnosticState>,
}

impl std::fmt::Debug for JitBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("JitBackend").finish_non_exhaustive()
    }
}

impl JitBackend {
    /// Straight-line native bodies below this size did not amortize their
    /// entry transition in the measured Phase 2 crossover. Backward-branch
    /// bodies remain eligible because their work can stay native across loop
    /// iterations.
    const MIN_STRAIGHT_LINE_INSTRUCTIONS: u32 = 45;

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
            id: NEXT_BACKEND_ID.fetch_add(1, Ordering::Relaxed),
            module: JITModule::new(builder),
            next_fn_id: 0,
            cache: FxHashMap::default(),
            loop_regions: FxHashMap::default(),
            loop_code_bytes: 0,
            loop_code_bytes_exhausted: false,
            loop_compile_time_exhausted: false,
            call_targets: FxHashMap::default(),
            thresholds: JitThresholds::default(),
            admission_min_straight_line_instructions: Self::MIN_STRAIGHT_LINE_INSTRUCTIONS,
            admission_allow_call_boundaries: false,
            stats: JitStats::default(),
            diagnostics: None,
        }
    }

    /// Return a snapshot of the counters collected by this backend.
    #[must_use]
    pub const fn stats(&self) -> JitStats {
        self.stats
    }

    /// Runtime-local generation used to scope cached admission decisions.
    pub(crate) const fn id(&self) -> u64 {
        self.id
    }

    /// Enable bounded detailed diagnostics for cached runtime-tier compilation
    /// and native exits, clearing any prior records.
    ///
    /// The low-level [`Self::compile_codeblock`] escape hatch does not enter
    /// the runtime cache and is therefore outside this diagnostic stream.
    pub fn enable_diagnostics(&mut self, limits: JitDiagnosticLimits) {
        self.diagnostics = Some(JitDiagnosticState::new(limits));
    }

    /// Disable detailed diagnostics and release their retained records.
    pub fn disable_diagnostics(&mut self) {
        self.diagnostics = None;
    }

    /// Return a clone of the current detailed diagnostic records.
    #[must_use]
    pub fn diagnostic_snapshot(&self) -> Option<JitDiagnosticSnapshot> {
        self.diagnostics.as_ref().map(JitDiagnosticState::snapshot)
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

    fn update_osr_counters(&mut self, mut update: impl FnMut(&mut JitOsrCounters)) {
        update(&mut self.stats.osr);
        if let Some(diagnostics) = &mut self.diagnostics {
            update(&mut diagnostics.osr);
        }
    }

    fn loop_compilation_breaker(&self) -> Option<JitOsrSuppressionReason> {
        if self.loop_compile_time_exhausted {
            Some(JitOsrSuppressionReason::CompileTime)
        } else if self.loop_code_bytes_exhausted {
            Some(JitOsrSuppressionReason::CodeBytes)
        } else {
            None
        }
    }

    fn new_loop_suppression_reason(&self) -> Option<JitOsrSuppressionReason> {
        self.loop_compilation_breaker().or_else(|| {
            (self.loop_regions.len() >= MAX_LOOP_REGION_STATES)
                .then_some(JitOsrSuppressionReason::RegionCapacity)
        })
    }

    fn record_loop_suppression(&mut self, reason: JitOsrSuppressionReason) {
        self.update_osr_counters(|counters| counters.suppressions.record(reason));
    }

    /// Observe one exact typed loop key without compiling or invoking it.
    ///
    /// New keys are retained only while the 64-state table has capacity.
    /// Once any global circuit breaker trips, unseen keys are rejected without
    /// allocating negative entries; already-retained keys remain queryable.
    #[allow(
        dead_code,
        reason = "wired into the post-backedge scheduler in Slice 4A1.4"
    )]
    fn observe_loop_region(&mut self, key: JitCacheKey) -> LoopRegionAction {
        debug_assert!(matches!(key.entry_point, JitEntryPoint::Loop { .. }));
        self.update_osr_counters(|counters| {
            counters.cache_requests = counters.cache_requests.saturating_add(1);
        });

        if self.loop_regions.contains_key(&key) {
            self.update_osr_counters(|counters| {
                counters.cache_hits = counters.cache_hits.saturating_add(1);
            });
            let suppression = self.loop_compilation_breaker();
            let threshold = self.thresholds.loop_backedges.max(1);
            let mut crossed_hotness = false;
            let mut terminal_suppression = None;
            let Some(state) = self.loop_regions.get_mut(&key) else {
                return LoopRegionAction::Closed;
            };
            let action = match state.kind {
                LoopRegionStateKind::Observed => {
                    state.backedges = state.backedges.saturating_add(1);
                    match state.backedges.cmp(&threshold) {
                        std::cmp::Ordering::Equal => {
                            crossed_hotness = true;
                            if let Some(reason) = suppression {
                                terminal_suppression = Some(reason);
                                LoopRegionAction::Suppressed(reason)
                            } else {
                                LoopRegionAction::Compile
                            }
                        }
                        std::cmp::Ordering::Less => LoopRegionAction::Cold,
                        std::cmp::Ordering::Greater => LoopRegionAction::Closed,
                    }
                }
                LoopRegionStateKind::Rejected { .. } | LoopRegionStateKind::Suppressed { .. } => {
                    LoopRegionAction::Closed
                }
            };
            if crossed_hotness {
                self.update_osr_counters(|counters| {
                    counters.hotness_crossings = counters.hotness_crossings.saturating_add(1);
                });
            }
            if let Some(reason) = terminal_suppression {
                self.loop_regions.insert(
                    key,
                    LoopRegionState {
                        kind: LoopRegionStateKind::Suppressed { reason },
                        backedges: threshold,
                    },
                );
                self.record_loop_suppression(reason);
            }
            return action;
        }

        self.update_osr_counters(|counters| {
            counters.cache_misses = counters.cache_misses.saturating_add(1);
        });
        if let Some(reason) = self.new_loop_suppression_reason() {
            self.record_loop_suppression(reason);
            return LoopRegionAction::Suppressed(reason);
        }

        let backedges = 1;
        self.loop_regions.insert(
            key,
            LoopRegionState {
                kind: LoopRegionStateKind::Observed,
                backedges,
            },
        );
        if backedges == self.thresholds.loop_backedges.max(1) {
            self.update_osr_counters(|counters| {
                counters.hotness_crossings = counters.hotness_crossings.saturating_add(1);
            });
            LoopRegionAction::Compile
        } else {
            LoopRegionAction::Cold
        }
    }

    #[allow(dead_code, reason = "called by the loop planner in Slice 4A1.4")]
    fn reject_loop_region(&mut self, key: JitCacheKey, reason: JitOsrRejectionReason) -> bool {
        let Some(state) = self.loop_regions.get_mut(&key) else {
            return false;
        };
        if state.kind != LoopRegionStateKind::Observed {
            return false;
        }
        state.kind = LoopRegionStateKind::Rejected { reason };
        self.update_osr_counters(|counters| counters.rejections.record(reason));
        true
    }

    #[allow(dead_code, reason = "called by the loop compiler in Slice 4A1.3")]
    fn record_loop_compile_attempt(&mut self) {
        self.update_osr_counters(|counters| {
            counters.compile_attempts = counters.compile_attempts.saturating_add(1);
        });
    }

    /// Account one completed native loop compile attempt and trip future-new-
    /// site circuit breakers after the unavoidable synchronous work. Failed
    /// lowering contributes time but never emitted-code bytes or compilations.
    #[allow(dead_code, reason = "called by the loop compiler in Slice 4A1.3")]
    fn record_loop_compile_result(&mut self, compiled: bool, code_bytes: usize, compile_ns: u128) {
        if compiled {
            self.loop_code_bytes = self.loop_code_bytes.saturating_add(code_bytes);
            self.loop_code_bytes_exhausted = self.loop_code_bytes >= MAX_LOOP_CODE_BYTES;
        }
        self.loop_compile_time_exhausted |= compile_ns > MAX_LOOP_COMPILE_NS;
        self.update_osr_counters(|counters| {
            if compiled {
                counters.compilations = counters.compilations.saturating_add(1);
                counters.code_bytes = counters.code_bytes.saturating_add(code_bytes);
            }
            counters.compile_time_ns = counters.compile_time_ns.saturating_add(compile_ns);
        });
    }

    #[allow(dead_code, reason = "called by the loop scheduler in Slice 4A1.4")]
    fn record_loop_entry(&mut self) {
        self.update_osr_counters(|counters| {
            counters.entries = counters.entries.saturating_add(1);
        });
    }

    #[allow(dead_code, reason = "called by the loop scheduler in Slice 4A1.4")]
    fn record_loop_exit(&mut self, kind: JitExitKind) {
        self.update_osr_counters(|counters| match kind {
            JitExitKind::EntryRejected => {
                counters.entry_rejections = counters.entry_rejections.saturating_add(1);
            }
            JitExitKind::Continuation => {
                counters.continuations = counters.continuations.saturating_add(1);
            }
            JitExitKind::Deopt => {
                counters.deopts = counters.deopts.saturating_add(1);
            }
            JitExitKind::Return
            | JitExitKind::Call
            | JitExitKind::Completion
            | JitExitKind::Budget => {}
        });
    }

    #[cfg(test)]
    fn allow_small_native_entries_for_tests(&mut self) {
        self.admission_min_straight_line_instructions = 0;
        // The semantic call-lowering tests intentionally exercise the existing
        // scheduler exit even though production admission rejects callers that
        // cannot yet resume natively after it.
        self.admission_allow_call_boundaries = true;
    }

    /// Record an ordinary function entry for tiering.
    pub(crate) fn record_function_entry(&mut self, code: &CodeBlock) {
        self.stats.function_entries = self.stats.function_entries.saturating_add(1);
        if code.jit_admission(self.id) != crate::vm::JitAdmissionState::Unknown {
            return;
        }
        let was_hot = self.is_hot(code);
        code.record_jit_function_entry(self.id);
        if !was_hot && self.is_hot(code) {
            self.stats.hotness_threshold_crossings =
                self.stats.hotness_threshold_crossings.saturating_add(1);
        }
    }

    /// Apply and cache the context-tier's static native-entry admission rule.
    /// Explicit low-level compilation does not call this method and therefore
    /// retains the complete shim fallback used for differential testing.
    pub(crate) fn admit_function_entry(&mut self, code: &CodeBlock) -> bool {
        use crate::vm::JitAdmissionState;

        match code.jit_admission(self.id) {
            JitAdmissionState::Allowed => return true,
            JitAdmissionState::Denied | JitAdmissionState::DeniedLeaf => return false,
            JitAdmissionState::Unknown => {}
        }

        let analysis = native::admission_profile(code, self.diagnostics.is_some());
        let (allowed, state, reason, profile, rejection) = match analysis {
            Ok(profile)
                if profile.call_instructions > 0 && !self.admission_allow_call_boundaries =>
            {
                (
                    false,
                    JitAdmissionState::Denied,
                    JitAdmissionReason::DeniedCallBoundary,
                    profile,
                    None,
                )
            }
            Ok(profile) if profile.backward_branches > 0 => (
                true,
                JitAdmissionState::Allowed,
                JitAdmissionReason::AllowedBackwardBranch,
                profile,
                None,
            ),
            Ok(profile)
                if profile.bytecode_instructions
                    >= self.admission_min_straight_line_instructions =>
            {
                (
                    true,
                    JitAdmissionState::Allowed,
                    JitAdmissionReason::AllowedStraightLineWork,
                    profile,
                    None,
                )
            }
            Ok(profile) => {
                let state = if profile.call_instructions == 0 && profile.property_instructions == 0
                {
                    JitAdmissionState::DeniedLeaf
                } else {
                    JitAdmissionState::Denied
                };
                (
                    false,
                    state,
                    JitAdmissionReason::DeniedStraightLineTooSmall,
                    profile,
                    None,
                )
            }
            Err(rejection) => (
                false,
                JitAdmissionState::Denied,
                JitAdmissionReason::DeniedNativeIneligible,
                native::NativeStaticProfile::default(),
                Some(rejection),
            ),
        };
        code.set_jit_admission(self.id, state);
        if !allowed {
            self.stats.admission_denials = self.stats.admission_denials.saturating_add(1);
        }
        if let Some(diagnostics) = &mut self.diagnostics {
            let (
                blocker,
                first_blocking_opcode,
                first_blocking_pc,
                supported_prefix_instructions,
                bytecode_instructions,
            ) = rejection.map_or(
                (
                    None,
                    None,
                    None,
                    profile.bytecode_instructions,
                    profile.bytecode_instructions,
                ),
                |rejection| {
                    (
                        Some(rejection.kind),
                        rejection
                            .first_blocking_opcode
                            .map(|opcode| format!("{opcode:?}")),
                        rejection.first_blocking_pc,
                        rejection.supported_prefix_instructions,
                        rejection.bytecode_instructions,
                    )
                },
            );
            diagnostics.record_admission(JitAdmissionRecord {
                code_id: code.debug_id,
                allowed,
                reason,
                leaf_fast_path: state == JitAdmissionState::DeniedLeaf,
                blocker,
                first_blocking_opcode,
                first_blocking_pc,
                supported_prefix_instructions,
                bytecode_instructions,
                native_backward_branches: profile.backward_branches,
                native_call_instructions: profile.call_instructions,
                native_property_instructions: profile.property_instructions,
            });
        }
        allowed
    }

    /// Record a backward edge for tiering.
    pub(crate) fn record_loop_backedge(
        &mut self,
        code: &CodeBlock,
        header_pc: u32,
        backedge_pc: u32,
    ) -> bool {
        self.stats.loop_backedges = self.stats.loop_backedges.saturating_add(1);
        let was_hot = self.is_hot(code);
        code.record_jit_loop_backedge(self.id);
        let is_hot = self.is_hot(code);
        let crossed_hotness = !was_hot && is_hot;
        if crossed_hotness {
            self.stats.hotness_threshold_crossings =
                self.stats.hotness_threshold_crossings.saturating_add(1);
        }
        self.record_loop_site(code, header_pc, backedge_pc, crossed_hotness, false);
        is_hot
    }

    /// Count one backedge after the current frame no longer needs to mutate
    /// code-block hotness.
    pub(crate) fn record_saturated_loop_backedge(
        &mut self,
        code: &CodeBlock,
        header_pc: u32,
        backedge_pc: u32,
    ) {
        self.stats.loop_backedges = self.stats.loop_backedges.saturating_add(1);
        self.stats.saturated_loop_backedges = self.stats.saturated_loop_backedges.saturating_add(1);
        self.record_loop_site(code, header_pc, backedge_pc, false, false);
    }

    /// Count a diagnostic-only backedge in a frame whose native-entry
    /// decision was already closed before dormant interpreter dispatch.
    pub(crate) fn record_closed_loop_backedge(
        &mut self,
        code: &CodeBlock,
        header_pc: u32,
        backedge_pc: u32,
    ) {
        self.stats.loop_backedges = self.stats.loop_backedges.saturating_add(1);
        self.record_loop_site(code, header_pc, backedge_pc, false, true);
    }

    fn record_loop_site(
        &mut self,
        code: &CodeBlock,
        header_pc: u32,
        backedge_pc: u32,
        crossed_hotness: bool,
        entry_decision_closed: bool,
    ) {
        let Some(diagnostics) = &self.diagnostics else {
            return;
        };
        let record = if diagnostics.has_loop_site(code.debug_id, header_pc, backedge_pc) {
            JitLoopSiteRecord {
                code_id: code.debug_id,
                header_pc,
                backedge_pc,
                ..JitLoopSiteRecord::default()
            }
        } else {
            match native::loop_admission_profile(code, header_pc, backedge_pc) {
                Ok(profile) => JitLoopSiteRecord {
                    code_id: code.debug_id,
                    header_pc,
                    backedge_pc,
                    static_osr_candidate: true,
                    region_instructions: profile.bytecode_instructions,
                    ..JitLoopSiteRecord::default()
                },
                Err(rejection) => JitLoopSiteRecord {
                    code_id: code.debug_id,
                    header_pc,
                    backedge_pc,
                    static_osr_blocker: Some(rejection.kind),
                    first_blocking_opcode: rejection
                        .first_blocking_opcode
                        .map(|opcode| format!("{opcode:?}")),
                    first_blocking_pc: rejection.first_blocking_pc,
                    region_instructions: rejection.bytecode_instructions,
                    ..JitLoopSiteRecord::default()
                },
            }
        };
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.record_loop_site(record, crossed_hotness, entry_decision_closed);
        }
    }

    /// Count one hot nonzero-PC frame handed to dormant interpreter dispatch.
    pub(crate) fn record_dormant_loop_frame(&mut self) {
        self.stats.dormant_loop_frames = self.stats.dormant_loop_frames.saturating_add(1);
    }

    /// Whether the current run must retain exact post-threshold loop records.
    /// Headline timing keeps diagnostics disabled and may use dormant dispatch.
    pub(crate) const fn observes_loop_backedges(&self) -> bool {
        self.diagnostics.is_some()
    }

    /// Whether an interpreter PC decrease was produced by an explicit
    /// same-frame branch to the reported target rather than exception unwind,
    /// frame replacement, or another VM transition.
    pub(crate) fn is_observed_loop_backedge(
        code: &CodeBlock,
        backedge_pc: u32,
        header_pc: u32,
    ) -> bool {
        if header_pc >= backedge_pc {
            return false;
        }
        let (instruction, _) = code.bytecode.next_instruction(backedge_pc as usize);
        match &instruction {
            Instruction::JumpTable { addresses, .. } => addresses
                .iter()
                .any(|address| address.as_u32() == header_pc),
            _ => same_frame_jump_target(&instruction) == Some(header_pc),
        }
    }

    /// Whether any same-frame branch can make a nonzero-PC frame eligible for
    /// the existing whole-function entry by returning to PC zero.
    pub(crate) fn can_reenter_at_pc_zero(code: &CodeBlock) -> bool {
        InstructionIterator::new(&code.bytecode).any(|(pc, _, instruction)| {
            pc > 0
                && match &instruction {
                    Instruction::JumpTable { addresses, .. } => {
                        addresses.iter().any(|address| address.as_u32() == 0)
                    }
                    _ => same_frame_jump_target(&instruction) == Some(0),
                }
        })
    }

    /// Record a native entry that returned to the interpreter.
    pub(crate) fn record_deopt(&mut self) {
        self.stats.deopts = self.stats.deopts.saturating_add(1);
    }

    /// Whether the interpreter needs to report call sites to this backend.
    ///
    /// Production call-target feedback is disabled until compiled callers
    /// have a continuation ABI. Detailed diagnostics remain independently
    /// opt-in and bounded.
    pub(crate) const fn observes_call_sites(&self) -> bool {
        self.diagnostics.is_some() || self.admission_allow_call_boundaries
    }

    /// Whether the interpreter must report storage-read sites to this backend.
    ///
    /// Storage attribution is diagnostics-only. Normal JIT execution retains
    /// no per-opcode observer or storage-site state.
    pub(crate) const fn observes_storage_sites(&self) -> bool {
        self.diagnostics.is_some()
    }

    /// Whether the interpreter must decode any pre-operation diagnostic site.
    pub(crate) const fn observes_interpreted_sites(&self) -> bool {
        self.observes_call_sites() || self.observes_storage_sites()
    }

    /// Record one interpreted storage read without retaining page data.
    pub(crate) fn observe_storage_site(
        &mut self,
        code: &CodeBlock,
        pc: u32,
        kind: JitStorageSiteKind,
        inline_cache_hit: Option<bool>,
    ) {
        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.record_storage_site(code.debug_id, pc, kind, inline_cache_hit);
        }
    }

    /// Observe one interpreted `Call` without changing production admission.
    ///
    /// Detailed records are bounded and source-free. The legacy last-target
    /// feedback remains available only to the existing in-crate native-call
    /// semantic tests, and only before the caller has attempted its entry.
    pub(crate) fn observe_call_site(
        &mut self,
        code: &CodeBlock,
        pc: u32,
        context: &Context,
        argument_count: usize,
        record_legacy_feedback: bool,
    ) {
        if !self.observes_call_sites() {
            return;
        }

        let function = context
            .vm
            .stack
            .calling_convention_get_function(argument_count);
        let ordinary_target = function.as_object().and_then(|object| {
            let function = object.downcast_ref::<OrdinaryFunction>()?;
            (function.codeblock().is_ordinary() && !function.codeblock().is_class_constructor())
                .then(|| function.codeblock().debug_id)
        });

        let target = if let Some(target_code_id) = ordinary_target {
            if self.admission_allow_call_boundaries && record_legacy_feedback {
                self.call_targets
                    .insert((code.debug_id, pc), target_code_id);
            }
            let budgeted = context.instruction_budget_remaining().is_some();
            let cached_native = self
                .cache
                .get(&JitCacheKey::function(
                    target_code_id,
                    budgeted,
                    self.diagnostics.is_some(),
                ))
                .map(|entry| entry.native);
            JitCallTargetObservation::Ordinary {
                code_id: target_code_id,
                cached_native,
            }
        } else {
            JitCallTargetObservation::NonOrdinary
        };

        if let Some(diagnostics) = &mut self.diagnostics {
            diagnostics.record_call_site(code.debug_id, pc, target);
        }
    }

    /// Return the monomorphic target recorded for a bytecode call site.
    pub(super) fn call_target(&self, code: &CodeBlock, pc: usize) -> Option<u64> {
        self.call_targets.get(&(code.debug_id, pc as u32)).copied()
    }

    /// Whether this code block has enough observed activity for compilation.
    #[must_use]
    pub(crate) fn is_hot(&self, code: &CodeBlock) -> bool {
        let (function_entries, loop_backedges) = code.jit_hotness(self.id);
        function_entries >= self.thresholds.function_entries
            || loop_backedges >= self.thresholds.loop_backedges
    }

    /// Return a cached entry if one exists, compiling and caching it otherwise.
    fn cached_entry(&mut self, code: &CodeBlock, charge_instruction_budget: bool) -> CachedEntry {
        self.stats.cache_requests = self.stats.cache_requests.saturating_add(1);
        let diagnostic = self.diagnostics.is_some();
        let cache_key = JitCacheKey::function(code.debug_id, charge_instruction_budget, diagnostic);

        if let Some(cached) = self.cache.get(&cache_key) {
            self.stats.cache_hits = self.stats.cache_hits.saturating_add(1);
            return *cached;
        }

        self.stats.cache_misses = self.stats.cache_misses.saturating_add(1);
        let started = Instant::now();
        let (entry, native, code_bytes, native_profile, rejection) =
            self.compile_codeblock_with_kind(code, charge_instruction_budget, diagnostic);
        let compile_ns = started.elapsed().as_nanos();
        self.stats.compile_time_ns = self.stats.compile_time_ns.saturating_add(compile_ns);
        self.stats.compilations = self.stats.compilations.saturating_add(1);
        if native {
            self.stats.native_compilations = self.stats.native_compilations.saturating_add(1);
        } else {
            self.stats.shim_compilations = self.stats.shim_compilations.saturating_add(1);
        }
        let cached = CachedEntry { entry, native };
        self.cache.insert(cache_key, cached);
        if let Some(diagnostics) = &mut self.diagnostics {
            let (
                blocker,
                first_blocking_opcode,
                first_blocking_pc,
                supported_prefix_instructions,
                bytecode_instructions,
            ) = rejection.map_or(
                (
                    None,
                    None,
                    None,
                    native_profile.bytecode_instructions,
                    native_profile.bytecode_instructions,
                ),
                |rejection| {
                    (
                        Some(rejection.kind),
                        rejection
                            .first_blocking_opcode
                            .map(|opcode| format!("{opcode:?}")),
                        rejection.first_blocking_pc,
                        rejection.supported_prefix_instructions,
                        rejection.bytecode_instructions,
                    )
                },
            );
            diagnostics.record_compile(JitCompileRecord {
                code_id: code.debug_id,
                entry_pc: 0,
                budgeted: charge_instruction_budget,
                outcome: if native {
                    JitCompileOutcome::Native
                } else {
                    JitCompileOutcome::Shim
                },
                blocker,
                first_blocking_opcode,
                first_blocking_pc,
                supported_prefix_instructions: if native {
                    bytecode_instructions
                } else {
                    supported_prefix_instructions
                },
                native_instructions: if native { bytecode_instructions } else { 0 },
                native_backward_branches: if native {
                    native_profile.backward_branches
                } else {
                    0
                },
                native_call_instructions: if native {
                    native_profile.call_instructions
                } else {
                    0
                },
                native_property_instructions: if native {
                    native_profile.property_instructions
                } else {
                    0
                },
                bytecode_instructions,
                compile_ns,
                code_bytes,
            });
        }
        cached
    }

    /// Invoke a cached entry for the current frame. This is the shared runtime
    /// hook used by both the explicit API and the context-owned tier.
    pub(crate) fn invoke_cached_entry(&mut self, code: &CodeBlock, context: &mut Context) -> u64 {
        let charge_instruction_budget = context.instruction_budget_remaining().is_some();
        let cached = self.cached_entry(code, charge_instruction_budget);
        if cached.native {
            self.stats.native_entries = self.stats.native_entries.saturating_add(1);
        }
        let started = if cached.native && self.diagnostics.is_some() {
            context.vm.jit_exit_pending = None;
            context.vm.jit_native_storage = JitNativeStorageRecord::default();
            Some(Instant::now())
        } else {
            None
        };
        // SAFETY: `context` is exclusively borrowed for the duration of the
        // native call, and the backend owns the generated code pointer.
        let status = (cached.entry)(std::ptr::from_mut(context));
        if cached.native && self.diagnostics.is_some() {
            let native_storage = std::mem::take(&mut context.vm.jit_native_storage);
            if let Some(diagnostics) = &mut self.diagnostics {
                diagnostics.record_native_storage(native_storage);
            }
        }
        let decoded_exit = JitExit::decode(status);
        if matches!(
            decoded_exit,
            Some(JitExit {
                kind: JitExitKind::Call,
                reason: JitExitReason::Scheduler,
                ..
            })
        ) {
            self.stats.scheduler_call_exits = self.stats.scheduler_call_exits.saturating_add(1);
        }
        if let Some(started) = started {
            let exit = decoded_exit.or_else(|| {
                if status & JIT_BREAK_BIT != 0 {
                    context.vm.jit_exit_pending.take().or(Some(JitExit {
                        kind: JitExitKind::Completion,
                        reason: JitExitReason::Unknown,
                        pc: context.vm.frame().pc,
                    }))
                } else {
                    None
                }
            });
            if let (Some(diagnostics), Some(exit)) = (&mut self.diagnostics, exit) {
                diagnostics.record_exit(code.debug_id, exit, started.elapsed().as_nanos());
            }
        }
        status
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

        if let Some(exit) = JitExit::decode(status)
            && matches!(exit.kind, JitExitKind::Deopt)
        {
            self.record_deopt();
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
        self.compile_codeblock_with_kind(code, false, false).0
    }

    fn compile_codeblock_with_kind(
        &mut self,
        code: &CodeBlock,
        charge_instruction_budget: bool,
        instrument_storage: bool,
    ) -> (
        extern "C" fn(*mut Context) -> u64,
        bool,
        usize,
        native::NativeStaticProfile,
        Option<native::NativeRejection>,
    ) {
        let collect_diagnostic_metadata = self.diagnostics.is_some();
        match native::compile(
            self,
            code,
            charge_instruction_budget,
            collect_diagnostic_metadata,
            instrument_storage,
        ) {
            native::NativeCompileResult::Compiled {
                entry,
                profile,
                code_bytes,
            } => (entry, true, code_bytes, profile, None),
            native::NativeCompileResult::Rejected(rejection) => {
                let (entry, code_bytes) = self.compile_shim_codeblock(code);
                (
                    entry,
                    false,
                    code_bytes,
                    native::NativeStaticProfile::default(),
                    Some(rejection),
                )
            }
        }
    }

    /// Compile a code block using the legacy shim bridge. This remains the
    /// complete-semantics fallback while the native allowlist grows.
    #[must_use]
    fn compile_shim_codeblock(
        &mut self,
        code: &CodeBlock,
    ) -> (extern "C" fn(*mut Context) -> u64, usize) {
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
        let code_bytes = cctx
            .compiled_code()
            .expect("defined function has compiled code")
            .code_buffer()
            .len();
        self.module.clear_context(&mut cctx);
        self.module.finalize_definitions().expect("finalize");

        let code_ptr = self.module.get_finalized_function(id);
        // SAFETY: the compiled function matches this signature, and `self` owns
        // the code for as long as the returned pointer is used.
        let entry = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*mut Context) -> u64>(code_ptr)
        };
        (entry, code_bytes)
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
    use crate::error::{EngineError, JsNativeErrorKind, RuntimeLimitError};
    use crate::vm::GlobalFunctionBinding;
    use crate::{JsValue, NativeFunction, js_string};

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

    fn first_function_code(source: &str) -> boa_gc::Gc<CodeBlock> {
        let mut context = Context::default();
        let script = crate::Script::parse(crate::Source::from_bytes(source), None, &mut context)
            .expect("parse");
        let code = script.codeblock(&mut context).expect("codeblock");
        let GlobalFunctionBinding { function_index, .. } = code.global_fns[0];
        code.constant_function(function_index as usize)
    }

    fn canonical_loop(code: &CodeBlock) -> (u32, u32) {
        InstructionIterator::new(&code.bytecode)
            .find_map(|(pc, _, instruction)| match instruction {
                Instruction::Jump { address } if address.as_u32() < pc as u32 => {
                    Some((address.as_u32(), pc as u32))
                }
                _ => None,
            })
            .expect("canonical backward jump")
    }

    fn loop_key(code_id: u64, header_pc: u32) -> JitCacheKey {
        JitCacheKey::loop_region(
            code_id,
            header_pc,
            header_pc + 1,
            JitOsrRepresentation::I32,
            false,
            false,
        )
    }

    fn enable_jit_without_admission_floor(context: &mut Context) {
        context.enable_jit();
        context
            .jit_backend
            .as_mut()
            .expect("JIT was enabled")
            .allow_small_native_entries_for_tests();
    }

    fn enable_jit_diagnostics_without_admission_floor(
        context: &mut Context,
        limits: JitDiagnosticLimits,
    ) {
        context.enable_jit_diagnostics(limits);
        context
            .jit_backend
            .as_mut()
            .expect("JIT was enabled")
            .allow_small_native_entries_for_tests();
    }

    #[test]
    fn jit_compile_diagnostics_are_opt_in_bounded_and_source_free() {
        let native_code = first_function_code(
            "function sum(n) { let total = 0; for (let i = 0; i < n; i++) { total = total + i; } return total; }",
        );
        let unsupported_code = first_function_code(
            "function distinctive_private_source_name(left, right) { return left & right; }",
        );

        let mut disabled = JitBackend::new();
        let _ = disabled.cached_entry(&native_code, false);
        assert_eq!(disabled.diagnostic_snapshot(), None);

        let mut native_backend = JitBackend::new();
        native_backend.enable_diagnostics(JitDiagnosticLimits::default());
        let _ = native_backend.cached_entry(&native_code, false);
        let native_snapshot = native_backend
            .diagnostic_snapshot()
            .expect("diagnostics enabled");
        assert_eq!(
            native_snapshot.schema_version,
            JIT_DIAGNOSTIC_SCHEMA_VERSION
        );
        assert_eq!(native_snapshot.compile_records.len(), 1);
        assert_eq!(native_snapshot.dropped_compile_records, 0);
        let native = &native_snapshot.compile_records[0];
        assert_eq!(native.outcome, JitCompileOutcome::Native);
        assert_eq!(native.blocker, None);
        assert!(native.bytecode_instructions > 0, "record: {native:?}");
        assert_eq!(native.native_instructions, native.bytecode_instructions);
        assert!(native.native_backward_branches > 0, "record: {native:?}");
        assert_eq!(native.native_call_instructions, 0);
        assert_eq!(native.native_property_instructions, 0);
        assert_eq!(
            native.supported_prefix_instructions,
            native.bytecode_instructions
        );
        assert!(native.code_bytes > 0, "record: {native:?}");

        let mut shim_backend = JitBackend::new();
        shim_backend.enable_diagnostics(JitDiagnosticLimits::default());
        let _ = shim_backend.cached_entry(&unsupported_code, false);
        let shim_snapshot = shim_backend
            .diagnostic_snapshot()
            .expect("diagnostics enabled");
        let shim = &shim_snapshot.compile_records[0];
        assert_eq!(shim.outcome, JitCompileOutcome::Shim);
        assert_eq!(shim.blocker, Some(JitCompileBlockerKind::UnsupportedOpcode));
        assert_eq!(shim.first_blocking_opcode.as_deref(), Some("BitAnd"));
        assert!(shim.first_blocking_pc.is_some(), "record: {shim:?}");
        assert_eq!(shim.native_instructions, 0);
        assert_eq!(shim.native_backward_branches, 0);
        assert_eq!(shim.native_call_instructions, 0);
        assert_eq!(shim.native_property_instructions, 0);
        assert!(shim.supported_prefix_instructions < shim.bytecode_instructions);
        assert!(shim.code_bytes > 0, "record: {shim:?}");
        let serialized = serde_json::to_string(&shim_snapshot).expect("serialize diagnostics");
        assert!(
            !serialized.contains("distinctive_private_source_name"),
            "diagnostics must not contain source or function names: {serialized}"
        );
        assert!(serialized.contains("\"unsupported_opcode\""));
        assert!(serialized.contains("\"BitAnd\""));

        let mut bounded = JitBackend::new();
        bounded.enable_diagnostics(JitDiagnosticLimits {
            compile_records: 1,
            admission_records: 0,
            exit_records: 0,
            call_records: 0,
            loop_records: 0,
            storage_records: 0,
        });
        let _ = bounded.cached_entry(&native_code, false);
        let _ = bounded.cached_entry(&unsupported_code, false);
        let bounded = bounded.diagnostic_snapshot().expect("diagnostics enabled");
        assert_eq!(bounded.compile_records.len(), 1);
        assert_eq!(bounded.dropped_compile_records, 1);

        let mut hard_bounded = JitBackend::new();
        hard_bounded.enable_diagnostics(JitDiagnosticLimits {
            compile_records: usize::MAX,
            admission_records: usize::MAX,
            exit_records: usize::MAX,
            call_records: usize::MAX,
            loop_records: usize::MAX,
            storage_records: usize::MAX,
        });
        let hard_bounded = hard_bounded
            .diagnostic_snapshot()
            .expect("diagnostics enabled");
        assert_eq!(
            hard_bounded.limits,
            JitDiagnosticLimits {
                compile_records: MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND,
                admission_records: MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND,
                exit_records: MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND,
                call_records: MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND,
                loop_records: MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND,
                storage_records: MAX_JIT_DIAGNOSTIC_RECORDS_PER_KIND,
            }
        );
    }

    #[test]
    fn jit_admission_diagnostics_are_bounded_and_source_free() {
        let tiny_code = first_function_code(
            "function distinctive_private_tiny(left, right) { return left + right; }",
        );
        let loop_code = first_function_code(
            "function distinctive_private_loop(limit) { let total = 0; for (let index = 0; index < limit; index++) { total += index; } return total; }",
        );
        let unsupported_code = first_function_code(
            "function distinctive_private_blocked(value) { return value & 7; }",
        );
        let call_boundary_code = first_function_code(
            "function distinctive_private_call_loop(callback, limit) { let total = 0; for (let index = 0; index < limit; index++) { total = total + callback(index); } return total; }",
        );

        let mut backend = JitBackend::new();
        backend.enable_diagnostics(JitDiagnosticLimits::default());
        assert!(!backend.admit_function_entry(&tiny_code));
        assert!(backend.admit_function_entry(&loop_code));
        assert!(!backend.admit_function_entry(&unsupported_code));
        assert!(!backend.admit_function_entry(&call_boundary_code));

        let snapshot = backend.diagnostic_snapshot().expect("diagnostics enabled");
        assert_eq!(snapshot.schema_version, JIT_DIAGNOSTIC_SCHEMA_VERSION);
        assert_eq!(snapshot.admission_records.len(), 4);
        assert_eq!(snapshot.dropped_admission_records, 0);
        assert!(snapshot.compile_records.is_empty());
        assert!(snapshot.admission_records.iter().any(|record| {
            !record.allowed
                && record.reason == JitAdmissionReason::DeniedStraightLineTooSmall
                && record.leaf_fast_path
                && record.blocker.is_none()
                && record.bytecode_instructions > 0
        }));
        assert!(snapshot.admission_records.iter().any(|record| {
            record.allowed
                && record.reason == JitAdmissionReason::AllowedBackwardBranch
                && !record.leaf_fast_path
                && record.native_backward_branches > 0
        }));
        assert!(snapshot.admission_records.iter().any(|record| {
            !record.allowed
                && record.reason == JitAdmissionReason::DeniedNativeIneligible
                && record.blocker == Some(JitCompileBlockerKind::UnsupportedOpcode)
                && record.first_blocking_opcode.as_deref() == Some("BitAnd")
                && record.first_blocking_pc.is_some()
                && record.supported_prefix_instructions < record.bytecode_instructions
        }));
        assert!(snapshot.admission_records.iter().any(|record| {
            !record.allowed
                && record.reason == JitAdmissionReason::DeniedCallBoundary
                && !record.leaf_fast_path
                && record.blocker.is_none()
                && record.native_backward_branches > 0
                && record.native_call_instructions > 0
        }));
        let serialized = serde_json::to_string(&snapshot).expect("serialize diagnostics");
        assert!(!serialized.contains("distinctive_private"));
        assert!(serialized.contains("\"denied_straight_line_too_small\""));
        assert!(serialized.contains("\"denied_native_ineligible\""));
        assert!(serialized.contains("\"denied_call_boundary\""));

        let mut bounded = JitBackend::new();
        bounded.enable_diagnostics(JitDiagnosticLimits {
            compile_records: 0,
            admission_records: 1,
            exit_records: 0,
            call_records: 0,
            loop_records: 0,
            storage_records: 0,
        });
        assert!(!bounded.admit_function_entry(&tiny_code));
        assert!(bounded.admit_function_entry(&loop_code));
        let bounded = bounded.diagnostic_snapshot().expect("diagnostics enabled");
        assert_eq!(bounded.admission_records.len(), 1);
        assert_eq!(bounded.dropped_admission_records, 1);
    }

    #[test]
    fn context_owned_jit_denies_call_boundary_without_compiling_an_artifact() {
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function increment(value) { return value + 1; } function sum(callback, limit) { let total = 0; for (let index = 0; index < limit; index++) { total = total + callback(index); } return total; } let answer = 0; for (let index = 0; index < 80; index++) { answer = sum(increment, 10); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(55));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert_eq!(stats.compilations, 0, "stats: {stats:?}");
        assert_eq!(stats.shim_compilations, 0, "stats: {stats:?}");
        assert_eq!(stats.native_compilations, 0, "stats: {stats:?}");
        assert_eq!(stats.scheduler_call_exits, 0, "stats: {stats:?}");
        let diagnostics = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        assert!(diagnostics.compile_records.is_empty());
        assert!(diagnostics.admission_records.iter().any(|record| {
            !record.allowed
                && record.reason == JitAdmissionReason::DeniedCallBoundary
                && record.native_backward_branches > 0
                && record.native_call_instructions > 0
        }));
        let call = diagnostics
            .call_records
            .iter()
            .find(|record| record.ordinary_calls > 0 && record.same_ordinary_target_calls > 0)
            .expect("the denied caller's dynamic call site was retained");
        assert_eq!(call.calls, call.ordinary_calls + call.non_ordinary_calls);
        assert_eq!(
            call.ordinary_calls,
            call.first_ordinary_target_calls
                + call.same_ordinary_target_calls
                + call.changed_ordinary_target_calls
        );
        assert_eq!(call.first_ordinary_target_calls, 1);
        assert_eq!(call.changed_ordinary_target_calls, 0);
        assert_eq!(diagnostics.dropped_call_observations, 0);
        let serialized = serde_json::to_string(&diagnostics).expect("serialize diagnostics");
        assert!(!serialized.contains("increment"));
        assert!(!serialized.contains("sum"));
    }

    #[test]
    fn context_owned_jit_call_diagnostics_attribute_targets_without_compiling_callers() {
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function hot(limit) { let total = 0; for (let index = 0; index < limit; index++) { total = total + index; } return total; } function subtract(value) { return value - 1; } function apply(callback, value) { return callback(value); } let answer = 0; for (let index = 0; index < 70; index++) { answer = hot(10); } for (let index = 0; index < 70; index++) { answer = apply(hot, 10); } answer = answer + apply(subtract, 5); answer = answer + apply(Math.max, 5); answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(54));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert_eq!(stats.scheduler_call_exits, 0, "stats: {stats:?}");
        let diagnostics = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        let call = diagnostics
            .call_records
            .iter()
            .find(|record| {
                record.same_ordinary_target_calls > 0
                    && record.changed_ordinary_target_calls > 0
                    && record.non_ordinary_calls > 0
            })
            .expect("the mixed dynamic call site was retained");
        assert_eq!(call.calls, call.ordinary_calls + call.non_ordinary_calls);
        assert_eq!(
            call.ordinary_calls,
            call.first_ordinary_target_calls
                + call.same_ordinary_target_calls
                + call.changed_ordinary_target_calls
        );
        assert_eq!(call.first_ordinary_target_calls, 1);
        assert!(call.cached_native_target_calls >= 70, "call: {call:?}");
        assert_eq!(call.cached_shim_target_calls, 0);
        assert!(diagnostics.admission_records.iter().any(|record| {
            record.code_id == call.caller_code_id
                && !record.allowed
                && record.reason == JitAdmissionReason::DeniedCallBoundary
        }));
        assert!(
            diagnostics
                .compile_records
                .iter()
                .all(|record| record.code_id != call.caller_code_id),
            "the denied caller must not install an artifact: {diagnostics:?}"
        );
    }

    #[test]
    fn jit_call_diagnostics_are_hard_bounded() {
        let source = "function increment(value) { return value + 1; } function first(callback, value) { return callback(value); } function second(callback, value) { return callback(value); } first(increment, 1) + second(increment, 2)";

        let run = |call_records| {
            let mut context = Context::default();
            context.enable_jit_diagnostics(JitDiagnosticLimits {
                call_records,
                ..JitDiagnosticLimits::default()
            });
            let script =
                crate::Script::parse(crate::Source::from_bytes(source), None, &mut context)
                    .expect("parse");
            let result = script.evaluate(&mut context).expect("evaluate");
            assert_eq!(result.as_i32(), Some(5));
            context
                .jit_diagnostic_snapshot()
                .expect("diagnostics were enabled")
        };

        let one = run(1);
        assert_eq!(one.call_records.len(), 1);
        assert!(one.dropped_call_observations >= 1, "snapshot: {one:?}");

        let zero = run(0);
        assert!(zero.call_records.is_empty());
        assert!(zero.dropped_call_observations >= 2, "snapshot: {zero:?}");
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
    fn context_owned_jit_suppresses_tiny_hot_function_entries() {
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
        assert_eq!(stats.admission_denials, 1, "stats: {stats:?}");
        assert_eq!(stats.compilations, 0, "stats: {stats:?}");
        assert_eq!(stats.native_entries, 0, "stats: {stats:?}");
    }

    #[test]
    fn denied_wrapper_still_schedules_eligible_callee() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(n) { let total = 0; for (let i = 0; i < n; i++) { total += i; } return total; } function wrapper(n) { return sum(n); } let answer = 0; for (let i = 0; i < 80; i++) { answer = wrapper(10); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(45));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.admission_denials >= 1, "stats: {stats:?}");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn denied_leaf_preserves_exception_unwind_to_caller() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function read(object) { return object.value; } const plain = { value: 1 }; for (let i = 0; i < 80; i++) { read(plain); } const throwing = { get value() { throw new Error('expected'); } }; let caught = false; try { read(throwing); } catch (error) { caught = error.message === 'expected'; } caught",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_boolean(), Some(true));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.admission_denials >= 1, "stats: {stats:?}");
    }

    #[test]
    fn denied_property_reader_with_getter_is_not_classified_as_leaf() {
        let mut context = Context::default();
        context.enable_jit();
        context
            .register_global_callable(
                js_string!("probeNestedInterpreter"),
                0,
                NativeFunction::from_copy_closure(|_, _, context| {
                    assert_eq!(context.active_jit_backend_id, 0);
                    Ok(JsValue::new(45))
                }),
            )
            .expect("register probe");
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function read(input) { return input.value; } const object = { get value() { return probeNestedInterpreter(); } }; let answer = 0; for (let i = 0; i < 80; i++) { answer = read(object); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");
        let top_level = script.codeblock(&mut context).expect("top-level codeblock");
        let GlobalFunctionBinding { function_index, .. } = top_level.global_fns[0];
        let read_code = top_level.constant_function(function_index as usize);
        let backend_id = context.jit_backend.as_ref().expect("JIT backend").id();

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(45));
        assert_eq!(
            read_code.jit_admission(backend_id),
            crate::vm::JitAdmissionState::Denied
        );

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.admission_denials >= 1, "stats: {stats:?}");
    }

    #[test]
    fn admission_cache_is_scoped_to_backend_generation() {
        let mut context = Context::default();
        context.enable_jit();
        let setup = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(n) { let total = 0; for (let i = 0; i < n; i++) { total += i; } return total; } let first = 0; for (let i = 0; i < 80; i++) { first = sum(10); } first",
            ),
            None,
            &mut context,
        )
        .expect("parse setup");
        assert_eq!(
            setup
                .evaluate(&mut context)
                .expect("evaluate setup")
                .as_i32(),
            Some(45)
        );
        assert!(
            context
                .jit_stats()
                .expect("first JIT backend")
                .native_compilations
                >= 1
        );

        context.disable_jit();
        assert_eq!(context.active_jit_backend_id, 0);
        let interpreted =
            crate::Script::parse(crate::Source::from_bytes("sum(10)"), None, &mut context)
                .expect("parse interpreted call");
        assert_eq!(
            interpreted
                .evaluate(&mut context)
                .expect("evaluate with JIT disabled")
                .as_i32(),
            Some(45)
        );

        context.enable_jit();
        let warm_again = crate::Script::parse(
            crate::Source::from_bytes(
                "let second = 0; for (let i = 0; i < 80; i++) { second = sum(10); } second",
            ),
            None,
            &mut context,
        )
        .expect("parse second warmup");
        assert_eq!(
            warm_again
                .evaluate(&mut context)
                .expect("evaluate second warmup")
                .as_i32(),
            Some(45)
        );
        let stats = context.jit_stats().expect("second JIT backend");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn hotness_cache_is_scoped_to_backend_generation() {
        let code = first_function_code(
            "function sum(n) { let total = 0; for (let i = 0; i < n; i++) { total += i; } return total; }",
        );
        let thresholds = JitThresholds {
            function_entries: 2,
            loop_backedges: 3,
        };

        let mut first = JitBackend::new();
        first.set_thresholds(thresholds);
        assert!(!first.is_hot(&code));
        first.record_function_entry(&code);
        assert!(!first.is_hot(&code));
        first.record_function_entry(&code);
        assert!(first.is_hot(&code));
        assert_eq!(first.stats().hotness_threshold_crossings, 1);

        let mut replacement = JitBackend::new();
        replacement.set_thresholds(thresholds);
        assert!(
            !replacement.is_hot(&code),
            "a replacement backend must not inherit the prior generation's hotness"
        );
        assert!(!replacement.record_loop_backedge(&code, 1, 2));
        assert!(!replacement.record_loop_backedge(&code, 1, 2));
        assert!(!replacement.is_hot(&code));
        assert!(replacement.record_loop_backedge(&code, 1, 2));
        assert!(replacement.is_hot(&code));
        assert_eq!(replacement.stats().hotness_threshold_crossings, 1);
    }

    #[test]
    fn context_owned_jit_latches_hot_nonzero_backedges_and_enters_on_next_call() {
        let mut context = Context::default();
        context.enable_jit();
        let definition = crate::Script::parse(
            crate::Source::from_bytes(
                "function once(limit) { let total = 0.5; for (let i = 0; i < limit; i++) { total = total + i; } return total; }",
            ),
            None,
            &mut context,
        )
        .expect("parse definition");
        definition.evaluate(&mut context).expect("define function");

        let first =
            crate::Script::parse(crate::Source::from_bytes("once(1024)"), None, &mut context)
                .expect("parse first call");
        assert_eq!(
            first
                .evaluate(&mut context)
                .expect("evaluate first call")
                .as_number(),
            Some(523_776.5)
        );
        let first_stats = context.jit_stats().expect("JIT was enabled");
        assert_eq!(first_stats.compilations, 0, "stats: {first_stats:?}");
        assert_eq!(first_stats.native_entries, 0, "stats: {first_stats:?}");
        assert_eq!(first_stats.loop_backedges, 256, "stats: {first_stats:?}");
        assert_eq!(
            first_stats.hotness_threshold_crossings, 1,
            "stats: {first_stats:?}"
        );
        assert_eq!(
            first_stats.saturated_loop_backedges, 0,
            "stats: {first_stats:?}"
        );
        assert_eq!(first_stats.dormant_loop_frames, 1, "stats: {first_stats:?}");

        let second =
            crate::Script::parse(crate::Source::from_bytes("once(1024)"), None, &mut context)
                .expect("parse second call");
        assert_eq!(
            second
                .evaluate(&mut context)
                .expect("evaluate second call")
                .as_number(),
            Some(523_776.5)
        );
        let second_stats = context.jit_stats().expect("JIT was enabled");
        assert_eq!(
            second_stats.hotness_threshold_crossings, 1,
            "stats: {second_stats:?}"
        );
        assert!(
            second_stats.native_compilations >= 1,
            "stats: {second_stats:?}"
        );
        assert!(second_stats.native_entries >= 1, "stats: {second_stats:?}");
    }

    #[test]
    fn context_owned_jit_latches_statically_ineligible_one_shot_loop() {
        let mut context = Context::default();
        context.enable_jit();
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function once(limit) { let total = 0; for (let i = 0; i < limit; i++) { total = (total + i) | 0; } return total; } once(1024)",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        assert_eq!(
            script.evaluate(&mut context).expect("evaluate").as_i32(),
            Some(523_776)
        );
        let stats = context.jit_stats().expect("JIT was enabled");
        assert_eq!(stats.compilations, 0, "stats: {stats:?}");
        assert_eq!(stats.native_entries, 0, "stats: {stats:?}");
        assert_eq!(stats.loop_backedges, 256, "stats: {stats:?}");
        assert_eq!(stats.hotness_threshold_crossings, 1, "stats: {stats:?}");
        assert_eq!(stats.saturated_loop_backedges, 0, "stats: {stats:?}");
        assert_eq!(stats.dormant_loop_frames, 1, "stats: {stats:?}");
    }

    #[test]
    fn jit_diagnostics_retain_exact_post_threshold_backedge_counts() {
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function once(limit) { let total = 0; for (let i = 0; i < limit; i++) { total = (total + i) | 0; } return total; } once(1024)",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        assert_eq!(
            script.evaluate(&mut context).expect("evaluate").as_i32(),
            Some(523_776)
        );
        let stats = context.jit_stats().expect("JIT was enabled");
        assert_eq!(stats.loop_backedges, 1024, "stats: {stats:?}");
        assert_eq!(stats.hotness_threshold_crossings, 1, "stats: {stats:?}");
        assert_eq!(
            stats.saturated_loop_backedges,
            1024 - 256,
            "stats: {stats:?}"
        );
        assert_eq!(stats.dormant_loop_frames, 0, "stats: {stats:?}");

        let snapshot = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics enabled");
        assert_eq!(snapshot.loop_records.len(), 1, "snapshot: {snapshot:?}");
        let record = &snapshot.loop_records[0];
        assert_eq!(record.backedges, 1024, "record: {record:?}");
        assert_eq!(record.hotness_crossings, 1, "record: {record:?}");
        assert_eq!(record.closed_entry_backedges, 0, "record: {record:?}");
        assert!(!record.static_osr_candidate, "record: {record:?}");
        assert_eq!(
            record.static_osr_blocker,
            Some(JitCompileBlockerKind::UnsupportedOpcode),
            "record: {record:?}"
        );
        assert_eq!(record.first_blocking_opcode.as_deref(), Some("BitOr"));
        assert!(record.first_blocking_pc.is_some(), "record: {record:?}");
        assert!(record.region_instructions > 0, "record: {record:?}");
        assert_eq!(snapshot.dropped_loop_observations, 0);
    }

    #[test]
    fn jit_loop_diagnostics_classify_static_candidate_without_approving_osr() {
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function once(limit) { let total = 0.5; for (let i = 0; i < limit; i++) { total = total + i; } return total; } once(1024)",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        assert_eq!(
            script.evaluate(&mut context).expect("evaluate").as_number(),
            Some(523_776.5)
        );
        let snapshot = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics enabled");
        assert_eq!(snapshot.loop_records.len(), 1, "snapshot: {snapshot:?}");
        let record = &snapshot.loop_records[0];
        assert_eq!(record.backedges, 1024, "record: {record:?}");
        assert_eq!(record.hotness_crossings, 1, "record: {record:?}");
        assert!(record.static_osr_candidate, "record: {record:?}");
        assert_eq!(record.static_osr_blocker, None, "record: {record:?}");
        assert_eq!(record.first_blocking_opcode, None, "record: {record:?}");
        assert!(record.region_instructions > 0, "record: {record:?}");

        let serialized = serde_json::to_string(&snapshot).expect("serialize diagnostics");
        assert!(!serialized.contains("once"));
    }

    #[test]
    fn jit_loop_planner_proves_fractional_live_in_and_path_specific_exit() {
        let code = first_function_code(
            "function once(limit) { let total = 0.5; for (let i = 0; i < limit; i++) { total = total + i; } return total; }",
        );
        let (header_pc, backedge_pc) = canonical_loop(&code);
        let plan = native::plan_loop_region(
            &code,
            header_pc,
            backedge_pc,
            JitOsrRepresentation::F64,
            true,
            false,
        )
        .expect("selected loop must have a complete static plan");

        assert_eq!(
            plan.key,
            JitCacheKey::loop_region(
                code.debug_id,
                header_pc,
                backedge_pc,
                JitOsrRepresentation::F64,
                true,
                false,
            )
        );
        assert_eq!(
            plan.entry,
            vec![
                native::LoopEntryValue {
                    register: 1,
                    representation: JitOsrRepresentation::F64,
                    source: native::LoopEntrySource::VmRegister,
                },
                native::LoopEntryValue {
                    register: 2,
                    representation: JitOsrRepresentation::F64,
                    source: native::LoopEntrySource::VmRegister,
                },
                native::LoopEntryValue {
                    register: 4,
                    representation: JitOsrRepresentation::F64,
                    source: native::LoopEntrySource::VmRegister,
                },
            ]
        );
        assert_eq!(plan.exits.len(), 1);
        assert_eq!(
            plan.exits[0].materialize,
            vec![native::LoopExitValue {
                register: 1,
                source: native::LoopExitSource::NativeValue,
            }]
        );
        assert!(plan.exits[0].from_pc >= header_pc);
        assert!(plan.exits[0].from_pc < backedge_pc);
        assert!(plan.exits[0].resume_pc > backedge_pc);
        assert_eq!(plan.instruction_pcs.first(), Some(&header_pc));
        assert_eq!(plan.instruction_pcs.last(), Some(&backedge_pc));
        assert!(
            !plan.requires_f64,
            "the artifact chooses F64 from live state, not a region-local constant"
        );

        let integer_key = native::plan_loop_region(
            &code,
            header_pc,
            backedge_pc,
            JitOsrRepresentation::I32,
            true,
            false,
        )
        .expect("representation is selected by the later guarded-state slice")
        .key;
        assert_ne!(integer_key, plan.key);
    }

    #[test]
    fn jit_loop_planner_rejects_unmodelled_region_operations() {
        let code = first_function_code(
            "function divided(limit) { let total = 1; for (let i = 1; i < limit; i++) { total = total / i; } return total; }",
        );
        let (header_pc, backedge_pc) = canonical_loop(&code);
        assert_eq!(
            native::plan_loop_region(
                &code,
                header_pc,
                backedge_pc,
                JitOsrRepresentation::F64,
                false,
                false,
            )
            .err(),
            Some(native::LoopPlanRejection::UnsupportedRegionOpcode)
        );
    }

    #[test]
    fn jit_loop_planner_preserves_untouched_exit_values_in_vm_registers() {
        let code = first_function_code(
            "function preserve(limit, result) { for (let i = 0; i < limit; i++) {} return result; }",
        );
        let (header_pc, backedge_pc) = canonical_loop(&code);
        let plan = native::plan_loop_region(
            &code,
            header_pc,
            backedge_pc,
            JitOsrRepresentation::I32,
            false,
            false,
        )
        .expect("empty numeric loop has a provable exit map");

        let preserved = plan.exits[0]
            .materialize
            .iter()
            .find(|value| value.source == native::LoopExitSource::PreservedVmValue)
            .expect("the returned argument is untouched by the native region");
        assert!(
            plan.entry
                .iter()
                .all(|entry| entry.register != preserved.register),
            "an untouched exit-only register must not be loaded into native state"
        );
    }

    #[test]
    fn jit_loop_planner_rejects_i32_for_region_float_constants() {
        let code = first_function_code(
            "function fractional(limit) { let total = 0; for (let i = 0; i < limit; i++) { total = 0.5; } return total; }",
        );
        let (header_pc, backedge_pc) = canonical_loop(&code);
        assert_eq!(
            native::plan_loop_region(
                &code,
                header_pc,
                backedge_pc,
                JitOsrRepresentation::I32,
                false,
                false,
            )
            .err(),
            Some(native::LoopPlanRejection::RepresentationMismatch)
        );
    }

    #[test]
    fn jit_loop_region_state_is_bounded_without_forgetting_exact_keys() {
        let mut backend = JitBackend::new();
        backend.enable_diagnostics(JitDiagnosticLimits::default());
        backend.set_thresholds(JitThresholds {
            function_entries: u32::MAX,
            loop_backedges: 2,
        });

        for code_id in 0..MAX_LOOP_REGION_STATES as u64 {
            assert_eq!(
                backend.observe_loop_region(loop_key(code_id, 10)),
                LoopRegionAction::Cold
            );
        }
        assert_eq!(backend.loop_regions.len(), MAX_LOOP_REGION_STATES);

        let retained = loop_key(0, 10);
        assert_eq!(
            backend.observe_loop_region(retained),
            LoopRegionAction::Compile,
            "a full table must not suppress an already-retained exact key"
        );
        assert_eq!(
            backend.observe_loop_region(loop_key(999, 10)),
            LoopRegionAction::Suppressed(JitOsrSuppressionReason::RegionCapacity)
        );
        assert_eq!(backend.loop_regions.len(), MAX_LOOP_REGION_STATES);

        assert!(backend.reject_loop_region(retained, JitOsrRejectionReason::InvalidControlFlow));
        assert_eq!(
            backend.observe_loop_region(retained),
            LoopRegionAction::Closed
        );
        assert_eq!(backend.loop_regions.len(), MAX_LOOP_REGION_STATES);

        let counters = backend.stats().osr;
        assert_eq!(counters.cache_requests, 67);
        assert_eq!(counters.cache_hits, 2);
        assert_eq!(counters.cache_misses, 65);
        assert_eq!(counters.hotness_crossings, 1);
        assert_eq!(counters.suppressions.region_capacity, 1);
        assert_eq!(counters.rejections.invalid_control_flow, 1);
        assert_eq!(
            backend.diagnostic_snapshot().expect("diagnostics").osr,
            counters
        );
    }

    #[test]
    fn jit_loop_region_circuit_breakers_suppress_only_future_new_sites() {
        let mut slow = JitBackend::new();
        slow.enable_diagnostics(JitDiagnosticLimits::default());
        slow.set_thresholds(JitThresholds {
            function_entries: u32::MAX,
            loop_backedges: 1,
        });
        let retained = loop_key(1, 10);
        assert_eq!(
            slow.observe_loop_region(retained),
            LoopRegionAction::Compile
        );
        slow.record_loop_compile_attempt();
        slow.record_loop_compile_result(false, 64, MAX_LOOP_COMPILE_NS + 1);
        assert_eq!(
            slow.observe_loop_region(loop_key(2, 10)),
            LoopRegionAction::Suppressed(JitOsrSuppressionReason::CompileTime)
        );
        assert!(slow.loop_regions.contains_key(&retained));
        assert!(!slow.loop_regions.contains_key(&loop_key(2, 10)));
        assert_eq!(slow.stats().osr.compile_attempts, 1);
        assert_eq!(slow.stats().osr.compilations, 0);
        assert_eq!(slow.stats().osr.compile_time_ns, MAX_LOOP_COMPILE_NS + 1);
        assert_eq!(slow.stats().osr.code_bytes, 0);
        assert_eq!(slow.stats().osr.suppressions.compile_time, 1);

        let mut bytes = JitBackend::new();
        bytes.record_loop_compile_result(true, MAX_LOOP_CODE_BYTES + 1, 1);
        assert_eq!(
            bytes.observe_loop_region(loop_key(3, 10)),
            LoopRegionAction::Suppressed(JitOsrSuppressionReason::CodeBytes)
        );
        assert!(bytes.loop_regions.is_empty());
        assert_eq!(bytes.stats().osr.compilations, 1);
        assert_eq!(bytes.stats().osr.code_bytes, MAX_LOOP_CODE_BYTES + 1);
        assert_eq!(bytes.stats().osr.suppressions.code_bytes, 1);
    }

    #[test]
    fn jit_loop_diagnostics_observe_denied_dormant_frames_and_respect_zero_cap() {
        let source = "function blocked(limit) { let total = 0; for (let i = 0; i < limit; i++) { total = (total + i) | 0; } return total; } let answer = 0; for (let call = 0; call < 40; call++) answer = blocked(10); answer";
        let run = |loop_records| {
            let mut context = Context::default();
            context.enable_jit_diagnostics(JitDiagnosticLimits {
                loop_records,
                ..JitDiagnosticLimits::default()
            });
            let script =
                crate::Script::parse(crate::Source::from_bytes(source), None, &mut context)
                    .expect("parse");
            assert_eq!(
                script.evaluate(&mut context).expect("evaluate").as_i32(),
                Some(45)
            );
            context
                .jit_diagnostic_snapshot()
                .expect("diagnostics enabled")
        };

        let retained = run(8);
        let blocked = retained
            .loop_records
            .iter()
            .find(|record| record.first_blocking_opcode.as_deref() == Some("BitOr"))
            .expect("blocked callee loop record");
        assert_eq!(blocked.backedges, 400, "record: {blocked:?}");
        assert!(blocked.closed_entry_backedges > 0, "record: {blocked:?}");
        assert_eq!(retained.dropped_loop_observations, 0);

        let zero = run(0);
        assert!(zero.loop_records.is_empty());
        assert_eq!(zero.dropped_loop_observations, 440);
    }

    #[test]
    fn jit_interpreted_storage_diagnostics_are_exact_bounded_and_source_free() {
        let source = "function readStorage(object, array, key) { let total = 0; for (let index = 0; index < 4; index++) { total += object.distinctive_private_storage_name; total += array[index]; total += object[key]; total += array.length; } return total; } readStorage({ distinctive_private_storage_name: 3 }, [1, 2, 3, 4], 'distinctive_private_storage_name')";
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let script = crate::Script::parse(crate::Source::from_bytes(source), None, &mut context)
            .expect("parse");
        assert_eq!(
            script.evaluate(&mut context).expect("evaluate").as_i32(),
            Some(50)
        );

        let snapshot = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics enabled");
        let record = |kind| {
            snapshot
                .storage_records
                .iter()
                .find(|record| record.kind == kind)
                .unwrap_or_else(|| panic!("missing {kind:?} record: {snapshot:?}"))
        };

        let named = record(JitStorageSiteKind::Named);
        assert_eq!(named.executions, 4, "record: {named:?}");
        assert_eq!(named.inline_cache_hits, 3, "record: {named:?}");
        assert_eq!(named.inline_cache_misses, 1, "record: {named:?}");
        assert_eq!(named.inline_cache_not_applicable, 0, "record: {named:?}");

        let dense = record(JitStorageSiteKind::Dense);
        assert_eq!(dense.executions, 4, "record: {dense:?}");
        assert_eq!(dense.inline_cache_hits, 3, "record: {dense:?}");
        assert_eq!(dense.inline_cache_misses, 1, "record: {dense:?}");
        assert_eq!(dense.inline_cache_not_applicable, 0, "record: {dense:?}");

        let computed = record(JitStorageSiteKind::Computed);
        assert_eq!(computed.executions, 4, "record: {computed:?}");
        assert_eq!(computed.inline_cache_hits, 0, "record: {computed:?}");
        assert_eq!(computed.inline_cache_misses, 0, "record: {computed:?}");
        assert_eq!(
            computed.inline_cache_not_applicable, 4,
            "record: {computed:?}"
        );

        let length = record(JitStorageSiteKind::Length);
        assert_eq!(length.executions, 4, "record: {length:?}");
        assert_eq!(length.inline_cache_hits, 0, "record: {length:?}");
        assert_eq!(length.inline_cache_misses, 0, "record: {length:?}");
        assert_eq!(length.inline_cache_not_applicable, 4, "record: {length:?}");

        assert_eq!(snapshot.storage_records.len(), 4, "snapshot: {snapshot:?}");
        assert_eq!(snapshot.dropped_storage_observations, 0);
        let serialized = serde_json::to_string(&snapshot).expect("serialize diagnostics");
        assert!(!serialized.contains("distinctive_private_storage_name"));
    }

    #[test]
    fn jit_storage_diagnostics_observe_denied_dormant_frames_and_respect_zero_cap() {
        let source = "function blocked(object) { return object.distinctive_private_dormant_storage | 0; } const object = { distinctive_private_dormant_storage: 7 }; let answer = 0; for (let call = 0; call < 40; call++) answer = blocked(object); answer";
        let run = |storage_records| {
            let mut context = Context::default();
            context.enable_jit_diagnostics(JitDiagnosticLimits {
                storage_records,
                ..JitDiagnosticLimits::default()
            });
            let script =
                crate::Script::parse(crate::Source::from_bytes(source), None, &mut context)
                    .expect("parse");
            assert_eq!(
                script.evaluate(&mut context).expect("evaluate").as_i32(),
                Some(7)
            );
            context
                .jit_diagnostic_snapshot()
                .expect("diagnostics enabled")
        };

        let retained = run(8);
        assert_eq!(retained.storage_records.len(), 1, "snapshot: {retained:?}");
        let named = &retained.storage_records[0];
        assert_eq!(named.kind, JitStorageSiteKind::Named);
        assert_eq!(named.executions, 40, "record: {named:?}");
        assert_eq!(named.inline_cache_hits, 39, "record: {named:?}");
        assert_eq!(named.inline_cache_misses, 1, "record: {named:?}");

        let zero = run(0);
        assert!(zero.storage_records.is_empty(), "snapshot: {zero:?}");
        assert_eq!(zero.dropped_storage_observations, 40);
        let serialized = serde_json::to_string(&retained).expect("serialize diagnostics");
        assert!(!serialized.contains("distinctive_private_dormant_storage"));
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
    fn diagnostic_native_storage_artifacts_count_guard_hits_misses_and_loads() {
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function named(object, n) { let total = 0; for (let i = 0; i < n; i++) total += object.distinctive_private_native_name; return total; } function dense(values, n) { let total = 0; for (let i = 0; i < n; i++) total += values[i]; return total; } let object = { distinctive_private_native_name: 3 }; let values = [1, 2, 3]; let namedAnswer = 0; let denseAnswer = 0; for (let j = 0; j < 80; j++) { namedAnswer = named(object, 10); denseAnswer = dense(values, 3); } object.extra = 1; namedAnswer = named(object, 10); values[1] = 2.5; denseAnswer = dense(values, 3); namedAnswer === 30 && denseAnswer === 6.5",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        assert_eq!(
            script
                .evaluate(&mut context)
                .expect("evaluate")
                .as_boolean(),
            Some(true)
        );
        let snapshot = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics enabled");
        let native = snapshot.native_storage;
        assert!(native.named_guard_hits > 0, "snapshot: {snapshot:?}");
        assert!(native.named_guard_misses > 0, "snapshot: {snapshot:?}");
        assert_eq!(native.named_loads, native.named_guard_hits);
        assert!(native.dense_guard_hits > 0, "snapshot: {snapshot:?}");
        assert!(native.dense_guard_misses > 0, "snapshot: {snapshot:?}");
        assert_eq!(native.dense_loads, native.dense_guard_hits);
        assert_eq!(
            context.vm.jit_native_storage,
            JitNativeStorageRecord::default(),
            "per-entry scratch counters must be merged and cleared"
        );
        let serialized = serde_json::to_string(&snapshot).expect("serialize diagnostics");
        assert!(!serialized.contains("distinctive_private_native_name"));
    }

    #[test]
    fn diagnostic_native_storage_uses_a_separate_cache_variant() {
        let mut context = Context::default();
        context.enable_jit();
        let warm = crate::Script::parse(
            crate::Source::from_bytes(
                "function sumObject(object, n) { let total = 0; for (let i = 0; i < n; i++) total += object.value; return total; } let cachedObject = { value: 3 }; let answer = 0; for (let j = 0; j < 80; j++) answer = sumObject(cachedObject, 10); answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");
        assert_eq!(
            warm.evaluate(&mut context).expect("evaluate").as_i32(),
            Some(30)
        );
        let production_compilations = context.jit_stats().expect("JIT enabled").compilations;
        assert_eq!(
            context.vm.jit_native_storage,
            JitNativeStorageRecord::default(),
            "production helpers must not update diagnostic scratch counters"
        );

        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let diagnostic = crate::Script::parse(
            crate::Source::from_bytes("sumObject(cachedObject, 10)"),
            None,
            &mut context,
        )
        .expect("parse diagnostic call");
        assert_eq!(
            diagnostic
                .evaluate(&mut context)
                .expect("evaluate diagnostic call")
                .as_i32(),
            Some(30)
        );
        let diagnostic_stats = context.jit_stats().expect("JIT enabled");
        assert_eq!(
            diagnostic_stats.compilations,
            production_compilations + 1,
            "diagnostics must compile a distinct artifact"
        );
        let snapshot = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics enabled");
        assert_eq!(snapshot.native_storage.named_guard_hits, 10);
        assert_eq!(snapshot.native_storage.named_guard_misses, 0);
        assert_eq!(snapshot.native_storage.named_loads, 10);

        context.disable_jit_diagnostics();
        let production_again = crate::Script::parse(
            crate::Source::from_bytes("sumObject(cachedObject, 10)"),
            None,
            &mut context,
        )
        .expect("parse production call");
        assert_eq!(
            production_again
                .evaluate(&mut context)
                .expect("evaluate production call")
                .as_i32(),
            Some(30)
        );
        assert_eq!(
            context.jit_stats().expect("JIT enabled").compilations,
            diagnostic_stats.compilations,
            "disabling diagnostics must reuse the production artifact"
        );
        assert_eq!(
            context.vm.jit_native_storage,
            JitNativeStorageRecord::default()
        );
    }

    #[test]
    fn context_owned_jit_deopts_numeric_type_and_overflow_guards() {
        let mut context = Context::default();
        enable_jit_without_admission_floor(&mut context);
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function add(left, right) { return left + right; } function subtract(left, right) { return left - right; } let value = 0; for (let i = 0; i < 80; i++) { value = add(i, 1); } for (let i = 0; i < 80; i++) { value = subtract(i, 1); } let overflow = add(2147483647, 1); let text = add(\"x\", \"y\"); let negativeZero = subtract(-0, 0); (overflow === 2147483648 && text === \"xy\" && Object.is(negativeZero, -0)) ? 1 : 0",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(1));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.deopts >= 3, "stats: {stats:?}");
    }

    #[test]
    fn jit_exit_diagnostics_are_exact_bounded_and_opt_in() {
        let mut context = Context::default();
        enable_jit_without_admission_floor(&mut context);
        let warmup = crate::Script::parse(
            crate::Source::from_bytes(
                "function add(left, right) { return left + right; } let warm = 0; for (let i = 0; i < 80; i++) { warm = add(i, 1); } warm",
            ),
            None,
            &mut context,
        )
        .expect("parse warmup");
        warmup.evaluate(&mut context).expect("warm up");
        assert_eq!(context.jit_diagnostic_snapshot(), None);

        enable_jit_diagnostics_without_admission_floor(
            &mut context,
            JitDiagnosticLimits {
                compile_records: 0,
                admission_records: 0,
                exit_records: 1,
                call_records: 0,
                loop_records: 0,
                storage_records: 0,
            },
        );
        let overflow = crate::Script::parse(
            crate::Source::from_bytes("add(2147483647, 1)"),
            None,
            &mut context,
        )
        .expect("parse overflow");
        let result = overflow.evaluate(&mut context).expect("evaluate overflow");
        assert_eq!(result.as_number(), Some(2_147_483_648.0));

        let text = crate::Script::parse(
            crate::Source::from_bytes("add('x', 'y')"),
            None,
            &mut context,
        )
        .expect("parse type mismatch");
        let result = text.evaluate(&mut context).expect("evaluate type mismatch");
        assert_eq!(
            result
                .as_string()
                .map(|value| value.to_std_string_escaped())
                .as_deref(),
            Some("xy")
        );

        let snapshot = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        assert!(snapshot.compile_records.is_empty());
        assert_eq!(snapshot.exit_records.len(), 1, "snapshot: {snapshot:?}");
        let exit = snapshot.exit_records[0];
        assert_eq!(exit.kind, JitDiagnosticExitKind::Deopt);
        assert_eq!(exit.reason, JitExitReason::IntegerOverflow);
        assert_eq!(exit.count, 1);
        assert!(
            snapshot.dropped_exit_records >= 1,
            "the distinct argument-type exit must be counted as dropped: {snapshot:?}"
        );

        context.disable_jit_diagnostics();
        assert_eq!(context.jit_diagnostic_snapshot(), None);
    }

    #[test]
    fn jit_return_diagnostic_reports_caller_resume_pc() {
        let mut context = Context::default();
        enable_jit_without_admission_floor(&mut context);
        let warmup = crate::Script::parse(
            crate::Source::from_bytes(
                "function add(left, right) { return left + right; } let warm = 0; for (let i = 0; i < 80; i++) { warm = add(i, 1); } warm",
            ),
            None,
            &mut context,
        )
        .expect("parse warmup");
        warmup.evaluate(&mut context).expect("warm up");

        enable_jit_diagnostics_without_admission_floor(
            &mut context,
            JitDiagnosticLimits::default(),
        );
        let call = crate::Script::parse(
            crate::Source::from_bytes("let answer = add(20, 22); answer"),
            None,
            &mut context,
        )
        .expect("parse nested call");
        let result = call.evaluate(&mut context).expect("evaluate nested call");
        assert_eq!(result.as_i32(), Some(42));

        let snapshot = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        let return_exit = snapshot
            .exit_records
            .iter()
            .find(|record| record.reason == JitExitReason::Return)
            .expect("native return was recorded");
        assert_eq!(return_exit.kind, JitDiagnosticExitKind::Return);
        assert_ne!(
            return_exit.pc, 0,
            "nested native return must identify the caller resume PC"
        );
    }

    #[test]
    fn context_owned_jit_charges_native_loop_limit() {
        let mut context = Context::default();
        context.enable_jit();

        let definition = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(n) { let total = 0; for (let i = 0; i < n; i++) { total = total + i; } return total; }",
            ),
            None,
            &mut context,
        )
        .expect("parse definition");
        definition.evaluate(&mut context).expect("define");

        let warmup = crate::Script::parse(
            crate::Source::from_bytes(
                "let warm = 0; for (let i = 0; i < 80; i++) { warm = sum(10); } warm",
            ),
            None,
            &mut context,
        )
        .expect("parse warmup");
        warmup.evaluate(&mut context).expect("warm up");

        context.runtime_limits_mut().set_loop_iteration_limit(3);
        let limited =
            crate::Script::parse(crate::Source::from_bytes("sum(10)"), None, &mut context)
                .expect("parse limited call");
        let error = limited
            .evaluate(&mut context)
            .expect_err("native loop must enforce the runtime limit");

        assert_eq!(
            error.as_engine(),
            Some(&EngineError::RuntimeLimit(RuntimeLimitError::LoopIteration))
        );
        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    fn warmed_sum_context(jit: bool) -> Context {
        let mut context = Context::default();
        if jit {
            context.enable_jit();
        }

        let definition = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(n) { let total = 0; for (let i = 0; i < n; i++) { total = total + i; } return total; }",
            ),
            None,
            &mut context,
        )
        .expect("parse definition");
        definition.evaluate(&mut context).expect("define");

        let warmup = crate::Script::parse(
            crate::Source::from_bytes(
                "let warm = 0; for (let i = 0; i < 80; i++) { warm = sum(10); } warm",
            ),
            None,
            &mut context,
        )
        .expect("parse warmup");
        warmup.evaluate(&mut context).expect("warm up");

        context
    }

    fn evaluate_with_instruction_budget(
        context: &mut Context,
        source: &str,
        budget: usize,
    ) -> Result<JsValue, crate::JsError> {
        context.set_instruction_budget(budget);
        let script = crate::Script::parse(crate::Source::from_bytes(source), None, context)
            .expect("parse budgeted script");
        script.evaluate(context)
    }

    #[test]
    fn context_owned_jit_runs_native_entry_with_instruction_budget() {
        let mut interpreter = warmed_sum_context(false);
        let mut context = warmed_sum_context(true);
        let before = context.jit_stats().expect("JIT was enabled");

        let expected = evaluate_with_instruction_budget(&mut interpreter, "sum(100)", 10_000)
            .expect("interpreter sum");
        let result =
            evaluate_with_instruction_budget(&mut context, "sum(100)", 10_000).expect("native sum");

        assert_eq!(result, expected);
        assert_eq!(
            context.instruction_budget_remaining(),
            interpreter.instruction_budget_remaining()
        );
        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(
            stats.native_entries > before.native_entries,
            "stats: {stats:?}"
        );
        assert_eq!(
            stats.deopts, before.deopts,
            "a finite budget must not force a guard deopt: {stats:?}"
        );
    }

    #[test]
    fn context_owned_jit_exhausts_instruction_budget_in_native_code() {
        let mut interpreter = warmed_sum_context(false);
        let mut context = warmed_sum_context(true);
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let before = context.jit_stats().expect("JIT was enabled");

        let expected_error = evaluate_with_instruction_budget(&mut interpreter, "sum(100)", 20)
            .expect_err("the interpreter budget must stop execution");
        let error = evaluate_with_instruction_budget(&mut context, "sum(100)", 20)
            .expect_err("the finite instruction budget must stop execution");

        assert_eq!(error, expected_error);
        assert_eq!(error.as_engine(), Some(&EngineError::NoInstructionsRemain));
        assert_eq!(context.instruction_budget_remaining(), Some(0));
        assert_eq!(
            context.instruction_budget_remaining(),
            interpreter.instruction_budget_remaining()
        );
        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(
            stats.native_entries > before.native_entries,
            "stats: {stats:?}"
        );
        assert_eq!(
            stats.deopts, before.deopts,
            "budget exhaustion must be a completion, not a guard deopt: {stats:?}"
        );
        let diagnostics = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        assert!(diagnostics.exit_records.iter().any(|record| {
            record.kind == JitDiagnosticExitKind::Budget
                && record.reason == JitExitReason::RuntimeLimit
        }));
    }

    #[test]
    fn context_owned_jit_refunds_budget_before_guard_deopt() {
        let prepare = |jit: bool| {
            let mut context = Context::default();
            if jit {
                enable_jit_without_admission_floor(&mut context);
            }
            let script = crate::Script::parse(
                crate::Source::from_bytes(
                    "function add(a, b) { return a + b; } let warm = 0; for (let i = 0; i < 80; i++) { warm = add(i, 1); } warm",
                ),
                None,
                &mut context,
            )
            .expect("parse warmup");
            script.evaluate(&mut context).expect("warm up");
            context
        };

        let mut interpreter = prepare(false);
        let mut context = prepare(true);
        let before = context.jit_stats().expect("JIT was enabled");
        let source = "[add(2147483647, 1), add('x', 'y')].join(',')";

        let expected = evaluate_with_instruction_budget(&mut interpreter, source, 1_000)
            .expect("interpreter guarded calls");
        let result = evaluate_with_instruction_budget(&mut context, source, 1_000)
            .expect("JIT guarded calls");

        assert_eq!(result, expected);
        assert_eq!(
            context.instruction_budget_remaining(),
            interpreter.instruction_budget_remaining(),
            "guard deopts must not double-charge the current bytecode"
        );
        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.deopts >= before.deopts + 2, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_preserves_exception_propagation_through_call() {
        let mut context = Context::default();
        enable_jit_without_admission_floor(&mut context);
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function fail() { throw new Error(\"boom\"); } function apply(function_value) { return function_value(); } let caught = 0; for (let i = 0; i < 80; i++) { try { apply(fail); } catch (error) { if (error.message === \"boom\") { caught++; } } } caught",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(80));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_preserves_recursion_limit() {
        let mut context = Context::default();
        context.enable_jit();

        let definition = crate::Script::parse(
            crate::Source::from_bytes(
                "function recurse(n) { if (n === 0) { return 0; } return 1 + recurse(n - 1); }",
            ),
            None,
            &mut context,
        )
        .expect("parse definition");
        definition.evaluate(&mut context).expect("define");

        let warmup = crate::Script::parse(
            crate::Source::from_bytes(
                "let warm = 0; for (let i = 0; i < 80; i++) { warm = recurse(10); } warm",
            ),
            None,
            &mut context,
        )
        .expect("parse warmup");
        warmup.evaluate(&mut context).expect("warm up");

        context.runtime_limits_mut().set_recursion_limit(8);
        let limited = crate::Script::parse(
            crate::Source::from_bytes("recurse(100)"),
            None,
            &mut context,
        )
        .expect("parse limited recursion");
        let error = limited
            .evaluate(&mut context)
            .expect_err("native recursion must enforce the runtime limit");

        assert_eq!(
            error.as_engine(),
            Some(&EngineError::RuntimeLimit(RuntimeLimitError::Recursion))
        );
    }

    #[test]
    fn context_owned_jit_deopts_dense_load_on_hole() {
        let mut context = Context::default();
        context.enable_jit();

        let setup = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum(values, n) { let total = 0; for (let i = 0; i < n; i++) { total = total + values[i]; } return total; } let values = [1, 2, 3]; let warm = 0; for (let i = 0; i < 80; i++) { warm = sum(values, 3); } warm",
            ),
            None,
            &mut context,
        )
        .expect("parse setup");
        setup.evaluate(&mut context).expect("setup");

        let hole = crate::Script::parse(
            crate::Source::from_bytes(
                "delete values[1]; let result = sum(values, 3); result !== result",
            ),
            None,
            &mut context,
        )
        .expect("parse hole case");
        let result = hole.evaluate(&mut context).expect("evaluate hole case");
        assert_eq!(result.as_boolean(), Some(true));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.deopts >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_survives_forced_gc_around_property_guard() {
        let mut context = Context::default();
        enable_jit_without_admission_floor(&mut context);

        let setup = crate::Script::parse(
            crate::Source::from_bytes(
                "function read(object) { return object.value; } let object = { value: 3 }; let warm = 0; for (let i = 0; i < 80; i++) { warm = read(object); } warm",
            ),
            None,
            &mut context,
        )
        .expect("parse setup");
        setup.evaluate(&mut context).expect("setup");

        boa_gc::force_collect();

        let after_gc = crate::Script::parse(
            crate::Source::from_bytes("read(object)"),
            None,
            &mut context,
        )
        .expect("parse after GC");
        let result = after_gc.evaluate(&mut context).expect("evaluate after GC");
        assert_eq!(result.as_i32(), Some(3));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_dispatches_native_ordinary_function_call() {
        let mut context = Context::default();
        enable_jit_diagnostics_without_admission_floor(
            &mut context,
            JitDiagnosticLimits::default(),
        );
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
        assert!(stats.scheduler_call_exits > 0, "stats: {stats:?}");
        let diagnostics = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        assert!(diagnostics.exit_records.iter().any(|record| {
            record.kind == JitDiagnosticExitKind::Call
                && record.reason == JitExitReason::Scheduler
                && record.count > 0
        }));
    }

    #[test]
    fn context_owned_jit_deopts_non_ordinary_call_to_interpreter() {
        let mut context = Context::default();
        enable_jit_without_admission_floor(&mut context);
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
    fn context_owned_jit_deopts_different_ordinary_call_target() {
        let mut context = Context::default();
        enable_jit_without_admission_floor(&mut context);
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function add(left, right) { return left + right; } function subtract(left, right) { return left - right; } function apply(function_value, left, right) { return function_value(left, right); } let answer = 0; for (let i = 0; i < 80; i++) { answer = answer + apply(add, 20, 22); } answer = answer + apply(subtract, 20, 22); answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(3358));

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 2, "stats: {stats:?}");
        assert!(stats.deopts >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_deopts_property_shape_mismatch_to_interpreter() {
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
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
        let diagnostics = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        assert!(diagnostics.exit_records.iter().any(|record| {
            record.kind == JitDiagnosticExitKind::Deopt
                && record.reason == JitExitReason::NamedProperty
        }));
    }

    #[test]
    fn context_owned_jit_reads_current_global_declarative_binding() {
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "let limit = 4; function sum() { let total = 0.5; for (let i = 0; i < limit; i++) { total = total + 1.5; } return total; } let warm = 0; for (let i = 0; i < 80; i++) { warm = sum(); } let before = sum(); limit = 2; before + ',' + sum()",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(
            result
                .as_string()
                .expect("string result")
                .to_std_string_escaped(),
            "6.5,3.5"
        );

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_compilations >= 1, "stats: {stats:?}");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
        let diagnostics = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        assert!(diagnostics.compile_records.iter().any(|record| {
            record.outcome == JitCompileOutcome::Native
                && record.first_blocking_opcode.is_none()
                && record.native_backward_branches > 0
        }));
    }

    #[test]
    fn context_owned_jit_deopts_mismatched_global_binding_read() {
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "let limit = 4; function sum() { let total = 0.5; for (let i = 0; i < limit; i++) { total = total + 1.5; } return total; } let warm = 0; for (let i = 0; i < 80; i++) { warm = sum(); } let before = sum(); limit = '1'; before + ',' + sum()",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(
            result
                .as_string()
                .expect("string result")
                .to_std_string_escaped(),
            "6.5,2"
        );

        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
        assert!(stats.deopts >= 1, "stats: {stats:?}");
        let diagnostics = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        assert!(diagnostics.exit_records.iter().any(|record| {
            record.kind == JitDiagnosticExitKind::Deopt
                && record.reason == JitExitReason::BindingRead
        }));
    }

    #[test]
    fn context_owned_jit_replays_tdz_binding_read() {
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        context.set_jit_thresholds(JitThresholds {
            function_entries: 1,
            loop_backedges: 1,
        });
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "function sum() { let total = 0.5; for (let i = 0; i < limit; i++) { total = total + 1.5; } return total; } sum(); let limit = 2;",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let error = script
            .evaluate(&mut context)
            .expect_err("TDZ read must throw after native guard fallback");
        assert_eq!(
            error.as_native().map(crate::JsNativeError::kind),
            Some(&JsNativeErrorKind::Reference)
        );

        let diagnostics = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        assert!(diagnostics.exit_records.iter().any(|record| {
            record.kind == JitDiagnosticExitKind::Deopt
                && record.reason == JitExitReason::BindingRead
        }));
    }

    #[test]
    fn context_owned_jit_binding_read_survives_forced_gc() {
        let mut context = Context::default();
        context.enable_jit();
        let setup = crate::Script::parse(
            crate::Source::from_bytes(
                "let holder = { value: 3 }; let limit = 10; function sum() { let total = 0; for (let i = 0; i < limit; i++) { total = total + holder.value; } return total; } let warm = 0; for (let i = 0; i < 80; i++) { warm = sum(); } warm",
            ),
            None,
            &mut context,
        )
        .expect("parse setup");
        setup.evaluate(&mut context).expect("warm up");

        boa_gc::force_collect();

        let call = crate::Script::parse(crate::Source::from_bytes("sum()"), None, &mut context)
            .expect("parse call");
        let result = call.evaluate(&mut context).expect("evaluate after GC");
        assert_eq!(result.as_i32(), Some(30));
        let stats = context.jit_stats().expect("JIT was enabled");
        assert!(stats.native_entries >= 1, "stats: {stats:?}");
    }

    #[test]
    fn context_owned_jit_binding_guard_refunds_instruction_budget() {
        let prepare = |jit: bool| {
            let mut context = Context::default();
            if jit {
                context.enable_jit();
            }
            let definition = crate::Script::parse(
                crate::Source::from_bytes(
                    "let limit = 4; function sum() { let total = 0.5; for (let i = 0; i < limit; i++) { total = total + 1.5; } return total; } let warm = 0; for (let i = 0; i < 80; i++) { warm = sum(); } warm",
                ),
                None,
                &mut context,
            )
            .expect("parse definition");
            definition.evaluate(&mut context).expect("warm up");
            context
        };

        let mut interpreter = prepare(false);
        let mut context = prepare(true);
        let expected =
            evaluate_with_instruction_budget(&mut interpreter, "limit = '2'; sum()", 1_000)
                .expect("interpreter binding fallback");
        let result = evaluate_with_instruction_budget(&mut context, "limit = '2'; sum()", 1_000)
            .expect("JIT binding fallback");

        assert_eq!(result, expected);
        assert_eq!(
            context.instruction_budget_remaining(),
            interpreter.instruction_budget_remaining(),
            "binding guard fallback must not double-charge GetName"
        );
    }

    #[test]
    fn context_owned_jit_rejects_direct_eval_binding_scope() {
        let mut context = Context::default();
        context.enable_jit_diagnostics(JitDiagnosticLimits::default());
        let script = crate::Script::parse(
            crate::Source::from_bytes(
                "let limit = 3; function sum() { eval(''); let total = 0; for (let index = 0; index < limit; index++) { total = total + 2; } return total; } let answer = 0; for (let index = 0; index < 80; index++) { answer = sum(); } answer",
            ),
            None,
            &mut context,
        )
        .expect("parse");

        let result = script.evaluate(&mut context).expect("evaluate");
        assert_eq!(result.as_i32(), Some(6));
        let diagnostics = context
            .jit_diagnostic_snapshot()
            .expect("diagnostics were enabled");
        assert!(diagnostics.admission_records.iter().any(|record| {
            !record.allowed
                && record.reason == JitAdmissionReason::DeniedNativeIneligible
                && record.first_blocking_opcode.as_deref() == Some("PushScope")
        }));
    }

    #[test]
    fn context_owned_jit_global_binding_reads_are_realm_scoped() {
        let evaluate = |limit: i32| {
            let mut context = Context::default();
            context.enable_jit();
            let source = format!(
                "let limit = {limit}; function sum() {{ let total = 0; for (let index = 0; index < limit; index++) {{ total = total + 2; }} return total; }} let answer = 0; for (let index = 0; index < 80; index++) {{ answer = sum(); }} answer"
            );
            let script = crate::Script::parse(
                crate::Source::from_bytes(source.as_bytes()),
                None,
                &mut context,
            )
            .expect("parse");
            let result = script.evaluate(&mut context).expect("evaluate");
            let stats = context.jit_stats().expect("JIT was enabled");
            assert!(stats.native_compilations >= 1, "stats: {stats:?}");
            assert!(stats.native_entries >= 1, "stats: {stats:?}");
            result.as_i32().expect("integer result")
        };

        assert_eq!(evaluate(2), 4);
        assert_eq!(evaluate(5), 10);
    }

    #[test]
    fn jit_exit_round_trip() {
        for (kind, pc) in [
            (JitExitKind::Deopt, 0),
            (JitExitKind::Return, 17),
            (JitExitKind::Call, u32::MAX),
            (JitExitKind::Completion, 42),
            (JitExitKind::Budget, 99),
            (JitExitKind::EntryRejected, 101),
            (JitExitKind::Continuation, 103),
        ] {
            let status = JitExit::encode_with_reason(kind, JitExitReason::Unknown, pc);
            assert_eq!(
                JitExit::decode(status),
                Some(JitExit {
                    kind,
                    reason: JitExitReason::Unknown,
                    pc,
                })
            );
        }

        let status =
            JitExit::encode_with_reason(JitExitKind::Deopt, JitExitReason::IntegerOverflow, 31);
        assert_eq!(
            JitExit::decode(status),
            Some(JitExit {
                kind: JitExitKind::Deopt,
                reason: JitExitReason::IntegerOverflow,
                pc: 31,
            })
        );

        let continuation =
            JitExit::encode_with_reason(JitExitKind::Continuation, JitExitReason::LoopExit, 47);
        assert_eq!(
            JitExit::decode(continuation),
            Some(JitExit {
                kind: JitExitKind::Continuation,
                reason: JitExitReason::LoopExit,
                pc: 47,
            })
        );

        assert_eq!(JitExit::decode(7), None);
        assert_eq!(JitExit::decode(JIT_BREAK_BIT), None);
    }

    /// Run `src` through both the interpreter and the JIT and assert identical
    /// `i32` results. Exercises the explicit JIT execution and
    /// deopt-to-interpreter hand-off across program shapes; the tiered tests
    /// below separately cover native loops, property reads, and calls.
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
