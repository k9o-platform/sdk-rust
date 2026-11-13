//! Moat: a local guard around a model call. No network, no account, no dependency on any other
//! namespace of this crate.
//!
//! Moat does four things in the developer's process:
//!
//! 1. It pins the system prompt. [`Moat::messages`] builds the message list itself, so untrusted
//!    input only ever becomes the `user` message, and input that carries a role or system marker
//!    is refused. The pin is structural: it stops injected text from becoming the system prompt,
//!    and it does not stop that text from reaching the model.
//! 2. It enforces a declared input schema before the input is placed in the message list.
//! 3. It enforces a declared output schema on what the model returns.
//! 4. It passes the output through an optional PII filter that you supply (for example a
//!    Barbican instance). Without a filter there is no PII step.
//!
//! A refusal is a value: every check returns `Result<_, `[`MoatRefusal`]`>` whose
//! [`MoatRefusal::reason`] is one of `role_marker`, `input_schema`, `output_schema` or `pii`.
//! A schema that cannot be used is a [`MoatConfigError`] at construction.
//!
//! ## Schema language
//!
//! A subset of JSON Schema, implemented here: `type` (`object`, `array`, `string`, `number`,
//! `integer`, `boolean`, `null`, or a list of them), `properties`, `required`,
//! `additionalProperties` (a boolean), `items` (one schema), `enum`, `const`, `minLength`,
//! `maxLength`, `pattern` (searched, not anchored; classes, groups, alternation and greedy or lazy
//! quantifiers, without look-around or back-references), `minimum`, `maximum`, `minItems` and
//! `maxItems`. Annotation keywords `title`, `description` and `$comment` are accepted and ignored.
//! Any other keyword is rejected at construction, so a typo never silently disables a check.

use crate::hash::{hex, sha256};
use crate::minire::Regex;
use serde::Serialize;
use serde_json::{Map, Value};
use std::fmt;
use std::sync::Arc;
use unicode_normalization::char::is_combining_mark;
use unicode_normalization::UnicodeNormalization;

/// One chat message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Message {
    /// `system` or `user`.
    pub role: String,
    pub content: String,
}

/// Why Moat refused. The set is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MoatReason {
    /// The input carries a role or system marker.
    RoleMarker,
    /// The input does not satisfy the input schema.
    InputSchema,
    /// The model output is not JSON or does not satisfy the output schema.
    OutputSchema,
    /// The PII filter flagged the output.
    Pii,
}

impl MoatReason {
    /// `role_marker`, `input_schema`, `output_schema` or `pii`.
    pub fn as_str(&self) -> &'static str {
        match self {
            MoatReason::RoleMarker => "role_marker",
            MoatReason::InputSchema => "input_schema",
            MoatReason::OutputSchema => "output_schema",
            MoatReason::Pii => "pii",
        }
    }
}

/// A refusal: the closed `reason` and a short `detail`. The detail never repeats the refused text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoatRefusal {
    pub reason: MoatReason,
    pub detail: String,
}

impl fmt::Display for MoatRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "moat refused ({}): {}",
            self.reason.as_str(),
            self.detail
        )
    }
}

impl std::error::Error for MoatRefusal {}

/// A schema or option that Moat cannot use; raised by [`Moat::new`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoatConfigError {
    pub message: String,
}

impl fmt::Display for MoatConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid moat configuration: {}", self.message)
    }
}

impl std::error::Error for MoatConfigError {}

/// A PII filter: gets the text to inspect, returns `Some(detail)` to refuse or `None` to pass.
/// Keep `detail` short and free of the matched text.
pub type PiiFilter = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// Options for [`Moat::new`].
#[derive(Clone)]
pub struct MoatOptions {
    /// The pinned system prompt.
    pub prompt: String,
    pub input_schema: Option<Value>,
    pub output_schema: Option<Value>,
    /// The optional PII filter.
    pub pii: Option<PiiFilter>,
}

impl MoatOptions {
    pub fn new(prompt: impl Into<String>) -> Self {
        Self {
            prompt: prompt.into(),
            input_schema: None,
            output_schema: None,
            pii: None,
        }
    }

    pub fn input_schema(mut self, schema: Value) -> Self {
        self.input_schema = Some(schema);
        self
    }

    pub fn output_schema(mut self, schema: Value) -> Self {
        self.output_schema = Some(schema);
        self
    }

