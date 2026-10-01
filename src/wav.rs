//! Wrap raw PCM in a RIFF/WAVE header, so audio kaibo stores is always a playable file.
//!
//! Gemini's speech models answer with bare samples (`audio/L16;codec=pcm;rate=24000`):
//! no container, nothing that tells a player the rate or the sample width. Stored as-is,
//! those bytes would be an artifact no ordinary tool can open, and the rate would live
//! only in a mime parameter the CAS does not keep. So the samples are wrapped here, once,
//! before they reach the store, and the stored object is an ordinary `audio/wav`.
//!
//! Pure Rust, forty-four bytes of header — no audio crate, in keeping with the
//! no-C-toolchain build (see `AGENTS.md`).

use thiserror::Error;

/// The PCM layout a header describes. Only 16-bit signed little-endian samples are
/// produced here, because that is the one layout any provider kaibo speaks sends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PcmFormat {
    pub sample_rate: u32,
    pub channels: u16,
}

/// Why raw PCM could not be wrapped. Each one is refused rather than papered over: a
/// header that disagrees with its samples plays as noise, which is a corrupt artifact that
/// looks like a successful one.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum WavError {
    #[error(
        "the PCM stream is {len} bytes, which is not a whole number of {frame}-byte \
         frames ({channels} channel(s) of 16-bit samples) — the provider sent a truncated \
         or mislabelled stream, so kaibo refuses to store it as audio"
    )]
    PartialFrame {
        len: usize,
        frame: usize,
        channels: u16,
    },
    #[error("the PCM stream is empty, so there is no audio to store")]
    Empty,
    #[error(
        "the PCM stream is {len} bytes, which exceeds the 4 GiB a WAVE header can \
         describe"
    )]
    TooLarge { len: usize },
    /// A rate or channel count a WAVE header cannot hold: zero, or so large the header's
    /// block-align or byte-rate field would overflow. These numbers come from a
    /// provider's mime parameter, so they are checked rather than trusted.
    #[error(
        "a PCM format needs a sample rate and at least one channel, and a WAVE header \
         cannot describe this one: {0:?}"
    )]
    InvalidFormat(PcmFormat),
}

