use dylos_core::MacAddr;

#[test]
fn format() {
    let mac = MacAddr::from_seed(b"test");
    let mac_str = mac.to_string();
    assert_eq!(mac_str.len(), 17);
    assert!(mac_str.starts_with("02:"));
    for (i, c) in mac_str.chars().enumerate() {
        if i % 3 == 2 {
            assert_eq!(c, ':');
        } else {
            assert!(c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
        }
    }
}

#[test]
fn bit_pattern() {
    let mac = MacAddr::from_seed(b"test");
    assert!(mac.is_locally_administered());
    assert!(mac.is_unicast());
}

#[test]
fn determinism() {
    let mac1 = MacAddr::from_seed(b"dylos");
    let mac2 = MacAddr::from_seed(b"dylos");
    assert_eq!(mac1, mac2);
    assert_eq!(mac1.to_string(), mac2.to_string());
}

#[test]
fn different_seeds_differ() {
    let mac1 = MacAddr::from_seed(b"dylos1");
    let mac2 = MacAddr::from_seed(b"dylos2");
    assert_ne!(mac1, mac2);
    assert_ne!(mac1.to_string(), mac2.to_string());
}
