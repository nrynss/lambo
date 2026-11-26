//! Reading the Lean models for `formal_constants_tests`: a lexer that drops
//! comments, the declaration boundary, parsers for definitions, structure
//! fields, records, anonymous constructors and tables, and an evaluator for
//! `Nat` formulas.

use std::ops::Range;
use std::time::Duration;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Kind {
    Ident,
    Num,
    Str,
    Char,
    Sym,
}

/// One Lean token, comments dropped.
#[derive(Clone, Debug)]
pub(super) struct Tok {
    kind: Kind,
    /// As written; a string or char literal keeps its quotes and escapes.
    text: String,
    line: usize,
    /// Column of the first character, in characters.
    col: usize,
    /// The first token on its line (once comments are dropped).
    line_start: bool,
}

/// The commands and modifiers that start a Lean declaration: a definition
/// ends at a line that starts, no further right than the definition, with one
/// of these (or `@[`, `#`), or at the end of the file.
pub(super) const DECL_KEYWORDS: &[&str] = &[
    "def",
    "theorem",
    "lemma",
    "example",
    "structure",
    "inductive",
    "class",
    "instance",
    "abbrev",
    "axiom",
    "opaque",
    "end",
    "namespace",
    "section",
    "open",
    "variable",
    "universe",
    "set_option",
    "attribute",
    "export",
    "private",
    "protected",
    "noncomputable",
    "partial",
    "unsafe",
    "mutual",
    "deriving",
    "macro",
    "macro_rules",
    "syntax",
    "notation",
    "infix",
    "infixl",
    "infixr",
    "prefix",
    "postfix",
    "elab",
];

pub(super) fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

pub(super) fn is_ident_rest(c: char) -> bool {
    c.is_alphanumeric() || matches!(c, '_' | '\'' | '!' | '?')
}

/// The length of the char literal at the start of `cs` (`'x'`, `'\n'`,
/// `'\x41'`), if there is one.
pub(super) fn char_literal_len(cs: &[char]) -> Option<usize> {
    match cs {
        ['\'', '\\', rest @ ..] => rest
            .iter()
            .take(8)
            .skip(1)
            .position(|&c| c == '\'')
            .map(|p| p + 4),
        ['\'', c, '\'', ..] if *c != '\'' && *c != '\n' => Some(3),
        _ => None,
    }
}

/// Lean source as tokens, with `--` line comments and `/- -/` block
/// comments (doc comments `/--`, `/-!` included, nesting honoured) dropped.
/// Identifiers keep their dotted names (`EdgeType.countsTowardGcConnectivity`);
/// `:=` and `=>` are one token; every other symbol is one character.
pub(super) fn lex(src: &str) -> Result<Vec<Tok>, String> {
    let cs: Vec<char> = src.chars().collect();
    let mut toks: Vec<Tok> = Vec::new();
    let (mut i, mut line, mut col) = (0usize, 1usize, 0usize);
    let mut last_line = 0usize;
    while i < cs.len() {
        let c = cs[i];
        let next = cs.get(i + 1).copied();
        if c == '\n' {
            i += 1;
            line += 1;
            col = 0;
            continue;
        }
        if c.is_whitespace() {
            i += 1;
            col += 1;
            continue;
        }
        if c == '-' && next == Some('-') {
            while i < cs.len() && cs[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && next == Some('-') {
            let (from, mut depth) = (line, 0usize);
            loop {
                match (cs.get(i), cs.get(i + 1)) {
                    (None, _) => return Err(format!("comment from line {from} never closes")),
                    (Some('/'), Some('-')) => {
                        depth += 1;
                        i += 2;
                        col += 2;
                    }
                    (Some('-'), Some('/')) => {
                        depth -= 1;
                        i += 2;
                        col += 2;
                        if depth == 0 {
                            break;
                        }
                    }
                    (Some('\n'), _) => {
                        i += 1;
                        line += 1;
                        col = 0;
                    }
                    _ => {
                        i += 1;
                        col += 1;
                    }
                }
            }
            continue;
        }

        let (start, start_line, start_col) = (i, line, col);
        let kind = if c == '"' {
            i += 1;
            loop {
                match cs.get(i) {
                    None => return Err(format!("string from line {start_line} never closes")),
                    Some('\\') => i += 2,
                    Some('"') => {
                        i += 1;
                        break;
                    }
                    Some('\n') => {
                        i += 1;
                        line += 1;
                    }
                    Some(_) => i += 1,
                }
            }
            Kind::Str
        } else if let Some(n) = (c == '\'').then(|| char_literal_len(&cs[i..])).flatten() {
            i += n;
            Kind::Char
        } else if is_ident_start(c) {
            i += 1;
            loop {
                match cs.get(i) {
                    Some(&d) if is_ident_rest(d) => i += 1,
                    Some('.') if cs.get(i + 1).is_some_and(|&d| is_ident_start(d)) => i += 1,
                    _ => break,
                }
            }
            Kind::Ident
        } else if c.is_ascii_digit() {
            while cs.get(i).is_some_and(char::is_ascii_digit) {
                i += 1;
            }
            Kind::Num
        } else if (c == ':' && next == Some('=')) || (c == '=' && next == Some('>')) {
            i += 2;
            Kind::Sym
        } else {
            i += 1;
            Kind::Sym
        };
        let text: String = cs[start..i].iter().collect();
        col = match text.rfind('\n') {
            Some(p) => text[p + 1..].chars().count(),
            None => start_col + (i - start),
        };
        toks.push(Tok {
            kind,
            text,
            line: start_line,
            col: start_col,
            line_start: start_line != last_line,
        });
        last_line = line;
    }
    Ok(toks)
}

/// A Lean string or char literal's contents, escapes resolved.
pub(super) fn unescape(body: &str) -> Option<String> {
    let mut out = String::new();
    let mut it = body.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        let hex = |it: &mut std::str::Chars<'_>, n: usize| {
            let digits: String = it.by_ref().take(n).collect();
            u32::from_str_radix(&digits, 16)
                .ok()
                .and_then(char::from_u32)
        };
        out.push(match it.next()? {
            'n' => '\n',
            't' => '\t',
            'r' => '\r',
            '\\' => '\\',
            '"' => '"',
            '\'' => '\'',
            'x' => hex(&mut it, 2)?,
            'u' => hex(&mut it, 4)?,
            _ => return None,
        });
    }
    Some(out)
}

