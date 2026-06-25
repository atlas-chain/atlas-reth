//! End-to-end narrative test exercising the whole Arkiv stack.
//!
//! Boots an Arkiv-enabled `EthereumNode` once, then walks through a story:
//!
//! 1. CREATE — three signers, varied payloads, content types, and
//!    attribute mixes (strings, numerics, entity-key refs).
//! 2. Query built-ins — `$owner`, `$contentType`, `$creator`.
//! 3. Query user annotations — string equality + numeric equality.
//! 4. Query boolean combinators — `AND`, `OR`, nested parens.
//! 5. Inclusion — `IN`, `NOT IN`.
//! 6. UPDATE — payload + annotation change; verify the old-value
//!    bitmap empties out and the new-value bitmap fills.
//! 7. EXTEND — `$expiration` moves cleanly.
//! 8. TRANSFER — `$owner` moves; non-owner UPDATE reverts.
//! 9. DELETE — entity disappears from every bitmap.
//! 10. atBlock — historical query observes pre-transfer state.
//! 11. Pagination — 30 entities, page_size=10, follow cursor across
//!     three pages with no overlap.
//! 12. Range queries — `>`, `>=`, `<`, `<=` on numeric attributes and
//!     `$createdAtBlock`.
//! 13. Glob queries — `~` / `!~` prefix matching on `$contentType`
//!     and user string attributes.
//!
//! All op submission, ABI encoding, signing, and query plumbing
//! lives in [`arkiv_e2e`] (the crate's `src/lib.rs`). This file is
//! pure narrative + assertions.

use alloy_primitives::{Address, B256};
use arkiv_e2e::{
    ATTR_ENTITY_KEY, ATTR_STRING, ATTR_UINT, CreateOp, OP_UPDATE, Operation, UpdateOp, WorldOps,
    boot,
};
use arkiv_node::rpc::EntityData;

fn ids_owned_by(results: &[EntityData], owner: Address) -> Vec<B256> {
    results
        .iter()
        .filter(|e| e.owner == Some(owner))
        .map(|e| e.key)
        .collect()
}

