//! beng_leecher — download (non-DRM) HLS video from De Schatkamer (Beeld & Geluid).
//!
//! Flow per job:
//!   1. Fetch the episode page HTML and extract the CloudFront-signed master .m3u8 URL.
//!   2. GET that signed URL once; CloudFront answers 302 + Set-Cookie (signed cookies).
//!   3. Hand the (unsigned) master URL + those cookies to ffmpeg, which follows the
//!      variant/audio playlists, downloads every segment and muxes video+audio to MP4.

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use axum::extract::{Path, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use regex::Regex;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::process::Command;
use tokio_util::io::ReaderStream;

const UA: &str = "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 \
                  (KHTML, like Gecko) Chrome/124.0 Safari/537.36";
const DEFAULT_ADDR: &str = "0.0.0.0:3380";
const DEFAULT_MEDIA_DIR: &str = "media";
const DEFAULT_CACHE_TTL: &str = "60m";
/// Working directory for browser-bound downloads; subject to the cache TTL.
const DOWNLOAD_DIR: &str = "downloads";

/// Runtime configuration assembled from CLI arguments.
#[derive(Clone)]
struct Config {
    addr: String,
    /// Where "save to media server" downloads are kept permanently.
    media_dir: String,
    /// Seconds to keep files in DOWNLOAD_DIR before purging; 0 disables purging.
    cache_ttl_secs: u64,
}

#[derive(Clone, Serialize)]
struct Job {
    status: String,
    progress: f32,
    message: String,
    title: String,
    filename: String,
    done: bool,
    error: bool,
    /// True when the file is permanently kept in the media library.
    kept: bool,
    /// On-disk path of the finished file (set on success). Not exposed to clients.
    #[serde(skip)]
    path: String,
}

impl Job {
    fn new() -> Self {
        Job {
            status: "In wachtrij".into(),
            progress: 0.0,
            message: String::new(),
            title: String::new(),
            filename: String::new(),
            done: false,
            error: false,
            kept: false,
            path: String::new(),
        }
    }
}

#[derive(Clone)]
struct AppState {
    jobs: Arc<Mutex<HashMap<String, Job>>>,
    client: reqwest::Client,
    cfg: Config,
}

#[derive(Deserialize)]
struct DownloadReq {
    url: String,
    /// When true, keep the file permanently in the media library on the server.
    #[serde(default)]
    keep: bool,
}

#[derive(Serialize)]
struct StartResp {
    id: String,
}

#[tokio::main]
async fn main() {
    let cfg = parse_args();

    std::fs::create_dir_all(DOWNLOAD_DIR).ok();
    std::fs::create_dir_all(&cfg.media_dir).ok();
    log(format!("cache (tijdelijk): ./{DOWNLOAD_DIR}/"));
    log(format!("mediabibliotheek (permanent): ./{}/", cfg.media_dir));
    if cfg.cache_ttl_secs == 0 {
        log("cache TTL: uit — tijdelijke bestanden worden niet opgeruimd".to_string());
    } else {
        log(format!(
            "cache TTL: {} min — oudere bestanden in ./{DOWNLOAD_DIR}/ worden opgeruimd",
            cfg.cache_ttl_secs / 60
        ));
    }

    // Warn early if the ffmpeg toolchain is missing, instead of failing per-job.
    for tool in ["ffmpeg", "ffprobe"] {
        if Command::new(tool).arg("-version").output().await.is_err() {
            log(format!("WAARSCHUWING: '{tool}' niet gevonden op PATH — downloads zullen mislukken"));
        }
    }

    // Redirects disabled so we can read the 302 + Set-Cookie from CloudFront ourselves.
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .user_agent(UA)
        .build()
        .expect("build http client");

    let state = AppState {
        jobs: Arc::new(Mutex::new(HashMap::new())),
        client,
        cfg: cfg.clone(),
    };

    if cfg.cache_ttl_secs > 0 {
        tokio::spawn(cleanup_loop(state.clone()));
    }

    let app = Router::new()
        .route("/", get(index))
        .route("/api/config", get(config))
        .route("/api/download", post(start))
        .route("/api/status/:id", get(status))
        .route("/api/file/:id", get(file))
        .with_state(state);

    let listener = match tokio::net::TcpListener::bind(&cfg.addr).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("Kan niet luisteren op {}: {e}", cfg.addr);
            std::process::exit(1);
        }
    };
    log(format!("beng_leecher luistert op http://{}", cfg.addr));
    if let Err(e) = axum::serve(listener, app).await {
        eprintln!("Server gestopt met fout: {e}");
        std::process::exit(1);
    }
}

