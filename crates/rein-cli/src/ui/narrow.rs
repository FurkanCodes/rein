//! Narrowing a menu to what's typed. Each word must be found, in an item's
//! name or in what's said of it, and a name counts for more. In a name the
//! letters of a word count too, in order, each one starting a word or
//! right after the one before, so `vrbp` finds `vercel-react-best-practices`;
//! never in what's said of it, where almost any word's letters are
//! somewhere: `react` once kept 171 of 210 skills, for "pRactices".

use std::ops::Range;

/// What's typed, as the words to find, in lower case.
pub struct Query {
    words: Vec<Vec<char>>,
}

/// Where a word was found, best first.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Place {
    /// The name starts with it: `skill` in `/skill-tester`.
    NameStart,
    /// A word in the name starts with it: `design` in `ckm:design-system`.
    NameWord,
    /// The name has it: `test` in `/pytest`.
    InName,
    /// A word of what's said starts with it.
    AboutWord,
    /// What's said has it, inside a word; only for a word of three letters
    /// or more, as two are inside almost anything.
    InAbout,
    /// The name has its letters, in order, each starting a word or right
    /// after the one before.
    NameLetters,
}

/// How well an item matches: less is better, then the earlier item.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Rank {
    /// Each word's place, added up.
    places: u32,
    /// How well the letters of words found only that way fit, negated.
    letters: i64,
}

/// Where the words are, in characters: in the name, and in what's said.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Marks {
    pub name: Vec<Range<usize>>,
    pub about: Vec<Range<usize>>,
}

/// One word, found: where, and at which characters.
struct Found {
    place: Place,
    at: Vec<Range<usize>>,
    letters: i64,
}

impl Query {
    pub fn new(typed: &str) -> Query {
        Query { words: typed.split_whitespace().map(|word| word.chars().map(lower).collect()).collect() }
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// How well `name` and `about` match: `None` when a word isn't found.
    pub fn rank(&self, name: &str, about: &str) -> Option<Rank> {
        let (name, about) = (Text::new(name), Text::new(about));
        let mut rank = Rank { places: 0, letters: 0 };
        for word in &self.words {
            let found = find(word, &name, &about)?;
            rank.places += found.place as u32;
            rank.letters -= found.letters;
        }
        Some(rank)
    }

    /// Where the words are in `name` and `about`, each where it counted.
    pub fn marks(&self, name: &str, about: &str) -> Marks {
        let (name, about) = (Text::new(name), Text::new(about));
        let mut marks = Marks::default();
        for found in self.words.iter().filter_map(|word| find(word, &name, &about)) {
            let into = if found.place < Place::AboutWord || found.place == Place::NameLetters { &mut marks.name } else { &mut marks.about };
            into.extend(found.at);
        }
        marks.name = merged(marks.name);
        marks.about = merged(marks.about);
        marks
    }
}

/// A text to look in: its characters in lower case, one for one, and
/// which of them start a word.
struct Text {
    lower: Vec<char>,
    starts: Vec<bool>,
}

impl Text {
    fn new(text: &str) -> Text {
        let chars: Vec<char> = text.chars().collect();
        let starts = (0..chars.len())
            .map(|i| i == 0 || !chars[i - 1].is_alphanumeric() || (chars[i].is_uppercase() && chars[i - 1].is_lowercase()) || (chars[i].is_numeric() != chars[i - 1].is_numeric()))
            .collect();
        Text { lower: chars.into_iter().map(lower).collect(), starts }
    }

    /// Where `word` is, each place it starts.
    fn at<'a>(&'a self, word: &'a [char]) -> impl Iterator<Item = usize> + 'a {
        self.lower.windows(word.len()).enumerate().filter(move |(_, window)| *window == word).map(|(i, _)| i)
    }

