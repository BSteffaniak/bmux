//! Publication-local HTTP representations. No request performs compression.
use std::io::{self, Write};
use std::sync::Arc;

use flate2::{Compression, write::GzEncoder};

// Aggregate per-publication limits, independent of feed/resource budgets.
const MIN_BYTES: usize = 1024;
const MAX_INPUT_BYTES: usize = 8 * 1024 * 1024;
const MAX_CACHE_BYTES: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, Default)]
pub(crate) struct Cache {
    pub json: Option<Arc<[u8]>>,
    pub full: Option<Arc<[u8]>>,
    pub delta: Option<Arc<[u8]>>,
    pub document: Option<Arc<[u8]>>,
}

struct Budget {
    input: usize,
    output: usize,
}

impl Cache {
    pub(crate) fn new(
        json: &[u8],
        binary: Option<&crate::BinaryPresentation>,
        document: Option<&crate::document::EncodedDocument>,
    ) -> Self {
        let mut budget = Budget {
            input: MAX_INPUT_BYTES,
            output: MAX_CACHE_BYTES,
        };
        Self {
            full: binary.and_then(|value| budget.encode(&value.snapshot)),
            delta: binary
                .and_then(|value| value.delta.as_ref())
                .and_then(|(_, bytes)| budget.encode(bytes)),
            document: document.and_then(|value| budget.encode(&value.bytes)),
            json: budget.encode(json),
        }
    }
}

struct LimitedBytes {
    bytes: Vec<u8>,
    limit: usize,
}

impl Write for LimitedBytes {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > self.limit - self.bytes.len() {
            return Err(io::Error::other("gzip cache budget exceeded"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Budget {
    fn encode(&mut self, bytes: &[u8]) -> Option<Arc<[u8]>> {
        // PNG is already compressed, including opaque snapshot submissions.
        if bytes.len() < MIN_BYTES
            || bytes.len() > self.input
            || self.output == 0
            || bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        {
            return None;
        }
        self.input -= bytes.len();
        let limit = self.output.min(bytes.len() - 1);
        let writer = LimitedBytes {
            bytes: Vec::with_capacity(limit),
            limit,
        };
        let mut encoder = GzEncoder::new(writer, Compression::fast());
        encoder.write_all(bytes).ok()?;
        let bytes = encoder.finish().ok()?.bytes;
        self.output -= bytes.len();
        Some(bytes.into())
    }
}

/// Absent headers and wildcard-only offers never opt in to gzip.
#[derive(Clone, Copy, Debug)]
pub(crate) struct AcceptEncoding {
    gzip: u16,
    identity: u16,
}

impl AcceptEncoding {
    pub(crate) fn from_headers(headers: &[u8]) -> Self {
        let value = std::str::from_utf8(headers).ok().and_then(|text| {
            text.split("\r\n")
                .filter_map(|line| line.split_once(':'))
                .find(|(name, _)| name.eq_ignore_ascii_case("accept-encoding"))
                .map(|(_, value)| value)
        });
        Self::parse(value)
    }

    fn parse(value: Option<&str>) -> Self {
        let mut gzip = None;
        let mut identity = None;
        let mut wildcard = None;
        for offer in value.unwrap_or("").split(',') {
            let mut parts = offer.trim().split(';');
            let coding = parts.next().unwrap_or("").trim();
            let quality = match parts.next() {
                None => 1000,
                Some(parameter) => parameter
                    .trim()
                    .split_once('=')
                    .filter(|(name, _)| name.trim().eq_ignore_ascii_case("q"))
                    .and_then(|(_, value)| quality(value.trim()))
                    .unwrap_or(0),
            };
            let quality = if parts.next().is_some() { 0 } else { quality };
            let slot = if coding.eq_ignore_ascii_case("gzip") {
                &mut gzip
            } else if coding.eq_ignore_ascii_case("identity") {
                &mut identity
            } else if coding == "*" {
                &mut wildcard
            } else {
                continue;
            };
            // Contradictory duplicates must not override a refusal.
            *slot = Some(slot.map_or(quality, |previous: u16| previous.min(quality)));
        }
        Self {
            gzip: gzip.unwrap_or(0),
            identity: identity.unwrap_or(if wildcard == Some(0) { 0 } else { 1000 }),
        }
    }

    pub(crate) fn select<'a>(
        self,
        identity: &'a [u8],
        gzip: Option<&'a Arc<[u8]>>,
    ) -> Option<Selected<'a>> {
        if self.gzip > 0
            && self.gzip >= self.identity
            && let Some(bytes) = gzip
        {
            return Some(Selected {
                bytes,
                gzip: true,
                identity_len: identity.len(),
            });
        }
        (self.identity > 0).then_some(Selected {
            bytes: identity,
            gzip: false,
            identity_len: identity.len(),
        })
    }
}

fn quality(value: &str) -> Option<u16> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if !matches!(whole, "0" | "1")
        || fraction.len() > 3
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
        || (whole == "1" && fraction.bytes().any(|byte| byte != b'0'))
    {
        return None;
    }
    let fraction_len = fraction.len();
    let fraction = fraction
        .bytes()
        .fold(0_u16, |acc, byte| acc * 10 + u16::from(byte - b'0'));
    let scale = match fraction_len {
        0 => 1000,
        1 => 100,
        2 => 10,
        _ => 1,
    };
    Some(if whole == "1" { 1000 } else { fraction * scale })
}

pub(crate) struct Selected<'a> {
    pub bytes: &'a [u8],
    pub gzip: bool,
    pub identity_len: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn negotiated_gzip_decodes_to_identical_bytes_and_reuses_cache() {
        let bytes = vec![b'x'; 4096];
        let cache = Cache::new(&bytes, None, None);
        let selected = AcceptEncoding::parse(Some("gzip"))
            .select(&bytes, cache.json.as_ref())
            .unwrap();
        assert!(selected.gzip);
        let mut decoded = Vec::new();
        flate2::read::GzDecoder::new(selected.bytes)
            .read_to_end(&mut decoded)
            .unwrap();
        assert_eq!(decoded, bytes);
        let cloned = cache.clone();
        assert!(Arc::ptr_eq(
            cache.json.as_ref().unwrap(),
            cloned.json.as_ref().unwrap()
        ));
    }

