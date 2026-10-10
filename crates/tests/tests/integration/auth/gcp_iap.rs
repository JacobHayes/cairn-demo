//! The Google Cloud IAP provider (H1), offline: assertions signed with ES256 keys generated
//! for the test, verified against a stub of Google's key set on loopback. A good assertion
//! names its user on any listener and from any peer; every spoiled one is refused; a
//! rotated key is fetched, and forged key ids do not fetch on every request (3.2, Security).

#[cfg(test)]
mod gcp_iap {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    use axum::body::Body;
    use axum::extract::State;
    use axum::routing::get;
    use axum::{Json, Router};
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use cairn_auth::gcp_iap::ASSERTION_HEADER;
    use cairn_auth::{
        AuthProvider, GcpIapConfig, GcpIapProvider, Peer, Presented, Refusal, Verdict,
    };
    use cairn_store::AuthStore;
    use p256::ecdsa::signature::Signer;
    use p256::ecdsa::{Signature, SigningKey};
    use serde_json::{Value, json};
    use url::Url;

    use crate::auth::support::{REMOTE, World, app, public, request, whoami};

    const AUDIENCE: &str = "/projects/123456789/global/backendServices/987654321";
    const ISSUER: &str = "https://cloud.google.com/iap";
    const SUBJECT: &str = "accounts.google.com:1234567890";
    const EMAIL: &str = "ann@example.org";

    /// A key the test signs with, under its key id.
    struct Key {
        id: &'static str,
        signing: SigningKey,
    }

    fn key(id: &'static str, seed: u8) -> Key {
        let signing = SigningKey::from_bytes(&[seed; 32].into()).unwrap();
        Key { id, signing }
    }

    impl Key {
        fn jwk(&self) -> Value {
            let point = self.signing.verifying_key().to_sec1_point(false);
            json!({
                "kty": "EC",
                "crv": "P-256",
                "alg": "ES256",
                "use": "sig",
                "kid": self.id,
                "x": URL_SAFE_NO_PAD.encode(point.x().unwrap()),
                "y": URL_SAFE_NO_PAD.encode(point.y().unwrap()),
            })
        }
    }

    /// A stub of Google's key set, counting fetches.
    struct KeySet {
        url: Url,
        keys: Arc<Mutex<Vec<Value>>>,
        fetches: Arc<AtomicUsize>,
    }

    #[derive(Clone)]
    struct Served {
        keys: Arc<Mutex<Vec<Value>>>,
        fetches: Arc<AtomicUsize>,
    }

    async fn serve_keys(State(served): State<Served>) -> Json<Value> {
        served.fetches.fetch_add(1, Ordering::SeqCst);
        Json(json!({ "keys": served.keys.lock().unwrap().clone() }))
    }

    impl KeySet {
        async fn start(keys: &[&Key]) -> Self {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let url = format!("http://{}/keys", listener.local_addr().unwrap());
            let served = Served {
                keys: Arc::new(Mutex::new(keys.iter().map(|key| key.jwk()).collect())),
                fetches: Arc::new(AtomicUsize::new(0)),
            };
            let router = Router::new()
                .route("/keys", get(serve_keys))
                .with_state(served.clone());
            tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            Self {
                url: url.parse().unwrap(),
                keys: served.keys,
                fetches: served.fetches,
            }
        }

        fn publish(&self, keys: &[&Key]) {
            *self.keys.lock().unwrap() = keys.iter().map(|key| key.jwk()).collect();
        }

        fn fetches(&self) -> usize {
            self.fetches.load(Ordering::SeqCst)
        }
    }

    fn config() -> GcpIapConfig {
        GcpIapConfig {
            name: "iap".parse().unwrap(),
            audience: AUDIENCE.to_owned(),
            auto_link: false,
        }
    }

    fn provider(world: &World, keys: &KeySet) -> GcpIapProvider {
        GcpIapProvider::with_key_set(config(), world.clock.clock(), keys.url.clone()).unwrap()
    }

    /// The claims IAP signs, issued at `now`, with what Google adds beside them.
    fn claims(now: i64) -> Value {
        json!({
            "iss": ISSUER,
            "aud": AUDIENCE,
            "sub": SUBJECT,
            "email": EMAIL,
            "hd": "example.org",
            "iat": now,
            "exp": now + 600,
            "identity_source": "GOOGLE",
            "google": { "access_levels": [] },
        })
    }

