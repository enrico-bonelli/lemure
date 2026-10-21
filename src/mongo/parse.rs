//! Parser della sintassi della shell di MongoDB.
//!
//! I valori sono letterali JavaScript (chiavi senza virgolette, stringhe con
//! apici singoli, `ObjectId("…")`, `ISODate("…")`, regex `/…/i`, …) e
//! diventano BSON. Un comando è `show …`, `use …` oppure una catena di
//! chiamate su `db` o su una collection; `plan` lo traduce in un'operazione
//! eseguibile verificando gli argomenti, così il controllo sintattico
//! dell'editor segnala anche gli errori di forma (es. filtro non oggetto).

use crate::sqlutil::{self, Dialect, SyntaxIssue, WordClass};
use mongodb::bson::{
    Binary, Bson, DateTime, Decimal128, Document, Regex, Timestamp, oid::ObjectId,
    spec::BinarySubtype,
};

#[derive(Clone, Debug, PartialEq)]
pub struct ParseError {
    /// Offset in byte nel testo del comando.
    pub pos: usize,
    pub message: String,
}

type PResult<T> = Result<T, ParseError>;

fn err<T>(pos: usize, message: impl Into<String>) -> PResult<T> {
    Err(ParseError {
        pos,
        message: message.into(),
    })
}

// ---------------------------------------------------------------------------
// Letterali
// ---------------------------------------------------------------------------

struct P<'a> {
    s: &'a str,
    b: &'a [u8],
    pos: usize,
}

fn is_ident_start(c: char) -> bool {
    c.is_alphabetic() || c == '_' || c == '$'
}

fn is_ident_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$'
}

/// Vero se il nome si può scrivere senza virgolette (chiave o `db.nome`).
pub fn is_identifier(name: &str) -> bool {
    let mut it = name.chars();
    it.next().is_some_and(is_ident_start) && it.all(is_ident_char)
}

