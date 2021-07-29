use crate::list::{ListCRDT, ROOT_ORDER};
use ropey::Rope;
use crate::splitable_span::SplitableSpan;

/// This file contains debugging assertions to validate the document's internal state.
///
/// This is used during fuzzing to make sure everything is working properly, and if not, find bugs
/// as early as possible.

impl ListCRDT {
    #[allow(unused)]
    pub fn dbg_assert_content_eq(&self, expected_content: &Rope) {
        if let Some(ref text) = self.text_content {
            assert_eq!(text, expected_content);
        }
    }

    // Used for testing.
    #[allow(unused)]
    pub fn check(&self, deep: bool) {
        self.index.check();

        if let Some(text) = self.text_content.as_ref() {
            assert_eq!(self.range_tree.len() as usize, text.len_chars());

            let num_deleted_items = self.deletes.iter().fold(0, |x, y| x + y.len());
            if let Some(del_content) = self.deleted_content.as_ref() {
                assert_eq!(del_content.chars().count(), num_deleted_items);
            }
        }

        if deep {
            self.check_shadow();
        }
    }


    fn check_shadow(&self) {
        // The shadow entries in txns name the smallest order for which all txns from
        // [shadow..txn.order] are transitive parents of the current txn.

        // I'm testing here sort of by induction. Iterating the txns in order allows us to assume
        // all previous txns have valid shadows while we advance.

        for txn in self.txns.iter() {
            // We contain prev_txn_order *and more*! See if we can extend the shadow by
            // looking at the other entries of parents.
            let mut parents = txn.parents.clone();
            let mut expect_shadow = txn.order;

            if parents[0] == ROOT_ORDER {
                // The root order will be sorted out of order, but it doesn't matter because
                // if it shows up at all it should be the only item in parents.
                debug_assert_eq!(parents.len(), 1);
                // And leave expect_shadow = txn.order.
            } else {
                parents.sort_by(|a, b| b.cmp(a)); // descending order

                // By induction, we can assume the previous shadows are correct.
                for parent_order in parents {
                    // Note parent_order could point in the middle of a txn run.
                    let (parent_txn, offs) = self.txns.find(parent_order).unwrap();

                    // dbg!(parent_txn.order + offs, expect_shadow);
                    // Shift it if the expected shadow points to the last item in the txn run.
                    if parent_txn.order + offs + 1 == expect_shadow {
                        expect_shadow = parent_txn.shadow;
                    } else { break; }
                }
            }

            assert_eq!(txn.shadow, expect_shadow);
        }
    }

    #[allow(unused)]
    pub fn check_all_changes_rle_merged(&self) {
        assert_eq!(self.client_data[0].item_orders.num_entries(), 1);
        assert_eq!(self.client_with_order.num_entries(), 1);
        assert_eq!(self.txns.num_entries(), 1);
    }
}