pub(super) fn render(toks: &[Tok]) -> String {
    toks.iter()
        .map(|t| t.text.as_str())
        .collect::<Vec<_>>()
        .join(" ")
}

pub(super) fn same(a: &[Tok], b: &[Tok]) -> bool {
    a.len() == b.len()
        && a.iter()
            .zip(b)
            .all(|(x, y)| x.kind == y.kind && x.text == y.text)
}

#[track_caller]
pub(super) fn lex_shape(shape: &str) -> Vec<Tok> {
    lex(shape).unwrap_or_else(|e| panic!("this test's Lean text does not lex: {e}\n  {shape}"))
}

/// A literal read out of a model.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Lit {
    Int(i128),
    Str(String),
    Char(char),
}

/// The literal at the start of `toks` and how many tokens it takes: a
/// number, a negative number in parentheses (`(-100)`, the form Lean needs
/// in an argument), a string or a char. A bare `-` is not a literal: `a - 1`
/// is a subtraction.
pub(super) fn literal_at(toks: &[Tok]) -> Option<(Lit, usize)> {
    let t = toks.first()?;
    let body = || t.text.get(1..t.text.len() - 1).and_then(unescape);
    match t.kind {
        Kind::Num => Some((Lit::Int(t.text.parse().ok()?), 1)),
        Kind::Str => Some((Lit::Str(body()?), 1)),
        Kind::Char => {
            let s = body()?;
            let mut cs = s.chars();
            let c = cs.next()?;
            cs.next().is_none().then_some((Lit::Char(c), 1))
        }
        Kind::Sym if t.text == "(" => match toks.get(1..4)? {
            [minus, n, close] if minus.text == "-" && n.kind == Kind::Num && close.text == ")" => {
                Some((Lit::Int(-n.text.parse::<i128>().ok()?), 4))
            }
            _ => None,
        },
        _ => None,
    }
}

/// Matches `want` at the start of `toks`, a `$` in `want` taking one
/// literal: the tokens consumed and the literals taken.
pub(super) fn match_at(toks: &[Tok], want: &[Tok]) -> Option<(usize, Vec<Lit>)> {
    let (mut j, mut lits) = (0, Vec::new());
    for w in want {
        if w.kind == Kind::Sym && w.text == "$" {
            let (lit, n) = literal_at(&toks[j..])?;
            lits.push(lit);
            j += n;
        } else {
            let t = toks.get(j)?;
            if t.kind != w.kind || t.text != w.text {
                return None;
            }
            j += 1;
        }
    }
    Some((j, lits))
}

/// `toks` split at the top-level commas between an `open` and a `close`
/// that enclose all of it, or `None` if they do not.
pub(super) fn elements<'t>(toks: &'t [Tok], open: &str, close: &str) -> Option<Vec<&'t [Tok]>> {
    let (first, last) = (toks.first()?, toks.last()?);
    if toks.len() < 2 || first.text != open || last.text != close {
        return None;
    }
    let inner = &toks[1..toks.len() - 1];
    let (mut out, mut from, mut depth) = (Vec::new(), 0, 0i32);
    for (j, t) in inner.iter().enumerate() {
        match t.text.as_str() {
            "(" | "[" | "{" | "⟨" => depth += 1,
            ")" | "]" | "}" | "⟩" => depth -= 1,
            "," if depth == 0 => {
                out.push(&inner[from..j]);
                from = j + 1;
            }
            _ => {}
        }
        if depth < 0 {
            return None;
        }
    }
    if depth != 0 {
        return None;
    }
    if !inner.is_empty() {
        out.push(&inner[from..]);
    }
    Some(out)
}

/// A value read from a model, with where it was read (`formal/LamboProofs/
/// <Model>.lean: <definition>`).
#[derive(Clone, Debug)]
pub(super) struct Pin<T> {
    pub(super) value: T,
    pub(super) at: String,
}

impl Pin<i128> {
    #[track_caller]
    pub(super) fn to<U: TryFrom<i128>>(self) -> Pin<U> {
        let value = U::try_from(self.value).unwrap_or_else(|_| {
            panic!(
                "{}: {} is out of range for the Rust type",
                self.at, self.value
            )
        });
        Pin { value, at: self.at }
    }

    #[track_caller]
    pub(super) fn ms(self) -> Pin<Duration> {
        let n = self.to::<u64>();
        Pin {
            value: Duration::from_millis(n.value),
            at: n.at,
        }
    }

    #[track_caller]
    pub(super) fn secs(self) -> Pin<Duration> {
        let n = self.to::<u64>();
        Pin {
            value: Duration::from_secs(n.value),
            at: n.at,
        }
    }

    /// The fixed-point integer at `scale`, as the `f64` it stands for.
    #[track_caller]
    pub(super) fn scaled(self, scale: i128) -> Pin<f64> {
        let at = format!("{} (at scale {scale})", self.at);
        let n = self.to::<i32>();
        let s = i32::try_from(scale).expect("a scale fits an i32");
        Pin {
            value: f64::from(n.value) / f64::from(s),
            at,
        }
    }
}

/// The literals a shape's `$` holes took, in order.
pub(super) struct Caps {
    pub(super) at: String,
    lits: Vec<Lit>,
}

impl Caps {
    #[track_caller]
    pub(super) fn lit(&self, i: usize) -> (&Lit, String) {
        let lit = self
            .lits
            .get(i)
            .unwrap_or_else(|| panic!("{}: the shape has no literal {}", self.at, i + 1));
        (lit, format!("{} (literal {})", self.at, i + 1))
    }

