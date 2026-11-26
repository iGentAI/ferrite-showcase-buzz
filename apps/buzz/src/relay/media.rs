//! Media on the blob plane: what may be uploaded, how it is staged, and why the
//! digest is the capability.
//!
//! Pure vocabulary and validation — the I/O lives in [`crate::relay::ingress`]
//! beside the other route handlers, mirroring the [`crate::relay::operator`]
//! split: decisions here, plumbing there.
//!
//! ## Why this module exists
//!
//! Every part of media EXCEPT the write path was already in the codebase: the
//! `media/{sha}` key builders, a `KIND_MEDIA_UPLOAD` audit kind, an
//! [`crate::relay::audit::AuditAction::MediaUploaded`] variant, the `media` blob
//! binding, and a public `/buzz/blob/` content route mapped onto the `media/`
//! key prefix. Nothing ever wrote a byte, so every attachment surface in the
//! client — image paste, file picker, avatars — had no backend at all.
//!
//! ## Three rules, each a security boundary
//!
//! **1. The name is the capability.** `/buzz/blob/` is PUBLIC. Anything under
//! `media/` is readable by anyone holding the URL. That is what lets a client
//! render a private room's image directly from the gateway without the relay
//! proxying bytes, and it is also the whole of the access control: media is not
//! channel-scoped, so a leaked URL is a leaked object. The name in the URL is an
//! HMAC of the content's SHA-256 under a per-deployment key
//! (`ingress::media::media_capability`), never the SHA-256 itself: a content
//! hash is guessable whenever the content is, and would let anyone confirm and
//! fetch a guessable private attachment. The HMAC still buys deduplication —
//! the same image posted to ten rooms is stored once — and an interrupted upload
//! is safe to retry, because identical bytes converge on the identical key.
//! Objects uploaded before the change keep their content-hash names and URLs.
//!
//! **2. The declared type must match the bytes.** The gateway derives
//! `Content-Type` from the key's EXTENSION, so whoever names the extension
//! chooses what the world is served. A caller who could declare `image/png` for
//! HTML bytes would have a stored cross-site scripting vector the moment any
//! agent sniffs the body. So [`verify_bytes`] holds the assembled object against
//! its declaration — by magic number where the format has one, and by UTF-8
//! validity for the text formats that do not — and the refusal happens before
//! anything reaches the blob plane.
//!
//! **3. Scriptable types are refused outright.** `svg`, `html`, `xml`, `js`,
//! `css` and `json` are types the gateway would serve with a markup or script
//! content type, from the SAME ORIGIN the app is served from. An uploaded SVG is
//! stored XSS with extra steps. [`extension_for_mime`] cannot produce any of
//! those extensions, which means no upload route can reach them by construction
//! rather than by remembering to check.

use ferrite_app::serde::{Deserialize, Serialize};

use crate::relay::crypto::{from_hex32, to_hex};
use crate::relay::event::PubKey;
use crate::relay::{BuzzError, Result};

/// One staged chunk, and the arithmetic that places it.
///
/// Deliberately the UI-bundle path's own constants and functions rather than a
/// second copy: "which chunk does this offset address" is already decided,
/// tested and reviewed in [`crate::relay::operator`], and two implementations of
/// it would be two chances to disagree about where a byte lands. 128 KiB raw is
/// ~171 KiB base64, comfortably inside the gateway's serialized ingress bound.
pub use crate::relay::operator::{chunk_count, chunk_index, CHUNK_BYTES};

/// The largest object one upload may carry.
///
/// Headroom under the 16 MiB fleet-default blob object bound, the same argument
/// the UI bundle's per-file cap makes: an object is assembled in memory before it
/// is written, and the app's whole cgroup is 512 MiB shared with everything else
/// it is serving.
pub const MAX_MEDIA_BYTES: u64 = 8 * 1024 * 1024;

/// How long an upload session and its staged chunks live.
///
/// Expiry IS the cleanup: an abandoned upload self-erases with no sweeper, which
/// is why staging rows live in the config store (expiring docs) rather than on
/// the blob plane, which has neither TTL nor scan. One hour is generous for an
/// 8 MiB object on a slow connection and short enough that abandoned staging does
/// not accumulate.
pub const UPLOAD_TTL_MS: u64 = 60 * 60 * 1000;

