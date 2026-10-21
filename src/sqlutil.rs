//! Analisi lessicale SQL leggera: usata per l'evidenziazione della sintassi,
//! per separare gli statement e per decidere se una query è paginabile lato
//! server (aggiungendo `LIMIT ... OFFSET ...`).

use std::collections::HashSet;
use std::sync::OnceLock;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tok {
    Space,
    Comment,
    Str,
    QuotedIdent,
    Number,
    Word,
    Param,
    Semicolon,
    OpenParen,
    CloseParen,
    Punct,
}

/// Token con offset sia in byte (per lo slicing) che in caratteri (per GTK).
#[derive(Clone, Copy, Debug)]
pub struct Token {
    pub kind: Tok,
    pub start: usize,
    pub end: usize,
    pub cstart: usize,
    pub cend: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dialect {
    MySql,
    Postgres,
    /// Sintassi della shell di MongoDB (`db.collection.find({...})`), simile a JavaScript.
    Mongo,
}

struct Cursor<'a> {
    src: &'a str,
    bytes: &'a [u8],
    pos: usize,
    chars: usize,
}

impl<'a> Cursor<'a> {
    #[inline]
    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }
    #[inline]
    fn peek_at(&self, n: usize) -> Option<u8> {
        self.bytes.get(self.pos + n).copied()
    }
    /// Avanza di un carattere (UTF-8 aware).
    #[inline]
    fn bump(&mut self) {
        if let Some(b) = self.peek() {
            let len = if b < 0x80 {
                1
            } else if b >= 0xF0 {
                4
            } else if b >= 0xE0 {
                3
            } else {
                2
            };
            self.pos = (self.pos + len).min(self.bytes.len());
            self.chars += 1;
        }
    }
    fn starts_with(&self, s: &str) -> bool {
        self.src[self.pos..].starts_with(s)
    }
}

fn is_word_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

pub fn lex(src: &str, dialect: Dialect) -> Vec<Token> {
    let mut c = Cursor {
        src,
        bytes: src.as_bytes(),
        pos: 0,
        chars: 0,
    };
    let mut out: Vec<Token> = Vec::with_capacity(src.len() / 4);
    let mongo = dialect == Dialect::Mongo;
    while let Some(b) = c.peek() {
        let (start, cstart) = (c.pos, c.chars);
        let kind = match b {
            b' ' | b'\t' | b'\n' | b'\r' | 0x0c => {
                while matches!(c.peek(), Some(b' ' | b'\t' | b'\n' | b'\r' | 0x0c)) {
                    c.bump();
                }
                Tok::Space
            }
            b'-' if !mongo && c.peek_at(1) == Some(b'-') => {
                while !matches!(c.peek(), None | Some(b'\n')) {
                    c.bump();
                }
                Tok::Comment
            }
            b'#' if dialect == Dialect::MySql => {
                while !matches!(c.peek(), None | Some(b'\n')) {
                    c.bump();
                }
                Tok::Comment
            }
            b'/' if mongo && c.peek_at(1) == Some(b'/') => {
                while !matches!(c.peek(), None | Some(b'\n')) {
                    c.bump();
                }
                Tok::Comment
            }
            // Espressione regolare JavaScript (`/abc/i`) dove è atteso un valore.
            b'/' if mongo && c.peek_at(1) != Some(b'*') && regex_allowed(&out, src) => {
                lex_regex(&mut c);
                Tok::Str
            }
            b'/' if c.peek_at(1) == Some(b'*') => {
                c.bump();
                c.bump();
                while c.peek().is_some() && !c.starts_with("*/") {
                    c.bump();
                }
                c.bump();
                c.bump();
                Tok::Comment
            }
            b'\'' => {
                let backslash = dialect != Dialect::Postgres
                    || (start > 0 && matches!(src.as_bytes()[start - 1], b'E' | b'e'));
                lex_quoted(&mut c, b'\'', backslash);
                Tok::Str
            }
            b'"' => {
                lex_quoted(&mut c, b'"', dialect != Dialect::Postgres);
                if dialect != Dialect::Postgres {
                    Tok::Str
                } else {
                    Tok::QuotedIdent
                }
            }
            b'`' if mongo => {
                lex_quoted(&mut c, b'`', true);
                Tok::Str
            }
            b'`' => {
                lex_quoted(&mut c, b'`', false);
                Tok::QuotedIdent
            }
            b'$' if dialect == Dialect::Postgres => {
                if c.peek_at(1).is_some_and(|d| d.is_ascii_digit()) {
                    c.bump();
                    while c.peek().is_some_and(|d| d.is_ascii_digit()) {
                        c.bump();
                    }
                    Tok::Param
                } else if let Some(tag_len) = dollar_tag(&src[c.pos..]) {
                    let tag = &src[c.pos..c.pos + tag_len];
                    for _ in 0..tag_len {
                        c.bump();
                    }
                    match src[c.pos..].find(tag) {
                        Some(i) => {
                            let target = c.pos + i + tag_len;
                            while c.pos < target {
                                c.bump();
                            }
                        }
                        None => {
                            while c.peek().is_some() {
                                c.bump();
                            }
                        }
                    }
                    Tok::Str
                } else {
                    c.bump();
                    Tok::Punct
                }
            }
            b'0'..=b'9' => {
                if b == b'0' && matches!(c.peek_at(1), Some(b'x' | b'X')) {
                    c.bump();
                    c.bump();
                    while c.peek().is_some_and(|d| d.is_ascii_hexdigit()) {
                        c.bump();
                    }
                } else {
                    while c.peek().is_some_and(|d| d.is_ascii_digit() || d == b'.') {
                        c.bump();
                    }
                    if matches!(c.peek(), Some(b'e' | b'E'))
                        && c.peek_at(1)
                            .is_some_and(|d| d.is_ascii_digit() || d == b'-' || d == b'+')
                    {
                        c.bump();
                        c.bump();
                        while c.peek().is_some_and(|d| d.is_ascii_digit()) {
                            c.bump();
                        }
                    }
                }
                // "1abc" in MySQL è un identificatore valido
                if c.peek().is_some_and(is_word_byte) {
                    while c.peek().is_some_and(is_word_byte) {
                        c.bump();
                    }
                    Tok::Word
                } else {
                    Tok::Number
                }
            }
            b'?' if !mongo => {
                c.bump();
                Tok::Param
            }
            b':' if dialect == Dialect::Postgres && c.peek_at(1) == Some(b':') => {
                c.bump();
                c.bump();
                Tok::Punct
            }
            b';' => {
                c.bump();
                Tok::Semicolon
            }
            b'(' => {
                c.bump();
                Tok::OpenParen
            }
            b')' => {
                c.bump();
                Tok::CloseParen
            }
            _ if is_word_byte(b) => {
                while c.peek().is_some_and(is_word_byte) {
                    c.bump();
                }
                Tok::Word
            }
            _ => {
                c.bump();
                Tok::Punct
            }
        };
        out.push(Token {
            kind,
            start,
            end: c.pos,
            cstart,
            cend: c.chars,
        });
    }
    out
}

