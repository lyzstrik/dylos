#![no_main]
#![forbid(unsafe_code)]

use dylos_agent::protocol::{Reply, Request};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|input: &str| {
    if let Ok(request) = Request::from_line(input) {
        let encoded = request.to_line();
        assert!(encoded.is_ok());
        if let Ok(encoded) = encoded {
            let reparsed = Request::from_line(&encoded);
            assert!(reparsed.is_ok());
            assert_eq!(reparsed.ok(), Some(request));
        }
    }
    if let Ok(reply) = Reply::from_line(input) {
        let encoded = reply.to_line();
        assert!(encoded.is_ok());
        if let Ok(encoded) = encoded {
            let reparsed = Reply::from_line(&encoded);
            assert!(reparsed.is_ok());
            assert_eq!(reparsed.ok(), Some(reply));
        }
    }
});
