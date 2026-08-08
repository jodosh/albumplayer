//! HTTP routes.
//!
//! Clients never see or supply filesystem paths. Audio and covers are addressed
//! by database ID, and the server resolves the ID to a path itself, then checks
//! that the result really does sit under a directory it is allowed to serve.
//! That is the whole defence against path traversal, and it holds even if the
//! database is somehow wrong.

use std::path::{Path, PathBuf};

use axum::extract::{ConnectInfo, Path as UrlPath, Query, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use tower::ServiceExt;
use tower_http::cors::{Any, CorsLayer};
use tower_http::services::ServeFile;

use albumplayer_core::query::{AlbumFilter, AlbumSort};
use albumplayer_core::Library;

use crate::{AppState, Error, Result};

/// Build the router.
pub fn routes(state: AppState) -> Router {
    // Health and login are the only unauthenticated routes; everything else
    // goes through the token check.
    let public = Router::new()
        .route("/api/health", get(health))
        .route("/api/auth/login", post(login));

    let private = Router::new()
        .route("/api/auth/logout", post(logout))
        .route("/api/stats", get(stats))
        .route("/api/albums", get(albums))
        .route("/api/albums/{id}", get(album))
        .route("/api/albums/{id}/cover", get(cover))
        .route("/api/artists", get(artists))
        .route("/api/tracks/{id}/stream", get(stream))
        .route("/api/sessions", post(start_session))
        .route("/api/sessions/{id}/end", post(end_session))
        .route("/api/plays", post(record_play))
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            require_token,
        ));

    // Unmatched API paths must stay JSON. Without this they would reach the
    // single-page fallback below and a client asking for a mistyped endpoint
    // would get HTML and a baffling parse error instead of a 404.
    let api = public
        .merge(private)
        .route("/api/{*rest}", axum::routing::any(api_not_found))
        .with_state(state.clone());

    // Serve the built UI when one is present. Unknown paths fall back to
    // index.html so the single-page app can own its own routing, but only
    // *after* the API routes, which must keep returning JSON 404s.
    let app = match &state.config.ui_dir {
        Some(dir) => {
            let index = dir.join("index.html");
            api.fallback_service(
                tower_http::services::ServeDir::new(dir)
                    .fallback(tower_http::services::ServeFile::new(index)),
            )
        }
        None => api,
    };

    // Outermost, so preflight requests are answered before the token check
    // sees them — an `OPTIONS` carries no Authorization header and would
    // otherwise be rejected with a 401 the browser reports as a load failure.
    app.layer(cors())
}

/// Cross-origin rules.
///
/// The desktop shell loads from `tauri://localhost` and talks to the server over
/// HTTP, so every one of its calls is cross-origin; only the browser UI, served
/// by this same process, is same-origin.
///
/// Any origin is permitted, deliberately. Authentication is a bearer token that
/// a browser never attaches on its own, and no cookies are used, so a hostile
/// page gains nothing by being allowed to *send* a request — it cannot obtain a
/// token. Credentials are explicitly not allowed, which is what makes a
/// wildcard origin safe rather than reckless.
fn cors() -> CorsLayer {
    use axum::http::{Method, header};

    CorsLayer::new()
        .allow_origin(Any)
        .allow_methods([Method::GET, Method::POST, Method::OPTIONS])
        .allow_headers([header::AUTHORIZATION, header::CONTENT_TYPE, header::RANGE])
        // Range metadata has to be readable by the client or seeking breaks.
        .expose_headers([
            header::CONTENT_RANGE,
            header::CONTENT_LENGTH,
            header::ACCEPT_RANGES,
        ])
        .max_age(std::time::Duration::from_secs(3600))
}

// ---------------------------------------------------------------- auth

#[derive(Deserialize)]
struct LoginRequest {
    password: String,
}

#[derive(Serialize)]
struct LoginResponse {
    token: String,
    expires_in_secs: u64,
}

