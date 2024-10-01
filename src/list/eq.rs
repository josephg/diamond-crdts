// This file contains an implementation of Eq / PartialEq for OpLog. The implementation is quite
// complex because:
//
// - Operation logs don't have a canonical ordering (because of bubbles)
// - Internal agent IDs are arbitrary.
//
// This implementation of Eq is mostly designed to help fuzz testing. It is not optimized for
// performance.

use rle::{HasLength, SplitableSpan};
use rle::zip::rle_zip3;
use crate::{AgentId, Frontier, LV};
use crate::list::ListOpLog;
use crate::frontier::sort_frontier;
use crate::causalgraph::graph::GraphEntrySimple;
use crate::rle::KVPair;

const VERBOSE: bool = true;
// const VERBOSE: bool = false;

impl PartialEq<Self> for ListOpLog {
    fn eq(&self, other: &Self) -> bool {
        if self.doc_id != other.doc_id { return false; }

        // This implementation is based on the equivalent version in the original diamond types
        // implementation.

        // Fields to check:
        // - [x] client_with_localtime, client_data,
        // - [x] operations (+ ins_content / del_content)
        // - [x] history
        // - [x] frontier

        // This check isn't sufficient. We'll check the frontier entries more thoroughly below.
        if self.cg.version.len() != other.cg.version.len() { return false; }

        // [self.agent] => other.agent.
        let mut agent_a_to_b = Vec::new();
        for c in self.cg.agent_assignment.client_data.iter() {
            // If there's no corresponding client in other (and the agent is actually in use), the
            // oplogs don't match.
            let other_agent = if let Some(other_agent) = other.get_agent_id(c.name) {
                if other.cg.agent_assignment.client_data[other_agent as usize].get_next_seq() != c.get_next_seq() {
                    // Make sure we have exactly the same number of edits for each agent.
                    return false;
                }

                other_agent
            } else {
                #[allow(clippy::collapsible_else_if)]
                if c.is_empty() {
                    AgentId::MAX // Just using this as a placeholder. Could use None but its awkward.
                } else {
                    // Agent missing.
                    if VERBOSE {
                        println!("Oplog does not match because agent ID is missing");
                    }
                    return false;
                }
            };
            agent_a_to_b.push(other_agent);
        }

        let map_lv_to_other = |t: LV| -> Option<LV> {
            let mut av = self.lv_to_agent_version(t);
            av.0 = agent_a_to_b[av.0 as usize];
            other.try_crdt_id_to_time(av)
        };

        // Check frontier contents. Note this is O(n^2) with the size of the respective frontiers.
        // Which should be fine in normal use, but its a DDOS risk.
        for t in self.cg.version.iter() {
            let other_time = map_lv_to_other(*t);
            if let Some(other_time) = other_time {
                if !other.cg.version.0.contains(&other_time) {
                    if VERBOSE { println!("Frontier is not contained by other frontier"); }
                    return false;
                }
            } else {
                // The time is unknown.
                if VERBOSE { println!("Frontier is not known in other doc"); }
                return false;
            }
        }

        // The core strategy here is we'll iterate through our local operations and make sure they
        // each have a corresponding operation in other. Because self.len == other.len, this will be
        // sufficient.

        // The other approach here would be to go through each agent in self.clients and scan the
        // corresponding changes in other.

        // Note this should be optimized if its going to be used for more than fuzz testing.
        // But this is pretty neat!
        for (mut op, mut txn, mut crdt_id) in rle_zip3(
            self.iter_ops(),
            self.iter_history(),
            self.cg.agent_assignment.client_with_lv.iter().map(|pair| pair.1)
        ) {

            // println!("op {:?} txn {:?} crdt {:?}", op, txn, crdt_id);

            // Unfortunately the operation range we found might be split up in other. We'll loop
            // grabbing as much of it as we can at a time.
            loop {
                // Look up the corresponding operation in other.

                // This maps via agents - so I think that sort of implicitly checks out.
                let Some(other_time) = map_lv_to_other(txn.span.start) else {
                    return false;
                };

                // Lets take a look at the operation.
                let (KVPair(_, other_op_int), offset) = other.operations.find_packed_with_offset(other_time);

                let mut other_op = other_op_int.to_operation(&other.operation_ctx);
                if offset > 0 { other_op.truncate_keeping_right(offset); }

                // Although op is contiguous, and all in a run from the same agent, the same isn't
                // necessarily true of other_op! The max length we can consume here is limited by
                // other_op's size in agent assignments.
                let (run, offset) = other.cg.agent_assignment.client_with_lv.find_packed_with_offset(other_time);
                let mut other_id = run.1;
                if offset > 0 { other_id.truncate_keeping_right(offset); }

                if agent_a_to_b[crdt_id.agent as usize] != other_id.agent {
                    if VERBOSE { println!("Ops do not match because agents differ"); }
                    return false;
                }
                if crdt_id.seq_range.start != other_id.seq_range.start {
                    if VERBOSE { println!("Ops do not match because CRDT sequence numbers differ"); }
                    return false;
                }

                let len_here = usize::min(other_op.len(),
                                          usize::min(op.len(),
                                                     usize::min(crdt_id.len(), other_id.len())));
                if other_op.len() > len_here {
                    other_op.truncate(len_here);
                }

                let remainder = if op.len() > len_here {
                    Some(op.truncate(len_here))
                } else { None };

                if op != other_op {
                    if VERBOSE { println!("Ops do not match at {}:\n{:?}\n{:?}", txn.span.start, op, other_op); }
                    return false;
                }

                // Ok, and we also need to check the txns match.
                let (other_txn_entry, offset) = other.cg.graph.entries.find_packed_with_offset(other_time);
                let mut other_txn: GraphEntrySimple = other_txn_entry.clone().into();
                if offset > 0 { other_txn.truncate_keeping_right(offset); }
                if other_txn.len() > len_here {
                    other_txn.truncate(len_here);
                }

                // We can't just compare txns because the parents need to be mapped!
                let Some(mapped_start) = map_lv_to_other(txn.span.start) else {
                    panic!("I think this should be unreachable, since we check the agent / seq matches above.");
                    // return false;
                };

                let mut mapped_txn = GraphEntrySimple {
                    span: (mapped_start..mapped_start + len_here).into(),
                    // .unwrap() should be safe here because we've already walked past this item's
                    // parents.
                    parents: Frontier(txn.parents.iter().map(|t| map_lv_to_other(*t).unwrap()).collect())
                };
                // mapped_txn.parents.sort_unstable();
                sort_frontier(&mut mapped_txn.parents.0);

                if other_txn != mapped_txn {
                    if VERBOSE { println!("Txns do not match {:?} (was {:?}) != {:?}", mapped_txn, txn, other_txn); }
                    return false;
                }

                if let Some(rem) = remainder {
                    op = rem;
                } else { break; }
                crdt_id.seq_range.start += len_here;
                txn.truncate_keeping_right(len_here);
            }
        }

        true
    }
}

