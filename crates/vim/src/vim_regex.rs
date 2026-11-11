//! Translates Vim regular expressions and `:substitute` replacement strings
//! into the syntax used by Zed's search, which is `fancy_regex` for patterns
//! and `$n` capture references for replacements.

use std::{iter::Peekable, str::Chars};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Magic {
    /// `\v`: every ASCII punctuation character except `_` is special.
    VeryMagic,
    /// `\m`: Vim's default.
    Magic,
    /// `\M`: only `^` and `$` are special.
    NoMagic,
    /// `\V`: only `\` is special.
    VeryNoMagic,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VimPattern {
    pub pattern: String,
    /// Set by `\c` (false) or `\C` (true) in the pattern.
    pub case_sensitive: Option<bool>,
}

/// Characters that need a backslash to be literal in a `fancy_regex` pattern.
fn push_literal(output: &mut String, character: char) {
    if matches!(
        character,
        '\\' | '.' | '+' | '*' | '?' | '(' | ')' | '|' | '[' | ']' | '{' | '}' | '^' | '$' | '#'
    ) {
        output.push('\\');
    }
    output.push(character);
}

fn class_for_letter(letter: char) -> Option<&'static str> {
    Some(match letter {
        's' => r"[ \t]",
        'S' => r"[^ \t\n]",
        'd' => "[0-9]",
        'D' => r"[^0-9\n]",
        'w' => "[0-9A-Za-z_]",
        'W' => r"[^0-9A-Za-z_\n]",
        'a' => "[A-Za-z]",
        'A' => r"[^A-Za-z\n]",
        'l' => "[a-z]",
        'L' => r"[^a-z\n]",
        'u' => "[A-Z]",
        'U' => r"[^A-Z\n]",
        'x' => "[0-9A-Fa-f]",
        'X' => r"[^0-9A-Fa-f\n]",
        'o' => "[0-7]",
        'O' => r"[^0-7\n]",
        'h' => "[A-Za-z_]",
        'H' => r"[^A-Za-z_\n]",
        // Vim's defaults for 'isident' and 'iskeyword' are close to `\w`,
        // including non-ASCII letters for 'iskeyword'.
        'i' | 'k' => r"[\w]",
        'I' | 'K' => r"[^\W\d]",
        'f' => r"[\w./~+,#$%-]",
        'F' => r"[^\W\d]",
        'p' => r"[^\x00-\x1f\x7f]",
        'P' => r"[^\x00-\x1f\x7f0-9]",
        _ => return None,
    })
}

struct Translator<'a> {
    chars: Peekable<Chars<'a>>,
    magic: Magic,
    output: String,
    case_sensitive: Option<bool>,
    /// Where each open group starts in `output`, to find the atom a closing
    /// group forms.
    group_starts: Vec<usize>,
    /// Start in `output` of the last complete atom, which quantifiers and
    /// `\@` assertions apply to.
    last_atom_start: Option<usize>,
    /// Whether the next token starts a branch, where `^` is an anchor and
    /// a quantifier is literal.
    at_branch_start: bool,
    match_start: Option<usize>,
    match_end: Option<usize>,
    has_top_level_alternation: bool,
}

/// A token after deciding whether Vim treats it as special.
enum Token {
    Special(char),
    Literal(char),
    /// A backslash followed by a letter or digit, which means the same
    /// thing whatever the magic level.
    Escape(char),
}

impl<'a> Translator<'a> {
    fn new(pattern: &'a str) -> Self {
        Self {
            chars: pattern.chars().peekable(),
            magic: Magic::Magic,
            output: String::new(),
            case_sensitive: None,
            group_starts: Vec::new(),
            last_atom_start: None,
            at_branch_start: true,
            match_start: None,
            match_end: None,
            has_top_level_alternation: false,
        }
    }

