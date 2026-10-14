//! Lemure: client SQL leggero per MySQL e PostgreSQL.
//!
//! La libreria contiene la parte indipendente dalla UI (configurazione,
//! accesso ai database, analisi SQL); l'interfaccia GTK4 è nel binario.

pub mod completion;
pub mod config;
pub mod db;
pub mod sqlutil;
pub mod worker;