    /// Set the PII filter; see [`PiiFilter`].
    pub fn pii<F>(mut self, filter: F) -> Self
    where
        F: Fn(&str) -> Option<String> + Send + Sync + 'static,
    {
        self.pii = Some(Arc::new(filter));
        self
    }
}

impl fmt::Debug for MoatOptions {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MoatOptions")
            .field("input_schema", &self.input_schema)
            .field("output_schema", &self.output_schema)
            .field("pii", &self.pii.as_ref().map(|_| "<filter>"))
            .finish_non_exhaustive()
    }
}

// ---------------------------------------------------------------------------------------------
// Schema
// ---------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Ty {
    Object,
    Array,
    String,
    Number,
    Integer,
    Boolean,
    Null,
}

impl Ty {
    fn parse(s: &str) -> Option<Ty> {
        Some(match s {
            "object" => Ty::Object,
            "array" => Ty::Array,
            "string" => Ty::String,
            "number" => Ty::Number,
            "integer" => Ty::Integer,
            "boolean" => Ty::Boolean,
            "null" => Ty::Null,
            _ => return None,
        })
    }

    fn name(&self) -> &'static str {
        match self {
            Ty::Object => "object",
            Ty::Array => "array",
            Ty::String => "string",
            Ty::Number => "number",
            Ty::Integer => "integer",
            Ty::Boolean => "boolean",
            Ty::Null => "null",
        }
    }

    fn matches(&self, v: &Value) -> bool {
        match (self, v) {
            (Ty::Object, Value::Object(_))
            | (Ty::Array, Value::Array(_))
            | (Ty::String, Value::String(_))
            | (Ty::Number, Value::Number(_))
            | (Ty::Boolean, Value::Bool(_))
            | (Ty::Null, Value::Null) => true,
            (Ty::Integer, Value::Number(n)) => {
                n.is_i64() || n.is_u64() || n.as_f64().map_or(false, |f| f.fract() == 0.0)
            }
            _ => false,
        }
    }
}

#[derive(Debug, Clone, Default)]
struct Schema {
    types: Option<Vec<Ty>>,
    properties: Vec<(String, Schema)>,
    required: Vec<String>,
    additional_properties: Option<bool>,
    items: Option<Box<Schema>>,
    enumeration: Option<Vec<Value>>,
    constant: Option<Value>,
    min_length: Option<u64>,
    max_length: Option<u64>,
    pattern: Option<Regex>,
    minimum: Option<f64>,
    maximum: Option<f64>,
    min_items: Option<u64>,
    max_items: Option<u64>,
}

fn bad<T>(path: &str, msg: impl Into<String>) -> Result<T, MoatConfigError> {
    Err(MoatConfigError {
        message: format!(
            "{}: {}",
            if path.is_empty() { "schema" } else { path },
            msg.into()
        ),
    })
}

fn uint(obj: &Map<String, Value>, key: &str, path: &str) -> Result<Option<u64>, MoatConfigError> {
    match obj.get(key) {
        None => Ok(None),
        Some(v) => match v.as_u64() {
            Some(n) => Ok(Some(n)),
            None => bad(path, format!("{key} must be a non-negative integer")),
        },
    }
}

fn num(obj: &Map<String, Value>, key: &str, path: &str) -> Result<Option<f64>, MoatConfigError> {
    match obj.get(key) {
        None => Ok(None),
        Some(Value::Number(n)) => Ok(n.as_f64()),
        Some(_) => bad(path, format!("{key} must be a number")),
    }
}

