//! A small JSON value, reader and writer. Pressroom takes no dependencies, and the
//! local contract needs only objects, arrays, strings, numbers, booleans and null.

use std::fmt::Write as _;

#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Value>),
    Obj(Vec<(String, Value)>),
}

impl Value {
    pub fn get(&self, key: &str) -> Option<&Value> {
        match self {
            Value::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    pub fn str(&self, key: &str) -> Option<&str> {
        match self.get(key) {
            Some(Value::Str(s)) => Some(s),
            _ => None,
        }
    }
    pub fn num(&self, key: &str) -> Option<f64> {
        match self.get(key) {
            Some(Value::Num(n)) => Some(*n),
            _ => None,
        }
    }
    pub fn u64(&self, key: &str) -> Option<u64> {
        self.num(key)
            .filter(|n| *n >= 0.0 && n.fract() == 0.0 && *n < 9.0e15)
            .map(|n| n as u64)
    }
    pub fn arr(&self, key: &str) -> &[Value] {
        match self.get(key) {
            Some(Value::Arr(items)) => items,
            _ => &[],
        }
    }
    pub fn strings(&self, key: &str) -> Vec<String> {
        self.arr(key)
            .iter()
            .filter_map(|v| match v {
                Value::Str(s) => Some(s.clone()),
                _ => None,
            })
            .collect()
    }
}

pub fn obj(fields: Vec<(&str, Value)>) -> Value {
    Value::Obj(
        fields
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect(),
    )
}
pub fn s(text: impl Into<String>) -> Value {
    Value::Str(text.into())
}
pub fn n(number: impl Into<f64>) -> Value {
    Value::Num(number.into())
}
pub fn n64(number: u64) -> Value {
    Value::Num(number as f64)
}
pub fn strs(items: &[String]) -> Value {
    Value::Arr(items.iter().map(|i| Value::Str(i.clone())).collect())
}

/// One line, no trailing newline.
pub fn write(value: &Value) -> String {
    let mut out = String::new();
    write_into(value, &mut out);
    out
}

fn write_into(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Num(n) => {
            if !n.is_finite() {
                out.push_str("null");
            } else if n.fract() == 0.0 && n.abs() < 9.0e15 {
                let _ = write!(out, "{}", *n as i64);
            } else {
                let _ = write!(out, "{n}");
            }
        }
        Value::Str(text) => write_string(text, out),
        Value::Arr(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_into(item, out);
            }
            out.push(']');
        }
        Value::Obj(fields) => {
            out.push('{');
            for (i, (key, item)) in fields.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_into(item, out);
            }
            out.push('}');
        }
    }
}

