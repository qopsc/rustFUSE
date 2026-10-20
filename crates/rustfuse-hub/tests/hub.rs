//! The hub orders pushes from several clients and streams them to everyone.

use std::sync::Arc;

use rustfuse_hub::{Event, Hub, HubClient, HubStore, Volume, tls};
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

async fn start_tls(root: Arc<dyn ObjectStore>, cert: &std::path::Path, key: &std::path::Path) -> u16 {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let server = tls::server_config(cert, key).unwrap();
    tokio::spawn(Hub::new(root, "secret".into()).serve_with(listener, Some(server)));
    port
}

async fn tls_client(addr: &str, ca: &std::path::Path) -> rustfuse_hub::Result<HubClient> {
    let config = tls::client_config(Some(ca)).unwrap();
    HubClient::connect_with(addr, Some(&config), "v", "secret", 0).await
}

#[tokio::test]
async fn tls_connections() {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
    let root: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    Volume::new(Arc::new(PrefixStore::new(root.clone(), "volumes/v")))
        .create()
        .await
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let write = |name: &str, pem: String| {
        let p = dir.path().join(name);
        std::fs::write(&p, pem).unwrap();
        p
    };

    // A private CA signs the hub's certificate for "localhost".
    let ca_key = KeyPair::generate().unwrap();
    let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
    ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let ca = ca_params.clone().self_signed(&ca_key).unwrap();
    let hub_key = KeyPair::generate().unwrap();
    let hub_cert = CertificateParams::new(vec!["localhost".into()])
        .unwrap()
        .signed_by(&hub_key, &ca, &ca_key)
        .unwrap();
    let ca_pem = write("ca.pem", ca.pem());
    let port = start_tls(
        root.clone(),
        &write("hub.pem", hub_cert.pem()),
        &write("hub.key", hub_key.serialize_pem()),
    )
    .await;

    let addr = format!("localhost:{port}");
    let mut a = tls_client(&addr, &ca_pem).await.unwrap();
    a.push(&cs(1, 1)).await.unwrap();
    assert_eq!(next_changeset(&mut a).await.seq, 1);
    // Through a CA, the certificate must name the host: it says localhost,
    // not the IP address.
    assert!(tls_client(&format!("127.0.0.1:{port}"), &ca_pem).await.is_err());
    // A CA nobody configured is refused.
    let other_key = KeyPair::generate().unwrap();
    let other = ca_params.self_signed(&other_key).unwrap();
    assert!(tls_client(&addr, &write("other.pem", other.pem())).await.is_err());
    // A plain client gets nowhere with a TLS hub.
    let plain = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        HubClient::connect(&addr, "v", "secret", 0),
    )
    .await;
    assert!(!matches!(plain, Ok(Ok(_))));

    // A self-signed certificate marked as a CA (what `openssl req -x509`
    // makes) works when given as the hub's CA file: it is pinned.
    let self_key = KeyPair::generate().unwrap();
    let mut self_params = CertificateParams::new(vec!["localhost".into()]).unwrap();
    self_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    let self_signed = write("self.pem", self_params.self_signed(&self_key).unwrap().pem());
    let port = start_tls(root, &self_signed, &write("self.key", self_key.serialize_pem())).await;
    let mut b = tls_client(&format!("localhost:{port}"), &self_signed)
        .await
        .unwrap();
    b.push(&cs(2, 1)).await.unwrap();
    assert_eq!(next_changeset(&mut b).await.seq, 1);
    assert!(tls_client(&format!("localhost:{port}"), &ca_pem).await.is_err());
}

