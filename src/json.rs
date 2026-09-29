//! A deliberately tiny JSON parser/emitter.
//!
//! The engine persists plans and journals as JSON. Rather than pulling in an
//! external crate for a handful of schema needs, this module implements just
//! enough strict JSON (RFC 8259 subset: objects, arrays, strings, numbers,
//! booleans, null) with position-aware error messages.

use std::collections::BTreeMap;
use std::fmt::Write as _;

#[derive(Debug, Clone, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(BTreeMap<String, Json>),
}

impl Json {
    pub fn parse(input: &str) -> Result<Json, String> {
        let bytes = input.as_bytes();
        let mut p = Parser { bytes, pos: 0 };
        p.skip_ws();
        let value = p.parse_value()?;
        p.skip_ws();
        if p.pos != bytes.len() {
            return Err(p.err("trailing characters after JSON value"));
        }
        Ok(value)
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }

    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(o) => o.get(key),
            _ => None,
        }
    }

    pub fn emit(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    pub fn emit_pretty(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out.push('\n');
        out
    }

    fn write(&self, out: &mut String, indent: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            Json::Num(n) => {
                if n.fract() == 0.0 && n.is_finite() && n.abs() < 1e16 {
                    let _ = write!(out, "{}", *n as i64);
                } else {
                    let _ = write!(out, "{}", n);
                }
            }
            Json::Str(s) => {
                out.push('"');
                escape_into(s, out);
                out.push('"');
            }
            Json::Arr(items) => {
                if items.is_empty() {
                    out.push_str("[]");
                    return;
                }
                out.push_str("[\n");
                for (i, item) in items.iter().enumerate() {
                    push_indent(out, indent + 1);
                    item.write(out, indent + 1);
                    if i + 1 < items.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                push_indent(out, indent);
                out.push(']');
            }
            Json::Obj(map) => {
                if map.is_empty() {
                    out.push_str("{}");
                    return;
                }
                out.push_str("{\n");
                for (i, (key, value)) in map.iter().enumerate() {
                    push_indent(out, indent + 1);
                    out.push('"');
                    escape_into(key, out);
                    out.push_str("\": ");
                    value.write(out, indent + 1);
                    if i + 1 < map.len() {
                        out.push(',');
                    }
                    out.push('\n');
                }
                push_indent(out, indent);
                out.push('}');
            }
        }
    }
}

fn push_indent(out: &mut String, indent: usize) {
    for _ in 0..indent {
        out.push_str("  ");
    }
}

fn escape_into(s: &str, out: &mut String) {
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{08}' => out.push_str("\\b"),
            '\u{0c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
}

impl From<&str> for Json {
    fn from(s: &str) -> Json {
        Json::Str(s.to_string())
    }
}

impl From<String> for Json {
    fn from(s: String) -> Json {
        Json::Str(s)
    }
}

impl From<bool> for Json {
    fn from(b: bool) -> Json {
        Json::Bool(b)
    }
}

