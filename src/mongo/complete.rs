//! Autocompletamento per la sintassi della shell di MongoDB.
//!
//! Il contesto si ricava dai token prima del cursore: dopo `db.` si
//! propongono le collection, dopo `db.coll.` i metodi, dentro gli oggetti
//! (in posizione di chiave) i campi della collection e gli operatori adatti
//! al punto in cui ci si trova (filtro, aggiornamento, pipeline).

use super::parse::is_identifier;
use crate::completion::{Completion, Context, Item, ItemKind, SchemaMeta, finish};
use crate::sqlutil::{self, Dialect, Tok, Token};

const DB_METHODS: &[(&str, &str)] = &[
    ("getCollection(", "getCollection(nome)"),
    ("getCollectionNames()", "elenco delle collection"),
    ("getSiblingDB(", "getSiblingDB(database)"),
    ("runCommand(", "runCommand({comando})"),
    ("adminCommand(", "adminCommand({comando})"),
    ("createCollection(", "createCollection(nome, opzioni)"),
    ("createView(", "createView(nome, origine, pipeline)"),
    ("stats()", "statistiche del database"),
    ("serverStatus()", "stato del server"),
    ("version()", "versione del server"),
    ("dropDatabase()", "elimina il database"),
];

const COLL_METHODS: &[(&str, &str)] = &[
    ("find(", "find(filtro, proiezione)"),
    ("findOne(", "findOne(filtro, proiezione)"),
    ("aggregate([", "aggregate([pipeline])"),
    ("countDocuments(", "countDocuments(filtro)"),
    ("estimatedDocumentCount()", "conteggio dai metadati"),
    ("distinct(", "distinct(campo, filtro)"),
    ("insertOne(", "insertOne({documento})"),
    ("insertMany([", "insertMany([documenti])"),
    ("updateOne(", "updateOne(filtro, {$set: …}, opzioni)"),
    ("updateMany(", "updateMany(filtro, {$set: …}, opzioni)"),
    ("replaceOne(", "replaceOne(filtro, documento)"),
    ("deleteOne(", "deleteOne(filtro)"),
    ("deleteMany(", "deleteMany(filtro)"),
    ("createIndex(", "createIndex({campo: 1}, opzioni)"),
    ("dropIndex(", "dropIndex(nome)"),
    ("getIndexes()", "elenco degli indici"),
    ("stats()", "statistiche della collection"),
    ("renameCollection(", "renameCollection(nuovo nome)"),
    ("drop()", "elimina la collection"),
];

const CURSOR_METHODS: &[(&str, &str)] = &[
    ("sort(", "sort({campo: 1})"),
    ("limit(", "limit(n)"),
    ("skip(", "skip(n)"),
    ("projection(", "projection({campo: 1})"),
    ("count()", "numero di documenti"),
    ("explain(", "explain(\"executionStats\")"),
    ("hint(", "hint(indice)"),
    ("maxTimeMS(", "maxTimeMS(ms)"),
];

const AGG_CHAIN: &[(&str, &str)] = &[
    ("explain(", "explain(\"executionStats\")"),
    ("count()", "numero di documenti"),
    ("maxTimeMS(", "maxTimeMS(ms)"),
];

