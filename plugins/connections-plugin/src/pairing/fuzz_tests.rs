//! Fuzz-style coverage of everything an unpaired peer reaches during a pairing window:
//! the `bmux/pair/1` frame decoder, the hello check, and (on the connecting side) the
//! pasted link decoder. Arbitrary and mutated input must never panic, never be accepted
//! without the link secret, and never size a buffer past the frame limit.
use super::protocol::{self, DeviceMessage, HostMessage, MAX_DEVICE_NAME, MAX_FRAME};
use super::{Event, Sessions};
use crate::link::{PairingLink, Secret};
use crate::trust::Role;
use proptest::prelude::*;
use std::time::Duration;

fn key(n: u8) -> iroh::PublicKey {
    iroh::SecretKey::from_bytes(&[n; 32]).public()
}

/// Reads one frame of `T` from `bytes` (then EOF).
fn read<T: for<'de> serde::Deserialize<'de>>(bytes: &[u8]) -> anyhow::Result<T> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let mut reader = bytes;
    runtime.block_on(protocol::read_frame::<T>(&mut reader))
}

fn frame(json: &[u8]) -> Vec<u8> {
    let mut bytes = u32::try_from(json.len()).unwrap().to_le_bytes().to_vec();
    bytes.extend_from_slice(json);
    bytes
}

fn valid_hello(secret: &Secret, role: Role) -> DeviceMessage {
    let transcript = protocol::transcript(&key(1), &key(2), secret, role);
    DeviceMessage::Hello {
        version: protocol::VERSION,
        role,
        device_name: "phone".into(),
        proof: protocol::hex(&protocol::proof(secret, &transcript)),
    }
}