impl<'a> P<'a> {
    fn new(s: &'a str) -> P<'a> {
        P {
            s,
            b: s.as_bytes(),
            pos: 0,
        }
    }

    fn skip_ws(&mut self) {
        loop {
            while self.pos < self.b.len() && self.b[self.pos].is_ascii_whitespace() {
                self.pos += 1;
            }
            let rest = &self.s[self.pos..];
            if rest.starts_with("//") {
                self.pos += rest.find('\n').unwrap_or(rest.len());
            } else if let Some(body) = rest.strip_prefix("/*") {
                self.pos += body.find("*/").map_or(rest.len(), |i| i + 4);
            } else {
                return;
            }
        }
    }

    fn peek(&mut self) -> Option<u8> {
        self.skip_ws();
        self.b.get(self.pos).copied()
    }

    fn eat(&mut self, c: u8) -> bool {
        if self.peek() == Some(c) {
            self.pos += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: u8, what: &str) -> PResult<()> {
        if self.eat(c) {
            Ok(())
        } else {
            let found = match self.peek() {
                None => "fine del testo".to_string(),
                Some(_) => format!("«{}»", self.s[self.pos..].chars().next().unwrap()),
            };
            err(self.pos, format!("atteso {what}, trovato {found}"))
        }
    }

    fn at_end(&mut self) -> bool {
        self.peek().is_none()
    }

    fn ident(&mut self) -> Option<(usize, String)> {
        self.skip_ws();
        let start = self.pos;
        let mut chars = self.s[start..].char_indices();
        match chars.next() {
            Some((_, c)) if is_ident_start(c) => {}
            _ => return None,
        }
        let end = chars
            .find(|(_, c)| !is_ident_char(*c))
            .map_or(self.s.len(), |(i, _)| start + i);
        self.pos = end;
        Some((start, self.s[start..end].to_string()))
    }

    fn value(&mut self) -> PResult<Bson> {
        let start = match self.peek() {
            None => return err(self.pos, "atteso un valore, trovato fine del testo"),
            Some(_) => self.pos,
        };
        match self.b[start] {
            b'{' => Ok(convert_extjson(self.object()?, start)?),
            b'[' => Ok(Bson::Array(self.array()?)),
            b'\'' | b'"' | b'`' => Ok(Bson::String(self.string()?)),
            b'/' => self.regex(),
            b'-' | b'+' | b'.' | b'0'..=b'9' => self.number(),
            _ => self.word_value(),
        }
    }

    fn object(&mut self) -> PResult<Document> {
        self.expect(b'{', "«{»")?;
        let mut doc = Document::new();
        loop {
            if self.eat(b'}') {
                return Ok(doc);
            }
            let key_pos = self.pos;
            let key = match self.peek() {
                Some(b'\'' | b'"' | b'`') => self.string()?,
                Some(b'0'..=b'9') => {
                    let start = self.pos;
                    while self.pos < self.b.len() && self.b[self.pos].is_ascii_digit() {
                        self.pos += 1;
                    }
                    self.s[start..self.pos].to_string()
                }
                _ => match self.ident() {
                    Some((_, k)) => k,
                    None => {
                        return err(
                            key_pos,
                            "atteso il nome di un campo (i nomi con punti o spazi vanno fra virgolette)",
                        );
                    }
                },
            };
            self.expect(b':', "«:» dopo il nome del campo")?;
            let v = self.value()?;
            doc.insert(key, v);
            if !self.eat(b',') {
                self.expect(b'}', "«,» o «}»")?;
                return Ok(doc);
            }
        }
    }

    fn array(&mut self) -> PResult<Vec<Bson>> {
        self.expect(b'[', "«[»")?;
        let mut out = Vec::new();
        loop {
            if self.eat(b']') {
                return Ok(out);
            }
            out.push(self.value()?);
            if !self.eat(b',') {
                self.expect(b']', "«,» o «]»")?;
                return Ok(out);
            }
        }
    }

    fn string(&mut self) -> PResult<String> {
        self.skip_ws();
        let start = self.pos;
        let quote = self.b[start];
        let mut out = String::new();
        let mut it = self.s[start + 1..].char_indices();
        while let Some((i, c)) = it.next() {
            match c {
                c if c as u32 == quote as u32 => {
                    self.pos = start + 1 + i + 1;
                    return Ok(out);
                }
                '\n' if quote != b'`' => break,
                '\\' => match it.next() {
                    Some((_, 'n')) => out.push('\n'),
                    Some((_, 't')) => out.push('\t'),
                    Some((_, 'r')) => out.push('\r'),
                    Some((_, 'b')) => out.push('\u{8}'),
                    Some((_, 'f')) => out.push('\u{c}'),
                    Some((_, '0')) => out.push('\0'),
                    Some((j, 'u')) => {
                        let hex = self.s.get(start + 2 + j..start + 6 + j).unwrap_or("");
                        let ch = u32::from_str_radix(hex, 16).ok().and_then(char::from_u32);
                        match ch {
                            Some(ch) if hex.len() == 4 => {
                                out.push(ch);
                                for _ in 0..4 {
                                    it.next();
                                }
                            }
                            _ => return err(start + 1 + j, "sequenza \\u non valida"),
                        }
                    }
                    Some((_, '\n')) => {}
                    Some((_, other)) => out.push(other),
                    None => break,
                },
                c => out.push(c),
            }
        }
        err(start, "stringa non terminata")
    }

    fn regex(&mut self) -> PResult<Bson> {
        let start = self.pos;
        let mut i = start + 1;
        let mut in_class = false;
        while i < self.b.len() {
            match self.b[i] {
                b'\\' => i += 1,
                b'\n' => break,
                b'[' => in_class = true,
                b']' => in_class = false,
                b'/' if !in_class => {
                    let pattern = self.s[start + 1..i].to_string();
                    let mut j = i + 1;
                    while j < self.b.len() && self.b[j].is_ascii_alphabetic() {
                        j += 1;
                    }
                    let flags = self.s[i + 1..j].to_string();
                    if let Some(bad) = flags.chars().find(|c| !"imsxlu".contains(*c)) {
                        return err(i + 1, format!("opzione di regex non valida: {bad}"));
                    }
                    self.pos = j;
                    return Ok(Bson::RegularExpression(Regex {
                        pattern,
                        options: sorted_flags(&flags),
                    }));
                }
                _ => {}
            }
            i += 1;
        }
        err(start, "espressione regolare non terminata")
    }

    fn number(&mut self) -> PResult<Bson> {
        let start = self.pos;
        let mut i = start;
        let neg = self.b[i] == b'-';
        if matches!(self.b[i], b'-' | b'+') {
            i += 1;
        }
        let rest = &self.s[i..];
        if rest.starts_with("Infinity") {
            self.pos = i + 8;
            return Ok(Bson::Double(if neg {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            }));
        }
        if rest.starts_with("0x") || rest.starts_with("0X") {
            let mut j = i + 2;
            while j < self.b.len() && self.b[j].is_ascii_hexdigit() {
                j += 1;
            }
            let v = i64::from_str_radix(&self.s[i + 2..j], 16).map_err(|_| ParseError {
                pos: start,
                message: "numero esadecimale non valido".into(),
            })?;
            self.pos = j;
            return Ok(int_bson(if neg { -v } else { v }));
        }
        let mut j = i;
        let mut float = false;
        while j < self.b.len() {
            match self.b[j] {
                b'0'..=b'9' => {}
                b'.' => float = true,
                b'e' | b'E' => {
                    float = true;
                    if matches!(self.b.get(j + 1), Some(b'+' | b'-')) {
                        j += 1;
                    }
                }
                _ => break,
            }
            j += 1;
        }
        let text = &self.s[start..j];
        if j == i || self.s[j..].chars().next().is_some_and(is_ident_char) {
            return err(
                start,
                format!("numero non valido: {}", &self.s[start..j.max(i + 1)]),
            );
        }
        self.pos = j;
        if !float && let Ok(v) = text.trim_start_matches('+').parse::<i64>() {
            return Ok(int_bson(v));
        }
        text.trim_start_matches('+')
            .parse::<f64>()
            .map(Bson::Double)
            .map_err(|_| ParseError {
                pos: start,
                message: format!("numero non valido: {text}"),
            })
    }

    /// Parole: costanti (`true`, `null`, …) e costruttori (`ObjectId("…")`, …).
    fn word_value(&mut self) -> PResult<Bson> {
        let Some((pos, mut word)) = self.ident() else {
            let c = self.s[self.pos..].chars().next().unwrap_or(' ');
            return err(self.pos, format!("carattere inatteso «{c}»"));
        };
        if word == "new" {
            word = match self.ident() {
                Some((_, w)) => w,
                None => return err(self.pos, "atteso un costruttore dopo «new»"),
            };
        }
        match word.as_str() {
            "true" => return Ok(Bson::Boolean(true)),
            "false" => return Ok(Bson::Boolean(false)),
            "null" | "undefined" => return Ok(Bson::Null),
            "Infinity" => return Ok(Bson::Double(f64::INFINITY)),
            "NaN" => return Ok(Bson::Double(f64::NAN)),
            "MinKey" | "MaxKey" if self.peek() != Some(b'(') => {
                return Ok(if word == "MinKey" {
                    Bson::MinKey
                } else {
                    Bson::MaxKey
                });
            }
            _ => {}
        }
        if self.peek() != Some(b'(') {
            return err(
                pos,
                format!(
                    "valore non riconosciuto: «{word}» (le variabili JavaScript non sono supportate)"
                ),
            );
        }
        let args = self.args()?;
        constructor(&word, pos, &args)
    }

    /// Argomenti di una chiamata: `( valore, … )` con la posizione di ciascuno.
    fn args(&mut self) -> PResult<Vec<(usize, Bson)>> {
        self.expect(b'(', "«(»")?;
        let mut out = Vec::new();
        loop {
            if self.eat(b')') {
                return Ok(out);
            }
            self.skip_ws();
            let pos = self.pos;
            out.push((pos, self.value()?));
            if !self.eat(b',') {
                self.expect(b')', "«,» o «)»")?;
                return Ok(out);
            }
        }
    }
}

fn sorted_flags(flags: &str) -> String {
    let mut f: Vec<char> = flags.chars().collect();
    f.sort_unstable();
    f.dedup();
    f.into_iter().collect()
}

fn int_bson(v: i64) -> Bson {
    match i32::try_from(v) {
        Ok(i) => Bson::Int32(i),
        Err(_) => Bson::Int64(v),
    }
}

/// Date ISO 8601 anche abbreviate: `2024-01-31`, `2024-01-31 10:00`, senza fuso = UTC.
fn parse_date(s: &str) -> Option<DateTime> {
    let mut t = s.trim().replacen(' ', "T", 1);
    if t.len() == 10 {
        t.push_str("T00:00:00");
    }
    if t.len() == 16 {
        t.push_str(":00");
    }
    let has_zone = t.ends_with('Z') || t.ends_with('z') || {
        let tail = &t[t.len().saturating_sub(6)..];
        tail.starts_with('+') || tail.starts_with('-')
    };
    if !has_zone {
        t.push('Z');
    }
    DateTime::parse_rfc3339_str(&t).ok()
}

fn num_arg(v: &Bson) -> Option<f64> {
    match v {
        Bson::Int32(i) => Some(*i as f64),
        Bson::Int64(i) => Some(*i as f64),
        Bson::Double(d) => Some(*d),
        _ => None,
    }
}

fn int_arg_of(v: &Bson) -> Option<i64> {
    match v {
        Bson::Int32(i) => Some(*i as i64),
        Bson::Int64(i) => Some(*i),
        Bson::Double(d) if d.fract() == 0.0 => Some(*d as i64),
        Bson::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn str_of(v: &Bson) -> Option<&str> {
    match v {
        Bson::String(s) => Some(s),
        _ => None,
    }
}

fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace() && *c != b'=') {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            _ => return None,
        } as u32;
        acc = (acc << 6) | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

fn constructor(name: &str, pos: usize, args: &[(usize, Bson)]) -> PResult<Bson> {
    let first = args.first().map(|a| &a.1);
    let arg_pos = args.first().map_or(pos, |a| a.0);
    let bad = |what: &str| err(arg_pos, format!("{name}: {what}"));
    match name {
        "ObjectId" => match first {
            None => Ok(Bson::ObjectId(ObjectId::new())),
            Some(Bson::String(s)) => match ObjectId::parse_str(s.trim()) {
                Ok(o) => Ok(Bson::ObjectId(o)),
                Err(_) => bad("servono 24 cifre esadecimali"),
            },
            _ => bad("atteso una stringa esadecimale"),
        },
        "ISODate" | "Date" => match first {
            None => Ok(Bson::DateTime(DateTime::now())),
            Some(Bson::String(s)) => match parse_date(s) {
                Some(d) => Ok(Bson::DateTime(d)),
                None => bad("data non valida (formato ISO 8601, es. \"2024-01-31T10:00:00Z\")"),
            },
            Some(v) => match num_arg(v) {
                Some(ms) => Ok(Bson::DateTime(DateTime::from_millis(ms as i64))),
                None => bad("attesa una stringa ISO 8601 o i millisecondi"),
            },
        },
        "NumberLong" | "Long" => match first.and_then(int_arg_of) {
            Some(v) => Ok(Bson::Int64(v)),
            None => bad("atteso un intero"),
        },
        "NumberInt" | "Int32" => match first.and_then(int_arg_of).map(i32::try_from) {
            Some(Ok(v)) => Ok(Bson::Int32(v)),
            _ => bad("atteso un intero a 32 bit"),
        },
        "Double" => match first.and_then(num_arg) {
            Some(v) => Ok(Bson::Double(v)),
            None => bad("atteso un numero"),
        },
        "NumberDecimal" | "Decimal128" => {
            let text = match first {
                Some(Bson::String(s)) => s.clone(),
                Some(v) => match num_arg(v) {
                    Some(n) => n.to_string(),
                    None => return bad("atteso un numero o una stringa"),
                },
                None => "0".into(),
            };
            match text.trim().parse::<Decimal128>() {
                Ok(d) => Ok(Bson::Decimal128(d)),
                Err(_) => bad("numero decimale non valido"),
            }
        }
        "UUID" => match first.and_then(str_of) {
            Some(s) => match mongodb::bson::Uuid::parse_str(s.trim()) {
                Ok(u) => Ok(Bson::Binary(Binary::from_uuid(u))),
                Err(_) => bad("UUID non valido"),
            },
            None => Ok(Bson::Binary(Binary::from_uuid(mongodb::bson::Uuid::new()))),
        },
        "BinData" => {
            let subtype = args.first().map(|a| &a.1).and_then(int_arg_of);
            let data = args.get(1).map(|a| &a.1).and_then(str_of);
            match (subtype, data.and_then(base64_decode)) {
                (Some(t), Some(bytes)) if (0..=255).contains(&t) => Ok(Bson::Binary(Binary {
                    subtype: BinarySubtype::from(t as u8),
                    bytes,
                })),
                _ => bad("attesi BinData(sottotipo, \"base64\")"),
            }
        }
        "Timestamp" => {
            let t = args.first().map(|a| &a.1).and_then(int_arg_of);
            let i = args.get(1).map(|a| &a.1).and_then(int_arg_of);
            match (t, i) {
                (Some(t), Some(i)) => Ok(Bson::Timestamp(Timestamp {
                    time: t as u32,
                    increment: i as u32,
                })),
                _ => bad("attesi Timestamp(secondi, incremento)"),
            }
        }
        "RegExp" => match (first.and_then(str_of), args.get(1).map(|a| &a.1)) {
            (Some(p), None) => Ok(Bson::RegularExpression(Regex {
                pattern: p.to_string(),
                options: String::new(),
            })),
            (Some(p), Some(Bson::String(f))) => Ok(Bson::RegularExpression(Regex {
                pattern: p.to_string(),
                options: sorted_flags(f),
            })),
            _ => bad("attesi RegExp(\"pattern\", \"opzioni\")"),
        },
        "MinKey" => Ok(Bson::MinKey),
        "MaxKey" => Ok(Bson::MaxKey),
        _ => err(
            pos,
            format!(
                "funzione non supportata: {name}() (le funzioni JavaScript non sono supportate)"
            ),
        ),
    }
}

/// Extended JSON: `{"$oid": "…"}`, `{"$date": …}`, `{"$numberLong": "…"}`…
/// (il formato con cui Lemure mostra i sotto-documenti) torna al tipo BSON.
fn convert_extjson(doc: Document, pos: usize) -> PResult<Bson> {
    if doc.len() != 1 {
        return Ok(Bson::Document(doc));
    }
    let (k, v) = doc.iter().next().unwrap();
    let as_arg = || vec![(pos, v.clone())];
    let out = match (k.as_str(), v) {
        ("$oid", Bson::String(_)) => constructor("ObjectId", pos, &as_arg())?,
        ("$date", Bson::String(_)) => constructor("ISODate", pos, &as_arg())?,
        ("$date", Bson::Document(d)) if d.contains_key("$numberLong") => {
            let ms = d.get("$numberLong").and_then(int_arg_of);
            match ms {
                Some(ms) => Bson::DateTime(DateTime::from_millis(ms)),
                None => return err(pos, "$date: millisecondi non validi"),
            }
        }
        ("$date", v) if num_arg(v).is_some() => constructor("ISODate", pos, &as_arg())?,
        ("$numberLong", _) => constructor("NumberLong", pos, &as_arg())?,
        ("$numberInt", _) => constructor("NumberInt", pos, &as_arg())?,
        ("$numberDecimal", _) => constructor("NumberDecimal", pos, &as_arg())?,
        ("$numberDouble", Bson::String(s)) => match s.parse::<f64>() {
            Ok(d) => Bson::Double(d),
            Err(_) => return err(pos, "$numberDouble non valido"),
        },
        ("$uuid", _) => constructor("UUID", pos, &as_arg())?,
        _ => Bson::Document(doc),
    };
    Ok(out)
}

/// Analizza un singolo valore (usato dai test e dalla validazione).
pub fn parse_value(src: &str) -> PResult<Bson> {
    let mut p = P::new(src);
    let v = p.value()?;
    if !p.at_end() {
        return err(p.pos, "testo inatteso dopo il valore");
    }
    Ok(v)
}

// ---------------------------------------------------------------------------
// Comandi
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
pub struct Call {
    pub name: String,
    pub pos: usize,
    pub args: Vec<(usize, Bson)>,
    /// Posizione della parentesi di chiusura (per gli errori sugli argomenti mancanti).
    pub end: usize,
}

#[derive(Clone, Debug)]
pub enum Command {
    Show {
        what: String,
        pos: usize,
    },
    Use {
        db: String,
        pos: usize,
    },
    /// Chiamate su `db` (`db.runCommand(...)`) o su una collection.
    Chain {
        /// `db.getSiblingDB("x")`: database diverso da quello corrente.
        db: Option<String>,
        coll: Option<(usize, String)>,
        calls: Vec<Call>,
    },
}

fn parse_call(p: &mut P, name: String, pos: usize) -> PResult<Call> {
    let args = p.args()?;
    Ok(Call {
        name,
        pos,
        args,
        end: p.pos.saturating_sub(1),
    })
}

pub fn parse_command(src: &str) -> PResult<Command> {
    let mut p = P::new(src);
    let Some((pos, first)) = p.ident() else {
        p.skip_ws();
        return err(
            p.pos,
            "atteso un comando: db.collection.metodo(…), show collections, use database",
        );
    };
    let cmd = match first.as_str() {
        "show" => {
            let Some((_, what)) = p.ident() else {
                return err(p.pos, "atteso: show dbs | collections");
            };
            Command::Show { what, pos }
        }
        "use" => {
            p.skip_ws();
            let start = p.pos;
            let end = src[start..]
                .find(|c: char| c.is_whitespace() || c == ';')
                .map_or(src.len(), |i| start + i);
            if end == start {
                return err(start, "atteso il nome del database dopo «use»");
            }
            p.pos = end;
            Command::Use {
                db: src[start..end].to_string(),
                pos: start,
            }
        }
        "db" => parse_chain(&mut p)?,
        other => {
            return err(
                pos,
                format!(
                    "comando sconosciuto «{other}»: si inizia con db., show o use (SQL non è supportato su MongoDB)"
                ),
            );
        }
    };
    p.eat(b';');
    if !p.at_end() {
        return err(p.pos, "testo inatteso dopo il comando");
    }
    Ok(cmd)
}

fn parse_chain(p: &mut P) -> PResult<Command> {
    let mut db = None;
    let mut segments: Vec<(usize, String)> = Vec::new();
    let mut calls: Vec<Call> = Vec::new();
    loop {
        if p.eat(b'[') {
            if !calls.is_empty() {
                return err(p.pos, "«[» inatteso");
            }
            p.skip_ws();
            let pos = p.pos;
            let Bson::String(name) = p.value()? else {
                return err(pos, "atteso il nome della collection fra virgolette");
            };
            p.expect(b']', "«]»")?;
            segments.push((pos, name));
            continue;
        }
        if !p.eat(b'.') {
            break;
        }
        let Some((pos, name)) = p.ident() else {
            return err(p.pos, "atteso un nome dopo «.»");
        };
        if p.peek() != Some(b'(') {
            if !calls.is_empty() {
                return err(pos, format!("atteso «(» dopo {name}"));
            }
            segments.push((pos, name));
            continue;
        }
        if calls.is_empty() && segments.is_empty() {
            // metodi di db che restituiscono una collection o un altro database
            match name.as_str() {
                "getCollection" => {
                    let call = parse_call(p, name, pos)?;
                    match call.args.first() {
                        Some((apos, Bson::String(c))) if call.args.len() == 1 => {
                            segments.push((*apos, c.clone()));
                        }
                        _ => return err(pos, "getCollection: atteso il nome della collection"),
                    }
                    continue;
                }
                "getSiblingDB" => {
                    let call = parse_call(p, name, pos)?;
                    match call.args.first() {
                        Some((_, Bson::String(d))) if call.args.len() == 1 => {
                            db = Some(d.clone());
                        }
                        _ => return err(pos, "getSiblingDB: atteso il nome del database"),
                    }
                    continue;
                }
                _ => {}
            }
        }
        calls.push(parse_call(p, name, pos)?);
    }
    if calls.is_empty() {
        let pos = p.pos;
        return err(
            pos,
            match segments.last() {
                Some((_, n)) => format!("manca il metodo: es. db.{n}.find({{}})"),
                None => "atteso «.» dopo db".to_string(),
            },
        );
    }
    let coll = (!segments.is_empty()).then(|| {
        let pos = segments[0].0;
        let name = segments
            .iter()
            .map(|s| s.1.as_str())
            .collect::<Vec<_>>()
            .join(".");
        (pos, name)
    });
    Ok(Command::Chain { db, coll, calls })
}

// ---------------------------------------------------------------------------
// Operazioni
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, Default, PartialEq)]
pub struct FindOp {
    pub filter: Document,
    pub projection: Option<Document>,
    pub sort: Option<Document>,
    pub hint: Option<Bson>,
    pub collation: Option<Document>,
    pub skip: u64,
    pub limit: Option<i64>,
    pub max_time_ms: Option<i64>,
    pub one: bool,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Find {
        coll: String,
        find: FindOp,
    },
    Aggregate {
        coll: String,
        pipeline: Vec<Document>,
        options: Document,
    },
    Count {
        coll: String,
        filter: Document,
        skip: u64,
        limit: Option<i64>,
        max_time_ms: Option<i64>,
    },
    /// Conteggio dei risultati di una pipeline (`aggregate([...]).count()`).
    AggregateCount {
        coll: String,
        pipeline: Vec<Document>,
        options: Document,
    },
    EstimatedCount {
        coll: String,
    },
    Distinct {
        coll: String,
        field: String,
        filter: Document,
    },
    Insert {
        coll: String,
        docs: Vec<Document>,
    },
    Update {
        coll: String,
        filter: Document,
        update: Bson,
        many: bool,
        upsert: bool,
        array_filters: Option<Vec<Document>>,
    },
    Replace {
        coll: String,
        filter: Document,
        doc: Document,
        upsert: bool,
    },
    Delete {
        coll: String,
        filter: Document,
        many: bool,
    },
    CreateIndex {
        coll: String,
        keys: Document,
        options: Document,
    },
    DropIndex {
        coll: String,
        index: Bson,
    },
    ListIndexes {
        coll: String,
    },
    DropCollection {
        coll: String,
    },
    CollStats {
        coll: String,
    },
    Rename {
        coll: String,
        to: String,
        drop_target: bool,
    },
    /// Comando generico (`db.runCommand`, `explain`, `db.stats()`…).
    RunCommand {
        cmd: Document,
        admin: bool,
    },
    ListCollections,
    ListDatabases,
    Use(String),
    CreateCollection {
        name: String,
        options: Document,
    },
    DropDatabase,
}

/// Operazione con l'eventuale database esplicito (`db.getSiblingDB("x")`).
#[derive(Clone, Debug, PartialEq)]
pub struct Planned {
    pub db: Option<String>,
    pub op: Op,
}

struct Args<'a> {
    call: &'a Call,
}