const QUERY_TOP: &[&str] = &[
    "$and",
    "$or",
    "$nor",
    "$expr",
    "$text",
    "$where",
    "$jsonSchema",
    "$comment",
];
const QUERY_FIELD: &[&str] = &[
    "$eq",
    "$ne",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$in",
    "$nin",
    "$exists",
    "$type",
    "$regex",
    "$options",
    "$not",
    "$elemMatch",
    "$size",
    "$all",
    "$mod",
];
const UPDATE_OPS: &[&str] = &[
    "$set",
    "$unset",
    "$inc",
    "$mul",
    "$rename",
    "$setOnInsert",
    "$min",
    "$max",
    "$currentDate",
    "$push",
    "$pull",
    "$addToSet",
    "$pop",
    "$pullAll",
];
const STAGES: &[&str] = &[
    "$match",
    "$project",
    "$group",
    "$sort",
    "$limit",
    "$skip",
    "$unwind",
    "$lookup",
    "$addFields",
    "$set",
    "$unset",
    "$count",
    "$facet",
    "$bucket",
    "$sortByCount",
    "$replaceRoot",
    "$sample",
    "$out",
    "$merge",
    "$graphLookup",
    "$unionWith",
];
const EXPRESSIONS: &[&str] = &[
    "$sum",
    "$avg",
    "$min",
    "$max",
    "$first",
    "$last",
    "$push",
    "$addToSet",
    "$cond",
    "$ifNull",
    "$concat",
    "$toUpper",
    "$toLower",
    "$substr",
    "$dateToString",
    "$size",
    "$arrayElemAt",
    "$filter",
    "$map",
    "$add",
    "$subtract",
    "$multiply",
    "$divide",
    "$eq",
    "$ne",
    "$gt",
    "$gte",
    "$lt",
    "$lte",
    "$and",
    "$or",
    "$not",
    "$in",
    "$toString",
    "$toInt",
    "$year",
    "$month",
    "$dayOfMonth",
];
const INDEX_OPTIONS: &[&str] = &[
    "unique",
    "name",
    "sparse",
    "expireAfterSeconds",
    "partialFilterExpression",
    "collation",
];
const VALUES: &[&str] = &[
    "ObjectId(",
    "ISODate(",
    "new Date(",
    "NumberLong(",
    "NumberInt(",
    "NumberDecimal(",
    "UUID(",
    "true",
    "false",
    "null",
];

fn byte_at(text: &str, chars: usize) -> usize {
    text.char_indices()
        .nth(chars)
        .map_or(text.len(), |(i, _)| i)
}

fn kw(label: &str, detail: &str) -> Item {
    Item {
        label: label
            .trim_end_matches("()")
            .trim_end_matches(['(', '[', '.', ' '])
            .to_string(),
        insert: label.to_string(),
        detail: detail.to_string(),
        kind: ItemKind::Keyword,
        alt: None,
    }
}

fn op_items(ops: &[&str], detail: &str) -> Vec<Item> {
    ops.iter().map(|o| kw(o, detail)).collect()
}

/// Database e collection a cui si riferisce il comando (`db.x.`, `db.getCollection("x").`).
#[derive(Default)]
struct Head {
    db: Option<String>,
    coll: Option<String>,
    /// Indice (in `toks`) del primo token dopo la testa: il punto prima del metodo.
    end: usize,
}

fn unquote(s: &str) -> String {
    let t = &s[1..];
    t.strip_suffix(&s[..1]).unwrap_or(t).to_string()
}

fn head(toks: &[&Token], text: &str) -> Option<Head> {
    let word = |i: usize| toks.get(i).map(|t| &text[t.start..t.end]);
    if word(0) != Some("db") {
        return None;
    }
    let mut h = Head::default();
    let mut segments: Vec<String> = Vec::new();
    let mut i = 1;
    loop {
        match word(i) {
            Some(".") => {
                let Some(name) = word(i + 1).filter(|_| toks[i + 1].kind == Tok::Word) else {
                    break;
                };
                if toks.get(i + 2).is_some_and(|t| t.kind == Tok::OpenParen) {
                    let arg = toks
                        .get(i + 3)
                        .filter(|t| t.kind == Tok::Str)
                        .map(|t| unquote(&text[t.start..t.end]));
                    let closed = toks.get(i + 4).is_some_and(|t| t.kind == Tok::CloseParen);
                    match (name, arg) {
                        ("getCollection", Some(c)) if closed && segments.is_empty() => {
                            segments.push(c);
                            i += 5;
                        }
                        ("getSiblingDB", Some(d)) if closed && segments.is_empty() => {
                            h.db = Some(d);
                            i += 5;
                        }
                        _ => break,
                    }
                } else {
                    segments.push(name.to_string());
                    i += 2;
                }
            }
            Some("[") => {
                let arg = toks
                    .get(i + 1)
                    .filter(|t| t.kind == Tok::Str)
                    .map(|t| unquote(&text[t.start..t.end]));
                match (arg, word(i + 2)) {
                    (Some(c), Some("]")) => {
                        segments.push(c);
                        i += 3;
                    }
                    _ => break,
                }
            }
            _ => break,
        }
    }
    h.end = i;
    h.coll = (!segments.is_empty()).then(|| segments.join("."));
    Some(h)
}

