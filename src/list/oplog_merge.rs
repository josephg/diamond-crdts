use std::collections::BinaryHeap;
use smallvec::SmallVec;
use rle::{AppendRle, HasLength};
use crate::list::ListOpLog;
use crate::dtrange::DTRange;
use crate::rle::KVPair;
use crate::{AgentId, CausalGraph};
use crate::causalgraph::graph::GraphEntrySimple;

impl CausalGraph {
    /// Find all the items to merge from other into self.
    fn to_merge(&self, other: &Self, agent_map: &[AgentId]) -> SmallVec<DTRange, 4> {
        // This method is in many ways a baby version of diff_slow, with some changes:
        // - We only look at the frontier. (This is not configurable - but it could be)
        // - It maps spans from other -> self
        // - Rather than having OnlyA / OnlyB items which mutually annihilate each other, this
        //   method simply discards items as soon as we find them in self.

        // Much like diff(), this method could be optimized easily by checking for some common
        // cases. I'm not sure how important is is though, since I doubt this method will be used
        // much.

        let mut queue = BinaryHeap::new();
        // dbg!(&other.frontier, &other.history);
        for ord in other.version.iter() {
            queue.push(*ord);
        }

        let mut result = SmallVec::new();

        while let Some(mut ord) = queue.pop() {
            // let mut ord = ord;
            // dbg!(ord, &queue);

            // Cases:
            // - ord is within self. In that case, discard it.
            // - ord not within self. Find the longest run we can - constrained by other txn and
            //  (agent,seq) pairs. If we find something we know, add to result and end. If not,
            //  add parents to queue.
            let containing_txn = other.graph.entries.find_packed(ord);

            // Discard any other entries from queue which name the same txn

            while let Some(peek_ord) = queue.peek() {
                let peek_ord = peek_ord;
                if *peek_ord >= containing_txn.span.start {
                    queue.pop();
                } else {
                    break;
                }
            }

            loop { // Add as much as we can from this txn.
                let (other_span, offset) = other.agent_assignment.client_with_lv.find_packed_with_offset(ord);
                let self_agent = agent_map[other_span.1.agent as usize];
                let seq = other_span.1.seq_range.start + offset;

                // Find out how many items we can eat
                let (r, offset) = self.agent_assignment.client_data[self_agent as usize]
                    .lv_for_seq.find_sparse(seq);
                if r.is_ok() {
                    // Overlap here. Discard from the queue.
                    break;
                }

                let id_start = ord - offset;
                if containing_txn.span.start >= id_start {
                    // We can take everything from the start of the txn.
                    result.push_reversed_rle((containing_txn.span.start..ord + 1).into());

                    // And push parents.
                    for p in containing_txn.parents.iter() {
                        queue.push(*p);
                    }

                    break;
                } else {
                    // Take back to id_start and iterate.
                    result.push_reversed_rle((id_start..ord + 1).into());
                    ord = id_start - 1;
                }
            }
        }

        result
    }
}

impl ListOpLog {
    /// Add all missing operations from the other oplog into this oplog. This method is mostly used
    /// by testing code, since you rarely have two local oplogs to merge together.
    pub fn add_missing_operations_from(&mut self, other: &Self) {
        // [other.agent] => self.agent
        let mut agent_map = Vec::with_capacity(other.cg.agent_assignment.client_data.len());

        // TODO: Construct this lazily.
        for c in other.cg.agent_assignment.client_data.iter() {
            let self_agent = self.get_or_create_agent_id(c.name);
            agent_map.push(self_agent);
        }

        // So we need to figure out which changes in other *aren't* in self. To do that, I'll walk
        // backwards through other, looking for changes which are missing in self.

        let spans = self.cg.to_merge(&other.cg, &agent_map);
        // dbg!(&spans);

        let mut time = self.len();
        for &s in spans.iter().rev() {
            // Operations
            let mut t = time;
            for (KVPair(_, op), content) in other.iter_range_simple(s) {
                // Operations don't need to be mapped at all.
                // dbg!(&op, content);
                self.push_op_internal(t, op.loc, op.kind, content);
                t += op.len();
            }

            // Agent assignments
            t = time;
            for mut span in other.iter_agent_mappings_range(s) {
                // Map other agent ID -> self agent IDs.
                span.agent = agent_map[span.agent as usize];
                self.assign_time_to_crdt_span(t, span);
                t += span.len();
            }

            // History entries (parents)
            t = time;
            for mut hist_entry in other.cg.graph.entries
                .iter_range_map(s, |e| GraphEntrySimple::from(e)) {

                let len = hist_entry.len();
                let span = (t..t + len).into();
                // We need to convert other parents to self parents. This is a bit gross but eh.
                // dbg!(&hist_entry.parents);
                for t in hist_entry.parents.0.iter_mut() {
                    let mut av = other.lv_to_agent_version(*t);
                    av.0 = agent_map[av.0 as usize];
                    let self_time = self.crdt_id_to_time(av);
                    *t = self_time;
                }

                hist_entry.parents.0.sort_unstable();
                // hist_entry.parents.sort_unstable_by(|a, b| a.cmp(b));
                hist_entry.parents.debug_check_sorted();
                // dbg!(&hist_entry.parents);

                self.cg.graph.push(hist_entry.parents.as_ref(), span);
                self.cg.version.advance_by_known_run(hist_entry.parents.as_ref(), span);
                t += len;
            }

            time += s.len();
        }
    }
}

#[cfg(test)]
mod test {
    use crate::list::ListOpLog;

    fn merge_into_and_check(dest: &mut ListOpLog, src: &ListOpLog) {
        // dbg!(&dest);
        dest.add_missing_operations_from(&src);
        dest.dbg_check(true);
        // dbg!(&dest);
        assert_eq!(dest, src);
    }

    fn merge_both_and_check(a: &mut ListOpLog, b: &mut ListOpLog) {
        // dbg!(&dest);
        a.add_missing_operations_from(&b);
        // dbg!(&a);
        a.dbg_check(true);

        b.add_missing_operations_from(&a);
        b.dbg_check(true);
        // dbg!(&dest);

        dbg!(&a, &b);
        assert_eq!(a, b);
    }

    #[test]
    fn smoke() {
        let mut a = ListOpLog::new();
        let mut b = ListOpLog::new();
        assert_eq!(a, b);
        // merge_and_check(&mut a, &b);

        a.get_or_create_agent_id_from_str("seph");
        a.add_insert(0, 0, "hi");
        merge_into_and_check(&mut b, &a);

        // Ok now we'll append data to both oplogs
        a.add_insert(0, 0, "aaa");
        b.get_or_create_agent_id_from_str("mike");
        b.add_delete_without_content(1, 0..2);

        merge_both_and_check(&mut a, &mut b);
    }
}