async fn login(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<std::net::SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(body): Json<LoginRequest>,
) -> Result<Json<LoginResponse>> {
    let client = client_address(&state, &headers, peer);

    // Argon2 verification is intentionally slow, so it must not run on an async
    // worker thread.
    let auth = std::sync::Arc::clone(&state.auth);
    let token = tokio::task::spawn_blocking(move || auth.login(client, &body.password))
        .await
        .map_err(|_| Error::Internal)??;

    Ok(Json(LoginResponse {
        token,
        expires_in_secs: state.config.session_ttl.as_secs(),
    }))
}

async fn logout(State(state): State<AppState>, request: Request) -> Result<Json<serde_json::Value>> {
    if let Some(token) = token_from(&request) {
        state.auth.logout(&token);
    }
    Ok(Json(serde_json::json!({ "ok": true })))
}

/// Which address a login attempt is counted against.
///
/// The socket address by default. Behind a tunnel every request arrives from
/// the same upstream, so the real client has to come from a header instead —
/// but only when the operator has said a proxy is genuinely in front, since
/// otherwise anyone could forge the header and get a fresh allowance per guess.
fn client_address(
    state: &AppState,
    headers: &axum::http::HeaderMap,
    peer: std::net::SocketAddr,
) -> std::net::IpAddr {
    if !state.config.trust_proxy {
        return peer.ip();
    }

    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());

    if let Some(ip) = header("cf-connecting-ip").and_then(|v| v.trim().parse().ok()) {
        return ip;
    }
    // With one trusted hop the last entry is what that hop observed; earlier
    // entries are whatever the caller chose to claim.
    if let Some(forwarded) = header("x-forwarded-for")
        && let Some(ip) = forwarded
            .rsplit(',')
            .next()
            .and_then(|v| v.trim().parse().ok())
    {
        return ip;
    }
    peer.ip()
}

async fn api_not_found() -> Error {
    Error::NotFound
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true, "version": env!("CARGO_PKG_VERSION") }))
}

/// Reject anything without a valid session token.
async fn require_token(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> std::result::Result<Response, Error> {
    let token = token_from(&request).ok_or(Error::Unauthorized)?;
    if !state.auth.verify(&token) {
        return Err(Error::Unauthorized);
    }
    Ok(next.run(request).await)
}

/// Pull the token from the `Authorization` header, or failing that the query
/// string.
///
/// The query fallback exists because `<audio src>` and `<img src>` cannot send
/// headers, and those tags point at exactly the streaming and cover routes.
fn token_from(request: &Request) -> Option<String> {
    let header = request
        .headers()
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_string);
    if header.is_some() {
        return header;
    }

    request.uri().query().and_then(|q| {
        q.split('&')
            .filter_map(|pair| pair.split_once('='))
            .find(|(key, _)| *key == "token")
            .map(|(_, value)| value.to_string())
    })
}

// ------------------------------------------------------------- library

