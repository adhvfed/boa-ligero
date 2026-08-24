/// Represents the limits of different runtime operations.
#[derive(Debug, Clone, Copy)]
pub struct RuntimeLimits {
    /// Max stack size before an error is thrown.
    stack_size: usize,

    /// Max loop iterations before an error is thrown.
    loop_iteration: u64,

    /// Max backtrace count in exception.
    backtrace_limit: usize,

    /// Max function recursion limit
    recursion: usize,

    /// Native-stack cost of one host re-entry, in units of one plain JS frame.
    host_frame_cost: usize,
}

impl Default for RuntimeLimits {
    #[inline]
    fn default() -> Self {
        Self {
            loop_iteration: u64::MAX,
            recursion: 512,
            backtrace_limit: 50,
            stack_size: 1024 * 10,
            host_frame_cost: 1,
        }
    }
}

impl RuntimeLimits {
    /// Return the loop iteration limit.
    ///
    /// If the limit is exceeded in a loop it will throw an error.
    ///
    /// The limit value [`u64::MAX`] means that there is no limit.
    #[inline]
    #[must_use]
    pub const fn loop_iteration_limit(&self) -> u64 {
        self.loop_iteration
    }

    /// Set the loop iteration limit.
    ///
    /// If the limit is exceeded in a loop it will throw an error.
    ///
    /// Setting the limit to [`u64::MAX`] means that there is no limit.
    #[inline]
    pub fn set_loop_iteration_limit(&mut self, value: u64) {
        self.loop_iteration = value;
    }

    /// Disable loop iteration limit.
    #[inline]
    pub fn disable_loop_iteration_limit(&mut self) {
        self.loop_iteration = u64::MAX;
    }

    /// Get max backtrace limit for an exception.
    ///
    /// Default is 50.
    #[inline]
    #[must_use]
    pub const fn backtrace_limit(&self) -> usize {
        self.backtrace_limit
    }

    /// Set max backtrace limit for an exception.
    #[inline]
    pub fn set_backtrace_limit(&mut self, value: usize) {
        self.backtrace_limit = value;
    }

    /// Get max stack size.
    #[inline]
    #[must_use]
    pub const fn stack_size_limit(&self) -> usize {
        self.stack_size
    }

    /// Set max stack size before an error is thrown.
    #[inline]
    pub fn set_stack_size_limit(&mut self, value: usize) {
        self.stack_size = value;
    }

    /// Get recursion limit.
    #[inline]
    #[must_use]
    pub const fn recursion_limit(&self) -> usize {
        self.recursion
    }

    /// Set recursion limit before an error is thrown.
    #[inline]
    pub fn set_recursion_limit(&mut self, value: usize) {
        self.recursion = value;
    }

    /// Get the native-stack cost charged against [`Self::recursion_limit`] for
    /// each host re-entry, expressed in units of one plain JS frame.
    ///
    /// Not every frame the recursion limit counts costs the same amount of
    /// native stack. A plain JS call pushes a heap-allocated `CallFrame` and
    /// keeps running inside the *same* `Context::run()` loop, so its native
    /// footprint is just the interpreter's own per-call bookkeeping. A host
    /// call that re-enters the VM (an accessor, or an embedder API such as
    /// `dispatchEvent` invoking a JS listener) instead nests a whole new
    /// native `Context::run()` frame chain underneath itself.
    ///
    /// Measured on `x86_64-unknown-linux-gnu`, release (`lto = "fat"`,
    /// `codegen-units = 1`), by recursing each way with both engine guards
    /// raised out of the way and bisecting the depth at which the process
    /// takes a stack-overflow abort:
    ///
    /// | thread stack | plain JS frames | host re-entries |
    /// |---|---|---|
    /// | 8 `MiB` (OS-default main thread) | crashes between 25,000-30,000 | crashes between 1,700-1,800 |
    /// | 2 `MiB` (`std::thread` default)  | crashes between  6,500-7,000  | crashes between   400-500   |
    ///
    /// Both profiles put one host re-entry at roughly **15-16 plain JS
    /// frames**, which a direct stack-pointer probe agrees with (~318 bytes
    /// per JS frame against ~4,924 bytes per host re-entry).
    ///
    /// Summing the two 1:1 -- the default, kept for compatibility -- therefore
    /// calibrates for neither: a limit low enough to bound host recursion is
    /// ~15x stricter than it needs to be for the JS-only recursion real pages
    /// actually perform, and a limit high enough for JS recursion does not
    /// bound host recursion at all. Because the two costs are *additive* (a
    /// page can freely interleave them), two independent limits cannot fix
    /// this either; only a single weighted budget bounds the real exposure.
    ///
    /// Default is 1, preserving the historical 1:1 accounting.
    #[inline]
    #[must_use]
    pub const fn host_frame_cost(&self) -> usize {
        self.host_frame_cost
    }

    /// Set the native-stack cost charged per host re-entry.
    ///
    /// See [`Self::host_frame_cost`] for how to pick a value.
    #[inline]
    pub fn set_host_frame_cost(&mut self, value: usize) {
        self.host_frame_cost = value;
    }
}
