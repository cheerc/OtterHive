//! 契約 §7.2 的 frame wire format：1 byte tag + 4 byte big-endian 長度 + 內容。
//!
//! 這些測試刻意「手工解析」位元組，而不是只用 `encode_frame`／`read_frame`
//! 互相驗證 —— 只驗同一對 encoder/decoder 自洽，會讓兩者同時寫錯時仍然通過。
//! wire layout 的斷言一律從裸位元組算起。

use std::io::Cursor;

use otterhive::terminal::frame::{encode_frame, read_frame, MAX_FRAME_BYTES};

// --- 契約 §7.2 的 tag 與方向 ---

const TAG_SNAPSHOT: u8 = 0x01;
const TAG_OUTPUT: u8 = 0x02;
const TAG_RESYNC: u8 = 0x03;
const TAG_ENDED: u8 = 0x04;
const TAG_ERROR: u8 = 0x05;
const TAG_INPUT: u8 = 0x10;
const TAG_RESIZE: u8 = 0x11;
const TAG_DETACH: u8 = 0x12;

/// 所有 frame 的 payload 上限：1 MiB。
///
/// 這是 payload 的長度，依 coordinator 裁定**包含** Snapshot 的 4B header 長度前綴與
/// header JSON、以及 Output 的 8B seq 前綴與其後 bytes；不含外層的 5B（tag + 長度）。
const ONE_MIB: usize = 1024 * 1024;

// --- 讀取 wire 的輔助函式（刻意獨立於 otterhive 的實作） ---

/// 依契約手動切出 tag、宣告長度與 payload，用於獨立驗證 encode 結果。
fn parse_wire(bytes: &[u8]) -> (u8, usize, &[u8]) {
    assert!(bytes.len() >= 5, "frame 至少要有 1B tag + 4B 長度");
    let tag = bytes[0];
    let declared_len = u32::from_be_bytes([bytes[1], bytes[2], bytes[3], bytes[4]]) as usize;
    (tag, declared_len, &bytes[5..5 + declared_len])
}

// --- 1. 八個 tag 的 round trip ---

#[test]
fn frame_round_trip_for_every_tag() {
    // 每個 tag 都帶一段不同的 payload，避免「tag 被忽略」的錯誤被發現。
    let cases: Vec<(u8, Vec<u8>)> = vec![
        (TAG_SNAPSHOT, b"snapshot-payload".to_vec()),
        (TAG_OUTPUT, b"output-payload".to_vec()),
        (TAG_RESYNC, Vec::new()),
        (TAG_ENDED, b"ended-payload".to_vec()),
        (TAG_ERROR, b"error-payload".to_vec()),
        (TAG_INPUT, b"input-payload".to_vec()),
        (TAG_RESIZE, b"resize-payload".to_vec()),
        (TAG_DETACH, Vec::new()),
    ];
    assert_eq!(cases.len(), 8, "契約 §7.2 定義了八個 tag");

    for (tag, payload) in cases {
        let encoded = encode_frame(tag, &payload);
        let (read_tag, read_payload) =
            read_frame(&mut Cursor::new(encoded)).expect("合法的 frame 應該讀得回來");
        assert_eq!(read_tag, tag, "tag 0x{tag:02x} 應該原樣往返");
        assert_eq!(
            read_payload, payload,
            "tag 0x{tag:02x} 的 payload 應該原樣往返"
        );
    }
}

#[test]
fn empty_payload_frames_are_readable() {
    // Resync 與 Detach 的內容為空，這是最小的合法 frame。
    for tag in [TAG_RESYNC, TAG_DETACH] {
        let encoded = encode_frame(tag, &[]);
        assert_eq!(encoded.len(), 5, "空 payload 的 frame 只有外層 5B");

        let (read_tag, read_payload) =
            read_frame(&mut Cursor::new(encoded)).expect("空 payload 應該讀得回來");
        assert_eq!(read_tag, tag);
        assert!(
            read_payload.is_empty(),
            "空 payload 讀回來應為空，實際長度 {}",
            read_payload.len()
        );
    }
}

// --- 2. wire layout：big-endian 長度與 tag 位置 ---

