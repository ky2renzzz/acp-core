#![no_main]

use libfuzzer_sys::fuzz_target;
use serde_json::Value;

fuzz_target!(|data: &[u8]| {
    // Try to parse as JSON, then canonicalize.
    // canonicalize should never panic on any valid serde_json::Value.
    if let Ok(value) = serde_json::from_slice::<Value>(data) {
        let canonical = acp_wire::canonicalize(&value);
        
        // The canonical form should be valid JSON.
        let reparsed: Result<Value, _> = serde_json::from_slice(&canonical);
        assert!(reparsed.is_ok(), "canonical form is not valid JSON");
        
        // Canonicalizing twice should be idempotent.
        if let Ok(v2) = reparsed {
            let canonical2 = acp_wire::canonicalize(&v2);
            assert_eq!(canonical, canonical2, "canonicalize is not idempotent");
        }
    }
});