/// Uploads one pubkey may begin per [`UPLOAD_RATE_WINDOW_MS`].
///
/// Media is the one route where a caller hands the deployment durable STORAGE, so
/// the bound is on starting uploads rather than on finishing them: a caller that
/// begins a thousand sessions and abandons them all should meet the limiter, not
/// just one that completes them.
pub const UPLOAD_RATE_LIMIT: i64 = 120;
pub const UPLOAD_RATE_WINDOW_MS: u64 = 60 * 60 * 1000;

/// One accepted media type: the wire MIME, the key extension the gateway maps
/// back to a `Content-Type`, and how the bytes prove they are what they claim.
struct MediaType {
    mime: &'static str,
    ext: &'static str,
    proof: Proof,
}

/// How an object's bytes are held against its declared type.
#[derive(Clone, Copy)]
enum Proof {
    /// The format opens with a fixed signature.
    Magic(&'static [u8]),
    /// The format has SEVERAL legitimate opening signatures, and refusing the
    /// less common ones would refuse ordinary files (an empty ZIP archive is not
    /// a malformed one).
    AnyMagic(&'static [&'static [u8]]),
    /// RIFF containers (`webp`, `wav`): `RIFF....<fourcc>`, where the four bytes
    /// at offset 8 name the form. The length field between them is content, so a
    /// prefix match alone would accept any RIFF file as any other.
    Riff(&'static [u8; 4]),
    /// ISO base media (`mp4`, `avif`): a `ftyp` box whose BRANDS say which format
    /// this actually is. The box name alone does not — it is shared by both — so
    /// each type names the brands it accepts and the brands that disqualify it.
    Iso {
        accept: &'static [&'static [u8; 4]],
        reject: &'static [&'static [u8; 4]],
    },
    /// MPEG audio: either an `ID3` tag or a bare frame sync, because a valid MP3
    /// is not obliged to carry a tag.
    Mp3,
    /// No signature exists, so the proof is that the bytes are valid UTF-8 — which
    /// is what makes them renderable as the text type they claim, and is served
    /// as `text/plain`, a type nothing executes.
    Utf8,
}

/// Brands that mark a file as AVIF rather than plain ISO base media.
const AVIF_BRANDS: &[&[u8; 4]] = &[b"avif", b"avis"];
/// Brands that mark a file as MP4 video.
const MP4_BRANDS: &[&[u8; 4]] = &[
    b"isom", b"iso2", b"iso4", b"iso5", b"iso6", b"mp41", b"mp42", b"mmp4", b"avc1", b"dash",
    b"M4V ", b"M4A ",
];

/// Every media type this deployment accepts.
///
/// Scriptable and markup types are ABSENT by construction, not filtered later:
/// there is no row here for `image/svg+xml`, `text/html`, `application/xml`,
/// `text/javascript`, `text/css` or `application/json`, so no caller can name one
/// and no key can carry those extensions. See the module docs for why that
/// matters on a public, same-origin content route.
const MEDIA_TYPES: &[MediaType] = &[
    // Images — the overwhelming majority of chat media.
    MediaType {
        mime: "image/png",
        ext: "png",
        proof: Proof::Magic(b"\x89PNG\r\n\x1a\n"),
    },
    MediaType {
        mime: "image/jpeg",
        ext: "jpg",
        proof: Proof::Magic(b"\xff\xd8\xff"),
    },
    MediaType {
        mime: "image/gif",
        ext: "gif",
        proof: Proof::Magic(b"GIF8"),
    },
    MediaType {
        mime: "image/webp",
        ext: "webp",
        proof: Proof::Riff(b"WEBP"),
    },
    MediaType {
        mime: "image/avif",
        ext: "avif",
        proof: Proof::Iso {
            accept: AVIF_BRANDS,
            reject: MP4_BRANDS,
        },
    },
    // Video and audio.
    MediaType {
        mime: "video/mp4",
        ext: "mp4",
        proof: Proof::Iso {
            accept: MP4_BRANDS,
            reject: AVIF_BRANDS,
        },
    },
    MediaType {
        mime: "video/webm",
        ext: "webm",
        proof: Proof::Magic(b"\x1a\x45\xdf\xa3"),
    },
    MediaType {
        mime: "audio/mpeg",
        ext: "mp3",
        proof: Proof::Mp3,
    },
    MediaType {
        mime: "audio/ogg",
        ext: "ogg",
        proof: Proof::Magic(b"OggS"),
    },
    MediaType {
        mime: "audio/wav",
        ext: "wav",
        proof: Proof::Riff(b"WAVE"),
    },
    // Documents. `pdf` and `zip` have signatures; the text forms do not, and are
    // served as `text/plain`, which is not a scriptable context.
    MediaType {
        mime: "application/pdf",
        ext: "pdf",
        proof: Proof::Magic(b"%PDF-"),
    },
    // Local file header, empty archive (end-of-central-directory only), and
    // spanned archive — all three are valid ZIPs.
    MediaType {
        mime: "application/zip",
        ext: "zip",
        proof: Proof::AnyMagic(&[b"PK\x03\x04", b"PK\x05\x06", b"PK\x07\x08"]),
    },
    MediaType {
        mime: "text/plain",
        ext: "txt",
        proof: Proof::Utf8,
    },
    MediaType {
        mime: "text/csv",
        ext: "csv",
        proof: Proof::Utf8,
    },
    MediaType {
        mime: "text/markdown",
        ext: "md",
        proof: Proof::Utf8,
    },
];