impl<'a> Args<'a> {
    fn sig(&self) -> &str {
        &self.call.name
    }

    fn get(&self, i: usize) -> Option<&'a (usize, Bson)> {
        self.call.args.get(i)
    }

    fn max(&self, n: usize) -> PResult<()> {
        match self.call.args.get(n) {
            Some((pos, _)) => err(*pos, format!("{}: troppi argomenti", self.sig())),
            None => Ok(()),
        }
    }

    fn doc(&self, i: usize, what: &str) -> PResult<Document> {
        match self.get(i) {
            Some((_, Bson::Document(d))) => Ok(d.clone()),
            Some((pos, _)) => err(
                *pos,
                format!("{}: {what} deve essere un oggetto {{…}}", self.sig()),
            ),
            None => err(self.call.end, format!("{}: manca {what}", self.sig())),
        }
    }

    fn opt_doc(&self, i: usize, what: &str) -> PResult<Option<Document>> {
        match self.get(i) {
            None | Some((_, Bson::Null)) => Ok(None),
            Some(_) => self.doc(i, what).map(Some),
        }
    }

    fn filter(&self, i: usize) -> PResult<Document> {
        Ok(self.opt_doc(i, "il filtro")?.unwrap_or_default())
    }

    fn docs(&self, i: usize, what: &str) -> PResult<Vec<Document>> {
        match self.get(i) {
            Some((pos, Bson::Array(items))) => items
                .iter()
                .map(|v| match v {
                    Bson::Document(d) => Ok(d.clone()),
                    _ => err(
                        *pos,
                        format!("{}: {what} deve contenere solo oggetti", self.sig()),
                    ),
                })
                .collect(),
            Some((pos, _)) => err(
                *pos,
                format!("{}: {what} deve essere un array […]", self.sig()),
            ),
            None => err(self.call.end, format!("{}: manca {what}", self.sig())),
        }
    }

    fn int(&self, i: usize, what: &str) -> PResult<i64> {
        match self.get(i) {
            Some((pos, v)) => match v {
                Bson::Int32(_) | Bson::Int64(_) | Bson::Double(_) => {
                    int_arg_of(v).ok_or_else(|| ParseError {
                        pos: *pos,
                        message: format!("{}: {what} deve essere un intero", self.sig()),
                    })
                }
                _ => err(
                    *pos,
                    format!("{}: {what} deve essere un numero", self.sig()),
                ),
            },
            None => err(self.call.end, format!("{}: manca {what}", self.sig())),
        }
    }

    fn string(&self, i: usize, what: &str) -> PResult<String> {
        match self.get(i) {
            Some((_, Bson::String(s))) => Ok(s.clone()),
            Some((pos, _)) => err(
                *pos,
                format!("{}: {what} deve essere una stringa", self.sig()),
            ),
            None => err(self.call.end, format!("{}: manca {what}", self.sig())),
        }
    }

    fn bool_opt(opts: &Document, key: &str) -> bool {
        matches!(opts.get(key), Some(Bson::Boolean(true)))
    }
}

