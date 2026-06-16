# beng_leecher

A small video downloader for **De Schatkamer** (Beeld & Geluid) with a simple
web interface. Paste or drag an episode URL, and the tool resolves the HLS
stream, downloads the separate video and audio renditions, and muxes them into a
single MP4 file you can save. Paste a **series** URL instead and it pops up a
multi-select of the playable episodes so you can grab a whole season at once.

> ⚠️ **Non-DRM content only.** This tool resolves and remuxes openly delivered
> HLS streams. It does not break, bypass, or circumvent any DRM or access
> control. If a stream is DRM-protected, it simply won't be found.

---

## Requirements

- **Rust** (edition 2024 — stable 1.85+).
- **ffmpeg** and **ffprobe** available on your `PATH`.
  See [Why the external ffmpeg dependency?](#why-the-external-ffmpeg-dependency)
  for the rationale.

Install ffmpeg:

```bash
# Debian / Ubuntu
sudo apt install ffmpeg

# macOS (Homebrew)
brew install ffmpeg
```

Verify:

```bash
ffmpeg -version
ffprobe -version
```

---

## Usage

```bash
cargo run                                   # defaults
cargo run -- -a 127.0.0.1:3380              # bind loopback only
cargo run -- -m /srv/media -t 6h            # media library + 6h cache
```

Then open <http://localhost:3380> in your browser (or
`http://<this-machine-ip>:3380` from another device on your network).

1. Copy an **episode** link (`…/aflevering/…`) or a **series** link
   (`…/serie/…`) from De Schatkamer.
