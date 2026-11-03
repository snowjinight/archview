// 文件列表高级搜索：语法解析 + 匹配引擎（serach.md 为语法说明文档）。
// Supports: "exact match", !exclude, |OR, *?wildcards, space-separated AND, <group>
//
// Grammar (recursive descent):
//   expr     := or_expr
//   or_expr  := and_expr ( '|' and_expr )*
//   and_expr := term ( term )*
//   term     := '!' (word | '<' or_expr '>')
//             | '<' or_expr '>'
//             | '"' phrase '"'
//             | word
//   word     := any sequence of non-whitespace, non-special chars
//   Special  : space | ! " < >  (use quotes to include them)
//
// 性能：含 * ? 的词在解析期一次性预编译为正则（Pattern 节点），
// 逐条目求值只跑 is_match——万级条目下避免每键每条目重复 Regex::new

use regex::Regex;

/// A parsed search expression node.
#[derive(Debug, Clone)]
pub enum SearchNode {
    /// Must match (plain word, literal substring)
    Include(String),
    /// 预编译通配符模式：* → [^\s]*、? → [^\s]，(?i) 大小写不敏感
    Pattern(Regex),
    /// Exact phrase match (no wildcard expansion)
    Exact(String),
    /// Must NOT match the child expression
    Not(Box<SearchNode>),
    /// AND group — all children must match
    And(Vec<SearchNode>),
    /// OR group — at least one child must match
    Or(Vec<SearchNode>),
}

/// Parsed search query tree.
#[derive(Debug, Clone)]
pub struct SearchQuery {
    pub root: Option<SearchNode>,
}

impl SearchQuery {
    /// Evaluate the query against a haystack string.
    pub fn matches(&self, haystack: &str) -> bool {
        match &self.root {
            Some(node) => eval_node(node, haystack),
            None => true, // empty query matches everything
        }
    }
}

/// Token types for the lexer.
#[derive(Debug, Clone, PartialEq)]
enum Token {
    Word(String),
    Exact(String),
    Not,
    Or,
    And, // implicit (whitespace between terms)
    LParen, // <
    RParen, // >
    End,
}

/// Lexical analyzer: convert query string into tokens.
/// Quoted phrases are extracted first and replaced with placeholders,
/// then the remaining string is tokenized by special characters.
struct Lexer {
    tokens: Vec<Token>,
    pos: usize,
}

impl Lexer {
    fn new(query: &str) -> Self {
        let mut tokens = Vec::new();

        // Step 1: Extract quoted phrases first
        let mut quoted_map: Vec<String> = Vec::new();
        let mut cleaned = String::new();
        let mut chars = query.chars().peekable();
        while let Some(&ch) = chars.peek() {
            if ch == '"' {
                chars.next();
                let mut phrase = String::new();
                while let Some(&c) = chars.peek() {
                    if c == '"' {
                        chars.next();
                        break;
                    }
                    phrase.push(c);
                    chars.next();
                }
                let idx = quoted_map.len();
                quoted_map.push(phrase);
                // Use a placeholder unlikely to appear in normal input
                cleaned.push('\x00');
                cleaned.push_str(&idx.to_string());
                cleaned.push('\x00');
            } else {
                cleaned.push(ch);
                chars.next();
            }
        }

        // Step 2: Tokenize
        let mut current = String::new();
        let mut chars = cleaned.chars().peekable();
        let mut last_was_term = false; // tracks if previous token was a term (for implicit AND)

        while let Some(&ch) = chars.peek() {
            match ch {
                '\x00' => {
                    // Quoted phrase placeholder
                    if !current.is_empty() {
                        tokens.push(Token::Word(current.trim().to_string()));
                        current.clear();
                        if last_was_term {
                            tokens.push(Token::And);
                        }
                        last_was_term = true;
                    } else if last_was_term {
                        tokens.push(Token::And);
                    }
                    chars.next(); // consume \x00
                    let mut idx_str = String::new();
                    while let Some(&c) = chars.peek() {
                        if c == '\x00' {
                            chars.next();
                            break;
                        }
                        idx_str.push(c);
                        chars.next();
                    }
                    if let Ok(idx) = idx_str.parse::<usize>() {
                        if idx < quoted_map.len() {
                            tokens.push(Token::Exact(quoted_map[idx].clone()));
                            last_was_term = true;
                        }
                    }
                }
                '|' => {
                    if !current.is_empty() {
                        tokens.push(Token::Word(current.trim().to_string()));
                        current.clear();
                    }
                    tokens.push(Token::Or);
                    last_was_term = false;
                    chars.next();
                }
                '!' => {
                    if !current.is_empty() {
                        tokens.push(Token::Word(current.trim().to_string()));
                        current.clear();
                        if last_was_term {
                            tokens.push(Token::And);
                        }
                    }
                    tokens.push(Token::Not);
                    last_was_term = false;
                    chars.next();
                }
                '<' => {
                    if !current.is_empty() {
                        tokens.push(Token::Word(current.trim().to_string()));
                        current.clear();
                        if last_was_term {
                            tokens.push(Token::And);
                        }
                    }
                    tokens.push(Token::LParen);
                    last_was_term = false;
                    chars.next();
                }
                '>' => {
                    if !current.is_empty() {
                        tokens.push(Token::Word(current.trim().to_string()));
                        current.clear();
                    }
                    tokens.push(Token::RParen);
                    last_was_term = true;
                    chars.next();
                }
                ' ' | '\t' | '\n' | '\r' => {
                    if !current.is_empty() {
                        tokens.push(Token::Word(current.trim().to_string()));
                        current.clear();
                        if last_was_term {
                            tokens.push(Token::And);
                        }
                        last_was_term = true;
                    }
                    chars.next();
                }
                _ => {
                    current.push(ch);
                    chars.next();
                }
            }
        }

        if !current.trim().is_empty() {
            tokens.push(Token::Word(current.trim().to_string()));
        }

        tokens.push(Token::End);

        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        &self.tokens[self.pos]
    }