    #[track_caller]
    pub(super) fn int(&self, i: usize) -> Pin<i128> {
        match self.lit(i) {
            (Lit::Int(n), at) => Pin { value: *n, at },
            (other, at) => panic!("{at}: {other:?} is not an integer"),
        }
    }

    #[track_caller]
    pub(super) fn str(&self, i: usize) -> Pin<String> {
        match self.lit(i) {
            (Lit::Str(s), at) => Pin {
                value: s.clone(),
                at,
            },
            (other, at) => panic!("{at}: {other:?} is not a string"),
        }
    }

    #[track_caller]
    pub(super) fn chr(&self, i: usize) -> Pin<char> {
        match self.lit(i) {
            (Lit::Char(c), at) => Pin { value: *c, at },
            (other, at) => panic!("{at}: {other:?} is not a char"),
        }
    }
}

/// A Lean model, comments dropped.
pub(super) struct Lean {
    model: &'static str,
    source: &'static str,
    toks: Vec<Tok>,
}

/// A definition's value (or a field's), as tokens.
pub(super) struct Val<'a> {
    pub(super) at: String,
    toks: &'a [Tok],
}

/// A `def f : T → U` by cases over the inductive `T`.
pub(super) struct Table<'a> {
    at: String,
    inductive: String,
    ctors: Vec<String>,
    /// `(Some(ctor), value)` for `| .ctor => value`, `(None, value)` for
    /// `| _ => value`.
    arms: Vec<(Option<String>, &'a [Tok])>,
}

impl Lean {
    #[track_caller]
    pub(super) fn new((model, source): (&'static str, &'static str)) -> Self {
        let toks = lex(source).unwrap_or_else(|e| panic!("formal/LamboProofs/{model}.lean: {e}"));
        Self {
            model,
            source,
            toks,
        }
    }

    pub(super) fn at(&self, what: &str) -> String {
        format!("formal/LamboProofs/{}.lean: {what}", self.model)
    }

    /// Whether a declaration (or a structure field) that starts at column
    /// `indent` ends before token `i`: the token starts a line, no further
    /// right than `indent`, and starts a declaration (or, inside a
    /// structure, the next field `name :`).
    pub(super) fn ends_before(&self, i: usize, indent: usize) -> bool {
        let Some(t) = self.toks.get(i) else {
            return true;
        };
        if !t.line_start || t.col > indent {
            return false;
        }
        match t.kind {
            Kind::Ident => {
                DECL_KEYWORDS.contains(&t.text.as_str())
                    || (indent > 0
                        && t.col == indent
                        && self.toks.get(i + 1).is_some_and(|n| n.text == ":"))
            }
            Kind::Sym => t.text == "@" || t.text == "#",
            _ => false,
        }
    }

    pub(super) fn end_from(&self, from: usize, indent: usize) -> usize {
        (from..self.toks.len())
            .find(|&i| self.ends_before(i, indent))
            .unwrap_or(self.toks.len())
    }

    /// The column of the first token on token `i`'s line.
    pub(super) fn indent_of(&self, i: usize) -> usize {
        let mut j = i;
        while !self.toks[j].line_start {
            j -= 1;
        }
        self.toks[j].col
    }

    /// The one `<keyword> <name>` in the model.
    #[track_caller]
    pub(super) fn find_decl(&self, keyword: &str, name: &str) -> usize {
        let hits: Vec<usize> = self
            .toks
            .windows(2)
            .enumerate()
            .filter(|(_, w)| {
                w[0].kind == Kind::Ident
                    && w[0].text == keyword
                    && w[1].kind == Kind::Ident
                    && w[1].text == name
            })
            .map(|(i, _)| i)
            .collect();
        match hits[..] {
            [i] => i,
            [] => panic!("{} is gone", self.at(&format!("`{keyword} {name}`"))),
            _ => panic!(
                "{} is declared {} times (lines {:?})",
                self.at(&format!("`{keyword} {name}`")),
                hits.len(),
                hits.iter().map(|&i| self.toks[i].line).collect::<Vec<_>>()
            ),
        }
    }

    /// Splits `from..end` into a header and a value at the first top-level
    /// `:=`, or before match arms (a `|` starting a line).
    #[track_caller]
    pub(super) fn split_header(
        &self,
        from: usize,
        end: usize,
        what: &str,
    ) -> (Range<usize>, Range<usize>) {
        let mut depth = 0i32;
        for (j, t) in self.toks.iter().enumerate().take(end).skip(from) {
            match t.text.as_str() {
                "(" | "[" | "{" | "⟨" => depth += 1,
                ")" | "]" | "}" | "⟩" => depth -= 1,
                ":=" if depth == 0 => return (from..j, j + 1..end),
                "|" if depth == 0 && t.line_start => return (from..j, j..end),
                _ => {}
            }
        }
        panic!("{}: no `:=` or match arms", self.at(what))
    }

    #[track_caller]
    pub(super) fn check_type(&self, header: Range<usize>, ty: &str, what: &str) {
        if !same(&self.toks[header.clone()], &lex_shape(&format!(": {ty}"))) {
            panic!(
                "{} is declared `{}`, not `: {ty}`",
                self.at(what),
                render(&self.toks[header])
            );
        }
    }