impl Schema {
    fn compile(v: &Value, path: &str) -> Result<Schema, MoatConfigError> {
        let obj = match v {
            Value::Object(o) => o,
            _ => return bad(path, "a schema must be an object"),
        };
        let mut s = Schema::default();
        for (key, val) in obj {
            match key.as_str() {
                "type" => {
                    let names: Vec<&Value> = match val {
                        Value::Array(a) if !a.is_empty() => a.iter().collect(),
                        Value::Array(_) => return bad(path, "type must not be an empty list"),
                        other => vec![other],
                    };
                    let mut types = Vec::new();
                    for n in names {
                        match n.as_str().and_then(Ty::parse) {
                            Some(t) => types.push(t),
                            None => return bad(path, "unknown type name"),
                        }
                    }
                    s.types = Some(types);
                }
                "properties" => {
                    let props = match val {
                        Value::Object(p) => p,
                        _ => return bad(path, "properties must be an object"),
                    };
                    for (name, sub) in props {
                        s.properties.push((
                            name.clone(),
                            Schema::compile(sub, &format!("{path}/properties/{name}"))?,
                        ));
                    }
                }
                "required" => match val {
                    Value::Array(a) => {
                        for n in a {
                            match n.as_str() {
                                Some(n) => s.required.push(n.to_string()),
                                None => return bad(path, "required must list strings"),
                            }
                        }
                    }
                    _ => return bad(path, "required must be an array"),
                },
                "additionalProperties" => match val {
                    Value::Bool(b) => s.additional_properties = Some(*b),
                    _ => return bad(path, "additionalProperties must be a boolean"),
                },
                "items" => {
                    s.items = Some(Box::new(Schema::compile(val, &format!("{path}/items"))?));
                }
                "enum" => match val {
                    Value::Array(a) if !a.is_empty() => s.enumeration = Some(a.clone()),
                    _ => return bad(path, "enum must be a non-empty array"),
                },
                "const" => s.constant = Some(val.clone()),
                "minLength" | "maxLength" | "minItems" | "maxItems" => {
                    let n = uint(obj, key, path)?;
                    match key.as_str() {
                        "minLength" => s.min_length = n,
                        "maxLength" => s.max_length = n,
                        "minItems" => s.min_items = n,
                        _ => s.max_items = n,
                    }
                }
                "pattern" => match val.as_str() {
                    Some(p) => match Regex::new(p) {
                        Ok(r) => s.pattern = Some(r),
                        Err(e) => return bad(path, format!("pattern is not usable: {e}")),
                    },
                    None => return bad(path, "pattern must be a string"),
                },
                "minimum" => s.minimum = num(obj, key, path)?,
                "maximum" => s.maximum = num(obj, key, path)?,
                "title" | "description" | "$comment" => {}
                other => return bad(path, format!("unsupported keyword `{other}`")),
            }
        }
        Ok(s)
    }

    /// True when the schema asks for exactly a string (so output is not parsed as JSON).
    fn is_string_only(&self) -> bool {
        matches!(self.types.as_deref(), Some([Ty::String]))
    }

