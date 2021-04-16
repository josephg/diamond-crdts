use crate::automerge::{TxnInternal, Op, TxnExternal, DocumentState, OpExternal, ClientData, MarkerEntry, Order, ROOT_ORDER};
use crate::range_tree::{RangeTree, NodeLeaf, Cursor, FullIndex};
use ropey::Rope;
use crate::common::{CRDTLocation, AgentId, CRDT_DOC_ROOT};
use smallvec::{SmallVec, smallvec};
use std::collections::BTreeSet;
use crate::split_list::SplitList;
use std::ptr::NonNull;
use crate::splitable_span::SplitableSpan;
use crate::automerge::order::OrderMarker;
use inlinable_string::InlinableString;
use std::cmp::Ordering;
use crate::automerge::sibling_range::SiblingRange;

pub(crate) struct OpIterator<'a> {
    txn: &'a TxnInternal,
    index: usize,
    order: Order,
}

impl<'a> Iterator for OpIterator<'a> {
    type Item = (&'a Op, Order); // (Operation, operation's order for inserts, or 0.)

    fn next(&mut self) -> Option<Self::Item> {
        if self.index >= self.txn.ops.len() { return None; }

        let current = &self.txn.ops[self.index];
        self.index += 1;
        let len = current.item_len();

        let old_order = self.order;
        self.order += len;
        Some((current, old_order))
    }
}

impl Op {
    fn item_len(&self) -> usize {
        match self {
            Op::Insert { content, .. } => { content.chars().count() },
            Op::Delete { .. } => { 0 }
        }
    }
}


impl TxnInternal {
    fn iter(&self) -> OpIterator {
        OpIterator {
            txn: self,
            index: 0,
            order: self.insert_order_start
        }
    }

    #[allow(unused)]
    fn check(&self) {
        // A transaction must not reference anything within itself.
        let mut next_order = self.insert_order_start;
        for (op, order) in self.iter() {
            if let Op::Insert { content, parent: predecessor } = op {
                assert_eq!(*predecessor, next_order);
                next_order += content.chars().count();
                // The reference can't be within the range, and can't reference anything we haven't
                // seen yet.
                assert!(*predecessor < self.insert_order_start);
            }
        }
        assert_eq!(next_order, self.insert_order_start + self.num_inserts);
    }

    fn get_item_parent(&self, item_order: Order) -> Order {
        // Scan the txn looking for the insert
        for (op, order) in self.iter() {
            if let Op::Insert { parent, .. } = op {
                // TODO: Add a field for content length. This is super inefficient.
                if item_order >= order { return *parent; }
            }
        }
        unreachable!("Failed invariant - txn does not contain item")
    }
}

// Toggleable for testing.
const USE_INNER_ROPE: bool = true;

fn ordering_from(x: isize) -> Ordering {
    if x < 0 { Ordering::Less }
    else if x > 0 { Ordering::Greater }
    else { Ordering::Equal }
}

impl DocumentState {
    fn new() -> Self {
        Self {
            frontier: smallvec![ROOT_ORDER],
            txns: vec![],
            client_data: vec![],

            range_tree: RangeTree::new(),
            markers: SplitList::new(),
            next_sibling_tree: RangeTree::new(),

            text_content: Rope::new()
        }
    }
    
    pub fn get_or_create_client_id(&mut self, name: &str) -> AgentId {
        // Probably a nicer way to write this.
        if name == "ROOT" { return AgentId::MAX; }

        if let Some(id) = self.get_client_id(name) {
            id
        } else {
            // Create a new id.
            self.client_data.push(ClientData {
                name: InlinableString::from(name),
                txn_orders: Vec::new(),
            });
            (self.client_data.len() - 1) as AgentId
        }
    }

    fn get_client_id(&self, name: &str) -> Option<AgentId> {
        if name == "ROOT" { Some(AgentId::MAX) }
        else {
            self.client_data.iter()
                .position(|client_data| &client_data.name == name)
                .map(|id| id as AgentId)
        }
    }

