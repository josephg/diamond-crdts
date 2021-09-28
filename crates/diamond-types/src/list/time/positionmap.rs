use content_tree::{ContentLength, ContentTreeWithIndex, FullIndex, Toggleable};
use rle::SplitableSpan;

use crate::list::time::positionmap::MapTag::*;
use std::pin::Pin;
use crate::list::{DoubleDeleteList, ListCRDT, Order, ROOT_ORDER};
use crate::list::ot::positional::{InsDelTag, PositionalComponent};
use std::ops::Range;
use crate::rangeextra::OrderRange;
use crate::list::time::patchiter::ListPatchItem;
use crate::list::time::history::branch_eq;

/// There's 3 states a component in the position map can be in:
/// - Not inserted (yet),
/// - Inserted
/// - Deleted
///
/// But for efficiency, when the state of an item matches the state in the current document, instead
/// of storing that state we simply store `Upstream`. This represents either an insert or a delete,
/// depending on the current document.
#[derive(Debug, Copy, Clone, Eq, PartialEq)]
pub(super) enum MapTag {
    NotInsertedYet,
    Inserted,
    Upstream,
}

// It would be nicer to just use RleRun but I want to customize
#[derive(Debug, Copy, Clone, Eq, PartialEq, Default)]
pub(super) struct PositionRun {
    pub(super) tag: MapTag,
    pub(super) final_len: usize, // This is the full length that we take up in the final document
    pub(super) content_len: usize, // 0 if we're in the NotInsertedYet state.
}

impl Default for MapTag {
    fn default() -> Self { MapTag::NotInsertedYet }
}

// impl From<InsDelTag> for PositionMapComponent {
//     fn from(c: InsDelTag) -> Self {
//         match c {
//             InsDelTag::Ins => Inserted,
//             InsDelTag::Del => Deleted,
//         }
//     }
// }

impl PositionRun {
    // pub(crate) fn new(val: PositionMapComponent, len: usize) -> Self {
    //     Self { val, content_len: len, final_len: 0 }
    // }
    pub(crate) fn new_void(len: usize) -> Self {
        Self { tag: MapTag::NotInsertedYet, final_len: len, content_len: 0 }
    }

    pub(crate) fn new_ins(len: usize) -> Self {
        Self { tag: MapTag::Inserted, final_len: len, content_len: len }
    }

    pub(crate) fn new_upstream(final_len: usize, content_len: usize) -> Self {
        Self { tag: MapTag::Upstream, final_len, content_len }
    }
}

impl SplitableSpan for PositionRun {
    fn len(&self) -> usize { self.final_len }

    fn truncate(&mut self, at: usize) -> Self {
        assert_ne!(self.tag, MapTag::Upstream);

        let remainder = self.final_len - at;
        self.final_len = at;

        match self.tag {
            NotInsertedYet => {
                Self { tag: self.tag, final_len: remainder, content_len: 0 }
            }
            Inserted => {
                self.content_len = at;
                Self { tag: self.tag, final_len: remainder, content_len: remainder }
            }
            Upstream => unreachable!()
        }
    }

    fn can_append(&self, other: &Self) -> bool {
        self.tag == other.tag
    }

    fn append(&mut self, other: Self) {
        self.final_len += other.final_len;
        self.content_len += other.content_len;
    }
}

impl ContentLength for PositionRun {
    fn content_len(&self) -> usize {
        self.content_len
        // This is the amount of space we take up right now.
        // if self.tag == Inserted { self.final_len } else { 0 }
    }

    fn content_len_at_offset(&self, offset: usize) -> usize {
        match self.tag {
            NotInsertedYet => 0,
            Inserted => offset,
            Upstream => panic!("Cannot service call")
        }
    }
}

type PositionMapInternal = ContentTreeWithIndex<PositionRun, FullIndex>;

/// A PositionMap is a data structure used internally to track a set of positional changes to the
/// document as a result of inserts and deletes.
///
/// This is used for a couple functions:
///
/// - When generating positional patches (eg for saving), each patch names its position with respect
/// to the state of the document when that patch was created. To do this, we walk the document in
/// time order and iteratively update a PositionMap as we visit each change
/// - When loading positional patches from disk or over the network, sometimes we need to interpret
/// positional information based on a particular version. For this, we generate a PositionMap
/// at the requested version (branch) and then use that to translate the incoming patch's position
/// information.
///
/// This data structure *should* also be used to generate and process OT changes, though they work
/// slightly differently in general.
#[derive(Debug, Eq, PartialEq)]
pub(super) struct PositionMap {
    /// Helpers to map from Order -> raw positions -> position at the current point in time
    map: Pin<Box<PositionMapInternal>>,
    // order_to_raw_map: OrderToRawInsertMap<'a>,

