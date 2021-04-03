// The btree here is used to map character -> document positions. It could also
// be extended to inline a rope, but I haven't done that here.

// The btree implementation here is based off ropey
// (https://github.com/cessen/ropey/) since that has pretty good performance in
// most cases.

// The common data structures are 

mod cursor;
mod root;
mod leaf;
mod internal;

// pub(crate) use cursor::Cursor;

use std::ops::Range;
use std::ptr::NonNull;
use std::marker;
use std::pin::Pin;

use super::common::*;
use std::marker::PhantomPinned;

pub use root::DeleteResult;

#[cfg(debug_assertions)]
const MAX_CHILDREN: usize = 8; // This needs to be minimum 8.
#[cfg(not(debug_assertions))]
const MAX_CHILDREN: usize = 32;


// Must fit in u8.
#[cfg(debug_assertions)]
const NUM_ENTRIES: usize = 4;
#[cfg(not(debug_assertions))]
const NUM_ENTRIES: usize = 32;


// This is the root of the tree. There's a bit of double-deref going on when you
// access the first node in the tree, but I can't think of a clean way around
// it.
#[derive(Debug)]
pub struct MarkerTree {
    count: ItemCount,
    root: Pin<Box<Node>>,
    _pin: marker::PhantomPinned,
}

#[derive(Debug)]
enum Node {
    Internal(NodeInternal),
    Leaf(NodeLeaf),
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ParentPtr {
    Root(NonNull<MarkerTree>),
    Internal(NonNull<NodeInternal>)
}

// Ugh I hate that I need this.
#[derive(Copy, Clone, Debug)]
enum NodePtr {
    Internal(NonNull<NodeInternal>),
    Leaf(NonNull<NodeLeaf>),
}

// trait NodeT: std::fmt::Debug {}
// impl<T> NodeT for NodeInternal<T> {}
// impl NodeT for NodeLeaf {}

#[derive(Debug)]
struct NodeInternal /*<T: NodeT>*/ {
    parent: ParentPtr,
    // Pairs of (count of subtree elements, subtree contents).
    // Left packed. The nodes are all the same type.
    // ItemCount only includes items which haven't been deleted.
    // data: [(ItemCount, Option<Box<Node>>); MAX_CHILDREN]
    data: [(ItemCount, Option<Pin<Box<Node>>>); MAX_CHILDREN],
    _pin: PhantomPinned, // Needed because children have parent pointers here.
    _drop: PrintDropInternal,
}

#[derive(Debug)]
pub struct NodeLeaf {
    parent: ParentPtr,
    len: u8, // Number of entries which have been populated
    data: [Entry; NUM_ENTRIES],
    _pin: PhantomPinned, // Needed because cursors point here.
    _drop: PrintDropLeaf
}

// struct NodeInternal {
//     children: [Box<Node>; MAX_CHILDREN],
// }

#[derive(Debug, Copy, Clone, Default)]
struct Entry {
    loc: CRDTLocation,
    len: i32, // negative if the chunk was deleted. Never 0 - TODO: could use NonZeroI32
}


#[derive(Copy, Clone, Debug)]
// pub struct Cursor<'a> { // TODO: Add this lifetime parameter back.
pub struct Cursor {
    node: NonNull<NodeLeaf>,
    idx: usize,
    offset: u32, // usize? ??. This is the offset into the item at idx.
    // _marker: marker::PhantomData<&'a Node>,
}

/// Helper struct to track pending size changes in the document which need to be propagated
#[derive(Debug)]
pub struct FlushMarker(i32);

impl Drop for FlushMarker {
    fn drop(&mut self) {
        if self.0 != 0 {
            panic!("Flush marker dropped without being flushed");
        }
    }
}

impl FlushMarker {
    fn flush(&mut self, node: &mut NodeLeaf) {
        node.update_parent_count(self.0);
        self.0 = 0;
    }
}

#[derive(Clone, Debug)]
struct PrintDropLeaf;

// For debugging.

// impl Drop for PrintDropLeaf {
//     fn drop(&mut self) {
//         eprintln!("DROP LEAF {:?}", self);
//     }
// }

#[derive(Clone, Debug)]
struct PrintDropInternal;

// impl Drop for PrintDropInternal {
//     fn drop(&mut self) {
//         eprintln!("DROP INTERNAL {:?}", self);
//     }
// }

unsafe fn pinbox_to_nonnull<T>(box_ref: &Pin<Box<T>>) -> NonNull<T> {
    NonNull::new_unchecked(box_ref.as_ref().get_ref() as *const _ as *mut _)
}

fn pinnode_to_nodeptr(box_ref: &Pin<Box<Node>>) -> NodePtr {
    let node_ref = box_ref.as_ref().get_ref();
    match node_ref {
        Node::Internal(n) => NodePtr::Internal(unsafe { NonNull::new_unchecked(n as *const _ as *mut _) }),
        Node::Leaf(n) => NodePtr::Leaf(unsafe { NonNull::new_unchecked(n as *const _ as *mut _) }),
    }
}


impl Entry {
    fn get_seq_range(self) -> Range<ClientSeq> {
        self.loc.seq .. self.loc.seq + (self.len.abs() as ClientSeq)
    }

