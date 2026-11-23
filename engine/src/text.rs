//! Text matching: `LIKE`, words for full-text search, English stemming,
//! excerpts, and trigram similarity.

use std::collections::{HashMap, HashSet};

use database_protocol::schema::TextConfig;

// ------------------------------------------------------------------ like

enum Part {
    /// These bytes exactly.
    Literal(Vec<u8>),
    /// Any one character.
    One,
}

/// A `LIKE` pattern, cut at each `%`.
pub struct Like {
    segments: Vec<Vec<Part>>,
    case_insensitive: bool,
}

impl Like {
    /// `%` matches any run of characters, `_` any one, and a backslash
    /// makes the next character literal.
    pub fn new(pattern: &str, case_insensitive: bool) -> Like {
        let pattern = if case_insensitive { pattern.to_lowercase() } else { pattern.to_string() };
        let mut segments = vec![Vec::new()];
        let mut literal = Vec::new();
        let mut chars = pattern.chars();
        let flush = |literal: &mut Vec<u8>, segments: &mut Vec<Vec<Part>>| {
            if !literal.is_empty() {
                segments.last_mut().unwrap().push(Part::Literal(std::mem::take(literal)));
            }
        };
        while let Some(c) = chars.next() {
            match c {
                '%' => {
                    flush(&mut literal, &mut segments);
                    segments.push(Vec::new());
                }
                '_' => {
                    flush(&mut literal, &mut segments);
                    segments.last_mut().unwrap().push(Part::One);
                }
                '\\' => {
                    let next = chars.next().unwrap_or('\\');
                    literal.extend_from_slice(next.encode_utf8(&mut [0; 4]).as_bytes());
                }
                c => literal.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            }
        }
        flush(&mut literal, &mut segments);
        Like { segments, case_insensitive }
    }

    pub fn matches(&self, text: &str) -> bool {
        if self.case_insensitive {
            self.run(text.to_lowercase().as_bytes())
        } else {
            self.run(text.as_bytes())
        }
    }

    fn run(&self, text: &[u8]) -> bool {
        let last = self.segments.len() - 1;
        if last == 0 {
            return match_at(&self.segments[0], text, 0) == Some(text.len());
        }
        // The first segment must sit at the start and the last at the
        // end; each one between is taken at its earliest place.
        let Some(mut pos) = match_at(&self.segments[0], text, 0) else { return false };
        for segment in &self.segments[1..last] {
            match find(segment, text, pos) {
                Some(end) => pos = end,
                None => return false,
            }
        }
        let tail = &self.segments[last];
        if tail.is_empty() {
            return true;
        }
        // Try the last segment at each place it could end the text.
        let mut start = pos;
        while start <= text.len() {
            if match_at(tail, text, start) == Some(text.len()) {
                return true;
            }
            start = next_char(text, start);
        }
        false
    }
}

fn next_char(text: &[u8], at: usize) -> usize {
    let mut next = at + 1;
    while next < text.len() && text[next] & 0xC0 == 0x80 {
        next += 1;
    }
    next
}

/// Where a segment ends if it matches at `at`.
fn match_at(segment: &[Part], text: &[u8], mut at: usize) -> Option<usize> {
    for part in segment {
        match part {
            Part::Literal(bytes) => {
                if !text[at.min(text.len())..].starts_with(bytes) {
                    return None;
                }
                at += bytes.len();
            }
            Part::One => {
                if at >= text.len() {
                    return None;
                }
                at = next_char(text, at);
            }
        }
    }
    Some(at)
}