/// Livello di annidamento: parentesi di una chiamata, oggetto o array.
struct Frame {
    kind: char,
    /// Per le chiamate: nome del metodo e indice dell'argomento corrente.
    method: String,
    arg: usize,
    /// Per oggetti e array: la chiave di cui sono il valore (`$match: {` → "$match").
    key: Option<String>,
}

pub fn complete(
    text: &str,
    cursor: usize,
    meta: &SchemaMeta,
    explicit: bool,
) -> Option<Completion> {
    let all = sqlutil::lex(text, Dialect::Mongo);
    let cur_b = byte_at(text, cursor);
    let at = all.iter().find(|t| t.cstart < cursor && cursor <= t.cend);
    let mut in_string = false;
    let (start, prefix) = match at {
        Some(t) if matches!(t.kind, Tok::Comment | Tok::Number) => return None,
        Some(t) if t.kind == Tok::Str => {
            let s = &text[t.start..t.end];
            let closed = s.len() >= 2 && s.ends_with(&s[..1]) && cursor == t.cend;
            if closed || s.starts_with('/') {
                return None;
            }
            in_string = true;
            (t.cstart + 1, text[t.start + 1..cur_b].to_string())
        }
        Some(t) if t.kind == Tok::Word => (t.cstart, text[t.start..cur_b].to_string()),
        _ => (cursor, String::new()),
    };

    let stmt_start = sqlutil::split_statements_in(&all, true, text, Dialect::Mongo)
        .iter()
        .filter(|s| s.cstart <= start)
        .map(|s| s.cstart)
        .max()
        .unwrap_or(0);
    let before: Vec<&Token> = all
        .iter()
        .filter(|t| {
            t.cstart >= stmt_start
                && t.cend <= start
                && !matches!(t.kind, Tok::Space | Tok::Comment)
        })
        .collect();
    let word = |t: &Token| &text[t.start..t.end];
    let last = before.last().map(|t| word(t));

    let mut items: Vec<Item> = Vec::new();
    let mut missing: Vec<(String, String)> = Vec::new();
    let mut context = Context::General;

    // campi della collection del comando (caricati su richiesta)
    let fields = |h: &Head, missing: &mut Vec<(String, String)>| -> Vec<(String, String)> {
        let Some(coll) = &h.coll else {
            return Vec::new();
        };
        let Some(i) = meta.find_table(h.db.as_deref(), coll) else {
            return Vec::new();
        };
        let t = &meta.tables[i];
        match &t.columns {
            Some(cols) => cols
                .iter()
                .map(|c| (c.name.clone(), c.type_name.clone()))
                .collect(),
            None => {
                missing.push((t.schema.clone(), t.name.clone()));
                Vec::new()
            }
        }
    };

    if in_string
        && last != Some(":")
        && !matches!(last, Some("{" | ","))
        && !prefix.starts_with('$')
    {
        return None;
    }

    match before.as_slice() {
        [] if !in_string => {
            for (w, d) in [
                ("db.", "database corrente"),
                ("show ", "dbs | collections"),
                ("use ", "cambia database"),
            ] {
                items.push(kw(w, d));
            }
        }
        [s] if word(s) == "show" => {
            context = Context::Table;
            for w in ["dbs", "collections"] {
                items.push(kw(w, ""));
            }
        }
        [u] if word(u) == "use" => {
            context = Context::Table;
            for s in &meta.schemas {
                items.push(Item {
                    label: s.clone(),
                    insert: s.clone(),
                    detail: "database".into(),
                    kind: ItemKind::Schema,
                    alt: None,
                });
            }
        }
        _ => {
            let h = head(&before, text)?;
            if last == Some(".") && !in_string && h.end + 1 == before.len() {
                // subito dopo la testa: collection o metodi
                match &h.coll {
                    None => {
                        context = Context::Table;
                        let db = h.db.as_deref().unwrap_or(&meta.default_schema);
                        for t in meta.tables.iter().filter(|t| t.schema == db) {
                            items.push(Item {
                                label: t.name.clone(),
                                insert: if is_identifier(&t.name) {
                                    t.name.clone()
                                } else {
                                    format!("getCollection(\"{}\")", t.name.replace('"', "\\\""))
                                },
                                detail: if t.is_view { "vista" } else { "collection" }.into(),
                                kind: if t.is_view {
                                    ItemKind::View
                                } else {
                                    ItemKind::Table
                                },
                                alt: None,
                            });
                        }
                        items.extend(DB_METHODS.iter().map(|(m, d)| kw(m, d)));
                    }
                    Some(_) => {
                        context = Context::Qualified;
                        items.extend(COLL_METHODS.iter().map(|(m, d)| kw(m, d)));
                    }
                }
            } else {
                context = args_context(
                    &before[h.end..],
                    text,
                    &h,
                    in_string,
                    &prefix,
                    &mut items,
                    &fields,
                    &mut missing,
                )?;
            }
        }
    }
    finish(items, missing, start, cursor, prefix, context, explicit)
}