/// Metodi che si possono concatenare dopo find().
const CURSOR_IGNORED: &[&str] = &["toArray", "pretty", "batchSize", "noCursorTimeout"];

pub fn plan(cmd: &Command) -> PResult<Planned> {
    match cmd {
        Command::Show { what, pos } => {
            let op = match what.as_str() {
                "dbs" | "databases" => Op::ListDatabases,
                "collections" | "tables" => Op::ListCollections,
                other => return err(*pos, format!("show {other}: atteso dbs o collections")),
            };
            Ok(Planned { db: None, op })
        }
        Command::Use { db, .. } => Ok(Planned {
            db: None,
            op: Op::Use(db.clone()),
        }),
        Command::Chain { db, coll, calls } => {
            let op = match coll {
                None => plan_db(&calls[0], &calls[1..])?,
                Some((_, coll)) => plan_coll(coll, &calls[0], &calls[1..])?,
            };
            Ok(Planned { db: db.clone(), op })
        }
    }
}

fn no_chain(rest: &[Call]) -> PResult<()> {
    match rest
        .iter()
        .find(|c| !CURSOR_IGNORED.contains(&c.name.as_str()))
    {
        Some(c) => err(c.pos, format!("{}() non si può concatenare qui", c.name)),
        None => Ok(()),
    }
}

