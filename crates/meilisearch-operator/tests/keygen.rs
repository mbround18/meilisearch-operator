#[test]
fn generates_64_char_key() {
    use rand::{Rng, distr::Alphanumeric};
    let key: String = rand::rng()
        .sample_iter(Alphanumeric)
        .take(64)
        .map(char::from)
        .collect();
    assert_eq!(key.len(), 64);
}