/// Parse CLI arguments into a [`Config`].
fn parse_args() -> Config {
    let mut cfg = Config {
        addr: DEFAULT_ADDR.to_string(),
        media_dir: DEFAULT_MEDIA_DIR.to_string(),
        cache_ttl_secs: parse_duration(DEFAULT_CACHE_TTL).unwrap(),
    };
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "-a" | "--address" => cfg.addr = next_value(&mut args, &a),
            "-m" | "--media-dir" => cfg.media_dir = next_value(&mut args, &a),
            "-t" | "--cache-ttl" => {
                let raw = next_value(&mut args, &a);
                match parse_duration(&raw) {
                    Some(s) => cfg.cache_ttl_secs = s,
                    None => {
                        eprintln!("Ongeldige duur '{raw}'. Gebruik bijv. 30m, 2h, 1d of 0 (uit).");
                        std::process::exit(2);
                    }
                }
            }
            "-h" | "--help" => {
                println!(
                    "beng_leecher — Schatkamer video downloader\n\n\
                     Gebruik: beng_leecher [opties]\n\n\
                     Opties:\n  \
                       -a, --address   <host:port>  Luisteradres (standaard {DEFAULT_ADDR})\n  \
                       -m, --media-dir <pad>        Map voor permanent bewaarde bestanden (standaard {DEFAULT_MEDIA_DIR})\n  \
                       -t, --cache-ttl <duur>       Bewaartijd tijdelijke downloads, bijv. 30m/2h/1d, 0=uit (standaard {DEFAULT_CACHE_TTL})\n  \
                       -h, --help                   Toon deze hulp"
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("Onbekend argument: {other}  (gebruik --help)");
                std::process::exit(2);
            }
        }
    }
    cfg
}

fn next_value(args: &mut impl Iterator<Item = String>, flag: &str) -> String {
    match args.next() {
        Some(v) => v,
        None => {
            eprintln!("{flag} vereist een waarde");
            std::process::exit(2);
        }
    }
}

/// Parse a duration like `30m`, `2h`, `1d`, `90s`, or a bare number (minutes).
/// Returns seconds; `0` means "disabled".
fn parse_duration(s: &str) -> Option<u64> {
    let s = s.trim();
    if s == "0" {
        return Some(0);
    }
    let (num, mult) = match s.chars().last()? {
        's' => (&s[..s.len() - 1], 1),
        'm' => (&s[..s.len() - 1], 60),
        'h' => (&s[..s.len() - 1], 3600),
        'd' => (&s[..s.len() - 1], 86400),
        c if c.is_ascii_digit() => (s, 60), // bare number = minutes
        _ => return None,
    };
    num.trim().parse::<u64>().ok().map(|n| n * mult)
}

/// Periodically purge files in DOWNLOAD_DIR older than the cache TTL.
async fn cleanup_loop(st: AppState) {
    let ttl = st.cfg.cache_ttl_secs;
    // Check at a tenth of the TTL, clamped to [60s, 1h].
    let interval = (ttl / 10).clamp(60, 3600);
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(interval)).await;
        let mut removed = 0u32;
        let Ok(mut dir) = tokio::fs::read_dir(DOWNLOAD_DIR).await else {
            continue;
        };
        while let Ok(Some(entry)) = dir.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("mp4") {
                continue;
            }
            let Ok(meta) = entry.metadata().await else { continue };
            let age = meta
                .modified()
                .ok()
                .and_then(|m| m.elapsed().ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            if age >= ttl {
                if tokio::fs::remove_file(&path).await.is_ok() {
                    removed += 1;
                    // Forget the matching job so the map doesn't grow unbounded.
                    if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                        st.jobs.lock().unwrap().remove(stem);
                    }
                }
            }
        }
        if removed > 0 {
            log(format!("cache opgeruimd: {removed} verlopen bestand(en) verwijderd"));
        }
    }
}

