//! `DelegationPipeline::new` refuses host inputs it cannot use before any
//! stage runs.

use super::fixtures::*;
use crate::VotingError;

#[test]
fn malformed_anchor_tree_state_bytes_are_refused_as_invalid_input() {
    // A length-delimited field header that promises five bytes and has none.
    let refused = match pipeline_with_anchor_tree_state(vec![0x0A, 0x05]) {
        Ok(_) => panic!("malformed anchor bytes must be refused at construction"),
        Err(error) => error,
    };

    assert!(
        matches!(refused, VotingError::InvalidInput { ref message } if message.contains("anchor tree state")),
        "caller-supplied anchor bytes are invalid input, got {refused:?}"
    );
}

#[test]
fn a_well_formed_anchor_tree_state_is_accepted() {
    assert!(pipeline_with_anchor_tree_state(Vec::new()).is_ok());
}