fn plan_db(call: &Call, rest: &[Call]) -> PResult<Op> {
    let a = Args { call };
    let op = match call.name.as_str() {
        "runCommand" | "adminCommand" => {
            a.max(1)?;
            let cmd = match a.get(0) {
                Some((_, Bson::String(name))) => mongodb::bson::doc! { name.clone(): 1 },
                _ => a.doc(0, "il comando")?,
            };
            Op::RunCommand {
                cmd,
                admin: call.name == "adminCommand",
            }
        }
        "getCollectionNames" | "getCollectionInfos" => {
            a.max(0)?;
            Op::ListCollections
        }
        "stats" => {
            a.max(0)?;
            Op::RunCommand {
                cmd: mongodb::bson::doc! { "dbStats": 1 },
                admin: false,
            }
        }
        "serverStatus" | "hostInfo" | "version" | "serverBuildInfo" => {
            a.max(0)?;
            let name = match call.name.as_str() {
                "version" | "serverBuildInfo" => "buildInfo",
                other => other,
            };
            Op::RunCommand {
                cmd: mongodb::bson::doc! { name: 1 },
                admin: true,
            }
        }
        "createCollection" | "createView" => {
            let name = a.string(0, "il nome della collection")?;
            let mut options = Document::new();
            if call.name == "createView" {
                a.max(3)?;
                options.insert("viewOn", a.string(1, "la collection di origine")?);
                options.insert("pipeline", a.docs(2, "la pipeline")?);
            } else {
                a.max(2)?;
                options = a.opt_doc(1, "le opzioni")?.unwrap_or_default();
            }
            Op::CreateCollection { name, options }
        }
        "dropDatabase" => {
            a.max(0)?;
            Op::DropDatabase
        }
        other => {
            return err(
                call.pos,
                format!(
                    "metodo di db non supportato: {other}() (per una collection: db.nome.{other}())"
                ),
            );
        }
    };
    no_chain(rest)?;
    Ok(op)
}

