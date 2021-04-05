use super::*;
// use std::mem;
use std::ptr::{self, NonNull};

impl NodeLeaf {
    // Note this doesn't return a Pin<Box<Self>> like the others. At the point of creation, there's
    // no reason for this object to be pinned. (Is that a bad idea? I'm not sure.)
    pub(super) unsafe fn new() -> Self {
        Self::new_with_parent(ParentPtr::Root(NonNull::dangling()))
    }

    pub(super) fn new_with_parent(parent: ParentPtr) -> Self {
        Self {
            parent,
            data: [Entry::default(); NUM_ENTRIES],
            len: 0,
            _pin: PhantomPinned,
            _drop: PrintDropLeaf,
        }
    }

    // pub fn find2(&self, loc: CRDTLocation) -> (ClientSeq, Option<usize>) {
    //     let mut raw_pos: ClientSeq = 0;

    //     for i in 0..NUM_ENTRIES {
    //         let entry = self.data[i];
    //         if entry.is_invalid() { break; }

    //         if entry.loc.client == loc.client && entry.get_seq_range().contains(&loc.seq) {
    //             if entry.len > 0 {
    //                 raw_pos += loc.seq - entry.loc.seq;
    //             }
    //             return (raw_pos, Some(i));
    //         } else {
    //             raw_pos += entry.get_text_len()
    //         }
    //     }
    //     (raw_pos, None)
    // }

    pub fn find(&self, loc: CRDTLocation) -> Option<Cursor> {
        for i in 0..self.len_entries() {
            let entry = self.data[i];

            if entry.loc.agent == loc.agent && entry.get_seq_range().contains(&loc.seq) {
                let offset = if entry.len > 0 {
                    loc.seq - entry.loc.seq
                } else { 0 };

                return Some(Cursor::new(
                    unsafe { NonNull::new_unchecked(self as *const _ as *mut _) },
                    i,
                    offset
                ))
            }
        }
        None
    }

    // Find a given text offset within the node
    // Returns (index, offset within entry)
    pub fn find_offset(&self, mut offset: u32, stick_end: bool) -> Option<(usize, u32)> {
        for i in 0..self.len_entries() {
            // if offset == 0 {
            //     return Some((i, 0));
            // }

            let entry = self.data[i];
            if entry.loc.agent == CLIENT_INVALID { break; }

            let text_len = entry.get_content_len();
            if offset < text_len || (stick_end && text_len == offset) {
                // Found it.
                return Some((i, offset));
            } else {
                offset -= text_len
            }
        }

        if offset == 0 { // Special case for the first inserted element - we may never enter the loop.
            Some((self.len_entries(), 0))
        } else { None }
    }

    // pub(super) fn actually_count_entries(&self) -> usize {
    //     self.data.iter()
    //     .position(|e| e.loc.client == CLIENT_INVALID)
    //     .unwrap_or(NUM_ENTRIES)
    // }
    pub(super) fn len_entries(&self) -> usize {
        self.len as usize
    }

    // Recursively (well, iteratively) ascend and update all the counts along
    // the way up. TODO: Move this - This method shouldn't be in NodeLeaf.
    pub(super) fn update_parent_count(&mut self, amt: i32) {
        if amt == 0 { return; }
        let mut child = NodePtr::Leaf(unsafe { NonNull::new_unchecked(self) });
        let mut parent = self.parent;

        loop {
            match parent {
                ParentPtr::Root(mut r) => {
                    unsafe { r.as_mut().count = r.as_ref().count.wrapping_add(amt as u32); }
                    break;
                },
                ParentPtr::Internal(mut n) => {
                    let idx = unsafe { n.as_mut() }.find_child(child).unwrap();
                    let c = &mut unsafe { n.as_mut() }.data[idx].0;
                    // :(
                    *c = c.wrapping_add(amt as u32);

                    // And recurse.
                    child = NodePtr::Internal(n);
                    parent = unsafe { n.as_mut() }.parent;
                },
            };
        }
    }

    /// Split this leaf node at the specified index, so 0..idx stays and idx.. moves to a new node.
    ///
    /// The new leaf node is not inserted into the tree by this method. It is returned.
    pub(super) fn split_at<F>(&mut self, idx: usize, notify: &mut F) -> NonNull<NodeLeaf>
        where F: FnMut(CRDTLocation, ClientSeq, NonNull<NodeLeaf>)
    {
        unsafe {
            let mut new_node = Self::new(); // The new node has a danging parent pointer
            let new_len = self.len_entries() - idx;
            ptr::copy_nonoverlapping(&self.data[idx], &mut new_node.data[0], new_len);
            new_node.len = new_len as u8;
            
            // "zero" out the old entries
            // TODO(optimization): We're currently copying / moving everything
            // *after* idx. If idx is small, we could instead move everything
            // before idx - which would save a bunch of calls to notify and save
            // us needing to fix up a bunch of parent pointers.
            let mut stolen_length = 0;
            for e in &mut self.data[idx..self.len as usize] {
                stolen_length += e.get_content_len();
                *e = Entry::default();
            }
            self.len = idx as u8;

            // eprintln!("split_at idx {} self_entries {} stolel_len {} self {:?}", idx, self_entries, stolen_length, &self);

            let mut inserted_node = Node::Leaf(Box::pin(new_node));
            // This is the pointer to the new item we'll end up returning.
            let new_leaf_ptr = NonNull::new_unchecked(inserted_node.unwrap_leaf_mut().get_unchecked_mut());
            for e in &inserted_node.unwrap_leaf().data[0..new_len] {
                notify(e.loc, e.get_seq_len(), new_leaf_ptr);
            }

            root::insert_after(self.parent, inserted_node, NodePtr::Leaf(NonNull::new_unchecked(self)), stolen_length);

            new_leaf_ptr
        }
    }
}
