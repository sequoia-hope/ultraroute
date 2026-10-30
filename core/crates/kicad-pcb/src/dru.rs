//! KiCad's custom design rules, the `.kicad_dru` file: each rule's layer clause, condition,
//! constraints and severity, and the condition language evaluated over the two objects a rule
//! is asked about (`A`, and `B` for a constraint between two).
//!
//! Evaluation is three-valued. A property or function the caller cannot answer (an area the
//! router has not placed copper in yet, say) is `Unknown`, and so is any test that depends on
//! it; [`RuleSet::resolve`] then reports the rules that may hold beside the one that surely
//! does, so a caller can take the strictest. The semantics follow KiCad 9.0.8's DRC, checked
//! with kicad-cli: the last matching rule in the file wins, over the net classes and the board's
//! minimums alike, and a rule between two objects is tried both ways round.

use crate::sexpr::{self, Node};

const MM: f64 = 1e-3;

/// A rules file, parsed.
#[derive(Debug, Clone, Default)]
pub struct RuleSet {
    pub rules: Vec<Rule>,
    /// Rules that could not be read, by name, with why; they are not in `rules`.
    pub errors: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct Rule {
    pub name: String,
    pub layer: Option<LayerSel>,
    /// The condition, absent for a rule that holds everywhere.
    pub condition: Option<Expr>,
    pub constraints: Vec<Constraint>,
    pub severity: Severity,
}

/// A rule's `(layer …)` clause.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LayerSel {
    Outer,
    Inner,
    /// A layer name, `?` and `*` standing for any character or run (`?.Cu`).
    Named(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    Error,
    Warning,
    /// The constraint is not checked where the rule holds.
    Ignore,
    Exclusion,
}

/// One `(constraint KIND …)`: lengths in metres, counts (`via_count`) as they are, a bare number
/// where the constraint takes one
/// (`min_resolved_spokes 1`) in `min`, and the words that follow the kind (`disallow track via`,
/// `zone_connection solid`) in `words`.
#[derive(Debug, Clone, PartialEq)]
pub struct Constraint {
    pub kind: String,
    pub min: Option<f64>,
    pub opt: Option<f64>,
    pub max: Option<f64>,
    pub words: Vec<String>,
}

/// A condition, parsed.
#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
    Or(Box<Expr>, Box<Expr>),
    And(Box<Expr>, Box<Expr>),
    Not(Box<Expr>),
    Cmp(Op, Box<Expr>, Box<Expr>),
    /// `A.NetName`: a property of one object, its name normalised (see [`norm`]).
    Prop(Side, String),
    /// `A.intersectsArea('x')`: a function of one object, its name normalised.
    Call(Side, String, Vec<Expr>),
    Str(String),
    /// A number; `true` where it carried a unit and so is a length in metres.
    Num(f64, bool),
}

impl Expr {
    /// Whether `f` holds for this condition or any part of it.
    pub fn any(&self, f: &dyn Fn(&Expr) -> bool) -> bool {
        f(self)
            || match self {
                Expr::Or(x, y) | Expr::And(x, y) | Expr::Cmp(_, x, y) => x.any(f) || y.any(f),
                Expr::Not(x) => x.any(f),
                Expr::Call(_, _, args) => args.iter().any(|a| a.any(f)),
                _ => false,
            }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Op {
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
}

/// What a property or function gives back.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    Str(String),
    /// A length, in metres.
    Len(f64),
    /// A plain number; beside a length, millimetres.
    Num(f64),
    Bool(bool),
    /// Equal to any member: the layers an object is on, the classes of a net.
    Set(Vec<String>),
    /// One of these, not known which: equal to a name if every one matches it, unequal if none
    /// does, else unknown (the copper layer of a track yet to be laid).
    OneOf(Vec<String>),
    /// No object there (`B` of a constraint about one object), or nothing of the kind.
    Null,
    /// Something no comparison takes: equal to nothing and unequal to nothing (KiCad 9.0.8 on a
    /// through via's `A.Layer`, checked with kicad-cli: `== 'B.Cu'` and `!= 'B.Cu'` both fail).
    Opaque,
    /// Something the caller cannot answer.
    Unknown,
}

/// Three-valued truth.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tri {
    True,
    False,
    Unknown,
}