/// Completamento dopo il metodo: concatenazioni (`).sort(`) e argomenti.
#[allow(clippy::too_many_arguments)]
fn args_context(
    toks: &[&Token],
    text: &str,
    h: &Head,
    in_string: bool,
    prefix: &str,
    items: &mut Vec<Item>,
    fields: &dyn Fn(&Head, &mut Vec<(String, String)>) -> Vec<(String, String)>,
    missing: &mut Vec<(String, String)>,
) -> Option<Context> {
    let word = |t: &Token| &text[t.start..t.end];
    let mut stack: Vec<Frame> = Vec::new();
    let mut first_method: Option<String> = None;
    for (i, t) in toks.iter().enumerate() {
        let w = word(t);
        let key_before = || {
            (i >= 2 && word(toks[i - 1]) == ":").then(|| {
                let k = toks[i - 2];
                if k.kind == Tok::Str {
                    unquote(word(k))
                } else {
                    word(k).to_string()
                }
            })
        };
        match (t.kind, w) {
            (Tok::OpenParen, _) => {
                let method = if i >= 1 && toks[i - 1].kind == Tok::Word {
                    word(toks[i - 1]).to_string()
                } else {
                    String::new()
                };
                if stack.is_empty() && first_method.is_none() {
                    first_method = Some(method.clone());
                }
                stack.push(Frame {
                    kind: '(',
                    method,
                    arg: 0,
                    key: None,
                });
            }
            (Tok::Punct, "{" | "[") => stack.push(Frame {
                kind: w.chars().next().unwrap(),
                method: String::new(),
                arg: 0,
                key: key_before(),
            }),
            (Tok::CloseParen, _) | (Tok::Punct, "}" | "]") => {
                stack.pop();
            }
            (Tok::Punct, ",") => {
                if let Some(f) = stack.last_mut() {
                    f.arg += 1;
                }
            }
            _ => {}
        }
    }
    let last = toks.last().map(|t| word(t));
    let first = first_method.unwrap_or_default();

    // `.metodo` concatenato dopo la chiamata principale
    if stack.is_empty() {
        if last == Some(".")
            && toks.len() >= 2
            && toks[toks.len() - 2].kind == Tok::CloseParen
            && !in_string
        {
            let list = if first == "aggregate" {
                AGG_CHAIN
            } else {
                CURSOR_METHODS
            };
            if matches!(first.as_str(), "find" | "aggregate") {
                items.extend(list.iter().map(|(m, d)| kw(m, d)));
                return Some(Context::Qualified);
            }
        }
        return value_items(items, in_string, prefix);
    }

    let top = stack.last().unwrap();
    let call = stack.iter().rev().find(|f| f.kind == '(').unwrap();
    let key_pos = top.kind == '{' && matches!(last, Some("{" | ","));
    let field_list = fields(h, missing);

    // "$campo" come valore (pipeline e espressioni)
    if in_string && prefix.starts_with('$') && !key_pos {
        for (f, t) in &field_list {
            items.push(Item {
                label: format!("${f}"),
                insert: format!("${f}"),
                detail: t.clone(),
                kind: ItemKind::Column,
                alt: None,
            });
        }
        return Some(Context::Qualified);
    }
    if !key_pos {
        return if in_string {
            None
        } else {
            value_items(items, false, prefix)
        };
    }

    let push_fields = |items: &mut Vec<Item>| {
        for (f, t) in &field_list {
            items.push(Item {
                label: f.clone(),
                insert: if in_string || is_identifier(f) {
                    f.clone()
                } else {
                    format!("\"{f}\"")
                },
                detail: t.clone(),
                kind: ItemKind::Column,
                alt: None,
            });
        }
    };
    // profondità degli oggetti dentro la chiamata
    let depth = stack.iter().rev().take_while(|f| f.kind != '(').count();
    let parent_key = top.key.as_deref().unwrap_or("");
    let ops: Vec<Item> = match call.method.as_str() {
        "aggregate" if depth == 2 && stack[stack.len() - 2].kind == '[' => {
            op_items(STAGES, "stadio")
        }
        "aggregate" => match parent_key {
            "$match" => {
                push_fields(items);
                op_items(QUERY_TOP, "")
            }
            "$group" | "$project" | "$addFields" | "$set" | "$sort" | "$unset" | "$bucket"
            | "$facet" => {
                push_fields(items);
                op_items(EXPRESSIONS, "espressione")
            }
            k if k.starts_with('$') => op_items(EXPRESSIONS, "espressione"),
            _ => {
                push_fields(items);
                op_items(QUERY_FIELD, "")
            }
        },
        "updateOne" | "updateMany" | "update" if call.arg == 1 => {
            if depth == 1 {
                op_items(UPDATE_OPS, "aggiornamento")
            } else {
                push_fields(items);
                Vec::new()
            }
        }
        "createIndex" if call.arg == 1 => op_items(INDEX_OPTIONS, "opzione"),
        "find" | "findOne" if call.arg == 1 => {
            push_fields(items);
            Vec::new()
        }
        "sort" | "projection" | "createIndex" | "insertOne" | "insertMany" | "replaceOne" => {
            push_fields(items);
            Vec::new()
        }
        _ => {
            // filtro: campi e operatori logici al primo livello, operatori di confronto dentro un campo
            if depth == 1 || matches!(parent_key, "$and" | "$or" | "$nor" | "") {
                push_fields(items);
                op_items(QUERY_TOP, "")
            } else {
                op_items(QUERY_FIELD, "")
            }
        }
    };
    // gli operatori solo quando si inizia a scriverli
    if prefix.starts_with('$') || items.is_empty() {
        items.extend(ops);
    }
    Some(Context::Qualified)
}