    /// The value of the one `def <name> : <ty>` (no parameters), up to where
    /// the declaration ends.
    #[track_caller]
    pub(super) fn def(&self, name: &str, ty: &str) -> Val<'_> {
        let i = self.find_decl("def", name);
        let end = self.end_from(i + 2, self.indent_of(i));
        let (header, value) = self.split_header(i + 2, end, name);
        self.check_type(header, ty, name);
        Val {
            at: self.at(name),
            toks: &self.toks[value],
        }
    }

    /// The fields of `structure <name>`'s body, and the column they start at.
    #[track_caller]
    pub(super) fn structure_body(&self, name: &str) -> (Range<usize>, usize) {
        let i = self.find_decl("structure", name);
        let end = self.end_from(i + 2, self.indent_of(i));
        let start = (i + 2..end)
            .find(|&j| self.toks[j].kind == Kind::Ident && self.toks[j].text == "where")
            .unwrap_or_else(|| panic!("{}: no `where`", self.at(name)))
            + 1;
        let first = self
            .toks
            .get(start)
            .filter(|t| start < end && t.line_start)
            .unwrap_or_else(|| panic!("{}: no fields on their own lines", self.at(name)));
        (start..end, first.col)
    }

    pub(super) fn field_starts(
        &self,
        body: Range<usize>,
        col: usize,
    ) -> impl Iterator<Item = usize> + '_ {
        body.filter(move |&j| {
            let t = &self.toks[j];
            t.line_start
                && t.col == col
                && t.kind == Kind::Ident
                && self.toks.get(j + 1).is_some_and(|n| n.text == ":")
        })
    }

    /// `structure <name>`'s field names, in order (the order an anonymous
    /// constructor `⟨…⟩` follows).
    #[track_caller]
    pub(super) fn fields(&self, name: &str) -> Vec<String> {
        let (body, col) = self.structure_body(name);
        self.field_starts(body, col)
            .map(|j| self.toks[j].text.clone())
            .collect()
    }

    /// The default of field `field : <ty> := …` in `structure <structure>`.
    #[track_caller]
    pub(super) fn field(&self, structure: &str, field: &str, ty: &str) -> Val<'_> {
        let what = format!("{structure}.{field}");
        let (body, col) = self.structure_body(structure);
        let hits: Vec<usize> = self
            .field_starts(body.clone(), col)
            .filter(|&j| self.toks[j].text == field)
            .collect();
        let [j] = hits[..] else {
            panic!(
                "{} is stated {} times, not once",
                self.at(&what),
                hits.len()
            );
        };
        let end = (j + 1..body.end)
            .find(|&k| self.ends_before(k, col))
            .unwrap_or(body.end);
        let (header, value) = self.split_header(j + 1, end, &what);
        self.check_type(header, ty, &what);
        Val {
            at: self.at(&what),
            toks: &self.toks[value],
        }
    }

    /// `inductive <name>`'s constructors, in order.
    #[track_caller]
    pub(super) fn ctors(&self, name: &str) -> Vec<String> {
        let i = self.find_decl("inductive", name);
        let end = self.end_from(i + 2, self.indent_of(i));
        let mut depth = 0i32;
        let mut out = Vec::new();
        for j in i + 2..end {
            match self.toks[j].text.as_str() {
                "(" | "[" | "{" | "⟨" => depth += 1,
                ")" | "]" | "}" | "⟩" => depth -= 1,
                "|" if depth == 0 => {
                    if let Some(c) = self.toks.get(j + 1).filter(|c| c.kind == Kind::Ident) {
                        out.push(c.text.clone());
                    }
                }
                _ => {}
            }
        }
        assert!(!out.is_empty(), "{}: no constructors", self.at(name));
        out
    }

    /// Checks `names` (the test's Lean constructor for each Rust variant)
    /// are `inductive <name>`'s constructors, each once.
    #[track_caller]
    pub(super) fn check_ctors(&self, name: &str, names: &[&str]) {
        check_names(&self.at(name), &self.ctors(name), names);
    }

    /// `def <name> : <ty>` by cases over the inductive `<ty>` starts with.
    #[track_caller]
    pub(super) fn table(&self, name: &str, ty: &str) -> Table<'_> {
        let at = self.at(name);
        let value = self.def(name, ty);
        let inductive = lex_shape(ty)[0].text.clone();
        let ctors = self.ctors(&inductive);
        let toks = value.toks;
        assert!(
            toks.first().is_some_and(|t| t.text == "|"),
            "{at}: not a table of match arms"
        );
        let mut starts: Vec<usize> = Vec::new();
        let mut depth = 0i32;
        for (j, t) in toks.iter().enumerate() {
            match t.text.as_str() {
                "(" | "[" | "{" | "⟨" => depth += 1,
                ")" | "]" | "}" | "⟩" => depth -= 1,
                "|" if depth == 0 => starts.push(j),
                _ => {}
            }
        }
        starts.push(toks.len());
        let arms = starts
            .windows(2)
            .map(|w| {
                let arm = &toks[w[0] + 1..w[1]];
                let arrow = arm
                    .iter()
                    .position(|t| t.text == "=>")
                    .unwrap_or_else(|| panic!("{at}: arm `{}` has no `=>`", render(arm)));
                let pattern = match &arm[..arrow] {
                    [dot, c] if dot.text == "." && c.kind == Kind::Ident => {
                        assert!(
                            ctors.contains(&c.text),
                            "{at}: `.{}` is not a constructor of {inductive}",
                            c.text
                        );
                        Some(c.text.clone())
                    }
                    [w] if w.text == "_" => None,
                    other => panic!("{at}: pattern `{}` is not `.ctor` or `_`", render(other)),
                };
                (pattern, &arm[arrow + 1..])
            })
            .collect();
        Table {
            at,
            inductive,
            ctors,
            arms,
        }
    }

    /// Asserts the model states each shape once (comments dropped, `$`
    /// holes taking a literal each), starting a line and ending where its
    /// declaration ends.
    #[track_caller]
    pub(super) fn captures(&self, shape: &str) -> Caps {
        let want = lex_shape(shape);
        let name = want
            .iter()
            .skip_while(|t| t.text != "def")
            .nth(1)
            .map_or_else(|| render(&want), |t| t.text.clone());
        let at = self.at(&name);
        let hits: Vec<(usize, usize, Vec<Lit>)> = (0..self.toks.len())
            .filter_map(|p| match_at(&self.toks[p..], &want).map(|(n, lits)| (p, p + n, lits)))
            .collect();
        let [(start, end, lits)] = &hits[..] else {
            panic!(
                "{at}: the model states this {} times, not once:\n  {}\n\
                 The model changed a definition the Rust is pinned to: change the Rust to \
                 match (or correct the model), then update this test's shape.",
                hits.len(),
                render(&want)
            );
        };
        let first = &self.toks[*start];
        assert!(
            first.line_start && self.ends_before(*end, first.col),
            "{at} (line {}) continues past this test's shape:\n  {}\n\
             The model changed a definition the Rust is pinned to.",
            first.line,
            render(&want)
        );
        Caps {
            at,
            lits: lits.clone(),
        }
    }

    /// [`Lean::captures`] for shapes with no holes.
    #[track_caller]
    pub(super) fn states(&self, shapes: &[&str]) {
        for shape in shapes {
            let caps = self.captures(shape);
            assert!(
                caps.lits.is_empty(),
                "{}: use `captures` for holes",
                caps.at
            );
        }
    }

    /// Asserts the model's text (doc comments included) states `text` once,
    /// whitespace-insensitive, a `$` in `text` taking a run of digits: the
    /// numbers taken. For prose pins only: a definition is read with the
    /// comment-free helpers above.
    #[track_caller]
    pub(super) fn doc_captures(&self, text: &str) -> Caps {
        let squash = |s: &str| format!(" {} ", s.split_whitespace().collect::<Vec<_>>().join(" "));
        let source = squash(self.source);
        let text = squash(text);
        let parts: Vec<&str> = text.split('$').collect();
        let hits: Vec<Vec<Lit>> = source
            .match_indices(parts[0])
            .filter_map(|(at, head)| {
                let mut rest = &source[at + head.len()..];
                let mut lits = Vec::new();
                for part in &parts[1..] {
                    let digits =
                        rest.len() - rest.trim_start_matches(|c: char| c.is_ascii_digit()).len();
                    lits.push(Lit::Int(rest[..digits].parse().ok()?));
                    rest = rest[digits..].strip_prefix(*part)?;
                }
                Some(lits)
            })
            .collect();
        let [lits] = &hits[..] else {
            panic!(
                "formal/LamboProofs/{}.lean states this {} times, not once:\n  {}\n\
                 The model's doc changed what it says about the Rust: check the Rust \
                 still does what the model now says, then update this test's copy.",
                self.model,
                hits.len(),
                text.trim()
            );
        };
        Caps {
            at: format!(
                "formal/LamboProofs/{}.lean: \"{}\"",
                self.model,
                text.trim()
            ),
            lits: lits.clone(),
        }
    }

    /// [`Lean::doc_captures`] for texts with no holes.
    #[track_caller]
    pub(super) fn doc_states(&self, texts: &[&str]) {
        for text in texts {
            let caps = self.doc_captures(text);
            assert!(
                caps.lits.is_empty(),
                "{}: use `doc_captures` for holes",
                caps.at
            );
        }
    }
}

