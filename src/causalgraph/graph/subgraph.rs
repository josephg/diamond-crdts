use std::collections::BinaryHeap;
use smallvec::{SmallVec, smallvec};
use rle::{MergeableIterator, MergeIter};
use crate::causalgraph::graph::{Graph, GraphEntryInternal};
use crate::{DTRange, Frontier, LV};
use crate::rle::RleVec;

fn push_light_dedup(f: &mut Frontier, new_item: LV) {
    if f.0.last() != Some(&new_item) {
        f.0.push(new_item);
    }
}

struct Filter<I: Iterator<Item = DTRange>> {
    iter: MergeIter<I, false>,
    current: Option<DTRange>, // Could use (usize::MAX, usize::MAX) or something for None but its gross.
}

impl<I: Iterator<Item = DTRange>> Filter<I> {
    fn new(iter: I) -> Self {
        let mut iter = iter.merge_spans_rev();
        let first = iter.next();
        Self {
            iter,
            current: first,
            // current: (usize::MAX, usize::MAX).into() // A bit dirty using this but eh.
        }
    }

    fn scan_until_start_below(&mut self, v: LV) -> Option<DTRange> {
        while self.current.map_or(false, |c| c.start > v) {
            self.current = self.iter.next();
        }
        self.current
    }
}

impl Graph {
    pub fn subgraph(&self, filter: &[DTRange], parents: &[LV]) -> (Graph, Frontier) {
        let filter_iter = filter.iter().copied().rev();
        self.subgraph_raw(filter_iter, parents)
    }

    // pub fn subgraph_in(&self, cg:

    // The filter iterator must be reverse-sorted.
    pub(crate) fn subgraph_raw<I: Iterator<Item=DTRange>>(&self, rev_filter_iter: I, parents: &[LV]) -> (Graph, Frontier) {
        // This algorithm iterates backwards through the causal graph looking for regions which
        // overlap the passed rev_filter_iter.

        // The traversal is similar to other graph traversal methods in graph/tools.rs. We walk the
        // graph with a priority queue of pending items which are in the subtree under the passed
        // parents object.

        #[derive(PartialOrd, Ord, Eq, PartialEq, Clone, Debug)]
        struct QueueEntry {
            target_parent: LV,
            child_indexes: SmallVec<usize, 2>,
            entry_in_frontier: bool,
        }

        let mut result_rev = Vec::<GraphEntryInternal>::new();
        let mut queue: BinaryHeap<QueueEntry> = parents.iter().map(|p| {
            QueueEntry {
                target_parent: *p,
                child_indexes: smallvec![],
                entry_in_frontier: true,
            }
        }).collect();
        let mut filtered_frontier = Frontier::default();

        fn update_parents(result_rev: &mut [GraphEntryInternal], frontier: &mut Frontier, entry: &QueueEntry, p: LV) {
            if entry.entry_in_frontier {
                push_light_dedup(frontier, p);
            }
            for idx in &entry.child_indexes {
                push_light_dedup(&mut result_rev[*idx].parents, p);
            }
        }

        let mut filter_iter = Filter::new(rev_filter_iter);

        'outer: while let Some(mut entry) = queue.pop() {
            // There's essentially 2 cases here:
            // 1. The entry is either inside a filtered item, or an earlier item in this txn
            //    is allowed by the filter.
            // 2. The filter doesn't allow the txn the entry is inside.

            let txn = self.entries.find_packed(entry.target_parent);

            while let Some(filter) = filter_iter.scan_until_start_below(entry.target_parent) {
                if filter.end <= txn.span.start {
                    // The txn (or the remainder of the txn) is not included by the filter.
                    break;
                }
                // dbg!(&filter);

                // We know at this point that the filter includes the target_parent.
                debug_assert!(txn.span.start < filter.end);
                debug_assert!(entry.target_parent >= filter.start);
                debug_assert!(entry.target_parent >= txn.span.start);

                // Case 1. We'll add a new parents entry this loop iteration.

                let p = entry.target_parent.min(filter.end - 1);

                update_parents(&mut result_rev, &mut filtered_frontier, &entry, p);

                let base = filter.start.max(txn.span.start);
                let mut child_indexes: SmallVec<usize, 2> = entry.child_indexes.clone();

                // For simplicity, pull out anything that is within this txn *and* this filter.
                while let Some(peeked_entry) = queue.peek() {
                    if peeked_entry.target_parent < base { break; }

                    let peeked_target = peeked_entry.target_parent.min(filter.end - 1);
                    update_parents(&mut result_rev, &mut filtered_frontier, &peeked_entry, peeked_target);
                    // iterations += 1;

                    for i in peeked_entry.child_indexes.iter() {
                        if !child_indexes.contains(i) {
                            child_indexes.push(*i);
                        }
                    }

                    queue.pop();
                }

                let idx_here = result_rev.len();
                result_rev.push(GraphEntryInternal {
                    span: (base..p + 1).into(),
                    shadow: txn.shadow, // This is pessimistic.
                    parents: Frontier::default(), // Parents current unknown!
                    // child_indexes: smallvec![],
                    child_indexes,
                });

                if filter.start > txn.span.start {
                    // The item we've just added has an (implicit) parent of base-1. We'll
                    // update entry and loop - which might either find more filter items
                    // within this txn, or it might bump us to the case below where the txn's
                    // items are added.
                    entry = QueueEntry {
                        target_parent: filter.start - 1,
                        child_indexes: smallvec![idx_here],
                        entry_in_frontier: false,
                    };
                } else { // filter.start <= txn.span.start.
                    // The rest of this txn is included in the filter. Copy it all in, push the
                    // parents and continue the outer loop.
                    if !txn.parents.is_empty() {
                        for p in txn.parents.iter() {
                            queue.push(QueueEntry {
                                target_parent: *p,
                                child_indexes: smallvec![idx_here],
                                entry_in_frontier: false,
                            })
                        }
                    }
                    continue 'outer;
                }
            }

            // If we're at the end of the filter, nothing else in the queue matters.
            if filter_iter.current.is_none() { break; }

            // Case 2. The remainder of this txn is filtered out.
            //
            // We'll create new queue entries for all of this txn's parents.
            let mut child_idxs = entry.child_indexes;
            let mut in_frontier = entry.entry_in_frontier;

            while let Some(peeked_entry) = queue.peek() {
                if peeked_entry.target_parent < txn.span.start { break; } // Next item is out of this txn.

                for i in peeked_entry.child_indexes.iter() {
                    if !child_idxs.contains(i) { child_idxs.push(*i); }
                }
                in_frontier |= peeked_entry.entry_in_frontier;
                // iterations += 1;

                queue.pop();
            }

            if txn.parents.0.len() == 1 {
                // A silly little optimization to avoid an unnecessary clone() below.
                queue.push(QueueEntry {
                    target_parent: txn.parents.0[0],
                    child_indexes: child_idxs,
                    entry_in_frontier: in_frontier,
                })
            } else {
                for p in txn.parents.iter() {
                    queue.push(QueueEntry {
                        target_parent: *p,
                        child_indexes: child_idxs.clone(),
                        entry_in_frontier: in_frontier,
                    })
                }
            }
        }