/// In JavaScript `/` apre una regex solo dove è atteso un valore (non dopo un operando).
fn regex_allowed(out: &[Token], src: &str) -> bool {
    match out
        .iter()
        .rev()
        .find(|t| !matches!(t.kind, Tok::Space | Tok::Comment))
    {
        None => true,
        Some(t) => match t.kind {
            Tok::OpenParen | Tok::Semicolon => true,
            Tok::Punct => matches!(
                &src[t.start..t.end],
                "," | ":" | "[" | "{" | "=" | "!" | "&" | "|" | "?"
            ),
            _ => false,
        },
    }
}

fn lex_regex(c: &mut Cursor) {
    c.bump();
    let mut in_class = false;
    while let Some(b) = c.peek() {
        match b {
            b'\\' => {
                c.bump();
                c.bump();
                continue;
            }
            b'\n' => return,
            b'[' => in_class = true,
            b']' => in_class = false,
            b'/' if !in_class => {
                c.bump();
                while c.peek().is_some_and(|f| f.is_ascii_alphabetic()) {
                    c.bump();
                }
                return;
            }
            _ => {}
        }
        c.bump();
    }
}

fn lex_quoted(c: &mut Cursor, quote: u8, backslash: bool) {
    c.bump();
    while let Some(b) = c.peek() {
        if backslash && b == b'\\' {
            c.bump();
            c.bump();
            continue;
        }
        c.bump();
        if b == quote {
            if c.peek() == Some(quote) {
                c.bump();
                continue;
            }
            return;
        }
    }
}

/// Riconosce un tag di dollar-quoting PostgreSQL (`$$` o `$tag$`), ritorna la lunghezza.
fn dollar_tag(s: &str) -> Option<usize> {
    let b = s.as_bytes();
    let mut i = 1;
    while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_') {
        i += 1;
    }
    (i < b.len() && b[i] == b'$').then_some(i + 1)
}

/// Statement individuato nel testo (offset in byte e caratteri).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stmt {
    pub start: usize,
    pub end: usize,
    pub cstart: usize,
    pub cend: usize,
}

/// Separa gli statement su `;` e (opzionalmente) sulle righe vuote.
pub fn split_statements(toks: &[Token], blank_line_delimits: bool, src: &str) -> Vec<Stmt> {
    split_statements_in(toks, blank_line_delimits, src, Dialect::MySql)
}

