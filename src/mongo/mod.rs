//! Supporto MongoDB: i comandi si scrivono con la sintassi della shell
//! (`db.collection.find({...}).sort({...})`), vengono analizzati in
//! `parse`, eseguiti con il driver sincrono in `exec` e completati in
//! `complete`. I documenti diventano righe della griglia: una colonna per
//! ogni campo di primo livello, i sotto-documenti in JSON.

pub mod complete;
pub mod exec;
pub mod parse;