#[test]
fn frame_length_prefix_is_four_byte_big_endian() {
    // 258 = 0x00000102。big-endian 應為 00 00 01 02；
    // little-endian 會是 02 01 00 00，據此可分辨。
    let payload = vec![0xABu8; 258];
    let encoded = encode_frame(TAG_OUTPUT, &payload);

    assert_eq!(encoded[0], TAG_OUTPUT, "第 0 byte 是 tag");
    assert_eq!(
        &encoded[1..5],
        &[0x00, 0x00, 0x01, 0x02],
        "長度前綴必須是 4 byte big-endian"
    );
    assert_eq!(encoded.len(), 5 + 258, "總長度 = 5B 外層 + payload");

    let (tag, declared_len, body) = parse_wire(&encoded);
    assert_eq!(tag, TAG_OUTPUT);
    assert_eq!(declared_len, 258);
    assert_eq!(body, payload.as_slice());
}

#[test]
fn frame_layout_puts_tag_before_length_and_payload() {
    let payload = b"hi".to_vec();
    let encoded = encode_frame(TAG_INPUT, &payload);

    assert_eq!(
        encoded,
        vec![TAG_INPUT, 0x00, 0x00, 0x00, 0x02, b'h', b'i'],
        "frame 必須是 tag(1B) + 長度(4B BE) + 內容 的固定順序"
    );
}

// --- 3. Snapshot / Output / Input 的 payload 內部 layout ---

#[test]
fn snapshot_payload_starts_with_four_byte_header_length() {
    // 契約 §7.2：Snapshot 內容 = 4B BE header 長度 + JSON header + 畫面 bytes。
    // 外層 frame 長度只界定整個 payload，payload 內部的前綴才界定 JSON header 與
    // ANSI 畫面 bytes 的分界，因此這個前綴不可省略。
    let header_json = br#"{"generation":3,"seq":42,"cols":80,"rows":24}"#;
    let ansi = b"\x1b[H\x1b[2Jhello".to_vec();

    let mut payload = Vec::new();
    payload.extend_from_slice(&(header_json.len() as u32).to_be_bytes());
    payload.extend_from_slice(header_json);
    payload.extend_from_slice(&ansi);

    let encoded = encode_frame(TAG_SNAPSHOT, &payload);
    let (tag, declared_len, body) = parse_wire(&encoded);
    assert_eq!(tag, TAG_SNAPSHOT);
    assert_eq!(
        declared_len,
        payload.len(),
        "外層長度等於整個 payload（含內部前綴）"
    );

    // 從 payload 開頭手工切出 header 邊界。
    let header_len = u32::from_be_bytes([body[0], body[1], body[2], body[3]]) as usize;
    let header = &body[4..4 + header_len];
    let screen = &body[4 + header_len..];

    assert_eq!(
        header, header_json,
        "JSON header 應該緊接在 4B 長度前綴之後"
    );
    assert_eq!(
        screen,
        ansi.as_slice(),
        "畫面 bytes 應該緊接在 JSON header 之後"
    );
    assert_eq!(
        4 + header_len + screen.len(),
        body.len(),
        "payload 必須剛好由三段構成，不留多餘尾巴"
    );
}

#[test]
fn output_payload_starts_with_eight_byte_big_endian_seq() {
    // 契約 §7.2：Output 內容 = 8B BE seq + PTY 輸出 bytes。
    let seq: u64 = 0x0102_0304_0506_0708;
    let bytes = b"output".to_vec();

    let mut payload = Vec::new();
    payload.extend_from_slice(&seq.to_be_bytes());
    payload.extend_from_slice(&bytes);

    let encoded = encode_frame(TAG_OUTPUT, &payload);
    let (tag, _, body) = parse_wire(&encoded);
    assert_eq!(tag, TAG_OUTPUT);
    assert_eq!(body.len(), 8 + bytes.len());

    let read_seq = u64::from_be_bytes([
        body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
    ]);
    assert_eq!(read_seq, seq, "seq 必須是 8 byte big-endian");
    assert_eq!(&body[8..], bytes.as_slice(), "seq 之後是原始輸出 bytes");
}