/// Offers `hello` from device key 2 to a fresh pairing session of host key 1 whose
/// secret is `secret`; returns whether it became a request.
fn offer(secret: &Secret, hello: &DeviceMessage) -> bool {
    let sessions = Sessions::default();
    sessions
        .open(Role::Full, None, Duration::from_secs(60))
        .unwrap();
    // Use the test's secret: `open` drew a random one.
    sessions.lock().as_mut().unwrap().secret = secret.clone();
    let Ok(submitted) = sessions.submit(&key(1), key(2), hello) else {
        return false;
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_time()
        .build()
        .unwrap();
    let session_id = sessions.lock().as_ref().unwrap().id.clone();
    let event = runtime
        .block_on(sessions.next_event(&session_id, Duration::ZERO))
        .unwrap();
    let Event::Request { device_name, .. } = event else {
        panic!("an accepted hello reaches the owner, got {event:?}");
    };
    // The owner only ever sees a bounded, printable label.
    assert!(!device_name.is_empty() && device_name.len() <= MAX_DEVICE_NAME);
    assert!(
        device_name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
    );
    drop(submitted);
    true
}

#[derive(Debug, Clone)]
enum Mutation {
    Flip { at: usize, bits: u8 },
    Truncate { len: usize },
    Insert { at: usize, bytes: Vec<u8> },
}

fn mutation() -> impl Strategy<Value = Mutation> {
    prop_oneof![
        (any::<usize>(), 1..=255_u8).prop_map(|(at, bits)| Mutation::Flip { at, bits }),
        any::<usize>().prop_map(|len| Mutation::Truncate { len }),
        (any::<usize>(), proptest::collection::vec(any::<u8>(), 1..8))
            .prop_map(|(at, bytes)| Mutation::Insert { at, bytes }),
    ]
}

fn apply(bytes: &mut Vec<u8>, mutation: &Mutation) {
    match mutation {
        Mutation::Flip { at, bits } if !bytes.is_empty() => {
            let index = at % bytes.len();
            bytes[index] ^= bits;
        }
        Mutation::Flip { .. } => {}
        Mutation::Truncate { len } => bytes.truncate(len % (bytes.len() + 1)),
        Mutation::Insert {
            at,
            bytes: inserted,
        } => {
            let index = at % (bytes.len() + 1);
            bytes.splice(index..index, inserted.iter().copied());
        }
    }
}

fn role() -> impl Strategy<Value = Role> {
    prop_oneof![Just(Role::Full), Just(Role::ViewOnly)]
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

    /// Raw bytes on the pairing stream: refused or decoded, never a panic, and a frame
    /// that decodes fits the limit.
    #[test]
    fn arbitrary_pairing_bytes_never_panic(bytes in proptest::collection::vec(any::<u8>(), 0..6_000)) {
        if read::<DeviceMessage>(&bytes).is_ok() || read::<HostMessage>(&bytes).is_ok() {
            let length = u32::from_le_bytes(bytes[..4].try_into().unwrap());
            prop_assert!(usize::try_from(length).unwrap() <= MAX_FRAME);
        }
    }

    /// A declared length over the limit is refused before the body is read, so no buffer
    /// is ever sized by it.
    #[test]
    fn oversized_lengths_are_refused_before_reading(length in (u32::try_from(MAX_FRAME).unwrap() + 1)..=u32::MAX) {
        let error = read::<DeviceMessage>(&length.to_le_bytes()).unwrap_err();
        prop_assert!(error.to_string().contains("too large"), "{:#}", error);
    }

    /// Arbitrary JSON-ish bodies inside a well-formed frame.
    #[test]
    fn arbitrary_frame_bodies_never_panic(body in "\\PC{0,512}") {
        let _ = read::<DeviceMessage>(&frame(body.as_bytes()));
        let _ = read::<HostMessage>(&frame(body.as_bytes()));
    }

    /// Mutations of a valid hello frame: a mutation that still decodes becomes a request
    /// only if its version, role and proof are untouched (its name may change).
    #[test]
    fn mutated_hellos_need_the_untouched_proof(
        secret in any::<[u8; 32]>(),
        role in role(),
        mutations in proptest::collection::vec(mutation(), 1..5),
    ) {
        let secret = Secret(secret);
        let original = valid_hello(&secret, role);
        let mut bytes = serde_json::to_vec(&original).unwrap();
        for mutation in &mutations {
            apply(&mut bytes, mutation);
        }
        let Ok(hello) = read::<DeviceMessage>(&frame(&bytes)) else {
            return Ok(());
        };
        let DeviceMessage::Hello { version, role: mutated_role, proof, .. } = &hello;
        let DeviceMessage::Hello { proof: original_proof, .. } = &original;
        let intact = *version == protocol::VERSION && *mutated_role == role && proof == original_proof;
        prop_assert_eq!(offer(&secret, &hello), intact);
    }

    /// Any hello field values: never a panic, never a request without the secret.
    #[test]
    fn arbitrary_hellos_are_refused_without_the_secret(
        secret in any::<[u8; 32]>(),
        version in any::<u32>(),
        role in role(),
        device_name in "\\PC{0,300}",
        proof in "[0-9a-fA-F]{0,80}|\\PC{0,80}",
    ) {
        let hello = DeviceMessage::Hello { version, role, device_name, proof };
        prop_assert!(!offer(&Secret(secret), &hello));
    }

    /// Any name a valid proof carries reaches the owner sanitized (checked in `offer`).
    #[test]
    fn device_names_reach_the_owner_sanitized(secret in any::<[u8; 32]>(), device_name in "\\PC{0,300}") {
        let secret = Secret(secret);
        let DeviceMessage::Hello { version, role, proof, .. } = valid_hello(&secret, Role::Full);
        let hello = DeviceMessage::Hello { version, role, device_name, proof };
        prop_assert!(offer(&secret, &hello));
    }

    /// Pasted links: never a panic; whatever decodes re-encodes to the same link.
    #[test]
    fn arbitrary_links_never_panic(text in "(bmux-pair:)?[a-z2-7A-Z \\n]{0,400}|\\PC{0,1200}") {
        if let Ok(link) = PairingLink::decode(&text) {
            prop_assert_eq!(PairingLink::decode(&link.encode().unwrap()).unwrap(), link);
        }
    }

    /// Damaged copies of a valid link are refused (the checksum), never misread.
    #[test]
    fn mutated_links_are_refused_or_unchanged(
        secret in any::<[u8; 32]>(),
        port in any::<u16>(),
        mutations in proptest::collection::vec(mutation(), 1..4),
    ) {
        let link = PairingLink {
            host_key: key(3),
            secret: Secret(secret),
            relay: Some("https://relay.example/".into()),
            addrs: vec![std::net::SocketAddr::from(([127, 0, 0, 1], port))],
        };
        let text = link.encode().unwrap();
        let mut bytes = text.into_bytes();
        for mutation in &mutations {
            apply(&mut bytes, mutation);
        }
        let mutated = String::from_utf8_lossy(&bytes);
        if let Ok(decoded) = PairingLink::decode(&mutated) {
            // Only a change the decoder normalizes away (case, surrounding space) decodes.
            prop_assert_eq!(decoded, link);
        }
    }
}
