use arrayvec::ArrayVec;
use itertools::Itertools;
use std::{cell::Cell, fmt};

use boa_gc::GcRefCell;
use boa_macros::{Finalize, Trace};

use crate::{
    JsString,
    object::shape::{Shape, WeakShape, slot::Slot},
};

#[cfg(test)]
mod tests;

pub(crate) const PIC_CAPACITY: usize = 4;

// ---------------------------------------------------------------------------
// Element-access inline cache
// ---------------------------------------------------------------------------

/// The kind of indexed storage observed at an element-access site.
///
/// Stored in [`ElementIC`] as feedback for both the interpreter fast path
/// and the JIT Stage 2 specialiser. Knowing that a site always sees
/// `DenseI32` lets the JIT emit a direct `i32` load without a tag check.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum IndexedKind {
    /// All elements are `i32` (stored as `ThinVec<i32>`).
    DenseI32,
    /// All elements fit in `f64` (stored as `ThinVec<f64>`).
    DenseF64,
    /// Elements are arbitrary `JsValue`s (stored as `ThinVec<JsValue>`).
    DenseElement,
    /// Sparse storage whose observed indexed property was a data descriptor.
    SparseData,
}

/// The seeded payload of an [`ElementIC`]. Bundled into its own struct so
/// that the unseeded state can be represented as `None` without needing a
/// sentinel `WeakShape` value.
#[derive(Clone, Debug, Trace, Finalize)]
struct ElementICEntry {
    /// Raw heap address of the cached receiver shape. Used for the hot-path
    /// pointer-equality compare. Never read without also checking `shape`
    /// liveness — see [`ElementIC::matches`].
    #[unsafe_ignore_trace]
    shape_addr: usize,

    /// Weak liveness guard for the cached shape.
    shape: WeakShape,

    /// The kind of indexed storage observed when this entry was created.
    /// `IndexedKind` contains no GC-managed pointers.
    #[unsafe_ignore_trace]
    indexed_kind: IndexedKind,
}

/// A single-entry inline cache for `GetPropertyByValue` /
/// `SetPropertyByValue` sites whose key is a numeric index (`obj[i]`).
///
/// ## Design
///
/// Unlike the named-property PIC, element-access ICs are monomorphic: a
/// site that iterates over one indexed receiver is the overwhelming common case.
/// When a second (different) receiver shape is observed we overwrite the
/// entry with the new shape, betting that the most-recent winner is the
/// future winner too (last-write-wins eviction).
///
/// ## Interior mutability
///
/// The seeded entry is held in a `GcRefCell` so that `seed` can update
/// the IC through a shared `&ElementIC` reference, matching the access
/// pattern used by the named-property `InlineCache::set`. The borrow
/// discipline mirrors the PIC: `matches` holds a short-lived immutable
/// borrow, `seed` holds a brief mutable borrow on the cold (miss) path.
///
/// ## GC soundness
///
/// The same fused address + liveness discipline as [`CacheEntry::matches`]
/// applies here: we store the raw heap address alongside a [`WeakShape`]
/// liveness guard, and the two checks are always performed together in
/// [`ElementIC::matches`]. Splitting them would reintroduce the
/// address-reuse false-positive described in the [`CacheEntry`] docs.
///
/// ## JIT fuel
///
/// [`ElementIC::indexed_kind`] exposes the observed storage kind so that JIT
/// Stage 2 can emit a specialised load (e.g. a direct `i32` array read)
/// without re-profiling element accesses from scratch.
#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) struct ElementIC {
    /// Cached entry, or `None` when the IC is unseeded (cold site).
    /// `GcRefCell` for interior mutability through a `&CodeBlock` reference.
    entry: GcRefCell<Option<ElementICEntry>>,
}

impl ElementIC {
    /// Return an empty (unseeded) IC.
    #[inline]
    pub(crate) fn new() -> Self {
        Self {
            entry: GcRefCell::new(None),
        }
    }

    /// Fused address + liveness check — the hot path.
    ///
    /// Returns `Some(kind)` iff the IC has a seeded entry whose shape is
    /// the same live shape as `shape`, `None` on any miss (including the
    /// unseeded state).
    ///
    /// The `Option` unwrap short-circuits before the address compare for
    /// the cold (unseeded) case; the address compare short-circuits before
    /// `is_upgradable()` for the wrong-shape case.
    #[inline]
    pub(crate) fn matches(&self, shape: &Shape) -> Option<IndexedKind> {
        let entry_ref = self.entry.borrow();
        let entry = entry_ref.as_ref()?;
        if entry.shape_addr == shape.to_addr_usize() && entry.shape.is_upgradable() {
            Some(entry.indexed_kind)
        } else {
            None
        }
    }

