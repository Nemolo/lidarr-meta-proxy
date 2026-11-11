# lidarr-meta-proxy

Metadata server alternativo per [Lidarr](https://github.com/Lidarr/Lidarr). Si mette al posto di
`api.lidarr.audio` e:

1. inoltra a monte le richieste per entità MusicBrainz, con cache SQLite;
2. crea artisti/album/tracce **assenti da MusicBrainz** partendo da Deezer (primaria) e Spotify (secondaria);
3. completa le discografie degli artisti MusicBrainz a cui mancano album presenti su Deezer/Spotify;
4. fornisce artwork album e foto artisti (anche per le entità MB che ne sono prive), servite dal proxy;
5. riconcilia periodicamente le entità sintetiche quando compaiono su MusicBrainz, e Lidarr le fonde
   tramite `oldIds` senza duplicati e mantenendo i file.

Rust (axum, tokio, sqlx/SQLite WAL, reqwest/rustls), distribuito come container Docker.

## Avvio rapido

```sh
docker compose up -d   # usa ghcr.io/nemolo/lidarr-meta-proxy:latest
# chiave API: Lidarr → Settings → General → Security
LIDARR_URL=http://localhost:8686 LIDARR_API_KEY=... ./scripts/set-metadata-source.sh http://lidarr-meta:5001
```

Lidarr ha un'impostazione nativa `MetadataSource` non visibile in UI; lo script la imposta con
`PUT /api/v1/config/metadataprovider` (serve `jq`). Per tornare al server ufficiale:
`./scripts/set-metadata-source.sh ""`. Nessun plugin è necessario: Lidarr chiama `<MetadataSource>/{route}`.

`PUBLIC_URL` deve essere l'indirizzo con cui **Lidarr** raggiunge il proxy (in compose: `http://lidarr-meta:5001`):
gli URL delle immagini restituiti a Lidarr sono costruiti da lì.

> **Write Audio Tags**: se attivo, Lidarr scrive gli ID come tag MusicBrainz nei file, quindi gli UUID
> sintetici finirebbero nei tag. Tenerlo disattivato, oppure accettare che i tag vengano riscritti dopo
> la riconciliazione.

## Configurazione (variabili d'ambiente)

| Variabile | Default | Descrizione |
|---|---|---|
| `BIND_ADDR` | `0.0.0.0:5001` | Indirizzo di ascolto |
| `PUBLIC_URL` | `http://localhost:<porta>` | URL del proxy visto da Lidarr (per `/img/...`) |
| `DATABASE_PATH` | `/data/meta.db` | Database SQLite (WAL) |
| `IMAGE_DIR` | `/data/images` | Cache immagini |
| `UPSTREAM_URL` | `https://api.lidarr.audio/api/v0.4` | Metadata server ufficiale |
| `UPSTREAM_CACHE_TTL` | `1h` | TTL cache upstream (`90s`, `15m`, `24h`, `7d` o secondi) |
| `SOURCE_TTL` | `7d` | Dopo quanto i dati Deezer/Spotify vengono riletti |
| `DEEZER_ENABLED` | `true` | Sorgente primaria (nessuna chiave) |
| `SPOTIFY_CLIENT_ID` / `SPOTIFY_CLIENT_SECRET` | — | Abilitano Spotify (client credentials) |
| `ITUNES_ENABLED` | `true` | Artwork ad alta risoluzione da iTunes Search |
| `ITUNES_COUNTRY` | `it` | Store iTunes |
| `ITUNES_ARTWORK_SIZE` | `3000` | Lato dell'artwork richiesto a iTunes |
| `MB_USER_AGENT` | `lidarr-meta-proxy/<ver> ( repo url )` | **Mettere un contatto proprio** (policy MusicBrainz) |
| `MB_INTERVAL_MS` | `1000` | Intervallo minimo tra richieste MusicBrainz (non scendere sotto 1000) |
| `RECONCILE_INTERVAL` | `24h` | Intervallo del job di riconciliazione (primo giro dopo 5 min) |
| `PREFETCH_INTERVAL` | `1h` | Intervallo del job di prefetch immagini |
| `ENRICH_MB_ARTISTS` | `true` | Completa le discografie degli artisti MB |
| `SEARCH_LIMIT` | `10` | Risultati Deezer/Spotify per ricerca |
| `SEARCH_MB_LOOKUPS` | `5` | Lookup MB massimi per ricerca (deduplica) |
| `UUID_NAMESPACE` | `6c1da2f0-5b7e-4c3a-9f1e-2d8b0a6c4e51` | Namespace degli UUID v5 sintetici: un UUID, oppure una stringa qualsiasi usata come seed (`uuid5(NAMESPACE_OID, seed)`). **Non cambiarlo dopo il primo avvio** (vedi sotto) |
| `RUST_LOG` | `info` | Livello di log |

Per i test esistono anche `DEEZER_URL`, `SPOTIFY_URL`, `SPOTIFY_AUTH_URL`, `ITUNES_URL`, `MB_URL`.

Il volume `/data` contiene database e immagini: **va salvato**. Gli UUID sintetici sono deterministici
(UUID v5), ma la corrispondenza UUID → ID Deezer/Spotify vive solo nel database.

Il container gira come utente non-root `65532`; con un bind mount (invece del volume nominato) la
cartella deve essere scrivibile da quell'utente (`chown 65532:65532 ./data`).

## Endpoint

| Route | Comportamento |
|---|---|
| `GET /artist/{id}` | MBID → risposta upstream + `oldIds` delle entità riconciliate + album aggiunti + poster integrato. UUID sintetico → costruito da DB/Deezer/Spotify; se riconciliato restituisce l'entità MB con l'UUID sintetico in `oldIds`. |
| `GET /album/{id}` | Analogo. Album sintetico = 1 release (ID diversi), media per numero di disco, `trackCount` coerente, `trackPosition` assoluta. |
| `GET /search?type=artist\|album\|all` | Risultati upstream + Deezer/Spotify, con deduplica (vedi sotto). |
| `POST /search/fingerprint` | Passthrough puro, senza cache. |
| `GET /recent/artist\|album?since=` | Upstream ∪ ID sintetici con `updated_at > since` ∪ artisti MB la cui discografia arricchita è cambiata. Se upstream fallisce → `limited: true` (Lidarr fa refresh completo). |
| `GET /img/{sha256}.{ext}` | Immagini dalla cache locale (`Cache-Control: immutable`). |
| `GET /health` | Stato DB e raggiungibilità upstream (sonda in cache 5 min). 200 se il DB funziona. |
| altro | Passthrough verso upstream. |

Codici: 404 solo se upstream risponde 404 (o un'entità sintetica non esiste); errori upstream/sorgenti → 503,
che Lidarr tratta come temporaneo. Se upstream è giù viene servita la copia in cache anche se scaduta.

### ID

`uuid5(NS, "<sorgente>:<tipo>:<id>")` con `tipo` ∈ `artist`, `album`, `release`, `track`, `recording`
(per le registrazioni l'ID è l'ISRC quando noto). `NS` viene da `UUID_NAMESPACE` (default in `src/ids.rs`).
Il namespace usato viene registrato nel database al primo avvio: se poi la variabile cambia, il proxy si rifiuta
di partire, perché tutti gli ID sintetici già noti a Lidarr cambierebbero. Per usare un namespace diverso
bisogna partire da un database vuoto (e da artisti sintetici non ancora aggiunti in Lidarr).

### Formato upstream

Il server ufficiale risponde con chiavi in case misto (`artistname`, `Albums`, `CoverType`…) e Lidarr le legge
case-insensitive. Il proxy normalizza le chiavi note in camelCase prima di elaborarle (i campi sconosciuti
restano invariati), così nessuna chiave compare due volte.

## Logica

**Artisti/album sintetici.** Discografia da Deezer `artist/{id}/albums` (solo album in cui l'artista è
l'artista principale; varianti regionali con stesso titolo e tipo ridotte a una, scegliendo l'ID più basso così
la scelta è stabile). `record_type` → `type`: `album`→Album, `ep`→EP, `single`→Single, `compile`→Album +
secondary `Compilation`; Spotify non distingue gli EP: un `single` con ≥4 tracce diventa EP.
Tutti gli album sintetici hanno `releaseStatuses: ["Official"]`. Durate: Deezer fornisce solo secondi.

**Arricchimento discografie MB.** Gli account Deezer/Spotify dell'artista si ricavano dai link upstream
(URL-relation MB), in mancanza da WS2 `artist/{mbid}?inc=url-rels`, più eventuali artisti sintetici già
riconciliati su quel MBID. Ogni album della sorgente è abbinato agli album MB con: entità già riconciliata →
titolo normalizzato + anno → stesso titolo con anno diverso se il titolo è univoco (ristampe) → barcode/UPC
(da `release?artist=<mbid>&inc=release-groups`). Solo i non abbinati vengono aggiunti, con `artistId` = MBID.
Deezer ha priorità su Spotify (stesso UPC o stesso titolo+anno). Il calcolo attende al massimo 20 s; se
non basta la risposta parte senza e il calcolo prosegue in background: quando il set di album aggiunti
cambia, l'artista compare in `/recent/artist` e Lidarr lo aggiorna.

**Ricerca e deduplica.** Un risultato Deezer/Spotify che corrisponde a un'entità MB (ID riconciliato, link
già presente nei risultati upstream, URL-relation MB, per gli album anche barcode) è sostituito dall'entità
MB, mai restituito come sintetico. Gli album con stesso artista+titolo di un risultato upstream sono scartati.
I lookup MB sono limitati da `SEARCH_MB_LOOKUPS` (1 req/s); un risultato che non è stato possibile verificare
entro il budget viene **omesso** (meglio un risultato in meno che un duplicato). I risultati album includono
sempre release e tracce: Lidarr scarta album di ricerca senza release.

**Immagini.**
- Artista sintetico: `poster` da Deezer `picture_xl` (per artisti Spotify: immagine più grande).
- Album sintetico: `cover` da iTunes (UPC, poi artista+titolo esatti), altrimenti Deezer `cover_xl`,
  poi Spotify (stesso UPC).
- Entità MB senza `poster`/`cover`: poster dall'artista Deezer/Spotify collegato via URL-relation; cover
  dagli album abbinati nell'arricchimento o via barcode delle release del release group (iTunes, Deezer
  `album/upc:`). Mai per solo nome.