    fn validate(&self, v: &Value, path: &str) -> Result<(), String> {
        let at = |m: String| -> String {
            if path.is_empty() {
                m
            } else {
                format!("{path}: {m}")
            }
        };
        if let Some(types) = &self.types {
            if !types.iter().any(|t| t.matches(v)) {
                let names: Vec<&str> = types.iter().map(|t| t.name()).collect();
                return Err(at(format!("expected {}", names.join(" or "))));
            }
        }
        if let Some(e) = &self.enumeration {
            if !e.iter().any(|x| json_eq(x, v)) {
                return Err(at("not one of the allowed values".into()));
            }
        }
        if let Some(c) = &self.constant {
            if !json_eq(c, v) {
                return Err(at("does not equal the required constant".into()));
            }
        }
        match v {
            Value::String(s) => {
                let len = s.chars().count() as u64;
                if self.min_length.map_or(false, |m| len < m) {
                    return Err(at("shorter than minLength".into()));
                }
                if self.max_length.map_or(false, |m| len > m) {
                    return Err(at("longer than maxLength".into()));
                }
                if let Some(p) = &self.pattern {
                    if !p.is_match(s) {
                        return Err(at("does not match pattern".into()));
                    }
                }
            }
            Value::Number(n) => {
                let f = n.as_f64().unwrap_or(f64::NAN);
                if self.minimum.map_or(false, |m| f < m) {
                    return Err(at("below minimum".into()));
                }
                if self.maximum.map_or(false, |m| f > m) {
                    return Err(at("above maximum".into()));
                }
            }
            Value::Array(items) => {
                if self.min_items.map_or(false, |m| (items.len() as u64) < m) {
                    return Err(at("fewer than minItems".into()));
                }
                if self.max_items.map_or(false, |m| (items.len() as u64) > m) {
                    return Err(at("more than maxItems".into()));
                }
                if let Some(sub) = &self.items {
                    for (i, item) in items.iter().enumerate() {
                        sub.validate(item, &format!("{path}/{i}"))?;
                    }
                }
            }
            Value::Object(obj) => {
                for r in &self.required {
                    if !obj.contains_key(r) {
                        return Err(at(format!("missing required property `{r}`")));
                    }
                }
                for (name, sub) in &self.properties {
                    if let Some(val) = obj.get(name) {
                        sub.validate(val, &format!("{path}/{name}"))?;
                    }
                }
                if self.additional_properties == Some(false) {
                    for k in obj.keys() {
                        if !self.properties.iter().any(|(n, _)| n == k) {
                            return Err(at(format!("unexpected property `{k}`")));
                        }
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
}

/// JSON equality where numbers compare by value (`1` equals `1.0`).
fn json_eq(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(x), Value::Number(y)) => x.as_f64() == y.as_f64(),
        (Value::Array(x), Value::Array(y)) => {
            x.len() == y.len() && x.iter().zip(y).all(|(p, q)| json_eq(p, q))
        }
        (Value::Object(x), Value::Object(y)) => {
            x.len() == y.len()
                && x.iter()
                    .all(|(k, v)| y.get(k).map_or(false, |w| json_eq(v, w)))
        }
        _ => a == b,
    }
}

// ---------------------------------------------------------------------------------------------
// Marker detection
// ---------------------------------------------------------------------------------------------

/// Default-ignorable and format code points, as inclusive (low, high) ranges. One shared table,
/// so the scan never depends on a platform property lookup.
const DEFAULT_IGNORABLE: [(u32, u32); 17] = [
    (0x00AD, 0x00AD),
    (0x034F, 0x034F),
    (0x061C, 0x061C),
    (0x115F, 0x1160),
    (0x17B4, 0x17B5),
    (0x180B, 0x180F),
    (0x200B, 0x200F),
    (0x202A, 0x202E),
    (0x2060, 0x206F),
    (0x3164, 0x3164),
    (0xFE00, 0xFE0F),
    (0xFEFF, 0xFEFF),
    (0xFFA0, 0xFFA0),
    (0xFFF0, 0xFFF8),
    (0x1BCA0, 0x1BCA3),
    (0x1D173, 0x1D17A),
    (0xE0000, 0xE0FFF),
];

fn is_default_ignorable(c: char) -> bool {
    let cp = c as u32;
    cp != 0
        && DEFAULT_IGNORABLE
            .iter()
            .any(|&(lo, hi)| cp >= lo && cp <= hi)
}

/// Cyrillic and Greek look-alikes of the marker letters, mapped to the lowercase Latin letter.
/// Mirrors the shared `moat-confusables.json` (version 2); a test keeps the two equal. This is not
/// general homoglyph detection.
const CONFUSABLES: [(char, char); 47] = [
    ('\u{0430}', 'a'),
    ('\u{0410}', 'a'),
    ('\u{0441}', 'c'),
    ('\u{0421}', 'c'),
    ('\u{0435}', 'e'),
    ('\u{0415}', 'e'),
    ('\u{0456}', 'i'),
    ('\u{0406}', 'i'),
    ('\u{043E}', 'o'),
    ('\u{041E}', 'o'),
    ('\u{0440}', 'p'),
    ('\u{0420}', 'p'),
    ('\u{0455}', 's'),
    ('\u{0405}', 's'),
    ('\u{0443}', 'y'),
    ('\u{0423}', 'y'),
    ('\u{0501}', 'd'),
    ('\u{04CF}', 'l'),
    ('\u{04C0}', 'l'),
    ('\u{0422}', 't'),
    ('\u{041C}', 'm'),
    ('\u{041D}', 'h'),
    ('\u{0445}', 'x'),
    ('\u{0425}', 'x'),
    ('\u{0391}', 'a'),
    ('\u{03B1}', 'a'),
    ('\u{0395}', 'e'),
    ('\u{03B5}', 'e'),
    ('\u{0399}', 'i'),
    ('\u{03B9}', 'i'),
    ('\u{039F}', 'o'),
    ('\u{03BF}', 'o'),
    ('\u{03A1}', 'p'),
    ('\u{03C1}', 'p'),
    ('\u{03A4}', 't'),
    ('\u{03C4}', 't'),
    ('\u{03A5}', 'y'),
    ('\u{03C5}', 'u'),
    ('\u{03BD}', 'v'),
    ('\u{039C}', 'm'),
    ('\u{039D}', 'n'),
    ('\u{03A7}', 'x'),
    ('\u{03C7}', 'x'),
    ('\u{03F2}', 'c'),
    ('\u{03F9}', 'c'),
    ('\u{0397}', 'h'),
    ('\u{0442}', 't'),
];
#[doc(hidden)]
pub fn confusables() -> &'static [(char, char)] {
    &CONFUSABLES
}

fn confusable(c: char) -> char {
    CONFUSABLES
        .iter()
        .find(|(from, _)| *from == c)
        .map_or(c, |(_, to)| *to)
}

/// Normalisation for marker matching only (the text sent to the model is unchanged): NFKC, NFKD
/// with combining marks dropped, default-ignorable code points removed, look-alikes mapped to
/// Latin, lower-cased, runs of horizontal whitespace collapsed to one space. Line breaks are kept, because the label check is line-start.
fn fold(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut in_space = false;
    for c in text.nfkc().nfkd() {
        if is_combining_mark(c) || is_default_ignorable(c) {
            continue;
        }
        let c = confusable(c);
        let c = match c {
            '\r' | '\u{0B}' | '\u{0C}' | '\u{85}' | '\u{2028}' | '\u{2029}' => '\n',
            _ => c,
        };
        if c == '\t'
            || matches!(
                c,
                '\u{00A0}' | '\u{1680}' | '\u{2000}'
                    ..='\u{200A}' | '\u{202F}' | '\u{205F}' | '\u{3000}' | ' '
            )
        {
            if !in_space {
                out.push(' ');
                in_space = true;
            }
            continue;
        }
        in_space = false;
        out.extend(c.to_lowercase());
    }
    out
}

const LINE_MARKERS: [&str; 5] = ["system:", "assistant:", "developer:", "tool:", "user:"];
const TOKEN_MARKERS: [&str; 7] = [
    "<|system|>",
    "<|im_start|>",
    "<|im_end|>",
    "[inst]",
    "<<sys>>",
    "### system",
    "### instruction",
];

/// The marker found in one string, if any.
fn text_marker(raw: &str) -> Option<&'static str> {
    let t = fold(raw);
    for line in t.split('\n') {
        let l = line.trim_start();
        for m in LINE_MARKERS {
            // the label, optional spaces, then the colon ("system :" counts)
            if let Some(rest) = l.strip_prefix(&m[..m.len() - 1]) {
                if rest.trim_start_matches(' ').starts_with(':') {
                    return Some("a line starting with a role label");
                }
            }
        }
    }
    for m in TOKEN_MARKERS {
        if t.contains(m) {
            return Some("a chat-template marker");
        }
    }
    if has_role_json(&t) {
        return Some("a JSON object with a privileged role");
    }
    None
}

/// `"role": "system"` (and developer / assistant) written out as JSON text, tolerant of spacing
/// and of single quotes.
fn has_role_json(t: &str) -> bool {
    let chars: Vec<char> = t.chars().collect();
    let n = chars.len();
    let mut i = 0;
    let skip_ws = |mut j: usize| {
        while j < n && chars[j].is_whitespace() {
            j += 1;
        }
        j
    };
    while i < n {
        if chars[i] == '"' || chars[i] == '\'' {
            let q = chars[i];
            let rest: String = chars[i + 1..n.min(i + 6)].iter().collect();
            if rest.starts_with("role") && chars.get(i + 5) == Some(&q) {
                let mut j = skip_ws(i + 6);
                if chars.get(j) == Some(&':') {
                    j = skip_ws(j + 1);
                    if matches!(chars.get(j), Some('"') | Some('\'')) {
                        let tail: String = chars[j + 1..n.min(j + 12)].iter().collect();
                        for role in ["system", "developer", "assistant"] {
                            if tail.starts_with(role) {
                                let end = j + 1 + role.len();
                                if matches!(chars.get(end), Some('"') | Some('\'')) {
                                    return true;
                                }
                            }
                        }
                    }
                }
            }
        }
        i += 1;
    }
    false
}

fn marker_in_value(v: &Value) -> Option<&'static str> {
    match v {
        Value::String(s) => {
            if let Some(m) = text_marker(s) {
                return Some(m);
            }
            // a string that is itself JSON: look inside it too
            let trimmed = s.trim_start();
            if trimmed.starts_with('{') || trimmed.starts_with('[') {
                if let Ok(inner) = serde_json::from_str::<Value>(s) {
                    return marker_in_value(&inner);
                }
            }
            None
        }
        Value::Array(a) => a.iter().find_map(marker_in_value),
        Value::Object(o) => {
            for (k, val) in o {
                if let Value::String(r) = val {
                    if fold(k).trim() == "role" {
                        let r = fold(r);
                        if matches!(r.trim(), "system" | "developer" | "assistant") {
                            return Some("a JSON object with a privileged role");
                        }
                    }
                }
            }
            for (k, val) in o {
                if let Some(m) = text_marker(k) {
                    return Some(m);
                }
                if let Some(m) = marker_in_value(val) {
                    return Some(m);
                }
            }
            None
        }
        _ => None,
    }
}

