use dylos_core::LabSpec;
use dylos_net::{Error, FabricPlan};

#[allow(clippy::unwrap_used)] // outside a `#[test]` fn, see clippy.toml
fn reference_lab() -> LabSpec {
    LabSpec::from_yaml_str(include_str!("../../../labs/abc.yaml")).unwrap()
}

fn names(plan: &FabricPlan) -> (Vec<&str>, Vec<(&str, &str)>) {
    let bridges = plan.bridges().iter().map(|b| b.name.as_str()).collect();
    let taps = plan
        .taps()
        .iter()
        .map(|t| (t.name.as_str(), t.bridge.as_str()))
        .collect();
    (bridges, taps)
}

#[test]
fn reference_lab_names_are_derived_from_the_spec_only() {
    let plan = FabricPlan::new("lab1", &reference_lab()).unwrap();
    let (bridges, taps) = names(&plan);
    assert_eq!(bridges, ["br-left", "br-right"]);
    assert_eq!(
        taps,
        [
            ("tap-A-eth0", "br-left"),
            ("tap-B-eth0", "br-left"),
            ("tap-B-eth1", "br-right"),
            ("tap-C-eth0", "br-right"),
        ]
    );
    assert_eq!(plan.netns_name(), "dylos-lab1");

    let clone = FabricPlan::new("lab1-clone-7", &reference_lab()).unwrap();
    assert_eq!(names(&clone), names(&plan));
    assert_eq!(clone.netns_name(), "dylos-lab1-clone-7");
}

#[test]
fn too_long_interface_names_are_rejected() {
    let mut spec = reference_lab();
    spec.segments[0].name = "a-very-long-seg".into();
    let err = FabricPlan::new("lab1", &spec).unwrap_err();
    assert!(
        matches!(err, Error::InvalidIfName { ref ifname, .. } if ifname == "br-a-very-long-seg")
    );

    let mut spec = reference_lab();
    spec.nodes[0].name = "router1".into();
    let err = FabricPlan::new("lab1", &spec).unwrap_err();
    assert!(matches!(err, Error::InvalidIfName { ref ifname, .. } if ifname == "tap-router1-eth0"));
}

#[test]
fn non_portable_interface_names_are_rejected() {
    let mut spec = reference_lab();
    spec.nodes[0].interfaces[0].name = "eth/0".into();
    let err = FabricPlan::new("lab1", &spec).unwrap_err();
    assert!(matches!(err, Error::InvalidIfName { .. }));
}

#[test]
fn colliding_interface_names_are_rejected() {
    let mut spec = reference_lab();
    // Node "B" + interface "x-y" and node "B-x" + interface "y" both give "tap-B-x-y".
    spec.nodes[1].interfaces[0].name = "x-y".into();
    let mut twin = spec.nodes[1].clone();
    twin.name = "B-x".into();
    twin.interfaces.truncate(1);
    twin.interfaces[0].name = "y".into();
    spec.nodes.push(twin);
    let err = FabricPlan::new("lab1", &spec).unwrap_err();
    assert!(matches!(err, Error::IfNameCollision { ref ifname, .. } if ifname == "tap-B-x-y"));
}

#[test]
fn invalid_lab_ids_are_rejected() {
    for id in ["", "has space", "../escape", &"x".repeat(65)] {
        let err = FabricPlan::new(id, &reference_lab()).unwrap_err();
        assert!(matches!(err, Error::InvalidLabId(_)), "{id:?}");
    }
}