    #[test]
    fn q_values_require_explicit_gzip_and_honor_identity() {
        let bytes = vec![b'x'; 4096];
        let cache = Cache::new(&bytes, None, None);
        for header in [
            None,
            Some("*"),
            Some("gzip;q=0"),
            Some("gzip;q=0.5"),
            Some("gzip;q=1.001"),
            Some("gzip;q=bogus"),
            Some("gzip, gzip;q=0"),
        ] {
            assert!(
                !AcceptEncoding::parse(header)
                    .select(&bytes, cache.json.as_ref())
                    .unwrap()
                    .gzip,
                "{header:?}"
            );
        }
        for header in ["GZip;Q=1.000", "gzip;q=0.7, identity;q=0.5", "gzip, *;q=0"] {
            assert!(
                AcceptEncoding::parse(Some(header))
                    .select(&bytes, cache.json.as_ref())
                    .unwrap()
                    .gzip
            );
        }
        assert!(
            AcceptEncoding::parse(Some("*;q=0"))
                .select(&bytes, cache.json.as_ref())
                .is_none()
        );
        assert!(
            AcceptEncoding::parse(Some("gzip, identity;q=0"))
                .select(&bytes, None)
                .is_none()
        );
    }

    #[test]
    fn aggregate_input_and_retained_output_are_bounded() {
        let bytes: Arc<[u8]> = vec![b'x'; MAX_INPUT_BYTES / 2].into();
        let binary = crate::BinaryPresentation {
            snapshot: Arc::clone(&bytes),
            delta: Some((1, Arc::clone(&bytes))),
        };
        let cache = Cache::new(&bytes, Some(&binary), None);
        assert!(cache.full.is_some());
        assert!(cache.delta.is_some());
        assert!(cache.json.is_none());
        assert!(
            cache
                .full
                .iter()
                .chain(cache.delta.iter())
                .map(|bytes| bytes.len())
                .sum::<usize>()
                <= MAX_CACHE_BYTES
        );

        let mut budget = Budget {
            input: 8192,
            output: 100,
        };
        let first = budget.encode(&[0; 4096]).unwrap();
        let remaining = budget.output;
        assert_eq!(remaining + first.len(), 100);
        if let Some(second) = budget.encode(&[0; 4096]) {
            assert!(first.len() + second.len() <= 100);
        }
        assert_eq!(budget.input, 0);
    }

    #[test]
    fn bounded_compression_skips_small_png_oversized_and_expanding_inputs() {
        let mut budget = Budget {
            input: MAX_INPUT_BYTES,
            output: MAX_CACHE_BYTES,
        };
        assert!(budget.encode(&[0; MIN_BYTES - 1]).is_none());
        let mut png = vec![0; 4096];
        png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        assert!(budget.encode(&png).is_none());
        assert!(budget.encode(&vec![0; MAX_INPUT_BYTES + 1]).is_none());
        let mut random = 1_u32;
        let noise: Vec<u8> = (0..4096)
            .map(|_| {
                random ^= random << 13;
                random ^= random >> 17;
                random ^= random << 5;
                random.to_le_bytes()[0]
            })
            .collect();
        assert!(budget.encode(&noise).is_none());
        let mut tiny = Budget {
            input: 4096,
            output: 1,
        };
        assert!(tiny.encode(&[0; 4096]).is_none());
        assert_eq!(tiny.input, 0);
    }
}