/// Valori (costruttori e costanti) in posizione di valore.
fn value_items(items: &mut Vec<Item>, in_string: bool, prefix: &str) -> Option<Context> {
    if in_string || prefix.is_empty() {
        return Some(Context::General);
    }
    items.extend(VALUES.iter().map(|v| kw(v, "")));
    Some(Context::General)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{CompColumn, TableMeta};

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
                        col("_id", "objectId"),
                        col("status", "string"),
                        col("total", "double"),
                        col("customer.name", "string"),
                    ]),
                },
                TableMeta {
                    schema: "shop".into(),
                    name: "order-log".into(),
                    is_view: false,
                    columns: None,
                },
            ],
        }
    }

    /// Completa nel punto indicato da `|`.
    fn comp(src: &str) -> Option<Completion> {
        let cur = src.find('|').unwrap();
        let text = src.replace('|', "");
        complete(&text, text[..cur].chars().count(), &meta(), false)
    }

    fn labels(c: &Completion) -> Vec<&str> {
        c.items.iter().map(|i| i.label.as_str()).collect()
    }

    #[test]
    fn collections_after_db() {
        let c = comp("db.|").unwrap();
        assert_eq!(c.context, Context::Table);
        let l = labels(&c);
        assert!(l.contains(&"orders") && l.contains(&"getCollection"));
        let log = c.items.iter().find(|i| i.label == "order-log").unwrap();
        assert_eq!(log.insert, "getCollection(\"order-log\")");
        let c = comp("db.ord|").unwrap();
        assert_eq!(labels(&c)[..2], ["order-log", "orders"]);
    }

    #[test]
    fn methods() {
        let c = comp("db.orders.f|").unwrap();
        assert_eq!(labels(&c)[..2], ["find", "findOne"]);
        assert_eq!(c.items[0].insert, "find(");
        let c = comp("db.orders.find({}).|").unwrap();
        assert!(labels(&c).contains(&"sort"));
        let c = comp("db.getCollection('orders').cou|").unwrap();
        assert_eq!(labels(&c), ["countDocuments", "estimatedDocumentCount"]);
    }

    #[test]
    fn fields_in_filters() {
        let c = comp("db.orders.find({|").unwrap();
        assert_eq!(c.context, Context::Qualified);
        let l = labels(&c);
        assert!(l.contains(&"status") && !l.contains(&"$or"));
        let dotted = c.items.iter().find(|i| i.label == "customer.name").unwrap();
        assert_eq!(dotted.insert, "\"customer.name\"");
        let c = comp("db.orders.find({status: 'paid', tot|").unwrap();
        assert_eq!(labels(&c), ["total"]);
        let c = comp("db.orders.find({total: {$g|").unwrap();
        assert_eq!(labels(&c), ["$gt", "$gte"]);
        let c = comp("db.orders.find({\"cust|").unwrap();
        assert_eq!(c.items[0].insert, "customer.name");
        assert!(comp("db.orders.find({status: 'pa|").is_none());
    }

    #[test]
    fn pipeline_and_updates() {
        let c = comp("db.orders.aggregate([{$m|").unwrap();
        assert_eq!(labels(&c), ["$match", "$merge"]);
        let c = comp("db.orders.aggregate([{$group: {_id: \"$st|").unwrap();
        assert_eq!(labels(&c), ["$status"]);
        let c = comp("db.orders.updateOne({}, {$s|").unwrap();
        assert_eq!(labels(&c), ["$set", "$setOnInsert"]);
        let c = comp("db.orders.updateOne({}, {$set: {to|").unwrap();
        assert_eq!(labels(&c)[0], "total");
    }

    #[test]
    fn lazy_fields_and_misc() {
        let c = comp("db.getCollection('order-log').find({|").unwrap();
        assert_eq!(
            c.missing,
            vec![("shop".to_string(), "order-log".to_string())]
        );
        let c = comp("use |").unwrap();
        assert!(labels(&c).contains(&"archive"));
        let c = comp("db.orders.find({_id: Obj|").unwrap();
        assert_eq!(labels(&c), ["ObjectId"]);
        assert!(comp("db.orders.find({a: 1}) // db.|").is_none());
    }
}