/// Where a segment ends at its first match at or after `from`.
fn find(segment: &[Part], text: &[u8], from: usize) -> Option<usize> {
    if segment.is_empty() {
        return Some(from);
    }
    if let [Part::Literal(bytes)] = segment {
        return find_bytes(&text[from..], bytes).map(|i| from + i + bytes.len());
    }
    let mut start = from;
    while start <= text.len() {
        if let Some(end) = match_at(segment, text, start) {
            return Some(end);
        }
        start = next_char(text, start);
    }
    None
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() {
        return Some(0);
    }
    let first = needle[0];
    let mut at = 0;
    while at + needle.len() <= haystack.len() {
        match haystack[at..=haystack.len() - needle.len()].iter().position(|b| *b == first) {
            Some(i) => {
                at += i;
                if haystack[at..].starts_with(needle) {
                    return Some(at);
                }
                at += 1;
            }
            None => return None,
        }
    }
    None
}

// ----------------------------------------------------------------- words

/// Only this much of a text is cut into words: a megabyte of prose is
/// more than any search needs, and a file of tens of megabytes pasted
/// into a body should not become a million index entries.
pub const MAX_INDEXED_BYTES: usize = 1_000_000;

/// In order, to be searched by halving.
const STOP_WORDS: &[&str] = &[
    "a", "about", "above", "after", "again", "against", "all", "am", "an", "and", "any", "are", "as", "at", "be", "because", "been", "before",
    "being", "below", "between", "both", "but", "by", "can", "did", "do", "does", "doing", "don", "down", "during", "each", "few", "for", "from",
    "further", "had", "has", "have", "having", "he", "her", "here", "hers", "herself", "him", "himself", "his", "how", "i", "if", "in", "into",
    "is", "it", "its", "itself", "just", "me", "more", "most", "my", "myself", "no", "nor", "not", "now", "of", "off", "on", "once", "only", "or",
    "other", "our", "ours", "ourselves", "out", "over", "own", "s", "same", "she", "should", "so", "some", "such", "t", "than", "that", "the",
    "their", "theirs", "them", "themselves", "then", "there", "these", "they", "this", "those", "through", "to", "too", "under", "until", "up",
    "very", "was", "we", "were", "what", "when", "where", "which", "while", "who", "whom", "why", "will", "with", "you", "your", "yours",
    "yourself", "yourselves",
];

/// A word of a text: where it lies, and the form it is indexed under.
pub struct Word {
    pub start: usize,
    pub end: usize,
    /// Empty for a word the configuration drops.
    pub term: String,
}

/// Cuts text into runs of letters and digits.
pub fn words(text: &str, config: TextConfig) -> Vec<Word> {
    let text = truncate(text, MAX_INDEXED_BYTES);
    let mut out = Vec::new();
    let mut start = None;
    for (i, c) in text.char_indices().chain(std::iter::once((text.len(), ' '))) {
        match (c.is_alphanumeric(), start) {
            (true, None) => start = Some(i),
            (false, Some(s)) => {
                out.push(Word { start: s, end: i, term: normalize(&text[s..i], config) });
                start = None;
            }
            _ => {}
        }
    }
    out
}

fn truncate(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn normalize(word: &str, config: TextConfig) -> String {
    let lower = word.to_lowercase();
    match config {
        TextConfig::Simple => lower,
        TextConfig::English => {
            if STOP_WORDS.binary_search(&lower.as_str()).is_ok() {
                String::new()
            } else {
                stem(&lower)
            }
        }
    }
}

/// The distinct terms of a text, each with how often it occurs.
pub fn terms(text: &str, config: TextConfig) -> HashMap<String, u32> {
    let mut out = HashMap::new();
    for word in words(text, config) {
        if !word.term.is_empty() {
            *out.entry(word.term).or_insert(0) += 1;
        }
    }
    out
}

/// The terms a search looks for, in order, each once.
pub fn query_terms(query: &str, config: TextConfig) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for word in words(query, config) {
        if !word.term.is_empty() && !out.contains(&word.term) {
            out.push(word.term);
        }
    }
    out
}

fn term_matches(term: &str, wanted: &str, prefix: bool) -> bool {
    if prefix {
        term.starts_with(wanted)
    } else {
        term == wanted
    }
}