/// Wrap 16-bit little-endian PCM samples in a canonical 44-byte WAVE header.
pub fn wrap_pcm16(samples: &[u8], format: PcmFormat) -> Result<Vec<u8>, WavError> {
    // `block_align` is a u16 and `byte_rate` a u32 in the header; either overflowing
    // would write a header that disagrees with its samples.
    // A zero rate or channel count makes `byte_rate` zero, so one filter refuses both.
    let block_align = format.channels.checked_mul(2);
    let byte_rate = block_align
        .and_then(|align| format.sample_rate.checked_mul(u32::from(align)))
        .filter(|rate| *rate > 0);
    let (Some(block_align), Some(byte_rate)) = (block_align, byte_rate) else {
        return Err(WavError::InvalidFormat(format));
    };
    if samples.is_empty() {
        return Err(WavError::Empty);
    }
    let frame = usize::from(block_align);
    if samples.len() % frame != 0 {
        return Err(WavError::PartialFrame {
            len: samples.len(),
            frame,
            channels: format.channels,
        });
    }
    // The RIFF size field counts everything after itself: 36 header bytes plus the data.
    let data_len = u32::try_from(samples.len())
        .ok()
        .filter(|n| n.checked_add(36).is_some())
        .ok_or(WavError::TooLarge { len: samples.len() })?;
    let mut out = Vec::with_capacity(44 + samples.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size for PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // format tag 1 = integer PCM
    out.extend_from_slice(&format.channels.to_le_bytes());
    out.extend_from_slice(&format.sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(samples);
    Ok(out)
}

/// Read the PCM layout out of a raw-audio mime such as `audio/L16;codec=pcm;rate=24000`.
///
/// Returns `None` when the mime is not raw 16-bit PCM at all (`audio/wav`, `audio/mpeg`),
/// which is how a caller tells "wrap this" from "store this as-is". A raw mime with no
/// `rate` is refused by returning `Err`: the rate is the one fact a header cannot guess,
/// and a wrong guess plays at the wrong pitch.
pub fn parse_raw_pcm_mime(mime: &str) -> Option<Result<PcmFormat, String>> {
    let mut parts = mime.split(';').map(str::trim);
    let essence = parts.next()?.to_ascii_lowercase();
    if essence != "audio/l16" && essence != "audio/pcm" {
        return None;
    }
    let mut rate = None;
    let mut channels = 1u16;
    for param in parts {
        let Some((key, value)) = param.split_once('=') else {
            continue;
        };
        match key.trim().to_ascii_lowercase().as_str() {
            "rate" => rate = value.trim().parse::<u32>().ok(),
            "channels" => match value.trim().parse::<u16>() {
                Ok(n) => channels = n,
                Err(_) => return Some(Err(format!("unreadable channel count in {mime:?}"))),
            },
            _ => {}
        }
    }
    Some(match rate {
        Some(sample_rate) => Ok(PcmFormat {
            sample_rate,
            channels,
        }),
        None => Err(format!(
            "raw PCM mime {mime:?} names no `rate`, and a WAVE header cannot guess one"
        )),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const MONO_24K: PcmFormat = PcmFormat {
        sample_rate: 24_000,
        channels: 1,
    };

    fn u32_at(b: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(b[at..at + 4].try_into().unwrap())
    }

    fn u16_at(b: &[u8], at: usize) -> u16 {
        u16::from_le_bytes(b[at..at + 2].try_into().unwrap())
    }

    /// Every header field, checked by offset against the canonical WAVE layout, and the
    /// samples following untouched.
    #[test]
    fn the_header_describes_the_samples_exactly() {
        let samples: Vec<u8> = (0..48u8).collect();
        let wav = wrap_pcm16(&samples, MONO_24K).unwrap();
        assert_eq!(wav.len(), 44 + 48);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32_at(&wav, 4), 36 + 48, "RIFF size counts what follows it");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32_at(&wav, 16), 16);
        assert_eq!(u16_at(&wav, 20), 1, "integer PCM");
        assert_eq!(u16_at(&wav, 22), 1, "mono");
        assert_eq!(u32_at(&wav, 24), 24_000);
        assert_eq!(u32_at(&wav, 28), 48_000, "byte rate = rate * 2 bytes");
        assert_eq!(u16_at(&wav, 32), 2, "block align");
        assert_eq!(u16_at(&wav, 34), 16, "bits per sample");
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32_at(&wav, 40), 48);
        assert_eq!(&wav[44..], &samples[..]);
    }

    #[test]
    fn stereo_doubles_the_frame() {
        let wav = wrap_pcm16(
            &[0; 8],
            PcmFormat {
                sample_rate: 44_100,
                channels: 2,
            },
        )
        .unwrap();
        assert_eq!(u16_at(&wav, 22), 2);
        assert_eq!(u32_at(&wav, 28), 44_100 * 4);
        assert_eq!(u16_at(&wav, 32), 4);
        assert!(matches!(
            wrap_pcm16(
                &[0; 6],
                PcmFormat {
                    sample_rate: 44_100,
                    channels: 2
                }
            ),
            Err(WavError::PartialFrame { frame: 4, .. })
        ));
    }

    /// A half sample means the stream was cut or mislabelled; a header over it would
    /// play as noise, so it is refused.
    #[test]
    fn a_partial_sample_is_refused_not_padded() {
        assert!(matches!(
            wrap_pcm16(&[0, 1, 2], MONO_24K),
            Err(WavError::PartialFrame { len: 3, .. })
        ));
        assert_eq!(wrap_pcm16(&[], MONO_24K), Err(WavError::Empty));
        assert!(matches!(
            wrap_pcm16(
                &[0, 0],
                PcmFormat {
                    sample_rate: 0,
                    channels: 1
                }
            ),
            Err(WavError::InvalidFormat(_))
        ));
    }

    /// The rate and channel count come from a provider's mime, so an extreme value is
    /// refused instead of overflowing a header field into a wrong but plausible number.
    #[test]
    fn a_format_the_header_cannot_hold_is_refused() {
        for format in [
            PcmFormat {
                sample_rate: 24_000,
                channels: u16::MAX,
            },
            PcmFormat {
                sample_rate: u32::MAX,
                channels: 1,
            },
        ] {
            assert_eq!(
                wrap_pcm16(&[0; 4], format),
                Err(WavError::InvalidFormat(format)),
                "{format:?}"
            );
        }
    }

    #[test]
    fn gemini_raw_mime_parses_to_its_layout() {
        assert_eq!(
            parse_raw_pcm_mime("audio/L16;codec=pcm;rate=24000"),
            Some(Ok(MONO_24K))
        );
        assert_eq!(
            parse_raw_pcm_mime("audio/pcm; rate=16000; channels=2"),
            Some(Ok(PcmFormat {
                sample_rate: 16_000,
                channels: 2
            }))
        );
    }

    /// A container format is not raw PCM and is stored as it came.
    #[test]
    fn a_container_mime_is_not_raw() {
        assert_eq!(parse_raw_pcm_mime("audio/wav"), None);
        assert_eq!(parse_raw_pcm_mime("audio/mpeg"), None);
        assert_eq!(parse_raw_pcm_mime("image/png"), None);
    }

    /// The rate is the one fact a header cannot guess — a wrong one plays at the wrong
    /// pitch — so a raw mime without it is refused.
    #[test]
    fn a_raw_mime_without_a_rate_is_refused() {
        let got = parse_raw_pcm_mime("audio/L16;codec=pcm").expect("it is raw PCM");
        assert!(got.unwrap_err().contains("rate"));
    }
}