    /// Where the first letter or digit is: a name's start, past a `/`.
    fn first(&self) -> usize {
        self.lower.iter().position(|c| c.is_alphanumeric()).unwrap_or(0)
    }
}

/// `c` in lower case, one character for one, so positions stay the same.
fn lower(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Where `word` is best found in `name` or `about`.
fn find(word: &[char], name: &Text, about: &Text) -> Option<Found> {
    let span = |i: usize| vec![Range { start: i, end: i + word.len() }];
    let whole = |place: Place, i: usize| Found { place, at: span(i), letters: 0 };
    if let Some(i) = name.at(word).find(|&i| i == name.first()) {
        return Some(whole(Place::NameStart, i));
    }
    if let Some(i) = name.at(word).find(|&i| name.starts[i]) {
        return Some(whole(Place::NameWord, i));
    }
    if let Some(i) = name.at(word).next() {
        return Some(whole(Place::InName, i));
    }
    if let Some(i) = about.at(word).find(|&i| about.starts[i]) {
        return Some(whole(Place::AboutWord, i));
    }
    if word.len() >= 3
        && let Some(i) = about.at(word).next()
    {
        return Some(whole(Place::InAbout, i));
    }
    let (letters, at) = letters(word, name)?;
    Some(Found { place: Place::NameLetters, at: at.into_iter().map(|i| i..i + 1).collect(), letters })
}

/// `word`'s letters in `name`, in order, each starting a word or right
/// after the one before: how well they fit, and where, the best of all
/// the ways they do. Letters together count for more than letters apart.
fn letters(word: &[char], name: &Text) -> Option<(i64, Vec<usize>)> {
    let n = name.lower.len();
    let (first, rest) = word.split_first()?;
    // best[k][i]: the best fit of the word's first k + 1 letters, the last
    // at i; from[k][i], where the letter before it is.
    let mut best: Vec<Vec<Option<i64>>> = vec![(0..n).map(|i| (name.lower[i] == *first && name.starts[i]).then_some(1)).collect()];
    let mut from: Vec<Vec<usize>> = vec![vec![0; n]];
    for &c in rest {
        let before = &best[best.len() - 1];
        let (mut next, mut back) = (vec![None; n], vec![0; n]);
        // The best fit so far ending before i - 1, and where.
        let mut apart: Option<(i64, usize)> = None;
        for i in 1..n {
            if i >= 2
                && let Some(score) = before[i - 2]
                && apart.is_none_or(|(best, _)| score > best)
            {
                apart = Some((score, i - 2));
            }
            if name.lower[i] != c {
                continue;
            }
            let together = before[i - 1].map(|score| (score + 3, i - 1));
            let started = apart.filter(|_| name.starts[i]).map(|(score, at)| (score + 1, at));
            if let Some((score, at)) = together.into_iter().chain(started).max_by_key(|(score, _)| *score) {
                next[i] = Some(score);
                back[i] = at;
            }
        }
        best.push(next);
        from.push(back);
    }
    let last = best.len() - 1;
    let (mut at, score) = best[last].iter().enumerate().filter_map(|(i, score)| Some((i, (*score)?))).max_by_key(|(i, score)| (*score, std::cmp::Reverse(*i)))?;
    let mut at_each = vec![at; best.len()];
    for k in (1..best.len()).rev() {
        at = from[k][at];
        at_each[k - 1] = at;
    }
    Some((score, at_each))
}

/// `ranges` in order, those that touch made one.
fn merged(mut ranges: Vec<Range<usize>>) -> Vec<Range<usize>> {
    ranges.sort_by_key(|range| range.start);
    let mut out: Vec<Range<usize>> = Vec::new();
    for range in ranges {
        match out.last_mut() {
            Some(last) if range.start <= last.end => last.end = last.end.max(range.end),
            _ => out.push(range),
        }
    }
    out
}

/// `text` from a little before the first of `marks` when it ends past
/// `room` columns, so what matched shows: with `…` where it was cut, and
/// the marks moved with it.
pub fn around(text: &str, marks: &[Range<usize>], room: usize) -> (String, Vec<Range<usize>>) {
    let chars: Vec<char> = text.chars().collect();
    let Some(first) = marks.first().filter(|first| width(&chars[..first.end.min(chars.len())]) > room) else {
        return (text.to_owned(), marks.to_vec());
    };
    // A few words before it, from the start of one.
    let mut start = first.start.saturating_sub(room / 4);
    while start > 0 && start < first.start && !chars[start - 1].is_whitespace() {
        start += 1;
    }
    let shown: String = std::iter::once('…').chain(chars[start..].iter().copied()).collect();
    let moved = marks.iter().filter(|mark| mark.start >= start).map(|mark| mark.start - start + 1..mark.end - start + 1).collect();
    (shown, moved)
}

fn width(chars: &[char]) -> usize {
    chars.iter().map(|&c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0)).sum()
}

/// `text` with the characters in `marks` drawn by `marked`, the rest by
/// `plain`.
pub fn paint(text: &str, marks: &[Range<usize>], plain: impl Fn(&str) -> String, marked: impl Fn(&str) -> String) -> String {
    let chars: Vec<char> = text.chars().collect();
    let mut out = String::new();
    let mut at = 0;
    for mark in marks.iter().filter(|mark| mark.start < chars.len()) {
        let end = mark.end.min(chars.len());
        if mark.start > at {
            out.push_str(&plain(&chars[at..mark.start].iter().collect::<String>()));
        }
        out.push_str(&marked(&chars[mark.start.max(at)..end].iter().collect::<String>()));
        at = end;
    }
    if at < chars.len() {
        out.push_str(&plain(&chars[at..].iter().collect::<String>()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The items `typed` keeps, best first, as a menu lists them.
    fn narrowed<'a>(typed: &str, items: &[(&'a str, &str)]) -> Vec<&'a str> {
        let query = Query::new(typed);
        let mut ranked: Vec<(Rank, usize)> = items.iter().enumerate().filter_map(|(i, (name, about))| Some((query.rank(name, about)?, i))).collect();
        ranked.sort();
        ranked.into_iter().map(|(_, i)| items[i].0).collect()
    }