/// A Lean `Nat` value. Lean's `Nat` is unbounded; the values here fit.
pub(super) type Nat = i128;

pub(super) fn nat(n: usize) -> Nat {
    Nat::try_from(n).expect("a usize fits an i128")
}

pub(super) fn millis(d: Duration) -> Nat {
    Nat::try_from(d.as_millis()).expect("a duration in milliseconds fits an i128")
}

impl Lean {
    /// The parameters of `def <name> (a b : Nat) … : Nat`, and its body.
    #[track_caller]
    pub(super) fn nat_fn(&self, name: &str) -> (Vec<String>, &[Tok]) {
        let i = self.find_decl("def", name);
        let end = self.end_from(i + 2, self.indent_of(i));
        let (header, value) = self.split_header(i + 2, end, name);
        let h = &self.toks[header];
        let mut params = Vec::new();
        let mut j = 0;
        while h.get(j).is_some_and(|t| t.text == "(") {
            let close = h[j..]
                .iter()
                .position(|t| t.text == ")")
                .map(|p| j + p)
                .unwrap_or_else(|| panic!("{}: unclosed parameter", self.at(name)));
            match &h[j + 1..close] {
                [names @ .., colon, ty]
                    if colon.text == ":"
                        && ty.text == "Nat"
                        && !names.is_empty()
                        && names.iter().all(|n| n.kind == Kind::Ident) =>
                {
                    params.extend(names.iter().map(|n| n.text.clone()));
                }
                other => panic!(
                    "{}: parameter `({})` is not a `Nat`",
                    self.at(name),
                    render(other)
                ),
            }
            j = close + 1;
        }
        match &h[j..] {
            [colon, ty] if colon.text == ":" && ty.text == "Nat" => {}
            other => panic!("{} is `{}`, not `… : Nat`", self.at(name), render(other)),
        }
        (params, &self.toks[value])
    }

    /// `def <name> … : Nat` applied to `args` and evaluated: literals,
    /// parameters, other such definitions, `+ - * / % ^` (with `Nat`'s
    /// truncating `-` and `/`), `min`, `max`, comparisons and
    /// `if … then … else`. Anything else fails the test.
    #[track_caller]
    pub(super) fn apply(&self, name: &str, args: &[Nat]) -> Pin<Nat> {
        let (params, body) = self.nat_fn(name);
        assert_eq!(
            params.len(),
            args.len(),
            "{} takes {params:?}",
            self.at(name)
        );
        let vars: Vec<(String, Nat)> = params.into_iter().zip(args.iter().copied()).collect();
        let call = std::iter::once(name.to_string())
            .chain(args.iter().map(ToString::to_string))
            .collect::<Vec<_>>()
            .join(" ");
        let at = self.at(&call);
        Pin {
            value: self.eval(body, &vars, &at),
            at,
        }
    }

    /// A value read with [`Lean::def`] and friends, evaluated as
    /// [`Lean::apply`] evaluates a body.
    #[track_caller]
    pub(super) fn eval_val(&self, val: &Val<'_>) -> Pin<Nat> {
        Pin {
            value: self.eval(val.toks, &[], &val.at),
            at: val.at.clone(),
        }
    }