    // fn map_external_crdt_location(&mut self, loc: &CRDTLocationExternal) -> CRDTLocation {
    //     CRDTLocation {
    //         agent: self.get_or_create_client_id(&loc.agent),
    //         seq: loc.seq
    //     }
    // }

    pub fn len(&self) -> usize {
        self.range_tree.as_ref().content_len()
    }

    fn branch_contains_version(&self, target: Order, branch: &[Order]) -> bool {
        println!("branch_contains_versions target: {} branch: {:?}", target, branch);
        // Order matters between these two lines because of how this is used in applyBackwards.
        if branch.len() == 0 { return false; }
        if target == ROOT_ORDER || branch.contains(&target) { return true; }

        // This works is via a DFS from the operation with a higher localOrder looking
        // for the Order of the smaller operation.
        // Note adding BTreeSet here adds a lot of code size. I could instead write this to use a
        // simple Vec<> + bsearch and then do BFS instead of DFS, which would be slower but smaller.
        let mut visited = BTreeSet::<Order>::new();
        let mut found = false;

        // LIFO queue. We could use a priority queue here but I'm not sure it'd be any
        // faster in practice.
        let mut queue = SmallVec::<[usize; 4]>::from(branch); //branch.to_vec();
        queue.sort_by(|a, b| b.cmp(a)); // descending so we hit the lowest first.

        while !found {
            let order = match queue.pop() {
                Some(o) => o,
                None => { break; }
            };

            if order <= target || order == ROOT_ORDER {
                if order == target { found = true; }
                continue;
            }

            if visited.contains(&order) { continue; }
            visited.insert(order);

            // let op = self.operation_by_order(order);
            let txn = &self.txns[order];

            // Operation versions. Add all of op's parents to the queue.
            queue.extend(txn.parents.iter().copied());

            // Ordered so we hit this next. This isn't necessary, the succeeds field
            // will just often be smaller than the parents.
            // if let Some(succeeds) = txn.succeeds {
            //     queue.push(succeeds);
            // }
        }

        found
    }

    /// Compare two versions to see if a>b, a<b, a==b or a||b (a and b are concurrent).
    /// This follows the pattern of PartialOrd, where we return None for concurrent operations.
    fn compare_versions(&self, a: Order, b: Order) -> Option<Ordering> {
        if a == b { return Some(Ordering::Equal); }

        // Its impossible for the operation with a smaller order to dominate the op with a larger
        // order
        let (start, target, result) = if a > b {
            (a, b, Ordering::Greater)
        } else {
            (b, a, Ordering::Less)
        };

        if self.branch_contains_version(target, &[start]) { Some(result) } else { None }
    }


    fn notify(markers: &mut SplitList<MarkerEntry<OrderMarker, FullIndex>>, entry: OrderMarker, ptr: NonNull<NodeLeaf<OrderMarker, FullIndex>>) {
        // eprintln!("notify callback {:?} {:?}", entry, ptr);
        // let markers = &mut client_data[entry.loc.agent as usize].markers;
        // for op in &mut markers[loc.seq as usize..(loc.seq+len) as usize] {
        //     *op = ptr;
        // }

        markers.replace_range(entry.order as usize, MarkerEntry {
            ptr, len: entry.len() as u32
        });
    }

    fn next_txn_with_inserts(&self, mut txn_order: usize) -> &TxnInternal {
        for txn in &self.txns[txn_order..] {
            if txn.num_inserts > 0 { return txn; }
        }
        unreachable!()
        // loop {
        //     let txn = &self.txns[txn_order];
        //     if txn.num_inserts == 0 {
        //         txn_order += 1;
        //     } else {
        //         return txn;
        //     }
        // }
    }

