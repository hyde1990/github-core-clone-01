//! Minimal JSON: a value type, a parser, and a serializer. No crates.
//!
//! A number that looks like an integer parses as `Int`; anything with a
//! fraction or an exponent parses as `Float`.
//!
//! Round 15 hardening (C3): both the parser and the serializer are
//! depth-capped at 128. A body nested deeper than that is rejected with
//! "JSON nesting too deep" instead of recursing until the stack dies, and
//! a value serialized past the cap emits `null` for the over-deep subtree
//! so the output stays valid JSON. Legitimate bodies nest at most ~6 deep.

/// The deepest nesting the parser and the serializer accept.
pub const MAX_JSON_DEPTH: usize = 128;

#[derive(Clone, Debug, PartialEq)]
pub enum Json {
    Null,
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

/// Build an object value from key/value pairs.
pub fn obj(fields: Vec<(&str, Json)>) -> Json {
    Json::Obj(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    )
}

impl Json {
    pub fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(flag) => Some(*flag),
            _ => None,
        }
    }

    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Json::Int(n) => Some(*n),
            Json::Float(f) => Some(*f as i64),
            _ => None,
        }
    }

    pub fn as_arr(&self) -> Option<&[Json]> {
        match self {
            Json::Arr(items) => Some(items),
            _ => None,
        }
    }

    /// Compact serialization. A subtree past `MAX_JSON_DEPTH` is written
    /// as `null` so the output stays valid JSON.
    pub fn stringify(&self) -> String {
        let mut out = String::new();
        self.write(&mut out, 0);
        out
    }

    fn write(&self, out: &mut String, depth: usize) {
        match self {
            Json::Null => out.push_str("null"),
            Json::Bool(true) => out.push_str("true"),
            Json::Bool(false) => out.push_str("false"),
            Json::Int(n) => out.push_str(&n.to_string()),
            Json::Float(f) => {
                if f.is_finite() {
                    out.push_str(&format!("{f}"));
                } else {
                    out.push_str("null");
                }
            }
            Json::Str(text) => write_json_string(out, text),
            Json::Arr(items) => {
                if depth > MAX_JSON_DEPTH {
                    out.push_str("null");
                    return;
                }
                out.push('[');
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    item.write(out, depth + 1);
                }
                out.push(']');
            }
            Json::Obj(fields) => {
                if depth > MAX_JSON_DEPTH {
                    out.push_str("null");
                    return;
                }
                out.push('{');
                for (i, (key, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write_json_string(out, key);
                    out.push(':');
                    value.write(out, depth + 1);
                }
                out.push('}');
            }
        }
    }
}

fn write_json_string(out: &mut String, text: &str) {
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{0008}' => out.push_str("\\b"),
            '\u{000C}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
}

/// Parse a JSON document. Errors are plain text; callers map them to 400.
/// A document nested deeper than `MAX_JSON_DEPTH` is rejected.
pub fn parse(text: &str) -> Result<Json, String> {
    let mut parser = Parser {
        bytes: text.as_bytes(),
        pos: 0,
    };
    parser.skip_ws();
    let value = parser.value(0)?;
    parser.skip_ws();
    if parser.pos != parser.bytes.len() {
        return Err("trailing characters".to_string());
    }
    Ok(value)
}