    // There's two ways we could handle double deletes:
    // 1. Use a double delete list. Have the map simply store whether or not an item was deleted
    // at all, and if something is deleted multiple times, mark as such in double_deletes.
    // 2. Have map store the number of times each item has been deleted. This would be better if
    // double deletes were common, but they're vanishingly rare in practice.
    double_deletes: DoubleDeleteList,
}

// impl<'a> Drop for OrigPatchesIter<'a> {
//     fn drop(&mut self) {
//         println!("Map entries {} nodes {:?}", self.map.count_entries(), self.map.count_nodes());
//         // dbg!(&self.map);
//     }
// }

impl PositionMap {
    pub(super) fn new_void(list: &ListCRDT) -> Self {
        let mut map = PositionMapInternal::new();

        let total_post_len = list.range_tree.offset_len();
        // let (order_to_raw_map, total_post_len) = OrderToRawInsertMap::new(&list.range_tree);
        // TODO: This is something we should cache somewhere.
        if total_post_len > 0 {
            map.push(PositionRun::new_void(total_post_len));
        }

        Self { map, double_deletes: DoubleDeleteList::new() }
    }

    pub(super) fn new_upstream(list: &ListCRDT) -> Self {
        let mut map = PositionMapInternal::new();

        let total_post_len = list.range_tree.offset_len();
        if total_post_len > 0 {
            let total_content_len = list.range_tree.content_len();
            // let (order_to_raw_map, total_post_len) = OrderToRawInsertMap::new(&list.range_tree);
            // TODO: This is something we should cache somewhere.
            map.push(PositionRun::new_upstream(total_post_len, total_content_len));
        }

        Self {
            map,
            // TODO: Eww gross! Refactor to avoid this allocation.
            double_deletes: list.double_deletes.clone()
        }
    }

    fn new_at_version_from_start(list: &ListCRDT, branch: &[Order]) -> Self {
        let mut result = Self::new_void(list);
        if branch != &[ROOT_ORDER] {
            let changes = list.txns.diff(&[ROOT_ORDER], branch).1;

            for range in changes.iter().rev() {
                let patches = list.patch_iter_in_range(range.clone());
                for patch in patches {
                    result.advance_all_by_range(list, patch);
                }
            }
        }

        result
    }

    fn new_at_version_from_end(list: &ListCRDT, branch: &[Order]) -> Self {
        let mut result = Self::new_upstream(list);

        if !branch_eq(branch, list.frontier.as_slice()) {
            let (changes, nil) = list.txns.diff(&list.frontier, branch);
            debug_assert!(nil.is_empty());

            for range in changes.iter() {
                let patches = list.patch_iter_in_range_rev(range.clone());
                for patch in patches {
                    result.retreat_all_by_range(list, patch);
                }
            }
        }

        result
    }

    pub(crate) fn new_at_version(list: &ListCRDT, branch: &[Order]) -> Self {
        // There's two strategies here: We could start at the start of time and walk forward, or we
        // could start at the current version and walk backward. Walking backward will be much more
        // common in practice, but either approach will generate an identical result.

        let sum: Order = branch.iter().sum();

        let start_work = sum;
        let end_work = list.get_next_order() * branch.len() as u32 - sum;

        if cfg!(debug_assertions) {
            // We should end up with identical results regardless of whether we start from the start
            // or end.
            let a = Self::new_at_version_from_start(list, branch);
            let b = Self::new_at_version_from_end(list, branch);
            assert_eq!(a, b);
            return a;
        }

        if start_work < end_work { Self::new_at_version_from_start(list, branch) }
        else { Self::new_at_version_from_end(list, branch) }
    }

    pub(super) fn order_to_raw(&self, list: &ListCRDT, order: Order) -> (InsDelTag, Range<Order>) {
        let cursor = list.get_cursor_before(order);
        let base = cursor.count_offset_pos() as Order;

        let e = cursor.get_raw_entry();
        let tag = if e.is_activated() { InsDelTag::Ins } else { InsDelTag::Del };
        (tag, base..(base + e.order_len() - cursor.offset as Order))
    }

