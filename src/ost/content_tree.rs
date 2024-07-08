use std::cell::Cell;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::mem;
use std::mem::replace;
use std::ops::{Index, IndexMut, Range, Sub};
use std::ptr::NonNull;
use content_tree::{NodeLeaf, UnsafeCursor};
use rle::{HasLength, HasRleKey, MergableSpan, RleDRun, SplitableSpan, SplitableSpanHelpers};
use crate::{DTRange, LV};
use crate::ost::{LEAF_CHILDREN, LeafIdx, LenPair, LenUpdate, NODE_CHILDREN, NodeIdx, remove_from_array, remove_from_array_fill};

pub(crate) trait Content: SplitableSpan + MergableSpan + Copy + HasLength + HasRleKey {
    /// The length of the item. If IS_CUR then this is the "current length". Otherwise, this is the
    /// end length of the item.
    fn content_len<const IS_CUR: bool>(&self) -> usize;
    fn content_len_cur(&self) -> usize { self.content_len::<true>() }
    fn content_len_end(&self) -> usize { self.content_len::<false>() }
    fn content_len_pair(&self) -> LenPair {
        LenPair {
            cur: self.content_len_cur(),
            end: self.content_len_end(),
        }
    }

    /// The default item must "not exist".
    fn exists(&self) -> bool;
    // fn current_len(&self) -> usize;

    // split_at_current_len() ?

    // fn underwater() -> Self;

    fn none() -> Self;
}

trait LeafMap {
    fn notify(&mut self, range: DTRange, leaf_idx: LeafIdx);
}

#[derive(Debug, Clone)]
pub(crate) struct ContentTree<V: Content> {
    leaves: Vec<ContentLeaf<V>>,
    nodes: Vec<ContentNode>,

    /// The number of internal nodes between the root and the leaves. This is initialized to 0,
    /// indicating we start with no internal nodes and just a single leaf.
    height: usize,

    /// The root node. If height == 0, this is a leaf (and has value 0). Otherwise, this is an index
    /// into the nodes vec pointing to the node representing the root.
    root: usize,
    // cursor: ContentCursor,

    /// There is a cached cursor currently at some content position.
    cursor: Cell<(usize, ContentCursor)>,
    total_len: LenPair,

    // Linked lists.
    free_leaf_pool_head: LeafIdx,
    free_node_pool_head: NodeIdx,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ContentCursor {
    // The item pointed to by the cursor should still be in the CPU's L1 cache. I could cache some
    // properties of the cursor's leaf item here, but I think it wouldn't improve performance -
    // since we wouldn't be saving any memory loads anyway.
    pub leaf_idx: LeafIdx,
    pub elem_idx: usize,

    /// Offset into the item.
    pub offset: usize,
}

// Wouldn't need this impl if LeafIdx defaulted to 0...
impl Default for ContentCursor {
    fn default() -> Self {
        ContentCursor {
            leaf_idx: LeafIdx(0),
            elem_idx: 0,
            offset: 0,
        }
    }
}

// const EMPTY_LEAF_DATA: (LV, LeafData) = (usize::MAX, LeafData::InsPtr(NonNull::dangling()));

const NODE_SPLIT_POINT: usize = NODE_CHILDREN / 2;
// const LEAF_CHILDREN: usize = LEAF_SIZE - 1;
const LEAF_SPLIT_POINT: usize = LEAF_CHILDREN / 2;

#[derive(Debug, Clone)]
pub struct ContentLeaf<V> {
    /// Each child object knows its own bounds.
    ///
    /// It may turn out to be more efficient to split each field in children into its own sub-array.
    children: [V; LEAF_CHILDREN],

    // /// (start of range, data). Start == usize::MAX for empty entries.
    // children: [(LV, V); LEAF_CHILDREN],

    // upper_bound: LV,
    next_leaf: LeafIdx,
    parent: NodeIdx,
}

#[derive(Debug, Clone)]
pub struct ContentNode {
    /// The index is either an index into the internal nodes or leaf nodes depending on the height.
    ///
    /// Children have an index of usize::MAX if the slot is unused.
    child_indexes: [usize; NODE_CHILDREN],

    /// Child entries point to either another node or a leaf. We disambiguate using the height.
    /// The named LV is the first LV of the child data.
    child_width: [LenPair; NODE_CHILDREN],
    parent: NodeIdx,
}

// fn initial_root_leaf<V: Content>() -> ContentLeaf<V> {
fn initial_root_leaf<V: Content>() -> ContentLeaf<V> {
    // The tree is initialized with an "underwater" item covering the range.
    // let mut children = [V::default(); LEAF_CHILDREN];
    // children[0] = V::underwater();

    ContentLeaf {
        children: [V::none(); LEAF_CHILDREN],
        next_leaf: LeafIdx(usize::MAX),
        parent: NodeIdx(usize::MAX), // This node won't exist yet - but thats ok.
    }
}

// /// A node child specifies the width of the recursive children and an index in the data
// /// structure.
// type ContentNodeChild = (LenPair, usize);
//
// const EMPTY_NODE_CHILD: ContentNodeChild = (LenPair { cur: 0, end: 0 }, usize::MAX);

const EMPTY_LEN_PAIR: LenPair = LenPair { cur: 0, end: 0 };

impl<V: Content> ContentLeaf<V> {
    fn is_full(&self) -> bool {
        self.children.last().unwrap().exists()
    }

    #[inline(always)]
    fn has_space(&self, space_wanted: usize) -> bool {
        if space_wanted == 0 { return true; }
        !self.children[LEAF_CHILDREN - space_wanted].exists()
    }

    fn is_last(&self) -> bool { !self.next_leaf.exists() }

    fn next<'a>(&self, leaves: &'a [ContentLeaf<V>]) -> Option<&'a ContentLeaf<V>> {
        if self.is_last() { None }
        else { Some(&leaves[self.next_leaf.0]) }
    }

    fn next_mut<'a>(&self, leaves: &'a mut [ContentLeaf<V>]) -> Option<&'a mut ContentLeaf<V>> {
        if self.is_last() { None }
        else { Some(&mut leaves[self.next_leaf.0]) }
    }

    fn remove_children(&mut self, del_range: Range<usize>) {
        remove_from_array_fill(&mut self.children, del_range, V::none());
    }
}

impl ContentNode {
    fn is_full(&self) -> bool {
        *self.child_indexes.last().unwrap() != usize::MAX
    }

    fn remove_children(&mut self, del_range: Range<usize>) {
        remove_from_array_fill(&mut self.child_indexes, del_range.clone(), usize::MAX);
        remove_from_array(&mut self.child_width, del_range.clone());
    }

    /// Returns the (local) index of the named child. Aborts if the child is not in this node.
    fn idx_of_child(&self, child: usize) -> usize {
        self.child_indexes
            .iter()
            .position(|i| *i == child)
            .unwrap()
    }
}

impl<V: Content> Default for ContentTree<V> {
    fn default() -> Self {
        Self::new()
    }
}

impl<V: Content> Index<LeafIdx> for ContentTree<V> {
    type Output = ContentLeaf<V>;