        result_rev.reverse();

        fn clean_frontier(graph: &Graph, f: &mut Frontier) {
            if f.len() >= 2 {
                f.0.reverse(); // Parents will always end up in reverse order.
                // I wish I didn't need to do this. At least I don't think it'll show up on the
                // performance profile.
                *f = graph.find_dominators(f.as_ref());
            }
        }

        // I'm collecting the root child indexes here instead of in the loop above because that lets
        // us break early in the above loop when the filter runs dry. And its pretty cheap to
        // collect the root_child_indexes here anyway, since we need to scan them in any case.
        let mut root_child_indexes = smallvec![];
        let list_last = result_rev.len();

        for (idx, e) in result_rev.iter_mut().enumerate() {
            clean_frontier(self, &mut e.parents);
            for idx in e.child_indexes.iter_mut() {
                *idx = list_last - *idx - 1;
            }
            e.child_indexes.reverse();
            if e.parents.is_empty() {
                root_child_indexes.push(idx);
            }
        }
        clean_frontier(self, &mut filtered_frontier);

        (Graph {
            entries: RleVec(result_rev),
            root_child_indexes,
        }, filtered_frontier)
    }
    
    /// Project a frontier in the local graph (self) onto some other_graph
    pub fn project_onto_other_graph(&self, frontier: &[LV], other_graph: &Graph) -> Frontier {
        let filter_iter = other_graph.entries.iter()
            .map(|e| e.span)
            .rev();
        self.project_onto_subgraph_raw(filter_iter, frontier)
    }

    pub fn project_onto_subgraph(&self, filter: &[DTRange], frontier: &[LV]) -> Frontier {
        let filter_iter = filter.iter().copied().rev();
        self.project_onto_subgraph_raw(filter_iter, frontier)
    }

    // TODO: Another way I could write this method would be to pass in the subgraph's frontier. Maybe better??
    pub(crate) fn project_onto_subgraph_raw<I: Iterator<Item=DTRange>>(&self, rev_filter_iter: I, frontier: &[LV]) -> Frontier {
        if frontier.is_empty() { return Frontier::root(); }

        let mut queue: BinaryHeap<usize> = BinaryHeap::new();
        let mut result = Frontier::default();

        fn dec(v_enc: usize) -> (bool, LV) {
            (v_enc % 2 == 1, v_enc >> 1)
        }
        fn enc(active: bool, v: LV) -> usize {
            (v << 1) + (active as usize)
        }

        for v in frontier {
            queue.push(enc(true, *v));
        }
        let mut num_active_entries = frontier.len();

        let mut filter_iter = Filter::new(rev_filter_iter);

        while let Some(vv) = queue.pop() {
            let (mut mark_active, v) = dec(vv);
            if mark_active { num_active_entries -= 1; }

            let txn = self.entries.find_packed(v);

            let Some(filter) = filter_iter.scan_until_start_below(v) else { break; };

            debug_assert!(v >= filter.start);
            debug_assert!(v >= txn.span.start);

            let mark_v = v.min(filter.end - 1);

            while let Some(peek_vv) = queue.peek() {
                let (peek_active, peek_v) = dec(*peek_vv);
                if peek_v >= txn.span.start {
                    if peek_v >= mark_v && !peek_active {
                        // Anything under the marked version is irrelevant.
                        mark_active = false;
                    }
                    if peek_active { num_active_entries -= 1; }
                    // Regardless, throw away anything else within this txn.
                    queue.pop();
                } else { break; }
            }

            if filter.end > txn.span.start && mark_active {
                debug_assert!(txn.span.start <= mark_v);

                result.0.push(mark_v);
                mark_active = false;
            }

            if mark_active {
                num_active_entries += txn.parents.len();
            } else if num_active_entries == 0 { break; }

            for p in txn.parents.iter() {
                queue.push(enc(mark_active, *p));
            }
        }

        result.0.reverse();
        result
    }
}