// ---- handlers -------------------------------------------------------------

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

/// Expose retention settings so the UI can label the "keep on server" option.
async fn config(State(st): State<AppState>) -> Json<serde_json::Value> {
    Json(serde_json::json!({
        "media_dir": st.cfg.media_dir,
        "cache_ttl_secs": st.cfg.cache_ttl_secs,
    }))
}

async fn start(State(st): State<AppState>, Json(req): Json<DownloadReq>) -> Json<StartResp> {
    let id = new_id();
    let mut job = Job::new();
    job.kept = req.keep;
    st.jobs.lock().unwrap().insert(id.clone(), job);

    let st2 = st.clone();
    let id2 = id.clone();
    let url = req.url.trim().to_string();
    log(format!(
        "[{id}] nieuwe taak aangevraagd ({}): {url}",
        if req.keep { "mediaserver" } else { "client" }
    ));
    tokio::spawn(async move { run_job(st2, id2, url, req.keep).await });

    Json(StartResp { id })
}

async fn status(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    match st.jobs.lock().unwrap().get(&id) {
        Some(job) => Json(job.clone()).into_response(),
        None => (StatusCode::NOT_FOUND, "onbekende taak").into_response(),
    }
}

async fn file(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let (path, filename) = {
        let jobs = st.jobs.lock().unwrap();
        match jobs.get(&id) {
            Some(j) if j.done && !j.error => (j.path.clone(), j.filename.clone()),
            _ => return (StatusCode::NOT_FOUND, "bestand nog niet klaar").into_response(),
        }
    };

    let f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(_) => return (StatusCode::NOT_FOUND, "bestand ontbreekt of verlopen").into_response(),
    };
    log(format!("[{id}] bestand wordt geserveerd: {filename}"));

    let body = axum::body::Body::from_stream(ReaderStream::new(f));
    Response::builder()
        .header(header::CONTENT_TYPE, "video/mp4")
        .header(
            header::CONTENT_DISPOSITION,
            format!("attachment; filename=\"{filename}\""),
        )
        .body(body)
        .unwrap()
}

// ---- job pipeline ---------------------------------------------------------