#[derive(Deserialize)]
struct AlbumsQuery {
    sort: Option<String>,
    search: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct AlbumSummary {
    id: i64,
    title: String,
    artist: String,
    year: Option<i32>,
    track_count: i64,
    disc_count: i64,
    duration_ms: i64,
    is_compilation: bool,
    play_count: i64,
    has_cover: bool,
}

async fn albums(
    State(state): State<AppState>,
    Query(query): Query<AlbumsQuery>,
) -> Result<Json<Vec<AlbumSummary>>> {
    let sort = match query.sort.as_deref() {
        Some(name) => AlbumSort::parse(name)
            .ok_or_else(|| Error::BadRequest(format!("unknown sort '{name}'")))?,
        None => AlbumSort::Artist,
    };
    let filter = AlbumFilter {
        search: query.search,
        include_missing: false,
        limit: query.limit.map(|n| n.clamp(1, 5000)),
    };

    let rows = state
        .db(move |library| {
            let albums = library.albums(sort, &filter)?;
            let mut out = Vec::with_capacity(albums.len());
            for a in albums {
                let has_cover = library.album_art(a.id)?.is_some();
                out.push(AlbumSummary {
                    id: a.id,
                    title: a.title,
                    artist: a.album_artist,
                    year: a.year,
                    track_count: a.track_count,
                    disc_count: a.disc_count,
                    duration_ms: a.duration_ms,
                    is_compilation: a.is_compilation,
                    play_count: a.play_count,
                    has_cover,
                });
            }
            Ok(out)
        })
        .await?;

    Ok(Json(rows))
}

#[derive(Serialize)]
struct TrackDetail {
    id: i64,
    disc_no: i64,
    track_no: i64,
    title: String,
    artist: String,
    duration_ms: i64,
    codec: Option<String>,
}

#[derive(Serialize)]
struct AlbumDetail {
    id: i64,
    title: String,
    artist: String,
    year: Option<i32>,
    track_count: i64,
    disc_count: i64,
    duration_ms: i64,
    is_compilation: bool,
    play_count: i64,
    has_cover: bool,
    /// Album ReplayGain, tagged if present and measured otherwise.
    gain_db: Option<f64>,
    peak: Option<f64>,
    tracks: Vec<TrackDetail>,
}

async fn album(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<i64>,
) -> Result<Json<AlbumDetail>> {
    let detail = state
        .db(move |library| {
            let a = library.album(id)?;
            let (gain_db, peak) = library.album_replaygain(id)?;
            let has_cover = library.album_art(id)?.is_some();
            let tracks = library
                .album_tracks(id)?
                .into_iter()
                .map(|t| TrackDetail {
                    id: t.id,
                    disc_no: t.disc_no,
                    track_no: t.track_no,
                    title: t.title,
                    artist: t.artist,
                    duration_ms: t.duration_ms,
                    codec: t.codec,
                })
                .collect();

            Ok(AlbumDetail {
                id: a.id,
                title: a.title,
                artist: a.album_artist,
                year: a.year,
                track_count: a.track_count,
                disc_count: a.disc_count,
                duration_ms: a.duration_ms,
                is_compilation: a.is_compilation,
                play_count: a.play_count,
                has_cover,
                gain_db,
                peak,
                tracks,
            })
        })
        .await?;

    Ok(Json(detail))
}

#[derive(Serialize)]
struct ArtistSummary {
    id: i64,
    name: String,
    album_count: i64,
    album_plays: i64,
    track_plays: i64,
}

async fn artists(State(state): State<AppState>) -> Result<Json<Vec<ArtistSummary>>> {
    let rows = state
        .db(|library| {
            Ok(library
                .artists(None)?
                .into_iter()
                .map(|a| ArtistSummary {
                    id: a.id,
                    name: a.name,
                    album_count: a.album_count,
                    album_plays: a.album_plays,
                    track_plays: a.track_plays,
                })
                .collect())
        })
        .await?;
    Ok(Json(rows))
}

async fn stats(State(state): State<AppState>) -> Result<Json<serde_json::Value>> {
    let stats = state.db(|library| Ok(library.stats()?)).await?;
    Ok(Json(serde_json::json!({
        "albums": stats.albums,
        "artists": stats.artists,
        "tracks": stats.tracks,
        "total_duration_ms": stats.total_duration_ms,
        "compilations": stats.compilations,
        "album_plays": stats.album_plays,
        "track_plays": stats.track_plays,
    })))
}

// ------------------------------------------------------------- media

/// Serve an album cover.
async fn cover(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<i64>,
    request: Request,
) -> Result<Response> {
    let art = state
        .db(move |library| library.album_art(id)?.ok_or(Error::NotFound))
        .await?;

    // A fetched cover is stored as a bare filename and resolved against this
    // server's own cache directory, so the same database works on a host and in
    // a container. Folder art is already an absolute path under the music root.
    let cache_dir = state.config.art_cache_dir();
    let path = art.resolve(&cache_dir);

    let path = verify_within(
        &path.to_string_lossy(),
        &[&state.config.music_root, &cache_dir],
    )?;
    serve_file(&path, request).await
}

/// Stream a track, with range support so clients can seek.
async fn stream(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<i64>,
    request: Request,
) -> Result<Response> {
    let path: String = state
        .db(move |library| {
            library
                .conn
                .query_row(
                    "SELECT path FROM track WHERE id = ?1",
                    [id],
                    |r| r.get::<_, String>(0),
                )
                .map_err(|_| Error::NotFound)
        })
        .await?;

    let path = verify_within(&path, &[&state.config.music_root])?;
    serve_file(&path, request).await
}

/// Resolve a path and confirm it really lies inside one of `roots`.
///
/// The path always comes from our own database, never from the request, so this
/// is belt and braces — but a symlink inside the music tree could otherwise
/// point anywhere on the host, and this server is internet-facing.
fn verify_within(path: &str, roots: &[&PathBuf]) -> Result<PathBuf> {
    let resolved = Path::new(path).canonicalize().map_err(|_| Error::NotFound)?;

    for root in roots {
        // Canonicalize the root too, so a symlinked mount still matches.
        let Ok(root) = root.canonicalize() else {
            continue;
        };
        if resolved.starts_with(&root) {
            return Ok(resolved);
        }
    }

    tracing::warn!(path, "refusing to serve a file outside the permitted roots");
    Err(Error::NotFound)
}

/// Hand a file to `ServeFile`, which implements conditional and range requests.
async fn serve_file(path: &Path, request: Request) -> Result<Response> {
    ServeFile::new(path)
        .oneshot(request)
        .await
        .map(IntoResponse::into_response)
        .map_err(|_| Error::Internal)
}

// -------------------------------------------------------- play history

#[derive(Deserialize)]
struct StartSession {
    album_id: i64,
}

async fn start_session(
    State(state): State<AppState>,
    Json(body): Json<StartSession>,
) -> Result<Json<serde_json::Value>> {
    let id = state
        .db(move |library| Ok(library.start_album_session(body.album_id)?.id))
        .await?;
    Ok(Json(serde_json::json!({ "session_id": id })))
}

async fn end_session(
    State(state): State<AppState>,
    UrlPath(id): UrlPath<i64>,
) -> Result<Json<serde_json::Value>> {
    let finished = state
        .db(move |library| {
            let album_id: i64 = library
                .conn
                .query_row(
                    "SELECT album_id FROM album_session WHERE id = ?1",
                    [id],
                    |r| r.get(0),
                )
                .map_err(|_| Error::NotFound)?;
            let session = albumplayer_core::plays::PlaySession { id, album_id };
            Ok(library.end_album_session(session)?)
        })
        .await?;
    Ok(Json(serde_json::json!({ "finished": finished })))
}

#[derive(Deserialize)]
struct RecordPlay {
    track_id: i64,
    session_id: Option<i64>,
    started_at: Option<i64>,
    ms_played: i64,
}

async fn record_play(
    State(state): State<AppState>,
    Json(body): Json<RecordPlay>,
) -> Result<Json<serde_json::Value>> {
    let completed = state
        .db(move |library: &Library| {
            let session = match body.session_id {
                Some(id) => {
                    let album_id: i64 = library
                        .conn
                        .query_row(
                            "SELECT album_id FROM album_session WHERE id = ?1",
                            [id],
                            |r| r.get(0),
                        )
                        .map_err(|_| Error::NotFound)?;
                    Some(albumplayer_core::plays::PlaySession { id, album_id })
                }
                None => None,
            };
            let started_at = body
                .started_at
                .unwrap_or_else(albumplayer_core::util::now_unix);
            Ok(library.record_play(session, body.track_id, started_at, body.ms_played)?)
        })
        .await?;
    Ok(Json(serde_json::json!({ "completed": completed })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;

    /// A router backed by an empty in-memory library.
    fn test_app() -> Router {
        let library = albumplayer_core::Library::open_in_memory().unwrap();
        let config = crate::Config {
            music_root: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            bind: "127.0.0.1:0".parse().unwrap(),
            password: "test-password".into(),
            scan_on_start: false,
            session_ttl: std::time::Duration::from_secs(60),
            ui_dir: None,
            trust_proxy: false,
        };
        let auth = crate::auth::Auth::new(&config.password, config.session_ttl).unwrap();
        routes(AppState {
            library: std::sync::Arc::new(std::sync::Mutex::new(library)),
            auth: std::sync::Arc::new(auth),
            config: std::sync::Arc::new(config),
        })
    }

    #[tokio::test]
    async fn a_preflight_is_answered_rather_than_rejected() {
        // The desktop shell lives at tauri://localhost, so every call it makes
        // is cross-origin and begins with a preflight. Answering that with 405
        // — or with a 401 from the token check — surfaces as "Load failed".
        let response = test_app()
            .oneshot(
                Request::builder()
                    .method(axum::http::Method::OPTIONS)
                    .uri("/api/auth/login")
                    .header("Origin", "tauri://localhost")
                    .header("Access-Control-Request-Method", "POST")
                    .header("Access-Control-Request-Headers", "content-type")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(
            response.status().is_success(),
            "preflight was rejected: {}",
            response.status()
        );
        let headers = response.headers();
        assert!(headers.contains_key("access-control-allow-origin"));
        let methods = headers
            .get("access-control-allow-methods")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default();
        assert!(methods.contains("POST"), "methods: {methods}");
    }

    #[tokio::test]
    async fn responses_carry_cors_headers() {
        let response = test_app()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("Origin", "tauri://localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();

        assert!(response.status().is_success());
        assert!(
            response
                .headers()
                .contains_key("access-control-allow-origin"),
            "a cross-origin caller could not read this response"
        );
    }

    #[tokio::test]
    async fn range_headers_stay_readable_across_origins() {
        // Without these exposed, a cross-origin client cannot see Content-Range
        // and seeking inside a track breaks.
        let response = test_app()
            .oneshot(
                Request::builder()
                    .uri("/api/health")
                    .header("Origin", "tauri://localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let exposed = response
            .headers()
            .get("access-control-expose-headers")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_ascii_lowercase();
        assert!(exposed.contains("content-range"), "exposed: {exposed}");
        assert!(exposed.contains("accept-ranges"), "exposed: {exposed}");
    }

    #[tokio::test]
    async fn authentication_is_still_required() {
        // CORS must not have opened anything up.
        let response = test_app()
            .oneshot(
                Request::builder()
                    .uri("/api/albums")
                    .header("Origin", "tauri://localhost")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
    }

    fn request_with(header: Option<&str>, uri: &str) -> Request {
        let mut builder = Request::builder().uri(uri);
        if let Some(value) = header {
            builder = builder.header(axum::http::header::AUTHORIZATION, value);
        }
        builder.body(Body::empty()).unwrap()
    }

    #[test]
    fn a_bearer_header_supplies_the_token() {
        let request = request_with(Some("Bearer abc123"), "/api/albums");
        assert_eq!(token_from(&request).as_deref(), Some("abc123"));
    }

    #[test]
    fn a_query_parameter_supplies_the_token_for_media_tags() {
        // <audio src> cannot set headers, so this path has to work.
        let request = request_with(None, "/api/tracks/1/stream?token=abc123");
        assert_eq!(token_from(&request).as_deref(), Some("abc123"));
    }

    #[test]
    fn the_header_wins_over_the_query_string() {
        let request = request_with(Some("Bearer fromheader"), "/api/x?token=fromquery");
        assert_eq!(token_from(&request).as_deref(), Some("fromheader"));
    }

    #[test]
    fn requests_without_a_token_yield_nothing() {
        assert_eq!(token_from(&request_with(None, "/api/albums")), None);
        // A malformed scheme is not a token.
        assert_eq!(token_from(&request_with(Some("Basic abc"), "/api/x")), None);
        // A different query parameter is not a token either.
        assert_eq!(token_from(&request_with(None, "/api/x?other=1")), None);
    }

    fn state_with(trust_proxy: bool) -> AppState {
        let library = albumplayer_core::Library::open_in_memory().unwrap();
        let config = crate::Config {
            music_root: std::env::temp_dir(),
            data_dir: std::env::temp_dir(),
            bind: "127.0.0.1:0".parse().unwrap(),
            password: "test-password".into(),
            scan_on_start: false,
            session_ttl: std::time::Duration::from_secs(60),
            ui_dir: None,
            trust_proxy,
        };
        let auth = crate::auth::Auth::new(&config.password, config.session_ttl).unwrap();
        AppState {
            library: std::sync::Arc::new(std::sync::Mutex::new(library)),
            auth: std::sync::Arc::new(auth),
            config: std::sync::Arc::new(config),
        }
    }

    fn headers_with(pairs: &[(&str, &str)]) -> axum::http::HeaderMap {
        let mut headers = axum::http::HeaderMap::new();
        for (k, v) in pairs {
            headers.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        headers
    }

    #[test]
    fn forwarded_headers_are_ignored_unless_a_proxy_is_trusted() {
        // Otherwise an attacker invents a new address per guess and the login
        // lockout never engages.
        let peer: std::net::SocketAddr = "203.0.113.7:5555".parse().unwrap();
        let spoofed = headers_with(&[
            ("x-forwarded-for", "1.2.3.4"),
            ("cf-connecting-ip", "5.6.7.8"),
        ]);
        assert_eq!(
            client_address(&state_with(false), &spoofed, peer),
            peer.ip(),
            "the socket address must win when no proxy is trusted"
        );
    }

    #[test]
    fn a_trusted_proxy_supplies_the_real_client() {
        // Behind a tunnel every request shares one peer address, so without
        // this one attacker would lock out the whole household.
        let peer: std::net::SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let state = state_with(true);

        let cf = headers_with(&[("cf-connecting-ip", "5.6.7.8")]);
        assert_eq!(
            client_address(&state, &cf, peer).to_string(),
            "5.6.7.8"
        );

        // With one hop, the last entry is what that hop actually saw; earlier
        // ones are whatever the caller claimed.
        let xff = headers_with(&[("x-forwarded-for", "1.1.1.1, 9.9.9.9")]);
        assert_eq!(
            client_address(&state, &xff, peer).to_string(),
            "9.9.9.9"
        );
    }

    #[test]
    fn a_trusted_proxy_falls_back_to_the_socket_when_headers_are_absent() {
        let peer: std::net::SocketAddr = "203.0.113.7:5555".parse().unwrap();
        let state = state_with(true);
        assert_eq!(
            client_address(&state, &headers_with(&[]), peer),
            peer.ip()
        );
        // Garbage in the header must not be taken as an address either.
        let junk = headers_with(&[("x-forwarded-for", "not-an-ip")]);
        assert_eq!(client_address(&state, &junk, peer), peer.ip());
    }

    #[test]
    fn files_outside_the_permitted_roots_are_refused() {
        let dir = std::env::temp_dir().join(format!("apsrv{}", std::process::id()));
        let music = dir.join("music");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(music.join("song.mp3"), b"audio").unwrap();
        std::fs::write(dir.join("secret.txt"), b"not music").unwrap();

        let roots = vec![&music];

        // A genuine track resolves.
        let inside = music.join("song.mp3");
        assert!(verify_within(&inside.to_string_lossy(), &roots).is_ok());

        // Anything above the root does not, however it is spelled.
        let outside = dir.join("secret.txt");
        assert!(verify_within(&outside.to_string_lossy(), &roots).is_err());
        let traversal = music.join("../secret.txt");
        assert!(verify_within(&traversal.to_string_lossy(), &roots).is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_symlink_escaping_the_music_tree_is_refused() {
        // A tempting way in: a symlink inside the library pointing at /etc.
        let dir = std::env::temp_dir().join(format!("apsym{}", std::process::id()));
        let music = dir.join("music");
        std::fs::create_dir_all(&music).unwrap();
        std::fs::write(dir.join("outside.txt"), b"secret").unwrap();

        let link = music.join("sneaky.mp3");
        if std::os::unix::fs::symlink(dir.join("outside.txt"), &link).is_ok() {
            assert!(
                verify_within(&link.to_string_lossy(), &[&music]).is_err(),
                "canonicalisation must follow the link and reject it"
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_file_is_not_found() {
        let music = PathBuf::from("/nonexistent-root");
        assert!(verify_within("/nonexistent-root/x.mp3", &[&music]).is_err());
    }
}