    fn get_content_len(&self) -> u32 {
        if self.len < 0 { 0 } else { self.len as u32 }
    }

    fn get_seq_len(&self) -> u32 {
        self.len.abs() as u32
    }

    fn keep_start(&mut self, cut_at: u32) {
        self.len = if self.len < 0 { -(cut_at as i32) } else { cut_at as i32 };
    }

    fn keep_end(&mut self, cut_at: u32) {
        self.loc.seq += cut_at;
        self.len += if self.len < 0 { cut_at as i32 } else { -(cut_at as i32) };
    }

    fn is_invalid(&self) -> bool {
        self.loc.client == CLIENT_INVALID
    }

    fn is_insert(&self) -> bool {
        debug_assert!(self.len != 0);
        self.len > 0
    }

    fn is_delete(&self) -> bool {
        !self.is_insert()
    }
}


impl Node {
    pub unsafe fn new() -> Self {
        Node::Leaf(NodeLeaf::new())
    }
    pub unsafe fn new_with_parent(parent: ParentPtr) -> Self {
        Node::Leaf(NodeLeaf::new_with_parent(parent))
    }

    fn get_parent_mut(&mut self) -> &mut ParentPtr {
        match self {
            Node::Leaf(l) => &mut l.parent,
            Node::Internal(i) => &mut i.parent,
        }
    }
    // fn unwrap_internal_mut_pin<'a>(self: &'a mut Pin<Box<Self>>) -> &'a mut NodeInternal {

    fn set_parent(self: &mut Pin<Box<Self>>, parent: ParentPtr) {
        unsafe {
            *self.as_mut().get_unchecked_mut().get_parent_mut() = parent;
        }
    }

    // pub fn get_parent(&self) -> ParentPtr {
    //     match self {
    //         Node::Leaf(l) => l.parent,
    //         Node::Internal(i) => i.parent,
    //     }
    // }

    fn unwrap_leaf(&self) -> &NodeLeaf {
        match self {
            Node::Leaf(l) => l,
            Node::Internal(_) => panic!("Expected leaf - found internal node"),
        }
    }
    // fn foo(this: Pin<Box<Self>>) -> NonNull<NodeLeaf> {
    //
    // }
    fn unwrap_leaf_mut(&mut self) -> &mut NodeLeaf {
        match self {
            Node::Leaf(l) => l,
            Node::Internal(_) => panic!("Expected leaf - found internal node"),
        }
    }
    fn unwrap_internal(&self) -> &NodeInternal {
        match self {
            Node::Internal(n) => n,
            Node::Leaf(_) => panic!("Expected internal node"),
        }
    }
    fn unwrap_internal_mut(&mut self) -> &mut NodeInternal {
        match self {
            Node::Internal(n) => n,
            Node::Leaf(_) => panic!("Expected internal node"),
        }
    }

    // TODO: These methods should probably return Pin<&mut NodeInternal>, with projections for fields.
    fn unwrap_internal_mut_pin<'a>(self: &'a mut Pin<Box<Self>>) -> &'a mut NodeInternal {
        unsafe {
            self.as_mut().get_unchecked_mut().unwrap_internal_mut()
        }
    }

    fn ptr_eq(&self, ptr: NodePtr) -> bool {
        match (self, ptr) {
            (Node::Internal(n), NodePtr::Internal(ptr)) => std::ptr::eq(n, ptr.as_ptr()),
            (Node::Leaf(n), NodePtr::Leaf(ptr)) => std::ptr::eq(n, ptr.as_ptr()),
            _ => panic!("Pointer type does not match")
        }
    }
}
