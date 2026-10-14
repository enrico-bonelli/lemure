//! Ogni connessione al database vive su un thread dedicato. La UI invia
//! closure da eseguire e attende il risultato con un canale asincrono, così il
//! main loop GTK non viene mai bloccato.

use crate::config::DataSource;
use crate::db::{CancelHandle, DbConn, DbResult};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

type Job = Box<dyn FnOnce(&mut DbConn) + Send>;

pub struct Worker {
    tx: mpsc::Sender<Job>,
    cancel: Arc<Mutex<Option<CancelHandle>>>,
    pub server_version: String,
}

/// Dopo questo periodo di inattività la connessione viene verificata (e
/// ristabilita se il server l'ha chiusa) prima di eseguire un nuovo job.
const IDLE_CHECK: Duration = Duration::from_secs(15);

impl Worker {
    pub async fn connect(ds: DataSource) -> DbResult<Worker> {
        let (res_tx, res_rx) = async_channel::bounded::<DbResult<(mpsc::Sender<Job>, String)>>(1);
        let cancel: Arc<Mutex<Option<CancelHandle>>> = Arc::default();
        let cancel_thread = cancel.clone();
        std::thread::Builder::new()
            .name(format!("db-{}", ds.name))
            .spawn(move || {
                let mut conn = match DbConn::open(&ds) {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = res_tx.send_blocking(Err(e));
                        return;
                    }
                };
                let version = conn.server_version();
                *cancel_thread.lock().unwrap() = Some(conn.cancel_handle());
                let (tx, rx) = mpsc::channel::<Job>();
                if res_tx.send_blocking(Ok((tx, version))).is_err() {
                    return;
                }
                let mut last_used = Instant::now();
                for job in rx {
                    if last_used.elapsed() > IDLE_CHECK
                        && !conn.is_alive()
                        && let Ok(c) = DbConn::open(&ds)
                    {
                        conn = c;
                        *cancel_thread.lock().unwrap() = Some(conn.cancel_handle());
                    }
                    job(&mut conn);
                    last_used = Instant::now();
                }
            })
            .map_err(|e| e.to_string())?;
        let (tx, server_version) = res_rx
            .recv()
            .await
            .map_err(|_| "Thread di connessione terminato".to_string())??;
        Ok(Worker {
            tx,
            cancel,
            server_version,
        })
    }

    /// Esegue `f` sul thread della connessione e ne restituisce il risultato.
    pub async fn run<R, F>(&self, f: F) -> DbResult<R>
    where
        R: Send + 'static,
        F: FnOnce(&mut DbConn) -> R + Send + 'static,
    {
        let (tx, rx) = async_channel::bounded(1);
        self.tx
            .send(Box::new(move |c| {
                let _ = tx.send_blocking(f(c));
            }))
            .map_err(|_| "Connessione chiusa".to_string())?;
        rx.recv()
            .await
            .map_err(|_| "Connessione interrotta".to_string())
    }

    /// Interrompe la query in corso (KILL QUERY / cancel request) da un thread separato.
    pub fn cancel(&self) {
        if let Some(h) = self.cancel.lock().unwrap().clone() {
            std::thread::spawn(move || {
                if let Err(e) = h.cancel() {
                    eprintln!("lemure: annullamento fallito: {e}");
                }
            });
        }
    }
}

/// Esegue un job su una connessione nuova e temporanea (es. conteggio righe),
/// senza occupare la connessione della scheda.
pub async fn run_detached<R, F>(ds: DataSource, f: F) -> DbResult<R>
where
    R: Send + 'static,
    F: FnOnce(&mut DbConn) -> DbResult<R> + Send + 'static,
{
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let res = DbConn::open(&ds).and_then(|mut c| f(&mut c));
        let _ = tx.send_blocking(res);
    });
    rx.recv()
        .await
        .map_err(|_| "Thread terminato".to_string())?
}