/// How often the query's words occur in a text, or `None` unless every
/// one of them does. A query with no words matches nothing.
pub fn match_count(text: &str, query: &str, config: TextConfig, prefix: bool) -> Option<u32> {
    let wanted = query_terms(query, config);
    if wanted.is_empty() {
        return None;
    }
    let have = terms(text, config);
    let mut total = 0;
    for w in &wanted {
        let n: u32 = if prefix { have.iter().filter(|(t, _)| t.starts_with(w.as_str())).map(|(_, n)| *n).sum() } else { have.get(w).copied().unwrap_or(0) };
        if n == 0 {
            return None;
        }
        total += n;
    }
    Some(total)
}

/// An excerpt of `text` around the first words the query matches, with
/// each match wrapped in `start` and `stop`. With no match, the first
/// `min_words` words.
pub fn headline(text: &str, query: &str, config: TextConfig, start: &str, stop: &str, max_words: usize, min_words: usize) -> String {
    let wanted = query_terms(query, config);
    let all = words(text, config);
    if all.is_empty() {
        return String::new();
    }
    let is_match = |w: &Word| !w.term.is_empty() && wanted.iter().any(|q| term_matches(&w.term, q, false));
    let (from, to) = match all.iter().position(is_match) {
        // A few words of lead-in, then as many as allowed.
        Some(first) => {
            let from = first.saturating_sub(max_words.max(1) / 5);
            (from, (from + max_words.max(1)).min(all.len()))
        }
        None => (0, min_words.max(1).min(all.len())),
    };
    let mut out = String::new();
    let mut at = all[from].start;
    for word in &all[from..to] {
        out.push_str(&text[at..word.start]);
        if is_match(word) {
            out.push_str(start);
            out.push_str(&text[word.start..word.end]);
            out.push_str(stop);
        } else {
            out.push_str(&text[word.start..word.end]);
        }
        at = word.end;
    }
    out
}

// --------------------------------------------------------------- stemmer

fn consonant(w: &[u8], i: usize) -> bool {
    match w[i] {
        b'a' | b'e' | b'i' | b'o' | b'u' => false,
        b'y' => i == 0 || !consonant(w, i - 1),
        _ => true,
    }
}

/// The number of vowel-consonant sequences in `w`: Porter's measure.
fn measure(w: &[u8]) -> usize {
    let mut m = 0;
    let mut i = 0;
    while i < w.len() && consonant(w, i) {
        i += 1;
    }
    loop {
        while i < w.len() && !consonant(w, i) {
            i += 1;
        }
        if i >= w.len() {
            return m;
        }
        while i < w.len() && consonant(w, i) {
            i += 1;
        }
        m += 1;
    }
}

fn has_vowel(w: &[u8]) -> bool {
    (0..w.len()).any(|i| !consonant(w, i))
}

fn double_consonant(w: &[u8]) -> bool {
    let n = w.len();
    n >= 2 && w[n - 1] == w[n - 2] && consonant(w, n - 1)
}

/// Ends consonant-vowel-consonant, the last not w, x or y.
fn cvc(w: &[u8]) -> bool {
    let n = w.len();
    n >= 3 && consonant(w, n - 3) && !consonant(w, n - 2) && consonant(w, n - 1) && !matches!(w[n - 1], b'w' | b'x' | b'y')
}

/// Replaces `suffix` by `with` if the word ends in it and what is left
/// has a measure above `min`. Returns whether the suffix was there.
fn replace(w: &mut Vec<u8>, suffix: &str, with: &str, min: usize) -> bool {
    if !w.ends_with(suffix.as_bytes()) {
        return false;
    }
    let stem = w.len() - suffix.len();
    if measure(&w[..stem]) > min {
        w.truncate(stem);
        w.extend_from_slice(with.as_bytes());
    }
    true
}

