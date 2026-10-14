#![no_main]
#![forbid(unsafe_code)]

use dylos_core::LabSpec;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    if let Ok(spec) = LabSpec::from_yaml_str(input) {
        // Parsing already validates; re-validation must remain consistent.
        assert!(spec.validate().is_ok());
    }
});