    fn is_special_unescaped(&self, character: char) -> bool {
        match self.magic {
            Magic::VeryMagic => {
                character.is_ascii_punctuation() && character != '_' && character != '\\'
            }
            Magic::Magic => matches!(character, '^' | '$' | '.' | '*' | '[' | '~'),
            Magic::NoMagic => matches!(character, '^' | '$'),
            Magic::VeryNoMagic => false,
        }
    }

    fn next_token(&mut self) -> Option<Token> {
        let character = self.chars.next()?;
        if character != '\\' {
            return Some(if self.is_special_unescaped(character) {
                Token::Special(character)
            } else {
                Token::Literal(character)
            });
        }
        let Some(escaped) = self.chars.next() else {
            // A trailing backslash is literal in Vim.
            return Some(Token::Literal('\\'));
        };
        if escaped.is_ascii_alphanumeric() || escaped == '_' {
            return Some(Token::Escape(escaped));
        }
        let special_when_escaped = match self.magic {
            Magic::VeryMagic => false,
            Magic::Magic => matches!(
                escaped,
                '(' | ')' | '|' | '+' | '?' | '=' | '{' | '@' | '%' | '<' | '>' | '&'
            ),
            Magic::NoMagic => matches!(
                escaped,
                '(' | ')'
                    | '|'
                    | '+'
                    | '?'
                    | '='
                    | '{'
                    | '@'
                    | '%'
                    | '<'
                    | '>'
                    | '&'
                    | '.'
                    | '*'
                    | '['
                    | '~'
            ),
            Magic::VeryNoMagic => matches!(
                escaped,
                '(' | ')'
                    | '|'
                    | '+'
                    | '?'
                    | '='
                    | '{'
                    | '@'
                    | '%'
                    | '<'
                    | '>'
                    | '&'
                    | '.'
                    | '*'
                    | '['
                    | '~'
                    | '^'
                    | '$'
            ),
        };
        Some(if special_when_escaped {
            Token::Special(escaped)
        } else {
            Token::Literal(escaped)
        })
    }

    /// Whether the rest of the pattern starts with something that ends a
    /// branch, which is where `$` is an anchor.
    fn at_branch_end(&self) -> bool {
        let mut rest = self.chars.clone();
        match rest.next() {
            None => true,
            Some('\\') => match rest.next() {
                Some('|') | Some(')') => self.magic != Magic::VeryMagic,
                Some('n') => true,
                _ => false,
            },
            Some('|') | Some(')') => self.magic == Magic::VeryMagic,
            _ => false,
        }
    }

    fn begin_atom(&mut self) -> usize {
        let start = self.output.len();
        self.last_atom_start = Some(start);
        self.at_branch_start = false;
        start
    }

    fn push_atom(&mut self, text: &str) {
        self.begin_atom();
        self.output.push_str(text);
    }

    fn push_literal_atom(&mut self, character: char) {
        self.begin_atom();
        push_literal(&mut self.output, character);
    }

    fn push_quantifier(&mut self, quantifier: &str, literal: char) {
        if self.last_atom_start.is_none() || self.at_branch_start {
            // Vim treats a quantifier with nothing before it as literal.
            self.push_literal_atom(literal);
            return;
        }
        self.output.push_str(quantifier);
        // Vim doesn't allow stacking quantifiers, and a second one would be
        // an error in the target syntax too.
        self.last_atom_start = None;
    }

    fn translate(mut self) -> Result<VimPattern, String> {
        while let Some(token) = self.next_token() {
            match token {
                Token::Literal(character) => self.push_literal_atom(character),
                Token::Escape(character) => self.translate_escape(character)?,
                Token::Special(character) => self.translate_special(character)?,
            }
        }
        if !self.group_starts.is_empty() {
            return Err("unmatched \\(".to_string());
        }
        let pattern = self.apply_match_bounds()?;
        Ok(VimPattern {
            pattern,
            case_sensitive: self.case_sensitive,
        })
    }