    fn index(&self, index: LeafIdx) -> &Self::Output {
        &self.leaves[index.0]
    }
}
impl<V: Content> IndexMut<LeafIdx> for ContentTree<V> {
    fn index_mut(&mut self, index: LeafIdx) -> &mut Self::Output {
        &mut self.leaves[index.0]
    }
}
impl<V: Content> Index<NodeIdx> for ContentTree<V> {
    type Output = ContentNode;

    fn index(&self, index: NodeIdx) -> &Self::Output {
        &self.nodes[index.0]
    }
}
impl<V: Content> IndexMut<NodeIdx> for ContentTree<V> {
    fn index_mut(&mut self, index: NodeIdx) -> &mut Self::Output {
        &mut self.nodes[index.0]
    }
}

#[inline]
fn inc_delta_update<V: Content>(delta_len: &mut LenUpdate, e: &V) {
    delta_len.cur += e.content_len_cur() as isize;
    delta_len.end += e.content_len_end() as isize;
}
#[inline]
fn dec_delta_update<V: Content>(delta_len: &mut LenUpdate, e: &V) {
    delta_len.cur -= e.content_len_cur() as isize;
    delta_len.end -= e.content_len_end() as isize;
}

// fn split_rle<V: Content>(val: RleDRun<V>, offset: usize) -> (RleDRun<V>, RleDRun<V>) {
//     debug_assert!(offset > 0);
//     debug_assert!(offset < (val.end - val.start));
//
//     (RleDRun {
//         start: val.start,
//         end: val.start + offset,
//         val: val.val,
//     }, RleDRun {
//         start: val.start + offset,
//         end: val.end,
//         val: val.val.at_offset(offset),
//     })
// }

impl<V: Content> ContentTree<V> {
    pub fn new() -> Self {
        debug_assert_eq!(V::none().content_len_pair(), LenPair::default());
        // debug_assert_eq!(V::none().len(), 0);
        debug_assert_eq!(V::none().exists(), false);

        Self {
            leaves: vec![initial_root_leaf()],
            nodes: vec![],
            // upper_bound: 0,
            height: 0,
            root: 0,
            cursor: Default::default(),
            total_len: Default::default(),
            free_leaf_pool_head: LeafIdx(usize::MAX),
            free_node_pool_head: NodeIdx(usize::MAX),
        }
    }

    pub fn clear(&mut self) {
        self.leaves.clear();
        self.nodes.clear();
        self.height = 0;
        self.root = 0;
        self.cursor = Default::default();
        self.total_len = Default::default();
        self.free_leaf_pool_head = LeafIdx(usize::MAX);
        self.free_node_pool_head = NodeIdx(usize::MAX);

        self.leaves.push(initial_root_leaf());
    }

    // fn create_new_root_node(&mut self, child_a: usize, child_b: usize, split_point: LenPair) -> NodeIdx {
    fn create_new_root_node(&mut self, child_a: usize, child_b: usize, b_size: LenPair) -> NodeIdx {
        self.height += 1;
        let mut new_root = ContentNode {
            child_indexes: [usize::MAX; NODE_CHILDREN],
            child_width: [Default::default(); NODE_CHILDREN],
            parent: Default::default(),
        };

        new_root.child_indexes[0] = child_a;
        new_root.child_indexes[1] = child_b;
        new_root.child_width[0] = self.total_len - b_size;
        new_root.child_width[1] = b_size;

        let new_idx = self.nodes.len();
        // println!("Setting root to {new_idx}");
        self.root = new_idx;
        self.nodes.push(new_root);
        NodeIdx(new_idx)
    }

    pub fn insert_notify<F>(&mut self, item: V, cursor: &mut ContentCursor, notify: &mut F)
        where F: FnMut(V, LeafIdx)
    {
        self.dbg_check();
        let mut delta_len = LenUpdate::default();
        self.splice_in(item, cursor, &mut delta_len, true, notify);
        self.flush_delta_len(cursor.leaf_idx, delta_len);
        self.dbg_check();
    }

    fn splice_in<F>(&mut self, item: V, cursor: &mut ContentCursor, delta_len: &mut LenUpdate, notify_here: bool, notify: &mut F)
        where F: FnMut(V, LeafIdx)
    {
        debug_assert!(item.exists());
        let mut leaf_idx = cursor.leaf_idx;
        let mut elem_idx = cursor.elem_idx;
        let mut offset = cursor.offset;

        let node = &mut self[leaf_idx];
        debug_assert_ne!(offset, usize::MAX);

        let remainder = if offset == 0 && elem_idx > 0 {
            // Roll the cursor back to opportunistically see if we can append.
            elem_idx -= 1;
            offset = node.children[elem_idx].len(); // blerp could be cleaner.
            None
        } else if offset == node.children[elem_idx].len() || offset == 0 {
            None
        } else {
            // We could also roll back to the previous leaf node if offset == 0 and
            // elem_idx == 0 but when I tried it, it didn't make any difference in practice
            // because insert() is always called with stick_end.

            // Remainder is the trimmed off returned value.
            // splice the item into the current cursor location.
            let entry: &mut V = &mut node.children[elem_idx];
            let remainder = entry.truncate(offset);
            dec_delta_update(delta_len, &remainder);
            // We don't need to update cursor since its already where it needs to be.

            Some(remainder)
        };

        if offset != 0 {
            // We're at the end of an element. Try and append here.
            debug_assert_eq!(offset, node.children[elem_idx].len());
            // Try and append as much as we can after the current entry
            let cur_entry: &mut V = &mut node.children[elem_idx];
            if cur_entry.can_append(&item) {
                inc_delta_update(delta_len, &item);
                // flush_marker += next.content_len() as isize;
                if notify_here { notify(item, leaf_idx) };
                cur_entry.append(item);
                cursor.offset = cur_entry.len();

                if let Some(remainder) = remainder {
                    let (leaf_idx_2, elem_idx_2) = self.splice_in_internal(remainder, None, leaf_idx, elem_idx + 1, delta_len, notify_here, notify);
                    // If the remainder was inserted into a new item, we might need to update the
                    // cursor.
                    if leaf_idx_2 != leaf_idx && elem_idx_2 > 0 {
                        cursor.leaf_idx = leaf_idx_2;
                        cursor.elem_idx = elem_idx_2 - 1;
                    }
                }
                return;
            }

            // Insert in the next slot.

            elem_idx += 1; // NOTE: Cursor might point past the end of the node.

            // Try and prepend to the start of the next item.
            // This optimization improves performance when the user hits backspace. We end up
            // merging all the deleted elements together. This adds complexity in exchange for
            // making the tree simpler. (For real edit sequences (like the automerge-perf data
            // set) this gives about an 8% performance increase on an earlier version of this code)

            // if remainder.is_none()
            //     // This is the same as the two lines below. TODO: Check which the compiler prefers.
            //     // && node.children.get(elem_idx).is_some_and(|v| v.exists())
            //     && elem_idx < node.children.len()
            //     && node.children[elem_idx].exists()
            // {
            //     // It may be worth being more aggressive here. We're currently not trying this trick
            //     // when the cursor is at the end of the current node. That might be worth trying!
            //     let cur_entry = &mut node.children[elem_idx];
            //     if item.can_append(cur_entry) {
            //         inc_delta_update(delta_len, &item);
            //         if notify_here { notify(item, leaf_idx) };
            //         // trailing_offset += item.len();
            //         cur_entry.prepend(item);
            //         cursor.offset = cur_entry.len();
            //         debug_assert!(remainder.is_none());
            //         return;
            //     }
            // }
        }

        cursor.offset = item.len();
        (leaf_idx, elem_idx) = self.splice_in_internal(item, remainder, leaf_idx, elem_idx, delta_len, notify_here, notify);
        cursor.leaf_idx = leaf_idx;
        cursor.elem_idx = elem_idx;
    }

