//! A minimal S-expression reader for the KiCad file family.
//!
//! KiCad's grammar (REFERENCES.md #59): lists in parentheses, bare atoms
//! (symbols and numbers), and double-quoted strings with backslash
//! escapes. Comments do not exist in the format. Every node carries the
//! line and column it started on so a reader error can point at the text.

use crate::KicadParse;

#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    /// `(head child…)`; the head is usually an atom but the grammar does
    /// not require it, so it is just the first child.
    List(Vec<Node>, Pos),
    /// Bare token: a symbol (`kicad_pcb`, `yes`) or a number (`1.6`, `-3`).
    Atom(String, Pos),
    /// Double-quoted string, escapes resolved.
    Str(String, Pos),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Pos {
    pub line: u32,
    pub col: u32,
    /// Byte offsets of the node's text: from its first character to just past its last (the
    /// closing paren of a list), so a writer can replace it and keep everything else.
    pub start: usize,
    pub end: usize,
}

impl Node {
    pub fn pos(&self) -> Pos {
        match self {
            Node::List(_, p) | Node::Atom(_, p) | Node::Str(_, p) => *p,
        }
    }

    /// The head symbol of a list (`(at 1 2)` ⇒ `"at"`), if any.
    pub fn head(&self) -> Option<&str> {
        match self {
            Node::List(items, _) => match items.first() {
                Some(Node::Atom(s, _)) => Some(s.as_str()),
                _ => None,
            },
            _ => None,
        }
    }

    /// Children after the head (empty for atoms and strings).
    pub fn args(&self) -> &[Node] {
        match self {
            Node::List(items, _) if !items.is_empty() => &items[1..],
            _ => &[],
        }
    }

    /// Every child list whose head is `name`.
    pub fn find_all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a Node> + 'a {
        self.args().iter().filter(move |n| n.head() == Some(name))
    }

    /// The first child list whose head is `name`.
    pub fn find(&self, name: &str) -> Option<&Node> {
        self.args().iter().find(|n| n.head() == Some(name))
    }

    /// Text of an atom or string (a list has none).
    pub fn text(&self) -> Option<&str> {
        match self {
            Node::Atom(s, _) | Node::Str(s, _) => Some(s.as_str()),
            Node::List(..) => None,
        }
    }

    /// Parse this atom (or string — KiCad quotes numbers in a few places)
    /// as a number.
    pub fn number(&self) -> Result<f64, KicadParse> {
        let p = self.pos();
        self.text()
            .and_then(|t| t.parse::<f64>().ok())
            .filter(|v| v.is_finite())
            .ok_or(KicadParse::Syntax {
                line: p.line,
                col: p.col,
                expected: "number".to_string(),
            })
    }

    /// The `i`-th argument as a number, or a syntax error naming the form.
    pub fn number_arg(&self, i: usize, form: &str) -> Result<f64, KicadParse> {
        match self.args().get(i) {
            Some(n) => n.number(),
            None => {
                let p = self.pos();
                Err(KicadParse::Syntax {
                    line: p.line,
                    col: p.col,
                    expected: format!("{form}: argument {i} (number)"),
                })
            }
        }
    }

    /// The `i`-th argument as text, or a syntax error naming the form.
    pub fn text_arg(&self, i: usize, form: &str) -> Result<&str, KicadParse> {
        match self.args().get(i).and_then(|n| n.text()) {
            Some(t) => Ok(t),
            None => {
                let p = self.pos();
                Err(KicadParse::Syntax {
                    line: p.line,
                    col: p.col,
                    expected: format!("{form}: argument {i} (text)"),
                })
            }
        }
    }
}

/// Parse one top-level S-expression. Trailing whitespace is allowed;
/// anything else after the closing paren is a syntax error.
pub fn parse(text: &str) -> Result<Node, KicadParse> {
    let mut lx = Lexer::new(text);
    let first = lx.next_token()?;
    let root = match first {
        Some(Tok::Open(p)) => parse_list(&mut lx, p)?,
        Some(Tok::Close(p)) => {
            return Err(KicadParse::Syntax {
                line: p.line,
                col: p.col,
                expected: "'('".to_string(),
            })
        }
        Some(Tok::Atom(_, p)) | Some(Tok::Str(_, p)) => {
            return Err(KicadParse::Syntax {
                line: p.line,
                col: p.col,
                expected: "'(' (a KiCad file is one list)".to_string(),
            })
        }
        None => {
            return Err(KicadParse::Syntax {
                line: 1,
                col: 1,
                expected: "'(' (empty input)".to_string(),
            })
        }
    };
    if let Some(t) = lx.next_token()? {
        let p = t.pos();
        return Err(KicadParse::Syntax {
            line: p.line,
            col: p.col,
            expected: "end of input".to_string(),
        });
    }
    Ok(root)
}