#[test]
fn input_payload_starts_with_eight_byte_big_endian_generation() {
    // 契約 §7.2：Input 內容 = 8B BE generation + bytes。
    let generation: u64 = 7;
    let typed = "你好".as_bytes().to_vec();

    let mut payload = Vec::new();
    payload.extend_from_slice(&generation.to_be_bytes());
    payload.extend_from_slice(&typed);

    let encoded = encode_frame(TAG_INPUT, &payload);
    let (tag, _, body) = parse_wire(&encoded);
    assert_eq!(tag, TAG_INPUT);

    let read_generation = u64::from_be_bytes([
        body[0], body[1], body[2], body[3], body[4], body[5], body[6], body[7],
    ]);
    assert_eq!(read_generation, generation);
    assert_eq!(&body[8..], typed.as_slice(), "generation 之後是輸入 bytes");
}

#[test]
fn resize_payload_is_json_with_generation_cols_and_rows() {
    // 契約 §7.2：Resize 內容 = JSON {"generation","cols","rows"}。
    //
    // Resize 的 payload 是 **JSON 文字**，不是二進位欄位，因此這裡不宣稱任何
    // big-endian 編碼 —— BE 只適用於 frame 長度前綴、Output 的 seq 與 Input 的
    // generation 這三個二進位欄位。測試只驗 JSON 可被解析出三個欄位及其型別。
    let json = br#"{"generation":2,"cols":100,"rows":30}"#;
    let encoded = encode_frame(TAG_RESIZE, json);
    let (tag, _, body) = parse_wire(&encoded);
    assert_eq!(tag, TAG_RESIZE);
    assert_eq!(body, json.as_slice(), "payload 應原樣保留 JSON bytes");

    // 真正解析 JSON，確認欄位名稱、型別與值，而不是只做字串包含檢查。
    let parsed: serde_json::Value =
        serde_json::from_slice(body).expect("Resize payload 應是有效的 JSON");
    let object = parsed.as_object().expect("Resize payload 應是 JSON 物件");
    assert_eq!(
        object.len(),
        3,
        "Resize JSON 應恰有 generation/cols/rows 三個欄位，實際：{object:?}"
    );
    assert_eq!(
        object.get("generation").and_then(serde_json::Value::as_u64),
        Some(2),
        "generation 應是整數 2"
    );
    assert_eq!(
        object.get("cols").and_then(serde_json::Value::as_u64),
        Some(100),
        "cols 應是整數 100"
    );
    assert_eq!(
        object.get("rows").and_then(serde_json::Value::as_u64),
        Some(30),
        "rows 應是整數 30"
    );
}

#[test]
fn ended_payload_is_json_with_generation_and_reason() {
    // 契約 §7.2：Ended 內容 = JSON {"generation","reason"}。
    let json = br#"{"generation":4,"reason":"exit"}"#;
    let encoded = encode_frame(TAG_ENDED, json);
    let (tag, _, body) = parse_wire(&encoded);
    assert_eq!(tag, TAG_ENDED);
    assert_eq!(body, json.as_slice());

    let parsed: serde_json::Value =
        serde_json::from_slice(body).expect("Ended payload 應是有效的 JSON");
    let object = parsed.as_object().expect("Ended payload 應是 JSON 物件");
    assert_eq!(
        object.len(),
        2,
        "Ended JSON 應恰有 generation/reason 兩個欄位，實際：{object:?}"
    );
    assert_eq!(
        object.get("generation").and_then(serde_json::Value::as_u64),
        Some(4),
        "generation 應是整數 4"
    );
    assert_eq!(
        object.get("reason").and_then(serde_json::Value::as_str),
        Some("exit"),
        "reason 應是字串 \"exit\""
    );
}