Le immagini vengono scaricate, deduplicate per SHA-256 del contenuto e servite da `PUBLIC_URL/img/<hash>.<ext>`
con `width`/`height` misurati. Dato che l'URL dipende dal contenuto, il download avviene quando l'entità viene
richiesta (per la pagina album e il poster si attende; negli elenchi discografia si usano le immagini già in cache
e le altre partono in background) più un job di prefetch periodico. I fallimenti sono ricordati per 24 h.

**Riconciliazione** (job, default ogni 24 h, MB a 1 req/s):
1. artisti: lookup `url?resource=<link Deezer/Spotify>`;
2. album: URL-relation → barcode → ISRC della prima traccia (con verifica del numero di tracce); la release
   deve essere accreditata all'artista proprietario. Se l'artista sintetico non è stato riconciliato per URL ma la
   release ha un unico artista accreditato, l'artista viene riconciliato con quello;
3. tracce/registrazioni mappate per ISRC, poi per (disco, posizione).

Un MBID viene salvato solo se il server upstream lo conosce già (altrimenti si riprova al giro successivo).
Al salvataggio `updated_at` si aggiorna, l'entità compare in `/recent/*` e da lì in poi viene restituita l'entità MB
con gli UUID sintetici in `oldIds` / `oldRecordingIds` (anche per release e tracce), che i servizi di refresh di
Lidarr usano per riassociare le entità esistenti. Gli album dell'artista non ancora su MB restano sotto l'artista
MB con lo stesso UUID.