async fn run_job(st: AppState, id: String, page_url: String, keep: bool) {
    set(&st, &id, |j| {
        j.status = "Video-URL opzoeken…".into();
    });
    log(format!("[{id}] stap 1/3: episodepagina ophalen en stream opzoeken"));

    let (master, cookie, title) = match resolve(&st.client, &id, &page_url).await {
        Ok(v) => v,
        Err(e) => {
            log(format!("[{id}] FOUT tijdens opzoeken: {e}"));
            return fail(&st, &id, e);
        }
    };
    log(format!("[{id}] titel: {title}"));

    let filename = format!("{}.mp4", sanitize(&title));
    set(&st, &id, |j| {
        j.title = title.clone();
        j.filename = filename.clone();
        j.status = "Downloaden…".into();
    });

    let duration = probe_duration(&master, &cookie).await.unwrap_or(0.0);
    log(format!("[{id}] duur volgens ffprobe: {duration:.0} sec"));
    let out = format!("{DOWNLOAD_DIR}/{id}.mp4");
    log(format!("[{id}] stap 3/3: ffmpeg start, muxt naar {out}"));

    let mut child = match Command::new("ffmpeg")
        .args(["-y", "-loglevel", "error", "-headers"])
        .arg(format!("Cookie: {cookie}\r\n"))
        .args([
            "-i",
            &master,
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-c",
            "copy",
            "-movflags",
            "+faststart",
            "-progress",
            "pipe:1",
            "-nostats",
            &out,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            log(format!("[{id}] FOUT: ffmpeg kon niet starten: {e}"));
            return fail(&st, &id, format!("ffmpeg kon niet starten: {e}"));
        }
    };

    // ffmpeg's -progress stream reports out_time_us (microseconds) per processed chunk.
    // We log every 10% so the console shows steady progress without flooding.
    if let Some(stdout) = child.stdout.take() {
        let mut last_logged_decile = 0u32;
        let mut lines = BufReader::new(stdout).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let Some(v) = line
                .strip_prefix("out_time_us=")
                .or_else(|| line.strip_prefix("out_time_ms="))
            else {
                continue;
            };
            if let Ok(us) = v.trim().parse::<f64>() {
                let secs = us / 1_000_000.0;
                let pct = if duration > 0.0 {
                    ((secs / duration) * 100.0).min(99.0) as f32
                } else {
                    0.0
                };
                set(&st, &id, |j| {
                    if duration > 0.0 {
                        j.progress = pct;
                        j.message = format!("{} / {} sec", secs as u64, duration as u64);
                    } else {
                        j.message = format!("{} sec verwerkt", secs as u64);
                    }
                });
                let decile = (pct as u32) / 10;
                if duration > 0.0 && decile > last_logged_decile {
                    last_logged_decile = decile;
                    log(format!(
                        "[{id}] voortgang {}% ({} / {} sec)",
                        decile * 10,
                        secs as u64,
                        duration as u64
                    ));
                }
            }
        }
    }

    let success = matches!(child.wait().await, Ok(s) if s.success());
    if success {
        let size = tokio::fs::metadata(&out).await.map(|m| m.len()).unwrap_or(0);
        let size_mb = size as f64 / 1_048_576.0;

        // For a "keep on server" job, move the finished file out of the cache dir
        // into the permanent media library so the TTL cleanup never touches it.
        let (final_path, msg) = if keep {
            let dest = unique_media_path(&st.cfg.media_dir, &filename);
            match tokio::fs::rename(&out, &dest).await {
                Ok(()) => {
                    log(format!("[{id}] KLAAR: opgeslagen op mediaserver: {dest} ({size_mb:.1} MB)"));
                    (dest.clone(), format!("Opgeslagen op de mediaserver: {dest}"))
                }
                Err(e) => {
                    // Fall back to serving from the cache dir if the move failed.
                    log(format!("[{id}] WAARSCHUWING: verplaatsen naar mediaserver mislukt ({e}); blijft in cache"));
                    (out.clone(), format!("Opgeslagen in cache (verplaatsen mislukte): {out}"))
                }
            }
        } else {
            log(format!("[{id}] KLAAR: {out} ({size_mb:.1} MB)"));
            (out.clone(), "Download voltooid".to_string())
        };

        set(&st, &id, |j| {
            j.progress = 100.0;
            j.done = true;
            j.status = "Klaar".into();
            j.message = msg;
            j.path = final_path;
        });
    } else {
        let mut err = String::new();
        if let Some(mut se) = child.stderr.take() {
            se.read_to_string(&mut err).await.ok();
        }
        let trimmed = err.trim();
        log(format!(
            "[{id}] FOUT: ffmpeg mislukt. stderr:\n{}",
            if trimmed.is_empty() { "(leeg)" } else { trimmed }
        ));
        let last = trimmed.lines().last().unwrap_or("onbekende fout").to_string();
        fail(&st, &id, format!("ffmpeg fout: {last}"));
    }
}