    /// Seed (or overwrite) the IC with the observed `(shape, indexed_kind)`.
    ///
    /// Called on the slow path when the receiver exposes cacheable indexed data. On the
    /// next execution with the same receiver shape, [`matches`] returns
    /// `Some(kind)` and the fast path skips generic internal-method dispatch
    /// and property-key conversion.
    ///
    /// [`matches`]: ElementIC::matches
    pub(crate) fn seed(&self, shape: &Shape, kind: IndexedKind) {
        *self.entry.borrow_mut() = Some(ElementICEntry {
            shape_addr: shape.to_addr_usize(),
            shape: shape.into(),
            indexed_kind: kind,
        });
    }

    /// Expose the cached indexed-storage kind for JIT feedback queries.
    ///
    /// Returns `None` when the IC is unseeded.
    #[inline]
    #[allow(dead_code)] // consumed by JIT Stage 2
    pub(crate) fn indexed_kind(&self) -> Option<IndexedKind> {
        self.entry.borrow().as_ref().map(|e| e.indexed_kind)
    }
}

// ---------------------------------------------------------------------------
// Named-property polymorphic inline cache (existing)
// ---------------------------------------------------------------------------

/// A cached shape-to-slot mapping for a polymorphic inline cache.
///
/// The address compare and the liveness check are intentionally fused into
/// [`CacheEntry::matches`]. Both halves are load-bearing for GC soundness —
/// see [`WeakShape::is_upgradable`] for the finalize-before-sweep argument
/// that the IC hit path relies on. Splitting them out for "one less load"
/// would silently reintroduce the use-after-free class of bug, so the
/// raw address field is private and there is no public accessor that
/// returns it without also checking liveness.
#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) struct CacheEntry {
    /// Address of the cached shape's `Inner` GC allocation. Used for the
    /// pointer-equality check that dominates the IC hit path.
    ///
    /// **Private on purpose.** Callers consume cache entries through
    /// [`CacheEntry::matches`], which pairs this load with the liveness
    /// check. Reading `shape_addr` alone is unsound: if the cached shape's
    /// allocation has been freed, the GC is free to reuse its address for
    /// a fresh allocation, and an unguarded pointer-equality compare will
    /// produce a false-positive hit on the new (and very different) shape.
    #[unsafe_ignore_trace]
    shape_addr: usize,

    /// Weak reference to the cached shape. Only consulted on the IC hit
    /// path for an aliveness check (`is_upgradable()`, no atomic ops); the
    /// `upgrade()` path is reserved for cold operations.
    pub(crate) shape: WeakShape,

    /// Slot within the shape's property table where the property lives.
    #[unsafe_ignore_trace]
    pub(crate) slot: Slot,
}

impl CacheEntry {
    /// Fused address + liveness check.
    ///
    /// Returns `true` iff `shape`'s GC allocation is the one this entry
    /// cached *and* that allocation is still live. The two checks must
    /// stay paired: see [`WeakShape::is_upgradable`] for the
    /// finalize-before-sweep argument that makes this single combined
    /// check sufficient (and the equivalent of an `upgrade()` call,
    /// minus the atomic ref-count traffic).
    #[inline]
    pub(crate) fn matches(&self, shape: &Shape) -> bool {
        self.shape_addr == shape.to_addr_usize() && self.shape.is_upgradable()
    }
}

/// An inline cache entry for a property access.
#[derive(Clone, Debug, Trace, Finalize)]
pub(crate) struct InlineCache {
    /// The property that is accessed.
    pub(crate) name: JsString,

    /// Cached shape-to-slot entries.
    ///
    /// Most sites are monomorphic. Keeping their first entry out of the
    /// `ArrayVec` avoids the polymorphic container's length/iteration work on
    /// every hit. A second live shape promotes the state to the existing
    /// bounded PIC without changing its liveness or megamorphic behavior.
    entries: GcRefCell<InlineCacheEntries>,

    /// Whether this access site has seen too many shapes and should no longer be cached.
    #[unsafe_ignore_trace]
    pub(crate) megamorphic: Cell<bool>,
}

/// Storage for a named-property inline cache.
#[derive(Clone, Debug, Trace, Finalize)]
enum InlineCacheEntries {
    /// The site has not observed a cacheable receiver yet.
    Empty,
    /// The common one-shape case.
    Mono(CacheEntry),
    /// The polymorphic case, bounded by [`PIC_CAPACITY`].
    Poly(Box<ArrayVec<CacheEntry, PIC_CAPACITY>>),
}

