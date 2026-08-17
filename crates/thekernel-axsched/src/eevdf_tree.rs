//! An allocation-free intrusive red-black tree for EEVDF ready entities.
//!
//! The tree is deliberately local rather than built on top of
//! `intrusive-collections`: EEVDF needs an augmentation that the latter does
//! not provide.  A tree owns exactly one strong `Arc` count for every linked
//! node.  Insertion consumes that count and stores it as a raw pointer;
//! removal reconstructs the same count.  Consequently the tree is usable on
//! a ready path without allocating or cloning an `Arc`.
//!
//! # Safety boundary and invariants
//!
//! The only unsafe code is the pointer manipulation in this module.  Safe
//! callers cannot obtain a mutable node while it is linked: the link and the
//! ordering fields are private, and an `Arc` returned by `remove` can be
//! staged before it is inserted again.  While linked, the following
//! invariants are maintained by every operation:
//!
//! * every non-null child and parent pointer is a node owned by this tree;
//! * links form one rooted, acyclic binary-search tree, ordered by the
//!   immutable-for-this-link `key` field;
//! * the root has no parent, every leaf is conceptually a black null node,
//!   and red nodes have black children;
//! * every root-to-null path has the same number of black nodes;
//! * `subtree_min` is exactly the minimum `eligible_at` in the complete
//!   subtree, with `u128::MAX` as the empty-subtree identity;
//! * a linked node has exactly one ownership unit held by the tree.  A node
//!   cannot be linked into two trees because a second insertion observes its
//!   private link state; a foreign removal is checked by both key and pointer
//!   identity.
//!
//! Selection never scans: it follows at most one child per tree level and
//! uses the cached subtree minimum to prune subtrees that contain no eligible
//! entity.
//!
//! Provenance: this implementation was independently derived from this
//! repository's `docs/design/0001-eevdf-readiness.md` contract.  No external
//! implementation or source-code text was consulted or incorporated; the
//! red-black algorithms and invariant tests below are original Rust code.

#![allow(dead_code)]

use alloc::sync::Arc;
use core::{cell::UnsafeCell, fmt, marker::PhantomData, mem::MaybeUninit, ptr};

const EMPTY_MIN: u128 = u128::MAX;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Color {
    Red,
    Black,
}

struct NodeState<K, T> {
    key: K,
    eligible_at: u128,
    parent: *mut EevdfNode<K, T>,
    left: *mut EevdfNode<K, T>,
    right: *mut EevdfNode<K, T>,
    color: Color,
    subtree_min: u128,
    linked: bool,
    value_initialized: bool,
}

/// A caller-owned intrusive EEVDF node.
///
/// `key` and `eligible_at` are the structural snapshots used by the tree.
/// They may be changed through [`EevdfNode::stage_unlinked`] only after the
/// node has been removed.  The value is never inspected by the tree.
pub struct EevdfNode<K, T> {
    // The scheduler transfers this payload between the task and the tree
    // while the node is unlinked. MaybeUninit makes that transfer explicit
    // without an allocation or an Arc cycle.
    value: UnsafeCell<MaybeUninit<T>>,
    state: UnsafeCell<NodeState<K, T>>,
}

impl<K, T> EevdfNode<K, T> {
    /// Creates an unlinked node.
    pub(crate) const fn new_unlinked(key: K, eligible_at: u128, value: T) -> Self {
        Self {
            value: UnsafeCell::new(MaybeUninit::new(value)),
            state: UnsafeCell::new(NodeState {
                key,
                eligible_at,
                parent: ptr::null_mut(),
                left: ptr::null_mut(),
                right: ptr::null_mut(),
                color: Color::Black,
                subtree_min: eligible_at,
                linked: false,
                value_initialized: true,
            }),
        }
    }

    /// Consumes an unlinked node and returns its payload.
    ///
    /// A linked node is owned by the tree rather than by its caller.  Keeping
    /// this operation crate-private and checking the link state prevents a
    /// caller from dropping the caller's ownership unit while a tree still
    /// contains the raw pointer.
    pub(crate) fn into_value(self) -> T {
        assert!(!self.is_linked(), "cannot consume a linked EEVDF node");
        // SAFETY: the node was constructed with an initialized value and the
        // link-state assertion above leaves this ownership unit with the
        // caller.  `take_value` marks the slot uninitialized before the node
        // is forgotten, so its Drop implementation does not run T's drop a
        // second time.
        let value = unsafe { self.take_value() };
        core::mem::forget(self);
        value
    }

    /// Returns the caller's payload.
    pub fn value(&self) -> &T {
        // SAFETY: callers use this while the node is linked, or after they
        // have restored a transferred value through put_value.
        unsafe { (*self.value.get()).assume_init_ref() }
    }

    /// Takes the payload while the node is unlinked and owned by the caller.
    /// The caller must restore a value before the node is dropped or linked.
    pub(crate) unsafe fn take_value(&self) -> T {
        let state = &mut *self.state.get();
        debug_assert!(state.value_initialized);
        state.value_initialized = false;
        (*self.value.get()).assume_init_read()
    }

    /// Restores a payload taken with take_value.
    pub(crate) unsafe fn put_value(&self, value: T) {
        let state = &mut *self.state.get();
        debug_assert!(!state.value_initialized);
        (*self.value.get()).write(value);
        state.value_initialized = true;
    }

