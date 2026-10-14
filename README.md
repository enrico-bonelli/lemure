# Lemure

Client SQL desktop per **MySQL** e **PostgreSQL**, leggero e veloce, scritto in Rust con GTK4.

Lemure nasce per il lavoro quotidiano dello sviluppatore: si sceglie un datasource, si scrive la
query con evidenziazione, controllo sintattico e autocompletamento, e si sfogliano i risultati
anche quando sono milioni di righe, senza che l'interfaccia si blocchi. È un'applicazione nativa
(niente Electron né JVM): parte in un istante e occupa poca memoria.

> **Attenzione:** le password dei datasource vengono salvate in chiaro nella cartella di
> configurazione dell'utente. Lemure è pensato per database di sviluppo e test.

## Funzionalità

**Connessioni e schema**
- Elenco di datasource MySQL e PostgreSQL nel pannello a sinistra: aggiunta, modifica,
  duplicazione, eliminazione e test della connessione. Un clic apre la connessione.
- Albero dello schema in ordine alfabetico: datasource → database (se non indicato) → tabelle e
  viste → colonne con tipo, `NOT NULL` e chiave (PRI/UNI/MUL/FK).
- Tasto destro su una tabella:
  - **Analizza tabella**: chiave primaria e tipo di chiave, indici, chiavi esterne uscenti ed
    entranti, colonne, proprietà, DDL;
  - mostra dati, conta righe, copia/inserisci nome.

**Editor SQL**
- Editor a schede con evidenziazione della sintassi, numeri di riga, auto-indentazione e
  `Ctrl+/` per commentare.
- Controllo sintattico in background: l'errore viene sottolineato con riga e colonna.
- **Autocompletamento** di tabelle, viste, schemi, colonne e parole chiave, in base al contesto:
  - dopo `FROM`/`JOIN`/`UPDATE`/`INTO` propone le tabelle;
  - dopo `alias.` propone le colonne della tabella, con gli alias risolti dalla query;
  - altrove propone le colonne delle tabelle usate nella query.

  I metadati vengono caricati una volta su una connessione separata. Con schemi molto grandi
  (oltre 3000 tabelle) le colonne vengono lette solo quando servono.
- **Query e script su file**: apri, salva e salva con nome; si possono anche trascinare i file
  nella finestra. Una scheda con modifiche non salvate mostra `●` e chiede conferma prima di
  chiudersi. Il menu **File recenti** (ultimi 15) ricorda anche il datasource usato con ogni file.
- La sessione (schede e testo delle query) viene salvata automaticamente e ripristinata
  all'avvio.

**Risultati**
- Una scheda risultati in basso per ogni scheda query.
- **Paginazione automatica oltre 500 righe**:
  - le `SELECT` vengono rieseguite con `LIMIT/OFFSET`, mentre il totale delle righe viene contato
    in background;
  - gli statement non paginabili (es. `SHOW`, query con un `LIMIT` proprio) vengono paginati in
    memoria;
  - per navigare ci sono i pulsanti, il numero di pagina, `Alt+←/→`, oppure lo **scorrimento
    automatico**: continuando a scorrere oltre la fine si passa alla pagina successiva.
- Griglia dati virtualizzata:
  - colonne ridimensionabili e riordinabili, con larghezza calcolata sul contenuto;
  - numeri allineati a destra, `NULL` evidenziati, dati binari in esadecimale;
  - copia di una cella oppure di righe intere con i valori separati da `|`;
  - doppio clic per vedere il valore completo (i JSON vengono indentati).
- **Esportazione CSV**:
  - separatore a scelta (`;` predefinito, `,`, tab, `|` o personalizzato) e intestazione
    opzionale;
  - si esportano tutte le righe oppure solo la pagina corrente;
  - le righe vengono scritte in streaming, senza caricarle in memoria (su PostgreSQL tramite
    `COPY`).
- **Stop** della query in corso: `KILL QUERY` su MySQL, cancel request su PostgreSQL.
- Ogni scheda ha la propria connessione, quindi più query possono girare in parallelo.

## Requisiti

- Rust 1.85 o successivo (edizione 2024)
- GTK 4.12 o successivo, con i file di sviluppo, e `pkg-config`

| Distribuzione | Pacchetti |
|---|---|
| Arch Linux | `sudo pacman -S --needed rust gtk4 pkgconf base-devel` |
| Debian / Ubuntu (24.04+) | `sudo apt install cargo libgtk-4-dev pkg-config build-essential` |
| Fedora | `sudo dnf install cargo gtk4-devel pkgconf-pkg-config gcc` |

Non servono librerie client di MySQL o PostgreSQL: i driver sono scritti interamente in Rust.

## Compilazione e installazione

```sh
git clone https://github.com/enrico-bonelli/lemure.git
cd lemure
cargo build --release
./target/release/lemure
```