impl Tri {
    fn of(b: bool) -> Tri {
        if b { Tri::True } else { Tri::False }
    }

    fn and(self, o: Tri) -> Tri {
        match (self, o) {
            (Tri::False, _) | (_, Tri::False) => Tri::False,
            (Tri::True, Tri::True) => Tri::True,
            _ => Tri::Unknown,
        }
    }

    fn or(self, o: Tri) -> Tri {
        match (self, o) {
            (Tri::True, _) | (_, Tri::True) => Tri::True,
            (Tri::False, Tri::False) => Tri::False,
            _ => Tri::Unknown,
        }
    }

    fn not(self) -> Tri {
        match self {
            Tri::True => Tri::False,
            Tri::False => Tri::True,
            Tri::Unknown => Tri::Unknown,
        }
    }
}

/// An object a condition asks about: a track, via, pad, zone or edge, as the caller models it.
/// Names come normalised by [`norm`] (`Pad_Type` is `padtype`); a length is in metres.
pub trait Object {
    fn property(&self, name: &str) -> Value;
    /// A function such as `intersectsArea('x')`; `Unknown` for one the caller does not know.
    fn call(&self, name: &str, args: &[Value]) -> Value;
}

/// A property or function name as conditions match it: lower case, without `_` or spaces
/// (KiCad shows `Pad Type` and accepts `Pad_Type`).
pub fn norm(name: &str) -> String {
    name.chars().filter(|c| *c != '_' && *c != ' ').flat_map(char::to_lowercase).collect()
}

/// What [`RuleSet::resolve`] found for one constraint kind.
#[derive(Debug, Clone, Default)]
pub struct Resolved<'r> {
    /// The last rule that surely holds, with its constraint; `None` where no rule surely holds,
    /// so the net classes and board minimums stand (unless a `maybe` holds).
    pub sure: Option<(&'r Rule, &'r Constraint)>,
    /// Rules after `sure` in the file that may hold: any of them would win if it did.
    pub maybe: Vec<(&'r Rule, &'r Constraint)>,
}

impl Resolved<'_> {
    /// The strictest minimum this could be: the sure rule's (or `fallback` where none is sure)
    /// and every rule that may hold. A rule that ignores the check is as loose as can be, so it
    /// counts only when it surely holds, as 0.
    pub fn strictest_min(&self, fallback: f64) -> f64 {
        let base = match self.sure {
            Some((rule, _)) if rule.severity == Severity::Ignore => 0.0,
            Some((_, c)) => c.min.unwrap_or(fallback),
            None => fallback,
        };
        self.maybe.iter().filter(|(r, _)| r.severity != Severity::Ignore).filter_map(|(_, c)| c.min).fold(base, f64::max)
    }

    /// Whether the outcome depends on something the caller could not answer.
    pub fn exact(&self) -> bool {
        self.maybe.is_empty()
    }
}

impl RuleSet {
    /// Reads a `.kicad_dru` file's text. A rule that cannot be read is left out and said in
    /// `errors`; a file that is not a rules file at all is an error.
    pub fn parse(text: &str) -> Result<RuleSet, String> {
        // `#` starts a comment line; the rest is a run of top-level lists, read as one.
        let body: String = text.lines().map(|l| if l.trim_start().starts_with('#') { "" } else { l }).collect::<Vec<_>>().join("\n");
        let root = sexpr::parse(&format!("(rules\n{body}\n)")).map_err(|e| format!("not a rules file: {e}"))?;
        let mut set = RuleSet::default();
        for node in root.args() {
            match node.head() {
                Some("version") => {}
                Some("rule") => match Rule::read(node) {
                    Ok(rule) => set.rules.push(rule),
                    Err(e) => set.errors.push(e),
                },
                other => set.errors.push(format!("line {}: unexpected {}", node.pos().line, other.unwrap_or("text"))),
            }
        }
        Ok(set)
    }