    fn next(&mut self) -> Token {
        let t = self.tokens[self.pos].clone();
        if self.pos < self.tokens.len() - 1 {
            self.pos += 1;
        }
        t
    }
}

/// Parser: recursive descent.
struct Parser {
    lex: Lexer,
}

impl Parser {
    fn new(query: &str) -> Self {
        Self { lex: Lexer::new(query) }
    }

    /// entry: parse the full expression
    fn parse(&mut self) -> Option<SearchNode> {
        let node = self.parse_or();
        if matches!(self.lex.peek(), Token::End) {
            node
        } else {
            // Unconsumed tokens — return what we have
            node
        }
    }

    /// or_expr := and_expr ( '|' and_expr )*
    fn parse_or(&mut self) -> Option<SearchNode> {
        let mut children = Vec::new();
        if let Some(first) = self.parse_and() {
            children.push(first);
        }

        while matches!(self.lex.peek(), Token::Or) {
            self.lex.next(); // consume '|'
            if let Some(child) = self.parse_and() {
                children.push(child);
            }
        }

        match children.len() {
            0 => None,
            1 => Some(children.into_iter().next().unwrap()),
            _ => Some(SearchNode::Or(children)),
        }
    }

    /// and_expr := term ( term )*
    /// Terms separated implicitly (no explicit operator) are AND-ed.
    fn parse_and(&mut self) -> Option<SearchNode> {
        let mut children = Vec::new();

        loop {
            // Skip explicit AND tokens
            if matches!(self.lex.peek(), Token::And) {
                self.lex.next();
                continue;
            }
            if let Some(term) = self.parse_term() {
                children.push(term);
            } else {
                break;
            }
        }

        match children.len() {
            0 => None,
            1 => Some(children.into_iter().next().unwrap()),
            _ => Some(SearchNode::And(children)),
        }
    }

    /// term := '!' (word | '<' or_expr '>')
    ///       | '<' or_expr '>'
    ///       | '"' phrase '"'
    ///       | word
    fn parse_term(&mut self) -> Option<SearchNode> {
        match self.lex.peek() {
            Token::Not => {
                self.lex.next(); // consume '!'
                match self.lex.peek() {
                    Token::LParen => {
                        self.lex.next(); // consume '<'
                        let inner = self.parse_or();
                        if matches!(self.lex.peek(), Token::RParen) {
                            self.lex.next(); // consume '>'
                        }
                        inner.map(|n| SearchNode::Not(Box::new(n)))
                    }
                    Token::Word(w) => {
                        let w = w.clone();
                        self.lex.next();
                        Some(SearchNode::Not(Box::new(word_node(w))))
                    }
                    Token::Exact(e) => {
                        let e = e.clone();
                        self.lex.next();
                        Some(SearchNode::Not(Box::new(SearchNode::Exact(e))))
                    }
                    _ => None,
                }
            }
            Token::LParen => {
                self.lex.next(); // consume '<'
                let inner = self.parse_or();
                if matches!(self.lex.peek(), Token::RParen) {
                    self.lex.next(); // consume '>'
                }
                inner
            }
            Token::Word(w) => {
                let w = w.clone();
                self.lex.next();
                Some(word_node(w))
            }
            Token::Exact(e) => {
                let e = e.clone();
                self.lex.next();
                Some(SearchNode::Exact(e))
            }
            _ => None,
        }
    }
}

/// Parse a search query string into a SearchQuery.
pub fn parse_query(query: &str) -> SearchQuery {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return SearchQuery { root: None };
    }
    let mut parser = Parser::new(trimmed);
    SearchQuery { root: parser.parse() }
}

/// Evaluate a search node against a haystack.
fn eval_node(node: &SearchNode, haystack: &str) -> bool {
    match node {
        SearchNode::Include(text) | SearchNode::Exact(text) => token_matches(text, haystack),
        SearchNode::Pattern(re) => re.is_match(haystack),
        SearchNode::Not(inner) => !eval_node(inner, haystack),
        SearchNode::And(children) => children.iter().all(|c| eval_node(c, haystack)),
        SearchNode::Or(children) => children.iter().any(|c| eval_node(c, haystack)),
    }
}