fn write_string(text: &str, out: &mut String) {
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

const MAX_DEPTH: usize = 32;

pub fn parse(text: &str) -> Result<Value, String> {
    let mut p = Parser {
        bytes: text.as_bytes(),
        at: 0,
    };
    p.space();
    let value = p.value(0)?;
    p.space();
    if p.at != p.bytes.len() {
        return Err(format!("unexpected text at byte {}", p.at));
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl Parser<'_> {
    fn space(&mut self) {
        while matches!(self.bytes.get(self.at), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.at += 1;
        }
    }
    fn eat(&mut self, word: &str) -> bool {
        if self.bytes[self.at..].starts_with(word.as_bytes()) {
            self.at += word.len();
            true
        } else {
            false
        }
    }
    fn value(&mut self, depth: usize) -> Result<Value, String> {
        if depth > MAX_DEPTH {
            return Err("nested too deeply".into());
        }
        match self.bytes.get(self.at) {
            None => Err("unexpected end".into()),
            Some(b'{') => {
                self.at += 1;
                let mut fields = Vec::new();
                self.space();
                if self.eat("}") {
                    return Ok(Value::Obj(fields));
                }
                loop {
                    self.space();
                    let key = self.string()?;
                    self.space();
                    if !self.eat(":") {
                        return Err(format!("expected ':' at byte {}", self.at));
                    }
                    self.space();
                    let item = self.value(depth + 1)?;
                    if fields.iter().any(|(k, _)| *k == key) {
                        return Err(format!("the key {key:?} appears twice"));
                    }
                    fields.push((key, item));
                    self.space();
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("}") {
                        return Ok(Value::Obj(fields));
                    }
                    return Err(format!("expected ',' or '}}' at byte {}", self.at));
                }
            }
            Some(b'[') => {
                self.at += 1;
                let mut items = Vec::new();
                self.space();
                if self.eat("]") {
                    return Ok(Value::Arr(items));
                }
                loop {
                    self.space();
                    items.push(self.value(depth + 1)?);
                    self.space();
                    if self.eat(",") {
                        continue;
                    }
                    if self.eat("]") {
                        return Ok(Value::Arr(items));
                    }
                    return Err(format!("expected ',' or ']' at byte {}", self.at));
                }
            }
            Some(b'"') => Ok(Value::Str(self.string()?)),
            Some(b't') if self.eat("true") => Ok(Value::Bool(true)),
            Some(b'f') if self.eat("false") => Ok(Value::Bool(false)),
            Some(b'n') if self.eat("null") => Ok(Value::Null),
            Some(_) => {
                let start = self.at;
                while matches!(
                    self.bytes.get(self.at),
                    Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')
                ) {
                    self.at += 1;
                }
                let text = std::str::from_utf8(&self.bytes[start..self.at]).unwrap_or("");
                text.parse::<f64>()
                    .ok()
                    .filter(|n| n.is_finite())
                    .map(Value::Num)
                    .ok_or_else(|| format!("unexpected text at byte {start}"))
            }
        }
    }
    fn string(&mut self) -> Result<String, String> {
        if self.bytes.get(self.at) != Some(&b'"') {
            return Err(format!("expected a string at byte {}", self.at));
        }
        self.at += 1;
        let mut out = String::new();
        loop {
            let start = self.at;
            while !matches!(self.bytes.get(self.at), None | Some(b'"' | b'\\')) {
                self.at += 1;
            }
            out.push_str(
                std::str::from_utf8(&self.bytes[start..self.at]).map_err(|_| "bad utf-8")?,
            );
            match self.bytes.get(self.at) {
                None => return Err("unterminated string".into()),
                Some(b'"') => {
                    self.at += 1;
                    return Ok(out);
                }
                _ => {
                    self.at += 1;
                    let c = *self.bytes.get(self.at).ok_or("unterminated escape")?;
                    self.at += 1;
                    match c {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'b' => out.push('\u{8}'),
                        b'f' => out.push('\u{c}'),
                        b'u' => {
                            let hex = self
                                .bytes
                                .get(self.at..self.at + 4)
                                .and_then(|h| std::str::from_utf8(h).ok())
                                .and_then(|h| u32::from_str_radix(h, 16).ok())
                                .ok_or("bad \\u escape")?;
                            self.at += 4;
                            // Surrogate pairs are not needed by the contract; a lone
                            // surrogate becomes the replacement character.
                            out.push(char::from_u32(hex).unwrap_or('\u{fffd}'));
                        }
                        _ => return Err("bad escape".into()),
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let v = obj(vec![
            ("op", s("run")),
            ("threads", n(8)),
            ("half", n(0.5)),
            (
                "paths",
                Value::Arr(vec![s("/a \"b\"\n"), Value::Null, Value::Bool(true)]),
            ),
            ("inner", obj(vec![])),
        ]);
        let text = write(&v);
        assert!(!text.contains('\n'));
        assert_eq!(parse(&text).unwrap(), v);
    }

    #[test]
    fn refuses_rubbish() {
        for bad in [
            "", "{", "{\"a\":}", "[1,]", "\"open", "{} x", "nul", "1e999",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
        let deep = "[".repeat(100) + &"]".repeat(100);
        assert!(parse(&deep).is_err());
    }

    #[test]
    fn readers() {
        let v = parse(r#"{"a":"x","n":3,"f":1.5,"neg":-1,"l":["p","q",1]}"#).unwrap();
        assert_eq!(v.str("a"), Some("x"));
        assert_eq!(v.u64("n"), Some(3));
        assert_eq!(v.u64("f"), None);
        assert_eq!(v.u64("neg"), None);
        assert_eq!(v.strings("l"), vec!["p".to_string(), "q".to_string()]);
        assert_eq!(v.str("missing"), None);
    }
}