    pub(crate) unsafe fn key(&self) -> K
    where
        K: Copy,
    {
        (*self.state.get()).key
    }

    /// Returns the cached eligibility value while the caller holds the
    /// scheduler/tree exclusion.
    pub(crate) unsafe fn eligible_at(&self) -> u128 {
        (*self.state.get()).eligible_at
    }

    pub(crate) fn is_linked(&self) -> bool {
        // SAFETY: tree operations are protected by the scheduler ownership
        // boundary, just like the rest of this node's structural state.
        unsafe { (*self.state.get()).linked }
    }

    /// Stages structural state after removal and before re-insertion.
    ///
    /// # Safety
    ///
    /// The caller must hold the scheduler's logical ownership of this task,
    /// prove that the node is not linked into any tree, and exclude all tree
    /// operations and other structural accesses for the complete duration of
    /// the call.  In particular, the caller must not use this operation while
    /// another CPU can remove, insert, or select this node.  The scheduler
    /// lock/transaction that owns the ready queue is the intended exclusion
    /// mechanism.  The method is crate-private because callers outside this
    /// scheduler crate cannot prove those conditions.
    pub(crate) unsafe fn stage_unlinked(&self, key: K, eligible_at: u128) {
        let state = &mut *self.state.get();
        debug_assert!(!state.linked);
        state.key = key;
        state.eligible_at = eligible_at;
        state.subtree_min = eligible_at;
    }
}

impl<K, T> Drop for EevdfNode<K, T> {
    fn drop(&mut self) {
        unsafe {
            if (*self.state.get()).value_initialized {
                ptr::drop_in_place((*self.value.get()).as_mut_ptr());
            }
        }
    }
}

// Structural state is mutated only by a tree operation holding exclusive
// scheduler ownership, or by the explicitly unsafe `stage_unlinked` contract.
// The sole safe concurrent observation is `value()`, whose `T: Sync` bound is
// carried by these conditional implementations.  K also travels with the
// task between SMP owners, so requiring K: Send + Sync is deliberately
// conservative and makes the proof independent of compiler auto-traits for
// UnsafeCell and raw links.
unsafe impl<K: Send + Sync, T: Send + Sync> Send for EevdfNode<K, T> {}
unsafe impl<K: Send + Sync, T: Send + Sync> Sync for EevdfNode<K, T> {}

/// Typed failures reported by the intrusive tree.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EevdfTreeError {
    /// The same ownership unit was inserted more than once.
    AlreadyLinked,
    /// Another node with the same key is already in this tree.
    DuplicateKey,
    /// The target is absent from this particular tree.
    ForeignNode,
}

/// An insertion error that retains the consumed ownership unit.
pub(crate) struct EevdfInsertError<K, T> {
    kind: EevdfTreeError,
    node: Arc<EevdfNode<K, T>>,
}

impl<K, T> EevdfInsertError<K, T> {
    /// Returns the typed cause without exposing the retained node.
    pub(crate) fn kind(&self) -> EevdfTreeError {
        self.kind
    }

    /// Recovers the exact `Arc` ownership unit supplied to `insert`.
    pub(crate) fn into_node(self) -> Arc<EevdfNode<K, T>> {
        self.node
    }
}

impl<K, T> fmt::Debug for EevdfInsertError<K, T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("EevdfInsertError")
            .field("kind", &self.kind)
            .finish_non_exhaustive()
    }
}

/// An intrusive augmented red-black tree ordered by `K`.
///
/// The tree stores no allocation of its own.  Its raw pointers represent the
/// one `Arc` ownership unit consumed for each successful insertion.
pub struct EevdfTree<K, T> {
    root: *mut EevdfNode<K, T>,
    len: usize,
    // This tells dropck that the raw pointers represent owned Arcs.
    _owned: PhantomData<Arc<EevdfNode<K, T>>>,
}

impl<K, T> EevdfTree<K, T> {
    /// Creates an empty tree.
    pub(crate) const fn new() -> Self {
        Self {
            root: ptr::null_mut(),
            len: 0,
            _owned: PhantomData,
        }
    }

    /// Number of linked nodes.
    pub(crate) const fn len(&self) -> usize {
        self.len
    }

    /// Whether no nodes are linked.
    pub(crate) const fn is_empty(&self) -> bool {
        self.len == 0
    }
}

// A tree is moved only as one scheduler-owned value, normally under an
// external run-queue lock.  Its raw links are not safe to share by `&Tree`, so
// no Sync implementation is provided; Send is sound when the key/payload are
// themselves transferable and shareable and every operation remains behind
// that scheduler ownership boundary.
unsafe impl<K: Send + Sync, T: Send + Sync> Send for EevdfTree<K, T> {}

