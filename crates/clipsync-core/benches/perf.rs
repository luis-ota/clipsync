use std::time::Instant;

use base64::Engine;
use clipsync_core::protocol::{DeviceId, Message};

fn main() {
    let text = Message::ClipboardText {
        mime: "text/plain;charset=utf-8".into(),
        content: "x".repeat(1024),
        origin: DeviceId::from("benchmark"),
        sha256: "a".repeat(64),
    };
    let image = Message::ClipboardImage {
        mime: "image/png".into(),
        data_b64: base64::engine::general_purpose::STANDARD.encode(vec![0xabu8; 1024 * 1024]),
        width: None,
        height: None,
        sha256: "b".repeat(64),
        origin: DeviceId::from("benchmark"),
    };

    for (name, message) in [("text-1k", text), ("image-1m", image)] {
        let start = Instant::now();
        let payload = serde_json::to_string(&message).expect("message serializes");
        let elapsed = start.elapsed();
        println!(
            "{name}: json_bytes={} encode_us={}",
            payload.len(),
            elapsed.as_micros()
        );
    }
}