/// Fetch the episode page, extract the signed master playlist URL, perform the
/// CloudFront cookie handshake, and return (unsigned master URL, Cookie header, title).
async fn resolve(
    client: &reqwest::Client,
    id: &str,
    page_url: &str,
) -> Result<(String, String, String), String> {
    if !page_url.starts_with("http") {
        return Err("Geef een geldige http(s)-URL op.".into());
    }

    let resp = client
        .get(page_url)
        .send()
        .await
        .map_err(|e| format!("Pagina ophalen mislukt: {e}"))?;
    log(format!("[{id}] pagina HTTP {}", resp.status().as_u16()));
    let html = resp
        .text()
        .await
        .map_err(|e| format!("Pagina lezen mislukt: {e}"))?;

    // The signed master URL is embedded in the page's JSON, with `&` escaped as &.
    let re = Regex::new(
        r#"https://sk-video[^"\\]*\.m3u8\?[A-Za-z0-9_=~.%-]*(?:\\u0026[A-Za-z0-9_=~.%-]*)*"#,
    )
    .unwrap();
    let signed = re
        .find(&html)
        .ok_or("Geen video-stream gevonden op deze pagina (DRM of geen aflevering?).")?
        .as_str()
        .replace("\\u0026", "&");

    let master_plain = signed.split('?').next().unwrap_or(&signed).to_string();
    log(format!("[{id}] stap 2/3: master gevonden: {master_plain}"));

    // GET the signed URL → 302 + Set-Cookie carrying the CloudFront signed cookies.
    let resp = client
        .get(&signed)
        .send()
        .await
        .map_err(|e| format!("CDN-handshake mislukt: {e}"))?;
    log(format!(
        "[{id}] CDN-handshake HTTP {}",
        resp.status().as_u16()
    ));

    let mut parts = Vec::new();
    for v in resp.headers().get_all(header::SET_COOKIE) {
        if let Ok(s) = v.to_str() {
            let nv = s.split(';').next().unwrap_or("").trim();
            if nv.starts_with("CloudFront-") {
                parts.push(nv.to_string());
            }
        }
    }
    if parts.is_empty() {
        return Err("Geen CloudFront-cookies ontvangen; URL mogelijk verlopen.".into());
    }
    log(format!("[{id}] {} CloudFront-cookies ontvangen", parts.len()));
    let cookie = parts.join("; ");

    let title = Regex::new(r"<title>([^<]*)</title>")
        .unwrap()
        .captures(&html)
        .map(|c| c[1].trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| "schatkamer-video".into());

    Ok((master_plain, cookie, title))
}

async fn probe_duration(master: &str, cookie: &str) -> Option<f64> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-headers"])
        .arg(format!("Cookie: {cookie}\r\n"))
        .args([
            "-show_entries",
            "format=duration",
            "-of",
            "default=nw=1:nk=1",
            "-i",
            master,
        ])
        .output()
        .await
        .ok()?;
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

// ---- helpers --------------------------------------------------------------

fn set(st: &AppState, id: &str, f: impl FnOnce(&mut Job)) {
    if let Some(j) = st.jobs.lock().unwrap().get_mut(id) {
        f(j);
    }
}

fn fail(st: &AppState, id: &str, msg: impl Into<String>) {
    set(st, id, |j| {
        j.error = true;
        j.done = true;
        j.status = "Fout".into();
        j.message = msg.into();
    });
}

fn new_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    format!("{t}{n}")
}

/// Print a timestamped (UTC HH:MM:SS) log line to stdout.
fn log(msg: impl AsRef<str>) {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let t = secs % 86_400;
    println!(
        "[{:02}:{:02}:{:02}] {}",
        t / 3600,
        (t % 3600) / 60,
        t % 60,
        msg.as_ref()
    );
}

/// Build a non-colliding path in the media dir for `filename`, appending
/// " (2)", " (3)", … if a file with that name already exists.
fn unique_media_path(dir: &str, filename: &str) -> String {
    let (stem, ext) = match filename.rsplit_once('.') {
        Some((s, e)) => (s, format!(".{e}")),
        None => (filename, String::new()),
    };
    let mut candidate = format!("{dir}/{filename}");
    let mut n = 2;
    while std::path::Path::new(&candidate).exists() {
        candidate = format!("{dir}/{stem} ({n}){ext}");
        n += 1;
    }
    candidate
}

/// Turn a page title into a safe-ish filename stem (drop the " | De Schatkamer" suffix).
fn sanitize(s: &str) -> String {
    let base = s.split('|').next().unwrap_or(s).trim();
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == ' ' || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.split_whitespace().collect::<Vec<_>>().join(" ");
    if cleaned.is_empty() {
        "schatkamer-video".into()
    } else {
        cleaned
    }
}

const INDEX_HTML: &str = include_str!("index.html");
