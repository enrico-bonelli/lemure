//! Autocompletamento di tabelle, colonne e parole chiave.
//!
//! I metadati dello schema vengono caricati una volta per datasource
//! (`SchemaMeta`); qui c'è solo logica in memoria: analisi del contesto con il
//! lexer SQL, risoluzione degli alias e filtro dei candidati.

use crate::sqlutil::{self, Dialect, Tok, Token};

#[derive(Clone, Debug)]
pub struct CompColumn {
    pub name: String,
    pub type_name: String,
}

#[derive(Clone, Debug)]
pub struct TableMeta {
    pub schema: String,
    pub name: String,
    pub is_view: bool,
    /// `None` = colonne non ancora caricate (schemi molto grandi: caricamento su richiesta).
    pub columns: Option<Vec<CompColumn>>,
}

#[derive(Clone, Debug, Default)]
pub struct SchemaMeta {
    /// Schema/database predefinito della connessione (le sue tabelle non vengono qualificate).
    pub default_schema: String,
    pub schemas: Vec<String>,
    pub tables: Vec<TableMeta>,
}

impl SchemaMeta {
    /// Cerca una tabella (senza distinzione maiuscole/minuscole), preferendo lo schema predefinito.
    pub fn find_table(&self, schema: Option<&str>, name: &str) -> Option<usize> {
        let matches = |t: &TableMeta| {
            t.name.eq_ignore_ascii_case(name)
                && schema.is_none_or(|s| t.schema.eq_ignore_ascii_case(s))
        };
        self.tables
            .iter()
            .position(|t| matches(t) && (schema.is_some() || t.schema == self.default_schema))
            .or_else(|| self.tables.iter().position(matches))
    }