    fn apply_match_bounds(&mut self) -> Result<String, String> {
        if self.match_start.is_none() && self.match_end.is_none() {
            return Ok(std::mem::take(&mut self.output));
        }
        if self.has_top_level_alternation {
            return Err("\\zs and \\ze can't be used with a top-level \\|".to_string());
        }
        let length = self.output.len();
        let start = self.match_start.unwrap_or(0);
        let end = self.match_end.unwrap_or(length).max(start);
        let mut pattern = String::new();
        if let Some(before) = self.output.get(..start)
            && !before.is_empty()
        {
            pattern.push_str("(?<=");
            pattern.push_str(before);
            pattern.push(')');
        }
        pattern.push_str(self.output.get(start..end).unwrap_or_default());
        if let Some(after) = self.output.get(end..)
            && !after.is_empty()
        {
            pattern.push_str("(?=");
            pattern.push_str(after);
            pattern.push(')');
        }
        Ok(pattern)
    }

    fn translate_escape(&mut self, character: char) -> Result<(), String> {
        match character {
            'v' => self.magic = Magic::VeryMagic,
            'm' => self.magic = Magic::Magic,
            'M' => self.magic = Magic::NoMagic,
            'V' => self.magic = Magic::VeryNoMagic,
            'c' => self.case_sensitive = Some(false),
            'C' => self.case_sensitive = Some(true),
            'n' => self.push_atom(r"\n"),
            't' => self.push_atom(r"\t"),
            'e' => self.push_atom(r"\x1b"),
            'r' => self.push_atom(r"\r"),
            'b' => self.push_atom(r"\x08"),
            '1'..='9' => {
                self.push_atom("\\");
                self.output.push(character);
            }
            'z' => match self.chars.next() {
                Some('s') => {
                    if !self.group_starts.is_empty() {
                        return Err("\\zs inside a group isn't supported".to_string());
                    }
                    self.match_start = Some(self.output.len());
                }
                Some('e') => {
                    if !self.group_starts.is_empty() {
                        return Err("\\ze inside a group isn't supported".to_string());
                    }
                    self.match_end = Some(self.output.len());
                }
                other => {
                    return Err(format!(
                        "\\z{} isn't supported",
                        other.map(String::from).unwrap_or_default()
                    ));
                }
            },
            '_' => self.translate_newline_class()?,
            _ => {
                let Some(class) = class_for_letter(character) else {
                    return Err(format!("\\{character} isn't supported"));
                };
                self.push_atom(class);
            }
        }
        Ok(())
    }

    /// `\_x`: the class `x` plus end-of-line.
    fn translate_newline_class(&mut self) -> Result<(), String> {
        match self.chars.next() {
            Some('.') => self.push_atom(r"(?s:.)"),
            Some('^') => self.output.push('^'),
            Some('$') => self.output.push('$'),
            Some('[') => {
                let class = self.translate_class()?;
                let Some(class) = class else {
                    return Err("unmatched \\_[".to_string());
                };
                self.push_atom(&format!("(?:\\n|{class})"));
            }
            Some(letter) => {
                let Some(class) = class_for_letter(letter) else {
                    return Err(format!("\\_{letter} isn't supported"));
                };
                self.push_atom(&format!("(?:\\n|{class})"));
            }
            None => return Err("\\_ needs a character class".to_string()),
        }
        Ok(())
    }

