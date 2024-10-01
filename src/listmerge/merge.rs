// Clippy complains about .as_mut_ref() below. But that construction is needed for the borrow
// checker.
#![allow(clippy::needless_option_as_deref)]

use std::cmp::Ordering;

use jumprope::JumpRopeBuf;
use smartstring::alias::String as SmartString;

use rle::{AppendRle, HasLength, MergableSpan, MergeableIterator, Searchable, SplitableSpanCtx, Trim, TrimCtx};
use rle::intersect::rle_intersect_rev;

use crate::{AgentId, CausalGraph, Frontier, LV};
use crate::causalgraph::agent_assignment::AgentAssignment;
use crate::causalgraph::graph::Graph;
use crate::dtrange::{DTRange, UNDERWATER_START};
use crate::list::buffered_iter::BufferedIter;
use crate::list::encoding::txn_trace::SpanningTreeWalker;
use crate::list::ListOpLog;
use crate::list::op_iter::OpMetricsIter;
use crate::list::op_metrics::{ListOperationCtx, ListOpMetrics};
use crate::list::operation::{ListOpKind, TextOperation};
use crate::listmerge::{Index, M2Tracker};
#[cfg(feature = "dot_export")]
use crate::listmerge::dot::DotColor::*;
use crate::listmerge::markers::{DelRange, Marker};
use crate::listmerge::merge::TransformedResult::{BaseMoved, DeleteAlreadyHappened};
use crate::listmerge::plan::{M1Plan, M1PlanAction};
use crate::listmerge::yjsspan::{CRDTSpan, INSERTED, NOT_INSERTED_YET};
use crate::ost::{IndexTree, LeafIdx, LenPair, LenUpdate};
use crate::ost::content_tree::{Content, ContentCursor, ContentTree, DeltaCursor};
use crate::rev_range::RangeRev;
use crate::rle::{KVPair, RleSpanHelpers, RleVec};
use crate::textinfo::TextInfo;
use crate::unicount::consume_chars;

// const ALLOW_FF: bool = true;

#[cfg(feature = "dot_export")]
const MAKE_GRAPHS: bool = false;

pub(super) fn notify_for<'a>(index: &'a mut Index) -> impl FnMut(CRDTSpan, LeafIdx) + 'a {
    move |entry: CRDTSpan, leaf| {
        debug_assert!(leaf.exists());

        // Note we can only mutate_entries when we have something to mutate. The list is started
        // with a big placeholder "underwater" entry which will be split up as needed.

        // println!("SET RANGE {:?} -> {:?}", entry.id, InsPtr(leaf));
        index.set_range(entry.id, Marker::InsPtr(leaf));
        // index.dbg_check();
    }
}


#[allow(unused)]
fn take_content<'a>(x: Option<&mut &'a str>, len: usize) -> Option<&'a str> {
    if let Some(s) = x {
        Some(consume_chars(s, len))
    } else { None }
}

impl M2Tracker {
    pub(super) fn new() -> Self {
        let mut result = Self {
            range_tree: ContentTree::new(),
            index: IndexTree::new(),

            #[cfg(feature = "merge_conflict_checks")]
            concurrent_inserts_collide: false,
        };

        // The list is initially populated with a dummy "underwater" item, which corresponds to
        // any characters that might exist in the document before the greatest common version.
        let underwater = CRDTSpan::new_underwater();
        result.range_tree.set_single_item_notify(underwater, notify_for(&mut result.index));

        result
    }

    pub(super) fn clear(&mut self) {
        let underwater = CRDTSpan::new_underwater();

        self.index.clear();
        self.range_tree.clear();
        self.range_tree.set_single_item_notify(underwater, notify_for(&mut self.index));
    }

    pub(super) fn marker_at(&self, lv: LV) -> LeafIdx {
        let marker = self.index.get_entry(lv).val;
        let Marker::InsPtr(leaf) = marker else { panic!("No marker at lv") };
        leaf
    }

    #[allow(unused)]
    pub(super) fn check_index(&self) {
        self.index.dbg_check();
        self.range_tree.dbg_check();

        // Go through each entry in the range tree and make sure we can find it using the index.
        for (leaf_idx, children) in self.range_tree.iter_leaves() {
            for e in children.iter() {
                if !e.exists() { break; }

                let marker = self.marker_at(e.id.start);
                debug_assert!(marker.exists());
                assert_eq!(marker, leaf_idx);
            }
        }
    }

    fn get_cursor_before(&self, lv: LV) -> ContentCursor {
        if lv == usize::MAX {
            // This never happens due to dummy data at the end of the list - which means we always
            // insert before some actual value. If we did, the right thing to do in this case would
            // be to return a cursor at the end of the range tree, but I don't have code for that
            // in content_tree, and it seems silly to add it for a case that never comes up in
            // practice.
            // self.range_tree.cursor_at_end()
            unreachable!();
        } else {
            let leaf_idx = self.marker_at(lv);
            self.range_tree.cursor_before_item(lv, leaf_idx)
        }
    }

    fn get_cursor_after(&self, lv: LV, stick_end: bool) -> ContentCursor {
        if lv == usize::MAX {
            self.range_tree.cursor_at_start_nothing_emplaced()
        } else {
            let leaf_idx = self.marker_at(lv);
            // let marker: NonNull<NodeLeaf<YjsSpan, ContentIndex>> = self.markers.at(order as usize).unwrap();
            // self.content_tree.
            let mut cursor = self.range_tree.cursor_before_item(lv, leaf_idx);
            // The cursor points to parent. This is safe because of guarantees provided by
            // cursor_before_item.
            cursor.inc_offset(&self.range_tree);
            if !stick_end { cursor.roll_next_item(&self.range_tree); }
            cursor
        }
    }