    fn get_item_order(&self, item_loc: CRDTLocation) -> usize {
        dbg!(item_loc);
        if item_loc == CRDT_DOC_ROOT {
            return ROOT_ORDER
        }

        let client_data: &ClientData = &self.client_data[item_loc.agent as usize];
        let txn = match client_data.txn_orders
        .binary_search_by_key(&item_loc.seq, |order| {
            let txn: &TxnInternal = &self.txns[*order];
            txn.insert_seq_start
        }) {
            Ok(seq) => {
                // If there's a delete followed by an insert, we might have landed in the delete
                // and not found the subsequent insert (which is the one we're interested in).
                let mut txn_order: Order = client_data.txn_orders[seq];
                self.next_txn_with_inserts(txn_order)
            }
            Err(next_seq) => {
                let txn_order: Order = client_data.txn_orders[next_seq - 1];
                &self.txns[txn_order]
            }
        };

        // dbg!(txn_order, txn);

        // Yikes the code above is complex. Make sure we found the right element.
        debug_assert!(txn.num_inserts > 0);
        assert!(item_loc.seq >= txn.id.seq && item_loc.seq < txn.id.seq + txn.num_inserts as u32);
        txn.insert_order_start + (item_loc.seq - txn.insert_seq_start) as usize
    }

    fn try_get_txn_order(&self, txn_id: CRDTLocation) -> Option<usize> {
        if txn_id == CRDT_DOC_ROOT {
            return Some(ROOT_ORDER)
        }
        let client = &self.client_data[txn_id.agent as usize];
        client.txn_orders.get(txn_id.seq as usize).copied()
    }

    fn get_txn_order(&self, txn_id: CRDTLocation) -> usize {
        self.try_get_txn_order(txn_id).unwrap()
    }

    fn get_txn_containing_item(&self, item_order: Order) -> &TxnInternal {
        // println!("get_txn_containing_item {}", item_order);
        match self.txns.binary_search_by_key(&item_order, |txn| {
            txn.insert_order_start
        }) {
            Ok(txn_order) => {
                // dbg!("-> OK", txn_order);
                self.next_txn_with_inserts(txn_order)
            }
            Err(txn_order) => {
                // dbg!("-> Err", txn_order);
                // &self.txns[next_order - 1]
                &self.txns[txn_order]
            }
        }
    }

    fn get_item_parent(&self, item_order: Order) -> Order {
        let txn = self.get_txn_containing_item(item_order);
        // Scan the txn looking for the insert
        for (op, order) in txn.iter() {
            if let Op::Insert { parent, .. } = op {
                // TODO: Add a field for content length. This is super inefficient.
                if item_order >= order { return *parent; }
            }
        }
        unreachable!("Failed invariant - txn does not contain item")
    }

    fn advance_frontier(&mut self, order: usize, parents: &SmallVec<[usize; 2]>) {
        // TODO: Port these javascript checks in debug mode.
        // assert(!this.branchContainsVersion(txn.order, this.frontier), 'doc already contains version')
        // for (const parent of txn.parentsOrder) {
        //     assert(this.branchContainsVersion(parent, this.frontier), 'operation in the future')
        // }

        let mut new_frontier = smallvec![order];

        // TODO: Make this code not need to allocate if the frontier is large.
        for order in self.frontier.iter() {
            if !parents.contains(order) {
                new_frontier.push(*order);
            }
        }

        self.frontier = new_frontier;
    }

    fn next_item_order(&self) -> usize {
        if let Some(txn) = self.txns.last() {
            txn.insert_order_start + txn.num_inserts
        } else { 0 }
    }