    /// An assertion over `claims`, its header naming `kid` and `alg`, signed by `key`.
    fn sign_as(claims: &Value, kid: &str, alg: &str, key: &Key) -> String {
        let header = json!({ "alg": alg, "typ": "JWT", "kid": kid });
        let encode = |value: &Value| URL_SAFE_NO_PAD.encode(serde_json::to_vec(value).unwrap());
        let message = format!("{}.{}", encode(&header), encode(claims));
        let signature: Signature = key.signing.sign(message.as_bytes());
        format!("{message}.{}", URL_SAFE_NO_PAD.encode(signature.to_bytes()))
    }

    fn sign(claims: &Value, key: &Key) -> String {
        sign_as(claims, key.id, "ES256", key)
    }

    async fn verdict(provider: &GcpIapProvider, assertion: Option<&str>) -> Verdict {
        let mut headers = axum::http::HeaderMap::new();
        if let Some(assertion) = assertion {
            headers.insert(ASSERTION_HEADER, assertion.parse().unwrap());
        }
        let presented = Presented {
            headers: &headers,
            peer: Peer::Unknown,
        };
        provider.authenticate(presented).await
    }

    fn now(world: &World) -> i64 {
        world.clock.clock().now().as_second()
    }

    /// A good assertion names its user through the layer, on a listener every machine
    /// reaches and from a remote peer: the signature, not the network, is the credential.
    /// The subject is IAP's account id and the email is listed as verified (H3).
    #[tokio::test]
    async fn a_signed_assertion_names_its_user_on_any_listener() {
        let signer = key("one", 7);
        let keys = KeySet::start(&[&signer]).await;
        let world = World::new();
        let iap: Arc<dyn AuthProvider> = Arc::new(provider(&world, &keys));
        let router = app(&world.auth(public(), vec![iap]));
        let assertion = sign(&claims(now(&world)), &signer);
        let get = request("/whoami", REMOTE)
            .header(ASSERTION_HEADER, &assertion)
            .body(Body::empty())
            .unwrap();
        let ann = whoami(&router, get).await.unwrap();
        let identities = world.store.identities_of(&ann.user).await.unwrap();
        assert_eq!(identities[0].subject.as_str(), SUBJECT);
        assert!(
            identities[0]
                .verified_emails
                .contains(&EMAIL.parse().unwrap())
        );
        let again = request("/whoami", REMOTE)
            .header(ASSERTION_HEADER, &assertion)
            .body(Body::empty())
            .unwrap();
        assert_eq!(whoami(&router, again).await.unwrap().user, ann.user);
        assert_eq!(keys.fetches(), 1, "the key set is kept between requests");
    }

    /// Without the header the provider reads nothing; with it, every spoiled assertion is
    /// refused as a bad credential, never passed to another provider.
    #[tokio::test]
    async fn every_spoiled_assertion_is_refused() {
        let signer = key("one", 7);
        let impostor = key("one", 8);
        let keys = KeySet::start(&[&signer]).await;
        let world = World::new();
        let iap = provider(&world, &keys);
        let at = now(&world);
        assert_eq!(verdict(&iap, None).await, Verdict::Absent);
        let good = verdict(&iap, Some(&sign(&claims(at), &signer))).await;
        assert!(matches!(good, Verdict::Identity(_)), "{good:?}");

        let with = |field: &str, value: Value| {
            let mut claims = claims(at);
            claims[field] = value;
            claims
        };
        let skew =
            i64::try_from(cairn_auth::limits::IAP_ASSERTION_CLOCK_SKEW_MAX.as_secs()).unwrap();
        let within_skew = sign(&with("exp", json!(at - skew + 1)), &signer);
        let within_skew = verdict(&iap, Some(&within_skew)).await;
        assert!(
            matches!(within_skew, Verdict::Identity(_)),
            "{within_skew:?}"
        );
        let mut tampered = sign(&claims(at), &signer);
        tampered.replace_range(tampered.len() - 4.., "AAAA");
        let cases = [
            (
                "audience",
                sign(
                    &with("aud", json!("/projects/1/global/backendServices/2")),
                    &signer,
                ),
            ),
            (
                "issuer",
                sign(&with("iss", json!("https://accounts.google.com")), &signer),
            ),
            ("expired", sign(&with("exp", json!(at - skew - 1)), &signer)),
            (
                "issued later",
                sign(&with("iat", json!(at + skew + 60)), &signer),
            ),
            ("other key", sign(&claims(at), &impostor)),
            ("tampered", tampered),
            ("algorithm", sign_as(&claims(at), "one", "ES384", &signer)),
            ("not a token", "not.a.token".to_owned()),
            ("empty", " ".to_owned()),
        ];
        for (case, assertion) in cases {
            assert_eq!(
                verdict(&iap, Some(&assertion)).await,
                Verdict::Refused(Refusal::Credential),
                "{case}"
            );
        }
    }

