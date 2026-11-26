/// Whether `path` matches `/`-separated `glob`. `**`(optionally `**/`) spans
/// separators; `*`/`?` stay within a segment. Pure — replaces the original's
/// regex-backed `glob_to_regex`.
///
/// Evaluates over Unicode scalar values (`char`) with dynamic programming in
/// `O(|glob| * |path|)` time and `O(|glob| + |path|)` space (glob length + path
/// length), preventing exponential backtracking on adversarial wildcard
/// sequences and matching multi-byte UTF-8 codepoints accurately with `?` and `*`.
pub fn glob_match(glob: &str, path: &str) -> bool {
    #[derive(Clone, Copy)]
    enum Token {
        StarStarSlash,
        StarStar,
        Star,
        Question,
        Literal(char),
    }

    let g_chars: Vec<char> = glob.chars().collect();
    let p_chars: Vec<char> = path.chars().collect();
    let mut tokens: Vec<Token> = Vec::with_capacity(g_chars.len());
    let mut i = 0;
    while i < g_chars.len() {
        if g_chars[i] == '*' && g_chars.get(i + 1) == Some(&'*') {
            if g_chars.get(i + 2) == Some(&'/') {
                tokens.push(Token::StarStarSlash);
                i += 3;
            } else {
                tokens.push(Token::StarStar);
                i += 2;
            }
        } else if g_chars[i] == '*' {
            tokens.push(Token::Star);
            i += 1;
        } else if g_chars[i] == '?' {
            tokens.push(Token::Question);
            i += 1;
        } else {
            tokens.push(Token::Literal(g_chars[i]));
            i += 1;
        }
    }

    let n = p_chars.len();
    let mut dp = vec![false; n + 1];
    dp[n] = true;

    for token in tokens.into_iter().rev() {
        let mut next = vec![false; n + 1];
        match token {
            Token::Literal(c) => {
                for j in 0..n {
                    next[j] = p_chars[j] == c && dp[j + 1];
                }
            }
            Token::Question => {
                for j in 0..n {
                    next[j] = p_chars[j] != '/' && dp[j + 1];
                }
            }
            Token::Star => {
                next[n] = dp[n];
                for j in (0..n).rev() {
                    next[j] = dp[j] || (p_chars[j] != '/' && next[j + 1]);
                }
            }
            Token::StarStar => {
                let any_true = dp.iter().any(|&v| v);
                if any_true {
                    let mut suffix_or = false;
                    for j in (0..=n).rev() {
                        suffix_or = suffix_or || dp[j];
                        next[j] = suffix_or;
                    }
                }
            }
            Token::StarStarSlash => {
                let mut slash_match_after = false;
                next[n] = dp[n];
                for j in (0..n).rev() {
                    if p_chars[j] == '/' && dp[j + 1] {
                        slash_match_after = true;
                    }
                    next[j] = dp[j] || slash_match_after;
                }
            }
        }
        dp = next;
    }

    dp[0]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_covers_segments_and_wildcards() {
        assert!(glob_match("**/*.rs", "src/relay/mod.rs"));
        assert!(glob_match("**/*.rs", "lib.rs"));
        assert!(!glob_match("*.rs", "src/lib.rs"));
        assert!(glob_match("src/?.rs", "src/a.rs"));
        assert!(glob_match("src/**", "src/a/b/c.txt"));
        assert!(glob_match("src/**/*.rs", "src/lib.rs"), "**/ matches zero dirs");
        assert!(glob_match("src/**/*.rs", "src/a/b/mod.rs"), "**/ matches nested dirs");
        assert!(!glob_match("src/**/*.rs", "src/lib.txt"), "extension must match");
        assert!(!glob_match("src/**/*.rs", "tests/lib.rs"), "prefix must match");
        assert!(!glob_match("src/?.rs", "src/ab.rs"));
    }

    #[test]
    fn glob_matches_multibyte_utf8_characters_and_linear_wildcards() {
        assert!(glob_match("caf?.txt", "café.txt"));
        assert!(glob_match("src/?/file.rs", "src/λ/file.rs"));
        assert!(!glob_match("caf?.txt", "caf/é.txt"));
        assert!(!glob_match("***/a", "a"));
        assert!(glob_match("***/a", "x/a"));

        let path = "a/".repeat(40) + "file.txt";
        assert!(!glob_match("**/**/**/**/**/**/**/*.rs", &path));
        assert!(!glob_match("*a*a*a*a*a*a*a*a*a*b", &"a".repeat(60)));
    }
}