impl<K: Ord + Copy, T> EevdfTree<K, T> {
    /// Inserts one `Arc` ownership unit without allocating.
    ///
    /// On failure, the returned error retains and can return that same unit
    /// through [`EevdfInsertError::into_node`].
    pub(crate) fn insert(
        &mut self,
        node: Arc<EevdfNode<K, T>>,
    ) -> Result<(), EevdfInsertError<K, T>> {
        let raw = Arc::into_raw(node) as *mut EevdfNode<K, T>;

        // SAFETY: `raw` is a live Arc allocation and remains owned by either
        // the returned error or this tree for the duration of this method.
        unsafe {
            if (*(*raw).state.get()).linked {
                return Err(EevdfInsertError {
                    kind: EevdfTreeError::AlreadyLinked,
                    node: Arc::from_raw(raw),
                });
            }
        }

        let key = unsafe { (*(*raw).state.get()).key };
        let mut parent = ptr::null_mut();
        let mut cursor = self.root;
        while !cursor.is_null() {
            parent = cursor;
            // SAFETY: every cursor was reached from this tree's links.
            let cursor_key = unsafe { (*(*cursor).state.get()).key };
            if key < cursor_key {
                cursor = unsafe { (*(*cursor).state.get()).left };
            } else if key > cursor_key {
                cursor = unsafe { (*(*cursor).state.get()).right };
            } else {
                return Err(EevdfInsertError {
                    kind: EevdfTreeError::DuplicateKey,
                    node: unsafe { Arc::from_raw(raw) },
                });
            }
        }

        // SAFETY: `raw` is not linked and `parent` is either null or a live
        // node in this tree.
        unsafe {
            let link = &mut *(*raw).state.get();
            link.parent = parent;
            link.left = ptr::null_mut();
            link.right = ptr::null_mut();
            link.color = Color::Red;
            link.subtree_min = (*(*raw).state.get()).eligible_at;
            link.linked = true;
            if parent.is_null() {
                self.root = raw;
            } else if key < (*(*parent).state.get()).key {
                (*(*parent).state.get()).left = raw;
            } else {
                (*(*parent).state.get()).right = raw;
            }
        }
        self.len += 1;
        // Recompute before and after fixup.  Rotations also recompute their
        // directly affected nodes, so each walk is logarithmic.
        unsafe { self.recompute_upwards(parent) };
        unsafe { self.insert_fixup(raw) };
        unsafe { self.recompute_upwards(raw) };
        Ok(())
    }

    /// Returns the minimum-key node without exposing any structural state.
    ///
    /// The returned borrow is tied to the tree borrow, so safe Rust prevents
    /// a concurrent mutable tree operation for its lifetime.  The tree is not
    /// `Sync`; callers additionally hold the scheduler's external run-queue
    /// lock when sharing it between CPUs.
    pub(crate) fn front(&self) -> Option<&EevdfNode<K, T>> {
        if self.root.is_null() {
            None
        } else {
            // SAFETY: root is a live node owned by this tree, and the shared
            // borrow of `self` keeps the tree (and therefore that node) alive.
            Some(unsafe { &*self.minimum(self.root) })
        }
    }

    /// Returns the cached minimum eligibility point for the whole tree.
    pub(crate) fn min_eligible_at(&self) -> Option<u128> {
        if self.root.is_null() {
            None
        } else {
            // SAFETY: the shared tree borrow excludes mutation through the
            // safe API, and the caller provides the scheduler lock boundary
            // for cross-CPU access.
            Some(unsafe { (*(*self.root).state.get()).subtree_min })
        }
    }

    /// Returns the earliest-deadline node whose cached eligibility is at or
    /// before `now` without unlinking it. The descent is O(log n).
    pub(crate) fn peek_earliest_eligible(&self, now: u128) -> Option<&EevdfNode<K, T>> {
        let mut cursor = self.root;
        while !cursor.is_null() {
            let (left, right, node_min, eligible) = unsafe {
                let link = &*(*cursor).state.get();
                (link.left, link.right, link.subtree_min, link.eligible_at)
            };
            if node_min > now {
                return None;
            }
            if !left.is_null() && unsafe { (*(*left).state.get()).subtree_min } <= now {
                cursor = left;
            } else if eligible <= now {
                // SAFETY: cursor is a live node owned by this tree and the
                // shared borrow excludes mutation for this lifetime.
                return Some(unsafe { &*cursor });
            } else if !right.is_null() && unsafe { (*(*right).state.get()).subtree_min } <= now {
                cursor = right;
            } else {
                return None;
            }
        }
        None
    }

    /// Removes exactly `node` from this tree and returns the same Arc unit.
    ///
    /// Key equality alone is insufficient: a same-key node from another tree
    /// is reported as [`EevdfTreeError::ForeignNode`].
    pub(crate) fn remove(
        &mut self,
        node: &EevdfNode<K, T>,
    ) -> Result<Arc<EevdfNode<K, T>>, EevdfTreeError> {
        let target = node as *const EevdfNode<K, T> as *mut EevdfNode<K, T>;
        if !self.contains_exact(target) {
            return Err(EevdfTreeError::ForeignNode);
        }
        let raw = unsafe { self.remove_raw(target) };
        // SAFETY: remove_raw detached the one Arc unit owned by this tree.
        Ok(unsafe { Arc::from_raw(raw) })
    }

    /// Alias that makes the exact-node nature of [`Self::remove`] explicit.
    pub(crate) fn remove_node(
        &mut self,
        node: &EevdfNode<K, T>,
    ) -> Result<Arc<EevdfNode<K, T>>, EevdfTreeError> {
        self.remove(node)
    }

