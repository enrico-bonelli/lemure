//! Lemure: client leggero per MySQL, PostgreSQL e MongoDB.
//!
//! La libreria contiene la parte indipendente dalla UI (configurazione,
//! accesso ai database, analisi SQL); l'interfaccia GTK4 è nel binario.

pub mod completion;
pub mod config;
pub mod db;
pub mod mongo;
pub mod sqlutil;
pub mod worker;