    #[track_caller]
    pub(super) fn eval(&self, toks: &[Tok], vars: &[(String, Nat)], at: &str) -> Nat {
        let mut e = Eval {
            lean: self,
            at,
            toks,
            pos: 0,
            vars,
        };
        let v = e.expr();
        if e.pos != toks.len() {
            e.fail();
        }
        v
    }
}

/// A recursive-descent evaluator over a `Nat` expression's tokens.
pub(super) struct Eval<'l, 't> {
    lean: &'l Lean,
    at: &'t str,
    toks: &'t [Tok],
    pos: usize,
    vars: &'t [(String, Nat)],
}

impl Eval<'_, '_> {
    pub(super) fn peek(&self) -> Option<&str> {
        self.toks.get(self.pos).map(|t| t.text.as_str())
    }

    #[track_caller]
    pub(super) fn fail(&self) -> ! {
        panic!(
            "{}: cannot evaluate `{}` (at `{}`)",
            self.at,
            render(self.toks),
            self.peek().unwrap_or("the end")
        )
    }

    pub(super) fn eat(&mut self, s: &str) -> bool {
        let hit = self.peek() == Some(s);
        if hit {
            self.pos += 1;
        }
        hit
    }

    #[track_caller]
    pub(super) fn expect(&mut self, s: &str) {
        if !self.eat(s) {
            self.fail();
        }
    }

    #[track_caller]
    pub(super) fn ok(&self, v: Option<Nat>) -> Nat {
        v.unwrap_or_else(|| self.fail())
    }

    pub(super) fn var(&self, name: &str) -> Option<Nat> {
        self.vars.iter().find(|(n, _)| n == name).map(|&(_, v)| v)
    }

    #[track_caller]
    pub(super) fn expr(&mut self) -> Nat {
        if self.eat("if") {
            let c = self.cmp();
            self.expect("then");
            let a = self.expr();
            self.expect("else");
            let b = self.expr();
            if c != 0 {
                a
            } else {
                b
            }
        } else {
            self.cmp()
        }
    }

    #[track_caller]
    pub(super) fn cmp(&mut self) -> Nat {
        let a = self.add();
        let Some(op) = self
            .peek()
            .filter(|op| ["<", "≤", ">", "≥", "="].contains(op))
            .map(str::to_string)
        else {
            return a;
        };
        self.pos += 1;
        let b = self.add();
        Nat::from(match op.as_str() {
            "<" => a < b,
            "≤" => a <= b,
            ">" => a > b,
            "≥" => a >= b,
            _ => a == b,
        })
    }

    #[track_caller]
    pub(super) fn add(&mut self) -> Nat {
        let mut v = self.mul();
        loop {
            if self.eat("+") {
                let r = self.mul();
                v = self.ok(v.checked_add(r));
            } else if self.eat("-") {
                let r = self.mul();
                v = (v - r).max(0);
            } else {
                return v;
            }
        }
    }

    #[track_caller]
    pub(super) fn mul(&mut self) -> Nat {
        let mut v = self.pow();
        loop {
            if self.eat("*") {
                let r = self.pow();
                v = self.ok(v.checked_mul(r));
            } else if self.eat("/") {
                let r = self.pow();
                v = v.checked_div(r).unwrap_or(0);
            } else if self.eat("%") {
                let r = self.pow();
                v = v.checked_rem(r).unwrap_or(v);
            } else {
                return v;
            }
        }
    }

    #[track_caller]
    pub(super) fn pow(&mut self) -> Nat {
        let base = self.app();
        if !self.eat("^") {
            return base;
        }
        let exp = self.pow();
        match u32::try_from(exp) {
            Ok(e) => self.ok(base.checked_pow(e)),
            Err(_) => self.fail(),
        }
    }

    /// `min a b`, `max a b`, `f a …` (a definition with parameters), or an
    /// atom.
    #[track_caller]
    pub(super) fn app(&mut self) -> Nat {
        let toks = self.toks;
        let Some(t) = toks.get(self.pos) else {
            self.fail()
        };
        if t.kind != Kind::Ident || self.var(&t.text).is_some() {
            return self.atom();
        }
        match t.text.as_str() {
            "min" | "max" => {
                self.pos += 1;
                let (a, b) = (self.atom(), self.atom());
                if t.text == "min" {
                    a.min(b)
                } else {
                    a.max(b)
                }
            }
            "if" | "then" | "else" => self.fail(),
            name => {
                self.pos += 1;
                let (params, _) = self.lean.nat_fn(name);
                let args: Vec<Nat> = params.iter().map(|_| self.atom()).collect();
                self.lean.apply(name, &args).value
            }
        }
    }

    #[track_caller]
    pub(super) fn atom(&mut self) -> Nat {
        let toks = self.toks;
        let Some(t) = toks.get(self.pos) else {
            self.fail()
        };
        match t.kind {
            Kind::Num => {
                self.pos += 1;
                self.ok(t.text.parse().ok())
            }
            Kind::Ident => {
                self.pos += 1;
                match self.var(&t.text) {
                    Some(v) => v,
                    None => self.lean.apply(&t.text, &[]).value,
                }
            }
            Kind::Sym if t.text == "(" => {
                self.pos += 1;
                let v = self.expr();
                self.expect(")");
                v
            }
            _ => self.fail(),
        }
    }
}

#[track_caller]
pub(super) fn check_names(at: &str, ctors: &[String], names: &[&str]) {
    let mut have: Vec<&str> = ctors.iter().map(String::as_str).collect();
    let mut want = names.to_vec();
    have.sort_unstable();
    want.sort_unstable();
    assert_eq!(
        have, want,
        "{at}: the Lean constructors and the test's names for the Rust variants differ"
    );
}