    /// Removes the earliest-deadline node whose `eligible_at <= now`.
    ///
    /// The cached minimum is consulted at every branch; no ineligible nodes
    /// are linearly skipped.
    pub(crate) fn pop_earliest_eligible(&mut self, now: u128) -> Option<Arc<EevdfNode<K, T>>> {
        let mut cursor = self.root;
        let mut candidate = ptr::null_mut();
        while !cursor.is_null() {
            let (left, right, node_min, eligible) = unsafe {
                let link = &*(*cursor).state.get();
                (
                    link.left,
                    link.right,
                    link.subtree_min,
                    (*(*cursor).state.get()).eligible_at,
                )
            };
            if node_min > now {
                break;
            }
            if !left.is_null() && unsafe { (*(*left).state.get()).subtree_min } <= now {
                cursor = left;
            } else if eligible <= now {
                candidate = cursor;
                break;
            } else if !right.is_null() && unsafe { (*(*right).state.get()).subtree_min } <= now {
                cursor = right;
            } else {
                break;
            }
        }
        if candidate.is_null() {
            None
        } else {
            // SAFETY: candidate was selected from this tree's links.
            let raw = unsafe { self.remove_raw(candidate) };
            // SAFETY: remove_raw detached the tree's sole ownership unit.
            Some(unsafe { Arc::from_raw(raw) })
        }
    }

    fn contains_exact(&self, target: *mut EevdfNode<K, T>) -> bool {
        if target.is_null() {
            return false;
        }
        // An unlinked node cannot be in this tree.  The linked bit also
        // rejects a node linked into a foreign tree before the key walk.
        if unsafe { !(*(*target).state.get()).linked } {
            return false;
        }
        let key = unsafe { (*(*target).state.get()).key };
        let mut cursor = self.root;
        while !cursor.is_null() {
            let cursor_key = unsafe { (*(*cursor).state.get()).key };
            if key < cursor_key {
                cursor = unsafe { (*(*cursor).state.get()).left };
            } else if key > cursor_key {
                cursor = unsafe { (*(*cursor).state.get()).right };
            } else {
                return cursor == target;
            }
        }
        false
    }

    unsafe fn remove_raw(&mut self, z: *mut EevdfNode<K, T>) -> *mut EevdfNode<K, T> {
        let mut y = z;
        let mut y_color = (*(*y).state.get()).color;
        let x;
        let x_parent;

        let z_left = (*(*z).state.get()).left;
        let z_right = (*(*z).state.get()).right;
        if z_left.is_null() {
            x = z_right;
            x_parent = (*(*z).state.get()).parent;
            self.transplant(z, z_right);
        } else if z_right.is_null() {
            x = z_left;
            x_parent = (*(*z).state.get()).parent;
            self.transplant(z, z_left);
        } else {
            y = self.minimum(z_right);
            y_color = (*(*y).state.get()).color;
            x = (*(*y).state.get()).right;
            if (*(*y).state.get()).parent == z {
                x_parent = y;
                if !x.is_null() {
                    (*(*x).state.get()).parent = y;
                }
            } else {
                x_parent = (*(*y).state.get()).parent;
                self.transplant(y, x);
                (*(*y).state.get()).right = z_right;
                (*(*z_right).state.get()).parent = y;
            }
            self.transplant(z, y);
            (*(*y).state.get()).left = z_left;
            (*(*z_left).state.get()).parent = y;
            (*(*y).state.get()).color = (*(*z).state.get()).color;
        }

        self.len -= 1;
        if y_color == Color::Black {
            self.delete_fixup(x, x_parent);
        }

        // Structural deletion changes only paths through the replacement and
        // the former successor parent.  Recomputing both paths is O(log n).
        self.recompute_upwards(x);
        self.recompute_upwards(x_parent);
        if !y.is_null() {
            self.recompute_upwards(y);
        }
        if !self.root.is_null() {
            (*(*self.root).state.get()).parent = ptr::null_mut();
            (*(*self.root).state.get()).color = Color::Black;
        }

        let z_link = &mut *(*z).state.get();
        z_link.parent = ptr::null_mut();
        z_link.left = ptr::null_mut();
        z_link.right = ptr::null_mut();
        z_link.color = Color::Black;
        z_link.subtree_min = (*(*z).state.get()).eligible_at;
        z_link.linked = false;
        z
    }

    unsafe fn minimum(&self, mut node: *mut EevdfNode<K, T>) -> *mut EevdfNode<K, T> {
        while !(*(*node).state.get()).left.is_null() {
            node = (*(*node).state.get()).left;
        }
        node
    }

    unsafe fn color(node: *mut EevdfNode<K, T>) -> Color {
        if node.is_null() {
            Color::Black
        } else {
            (*(*node).state.get()).color
        }
    }

    unsafe fn set_color(node: *mut EevdfNode<K, T>, color: Color) {
        if !node.is_null() {
            (*(*node).state.get()).color = color;
        }
    }

    unsafe fn recompute(&self, node: *mut EevdfNode<K, T>) {
        if node.is_null() {
            return;
        }
        let link = &mut *(*node).state.get();
        let left_min = if link.left.is_null() {
            EMPTY_MIN
        } else {
            (*(*link.left).state.get()).subtree_min
        };
        let right_min = if link.right.is_null() {
            EMPTY_MIN
        } else {
            (*(*link.right).state.get()).subtree_min
        };
        link.subtree_min = (*(*node).state.get())
            .eligible_at
            .min(left_min)
            .min(right_min);
    }