fn parse_list(lx: &mut Lexer, open: Pos) -> Result<Node, KicadParse> {
    let mut items = Vec::new();
    loop {
        match lx.next_token()? {
            Some(Tok::Open(p)) => items.push(parse_list(lx, p)?),
            Some(Tok::Close(close)) => return Ok(Node::List(items, Pos { end: close.end, ..open })),
            Some(Tok::Atom(s, p)) => items.push(Node::Atom(s, p)),
            Some(Tok::Str(s, p)) => items.push(Node::Str(s, p)),
            None => {
                return Err(KicadParse::Syntax {
                    line: open.line,
                    col: open.col,
                    expected: "')' closing this list before end of input".to_string(),
                })
            }
        }
    }
}

enum Tok {
    Open(Pos),
    Close(Pos),
    Atom(String, Pos),
    Str(String, Pos),
}

impl Tok {
    fn pos(&self) -> Pos {
        match self {
            Tok::Open(p) | Tok::Close(p) | Tok::Atom(_, p) | Tok::Str(_, p) => *p,
        }
    }
}

struct Lexer<'a> {
    chars: std::iter::Peekable<std::str::Chars<'a>>,
    line: u32,
    col: u32,
    offset: usize,
}

impl<'a> Lexer<'a> {
    fn new(text: &'a str) -> Self {
        Lexer {
            chars: text.chars().peekable(),
            line: 1,
            col: 1,
            offset: 0,
        }
    }

    fn bump(&mut self) -> Option<char> {
        let c = self.chars.next()?;
        self.offset += c.len_utf8();
        if c == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(c)
    }