impl fmt::Display for InlineCache {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "(name:{} entries:", self.name.display_escaped())?;

        if self.megamorphic.get() {
            return write!(f, "(megamorphic))");
        }

        let entries = self.entries.borrow();
        match &*entries {
            InlineCacheEntries::Empty => write!(f, "())"),
            InlineCacheEntries::Mono(entry) => {
                write!(f, "({:#x}))", entry.shape.to_addr_usize())
            }
            InlineCacheEntries::Poly(entries) => {
                // `shape_addr` is private — `WeakShape::to_addr_usize()`
                // returns the same address while the shape is live and `0`
                // once it's been collected, which is a strictly more
                // informative display anyway.
                let entries = entries.iter().map(|e| e.shape.to_addr_usize()).format(", ");
                write!(f, "({entries:#x}))")
            }
        }
    }
}

impl InlineCache {
    pub(crate) fn new(name: JsString) -> Self {
        Self {
            name,
            entries: GcRefCell::new(InlineCacheEntries::Empty),
            megamorphic: Cell::new(false),
        }
    }

    /// Cache a `(shape, slot)` pair. If the cache is full, transition to
    /// megamorphic and stop caching. Stale (dead-weak) entries are evicted
    /// before deciding whether the cache is full.
    pub(crate) fn set(&self, shape: &Shape, slot: Slot) {
        if self.megamorphic.get() {
            return;
        }

        let new_entry = CacheEntry {
            shape_addr: shape.to_addr_usize(),
            shape: shape.into(),
            slot,
        };

        let mut entries = self.entries.borrow_mut();
        let previous = std::mem::replace(&mut *entries, InlineCacheEntries::Empty);

        *entries = match &previous {
            InlineCacheEntries::Empty => InlineCacheEntries::Mono(new_entry),
            InlineCacheEntries::Mono(previous) => {
                if previous.shape.is_upgradable() {
                    let mut entries = ArrayVec::new();
                    entries.push(previous.clone());
                    entries.push(new_entry);
                    InlineCacheEntries::Poly(Box::new(entries))
                } else {
                    // The old weak shape was collected while this site was
                    // cold; reuse the compact monomorphic representation.
                    InlineCacheEntries::Mono(new_entry)
                }
            }
            InlineCacheEntries::Poly(previous) => {
                let mut entries = (**previous).clone();
                // Cleanup pass: drop entries whose shapes have been
                // collected. This remains cold-path work.
                entries.retain(|entry| entry.shape.is_upgradable());

                if entries.try_push(new_entry).is_err() {
                    // Polymorphic cache is full, transition to megamorphic.
                    self.megamorphic.set(true);
                    InlineCacheEntries::Empty
                } else {
                    InlineCacheEntries::Poly(Box::new(entries))
                }
            }
        };
    }

    /// Returns the number of live cache slots, for focused cache tests.
    #[cfg(test)]
    pub(crate) fn entry_count(&self) -> usize {
        match &*self.entries.borrow() {
            InlineCacheEntries::Empty => 0,
            InlineCacheEntries::Mono(_) => 1,
            InlineCacheEntries::Poly(entries) => entries.len(),
        }
    }

    /// Returns a copy of a cache entry, for focused cache tests.
    #[cfg(test)]
    pub(crate) fn entry(&self, index: usize) -> Option<CacheEntry> {
        match &*self.entries.borrow() {
            InlineCacheEntries::Empty => None,
            InlineCacheEntries::Mono(entry) => (index == 0).then(|| entry.clone()),
            InlineCacheEntries::Poly(entries) => entries.get(index).cloned(),
        }
    }

    /// Fast IC lookup. Returns the cached `Slot` for the given shape, or
    /// `None` on miss / megamorphic / cache-stale.
    ///
    /// This is the hot path of every cached property access. The work is:
    ///   * one branch on `megamorphic`
    ///   * a borrow of the entries vec (debug-checked refcount in
    ///     `GcRefCell`, ~one load + cmov)
    ///   * up to `PIC_CAPACITY` (=4) [`CacheEntry::matches`] calls — each
    ///     a pointer-equality compare paired with a plain-load liveness
    ///     check on the entry's `WeakShape`
    ///
    /// The previous implementation called `WeakShape::upgrade()` per
    /// candidate entry, costing two atomic ref-count operations per IC hit
    /// (one to construct the `Gc`, one to drop it).
    #[inline]
    pub(crate) fn get(&self, shape: &Shape) -> Option<Slot> {
        if self.megamorphic.get() {
            return None;
        }

        match &*self.entries.borrow() {
            InlineCacheEntries::Empty => None,
            InlineCacheEntries::Mono(entry) => entry.matches(shape).then_some(entry.slot),
            InlineCacheEntries::Poly(entries) => {
                for entry in entries.iter() {
                    if entry.matches(shape) {
                        return Some(entry.slot);
                    }
                }
                None
            }
        }
    }
}