#[tokio::test(flavor = "multi_thread")]
async fn storage_through_the_hub() {
    let root: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    let addr = start(root.clone()).await;

    assert!(HubStore::connect(&addr, None, "v", "wrong").await.is_err());
    assert!(HubStore::connect(&addr, None, "../x", "secret").await.is_err());

    // Works before the volume exists, and stays inside it.
    let store = HubStore::connect(&addr, None, "v", "secret").await.unwrap();
    rustfuse_store::conformance::run(&store, "t").await;
    store.put("x", bytes::Bytes::from_static(b"v")).await.unwrap();
    assert_eq!(
        root.get("volumes/v/x").await.unwrap(),
        bytes::Bytes::from_static(b"v")
    );

    assert!(store.get("../w/x").await.is_err());
    assert!(store.list("a/../../", None, 10).await.is_err());

    // Many calls in flight on the one connection, large ones included.
    let big = bytes::Bytes::from(vec![7u8; 20 << 20]);
    let store = Arc::new(store);
    let mut tasks = Vec::new();
    for i in 0..32 {
        let (store, big) = (store.clone(), big.clone());
        tasks.push(tokio::spawn(async move {
            let key = format!("p/{i}");
            store.put(&key, big.slice(..(i << 16) + 1)).await.unwrap();
            let got = store.get_range(&key, 1..(i << 16) as u64 + 1).await.unwrap();
            assert_eq!(got.len(), i << 16);
        }));
    }
    for t in tasks {
        t.await.unwrap();
    }
    store.put("big", big.clone()).await.unwrap();
    assert_eq!(store.get("big").await.unwrap(), big);

    // A dropped connection is reopened on the next call.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let proxy = listener.local_addr().unwrap().to_string();
    let target = addr.clone();
    let cut = Arc::new(tokio::sync::Notify::new());
    let cut2 = cut.clone();
    tokio::spawn(async move {
        loop {
            let (mut a, _) = listener.accept().await.unwrap();
            let mut b = tokio::net::TcpStream::connect(&target).await.unwrap();
            let cut = cut2.clone();
            tokio::spawn(async move {
                tokio::select! {
                    _ = tokio::io::copy_bidirectional(&mut a, &mut b) => {}
                    _ = cut.notified() => {}
                }
            });
        }
    });
    let s = HubStore::connect(&proxy, None, "v", "secret").await.unwrap();
    assert_eq!(s.get("x").await.unwrap(), bytes::Bytes::from_static(b"v"));
    cut.notify_waiters();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(s.get("x").await.unwrap(), bytes::Bytes::from_static(b"v"));
}

#[tokio::test]
async fn two_hubs_share_a_volume() {
    let root: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    Volume::new(Arc::new(PrefixStore::new(root.clone(), "volumes/v")))
        .create()
        .await
        .unwrap();
    let (h1, h2) = (start(root.clone()).await, start(root.clone()).await);
    let mut a = HubClient::connect(&h1, "v", "secret", 0).await.unwrap();
    let mut b = HubClient::connect(&h2, "v", "secret", 0).await.unwrap();

    // Both hubs append at once and keep losing numbers to each other.
    const N: u64 = 20;
    let (ra, rb) = tokio::join!(
        async {
            for i in 1..=N {
                a.push(&cs(1, i)).await.unwrap();
            }
            a
        },
        async {
            for i in 1..=N {
                b.push(&cs(2, i)).await.unwrap();
            }
            b
        }
    );
    let (mut a, mut b) = (ra, rb);

    // Each machine sees the whole log, in the same order, once.
    let mut logs = Vec::new();
    for c in [&mut a, &mut b] {
        let mut seen = Vec::new();
        while seen.len() < 2 * N as usize {
            let cs = tokio::time::timeout(std::time::Duration::from_secs(10), next_changeset(c))
                .await
                .expect("changesets from the other hub arrive");
            seen.push(cs);
        }
        logs.push(seen);
    }
    assert_eq!(logs[0], logs[1]);
    let seqs: Vec<u64> = logs[0].iter().map(|c| c.seq).collect();
    assert_eq!(seqs, (1..=2 * N).collect::<Vec<_>>());
    let mut ids: Vec<(u64, u64)> = logs[0].iter().map(|c| (c.version.replica.0, c.local)).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 2 * N as usize, "a changeset was lost or doubled");
}

#[tokio::test]
async fn clients_fail_over_to_the_next_hub() {
    let root: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    Volume::new(Arc::new(PrefixStore::new(root.clone(), "volumes/v")))
        .create()
        .await
        .unwrap();
    let dead = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().to_string()
    };
    let live = start(root).await;
    let both = format!("{dead}, {live}");
    let mut c = HubClient::connect(&both, "v", "secret", 0).await.unwrap();
    c.push(&cs(1, 1)).await.unwrap();
    assert_eq!(next_changeset(&mut c).await.seq, 1);
    let store = HubStore::connect(&both, None, "v", "secret").await.unwrap();
    assert!(store.head("volume.json").await.unwrap().is_some());
    assert!(HubClient::connect(&dead, "v", "secret", 0).await.is_err());
}

