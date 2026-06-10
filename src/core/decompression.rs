use brotli::BrotliDecompress;
use bytes::Bytes;
use flate2::read::{DeflateDecoder, GzDecoder, ZlibDecoder};
use zstd::stream::read::Decoder as ZstdDecoder;

use crate::middleware::{HeaderMap, header_value, remove_header};

fn read_decoder_to_bytes<R: std::io::Read>(mut reader: R) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    reader.read_to_end(&mut out).ok().map(|_| out)
}

fn decode_deflate(bytes: &[u8]) -> Option<Vec<u8>> {
    // HTTP "deflate" is zlib-wrapped deflate per RFC 9110. Some servers send
    // raw deflate, so keep that fallback for interoperability.
    read_decoder_to_bytes(ZlibDecoder::new(bytes))
        .or_else(|| read_decoder_to_bytes(DeflateDecoder::new(bytes)))
}

fn looks_like_zstd_frame(bytes: &[u8]) -> bool {
    matches!(bytes.get(0..4), Some([0x28, 0xb5, 0x2f, 0xfd]))
        || matches!(
            bytes.get(0..4),
            Some([magic, 0x2a, 0x4d, 0x18]) if (0x50..=0x5f).contains(magic)
        )
}

/// Returns the canonical response body bytes, transparently decompressing
/// gzip/deflate/br/zstd. On success the `content-encoding`/`content-length` headers
/// are stripped so they match the decoded body.
pub fn decoded_response_body(res_headers: &mut HeaderMap, res_bytes: &Bytes) -> Bytes {
    let encoding = header_value(res_headers, "content-encoding")
        .unwrap_or_default()
        .to_lowercase();

    let decoded = if encoding.contains("gzip") {
        read_decoder_to_bytes(GzDecoder::new(&res_bytes[..]))
    } else if encoding.contains("deflate") {
        decode_deflate(res_bytes)
    } else if encoding.contains("br") {
        let mut out = Vec::new();
        BrotliDecompress(&mut &res_bytes[..], &mut out)
            .ok()
            .map(|_| out)
    } else if encoding.contains("zstd") {
        if looks_like_zstd_frame(res_bytes) {
            ZstdDecoder::new(&res_bytes[..])
                .ok()
                .and_then(read_decoder_to_bytes)
        } else {
            Some(res_bytes.to_vec())
        }
    } else {
        None
    };

    if let Some(out) = decoded {
        remove_header(res_headers, "content-encoding");
        remove_header(res_headers, "content-length");
        return Bytes::from(out);
    }

    res_bytes.clone()
}