/// The Porter stemmer: `invoices` and `invoiced` both become `invoic`.
/// Words with anything but ASCII letters are left as they are.
pub fn stem(word: &str) -> String {
    if word.len() <= 2 || !word.bytes().all(|b| b.is_ascii_lowercase()) {
        return word.to_string();
    }
    let mut w = word.as_bytes().to_vec();

    // 1a: plurals.
    if w.ends_with(b"sses") || w.ends_with(b"ies") {
        w.truncate(w.len() - 2);
    } else if w.ends_with(b"s") && !w.ends_with(b"ss") {
        w.pop();
    }

    // 1b: -ed and -ing.
    let mut tidy = false;
    if w.ends_with(b"eed") {
        if measure(&w[..w.len() - 3]) > 0 {
            w.pop();
        }
    } else if w.ends_with(b"ed") && has_vowel(&w[..w.len() - 2]) {
        w.truncate(w.len() - 2);
        tidy = true;
    } else if w.ends_with(b"ing") && has_vowel(&w[..w.len() - 3]) {
        w.truncate(w.len() - 3);
        tidy = true;
    }
    if tidy {
        if w.ends_with(b"at") || w.ends_with(b"bl") || w.ends_with(b"iz") {
            w.push(b'e');
        } else if double_consonant(&w) && !matches!(w[w.len() - 1], b'l' | b's' | b'z') {
            w.pop();
        } else if measure(&w) == 1 && cvc(&w) {
            w.push(b'e');
        }
    }

    // 1c: a final y after a vowel.
    if w.ends_with(b"y") && has_vowel(&w[..w.len() - 1]) {
        let n = w.len();
        w[n - 1] = b'i';
    }

    // 2 and 3: double suffixes down to single ones.
    const STEP2: &[(&str, &str)] = &[
        ("ational", "ate"), ("tional", "tion"), ("enci", "ence"), ("anci", "ance"), ("izer", "ize"), ("abli", "able"), ("alli", "al"),
        ("entli", "ent"), ("eli", "e"), ("ousli", "ous"), ("ization", "ize"), ("ation", "ate"), ("ator", "ate"), ("alism", "al"),
        ("iveness", "ive"), ("fulness", "ful"), ("ousness", "ous"), ("aliti", "al"), ("iviti", "ive"), ("biliti", "ble"),
    ];
    const STEP3: &[(&str, &str)] = &[("icate", "ic"), ("ative", ""), ("alize", "al"), ("iciti", "ic"), ("ical", "ic"), ("ful", ""), ("ness", "")];
    for rules in [STEP2, STEP3] {
        for (suffix, with) in rules {
            if replace(&mut w, suffix, with, 0) {
                break;
            }
        }
    }

    // 4: single suffixes, where enough of a word is left.
    const STEP4: &[&str] =
        &["al", "ance", "ence", "er", "ic", "able", "ible", "ant", "ement", "ment", "ent", "ion", "ou", "ism", "ate", "iti", "ous", "ive", "ize"];
    for suffix in STEP4 {
        if w.ends_with(suffix.as_bytes()) {
            let stem = w.len() - suffix.len();
            let allowed = *suffix != "ion" || (stem > 0 && matches!(w[stem - 1], b's' | b't'));
            if allowed && measure(&w[..stem]) > 1 {
                w.truncate(stem);
            }
            break;
        }
    }

    // 5: a final e, and a doubled l.
    if w.ends_with(b"e") {
        let stem = &w[..w.len() - 1];
        let m = measure(stem);
        if m > 1 || (m == 1 && !cvc(stem)) {
            w.pop();
        }
    }
    if measure(&w) > 1 && double_consonant(&w) && w.ends_with(b"l") {
        w.pop();
    }
    String::from_utf8(w).unwrap_or_else(|_| word.to_string())
}

// ------------------------------------------------------------ similarity