#[tokio::test]
async fn resent_changesets_are_not_appended_twice() {
    let root: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    Volume::new(Arc::new(PrefixStore::new(root.clone(), "volumes/v")))
        .create()
        .await
        .unwrap();
    let (h1, h2) = (start(root.clone()).await, start(root.clone()).await);
    let mut a = HubClient::connect(&h1, "v", "secret", 0).await.unwrap();
    a.push(&cs(1, 1)).await.unwrap();
    assert_eq!(next_changeset(&mut a).await.seq, 1);
    // Again on the same hub, then on the other one after a newer change.
    a.push(&cs(1, 1)).await.unwrap();
    let mut b = HubClient::connect(&h2, "v", "secret", 0).await.unwrap();
    b.push(&cs(2, 1)).await.unwrap();
    b.push(&cs(1, 1)).await.unwrap();
    let mut acks = Vec::new();
    while acks.len() < 2 {
        if let Event::Pushed { local, seq } = b.next().await.unwrap().unwrap() {
            acks.push((local, seq));
        }
    }
    assert_eq!(acks, [(1, 2), (1, 1)]);
    let log = Volume::new(Arc::new(PrefixStore::new(root, "volumes/v")));
    assert_eq!(log.head().await.unwrap(), 2);
}

#[tokio::test]
async fn per_machine_keys() {
    use rustfuse_hub::keys;
    use std::time::Duration;
    let root: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    for v in ["v", "w"] {
        Volume::new(Arc::new(PrefixStore::new(root.clone(), &format!("volumes/{v}"))))
            .create()
            .await
            .unwrap();
    }
    // No shared token: only keys work. Keys are re-read every 200 ms.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap().to_string();
    let ttl = Duration::from_millis(200);
    tokio::spawn(Hub::with_key_ttl(root.clone(), String::new(), ttl).serve(listener));

    let (laptop, token) = keys::create(&*root, "laptop", vec!["v".into()]).await.unwrap();
    let (_, any) = keys::create(&*root, "server", vec![]).await.unwrap();
    assert!(HubClient::connect(&addr, "v", "", 0).await.is_err());
    assert!(HubClient::connect(&addr, "v", "secret", 0).await.is_err());
    let mut c = HubClient::connect(&addr, "v", &token, 0).await.unwrap();
    let err = HubClient::connect(&addr, "w", &token, 0).await.err().unwrap();
    assert!(err.to_string().contains("does not open volume w"), "{err}");
    assert!(HubClient::connect(&addr, "w", &any, 0).await.is_ok());
    let store = HubStore::connect(&addr, None, "v", &token).await.unwrap();
    assert!(store.head("volume.json").await.unwrap().is_some());
    // The storage proxy cannot reach the keys.
    assert!(store.get("../../keys/x").await.is_err());
    c.push(&cs(1, 1)).await.unwrap();
    assert_eq!(next_changeset(&mut c).await.seq, 1);

    // Revoking closes the open connections and refuses new ones.
    assert!(keys::revoke(&*root, &laptop.id).await.unwrap());
    tokio::time::sleep(ttl * 3).await;
    let closed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match c.next().await {
                Some(Ok(_)) => continue,
                other => return other.map(|r| r.map(|_| ())),
            }
        }
    })
    .await
    .expect("log connection closed");
    assert!(matches!(closed, None | Some(Err(_))), "{closed:?}");
    // A storage call reconnects and is refused.
    assert!(store.head("volume.json").await.is_err());
    assert!(HubClient::connect(&addr, "v", &token, 0).await.is_err());
    assert!(HubClient::connect(&addr, "v", &any, 0).await.is_ok());
}

#[tokio::test]
async fn lists_volumes_a_token_opens() {
    let root: Arc<dyn ObjectStore> = Arc::new(MemoryStore::new());
    // Names sorting just before and after another's objects ('-' and '.'
    // sort before '/', '0' after it).
    for name in ["app", "app-b", "app.c", "app0", "zeta"] {
        let v = Volume::new(Arc::new(PrefixStore::new(
            root.clone(),
            &format!("volumes/{name}"),
        )));
        v.create().await.unwrap();
        v.put_changeset(&cs(1, 1)).await.unwrap();
    }
    // Objects that are not volumes are left out.
    root.put("volumes/stray", bytes::Bytes::from_static(b"x"))
        .await
        .unwrap();
    root.put(
        "volumes/partial/log/00000000000000000001",
        bytes::Bytes::from_static(b"x"),
    )
    .await
    .unwrap();
    let all = vec!["app", "app-b", "app.c", "app0", "zeta"];
    assert_eq!(rustfuse_hub::list_volumes(&*root).await.unwrap(), all);

    let addr = start(root.clone()).await;
    assert_eq!(rustfuse_hub::volumes(&addr, None, "secret").await.unwrap(), all);
    let (_, token) = rustfuse_hub::keys::create(&*root, "laptop", vec!["app0".into(), "gone".into()])
        .await
        .unwrap();
    assert_eq!(
        rustfuse_hub::volumes(&addr, None, &token).await.unwrap(),
        vec!["app0"]
    );
    let err = rustfuse_hub::volumes(&addr, None, "wrong").await.err().unwrap();
    assert!(err.to_string().contains("invalid token"), "{err}");
}