fn plan_coll(coll: &str, call: &Call, rest: &[Call]) -> PResult<Op> {
    let a = Args { call };
    let coll = coll.to_string();
    let op = match call.name.as_str() {
        "find" | "findOne" => {
            a.max(2)?;
            let mut find = FindOp {
                filter: a.filter(0)?,
                projection: a.opt_doc(1, "la proiezione")?,
                one: call.name == "findOne",
                ..Default::default()
            };
            if find.one {
                find.limit = Some(1);
                no_chain(rest)?;
                return Ok(Op::Find { coll, find });
            }
            return plan_cursor(coll, find, rest);
        }
        "aggregate" => {
            a.max(2)?;
            let pipeline = a.docs(0, "la pipeline")?;
            let options = a.opt_doc(1, "le opzioni")?.unwrap_or_default();
            return plan_aggregate(coll, pipeline, options, rest);
        }
        "countDocuments" | "count" => {
            a.max(2)?;
            let filter = a.filter(0)?;
            let opts = a.opt_doc(1, "le opzioni")?.unwrap_or_default();
            Op::Count {
                coll,
                filter,
                skip: opts.get("skip").and_then(int_arg_of).unwrap_or(0).max(0) as u64,
                limit: opts.get("limit").and_then(int_arg_of),
                max_time_ms: opts.get("maxTimeMS").and_then(int_arg_of),
            }
        }
        "estimatedDocumentCount" => {
            a.max(1)?;
            Op::EstimatedCount { coll }
        }
        "distinct" => {
            a.max(2)?;
            Op::Distinct {
                coll,
                field: a.string(0, "il nome del campo")?,
                filter: a.filter(1)?,
            }
        }
        "insertOne" => {
            a.max(2)?;
            Op::Insert {
                coll,
                docs: vec![a.doc(0, "il documento")?],
            }
        }
        "insertMany" => {
            a.max(2)?;
            Op::Insert {
                coll,
                docs: a.docs(0, "l'elenco dei documenti")?,
            }
        }
        "insert" => {
            a.max(2)?;
            let docs = match a.get(0) {
                Some((_, Bson::Array(_))) => a.docs(0, "l'elenco dei documenti")?,
                _ => vec![a.doc(0, "il documento")?],
            };
            Op::Insert { coll, docs }
        }
        "updateOne" | "updateMany" | "update" => {
            a.max(3)?;
            let filter = a.doc(0, "il filtro")?;
            let update = match a.get(1) {
                Some((_, Bson::Document(d))) => {
                    if d.keys().next().is_some_and(|k| !k.starts_with('$')) {
                        return err(
                            a.get(1).unwrap().0,
                            format!(
                                "{}: l'aggiornamento deve usare operatori come $set (per sostituire il documento usa replaceOne)",
                                call.name
                            ),
                        );
                    }
                    Bson::Document(d.clone())
                }
                Some((_, Bson::Array(_))) => Bson::Array(
                    a.docs(1, "la pipeline")?
                        .into_iter()
                        .map(Bson::Document)
                        .collect(),
                ),
                Some((pos, _)) => {
                    return err(
                        *pos,
                        format!("{}: l'aggiornamento deve essere un oggetto", call.name),
                    );
                }
                None => return err(call.end, format!("{}: manca l'aggiornamento", call.name)),
            };
            let opts = a.opt_doc(2, "le opzioni")?.unwrap_or_default();
            let array_filters = match opts.get("arrayFilters") {
                Some(Bson::Array(items)) => Some(
                    items
                        .iter()
                        .filter_map(|v| v.as_document().cloned())
                        .collect(),
                ),
                _ => None,
            };
            Op::Update {
                coll,
                filter,
                update,
                many: call.name == "updateMany" || Args::bool_opt(&opts, "multi"),
                upsert: Args::bool_opt(&opts, "upsert"),
                array_filters,
            }
        }
        "replaceOne" => {
            a.max(3)?;
            let filter = a.doc(0, "il filtro")?;
            let doc = a.doc(1, "il documento")?;
            if doc.keys().any(|k| k.starts_with('$')) {
                return err(
                    a.get(1).unwrap().0,
                    "replaceOne: il documento non può contenere operatori (per modificarlo usa updateOne)",
                );
            }
            let opts = a.opt_doc(2, "le opzioni")?.unwrap_or_default();
            Op::Replace {
                coll,
                filter,
                doc,
                upsert: Args::bool_opt(&opts, "upsert"),
            }
        }
        "deleteOne" | "deleteMany" | "remove" => {
            a.max(2)?;
            let filter = a.doc(0, "il filtro")?;
            let just_one = matches!(a.get(1), Some((_, Bson::Boolean(true))))
                || matches!(a.get(1), Some((_, Bson::Document(d))) if Args::bool_opt(d, "justOne"));
            Op::Delete {
                coll,
                filter,
                many: call.name == "deleteMany" || (call.name == "remove" && !just_one),
            }
        }
        "createIndex" | "ensureIndex" => {
            a.max(2)?;
            Op::CreateIndex {
                coll,
                keys: a.doc(0, "le chiavi dell'indice")?,
                options: a.opt_doc(1, "le opzioni")?.unwrap_or_default(),
            }
        }
        "dropIndex" => {
            a.max(1)?;
            let index = match a.get(0) {
                Some((_, v @ (Bson::String(_) | Bson::Document(_)))) => v.clone(),
                Some((pos, _)) => {
                    return err(*pos, "dropIndex: atteso il nome o le chiavi dell'indice");
                }
                None => return err(call.end, "dropIndex: manca il nome dell'indice"),
            };
            Op::DropIndex { coll, index }
        }
        "dropIndexes" => {
            a.max(1)?;
            Op::DropIndex {
                coll,
                index: a.get(0).map_or(Bson::String("*".into()), |v| v.1.clone()),
            }
        }
        "getIndexes" => {
            a.max(0)?;
            Op::ListIndexes { coll }
        }
        "drop" => {
            a.max(0)?;
            Op::DropCollection { coll }
        }
        "stats" => {
            a.max(1)?;
            Op::CollStats { coll }
        }
        "renameCollection" => {
            a.max(2)?;
            Op::Rename {
                coll,
                to: a.string(0, "il nuovo nome")?,
                drop_target: matches!(a.get(1), Some((_, Bson::Boolean(true)))),
            }
        }
        other => {
            return err(call.pos, format!("metodo non supportato: {other}()"));
        }
    };
    no_chain(rest)?;
    Ok(op)
}

/// Comando `find` del server: usato per l'esecuzione e per `explain`.
pub fn find_command(coll: &str, f: &FindOp) -> Document {
    let mut cmd = mongodb::bson::doc! { "find": coll, "filter": f.filter.clone() };
    if let Some(p) = &f.projection {
        cmd.insert("projection", p.clone());
    }
    if let Some(s) = &f.sort {
        cmd.insert("sort", s.clone());
    }
    if let Some(h) = &f.hint {
        cmd.insert("hint", h.clone());
    }
    if let Some(c) = &f.collation {
        cmd.insert("collation", c.clone());
    }
    if f.skip > 0 {
        cmd.insert("skip", f.skip as i64);
    }
    if let Some(l) = f.limit {
        cmd.insert("limit", l);
        if f.one {
            cmd.insert("singleBatch", true);
        }
    }
    if let Some(ms) = f.max_time_ms {
        cmd.insert("maxTimeMS", ms);
    }
    cmd
}

pub fn aggregate_command(coll: &str, pipeline: &[Document], options: &Document) -> Document {
    let mut cmd = mongodb::bson::doc! {
        "aggregate": coll,
        "pipeline": pipeline.to_vec(),
        "cursor": {},
    };
    for (k, v) in options {
        if k != "cursor" {
            cmd.insert(k.clone(), v.clone());
        }
    }
    cmd
}

fn explain_op(cmd: Document, call: &Call) -> PResult<Op> {
    let verbosity = match call.args.first() {
        None => "queryPlanner".to_string(),
        Some((_, Bson::Boolean(true))) => "allPlansExecution".to_string(),
        Some((_, Bson::String(v)))
            if matches!(
                v.as_str(),
                "queryPlanner" | "executionStats" | "allPlansExecution"
            ) =>
        {
            v.clone()
        }
        Some((pos, _)) => {
            return err(
                *pos,
                "explain: atteso \"queryPlanner\", \"executionStats\" o \"allPlansExecution\"",
            );
        }
    };
    Ok(Op::RunCommand {
        cmd: mongodb::bson::doc! { "explain": cmd, "verbosity": verbosity },
        admin: false,
    })
}

fn plan_cursor(coll: String, mut find: FindOp, rest: &[Call]) -> PResult<Op> {
    for (i, call) in rest.iter().enumerate() {
        let a = Args { call };
        match call.name.as_str() {
            "sort" => {
                a.max(1)?;
                find.sort = Some(a.doc(0, "l'ordinamento")?);
            }
            "projection" => {
                a.max(1)?;
                find.projection = Some(a.doc(0, "la proiezione")?);
            }
            // Più skip si sommano: la paginazione aggiunge il proprio in coda.
            "skip" => {
                a.max(1)?;
                find.skip += a.int(0, "il numero di documenti")?.max(0) as u64;
            }
            "limit" => {
                a.max(1)?;
                let n = a.int(0, "il numero di documenti")?.abs();
                if n > 0 {
                    find.limit = Some(find.limit.map_or(n, |l| l.min(n)));
                }
            }
            "hint" => {
                a.max(1)?;
                find.hint = a.get(0).map(|v| v.1.clone());
            }
            "collation" => {
                a.max(1)?;
                find.collation = Some(a.doc(0, "la collation")?);
            }
            "maxTimeMS" => {
                a.max(1)?;
                find.max_time_ms = Some(a.int(0, "i millisecondi")?);
            }
            "count" | "itcount" | "size" => {
                no_chain(&rest[i + 1..])?;
                // count() ignora skip e limit come nella shell; size() li applica
                let apply = call.name != "count";
                return Ok(Op::Count {
                    coll,
                    filter: find.filter,
                    skip: if apply { find.skip } else { 0 },
                    limit: if apply { find.limit } else { None },
                    max_time_ms: find.max_time_ms,
                });
            }
            "explain" => {
                no_chain(&rest[i + 1..])?;
                return explain_op(find_command(&coll, &find), call);
            }
            name if CURSOR_IGNORED.contains(&name) => {}
            other => {
                return err(
                    call.pos,
                    format!("metodo del cursore non supportato: {other}()"),
                );
            }
        }
    }
    Ok(Op::Find { coll, find })
}

