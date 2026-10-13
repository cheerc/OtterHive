//! 契約 §7.2 的 frame 編解碼。
//!
//! 目前僅有測試骨架（RED）：frame 尚無實作，因此
//! `cargo test --locked --test frame` 以 compile-RED 失敗。
//! 完整測試在 `tests/frame.rs`。

#![cfg(test)]

use crate::terminal::frame::MAX_FRAME_BYTES;

#[test]
fn max_frame_bytes_constant_is_exposed() {
    // 契約 §7.2 的 payload 上限：1 MiB。
    assert_eq!(
        MAX_FRAME_BYTES,
        1024 * 1024,
        "契約 §7.2 的 payload 上限是 1 MiB = 1048576 bytes"
    );
}

#[test]
fn encode_then_read_preserves_tag_and_payload() {
    let tag = 0x02u8;
    let payload = b"frame unit test".to_vec();
    let encoded = crate::terminal::frame::encode_frame(tag, &payload);

    let (read_tag, read_payload) =
        crate::terminal::frame::read_frame(&mut std::io::Cursor::new(encoded))
            .expect("剛剛編碼出來的 frame 應該讀得回來");

    assert_eq!(read_tag, tag, "tag 應該原樣往返");
    assert_eq!(read_payload, payload, "payload 應該原樣往返");
}
