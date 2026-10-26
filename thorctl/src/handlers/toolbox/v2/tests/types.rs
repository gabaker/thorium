//! Tests for the manifest types

use super::super::types::{ToolManifest, ToolVersionManifest};

/// Versions publish as `SemVer` ascending, then other labels sorted, then the latest label
#[test]
fn publish_order_is_semver_then_custom_then_latest() {
    // a tool with a mix of labels and a latest
    let mut tool = ToolManifest::default();
    for label in [
        "latest", "11.2", "10.3.2", "1.10.0", "1.9.0", "zeta", "12.0",
    ] {
        tool.versions
            .insert(label.to_owned(), ToolVersionManifest::default());
    }
    tool.latest = Some("latest".to_owned());
    // 10.3.2 and the others parse as SemVer only when they have three parts
    assert_eq!(
        tool.publish_order(),
        [
            "1.9.0", "1.10.0", "10.3.2", "11.2", "12.0", "zeta", "latest"
        ]
    );
    // a latest label the tool doesn't have leaves every label in its sorted place
    tool.latest = Some("missing".to_owned());
    assert_eq!(
        tool.publish_order(),
        [
            "1.9.0", "1.10.0", "10.3.2", "11.2", "12.0", "latest", "zeta"
        ]
    );
}
