use std::collections::BTreeMap;

use managed_inference_contract::{KeySetError, ManagedInferenceKeySet};

#[test]
fn gateway_key_set_retains_raw_constructor_and_public_key_export() {
    let keys =
        ManagedInferenceKeySet::from_raw(BTreeMap::from([("key-1".to_owned(), vec![0_u8; 32])]))
            .expect("a gateway-decoded Ed25519 public key is accepted");

    assert_eq!(
        keys.public_keys_base64(),
        BTreeMap::from([(
            "key-1".to_owned(),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA".to_owned(),
        )])
    );
}

#[test]
fn gateway_key_set_rejects_empty_and_invalid_configuration() {
    assert_eq!(
        ManagedInferenceKeySet::from_raw(BTreeMap::new()),
        Err(KeySetError::Empty)
    );

    for (key_id, key) in [
        ("".to_owned(), vec![0_u8; 32]),
        ("k".repeat(129), vec![0_u8; 32]),
        ("key-1".to_owned(), vec![0_u8; 31]),
        ("key-1".to_owned(), vec![0_u8; 33]),
    ] {
        assert_eq!(
            ManagedInferenceKeySet::from_raw(BTreeMap::from([(key_id.clone(), key)])),
            Err(KeySetError::InvalidKey(key_id))
        );
    }
}
