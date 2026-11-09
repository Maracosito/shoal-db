//! Text tokenization for full-text indexing and querying.
//!
//! The tokenizer is intentionally simple and deterministic so that the same
//! configuration produces identical token streams at index time and query
//! time:
//!
//! * Splits on any non-alphanumeric character (Unicode-aware via
//!   [`char::is_alphanumeric`]). `"don't"` therefore tokenizes to
//!   `["don", "t"]`.
//! * Optional lowercasing (Unicode-aware, on by default).
//! * Optional diacritic folding for common Latin characters
//!   (`café` → `cafe`). Folding is applied **after** lowercasing, so the
//!   fold table only needs lowercase entries; if you disable lowercasing,
//!   uppercase accented characters pass through unfolded.
//! * Optional stopword removal and min/max token length filtering. Filtered
//!   tokens still advance the position counter, so positional gaps are
//!   preserved.

use std::collections::HashSet;

/// A single token produced by the tokenizer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    /// The normalized token text.
    pub text: String,
    /// Zero-based position of the token within the input field. Positions
    /// advance for every raw token, including ones removed by stopword or
    /// length filtering.
    pub position: u32,
}

/// Configuration for [`Tokenizer`].
#[derive(Debug, Clone)]
pub struct TokenizerConfig {
    /// Lowercase tokens (Unicode-aware). Default: `true`.
    pub lowercase: bool,
    /// Fold common Latin diacritics to ASCII (`é` → `e`, `ß` → `ss`).
    /// Default: `true`.
    pub fold_diacritics: bool,
    /// Tokens shorter than this many characters are dropped. Default: `1`.
    pub min_token_len: usize,
    /// Tokens longer than this many characters are dropped (not truncated).
    /// Default: `64`.
    pub max_token_len: usize,
    /// Stopwords to drop. Compared **after** normalization, so entries should
    /// be lowercase when `lowercase` is enabled. Default: empty.
    pub stopwords: HashSet<String>,
}

impl Default for TokenizerConfig {
    fn default() -> Self {
        TokenizerConfig {
            lowercase: true,
            fold_diacritics: true,
            min_token_len: 1,
            max_token_len: 64,
            stopwords: HashSet::new(),
        }
    }
}

/// A deterministic, configuration-driven tokenizer.
#[derive(Debug, Clone)]
pub struct Tokenizer {
    config: TokenizerConfig,
}

impl Default for Tokenizer {
    fn default() -> Self {
        Tokenizer::new(TokenizerConfig::default())
    }
}

impl Tokenizer {
    /// Create a tokenizer with the given configuration.
    pub fn new(config: TokenizerConfig) -> Self {
        Tokenizer { config }
    }

    /// Access the active configuration.
    pub fn config(&self) -> &TokenizerConfig {
        &self.config
    }

    /// Tokenize `text` into a vector of normalized tokens with positions.
    pub fn tokenize(&self, text: &str) -> Vec<Token> {
        let mut out = Vec::new();
        let mut current = String::new();
        let mut position: u32 = 0;
        for ch in text.chars() {
            if ch.is_alphanumeric() {
                if self.config.lowercase {
                    for lc in ch.to_lowercase() {
                        self.push_normalized(lc, &mut current);
                    }
                } else {
                    self.push_normalized(ch, &mut current);
                }
            } else if !current.is_empty() {
                self.flush(&mut current, &mut position, &mut out);
            }
        }
        if !current.is_empty() {
            self.flush(&mut current, &mut position, &mut out);
        }
        out
    }

    fn push_normalized(&self, c: char, buf: &mut String) {
        if self.config.fold_diacritics {
            fold_into(c, buf);
        } else {
            buf.push(c);
        }
    }

    fn flush(&self, current: &mut String, position: &mut u32, out: &mut Vec<Token>) {
        let token = std::mem::take(current);
        let pos = *position;
        // Positions always advance, even for filtered tokens.
        *position += 1;
        let char_len = token.chars().count();
        if char_len < self.config.min_token_len || char_len > self.config.max_token_len {
            return;
        }
        if self.config.stopwords.contains(&token) {
            return;
        }
        out.push(Token {
            text: token,
            position: pos,
        });
    }
}

/// Fold a single (already lowercased) character into `buf`, mapping common
/// Latin diacritics to their ASCII base form. Characters without an entry are
/// pushed unchanged.
fn fold_into(c: char, buf: &mut String) {
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => buf.push('a'),
        'ç' | 'ć' | 'č' => buf.push('c'),
        'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ė' | 'ę' => buf.push('e'),
        'ì' | 'í' | 'î' | 'ï' | 'ī' | 'į' => buf.push('i'),
        'ñ' | 'ń' => buf.push('n'),
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' => buf.push('o'),
        'ù' | 'ú' | 'û' | 'ü' | 'ū' => buf.push('u'),
        'ý' | 'ÿ' => buf.push('y'),
        'š' | 'ś' => buf.push('s'),
        'ž' | 'ź' | 'ż' => buf.push('z'),
        'đ' => buf.push('d'),
        'ł' => buf.push('l'),
        'ß' => buf.push_str("ss"),
        'æ' => buf.push_str("ae"),
        'œ' => buf.push_str("oe"),
        _ => buf.push(c),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(tokens: &[Token]) -> Vec<&str> {
        tokens.iter().map(|t| t.text.as_str()).collect()
    }

    #[test]
    fn basic_split_and_lowercase() {
        let t = Tokenizer::default();
        let tokens = t.tokenize("Hello, World! Rust2024");
        assert_eq!(texts(&tokens), vec!["hello", "world", "rust2024"]);
        assert_eq!(
            tokens.iter().map(|t| t.position).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn apostrophes_split() {
        let t = Tokenizer::default();
        assert_eq!(texts(&t.tokenize("don't")), vec!["don", "t"]);
    }

    #[test]
    fn diacritic_folding() {
        let t = Tokenizer::default();
        assert_eq!(
            texts(&t.tokenize("Café Köln œuvre straße")),
            vec!["cafe", "koln", "oeuvre", "strasse"]
        );
    }

    #[test]
    fn folding_can_be_disabled() {
        let t = Tokenizer::new(TokenizerConfig {
            fold_diacritics: false,
            ..TokenizerConfig::default()
        });
        assert_eq!(texts(&t.tokenize("Café")), vec!["café"]);
    }

    #[test]
    fn stopwords_preserve_positions() {
        let mut stop = HashSet::new();
        stop.insert("world".to_string());
        let t = Tokenizer::new(TokenizerConfig {
            stopwords: stop,
            ..TokenizerConfig::default()
        });
        let tokens = t.tokenize("hello world again");
        assert_eq!(texts(&tokens), vec!["hello", "again"]);
        assert_eq!(tokens[0].position, 0);
        assert_eq!(tokens[1].position, 2);
    }

    #[test]
    fn length_filters() {
        let t = Tokenizer::new(TokenizerConfig {
            min_token_len: 2,
            max_token_len: 5,
            ..TokenizerConfig::default()
        });
        let tokens = t.tokenize("a ab abcde abcdef");
        assert_eq!(texts(&tokens), vec!["ab", "abcde"]);
        // Positions still advance for dropped tokens.
        assert_eq!(tokens[0].position, 1);
        assert_eq!(tokens[1].position, 2);
    }

    #[test]
    fn empty_and_punctuation_only() {
        let t = Tokenizer::default();
        assert!(t.tokenize("").is_empty());
        assert!(t.tokenize("... --- !!!").is_empty());
    }
}
