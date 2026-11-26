//! The fuzzy context matcher for `apply_patch`. Ported VERBATIM from Codex
//! `apply-patch/src/seek_sequence.rs`. The fallback ladder is
//! load-bearing and order-sensitive — a later (more permissive) rung must never
//! mask an earlier (stricter) match.
//!
//! Ladder: exact slice-eq → ignore trailing whitespace (`trim_end`) → ignore
//! leading+trailing whitespace (`trim`) → normalize common Unicode punctuation
//! (dashes/quotes/exotic spaces → ASCII) → `None`. Defensive cases: empty
//! `pattern` → `Some(start)`; `pattern.len() > lines.len()` → `None`; `eof=true`
//! anchors the search to the end of the file.

/// Find the sequence of `pattern` lines within `lines` at or after `start`.
/// Returns the start index of the match, or `None`. See module docs for the
/// strictness ladder.
pub fn seek_sequence(
    lines: &[String],
    pattern: &[String],
    start: usize,
    eof: bool,
) -> Option<usize> {
    if start > lines.len() {
        return None;
    }
    if pattern.is_empty() {
        // An empty pattern anchored to the end of the file matches at its end.
        return Some(if eof { lines.len() } else { start });
    }
    // Pattern longer than input can never match (and avoids an OOB slice).
    if pattern.len() > lines.len() {
        return None;
    }
    // An end-of-file anchor never matches before `start`: an earlier chunk
    // already consumed those lines.
    let search_start = if eof {
        let anchored = lines.len() - pattern.len();
        if anchored < start {
            return None;
        }
        anchored
    } else {
        start
    };

    // 1. Exact match.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        if lines[i..i + pattern.len()] == *pattern {
            return Some(i);
        }
    }
    // 2. Ignore trailing whitespace.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim_end() != pat.trim_end() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }
    // 3. Ignore leading + trailing whitespace.
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if lines[i + p_idx].trim() != pat.trim() {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }
    // 4. Normalize common Unicode punctuation to ASCII (mirrors `git apply`).
    for i in search_start..=lines.len().saturating_sub(pattern.len()) {
        let mut ok = true;
        for (p_idx, pat) in pattern.iter().enumerate() {
            if normalise(&lines[i + p_idx]) != normalise(pat) {
                ok = false;
                break;
            }
        }
        if ok {
            return Some(i);
        }
    }
    None
}

fn normalise(s: &str) -> String {
    s.trim()
        .chars()
        .map(|c| match c {
            // Dash / hyphen code-points → ASCII '-'
            '\u{2010}' | '\u{2011}' | '\u{2012}' | '\u{2013}' | '\u{2014}' | '\u{2015}'
            | '\u{2212}' => '-',
            // Fancy single quotes → '\''
            '\u{2018}' | '\u{2019}' | '\u{201A}' | '\u{201B}' => '\'',
            // Fancy double quotes → '"'
            '\u{201C}' | '\u{201D}' | '\u{201E}' | '\u{201F}' => '"',
            // Non-breaking + odd spaces → normal space
            '\u{00A0}' | '\u{2002}' | '\u{2003}' | '\u{2004}' | '\u{2005}' | '\u{2006}'
            | '\u{2007}' | '\u{2008}' | '\u{2009}' | '\u{200A}' | '\u{202F}' | '\u{205F}'
            | '\u{3000}' => ' ',
            other => other,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::seek_sequence;

    fn v(s: &[&str]) -> Vec<String> {
        s.iter().map(|x| x.to_string()).collect()
    }

    #[test]
    fn exact_match() {
        assert_eq!(
            seek_sequence(&v(&["foo", "bar", "baz"]), &v(&["bar", "baz"]), 0, false),
            Some(1)
        );
    }

    #[test]
    fn rstrip_match() {
        assert_eq!(
            seek_sequence(&v(&["foo   ", "bar\t\t"]), &v(&["foo", "bar"]), 0, false),
            Some(0)
        );
    }

    #[test]
    fn trim_both_sides() {
        assert_eq!(
            seek_sequence(
                &v(&["    foo   ", "   bar\t"]),
                &v(&["foo", "bar"]),
                0,
                false
            ),
            Some(0)
        );
    }

    #[test]
    fn unicode_normalize_dashes_and_quotes() {
        // en-dash + curly quote in the file; ASCII in the pattern → still matches.
        let lines = v(&["let x = \u{2018}a\u{2019}; // \u{2014} note"]);
        let pat = v(&["let x = 'a'; // - note"]);
        assert_eq!(seek_sequence(&lines, &pat, 0, false), Some(0));
    }

    #[test]
    fn genuinely_different_does_not_match() {
        assert_eq!(
            seek_sequence(&v(&["foo", "bar"]), &v(&["foo", "qux"]), 0, false),
            None
        );
    }

    #[test]
    fn empty_pattern_matches_at_start() {
        assert_eq!(seek_sequence(&v(&["a", "b"]), &v(&[]), 1, false), Some(1));
    }

    #[test]
    fn eof_never_matches_before_start_and_anchors_an_empty_pattern() {
        // The end-of-file position (index 2) is before `start`.
        assert_eq!(seek_sequence(&v(&["a", "b", "c"]), &v(&["c"]), 3, true), None);
        assert_eq!(seek_sequence(&v(&["a", "b"]), &v(&[]), 0, true), Some(2));
        assert_eq!(seek_sequence(&v(&["a"]), &v(&[]), 2, false), None);
    }

    #[test]
    fn pattern_longer_than_input() {
        assert_eq!(
            seek_sequence(&v(&["one"]), &v(&["a", "b", "c"]), 0, false),
            None
        );
    }

    #[test]
    fn eof_anchors_to_end() {
        // pattern occurs twice; eof=true picks the LAST occurrence.
        let lines = v(&["x", "end", "x", "end"]);
        assert_eq!(seek_sequence(&lines, &v(&["end"]), 0, true), Some(3));
        // without eof, the FIRST forward match from start wins.
        assert_eq!(seek_sequence(&lines, &v(&["end"]), 0, false), Some(1));
    }

    #[test]
    fn exact_beats_later_fuzzy_rung() {
        // An exact match later must not be masked by a fuzzy match earlier:
        // here the only exact match is index 1; a trailing-ws variant is at 0.
        // Exact rung runs first across all indices, so index 1 wins.
        let lines = v(&["foo ", "foo"]);
        assert_eq!(seek_sequence(&lines, &v(&["foo"]), 0, false), Some(1));
    }
}