2. **Paste it** into the field (or **drag the link** onto the box).
3. Optionally tick **Permanent bewaren op de mediaserver** to keep the file in
   the server's media library instead of as a throwaway download (see
   [Storage & retention](#storage--retention)).
4. Press **Download**.
   - For an episode, a progress row appears immediately.
   - For a series, a popup lists the playable episodes — pick which ones to
     download (see [Downloading a whole series](#downloading-a-whole-series)).
5. Each download gets its own progress row; the server console logs every step.
   When a row finishes, click its save button to download the MP4 to your
   browser.

The server console prints timestamped log lines for every job (page fetch, CDN
handshake, ffprobe duration, ffmpeg progress per 10%, and the final result or
the full ffmpeg stderr on failure), so you can always see what's going on.

### Command-line options

| Flag                  | Default        | Meaning                                                                 |
| --------------------- | -------------- | ----------------------------------------------------------------------- |
| `-a`, `--address`     | `0.0.0.0:3380` | Listen address (`host:port`).                                           |
| `-m`, `--media-dir`   | `media`        | Directory for permanently-kept ("save to server") files.               |
| `-t`, `--cache-ttl`   | `60m`          | How long throwaway downloads survive before being purged. `0` disables. |
| `-h`, `--help`        | —              | Show help.                                                              |

Durations accept `s`/`m`/`h`/`d` suffixes (e.g. `90s`, `30m`, `6h`, `1d`); a
bare number is interpreted as minutes.

By default the server binds to `0.0.0.0:3380`, i.e. **all network interfaces**,
so it's reachable from other machines on your network — not just loopback. Use
`-a 127.0.0.1:3380` for local-only access. Note the server has **no
authentication**, so only expose it on networks you trust.

## Downloading a whole series

Paste a series URL — e.g.
`https://schatkamer.beeldengeluid.nl/serie/<id>/<slug>` — and the tool fetches
the series page and shows a **multi-select popup** of its episodes:

- The server always appends `?alleenafspeelbaar=ja` when fetching the series, so
  only **playable** (downloadable, non-DRM) episodes are listed.
- Click episodes to toggle them; **Shift-click** selects a contiguous range.
  **Alles selecteren** / **Niets** toggle everything, and a live counter shows
  how many of N are selected.
- The **Permanent bewaren op de mediaserver** toggle in the popup applies to all
  selected episodes.
- Confirming starts one independent download per selected episode; each gets its
  own progress row and runs concurrently.

Episodes that are **already downloaded** are shown greyed out with a
*✓ al gedownload* badge and can't be re-selected (see
[Skipping already-downloaded episodes](#skipping-already-downloaded-episodes)).

A URL containing `/serie/` (and not `/aflevering/`) is treated as a series; an
`/aflevering/` URL is downloaded directly as a single episode.

## Storage & retention

There are two places a finished file can live:

- **Cache (`./downloads/`)** — the default. Used for browser-bound downloads.
  Files are named by their **episode id** (e.g. `2101608060045577131.mp4`) so
  they can be recognised later, and are **automatically purged** once older than
  `--cache-ttl` (default 60 minutes). A background task sweeps the directory
  periodically (every TTL/10, clamped to 1–60 min). Set `--cache-ttl 0` to keep
  cached files indefinitely (no purging).
- **Media library (`--media-dir`, default `./media/`)** — used when the
  **Permanent bewaren op de mediaserver** checkbox is ticked. The finished file
  is moved here under its episode title (e.g.
  `media/WE ZIJN WEER THUIS - Afl_ 11_ Water in wijn.mp4`), with a numeric
  suffix on name collisions, and is **never** touched by the cache cleanup. This
  is the "download to the media server instead of the web client" mode — you can
  fire off a download and leave it on the server; downloading it to your browser
  afterwards is optional. A small `media/.beng_leecher_index.json` maps episode
  ids to their saved filenames so kept files are recognised across restarts.

Jobs are tracked in memory; a kept file persists on disk regardless, while a
cached file disappears from the job list when it is purged.

## Skipping already-downloaded episodes

Each episode has a stable key — the numeric id from its `…/aflevering/<id>` URL.
Before downloading, the tool checks whether that episode already exists and, if
so, **does not download it again**:

- **Already in the media library** → the job completes immediately, pointing at
  the existing file (works in both modes).
- **Already in the cache** (and you didn't ask to keep it) → likewise skipped.
- **Already in the cache but you ticked "keep"** → the cached file is **moved**
  into the media library instead of being downloaded again.

The status row shows *"Al aanwezig in cache/mediabibliotheek — niet opnieuw
gedownload"* in these cases. A concurrent in-progress guard also prevents two
jobs from downloading the same episode at once. For a series, this same check
drives the *✓ al gedownload* badges in the selection popup, so you don't even
pick episodes you already have. (If a cached file was purged by the TTL, the
episode is simply downloaded again.)

---

## How it works

De Schatkamer delivers video as **HLS** (HTTP Live Streaming). A *master*
`.m3u8` playlist lists several quality variants and a separate audio rendition;
each variant is its own playlist of short MPEG-TS (`.ts`) segments. Crucially,
**video and audio are delivered as separate streams** — they must be downloaded
independently and then combined.

The pipeline for one download:

1. **Fetch the episode page.** The server-rendered HTML embeds the
   CloudFront-signed master `.m3u8` URL (inside the page's JSON, with `&`
   escaped as `&`). A regex extracts it and un-escapes it.
2. **Authenticate** against the CDN — see
   [How authentication works](#how-authentication-works) below.
3. **Hand off to ffmpeg.** The (unsigned) master playlist URL plus the auth
   cookies are passed to ffmpeg. ffmpeg picks the highest-bandwidth video
   variant, follows the audio rendition playlist, downloads every segment of
   both, and **muxes** them into one MP4 — with `-c copy`, so the audio/video
   are remuxed without re-encoding (fast, lossless).
4. **Report progress.** ffmpeg's `-progress` output is parsed and compared
   against the total duration (obtained up front via `ffprobe`) to drive the
   progress bar. The browser polls `/api/status/<id>` until the job is done.

For a **series** URL the server fetches the series page (forcing
`alleenafspeelbaar=ja`) and extracts each episode's id + title from the
backslash-escaped JSON embedded in the page, builds the `…/aflevering/<id>` URLs,
and returns them to the UI for selection. Each chosen episode then runs through
the exact pipeline above as its own job.

```
episode page ──► signed master .m3u8 URL
                       │
                       ▼
              CloudFront handshake  ──►  signed cookies
                       │
                       ▼
   ffmpeg ◄── master URL + Cookie header
     │  (follows variant + audio playlists, downloads all .ts segments)
     ▼
   out.mp4  (H.264 video + AAC audio, remuxed, no re-encode)
```

### HTTP API

The web UI is a thin client over a small JSON API:

| Method | Path                | Purpose                                            |
| ------ | ------------------- | -------------------------------------------------- |
| `GET`  | `/`                 | The single-page web UI.                            |
| `GET`  | `/api/config`       | Retention settings: `media_dir`, `cache_ttl_secs` (UI labelling). |
| `POST` | `/api/series`       | Body `{"url": "<series url>"}` → `{"series_title", "episodes":[{"url","title","downloaded"}]}`. Lists playable episodes; `downloaded` flags those already in cache/media. |
| `POST` | `/api/download`     | Body `{"url": "<episode url>", "keep": <bool>}` → `{"id": "..."}`. Starts a job. `keep` defaults to `false`. |
| `GET`  | `/api/status/<id>`  | Job status: `status`, `progress`, `message`, `title`, `done`, `error`, `kept`. |
| `GET`  | `/api/file/<id>`    | Streams the finished MP4 as a download.            |

Jobs are tracked in memory, so a server restart clears the job list. Files in
the media library persist on disk; cached files persist until the TTL purges
them.

---

## How authentication works

The video segments live on a CloudFront CDN
(`sk-video.cdn.beeldengeluid.nl`) that is protected with
**[CloudFront signed cookies](https://docs.aws.amazon.com/AmazonCloudFront/latest/DeveloperGuide/private-content-signed-cookies.html)**.
Requesting a segment directly without credentials returns:

```xml
<Error><Code>MissingKey</Code><Message>Missing Key-Pair-Id query parameter or cookie value</Message></Error>
```

The trick is in *how* the signed credentials are issued. The signed master URL
on the episode page carries the signature as **query parameters**
(`CloudFront-Policy`, `CloudFront-Signature`, `CloudFront-Key-Pair-Id`,
`Expires`, …). When you request that URL, CloudFront does **not** return the
playlist directly — instead it responds with a `302 redirect` and a set of
`Set-Cookie` headers that move those same signed values into **cookies**:

```
HTTP/2 302
set-cookie: CloudFront-Policy=…; Path=/; Secure; HttpOnly; …
set-cookie: CloudFront-Signature=…; Path=/; Secure; HttpOnly; …
set-cookie: CloudFront-Key-Pair-Id=…; Path=/; Secure; HttpOnly; …
location: /…/<asset>.m3u8        ← same path, no query string
```

The signed **policy is a wildcard** over the whole asset path
(`https://sk-video.cdn.beeldengeluid.nl/<asset>/*`). So once you hold those
three cookies, they authorize **every** request under that asset — the master
playlist, each variant playlist, the audio playlist, and all `.ts` segments.

This tool handles the handshake explicitly:

1. The HTTP client is built with **redirects disabled**, so the `302` and its
   `Set-Cookie` headers are visible rather than being silently followed.
2. It performs a single `GET` on the signed master URL and scrapes the
   `CloudFront-*` cookies out of the response headers.
3. It strips the query string from the master URL to get the plain
   (post-redirect) URL.
4. That plain URL plus a `Cookie: CloudFront-Policy=…; CloudFront-Signature=…;
   CloudFront-Key-Pair-Id=…` header are passed to ffmpeg via `-headers`. ffmpeg
   reuses the same header on every follow-up request (variant playlists and
   segments), which is exactly what the wildcard policy authorizes.

Because the credentials are time-limited (`Expires` / `DateLessThan`), the
extracted URL and cookies are only valid for a window. The tool fetches a fresh
signed URL from the episode page for every job, so each download starts with
valid credentials. If a page yields no signed URL (e.g. DRM-protected or not an
episode page), the job fails with a clear message instead of producing a broken
file.

---

## Why the external ffmpeg dependency?

The downloaded content is **HLS with separate video and audio streams**, and the
goal is a single playable MP4. That requires *demuxing* the MPEG-TS segments and
*muxing* the H.264 video and AAC audio into an MP4 container. Note this is a
**remux, not a transcode** — no re-encoding happens, so it's fast and lossless.

Doing that muxing well is exactly the kind of fiddly, edge-case-heavy work that
ffmpeg already solves perfectly: HLS playlist following, segment fetching with
custom headers, MPEG-TS demuxing, PTS/DTS timestamp handling, ADTS→raw AAC
conversion, deriving AVCC extradata from in-stream SPS/PPS, and writing a clean
`+faststart` MP4. Shelling out to the `ffmpeg` binary makes this robust with
very little code.

### Why not a Rust ffmpeg crate?

The popular options (**`ffmpeg-next`**, **`rsmpeg`**, `ac-ffmpeg`, …) are
**bindings to ffmpeg's C libraries** (libavcodec/libavformat/…). They don't
remove the native dependency — they *relocate* it from runtime to build time:
you'd need the matching `libav*-dev` headers, `pkg-config`, and a C/clang
toolchain to compile. For a small self-contained tool, "an `ffmpeg` binary on
`PATH`" is the lighter, more portable requirement.

### Why not pure Rust?

A pure-Rust remux *is* feasible here — combine `m3u8-rs` (HLS parsing),
`mpeg2ts` (TS demux), and `mp4` (MP4 mux), with no C dependency at all. But the
hard parts (timestamp continuity across segments, ADTS framing, SPS/PPS →
extradata, track timeline alignment) are precisely where subtle bugs live, and
ffmpeg handles all of them for free. Pure Rust would be the path to take only if
dropping the external binary became a hard requirement.

In short: the external ffmpeg dependency buys a large amount of correctness for
a small amount of glue code, which is the right trade-off for this tool.

---

## Project layout

```
src/main.rs      Web server, CLI args, CloudFront handshake, job manager,
                 ffmpeg orchestration, cache cleanup
src/index.html   Single-page web UI (embedded into the binary via include_str!)
downloads/       Cache: throwaway downloads, purged after the TTL (git-ignored)
media/           Media library: permanently-kept downloads (git-ignored)
```

---

## Disclaimer

For personal use with non-DRM content only. Respect Beeld & Geluid's terms of
use and applicable copyright. You are responsible for how you use this tool.