/// Come `split_statements`; con MongoDB (come in JavaScript) anche un a capo
/// seguito da `db`, `show` o `use` inizia un nuovo comando, mentre una riga
/// che inizia con `.` continua la catena.
pub fn split_statements_in(
    toks: &[Token],
    blank_line_delimits: bool,
    src: &str,
    dialect: Dialect,
) -> Vec<Stmt> {
    let mut out = Vec::new();
    let mut cur: Option<Stmt> = None;
    let mut depth = 0i32;
    let close = |cur: &mut Option<Stmt>, out: &mut Vec<Stmt>| {
        if let Some(s) = cur.take() {
            out.push(s);
        }
    };
    for (i, t) in toks.iter().enumerate() {
        match t.kind {
            Tok::Semicolon => {
                close(&mut cur, &mut out);
                depth = 0;
            }
            Tok::Space => {
                let newlines = src[t.start..t.end].bytes().filter(|&b| b == b'\n').count();
                let new_command = dialect == Dialect::Mongo
                    && newlines >= 1
                    && toks.get(i + 1).is_some_and(|n| {
                        n.kind == Tok::Word && matches!(&src[n.start..n.end], "db" | "show" | "use")
                    });
                if depth == 0 && ((blank_line_delimits && newlines >= 2) || new_command) {
                    close(&mut cur, &mut out);
                }
            }
            Tok::Comment if cur.is_none() => {}
            _ => {
                match t.kind {
                    Tok::OpenParen => depth += 1,
                    Tok::CloseParen => depth -= 1,
                    _ => {}
                }
                match cur.as_mut() {
                    Some(s) => {
                        s.end = t.end;
                        s.cend = t.cend;
                    }
                    None => {
                        cur = Some(Stmt {
                            start: t.start,
                            end: t.end,
                            cstart: t.cstart,
                            cend: t.cend,
                        })
                    }
                }
            }
        }
    }
    close(&mut cur, &mut out);
    out
}

/// Statement sotto il cursore (offset in caratteri). Se il cursore è fra due
/// statement viene preferito quello precedente.
pub fn statement_at(stmts: &[Stmt], cursor: usize) -> Option<Stmt> {
    if let Some(s) = stmts
        .iter()
        .find(|s| cursor >= s.cstart && cursor <= s.cend)
    {
        return Some(*s);
    }
    stmts
        .iter()
        .rev()
        .find(|s| s.cend <= cursor)
        .or_else(|| stmts.first())
        .copied()
}

fn upper_word(src: &str, t: &Token) -> String {
    src[t.start..t.end].to_ascii_uppercase()
}

/// Se lo statement è una SELECT senza clausole che impediscono di aggiungere
/// `LIMIT/OFFSET`, ritorna il testo pulito da usare come base per la paginazione.
pub fn pageable_base(sql: &str, dialect: Dialect) -> Option<String> {
    if dialect == Dialect::Mongo {
        return crate::mongo::parse::pageable_base(sql);
    }
    let toks = lex(sql, dialect);
    let mut depth = 0i32;
    let mut first: Option<String> = None;
    let mut last_meaningful = 0usize;
    for t in &toks {
        match t.kind {
            Tok::Space | Tok::Comment => continue,
            Tok::Semicolon => {
                // più di uno statement: non paginabile
                if toks.iter().any(|o| {
                    o.start > t.start
                        && !matches!(o.kind, Tok::Space | Tok::Comment | Tok::Semicolon)
                }) {
                    return None;
                }
                continue;
            }
            Tok::OpenParen => depth += 1,
            Tok::CloseParen => depth -= 1,
            Tok::Word => {
                let w = upper_word(sql, t);
                // Istruzioni che modificano dati (anche dentro una CTE) non vanno
                // rieseguite a ogni pagina.
                if matches!(w.as_str(), "INSERT" | "UPDATE" | "DELETE" | "MERGE") {
                    return None;
                }
                if depth == 0
                    && matches!(
                        w.as_str(),
                        "LIMIT" | "OFFSET" | "FETCH" | "FOR" | "INTO" | "LOCK" | "PROCEDURE"
                    )
                {
                    return None;
                }
                if first.is_none() {
                    first = Some(w);
                }
            }
            _ => {}
        }
        if first.is_none() && t.kind != Tok::OpenParen {
            return None;
        }
        last_meaningful = t.end;
    }
    match first.as_deref() {
        Some("SELECT" | "WITH" | "VALUES" | "TABLE") => {
            Some(sql[..last_meaningful].trim().to_string())
        }
        _ => None,
    }
}

pub fn paged_sql(base: &str, limit: usize, offset: usize, dialect: Dialect) -> String {
    match dialect {
        // vedi mongo::parse: skip/limit aggiunti in coda alla catena
        Dialect::Mongo => format!("{base}\n.skip({offset}).limit({limit})"),
        _ => format!("{base}\nLIMIT {limit} OFFSET {offset}"),
    }
}

