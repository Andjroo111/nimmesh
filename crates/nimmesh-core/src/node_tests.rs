//! # node_tests — the MeshNode unit suite, extracted from `node.rs` so the logic module stays
//! under the 800-line ceiling (matches the repo's `swap_*_tests` sibling convention).

use crate::node::{to_sender_id, to_tx_id};

#[test]
fn id_helpers_truncate_and_pad() {
    assert_eq!(to_sender_id(&[1, 2, 3]), [1, 2, 3, 0, 0, 0, 0, 0]);
    assert_eq!(to_sender_id(&[9; 12]), [9; 8]);
    let id = to_tx_id(&[7; 4]);
    assert_eq!(&id.0[..4], &[7, 7, 7, 7]);
    assert_eq!(id.0[4], 0);
}