    /// Splice in an item, and optionally remainder afterwards. Returns the (leaf_idx, elem_idx) of
    /// the inserted item.
    fn splice_in_internal<F>(&mut self, item: V, remainder: Option<V>, mut leaf_idx: LeafIdx, mut elem_idx: usize, delta_len: &mut LenUpdate, notify_here: bool, notify: &mut F) -> (LeafIdx, usize)
        where F: FnMut(V, LeafIdx)
    {
        let space_needed = 1 + remainder.is_some() as usize;
        (leaf_idx, elem_idx) = self.make_space_in_leaf_for(space_needed, leaf_idx, elem_idx, delta_len, notify_here, notify);

        notify(item, leaf_idx);
        let leaf = &mut self.leaves[leaf_idx.0];
        inc_delta_update(delta_len, &item);
        leaf.children[elem_idx] = item;

        if let Some(remainder) = remainder {
            inc_delta_update(delta_len, &remainder);
            leaf.children[elem_idx + 1] = remainder;
        }

        (leaf_idx, elem_idx)
    }

    fn flush_delta_len(&mut self, leaf_idx: LeafIdx, delta: LenUpdate) {
        if delta.is_empty() { return; }

        let mut idx = self.leaves[leaf_idx.0].parent;
        let mut child = leaf_idx.0;
        while !idx.is_root() {
            let n = &mut self.nodes[idx.0];
            let pos = n.idx_of_child(child);
            debug_assert!(pos < n.child_width.len());

            n.child_width[pos % n.child_width.len()].update_by(delta);

            child = idx.0;
            idx = n.parent;
        }

        self.total_len.update_by(delta);
    }


    fn make_space_in_leaf_for<F>(&mut self, space_wanted: usize, leaf_idx: LeafIdx, elem_idx: usize, delta_len: &mut LenUpdate, notify_here: bool, notify: &mut F) -> (LeafIdx, usize)
        where F: FnMut(V, LeafIdx)
    {
        assert!(space_wanted == 1 || space_wanted == 2);

        if self.leaves[leaf_idx.0].has_space(space_wanted) {
            let leaf = &mut self.leaves[leaf_idx.0];

            // Could scan to find the actual length of the children, then only memcpy that many. But
            // memcpy is cheap.
            leaf.children.copy_within(elem_idx..LEAF_CHILDREN - space_wanted, elem_idx + space_wanted);
        } else {
            self.flush_delta_len(leaf_idx, *delta_len);
            let new_node = self.split_leaf(leaf_idx, notify_here, notify);

            if elem_idx >= LEAF_SPLIT_POINT {
                // We're inserting into the newly created node.
                *delta_len = LenUpdate::default();

                return (new_node, elem_idx - LEAF_SPLIT_POINT);
            }
        }
        (leaf_idx, elem_idx)
    }

    //
    // /// Insert item at the position pointed to by the cursor. The cursor is modified in-place to
    // /// point after the inserted items.
    // ///
    // /// If the cursor points in the middle of an item, the item is split.
    // ///
    // /// The list of items must have a maximum length of 3, so we can always insert all the new items
    // /// in half of a leaf node. (This is a somewhat artificial constraint, but its fine here.)
    // fn insert_internal<F>(&mut self, item: V, cursor: &mut ContentCursor, delta_len: &mut LenUpdate, update_cursor: bool, mut notify_here: bool, notify: &mut F)
    //     where F: FnMut(V, LeafIdx)
    // {
    //     // cursor.get_node_mut() would be better but it would borrow the cursor.
    //     let mut node = &mut self[cursor.leaf_idx];
    //
    //     debug_assert_ne!(cursor.offset, usize::MAX);
    //
    //     let remainder = if cursor.offset == 0 && cursor.elem_idx > 0 {
    //         // Roll the cursor back to opportunistically see if we can append.
    //         cursor.elem_idx -= 1;
    //         cursor.offset = node.children[cursor.elem_idx].len(); // blerp could be cleaner.
    //         None
    //     } else if cursor.offset == node.children[cursor.elem_idx].len() || cursor.offset == 0 {
    //         None
    //     } else {
    //         // We could also roll back to the previous leaf node if cursor.offset == 0 and
    //         // cursor.elem_idx == 0 but when I tried it, it didn't make any difference in practice
    //         // because insert() is always called with stick_end.
    //
    //         // Remainder is the trimmed off returned value.
    //         // splice the item into the current cursor location.
    //         let entry: &mut V = &mut node.children[cursor.elem_idx];
    //         let remainder = entry.truncate(cursor.offset);
    //         dec_delta_update(delta_len, &remainder);
    //         // We don't need to update cursor since its already where it needs to be.
    //
    //         Some(remainder)
    //     };
    //
    //     // If we prepend to the start of the following leaf node, the cursor will need to be
    //     // adjusted accordingly.
    //     // let mut trailing_offset = 0;
    //
    //     if cursor.offset != 0 {
    //         // We're at the end of an element. Try and append here.
    //         debug_assert_eq!(cursor.offset, node.children[cursor.elem_idx].len());
    //         // Try and append as much as we can after the current entry
    //         let cur_entry: &mut V = &mut node.children[cursor.elem_idx];
    //         if cur_entry.can_append(&item) {
    //             inc_delta_update(delta_len, &item);
    //             // flush_marker += next.content_len() as isize;
    //             if notify_here { notify(item, cursor.leaf_idx) };
    //             cur_entry.append(item);
    //
    //             if update_cursor {
    //                 cursor.offset = cur_entry.len();
    //             }
    //             return;
    //         }
    //
    //         // Roll to the next item and try and prepend.
    //         cursor.offset = 0;
    //         cursor.elem_idx += 1; // NOTE: Cursor might point past the end of the node.
    //
    //         // We'll also try to *prepend* the item to the front of the subsequent element.
    //         if remainder.is_none()
    //             // This is the same as the two lines below. TODO: Check which the compiler prefers.
    //             // && node.children.get(cursor.elem_idx).is_some_and(|v| v.exists())
    //             && cursor.elem_idx < node.children.len()
    //             && node.children[cursor.elem_idx].exists()
    //         {
    //             // This optimization improves performance when the user hits backspace. We end up
    //             // merging all the deleted elements together. This adds complexity in exchange for
    //             // making the tree simpler. For real edit sequences (like the automerge-perf data
    //             // set) this gives about an 8% performance increase.
    //
    //             // It may be worth being more aggressive here. We're currently not trying this trick
    //             // when the cursor is at the end of the current node. That might be worth trying!
    //
    //             let cur_entry = &mut node.children[cursor.elem_idx];
    //             if item.can_append(cur_entry) {
    //                 inc_delta_update(delta_len, &item);
    //                 if notify_here { notify(item, cursor.leaf_idx) };
    //                 // trailing_offset += item.len();
    //                 cur_entry.prepend(item);
    //
    //                 if update_cursor {
    //                     cursor.offset = cur_entry.len();
    //                 }
    //                 return;
    //             }
    //         }
    //     }
    //
    //     debug_assert_eq!(cursor.offset, 0);
    //
    //     // Step 2: Make room in the leaf for the new items.
    //     let space_needed = 1 + remainder.is_some() as usize;
    //     self.make
    //     if !node.has_space(space_needed) {
    //         todo!("split node");
    //     } else {
    //
    //     }
    //
    //
    //     let num_filled = node.len_entries();
    //     debug_assert!(space_needed > 0);
    //     assert!(space_needed <= LE / 2);
    //
    //     let remainder_moved = if num_filled + space_needed > LE {
    //         // We need to split the node. The proper b-tree way to do this is to make sure there's
    //         // always N/2 items in every leaf after a split, but I don't think it'll matter here.
    //         // Instead I'll split at idx, and insert the new items in whichever child has more space
    //         // afterwards.
    //
    //         // We have to flush regardless, because we might have truncated the current element.
    //         node.flush_metric_update(delta_len);
    //
    //         if cursor.elem_idx < LE / 2 {
    //             // Split then elements go in left branch, so the cursor isn't updated.
    //             node.split_at(cursor.elem_idx, 0, notify);
    //             node.num_entries += space_needed as u8;
    //             false
    //         } else {
    //             // This will adjust num_entries based on the padding parameter.
    //             let new_node_ptr = node.split_at(cursor.elem_idx, space_needed, notify);
    //             cursor.node = new_node_ptr;
    //             cursor.elem_idx = 0;
    //             node = &mut *cursor.node.as_ptr();
    //             notify_here = true;
    //             true
    //         }
    //     } else {
    //         // We need to move the existing items. This doesn't effect sizes.
    //         if num_filled > cursor.elem_idx {
    //             node.children[..].copy_within(cursor.elem_idx..num_filled, cursor.elem_idx + space_needed);
    //         }
    //         node.num_entries += space_needed as u8;
    //         false
    //     };
    //
    //     // Step 3: There's space now, so we can just insert.
    //
    //     let remainder_idx = cursor.elem_idx + items.len();
    //
    //     if !items.is_empty() {
    //         for e in items {
    //             I::increment_marker(delta_len, e);
    //             // flush_marker.0 += e.content_len() as isize;
    //             if notify_here { notify(*e, cursor.node) };
    //         }
    //         node.children[cursor.elem_idx..cursor.elem_idx + items.len()].copy_from_slice(items);
    //
    //         // Point the cursor to the end of the last inserted item.
    //         cursor.elem_idx += items.len() - 1;
    //         cursor.offset = items[items.len() - 1].len();
    //
    //         if trailing_offset > 0 {
    //             cursor.move_forward_by_offset(trailing_offset, Some(delta_len));
    //         }
    //     }
    //
    //     // The cursor isn't updated to point after remainder.
    //     if let Some(e) = remainder {
    //         I::increment_marker(delta_len, &e);
    //         if remainder_moved {
    //             notify(e, cursor.node);
    //         }
    //         node.children[remainder_idx] = e;
    //     }
    // }