    pub(super) fn retreat_all_by_range(&mut self, list: &ListCRDT, patch: ListPatchItem) {
        let mut target = patch.target_range();
        while !target.is_empty() {
            let len = self.retreat_first_by_range(list, target.clone(), patch.op_type);
            target.start += len;
            debug_assert!(target.start <= target.end);
        }
    }

    pub(super) fn retreat_first_by_range(&mut self, list: &ListCRDT, target: Range<Order>, op_type: InsDelTag) -> Order {
        // dbg!(&target, self.map.iter().collect::<Vec<_>>());
        // This variant is only actually used in one place - which makes things easier.

        let (final_tag, raw_range) = self.order_to_raw(list, target.start);
        let raw_start = raw_range.start;
        let mut len = Order::min(raw_range.order_len(), target.order_len());

        let mut cursor = self.map.mut_cursor_at_offset_pos(raw_start as usize, false);
        if op_type == InsDelTag::Del {
            let e = cursor.get_raw_entry();
            len = len.min((e.final_len - cursor.offset) as u32);
            debug_assert!(len > 0);

            // Usually there's no double-deletes, but we need to check just in case.
            let allowed_len = self.double_deletes.find_zero_range(target.start, len);
            if allowed_len == 0 { // Unlikely. There's a double delete here.
                let len_dd_here = self.double_deletes.decrement_delete_range(target.start, len);
                debug_assert!(len_dd_here > 0);

                // What a minefield. O_o
                return len_dd_here;
            } else {
                len = allowed_len;
            }
        }

        debug_assert!(len >= 1);
        // So the challenge here is we need to un-merge upstream position runs into their
        // constituent parts. We can't use replace_range for this because that calls truncate().
        // let mut len_remaining = len;
        // while len_remaining > 0 {
        //
        // }
        if op_type == InsDelTag::Ins && final_tag == InsDelTag::Del {
            // The easy case. The entry in PositionRun will be Inserted.
            debug_assert_eq!(cursor.get_raw_entry().tag, Inserted);
            cursor.replace_range(PositionRun::new_void(len as _));
        } else {
            // We have merged everything into Upstream. We need to pull it apart, which is bleh.
            debug_assert_eq!(cursor.get_raw_entry().tag, Upstream);
            debug_assert_eq!(op_type, final_tag); // Ins/Ins or Del/Del.
            // TODO: Is this a safe assumption? Let the fuzzer verify it.
            assert!(cursor.get_raw_entry().len() - cursor.offset >= len as usize);

            let (new_entry, eat_content) = match op_type {
                InsDelTag::Ins => (PositionRun::new_void(len as _), len as usize),
                InsDelTag::Del => (PositionRun::new_ins(len as _), 0),
            };

            let current_entry = cursor.get_raw_entry();

            // So we want to replace the cursor entry with [start, X, end]. The trick is figuring
            // out where we split the content in the current entry.
            if cursor.offset == 0 {
                // dbg!(&new_entry, current_entry);
                // Cursor is at the start of this entry. This variant is easier.
                let remainder = PositionRun::new_upstream(
                    current_entry.final_len - new_entry.final_len,
                    current_entry.content_len - eat_content
                );
                // dbg!(remainder);
                if remainder.final_len > 0 {
                    cursor.replace_entry(&[new_entry, remainder]);
                } else {
                    cursor.replace_entry(&[new_entry]);
                }
            } else {
                // TODO: Accidentally this whole thing. Clean me up buttercup!

                // The cursor isn't at the start. We need to figure out how much to slice off.
                // Basically, we need to know how much content is in cursor.offset.

                // TODO(opt): A cursor comparator function would make this much more performant.
                let entry_start_offset = raw_start as usize - cursor.offset;
                let start_cursor = list.range_tree.cursor_at_offset_pos(entry_start_offset, true);
                let start_content = start_cursor.count_content_pos();

                // TODO: Reuse the cursor from order_to_raw().
                let midpoint_cursor = list.range_tree.cursor_at_offset_pos(raw_start as _, true);
                let midpoint_content = midpoint_cursor.count_content_pos();

                let content_chomp = midpoint_content - start_content;

                let start = PositionRun::new_upstream(cursor.offset, content_chomp);

                let remainder = PositionRun::new_upstream(
                    current_entry.final_len - new_entry.final_len - cursor.offset,
                    current_entry.content_len - eat_content - content_chomp
                );

                if remainder.final_len > 0 {
                    cursor.replace_entry(&[start, new_entry, remainder]);
                } else {
                    cursor.replace_entry(&[start, new_entry]);
                }
            }
        }
        len
    }