/// Look one declared MIME up, ignoring any `; charset=` parameter and case.
fn media_type(mime: &str) -> Option<&'static MediaType> {
    let bare = mime
        .split(';')
        .next()
        .unwrap_or(mime)
        .trim()
        .to_ascii_lowercase();
    MEDIA_TYPES.iter().find(|row| row.mime == bare)
}

/// The key extension for a declared MIME, or `None` when this deployment does
/// not accept that type.
///
/// This function IS the allowlist: an extension it cannot produce is an extension
/// no media key can carry, which is what keeps scriptable types off a public
/// same-origin route by construction.
#[must_use]
pub fn extension_for_mime(mime: &str) -> Option<&'static str> {
    media_type(mime).map(|row| row.ext)
}

/// Every MIME this deployment accepts, for the client's file picker to filter on.
#[must_use]
pub fn accepted_mimes() -> Vec<&'static str> {
    MEDIA_TYPES.iter().map(|row| row.mime).collect()
}

/// The `Content-Type` to serve a stored object with, from its key's extension.
///
/// The inverse of [`extension_for_mime`], over the same allowlist: an extension
/// no accepted type produces gets `None`, and the caller refuses to serve it
/// rather than guessing. The text types carry an explicit UTF-8 charset, which
/// is what [`verify_bytes`] proved they are.
#[must_use]
pub fn mime_for_extension(ext: &str) -> Option<String> {
    let row = MEDIA_TYPES.iter().find(|row| row.ext == ext)?;
    Some(match row.proof {
        Proof::Utf8 => format!("{}; charset=utf-8", row.mime),
        _ => row.mime.to_string(),
    })
}

/// Every brand an ISO base-media file declares: its major brand, then each
/// compatible brand, bounded BOTH by the box length it declares and by the bytes
/// actually held — a truncated upload or a lying length must not walk off the
/// buffer, and `clippy::indexing_slicing` is denied here precisely so that class
/// of bug cannot be written.
fn iso_brands(bytes: &[u8]) -> Option<Vec<[u8; 4]>> {
    // A `ftyp` box is at MINIMUM 16 bytes — length, name, major brand, minor
    // version — so anything shorter cannot be one. Checking it here is also what
    // makes the walk below well-formed: `clamp(16, len)` panics outright when
    // `len < 16`, which an 8-to-15 byte upload containing `ftyp` would reach.
    if bytes.len() < 16 {
        return None;
    }
    if bytes.get(4..8)? != b"ftyp" {
        return None;
    }
    let declared = u32::from_be_bytes(<[u8; 4]>::try_from(bytes.get(0..4)?).ok()?) as usize;
    // A box claiming to be smaller than its own header is malformed. Refused
    // rather than rounded up, so a bad length cannot be promoted into a
    // valid-looking box.
    if declared < 16 {
        return None;
    }
    // Never past what we actually hold, whatever the length field claims.
    let end = declared.min(bytes.len());
    let mut brands = Vec::new();
    if let Some(major) = bytes.get(8..12).and_then(|b| <[u8; 4]>::try_from(b).ok()) {
        brands.push(major);
    }
    let mut at: usize = 16;
    while at.saturating_add(4) <= end {
        if let Some(brand) = bytes
            .get(at..at.saturating_add(4))
            .and_then(|b| <[u8; 4]>::try_from(b).ok())
        {
            brands.push(brand);
        }
        at = at.saturating_add(4);
    }
    Some(brands)
}