    /// This method always splits a node in the middle. This isn't always optimal, but its simpler.
    /// TODO: Try splitting at the "correct" point and see if that makes any difference to
    /// performance.
    fn split_node(&mut self, old_idx: NodeIdx, children_are_leaves: bool) -> NodeIdx {
        // Split a full internal node into 2 nodes.
        let new_node_idx = self.nodes.len();
        // println!("split node -> {new_node_idx}");
        let old_node = &mut self.nodes[old_idx.0];
        // The old leaf must be full before we split it.
        debug_assert!(old_node.is_full());

        // let split_size: LenPair = old_node.child_width[LEAF_SPLIT_POINT..].iter().copied().sum();
        let split_size: LenPair = old_node.child_width[..LEAF_SPLIT_POINT].iter().copied().sum();

        // eprintln!("split node {:?} -> {:?} + {:?} (leaves: {children_are_leaves})", old_idx, old_idx, new_node_idx);
        // eprintln!("split start {:?} / {:?}", &old_node.children[..NODE_SPLIT_POINT], &old_node.children[NODE_SPLIT_POINT..]);

        let mut new_node = ContentNode {
            child_indexes: [usize::MAX; NODE_CHILDREN],
            child_width: [LenPair::default(); NODE_CHILDREN],
            parent: NodeIdx(usize::MAX), // Overwritten below.
        };

        new_node.child_indexes[0..NODE_SPLIT_POINT].copy_from_slice(&old_node.child_indexes[NODE_SPLIT_POINT..]);
        new_node.child_width[0..NODE_SPLIT_POINT].copy_from_slice(&old_node.child_width[NODE_SPLIT_POINT..]);
        old_node.child_indexes[NODE_SPLIT_POINT..].fill(usize::MAX);

        if children_are_leaves {
            for idx in &new_node.child_indexes[..NODE_SPLIT_POINT] {
                self.leaves[*idx].parent = NodeIdx(new_node_idx);
            }
        } else {
            for idx in &new_node.child_indexes[..NODE_SPLIT_POINT] {
                self.nodes[*idx].parent = NodeIdx(new_node_idx);
            }
        }

        debug_assert_eq!(new_node_idx, self.nodes.len());
        // let split_point_lv = new_node.children[0].0;
        self.nodes.push(new_node);

        // It would be much nicer to do this above earlier - and in earlier versions I did.
        // The problem is that both create_new_root_node and insert_into_node can insert new items
        // into self.nodes. If that happens, the new node index we're expecting to use is used by
        // another node. Hence, we need to call self.nodes.push() before calling any other function
        // which modifies the node list.
        let old_node = &self.nodes[old_idx.0];
        if old_idx.0 == self.root {
            // We'll make a new root.
            let parent = self.create_new_root_node(old_idx.0, new_node_idx, split_size);
            self.nodes[old_idx.0].parent = parent;
            self.nodes[new_node_idx].parent = parent
        } else {
            let parent = old_node.parent;
            self.nodes[new_node_idx].parent = self.split_child_of_node(parent, new_node_idx, old_idx.0, split_size, false);
        }

        NodeIdx(new_node_idx)
    }