    #[inline]
    pub(super) fn advance_all_by_range(&mut self, list: &ListCRDT, patch: ListPatchItem) {
        let mut target = patch.target_range();
        while !target.is_empty() {
            let len = self.advance_first_by_range(list, target.clone(), patch.op_type, true).1;
            target.start += len;
            debug_assert!(target.start <= target.end);
        }
    }

    #[inline(always)]
    pub(super) fn advance_first_by_range(&mut self, list: &ListCRDT, target: Range<Order>, op_type: InsDelTag, handle_dd: bool) -> (Option<PositionalComponent>, Order) {
        let (final_tag, raw_range) = self.order_to_raw(list, target.start);
        let raw_start = raw_range.start;
        let mut len = Order::min(raw_range.order_len(), target.order_len());

        let mut cursor = self.map.mut_cursor_at_offset_pos(raw_start as usize, false);

        if op_type == InsDelTag::Del {
            // So the item will usually be in the Inserted state. If its in the Deleted
            // state, we need to mark it as double-deleted.
            let e = cursor.get_raw_entry();

            if handle_dd {
                // Handling double-deletes is only an issue while consuming. Never advancing.
                len = len.min((e.final_len - cursor.offset) as u32);
                debug_assert!(len > 0);
                if e.tag == Upstream { // This can never happen while consuming. Only while advancing.
                    // self.double_deletes.increment_delete_range(raw_start, len);
                    self.double_deletes.increment_delete_range(target.start, len);
                    return (None, len);
                }
            } else {
                // When the insert was created, the content must exist in the document.
                // TODO: Actually verify this assumption when integrating remote txns.
                debug_assert_eq!(e.tag, Inserted);
            }
        }

        let content_pos = cursor.count_content_pos() as u32;
        // Life could be so simple...
        // cursor.replace_range(PositionRun::new(op_type.into(), len as _));

        // So there's kinda 3 different states
        if final_tag == op_type {
            // Transition into the Upstream state
            let content_len: usize = if op_type == InsDelTag::Del { 0 } else { len as usize };
            cursor.replace_range(PositionRun::new_upstream(len as _, content_len));
            // Calling compress_node (in just this branch) improves performance by about 1%.
            cursor.inner.compress_node();
        } else {
            debug_assert_eq!(op_type, InsDelTag::Ins);
            debug_assert_eq!(final_tag, InsDelTag::Del);
            cursor.replace_range(PositionRun::new_ins(len as _));
        }

        // println!("{} {} {}", self.map.count_entries(), self.map.count_nodes().1, self.map.iter().count());
        // dbg!(("after advance", self.map.iter().collect::<Vec<_>>()));

        (Some(PositionalComponent {
            pos: content_pos,
            len,
            content_known: false, // TODO: Add support for content
            tag: op_type.into(),
        }), len)
    }

    pub(crate) fn check(&self) {
        self.map.check();
    }

    pub(crate) fn check_void(&self) {
        self.map.check();
        for item in self.map.raw_iter() {
            assert_eq!(item.tag, MapTag::NotInsertedYet);
        }
        for d in self.double_deletes.iter() {
            assert_eq!(d.1.excess_deletes, 0);
        }
    }

    pub(crate) fn check_upstream(&self, list: &ListCRDT) {
        // dbg!(&self.map);
        self.map.check();
        for item in self.map.raw_iter() {
            assert_eq!(item.tag, MapTag::Upstream);
        }

        // dbg!(self.double_deletes.iter_raw().collect::<Vec<_>>());
        // dbg!(list.double_deletes.iter_raw().collect::<Vec<_>>());
        assert!(self.double_deletes.iter_merged().eq(list.double_deletes.iter_merged()));
    }
}