struct Parser<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Parser<'a> {
    fn skip_ws(&mut self) {
        while self.pos < self.bytes.len()
            && matches!(self.bytes[self.pos], b' ' | b'\t' | b'\n' | b'\r')
        {
            self.pos += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn expect(&mut self, byte: u8) -> Result<(), String> {
        if self.peek() == Some(byte) {
            self.pos += 1;
            Ok(())
        } else {
            Err(format!("expected '{}'", byte as char))
        }
    }

    fn value(&mut self, depth: usize) -> Result<Json, String> {
        if depth > MAX_JSON_DEPTH {
            return Err("JSON nesting too deep".to_string());
        }
        match self.peek() {
            Some(b'{') => self.object(depth),
            Some(b'[') => self.array(depth),
            Some(b'"') => Ok(Json::Str(self.string()?)),
            Some(b't') => self.literal("true", Json::Bool(true)),
            Some(b'f') => self.literal("false", Json::Bool(false)),
            Some(b'n') => self.literal("null", Json::Null),
            Some(b'-') | Some(b'0'..=b'9') => self.number(),
            Some(c) => Err(format!("unexpected character '{}'", c as char)),
            None => Err("unexpected end of input".to_string()),
        }
    }

    fn literal(&mut self, word: &str, value: Json) -> Result<Json, String> {
        if self.bytes.len() - self.pos >= word.len()
            && &self.bytes[self.pos..self.pos + word.len()] == word.as_bytes()
        {
            self.pos += word.len();
            Ok(value)
        } else {
            Err(format!("bad literal, expected {word}"))
        }
    }

    fn object(&mut self, depth: usize) -> Result<Json, String> {
        self.expect(b'{')?;
        let mut fields = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b'}') {
            self.pos += 1;
            return Ok(Json::Obj(fields));
        }
        loop {
            self.skip_ws();
            if self.peek() != Some(b'"') {
                return Err("expected object key".to_string());
            }
            let key = self.string()?;
            self.skip_ws();
            self.expect(b':')?;
            self.skip_ws();
            let value = self.value(depth + 1)?;
            fields.push((key, value));
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b'}') => {
                    self.pos += 1;
                    return Ok(Json::Obj(fields));
                }
                _ => return Err("expected ',' or '}' in object".to_string()),
            }
        }
    }

    fn array(&mut self, depth: usize) -> Result<Json, String> {
        self.expect(b'[')?;
        let mut items = Vec::new();
        self.skip_ws();
        if self.peek() == Some(b']') {
            self.pos += 1;
            return Ok(Json::Arr(items));
        }
        loop {
            self.skip_ws();
            items.push(self.value(depth + 1)?);
            self.skip_ws();
            match self.peek() {
                Some(b',') => self.pos += 1,
                Some(b']') => {
                    self.pos += 1;
                    return Ok(Json::Arr(items));
                }
                _ => return Err("expected ',' or ']' in array".to_string()),
            }
        }
    }

    fn string(&mut self) -> Result<String, String> {
        self.expect(b'"')?;
        let mut out = String::new();
        loop {
            let Some(byte) = self.peek() else {
                return Err("unterminated string".to_string());
            };
            match byte {
                b'"' => {
                    self.pos += 1;
                    return Ok(out);
                }
                b'\\' => {
                    self.pos += 1;
                    let escaped = self.peek().ok_or("unterminated escape")?;
                    self.pos += 1;
                    match escaped {
                        b'"' => out.push('"'),
                        b'\\' => out.push('\\'),
                        b'/' => out.push('/'),
                        b'b' => out.push('\u{0008}'),
                        b'f' => out.push('\u{000C}'),
                        b'n' => out.push('\n'),
                        b'r' => out.push('\r'),
                        b't' => out.push('\t'),
                        b'u' => {
                            let code = self.hex4()?;
                            if (0xD800..0xDC00).contains(&code) {
                                // High surrogate: a low surrogate must follow.
                                if self.peek() == Some(b'\\')
                                    && self.bytes.get(self.pos + 1) == Some(&b'u')
                                {
                                    self.pos += 2;
                                    let low = self.hex4()?;
                                    if (0xDC00..0xE000).contains(&low) {
                                        let combined =
                                            0x10000 + ((code - 0xD800) << 10) + (low - 0xDC00);
                                        out.push(
                                            char::from_u32(combined).ok_or("bad surrogate pair")?,
                                        );
                                    } else {
                                        return Err("bad low surrogate".to_string());
                                    }
                                } else {
                                    return Err("unpaired surrogate".to_string());
                                }
                            } else {
                                out.push(char::from_u32(code).ok_or("bad \\u escape")?);
                            }
                        }
                        _ => return Err("bad string escape".to_string()),
                    }
                }
                _ => {
                    let width = utf8_width(byte);
                    if self.pos + width > self.bytes.len() {
                        return Err("unterminated string".to_string());
                    }
                    let slice = std::str::from_utf8(&self.bytes[self.pos..self.pos + width])
                        .map_err(|_| "bad utf-8 in string".to_string())?;
                    out.push_str(slice);
                    self.pos += width;
                }
            }
        }
    }

    fn hex4(&mut self) -> Result<u32, String> {
        if self.pos + 4 > self.bytes.len() {
            return Err("bad \\u escape".to_string());
        }
        let text = std::str::from_utf8(&self.bytes[self.pos..self.pos + 4])
            .map_err(|_| "bad \\u escape".to_string())?;
        let value = u32::from_str_radix(text, 16).map_err(|_| "bad \\u escape".to_string())?;
        self.pos += 4;
        Ok(value)
    }

    fn number(&mut self) -> Result<Json, String> {
        let start = self.pos;
        if self.peek() == Some(b'-') {
            self.pos += 1;
        }
        let mut is_float = false;
        while let Some(c) = self.peek() {
            match c {
                b'0'..=b'9' => self.pos += 1,
                b'.' | b'e' | b'E' | b'+' | b'-' => {
                    is_float = true;
                    self.pos += 1;
                }
                _ => break,
            }
        }
        let text = std::str::from_utf8(&self.bytes[start..self.pos])
            .map_err(|_| "bad number".to_string())?;
        if is_float {
            text.parse::<f64>()
                .map(Json::Float)
                .map_err(|_| "bad number".to_string())
        } else {
            text.parse::<i64>()
                .map(Json::Int)
                .map_err(|_| "bad number".to_string())
        }
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
    fn roundtrip() {
        let text = r#"{"a":[1,2.5,"x\n\u00e9",true,false,null],"b":{"c":{}}}"#;
        let value = parse(text).unwrap();
        assert_eq!(value.get("a").unwrap().as_arr().unwrap().len(), 6);
        assert_eq!(
            value.stringify(),
            r#"{"a":[1,2.5,"x\né",true,false,null],"b":{"c":{}}}"#
        );
    }

    #[test]
    fn rejects_bad_input() {
        assert!(parse("").is_err());
        assert!(parse("{").is_err());
        assert!(parse("[1,]").is_err());
        assert!(parse("{\"a\":}").is_err());
        assert!(parse("01x").is_err());
    }

    #[test]
    fn escapes() {
        assert_eq!(
            parse(r#""a\"b\\c\/d""#).unwrap().as_str().unwrap(),
            "a\"b\\c/d"
        );
        assert_eq!(
            parse(r#""\ud83d\ude00""#).unwrap().as_str().unwrap(),
            "\u{1F600}"
        );
    }

    #[test]
    fn rejects_deep_nesting() {
        // 200 nested arrays: must fail fast instead of recursing to a crash.
        let deep = "[".repeat(200) + &"]".repeat(200);
        let err = parse(&deep).unwrap_err();
        assert!(err.contains("too deep"), "got: {err}");
        // 200 nested objects likewise.
        let mut obj = String::new();
        for _ in 0..200 {
            obj.push_str("{\"a\":");
        }
        obj.push('1');
        for _ in 0..200 {
            obj.push('}');
        }
        assert!(parse(&obj).is_err());
        // Shallow nesting still parses.
        assert!(parse("[[[[1]]]]").is_ok());
    }

    #[test]
    fn stringify_caps_deep_values() {
        // A 130-deep value serializes to valid JSON with the over-deep
        // subtree replaced by null (it cannot round-trip through the
        // 128-deep parser, by design).
        let mut value = Json::Int(1);
        for _ in 0..130 {
            value = Json::Arr(vec![value]);
        }
        let text = value.stringify();
        assert!(text.contains("null"));
        // The output is still balanced JSON.
        let mut depth = 0i32;
        for ch in text.chars() {
            match ch {
                '[' => depth += 1,
                ']' => depth -= 1,
                _ => {}
            }
        }
        assert_eq!(depth, 0);
    }
}