impl Val<'_> {
    /// Matches the whole value against `shape` (`$` holes taking literals).
    #[track_caller]
    pub(super) fn shape(&self, shape: &str) -> Caps {
        match match_at(self.toks, &lex_shape(shape)) {
            Some((n, lits)) if n == self.toks.len() => Caps {
                at: self.at.clone(),
                lits,
            },
            _ => panic!("{} is `{}`, not `{shape}`", self.at, render(self.toks)),
        }
    }

    /// A natural-number literal.
    #[track_caller]
    pub(super) fn nat(&self) -> Pin<i128> {
        match self.toks {
            [t] if t.kind == Kind::Num => self.shape("$").int(0).relabel(&self.at),
            _ => panic!("{} is `{}`, not a literal", self.at, render(self.toks)),
        }
    }

    /// An integer literal: `n`, `-n` or `(-n)`.
    #[track_caller]
    pub(super) fn int(&self) -> Pin<i128> {
        match self.toks {
            [minus, n] if minus.text == "-" && n.kind == Kind::Num => Pin {
                value: -n
                    .text
                    .parse::<i128>()
                    .unwrap_or_else(|_| panic!("{}: `{}` is too large", self.at, n.text)),
                at: self.at.clone(),
            },
            _ => self.shape("$").int(0).relabel(&self.at),
        }
    }

    #[track_caller]
    pub(super) fn bool(&self) -> Pin<bool> {
        match self.toks {
            [t] if t.kind == Kind::Ident && (t.text == "true" || t.text == "false") => Pin {
                value: t.text == "true",
                at: self.at.clone(),
            },
            _ => panic!("{} is `{}`, not a Bool", self.at, render(self.toks)),
        }
    }

    #[track_caller]
    pub(super) fn string(&self) -> Pin<String> {
        self.shape("$").str(0).relabel(&self.at)
    }

    /// A constructor, `.ctor`.
    #[track_caller]
    pub(super) fn ctor(&self) -> Pin<String> {
        match self.toks {
            [dot, c] if dot.text == "." && c.kind == Kind::Ident => Pin {
                value: c.text.clone(),
                at: self.at.clone(),
            },
            _ => panic!("{} is `{}`, not `.ctor`", self.at, render(self.toks)),
        }
    }

    #[track_caller]
    pub(super) fn list(&self) -> Vec<Val<'_>> {
        elements(self.toks, "[", "]")
            .unwrap_or_else(|| panic!("{} is `{}`, not a list", self.at, render(self.toks)))
            .into_iter()
            .enumerate()
            .map(|(i, toks)| Val {
                at: format!("{} (element {})", self.at, i + 1),
                toks,
            })
            .collect()
    }

    /// A list of string literals.
    #[track_caller]
    pub(super) fn strings(&self) -> Pin<Vec<String>> {
        Pin {
            value: self.list().iter().map(|v| v.string().value).collect(),
            at: self.at.clone(),
        }
    }

    /// A list of natural-number literals.
    #[track_caller]
    pub(super) fn nats(&self) -> Pin<Vec<i128>> {
        Pin {
            value: self.list().iter().map(|v| v.nat().value).collect(),
            at: self.at.clone(),
        }
    }

    /// Field `field` of a record literal `{ a := …, b := … }`.
    #[track_caller]
    pub(super) fn record(&self, field: &str) -> Val<'_> {
        let entries = elements(self.toks, "{", "}")
            .unwrap_or_else(|| panic!("{} is `{}`, not a record", self.at, render(self.toks)));
        let hits: Vec<&[Tok]> = entries
            .into_iter()
            .filter_map(|e| match e {
                [name, eq, value @ ..] if name.text == field && eq.text == ":=" => Some(value),
                _ => None,
            })
            .collect();
        let [toks] = hits[..] else {
            panic!("{}: field `{field}` is set {} times", self.at, hits.len());
        };
        Val {
            at: format!("{} ({field})", self.at),
            toks,
        }
    }

    /// Field `field` of an anonymous constructor `⟨…⟩` of a structure with
    /// `fields` (from [`Lean::fields`]).
    #[track_caller]
    pub(super) fn anon(&self, fields: &[String], field: &str) -> Val<'_> {
        let parts = elements(self.toks, "⟨", "⟩")
            .unwrap_or_else(|| panic!("{} is `{}`, not `⟨…⟩`", self.at, render(self.toks)));
        assert_eq!(
            parts.len(),
            fields.len(),
            "{}: `{}` does not give each of {fields:?}",
            self.at,
            render(self.toks)
        );
        let i = fields
            .iter()
            .position(|f| f == field)
            .unwrap_or_else(|| panic!("{}: no field `{field}` in {fields:?}", self.at));
        Val {
            at: format!("{} ({field})", self.at),
            toks: parts[i],
        }
    }
}

impl<T> Pin<T> {
    pub(super) fn relabel(self, at: &str) -> Self {
        Pin {
            value: self.value,
            at: at.to_string(),
        }
    }
}

impl Table<'_> {
    /// Checks `names` (the test's Lean constructor for each Rust variant)
    /// are the inductive's constructors, each once.
    #[track_caller]
    pub(super) fn check_names(&self, names: &[&str]) {
        check_names(&self.at, &self.ctors, names);
    }

    /// The table's value at constructor `ctor`.
    #[track_caller]
    pub(super) fn at(&self, ctor: &str) -> Val<'_> {
        assert!(
            self.ctors.iter().any(|c| c == ctor),
            "{}: `.{ctor}` is not a constructor of {}",
            self.at,
            self.inductive
        );
        let (_, toks) = self
            .arms
            .iter()
            .find(|(p, _)| p.as_deref().is_none_or(|p| p == ctor))
            .unwrap_or_else(|| panic!("{}: no arm for `.{ctor}`", self.at));
        Val {
            at: format!("{} (.{ctor})", self.at),
            toks,
        }
    }
}

impl Pin<char> {
    pub(super) fn map_to_string(self) -> Pin<String> {
        Pin {
            value: self.value.to_string(),
            at: self.at,
        }
    }
}

/// The reader itself: the cases the pins rely on.
mod tests {
    use super::*;