// #[derive(Debug)]
// pub(crate) struct OrderToRawInsertMap<'a>(Vec<(&'a RangeTreeLeaf, u32)>);
//
// impl<'a> OrderToRawInsertMap<'a> {
//     fn ord_refs(a: &RangeTreeLeaf, b: &RangeTreeLeaf) -> Ordering {
//         let a_ptr = a as *const _;
//         let b_ptr = b as *const _;
//
//         if a_ptr == b_ptr { Ordering::Equal }
//         else if a_ptr < b_ptr { Ordering::Less }
//         else { Ordering::Greater }
//     }
//
//     fn new(range_tree: &'a RangeTree) -> (Self, u32) {
//         let mut nodes = Vec::new();
//         let mut insert_position = 0;
//
//         for node in range_tree.node_iter() {
//             nodes.push((node, insert_position));
//             let len_here: u32 = node.as_slice().iter().map(|e| e.order_len()).sum();
//             insert_position += len_here;
//         }
//
//         nodes.sort_unstable_by(|a, b| {
//             Self::ord_refs(a.0, b.0)
//         });
//
//         // dbg!(nodes.iter().map(|n| n.0 as *const _).collect::<Vec<_>>());
//
//         (Self(nodes), insert_position)
//     }
//
//     /// Returns the raw insert position (as if no deletes ever happened) of the requested item. The
//     /// returned range always starts with the requested order and the end is the maximum range.
//     fn order_to_raw(&self, doc: &ListCRDT, ins_order: Order) -> (InsDelTag, Range<Order>) {
//         let marker = doc.marker_at(ins_order);
//
//         let leaf = unsafe { marker.as_ref() };
//         if cfg!(debug_assertions) {
//             // The requested item must be in the returned leaf.
//             leaf.find(ins_order).unwrap();
//         }
//
//         // TODO: Check if this is actually more efficient compared to a linear scan.
//         let idx = self.0.binary_search_by(|elem| {
//             Self::ord_refs(elem.0, leaf)
//         }).unwrap();
//
//         let mut start_position = self.0[idx].1;
//         for e in leaf.as_slice() {
//             if let Some(offset) = e.contains(ins_order) {
//                 let tag = if e.is_activated() { InsDelTag::Ins } else { InsDelTag::Del };
//                 return (tag, (start_position + offset as u32)..(start_position + e.order_len()));
//             } else {
//                 start_position += e.order_len();
//             }
//         }
//
//         unreachable!("Marker tree is invalid");
//     }
//
//     // /// Same as raw_insert_order, but constrain the return value based on the length
//     // fn raw_insert_order_limited(&self, doc: &ListCRDT, order: Order, max_len: Order) -> Range<Order> {
//     //     let mut result = self.raw_insert_order(list, order);
//     //     result.end = result.end.min(result.start + max_len);
//     //     result
//     // }
// }



#[cfg(test)]
mod test {
    use rle::test_splitable_methods_valid;
    use super::*;
    use crate::test_helpers::*;

    #[test]
    fn positionrun_is_splitablespan() {
        test_splitable_methods_valid(PositionRun::new_void(5));
        test_splitable_methods_valid(PositionRun::new_ins(5));
    }

    fn check_doc(list: &ListCRDT) {
        // We should be able to go forward from void to upstream.
        let mut map = PositionMap::new_void(list);
        for patch in list.patch_iter() {
            // dbg!(&patch);
            map.advance_all_by_range(list, patch);
        }
        // dbg!(&map);
        map.check_upstream(list);

        // And go back from upstream to void, by iterating backwards through all changes.
        let mut map = PositionMap::new_upstream(list);
        for patch in list.patch_iter_rev() {
            map.retreat_all_by_range(list, patch);
        }
        map.check_void();
    }

    #[test]
    fn foo() {
        let mut doc = ListCRDT::new();
        doc.get_or_create_agent_id("seph");
        doc.local_insert(0, 0, "hi there");
        doc.local_delete(0, 2, 3);

        let map = PositionMap::new_at_version(&doc, &[5]);
        dbg!(&map);
    }

    #[test]
    fn fuzz_walk_single_docs() {
        let iter = RandomSingleDocIter::new(2, 10).take(1000);
        for doc in iter {
            check_doc(&doc);
        }
    }

    #[test]
    fn fuzz_walk_multi_docs() {
        for i in 0..30 {
            let docs = gen_complex_docs(i, 20);
            check_doc(&docs[0]); // I could do this every iteration of each_complex, but its slow.
        }
    }

    #[test]
    #[ignore]
    fn fuzz_walk_multi_docs_forever() {
        for i in 0.. {
            if i % 1000 == 0 { println!("{}", i); }
            // println!("{}", i);
            let docs = gen_complex_docs(i, 20);
            check_doc(&docs[0]); // I could do this every iteration of each_complex, but its slow.
        }
    }
}