    #[must_use]
    fn split_child_of_node(&mut self, mut node_idx: NodeIdx, child_idx: usize, new_child_idx: usize, stolen_len: LenPair, children_are_leaves: bool) -> NodeIdx {
        let mut node = &mut self[node_idx];

        // Where will the child go? I wonder if the compiler can do anything smart with this...
        let mut child_pos = node.child_indexes
            .iter()
            .position(|idx| { *idx == child_idx })
            .unwrap() % node.child_width.len();

        if node.is_full() {
            let new_node = self.split_node(node_idx, children_are_leaves);

            if child_pos >= NODE_SPLIT_POINT {
                // Actually we're inserting into the new node.
                child_pos -= NODE_SPLIT_POINT;
                node_idx = new_node;
            }
            // Technically this only needs to be reassigned in the if() above, but reassigning it
            // in all cases is necessary for the borrowck.
            node = &mut self[node_idx];
        }

        node.child_width[child_pos] -= stolen_len;

        let insert_pos = (child_pos + 1) % node.child_width.len();

        // dbg!(&node);
        // println!("insert_into_node n={:?} after_child {after_child} pos {insert_pos}, new_child {:?}", node_idx, new_child);


        // Could scan to find the actual length of the children, then only memcpy that many. But
        // memcpy is cheap.
        node.child_indexes.copy_within(insert_pos..NODE_CHILDREN - 1, insert_pos + 1);
        node.child_indexes[insert_pos] = new_child_idx;

        node.child_width.copy_within(insert_pos..NODE_CHILDREN - 1, insert_pos + 1);
        node.child_width[insert_pos] = stolen_len;

        node_idx
    }

    fn split_leaf<F>(&mut self, old_idx: LeafIdx, notify_here: bool, notify: &mut F) -> LeafIdx
        where F: FnMut(V, LeafIdx)
    {
        // This function splits a full leaf node in the middle, into 2 new nodes.
        // The result is two nodes - old_leaf with items 0..N/2 and new_leaf with items N/2..N.

        let old_height = self.height;
        // TODO: This doesn't currently use the pool of leaves that we have so carefully prepared.

        let new_leaf_idx = self.leaves.len(); // Weird instruction order for borrowck.
        let mut old_leaf = &mut self.leaves[old_idx.0];
        // debug_assert!(old_leaf.is_full());
        debug_assert!(!old_leaf.has_space(2));

        if notify_here {
            for v in &old_leaf.children[LEAF_SPLIT_POINT..] {
                // This index isn't actually valid yet, but because we've borrowed self mutably
                // here, the borrow checker will make sure that doesn't matter.
                notify(v.clone(), LeafIdx(new_leaf_idx));
            }
        }

        let new_size: LenPair = old_leaf.children[LEAF_SPLIT_POINT..]
            .iter()
            .map(|v| if v.exists() { v.content_len_pair() } else { LenPair::default() })
            .sum();


        let parent = if old_height == 0 {
            // Insert this leaf into a new root node. This has to be the first node.
            let parent = self.create_new_root_node(old_idx.0, new_leaf_idx, new_size);
            old_leaf = &mut self.leaves[old_idx.0]; // borrowck
            debug_assert_eq!(parent, NodeIdx(0));
            // let parent = NodeIdx(self.nodes.len());
            old_leaf.parent = NodeIdx(0);
            // debug_assert_eq!(old_leaf.parent, NodeIdx(0)); // Ok because its the default.
            // old_leaf.parent = NodeIdx(0); // Could just default nodes to have a parent of 0.

            NodeIdx(0)
        } else {
            let mut parent = old_leaf.parent;
            // The parent may change by calling insert_into_node - since the node we're inserting
            // into may split off.

            parent = self.split_child_of_node(parent, old_idx.0, new_leaf_idx, new_size, true);
            old_leaf = &mut self.leaves[old_idx.0]; // borrowck.
            parent
        };

        // The old leaf must be full before we split it.
        // debug_assert!(old_leaf.data.last().unwrap().is_some());

        let mut new_leaf = ContentLeaf {
            children: [V::none(); LEAF_CHILDREN],
            next_leaf: old_leaf.next_leaf,
            parent,
        };

        // We'll steal the second half of the items in OLD_LEAF.
        // Could use ptr::copy_nonoverlapping but this is safe, and they compile to the same code.
        new_leaf.children[0..LEAF_SPLIT_POINT].copy_from_slice(&old_leaf.children[LEAF_SPLIT_POINT..]);

        // Needed to mark that these items are gone now.
        old_leaf.children[LEAF_SPLIT_POINT..].fill(V::none());

        // old_leaf.upper_bound = split_lv;
        old_leaf.next_leaf = LeafIdx(new_leaf_idx);

        self.leaves.push(new_leaf);
        debug_assert_eq!(self.leaves.len() - 1, new_leaf_idx);

        LeafIdx(new_leaf_idx)
    }

    /// This function blindly assumes the item is definitely in the recursive children.
    ///
    /// Returns (child index, len_remaining).
    fn find_pos_in_node<const IS_CUR: bool>(node: &ContentNode, mut at_pos: usize) -> (usize, usize) {
        for i in 0..NODE_CHILDREN {
            let width = node.child_width[i].get::<IS_CUR>();
            if at_pos <= width { return (node.child_indexes[i], at_pos); }
            at_pos -= width;
        }
        panic!("Position not in node");
    }

    /// Returns (index, offset).
    fn find_pos_in_leaf<const IS_CUR: bool>(leaf: &ContentLeaf<V>, mut at_pos: usize) -> (usize, usize) {
        for i in 0..LEAF_CHILDREN {
            let width = leaf.children[i].content_len::<IS_CUR>();
            if at_pos <= width { return (i, at_pos); }
            at_pos -= width;
        }
        panic!("Position not in leaf");
    }

    // fn check_cursor_at(&self, cursor: ContentCursor, lv: LV, at_end: bool) {
    //     assert!(cfg!(debug_assertions));
    //     let leaf = &self.leaves[cursor.leaf_idx.0];
    //     let lower_bound = leaf.bounds[cursor.elem_idx];
    //
    //     let next = cursor.elem_idx + 1;
    //     let upper_bound = if next < LEAF_CHILDREN && leaf.bounds[next] != usize::MAX {
    //         leaf.bounds[next]
    //     } else {
    //         self.leaf_upper_bound(leaf)
    //     };
    //     assert!(lv >= lower_bound);
    //
    //     if at_end {
    //         assert_eq!(lv, upper_bound);
    //     } else {
    //         assert!(lv < upper_bound, "Cursor is not within expected bound. Expect {lv} / upper_bound {upper_bound}");
    //     }
    // }

    // fn cursor_to_next(&self, cursor: &mut ContentCursor) {
    //     let leaf = &self.leaves[cursor.leaf_idx.0];
    //     let next_idx = cursor.elem_idx + 1;
    //     if next_idx >= LEAF_CHILDREN || leaf.bounds[next_idx] == usize::MAX {
    //         cursor.elem_idx = 0;
    //         cursor.leaf_idx = leaf.next_leaf;
    //     } else {
    //         cursor.elem_idx += 1;
    //     }
    // }

    pub fn cursor_at_start() -> ContentCursor {
        // This is always valid because there is always at least 1 leaf item, and its always
        // the first item in the tree.
        ContentCursor::default()
    }

    fn cursor_at_content_pos<const IS_CUR: bool>(&self, content_pos: usize) -> ContentCursor {
        // TODO: Get cached cursor.

        // Make a cursor by descending from the root.
        let mut idx = self.root;
        let mut pos_remaining = content_pos;

        for _h in 0..self.height {
            let n = &self.nodes[idx];
            (idx, pos_remaining) = Self::find_pos_in_node::<IS_CUR>(n, pos_remaining);
        }

        let (elem_idx, offset) = Self::find_pos_in_leaf::<IS_CUR>(&self.leaves[idx], pos_remaining);
        ContentCursor {
            leaf_idx: LeafIdx(idx),
            elem_idx,
            offset,
        }
    }

