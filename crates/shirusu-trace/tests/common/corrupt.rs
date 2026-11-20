//! Trace のバイト列を壊すテスト用の道具（shirusu-analysis の view.rs と shirusu-cli の cli.rs が
//! #[path] で取り込む: どちらも Blob を読めない Trace の扱いを検査する）。

use shirusu_trace::container::{
    parse_raw_header, serialize_header, BlockHeader, BlockType, Compression,
};

/// Blob Chunk の本体 1 バイトを壊し、Block の CRC32C を再計算して
/// 整合させる。Blob ID と内容の BLAKE3 が一致しなくなるため、
/// Reader はその Blob を取り込まず MissingBlob として扱う。
pub fn corrupt_blob_body(bytes: &[u8]) -> Vec<u8> {
    let mut pos = 0usize;
    while pos + 32 <= bytes.len() {
        let raw = parse_raw_header(&bytes[pos..]).expect("valid header");
        if raw.block_type_code == BlockType::BlobChunk.code() {
            let header = BlockHeader {
                block_type: BlockType::BlobChunk,
                compression: Compression::from_code(raw.compression_code).unwrap(),
                sequence: raw.sequence,
                stored_len: raw.stored_len,
                raw_len: raw.raw_len,
            };
            let stored = &bytes[pos + 32..pos + 32 + raw.stored_len as usize];
            let mut payload = shirusu_trace::container::decode_payload(&header, stored).unwrap();
            payload[36] ^= 0xFF;
            let new_header = BlockHeader {
                block_type: BlockType::BlobChunk,
                compression: Compression::None,
                sequence: raw.sequence,
                stored_len: payload.len() as u32,
                raw_len: payload.len() as u32,
            };
            let mut out = bytes[..pos].to_vec();
            out.extend_from_slice(&serialize_header(&new_header, &payload));
            out.extend_from_slice(&payload);
            out.extend_from_slice(&bytes[pos + 32 + raw.stored_len as usize..]);
            return out;
        }
        pos += 32 + raw.stored_len as usize;
    }
    panic!("no blob chunk found");
}