    const SRC: &str = r#"
/-- Was `def a : Nat := 1` before. -/
def a : Nat := 2
-- def b : Nat := 3
def b : Nat := 4
def c : Nat :=
  -- a harmless comment
  5
def d : Nat := 6 * 2
def e : Nat := 7
  + 1
structure S where
  /-- The first. -/
  x : Nat := 8
  y : Nat := 9
    * 2
  deriving Repr
def s : String := "a  b"
inductive T where
  | p
  /-- doc -/
  | q
deriving Repr
def f : T → Nat
  | .p => 1
  | _ => 0
def r : S := { x := 1, y := some 2 }
"#;

    fn lean() -> Lean {
        Lean::new(("Reader", SRC))
    }

    #[test]
    fn comments_do_not_count() {
        let l = lean();
        assert_eq!(l.def("a", "Nat").nat().value, 2);
        assert_eq!(l.def("b", "Nat").nat().value, 4);
        assert_eq!(l.def("c", "Nat").nat().value, 5);
    }

    #[test]
    #[should_panic(expected = "not a literal")]
    fn an_extended_value_fails() {
        let _ = lean().def("d", "Nat").nat();
    }

    #[test]
    #[should_panic(expected = "not a literal")]
    fn a_value_continued_on_the_next_line_fails() {
        let _ = lean().def("e", "Nat").nat();
    }

    #[test]
    #[should_panic(expected = "continues past")]
    fn a_shape_must_end_with_its_declaration() {
        lean().states(&["def d : Nat := 6"]);
    }

    #[test]
    fn fields_are_read_within_their_structure() {
        let l = lean();
        assert_eq!(l.fields("S"), ["x", "y"]);
        assert_eq!(l.field("S", "x", "Nat").nat().value, 8);
    }

    #[test]
    #[should_panic(expected = "not a literal")]
    fn a_field_continued_on_the_next_line_fails() {
        let _ = lean().field("S", "y", "Nat").nat();
    }

    #[test]
    fn string_literals_keep_their_spaces() {
        let l = lean();
        assert_eq!(l.def("s", "String").string().value, "a  b");
        let caught = std::panic::catch_unwind(|| l.states(&[r#"def s : String := "a b""#]));
        assert!(caught.is_err());
    }

    #[test]
    fn tables_and_records() {
        let l = lean();
        assert_eq!(l.ctors("T"), ["p", "q"]);
        let f = l.table("f", "T → Nat");
        assert_eq!(f.at("p").nat().value, 1);
        assert_eq!(f.at("q").nat().value, 0);
        let r = l.def("r", "S");
        assert_eq!(r.record("x").nat().value, 1);
        assert_eq!(r.record("y").shape("some $").int(0).value, 2);
    }

    #[test]
    fn doc_text_is_read_with_its_numbers() {
        let l = lean();
        let caps = l.doc_captures("Was `def a : Nat := $` before.");
        assert_eq!(caps.int(0).value, 1);
        l.doc_states(&["Was `def a : Nat := 1`\n before."]);
    }

    #[test]
    fn lexing_edge_cases() {
        let toks = lex(
            r#"/- outer /- inner -/ still outer -/ def s : String := "a -- b"
def c : Char := '\''
def e : Char := '…'"#,
        )
        .expect("lexes");
        let texts: Vec<&str> = toks.iter().map(|t| t.text.as_str()).collect();
        assert_eq!(
            texts,
            [
                "def",
                "s",
                ":",
                "String",
                ":=",
                r#""a -- b""#,
                "def",
                "c",
                ":",
                "Char",
                ":=",
                r"'\''",
                "def",
                "e",
                ":",
                "Char",
                ":=",
                "'…'",
            ]
        );
        assert_eq!(literal_at(&toks[5..]), Some((Lit::Str("a -- b".into()), 1)));
        assert_eq!(literal_at(&toks[11..]), Some((Lit::Char('\''), 1)));
        assert_eq!(literal_at(&toks[17..]), Some((Lit::Char('…'), 1)));
    }

    #[test]
    fn a_negative_literal_needs_its_parentheses() {
        let l = Lean::new((
            "Neg",
            "def f (x : Int) : Int := clamp (-100) x
def g (x : Int) : Int := x - 100",
        ));
        assert_eq!(
            l.captures("def f (x : Int) : Int := clamp $ x")
                .int(0)
                .value,
            -100
        );
        let caught = std::panic::catch_unwind(|| l.captures("def g (x : Int) : Int := x $"));
        assert!(caught.is_err(), "`- 100` is a subtraction, not a literal");
    }

    #[test]
    fn division_and_remainder_by_zero_are_lean_s() {
        let l = Lean::new((
            "Div",
            "def d (x : Nat) : Nat := x / 0
def r (x : Nat) : Nat := x % 0",
        ));
        assert_eq!(l.apply("d", &[7]).value, 0);
        assert_eq!(l.apply("r", &[7]).value, 7);
    }

    #[test]
    fn nat_formulas_evaluate() {
        let l = Lean::new((
            "Eval",
            "def k : Nat := 3
def g (x y : Nat) : Nat := if k * x < y then y - 10 else min (x / 2) (2 ^ 4 % 5)
def h (max : Nat) : Nat := max / 2 + g 1 4
def p : Nat := h 9",
        ));
        assert_eq!(l.apply("g", &[1, 4]).value, 0);
        assert_eq!(l.apply("g", &[1, 40]).value, 30);
        assert_eq!(l.apply("g", &[9, 4]).value, 1);
        assert_eq!(l.apply("h", &[9]).value, 4);
        assert_eq!(l.apply("p", &[]).value, 4);
    }

    #[test]
    #[should_panic(expected = "cannot evaluate")]
    fn an_unknown_form_does_not_evaluate() {
        let l = Lean::new(("Eval", "def q (x : Nat) : Nat := x &&& 1"));
        let _ = l.apply("q", &[1]);
    }

    #[test]
    #[should_panic(expected = "is not a constructor")]
    fn a_misnamed_constructor_fails() {
        let _ = lean().table("f", "T → Nat").at("r");
    }

    #[test]
    #[should_panic(expected = "differ")]
    fn names_must_cover_the_inductive() {
        lean().table("f", "T → Nat").check_names(&["p"]);
    }
}
