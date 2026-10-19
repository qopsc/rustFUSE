//! The hub orders pushes from several clients and streams them to everyone.

use std::sync::Arc;

use rustfuse_hub::{Event, Hub, HubClient, Volume, tls};
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
