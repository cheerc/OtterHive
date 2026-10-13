//! 契約 §7.2 的 frame 編解碼。
//!
//! frame = 1 byte tag + 4 byte big-endian 長度 + 內容。
//!
//! `encode_frame` 保留既定的 `Vec<u8>` 回傳型別，因此**寫入端不做上界檢查**；
//! 有界寫入（避免呼叫端送出超限 frame）屬 T4 的 socket 寫入層，屆時會補
//! fallible helper 與對應的 RED 測試。`read_frame` 則**必須**拒絕超限的
//! 宣告長度，而且在配置緩衝區之前就先驗長度 —— 否則一個宣告 4 GiB 的
//! 損壞或惡意 frame 就會在讀取時觸發大量配置。

use std::io::Read;

/// 單一 frame 的 payload 上限：1 MiB。
///
/// 這個上限套用於**整個 payload**：Snapshot 的 4B header 長度前綴、JSON header
/// 與畫面 bytes 都計入；Output 的 8B seq 前綴與其後 bytes 也計入。外層的
/// 5B（tag + 長度）不計入。
pub const MAX_FRAME_BYTES: usize = 1024 * 1024;

/// 編碼成 `tag + 4B BE 長度 + 內容`。
///
/// 長度以 `u32` 編碼；`payload` 超過 `MAX_FRAME_BYTES` 時本函式**不會**拒絕，
/// 這個邊界由讀取端與 T4 的有界寫入負責。
pub fn encode_frame(tag: u8, payload: &[u8]) -> Vec<u8> {
    let mut framed = Vec::with_capacity(5 + payload.len());
    framed.push(tag);
    framed.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    framed.extend_from_slice(payload);
    framed
}

/// 讀取一個 frame，回傳 `(tag, payload)`。
///
/// - 串流在 tag、長度前綴或 payload 中途結束 → [`std::io::ErrorKind::UnexpectedEof`]。
///   使用 `read_exact`，因此部分讀取（socket 一次只給一部分位元組）可以正確累積。
/// - 宣告長度超過 [`MAX_FRAME_BYTES`] → [`std::io::ErrorKind::InvalidData`]，
///   且在配置任何緩衝區之前就回傳。呼叫端（daemon）收到這個錯誤會關閉連線
///   （契約 §7.2）。
pub fn read_frame(reader: &mut impl Read) -> std::io::Result<(u8, Vec<u8>)> {
    let mut tag = [0u8; 1];
    reader.read_exact(&mut tag)?;

    let mut length = [0u8; 4];
    reader.read_exact(&mut length)?;
    let declared = u32::from_be_bytes(length) as usize;

    // 先驗長度再配置：宣告 0xFFFFFFFF 時不得真的分配對應的緩衝區。
    if declared > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame payload {declared} bytes 超過上限 {MAX_FRAME_BYTES} bytes"),
        ));
    }

    let mut payload = vec![0u8; declared];
    reader.read_exact(&mut payload)?;
    Ok((tag[0], payload))
}

#[cfg(test)]
mod tests {
    use super::{encode_frame, read_frame, MAX_FRAME_BYTES};
    use std::io::{Cursor, Read};

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
        let encoded = encode_frame(tag, &payload);

        let (read_tag, read_payload) =
            read_frame(&mut Cursor::new(encoded)).expect("剛剛編碼出來的 frame 應該讀得回來");

        assert_eq!(read_tag, tag, "tag 應該原樣往返");
        assert_eq!(read_payload, payload, "payload 應該原樣往返");
    }

    #[test]
    fn encoded_layout_is_tag_then_big_endian_length_then_payload() {
        // 外層 layout 的最小驗證：tag、4B BE 長度、內容的順序與寬度。
        let payload = b"hi".to_vec();
        let encoded = encode_frame(0x10, &payload);

        assert_eq!(encoded.len(), 7, "5B 外層 + 2B payload");
        assert_eq!(encoded[0], 0x10, "第 0 byte 是 tag");
        assert_eq!(&encoded[1..5], &[0, 0, 0, 2], "長度是 4B big-endian");
        assert_eq!(&encoded[5..], payload.as_slice(), "其後是內容");
    }

    #[test]
    fn empty_payload_frame_round_trips() {
        // Resync 與 Detach 的內容為空，是最小的合法 frame。
        let encoded = encode_frame(0x03, &[]);
        assert_eq!(encoded.len(), 5, "空 payload 只有外層 5B");

        let (tag, payload) =
            read_frame(&mut Cursor::new(encoded)).expect("空 payload 應該讀得回來");
        assert_eq!(tag, 0x03);
        assert!(payload.is_empty(), "payload 應為空");
    }

    #[test]
    fn oversize_declared_length_is_refused_without_allocating() {
        // 宣告 0xFFFFFFFF 但完全沒有 body。實作必須在配置之前回錯。
        let mut stream = vec![0x02u8];
        stream.extend_from_slice(&u32::MAX.to_be_bytes());

        let error = read_frame(&mut Cursor::new(stream)).expect_err("0xFFFFFFFF 必須被拒絕");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::InvalidData,
            "超限宣告長度應回 InvalidData，實際：{error}"
        );
    }

    #[test]
    fn truncated_frame_is_unexpected_eof() {
        // 完整 header、但 body 缺一個 byte。
        let encoded = encode_frame(0x02, &[0xAB; 4]);
        let truncated = &encoded[..encoded.len() - 1];

        let error =
            read_frame(&mut Cursor::new(truncated.to_vec())).expect_err("截斷的 body 應該回錯");
        assert_eq!(
            error.kind(),
            std::io::ErrorKind::UnexpectedEof,
            "截斷 body 應回 UnexpectedEof，實際：{error}"
        );
    }

    #[test]
    fn reader_returning_one_byte_at_a_time_is_supported() {
        // 真實 socket read 經常只給一部分位元組；read_exact 必須能累積。
        struct Dribble<'a> {
            data: &'a [u8],
            pos: usize,
        }

        impl Read for Dribble<'_> {
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
        let encoded = encode_frame(0x10, &payload);

        let (tag, read_payload) = read_frame(&mut Dribble {
            data: &encoded,
            pos: 0,
        })
        .expect("partial reader 也應該讀得完整");

        assert_eq!(tag, 0x10);
        assert_eq!(read_payload, payload, "累積後的 payload 必須相同");
    }
}
