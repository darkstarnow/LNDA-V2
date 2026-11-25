pub struct RustLexer {
    chars: Vec<char>,
    position: usize,
    line: usize,
}

impl RustLexer {
    pub fn new(source: &str) -> Self {
        Self {
            chars: source.chars().collect(),
            position: 0,
            line: 1,
        }
    }

    pub fn first_comment(mut self) -> Option<usize> {
        while let Some(current) = self.peek(0) {
            match current {
                '/' if matches!(self.peek(1), Some('/' | '*')) => return Some(self.line),
                '"' => self.skip_string(1),
                'r' | 'b' | 'c' if self.at_word_start() && self.raw_string_hashes().is_some() => {
                    self.skip_raw_string()
                }
                'b' | 'c' if self.at_word_start() && self.peek(1) == Some('"') => {
                    self.skip_string(2)
                }
                'b' if self.at_word_start() && self.peek(1) == Some('\'') => self.skip_quote(2),
                '\'' => self.skip_quote(1),
                _ => self.advance(1),
            }
        }
        None
    }

    fn peek(&self, offset: usize) -> Option<char> {
        self.chars.get(self.position + offset).copied()
    }

    fn advance(&mut self, count: usize) {
        for _ in 0..count {
            if self.peek(0) == Some('\n') {
                self.line += 1;
            }
            self.position += 1;
        }
    }

    fn at_word_start(&self) -> bool {
        self.position == 0
            || !self.chars[self.position - 1].is_alphanumeric()
                && self.chars[self.position - 1] != '_'
    }

    fn raw_string_hashes(&self) -> Option<usize> {
        let mut offset = usize::from(matches!(self.peek(0), Some('b' | 'c')));
        if self.peek(offset) != Some('r') {
            return None;
        }
        offset += 1;
        let mut hashes = 0;
        while self.peek(offset + hashes) == Some('#') {
            hashes += 1;
        }
        (self.peek(offset + hashes) == Some('"')).then_some(hashes)
    }

    fn skip_raw_string(&mut self) {
        let hashes = self.raw_string_hashes().unwrap_or(0);
        while self.peek(0) != Some('"') {
            self.advance(1);
        }
        self.advance(1);
        while let Some(current) = self.peek(0) {
            self.advance(1);
            if current == '"' && (0..hashes).all(|index| self.peek(index) == Some('#')) {
                self.advance(hashes);
                return;
            }
        }
    }

    fn skip_string(&mut self, prefix: usize) {
        self.advance(prefix);
        while let Some(current) = self.peek(0) {
            match current {
                '\\' => self.advance(2),
                '"' => {
                    self.advance(1);
                    return;
                }
                _ => self.advance(1),
            }
        }
    }

    fn skip_quote(&mut self, prefix: usize) {
        self.advance(prefix);
        match (self.peek(0), self.peek(1)) {
            (Some('\\'), _) => {
                self.advance(2);
                while self.peek(0).is_some_and(|current| current != '\'') {
                    self.advance(1);
                }
                self.advance(1);
            }
            (Some(_), Some('\'')) => self.advance(2),
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::RustLexer;

    #[test]
    fn finds_line_and_block_comments() {
        assert_eq!(
            RustLexer::new("fn a() {}\n// note\n").first_comment(),
            Some(2)
        );
        assert_eq!(
            RustLexer::new("let x = 1; /* note */").first_comment(),
            Some(1)
        );
    }

    #[test]
    fn ignores_comment_markers_inside_literals() {
        let source =
            "let a = \"// no\";\nlet b = r#\"/* no */\"#;\nlet c = '/';\nfn d<'a>(x: &'a str) {}\n";
        assert_eq!(RustLexer::new(source).first_comment(), None);
    }
}