/// Whether an ISO base-media file's brands identify it as the declared type.
///
/// A disqualifying brand loses outright, before any accept is considered: brand
/// lists are open-ended, so a file naming both an AVIF and an MP4 brand is
/// ambiguous, and an ambiguous file must be refused rather than classified by
/// whichever check happened to run first.
fn iso_is(bytes: &[u8], accept: &[&[u8; 4]], reject: &[&[u8; 4]]) -> bool {
    let Some(brands) = iso_brands(bytes) else {
        return false;
    };
    if brands
        .iter()
        .any(|brand| reject.iter().any(|r| r.as_slice() == brand.as_slice()))
    {
        return false;
    }
    brands
        .iter()
        .any(|brand| accept.iter().any(|a| a.as_slice() == brand.as_slice()))
}

/// Whether these bytes open as MPEG audio: an `ID3` tag, or a bare frame sync
/// (eleven set bits) for the many valid MP3s that carry no tag at all.
fn is_mpeg_audio(bytes: &[u8]) -> bool {
    if bytes.starts_with(b"ID3") {
        return true;
    }
    match (bytes.first(), bytes.get(1)) {
        (Some(0xff), Some(second)) => second & 0xe0 == 0xe0,
        _ => false,
    }
}

/// Hold assembled bytes against the type they were declared as.
///
/// The gateway serves `Content-Type` from the key's extension, so this is what
/// stops a caller choosing what the world is served for their bytes. Refused
/// AFTER assembly and BEFORE the blob write, so a mismatch costs the caller a
/// round trip and costs the deployment nothing durable.
pub fn verify_bytes(mime: &str, bytes: &[u8]) -> Result<()> {
    let Some(row) = media_type(mime) else {
        return Err(BuzzError::InvalidEvent(format!(
            "{mime} is not a media type this deployment accepts"
        )));
    };
    let ok = match row.proof {
        Proof::Magic(signature) => bytes.starts_with(signature),
        Proof::AnyMagic(signatures) => signatures.iter().any(|s| bytes.starts_with(s)),
        // `RIFF` + 4 length bytes + the form's own fourcc. The length is content,
        // so the fourcc has to be read at its offset rather than matched as part
        // of one prefix — otherwise every RIFF file passes as every other.
        Proof::Riff(form) => {
            bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(form.as_slice())
        }
        // An ISO base-media file's BRANDS say what it is; the `ftyp` box name is
        // shared by every format in the family, AVIF and MP4 included.
        Proof::Iso { accept, reject } => iso_is(bytes, accept, reject),
        Proof::Mp3 => is_mpeg_audio(bytes),
        Proof::Utf8 => core::str::from_utf8(bytes).is_ok(),
    };
    if !ok {
        return Err(BuzzError::InvalidEvent(format!(
            "the uploaded bytes are not {mime}; the declared type must match the content, \
             because it decides the Content-Type this object is served with"
        )));
    }
    Ok(())
}

/// A validated upload declaration: canonical digest, accepted type, and the key
/// extension that will carry it.
pub struct Declaration {
    pub sha256: String,
    pub size: u64,
    pub mime: String,
    pub ext: &'static str,
}

/// Validate what a caller says they are about to upload.
///
/// Everything checkable WITHOUT the bytes happens here, so a caller learns their
/// upload is unacceptable before sending 8 MiB of it. What cannot be checked yet
/// — that the bytes hash to this digest and match this type — is checked on
/// assembly by [`verify_bytes`] and the digest comparison in the route.
pub fn validate_declaration(sha256: &str, size: u64, mime: &str) -> Result<Declaration> {
    let Some(digest) = from_hex32(sha256) else {
        return Err(BuzzError::InvalidEvent(
            "a media upload needs a 32-byte hex sha256 of its content".into(),
        ));
    };
    if size == 0 {
        return Err(BuzzError::InvalidEvent(
            "an empty object cannot be uploaded".into(),
        ));
    }
    if size > MAX_MEDIA_BYTES {
        return Err(BuzzError::TooLarge {
            got: usize::try_from(size).unwrap_or(usize::MAX),
            max: usize::try_from(MAX_MEDIA_BYTES).unwrap_or(usize::MAX),
        });
    }
    let Some(ext) = extension_for_mime(mime) else {
        return Err(BuzzError::InvalidEvent(format!(
            "{mime} is not a media type this deployment accepts"
        )));
    };
    Ok(Declaration {
        sha256: to_hex(&digest),
        size,
        mime: mime
            .split(';')
            .next()
            .unwrap_or(mime)
            .trim()
            .to_ascii_lowercase(),
        ext,
    })
}

