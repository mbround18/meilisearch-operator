#[test]
fn generates_64_char_key() {
    use rand::{Rng, distributions::Alphanumeric};
    let key: String = rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();
    assert_eq!(key.len(), 64);
}