/// 词 → 匹配节点：含 * / ? 时预编译为正则（Pattern），否则字面子串（Include）。
/// 编译失败降级为字面子串（兜底保险，实际不会发生——特殊字符已全部转义）
fn word_node(word: String) -> SearchNode {
    if word.contains('*') || word.contains('?') {
        let escaped = regex_escape(&word);
        let pattern = escaped.replace('*', "[^\\s]*").replace('?', "[^\\s]");
        if let Ok(re) = Regex::new(&format!("(?i){pattern}")) {
            return SearchNode::Pattern(re);
        }
    }
    SearchNode::Include(word)
}

/// Match a plain word or exact phrase: case-insensitive literal substring.
/// （通配符已在解析期转入 Pattern 节点，此处不再处理 * ?）
fn token_matches(text: &str, haystack: &str) -> bool {
    haystack.to_lowercase().contains(&text.to_lowercase())
}

/// Escape regex special characters in a pattern, preserving `*` and `?`.
fn regex_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '*' | '?' => out.push(ch),
            '.' | '+' | '^' | '$' | '(' | ')' | '[' | ']' | '{' | '}' | '\\' | '|' => {
                out.push('\\');
                out.push(ch);
            }
            _ => out.push(ch),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(query: &str, key: &str) -> bool {
        parse_query(query).matches(key)
    }

    #[test]
    fn test_basic_and() {
        assert!(m("aem num", "aem_num_param"));
        assert!(!m("aem num", "aem_only"));
        assert!(!m("aem num", "num_only"));
    }

    #[test]
    fn test_basic_or() {
        assert!(m("aem|tnr", "aem_param"));
        assert!(m("aem|tnr", "tnr_param"));
        assert!(!m("aem|tnr", "other"));
    }

    #[test]
    fn test_not() {
        assert!(m("report !temp", "report_value"));
        assert!(!m("report !temp", "report_temp"));
    }

    #[test]
    fn test_exact() {
        assert!(m("\"aem num\"", "aem num field"));
        assert!(!m("\"aem num\"", "aem_other_num"));
    }

    #[test]
    fn test_wildcard() {
        assert!(m("aem*", "aem_gain"));
        assert!(m("aem*", "aemification"));
        assert!(!m("aem*", "tnr_gain"));
        assert!(m("file?.txt", "file1.txt"));
        assert!(!m("file?.txt", "file12.txt"));
    }

    #[test]
    fn test_or_precedence_default() {
        // a b | c  →  (a AND b) OR c
        assert!(m("a b|c", "a_b_value"));
        assert!(m("a b|c", "c_value"));
        assert!(!m("a b|c", "a_only"));
    }

    #[test]
    fn test_group_changes_precedence() {
        // a <b|c>  →  a AND (b OR c)
        assert!(m("a <b|c>", "a_b_value"));
        assert!(m("a <b|c>", "a_c_value"));
        assert!(!m("a <b|c>", "b_only"));
        assert!(!m("a <b|c>", "c_only"));
        assert!(!m("a <b|c>", "a_only"));
    }

    #[test]
    fn test_group_not() {
        // !<aem|tnr>  →  NOT (aem OR tnr)
        assert!(!m("!<aem|tnr>", "aem_param"));
        assert!(!m("!<aem|tnr>", "tnr_param"));
        assert!(m("!<aem|tnr>", "other_param"));
    }

    #[test]
    fn test_nested_groups() {
        // <a <b|c> | d>  →  (a AND (b OR c)) OR d
        assert!(m("<a <b|c>|d>", "a_b_value"));
        assert!(m("<a <b|c>|d>", "a_c_value"));
        assert!(m("<a <b|c>|d>", "d_value"));
        assert!(!m("<a <b|c>|d>", "a_only"));
        assert!(!m("<a <b|c>|d>", "b_only"));
    }

    #[test]
    fn test_complex() {
        // aem <num | mode> !temp
        assert!(m("aem <num|mode> !temp", "aem_num_value"));
        assert!(m("aem <num|mode> !temp", "aem_mode_value"));
        assert!(!m("aem <num|mode> !temp", "aem_temp_value"));
        assert!(!m("aem <num|mode> !temp", "num_value"));
    }

    #[test]
    fn test_empty_query() {
        assert!(m("", "anything"));
        assert!(m("   ", "anything"));
    }

    #[test]
    fn test_case_insensitive() {
        assert!(m("AEM", "aem_value"));
        assert!(m("aem", "AEM_VALUE"));
    }

    #[test]
    fn test_wildcard_no_cross_field() {
        // `*` matches any non-whitespace sequence. `a*b` requires `a` and `b`
        // to be in the same whitespace-delimited token.
        assert!(!m("a*b", "a x b"));
        assert!(m("a*b", "axb"));
        assert!(m("a*b", "a_value_b"));
    }
}