    /// The constraint of `kind` between `a` and `b` (or on `a` alone) on `layer` (of
    /// `copper_layers` copper layers), as KiCad resolves it: the last rule in the file with such
    /// a constraint whose layer clause takes `layer` and whose condition holds, tried both ways
    /// round when `b` is given. Rules after it whose condition is unknown come back as `maybe`.
    pub fn resolve<'r>(&'r self, kind: &str, a: &dyn Object, b: Option<&dyn Object>, layer: &str, copper_layers: usize) -> Resolved<'r> {
        let mut out = Resolved::default();
        for rule in self.rules.iter().rev() {
            let Some(c) = rule.constraints.iter().find(|c| c.kind == kind) else { continue };
            if !rule.layer.as_ref().is_none_or(|l| l.takes(layer, copper_layers)) {
                continue;
            }
            let holds = match (&rule.condition, b) {
                (None, _) => Tri::True,
                (Some(e), None) => eval(e, a, None),
                (Some(e), Some(b)) => eval(e, a, Some(b)).or(eval(e, b, Some(a))),
            };
            match holds {
                Tri::True => {
                    out.sure = Some((rule, c));
                    break;
                }
                Tri::Unknown => out.maybe.push((rule, c)),
                Tri::False => {}
            }
        }
        out
    }

    /// Whether any rule has a constraint of `kind`.
    pub fn has(&self, kind: &str) -> bool {
        self.rules.iter().any(|r| r.constraints.iter().any(|c| c.kind == kind))
    }
}

impl LayerSel {
    /// Whether the clause takes `layer` on a board of `copper_layers` copper layers.
    pub fn takes(&self, layer: &str, copper_layers: usize) -> bool {
        let _ = copper_layers;
        match self {
            LayerSel::Outer => layer == "F.Cu" || layer == "B.Cu",
            LayerSel::Inner => layer.starts_with("In") && layer.ends_with(".Cu"),
            LayerSel::Named(pattern) => wildcard(pattern, layer, false),
        }
    }
}

impl Rule {
    fn read(node: &Node) -> Result<Rule, String> {
        let line = node.pos().line;
        let name = node.args().first().and_then(Node::text).ok_or(format!("line {line}: a rule with no name"))?.to_string();
        let at = |what: String| format!("rule \"{name}\" (line {line}): {what}");
        let mut rule = Rule { name: name.clone(), layer: None, condition: None, constraints: Vec::new(), severity: Severity::Error };
        for part in &node.args()[1..] {
            match part.head() {
                Some("layer") => {
                    let l = part.args().first().and_then(Node::text).ok_or_else(|| at("an empty layer clause".into()))?;
                    rule.layer = Some(match l {
                        "outer" => LayerSel::Outer,
                        "inner" => LayerSel::Inner,
                        other => LayerSel::Named(other.to_string()),
                    });
                }
                Some("condition") => {
                    let text = part.args().first().and_then(Node::text).ok_or_else(|| at("an empty condition".into()))?;
                    rule.condition = Some(parse_expr(text).map_err(&at)?);
                }
                Some("constraint") => rule.constraints.push(Constraint::read(part).map_err(&at)?),
                Some("severity") => {
                    rule.severity = match part.args().first().and_then(Node::text) {
                        Some("error") => Severity::Error,
                        Some("warning") => Severity::Warning,
                        Some("ignore") => Severity::Ignore,
                        Some("exclusion") => Severity::Exclusion,
                        other => return Err(at(format!("severity {}", other.unwrap_or("")))),
                    }
                }
                other => return Err(at(format!("unexpected {}", other.unwrap_or("text")))),
            }
        }
        Ok(rule)
    }
}

