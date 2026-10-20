//! Behavioural test suite every [`ObjectStore`] implementation must pass.
//!
//! Used by the unit tests for [`crate::MemoryStore`] and by the integration
//! tests that run against real rustFS/MinIO endpoints. Panics on failure.

use bytes::Bytes;

use crate::{Error, ObjectStore};

/// Runs the suite using keys under `prefix`, cleaning up afterwards.
pub async fn run(store: &dyn ObjectStore, prefix: &str) {
    let k = |name: &str| format!("{prefix}/{name}");
    let data = Bytes::from((0..=255u8).cycle().take(10_000).collect::<Vec<_>>());

    // Missing objects.
    assert!(matches!(store.get(&k("missing")).await, Err(Error::NotFound(_))));
    assert!(matches!(
        store.get_range(&k("missing"), 0..10).await,
        Err(Error::NotFound(_))
    ));
    assert_eq!(store.head(&k("missing")).await.unwrap(), None);
    store.delete(&k("missing")).await.unwrap();

    // Round trip.
    store.put(&k("a"), data.clone()).await.unwrap();
    assert_eq!(store.get(&k("a")).await.unwrap(), data);
    let meta = store.head(&k("a")).await.unwrap().unwrap();
    assert_eq!(meta.size, data.len() as u64);
    // Cleanup decides by age, so stores must report when objects were
    // written (allowing for clock skew between client and store).
    let age = |t: Option<std::time::SystemTime>| {
        let t = t.expect("store reports modification times");
        let now = std::time::SystemTime::now();
        now.duration_since(t).unwrap_or_else(|e| e.duration())
    };
    assert!(age(meta.modified) < std::time::Duration::from_secs(3600));

    // Ranges, including clipping past the end.
    assert_eq!(store.get_range(&k("a"), 0..1).await.unwrap(), data.slice(0..1));
    assert_eq!(
        store.get_range(&k("a"), 100..5000).await.unwrap(),
        data.slice(100..5000)
    );
    assert_eq!(
        store.get_range(&k("a"), 9990..20_000).await.unwrap(),
        data.slice(9990..)
    );
    assert!(store.get_range(&k("a"), 10_000..10_010).await.unwrap().is_empty());
    assert!(store.get_range(&k("a"), 5..5).await.unwrap().is_empty());

    // Empty objects.
    store.put(&k("empty"), Bytes::new()).await.unwrap();
    assert!(store.get(&k("empty")).await.unwrap().is_empty());
    assert!(store.get_range(&k("empty"), 0..10).await.unwrap().is_empty());
    assert_eq!(store.head(&k("empty")).await.unwrap().unwrap().size, 0);

    // Overwrite.
    store.put(&k("a"), Bytes::from_static(b"new")).await.unwrap();
    assert_eq!(store.get(&k("a")).await.unwrap(), Bytes::from_static(b"new"));

    // Create-only writes, racing: exactly one wins and its data stays.
    assert!(
        !store
            .put_if_absent(&k("a"), Bytes::from_static(b"x"))
            .await
            .unwrap()
    );
    assert_eq!(store.get(&k("a")).await.unwrap(), Bytes::from_static(b"new"));
    let once = k("once");
    let racers = (0..8u8).map(|i| store.put_if_absent(&once, Bytes::from(vec![i; 100])));
    let won: Vec<usize> = futures::future::join_all(racers)
        .await
        .into_iter()
        .enumerate()
        .filter_map(|(i, r)| r.unwrap().then_some(i))
        .collect();
    assert_eq!(won.len(), 1, "put_if_absent let {won:?} write");
    assert_eq!(
        store.get(&once).await.unwrap(),
        Bytes::from(vec![won[0] as u8; 100])
    );
    store.delete(&once).await.unwrap();

    // Listing with pagination; "list/" must not match "list2/".
    for i in 0..5 {
        store
            .put(&k(&format!("list/{i}")), Bytes::from_static(b"x"))
            .await
            .unwrap();
    }
    store.put(&k("list2/x"), Bytes::from_static(b"x")).await.unwrap();
    let list_prefix = k("list/");
    let mut seen = Vec::new();
    let mut after: Option<String> = None;
    loop {
        let page = store.list(&list_prefix, after.as_deref(), 2).await.unwrap();
        assert!(page.objects.len() <= 2);
        assert!(
            page.objects
                .iter()
                .all(|o| age(o.modified) < std::time::Duration::from_secs(3600))
        );
        seen.extend(page.objects.iter().map(|o| o.key.clone()));
        match page.next_start_after {
            Some(next) => after = Some(next),
            None => break,
        }
    }
    let expected: Vec<String> = (0..5).map(|i| k(&format!("list/{i}"))).collect();
    assert_eq!(seen, expected);

    let page = store.list(&list_prefix, Some(&k("list/2")), 100).await.unwrap();
    let keys: Vec<_> = page.objects.into_iter().map(|o| o.key).collect();
    assert_eq!(keys, vec![k("list/3"), k("list/4")]);
    assert_eq!(page.next_start_after, None);

    // Delete and clean up everything we wrote.
    store.delete(&k("a")).await.unwrap();
    assert_eq!(store.head(&k("a")).await.unwrap(), None);
    let all = store.list(&format!("{prefix}/"), None, 1000).await.unwrap();
    for o in all.objects {
        store.delete(&o.key).await.unwrap();
    }
    assert!(
        store
            .list(&format!("{prefix}/"), None, 1000)
            .await
            .unwrap()
            .objects
            .is_empty()
    );
}