/// One in-flight media upload, written with a TTL at `keys::media_upload(id)`.
///
/// Written ONCE at `begin` and never rewritten — the UI-bundle path's hard-won
/// shape. Per-chunk state lives in its own expiring rows, so staging a chunk is
/// one unconditional write rather than a read-modify-write of a growing document
/// that concurrent writers contend on.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct MediaUpload {
    /// Lowercase hex digest of the content, as declared at `begin`.
    pub sha256: String,
    pub size: u64,
    pub mime: String,
    /// The key extension, decided by the allowlist at `begin` rather than taken
    /// from a caller-supplied filename.
    pub ext: String,
    /// Hex pubkey of the uploader, for the audit line.
    pub by: String,
    pub created_ms: u64,
}

impl MediaUpload {
    /// When this session and every chunk staged under it expire. Derived from
    /// `created_ms`, never from "now", so a late chunk cannot extend an upload's
    /// life indefinitely by refreshing it.
    #[must_use]
    pub const fn expires_at_ms(&self) -> u64 {
        self.created_ms.saturating_add(UPLOAD_TTL_MS)
    }

    /// How many chunks this object stages.
    #[must_use]
    pub fn chunks(&self) -> u64 {
        chunk_count(self.size)
    }
}

/// Mint an upload id: 16 hex over the request's correlation id, the instant, and
/// the caller — the same "no entropy source, so derive from per-request
/// uniqueness" rule the invite nonce and the UI-bundle upload id both use.
#[must_use]
pub fn upload_id(correlation_id: &str, now_ms: u64, by: &PubKey) -> String {
    crate::relay::operator::upload_id(correlation_id, now_ms, by)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const SHA: &str = "abababababababababababababababababababababababababababababababab";

    #[test]
    fn every_accepted_extension_maps_back_to_its_type_and_nothing_else_does() {
        for mime in accepted_mimes() {
            let ext = extension_for_mime(mime).unwrap();
            let served = mime_for_extension(ext).unwrap();
            assert!(served.starts_with(mime), "{ext} served as {served}");
        }
        for scriptable in ["svg", "html", "htm", "js", "css", "json", "xml", ""] {
            assert_eq!(mime_for_extension(scriptable), None, "{scriptable}");
        }
        assert_eq!(
            mime_for_extension("txt").as_deref(),
            Some("text/plain; charset=utf-8")
        );
    }

    #[test]
    fn the_allowlist_cannot_produce_a_scriptable_extension() {
        // The security property is that these are ABSENT, not filtered: no caller
        // can name one, so no key can carry one, so the gateway can never serve a
        // user-supplied object as markup or script from the app's own origin.
        for scriptable in [
            "image/svg+xml",
            "text/html",
            "application/xml",
            "text/xml",
            "text/javascript",
            "application/javascript",
            "text/css",
            "application/json",
            "application/xhtml+xml",
        ] {
            assert_eq!(
                extension_for_mime(scriptable),
                None,
                "accepted {scriptable}"
            );
        }
        // And no accepted row carries one of those extensions by another name.
        for row in MEDIA_TYPES {
            assert!(
                !matches!(
                    row.ext,
                    "svg" | "html" | "htm" | "xml" | "js" | "mjs" | "css" | "json"
                ),
                "{} maps to the scriptable extension {}",
                row.mime,
                row.ext
            );
        }
    }

    #[test]
    fn a_declared_type_is_matched_case_and_parameter_insensitively() {
        assert_eq!(extension_for_mime("image/png"), Some("png"));
        assert_eq!(extension_for_mime("IMAGE/PNG"), Some("png"));
        assert_eq!(extension_for_mime("text/plain; charset=utf-8"), Some("txt"));
        assert_eq!(extension_for_mime("  image/webp  "), Some("webp"));
        assert_eq!(extension_for_mime("image/png-ish"), None);
        assert_eq!(extension_for_mime(""), None);
    }

    #[test]
    fn bytes_must_actually_be_the_type_they_claim() {
        // The attack this closes: HTML bytes declared as an image would be served
        // as image/png from the app's own origin, and any content sniffing turns
        // that into stored XSS.
        let html = b"<html><script>alert(1)</script></html>";
        assert!(verify_bytes("image/png", html).is_err());
        assert!(verify_bytes("image/jpeg", html).is_err());
        assert!(verify_bytes("application/pdf", html).is_err());

        // Real signatures pass.
        assert!(verify_bytes("image/png", b"\x89PNG\r\n\x1a\n rest").is_ok());
        assert!(verify_bytes("image/jpeg", b"\xff\xd8\xff\xe0 rest").is_ok());
        assert!(verify_bytes("image/gif", b"GIF89a rest").is_ok());
        assert!(verify_bytes("application/pdf", b"%PDF-1.7 rest").is_ok());
        assert!(verify_bytes("application/zip", b"PK\x03\x04 rest").is_ok());
        assert!(verify_bytes("audio/ogg", b"OggS rest").is_ok());
        assert!(verify_bytes("video/webm", b"\x1a\x45\xdf\xa3 rest").is_ok());
    }

    #[test]
    fn a_riff_container_must_name_its_own_form() {
        // `RIFF` + length + fourcc. A prefix match alone would accept a WAV as a
        // WEBP, because the four bytes between them are a length, not a type.
        let webp = b"RIFF\x24\x00\x00\x00WEBPVP8 ";
        let wav = b"RIFF\x24\x00\x00\x00WAVEfmt ";
        assert!(verify_bytes("image/webp", webp).is_ok());
        assert!(verify_bytes("audio/wav", wav).is_ok());
        assert!(
            verify_bytes("image/webp", wav).is_err(),
            "a WAV passed as WEBP"
        );
        assert!(
            verify_bytes("audio/wav", webp).is_err(),
            "a WEBP passed as WAV"
        );
        // Truncated headers are refused rather than panicking on a short slice.
        assert!(verify_bytes("image/webp", b"RIFF").is_err());
        assert!(verify_bytes("image/webp", b"").is_err());
    }

    #[test]
    fn an_iso_media_file_is_identified_by_its_brands_not_its_box_name() {
        // `ftyp` is shared by the whole ISO base-media family, so the box name
        // alone cannot tell AVIF from MP4 — and the declared type picks the key
        // extension, which picks the Content-Type the world is served.
        let mp4 = b"\x00\x00\x00\x18ftypisom\x00\x00\x02\x00isomiso2";
        let avif = b"\x00\x00\x00\x1cftypavif\x00\x00\x00\x00avifmif1miaf";
        assert!(verify_bytes("video/mp4", mp4).is_ok());
        assert!(verify_bytes("image/avif", avif).is_ok());
        // The cross-type pairs that the shared check used to accept.
        assert!(
            verify_bytes("image/avif", mp4).is_err(),
            "an MP4 passed as AVIF"
        );
        assert!(
            verify_bytes("video/mp4", avif).is_err(),
            "an AVIF passed as MP4"
        );

        // An AVIF whose MAJOR brand is `mif1` still declares `avif` among its
        // compatible brands, which is why those are scanned too.
        let mif1 = b"\x00\x00\x00\x1cftypmif1\x00\x00\x00\x00mif1avifmiaf";
        assert!(verify_bytes("image/avif", mif1).is_ok());
        assert!(verify_bytes("video/mp4", mif1).is_err());

        // An unrelated ISO brand is neither.
        let heic = b"\x00\x00\x00\x18ftypheic\x00\x00\x00\x00heicmif1";
        assert!(verify_bytes("image/avif", heic).is_err());
        assert!(verify_bytes("video/mp4", heic).is_err());

        // Not ISO media at all, and every truncation, are REFUSED — asserted
        // strictly, because a test for a security boundary that tolerates
        // acceptance is not a test. The 8-to-15 byte cases are the ones that
        // used to panic inside the parser rather than return.
        for hostile in [
            &b"not an mp4 at all"[..],
            &b"\x00\x00"[..],
            &b""[..],
            &b"\x00\x00\x00\x18ftyp"[..],
            &b"\x00\x00\x00\x18ftypis"[..],
            &b"\x00\x00\x00\x18ftypisom"[..],
            // A length field far larger than the bytes actually held.
            &b"\xff\xff\xff\xffftypavif"[..],
            // A length field smaller than the box's own header.
            &b"\x00\x00\x00\x04ftypisom"[..],
        ] {
            assert!(
                verify_bytes("video/mp4", hostile).is_err(),
                "mp4 accepted {hostile:?}"
            );
            assert!(
                verify_bytes("image/avif", hostile).is_err(),
                "avif accepted {hostile:?}"
            );
        }
    }

    #[test]
    fn a_sixteen_byte_ftyp_box_is_the_smallest_that_can_be_classified() {
        // Exactly the header: length, name, major brand, minor version. The
        // major brand alone is enough to classify it, and one byte less is not a
        // box at all.
        let exact = b"\x00\x00\x00\x10ftypavif\x00\x00\x00\x00";
        assert_eq!(exact.len(), 16);
        assert!(verify_bytes("image/avif", exact).is_ok());
        assert!(verify_bytes("video/mp4", exact).is_err());
        assert!(verify_bytes("image/avif", &exact[..15]).is_err());
    }

    #[test]
    fn an_ambiguous_brand_list_is_refused_by_both() {
        // Brand lists are open-ended, so a file naming both families is
        // ambiguous — and an ambiguous file must not be classified by whichever
        // check ran first.
        let both = b"\x00\x00\x00\x20ftypisom\x00\x00\x00\x00isomavif";
        assert!(verify_bytes("video/mp4", both).is_err());
        assert!(verify_bytes("image/avif", both).is_err());
    }

    #[test]
    fn the_legitimate_mp3_and_zip_variants_are_accepted() {
        // An MP3 is not obliged to carry an ID3 tag; a bare frame sync is a
        // perfectly ordinary file, and refusing it would refuse real uploads.
        assert!(verify_bytes("audio/mpeg", b"ID3\x04\x00").is_ok());
        assert!(verify_bytes("audio/mpeg", b"\xff\xfb\x90\x00").is_ok());
        assert!(verify_bytes("audio/mpeg", b"\xff\xf3\x00\x00").is_ok());
        assert!(
            verify_bytes("audio/mpeg", b"\xff\x00\x00\x00").is_err(),
            "no frame sync"
        );
        assert!(verify_bytes("audio/mpeg", b"").is_err());

        // An empty archive carries only an end-of-central-directory record, and a
        // spanned one opens with its own marker.
        assert!(verify_bytes("application/zip", b"PK\x03\x04 rest").is_ok());
        assert!(verify_bytes("application/zip", b"PK\x05\x06\x00\x00").is_ok());
        assert!(verify_bytes("application/zip", b"PK\x07\x08 rest").is_ok());
        assert!(verify_bytes("application/zip", b"PK bogus").is_err());
    }

    #[test]
    fn signature_less_text_is_held_to_being_text() {
        assert!(verify_bytes("text/plain", "hello \u{1f41d}".as_bytes()).is_ok());
        assert!(verify_bytes("text/csv", b"a,b\n1,2\n").is_ok());
        // Invalid UTF-8 is not text, whatever it was declared as.
        assert!(verify_bytes("text/plain", &[0xff, 0xfe, 0x00]).is_err());
    }

    #[test]
    fn a_declaration_is_bounded_and_canonicalized() {
        let d = validate_declaration(&SHA.to_uppercase(), 1024, "IMAGE/PNG").unwrap();
        assert_eq!(d.sha256, SHA, "the digest canonicalizes to lowercase");
        assert_eq!(d.ext, "png");
        assert_eq!(d.mime, "image/png");

        assert!(validate_declaration("zz", 10, "image/png").is_err());
        assert!(
            validate_declaration(SHA, 0, "image/png").is_err(),
            "empty accepted"
        );
        assert!(validate_declaration(SHA, MAX_MEDIA_BYTES, "image/png").is_ok());
        assert!(validate_declaration(SHA, MAX_MEDIA_BYTES + 1, "image/png").is_err());
        assert!(validate_declaration(SHA, 10, "image/svg+xml").is_err());
    }

    #[test]
    fn an_upload_expires_from_its_start_not_from_now() {
        // The self-erase contract: a late chunk must not be able to keep an
        // abandoned upload alive by refreshing its rows.
        let upload = MediaUpload {
            sha256: SHA.into(),
            size: 3 * CHUNK_BYTES as u64,
            mime: "image/png".into(),
            ext: "png".into(),
            by: SHA.into(),
            created_ms: 1_700_000_000_000,
        };
        assert_eq!(upload.expires_at_ms(), upload.created_ms + UPLOAD_TTL_MS);
        assert_eq!(upload.chunks(), 3);
        let mut late = upload.clone();
        late.created_ms = u64::MAX - 5;
        assert_eq!(late.expires_at_ms(), u64::MAX, "saturating, never wrapping");
    }
}