    // #[inline]
    // fn get_leaf_and_bound(&mut self, idx: LeafIdx) -> (&mut ContentLeaf<V>, LV) {
    //     Self::get_leaf_and_bound_2(&mut self.leaves, idx)
    // }
    //
    // fn get_leaf_and_bound_2(leaves: &mut Vec<ContentLeaf<V>>, idx: LeafIdx) -> (&mut ContentLeaf<V>, LV) {
    //     let leaf = &leaves[idx.0];
    //     let upper_bound = Self::leaf_upper_bound_2(leaves, leaf);
    //     (&mut leaves[idx.0], upper_bound)
    // }


    fn first_leaf(&self) -> LeafIdx {
        if cfg!(debug_assertions) {
            // dbg!(&self);
            let mut idx = self.root;
            for _ in 0..self.height {
                idx = self.nodes[idx].child_indexes[0];
            }
            debug_assert_eq!(idx, 0);
        }
        LeafIdx(0)
    }

    pub fn is_empty(&self) -> bool {
        let first_leaf = &self.leaves[self.first_leaf().0];
        first_leaf.children[0].is_empty()
    }

    // pub fn count_items(&self) -> usize {
    //     let mut count = 0;
    //     let mut leaf = &self[self.first_leaf()];
    //     loop {
    //         // SIMD should make this fast.
    //         count += leaf.bounds.iter().filter(|b| **b != usize::MAX).count();
    //
    //         // There is always at least one leaf.
    //         if leaf.is_last() { break; }
    //         else {
    //             leaf = &self[leaf.next_leaf];
    //         }
    //     }
    //
    //     count
    // }

    /// Iterate over the contents of the index. Note the index tree may contain extra entries
    /// for items within the range, with a value of V::default.
    pub fn iter(&self) -> ContentTreeIter<V> {
        ContentTreeIter {
            tree: self,
            leaf_idx: self.first_leaf(),
            // leaf: &self.leaves[self.first_leaf()],
            elem_idx: 0,
        }
    }

    pub fn to_vec(&self) -> Vec<V> {
        self.iter().collect::<Vec<_>>()
    }


    fn dbg_check_walk_internal(&self, idx: usize, height: usize, mut expect_next_leaf_idx: LeafIdx, expect_parent: NodeIdx, expect_size: LenPair) -> LeafIdx {
        if height == self.height {
            assert!(idx < self.leaves.len());
            // The item is a leaf node. Check that the previous leaf is correct.
            let leaf = &self.leaves[idx];
            assert_eq!(leaf.parent, expect_parent);
            assert_eq!(idx, expect_next_leaf_idx.0);

            let leaf_size: LenPair = leaf.children.iter()
                .filter(|c| c.exists())
                .map(|c| c.content_len_pair())
                .sum();
            assert_eq!(leaf_size, expect_size);

            leaf.next_leaf
        } else {
            assert!(idx < self.nodes.len());
            let node = &self.nodes[idx];
            assert_eq!(node.parent, expect_parent);

            let mut actual_node_size = LenPair::default();

            for i in 0..node.child_indexes.len() {
                let child_idx = node.child_indexes[i];
                if child_idx == usize::MAX {
                    assert!(i >= 1); // All nodes have at least 1 child.
                    // All subsequent child_indexes must be usize::MAX.
                    assert!(node.child_indexes[i..].iter().all(|i| *i == usize::MAX));
                    break;
                }

                let child_size = node.child_width[i];
                actual_node_size += child_size;

                expect_next_leaf_idx = self.dbg_check_walk_internal(child_idx, height + 1, expect_next_leaf_idx, NodeIdx(idx), child_size);
            }
            assert_eq!(actual_node_size, expect_size);

            expect_next_leaf_idx
        }
    }

    fn dbg_check_walk(&self) {
        let last_next_ptr = self.dbg_check_walk_internal(0, 0, LeafIdx(0), NodeIdx(usize::MAX), self.total_len);
        assert_eq!(last_next_ptr.0, usize::MAX);
    }


    #[allow(unused)]
    pub(crate) fn dbg_check(&self) {
        // Invariants:
        // - Except for the root item, all leaves must have at least 1 data entry.
        // - The next pointers iterate through all items in sequence
        // - There is at least 1 leaf node
        // - The width of all items is correct.

        // This code does 2 traversals of the data structure:
        // 1. We walk the leaves by following next_leaf pointers in each leaf node
        // 2. We recursively walk the tree

        // Walk the tree structure in the nodes.
        self.dbg_check_walk();

        // Walk the leaves in sequence.
        let mut leaves_visited = 0;
        let mut leaf_idx = self.first_leaf();
        loop {
            let leaf = &self[leaf_idx];
            leaves_visited += 1;

            if leaf_idx == self.first_leaf() {
                // First leaf. This can be empty - but only if the whole data structure is empty.
                if !leaf.children[0].exists() {
                    assert!(!leaf.next_leaf.exists());
                    assert_eq!(self.total_len, LenPair::default());
                }
            } else {
                assert!(leaf.children[0].exists(), "Only the first leaf can be empty");
            }

            // The size is checked in dbg_check_walk().

            if leaf.is_last() { break; }
            else {
                let next_leaf = &self[leaf.next_leaf];
                // assert!(next_leaf.bounds[0] > prev);
                // assert_eq!(leaf.upper_bound, next_leaf.bounds[0]);
            }
            leaf_idx = leaf.next_leaf;
        }

        let mut leaf_pool_size = 0;
        let mut i = self.free_leaf_pool_head;
        while i.0 != usize::MAX {
            leaf_pool_size += 1;
            i = self.leaves[i.0].next_leaf;
        }
        assert_eq!(leaves_visited + leaf_pool_size, self.leaves.len());

        if self.height == 0 {
            assert!(self.root < self.leaves.len());
        } else {
            assert!(self.root < self.nodes.len());
        }


        // let (lv, cursor) = self.cursor.get();
        // self.check_cursor_at(cursor, lv, false);
    }

    // #[allow(unused)]
    // pub(crate) fn dbg_check_eq_2(&self, other: impl IntoIterator<Item = RleDRun<V>>) {
    //     self.dbg_check();
    //
    //     let mut tree_iter = self.iter();
    //     // let mut expect_iter = expect.into_iter();
    //
    //     // while let Some(expect_val) = expect_iter.next() {
    //     let mut actual_remainder = None;
    //     for mut expect in other.into_iter() {
    //         loop {
    //             let mut actual = actual_remainder.take().unwrap_or_else(|| {
    //                 tree_iter.next().expect("Tree missing item")
    //             });
    //
    //             // Skip anything before start.
    //             if actual.end <= expect.start {
    //                 continue;
    //             }
    //
    //             // Trim the start of actual_next
    //             if actual.start < expect.start {
    //                 (_, actual) = split_rle(actual, expect.start - actual.start);
    //             } else if expect.start < actual.start {
    //                 panic!("Missing element");
    //             }
    //
    //             assert_eq!(actual.start, expect.start);
    //             let r = DTRange { start: actual.start, end: actual.start + usize::min(actual.len(), expect.len()) };
    //             assert!(expect.val.eq(&actual.val, usize::min(actual.len(), expect.len())),
    //                     "at {:?}: expect {:?} != actual {:?} (len={})", r, &expect.val, &actual.val, usize::min(actual.len(), expect.len()));
    //             // assert_eq!(expect.val, actual.val, "{:?}", &tree_iter);
    //
    //             if actual.end > expect.end {
    //                 // We don't need to split it here because that'll happen on the next iteration anyway.
    //                 actual_remainder = Some(actual);
    //                 // actual_remainder = Some(split_rle(actual, expect.end - actual.start).1);
    //                 break;
    //             } else if actual.end >= expect.end {
    //                 break;
    //             } else {
    //                 // actual.end < expect.end
    //                 // Keep the rest of expect for the next iteration.
    //                 (_, expect) = split_rle(expect, actual.end - expect.start);
    //                 debug_assert_eq!(expect.start, actual.end);
    //                 // And continue with this expected item.
    //             }
    //         }
    //     }
    // }

