#![allow(dead_code)] // TODO: turn this off and clean up before releasing.

pub use alloc::*;
pub use common::{LocalOp, AgentId};

pub mod list;

mod common;
mod range_tree;
mod split_list;
mod splitable_span;
mod alloc;
mod order;
mod rle;
mod unicount;
mod merge_iter;

#[cfg(test)]
mod tests {
    // As per smartstring's documentation.
    #[test]
    fn validate_smartstring() {
        smartstring::validate();
    }
}