#[tokio::test(flavor = "multi_thread")]
async fn full_pipeline() -> eyre::Result<()> {
    let mut world = boot().await?;
    let alice = world.address(0);
    let bob = world.address(1);
    let carol = world.address(2);

    // ── 1. CREATE: three entities, varied annotation mix ────────────
    //
    // alice: text/plain + tag="music" + score=42
    // bob:   text/html  + tag="news"  + score=7
    // carol: image/png  + tag="music" (only) + ref → alice's entity key

    let alice_key = world
        .create(
            0,
            CreateOp::new()
                .payload(b"alice payload".to_vec())
                .content_type("text/plain")
                .btl(1_000)
                .string_attr("tag", "music")
                .numeric_attr("score", 42),
        )
        .await?;

    let bob_key = world
        .create(
            1,
            CreateOp::new()
                .payload(b"bob payload".to_vec())
                .content_type("text/html")
                .btl(1_000)
                .string_attr("tag", "news")
                .numeric_attr("score", 7),
        )
        .await?;

    let carol_key = world
        .create(
            2,
            CreateOp::new()
                .payload(b"carol payload".to_vec())
                .content_type("image/png")
                .btl(1_000)
                .string_attr("tag", "music")
                .entity_key_attr("ref", alice_key),
        )
        .await?;

    let all = world.query("*").await?;
    assert_eq!(all.len(), 3, "all entities count");

    // Sanity: every entity's `key` round-trips through the wire format.
    let returned_keys: Vec<B256> = all.iter().map(|e| e.key).collect();
    assert!(returned_keys.contains(&alice_key));
    assert!(returned_keys.contains(&bob_key));
    assert!(returned_keys.contains(&carol_key));

    // ── 2. Query built-ins ──────────────────────────────────────────

    let alice_owned = world.query(&format!("$owner = {:#x}", alice)).await?;
    assert_eq!(ids_owned_by(&alice_owned, alice), vec![alice_key]);

    let html = world.query(r#"$contentType = "text/html""#).await?;
    assert_eq!(html.len(), 1);
    assert_eq!(html[0].key, bob_key);

    let bob_created = world.query(&format!("$creator = {:#x}", bob)).await?;
    assert_eq!(bob_created.len(), 1);
    assert_eq!(bob_created[0].key, bob_key);

    let by_key = world.query(&format!("$key = {:#x}", alice_key)).await?;
    assert_eq!(by_key.len(), 1);
    assert_eq!(by_key[0].key, alice_key);

    // ── 3. Query user annotations ───────────────────────────────────

    let music = world.query(r#"tag = "music""#).await?;
    assert_eq!(music.len(), 2, "alice + carol tagged music");

    let score_42 = world.query("score = 42").await?;
    assert_eq!(score_42.len(), 1);
    assert_eq!(score_42[0].key, alice_key);

    // Attribute wire-shape round-trip — discriminator survives through
    // storage + RPC, and per-type serialization matches the SDK contract
    // (UINT → decimal string, STRING → UTF-8, ENTITY_KEY → `0x` + 64
    // hex chars).
    let carol_entity = music
        .iter()
        .find(|e| e.key == carol_key)
        .expect("carol in tag=music results");
    assert_eq!(carol_entity.attributes.len(), 2);

    let tag_attr = carol_entity
        .attributes
        .iter()
        .find(|a| a.key == "tag")
        .expect("tag attr on carol");
    assert_eq!(tag_attr.value_type, ATTR_STRING);
    assert_eq!(tag_attr.value, "music");

    let ref_attr = carol_entity
        .attributes
        .iter()
        .find(|a| a.key == "ref")
        .expect("ref attr on carol");
    assert_eq!(ref_attr.value_type, ATTR_ENTITY_KEY);
    assert_eq!(ref_attr.value, format!("{:#x}", alice_key));

    // Query by a user-defined ENTITY_KEY attribute as a predicate (not
    // just the built-in `$key`): carol is the only entity whose `ref`
    // points at alice.
    let by_ref = world.query(&format!("ref = {:#x}", alice_key)).await?;
    assert_eq!(by_ref.len(), 1);
    assert_eq!(by_ref[0].key, carol_key);

    let alice_entity = music
        .iter()
        .find(|e| e.key == alice_key)
        .expect("alice in tag=music results");
    let score_attr = alice_entity
        .attributes
        .iter()
        .find(|a| a.key == "score")
        .expect("score attr on alice");
    assert_eq!(score_attr.value_type, ATTR_UINT);
    assert_eq!(score_attr.value, "42");

    // ── 4. Boolean combinators ──────────────────────────────────────

    let plain_and_music = world
        .query(r#"$contentType = "text/plain" && tag = "music""#)
        .await?;
    assert_eq!(plain_and_music.len(), 1);
    assert_eq!(plain_and_music[0].key, alice_key);

    let news_or_image = world
        .query(r#"tag = "news" || $contentType = "image/png""#)
        .await?;
    assert_eq!(news_or_image.len(), 2, "bob + carol");

    // Nested parens — `(a || b) && c`
    let q = format!(
        r#"(tag = "music" || tag = "news") && $owner = {:#x}"#,
        alice,
    );
    assert_eq!(world.query(&q).await?.len(), 1, "alice has tag=music");

    // NOT around a paren
    let q = r#"NOT (tag = "music")"#;
    let not_music = world.query(q).await?;
    assert_eq!(not_music.len(), 1);
    assert_eq!(not_music[0].key, bob_key);

    // ── 5. Inclusion ────────────────────────────────────────────────

    let in_tags = world.query(r#"tag IN ("music" "news")"#).await?;
    assert_eq!(in_tags.len(), 3);

    let not_in_tags = world.query(r#"tag NOT IN ("music")"#).await?;
    assert_eq!(not_in_tags.len(), 1);
    assert_eq!(not_in_tags[0].key, bob_key);

    let by_score_set = world.query("score IN (7 42)").await?;
    assert_eq!(by_score_set.len(), 2, "alice + bob have those scores");

    // ── 6. UPDATE: change payload + annotations ─────────────────────

    world
        .update(
            0,
            alice_key,
            UpdateOp::new()
                .payload(b"alice v2".to_vec())
                .content_type("text/plain")
                .string_attr("tag", "podcast")
                .numeric_attr("score", 100),
        )
        .await?;

    let old_tag = world.query(r#"tag = "music""#).await?;
    assert_eq!(old_tag.len(), 1, "only carol still has tag=music");
    assert_eq!(old_tag[0].key, carol_key);

    let new_tag = world.query(r#"tag = "podcast""#).await?;
    assert_eq!(new_tag.len(), 1);
    assert_eq!(new_tag[0].key, alice_key);
    let payload_ref = new_tag[0]
        .payload_ref
        .as_ref()
        .expect("query returns payload reference, not raw payload bytes");
    assert_eq!(payload_ref.content_type.as_deref(), Some("text/plain"));
    assert_eq!(payload_ref.size_bytes, b"alice v2".len() as u64);

    // Old score=42 bitmap should be empty for alice.
    assert!(world.query("score = 42").await?.is_empty());
    let score_100 = world.query("score = 100").await?;
    assert_eq!(score_100.len(), 1);
    assert_eq!(score_100[0].key, alice_key);

    // ── 7. EXTEND ───────────────────────────────────────────────────

    let bob_entity = world.query(&format!("$key = {:#x}", bob_key)).await?;
    let old_expiration = bob_entity[0]
        .expires_at
        .expect("expires_at included by default");
    world.extend(1, bob_key, 5_000).await?;

    let bob_entity = world.query(&format!("$key = {:#x}", bob_key)).await?;
    let new_expiration = bob_entity[0]
        .expires_at
        .expect("expires_at included by default");
    assert!(
        new_expiration > old_expiration,
        "extend should raise expiration: {old_expiration} -> {new_expiration}"
    );

    // Old expiration bitmap is empty; new one contains bob.
    assert!(
        world
            .query(&format!("$expiration = {old_expiration}"))
            .await?
            .is_empty()
    );
    let still_bob = world
        .query(&format!("$expiration = {new_expiration}"))
        .await?;
    assert_eq!(still_bob.len(), 1);
    assert_eq!(still_bob[0].key, bob_key);

    // ── 8. TRANSFER + negative-path UPDATE from old owner ───────────

    // Capture the head BEFORE the transfer for the historical assertion.
    let block_before_transfer = world.head_block().await?;

    world.transfer(0, alice_key, carol).await?;

    let alice_owned = world.query(&format!("$owner = {:#x}", alice)).await?;
    assert!(
        alice_owned.is_empty(),
        "alice no longer owns anything: {:?}",
        alice_owned.iter().map(|e| e.key).collect::<Vec<_>>()
    );
    let carol_owned = world.query(&format!("$owner = {:#x}", carol)).await?;
    let carol_keys: Vec<B256> = carol_owned.iter().map(|e| e.key).collect();
    assert!(carol_keys.contains(&alice_key));
    assert!(carol_keys.contains(&carol_key));

    // Negative: alice (the old owner) tries to UPDATE — the precompile
    // enforces the owner check and reverts with the `NotOwner` selector,
    // surfacing as status=0x0 to the caller.
    let bad_update = Operation {
        operationType: OP_UPDATE,
        entityKey: alice_key,
        payload: alloy_primitives::Bytes::from_static(b"sneaky"),
        contentType: arkiv_e2e::Mime128 {
            data: Default::default(),
        },
        attributes: vec![],
        btl: 0,
        newOwner: Address::ZERO,
    };
    world
        .submit_expecting_revert(0, bad_update, "UPDATE by non-owner")
        .await?;

    // ── 9. DELETE — bob deletes his entity ──────────────────────────

    world.delete(1, bob_key).await?;

    let bob_gone = world.query(&format!("$key = {:#x}", bob_key)).await?;
    assert!(
        bob_gone.is_empty(),
        "bob's entity should be gone from $key bitmap"
    );

    let by_news = world.query(r#"tag = "news""#).await?;
    assert!(by_news.is_empty(), "bob's tag=news bitmap should be empty");

    let by_owner_bob = world.query(&format!("$owner = {:#x}", bob)).await?;
    assert!(by_owner_bob.is_empty(), "$owner=bob bitmap should be empty");

    // ── 10. Historical query (atBlock) ──────────────────────────────
    //
    // Re-run the alice-owned query at `block_before_transfer` and
    // expect alice's entity back, even though at head it belongs to
    // carol.

    let historical = world
        .query_at(&format!("$owner = {:#x}", alice), block_before_transfer)
        .await?;
    assert_eq!(
        historical.len(),
        1,
        "at block {block_before_transfer} alice still owned 1 entity",
    );
    assert_eq!(historical[0].key, alice_key);

    // ── 11. Pagination: 30 entities, page_size=10, follow cursor ───
    //
    // Bulk-create 30 entities owned by alice with a shared
    // `bulk=true` annotation so we can isolate them from the
    // pre-existing ones.

    for i in 0..30 {
        world
            .create(
                0,
                CreateOp::new()
                    .payload(format!("bulk-{i}").into_bytes())
                    .content_type("application/octet-stream")
                    .btl(1_000)
                    .string_attr("bulk", "true"),
            )
            .await?;
    }

    let all_bulk = world.query_paginated(r#"bulk = "true""#, 10).await?;
    assert_eq!(
        all_bulk.len(),
        30,
        "all 30 bulk entities returned across pages"
    );

    // No duplicates between pages.
    let mut keys: Vec<B256> = all_bulk.iter().map(|e| e.key).collect();
    keys.sort();
    keys.dedup();
    assert_eq!(keys.len(), 30, "all returned keys are unique across pages");

    // ── 12. Range queries ───────────────────────────────────────────
    //
    // Five entities with `price` at [10, 25, 50, 75, 100], isolated
    // by `range_batch="price_test"` so earlier entities don't bleed
    // into the assertions. Also verifies `$createdAtBlock` range scan.

    let block_before_range_batch = world.head_block().await?;
    let prices = [10u64, 25, 50, 75, 100];
    for &p in &prices {
        world
            .create(
                0,
                CreateOp::new()
                    .content_type("application/octet-stream")
                    .btl(1_000)
                    .string_attr("range_batch", "price_test")
                    .numeric_attr("price", p),
            )
            .await?;
    }
    let block_after_range_batch = world.head_block().await?;

    let gt50 = world
        .query(r#"price > 50 && range_batch = "price_test""#)
        .await?;
    assert_eq!(gt50.len(), 2, "price > 50: expect 75 and 100");

    let gte50 = world
        .query(r#"price >= 50 && range_batch = "price_test""#)
        .await?;
    assert_eq!(gte50.len(), 3, "price >= 50: expect 50, 75, 100");

    let lt50 = world
        .query(r#"price < 50 && range_batch = "price_test""#)
        .await?;
    assert_eq!(lt50.len(), 2, "price < 50: expect 10, 25");

    let lte50 = world
        .query(r#"price <= 50 && range_batch = "price_test""#)
        .await?;
    assert_eq!(lte50.len(), 3, "price <= 50: expect 10, 25, 50");

    // Composed range: 25 <= price <= 75
    let mid_range = world
        .query(r#"price >= 25 && price <= 75 && range_batch = "price_test""#)
        .await?;
    assert_eq!(mid_range.len(), 3, "25 <= price <= 75: expect 25, 50, 75");

    // $createdAtBlock range covers exactly the five entities just created.
    let by_block = world
        .query(&format!(
            r#"$createdAtBlock >= {} && $createdAtBlock <= {} && range_batch = "price_test""#,
            block_before_range_batch + 1,
            block_after_range_batch,
        ))
        .await?;
    assert_eq!(
        by_block.len(),
        5,
        "$createdAtBlock range covers all 5 price entities"
    );

    // ── 13. Glob queries ─────────────────────────────────────────────
    //
    // Glob (`~`) and not-glob (`!~`) on `$contentType` and a user
    // string attribute. Entities are isolated by a batch marker so
    // earlier entities don't pollute counts.

    world
        .create(
            0,
            CreateOp::new()
                .content_type("video/mp4")
                .btl(1_000)
                .string_attr("glob_batch", "ct_test"),
        )
        .await?;
    world
        .create(
            0,
            CreateOp::new()
                .content_type("video/webm")
                .btl(1_000)
                .string_attr("glob_batch", "ct_test"),
        )
        .await?;
    world
        .create(
            0,
            CreateOp::new()
                .content_type("audio/mp3")
                .btl(1_000)
                .string_attr("glob_batch", "ct_test"),
        )
        .await?;

    // "video/*" prefix matches mp4 and webm only.
    let videos = world
        .query(r#"$contentType ~ "video/*" && glob_batch = "ct_test""#)
        .await?;
    assert_eq!(videos.len(), 2, "$contentType ~ video/*: mp4 + webm");

    // Negated glob within the batch: all non-video entities → audio/mp3.
    let non_videos = world
        .query(r#"glob_batch = "ct_test" && $contentType !~ "video/*""#)
        .await?;
    assert_eq!(
        non_videos.len(),
        1,
        "$contentType !~ video/*: only audio/mp3"
    );
    assert_eq!(non_videos[0].content_type.as_deref(), Some("audio/mp3"));

    // Glob on a user string attribute.
    let genre_keys = [
        world
            .create(
                0,
                CreateOp::new()
                    .btl(1_000)
                    .string_attr("genre", "podcast/tech")
                    .string_attr("genre_batch", "test"),
            )
            .await?,
        world
            .create(
                0,
                CreateOp::new()
                    .btl(1_000)
                    .string_attr("genre", "podcast/music")
                    .string_attr("genre_batch", "test"),
            )
            .await?,
        world
            .create(
                0,
                CreateOp::new()
                    .btl(1_000)
                    .string_attr("genre", "news")
                    .string_attr("genre_batch", "test"),
            )
            .await?,
    ];

    // "podcast/*" matches tech and music, not news.
    let podcasts = world.query(r#"genre ~ "podcast/*""#).await?;
    assert_eq!(podcasts.len(), 2, "genre ~ podcast/*: tech + music");
    let podcast_keys: Vec<B256> = podcasts.iter().map(|e| e.key).collect();
    assert!(podcast_keys.contains(&genre_keys[0]));
    assert!(podcast_keys.contains(&genre_keys[1]));

    // !~ within the batch: only the "news" entity survives.
    let not_podcasts = world
        .query(r#"genre_batch = "test" && genre !~ "podcast/*""#)
        .await?;
    assert_eq!(not_podcasts.len(), 1, "genre !~ podcast/*: only news");
    assert_eq!(not_podcasts[0].key, genre_keys[2]);

    Ok(())
}