    const SKILLS: &[(&str, &str)] = &[
        ("/supabase-postgres-best-practices", "on · Postgres best practices maintained by Supabase, for Postgres running anywhere."),
        ("/engineering-advanced-skills:feature-flags-architect", "a plugin's · on · Use when adding, retiring, or auditing feature flags."),
        ("/vercel-react-best-practices", "on · React and Next.js performance optimization guidelines from Vercel Engineering."),
        ("/python-testing-patterns", "on · Implement comprehensive testing strategies with pytest, fixtures and mocks."),
        ("/react-native", "on · Build mobile apps."),
        ("/frontend-checks", "on · Lint the styles, check the bundle size, then set up Vitest for React components and run it."),
        ("/latest-news", "on · What's new."),
    ];

    #[test]
    fn every_word_is_found_whole_and_names_count_for_more() {
        assert_eq!(narrowed("react", SKILLS), ["/react-native", "/vercel-react-best-practices", "/frontend-checks"], "not for the letters of pRactices");
        assert_eq!(narrowed("react test", SKILLS), ["/frontend-checks"], "every word: Vitest has test in it");
        assert_eq!(narrowed("TEST", SKILLS), ["/python-testing-patterns", "/latest-news", "/frontend-checks"], "case aside; a name first");
        assert_eq!(narrowed("vrbp", SKILLS), ["/vercel-react-best-practices"], "a name's letters, each starting a word");
        assert_eq!(narrowed("eaff", SKILLS), ["/engineering-advanced-skills:feature-flags-architect"], "the words' first letters");
        assert!(narrowed("rct", SKILLS).is_empty(), "letters only start a word or follow the one before");
        assert_eq!(narrowed("plugin's", SKILLS), ["/engineering-advanced-skills:feature-flags-architect"], "what it's said to be");
        assert!(narrowed("mp", SKILLS).is_empty(), "two letters inside a word of what's said aren't enough");
        assert!(Query::new("  ").is_empty(), "nothing typed: the menu shows all, as it was");
    }

    #[test]
    fn the_letters_shown_are_where_they_fit_best() {
        let query = Query::new("vrbp");
        assert_eq!(query.marks("/vercel-react-best-practices", "").name, [1..2, 8..9, 14..15, 19..20]);
        let query = Query::new("gpt56");
        assert_eq!(query.marks("gpt-5.6-sol", "").name, [0..3, 4..5, 6..7], "digits start a word of their own");
        let query = Query::new("abc");
        assert_eq!(query.marks("a-b-c-ab-cx", "").name, [6..8, 9..10], "of the ways they fit, the one with letters together");
        let query = Query::new("react vitest");
        let marks = query.marks("/frontend-checks", "Set up Vitest for React.");
        assert_eq!((marks.name, marks.about), (vec![], vec![7..13, 18..23]));
    }

    #[test]
    fn what_matched_past_the_room_is_shown_with_a_little_before_it() {
        let about = "Lint the styles, check the bundle size, then set up Vitest for React components and run it.";
        let marks = Query::new("vitest").marks("/frontend-checks", about).about;
        let (shown, moved) = around(about, &marks, 30);
        assert_eq!(shown, "…set up Vitest for React components and run it.");
        assert_eq!(shown.chars().skip(moved[0].start).take(6).collect::<String>(), "Vitest");
        assert_eq!(around(about, &marks, 80), (about.to_owned(), marks.clone()), "it shows already: as it is");
        let painted = paint("Vitest x", &[Range { start: 0, end: 6 }], |text| text.to_owned(), |text| format!("[{text}]"));
        assert_eq!(painted, "[Vitest] x");
    }
}