impl Eq for ListOpLog {}


#[cfg(test)]
mod test {
    use crate::list::ListOpLog;

    fn is_eq(a: &ListOpLog, b: &ListOpLog) -> bool {
        let a_eq_b = a.eq(b);
        let b_eq_a = b.eq(a);
        if a_eq_b != b_eq_a { dbg!(a_eq_b, b_eq_a); }
        assert_eq!(a_eq_b, b_eq_a);
        a_eq_b
    }

    #[test]
    fn eq_smoke_test() {
        let mut a = ListOpLog::new();
        assert!(is_eq(&a, &a));
        a.get_or_create_agent_id_from_str("seph");
        a.get_or_create_agent_id_from_str("mike");
        a.add_insert_at(0, &[], 0, "Aa");
        a.add_insert_at(1, &[], 0, "b");
        a.add_delete_at(0, &[1, 2], 0..2);

        // Same history, different order.
        let mut b = ListOpLog::new();
        b.get_or_create_agent_id_from_str("mike");
        b.get_or_create_agent_id_from_str("seph");
        b.add_insert_at(0, &[], 0, "b");
        b.add_insert_at(1, &[], 0, "Aa");
        b.add_delete_at(1, &[0, 2], 0..2);

        assert!(is_eq(&a, &b));

        // And now with the edits interleaved
        let mut c = ListOpLog::new();
        c.get_or_create_agent_id_from_str("seph");
        c.get_or_create_agent_id_from_str("mike");
        c.add_insert_at(0, &[], 0, "A");
        c.add_insert_at(1, &[], 0, "b");
        c.add_insert_at(0, &[0], 1, "a");
        c.add_delete_at(0, &[1, 2], 0..2);

        assert!(is_eq(&a, &c));
        assert!(is_eq(&b, &c));
    }
}