    pub fn set_columns(&mut self, schema: &str, table: &str, cols: Vec<CompColumn>) {
        if let Some(i) = self.find_table(Some(schema), table) {
            self.tables[i].columns = Some(cols);
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ItemKind {
    Column,
    Table,
    View,
    Schema,
    Keyword,
}

#[derive(Clone, Debug)]
pub struct Item {
    pub label: String,
    pub insert: String,
    pub detail: String,
    pub kind: ItemKind,
    /// Chiave alternativa per il filtro (es. `schema.tabella`).
    pub alt: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Context {
    /// Dopo FROM/JOIN/UPDATE/INTO: si propongono tabelle.
    Table,
    /// Dopo `alias.` o `tabella.`: colonne (o tabelle se il qualificatore è uno schema).
    Qualified,
    /// Altrove: colonne delle tabelle della query, tabelle e keyword.
    General,
}

#[derive(Clone, Debug)]
pub struct Completion {
    /// Intervallo (in caratteri) da sostituire con l'elemento scelto.
    pub start: usize,
    pub end: usize,
    pub prefix: String,
    pub context: Context,
    pub items: Vec<Item>,
    /// Tabelle referenziate le cui colonne non sono ancora caricate.
    pub missing: Vec<(String, String)>,
}

/// Riferimento a una tabella nella query (`schema.tabella alias`).
#[derive(Clone, Debug, PartialEq)]
struct TableRef {
    schema: Option<String>,
    table: String,
    alias: Option<String>,
}

const MAX_ITEMS: usize = 300;

fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'`' | b'"') && b[b.len() - 1] == b[0] {
        let q = b[0] as char;
        s[1..s.len() - 1].replace(&format!("{q}{q}"), &q.to_string())
    } else if !b.is_empty() && matches!(b[0], b'`' | b'"') {
        s[1..].to_string()
    } else {
        s.to_string()
    }
}

fn is_ident(t: &Token, text: &str) -> bool {
    match t.kind {
        Tok::QuotedIdent => true,
        Tok::Word => !sqlutil::is_reserved(&text[t.start..t.end]),
        _ => false,
    }
}

fn is_dot(t: &Token, text: &str) -> bool {
    t.kind == Tok::Punct && &text[t.start..t.end] == "."
}

fn upper(t: &Token, text: &str) -> String {
    text[t.start..t.end].to_ascii_uppercase()
}

/// Estrae le tabelle referenziate (FROM, JOIN, UPDATE, INTO) con i loro alias.
fn table_refs(toks: &[Token], text: &str) -> Vec<TableRef> {
    let mut refs = Vec::new();
    let mut expect = false;
    let mut in_from = false;
    let mut i = 0;
    while i < toks.len() {
        let t = &toks[i];
        if t.kind == Tok::Word {
            match upper(t, text).as_str() {
                "FROM" | "JOIN" | "UPDATE" | "INTO" => {
                    in_from = upper(t, text) == "FROM";
                    expect = true;
                    i += 1;
                    continue;
                }
                "WHERE" | "ON" | "SET" | "GROUP" | "ORDER" | "HAVING" | "LIMIT" | "UNION"
                | "SELECT" | "VALUES" | "USING" | "WINDOW" | "RETURNING" => {
                    in_from = false;
                    expect = false;
                }
                _ => {}
            }
        }
        if expect && is_ident(t, text) {
            let mut name = unquote(&text[t.start..t.end]);
            let mut schema = None;
            i += 1;
            if i + 1 < toks.len() && is_dot(&toks[i], text) && is_ident(&toks[i + 1], text) {
                schema = Some(name);
                name = unquote(&text[toks[i + 1].start..toks[i + 1].end]);
                i += 2;
            }
            if i < toks.len() && toks[i].kind == Tok::Word && upper(&toks[i], text) == "AS" {
                i += 1;
            }
            let mut alias = None;
            if i < toks.len() && is_ident(&toks[i], text) {
                alias = Some(unquote(&text[toks[i].start..toks[i].end]));
                i += 1;
            }
            refs.push(TableRef {
                schema,
                table: name,
                alias,
            });
            expect = false;
            continue;
        }
        match t.kind {
            Tok::Punct if &text[t.start..t.end] == "," && in_from => expect = true,
            Tok::OpenParen => expect = false,
            _ => {}
        }
        i += 1;
    }
    refs
}

/// Offset in byte del carattere `ch` (o fine testo).
fn byte_at(text: &str, ch: usize) -> usize {
    text.char_indices()
        .nth(ch)
        .map(|(b, _)| b)
        .unwrap_or(text.len())
}

pub fn complete(
    text: &str,
    cursor: usize,
    dialect: Dialect,
    meta: &SchemaMeta,
    explicit: bool,
) -> Option<Completion> {
    if dialect == Dialect::Mongo {
        return crate::mongo::complete::complete(text, cursor, meta, explicit);
    }
    let all = sqlutil::lex(text, dialect);

    // Nessun completamento dentro stringhe, commenti e numeri.
    if let Some(t) = all.iter().find(|t| t.cstart < cursor && cursor <= t.cend) {
        let s = &text[t.start..t.end];
        let inside = match t.kind {
            Tok::Str => cursor < t.cend || s.len() < 2 || !s.ends_with(&s[..1]),
            Tok::Comment => cursor < t.cend || !s.starts_with("/*"),
            Tok::Number => true,
            _ => false,
        };
        if inside {
            return None;
        }
    }

    // Prefisso: la parola (o identificatore quotato) che termina al cursore.
    let prefix_tok = all.iter().find(|t| {
        t.cstart < cursor && cursor <= t.cend && matches!(t.kind, Tok::Word | Tok::QuotedIdent)
    });
    let (start, raw_prefix) = match prefix_tok {
        Some(t) => (t.cstart, &text[t.start..byte_at(text, cursor)]),
        None => (cursor, ""),
    };
    let quoted = raw_prefix.starts_with('`') || raw_prefix.starts_with('"');
    let prefix = raw_prefix.trim_start_matches(['`', '"']).to_string();

    // Statement corrente: fra il `;` precedente e quello successivo.
    let stmt_start = all
        .iter()
        .rev()
        .find(|t| t.kind == Tok::Semicolon && t.cend <= start)
        .map_or(0, |t| t.cend);
    let stmt_end = all
        .iter()
        .find(|t| t.kind == Tok::Semicolon && t.cstart >= cursor)
        .map_or(usize::MAX, |t| t.cstart);
    let toks: Vec<Token> = all
        .iter()
        .filter(|t| {
            t.cstart >= stmt_start
                && t.cend <= stmt_end
                && !matches!(t.kind, Tok::Space | Tok::Comment)
        })
        .filter(|t| prefix_tok.is_none_or(|p| p.cstart != t.cstart))
        .copied()
        .collect();
    let before: Vec<&Token> = toks.iter().filter(|t| t.cend <= start).collect();
    let refs = table_refs(&toks, text);

    let mut items: Vec<Item> = Vec::new();
    let mut missing: Vec<(String, String)> = Vec::new();
    let quote = |name: &str| {
        if quoted {
            sqlutil::quote_ident(name, dialect)
        } else {
            sqlutil::quote_ident_if_needed(name, dialect)
        }
    };
    let push_columns = |t: &TableMeta,
                        with_table: bool,
                        items: &mut Vec<Item>,
                        missing: &mut Vec<(String, String)>| {
        match &t.columns {
            Some(cols) => {
                for c in cols {
                    items.push(Item {
                        label: c.name.clone(),
                        insert: quote(&c.name),
                        detail: if with_table {
                            format!("{} · {}", t.name, c.type_name)
                        } else {
                            c.type_name.clone()
                        },
                        kind: ItemKind::Column,
                        alt: None,
                    });
                }
            }
            None => missing.push((t.schema.clone(), t.name.clone())),
        }
    };
    let table_item = |t: &TableMeta, qualify: bool| {
        let insert = if qualify && t.schema != meta.default_schema && !t.schema.is_empty() {
            format!("{}.{}", quote(&t.schema), quote(&t.name))
        } else {
            quote(&t.name)
        };
        let other_schema = t.schema != meta.default_schema;
        Item {
            label: t.name.clone(),
            insert,
            detail: if other_schema {
                t.schema.clone()
            } else {
                String::new()
            },
            kind: if t.is_view {
                ItemKind::View
            } else {
                ItemKind::Table
            },
            alt: other_schema.then(|| format!("{}.{}", t.schema, t.name)),
        }
    };

    // Qualificatore: `x.` subito prima del prefisso.
    let qualifier = match before.as_slice() {
        [.., q, dot] if is_dot(dot, text) && matches!(q.kind, Tok::Word | Tok::QuotedIdent) => {
            Some(unquote(&text[q.start..q.end]))
        }
        _ => None,
    };

    let context;
    if let Some(q) = qualifier {
        context = Context::Qualified;
        let by_alias = refs
            .iter()
            .find(|r| {
                r.alias
                    .as_deref()
                    .is_some_and(|a| a.eq_ignore_ascii_case(&q))
            })
            .or_else(|| refs.iter().find(|r| r.table.eq_ignore_ascii_case(&q)));
        let target = match by_alias {
            Some(r) => meta.find_table(r.schema.as_deref(), &r.table),
            None => meta.find_table(None, &q),
        };
        if let Some(i) = target {
            push_columns(&meta.tables[i], false, &mut items, &mut missing);
        } else if meta.schemas.iter().any(|s| s.eq_ignore_ascii_case(&q)) {
            for t in meta
                .tables
                .iter()
                .filter(|t| t.schema.eq_ignore_ascii_case(&q))
            {
                items.push(table_item(t, false));
            }
        } else {
            return None;
        }
    } else {
        let prev = before.last().map(|t| upper(t, text)).unwrap_or_default();
        let prev2 = before
            .len()
            .checked_sub(2)
            .map(|i| upper(before[i], text))
            .unwrap_or_default();
        // FROM a, b: dopo la virgola si è ancora nella lista delle tabelle
        let in_from_list = prev == "," && {
            let last_clause = before.iter().rev().find_map(|t| {
                let u = upper(t, text);
                matches!(
                    u.as_str(),
                    "FROM"
                        | "SELECT"
                        | "WHERE"
                        | "ON"
                        | "SET"
                        | "GROUP"
                        | "ORDER"
                        | "HAVING"
                        | "VALUES"
                        | "BY"
                )
                .then_some(u)
            });
            last_clause.as_deref() == Some("FROM")
        };
        let table_ctx = matches!(
            prev.as_str(),
            "FROM" | "JOIN" | "UPDATE" | "INTO" | "DESCRIBE"
        ) || (prev == "TABLE"
            && matches!(
                prev2.as_str(),
                "ALTER" | "DROP" | "TRUNCATE" | "LOCK" | "ANALYZE"
            ))
            || in_from_list;
        if table_ctx {
            context = Context::Table;
            for t in &meta.tables {
                items.push(table_item(t, true));
            }
            for s in meta.schemas.iter().filter(|s| **s != meta.default_schema) {
                items.push(Item {
                    label: s.clone(),
                    insert: quote(s),
                    detail: "schema".into(),
                    kind: ItemKind::Schema,
                    alt: None,
                });
            }
        } else {
            context = Context::General;
            for r in &refs {
                if let Some(i) = meta.find_table(r.schema.as_deref(), &r.table) {
                    push_columns(&meta.tables[i], true, &mut items, &mut missing);
                }
            }
            if !prefix.is_empty() {
                for t in &meta.tables {
                    items.push(table_item(t, true));
                }
                let lower = prefix.chars().all(|c| !c.is_ascii_uppercase());
                for w in sqlutil::completion_words() {
                    items.push(Item {
                        label: w.to_string(),
                        insert: if lower {
                            w.to_ascii_lowercase()
                        } else {
                            w.to_string()
                        },
                        detail: String::new(),
                        kind: ItemKind::Keyword,
                        alt: None,
                    });
                }
            }
        }
    }

    finish(items, missing, start, cursor, prefix, context, explicit)
}

/// Filtra e ordina i candidati: prima chi inizia con il prefisso, poi (con
/// almeno 2 caratteri) chi lo contiene.
pub(crate) fn finish(
    items: Vec<Item>,
    mut missing: Vec<(String, String)>,
    start: usize,
    cursor: usize,
    prefix: String,
    context: Context,
    explicit: bool,
) -> Option<Completion> {
    let p = prefix.to_lowercase();
    let mut ranked: Vec<(u8, Item)> = items
        .into_iter()
        .filter_map(|it| {
            let l = it.label.to_lowercase();
            let alt = it.alt.as_deref().map(str::to_lowercase).unwrap_or_default();
            if l.starts_with(&p) || (!alt.is_empty() && alt.starts_with(&p)) {
                Some((0, it))
            } else if p.chars().count() >= 2 && l.contains(&p) {
                Some((1, it))
            } else {
                None
            }
        })
        .collect();
    ranked.sort_by(|a, b| {
        (a.0, a.1.kind, a.1.label.to_lowercase(), &a.1.detail).cmp(&(
            b.0,
            b.1.kind,
            b.1.label.to_lowercase(),
            &b.1.detail,
        ))
    });
    ranked.dedup_by(|a, b| {
        a.1.kind == b.1.kind && a.1.insert == b.1.insert && a.1.detail == b.1.detail
    });
    let mut items: Vec<Item> = ranked
        .into_iter()
        .map(|(_, it)| it)
        .take(MAX_ITEMS)
        .collect();

    // Una parola già completa non va riproposta.
    if !explicit && items.len() == 1 && items[0].label.eq_ignore_ascii_case(&prefix) {
        items.clear();
    }
    missing.sort();
    missing.dedup();
    if items.is_empty() && missing.is_empty() {
        return None;
    }
    Some(Completion {
        start,
        end: cursor,
        prefix,
        context,
        items,
        missing,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn meta() -> SchemaMeta {
        let col = |n: &str, t: &str| CompColumn {
            name: n.into(),
            type_name: t.into(),
        };
        SchemaMeta {
            default_schema: "shop".into(),
            schemas: vec!["shop".into(), "archive".into()],
            tables: vec![
                TableMeta {
                    schema: "shop".into(),
                    name: "orders".into(),
                    is_view: false,
                    columns: Some(vec![
                        col("id", "int"),
                        col("customer_id", "int"),
                        col("total", "decimal"),
                    ]),
                },
                TableMeta {
                    schema: "shop".into(),
                    name: "customers".into(),
                    is_view: false,
                    columns: Some(vec![
                        col("id", "int"),
                        col("name", "varchar"),
                        col("order count", "int"),
                    ]),
                },
                TableMeta {
                    schema: "shop".into(),
                    name: "order_items".into(),
                    is_view: false,
                    columns: None,
                },
                TableMeta {
                    schema: "archive".into(),
                    name: "orders_2020".into(),
                    is_view: false,
                    columns: Some(vec![col("id", "int")]),
                },
            ],
        }
    }

    /// `|` indica la posizione del cursore.
    fn run(sql: &str, explicit: bool) -> Option<Completion> {
        let cursor = sql.find('|').unwrap();
        let text = sql.replace('|', "");
        let cur_chars = text[..cursor].chars().count();
        complete(&text, cur_chars, Dialect::MySql, &meta(), explicit)
    }

    fn labels(c: &Completion) -> Vec<String> {
        c.items.iter().map(|i| i.label.clone()).collect()
    }

    #[test]
    fn tables_after_from() {
        let c = run("SELECT * FROM ord|", false).unwrap();
        assert_eq!(c.context, Context::Table);
        assert_eq!(labels(&c), ["order_items", "orders", "orders_2020"]);
        assert_eq!(c.start, 14);
        let archived = c.items.iter().find(|i| i.label == "orders_2020").unwrap();
        assert_eq!(archived.insert, "archive.orders_2020");
    }

    #[test]
    fn tables_after_join_and_comma() {
        assert_eq!(
            run("select * from orders o join cu|", false)
                .unwrap()
                .context,
            Context::Table
        );
        let c = run("select * from orders o, |", true).unwrap();
        assert_eq!(c.context, Context::Table);
    }

    #[test]
    fn columns_by_alias() {
        let c = run(
            "SELECT o.| FROM orders o JOIN customers c ON c.id = o.customer_id",
            false,
        )
        .unwrap();
        assert_eq!(c.context, Context::Qualified);
        assert_eq!(labels(&c), ["customer_id", "id", "total"]);
        let c = run("SELECT * FROM orders AS o JOIN customers c ON c.na|", false).unwrap();
        assert_eq!(labels(&c), ["name"]);
    }

    #[test]
    fn columns_by_table_name_and_quoting() {
        let c = run("SELECT customers.ord| FROM customers", false).unwrap();
        assert_eq!(c.items[0].insert, "`order count`");
    }

    #[test]
    fn general_context_mixes_columns_tables_keywords() {
        let c = run("SELECT to| FROM orders WHERE id = 1", false).unwrap();
        assert_eq!(c.items[0].label, "total");
        assert_eq!(c.items[0].kind, ItemKind::Column);
        assert!(
            c.items
                .iter()
                .any(|i| i.kind == ItemKind::Keyword && i.label == "TO")
        );
        // il caso del prefisso determina quello delle keyword
        assert!(c.items.iter().any(|i| i.insert == "to"));
    }

    #[test]
    fn schema_qualifier_lists_its_tables() {
        let c = run("SELECT * FROM archive.|", false).unwrap();
        assert_eq!(labels(&c), ["orders_2020"]);
        assert_eq!(c.items[0].insert, "orders_2020");
    }

    #[test]
    fn missing_columns_are_reported() {
        let c = run("SELECT oi.| FROM order_items oi", false).unwrap();
        assert!(c.items.is_empty());
        assert_eq!(
            c.missing,
            vec![("shop".to_string(), "order_items".to_string())]
        );
    }

    #[test]
    fn no_completion_in_strings_comments_numbers() {
        assert!(run("SELECT 'ord|' FROM orders", true).is_none());
        assert!(run("SELECT 1 -- ord|", true).is_none());
        assert!(run("SELECT 12|", true).is_none());
    }

    #[test]
    fn only_current_statement_is_considered() {
        let c = run(
            "SELECT * FROM customers c;\nSELECT c.| FROM orders c",
            false,
        )
        .unwrap();
        assert_eq!(labels(&c), ["customer_id", "id", "total"]);
    }

    #[test]
    fn exact_word_is_not_reproposed() {
        assert!(run("SELECT * FROM orders o WHERE o.total|", false).is_none());
    }

    #[test]
    fn contains_match_ranks_after_prefix() {
        let c = run("SELECT * FROM items|", false).unwrap();
        assert_eq!(labels(&c), ["order_items"]);
        let c = run("SELECT * FROM rder|", false).unwrap();
        assert_eq!(labels(&c), ["order_items", "orders", "orders_2020"]);
    }

    #[test]
    fn other_schema_tables_match_by_qualified_name() {
        let c = run("SELECT * FROM arch|", false).unwrap();
        assert_eq!(labels(&c), ["orders_2020", "archive"]);
        assert_eq!(c.items[1].kind, ItemKind::Schema);
    }

    #[test]
    fn quoted_prefix_is_replaced_with_quotes() {
        let c = run("SELECT * FROM `ord|", false).unwrap();
        assert_eq!(c.items[0].insert, "`order_items`");
        assert_eq!(c.start, 14);
    }
}