fn plan_aggregate(
    coll: String,
    mut pipeline: Vec<Document>,
    mut options: Document,
    rest: &[Call],
) -> PResult<Op> {
    for (i, call) in rest.iter().enumerate() {
        let a = Args { call };
        match call.name.as_str() {
            // estensione di Lemure (usata dalla paginazione): diventano stadi della pipeline
            "skip" => {
                a.max(1)?;
                pipeline.push(
                    mongodb::bson::doc! { "$skip": a.int(0, "il numero di documenti")?.max(0) },
                );
            }
            "limit" => {
                a.max(1)?;
                pipeline.push(
                    mongodb::bson::doc! { "$limit": a.int(0, "il numero di documenti")?.max(1) },
                );
            }
            "maxTimeMS" => {
                a.max(1)?;
                options.insert("maxTimeMS", a.int(0, "i millisecondi")?);
            }
            "count" | "itcount" | "size" => {
                no_chain(&rest[i + 1..])?;
                return Ok(Op::AggregateCount {
                    coll,
                    pipeline,
                    options,
                });
            }
            "explain" => {
                no_chain(&rest[i + 1..])?;
                return explain_op(aggregate_command(&coll, &pipeline, &options), call);
            }
            name if CURSOR_IGNORED.contains(&name) => {}
            other => {
                return err(
                    call.pos,
                    format!("metodo non supportato dopo aggregate: {other}()"),
                );
            }
        }
    }
    Ok(Op::Aggregate {
        coll,
        pipeline,
        options,
    })
}

pub fn parse_and_plan(src: &str) -> PResult<Planned> {
    plan(&parse_command(src)?)
}

fn js_string(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Riferimento a una collection nella sintassi della shell
/// (`db.nome`, `db.getCollection("a-b")`, `db.getSiblingDB("x").nome`).
pub fn collection_ref(db: Option<&str>, coll: &str) -> String {
    let base = match db {
        Some(d) => format!("db.getSiblingDB({})", js_string(d)),
        None => "db".to_string(),
    };
    if is_identifier(coll) && !coll.starts_with('$') {
        format!("{base}.{coll}")
    } else {
        format!("{base}.getCollection({})", js_string(coll))
    }
}

/// Le pipeline con `$out`/`$merge` scrivono dati: non vanno rieseguite per pagina.
fn writes_data(pipeline: &[Document]) -> bool {
    pipeline
        .iter()
        .any(|s| s.contains_key("$out") || s.contains_key("$merge"))
}

/// Testo base per la paginazione lato server: find senza limit e pipeline di sola lettura.
pub fn pageable_base(src: &str) -> Option<String> {
    let planned = parse_and_plan(src).ok()?;
    let ok = match &planned.op {
        Op::Find { find, .. } => !find.one && find.limit.is_none(),
        Op::Aggregate { pipeline, .. } => !writes_data(pipeline),
        _ => false,
    };
    if !ok {
        return None;
    }
    // via il `;` finale e gli spazi, così si possono accodare .skip().limit()
    let t = src.trim_end();
    Some(t.strip_suffix(';').unwrap_or(t).trim().to_string())
}

/// Tipo dello statement (stile SQL) per capire se lo schema è cambiato.
pub fn statement_keyword(src: &str) -> &'static str {
    match parse_and_plan(src).map(|p| p.op) {
        Ok(Op::CreateCollection { .. } | Op::CreateIndex { .. }) => "CREATE",
        Ok(Op::DropCollection { .. } | Op::DropIndex { .. } | Op::DropDatabase) => "DROP",
        Ok(Op::Rename { .. }) => "RENAME",
        Ok(Op::Insert { .. }) => "INSERT",
        Ok(Op::Update { .. } | Op::Replace { .. }) => "UPDATE",
        Ok(Op::Delete { .. }) => "DELETE",
        Ok(Op::Use(_)) => "USE",
        Ok(_) => "SELECT",
        Err(_) => "",
    }
}

/// Controllo sintattico di tutto lo script: il primo errore trovato.
pub fn check_script(text: &str) -> Option<SyntaxIssue> {
    let toks = sqlutil::lex(text, Dialect::Mongo);
    for st in sqlutil::split_statements_in(&toks, true, text, Dialect::Mongo) {
        let src = &text[st.start..st.end];
        if let Err(e) = parse_and_plan(src) {
            let abs = (st.start + e.pos).min(text.len());
            let before = &text[..abs];
            let line = before.matches('\n').count() + 1;
            let column = before[before.rfind('\n').map_or(0, |i| i + 1)..]
                .chars()
                .count()
                + 1;
            return Some(SyntaxIssue {
                line,
                column,
                message: e.message,
            });
        }
    }
    None
}

// ---------------------------------------------------------------------------
// Evidenziazione
// ---------------------------------------------------------------------------

const CONSTRUCTORS: &[&str] = &[
    "ObjectId",
    "ISODate",
    "Date",
    "NumberLong",
    "NumberInt",
    "NumberDecimal",
    "Decimal128",
    "UUID",
    "BinData",
    "Timestamp",
    "RegExp",
    "MinKey",
    "MaxKey",
    "Long",
    "Int32",
    "Double",
];