    /// Compare two item orders to see the order in which they should end up in the resulting
    /// document. The ordering follows the resulting positions - so a<b implies a earlier than b in
    /// the document.
    fn cmp_item_order2(&self, a: Order, txn_a: &TxnInternal, b: Order, txn_b: &TxnInternal) -> Ordering {
        if a == b { return Ordering::Equal; }

        dbg!(txn_a, txn_b);
        if txn_a.id.agent == txn_b.id.agent {
            // We can just compare the sequence numbers to see which is newer.
            // Newer (higher seq) -> earlier in the document.
            txn_b.id.seq.cmp(&txn_a.id.seq)
        } else {
            let cmp = self.compare_versions(txn_a.order, txn_b.order);
            cmp.unwrap_or_else(|| {
                // Do'h - they're concurrent. Order based on sorting the agent strings.
                let a_name = &self.client_data[txn_a.id.agent as usize].name;
                let b_name = &self.client_data[txn_b.id.agent as usize].name;
                a_name.cmp(&b_name)
            })
        }
    }

    fn cmp_item_order(&self, a: Order, b: Order) -> Ordering {
        if a == b { return Ordering::Equal; }

        let txn_a = self.get_txn_containing_item(a);
        let txn_b = self.get_txn_containing_item(b);
        self.cmp_item_order2(a, txn_a, b, txn_b)
    }

    fn get_cursor_before(&self, item: Order) -> Cursor<OrderMarker, FullIndex> {
        assert_ne!(item, ROOT_ORDER);
        let marker: NonNull<NodeLeaf<OrderMarker, FullIndex>> = self.markers[item];
        unsafe { RangeTree::cursor_before_item(item, marker) }
    }

    fn get_cursor_after(&self, parent: Order) -> Cursor<OrderMarker, FullIndex> {
        if parent == ROOT_ORDER {
            self.range_tree.iter()
        } else {
            let marker: NonNull<NodeLeaf<OrderMarker, FullIndex>> = self.markers[parent];
            // self.range_tree.
            let mut cursor = unsafe {
                RangeTree::cursor_before_item(parent, marker)
            };
            // The cursor points to parent. This is safe because of guarantees provided by
            // cursor_before_item.
            cursor.offset += 1;
            cursor
        }
    }