impl Constraint {
    fn read(node: &Node) -> Result<Constraint, String> {
        let mut args = node.args().iter();
        let kind = args.next().and_then(Node::text).ok_or("a constraint with no kind")?.to_string();
        let mut c = Constraint { kind, min: None, opt: None, max: None, words: Vec::new() };
        for arg in args {
            match (arg.head(), arg) {
                (Some(bound @ ("min" | "opt" | "max")), _) => {
                    let text = arg.args().first().and_then(Node::text).ok_or(format!("{} {bound} with no value", c.kind))?;
                    // A count is a plain number; anything else a length.
                    let count = matches!(c.kind.as_str(), "via_count" | "min_resolved_spokes");
                    let v = if count { text.trim().parse().ok() } else { quantity(text) }.ok_or(format!("{} {bound} {text}: not a number", c.kind))?;
                    match bound {
                        "min" => c.min = Some(v),
                        "opt" => c.opt = Some(v),
                        _ => c.max = Some(v),
                    }
                }
                (None, Node::Atom(t, _)) | (None, Node::Str(t, _)) => match t.parse::<f64>() {
                    Ok(v) if c.words.is_empty() => c.min = Some(v),
                    _ => c.words.push(t.clone()),
                },
                _ => return Err(format!("{}: unexpected {}", c.kind, arg.head().unwrap_or("list"))),
            }
        }
        Ok(c)
    }
}

/// A constraint's value: a length (`0.2mm`, `5mil`, a bare number in millimetres) in metres.
fn quantity(text: &str) -> Option<f64> {
    let (v, unit) = number_with_unit(text.trim())?;
    Some(if unit { v } else { v * MM })
}

/// A number with an optional unit; `true` with a unit, the value then in metres.
fn number_with_unit(t: &str) -> Option<(f64, bool)> {
    let split = t.find(|c: char| c.is_ascii_alphabetic() && c != 'e' && c != 'E').unwrap_or(t.len());
    let (number, unit) = t.split_at(split);
    let v: f64 = number.trim().parse().ok()?;
    let scale = match unit.trim() {
        "" => return Some((v, false)),
        "mm" => MM,
        "mil" | "mils" => 25.4e-6,
        "in" => 25.4e-3,
        "um" => 1e-6,
        "nm" => 1e-9,
        _ => return None,
    };
    Some((v * scale, true))
}

