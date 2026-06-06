use std::collections::HashSet;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) enum AggOp {
    Count,
    Sum,
    Avg,
    Min,
    Max,
    Stddev,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct AggFunc {
    pub(crate) op: AggOp,
    pub(crate) field: Option<String>,
}

#[derive(Clone, Debug)]
pub(crate) enum MetricExpr {
    Number(f64),
    Agg(AggFunc),
    Func { name: String, args: Vec<MetricExpr> },
    Add(Box<MetricExpr>, Box<MetricExpr>),
    Sub(Box<MetricExpr>, Box<MetricExpr>),
    Mul(Box<MetricExpr>, Box<MetricExpr>),
    Div(Box<MetricExpr>, Box<MetricExpr>),
    Neg(Box<MetricExpr>),
}

#[derive(Clone, Debug, PartialEq)]
enum Token {
    Number(f64),
    Ident(String),
    Plus,
    Minus,
    Star,
    Slash,
    LParen,
    RParen,
    Comma,
    End,
}

fn tokenize(input: &str) -> Result<Vec<Token>, String> {
    let mut chars = input.chars().peekable();
    let mut tokens = Vec::new();
    while let Some(&ch) = chars.peek() {
        if ch.is_whitespace() {
            chars.next();
            continue;
        }
        if ch.is_ascii_digit()
            || (ch == '.' && chars.clone().nth(1).is_some_and(|c| c.is_ascii_digit()))
        {
            let mut buf = String::new();
            let mut seen_dot = false;
            while let Some(&c) = chars.peek() {
                if c.is_ascii_digit() {
                    buf.push(c);
                    chars.next();
                } else if c == '.' && !seen_dot {
                    seen_dot = true;
                    buf.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            let number = buf
                .parse::<f64>()
                .map_err(|_| format!("Invalid number '{buf}'"))?;
            tokens.push(Token::Number(number));
            continue;
        }
        if ch.is_ascii_alphabetic() || ch == '_' {
            let mut buf = String::new();
            while let Some(&c) = chars.peek() {
                if c.is_ascii_alphanumeric() || c == '_' || c == '.' {
                    buf.push(c);
                    chars.next();
                } else {
                    break;
                }
            }
            tokens.push(Token::Ident(buf));
            continue;
        }
        match ch {
            '+' => {
                chars.next();
                tokens.push(Token::Plus);
            }
            '-' => {
                chars.next();
                tokens.push(Token::Minus);
            }
            '*' => {
                chars.next();
                tokens.push(Token::Star);
            }
            '/' => {
                chars.next();
                tokens.push(Token::Slash);
            }
            '(' => {
                chars.next();
                tokens.push(Token::LParen);
            }
            ')' => {
                chars.next();
                tokens.push(Token::RParen);
            }
            ',' => {
                chars.next();
                tokens.push(Token::Comma);
            }
            _ => {
                return Err(format!("Unexpected character '{ch}' in metric formula."));
            }
        }
    }
    tokens.push(Token::End);
    Ok(tokens)
}

struct Parser {
    tokens: Vec<Token>,
    pos: usize,
}

impl Parser {
    fn new(tokens: Vec<Token>) -> Self {
        Self { tokens, pos: 0 }
    }

    fn peek(&self) -> &Token {
        self.tokens.get(self.pos).unwrap_or(&Token::End)
    }

    fn next(&mut self) -> Token {
        let tok = self.tokens.get(self.pos).cloned().unwrap_or(Token::End);
        self.pos += 1;
        tok
    }

    fn expect(&mut self, expected: Token) -> Result<(), String> {
        let tok = self.next();
        if tok == expected {
            Ok(())
        } else {
            Err(format!("Expected {:?}, found {:?}", expected, tok))
        }
    }

    fn parse_expr(&mut self) -> Result<MetricExpr, String> {
        let mut node = self.parse_term()?;
        loop {
            match self.peek() {
                Token::Plus => {
                    self.next();
                    let rhs = self.parse_term()?;
                    node = MetricExpr::Add(Box::new(node), Box::new(rhs));
                }
                Token::Minus => {
                    self.next();
                    let rhs = self.parse_term()?;
                    node = MetricExpr::Sub(Box::new(node), Box::new(rhs));
                }
                _ => break,
            }
        }
        Ok(node)
    }

    fn parse_term(&mut self) -> Result<MetricExpr, String> {
        let mut node = self.parse_factor()?;
        loop {
            match self.peek() {
                Token::Star => {
                    self.next();
                    let rhs = self.parse_factor()?;
                    node = MetricExpr::Mul(Box::new(node), Box::new(rhs));
                }
                Token::Slash => {
                    self.next();
                    let rhs = self.parse_factor()?;
                    node = MetricExpr::Div(Box::new(node), Box::new(rhs));
                }
                _ => break,
            }
        }
        Ok(node)
    }

    fn parse_factor(&mut self) -> Result<MetricExpr, String> {
        match self.next() {
            Token::Number(n) => Ok(MetricExpr::Number(n)),
            Token::Minus => {
                let inner = self.parse_factor()?;
                Ok(MetricExpr::Neg(Box::new(inner)))
            }
            Token::Ident(name) => self.parse_function(name),
            Token::LParen => {
                let expr = self.parse_expr()?;
                self.expect(Token::RParen)?;
                Ok(expr)
            }
            other => Err(format!("Unexpected token {:?} in metric formula.", other)),
        }
    }

    fn parse_function(&mut self, name: String) -> Result<MetricExpr, String> {
        if !matches!(self.peek(), Token::LParen) {
            return Err(format!(
                "Unexpected identifier '{name}' without function call."
            ));
        }
        self.expect(Token::LParen)?;
        let lower = name.to_ascii_lowercase();
        let is_agg = matches!(
            lower.as_str(),
            "count"
                | "sum"
                | "avg"
                | "average"
                | "mean"
                | "min"
                | "max"
                | "stddev"
                | "stdev"
                | "std"
        );

        if is_agg {
            match self.peek() {
                Token::RParen => {
                    self.next();
                    if lower != "count" {
                        return Err(format!(
                            "Metric function '{name}' requires a field argument."
                        ));
                    }
                    return Ok(MetricExpr::Agg(AggFunc {
                        op: AggOp::Count,
                        field: None,
                    }));
                }
                Token::Ident(field) => {
                    if matches!(self.tokens.get(self.pos + 1), Some(Token::RParen)) {
                        let field = field.clone();
                        self.next();
                        self.expect(Token::RParen)?;
                        let op = match lower.as_str() {
                            "count" => AggOp::Count,
                            "sum" => AggOp::Sum,
                            "avg" | "average" | "mean" => AggOp::Avg,
                            "min" => AggOp::Min,
                            "max" => AggOp::Max,
                            "stddev" | "stdev" | "std" => AggOp::Stddev,
                            _ => {
                                return Err(format!("Unsupported metric function '{name}'."));
                            }
                        };
                        let field = if op == AggOp::Count {
                            None
                        } else {
                            Some(field)
                        };
                        return Ok(MetricExpr::Agg(AggFunc { op, field }));
                    }
                }
                _ => {}
            }
        }

        if matches!(self.peek(), Token::RParen) {
            self.next();
            return Err(format!(
                "Metric function '{name}' requires at least one argument."
            ));
        }
        let mut args = Vec::new();
        loop {
            let expr = self.parse_expr()?;
            args.push(expr);
            match self.peek() {
                Token::Comma => {
                    self.next();
                }
                Token::RParen => {
                    self.next();
                    break;
                }
                other => {
                    return Err(format!(
                        "Expected ',' or ')' after function argument, found {:?}.",
                        other
                    ));
                }
            }
        }
        let allowed = [
            "abs", "ceil", "floor", "round", "sqrt", "log", "ln", "exp", "min", "max", "clamp",
            "coalesce", "ifnull", "safe_div",
        ];
        if !allowed.iter().any(|f| f == &lower) {
            return Err(format!("Unsupported metric function '{name}'."));
        }
        Ok(MetricExpr::Func { name: lower, args })
    }
}

const MAX_METRIC_EXPR_DEPTH: usize = 32;
const MAX_METRIC_EXPR_NODES: usize = 256;

pub(crate) fn parse_metric_formula(expr: &str) -> Result<MetricExpr, String> {
    let tokens = tokenize(expr)?;
    let mut parser = Parser::new(tokens);
    let node = parser.parse_expr()?;
    let (depth, nodes) = metric_expr_stats(&node);
    if depth > MAX_METRIC_EXPR_DEPTH {
        return Err(format!(
            "Metric formula is too deep (depth {depth}, limit {MAX_METRIC_EXPR_DEPTH})."
        ));
    }
    if nodes > MAX_METRIC_EXPR_NODES {
        return Err(format!(
            "Metric formula is too complex (nodes {nodes}, limit {MAX_METRIC_EXPR_NODES})."
        ));
    }
    match parser.peek() {
        Token::End => Ok(node),
        other => Err(format!(
            "Unexpected trailing token {:?} in metric formula.",
            other
        )),
    }
}

fn metric_expr_stats(expr: &MetricExpr) -> (usize, usize) {
    fn walk(expr: &MetricExpr, depth: usize, max_depth: &mut usize, nodes: &mut usize) {
        *nodes += 1;
        if depth > *max_depth {
            *max_depth = depth;
        }
        match expr {
            MetricExpr::Number(_) | MetricExpr::Agg(_) => {}
            MetricExpr::Func { args, .. } => {
                for arg in args {
                    walk(arg, depth + 1, max_depth, nodes);
                }
            }
            MetricExpr::Add(a, b)
            | MetricExpr::Sub(a, b)
            | MetricExpr::Mul(a, b)
            | MetricExpr::Div(a, b) => {
                walk(a, depth + 1, max_depth, nodes);
                walk(b, depth + 1, max_depth, nodes);
            }
            MetricExpr::Neg(inner) => walk(inner, depth + 1, max_depth, nodes),
        }
    }
    let mut max_depth = 0usize;
    let mut nodes = 0usize;
    walk(expr, 1, &mut max_depth, &mut nodes);
    (max_depth, nodes)
}

pub(crate) fn collect_agg_funcs(expr: &MetricExpr) -> Vec<AggFunc> {
    fn walk(expr: &MetricExpr, out: &mut HashSet<AggFunc>) {
        match expr {
            MetricExpr::Number(_) => {}
            MetricExpr::Agg(func) => {
                out.insert(func.clone());
            }
            MetricExpr::Func { args, .. } => {
                for arg in args {
                    walk(arg, out);
                }
            }
            MetricExpr::Add(a, b)
            | MetricExpr::Sub(a, b)
            | MetricExpr::Mul(a, b)
            | MetricExpr::Div(a, b) => {
                walk(a, out);
                walk(b, out);
            }
            MetricExpr::Neg(inner) => walk(inner, out),
        }
    }
    let mut out = HashSet::new();
    walk(expr, &mut out);
    out.into_iter().collect()
}

pub(crate) fn eval_metric_expr<F>(expr: &MetricExpr, lookup: F) -> Option<f64>
where
    F: Fn(&AggFunc) -> Option<f64> + Copy,
{
    match expr {
        MetricExpr::Number(n) => Some(*n),
        MetricExpr::Agg(func) => lookup(func),
        MetricExpr::Func { name, args } => match name.as_str() {
            "abs" => {
                let v = eval_metric_expr(args.first()?, lookup)?;
                Some(v.abs())
            }
            "ceil" => {
                let v = eval_metric_expr(args.first()?, lookup)?;
                Some(v.ceil())
            }
            "floor" => {
                let v = eval_metric_expr(args.first()?, lookup)?;
                Some(v.floor())
            }
            "round" => {
                let v = eval_metric_expr(args.first()?, lookup)?;
                Some(v.round())
            }
            "sqrt" => {
                let v = eval_metric_expr(args.first()?, lookup)?;
                if v < 0.0 { None } else { Some(v.sqrt()) }
            }
            "log" | "ln" => {
                let v = eval_metric_expr(args.first()?, lookup)?;
                if v <= 0.0 { None } else { Some(v.ln()) }
            }
            "exp" => {
                let v = eval_metric_expr(args.first()?, lookup)?;
                Some(v.exp())
            }
            "min" => {
                let mut min = eval_metric_expr(args.first()?, lookup)?;
                for arg in args.iter().skip(1) {
                    let v = eval_metric_expr(arg, lookup)?;
                    if v < min {
                        min = v;
                    }
                }
                Some(min)
            }
            "max" => {
                let mut max = eval_metric_expr(args.first()?, lookup)?;
                for arg in args.iter().skip(1) {
                    let v = eval_metric_expr(arg, lookup)?;
                    if v > max {
                        max = v;
                    }
                }
                Some(max)
            }
            "clamp" => {
                if args.len() < 3 {
                    return None;
                }
                let v = eval_metric_expr(args.first()?, lookup)?;
                let lo = eval_metric_expr(args.get(1)?, lookup)?;
                let hi = eval_metric_expr(args.get(2)?, lookup)?;
                Some(v.max(lo).min(hi))
            }
            "coalesce" | "ifnull" => {
                for arg in args {
                    if let Some(v) = eval_metric_expr(arg, lookup) {
                        return Some(v);
                    }
                }
                None
            }
            "safe_div" => {
                if args.len() < 2 {
                    return None;
                }
                let num = eval_metric_expr(args.first()?, lookup)?;
                let denom = eval_metric_expr(args.get(1)?, lookup)?;
                if denom == 0.0 {
                    Some(0.0)
                } else {
                    Some(num / denom)
                }
            }
            _ => None,
        },
        MetricExpr::Add(a, b) => Some(eval_metric_expr(a, lookup)? + eval_metric_expr(b, lookup)?),
        MetricExpr::Sub(a, b) => Some(eval_metric_expr(a, lookup)? - eval_metric_expr(b, lookup)?),
        MetricExpr::Mul(a, b) => Some(eval_metric_expr(a, lookup)? * eval_metric_expr(b, lookup)?),
        MetricExpr::Div(a, b) => {
            let denom = eval_metric_expr(b, lookup)?;
            if denom == 0.0 {
                None
            } else {
                Some(eval_metric_expr(a, lookup)? / denom)
            }
        }
        MetricExpr::Neg(inner) => Some(-eval_metric_expr(inner, lookup)?),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_metric_formula_handles_arithmetic() {
        let expr = parse_metric_formula("1 - (sum(downtime) / sum(total))").expect("parse");
        let aggs = collect_agg_funcs(&expr);
        assert_eq!(aggs.len(), 2);
    }

    #[test]
    fn parse_metric_formula_handles_nested_functions() {
        let expr = parse_metric_formula("coalesce(max(sum(a), 1), 0)").expect("parse");
        let aggs = collect_agg_funcs(&expr);
        assert_eq!(aggs.len(), 1);
        assert!(aggs.iter().any(|agg| matches!(agg.op, AggOp::Sum)));
    }

    #[test]
    fn eval_metric_formula_supports_safe_div() {
        let expr = parse_metric_formula("coalesce(safe_div(sum(a), sum(b)), 0)").expect("parse");
        let lookup = |agg: &AggFunc| match (&agg.op, agg.field.as_deref()) {
            (AggOp::Sum, Some("a")) => Some(10.0),
            (AggOp::Sum, Some("b")) => Some(0.0),
            _ => None,
        };
        let value = eval_metric_expr(&expr, lookup);
        assert_eq!(value, Some(0.0));
    }
}