/// SQL per contare le righe di una query paginabile. Quando possibile la
/// lista delle colonne viene sostituita da `COUNT(*)` (più veloce e funziona
/// anche con colonne omonime nelle JOIN, che MySQL rifiuta in una subquery);
/// altrimenti la query viene racchiusa in una subquery.
pub fn count_sql(base: &str, dialect: Dialect) -> String {
    if dialect == Dialect::Mongo {
        return format!("{base}\n.maxTimeMS(15000).size()");
    }
    if let Some(direct) = direct_count_sql(base, dialect) {
        return direct;
    }
    match dialect {
        // L'hint limita il conteggio a 15s su tabelle enormi.
        Dialect::MySql => {
            format!(
                "SELECT /*+ MAX_EXECUTION_TIME(15000) */ COUNT(*) FROM (\n{base}\n) AS lemure_count"
            )
        }
        Dialect::Postgres | Dialect::Mongo => {
            format!("SELECT COUNT(*) FROM (\n{base}\n) AS lemure_count")
        }
    }
}

fn direct_count_sql(base: &str, dialect: Dialect) -> Option<String> {
    let toks = lex(base, dialect);
    let mut depth = 0i32;
    let mut words: Vec<(String, &Token)> = Vec::new();
    for t in &toks {
        match t.kind {
            Tok::OpenParen => depth += 1,
            Tok::CloseParen => depth -= 1,
            Tok::Word if depth == 0 => words.push((upper_word(base, t), t)),
            _ => {}
        }
    }
    if words.first().map(|w| w.0.as_str()) != Some("SELECT") {
        return None;
    }
    if words
        .get(1)
        .is_some_and(|w| matches!(w.0.as_str(), "DISTINCT" | "DISTINCTROW" | "ALL"))
    {
        return None;
    }
    let blocked = [
        "GROUP",
        "HAVING",
        "UNION",
        "INTERSECT",
        "EXCEPT",
        "WINDOW",
        "QUALIFY",
    ];
    if words.iter().any(|w| blocked.contains(&w.0.as_str())) {
        return None;
    }
    let from = words.iter().find(|w| w.0 == "FROM")?.1.start;
    let end = words
        .windows(2)
        .find(|p| p[0].0 == "ORDER" && p[1].0 == "BY")
        .map(|p| p[0].1.start)
        .unwrap_or(base.len());
    Some(format!("SELECT COUNT(*) {}", base[from..end].trim_end()))
}

/// Primo token-parola dello statement in maiuscolo (per capire il tipo).
pub fn first_keyword(sql: &str, dialect: Dialect) -> String {
    if dialect == Dialect::Mongo {
        return crate::mongo::parse::statement_keyword(sql).to_string();
    }
    lex(sql, dialect)
        .iter()
        .find(|t| t.kind == Tok::Word)
        .map(|t| upper_word(sql, t))
        .unwrap_or_default()
}

