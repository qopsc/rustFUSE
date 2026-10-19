//! The hub orders pushes from several clients and streams them to everyone.

use std::sync::Arc;

use rustfuse_hub::{Event, Hub, HubClient, Volume};
use rustfuse_store::{MemoryStore, ObjectStore, PrefixStore};
use rustfuse_types::{Changeset, FORMAT_VERSION, Hlc, ReplicaId, Version};

fn cs(replica: u64, local: u64) -> Changeset {
    Changeset {
        format: FORMAT_VERSION,
        seq: 0,
        version: Version {
            time: Hlc(local),
            replica: ReplicaId(replica),
        },
        local,
        ops: vec![],
    }
}

async fn start(root: Arc<dyn ObjectStore>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    tokio::spawn(Hub::new(root, "secret".into()).serve(listener));
    addr
}

async fn next_changeset(c: &mut HubClient) -> Changeset {
    loop {
        match c.next().await.unwrap().unwrap() {
            Event::Changeset(cs) => return cs,
            Event::Pushed { .. } => {}
        }
    }
}

#[tokio::test]
async fn orders_and_streams() {
    let root: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    Volume::new(Arc::new(PrefixStore::new(root.clone(), "volumes/v")))
        .create()
        .await
        .unwrap();
    let addr = start(root.clone()).await;

    assert!(HubClient::connect(&addr, "v", "wrong", 0).await.is_err());
    assert!(HubClient::connect(&addr, "../x", "secret", 0).await.is_err());
    assert!(HubClient::connect(&addr, "missing", "secret", 0).await.is_err());

    let mut a = HubClient::connect(&addr, "v", "secret", 0).await.unwrap();
    let mut b = HubClient::connect(&addr, "v", "secret", 0).await.unwrap();
    assert_eq!(a.head(), 0);
    a.push(&cs(1, 1)).await.unwrap();
    b.push(&cs(2, 1)).await.unwrap();
    a.push(&cs(1, 2)).await.unwrap();

    let mut seen_a = Vec::new();
    let mut seen_b = Vec::new();
    for _ in 0..3 {
        seen_a.push(next_changeset(&mut a).await);
        seen_b.push(next_changeset(&mut b).await);
    }
    assert_eq!(seen_a, seen_b);
    assert_eq!(seen_a.iter().map(|c| c.seq).collect::<Vec<_>>(), [1, 2, 3]);

    // A late client catches up from the given point, then gets live ones.
    let mut c = HubClient::connect(&addr, "v", "secret", 1).await.unwrap();
    assert_eq!(c.head(), 3);
    assert_eq!(next_changeset(&mut c).await.seq, 2);
    assert_eq!(next_changeset(&mut c).await.seq, 3);
    b.push(&cs(2, 2)).await.unwrap();
    assert_eq!(next_changeset(&mut c).await.seq, 4);

    // A new hub over the same bucket continues the log.
    let addr2 = start(root).await;
    let mut d = HubClient::connect(&addr2, "v", "secret", 2).await.unwrap();
    assert_eq!(d.head(), 4);
    assert_eq!(next_changeset(&mut d).await.seq, 3);
    assert_eq!(next_changeset(&mut d).await.seq, 4);
    d.push(&cs(3, 1)).await.unwrap();
    match d.next().await.unwrap().unwrap() {
        Event::Changeset(cs) => assert_eq!(cs.seq, 5),
        Event::Pushed { local, seq } => assert_eq!((local, seq), (1, 5)),
    }
}