#[test]
fn error_payload_is_json_with_code_and_message() {
    // 契約 §7.2：Error 內容 = JSON {"code","message"}。
    // byte string literal 只能是 ASCII，因此中文訊息用 str 再轉 bytes。
    let json: &[u8] = r#"{"code":"stale_generation","message":"世代不符"}"#.as_bytes();
    let encoded = encode_frame(TAG_ERROR, json);
    let (tag, _, body) = parse_wire(&encoded);
    assert_eq!(tag, TAG_ERROR);
    assert_eq!(body, json, "payload 應原樣保留 JSON bytes");

    let parsed: serde_json::Value =
        serde_json::from_slice(body).expect("Error payload 應是有效的 JSON");
    let object = parsed.as_object().expect("Error payload 應是 JSON 物件");
    assert_eq!(
        object.len(),
        2,
        "Error JSON 應恰有 code/message 兩個欄位，實際：{object:?}"
    );
    assert_eq!(
        object.get("code").and_then(serde_json::Value::as_str),
        Some("stale_generation"),
        "code 應是字串 \"stale_generation\""
    );
    assert_eq!(
        object.get("message").and_then(serde_json::Value::as_str),
        Some("世代不符"),
        "message 應保留原字串"
    );
}

// --- 4. 1 MiB 界限 ---

#[test]
fn max_frame_bytes_is_one_mib() {
    assert_eq!(
        MAX_FRAME_BYTES, ONE_MIB,
        "契約 §7.2 的上限是 1 MiB = 1048576 bytes"
    );
}

#[test]
fn payload_exactly_at_limit_is_accepted() {
    let payload = vec![0x5Au8; MAX_FRAME_BYTES];
    let encoded = encode_frame(TAG_OUTPUT, &payload);
    assert_eq!(
        encoded.len(),
        5 + MAX_FRAME_BYTES,
        "外層 5B 不計入 payload 上限"
    );

    let (tag, declared_len, body) = parse_wire(&encoded);
    assert_eq!(tag, TAG_OUTPUT);
    assert_eq!(declared_len, MAX_FRAME_BYTES);
    assert_eq!(body.len(), MAX_FRAME_BYTES);

    // 以 wire bytes 讀回來，確認剛好在界內可完整解碼。
    let (read_tag, read_payload) =
        read_frame(&mut Cursor::new(encoded)).expect("剛好在上限的 frame 應該被接受");
    assert_eq!(read_tag, TAG_OUTPUT);
    assert_eq!(read_payload.len(), MAX_FRAME_BYTES);
}

#[test]
fn payload_one_byte_over_limit_is_rejected_by_the_reader() {
    let payload = vec![0x5Au8; MAX_FRAME_BYTES + 1];

    // 本測試只驗證 **decoder 拒絕** 超限 payload。
    //
    // 寫入端屬 T4：`encode_frame` 的既定回傳型別是 `Vec<u8>`，本輪不提供 fallible
    // 版本，因此這裡不斷言寫入端會拒絕，也不宣稱已拒絕 —— 有界寫入會在 T4
    // socket 寫入前補 RED 驗證。
    let encoded = encode_frame(TAG_OUTPUT, &payload);
    let (_, declared_len, _) = parse_wire(&encoded);
    assert_eq!(
        declared_len,
        MAX_FRAME_BYTES + 1,
        "前提：宣告長度應為上限 +1"
    );

    // 讀取端必須拒絕這個 frame（daemon 端遇此情形會關閉連線）。
    let error =
        read_frame(&mut Cursor::new(encoded)).expect_err("超過 1 MiB 的 payload 應該被拒絕");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::InvalidData,
        "超過上限應回 InvalidData，實際：{error}"
    );
}

#[test]
fn oversized_declared_length_is_rejected_without_reading_body() {
    // 惡意或損壞的 client 可以宣告 4 GiB。若實作先配置再檢查，就會被 OOM 拖垮。
    // 測試用「宣告 0xFFFFFFFF 但完全沒有 body」的 stream 驗證：必須立刻回錯。
    let mut stream = Vec::new();
    stream.push(TAG_OUTPUT);
    stream.extend_from_slice(&u32::MAX.to_be_bytes());

    let error = read_frame(&mut Cursor::new(stream)).expect_err("0xFFFFFFFF 的宣告長度必須被拒絕");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::InvalidData,
        "超過上限應回 InvalidData，實際：{error}"
    );
}

// --- 5. 截斷與 partial read ---

