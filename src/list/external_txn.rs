use smartstring::alias::{String as SmartString};
use smallvec::{SmallVec, smallvec};
use crate::list::{ListCRDT, Order, ROOT_ORDER};
use crate::order::OrderSpan;
use std::collections::BinaryHeap;
use std::cmp::{Ordering, Reverse};
use crate::rle::{Rle, KVPair};
use crate::common::{AgentId, CRDT_DOC_ROOT, CRDTLocation};
use crate::splitable_span::SplitableSpan;
// use crate::LocalOp;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteId {
    pub agent: SmartString,
    pub seq: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RemoteOp {
    Ins {
        origin_left: RemoteId,
        origin_right: RemoteId,
        // ins_content: SmartString, // ?? Or just length?
        len: u32,
    },

    Del {
        id: RemoteId,
        len: u32,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RemoteTxn {
    pub id: RemoteId,
    pub parents: SmallVec<[RemoteId; 2]>, // usually 1 entry
    pub ops: SmallVec<[RemoteOp; 2]>, // usually 1-2 entries.

    // pub ins_content: SmartString;
}

// #[derive(Clone, Debug, Eq, PartialEq)]
// pub struct BraidTxn {
//     pub id: RemoteId,
//     pub parents: SmallVec<[RemoteId; 2]>, // usually 1 entry
//     pub ops: SmallVec<[LocalOp; 2]> // usually 1-2 entries.
// }

// thread_local! {
// const REMOTE_ROOT: RemoteId = RemoteId {
//     agent: "ROOT".into(),
//     seq: u32::MAX
// };
// }

/// A vector clock names the *next* expected sequence number for each client in the document.
/// Any entry missing from a vector clock is implicitly 0 - which is to say, the next expected
/// sequence number is 0.
type VectorClock = Vec<RemoteId>;

impl ListCRDT {
    pub(crate) fn remote_id_to_order(&self, id: &RemoteId) -> Order {
        let agent = self.get_agent_id(id.agent.as_str()).unwrap();
        if agent == AgentId::MAX { ROOT_ORDER }
        else { self.client_data[agent as usize].seq_to_order(id.seq) }
    }

    fn crdt_loc_to_remote_id(&self, loc: CRDTLocation) -> RemoteId {
        RemoteId {
            agent: if loc.agent == CRDT_DOC_ROOT.agent {
                "ROOT".into()
            } else {
                self.client_data[loc.agent as usize].name.clone()
            },
            seq: loc.seq
        }
    }

    pub(crate) fn order_to_remote_id(&self, order: Order) -> RemoteId {
        let crdt_loc = self.get_crdt_location(order);
        self.crdt_loc_to_remote_id(crdt_loc)
    }

    pub(crate) fn order_to_remote_id_span(&self, order: Order, max_size: u32) -> (RemoteId, u32) {
        let crdt_span = self.get_crdt_span(order, max_size);
        (self.crdt_loc_to_remote_id(crdt_span.loc), crdt_span.len)
    }

    pub fn get_vector_clock(&self) -> VectorClock {
        self.client_data.iter()
            .filter(|c| !c.item_orders.is_empty())
            .map(|c| {
                RemoteId {
                    agent: c.name.clone(),
                    seq: c.item_orders.last().unwrap().end()
                }
            })
            .collect()
    }

    // -> SmallVec<[OrderSpan; 4]>
    /// This method returns the list of spans of orders which will bring a client up to date
    /// from the specified vector clock version.
    pub fn get_versions_since(&self, vv: &VectorClock) -> Rle<OrderSpan> {
        #[derive(Clone, Copy, Debug, Eq)]
        struct OpSpan {
            agent_id: usize,
            next_order: Order,
            idx: usize,
        }

        impl PartialEq for OpSpan {
            fn eq(&self, other: &Self) -> bool {
                self.next_order == other.next_order
            }
        }

        impl PartialOrd for OpSpan {
            fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
                self.next_order.partial_cmp(&other.next_order)
            }
        }

        impl Ord for OpSpan {
            fn cmp(&self, other: &Self) -> Ordering {
                self.next_order.cmp(&other.next_order)
            }
        }

        let mut heap = BinaryHeap::new();
        // We need to go through all clients in the local document because we need to include
        // all entries for any client which *isn't* named in the vector clock.
        for (agent_id, client) in self.client_data.iter().enumerate() {
            let from_seq = vv.iter()
                .find(|rid| rid.agent == client.name)
                .map_or(0, |rid| rid.seq);

            let idx = client.item_orders.search(from_seq).unwrap_or_else(|idx| idx);
            if idx < client.item_orders.0.len() {
                let entry = &client.item_orders.0[idx];

                heap.push(Reverse(OpSpan {
                    agent_id,
                    next_order: entry.1.order + from_seq.saturating_sub(entry.0),
                    idx,
                }));
            }
        }

        let mut result = Rle::new();

        while let Some(Reverse(e)) = heap.pop() {
            // Append a span of orders from here and requeue.
            let c = &self.client_data[e.agent_id];
            let KVPair(_, span) = c.item_orders.0[e.idx];
            result.append(OrderSpan {
                // Kinda gross but at least its branchless.
                order: span.order.max(e.next_order),
                len: span.len - (e.next_order - span.order),
            });

            // And potentially requeue this agent.
            if e.idx + 1 < c.item_orders.0.len() {
                heap.push(Reverse(OpSpan {
                    agent_id: e.agent_id,
                    next_order: c.item_orders.0[e.idx + 1].1.order,
                    idx: e.idx + 1,
                }));
            }
        }

        result
    }

    /// This function is used to build an iterator for converting internal txns to remote
    /// transactions.
    pub fn next_remote_txn_from_order(&self, span: OrderSpan) -> (RemoteTxn, u32) {
        let (txn, offset) = self.txns.find(span.order).unwrap();

        let parents = if let Some(order) = txn.parent_at_offset(offset as _) {
            smallvec![self.order_to_remote_id(order)]
        } else {
            txn.parents.iter().map(|order| self.order_to_remote_id(*order))
                .collect()
        };

        let len = u32::min(span.len, txn.len - offset);
        assert!(len > 0);

        let mut ops = SmallVec::new();
        let mut order = span.order;
        let mut len_remaining = len;
        while len_remaining > 0 {
            // Look up the change at order and append a span with maximum size len_remaining.
            // dbg!(order, len_remaining);

            // Each entry has its length limited by 4 things:
            // - the requested span length (span.len)
            // - The length of this txn entry (related to contiguous user edits)
            // - The length of the delete or insert operation
            // - For deletes, the contiguous section of items deleted which have the same agent id
            if let Some((d, offset)) = self.deletes.find(order) {
                // Its a delete.

                let len_limit_2 = u32::min(d.1.len - offset, len_remaining);
                let (id, len) = self.order_to_remote_id_span(d.1.order + offset, len_limit_2);
                ops.push(RemoteOp::Del { id, len });
                len_remaining -= len;
                order += len;
            } else {
                // It must be an insert. Fish information out of the range tree.
                let cursor = self.get_cursor_before(order);
                let entry = cursor.get_entry();
                let len = u32::min((entry.len() - cursor.offset) as u32, len_remaining);
                // We need to fetch the inserted CRDT span ID to limit the length.
                // let len = self.get_crdt_span(entry.order + cursor.offset as u32, len_limit_2).len;
                ops.push(RemoteOp::Ins {
                    origin_left: self.order_to_remote_id(entry.origin_left_at_offset(cursor.offset as u32)),
                    origin_right: self.order_to_remote_id(entry.origin_right),
                    len
                });
                len_remaining -= len;
                order += len;
            }
        }

        (RemoteTxn {
            id: self.order_to_remote_id(span.order),
            parents,
            ops,
        }, len)
    }

    // This isn't the final form of this, but its good enough for now.
    pub(crate) fn copy_txn_range_into(&self, dest: &mut Self, mut span: OrderSpan) {
        while span.len > 0 {
            let (txn, len) = self.next_remote_txn_from_order(span);
            // dbg!(&txn, len);
            debug_assert!(len > 0);
            debug_assert!(len <= span.len);
            span.consume_start(len);
            dest.apply_remote_txn(&txn);
        }
    }

    pub fn replicate_into(&self, dest: &mut Self) {
        let clock = dest.get_vector_clock();
        let order_ranges = self.get_versions_since(&clock);
        for span in order_ranges.iter() {
            self.copy_txn_range_into(dest, *span);
        }
    }
}


#[cfg(test)]
mod tests {
    use crate::list::ListCRDT;
    use crate::list::external_txn::{RemoteId, VectorClock};
    use crate::order::OrderSpan;

    #[test]
    fn version_vector() {
        let mut doc = ListCRDT::new();
        assert_eq!(doc.get_vector_clock(), vec![]);
        doc.get_or_create_agent_id("seph"); // 0
        assert_eq!(doc.get_vector_clock(), vec![]);
        doc.local_insert(0, 0, "hi".into());
        assert_eq!(doc.get_vector_clock(), vec![
            RemoteId {
                agent: "seph".into(),
                seq: 2
            }
        ]);
    }

    #[test]
    fn test_versions_since() {
        let mut doc = ListCRDT::new();
        doc.get_or_create_agent_id("seph"); // 0
        doc.local_insert(0, 0, "hi".into());
        doc.get_or_create_agent_id("mike"); // 0
        doc.local_insert(1, 2, "yo".into());
        doc.local_insert(0, 4, "a".into());

        // When passed an empty vector clock, we fetch all versions from the start.
        let vs = doc.get_versions_since(&VectorClock::new());
        assert_eq!(vs.0, vec![OrderSpan { order: 0, len: 5 }]);

        let vs = doc.get_versions_since(&vec![RemoteId {
            agent: "seph".into(),
            seq: 2
        }]);
        assert_eq!(vs.0, vec![OrderSpan { order: 2, len: 3 }]);

        let vs = doc.get_versions_since(&vec![RemoteId {
            agent: "seph".into(),
            seq: 100
        }, RemoteId {
            agent: "mike".into(),
            seq: 100
        }]);
        assert_eq!(vs.0, vec![]);
    }

    #[test]
    fn external_txns() {
        let mut doc = ListCRDT::new();
        doc.get_or_create_agent_id("seph"); // 0
        doc.local_insert(0, 0, "hi".into());
        doc.local_delete(0, 0, 2);

        // dbg!(&doc);
        dbg!(doc.next_remote_txn_from_order(OrderSpan { order: 0, len: 40 }));
    }
}