    pub(super) fn integrate(&mut self, aa: &AgentAssignment, agent: AgentId, item: CRDTSpan, mut dc: DeltaCursor, mut cursor_pos: LenPair) -> usize {
        debug_assert_eq!(dc.0.get_pos(&self.range_tree), cursor_pos);
        // let DeltaCursor(mut cursor, mut delta) = dc;
        debug_assert!(item.len() > 0);

        // Ok now that's out of the way, lets integrate!

        // These are almost never used. Could avoid the clone here... though its pretty cheap.
        let left_cursor = dc.0.clone();
        let mut scan_cursor = dc.0.clone();
        let mut scan_pos = cursor_pos;
        let mut scanning = false;

        loop {
            if dc.0.offset > 0 // If cursor > 0, the item we're on now is INSERTED.
                || !dc.roll_next_item(&mut self.range_tree) { // End of the document
                break;
            }

            // let other_entry: CRDTSpan = *cursor.get_raw_entry();
            
            // We don't care about the offset because its 0.
            let other_entry = *dc.0.get_item(&self.range_tree).0;

            // When concurrent edits happen, the range of insert locations goes from the insert
            // position itself (passed in through cursor) to the next item which existed at the
            // time in which the insert occurred.
            let other_lv = other_entry.id.start;
            // This test is almost always true. (Ie, we basically always break here).
            if other_lv == item.origin_right { break; }

            // We're now in the rare case there's actually concurrent inserts. To make the logic
            // simpler, at this point we'll zero out the delta.
            dc.flush_delta_and_clear(&mut self.range_tree);
            // After this point, dc.1 is always zero.

            debug_assert_eq!(dc.1, LenUpdate::default());

            debug_assert_eq!(other_entry.current_state, NOT_INSERTED_YET);
            // if other_entry.state != NOT_INSERTED_YET { break; }

            // When preparing example data, its important that the data can merge the same
            // regardless of editing trace (so the output isn't dependent on the algorithm used to
            // merge).
            #[cfg(feature = "merge_conflict_checks")] {
                //println!("Concurrent changes {:?} vs {:?}", item.id, other_entry.id);
                self.concurrent_inserts_collide = true;
            }

            // This code could be better optimized, but its already O(n * log n), and its extremely
            // rare that you actually get concurrent inserts at the same location in the document
            // anyway.

            let other_left_lv = other_entry.origin_left_at_offset(dc.0.offset);
            let other_left_cursor = self.get_cursor_after(other_left_lv, false);

            // YjsMod / Fugue semantics. (The code here is the same for both CRDTs).
            match other_left_cursor.cmp(&left_cursor, &self.range_tree) {
                Ordering::Less => { break; } // Top row
                Ordering::Greater => {} // Bottom row. Continue.
                Ordering::Equal => {
                    if item.origin_right == other_entry.origin_right {
                        // Origin_right matches. Items are concurrent. Order by agent names.
                        let my_name = aa.get_agent_name(agent);

                        let (other_agent, other_seq) = aa.local_to_agent_version(other_lv);
                        let other_name = aa.get_agent_name(other_agent);
                        // eprintln!("concurrent insert at the same place {} ({}) vs {} ({})", item.id.start, my_name, other_lv, other_name);

                        // It's possible for a user to conflict with themselves if they commit to
                        // multiple branches. In this case, sort by seq number.
                        let ins_here = match my_name.cmp(&other_name) {
                            Ordering::Less => true,
                            Ordering::Equal => {
                                // We can't compare versions here because sequence numbers could be
                                // used out of order, and the relative version ordering isn't
                                // consistent in that case.
                                //
                                // We could cache this but this code doesn't run often anyway.
                                let item_seq = aa.local_to_agent_version(item.id.start).1;
                                item_seq < other_seq
                            }
                            Ordering::Greater => false,
                        };

                        // Insert here.
                        if ins_here { break; }
                        else { scanning = false; }
                    } else {
                        // Set scanning based on how the origin_right entries are ordered.
                        let my_right_cursor = self.get_cursor_before(item.origin_right);
                        let other_right_cursor = self.get_cursor_before(other_entry.origin_right);

                        if other_right_cursor.cmp(&my_right_cursor, &self.range_tree) == Ordering::Less {
                            if !scanning {
                                scanning = true;
                                scan_cursor = dc.0.clone();
                                scan_pos = cursor_pos;
                            }
                        } else {
                            scanning = false;
                        }
                    }
                }
            }

            // This looks wrong. The entry in the range tree is a run with:
            // - Incrementing orders (maybe from different peers)
            // - With incrementing origin_left.
            // Q: Is it possible that we get different behaviour if we don't separate out each
            // internal run within the entry and visit each one separately?
            //
            // The fuzzer says no, we don't need to do that. I assume it's because internal entries
            // have higher origin_left, and thus they can't be peers with the newly inserted item
            // (which has a lower origin_left).
            let (item, offset) = dc.0.get_item(&self.range_tree);
            cursor_pos.cur += if item.takes_up_space::<true>() { item.len() - offset } else { 0 };
            cursor_pos.end += if item.takes_up_space::<false>() { item.len() - offset } else { 0 };

            // Just using the cursor version of next_entry since delta is 0.
            if !dc.0.next_entry(&self.range_tree).0 {
                // This is dirty. If the cursor can't move to the next entry, we still need to move
                // it to the end of the current element or we'll prepend. next_entry() doesn't do
                // that for some reason.
                //
                // Also, this case never actually comes up due to dummy data at the end of the range
                // tree. It would be equally valid to put an unreachable!(); here and just abort.
                dc.0.offset = other_entry.len();
                break;
            }

            debug_assert_eq!(dc.0.get_pos(&self.range_tree), cursor_pos);
        }

        if scanning {
            debug_assert_eq!(dc.1, LenUpdate::default());
            debug_assert_eq!(scan_cursor.get_pos(&self.range_tree), scan_pos);
            dc.0 = scan_cursor;
            cursor_pos = scan_pos;
        }

        // if cfg!(debug_assertions) {
        //     let pos = unsafe { cursor.unsafe_count_content_pos() };
        //     let len = self.old_range_tree.content_len();
        //     assert!(pos <= len);
        // }

        // Now insert here.
        let end_pos = cursor_pos.end;
        // The insert operation moves the cursor to after the inserted item.
        cursor_pos += item.content_len_pair();

        // println!("insert entry {:?}", item.id);
        self.range_tree.insert(item, &mut dc, true, &mut notify_for(&mut self.index));

        self.range_tree.emplace_cursor(cursor_pos, dc);