    fn translate_special(&mut self, character: char) -> Result<(), String> {
        match character {
            '^' => {
                if self.at_branch_start {
                    self.output.push('^');
                } else {
                    self.push_literal_atom('^');
                }
            }
            '$' => {
                if self.at_branch_end() {
                    self.output.push('$');
                } else {
                    self.push_literal_atom('$');
                }
            }
            '.' => self.push_atom("."),
            '~' => self.push_literal_atom('~'),
            '*' => self.push_quantifier("*", '*'),
            '+' => self.push_quantifier("+", '+'),
            '?' | '=' => self.push_quantifier("?", character),
            '{' => self.translate_brace()?,
            '(' => {
                self.group_starts.push(self.output.len());
                self.output.push('(');
                self.at_branch_start = true;
                self.last_atom_start = None;
            }
            ')' => {
                let Some(start) = self.group_starts.pop() else {
                    return Err("unmatched \\)".to_string());
                };
                self.output.push(')');
                self.last_atom_start = Some(start);
                self.at_branch_start = false;
            }
            '|' => {
                if self.group_starts.is_empty() {
                    self.has_top_level_alternation = true;
                }
                self.output.push('|');
                self.at_branch_start = true;
                self.last_atom_start = None;
            }
            '<' => {
                self.output.push_str(r"\b(?=\w)");
            }
            '>' => {
                self.output.push_str(r"\b(?<=\w)");
            }
            '%' => self.translate_percent()?,
            '@' => self.translate_assertion()?,
            '[' => match self.translate_class()? {
                Some(class) => self.push_atom(&class),
                None => self.push_literal_atom('['),
            },
            '&' => return Err("\\& (branch concatenation) isn't supported".to_string()),
            _ => self.push_literal_atom(character),
        }
        Ok(())
    }

    /// `\{n,m}` and friends; the closing brace may be escaped.
    fn translate_brace(&mut self) -> Result<(), String> {
        let mut contents = String::new();
        loop {
            match self.chars.next() {
                Some('}') => break,
                Some('\\') if self.chars.peek() == Some(&'}') => {
                    self.chars.next();
                    break;
                }
                Some(character) if character.is_ascii_digit() || matches!(character, ',' | '-') => {
                    contents.push(character)
                }
                _ => return Err("invalid \\{ quantifier".to_string()),
            }
        }
        let (lazy, bounds) = match contents.strip_prefix('-') {
            Some(rest) => (true, rest),
            None => (false, contents.as_str()),
        };
        let quantifier = match bounds.split_once(',') {
            None if bounds.is_empty() => "*".to_string(),
            None => format!("{{{bounds}}}"),
            Some(("", "")) => "*".to_string(),
            Some(("", max)) => format!("{{0,{max}}}"),
            Some((min, max)) => format!("{{{min},{max}}}"),
        };
        if lazy {
            self.push_quantifier(&format!("{quantifier}?"), '{');
        } else {
            self.push_quantifier(&quantifier, '{');
        }
        Ok(())
    }

    fn translate_percent(&mut self) -> Result<(), String> {
        match self.chars.next() {
            Some('(') => {
                self.group_starts.push(self.output.len());
                self.output.push_str("(?:");
                self.at_branch_start = true;
                self.last_atom_start = None;
            }
            Some('^') => self.output.push_str(r"\A"),
            Some('$') => self.output.push_str(r"\z"),
            Some(radix @ ('d' | 'x' | 'u' | 'U' | 'o')) => {
                let (base, max_digits) = match radix {
                    'd' => (10, 10),
                    'o' => (8, 11),
                    'x' => (16, 2),
                    'u' => (16, 4),
                    _ => (16, 8),
                };
                let mut digits = String::new();
                while digits.len() < max_digits
                    && let Some(digit) = self.chars.peek().copied()
                    && digit.is_digit(base)
                {
                    digits.push(digit);
                    self.chars.next();
                }
                let character = u32::from_str_radix(&digits, base)
                    .ok()
                    .and_then(char::from_u32)
                    .ok_or_else(|| format!("invalid character code \\%{radix}{digits}"))?;
                self.push_literal_atom(character);
            }
            other => {
                return Err(format!(
                    "\\%{} isn't supported",
                    other.map(String::from).unwrap_or_default()
                ));
            }
        }
        Ok(())
    }

