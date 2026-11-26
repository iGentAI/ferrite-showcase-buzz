//! The vendored-source drift guard.
//!
//! `src/crypto/` and `src/b64.rs` are COPIES of buzz's (`buzz/src/crypto/`
//! and `buzz/src/relay/b64.rs`) — verbatim but for one mechanical path
//! rewrite — because the code gate admits no second dependency and two
//! hand-maintained copies of a signing primitive would be two chances to
//! disagree about a signature. The `crypto_kernel/` tree is
//! `@generated` on both sides from `packages/sha256-hmac`; this guard also
//! proves those generated trees agree.
//!
//! A copy nobody checks is the failure mode this file exists to prevent. The
//! transform is applied HERE, to the buzz source, and the result must equal the
//! vendored file byte for byte: an edit on either side fails the test and names
//! the file. This is the same idea as `sync-upstream.mjs --check-offline`, for
//! the same reason.

use std::path::PathBuf;

/// The mechanical transform, in one place: buzz's module paths rewritten onto
/// this crate's. Two rules, because the doctest in `sha256.rs` refers to the
/// crate from OUTSIDE (`buzz::relay::crypto`) while the code refers to it from
/// inside (`crate::relay::crypto`).
fn vendor_transform(source: &str) -> String {
    source
        .replace("crate::relay::crypto", "crate::crypto")
        .replace("buzz::relay::crypto", "buzz_process_app::crypto")
}

/// `b64.rs` sits one level up in buzz, so it also needs the bare `crate::relay`
/// prefix rewritten.
fn vendor_transform_b64(source: &str) -> String {
    vendor_transform(source)
        .replace("crate::relay::", "crate::")
        .replace("buzz::relay::", "buzz_process_app::")
}

fn repo_root() -> PathBuf {
    // CARGO_MANIFEST_DIR is `<repo>/buzz-process-app`.
    let mut root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    root.pop();
    root
}

fn read(path: &PathBuf) -> String {
    std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()))
}

#[test]
fn the_vendored_crypto_matches_its_source() {
    let root = repo_root();
    let files = [
        "mod.rs",
        "schnorr.rs",
        "field.rs",
        "scalar.rs",
        "point.rs",
        "bech32.rs",
    ];
    for name in files {
        let source = read(&root.join("buzz/src/crypto").join(name));
        let vendored = read(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("src/crypto")
                .join(name),
        );
        assert_eq!(
            vendor_transform(&source),
            vendored,
            "src/crypto/{name} has drifted from buzz/src/crypto/{name}. \
             Re-copy it with the documented transform (see this file's header)."
        );
    }
}

#[test]
fn the_generated_crypto_kernel_matches_its_source() {
    let root = repo_root();
    for name in ["mod.rs", "sha256.rs", "hmac.rs"] {
        let source = read(&root.join("buzz/src/crypto_kernel").join(name));
        let vendored = read(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("src/crypto_kernel")
                .join(name),
        );
        assert_eq!(
            source, vendored,
            "crypto_kernel/{name} differs; run ferrite-pkg sync"
        );
    }
}

#[test]
fn the_vendored_base64_matches_its_source() {
    let root = repo_root();
    let source = read(&root.join("buzz/src/relay/b64.rs"));
    let vendored = read(&PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/b64.rs"));
    assert_eq!(
        vendor_transform_b64(&source),
        vendored,
        "src/b64.rs has drifted from buzz/src/relay/b64.rs; re-copy it with the \
         documented transform"
    );
}