    // #[allow(unused)]
    // pub(crate) fn dbg_check_eq<'a>(&self, vals: impl IntoIterator<Item = &'a V>) where V: 'a {
    //     self.dbg_check_eq_2(vals.into_iter().copied());
    // }

}

#[derive(Debug)]
pub struct ContentTreeIter<'a, V: Content> {
    tree: &'a ContentTree<V>,
    leaf_idx: LeafIdx,
    // leaf: &'a ContentLeaf<V>,
    elem_idx: usize,
}

impl<'a, V: Content> Iterator for ContentTreeIter<'a, V> {
    // type Item = (DTRange, V);
    type Item = V;

    fn next(&mut self) -> Option<Self::Item> {
        // if self.leaf_idx.0 == usize::MAX {
        // debug_assert!(self.elem_idx < LEAF_CHILDREN);
        if self.leaf_idx.0 >= self.tree.leaves.len() || self.elem_idx >= LEAF_CHILDREN { // Avoid a bounds check.
            return None;
        }

        let leaf = &self.tree[self.leaf_idx];

        let data = leaf.children[self.elem_idx].clone();

        self.elem_idx += 1;
        if self.elem_idx >= LEAF_CHILDREN || leaf.children[self.elem_idx].is_empty() {
            self.leaf_idx = leaf.next_leaf;
            self.elem_idx = 0;
        }

        Some(data)
    }
}

#[cfg(test)]
mod test {
    use std::fmt::Debug;
    use rle::{HasLength, HasRleKey, MergableSpan, SplitableSpan, SplitableSpanHelpers};
    use crate::ost::LeafIdx;
    use super::{Content, ContentTree};

    /// This is a simple span object for testing.
    #[derive(Debug, Clone, Copy, Eq, PartialEq)]
    struct TestRange {
        id: u32,
        len: u32,
        is_activated: bool,
        exists: bool,
    }

    impl Default for TestRange {
        fn default() -> Self {
            Self {
                id: u32::MAX,
                len: u32::MAX,
                is_activated: false,
                exists: false,
            }
        }
    }

    impl HasLength for TestRange {
        fn len(&self) -> usize { self.len as usize }
    }
    impl SplitableSpanHelpers for TestRange {
        fn truncate_h(&mut self, at: usize) -> Self {
            assert!(at > 0 && at < self.len as usize);
            assert!(self.exists);
            let other = Self {
                id: self.id + at as u32,
                len: self.len - at as u32,
                is_activated: self.is_activated,
                exists: self.exists,
            };
            self.len = at as u32;
            other
        }

        fn truncate_keeping_right_h(&mut self, at: usize) -> Self {
            let mut other = *self;
            *self = other.truncate(at);
            other
        }
    }
    impl MergableSpan for TestRange {
        fn can_append(&self, other: &Self) -> bool {
            assert!(self.exists);
            other.id == self.id + self.len && other.is_activated == self.is_activated
        }

        fn append(&mut self, other: Self) {
            assert!(self.can_append(&other));
            self.len += other.len;
        }

        fn prepend(&mut self, other: Self) {
            assert!(other.can_append(self));
            self.len += other.len;
            self.id = other.id;
        }
    }

    impl HasRleKey for TestRange {
        fn rle_key(&self) -> usize {
            self.id as usize
        }
    }

    impl Content for TestRange {
        fn content_len<const IS_CUR: bool>(&self) -> usize {
            if !self.exists { 0 }
            else if IS_CUR {
                if self.is_activated { self.len() } else { 0 }
            } else {
                self.len()
            }
        }

        fn exists(&self) -> bool {
            self.exists
        }

        fn none() -> Self {
            Self::default()
        }
    }

    fn null_notify<V>(_v: V, _idx: LeafIdx) {}
    fn debug_notify<V: Debug>(v: V, idx: LeafIdx) {
        println!("Notify {:?} at {:?}", v, idx);
    }