    /// `\@=`, `\@!`, `\@<=`, `\@<!` and `\@>` wrap the preceding atom.
    fn translate_assertion(&mut self) -> Result<(), String> {
        let prefix = match self.chars.next() {
            Some('=') => "(?=",
            Some('!') => "(?!",
            Some('>') => "(?>",
            Some('<') => match self.chars.next() {
                Some('=') => "(?<=",
                Some('!') => "(?<!",
                _ => return Err("invalid \\@< assertion".to_string()),
            },
            _ => return Err("invalid \\@ assertion".to_string()),
        };
        let Some(start) = self.last_atom_start else {
            return Err("\\@ needs something before it".to_string());
        };
        let atom = self.output.split_off(start);
        self.output.push_str(prefix);
        self.output.push_str(&atom);
        self.output.push(')');
        self.last_atom_start = Some(start);
        Ok(())
    }

    /// Translates a `[...]` collection whose `[` was already consumed.
    /// Returns `None`, consuming nothing, when there is no closing `]`, in
    /// which case Vim treats the `[` literally.
    fn translate_class(&mut self) -> Result<Option<String>, String> {
        let mut lookahead = self.chars.clone();
        let mut class = String::from("[");
        let mut first = true;
        if lookahead.peek() == Some(&'^') {
            lookahead.next();
            class.push('^');
        }
        loop {
            let Some(character) = lookahead.next() else {
                return Ok(None);
            };
            match character {
                ']' if !first => break,
                '[' if lookahead.peek() == Some(&':') => {
                    let mut posix = String::from("[");
                    let mut closed = false;
                    for next in lookahead.by_ref() {
                        posix.push(next);
                        if posix.len() > 2 && posix.ends_with(":]") {
                            closed = true;
                            break;
                        }
                    }
                    if !closed {
                        return Ok(None);
                    }
                    class.push_str(&posix);
                }
                '\\' => match lookahead.next() {
                    Some('e') => class.push_str(r"\x1b"),
                    Some('t') => class.push_str(r"\t"),
                    Some('r') => class.push_str(r"\r"),
                    Some('n') => class.push_str(r"\n"),
                    Some('b') => class.push_str(r"\x08"),
                    Some('d') => {
                        let mut digits = String::new();
                        while let Some(digit) = lookahead.peek().copied()
                            && digit.is_ascii_digit()
                        {
                            digits.push(digit);
                            lookahead.next();
                        }
                        let code = digits
                            .parse::<u32>()
                            .ok()
                            .and_then(char::from_u32)
                            .ok_or_else(|| "invalid \\d in a collection".to_string())?;
                        class.push_str(&format!(r"\x{{{:x}}}", code as u32));
                    }
                    Some(escaped @ (']' | '^' | '-' | '\\')) => {
                        class.push('\\');
                        class.push(escaped);
                    }
                    Some(other) => {
                        // Vim keeps the backslash of an unknown escape.
                        class.push_str(r"\\");
                        push_class_literal(&mut class, other);
                    }
                    None => return Ok(None),
                },
                '-' => class.push('-'),
                _ => push_class_literal(&mut class, character),
            }
            first = false;
        }
        class.push(']');
        self.chars = lookahead;
        Ok(Some(class))
    }
}

fn push_class_literal(class: &mut String, character: char) {
    if matches!(character, '[' | ']' | '\\' | '&' | '~' | '^') {
        class.push('\\');
    }
    class.push(character);
}

/// Translates a Vim pattern. Patterns are "magic" by default, as in Vim, and
/// `\v`, `\m`, `\M` and `\V` change that anywhere in the pattern.
pub fn translate_pattern(pattern: &str) -> Result<VimPattern, String> {
    Translator::new(pattern).translate()
}