    unsafe fn recompute_upwards(&self, mut node: *mut EevdfNode<K, T>) {
        while !node.is_null() {
            self.recompute(node);
            node = (*(*node).state.get()).parent;
        }
    }

    unsafe fn rotate_left(&mut self, x: *mut EevdfNode<K, T>) {
        let y = (*(*x).state.get()).right;
        debug_assert!(!y.is_null());
        let y_left = (*(*y).state.get()).left;
        (*(*x).state.get()).right = y_left;
        if !y_left.is_null() {
            (*(*y_left).state.get()).parent = x;
        }
        let x_parent = (*(*x).state.get()).parent;
        (*(*y).state.get()).parent = x_parent;
        if x_parent.is_null() {
            self.root = y;
        } else if x == (*(*x_parent).state.get()).left {
            (*(*x_parent).state.get()).left = y;
        } else {
            (*(*x_parent).state.get()).right = y;
        }
        (*(*y).state.get()).left = x;
        (*(*x).state.get()).parent = y;
        self.recompute(x);
        self.recompute(y);
    }

    unsafe fn rotate_right(&mut self, x: *mut EevdfNode<K, T>) {
        let y = (*(*x).state.get()).left;
        debug_assert!(!y.is_null());
        let y_right = (*(*y).state.get()).right;
        (*(*x).state.get()).left = y_right;
        if !y_right.is_null() {
            (*(*y_right).state.get()).parent = x;
        }
        let x_parent = (*(*x).state.get()).parent;
        (*(*y).state.get()).parent = x_parent;
        if x_parent.is_null() {
            self.root = y;
        } else if x == (*(*x_parent).state.get()).right {
            (*(*x_parent).state.get()).right = y;
        } else {
            (*(*x_parent).state.get()).left = y;
        }
        (*(*y).state.get()).right = x;
        (*(*x).state.get()).parent = y;
        self.recompute(x);
        self.recompute(y);
    }

    unsafe fn insert_fixup(&mut self, mut z: *mut EevdfNode<K, T>) {
        while !(*(*z).state.get()).parent.is_null()
            && Self::color((*(*z).state.get()).parent) == Color::Red
        {
            let parent = (*(*z).state.get()).parent;
            let grand = (*(*parent).state.get()).parent;
            if parent == (*(*grand).state.get()).left {
                let uncle = (*(*grand).state.get()).right;
                if Self::color(uncle) == Color::Red {
                    Self::set_color(parent, Color::Black);
                    Self::set_color(uncle, Color::Black);
                    Self::set_color(grand, Color::Red);
                    z = grand;
                } else {
                    if z == (*(*parent).state.get()).right {
                        z = parent;
                        self.rotate_left(z);
                    }
                    let parent = (*(*z).state.get()).parent;
                    let grand = (*(*parent).state.get()).parent;
                    Self::set_color(parent, Color::Black);
                    Self::set_color(grand, Color::Red);
                    self.rotate_right(grand);
                }
            } else {
                let uncle = (*(*grand).state.get()).left;
                if Self::color(uncle) == Color::Red {
                    Self::set_color(parent, Color::Black);
                    Self::set_color(uncle, Color::Black);
                    Self::set_color(grand, Color::Red);
                    z = grand;
                } else {
                    if z == (*(*parent).state.get()).left {
                        z = parent;
                        self.rotate_right(z);
                    }
                    let parent = (*(*z).state.get()).parent;
                    let grand = (*(*parent).state.get()).parent;
                    Self::set_color(parent, Color::Black);
                    Self::set_color(grand, Color::Red);
                    self.rotate_left(grand);
                }
            }
        }
        Self::set_color(self.root, Color::Black);
    }

    unsafe fn transplant(&mut self, old: *mut EevdfNode<K, T>, new: *mut EevdfNode<K, T>) {
        let parent = (*(*old).state.get()).parent;
        if parent.is_null() {
            self.root = new;
        } else if old == (*(*parent).state.get()).left {
            (*(*parent).state.get()).left = new;
        } else {
            (*(*parent).state.get()).right = new;
        }
        if !new.is_null() {
            (*(*new).state.get()).parent = parent;
        }
    }