    #[test]
    fn foo() {
        let mut tree: ContentTree<TestRange> = ContentTree::new();
        let mut cursor = tree.cursor_at_content_pos::<true>(0);

        tree.insert_notify(TestRange {
            id: 123,
            len: 10,
            is_activated: false,
            exists: true,
        }, &mut cursor, &mut debug_notify);
        dbg!(&cursor);

        cursor.offset = 2;
        tree.insert_notify(TestRange {
            id: 321,
            len: 20,
            is_activated: false,
            exists: true,
        }, &mut cursor, &mut debug_notify);
        dbg!(&cursor);

        dbg!(&tree);
    }


//     use std::ops::Range;
//     use std::pin::Pin;
//     use rand::prelude::SmallRng;
//     use rand::{Rng, SeedableRng, thread_rng};
//     use content_tree::{ContentTreeRaw, null_notify, RawPositionMetricsUsize};
//     use crate::list_fuzzer_tools::fuzz_multithreaded;
//     use super::*;
//
//     #[derive(Debug, Copy, Clone, Eq, PartialEq)]
//     enum Foo { A, B, C }
//     use Foo::*;
//
//     #[derive(Debug, Copy, Clone, Eq, PartialEq, Default)]
//     struct X(usize);
//     impl IndexContent for X {
//         fn try_append(&mut self, offset: usize, other: &Self, other_len: usize) -> bool {
//             debug_assert!(offset > 0);
//             debug_assert!(other_len > 0);
//             &self.at_offset(offset) == other
//         }
//
//         fn at_offset(&self, offset: usize) -> Self {
//             X(self.0 + offset)
//         }
//
//         fn eq(&self, other: &Self, _upto_len: usize) -> bool {
//             self.0 == other.0
//         }
//     }
//
//     #[test]
//     fn empty_tree_is_empty() {
//         let tree = ContentTree::<X>::new();
//
//         tree.dbg_check_eq(&[]);
//     }
//
//     #[test]
//     fn overlapping_sets() {
//         let mut tree = ContentTree::new();
//
//         tree.set_range((5..10).into(), X(100));
//         tree.dbg_check_eq(&[RleDRun::new(5..10, X(100))]);
//         // assert_eq!(tree.to_vec(), &[((5..10).into(), Some(A))]);
//         // dbg!(&tree.leaves[0]);
//         tree.set_range((5..11).into(), X(200));
//         tree.dbg_check_eq(&[RleDRun::new(5..11, X(200))]);
//
//         tree.set_range((5..10).into(), X(100));
//         tree.dbg_check_eq(&[
//             RleDRun::new(5..10, X(100)),
//             RleDRun::new(10..11, X(205)),
//         ]);
//
//         tree.set_range((2..50).into(), X(300));
//         // dbg!(&tree.leaves);
//         tree.dbg_check_eq(&[RleDRun::new(2..50, X(300))]);
//
//     }
//
//     #[test]
//     fn split_values() {
//         let mut tree = ContentTree::new();
//         tree.set_range((10..20).into(), X(100));
//         tree.set_range((12..15).into(), X(200));
//         tree.dbg_check_eq(&[
//             RleDRun::new(10..12, X(100)),
//             RleDRun::new(12..15, X(200)),
//             RleDRun::new(15..20, X(105)),
//         ]);
//     }
//
//     #[test]
//     fn set_inserts_1() {
//         let mut tree = ContentTree::new();
//
//         tree.set_range((5..10).into(), X(100));
//         tree.dbg_check_eq(&[RleDRun::new(5..10, X(100))]);
//
//         tree.set_range((5..10).into(), X(200));
//         tree.dbg_check_eq(&[RleDRun::new(5..10, X(200))]);
//
//         // dbg!(&tree);
//         tree.set_range((15..20).into(), X(300));
//         // dbg!(tree.iter().collect::<Vec<_>>());
//         tree.dbg_check_eq(&[
//             RleDRun::new(5..10, X(200)),
//             RleDRun::new(15..20, X(300)),
//         ]);
//
//         // dbg!(&tree);
//         // dbg!(tree.iter().collect::<Vec<_>>());
//     }
//
//     #[test]
//     fn set_inserts_2() {
//         let mut tree = ContentTree::new();
//         tree.set_range((5..10).into(), X(100));
//         tree.set_range((1..5).into(), X(200));
//         // dbg!(&tree);
//         tree.dbg_check_eq(&[
//             RleDRun::new(1..5, X(200)),
//             RleDRun::new(5..10, X(100)),
//         ]);
//         dbg!(&tree.leaves[0]);
//
//         tree.set_range((3..8).into(), X(300));
//         // dbg!(&tree);
//         // dbg!(tree.iter().collect::<Vec<_>>());
//         tree.dbg_check_eq(&[
//             RleDRun::new(1..3, X(200)),
//             RleDRun::new(3..8, X(300)),
//             RleDRun::new(8..10, X(103)),
//         ]);
//     }
//
//     #[test]
//     fn split_leaf() {
//         let mut tree = ContentTree::new();
//         // Using 10, 20, ... so they don't merge.
//         tree.set_range(10.into(), X(100));
//         tree.dbg_check();
//         tree.set_range(20.into(), X(200));
//         tree.set_range(30.into(), X(100));
//         tree.set_range(40.into(), X(200));
//         tree.dbg_check();
//         // dbg!(&tree);
//         tree.set_range(50.into(), X(100));
//         tree.dbg_check();
//
//         // dbg!(&tree);
//         // dbg!(tree.iter().collect::<Vec<_>>());
//
//         tree.dbg_check_eq(&[
//             RleDRun::new(10..11, X(100)),
//             RleDRun::new(20..21, X(200)),
//             RleDRun::new(30..31, X(100)),
//             RleDRun::new(40..41, X(200)),
//             RleDRun::new(50..51, X(100)),
//         ]);
//     }
//
//     #[test]
//     fn clear_range() {
//         // for i in 2..20 {
//         for i in 2..50 {
//             eprintln!("i: {i}");
//             let mut tree = ContentTree::new();
//             for base in 0..i {
//                 tree.set_range((base*3..base*3+2).into(), X(base + 100));
//             }
//             // dbg!(tree.iter().collect::<Vec<_>>());
//
//             let ceil = i*3 - 2;
//             // dbg!(ceil);
//             // dbg!(&tree);
//             tree.dbg_check();
//             tree.set_range((1..ceil).into(), X(99));
//             // dbg!(tree.iter().collect::<Vec<_>>());
//
//             tree.dbg_check_eq(&[
//                 RleDRun::new(0..1, X(100)),
//                 RleDRun::new(1..ceil, X(99)),
//                 RleDRun::new(ceil..ceil+1, X(i - 1 + 100 + 1)),
//             ]);
//         }
//     }
//
//     fn fuzz(seed: u64, verbose: bool) {
//         let mut rng = SmallRng::seed_from_u64(seed);
//         let mut tree = ContentTree::new();
//         // let mut check_tree: Pin<Box<ContentTreeRaw<RleDRun<Option<i32>>, RawPositionMetricsUsize>>> = ContentTreeRaw::new();
//         let mut check_tree: Pin<Box<ContentTreeRaw<DTRange, RawPositionMetricsUsize>>> = ContentTreeRaw::new();
//         const START_JUNK: usize = 1_000_000;
//         check_tree.replace_range_at_offset(0, (START_JUNK..START_JUNK *2).into());
//
//         for _i in 0..1000 {
//             if verbose { println!("i: {}", _i); }
//             // This will generate some overlapping ranges sometimes but not too many.
//             let val = rng.gen_range(0..100) + 100;
//             // let start = rng.gen_range(0..3);
//             let start = rng.gen_range(0..1000);
//             let len = rng.gen_range(0..100) + 1;
//             // let start = rng.gen_range(0..100);
//             // let len = rng.gen_range(0..100) + 1;
//
//             // dbg!(&tree, start, len, val);
//             // if _i == 19 {
//             //     println!("blerp");
//             // }
//
//             // if _i == 14 {
//             //     dbg!(val, start, len);
//             //     dbg!(tree.iter().collect::<Vec<_>>());
//             // }
//             tree.set_range((start..start+len).into(), X(val));
//             // dbg!(&tree);
//             tree.dbg_check();
//
//             // dbg!(check_tree.iter().collect::<Vec<_>>());
//
//             check_tree.replace_range_at_offset(start, (val..val+len).into());
//
//             // if _i == 14 {
//             //     dbg!(tree.iter().collect::<Vec<_>>());
//             //     dbg!(check_tree.iter_with_pos().filter_map(|(pos, r)| {
//             //         if r.start >= START_JUNK { return None; }
//             //         Some(RleDRun::new(pos..pos+r.len(), X(r.start)))
//             //     }).collect::<Vec<_>>());
//             // }
//
//             // check_tree.iter
//             tree.dbg_check_eq_2(check_tree.iter_with_pos().filter_map(|(pos, r)| {
//                 if r.start >= START_JUNK { return None; }
//                 Some(RleDRun::new(pos..pos+r.len(), X(r.start)))
//             }));
//         }
//     }
//
//     #[test]
//     fn fuzz_once() {
//         fuzz(22, true);
//     }
//
//     #[test]
//     #[ignore]
//     fn tree_fuzz_forever() {
//         fuzz_multithreaded(u64::MAX, |seed| {
//             if seed % 100 == 0 {
//                 println!("Iteration {}", seed);
//             }
//             fuzz(seed, false);
//         })
//     }
}