fn collect_strings<'a>(v: &'a Value, out: &mut Vec<&'a str>) {
    match v {
        Value::String(s) => out.push(s),
        Value::Array(a) => a.iter().for_each(|x| collect_strings(x, out)),
        Value::Object(o) => {
            for (k, val) in o {
                out.push(k);
                collect_strings(val, out);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------------------------
// Moat
// ---------------------------------------------------------------------------------------------

/// A configured guard. Build one with [`Moat::new`].
pub struct Moat {
    prompt: String,
    input_schema: Option<Schema>,
    output_schema: Option<Schema>,
    pii: Option<PiiFilter>,
}

impl fmt::Debug for Moat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Moat")
            .field("input_schema", &self.input_schema.is_some())
            .field("output_schema", &self.output_schema.is_some())
            .field("pii", &self.pii.is_some())
            .finish_non_exhaustive()
    }
}

impl Moat {
    /// Create a guard. Fails with [`MoatConfigError`] when a schema uses a keyword outside the
    /// supported subset or is otherwise unusable.
    pub fn new(options: MoatOptions) -> Result<Moat, MoatConfigError> {
        let input_schema = options
            .input_schema
            .as_ref()
            .map(|s| Schema::compile(s, "inputSchema"))
            .transpose()?;
        let output_schema = options
            .output_schema
            .as_ref()
            .map(|s| Schema::compile(s, "outputSchema"))
            .transpose()?;
        Ok(Moat {
            prompt: options.prompt,
            input_schema,
            output_schema,
            pii: options.pii,
        })
    }

    /// The message list for a model call: the pinned prompt as `system`, the untrusted input as
    /// `user`. A string input is sent as is; any other JSON value is sent as compact JSON text.
    pub fn messages(&self, input: &Value) -> Result<Vec<Message>, MoatRefusal> {
        if let Some(schema) = &self.input_schema {
            schema.validate(input, "").map_err(|detail| MoatRefusal {
                reason: MoatReason::InputSchema,
                detail,
            })?;
        }
        if let Some(what) = marker_in_value(input) {
            return Err(MoatRefusal {
                reason: MoatReason::RoleMarker,
                detail: format!("the input contains {what}"),
            });
        }
        let content = match input {
            Value::String(s) => s.clone(),
            other => serde_json::to_string(other).expect("a JSON value serialises"),
        };
        // The serialised text can join pieces that were separate (for example key and value).
        if let Some(what) = text_marker(&content) {
            return Err(MoatRefusal {
                reason: MoatReason::RoleMarker,
                detail: format!("the input contains {what}"),
            });
        }
        Ok(vec![
            Message {
                role: "system".into(),
                content: self.prompt.clone(),
            },
            Message {
                role: "user".into(),
                content,
            },
        ])
    }

    /// [`Moat::messages`] for a plain text input.
    pub fn messages_text(&self, input: &str) -> Result<Vec<Message>, MoatRefusal> {
        self.messages(&Value::String(input.to_string()))
    }

    /// Validate and filter what the model returned. The text is parsed as JSON unless the output
    /// schema asks for exactly a string (or there is no output schema, which yields the text as a
    /// JSON string). Returns the validated value.
    pub fn check_output(&self, model_output: &str) -> Result<Value, MoatRefusal> {
        let value = match &self.output_schema {
            Some(schema) if !schema.is_string_only() => serde_json::from_str::<Value>(model_output)
                .map_err(|_| MoatRefusal {
                    reason: MoatReason::OutputSchema,
                    detail: "the output is not valid JSON".into(),
                })?,
            _ => Value::String(model_output.to_string()),
        };
        if let Some(schema) = &self.output_schema {
            schema.validate(&value, "").map_err(|detail| MoatRefusal {
                reason: MoatReason::OutputSchema,
                detail,
            })?;
        }
        if let Some(filter) = &self.pii {
            let mut texts: Vec<&str> = vec![model_output];
            collect_strings(&value, &mut texts);
            for t in texts {
                if let Some(detail) = filter(t) {
                    return Err(MoatRefusal {
                        reason: MoatReason::Pii,
                        detail,
                    });
                }
            }
        }
        Ok(value)
    }

    /// Lowercase hex SHA-256 of the pinned prompt (UTF-8), for example to pass to Rampart.
    pub fn prompt_sha256(&self) -> String {
        hex(&sha256(self.prompt.as_bytes()))
    }
}