        // We return the resulting end pos of the insert, *before* its been inserted.
        end_pos
    }

    fn apply_range(&mut self, aa: &AgentAssignment, op_ctx: &ListOperationCtx, ops: &RleVec<KVPair<ListOpMetrics>>, range: DTRange, mut to: Option<&mut JumpRopeBuf>) {
        if range.is_empty() { return; }

        let mut iter = OpMetricsIter::new(ops, op_ctx, range);
        // let mut iter = OpMetricsIter::new(&text_info.ops, &text_info.ctx, range);
        while let Some(mut pair) = iter.next() {
            loop {
                let span = aa.local_span_to_agent_span(pair.span());

                let len = span.len();
                let remainder = pair.trim_ctx(len, iter.ctx);

                let content = iter.get_content(&pair);

                self.apply_to(aa, op_ctx, span.agent, &pair, content, to.as_deref_mut());

                if let Some(r) = remainder {
                    pair = r;
                } else { break; }
            }
        }
    }

    fn apply_to(&mut self, aa: &AgentAssignment, ctx: &ListOperationCtx, agent: AgentId, op_pair: &KVPair<ListOpMetrics>, content: Option<&str>, mut to: Option<&mut JumpRopeBuf>) {
        let mut op_pair = op_pair.clone();

        loop {
            let (len_here, transformed_pos) = self.apply(aa, ctx, &op_pair, usize::MAX, agent);

            let remainder = op_pair.trim_ctx(len_here, ctx);

            // dbg!((&op_pair, len_here, transformed_pos));
            if let BaseMoved(pos) = transformed_pos {
                if let Some(to) = to.as_mut() {
                    // Apply the operation here.
                    match op_pair.1.kind {
                        ListOpKind::Ins => {
                            // dbg!(&self.range_tree);
                            // println!("Insert '{}' at {} (len {})", op.content, ins_pos, op.len());
                            debug_assert!(op_pair.1.content_pos.is_some()); // Ok if this is false - we'll just fill with junk.
                            let content = content.unwrap();
                            assert!(pos <= to.len_chars());
                            to.insert(pos, content);
                        }
                        ListOpKind::Del => {
                            // Actually delete the item locally.
                            let del_end = pos + len_here;
                            debug_assert!(to.len_chars() >= del_end);
                            // println!("Delete {}..{} (len {}) '{}'", del_start, del_end, mut_len, to.content.slice_chars(del_start..del_end).collect::<String>());
                            to.remove(pos..del_end);
                        }
                    }
                }
            }

            if let Some(r) = remainder {
                op_pair = r;
                // Curiously, we don't need to update content because we only use content for
                // inserts, and inserts are always processed in one go. (Ie, there's never a
                // remainder to worry about).
                debug_assert_ne!(op_pair.1.kind, ListOpKind::Ins);
            } else { break; }
        }
    }

    /// This is for advancing us directly based on the edit.
    ///
    /// This method does 2 things:
    ///
    /// 1. Advance the tracker (self) based on the passed operation. This will insert new items in
    ///    to the tracker object, and should only be done exactly once for each operation in the set
    ///    we care about
    /// 2. Figure out where the operation will land in the resulting document (if anywhere).
    ///    The resulting operation could happen never (if it's a double delete), once (inserts)
    ///    or generate many individual edits (eg if a delete is split). This method should be called
    ///    in a loop.
    ///
    /// Returns (size here, transformed insert / delete position).
    ///
    /// For inserts, the expected behaviour is this:
    ///
    /// |           | OriginLeft | OriginRight |
    /// |-----------|------------|-------------|
    /// | NotInsYet | Before     | After       |
    /// | Inserted  | After      | Before      |
    /// | Deleted   | Before     | Before      |
    pub(super) fn apply(&mut self, aa: &AgentAssignment, _ctx: &ListOperationCtx, op_pair: &KVPair<ListOpMetrics>, max_len: usize, agent: AgentId) -> (usize, TransformedResult) {
        // self.check_new_index();

        // dbg!(&op_pair.0);
        // if op_pair.0 == 468 {
        //     println!();
        // }

        // The op must have been applied at the branch that the tracker is currently at.
        let len = max_len.min(op_pair.len());
        let op = &op_pair.1;

        // dbg!(op);
        match op.kind {
            ListOpKind::Ins => {
                if !op.loc.fwd { unimplemented!("Implement me!") }

                // To implement this we need to:
                // 1. Find the item directly before the requested position. This is our origin-left.
                // 2. Scan forward until the next item which isn't in the not yet inserted state.
                // this is our origin right.
                // 3. Use the integrate() method to actually insert - since we need to handle local
                // conflicts.

                // UNDERWATER_START = 4611686018427387903

                let (origin_left, end_pos, mut new_cursor) = if op.start() == 0 {
                    (usize::MAX, 0, self.range_tree.mut_cursor_at_start())
                } else {
                    let (mut end_pos, mut cursor) = self.range_tree.mut_cursor_before_cur_pos(op.start() - 1);
                    let (e, offset) = cursor.0.get_item(&self.range_tree);
                    let origin_left = e.id.start + offset;
                    end_pos += e.takes_up_space::<false>() as usize;
                    // if CHECK_TREES { assert_eq!(origin_left, origin_left_2); }
                    cursor.0.inc_offset(&self.range_tree);

                    (origin_left, end_pos, cursor)
                };
                let cursor_pos = LenPair::new(op.start(), end_pos);
                debug_assert_eq!(new_cursor.0.get_pos(&self.range_tree), cursor_pos);

                // Origin_right should be the next item which isn't in the NotInsertedYet state.
                // If we reach the end of the document before that happens, use usize::MAX.

                let origin_right = if !new_cursor.roll_next_item(&mut self.range_tree) {
                    // Because the list has underwater elements, this never happens in practice.
                    // unreachable!() would be equally valid here.
                    usize::MAX
                } else {
                    let mut c2 = new_cursor.0.clone();

                    // Just scan forward until we find the next item that exists at this point
                    // in time. This scan is O(n) but fast in the average case. It might be faster
                    // in some cases to scan up the tree instead - which would be O(log n) but
                    // (I think) slower on average.
                    loop {
                        let (e, offset) = c2.get_item(&self.range_tree);

                        if e.current_state != NOT_INSERTED_YET {
                            break e.at_offset(offset);
                        }
                        let is_next_entry = c2.next_entry(&self.range_tree).0;
                        debug_assert!(is_next_entry);
                    }
                };

                let mut lv_span = op_pair.span();
                lv_span.trim(len);

                let item = CRDTSpan {
                    id: lv_span,
                    origin_left,
                    origin_right,
                    current_state: INSERTED,
                    end_state_ever_deleted: false,
                };

                let ins_xf_pos = self.integrate(aa, agent, item, new_cursor, cursor_pos);
                (len, BaseMoved(ins_xf_pos))
            }

            ListOpKind::Del => {
                // Delete as much as we can. We might not be able to delete everything because of
                // double deletes and inserts inside the deleted range. This is extra annoying
                // because we need to move backwards through the deleted items if we're rev.
                debug_assert!(op.len() > 0);

                let fwd = op.loc.fwd;

                let (cursor_pos, mut new_cursor, len) = if fwd {
                    let start_pos = op.start();
                    let (end_pos, cursor) = self.range_tree.mut_cursor_before_cur_pos(start_pos);
                    (LenPair::new(start_pos, end_pos), cursor, len)
                } else {
                    // We're moving backwards. We need to delete as many items as we can before the
                    // end of the op.
                    let last_pos = op.loc.span.last();
                    // Find the last entry
                    let (end_pos, mut cursor) = self.range_tree.mut_cursor_before_cur_pos(last_pos);
                    // let mut cursor_pos = LenPair::new(last_pos, end_pos);

                    let (e, offset) = cursor.0.get_item(&self.range_tree);
                    let entry_origin_start = last_pos - offset;
                    // let edit_start = entry_origin_start.max(op.start());
                    let edit_start = entry_origin_start.max(op.end() - len);
                    let len = op.end() - edit_start;

                    debug_assert!(len <= max_len);
                    debug_assert!(e.takes_up_space::<true>()); // We can't delete this item otherwise.

                    let slide_back_by = len - 1;
                    cursor.0.offset -= slide_back_by;

                    let cursor_pos = LenPair::new(
                        last_pos - slide_back_by,
                        end_pos - if e.takes_up_space::<false>() { slide_back_by } else { 0 }
                    );

                    (cursor_pos, cursor, len)
                };
                debug_assert_eq!(new_cursor.0.get_pos(&self.range_tree), cursor_pos);

                let (e, _offset) = new_cursor.0.get_item(&self.range_tree);
                assert_eq!(e.current_state, INSERTED);

                // If we've never been deleted locally, we'll need to do that.
                let ever_deleted = e.end_state_ever_deleted;

                // The transformed position that this delete is at. Only actually needed if we're
                // modifying
                // let del_start_xf = cursor_pos.end;
                debug_assert_eq!(new_cursor.0.get_pos(&self.range_tree), cursor_pos);

                let (len2, target) = self.range_tree.mutate_entry(
                    &mut new_cursor,
                    len,
                    &mut notify_for(&mut self.index),
                    |e| {
                        // println!("mutate entry deleting {:?}", e.id);
                        e.delete();
                        e.id
                    }
                );

                // The cursor shouldn't have moved, since the item we traversed over was
                // deleted.
                self.range_tree.emplace_cursor(cursor_pos, new_cursor);

                // ContentTree should come to the same length conclusion as us.
                if !fwd { debug_assert_eq!(len2, len); }
                let len = len2;

                debug_assert_eq!(len, target.len());
                // debug_assert_eq!(del_start_xf, upstream_cursor_pos(&old_cursor));

                let lv_start = op_pair.0;

                self.index.set_range((lv_start..lv_start + len).into(), Marker::Del(DelRange {
                    target: if fwd { target.start } else { target.end },
                    fwd,
                }).into());

                (len, if !ever_deleted {
                    BaseMoved(cursor_pos.end)
                } else {
                    DeleteAlreadyHappened
                })
            }
        }
    }

    /// Walk through a set of spans, adding them to this tracker.
    ///
    /// Returns the tracker's frontier after this has happened; which will be at some pretty
    /// arbitrary point in time based on the traversal. I could save that in a tracker field? Eh.
    pub(super) fn walk(&mut self, graph: &Graph, aa: &AgentAssignment, op_ctx: &ListOperationCtx, ops: &RleVec<KVPair<ListOpMetrics>>, start_at: Frontier, rev_spans: &[DTRange], mut apply_to: Option<&mut JumpRopeBuf>) -> Frontier {
        let mut walker = SpanningTreeWalker::new(graph, rev_spans, start_at);

        for walk in &mut walker {
            for range in walk.retreat {
                self.retreat_by_range(range);
            }

            for range in walk.advance_rev.into_iter().rev() {
                self.advance_by_range(range);
            }

            debug_assert!(!walk.consume.is_empty());
            self.apply_range(aa, op_ctx, ops, walk.consume, apply_to.as_deref_mut());
        }

        walker.into_frontier()
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub(crate) enum TransformedResult {
    BaseMoved(usize),
    DeleteAlreadyHappened,
}

type TransformedTriple = (LV, ListOpMetrics, TransformedResult);

impl TransformedResult {
    pub(super) fn not_moved(op_pair: KVPair<ListOpMetrics>) -> TransformedTriple {
        let start = op_pair.1.start();
        (op_pair.0, op_pair.1, TransformedResult::BaseMoved(start))
    }
}

#[derive(Debug)]
pub(crate) struct TransformedOpsIterRaw<'a> {
    // subgraph: &'a Graph,
    aa: &'a AgentAssignment,
    op_ctx: &'a ListOperationCtx,
    ops: &'a RleVec<KVPair<ListOpMetrics>>,
    op_iter: Option<BufferedIter<OpMetricsIter<'a>>>,

    tracker: M2Tracker,
    plan: M1Plan,

    /// Where are we up to in the plan?
    plan_idx: usize,

    /// We're in output mode (and we've already built the starting state)
    applying: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TransformedResultRaw {
    FF(DTRange),
    Apply {
        xf_pos: usize,
        op: KVPair<ListOpMetrics>,
    },
    DeleteAlreadyHappened(DTRange),
}

impl HasLength for TransformedResultRaw {
    fn len(&self) -> usize {
        match self {
            TransformedResultRaw::FF(r) => r.len(),
            TransformedResultRaw::Apply { op, .. } => op.len(),
            TransformedResultRaw::DeleteAlreadyHappened(r) => r.len(),
        }
    }
}

impl TransformedResultRaw {
    pub fn lv_range(&self) -> DTRange {
        match self {
            TransformedResultRaw::FF(r) | TransformedResultRaw::DeleteAlreadyHappened(r) => *r,
            TransformedResultRaw::Apply { op, .. } => op.range(),
        }
    }
}


impl<'a> TransformedOpsIterRaw<'a> {
    pub(crate) fn from_plan(aa: &'a AgentAssignment, op_ctx: &'a ListOperationCtx,
                            ops: &'a RleVec<KVPair<ListOpMetrics>>,
                            plan: M1Plan) -> Self {
        Self {
            aa,
            op_ctx,
            ops,
            plan,
            op_iter: None,
            tracker: M2Tracker::new(), // NOTE: This allocates, even if we don't need it.
            plan_idx: 0,
            applying: false,
            // max_frontier: common,
        }
    }

    pub(crate) fn new(subgraph: &'a Graph, aa: &'a AgentAssignment, op_ctx: &'a ListOperationCtx,
                      ops: &'a RleVec<KVPair<ListOpMetrics>>,
                      from_frontier: &[LV], merge_frontier: &[LV]) -> Self {
        let (plan, _common) = subgraph.make_m1_plan(Some(ops), from_frontier, merge_frontier, true);
        Self::from_plan(aa, op_ctx, ops, plan)
    }

    // Returns (remainder, item_here);
    fn next_from(aa: &AgentAssignment, tracker: &mut M2Tracker, op_ctx: &ListOperationCtx, mut pair: KVPair<ListOpMetrics>)
        -> (Option<KVPair<ListOpMetrics>>, TransformedResultRaw)
    {
        // Ok, try to consume as much as we can from pair.
        let span = aa.local_span_to_agent_span(pair.span());
        let len = span.len().min(pair.len());

        let (consumed_here, xf_result) = tracker.apply(aa, op_ctx, &pair, len, span.agent);

        let remainder = pair.trim_ctx(consumed_here, op_ctx);

        // (Time, OperationInternal, TransformedResult)
        let result = match xf_result {
            BaseMoved(xf_pos) => {
                TransformedResultRaw::Apply { xf_pos, op: pair }
            },
            DeleteAlreadyHappened => TransformedResultRaw::DeleteAlreadyHappened(pair.span()),
        };

        (remainder, result)
    }

    /// Returns if concurrent inserts ever collided at the same location while traversing.
    #[cfg(feature = "merge_conflict_checks")]
    pub(crate) fn concurrent_inserts_collided(&self) -> bool {
        self.tracker.concurrent_inserts_collide
    }
}

impl<'a> Iterator for TransformedOpsIterRaw<'a> {
    type Item = TransformedResultRaw;

    fn next(&mut self) -> Option<Self::Item> {
        // if self.plan_idx >= self.plan.0.len() { return None; }

        if let Some(op_iter) = self.op_iter.as_mut() {
            if let Some(pair) = op_iter.next() {
                // dbg!(&pair);

                let (remainder, result) = Self::next_from(self.aa, &mut self.tracker, self.op_ctx, pair);
                if let Some(r) = remainder {
                    op_iter.push_back(r);
                }
                return Some(result);
            } else { self.op_iter = None; }
        }

        while self.plan_idx < self.plan.0.len() {
            let action = &self.plan.0[self.plan_idx];
            self.plan_idx += 1;

            match action {
                M1PlanAction::Retreat(span) => {
                    self.tracker.retreat_by_range(*span);
                }
                M1PlanAction::Advance(span) => {
                    self.tracker.advance_by_range(*span);
                }
                M1PlanAction::Apply(span) => {
                    if !self.applying {
                        // Just apply it directly to the tracker.
                        self.tracker.apply_range(self.aa, self.op_ctx, self.ops, *span, None);
                    } else {
                        let mut op_iter = BufferedIter::new(OpMetricsIter::new(self.ops, self.op_ctx, *span));
                        if let Some(pair) = op_iter.next() {
                            let (remainder, result) = Self::next_from(self.aa, &mut self.tracker, self.op_ctx, pair);
                            if let Some(r) = remainder {
                                op_iter.push_back(r);
                            }
                            self.op_iter = Some(op_iter);
                            return Some(result);
                        }
                    }
                }
                M1PlanAction::FF(span) => {
                    // FF doesn't make sense unless we're applying the operations.
                    debug_assert!(self.applying);
                    return Some(TransformedResultRaw::FF(*span));
                }
                M1PlanAction::Clear => {
                    self.tracker.clear();
                }
                M1PlanAction::BeginOutput => {
                    self.applying = true;
                }
            }
        }

        // self.close_tracker();
        return None;
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum TransformedSimpleOp {
    Apply(KVPair<ListOpMetrics>),
    DeleteAlreadyHappened(DTRange),
}

/// This wraps TransformedOpsIterRaw to provide the same API as the older transformed ops iterator.
/// In particular, it iterates over *operations* rather than iterating over how the operation was
/// transformed.
///
/// TODO: Name me.
pub(crate) struct TransformedSimpleOpsIter<'a> {
    inner: TransformedOpsIterRaw<'a>,
    ff_iter: Option<(std::slice::Iter<'a, KVPair<ListOpMetrics>>, usize)>,
}

impl<'a> From<TransformedOpsIterRaw<'a>> for TransformedSimpleOpsIter<'a> {
    fn from(inner: TransformedOpsIterRaw<'a>) -> Self {
        Self { inner, ff_iter: None }
    }
}

impl<'a> Iterator for TransformedSimpleOpsIter<'a> {
    type Item = TransformedSimpleOp;

    fn next(&mut self) -> Option<Self::Item> {
        use TransformedSimpleOp::*;

        if let Some((ff_iter, end)) = self.ff_iter.as_mut() {
            if let Some(item) = ff_iter.next() {
                let mut item = item.clone();
                if item.0 < *end {
                    let item_end = item.end();
                    if item_end > *end {
                        item.truncate_ctx(*end - item.0, self.inner.op_ctx);
                    }
                    return Some(Apply(item));
                }
            }

            self.ff_iter = None;
        }

        // Otherwise, take the next item from the internal iterator.
        match self.inner.next() {
            None => None,
            Some(TransformedResultRaw::Apply { xf_pos, mut op }) => {
                op.1.transpose_to(xf_pos);
                Some(Apply(op))
            },
            Some(TransformedResultRaw::FF(range)) => {
                debug_assert!(!range.is_empty());

                let start_idx = self.inner.ops.find_next_index(range.start);
                let mut first = self.inner.ops[start_idx].clone();
                if first.0 < range.start {
                    first.truncate_keeping_right_ctx(range.start - first.0, self.inner.op_ctx);
                }

                self.ff_iter = Some((self.inner.ops.0[start_idx+1..].iter(), range.end));

                Some(Apply(first))
            },
            Some(TransformedResultRaw::DeleteAlreadyHappened(range)) => Some(DeleteAlreadyHappened(range)),
        }
    }
}


pub fn reverse_str(s: &str) -> SmartString {
    let mut result = SmartString::new();
    result.extend(s.chars().rev());
    result
}

impl TextInfo {
    pub(crate) fn get_xf_operations_full<'a>(&'a self, subgraph: &'a Graph, aa: &'a AgentAssignment, from: &[LV], merging: &[LV]) -> TransformedOpsIterRaw<'a> {
        TransformedOpsIterRaw::new(subgraph, aa, &self.ctx, &self.ops, from, merging)
    }

    pub(crate) fn with_xf_iter<F: FnOnce(TransformedOpsIterRaw, Frontier) -> R, R>(&self, cg: &CausalGraph, from: &[LV], merge_frontier: &[LV], f: F) -> R {
        // This is a big dirty mess for now, but it should be correct at least.
        let conflict = cg.graph.find_conflicting_simple(from, merge_frontier);

        let final_frontier = cg.graph.find_dominators_2(from, merge_frontier);
        // if final_frontier.as_ref() == from { return final_frontier; } // Nothing to do!

        // This looks inefficient - since after all, we only care about the operations in the
        // conflict zone. But because we scan the intersection of these operations and the conflict,
        // and scan them backwards, it works out to be efficient in practice.
        let op_spans = self.ops.iter().map(|e| e.span())
            .rev()
            .merge_spans_rev();

        // We create the subgraph from operations which intersect:
        // - The graph passed in
        // - The conflict zone between from -> merge_frontier
        // - The operations on this text document
        let iter = rle_intersect_rev(op_spans, conflict.rev_spans.iter().copied())
            .map(|pair| pair.0);

        let (subgraph, _ff) = cg.graph.subgraph_raw(iter.clone(), final_frontier.as_ref());

        // println!("{}", subgraph.0.0.len());
        // subgraph.dbg_check_subgraph(true); // For debugging.
        // dbg!(&subgraph, ff.as_ref());

        let from = cg.graph.project_onto_subgraph_raw(iter.clone(), from);
        let merge_frontier = cg.graph.project_onto_subgraph_raw(iter.clone(), merge_frontier);

        // let mut iter = TransformedOpsIter::new(oplog, &self.frontier, merge_frontier);
        let iter = self.get_xf_operations_full(&subgraph, &cg.agent_assignment, from.as_ref(), merge_frontier.as_ref());
        f(iter, final_frontier)
    }

    /// Iterate through all the *transformed* operations from some point in time. Internally, the
    /// OpLog stores all changes as they were when they were created. This makes a lot of sense from
    /// CRDT academic point of view (and makes signatures and all that easy). But its is rarely
    /// useful for a text editor.
    ///
    /// `get_xf_operations` returns an iterator over the *transformed changes*. That is, the set of
    /// changes that could be applied linearly to a document to bring it up to date.
    pub fn xf_operations_from<'a>(&'a self, cg: &'a CausalGraph, from: &[LV], merging: &[LV]) -> Vec<(DTRange, Option<TextOperation>)> {
        self.with_xf_iter(cg, from, merging, |raw_iter, _| {
            let iter: TransformedSimpleOpsIter = raw_iter.into();
            iter.map(|op| {
                match op {
                    TransformedSimpleOp::Apply(metrics) => {
                        let span = metrics.span();
                        let content = metrics.1.get_content(&self.ctx);
                        (span, Some((metrics.1, content).into()))
                    }
                    TransformedSimpleOp::DeleteAlreadyHappened(r) => (r, None),
                }
            }).collect()
        })
    }

    /// Get all transformed operations from the start of time.
    ///
    /// This is a shorthand for `oplog.xf_operations_from(&[], oplog.local_version)`, but
    /// I hope that future optimizations make this method way faster.
    pub fn iter_xf_operations<'a>(&'a self, cg: &'a CausalGraph) -> Vec<(DTRange, Option<TextOperation>)> {
        self.xf_operations_from(cg, &[], cg.version.as_ref())
    }

    /// Add everything in merge_frontier into the set..
    pub fn merge_into(&self, into: &mut JumpRopeBuf, cg: &CausalGraph, from: &[LV], merge_frontier: &[LV]) -> Frontier {
        // println!("merge from {:?} + {:?}", from, merge_frontier);
        self.with_xf_iter(cg, from, merge_frontier, |iter, final_frontier| {
            // iter.plan.dbg_print();

            for xf in iter {
                match xf {
                    TransformedResultRaw::Apply { xf_pos, op: KVPair(_, mut op) } => {
                        op.transpose_to(xf_pos);
                        self.apply_op_to(op, into);
                    }

                    TransformedResultRaw::FF(range) => {
                        // Activate *SUPER FAST MODE*.
                        for KVPair(_, op) in self.ops.iter_range_ctx(range, &self.ctx) {
                            // dbg!(&op);
                            self.apply_op_to(op, into);
                        }
                    }

                    TransformedResultRaw::DeleteAlreadyHappened(_) => {} // Discard.
                }
            }

            final_frontier
        })
    }


    // /// Add everything in merge_frontier into the set..
    // pub fn merge_into(&self, into: &mut JumpRopeBuf, cg: &CausalGraph, from: &[LV], merge_frontier: &[LV]) -> Frontier {
    //     let (graph, flat) = match FlattenedOps::new_from_subgraph(cg, from, merge_frontier, &self.ops) {
    //         Ok(flat) => { flat }
    //         Err(final_frontier) => { return final_frontier; }
    //     };
    //
    //     let mut iter = flat.iter(&graph, &cg.agent_assignment, &self.ctx, &self.ops);
    //     for (_lv, origin_op, xf) in &mut iter {
    //         match (origin_op.kind, xf) {
    //             (ListOpKind::Ins, BaseMoved(pos)) => {
    //                 // println!("Insert '{}' at {} (len {})", op.content, ins_pos, op.len());
    //                 debug_assert!(origin_op.content_pos.is_some()); // Ok if this is false - we'll just fill with junk.
    //                 let content = origin_op.get_content(&self.ctx).unwrap();
    //                 assert!(pos <= into.len_chars());
    //                 if origin_op.loc.fwd {
    //                     into.insert(pos, content);
    //                 } else {
    //                     // We need to insert the content in reverse order.
    //                     let c = reverse_str(content);
    //                     into.insert(pos, &c);
    //                 }
    //             }
    //
    //             (_, DeleteAlreadyHappened) => {}, // Discard.
    //
    //             (ListOpKind::Del, BaseMoved(pos)) => {
    //                 let del_end = pos + origin_op.len();
    //                 debug_assert!(into.len_chars() >= del_end);
    //                 // println!("Delete {}..{} (len {}) '{}'", del_start, del_end, mut_len, to.content.slice_chars(del_start..del_end).collect::<String>());
    //                 into.remove(pos..del_end);
    //             }
    //         }
    //     }
    //
    //     iter.into_frontier()
    // }
}