#[test]
fn truncated_header_is_an_error() {
    // 只給部分長度前綴（0..3 byte）。完整的 1B tag + 4B 長度 0 是合法的空 payload
    // frame，由 empty_payload_frames_are_readable 覆蓋，因此 prefix_len = 4 不是截斷。
    for prefix_len in 0..4usize {
        let mut stream = vec![TAG_OUTPUT];
        stream.extend(std::iter::repeat(0x00).take(prefix_len));

        let error = read_frame(&mut Cursor::new(stream)).expect_err("截斷的 frame header 應該回錯");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "截斷 header 應回 UnexpectedEof，實際：{error}"
        );
    }
}

#[test]
fn truncated_body_is_an_error() {
    let payload = vec![0x11u8; 64];
    let encoded = encode_frame(TAG_OUTPUT, &payload);

    // 保留完整外層，但 body 少給一個 byte。
    for body_len in [0usize, 1, 32, 63] {
        let mut stream = Vec::new();
        stream.extend_from_slice(&encoded[..5]);
        stream.extend_from_slice(&encoded[5..5 + body_len]);

        let error = read_frame(&mut Cursor::new(stream)).expect_err("截斷的 frame body 應該回錯");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "body 只有 {body_len} bytes 時應回 UnexpectedEof，實際：{error}"
        );
    }
}

#[test]
fn empty_stream_is_an_error() {
    let error = read_frame(&mut Cursor::new(Vec::new())).expect_err("空 stream 應該回錯");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::UnexpectedEof,
        "空 stream 應回 UnexpectedEof，實際：{error}"
    );
}

#[test]
fn reader_that_returns_partial_reads_is_supported() {
    // 真實的 socket read 經常只給一部分位元組。實作不得假設一次 read_exact
    // 就能拿到資料 —— read_exact 本身允許 partial，但若實作自己用 read() 迴圈
    // 就必須正確累積。用一個每次只吐 1 byte 的 reader 來驗證。
    struct DribbleReader<'a> {
        data: &'a [u8],
        pos: usize,
    }

    impl std::io::Read for DribbleReader<'_> {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.pos >= self.data.len() || buf.is_empty() {
                return Ok(0);
            }
            buf[0] = self.data[self.pos];
            self.pos += 1;
            Ok(1)
        }
    }

    let payload = "每次只給一個 byte".as_bytes().to_vec();
    let encoded = encode_frame(TAG_INPUT, &payload);

    let (tag, read_payload) = read_frame(&mut DribbleReader {
        data: &encoded,
        pos: 0,
    })
    .expect("partial reader 也應該能讀出完整 frame");

    assert_eq!(tag, TAG_INPUT);
    assert_eq!(
        read_payload, payload,
        "partial read 累積後的 payload 必須與原始相同"
    );
}

#[test]
fn consecutive_frames_are_read_in_sequence() {
    // socket 上是連續多個 frame 的 stream，讀取必須能跨 frame 對齊。
    let mut stream = Vec::new();
    stream.extend_from_slice(&encode_frame(TAG_OUTPUT, b"first"));
    stream.extend_from_slice(&encode_frame(TAG_RESYNC, b""));
    stream.extend_from_slice(&encode_frame(TAG_OUTPUT, b"third"));

    let mut reader = Cursor::new(stream);

    let (tag1, payload1) = read_frame(&mut reader).expect("第一個 frame");
    assert_eq!(
        (tag1, payload1.as_slice()),
        (TAG_OUTPUT, b"first".as_slice())
    );

    let (tag2, payload2) = read_frame(&mut reader).expect("第二個 frame");
    assert_eq!((tag2, payload2.as_slice()), (TAG_RESYNC, [].as_slice()));

    let (tag3, payload3) = read_frame(&mut reader).expect("第三個 frame");
    assert_eq!(
        (tag3, payload3.as_slice()),
        (TAG_OUTPUT, b"third".as_slice())
    );

    // 讀完之後串流已盡。
    let error = read_frame(&mut reader).expect_err("串流用盡後應回錯");
    assert_eq!(error.kind(), std::io::ErrorKind::UnexpectedEof);
}
