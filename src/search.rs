//! Text normalisation and matching for full-text search.
//!
//! Page text is folded once (lowercase, ligatures expanded, whitespace runs
//! collapsed, line-end hyphenation joined) and cached; each query is then a
//! plain scan over the folded text, so repeated searches are near instant.

/// Folded page text plus a map from folded index to PDFium char index.
#[derive(Clone, Debug, Default)]
pub struct PageText {
    pub text: Vec<char>,
    pub map: Vec<u32>,
}

pub const MAX_HITS_PER_PAGE: usize = 2000;

fn expand(c: char, out: &mut impl FnMut(char)) {
    match c {
        '\u{FB00}' => "ff".chars().for_each(&mut *out),
        '\u{FB01}' => "fi".chars().for_each(&mut *out),
        '\u{FB02}' => "fl".chars().for_each(&mut *out),
        '\u{FB03}' => "ffi".chars().for_each(&mut *out),
        '\u{FB04}' => "ffl".chars().for_each(&mut *out),
        '\u{FB05}' | '\u{FB06}' => "st".chars().for_each(&mut *out),
        '\u{2018}' | '\u{2019}' | '\u{201B}' | '\u{2032}' => out('\''),
        '\u{201C}' | '\u{201D}' | '\u{201F}' | '\u{2033}' => out('"'),
        '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2212}' => out('-'),
        _ => {
            for l in c.to_lowercase() {
                out(l);
            }
        }
    }
}

fn is_space(c: char) -> bool {
    c.is_whitespace() || c.is_control()
}

/// Folds PDFium code points (one per char index) into searchable text.
pub fn fold_page(codepoints: &[u32]) -> PageText {
    let mut text: Vec<char> = Vec::with_capacity(codepoints.len());
    let mut map: Vec<u32> = Vec::with_capacity(codepoints.len());
    let mut pending_space: Option<u32> = None;
    // Set after joining a line-end hyphen: swallow the rest of the line break.
    let mut joined = false;
    for (i, &cp) in codepoints.iter().enumerate() {
        let i = i as u32;
        let c = char::from_u32(cp).unwrap_or(' ');
        // Soft hyphens and PDFium's "hyphen at end of line" marker.
        if c == '\u{00AD}' || c == '\u{0002}' || c == '\u{FFFE}' {
            continue;
        }
        if is_space(c) {
            let line_break = c == '\n' || c == '\r';
            // Join "exam-\nple" into "example".
            if line_break && text.len() >= 2 && text[text.len() - 1] == '-' && text[text.len() - 2].is_alphabetic() {
                text.pop();
                map.pop();
                pending_space = None;
                joined = true;
                continue;
            }
            if !joined && !text.is_empty() && pending_space.is_none() && text.last() != Some(&' ') {
                pending_space = Some(i);
            }
            continue;
        }
        joined = false;
        if let Some(si) = pending_space.take() {
            text.push(' ');
            map.push(si);
        }
        expand(c, &mut |f| {
            text.push(f);
            map.push(i);
        });
    }
    PageText { text, map }
}

/// Folds a user query the same way page text is folded.
pub fn fold_query(query: &str) -> Vec<char> {
    let mut out = Vec::new();
    let mut space = false;
    for c in query.chars() {
        if is_space(c) {
            space = !out.is_empty();
            continue;
        }
        if space {
            out.push(' ');
            space = false;
        }
        expand(c, &mut |f| out.push(f));
    }
    out
}

/// Non-overlapping matches of `query` in `page`, as PDFium `(start, count)` char ranges.
pub fn find(page: &PageText, query: &[char]) -> Vec<(usize, usize)> {
    let mut hits = Vec::new();
    let (t, q) = (&page.text, query);
    if q.is_empty() || q.len() > t.len() {
        return hits;
    }
    let first = q[0];
    let mut i = 0;
    while i + q.len() <= t.len() {
        if t[i] == first && t[i..i + q.len()] == *q {
            let start = page.map[i] as usize;
            let end = page.map[i + q.len() - 1] as usize;
            hits.push((start, end + 1 - start));
            if hits.len() >= MAX_HITS_PER_PAGE {
                break;
            }
            i += q.len();
        } else {
            i += 1;
        }
    }
    hits
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cps(s: &str) -> Vec<u32> {
        s.chars().map(|c| c as u32).collect()
    }

    #[test]
    fn folds_case_and_whitespace() {
        let p = fold_page(&cps("  Hello,\r\n  WORLD\t!"));
        assert_eq!(p.text.iter().collect::<String>(), "hello, world !");
        assert_eq!(p.text.len(), p.map.len());
    }

    #[test]
    fn joins_hyphenation_and_ligatures() {
        let p = fold_page(&cps("in\u{FB01}nite infor-\r\nmation"));
        assert_eq!(p.text.iter().collect::<String>(), "infinite information");
        let hits = find(&p, &fold_query("Information"));
        assert_eq!(hits.len(), 1);
        // "infor-\r\nmation" spans original chars 8..=21.
        assert_eq!(hits[0], (8, 14));
    }

    #[test]
    fn finds_across_line_breaks_and_maps_back() {
        let src = "The quick\r\nbrown fox. the QUICK brown";
        let p = fold_page(&cps(src));
        let hits = find(&p, &fold_query("quick  brown"));
        assert_eq!(hits.len(), 2);
        let (s, n) = hits[0];
        assert_eq!(&src[s..s + n], "quick\r\nbrown");
        let (s, n) = hits[1];
        assert_eq!(&src[s..s + n], "QUICK brown");
    }

    #[test]
    fn cyrillic_and_quotes() {
        let p = fold_page(&cps("Привет, “Мир”"));
        assert_eq!(find(&p, &fold_query("\"мир\"")).len(), 1);
        assert_eq!(find(&p, &fold_query("ПРИВЕТ")).len(), 1);
    }

    #[test]
    fn empty_query_matches_nothing() {
        let p = fold_page(&cps("abc"));
        assert!(find(&p, &fold_query("   ")).is_empty());
    }
}
