//! Tests of the writer lease's refusal message

use super::LeaseError;

/// A held lease's message names the holder and says to wait, without offering a flag that only
/// some commands accept
#[test]
fn a_held_lease_names_the_holder_and_offers_no_flag() {
    // the message a refused writer sees
    let message = LeaseError::Held {
        holder: "migrate-job-7".to_owned(),
    }
    .to_string();
    assert!(message.contains("migrate-job-7"), "{message}");
    assert!(message.contains("wait for it to finish"), "{message}");
    assert!(!message.contains("--force-break-lease"), "{message}");
}