/// `?` and `*` wildcards over `text`, ignoring case where `fold`.
pub fn wildcard(pattern: &str, text: &str, fold: bool) -> bool {
    let low = |s: &str| if fold { s.to_lowercase() } else { s.to_string() };
    let (p, t): (Vec<char>, Vec<char>) = (low(pattern).chars().collect(), low(text).chars().collect());
    let (mut pi, mut ti, mut star, mut mark) = (0, 0, None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ti;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Evaluates a condition with `a` as `A` and `b` as `B`.
pub fn eval(e: &Expr, a: &dyn Object, b: Option<&dyn Object>) -> Tri {
    match e {
        Expr::Or(x, y) => eval(x, a, b).or(eval(y, a, b)),
        Expr::And(x, y) => {
            let left = eval(x, a, b);
            if left == Tri::False {
                return Tri::False;
            }
            left.and(eval(y, a, b))
        }
        Expr::Not(x) => eval(x, a, b).not(),
        Expr::Cmp(op, x, y) => compare(*op, &value(x, a, b), &value(y, a, b)),
        other => truth(&value(other, a, b)),
    }
}

fn truth(v: &Value) -> Tri {
    match v {
        Value::Bool(b) => Tri::of(*b),
        Value::Num(n) | Value::Len(n) => Tri::of(*n != 0.0),
        Value::Unknown => Tri::Unknown,
        Value::Str(s) => Tri::of(!s.is_empty()),
        Value::Set(_) | Value::Null | Value::Opaque => Tri::False,
        Value::OneOf(_) => Tri::Unknown,
    }
}

fn value(e: &Expr, a: &dyn Object, b: Option<&dyn Object>) -> Value {
    let side = |s: Side| match s {
        Side::A => Some(a),
        Side::B => b,
    };
    match e {
        Expr::Str(s) => Value::Str(s.clone()),
        Expr::Num(v, true) => Value::Len(*v),
        Expr::Num(v, false) => Value::Num(*v),
        Expr::Prop(s, name) => side(*s).map_or(Value::Null, |o| o.property(name)),
        Expr::Call(s, name, args) => {
            let args: Vec<Value> = args.iter().map(|x| value(x, a, b)).collect();
            side(*s).map_or(Value::Null, |o| o.call(name, &args))
        }
        cond => match eval(cond, a, b) {
            Tri::True => Value::Bool(true),
            Tri::False => Value::Bool(false),
            Tri::Unknown => Value::Unknown,
        },
    }
}

/// Two values compared as KiCad's expressions do: strings by wildcard, either way round and
/// ignoring case; a layer set equal to a layer it holds; numbers by value.
fn compare(op: Op, x: &Value, y: &Value) -> Tri {
    use Value::*;
    let eq = match (x, y) {
        (Unknown, _) | (_, Unknown) => return Tri::Unknown,
        (Opaque, _) | (_, Opaque) => return Tri::False,
        (Null, Null) => Some(true),
        (Null, _) | (_, Null) => Some(false),
        (Str(p), Str(q)) => Some(p.eq_ignore_ascii_case(q) || wildcard(q, p, true) || wildcard(p, q, true)),
        (Set(ls), Str(s)) | (Str(s), Set(ls)) => Some(ls.iter().any(|l| wildcard(s, l, true))),
        (OneOf(ls), Str(s)) | (Str(s), OneOf(ls)) => {
            let hits = ls.iter().filter(|l| l.eq_ignore_ascii_case(s) || wildcard(s, l, true)).count();
            if hits == 0 {
                Some(false)
            } else if hits == ls.len() {
                Some(true)
            } else {
                return match op {
                    Op::Eq | Op::Ne => Tri::Unknown,
                    _ => Tri::False,
                };
            }
        }
        (Bool(p), Bool(q)) => Some(p == q),
        _ => None,
    };
    if let Some(eq) = eq {
        return match op {
            Op::Eq => Tri::of(eq),
            Op::Ne => Tri::of(!eq),
            _ => Tri::False,
        };
    }
    // A bare number beside a length is in millimetres.
    let length = matches!(x, Len(_)) || matches!(y, Len(_));
    let num = |v: &Value| match v {
        Len(n) => Some(*n),
        Num(n) => Some(if length { n * MM } else { *n }),
        Str(s) => number_with_unit(s).map(|(v, unit)| if unit || !length { v } else { v * MM }),
        Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
        _ => None,
    };
    let (Some(p), Some(q)) = (num(x), num(y)) else { return Tri::False };
    Tri::of(match op {
        Op::Eq => (p - q).abs() < 1e-12,
        Op::Ne => (p - q).abs() >= 1e-12,
        Op::Lt => p < q,
        Op::Le => p <= q,
        Op::Gt => p > q,
        Op::Ge => p >= q,
    })
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Ident(String),
    Str(String),
    Num(f64, bool),
    Op(&'static str),
}

fn tokens(text: &str) -> Result<Vec<Tok>, String> {
    let c: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '\'' || ch == '"' {
            let end = c[i + 1..].iter().position(|&x| x == ch).ok_or(format!("an unclosed string in {text:?}"))? + i + 1;
            out.push(Tok::Str(c[i + 1..end].iter().collect()));
            i = end + 1;
        } else if ch.is_ascii_digit() || (ch == '.' && c.get(i + 1).is_some_and(char::is_ascii_digit)) {
            let start = i;
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '.') {
                i += 1;
            }
            let t: String = c[start..i].iter().collect();
            let (v, unit) = number_with_unit(&t).ok_or(format!("{t}: not a number"))?;
            out.push(Tok::Num(v, unit));
        } else if ch.is_alphabetic() || ch == '_' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_') {
                i += 1;
            }
            out.push(Tok::Ident(c[start..i].iter().collect()));
        } else {
            let two: String = c[i..(i + 2).min(c.len())].iter().collect();
            let op = ["==", "!=", "<=", ">=", "&&", "||"].into_iter().find(|o| *o == two);
            if let Some(op) = op {
                out.push(Tok::Op(op));
                i += 2;
            } else {
                let one = ["(", ")", ".", ",", "<", ">", "!", "+", "-", "*", "/"].into_iter().find(|o| o.starts_with(ch)).ok_or(format!("{ch:?} in {text:?}"))?;
                out.push(Tok::Op(one));
                i += 1;
            }
        }
    }
    Ok(out)
}

