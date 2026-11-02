//! Matching what's typed after `@` against the project's paths, the way
//! file finders do: the letters in order, not necessarily together, so
//! `prsr` finds `src/parser.rs`.

/// How well `query` matches `path`, ignoring case: `None` if its letters
/// aren't all there in order. Higher is better. Letters next to each other
/// count for more, and so do letters that start a word (after `/`, `_`,
/// `-`, `.` or a lower-case letter, for an upper-case one) and letters in
/// the file's own name. Of all the ways the letters fit, the best counts,
/// not just the first: `step` should find the `st` that starts `steps.rs`,
/// not the `s` in `crates`. A shorter path wins a tie.
pub fn score(query: &str, path: &str) -> Option<i64> {
    let wanted: Vec<char> = query.chars().flat_map(char::to_lowercase).collect();
    // Most paths don't have the letters at all: a quick look says so
    // before the careful one.
    let mut rest = path.chars().flat_map(char::to_lowercase);
    if !wanted.iter().all(|&c| rest.any(|p| p == c)) {
        return None;
    }
    let chars: Vec<char> = path.chars().collect();
    if wanted.is_empty() {
        return Some(-(chars.len() as i64));
    }
    let name_starts = path.trim_end_matches('/').rfind('/').map_or(0, |i| path[..=i].chars().count());
    let worth = |i: usize| {
        let starts_word = i == 0 || matches!(chars[i - 1], '/' | '_' | '-' | '.' | ' ') || (chars[i].is_uppercase() && chars[i - 1].is_lowercase());
        1 + if starts_word { 6 } else { 0 } + if i >= name_starts { 3 } else { 0 }
    };
    let same = |i: usize, c: char| chars[i].to_lowercase().eq(std::iter::once(c));
    // best[i]: the best score with the letters so far fitted, the last at i.
    let mut best: Vec<Option<i64>> = (0..chars.len()).map(|i| same(i, wanted[0]).then(|| worth(i))).collect();
    for &c in &wanted[1..] {
        let mut next = vec![None; chars.len()];
        // The best of the letters before, ending anywhere before i - 1.
        let mut before: Option<i64> = None;
        for i in 1..chars.len() {
            if i >= 2 {
                before = before.max(best[i - 2]);
            }
            if same(i, c) {
                let together = best[i - 1].map(|score| score + 8);
                let apart = before.map(|score| score - 2);
                next[i] = together.max(apart).map(|score| score + worth(i));
            }
        }
        best = next;
    }
    best.into_iter().flatten().max().map(|total| total * 16 - chars.len() as i64)
}

/// The `limit` best of `paths` for `query`, best first; for an empty
/// query, the first `limit` as they are.
pub fn best<'a>(query: &str, paths: &'a [String], limit: usize) -> Vec<&'a str> {
    if query.is_empty() {
        return paths.iter().take(limit).map(String::as_str).collect();
    }
    let mut scored: Vec<(i64, &str)> = paths.iter().filter_map(|path| Some((score(query, path)?, path.as_str()))).collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(limit).map(|(_, path)| path).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn paths(list: &[&str]) -> Vec<String> {
        list.iter().map(|p| (*p).to_owned()).collect()
    }

    #[test]
    fn finds_letters_in_order() {
        assert!(score("prsr", "src/parser.rs").is_some());
        assert!(score("PARSER", "src/parser.rs").is_some(), "case doesn't matter");
        assert!(score("srs", "src/parser.rs").is_some() && score("rsp", "src/parser.rs").is_none(), "in order: no p comes after r and s");
        assert!(score("xyz", "src/parser.rs").is_none());
    }

    #[test]
    fn ranks_the_likely_file_first() {
        let all = paths(&["docs/compare.md", "src/parser.rs", "src/ui/prompt.rs", "tests/parser_test.rs", "crates/rein-cli/src/ui/steps.rs"]);
        assert_eq!(best("parser", &all, 2), ["src/parser.rs", "tests/parser_test.rs"], "a whole word in the name, the shorter path first");
        assert_eq!(best("steps", &all, 1), ["crates/rein-cli/src/ui/steps.rs"]);
        assert_eq!(best("ui/pr", &all, 1), ["src/ui/prompt.rs"]);
        let deep = paths(&["crates/rein-cli/tests/screen.rs", "crates/rein-cli/src/ui/steps.rs"]);
        assert_eq!(best("ste", &deep, 1), ["crates/rein-cli/src/ui/steps.rs"], "the st that starts a name, not the s in crates");
        assert_eq!(best("scr", &deep, 1), ["crates/rein-cli/tests/screen.rs"]);
        assert_eq!(best("", &all, 2), ["docs/compare.md", "src/parser.rs"], "nothing typed: as they come");
    }
}