#[cfg(test)]
mod test {
    use std::fs::File;
    use std::io::Read;

    use rle::{MergeableIterator, SplitableSpan};

    use crate::dtrange::UNDERWATER_START;
    use crate::list::ListOpLog;
    use crate::listmerge::simple_oplog::SimpleOpLog;
    use crate::listmerge::yjsspan::{deleted_n_state, DELETED_ONCE, SpanState};
    use crate::stats::take_stats;

    use super::*;

    #[test]
    fn test_ff() {
        let mut list = SimpleOpLog::new();
        list.add_insert("a", 0, "aaa");

        let mut result = JumpRopeBuf::new();
        list.merge_raw(&mut result, &[], &[1]);
        list.merge_raw(&mut result, &[1], &[2]);

        assert_eq!(result, "aaa");
    }

    #[test]
    fn test_ff_goop() {
        let mut list = SimpleOpLog::new();
        list.add_insert("a", 0, "a");
        list.goop(5);
        list.add_insert("a", 1, "bb");

        let mut result = JumpRopeBuf::new();
        let f1 = list.merge_raw(&mut result, &[], &[5]);
        list.merge_raw(&mut result, f1.as_ref(), &[7]);

        assert_eq!(result, "abb");
    }

    #[test]
    fn test_ff_merge() {
        let mut list = SimpleOpLog::new();

        list.add_insert_at("a", &[], 0, "aaa");
        list.add_insert_at("b", &[], 0, "bbb");
        assert_eq!("aaabbb", list.to_string());

        list.add_insert_at("a", &[2, 5], 0, "ccc"); // 8
        assert_eq!("cccaaabbb", list.to_string());
    }