/// Parses a rule's condition.
pub fn parse_expr(text: &str) -> Result<Expr, String> {
    let toks = tokens(text)?;
    let mut p = Parser { toks: &toks, at: 0, text };
    let e = p.or()?;
    if p.at < toks.len() {
        return Err(format!("unexpected {:?} in {text:?}", toks[p.at]));
    }
    Ok(e)
}

struct Parser<'a> {
    toks: &'a [Tok],
    at: usize,
    text: &'a str,
}

impl Parser<'_> {
    fn peek_op(&self, op: &str) -> bool {
        matches!(self.toks.get(self.at), Some(Tok::Op(o)) if *o == op)
    }

    fn expect(&mut self, op: &str) -> Result<(), String> {
        if self.peek_op(op) {
            self.at += 1;
            Ok(())
        } else {
            Err(format!("{op:?} expected in {:?}", self.text))
        }
    }

    fn or(&mut self) -> Result<Expr, String> {
        let mut e = self.and()?;
        while self.peek_op("||") {
            self.at += 1;
            e = Expr::Or(Box::new(e), Box::new(self.and()?));
        }
        Ok(e)
    }

    fn and(&mut self) -> Result<Expr, String> {
        let mut e = self.cmp()?;
        while self.peek_op("&&") {
            self.at += 1;
            e = Expr::And(Box::new(e), Box::new(self.cmp()?));
        }
        Ok(e)
    }

    fn cmp(&mut self) -> Result<Expr, String> {
        let left = self.unary()?;
        let op = match self.toks.get(self.at) {
            Some(Tok::Op("==")) => Op::Eq,
            Some(Tok::Op("!=")) => Op::Ne,
            Some(Tok::Op("<")) => Op::Lt,
            Some(Tok::Op("<=")) => Op::Le,
            Some(Tok::Op(">")) => Op::Gt,
            Some(Tok::Op(">=")) => Op::Ge,
            _ => return Ok(left),
        };
        self.at += 1;
        Ok(Expr::Cmp(op, Box::new(left), Box::new(self.unary()?)))
    }

    fn unary(&mut self) -> Result<Expr, String> {
        if self.peek_op("!") {
            self.at += 1;
            return Ok(Expr::Not(Box::new(self.unary()?)));
        }
        if self.peek_op("-") {
            self.at += 1;
            return match self.unary()? {
                Expr::Num(v, unit) => Ok(Expr::Num(-v, unit)),
                _ => Err(format!("a minus before something not a number in {:?}", self.text)),
            };
        }
        self.atom()
    }

    fn atom(&mut self) -> Result<Expr, String> {
        let tok = self.toks.get(self.at).cloned().ok_or(format!("{:?} ends early", self.text))?;
        self.at += 1;
        match tok {
            Tok::Op("(") => {
                let e = self.or()?;
                self.expect(")")?;
                Ok(e)
            }
            Tok::Str(s) => Ok(Expr::Str(s)),
            Tok::Num(v, unit) => Ok(Expr::Num(v, unit)),
            Tok::Ident(id) => {
                let side = match id.as_str() {
                    "A" => Side::A,
                    "B" => Side::B,
                    _ => return Err(format!("{id}: only A and B have properties ({:?})", self.text)),
                };
                self.expect(".")?;
                let Some(Tok::Ident(name)) = self.toks.get(self.at).cloned() else {
                    return Err(format!("a property name expected after {id}. in {:?}", self.text));
                };
                self.at += 1;
                if self.peek_op("(") {
                    self.at += 1;
                    let mut args = Vec::new();
                    while !self.peek_op(")") {
                        args.push(self.or()?);
                        if self.peek_op(",") {
                            self.at += 1;
                        }
                    }
                    self.expect(")")?;
                    Ok(Expr::Call(side, norm(&name), args))
                } else {
                    Ok(Expr::Prop(side, norm(&name)))
                }
            }
            other => Err(format!("unexpected {other:?} in {:?}", self.text)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// An object of named properties; anything else unknown.
    struct Obj(HashMap<&'static str, Value>);

    impl Object for Obj {
        fn property(&self, name: &str) -> Value {
            self.0.get(name).cloned().unwrap_or(Value::Unknown)
        }
        fn call(&self, name: &str, args: &[Value]) -> Value {
            match (name, args) {
                ("isplated", _) => self.property("plated"),
                ("intersectsarea", [Value::Str(area)]) => self.0.get("area").map_or(Value::Unknown, |v| Value::Bool(*v == Value::Str(area.clone()))),
                _ => Value::Unknown,
            }
        }
    }

    fn obj(props: &[(&'static str, Value)]) -> Obj {
        Obj(props.iter().cloned().collect())
    }

    fn s(v: &str) -> Value {
        Value::Str(v.to_string())
    }

    #[test]
    fn conditions_parse_and_evaluate() {
        let via = obj(&[("type", s("Via")), ("net", Value::Num(3.0)), ("netname", s("/QSPI_SD0")), ("plated", Value::Bool(true))]);
        let track = obj(&[("type", s("Track")), ("net", Value::Num(4.0)), ("netname", s("GND"))]);
        let t = |c: &str, a: &Obj, b: &Obj| eval(&parse_expr(c).unwrap(), a, Some(b));
        assert_eq!(t("A.Type == 'via' && B.Type == 'track'", &via, &track), Tri::True);
        assert_eq!(t("A.Type == 'Via' && B.Type == A.Type", &via, &track), Tri::False);
        assert_eq!(t("A.Net != B.Net", &via, &track), Tri::True);
        assert_eq!(t("A.NetName == '/QSPI_*'", &via, &track), Tri::True);
        assert_eq!(t("!(A.isPlated()) || B.Type == 'Pad'", &via, &track), Tri::False);
        // An area the caller cannot place is unknown, and so is what hangs on it, unless the
        // rest decides it.
        assert_eq!(t("A.intersectsArea('fine') && B.Type == 'Track'", &via, &track), Tri::Unknown);
        assert_eq!(t("A.intersectsArea('fine') && B.Type == 'Pad'", &via, &track), Tri::False);
        assert_eq!(t("A.intersectsArea('fine') || A.Type == 'Via'", &via, &track), Tri::True);
        let hole = obj(&[("holesizex", Value::Len(0.4e-3))]);
        assert_eq!(t("A.Hole_Size_X > 0.3mm", &hole, &track), Tri::True);
        assert_eq!(t("A.Hole_Size_X > 0.3", &hole, &track), Tri::True);
        assert_eq!(t("A.Hole_Size_X > 0.5mm", &hole, &track), Tri::False);
        let via = obj(&[("layer", Value::Opaque)]);
        assert_eq!(t("A.Layer == 'B.Cu'", &via, &track), Tri::False);
        assert_eq!(t("A.Layer != 'B.Cu'", &via, &track), Tri::False);
    }

    #[test]
    fn a_rules_file_reads_with_its_layers_constraints_and_severities() {
        let text = r#"(version 1)
# A comment.
(rule "Minimum Trace Width (outer layer)"
	(constraint track_width (min 5mil))
	(layer outer)
	(condition "A.Type == 'track'"))
(rule "seam pads live on the board edge"
  (condition "A.memberOfFootprint('SEAM*')")
  (severity ignore)
  (constraint edge_clearance))
(rule "mipi_pairs"
  (condition "A.NetClass == 'MIPI'")
  (constraint diff_pair_gap (min 0.13) (opt 0.14) (max 0.16))
  (constraint diff_pair_uncoupled (max 1.5)))
(rule "Motor layers"
  (condition "A.NetClass == 'Motor' && A.Layer != 'B.Cu'")
  (constraint disallow track via))
(rule "one spoke" (constraint min_resolved_spokes 1))
(rule "broken" (condition "A.Type == ") (constraint clearance (min 1mm)))
"#;
        let set = RuleSet::parse(text).unwrap();
        assert_eq!(set.rules.len(), 5);
        assert_eq!(set.errors.len(), 1, "{:?}", set.errors);
        assert!(set.errors[0].contains("broken"));
        let w = &set.rules[0];
        assert_eq!(w.layer, Some(LayerSel::Outer));
        assert!((w.constraints[0].min.unwrap() - 0.127e-3).abs() < 1e-12);
        assert_eq!(set.rules[1].severity, Severity::Ignore);
        assert!((set.rules[2].constraints[0].opt.unwrap() - 0.14e-3).abs() < 1e-12);
        assert_eq!(set.rules[3].constraints[0].words, ["track", "via"]);
        assert_eq!(set.rules[4].constraints[0].min, Some(1.0));
        let counted = RuleSet::parse("(rule \"v\" (constraint via_count (max 3)))").unwrap();
        assert_eq!(counted.rules[0].constraints[0].max, Some(3.0));
    }

    #[test]
    fn the_last_rule_that_holds_wins_and_unknown_ones_after_it_are_maybe() {
        let text = r#"(version 1)
(rule "all" (constraint clearance (min 0.2mm)))
(rule "rf" (condition "A.NetName == 'RF*'") (constraint clearance (min 0.3mm)))
(rule "fine" (condition "A.intersectsArea('fine')") (constraint clearance (min 0.1mm)))
(rule "inner only" (layer inner) (constraint clearance (min 0.5mm)))
"#;
        let set = RuleSet::parse(text).unwrap();
        let rf = obj(&[("netname", s("RF_OUT"))]);
        let gnd = obj(&[("netname", s("GND"))]);
        let r = set.resolve("clearance", &gnd, Some(&rf), "F.Cu", 4);
        // Tried both ways round: B is the RF net.
        assert_eq!(r.sure.unwrap().0.name, "rf");
        assert_eq!(r.maybe.len(), 1);
        assert!((r.strictest_min(0.0) - 0.3e-3).abs() < 1e-12);
        let r = set.resolve("clearance", &gnd, Some(&gnd), "In1.Cu", 4);
        assert_eq!(r.sure.unwrap().0.name, "inner only");
        let placed = obj(&[("netname", s("GND")), ("area", s("fine"))]);
        let r = set.resolve("clearance", &placed, Some(&gnd), "F.Cu", 4);
        assert_eq!(r.sure.unwrap().0.name, "fine");
        assert!(r.exact());
        assert!((r.strictest_min(0.0) - 0.1e-3).abs() < 1e-12);
    }
}