/// Translates the replacement part of `:substitute` into the syntax Zed's
/// search replaces with: `${n}` for groups, `$$` for `$`, and `\\`, `\n`
/// and `\t` escapes.
pub fn translate_replacement(replacement: &str) -> Result<String, String> {
    let mut output = String::new();
    let mut chars = replacement.chars();
    while let Some(character) = chars.next() {
        match character {
            '&' => output.push_str("${0}"),
            '$' => output.push_str("$$"),
            '\\' => match chars.next() {
                Some(digit @ '0'..='9') => {
                    output.push_str("${");
                    output.push(digit);
                    output.push('}');
                }
                // Vim inserts a line break for `\r`, and `\n` is a NUL in
                // the buffer, which is never what's wanted here.
                Some('r') | Some('n') => output.push_str(r"\n"),
                Some('t') => output.push_str(r"\t"),
                Some('\\') => output.push_str(r"\\"),
                Some('&') => output.push('&'),
                Some(case @ ('u' | 'U' | 'l' | 'L' | 'e' | 'E')) => {
                    return Err(format!("\\{case} in a replacement isn't supported"));
                }
                Some('=') => {
                    return Err("\\= expressions in a replacement aren't supported".to_string());
                }
                Some('$') => output.push_str("$$"),
                Some(other) => output.push(other),
                None => output.push_str(r"\\"),
            },
            _ => output.push(character),
        }
    }
    Ok(output)
}

/// Splits the text after `:s` into the raw pattern, the raw replacement and
/// the flags. The delimiter may be escaped to use it literally; all other
/// escapes are kept for [`translate_pattern`] and [`translate_replacement`].
pub fn split_substitute(chars: &mut Peekable<Chars>) -> Option<(String, String, String)> {
    let delimiter = chars
        .next()
        .filter(|c| !c.is_alphanumeric() && *c != '"' && *c != '|' && *c != '\'' && *c != '\\')?;
    let mut parts = [String::new(), String::new(), String::new()];
    let mut phase = 0;
    while let Some(character) = chars.next() {
        if phase < 2 && character == '\\' {
            match chars.next() {
                Some(next) if next == delimiter => parts[phase].push(next),
                Some(next) => {
                    parts[phase].push('\\');
                    parts[phase].push(next);
                }
                None => parts[phase].push('\\'),
            }
        } else if phase < 2 && character == delimiter {
            phase += 1;
        } else if let Some(part) = parts.get_mut(phase) {
            part.push(character);
        }
    }
    let [pattern, replacement, flags] = parts;
    Some((pattern, replacement, flags))
}

#[cfg(test)]
mod test {
    use super::*;

    fn translated(pattern: &str) -> String {
        translate_pattern(pattern)
            .expect("pattern should translate")
            .pattern
    }

    fn replace_all(pattern: &str, replacement: &str, text: &str) -> String {
        let pattern = translate_pattern(pattern).expect("pattern should translate");
        let regex = fancy_regex::RegexBuilder::new(&pattern.pattern)
            .case_insensitive(pattern.case_sensitive == Some(false))
            .multi_line(true)
            .build()
            .expect("translated pattern should compile");
        let replacement = translate_replacement(replacement)
            .expect("replacement should translate")
            .replace(r"\n", "\n")
            .replace(r"\t", "\t")
            .replace(r"\\", "\\");
        regex.replace_all(text, replacement.as_str()).into_owned()
    }

    #[test]
    fn test_magic_patterns() {
        assert_eq!(translated(r"a\+b"), "a+b");
        assert_eq!(translated(r"a+b"), r"a\+b");
        assert_eq!(translated(r"\(ab\)\{2}"), "(ab){2}");
        assert_eq!(translated(r"(x)"), r"\(x\)");
        assert_eq!(translated(r"a\|b"), "a|b");
        assert_eq!(translated(r"x\{-1,}"), "x{1,}?");
        assert_eq!(translated(r"*a"), r"\*a");
        assert_eq!(translated(r"a^b$"), r"a\^b$");
        assert_eq!(translated(r"a$b"), r"a\$b");
        assert_eq!(translated(r"foo\="), "foo?");
        assert_eq!(translated(r"\%(a\)"), "(?:a)");
    }