impl From<u64> for Json {
    fn from(n: u64) -> Json {
        Json::Num(n as f64)
    }
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn err(&self, msg: &str) -> String {
        format!("JSON 解析失败（位置 {}）：{}", self.pos, msg)
    }

    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len() {
            match self.bytes[self.pos] {
                b' ' | b'\t' | b'\n' | b'\r' => self.pos += 1,
                _ => break,
            }
        }
    }

    fn parse_value(&mut self) -> Result<Json, String> {
        self.skip_ws();
        if self.pos >= self.bytes.len() {
            return Err(self.err("意外的输入结束"));
        }
        match self.bytes[self.pos] {
            b'{' => self.parse_object(),
            b'[' => self.parse_array(),
            b'"' => self.parse_string().map(Json::Str),
            b't' | b'f' => self.parse_bool(),
            b'n' => self.parse_null(),
            b'-' | b'0'..=b'9' => self.parse_number(),
            other => Err(self.err(&format!("意外的字符 {:?}", other as char))),
        }
    }

    fn parse_object(&mut self) -> Result<Json, String> {
        self.pos += 1; // {
        let mut map = BTreeMap::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(map));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err(self.err("对象键必须是字符串"));
            }
            let key = self.parse_string()?;
            self.skip_ws();
            if self.next_byte() != Some(b':') {
                return Err(self.err("对象键后缺少 ':'"));
            }
            let value = self.parse_value()?;
            if map.insert(key, value).is_some() {
                return Err(self.err("对象中存在重复键"));
            }
            self.skip_ws();
            match self.next_byte() {
                Some(b',') => continue,
                Some(b'}') => break,
                _ => return Err(self.err("对象成员后应为 ',' 或 '}'")),
            }
        }
        Ok(Json::Obj(map))
    }

    fn parse_array(&mut self) -> Result<Json, String> {
        self.pos += 1; // [
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            items.push(self.parse_value()?);
            self.skip_ws();
            match self.next_byte() {
                Some(b',') => continue,
                Some(b']') => break,
                _ => return Err(self.err("数组元素后应为 ',' 或 ']'")),
            }
        }
        Ok(Json::Arr(items))
    }

    fn parse_string(&mut self) -> Result<String, String> {
        self.pos += 1; // opening quote
        let mut result = String::new();
        loop {
            match self.next_byte() {
                None => return Err(self.err("字符串未闭合")),
                Some(b'"') => break,
                Some(b'\\') => {
                    let escaped = self.next_byte().ok_or_else(|| self.err("不完整的转义"))?;
                    match escaped {
                        b'"' => result.push('"'),
                        b'\\' => result.push('\\'),
                        b'/' => result.push('/'),
                        b'n' => result.push('\n'),
                        b't' => result.push('\t'),
                        b'r' => result.push('\r'),
                        b'b' => result.push('\u{08}'),
                        b'f' => result.push('\u{0c}'),
                        b'u' => {
                            let cp = self.parse_unicode_escape()?;
                            if (0xD800..=0xDBFF).contains(&cp) {
                                // high surrogate; require low surrogate
                                if self.next_byte() != Some(b'\\') || self.next_byte() != Some(b'u')
                                {
                                    return Err(self.err("缺少低位代理项"));
                                }
                                let low = self.parse_unicode_escape()?;
                                if !(0xDC00..=0xDFFF).contains(&low) {
                                    return Err(self.err("低位代理项非法"));
                                }
                                let c = 0x10000
                                    + ((cp - 0xD800) << 10)
                                    + (low - 0xDC00);
                                if let Some(ch) = char::from_u32(c) {
                                    result.push(ch);
                                }
                            } else if let Some(ch) = char::from_u32(cp) {
                                result.push(ch);
                            } else {
                                return Err(self.err("非法的 Unicode 码点"));
                            }
                        }
                        other => {
                            return Err(self.err(&format!("非法转义 \\{}", other as char)))
                        }
                    }
                }
                Some(byte) => {
                    // Collect a full UTF-8 sequence (input is &str so it is valid UTF-8).
                    let width = utf8_width(byte);
                    let start = self.pos - 1;
                    let end = (start + width).min(self.bytes.len());
                    let slice = std::str::from_utf8(&self.bytes[start..end])
                        .map_err(|_| self.err("非法的 UTF-8 字节"))?;
                    result.push_str(slice);
                    self.pos = end;
                }
            }
        }
        Ok(result)
    }

    fn parse_unicode_escape(&mut self) -> Result<u32, String> {
        let mut value = 0u32;
        for _ in 0..4 {
            let b = self.next_byte().ok_or_else(|| self.err("不完整的 \\u 转义"))?;
            let digit = (b as char).to_digit(16).ok_or_else(|| self.err("\\u 后应为十六进制数字"))?;
            value = value * 16 + digit;
        }
        Ok(value)
    }

    fn parse_bool(&mut self) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(b"true") {
            self.pos += 4;
            Ok(Json::Bool(true))
        } else if self.bytes[self.pos..].starts_with(b"false") {
            self.pos += 5;
            Ok(Json::Bool(false))
        } else {
            Err(self.err("非法的字面量"))
        }
    }

    fn parse_null(&mut self) -> Result<Json, String> {
        if self.bytes[self.pos..].starts_with(b"null") {
            self.pos += 4;
            Ok(Json::Null)
        } else {
            Err(self.err("非法的字面量"))
        }
    }

    fn parse_number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        while let Some(b) = self.peek() {
            match b {
                b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-' => self.pos += 1,
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| self.err("非法数字"))?;
        text.parse::<f64>()
            .map(Json::Num)
            .map_err(|_| self.err(&format!("非法数字 {:?}", text)))
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn next_byte(&mut self) -> Option<u8> {
        let b = self.bytes.get(self.pos).copied();
        if b.is_some() {
            self.pos += 1;
        }
        b
    }
}

fn utf8_width(first: u8) -> usize {
    if first < 0x80 {
        1
    } else if first >> 5 == 0b110 {
        2
    } else if first >> 4 == 0b1110 {
        3
    } else {
        4
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_unicode_and_escapes() {
        let text = r#"{"msg":"中文 / emoji 😀","nl":"a\nb","u":"é"}"#;
        let parsed = Json::parse(text).unwrap();
        assert_eq!(parsed.get("msg").unwrap().as_str(), Some("中文 / emoji 😀"));
        assert_eq!(parsed.get("nl").unwrap().as_str(), Some("a\nb"));
        let again = Json::parse(&parsed.emit()).unwrap();
        assert_eq!(parsed, again);
    }

    #[test]
    fn rejects_trailing_and_duplicate_keys() {
        assert!(Json::parse("{}x").is_err());
        assert!(Json::parse(r#"{"a":1,"a":2}"#).is_err());
    }
}
