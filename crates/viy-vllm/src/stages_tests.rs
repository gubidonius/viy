use super::*;
use crate::boot::build_boot_script;
use viy_config::Config;

#[test]
fn every_stage_has_a_hint() {
    for (key, _, _) in STAGES {
        assert!(!hint(key).is_empty(), "{key}");
    }
    assert_eq!(hint("endpoint"), "");
}

/// The stage markers the boot script itself prints are in the script, so a
/// reworded echo cannot fold a stage away unnoticed.
#[test]
fn the_scripts_own_markers_are_in_the_script() {
    let script = build_boot_script(&Config::defaults(), "sk-SYNTHETIC");
    for (key, _, marker) in STAGES {
        if marker.starts_with("RPCTL_PHASE") || marker.starts_with("=== boot") {
            assert!(script.contains(marker), "{key}: {marker:?}");
        }
    }
}