## Schema SQLite

`migrations/` (applicate all'avvio con `sqlx::migrate!`): `entity`, `source_link`, `upstream_cache`,
`source_cache` (risposte Deezer/Spotify/MB/iTunes), `image`, `entity_image`, `reconcile_log`, `enrichment`.

## Release

Il workflow `.github/workflows/release.yml` pubblica, per ogni versione:

- l'immagine Docker multi-arch (linux/amd64, linux/arm64) su `ghcr.io/nemolo/lidarr-meta-proxy`
  con i tag `X.Y.Z`, `X.Y` e `latest` (le pre-release, es. `0.2.0-rc.1`, solo con il tag esatto);
- una GitHub Release con note generate dai commit/PR, binari Linux x86_64 e aarch64 (glibc ≥ 2.35)
  e checksum SHA-256.

Per rilasciare: aggiornare `version` in `Cargo.toml` (e `Cargo.lock` con `cargo check`), fare il commit su `main`, poi

```sh
git tag v0.2.0 && git push origin v0.2.0
```

oppure Actions → **Release** → *Run workflow* indicando la versione (il tag viene creato dalla release).
Il workflow si ferma se la versione non coincide con `Cargo.toml` o se il tag esiste già.

## Sviluppo

```sh
cargo test                     # unit + integrazione (wiremock al posto di upstream/Deezer/MB/iTunes)
cargo clippy --all-targets -- -D warnings
RUST_LOG=debug DATABASE_PATH=./data/meta.db IMAGE_DIR=./data/images cargo run
```

I test in `tests/api.rs` verificano il contratto JSON contro i campi delle classi C# in
`NzbDrone.Core/MetadataSource/SkyHook/Resource` (tipi, liste mai nulle, `artistId` presente in `artists`,
`trackCount` = numero di tracce, media coerenti) e coprono passthrough/cache, 503 vs 404, artisti e album
sintetici, immagini, arricchimento, deduplica della ricerca, `/recent` e riconciliazione con `oldIds`.
I mock sono scritti a mano sul formato documentato delle API; `scripts/record-fixtures.sh` salva risposte
reali in `tests/fixtures/recorded/` per confrontarli.

## Stato delle milestone

| | |
|---|---|
| M1 Passthrough + cache | implementato, testato con mock |
| M2 Sintetici da Deezer | implementato, testato con mock |
| M3 Immagini | implementato, testato con mock |
| M4 Arricchimento MB + Spotify + deduplica | implementato; Spotify non coperto da test |
| M5 Riconciliazione | implementato, fusione verificata a livello di risposte (stub MB) |

Ancora da fare: test E2E con un Lidarr reale (aggiunta artista sintetico, import, fusione dopo riconciliazione)
e registrazione di fixture reali.

## Limiti e note

- Le entità sintetiche non sono trovabili via fingerprint AcoustID.
- Le API Spotify per app in development mode sono ridotte e cambiano nel tempo: ogni 403 viene trattato
  come "nessun dato" e il proxy prosegue con Deezer.
- Rate limit: Deezer ~40 richieste/5 s (limite documentato 50), iTunes 20/min, Spotify 5/s, MusicBrainz 1/s.
- Immagini da CDN di terzi: solo uso personale.
- Inserimento manuale di entità: fuori scope.