Per installarlo nel profilo utente (binario in `~/.local/bin`, voce nel menu delle applicazioni e
icona):

```sh
./install.sh
```

Per disinstallarlo basta rimuovere `~/.local/bin/lemure`,
`~/.local/share/applications/it.lemure.Lemure.desktop` e
`~/.local/share/icons/hicolor/scalable/apps/it.lemure.Lemure.svg`.

## Primi passi

1. Crea un datasource con il pulsante **+** in alto a sinistra e premi **Test connessione**.
2. Clicca sul datasource per collegarti ed esplorare tabelle e colonne.
3. Scrivi una query nell'editor e premi `Ctrl+Invio`.

## Scorciatoie

| Tasti | Azione |
|---|---|
| `Ctrl+Invio` | Esegue lo statement sotto il cursore (o la selezione) |
| `Ctrl+Shift+Invio`, `Alt+X` | Esegue l'intero script |
| `Ctrl+T` / `Ctrl+W` | Nuova scheda / chiudi scheda |
| `Ctrl+O` / `Ctrl+S` / `Ctrl+Shift+S` | Apri / salva / salva con nome |
| `F2` o doppio clic sulla scheda | Rinomina la scheda (salva con nome se è legata a un file) |
| `Ctrl+Spazio` | Autocompletamento (`↑/↓` per scegliere, `Invio`/`Tab` per inserire, `Esc` per chiudere) |
| `Ctrl+/` | Commenta/decommenta |
| `Ctrl+C` / `Ctrl+Shift+C` | Copia cella / copia righe con `\|` |
| `Alt+←` / `Alt+→` | Pagina precedente / successiva |
| `Ctrl++` / `Ctrl+-` / `Ctrl+0` | Dimensione dei caratteri |

Gli statement sono separati da `;` oppure da una riga vuota.

## Dove vengono salvati i dati

Tutto in `~/.config/lemure/` (o `$XDG_CONFIG_HOME/lemure/`), in file con permessi `0600`:

| File | Contenuto |
|---|---|
| `datasources.json` | datasource, password comprese (in chiaro) |
| `settings.json` | caratteri, separatore CSV, dimensioni di finestra e pannelli |
| `session.json` | schede aperte: testo, datasource, file associato e modifiche non salvate |
| `recent.json` | ultimi file aperti o salvati, con il datasource usato |

Le query salvate su file restano nella cartella che hai scelto. Alla riapertura, le schede legate
a un file e senza modifiche pendenti vengono rilette dal disco.

Per ridurre l'uso di memoria a scapito della fluidità si può usare il renderer software di GTK:
`GSK_RENDERER=cairo lemure`.

## Architettura

| Modulo | Ruolo |
|---|---|
| `src/db.rs` | driver sincroni (`mysql`, `postgres`) con protocollo testuale, metadati, analisi delle tabelle, export CSV in streaming |
| `src/worker.rs` | un thread per connessione: la UI invia closure e attende il risultato su un canale asincrono, quindi il main loop GTK non si blocca mai |
| `src/sqlutil.rs` | lexer SQL (evidenziazione, separazione degli statement, query paginabili, SQL di conteggio) e controllo sintattico con `sqlparser` |
| `src/completion.rs` | autocompletamento: analisi del contesto e degli alias, filtro dei candidati |
| `src/config.rs` | datasource, impostazioni, sessione e file recenti |
| `src/ui/` | interfaccia GTK4: `ColumnView` virtualizzata per i risultati, `TreeListModel` caricato su richiesta per lo schema |

## Test

```sh
cargo test        # test unitari
```

I test d'integrazione girano contro database reali e vengono saltati se le variabili d'ambiente
non sono impostate. Per eseguirli si possono usare due container usa e getta:

```sh
docker run -d --name lemure-test-mysql -e MYSQL_ROOT_PASSWORD=lemure -e MYSQL_DATABASE=testdb -p 13306:3306 mysql:8.4
docker run -d --name lemure-test-pg -e POSTGRES_PASSWORD=lemure -e POSTGRES_DB=testdb -p 15432:5432 postgres:16
# attendere qualche secondo che i server siano pronti, poi:
docker exec -i lemure-test-mysql mysql -uroot -plemure testdb < testdata/mysql_seed.sql
docker exec -i lemure-test-pg psql -q -U postgres testdb < testdata/pg_seed.sql
LEMURE_TEST_MYSQL=127.0.0.1:13306 LEMURE_TEST_PG=127.0.0.1:15432 cargo test --test live_db
```

La feature `devtools` (`cargo build --features devtools`) abilita uno script di automazione della
UI (`LEMURE_DEVSCRIPT=file`) che può anche salvare screenshot; serve per provare l'interfaccia
senza interazione manuale.

## Licenza

[MIT](LICENSE)
