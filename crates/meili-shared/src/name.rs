pub fn normalize_kebab_dedup(input: &str) -> String {
    use std::collections::HashSet;
    let mut seen: HashSet<String> = HashSet::new();
    let mut parts_out: Vec<String> = Vec::new();
    for raw in input.split('-') {
        let part = raw.trim();
        if part.is_empty() {
            continue;
        }
        if seen.insert(part.to_string()) {
            parts_out.push(part.to_string());
        }
    }
    parts_out.join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removes_duplicate_tokens_and_trims() {
        assert_eq!(normalize_kebab_dedup("a-b-a-c"), "a-b-c");
        assert_eq!(normalize_kebab_dedup(" a - a - b "), "a-b");
        assert_eq!(normalize_kebab_dedup("--a---b--b-"), "a-b");
        assert_eq!(normalize_kebab_dedup(""), "");
    }
}