    unsafe fn delete_fixup(
        &mut self,
        mut x: *mut EevdfNode<K, T>,
        mut x_parent: *mut EevdfNode<K, T>,
    ) {
        while x != self.root && Self::color(x) == Color::Black {
            if x_parent.is_null() {
                break;
            }
            if x == (*(*x_parent).state.get()).left {
                let mut sibling = (*(*x_parent).state.get()).right;
                if Self::color(sibling) == Color::Red {
                    Self::set_color(sibling, Color::Black);
                    Self::set_color(x_parent, Color::Red);
                    self.rotate_left(x_parent);
                    sibling = (*(*x_parent).state.get()).right;
                }
                let sibling_left = if sibling.is_null() {
                    ptr::null_mut()
                } else {
                    (*(*sibling).state.get()).left
                };
                let sibling_right = if sibling.is_null() {
                    ptr::null_mut()
                } else {
                    (*(*sibling).state.get()).right
                };
                if Self::color(sibling_left) == Color::Black
                    && Self::color(sibling_right) == Color::Black
                {
                    Self::set_color(sibling, Color::Red);
                    x = x_parent;
                    x_parent = (*(*x).state.get()).parent;
                } else {
                    if Self::color(sibling_right) == Color::Black {
                        Self::set_color(sibling_left, Color::Black);
                        Self::set_color(sibling, Color::Red);
                        if !sibling.is_null() {
                            self.rotate_right(sibling);
                        }
                        sibling = (*(*x_parent).state.get()).right;
                    }
                    Self::set_color(sibling, Self::color(x_parent));
                    Self::set_color(x_parent, Color::Black);
                    let sibling_right = if sibling.is_null() {
                        ptr::null_mut()
                    } else {
                        (*(*sibling).state.get()).right
                    };
                    Self::set_color(sibling_right, Color::Black);
                    self.rotate_left(x_parent);
                    x = self.root;
                    x_parent = ptr::null_mut();
                }
            } else {
                let mut sibling = (*(*x_parent).state.get()).left;
                if Self::color(sibling) == Color::Red {
                    Self::set_color(sibling, Color::Black);
                    Self::set_color(x_parent, Color::Red);
                    self.rotate_right(x_parent);
                    sibling = (*(*x_parent).state.get()).left;
                }
                let sibling_right = if sibling.is_null() {
                    ptr::null_mut()
                } else {
                    (*(*sibling).state.get()).right
                };
                let sibling_left = if sibling.is_null() {
                    ptr::null_mut()
                } else {
                    (*(*sibling).state.get()).left
                };
                if Self::color(sibling_right) == Color::Black
                    && Self::color(sibling_left) == Color::Black
                {
                    Self::set_color(sibling, Color::Red);
                    x = x_parent;
                    x_parent = (*(*x).state.get()).parent;
                } else {
                    if Self::color(sibling_left) == Color::Black {
                        Self::set_color(sibling_right, Color::Black);
                        Self::set_color(sibling, Color::Red);
                        if !sibling.is_null() {
                            self.rotate_left(sibling);
                        }
                        sibling = (*(*x_parent).state.get()).left;
                    }
                    Self::set_color(sibling, Self::color(x_parent));
                    Self::set_color(x_parent, Color::Black);
                    let sibling_left = if sibling.is_null() {
                        ptr::null_mut()
                    } else {
                        (*(*sibling).state.get()).left
                    };
                    Self::set_color(sibling_left, Color::Black);
                    self.rotate_right(x_parent);
                    x = self.root;
                    x_parent = ptr::null_mut();
                }
            }
        }
        Self::set_color(x, Color::Black);
    }
}

impl<K, T> Drop for EevdfTree<K, T> {
    fn drop(&mut self) {
        // Drop does not need to preserve balancing.  A parent-pointer
        // post-order walk avoids allocating a temporary stack while still
        // clearing each link before releasing the tree's Arc ownership unit.
        let mut current = self.root;
        let mut previous = ptr::null_mut();
        while !current.is_null() {
            // SAFETY: current is a node still owned by this tree.
            let (parent, left, right) = unsafe {
                let link = &*(*current).state.get();
                (link.parent, link.left, link.right)
            };
            let next = if previous == parent {
                if !left.is_null() {
                    left
                } else if !right.is_null() {
                    right
                } else {
                    ptr::null_mut()
                }
            } else if previous == left {
                if !right.is_null() {
                    right
                } else {
                    ptr::null_mut()
                }
            } else {
                ptr::null_mut()
            };
            if !next.is_null() {
                previous = current;
                current = next;
                continue;
            }

            let next_parent = parent;
            // SAFETY: current is no longer traversed as a live node after its
            // Arc is reconstructed and dropped.  Parent child pointers are
            // intentionally left as raw identity markers until the parent is
            // visited, allowing this stackless walk to distinguish its left
            // and right post-order phases without allocating.
            unsafe {
                if parent.is_null() {
                    self.root = ptr::null_mut();
                }
                let link = &mut *(*current).state.get();
                link.parent = ptr::null_mut();
                link.left = ptr::null_mut();
                link.right = ptr::null_mut();
                link.linked = false;
                link.subtree_min = (*(*current).state.get()).eligible_at;
                self.len -= 1;
                drop(Arc::from_raw(current));
            }
            previous = current;
            current = next_parent;
        }
    }
}