    #[test]
    fn test_merge_inserts() {
        let mut list = SimpleOpLog::new();
        list.add_insert_at("a", &[], 0, "aaa");
        list.add_insert_at("b", &[], 0, "bbb");

        assert_eq!(list.to_string(), "aaabbb");
    }

    #[test]
    fn test_merge_deletes_1() {
        let mut list = SimpleOpLog::new();

        list.add_insert("a", 0, "aaa");

        list.add_delete_at("a", &[2], 1..2); // &[3]
        list.add_delete_at("b", &[2], 0..3); // &[6]

        // M2Tracker::apply_to_checkout(&mut list.checkout, &list.ops, (0..list.ops.len()).into());
        // list.checkout.merge_changes_m2(&list.ops, (3..list.ops.len()).into());
        // list.branch.merge(&list.oplog, &[3, 6]);
        assert_eq!(list.to_string(), "");
    }

    #[test]
    fn test_merge_deletes_2() {
        let mut list = SimpleOpLog::new();

        let t = list.add_insert_at("a", &[], 0, "aaa");
        list.add_delete_at("a", &[t], 1..2); // 3
        list.add_delete_at("b", &[t], 0..3); // 6
        // dbg!(&list.ops);

        // list.checkout.merge_changes_m2(&list.ops, (0..list.ops.len()).into());
        // list.branch.merge(&list.oplog, &[3, 6]);
        // dbg!(&list.branch);
        assert_eq!(list.to_string(), "");
    }