    fn next_token(&mut self) -> Result<Option<Tok>, KicadParse> {
        while let Some(&c) = self.chars.peek() {
            if c.is_whitespace() {
                self.bump();
            } else {
                break;
            }
        }
        let mut pos = Pos {
            line: self.line,
            col: self.col,
            start: self.offset,
            end: self.offset,
        };
        let Some(c) = self.bump() else {
            return Ok(None);
        };
        pos.end = self.offset;
        match c {
            '(' => Ok(Some(Tok::Open(pos))),
            ')' => Ok(Some(Tok::Close(pos))),
            '"' => {
                let mut s = String::new();
                loop {
                    match self.bump() {
                        Some('"') => break,
                        Some('\\') => match self.bump() {
                            Some('n') => s.push('\n'),
                            Some('t') => s.push('\t'),
                            Some('r') => s.push('\r'),
                            Some(other) => s.push(other),
                            None => {
                                return Err(KicadParse::Syntax {
                                    line: pos.line,
                                    col: pos.col,
                                    expected: "closing '\"' (input ended inside an escape)"
                                        .to_string(),
                                })
                            }
                        },
                        Some(ch) => s.push(ch),
                        None => {
                            return Err(KicadParse::Syntax {
                                line: pos.line,
                                col: pos.col,
                                expected: "closing '\"' before end of input".to_string(),
                            })
                        }
                    }
                }
                Ok(Some(Tok::Str(s, Pos { end: self.offset, ..pos })))
            }
            _ => {
                let mut s = String::new();
                s.push(c);
                while let Some(&n) = self.chars.peek() {
                    if n.is_whitespace() || n == '(' || n == ')' || n == '"' {
                        break;
                    }
                    s.push(n);
                    self.bump();
                }
                Ok(Some(Tok::Atom(s, Pos { end: self.offset, ..pos })))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_nested_lists_atoms_and_strings() {
        let n = parse("(a (b 1 -2.5) \"q \\\"x\\\"\" yes)").unwrap();
        assert_eq!(n.head(), Some("a"));
        let b = n.find("b").unwrap();
        assert_eq!(b.number_arg(0, "b").unwrap(), 1.0);
        assert_eq!(b.number_arg(1, "b").unwrap(), -2.5);
        assert_eq!(n.args()[1].text(), Some("q \"x\""));
        assert_eq!(n.args()[2].text(), Some("yes"));
    }

    #[test]
    fn a_node_knows_its_bytes_in_the_text() {
        let text = "(a (b \"é\" 1) c)";
        let n = parse(text).unwrap();
        let b = n.find("b").unwrap();
        assert_eq!(&text[b.pos().start..b.pos().end], "(b \"é\" 1)");
        assert_eq!(&text[n.pos().start..n.pos().end], text);
        let c = &n.args()[1];
        assert_eq!(&text[c.pos().start..c.pos().end], "c");
    }

    #[test]
    fn positions_are_one_based_line_and_column() {
        let n = parse("(a\n  (b 1))").unwrap();
        let b = n.find("b").unwrap();
        assert_eq!((b.pos().line, b.pos().col), (2, 3));
    }

    #[test]
    fn unbalanced_open_points_at_the_open_paren() {
        let e = parse("(a (b 1)").unwrap_err();
        match e {
            KicadParse::Syntax { line, col, .. } => assert_eq!((line, col), (1, 1)),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn trailing_garbage_is_a_syntax_error() {
        let e = parse("(a) b").unwrap_err();
        match e {
            KicadParse::Syntax {
                line,
                col,
                expected,
            } => {
                assert_eq!((line, col), (1, 5));
                assert!(expected.contains("end of input"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn unterminated_string_is_a_syntax_error() {
        assert!(matches!(
            parse("(a \"open").unwrap_err(),
            KicadParse::Syntax { .. }
        ));
    }

    #[test]
    fn bad_number_names_the_form() {
        let n = parse("(at x 2)").unwrap();
        let e = n.number_arg(0, "at").unwrap_err();
        assert!(matches!(e, KicadParse::Syntax { expected, .. } if expected == "number"));
    }
}

/// KiCad 10 names a net wherever it is used, `(net "GND")`, and keeps no numbered table. Each
/// such form becomes `(net N "GND")` and the board gets the table KiCad 9 wrote: net 0 the empty
/// name, then each other name numbered as first met in the file. A board with a table is left
/// as it is. Whether the board named its nets so.
pub fn number_nets(root: &mut Node) -> bool {
    let named = |n: &Node| n.head() == Some("net") && n.args().len() == 1 && matches!(n.args()[0], Node::Str(..));
    fn any(n: &Node, named: &dyn Fn(&Node) -> bool) -> bool {
        named(n) || matches!(n, Node::List(items, _) if items.iter().any(|c| any(c, named)))
    }
    if !any(root, &named) || root.args().iter().any(|n| n.head() == Some("net") && n.args().first().is_some_and(|a| a.number().is_ok())) {
        return false;
    }
    let names = net_names(root);
    fn renumber(n: &mut Node, names: &[String]) {
        let Node::List(items, pos) = n else { return };
        if items.len() == 2 && matches!(&items[0], Node::Atom(h, _) if h == "net") {
            if let Node::Str(name, at) = &items[1] {
                let number = names.iter().position(|x| x == name).unwrap_or(0);
                *items = vec![Node::Atom("net".into(), *pos), Node::Atom(number.to_string(), *at), Node::Str(name.clone(), *at)];
                return;
            }
        }
        for c in items.iter_mut() {
            renumber(c, names);
        }
    }
    renumber(root, &names);
    if let Node::List(items, pos) = root {
        let at = 1.min(items.len());
        let table = names.iter().enumerate().map(|(i, name)| Node::List(vec![Node::Atom("net".into(), *pos), Node::Atom(i.to_string(), *pos), Node::Str(name.clone(), *pos)], *pos));
        items.splice(at..at, table);
    }
    true
}

/// The names of a board that names its nets in place, numbered as [`number_nets`] numbers them.
pub fn net_names(root: &Node) -> Vec<String> {
    fn walk(n: &Node, out: &mut Vec<String>) {
        let Node::List(items, _) = n else { return };
        if items.len() == 2 && matches!(&items[0], Node::Atom(h, _) if h == "net") {
            if let Node::Str(name, _) = &items[1] {
                if !out.contains(name) {
                    out.push(name.clone());
                }
                return;
            }
        }
        for c in items {
            walk(c, out);
        }
    }
    let mut out = vec![String::new()];
    walk(root, &mut out);
    out
}