    fn internal_apply_ops(&mut self, txn_order: Order) {
        let txn = &self.txns[txn_order];
        // Apply the operation to the marker tree & document
        // TODO: Use iter on ops instead of unrolling it here.
        let mut item_order = txn.insert_order_start;
        let next_doc_item_order = self.next_item_order();

        for op in txn.ops.iter() {
            match op {
                Op::Insert { content, mut parent } => {
                    // We need to figure out the insert position. Usually this is right after our
                    // parent, but if the parent already has children, we need to check where
                    // amongst our parents' children we fit in.
                    //
                    // The first child (if present in the document) will always be the position-wise
                    // successor to our parent.

                    // This cursor points to the desired insert location; which might contain
                    // a sibling to skip.
                    let mut marker_cursor = self.get_cursor_after(parent);
                    dbg!(&marker_cursor);
                    let mut pos = marker_cursor.count_pos();

                    // Next sibling tree is indexed by raw length (not including deletes)
                    // This is outside of the loop because we need to modify the sibling tree here.
                    let mut sibling_cursor = self.next_sibling_tree.cursor_at_offset_pos(pos.len as usize, false);

                    // Twin cursors. We walk them both forward until we find the correct insert
                    // position.

                    // let next_sibling = self.next_sibling_tree.get(pos).unwrap();

                    // If this returns None, we're inserting into the end of the document. The
                    // cursors are fine.
                    // let mut prev_sibling = None;
                    loop {
                        if let Some(mut sibling_order) = marker_cursor.get_item() {
                            // Scan siblings to find the insert position.

                            // 1. Check that the adjacent item is actually a sibling. If the parent
                            // doesn't match, this is the sibling of one of our parents and we've
                            // reached the end of our parents' children, and we can just insert
                            // here.
                            let sibling_txn = self.get_txn_containing_item(sibling_order);
                            let sibling_parent = sibling_txn.get_item_parent(sibling_order);

                            // ?? I think so...
                            assert!(sibling_parent <= parent);

                            // This is not one of our siblings. Insert here.
                            if sibling_parent != parent { break; }

                            dbg!(sibling_order, item_order);
                            let order = self.cmp_item_order2(sibling_order, sibling_txn, item_order, txn);
                            assert_ne!(order, Ordering::Equal);
                            // We go before our sibling. Insert here.
                            if order == Ordering::Less { break; }

                            // Skip to the next item.
                            // This should always exist in next_sibling_tree.
                            let next_sibling = sibling_cursor.get_item().unwrap();
                            if next_sibling == Order::MAX {
                                // The new item should be inserted at the very end of the document.
                                marker_cursor = self.range_tree.cursor_at_end();
                                sibling_cursor = self.next_sibling_tree.cursor_at_end();
                                break;
                            } else {
                                marker_cursor = self.get_cursor_before(next_sibling);
                                pos = marker_cursor.count_pos();
                                sibling_cursor = self.next_sibling_tree.cursor_at_offset_pos(pos.len as usize, false);
                            }
                        } else {
                            // We've reached the end of the document.
                            break;
                        }
                    }

                    println!("predecessor order {}", parent);

                    // Ok now we'll update the marker tree and sibling tree.

                    // let cursor_pos = cursor.count_pos();
                    dbg!(pos);

                    let inserted_len = content.chars().count();
                    let markers = &mut self.markers;
                    self.range_tree.insert(marker_cursor, OrderMarker {
                        order: item_order as u32,
                        len: inserted_len as _
                    }, |entry, leaf| {
                        DocumentState::notify(markers, entry, leaf);
                    });

                    // TODO: This is wrong.
                    self.next_sibling_tree.insert(sibling_cursor, SiblingRange {
                        len: inserted_len,
                        next_sibling: ROOT_ORDER
                    }, |_e, _l| {});

                    if USE_INNER_ROPE {
                        self.text_content.insert(pos.content as usize, content);
                        assert_eq!(self.text_content.len_chars(), self.range_tree.content_len());
                    }

                    if cfg!(debug_assertions) {
                        self.range_tree.check();
                    }

                    item_order += inserted_len;
                }
                Op::Delete { mut target, mut span } => {
                    // The span we're deleting might be split by inserts locally. Eg xxx<hi>xxx.
                    // We'll loop through deleting as much as we can each time from the document.
                    while span > 0 {
                        let cursor = self.get_cursor_before(target);
                        // dbg!(&cursor);

                        let cursor_pos = cursor.count_pos().content as usize;
                        // dbg!(cursor_pos);

                        let markers = &mut self.markers;

                        let deleted_here = self.range_tree.remote_delete(cursor, span, |entry, leaf| {
                            DocumentState::notify(markers, entry, leaf);
                        });

                        // We don't need to update the sibling tree.

                        if USE_INNER_ROPE {
                            self.text_content.remove(cursor_pos..cursor_pos + deleted_here);
                            assert_eq!(self.text_content.len_chars(), self.range_tree.content_len());
                        }

                        span -= deleted_here;
                        // This is safe because the deleted span is guaranteed to be order-contiguous.
                        target += deleted_here;
                    }
                }
            }
        }

    }

    fn handle_transaction(&mut self, txn_ext: TxnExternal) -> usize {
        // let id = self.map_external_crdt_location(&txn_ext.id);
        let id = txn_ext.id;

        if let Some(existing) = self.try_get_txn_order(id) {
            return existing;
        }

        let parents: SmallVec<[usize; 2]> = txn_ext.parents.iter().map(|p| {
            // self.get_txn_order(self.map_external_crdt_location(p))
            self.get_txn_order(*p)
        }).collect();

        // Go through the ops and count the number of inserted items
        let mut num_inserts = 0;
        let ops = txn_ext.ops.iter().map(|op_ext: &OpExternal| {
            match op_ext {
                OpExternal::Insert { content, parent } => {
                    num_inserts += content.chars().count();
                    Op::Insert {
                        content: content.clone(),
                        // parent: self.get_item_order(self.map_external_crdt_location(predecessor))
                        parent: self.get_item_order(*parent)
                    }
                }
                OpExternal::Delete { target, span } => {
                    Op::Delete {
                        target: self.get_item_order(*target),
                        span: *span
                    }
                }
            }
        }).collect();

        // TODO: Check the external item's insert_seq_start is correct.

        let order = self.txns.len();
        self.advance_frontier(order, &parents);
        // self.crdt_to_order.insert(id, order);
        self.client_data[id.agent as usize].txn_orders.push(order);

        let txn = TxnInternal {
            id,
            order, // TODO: Remove me!
            parents,
            insert_seq_start: txn_ext.insert_seq_start,
            insert_order_start: self.next_item_order(),
            num_inserts,
            dominates: 0,
            submits: 0,
            ops,
        };

        // Last because we need to access the transaction above.
        self.txns.push(txn);

        // internal_apply_ops depends on the transaction being in self.txns.
        self.internal_apply_ops(order);

        self.check();

        order
    }