    fn items(tracker: &M2Tracker, filter_underwater: usize) -> Vec<CRDTSpan> {
        let trim_from = UNDERWATER_START + filter_underwater;

        tracker.range_tree
            .iter()
            .filter_map(|mut i| {
                // dbg!((i.id.end, trim_from, i.id.start));
                if i.id.start >= trim_from {
                    assert_eq!(i.current_state, INSERTED);
                    return None;
                }

                if i.id.end > trim_from {
                    assert_eq!(i.current_state, INSERTED);
                    i.truncate(i.id.end - trim_from);
                }

                Some(i)
            })
            .merge_spans()
            .collect()
    }

    fn items_state(tracker: &M2Tracker, filter_underwater: usize) -> Vec<(usize, SpanState)> {
        items(tracker, filter_underwater).iter().map(|i| (i.len(), i.current_state)).collect()
    }

    #[test]
    fn test_concurrent_insert() {
        let mut list = SimpleOpLog::new();

        list.add_insert_at("a", &[], 0, "aaa");
        list.add_insert_at("b", &[], 0, "bbb");

        let mut content = JumpRopeBuf::new();
        let mut t = M2Tracker::new();
        t.apply_range(&list.cg.agent_assignment, &list.info.ctx, &list.info.ops, (0..3).into(), Some(&mut content));
        t.retreat_by_range((0..3).into());
        t.apply_range(&list.cg.agent_assignment, &list.info.ctx, &list.info.ops, (3..6).into(), Some(&mut content));

        let i: Vec<_> = items(&t, 0).iter().map(|i| (i.id, i.current_state)).collect();
        assert_eq!(i, &[
            ((0..3).into(), NOT_INSERTED_YET),
            ((3..6).into(), INSERTED),
        ]);
        // dbg!(&t);
        // t.apply_range_at_version()

        assert_eq!(content, "aaabbb");
    }