    /// Google rotates its keys: an assertion under a new key id fetches the set again and
    /// passes. A forged key id is refused, and fetches again only once the refetch interval
    /// has passed since the last fetch.
    #[tokio::test]
    async fn a_new_key_is_fetched_but_forged_key_ids_do_not_fetch_every_time() {
        let old = key("old", 7);
        let new = key("new", 9);
        let keys = KeySet::start(&[&old]).await;
        let world = World::new();
        let iap = provider(&world, &keys);
        let good = verdict(&iap, Some(&sign(&claims(now(&world)), &old))).await;
        assert!(matches!(good, Verdict::Identity(_)), "{good:?}");
        assert_eq!(keys.fetches(), 1);

        keys.publish(&[&old, &new]);
        world.clock.advance(61);
        let rotated = verdict(&iap, Some(&sign(&claims(now(&world)), &new))).await;
        assert!(matches!(rotated, Verdict::Identity(_)), "{rotated:?}");
        assert_eq!(keys.fetches(), 2);

        let forged = key("forged", 11);
        for _ in 0..3 {
            let refused = verdict(&iap, Some(&sign(&claims(now(&world)), &forged))).await;
            assert_eq!(refused, Verdict::Refused(Refusal::Credential));
        }
        assert_eq!(keys.fetches(), 2, "within the interval, no fetch");
        world.clock.advance(61);
        let refused = verdict(&iap, Some(&sign(&claims(now(&world)), &forged))).await;
        assert_eq!(refused, Verdict::Refused(Refusal::Credential));
        assert_eq!(keys.fetches(), 3, "past the interval, one fetch");
    }

    /// When the key set cannot be fetched, the assertion cannot be checked: refused as
    /// unavailable, not as a bad credential.
    #[tokio::test]
    async fn an_unreachable_key_set_refuses_as_unavailable() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url: Url = format!("http://{}/keys", listener.local_addr().unwrap())
            .parse()
            .unwrap();
        drop(listener);
        let world = World::new();
        let iap = GcpIapProvider::with_key_set(config(), world.clock.clock(), url).unwrap();
        let signer = key("one", 7);
        let refused = verdict(&iap, Some(&sign(&claims(now(&world)), &signer))).await;
        assert!(
            matches!(refused, Verdict::Refused(Refusal::Unavailable(_))),
            "{refused:?}"
        );
    }

    /// The provider refuses to start with an audience that is not an IAP resource, or a key
    /// set Cairn may not fetch from.
    #[test]
    fn a_bad_audience_or_key_set_refuses_to_start() {
        let world = World::new();
        let google: Url = cairn_auth::gcp_iap::GOOGLE_KEY_SET_URL.parse().unwrap();
        for audience in ["", "123456789", " /projects/1/global/backendServices/2"] {
            let config = GcpIapConfig {
                audience: audience.to_owned(),
                ..config()
            };
            let made = GcpIapProvider::with_key_set(config, world.clock.clock(), google.clone());
            assert!(made.is_err(), "{audience:?}");
        }
        let plain = "http://keys.example/keys".parse().unwrap();
        assert!(GcpIapProvider::with_key_set(config(), world.clock.clock(), plain).is_err());
        assert!(GcpIapProvider::new(config(), world.clock.clock()).is_ok());
    }
}