    #[test]
    fn test_very_magic_patterns() {
        assert_eq!(translated(r"\v(a|b)+"), "(a|b)+");
        assert_eq!(translated(r"\v\(a\)"), r"\(a\)");
        assert_eq!(translated(r"\v\w{2,3}"), "[0-9A-Za-z_]{2,3}");
        assert_eq!(translated(r"\v<foo>"), r"\b(?=\w)foo\b(?<=\w)");
        assert_eq!(translated(r"\va{-}"), "a*?");
        assert_eq!(translated(r"\va{,3}"), "a{0,3}");
        assert_eq!(translated(r"\v%(a)"), "(?:a)");
        assert_eq!(translated(r"\vfoo(bar)@="), "foo(?=(bar))");
        assert_eq!(translated(r"\v(foo)@<!bar"), "(?<!(foo))bar");
    }

    #[test]
    fn test_no_magic_patterns() {
        assert_eq!(translated(r"\Va.b*"), r"a\.b\*");
        assert_eq!(translated(r"\Va\.b"), "a.b");
        assert_eq!(translated(r"\Ma.b\*"), r"a\.b*");
    }

    #[test]
    fn test_case_and_classes() {
        let pattern = translate_pattern(r"\cFoo").expect("should translate");
        assert_eq!(pattern.case_sensitive, Some(false));
        assert_eq!(pattern.pattern, "Foo");
        assert_eq!(translated(r"[a-z]"), "[a-z]");
        assert_eq!(translated(r"[]x]"), r"[\]x]");
        assert_eq!(translated(r"[[:alpha:]_]"), "[[:alpha:]_]");
        assert_eq!(translated(r"a[b"), r"a\[b");
    }

    #[test]
    fn test_match_bounds() {
        assert_eq!(translated(r"foo\zsbar"), "(?<=foo)bar");
        assert_eq!(translated(r"foo\zebar"), "foo(?=bar)");
        assert!(translate_pattern(r"a\zsb\|c").is_err());
    }

    #[test]
    fn test_replacements() {
        assert_eq!(
            translate_replacement(r"<&> \1 \\ $x \&"),
            Ok(r"<${0}> ${1} \\ $$x &".to_string())
        );
        assert!(translate_replacement(r"\u\1").is_err());
    }

    #[test]
    fn test_substitutions_behave_like_vim() {
        assert_eq!(
            replace_all(r"\v(\w+) (\w+)", r"\2 \1", "hello world"),
            "world hello"
        );
        assert_eq!(replace_all(r"\v\d+", "<&>", "a1 b22"), "a<1> b<22>");
        assert_eq!(replace_all(r"\<the\>", "a", "the other the"), "a other a");
        assert_eq!(replace_all(r"a\{2,}", "x", "a aa aaa"), "a x x");
        assert_eq!(replace_all(r"\v^\s+", "", "  x\n\ty"), "x\ny");
        assert_eq!(replace_all(r"\v,\s*", r"\r", "a, b,c"), "a\nb\nc");
        assert_eq!(replace_all(r"\Va.c", "x", "abc a.c"), "abc x");
        assert_eq!(replace_all(r"foo\zsbar", "BAZ", "foobar bar"), "fooBAZ bar");
        assert_eq!(replace_all(r"\v(a)\1", "b", "aa a"), "b a");
        assert_eq!(replace_all(r"\cHELLO", "hi", "hello Hello"), "hi hi");
    }

    #[test]
    fn test_split_substitute() {
        let mut chars = r"/a\/b/c\/d/g".chars().peekable();
        assert_eq!(
            split_substitute(&mut chars),
            Some(("a/b".to_string(), "c/d".to_string(), "g".to_string()))
        );
        let mut chars = r"#\v(a)#\1#".chars().peekable();
        assert_eq!(
            split_substitute(&mut chars),
            Some((r"\v(a)".to_string(), r"\1".to_string(), String::new()))
        );
    }
}