/// Errore di sintassi trovato dal parser: posizione (1-based) e messaggio.
#[derive(Clone, Debug, PartialEq)]
pub struct SyntaxIssue {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

/// Controllo sintattico con `sqlparser`. Gli statement che il parser non
/// conosce (es. alcune varianti di SHOW) vengono segnalati come avvisi.
pub fn check_syntax(sql: &str, dialect: Dialect) -> Option<SyntaxIssue> {
    use sqlparser::dialect::{MySqlDialect, PostgreSqlDialect};
    use sqlparser::parser::Parser;
    if sql.trim().is_empty() {
        return None;
    }
    let res = match dialect {
        Dialect::MySql => Parser::parse_sql(&MySqlDialect {}, sql),
        Dialect::Postgres => Parser::parse_sql(&PostgreSqlDialect {}, sql),
        Dialect::Mongo => return crate::mongo::parse::check_script(sql),
    };
    let err = res.err()?;
    let msg = err.to_string();
    // Gli errori a fine testo ("found: EOF") non hanno posizione: si usa l'ultimo carattere.
    let (line, column) = parse_location(&msg).unwrap_or_else(|| {
        let t = sql.trim_end();
        let line = t.lines().count().max(1);
        let col = t
            .lines()
            .last()
            .map(|l| l.chars().count())
            .unwrap_or(1)
            .max(1);
        (line, col)
    });
    let clean = msg
        .trim_start_matches("sql parser error: ")
        .trim_start_matches("SQL parser error: ")
        .to_string();
    let clean = match clean.rfind(" at Line: ") {
        Some(i) => clean[..i].to_string(),
        None => clean,
    };
    Some(SyntaxIssue {
        line,
        column,
        message: clean,
    })
}

fn parse_location(msg: &str) -> Option<(usize, usize)> {
    let i = msg.rfind("Line: ")?;
    let rest = &msg[i + 6..];
    let comma = rest.find(',')?;
    let line = rest[..comma].trim().parse().ok()?;
    let j = rest.find("Column: ")?;
    let col: String = rest[j + 8..]
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    Some((line, col.parse().ok()?))
}

// ---------------------------------------------------------------------------
// Parole chiave per l'evidenziazione
// ---------------------------------------------------------------------------

const KEYWORDS: &str = "ADD ALL ALTER ANALYZE AND ANY AS ASC AUTO_INCREMENT BEGIN BETWEEN BY CALL CASCADE CASE \
CAST CHANGE CHECK COLLATE COLUMN COMMENT COMMIT CONSTRAINT CREATE CROSS CURRENT_DATE CURRENT_TIME \
CURRENT_TIMESTAMP DATABASE DATABASES DEFAULT DELETE DESC DESCRIBE DISTINCT DO DROP DUPLICATE ELSE END ENGINE \
ESCAPE EXCEPT EXISTS EXPLAIN FALSE FETCH FIRST FOR FOREIGN FROM FULL FUNCTION GRANT GROUP HAVING IF IGNORE \
ILIKE IN INDEX INNER INSERT INTERSECT INTERVAL INTO IS JOIN KEY KEYS LATERAL LEFT LIKE LIMIT LOCK MERGE \
MODIFY NATURAL NOT NULL NULLS OFFSET ON ONLY OR ORDER OUTER OVER PARTITION PRIMARY PROCEDURE RECURSIVE \
REFERENCES REGEXP RENAME REPLACE RESTRICT RETURNING REVOKE RIGHT ROLLBACK ROW ROWS SAVEPOINT SCHEMA SELECT \
SEPARATOR SET SHOW START STATUS TABLE TABLES TEMPORARY THEN TO TRANSACTION TRIGGER TRUE TRUNCATE UNION \
UNIQUE UNSIGNED UPDATE USE USING VALUES VIEW WHEN WHERE WINDOW WITH ZEROFILL VARIABLES PROCESSLIST \
COLUMNS FIELDS GLOBAL SESSION LOCAL ASCENDING DESCENDING NO ACTION SEQUENCE OWNER EXTENSION RETURNS \
LANGUAGE DECLARE CURSOR LOOP WHILE LEAVE ITERATE REPEAT UNTIL HANDLER CONTINUE EXIT OPEN CLOSE \
DELIMITER ELSEIF ELSIF RAISE NOTICE EXECUTE PREPARE DEALLOCATE COPY VACUUM ALGORITHM DEFINER DETERMINISTIC \
SQL SECURITY INVOKER STRAIGHT_JOIN SQL_CALC_FOUND_ROWS HIGH_PRIORITY LOW_PRIORITY DELAYED QUICK \
USER CONFLICT NOTHING EXCLUDE MICROSECOND SECOND MINUTE HOUR DAY WEEK MONTH QUARTER \
GENERATED ALWAYS IDENTITY STORED VIRTUAL FILTER WITHIN TIES ROLLUP";

const TYPES: &str = "BIGINT BINARY BIT BLOB BOOL BOOLEAN BYTEA CHAR CHARACTER DATE DATETIME DEC DECIMAL DOUBLE \
ENUM FLOAT FLOAT4 FLOAT8 INT INT2 INT4 INT8 INTEGER JSON JSONB LONGBLOB LONGTEXT MEDIUMBLOB MEDIUMINT \
MEDIUMTEXT MONEY NUMERIC REAL SERIAL BIGSERIAL SMALLSERIAL SMALLINT TEXT TIME TIMESTAMP TIMESTAMPTZ TIMETZ \
TINYBLOB TINYINT TINYTEXT UUID VARBINARY VARCHAR VARYING YEAR INET CIDR MACADDR XML GEOMETRY POINT ARRAY \
PRECISION ZONE";

fn set_of(list: &'static str) -> HashSet<&'static str> {
    list.split_ascii_whitespace().collect()
}

fn keyword_set() -> &'static HashSet<&'static str> {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    S.get_or_init(|| set_of(KEYWORDS))
}

fn type_set() -> &'static HashSet<&'static str> {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    S.get_or_init(|| set_of(TYPES))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WordClass {
    Keyword,
    Type,
    Function,
    Plain,
}

/// Classifica una parola secondo il dialetto (per l'evidenziazione).
pub fn classify_word_in(word: &str, followed_by_paren: bool, dialect: Dialect) -> WordClass {
    if dialect == Dialect::Mongo {
        crate::mongo::parse::classify_word(word, followed_by_paren)
    } else {
        classify_word(word, followed_by_paren)
    }
}

/// Classifica una parola senza allocare (buffer su stack).
pub fn classify_word(word: &str, followed_by_paren: bool) -> WordClass {
    let bytes = word.as_bytes();
    if bytes.len() > 32 {
        return if followed_by_paren {
            WordClass::Function
        } else {
            WordClass::Plain
        };
    }
    let mut buf = [0u8; 32];
    for (i, b) in bytes.iter().enumerate() {
        buf[i] = b.to_ascii_uppercase();
    }
    let up = std::str::from_utf8(&buf[..bytes.len()]).unwrap_or("");
    if type_set().contains(up) {
        WordClass::Type
    } else if keyword_set().contains(up) {
        // Funzioni che sono anche parole chiave (es. IF(), REPLACE(), LEFT())
        if followed_by_paren
            && matches!(up, "IF" | "REPLACE" | "LEFT" | "RIGHT" | "INSERT" | "CHAR")
        {
            WordClass::Function
        } else {
            WordClass::Keyword
        }
    } else if followed_by_paren {
        WordClass::Function
    } else {
        WordClass::Plain
    }
}

const FUNCTIONS: &str = "ABS ARRAY_AGG AVG CAST CEIL COALESCE CONCAT CONCAT_WS CONVERT COUNT CURDATE \
CURRENT_DATE CURRENT_TIMESTAMP DATE DATE_ADD DATE_FORMAT DATE_SUB DATE_TRUNC DATEDIFF DENSE_RANK EXTRACT \
FLOOR GREATEST GROUP_CONCAT IFNULL JSON_ARRAYAGG JSON_EXTRACT JSON_OBJECT JSON_UNQUOTE LAG LEAD LEAST LENGTH \
LOWER LPAD LTRIM MAX MIN NOW NULLIF RANK REGEXP_REPLACE ROUND ROW_NUMBER RPAD RTRIM STRING_AGG STR_TO_DATE \
SUBSTRING SUM TO_CHAR TO_DATE TRIM UPPER UUID";

/// Parole proposte dall'autocompletamento (keyword, tipi e funzioni comuni), ordinate.
pub fn completion_words() -> &'static [&'static str] {
    static W: OnceLock<Vec<&'static str>> = OnceLock::new();
    W.get_or_init(|| {
        let mut v: Vec<&'static str> = KEYWORDS
            .split_ascii_whitespace()
            .chain(TYPES.split_ascii_whitespace())
            .chain(FUNCTIONS.split_ascii_whitespace())
            .collect();
        v.sort_unstable();
        v.dedup();
        v
    })
}

/// Parole riservate (MySQL 8 e PostgreSQL): solo queste richiedono la quotatura
/// quando sono usate come identificatori.
const RESERVED: &str = "ADD ALL ALTER ANALYZE AND ANY ARRAY AS ASC ASYMMETRIC BETWEEN BIGINT BINARY BLOB BOTH BY \
CALL CASCADE CASE CAST CHANGE CHAR CHARACTER CHECK COLLATE COLUMN CONDITION CONSTRAINT CONTINUE CONVERT CREATE \
CROSS CUBE CUME_DIST CURRENT_DATE CURRENT_ROLE CURRENT_TIME CURRENT_TIMESTAMP CURRENT_USER CURSOR DATABASE \
DATABASES DECIMAL DECLARE DEFAULT DEFERRABLE DELETE DENSE_RANK DESC DESCRIBE DISTINCT DIV DO DOUBLE DROP DUAL \
EACH ELSE ELSEIF EMPTY END EXCEPT EXISTS EXIT EXPLAIN FALSE FETCH FIRST_VALUE FLOAT FOR FORCE FOREIGN FREEZE \
FROM FULL FULLTEXT FUNCTION GRANT GROUP GROUPING GROUPS HAVING IF IGNORE ILIKE IN INDEX INNER INOUT INSERT INT \
INTEGER INTERSECT INTERVAL INTO IS ITERATE JOIN JSON_TABLE KEY KEYS KILL LAG LAST_VALUE LATERAL LEAD LEADING \
LEAVE LEFT LIKE LIMIT LINEAR LINES LOAD LOCALTIME LOCALTIMESTAMP LOCK LONG LOOP MATCH MOD NATURAL NOT NTILE \
NULL NUMERIC OF OFFSET ON ONLY OPTION OR ORDER OUT OUTER OVER PARTITION PERCENT_RANK PLACING PRECISION PRIMARY \
PROCEDURE PURGE RANGE RANK READ REAL RECURSIVE REFERENCES REGEXP RELEASE RENAME REPEAT REPLACE REQUIRE \
RESIGNAL RESTRICT RETURN RETURNING REVOKE RIGHT RLIKE ROW ROWS ROW_NUMBER SCHEMA SCHEMAS SELECT SESSION_USER \
SET SHOW SIGNAL SIMILAR SMALLINT SOME SPATIAL SQL STARTING STRAIGHT_JOIN SYMMETRIC SYSTEM TABLE TABLESAMPLE \
TERMINATED THEN TINYINT TO TRAILING TRIGGER TRUE UNDO UNION UNIQUE UNLOCK UNSIGNED UPDATE USAGE USE USER \
USING VALUES VARBINARY VARCHAR VARIADIC VERBOSE WHEN WHERE WHILE WINDOW WITH WRITE XOR ZEROFILL";

fn reserved_set() -> &'static HashSet<&'static str> {
    static S: OnceLock<HashSet<&'static str>> = OnceLock::new();
    S.get_or_init(|| set_of(RESERVED))
}

/// Vero se la parola è una keyword o un tipo riservato.
pub fn is_reserved(word: &str) -> bool {
    let up = word.to_ascii_uppercase();
    keyword_set().contains(up.as_str()) || type_set().contains(up.as_str())
}

/// Quota l'identificatore solo se necessario (caratteri speciali o keyword).
pub fn quote_ident_if_needed(name: &str, dialect: Dialect) -> String {
    match dialect {
        Dialect::Postgres | Dialect::Mongo => quote_ident(name, dialect),
        Dialect::MySql => {
            let simple = name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
                && !reserved_set().contains(name.to_ascii_uppercase().as_str());
            if simple {
                name.to_string()
            } else {
                quote_ident(name, dialect)
            }
        }
    }
}

/// Quota un identificatore secondo il dialetto.
pub fn quote_ident(name: &str, dialect: Dialect) -> String {
    match dialect {
        // nella shell i nomi non si quotano: chi chiama usa getCollection("…") se serve
        Dialect::Mongo => name.to_string(),
        Dialect::MySql => format!("`{}`", name.replace('`', "``")),
        Dialect::Postgres => {
            let simple = name
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_lowercase() || c == '_')
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
                && !reserved_set().contains(name.to_ascii_uppercase().as_str());
            if simple {
                name.to_string()
            } else {
                format!("\"{}\"", name.replace('"', "\"\""))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str, d: Dialect) -> Vec<(Tok, &str)> {
        lex(src, d)
            .into_iter()
            .filter(|t| t.kind != Tok::Space)
            .map(|t| (t.kind, &src[t.start..t.end]))
            .collect()
    }

    #[test]
    fn lex_strings_and_comments() {
        let k = kinds(
            "SELECT 'a;b', \"x\" -- c;\n/* ; */ `t;` FROM t;",
            Dialect::MySql,
        );
        assert_eq!(k[1], (Tok::Str, "'a;b'"));
        assert_eq!(k[3], (Tok::Str, "\"x\""));
        assert_eq!(k[4].0, Tok::Comment);
        assert_eq!(k[5], (Tok::Comment, "/* ; */"));
        assert_eq!(k[6], (Tok::QuotedIdent, "`t;`"));
        assert_eq!(k.last().unwrap().0, Tok::Semicolon);
    }

    #[test]
    fn lex_mysql_backslash_escape() {
        let k = kinds(r"SELECT 'it\'s;' ; x", Dialect::MySql);
        assert_eq!(k[1], (Tok::Str, r"'it\'s;'"));
        assert_eq!(k[2].0, Tok::Semicolon);
    }

    #[test]
    fn lex_pg_dollar_quote_and_params() {
        let k = kinds("SELECT $1, $$a;b$$, $f$x$f$, \"Id\"", Dialect::Postgres);
        assert_eq!(k[1], (Tok::Param, "$1"));
        assert_eq!(k[3], (Tok::Str, "$$a;b$$"));
        assert_eq!(k[5], (Tok::Str, "$f$x$f$"));
        assert_eq!(k[7], (Tok::QuotedIdent, "\"Id\""));
    }

    #[test]
    fn lex_char_offsets_with_unicode() {
        let src = "SELECT 'àè' , x";
        let t = lex(src, Dialect::MySql);
        let x = t.iter().find(|t| &src[t.start..t.end] == "x").unwrap();
        assert_eq!(x.cstart, 14);
        assert_eq!(x.start, 16);
    }

    #[test]
    fn split_on_semicolon_and_blank_lines() {
        let src = "select 1;\nselect ';'\n\nselect 3\n  -- c\nfrom t";
        let toks = lex(src, Dialect::MySql);
        let st = split_statements(&toks, true, src);
        let texts: Vec<&str> = st.iter().map(|s| &src[s.start..s.end]).collect();
        assert_eq!(
            texts,
            vec!["select 1", "select ';'", "select 3\n  -- c\nfrom t"]
        );
        let st2 = split_statements(&toks, false, src);
        assert_eq!(st2.len(), 2);
    }

    #[test]
    fn blank_line_inside_parens_does_not_split() {
        let src = "select * from (\n\nselect 1) x";
        let st = split_statements(&lex(src, Dialect::MySql), true, src);
        assert_eq!(st.len(), 1);
    }

    #[test]
    fn statement_under_cursor() {
        let src = "select 1;\nselect 2;";
        let st = split_statements(&lex(src, Dialect::MySql), true, src);
        assert_eq!(statement_at(&st, 3).unwrap().start, 0);
        assert_eq!(statement_at(&st, 9).unwrap().start, 0);
        assert_eq!(statement_at(&st, 12).unwrap().start, 10);
    }

    #[test]
    fn pageable_detection() {
        let d = Dialect::MySql;
        assert_eq!(
            pageable_base("SELECT * FROM t;  ", d).as_deref(),
            Some("SELECT * FROM t")
        );
        assert!(pageable_base("select a from t -- c", d).is_some());
        assert!(pageable_base("SELECT * FROM t LIMIT 10", d).is_none());
        assert!(pageable_base("SELECT * FROM (SELECT * FROM t LIMIT 10) x", d).is_some());
        assert!(pageable_base("SELECT * FROM t FOR UPDATE", d).is_none());
        assert!(pageable_base("WITH x AS (SELECT 1) SELECT * FROM x", d).is_some());
        assert!(pageable_base("(SELECT 1) UNION (SELECT 2)", d).is_some());
        assert!(pageable_base("SHOW TABLES", d).is_none());
        assert!(pageable_base("UPDATE t SET a=1", d).is_none());
        assert!(pageable_base("SELECT 1; SELECT 2", d).is_none());
        assert!(
            pageable_base(
                "WITH d AS (DELETE FROM t RETURNING *) SELECT * FROM d",
                Dialect::Postgres
            )
            .is_none()
        );
        assert!(
            pageable_base(
                "WITH x AS (SELECT 1) INSERT INTO t SELECT * FROM x",
                Dialect::Postgres
            )
            .is_none()
        );
    }

    #[test]
    fn count_queries() {
        let d = Dialect::MySql;
        assert_eq!(
            count_sql(
                "SELECT a.id, b.id FROM a JOIN b ON a.x = b.x WHERE a.y > 1 ORDER BY a.id",
                d
            ),
            "SELECT COUNT(*) FROM a JOIN b ON a.x = b.x WHERE a.y > 1"
        );
        assert_eq!(
            count_sql("SELECT (SELECT max(x) FROM t2 ORDER BY 1) FROM t", d),
            "SELECT COUNT(*) FROM t"
        );
        assert!(count_sql("SELECT DISTINCT a FROM t", d).contains("lemure_count"));
        assert!(count_sql("SELECT a, count(*) FROM t GROUP BY a", d).contains("lemure_count"));
        assert!(count_sql("SELECT 1 UNION SELECT 2", d).contains("lemure_count"));
        assert!(
            count_sql("WITH x AS (SELECT 1) SELECT * FROM x", Dialect::Postgres)
                .contains("lemure_count")
        );
    }

    #[test]
    fn syntax_check_reports_position() {
        assert!(check_syntax("SELECT * FROM t WHERE a = 1", Dialect::MySql).is_none());
        let e = check_syntax("SELECT a FROM t WHERE", Dialect::MySql).unwrap();
        assert_eq!(e.line, 1);
        assert!(e.column > 1, "{e:?}");
        let e = check_syntax("SELECT 1;\nSELECT (1 FROM t", Dialect::Postgres).unwrap();
        assert_eq!(e.line, 2);
    }

    #[test]
    fn word_classes() {
        assert_eq!(classify_word("select", false), WordClass::Keyword);
        assert_eq!(classify_word("varchar", true), WordClass::Type);
        assert_eq!(classify_word("count", true), WordClass::Function);
        assert_eq!(classify_word("customers", false), WordClass::Plain);
        assert_eq!(classify_word("IF", true), WordClass::Function);
    }

    #[test]
    fn quoting() {
        assert_eq!(quote_ident("a`b", Dialect::MySql), "`a``b`");
        assert_eq!(quote_ident("orders", Dialect::Postgres), "orders");
        assert_eq!(quote_ident("Orders", Dialect::Postgres), "\"Orders\"");
        assert_eq!(quote_ident("user", Dialect::Postgres), "\"user\"");
        assert_eq!(quote_ident("status", Dialect::Postgres), "status");
        assert_eq!(quote_ident_if_needed("status", Dialect::MySql), "status");
        assert_eq!(quote_ident_if_needed("order", Dialect::MySql), "`order`");
        assert_eq!(quote_ident_if_needed("my col", Dialect::MySql), "`my col`");
    }
}
