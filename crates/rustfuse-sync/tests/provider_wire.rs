//! The File Provider wire format, pinned by the same samples the Swift
//! client's tests use (`macos/RustFuseKit/Tests/RustFuseKitTests/wire.json`):
//! every sample must parse and come back unchanged.

use rustfuse_sync::provider::{Request, Response};
use serde_json::Value;

const SAMPLES: &str = include_str!("../../../macos/RustFuseKit/Tests/RustFuseKitTests/wire.json");

#[test]
fn wire_samples_round_trip() {
    let samples: Value = serde_json::from_str(SAMPLES).unwrap();
    for sample in samples["requests"].as_array().unwrap() {
        let req: Request = serde_json::from_value(sample.clone()).unwrap_or_else(|e| panic!("{sample}: {e}"));
        assert_eq!(&serde_json::to_value(&req).unwrap(), sample);
    }
    for sample in samples["responses"].as_array().unwrap() {
        let resp: Response =
            serde_json::from_value(sample.clone()).unwrap_or_else(|e| panic!("{sample}: {e}"));
        assert_eq!(&serde_json::to_value(&resp).unwrap(), sample);
    }
}