#[cfg(test)]
mod test {
    use std::ops::Range;
    use smallvec::smallvec;
    use rle::intersect::{rle_intersect, rle_intersect_first};
    use rle::MergeableIterator;
    use crate::causalgraph::graph::Graph;
    use crate::{DTRange, Frontier, LV};
    use crate::causalgraph::graph::tools::test::fancy_graph;
    use crate::rle::RleVec;

    fn check_subgraph(g: &Graph, filter: &[Range<usize>], frontier: &[LV], expect_parents: &[&[LV]], expect_frontier: &[LV]) {
        let filter: Vec<DTRange> = filter.iter().map(|r| r.clone().into()).collect();
        let (subgraph, ff) = g.subgraph(&filter, frontier);
        // dbg!(&subgraph);

        assert_eq!(ff.as_ref(), expect_frontier);

        // The entries in the subgraph should be the same as the diff, passed through the filter.
        let mut diff = g.diff_rev(&[], frontier).1;
        diff.reverse();

        // dbg!(&diff, &filter);
        let expected_items = rle_intersect_first(diff.iter().copied(), filter.iter().copied())
            .collect::<Vec<_>>();

        let actual_items = subgraph.entries.iter()
            .map(|e| e.span)
            .merge_spans()
            .collect::<Vec<_>>();

        // dbg!(&expected_items, &actual_items);
        assert_eq!(expected_items, actual_items);

        for (entry, expect_parents) in subgraph.entries.iter().zip(expect_parents.iter()) {
            assert_eq!(entry.parents.as_ref(), *expect_parents);
        }

        subgraph.dbg_check_subgraph(true);

        let actual_projection = g.project_onto_subgraph(&filter, frontier);
        assert_eq!(actual_projection.as_ref(), expect_frontier);
    }

    #[test]
    fn test_subgraph() {
        let graph = fancy_graph();

        check_subgraph(&graph, &[0..11], &[5, 10], &[
            &[], &[], &[1, 4], &[2, 8],
        ], &[5, 10]);
        check_subgraph(&graph, &[1..11], &[5, 10], &[
            &[], &[], &[1, 4], &[2, 8],
        ], &[5, 10]);
        check_subgraph(&graph, &[5..6], &[5, 10], &[&[]], &[5]);
        check_subgraph(&graph, &[0..1, 10..11], &[5, 10], &[
            &[], &[0]
        ], &[10]);
        check_subgraph(&graph, &[0..11], &[10], &[
            &[], &[], &[1, 4], &[2, 8],
        ], &[10]);
        check_subgraph(&graph, &[0..11], &[5], &[
            &[]
        ], &[5]);
        check_subgraph(&graph, &[0..3, 9..11], &[10], &[
            &[], &[2]
        ], &[10]);
        check_subgraph(&graph, &[9..11], &[3], &[], &[]);
        check_subgraph(&graph, &[5..6], &[9], &[], &[]);
        check_subgraph(&graph, &[0..1, 2..3], &[2], &[&[], &[0]], &[2]);
        check_subgraph(&graph, &[0..1, 2..3], &[9], &[&[], &[0]], &[2]);
    }
    //
    // #[test]
    // fn subgraph_is_collapsed() {
    //     let parents = Parents(RleVec(vec![
    //         ParentsEntryInternal { // 0-10
    //             span: (0..11).into(), shadow: 0,
    //             parents: Frontier::from_sorted(&[]),
    //         },
    //         ParentsEntryInternal { // 10-20
    //             span: (10..21).into(), shadow: 10,
    //             parents: Frontier::from_sorted(&[1]),
    //         },
    //     ]));
    //
    //     check_subgraph(&parents, &[0..2, 10..12], &[10, 20], &[&[], &[1]], &[11]);
    // }
}