    #[test]
    fn test_concurrent_delete() {
        let mut list = SimpleOpLog::new();

        list.add_insert("a", 0, "aaa");

        list.add_delete_at("a", &[2], 1..2);
        list.add_delete_at("b", &[2], 0..3);

        let mut content = JumpRopeBuf::new();
        let mut t = M2Tracker::new();
        t.apply_range(&list.cg.agent_assignment, &list.info.ctx, &list.info.ops, (0..4).into(), Some(&mut content));
        t.retreat_by_range((3..4).into());
        t.apply_range(&list.cg.agent_assignment, &list.info.ctx, &list.info.ops, (4..7).into(), Some(&mut content));
        t.advance_by_range((3..4).into());

        assert_eq!(items_state(&t, 0), &[
            (1, deleted_n_state(1)),
            (1, deleted_n_state(2)),
            (1, deleted_n_state(1)),
        ]);
        // dbg!(&t);

        assert_eq!(content, "");
        // t.apply_range_at_version()
    }

    #[test]
    fn unroll_delete() {
        let mut list = SimpleOpLog::new();
        list.add_insert("a", 0, "hi there"); // 0..8
        list.add_delete("a", 2..5); // 8..11

        let mut t = M2Tracker::new();

        let mut content = JumpRopeBuf::new();
        let end = list.cg.len();
        // dbg!(end);
        t.apply_range(&list.cg.agent_assignment, &list.info.ctx, &list.info.ops, (0..end).into(), Some(&mut content));
        assert_eq!(content, "hiere");
        // dbg!(&t);

        // t.retreat_by_range((0..end).into());
        t.retreat_by_range((8..end).into()); // undelete
        t.retreat_by_range((7..8).into()); // Uninsert the last character
        // dbg!(&t);
        // dbg!(items_state(&t, 0));
        assert_eq!(items_state(&t, 0), &[
            // It'd be nice if this collapsed together but whatever.
            (2, INSERTED),
            (3, INSERTED),
            (2, INSERTED),
            (1, NOT_INSERTED_YET),
        ]);
    }

    #[test]
    fn backspace() {
        let mut list = SimpleOpLog::new();
        list.add_insert("seph", 0, "abc"); // 2
        list.add_delete("seph", 2..3); // 3 -> "ab_"
        list.add_delete("seph", 1..2); // 4 -> "a__"
        let t = list.add_delete("seph", 0..1); // 5 -> "___"
        assert_eq!(t, 5);

        let mut t = M2Tracker::new();
        t.apply_range(&list.cg.agent_assignment, &list.info.ctx, &list.info.ops, (3..6).into(), None);
        assert_eq!(items_state(&t, 3), &[(3, DELETED_ONCE)]);

        t.retreat_by_range((5..6).into());
        assert_eq!(items_state(&t, 3), &[(1, INSERTED), (2, DELETED_ONCE)]);
        // dbg!(&t);

        assert_eq!(list.to_string(), "");

        // list.checkout.merge_branch(&list.ops, &[4]);
        // dbg!(&list.checkout);
    }