/// Short name used by callers that do not need the EEVDF-specific spelling.
pub(crate) type AugmentedRbTree<K, T> = EevdfTree<K, T>;
/// Short name for an intrusive node.
pub(crate) type AugmentedRbNode<K, T> = EevdfNode<K, T>;
/// Short name for insertion/removal errors.
pub(crate) type AugmentedTreeError = EevdfTreeError;

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;
    use core::cmp::Ordering;

    type Node = Arc<EevdfNode<i32, i32>>;

    fn node(key: i32, eligible_at: u128) -> Node {
        Arc::new(EevdfNode::new_unlinked(key, eligible_at, key))
    }

    unsafe fn state(node: &Node) -> &NodeState<i32, i32> {
        &*(*Arc::as_ptr(node)).state.get()
    }

    fn key(node: &Node) -> i32 {
        unsafe { state(node).key }
    }

    fn eligible_at(node: &Node) -> u128 {
        unsafe { state(node).eligible_at }
    }

    fn linked(node: &Node) -> bool {
        unsafe { state(node).linked }
    }

    fn assert_send<T: Send>() {}

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn conditional_smp_traits_are_explicit() {
        assert_send_sync::<EevdfNode<i32, i32>>();
        assert_send::<EevdfTree<i32, i32>>();
    }

    fn verify(tree: &EevdfTree<i32, i32>) {
        if tree.root.is_null() {
            assert_eq!(tree.len, 0);
            return;
        }
        unsafe {
            assert_eq!((*(*tree.root).state.get()).parent, ptr::null_mut());
            assert_eq!((*(*tree.root).state.get()).color, Color::Black);
            let mut seen = Vec::new();
            let (count, black_height, min) =
                verify_node(tree.root, ptr::null_mut(), None, None, &mut seen);
            assert_eq!(count, tree.len);
            assert_eq!(min, (*(*tree.root).state.get()).subtree_min);
            assert!(black_height > 0);
            assert_eq!(seen.len(), tree.len);
            for ptr in seen {
                assert!((*(*ptr).state.get()).linked);
            }
        }
    }

    unsafe fn verify_node(
        node: *mut EevdfNode<i32, i32>,
        parent: *mut EevdfNode<i32, i32>,
        lower: Option<i32>,
        upper: Option<i32>,
        seen: &mut Vec<*mut EevdfNode<i32, i32>>,
    ) -> (usize, usize, u128) {
        if node.is_null() {
            return (0, 1, EMPTY_MIN);
        }
        assert!(!seen.contains(&node), "cycle or duplicate node");
        seen.push(node);
        let link = &*(*node).state.get();
        assert_eq!(link.parent, parent);
        assert!(link.linked);
        if let Some(lower) = lower {
            assert_eq!((*(*node).state.get()).key.cmp(&lower), Ordering::Greater);
        }
        if let Some(upper) = upper {
            assert_eq!((*(*node).state.get()).key.cmp(&upper), Ordering::Less);
        }
        if link.color == Color::Red {
            assert_eq!(EevdfTree::<i32, i32>::color(link.left), Color::Black);
            assert_eq!(EevdfTree::<i32, i32>::color(link.right), Color::Black);
        }
        let (left_count, left_black, left_min) = verify_node(
            link.left,
            node,
            lower,
            Some((*(*node).state.get()).key),
            seen,
        );
        let (right_count, right_black, right_min) = verify_node(
            link.right,
            node,
            Some((*(*node).state.get()).key),
            upper,
            seen,
        );
        assert_eq!(left_black, right_black, "black-height mismatch");
        let expected_min = (*(*node).state.get())
            .eligible_at
            .min(left_min)
            .min(right_min);
        assert_eq!(link.subtree_min, expected_min);
        (
            left_count + right_count + 1,
            left_black + usize::from(link.color == Color::Black),
            expected_min,
        )
    }

    #[test]
    fn rotations_and_exact_arc_ownership() {
        for keys in [
            [3, 2, 1], // LL
            [3, 1, 2], // LR
            [1, 3, 2], // RL
            [1, 2, 3], // RR
        ] {
            let mut tree = EevdfTree::new();
            let nodes: Vec<_> = keys.into_iter().map(|key| node(key, key as u128)).collect();
            for n in &nodes {
                assert_eq!(Arc::strong_count(n), 1);
                tree.insert(n.clone()).unwrap();
                assert_eq!(Arc::strong_count(n), 2);
                verify(&tree);
            }
            let removed = tree.remove(nodes[1].as_ref()).unwrap();
            assert!(Arc::ptr_eq(&removed, &nodes[1]));
            assert_eq!(Arc::strong_count(&removed), 2);
            drop(removed);
            verify(&tree);
        }
    }

    #[test]
    fn front_and_min_eligible_cache_queries_follow_mutations() {
        let mut tree = EevdfTree::new();
        assert!(tree.front().is_none());
        assert_eq!(tree.min_eligible_at(), None);

        let high = node(3, 50);
        let low = node(1, 30);
        let middle = node(2, 10);
        // This insertion order exercises the LR rotation while making the
        // minimum-key result independent of the root's physical position.
        for task in [&high, &low, &middle] {
            tree.insert(task.clone()).unwrap();
        }
        assert_eq!(tree.front().map(|task| *task.value()), Some(1));
        assert_eq!(tree.min_eligible_at(), Some(10));
        verify(&tree);

        let removed = tree.remove(middle.as_ref()).unwrap();
        drop(removed);
        assert_eq!(tree.front().map(|task| *task.value()), Some(1));
        assert_eq!(tree.min_eligible_at(), Some(30));
        verify(&tree);

        let removed = tree.pop_earliest_eligible(100).unwrap();
        assert!(Arc::ptr_eq(&removed, &low));
        drop(removed);
        assert_eq!(tree.front().map(|task| *task.value()), Some(3));
        assert_eq!(tree.min_eligible_at(), Some(50));

        let removed = tree.remove(high.as_ref()).unwrap();
        drop(removed);
        assert!(tree.front().is_none());
        assert_eq!(tree.min_eligible_at(), None);
        assert!(tree.is_empty());
    }

    #[test]
    fn duplicate_and_foreign_operations_are_typed_and_lossless() {
        let mut first = EevdfTree::new();
        let mut second = EevdfTree::new();
        let original = node(7, 0);
        first.insert(original.clone()).unwrap();
        let same = original.clone();
        let err = first.insert(same).unwrap_err();
        assert_eq!(err.kind(), EevdfTreeError::AlreadyLinked);
        let recovered = err.into_node();
        assert!(Arc::ptr_eq(&recovered, &original));

        let duplicate = node(7, 9);
        let err = first.insert(duplicate).unwrap_err();
        assert_eq!(err.kind(), EevdfTreeError::DuplicateKey);
        let duplicate = err.into_node();
        assert!(!linked(&duplicate));
        assert_eq!(unsafe { state(&duplicate).subtree_min }, 9);

        let foreign = node(9, 0);
        second.insert(foreign.clone()).unwrap();
        assert!(matches!(
            first.remove(foreign.as_ref()),
            Err(EevdfTreeError::ForeignNode)
        ));
        let unlinked = node(10, 0);
        assert!(matches!(
            first.remove(unlinked.as_ref()),
            Err(EevdfTreeError::ForeignNode)
        ));
    }

    #[test]
    fn selection_prunes_ineligible_earlier_deadlines() {
        let mut tree = EevdfTree::new();
        let early_ineligible = node(1, 100);
        let later_eligible = node(2, 5);
        let earliest_eligible = node(3, 5);
        for n in [&early_ineligible, &later_eligible, &earliest_eligible] {
            tree.insert(n.clone()).unwrap();
        }
        let selected = tree.pop_earliest_eligible(5).unwrap();
        // Key order wins among eligible nodes; the key-1 deadline is ineligible.
        assert!(Arc::ptr_eq(&selected, &later_eligible));
        verify(&tree);
        assert!(Arc::ptr_eq(
            &tree.pop_earliest_eligible(99).unwrap(),
            &earliest_eligible
        ));
        verify(&tree);
        assert!(Arc::ptr_eq(
            &tree.pop_earliest_eligible(100).unwrap(),
            &early_ineligible
        ));
        verify(&tree);
        assert!(tree.is_empty());
    }

    #[test]
    fn remove_stage_reinsert_and_drop_clear_links() {
        let mut tree = EevdfTree::new();
        let task = node(4, 4);
        let external_clone = task.clone();
        tree.insert(task.clone()).unwrap();
        let removed = tree.remove(task.as_ref()).unwrap();
        assert!(Arc::ptr_eq(&removed, &task));
        assert_eq!(Arc::strong_count(&task), 3);
        // Keep three strong references alive.  A unique Arc is intentionally
        // unavailable; scheduler logical ownership is the unsafe staging
        // contract.
        unsafe { removed.stage_unlinked(1, 99) };
        tree.insert(removed.clone()).unwrap();
        verify(&tree);
        drop(removed);
        let removed = tree.remove(task.as_ref()).unwrap();
        assert!(!linked(&removed));
        drop(removed);
        drop(external_clone);
        assert_eq!(Arc::strong_count(&task), 1);

        let leaked = node(8, 8);
        tree.insert(leaked.clone()).unwrap();
        assert!(linked(&leaked));
        drop(tree);
        assert!(!linked(&leaked));
        assert_eq!(Arc::strong_count(&leaked), 1);
    }

    #[test]
    fn deterministic_random_trace_matches_full_scan_oracle() {
        let mut tree = EevdfTree::new();
        let nodes: Vec<_> = (0..96)
            .map(|key| node(key, (key * 17 % 31) as u128))
            .collect();
        let mut present = vec![false; nodes.len()];
        let mut state = 0x9e37_79b9_u64;
        for _ in 0..4000 {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            let index = ((state >> 16) as usize) % nodes.len();
            match (state >> 8) % 4 {
                0 if !present[index] => {
                    tree.insert(nodes[index].clone()).unwrap();
                    present[index] = true;
                }
                1 if present[index] => {
                    let removed = tree.remove(nodes[index].as_ref()).unwrap();
                    assert!(Arc::ptr_eq(&removed, &nodes[index]));
                    drop(removed);
                    present[index] = false;
                }
                _ => {
                    let now = (state >> 24) % 47;
                    let expected = nodes
                        .iter()
                        .enumerate()
                        .filter(|(i, n)| present[*i] && eligible_at(n) <= now as u128)
                        .min_by_key(|(_, n)| key(n));
                    let got = tree.pop_earliest_eligible(now as u128);
                    match (expected, got) {
                        (None, None) => {}
                        (Some((i, _)), Some(got)) => {
                            assert!(Arc::ptr_eq(&got, &nodes[i]));
                            present[i] = false;
                            drop(got);
                        }
                        (None, Some(_)) => panic!("oracle selected an ineligible node"),
                        (Some(_), None) => panic!("oracle missed an eligible node"),
                    }
                }
            }
            for (index, n) in nodes.iter().enumerate() {
                assert_eq!(
                    Arc::strong_count(n),
                    if present[index] { 2 } else { 1 },
                    "Arc count mismatch for node {index}"
                );
            }
            verify(&tree);
        }
    }

    #[test]
    fn drop_releases_every_arc_and_clears_every_link() {
        let nodes: Vec<_> = (0..128).map(|key| node(key, key as u128)).collect();
        let mut tree = EevdfTree::new();
        for n in &nodes {
            tree.insert(n.clone()).unwrap();
        }
        verify(&tree);
        drop(tree);
        for n in nodes {
            assert!(!linked(&n));
            assert_eq!(Arc::strong_count(&n), 1);
        }
    }
}
