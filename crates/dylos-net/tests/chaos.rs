#![allow(clippy::unwrap_used)]
use dylos_net::ChaosProfile;

#[test]
fn profiles_validate_units_and_partition() {
    let profile = ChaosProfile::parse("100:20:5").unwrap();
    assert_eq!(profile.delay_ms, 100);
    assert_eq!(profile.jitter_ms, 20);
    assert_eq!(profile.loss_percent, 5);
    assert_eq!(ChaosProfile::parse("0:0:100").unwrap().loss_percent, 100);
    for invalid in ["", "1:2", "0:1:0", "1:0:101", "-1:0:0", "1:0:NaN"] {
        assert!(ChaosProfile::parse(invalid).is_err(), "{invalid}");
    }
}