    fn check(&self) {
        assert_eq!(self.range_tree.len().len, self.next_sibling_tree.len());
        if USE_INNER_ROPE {
            assert_eq!(self.text_content.len_chars(), self.range_tree.content_len());
        }
        // ... TODO: More invasive checks here. There's a *lot* of invariants we're maintaining!
    }
}


#[cfg(test)]
mod tests {
    use crate::automerge::{DocumentState, TxnExternal, OpExternal};
    use crate::common::{CRDTLocation, CRDT_DOC_ROOT};
    use inlinable_string::InlinableString;
    use smallvec::smallvec;
}

    #[test]
    fn insert_stuff() {
        let mut state = DocumentState::new();
        let agent = state.get_or_create_client_id("seph");
        state.handle_transaction(TxnExternal {
            id: CRDTLocation {
                agent,
                seq: 0
            },
            insert_seq_start: 0,
            parents: smallvec![CRDT_DOC_ROOT],
            ops: smallvec![OpExternal::Insert {
                content: InlinableString::from("oh hai"),
                parent: CRDT_DOC_ROOT
            }]
        });

        state.handle_transaction(TxnExternal {
            id: CRDTLocation {
                agent,
                seq: 1
            },
            insert_seq_start: 5,
            parents: smallvec![CRDTLocation {
                agent,
                seq: 0
            }],
            ops: smallvec![OpExternal::Insert {
                content: InlinableString::from("yooo"),
                parent: CRDTLocation {
                    agent: 0,
                    seq: 5
                }
            }]
        });
        state.handle_transaction(TxnExternal {
            id: CRDTLocation {
                agent,
                seq: 2
            },
            insert_seq_start: 9,
            parents: smallvec![CRDTLocation {
                agent,
                seq: 1
            }],
            ops: smallvec![OpExternal::Delete {
                target: CRDTLocation {
                    agent: 0,
                    seq: 3,
                },
                span: 3
            }]
        });

        dbg!(state);
    }

    #[test]
    fn concurrent_writes() {
        let seph = TxnExternal {
            id: CRDTLocation {
                agent: 0,
                seq: 0
            },
            insert_seq_start: 0,
            parents: smallvec![CRDT_DOC_ROOT],
            ops: smallvec![OpExternal::Insert {
                content: InlinableString::from("hi from seph"),
                parent: CRDT_DOC_ROOT
            }]
        };

        let mike = TxnExternal {
            id: CRDTLocation {
                agent: 1,
                seq: 0
            },
            insert_seq_start: 0,
            parents: smallvec![CRDT_DOC_ROOT],
            ops: smallvec![OpExternal::Insert {
                content: InlinableString::from("hi from mike"),
                parent: CRDT_DOC_ROOT
            }]
        };

        let mut state1 = DocumentState::new();
        let agent0 = state1.get_or_create_client_id("seph");
        let agent1 = state1.get_or_create_client_id("mike");

        state1.handle_transaction(seph);
        state1.handle_transaction(mike);

        // What happens !?
        dbg!(state1);
    }