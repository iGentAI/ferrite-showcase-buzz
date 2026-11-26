//! A real topic trim must reset a slow Buzz cursor before delivering a suffix.
//!
//! This gate exercises the `platform-topic-read-wait` build only: it needs
//! `TestHarness::with_topic_segment_bytes`, `trim_topic` and `get_retention`,
//! which neither the released platform builder nor the vendored
//! `ferrite-app-test` expose.
//! It is compiled out of the default build rather than left uncompilable, so
//! `cargo test` builds every binary. The shipped `read_from` pump's
//! detection is pinned by `delivery::tests::released_read_from_pages_detect_loss_only_on_a_non_empty_page`;
//! an end-to-end trim on the default build stays unverified.
#![cfg(feature = "platform-topic-read-wait")]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::indexing_slicing)]
use super::common;
use buzz::relay::delivery::check_wait_continuity;
use buzz::relay::session::{Cursor, ResetReason};
use ferrite_app_test::TestHarness;

#[test]
 fn explicit_topic_trim_resets_a_slow_buzz_cursor() {
    common::run_scenario(|| async move {
    let env = TestHarness::new().with_store("nonces").with_topics("feed").with_topic_segment_bytes(100).build().await;
    let mut feed = env.topics("feed");
    feed.create_topic("community").await.unwrap();
    for _ in 0..20 { feed.append("community", b"event", &[1; 40]).await.unwrap(); }
    let before = feed.get_retention("community").await.unwrap();
    assert_eq!(before.retained_base, 0);
    let trimmed = feed.trim_topic("community", 12).await.unwrap();
    assert!(trimmed.retained_base > 0 && trimmed.retained_base <= 12);
    let page = feed.read_wait("community", 0, 20, 4096, 1).await.unwrap();
    assert!(page.trimmed);
    assert_eq!(page.retained_base, trimmed.retained_base);
    let first = page.page.records.first().map(|record| record.offset);
    assert_eq!(check_wait_continuity(Cursor::new(1, 0), 1, page.retained_base, first), Some(ResetReason::RetentionClamped));
    // The same retained-base verdict remains decisive if filtering delivers no records.
    assert_eq!(check_wait_continuity(Cursor::new(1, 0), 1, page.retained_base, None), Some(ResetReason::RetentionClamped));
    assert_eq!(check_wait_continuity(Cursor::new(1, page.retained_base), 1, page.retained_base, first), None);
    env.shutdown().await;

    });
}