    #[test]
    fn ins_back() {
        let mut list = SimpleOpLog::new();

        list.add_insert("seph", 0, "c");
        list.add_insert("seph", 0, "b");
        list.add_insert("seph", 0, "a");

        assert_eq!(list.to_string(), "abc");
    }

    #[cfg(feature = "gen_test_data")]
    fn dump_index_stats(bench_name: &str) {
        let mut bytes = vec![];
        File::open(format!("benchmark_data/{bench_name}.dt")).unwrap().read_to_end(&mut bytes).unwrap();
        let o = ListOpLog::load_from(&bytes).unwrap();

        let out_file = format!("idxtrace_{bench_name}.json");
        let mut iter = o.get_xf_operations_full(&[], o.cg.version.as_ref());
        while let Some(_) = iter.next() {}
        let json = iter.tracker.index.actions_to_json();
        std::fs::write(&out_file, &json).unwrap();
        println!("wrote index writes to {out_file}");
    }


    #[test]
    #[ignore]
    fn print_stats() {
        // node_nodecc: 72135
        // git-makefile: 23166
        let mut bytes = vec![];

        File::open("benchmark_data/git-makefile.dt").unwrap().read_to_end(&mut bytes).unwrap();
        let o = ListOpLog::load_from(&bytes).unwrap();
        o.checkout_tip();

        take_stats();

        println!("----");
        bytes.clear();
        File::open("benchmark_data/node_nodecc.dt").unwrap().read_to_end(&mut bytes).unwrap();
        let o = ListOpLog::load_from(&bytes).unwrap();
        o.checkout_tip();

        take_stats();


        // println!("----");
        // bytes.clear();
        // File::open("benchmark_data/friendsforever.dt").unwrap().read_to_end(&mut bytes).unwrap();
        // let o = ListOpLog::load_from(&bytes).unwrap();
        // o.checkout_tip();
        //
        // let mut iter = o.get_xf_operations_full(&[], o.cg.version.as_ref());
        // while let Some(_) = iter.next() {}
        // let json = iter.tracker.index.actions_to_json();
        // fs::write("friendsforever.json", &json).unwrap();
        // println!("wrote index writes to friendsforever.json");
    }

    // Run me in release mode!
    // $ cargo test --release --features gen_test_data -- --ignored --nocapture gen_index_traces
    #[cfg(feature = "gen_test_data")]
    #[test]
    #[ignore]
    fn gen_index_traces() {
        for name in &["friendsforever", "git-makefile", "node_nodecc", "clownschool"] {
            dump_index_stats(*name);
        }
    }
}






// #[derive(Debug)]
// struct FlattenedOps {
//     common_ancestor: Frontier,
//     conflict_ops: SmallVec<[DTRange; 4]>,
//     new_ops: SmallVec<[DTRange; 4]>,
//     from_frontier: Frontier,
//     merge_frontier: Frontier,
// }
//
// impl FlattenedOps {
//     fn new_from_subgraph(cg: &CausalGraph, from_frontier: &[LV], merge_frontier: &[LV], ops: &RleVec<KVPair<ListOpMetrics>>) -> Result<(Graph, Self), Frontier> {
//         // This is a big dirty mess for now, but it should be correct at least.
//         let global_conflict_zone = cg.graph.find_conflicting_simple(from_frontier, merge_frontier);
//         let earliest = global_conflict_zone.common_ancestor.0.get(0).copied().unwrap_or(0);
//
//         let final_frontier_global = cg.graph.find_dominators_2(from_frontier, merge_frontier);
//         // if final_frontier.as_ref() == from { return final_frontier; } // Nothing to do!
//
//         // We actually only need the ops in intersection b
//         let op_spans = ops.iter().map(|e| e.span())
//             .rev()
//             // .merge_spans_rev()
//             .take_while(|r| r.end > earliest);
//         // let iter = rle_intersect_rev_first(op_spans, global_conflict_zone.rev_spans.iter().copied());
//         let iter = op_spans;
//
//         let (subgraph, _ff) = cg.graph.subgraph_raw(iter.clone(), final_frontier_global.as_ref());
//
//         // println!("{}", subgraph.0.0.len());
//         // subgraph.dbg_check_subgraph(true); // For debugging.
//         // dbg!(&subgraph, ff.as_ref());
//
//         let from_frontier = cg.graph.project_onto_subgraph_raw(iter.clone(), from_frontier);
//         let merge_frontier = cg.graph.project_onto_subgraph_raw(iter.clone(), merge_frontier);
//
//         let mut new_ops: SmallVec<[DTRange; 4]> = smallvec![];
//         let mut conflict_ops: SmallVec<[DTRange; 4]> = smallvec![];
//
//         // Process the conflicting edits again, this time just scanning the subgraph.
//         let common_ancestor = subgraph.find_conflicting(from_frontier.as_ref(), merge_frontier.as_ref(), |span, flag| {
//             // Note we'll be visiting these operations in reverse order.
//             let target = match flag {
//                 DiffFlag::OnlyB => &mut new_ops,
//                 _ => &mut conflict_ops
//             };
//             target.push_reversed_rle(span);
//         });
//         // dbg!(&common_ancestor);
//
//         let final_frontier_subgraph = subgraph.find_dominators_2(from_frontier.as_ref(), merge_frontier.as_ref());
//         if final_frontier_subgraph == from_frontier { return Result::Err(final_frontier_global); } // Nothing to do! Just an optimization... Not sure if its necessary.
//
//         // dbg!(ops.iter().map(|e| e.span())
//         //     .rev()
//         //     .take_while(|r| r.end > earliest).collect::<Vec<_>>());
//         // dbg!(&subgraph);
//         // dbg!(&new_ops);
//
//         Result::Ok((subgraph, Self {
//             from_frontier,
//             merge_frontier,
//             common_ancestor,
//             conflict_ops,
//             new_ops,
//         }))
//     }
//
//     fn iter<'a>(self, subgraph: &'a Graph, aa: &'a AgentAssignment, op_ctx: &'a ListOperationCtx, ops: &'a RleVec<KVPair<ListOpMetrics>>) -> TransformedOpsIter<'a> {
//         TransformedOpsIter {
//             subgraph,
//             aa,
//             op_ctx,
//             ops,
//             op_iter: None,
//             ff_mode: true,
//             did_ff: false,
//             merge_frontier: self.merge_frontier,
//             common_ancestor: self.common_ancestor,
//             conflict_ops: self.conflict_ops,
//             new_ops: self.new_ops,
//             next_frontier: self.from_frontier,
//             phase2: None,
//         }
//     }
// }
