//! XPath 1.0 grammar (sections 2 and 3) to an expression tree.

use crate::XPathError;
use crate::lexer::Token;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Axis {
    Ancestor,
    AncestorOrSelf,
    Attribute,
    Child,
    Descendant,
    DescendantOrSelf,
    Following,
    FollowingSibling,
    Namespace,
    Parent,
    Preceding,
    PrecedingSibling,
    Itself,
}

impl Axis {
    fn named(name: &str) -> Option<Axis> {
        Some(match name {
            "ancestor" => Axis::Ancestor,
            "ancestor-or-self" => Axis::AncestorOrSelf,
            "attribute" => Axis::Attribute,
            "child" => Axis::Child,
            "descendant" => Axis::Descendant,
            "descendant-or-self" => Axis::DescendantOrSelf,
            "following" => Axis::Following,
            "following-sibling" => Axis::FollowingSibling,
            "namespace" => Axis::Namespace,
            "parent" => Axis::Parent,
            "preceding" => Axis::Preceding,
            "preceding-sibling" => Axis::PrecedingSibling,
            "self" => Axis::Itself,
            _ => return None,
        })
    }

    /// Reverse axes number their nodes from the context node outwards.
    pub(crate) fn is_reverse(self) -> bool {
        matches!(
            self,
            Axis::Ancestor | Axis::AncestorOrSelf | Axis::Preceding | Axis::PrecedingSibling
        )
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum NodeTest {
    /// `*`: any node of the axis's principal type.
    Any,
    /// A name without a prefix, matched against the principal node type.
    /// In XML it names something in no namespace.
    Name(String),
    /// `prefix:name`, the prefix bound by the namespaces a query is run
    /// with.
    Prefixed(String, String),
    /// `prefix:*`: anything in that namespace.
    AnyIn(String),
    Node,
    Text,
    Comment,
    ProcessingInstruction(Option<String>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Step {
    pub axis: Axis,
    pub test: NodeTest,
    pub predicates: Vec<Expr>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Op {
    Or,
    And,
    Eq,
    Neq,
    Lt,
    Le,
    Gt,
    Ge,
    Add,
    Sub,
    Mul,
    Div,
    Mod,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Expr {
    Binary(Op, Box<Expr>, Box<Expr>),
    Negate(Box<Expr>),
    Union(Box<Expr>, Box<Expr>),
    Literal(String),
    Number(f64),
    Variable(String),
    Call(String, Vec<Expr>),
    /// A location path; `absolute` starts at the document root.
    Path {
        absolute: bool,
        steps: Vec<Step>,
    },
    /// A primary expression with predicates, optionally followed by a
    /// relative path (`(//a)[1]/@href`, `id('x')//p`).
    Filter {
        primary: Box<Expr>,
        predicates: Vec<Expr>,
        steps: Vec<Step>,
    },
    /// Part of a predicate that walks the document and depends only on the
    /// node being tested, not its position or the size of the set, and
    /// whose value only matters as a number or a yes or no. The evaluator
    /// keeps that for each node in the numbered slot for the rest of the
    /// run. Made by [`memoize`].
    Memo(Box<Expr>, usize),
}

pub(crate) fn parse(src: &str, tokens: Vec<Token>) -> Result<Expr, XPathError> {
    let mut p = Parser {
        src,
        tokens,
        pos: 0,
        depth: 0,
    };
    if p.tokens.is_empty() {
        return Err(p.error("empty expression"));
    }
    let expr = p.expr()?;
    if p.pos < p.tokens.len() {
        return Err(p.error(&format!(
            "unexpected {} after a complete expression",
            p.tokens[p.pos].describe()
        )));
    }
    Ok(expr)
}

struct Parser<'s> {
    src: &'s str,
    tokens: Vec<Token>,
    pos: usize,
    /// How deep the tree being built goes: brackets, predicates, function
    /// arguments, negations and operator chains. Evaluating and dropping
    /// the tree recurse that deep, so it is capped before the tree exists.
    depth: usize,
}

/// Far beyond any real query, and far inside a thread's stack.
const MAX_DEPTH: usize = 256;

const NODE_TYPES: [&str; 4] = ["comment", "text", "processing-instruction", "node"];

impl Parser<'_> {
    fn error(&self, reason: &str) -> XPathError {
        XPathError {
            query: self.src.to_string(),
            reason: reason.to_string(),
        }
    }

    fn peek(&self) -> Option<&Token> {
        self.tokens.get(self.pos)
    }

    fn peek_at(&self, n: usize) -> Option<&Token> {
        self.tokens.get(self.pos + n)
    }

    fn eat(&mut self, t: &Token) -> bool {
        if self.peek() == Some(t) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, t: Token, what: &str) -> Result<(), XPathError> {
        if self.eat(&t) {
            Ok(())
        } else {
            Err(self.error(&format!("expected {what}")))
        }
    }

    fn deeper(&mut self) -> Result<(), XPathError> {
        self.depth += 1;
        if self.depth > MAX_DEPTH {
            return Err(self.error(&format!(
                "expression nested too deeply (over {MAX_DEPTH} levels)"
            )));
        }
        Ok(())
    }

    fn expr(&mut self) -> Result<Expr, XPathError> {
        self.deeper()?;
        let e = stacker::maybe_grow(64 * 1024, 1024 * 1024, || self.binary(0));
        self.depth -= 1;
        e
    }

    /// Precedence climbing over the binary operators, loosest first.
    fn binary(&mut self, level: usize) -> Result<Expr, XPathError> {
        const LEVELS: &[&[(Token, Op)]] = &[
            &[(Token::Or, Op::Or)],
            &[(Token::And, Op::And)],
            &[(Token::Eq, Op::Eq), (Token::Neq, Op::Neq)],
            &[
                (Token::Lt, Op::Lt),
                (Token::Le, Op::Le),
                (Token::Gt, Op::Gt),
                (Token::Ge, Op::Ge),
            ],
            &[(Token::Plus, Op::Add), (Token::Minus, Op::Sub)],
            &[
                (Token::Multiply, Op::Mul),
                (Token::Div, Op::Div),
                (Token::Mod, Op::Mod),
            ],
        ];
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut left = self.binary(level + 1)?;
        // Each operator puts the chain so far one level further down.
        let mut chain = 0;
        let result = 'outer: loop {
            for (token, op) in LEVELS[level] {
                if self.eat(token) {
                    chain += 1;
                    if let Err(e) = self.deeper() {
                        break 'outer Err(e);
                    }
                    let right = match self.binary(level + 1) {
                        Ok(r) => r,
                        Err(e) => break 'outer Err(e),
                    };
                    left = Expr::Binary(*op, Box::new(left), Box::new(right));
                    continue 'outer;
                }
            }
            break Ok(left);
        };
        self.depth -= chain;
        result
    }

    fn unary(&mut self) -> Result<Expr, XPathError> {
        if self.eat(&Token::Minus) {
            self.deeper()?;
            let inner = stacker::maybe_grow(64 * 1024, 1024 * 1024, || self.unary());
            self.depth -= 1;
            return Ok(Expr::Negate(Box::new(inner?)));
        }
        let mut left = self.path_expr()?;
        let mut chain = 0;
        let mut result = Ok(());
        while self.eat(&Token::Pipe) {
            chain += 1;
            result = self.deeper();
            if result.is_err() {
                break;
            }
            match self.path_expr() {
                Ok(right) => left = Expr::Union(Box::new(left), Box::new(right)),
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        self.depth -= chain;
        result.map(|()| left)
    }

    fn starts_filter(&self) -> bool {
        match self.peek() {
            Some(Token::Literal(_) | Token::Number(_) | Token::Variable(_) | Token::LParen) => true,
            Some(Token::Name(name)) => {
                self.peek_at(1) == Some(&Token::LParen) && !NODE_TYPES.contains(&name.as_str())
            }
            _ => false,
        }
    }

    fn path_expr(&mut self) -> Result<Expr, XPathError> {
        if !self.starts_filter() {
            return self.location_path();
        }
        let primary = self.primary()?;
        let predicates = self.predicates()?;
        let mut steps = Vec::new();
        if matches!(self.peek(), Some(Token::Slash | Token::DoubleSlash)) {
            steps = self.relative_steps(true)?;
        }
        if predicates.is_empty() && steps.is_empty() {
            return Ok(primary);
        }
        Ok(Expr::Filter {
            primary: Box::new(primary),
            predicates,
            steps,
        })
    }

    fn primary(&mut self) -> Result<Expr, XPathError> {
        match self.peek().cloned() {
            Some(Token::Literal(s)) => {
                self.pos += 1;
                Ok(Expr::Literal(s))
            }
            Some(Token::Number(n)) => {
                self.pos += 1;
                Ok(Expr::Number(n))
            }
            Some(Token::Variable(v)) => {
                self.pos += 1;
                Ok(Expr::Variable(v))
            }
            Some(Token::LParen) => {
                self.pos += 1;
                let e = self.expr()?;
                self.expect(Token::RParen, "\")\"")?;
                Ok(e)
            }
            Some(Token::Name(name)) => {
                self.pos += 2; // name and "("
                let mut args = Vec::new();
                if !self.eat(&Token::RParen) {
                    loop {
                        args.push(self.expr()?);
                        if self.eat(&Token::RParen) {
                            break;
                        }
                        self.expect(Token::Comma, "\",\" or \")\" in a function call")?;
                    }
                }
                Ok(Expr::Call(name, args))
            }
            None => Err(self.error("the expression ends where a value was expected")),
            Some(other) => {
                Err(self.error(&format!("expected a value, found {}", other.describe())))
            }
        }
    }

    fn predicates(&mut self) -> Result<Vec<Expr>, XPathError> {
        let mut out = Vec::new();
        while self.eat(&Token::LBracket) {
            out.push(self.expr()?);
            self.expect(Token::RBracket, "\"]\" to close a predicate")?;
        }
        Ok(out)
    }

    fn location_path(&mut self) -> Result<Expr, XPathError> {
        match self.peek() {
            Some(Token::Slash) => {
                self.pos += 1;
                // "/" alone is the root; "/" followed by a step is a path.
                let steps = if self.starts_step() {
                    self.relative_steps(false)?
                } else {
                    Vec::new()
                };
                Ok(Expr::Path {
                    absolute: true,
                    steps,
                })
            }
            Some(Token::DoubleSlash) => Ok(Expr::Path {
                absolute: true,
                steps: self.relative_steps(true)?,
            }),
            _ => Ok(Expr::Path {
                absolute: false,
                steps: self.relative_steps(false)?,
            }),
        }
    }

    fn starts_step(&self) -> bool {
        matches!(
            self.peek(),
            Some(Token::Dot | Token::DotDot | Token::At | Token::Name(_))
        )
    }

    /// Steps of a relative path. With `leading_separator`, the first token
    /// is a `/` or `//` that joins the steps to what came before.
    fn relative_steps(&mut self, leading_separator: bool) -> Result<Vec<Step>, XPathError> {
        let mut steps = Vec::new();
        let mut separator = if leading_separator {
            self.separator()
        } else {
            Some(false)
        };
        while let Some(double) = separator {
            let mut step = self.step()?;
            if double {
                // "//x" is descendant-or-self::node()/child::x, which without
                // predicates is descendant::x: one scan instead of one per node.
                if step.axis == Axis::Child && step.predicates.is_empty() {
                    step.axis = Axis::Descendant;
                } else {
                    steps.push(Step {
                        axis: Axis::DescendantOrSelf,
                        test: NodeTest::Node,
                        predicates: Vec::new(),
                    });
                }
            }
            steps.push(step);
            separator = self.separator();
        }
        Ok(steps)
    }

    /// `Some(false)` for `/`, `Some(true)` for `//`, `None` otherwise.
    fn separator(&mut self) -> Option<bool> {
        if self.eat(&Token::Slash) {
            Some(false)
        } else if self.eat(&Token::DoubleSlash) {
            Some(true)
        } else {
            None
        }
    }

    fn step(&mut self) -> Result<Step, XPathError> {
        if self.eat(&Token::Dot) {
            return Ok(Step {
                axis: Axis::Itself,
                test: NodeTest::Node,
                predicates: Vec::new(),
            });
        }
        if self.eat(&Token::DotDot) {
            return Ok(Step {
                axis: Axis::Parent,
                test: NodeTest::Node,
                predicates: Vec::new(),
            });
        }
        let axis = if self.eat(&Token::At) {
            Axis::Attribute
        } else if let (Some(Token::Name(name)), Some(Token::ColonColon)) =
            (self.peek(), self.peek_at(1))
        {
            let axis =
                Axis::named(name).ok_or_else(|| self.error(&format!("unknown axis {name:?}")))?;
            self.pos += 2;
            axis
        } else {
            Axis::Child
        };
        let test = self.node_test()?;
        let predicates = self.predicates()?;
        Ok(Step {
            axis,
            test,
            predicates,
        })
    }

    fn node_test(&mut self) -> Result<NodeTest, XPathError> {
        let name = match self.peek().cloned() {
            Some(Token::Name(name)) => name,
            None => return Err(self.error("the expression ends where a step was expected")),
            Some(other) => {
                return Err(self.error(&format!(
                    "expected a node test (a name, *, text(), node() ...), found {}",
                    other.describe()
                )));
            }
        };
        self.pos += 1;
        if NODE_TYPES.contains(&name.as_str()) && self.eat(&Token::LParen) {
            let test = match name.as_str() {
                "comment" => NodeTest::Comment,
                "text" => NodeTest::Text,
                "node" => NodeTest::Node,
                _ => {
                    let target = match self.peek().cloned() {
                        Some(Token::Literal(t)) => {
                            self.pos += 1;
                            Some(t)
                        }
                        _ => None,
                    };
                    NodeTest::ProcessingInstruction(target)
                }
            };
            self.expect(Token::RParen, "\")\" after a node type")?;
            return Ok(test);
        }
        if name == "*" {
            return Ok(NodeTest::Any);
        }
        if let Some((prefix, local)) = name.split_once(':') {
            return Ok(if local == "*" {
                NodeTest::AnyIn(prefix.to_string())
            } else {
                NodeTest::Prefixed(prefix.to_string(), local.to_string())
            });
        }
        Ok(NodeTest::Name(name))
    }
}

impl Expr {
    /// Every `$variable` the expression uses.
    pub(crate) fn variables(&self, out: &mut Vec<String>) {
        let steps = |steps: &[Step], out: &mut Vec<String>| {
            for p in steps.iter().flat_map(|s| &s.predicates) {
                p.variables(out);
            }
        };
        match self {
            Expr::Variable(v) => out.push(v.clone()),
            Expr::Binary(_, a, b) | Expr::Union(a, b) => {
                a.variables(out);
                b.variables(out);
            }
            Expr::Negate(e) => e.variables(out),
            Expr::Call(_, args) => args.iter().for_each(|a| a.variables(out)),
            Expr::Path { steps: s, .. } => steps(s, out),
            Expr::Filter {
                primary,
                predicates,
                steps: s,
            } => {
                primary.variables(out);
                predicates.iter().for_each(|p| p.variables(out));
                steps(s, out);
            }
            Expr::Memo(e, _) => e.variables(out),
            Expr::Literal(_) | Expr::Number(_) => {}
        }
    }

    /// True when the value reads the position or size of the set being
    /// filtered. Predicates inside a path or filter have a context of their
    /// own, so what they read doesn't count.
    fn reads_position(&self) -> bool {
        match self {
            Expr::Call(name, args) => {
                matches!(name.as_str(), "position" | "last")
                    || args.iter().any(Expr::reads_position)
            }
            Expr::Negate(e) | Expr::Memo(e, _) => e.reads_position(),
            Expr::Binary(_, a, b) | Expr::Union(a, b) => a.reads_position() || b.reads_position(),
            Expr::Filter { primary, .. } => primary.reads_position(),
            Expr::Literal(_) | Expr::Number(_) | Expr::Variable(_) | Expr::Path { .. } => false,
        }
    }

    /// True when working the expression out walks the document beyond the
    /// node itself and its attributes, which is what makes testing a node
    /// twice expensive.
    fn walks(&self) -> bool {
        match self {
            Expr::Path { absolute, steps } => {
                *absolute
                    || steps.iter().any(|s| {
                        !matches!(s.axis, Axis::Itself | Axis::Attribute)
                            || s.predicates.iter().any(Expr::walks)
                    })
            }
            Expr::Filter { .. } => true,
            Expr::Literal(_) | Expr::Number(_) | Expr::Variable(_) => false,
            Expr::Negate(e) | Expr::Memo(e, _) => e.walks(),
            Expr::Binary(_, a, b) | Expr::Union(a, b) => a.walks() || b.walks(),
            Expr::Call(_, args) => args.iter().any(Expr::walks),
        }
    }
}

/// Wraps in [`Expr::Memo`] every predicate that walks the document without
/// reading its position. In one that does read it, wraps the parts that
/// walk without reading it and give a yes or no or a number: `and`/`or`
/// operands, arguments to `not()` and `boolean()`, and calls such as
/// `count()`. A predicate tests the same node many times when predicates
/// nest: in `//*[..//*[..//*[.]]]` the inner one sees every node once for
/// each node the outer one tests, and the work grew as nodes^depth.
///
/// A node-set compared with the position, as in `[..//x = position()]`, is
/// still worked out every time: keeping node-sets for every node could
/// take memory growing as nodes squared.
///
/// `scalar` says which extension functions give only a number or a yes
/// or no.
pub(crate) fn memoize(expr: &mut Expr, scalar: &dyn Fn(&str) -> bool) {
    visit(expr, &mut Slots { next: 0, scalar });
}

struct Slots<'s> {
    next: usize,
    scalar: &'s dyn Fn(&str) -> bool,
}

fn visit(expr: &mut Expr, slots: &mut Slots<'_>) {
    match expr {
        Expr::Binary(_, a, b) | Expr::Union(a, b) => {
            visit(a, slots);
            visit(b, slots);
        }
        Expr::Negate(e) | Expr::Memo(e, _) => visit(e, slots),
        Expr::Call(_, args) => args.iter_mut().for_each(|a| visit(a, slots)),
        Expr::Path { steps, .. } => {
            for p in steps.iter_mut().flat_map(|s| &mut s.predicates) {
                predicate(p, slots);
            }
        }
        Expr::Filter {
            primary,
            predicates,
            steps,
        } => {
            visit(primary, slots);
            let in_steps = steps.iter_mut().flat_map(|s| &mut s.predicates);
            for p in predicates.iter_mut().chain(in_steps) {
                predicate(p, slots);
            }
        }
        Expr::Literal(_) | Expr::Number(_) | Expr::Variable(_) => {}
    }
}

fn predicate(p: &mut Expr, slots: &mut Slots<'_>) {
    visit(p, slots);
    keep_or_look_inside(p, slots);
}

fn keep_or_look_inside(e: &mut Expr, slots: &mut Slots<'_>) {
    if !e.walks() {
        return;
    }
    if !e.reads_position() {
        let inner = std::mem::replace(e, Expr::Number(0.0));
        *e = Expr::Memo(Box::new(inner), slots.next);
        slots.next += 1;
        return;
    }
    match e {
        Expr::Binary(Op::And | Op::Or, a, b) => {
            keep_or_look_inside(a, slots);
            keep_or_look_inside(b, slots);
        }
        Expr::Call(name, args) if matches!(name.as_str(), "not" | "boolean") => {
            args.iter_mut().for_each(|a| keep_or_look_inside(a, slots));
        }
        _ => scalar_calls(e, slots),
    }
}

/// Wraps the calls under `e` that walk without reading the position and
/// give a number or a yes or no.
fn scalar_calls(e: &mut Expr, slots: &mut Slots<'_>) {
    match e {
        Expr::Binary(_, a, b) | Expr::Union(a, b) => {
            scalar_calls(a, slots);
            scalar_calls(b, slots);
        }
        Expr::Negate(e) => scalar_calls(e, slots),
        Expr::Call(name, _) => {
            let scalar = matches!(
                name.as_str(),
                "count"
                    | "sum"
                    | "string-length"
                    | "number"
                    | "boolean"
                    | "not"
                    | "contains"
                    | "starts-with"
                    | "floor"
                    | "ceiling"
                    | "round"
            ) || (slots.scalar)(name);
            if scalar && e.walks() && !e.reads_position() {
                keep_or_look_inside(e, slots);
            } else if let Expr::Call(_, args) = e {
                args.iter_mut().for_each(|a| scalar_calls(a, slots));
            }
        }
        _ => {}
    }
}