fn trigrams(text: &str) -> HashSet<[char; 3]> {
    let mut out = HashSet::new();
    let lower = text.to_lowercase();
    for word in lower.split(|c: char| !c.is_alphanumeric()).filter(|w| !w.is_empty()) {
        // Each word padded with two spaces before and one after, as
        // pg_trgm does, so that word starts weigh more than word ends.
        let padded: Vec<char> = "  ".chars().chain(word.chars()).chain(" ".chars()).collect();
        for window in padded.windows(3) {
            out.insert([window[0], window[1], window[2]]);
        }
    }
    out
}

/// How alike two texts are, from 0 to 1: the share of their three-letter
/// runs that both have. The measure of Postgres' `pg_trgm`.
pub fn similarity(a: &str, b: &str) -> f64 {
    let (a, b) = (trigrams(a), trigrams(b));
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let shared = a.intersection(&b).count();
    // Rounded as a 32-bit float, which is what pg_trgm returns.
    f64::from((shared as f64 / (a.len() + b.len() - shared) as f64) as f32)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn like(text: &str, pattern: &str) -> bool {
        Like::new(pattern, false).matches(text)
    }

    #[test]
    fn stop_words_are_in_order() {
        assert!(STOP_WORDS.windows(2).all(|pair| pair[0] < pair[1]));
        assert_eq!(normalize("The", TextConfig::English), "");
        assert_eq!(normalize("Yourselves", TextConfig::English), "");
        assert_eq!(normalize("theory", TextConfig::English), "theori");
    }

    #[test]
    fn like_follows_sql() {
        assert!(like("hello", "hello"));
        assert!(!like("hello", "hell"));
        assert!(like("hello", "h%"));
        assert!(like("hello", "%llo"));
        assert!(like("hello", "%ell%"));
        assert!(like("hello", "h_llo"));
        assert!(!like("hllo", "h_llo"));
        assert!(like("hello", "%"));
        assert!(like("", "%"));
        assert!(!like("", "_"));
        assert!(like("abcabc", "%abc"));
        assert!(like("aXbXc", "a%b%c"));
        assert!(!like("aXbX", "a%b%c"));
        assert!(like("héllo", "h_llo"), "_ is one character, not one byte");
        assert!(like("50%", "50\\%"));
        assert!(!like("500", "50\\%"));
        assert!(like("a_b", "a\\_b"));
        assert!(!like("axb", "a\\_b"));
        assert!(like("back\\slash", "back\\\\slash"));
        assert!(like("xx.copy@server", "%.copy@server"));
        assert!(like("INBOX/accounts/sales", "INBOX/accounts/%"));
        assert!(like("aaa", "%a%a%a%"));
        assert!(!like("aa", "%a%a%a%"));
        assert!(like("abab", "%ab"));
        assert!(Like::new("%WORLD%", true).matches("Hello World"));
        assert!(!Like::new("%WORLD%", false).matches("Hello World"));
        assert!(Like::new("É%", true).matches("école"));
    }

    #[test]
    fn stems_as_porter_does() {
        let pairs = [
            ("caresses", "caress"), ("ponies", "poni"), ("ties", "ti"), ("caress", "caress"), ("cats", "cat"), ("feed", "feed"),
            ("agreed", "agre"), ("plastered", "plaster"), ("bled", "bled"), ("motoring", "motor"), ("sing", "sing"),
            ("conflated", "conflat"), ("troubled", "troubl"), ("sized", "size"), ("hopping", "hop"), ("tanned", "tan"),
            ("falling", "fall"), ("hissing", "hiss"), ("fizzed", "fizz"), ("failing", "fail"), ("filing", "file"), ("happy", "happi"),
            ("sky", "sky"), ("relational", "relat"), ("conditional", "condit"), ("rational", "ration"), ("digitizer", "digit"),
            ("vietnamization", "vietnam"), ("predication", "predic"), ("operator", "oper"), ("feudalism", "feudal"),
            ("decisiveness", "decis"), ("hopefulness", "hope"), ("callousness", "callous"), ("triplicate", "triplic"),
            ("formative", "form"), ("formalize", "formal"), ("electrical", "electr"), ("hopeful", "hope"), ("goodness", "good"),
            ("revival", "reviv"), ("allowance", "allow"), ("inference", "infer"), ("airliner", "airlin"), ("adjustable", "adjust"),
            ("defensible", "defens"), ("irritant", "irrit"), ("replacement", "replac"), ("adjustment", "adjust"),
            ("dependent", "depend"), ("adoption", "adopt"), ("communism", "commun"), ("activate", "activ"),
            ("homologous", "homolog"), ("effective", "effect"), ("bowdlerize", "bowdler"), ("probate", "probat"), ("rate", "rate"),
            ("cease", "ceas"), ("controlling", "control"), ("rolling", "roll"), ("invoices", "invoic"), ("invoice", "invoic"),
            ("invoiced", "invoic"), ("meeting", "meet"), ("meetings", "meet"), ("running", "run"), ("payments", "payment"),
        ];
        for (word, want) in pairs {
            assert_eq!(stem(word), want, "{word}");
        }
        assert_eq!(stem("naïve"), "naïve");
        assert_eq!(stem("x1"), "x1");
    }

    #[test]
    fn words_and_terms() {
        let simple: Vec<String> = words("Hello, wörld! user@example.com x-ray 42", TextConfig::Simple).into_iter().map(|w| w.term).collect();
        assert_eq!(simple, ["hello", "wörld", "user", "example", "com", "x", "ray", "42"]);
        assert_eq!(query_terms("The invoices are overdue, the INVOICE", TextConfig::English), ["invoic", "overdu"]);
        assert_eq!(terms("pay the payment paying", TextConfig::English).get("pai"), Some(&2));
    }

    #[test]
    fn matching_needs_every_word() {
        let text = "Your invoice for March is overdue. Please pay the invoice.";
        assert_eq!(match_count(text, "invoices overdue", TextConfig::English, false), Some(3));
        assert_eq!(match_count(text, "invoice refund", TextConfig::English, false), None);
        assert_eq!(match_count(text, "the", TextConfig::English, false), None, "only stop words: nothing to look for");
        assert_eq!(match_count(text, "inv over", TextConfig::Simple, true), Some(3));
        assert_eq!(match_count(text, "inv over", TextConfig::Simple, false), None);
    }

    #[test]
    fn headline_marks_the_matches() {
        let text = "Dear customer, your invoice for March is overdue. Please pay the invoice today.";
        let h = headline(text, "invoices", TextConfig::English, "<em>", "</em>", 50, 10);
        assert_eq!(h, "Dear customer, your <em>invoice</em> for March is overdue. Please pay the <em>invoice</em> today");
        let long: String = (0..200).map(|i| format!("word{i} ")).collect::<String>() + "needle and more words after it";
        let h = headline(&long, "needle", TextConfig::Simple, "[", "]", 10, 3);
        assert!(h.contains("[needle]") && h.split_whitespace().count() <= 10, "{h}");
        assert_eq!(headline(text, "refund", TextConfig::English, "<em>", "</em>", 50, 3), "Dear customer, your");
        assert_eq!(headline("", "x", TextConfig::Simple, "<", ">", 5, 5), "");
    }

    #[test]
    fn similarity_matches_pg_trgm() {
        assert_eq!(similarity("word", "two words"), 0.363_636_37_f32 as f64);
        assert_eq!(similarity("Starbucks", "starbucks"), 1.0);
        assert_eq!(similarity("abc", "xyz"), 0.0);
        assert_eq!(similarity("", "abc"), 0.0);
        // 13 shared runs of 17 in all.
        assert!((similarity("Amazon Prime", "AMAZON PRIME*2K4") - 13.0 / 17.0).abs() < 1e-6);
    }
}