pub fn classify_word(word: &str, followed_by_paren: bool) -> WordClass {
    if word.starts_with('$') {
        return WordClass::Keyword;
    }
    match word {
        "db" | "show" | "use" | "new" | "true" | "false" | "null" | "undefined" => {
            WordClass::Keyword
        }
        w if CONSTRUCTORS.contains(&w) => WordClass::Type,
        _ if followed_by_paren => WordClass::Function,
        _ => WordClass::Plain,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::doc;

    #[test]
    fn values() {
        let v = parse_value(
            "{a: 1, 'b c': \"x\\n\", d: [true, null, -2.5], e: ObjectId(\"5ceb9b5211c90d9af9b83548\"), \
             f: ISODate('2024-01-31'), g: /ab+c/i, h: NumberLong(5), i: {$oid: '5ceb9b5211c90d9af9b83548'}, \
             j: 3000000000, k: new Date(0), l: 1e3,}",
        )
        .unwrap();
        let d = v.as_document().unwrap();
        assert_eq!(d.get("a"), Some(&Bson::Int32(1)));
        assert_eq!(d.get_str("b c").unwrap(), "x\n");
        assert_eq!(
            d.get_array("d").unwrap(),
            &vec![Bson::Boolean(true), Bson::Null, Bson::Double(-2.5)]
        );
        assert!(matches!(d.get("e"), Some(Bson::ObjectId(_))));
        assert_eq!(d.get("e"), d.get("i"));
        let Some(Bson::DateTime(f)) = d.get("f") else {
            panic!()
        };
        assert_eq!(f.try_to_rfc3339_string().unwrap(), "2024-01-31T00:00:00Z");
        assert_eq!(
            d.get("g"),
            Some(&Bson::RegularExpression(Regex {
                pattern: "ab+c".into(),
                options: "i".into()
            }))
        );
        assert_eq!(d.get("h"), Some(&Bson::Int64(5)));
        assert_eq!(d.get("j"), Some(&Bson::Int64(3_000_000_000)));
        assert_eq!(d.get("k"), Some(&Bson::DateTime(DateTime::from_millis(0))));
        assert_eq!(d.get("l"), Some(&Bson::Double(1000.0)));
    }

    #[test]
    fn value_errors_have_positions() {
        let e = parse_value("{a: 1 b: 2}").unwrap_err();
        assert_eq!(e.pos, 6);
        let e = parse_value("{a: foo}").unwrap_err();
        assert!(e.message.contains("foo"));
        assert_eq!(e.pos, 4);
        assert!(parse_value("{a: ObjectId('xyz')}").is_err());
        assert!(parse_value("{a: 'abc}").is_err());
    }

    #[test]
    fn find_chain() {
        let p = parse_and_plan(
            "db.orders.find({status: 'paid'}, {total: 1}).sort({total: -1}).skip(10).limit(5);",
        )
        .unwrap();
        let Op::Find { coll, find } = p.op else {
            panic!()
        };
        assert_eq!(coll, "orders");
        assert_eq!(find.filter, doc! {"status": "paid"});
        assert_eq!(find.projection, Some(doc! {"total": 1}));
        assert_eq!(find.sort, Some(doc! {"total": -1}));
        assert_eq!((find.skip, find.limit), (10, Some(5)));
    }

    #[test]
    fn collection_names() {
        let names = [
            ("db.system.users.find()", "system.users"),
            ("db['my-coll'].find()", "my-coll"),
            ("db.getCollection(\"a b\").find()", "a b"),
        ];
        for (src, want) in names {
            match parse_and_plan(src).unwrap().op {
                Op::Find { coll, .. } => assert_eq!(coll, want),
                other => panic!("{other:?}"),
            }
        }
        let p = parse_and_plan("db.getSiblingDB('other').c.countDocuments({})").unwrap();
        assert_eq!(p.db.as_deref(), Some("other"));
        assert!(matches!(p.op, Op::Count { .. }));
    }

    #[test]
    fn other_commands() {
        assert_eq!(parse_and_plan("show dbs").unwrap().op, Op::ListDatabases);
        assert_eq!(
            parse_and_plan("show collections;").unwrap().op,
            Op::ListCollections
        );
        assert_eq!(
            parse_and_plan("use my-db").unwrap().op,
            Op::Use("my-db".into())
        );
        assert!(matches!(
            parse_and_plan("db.c.updateMany({a: 1}, {$set: {b: 2}}, {upsert: true})")
                .unwrap()
                .op,
            Op::Update {
                many: true,
                upsert: true,
                ..
            }
        ));
        assert!(matches!(
            parse_and_plan("db.c.aggregate([{$match: {}}]).count()")
                .unwrap()
                .op,
            Op::AggregateCount { .. }
        ));
        assert!(matches!(
            parse_and_plan("db.c.find({}).explain('executionStats')")
                .unwrap()
                .op,
            Op::RunCommand { .. }
        ));
        assert!(matches!(
            parse_and_plan("db.runCommand({ping: 1})").unwrap().op,
            Op::RunCommand { admin: false, .. }
        ));
    }

    #[test]
    fn plan_errors() {
        let e = parse_and_plan("db.c.find('x')").unwrap_err();
        assert_eq!(e.pos, 10);
        assert!(parse_and_plan("db.c.updateOne({}, {b: 2})").is_err());
        assert!(parse_and_plan("db.c.find({}).foo()").is_err());
        assert!(parse_and_plan("db.c").is_err());
        assert!(parse_and_plan("SELECT * FROM t").is_err());
        assert!(parse_and_plan("db.c.find({}) x").is_err());
    }

    #[test]
    fn collection_refs() {
        assert_eq!(collection_ref(None, "orders"), "db.orders");
        assert_eq!(collection_ref(None, "a-b"), "db.getCollection(\"a-b\")");
        let r = collection_ref(Some("x"), "c\"d");
        assert_eq!(r, "db.getSiblingDB(\"x\").getCollection(\"c\\\"d\")");
        let Command::Chain { db, coll, .. } = parse_command(&format!("{r}.find()")).unwrap() else {
            panic!()
        };
        assert_eq!(
            (db.as_deref(), coll.map(|c| c.1).as_deref()),
            (Some("x"), Some("c\"d"))
        );
    }

    #[test]
    fn paging() {
        let base = pageable_base("db.c.find({a: 1}).sort({b: 1});").unwrap();
        assert_eq!(base, "db.c.find({a: 1}).sort({b: 1})");
        let paged = sqlutil::paged_sql(&base, 501, 500, Dialect::Mongo);
        let Op::Find { find, .. } = parse_and_plan(&paged).unwrap().op else {
            panic!()
        };
        assert_eq!((find.skip, find.limit), (500, Some(501)));
        assert!(pageable_base("db.c.find().limit(3)").is_none());
        assert!(pageable_base("db.c.findOne()").is_none());
        assert!(pageable_base("db.c.aggregate([{$out: 'x'}])").is_none());
        let agg = pageable_base("db.c.aggregate([{$match: {}}]) // fine").unwrap();
        let Op::Aggregate { pipeline, .. } =
            parse_and_plan(&sqlutil::paged_sql(&agg, 501, 0, Dialect::Mongo))
                .unwrap()
                .op
        else {
            panic!()
        };
        assert_eq!(pipeline.len(), 3);
        let count = sqlutil::count_sql(&base, Dialect::Mongo);
        assert!(matches!(
            parse_and_plan(&count).unwrap().op,
            Op::Count {
                max_time_ms: Some(15000),
                ..
            }
        ));
    }

    #[test]
    fn script_check_and_split() {
        let text = "db.a.find({\n\n  x: 1\n})\n\ndb.b.find({y: })";
        let toks = sqlutil::lex(text, Dialect::Mongo);
        assert_eq!(sqlutil::split_statements(&toks, true, text).len(), 2);
        let issue = check_script(text).unwrap();
        assert_eq!((issue.line, issue.column), (6, 15));
        assert!(check_script("db.a.find({re: /a;b/}) // c;\n").is_none());
        // a capo + db = nuovo comando; a capo + . = stessa catena
        let text = "db.a.find()\n  .sort({x: 1})\n// nota\ndb.b.drop()\nshow dbs";
        let toks = sqlutil::lex(text, Dialect::Mongo);
        let st = sqlutil::split_statements_in(&toks, true, text, Dialect::Mongo);
        assert_eq!(st.len(), 3);
        assert!(check_script(text).is_none());
    }
}
