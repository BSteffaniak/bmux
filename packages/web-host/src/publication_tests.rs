use super::*;
use crate::snapshot_feed;

fn binary(size: usize) -> BinaryPresentation {
    BinaryPresentation {
        snapshot: Arc::from(vec![1; size]),
        delta: None,
    }
}

fn resources(count: usize, size: usize) -> BTreeMap<String, Resource> {
    (0..count)
        .map(|index| {
            let mut bytes = vec![0; size.max(8)];
            bytes[..8].copy_from_slice(&u64::try_from(index).unwrap().to_le_bytes());
            let digest = Sha256::digest(&bytes);
            let id = digest.iter().map(|byte| format!("{byte:02x}")).collect();
            (
                id,
                Resource {
                    bytes: Arc::from(bytes),
                    mime_type: "application/octet-stream".into(),
                },
            )
        })
        .collect()
}

#[test]
fn representation_limits_reject_atomically() {
    let (publisher, feed) = snapshot_feed(128).unwrap();
    publisher
        .publish_representations(1, None, binary(1), BTreeMap::new())
        .unwrap();
    for bad in [
        binary(0),
        binary(129),
        BinaryPresentation {
            snapshot: Arc::from([1]),
            delta: Some((0, Arc::from([2]))),
        },
    ] {
        assert!(
            publisher
                .publish_representations(2, None, bad, BTreeMap::new())
                .is_err()
        );
    }
    assert!(
        publisher
            .publish_representations(2, Some(Arc::from(vec![0; 129])), binary(1), BTreeMap::new())
            .is_err()
    );
    for bad in [
        resources(MAX_RESOURCES + 1, 8),
        resources(1, MAX_RESOURCE_BYTES + 1),
        resources(5, MAX_RESOURCE_BYTES),
    ] {
        assert!(
            publisher
                .publish_representations(2, None, binary(1), bad)
                .is_err()
        );
        assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 1);
    }
    let mut bad = resources(1, 8);
    bad.values_mut().next().unwrap().mime_type = "application/x\r\ninjected: value".into();
    assert!(
        publisher
            .publish_representations(2, None, binary(1), bad)
            .is_err()
    );
    publisher
        .publish_representations(2, None, binary(1), resources(3, MAX_RESOURCE_BYTES))
        .unwrap();
    assert!(
        publisher
            .publish_representations(3, None, binary(1), resources(2, MAX_RESOURCE_BYTES))
            .is_err()
    );
    assert_eq!(feed.receiver.borrow().as_ref().unwrap().revision, 2);
    assert!(
        publisher
            .publish_representations(2, None, binary(1), BTreeMap::new())
            .is_err()
    );
    drop(feed);
    assert_eq!(
        publisher
            .publish_representations(3, None, binary(1), BTreeMap::new())
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
}
