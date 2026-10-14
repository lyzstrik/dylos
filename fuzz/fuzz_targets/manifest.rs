#![no_main]
#![forbid(unsafe_code)]

use dylos_core::manifest::SnapshotManifest;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    if let Ok(manifest) = SnapshotManifest::from_json_str(input) {
        let encoded = manifest.to_json_string();
        assert!(encoded.is_ok());
        if let Ok(encoded) = encoded {
            let reparsed = SnapshotManifest::from_json_str(&encoded);
            assert!(reparsed.is_ok());
            assert_eq!(reparsed.ok(), Some(manifest));
        }
    }
});
