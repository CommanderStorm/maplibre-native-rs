//! Tile rendering server example with an async [`moka`] cache and
//! prefetching.
//!
//! Layout: a `moka::future::Cache` keyed by `(z, x, y)` holds rendered PNG
//! tiles. Requests go through `try_get_with`, which dedupes concurrent
//! callers asking for the same tile. On every request we additionally
//! kick off background renders of the 8 same-zoom neighbours so that, by
//! the time the user pans or zooms, those tiles are already in the cache.
//!
//! Pre-fetch tasks share the same cache as the request path: if the user
//! requests a tile that's already mid-prefetch, `try_get_with` joins the
//! in-flight future instead of starting a second render. The underlying
//! [`SingleThreadedRenderPool`] still serialises rendering onto its
//! worker thread, but the prefetcher fans render requests out so the pool
//! has work to do during HTTP idle time.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{Html, Response},
    routing::get,
    Router,
};
use maplibre_native::SingleThreadedRenderPool;
use moka::future::Cache;

type TileKey = (u8, u32, u32);

/// PNG bytes wrapped in `Arc` so cache hits are clone-cheap.
type TileBytes = Arc<Vec<u8>>;

#[derive(Clone)]
struct AppState {
    /// Cache of rendered PNG bytes. Capacity bounded by entry count to
    /// keep the example simple; a real deployment would weight by bytes.
    cache: Cache<TileKey, TileBytes>,
    /// Path to the style.json the server renders.
    style: Arc<PathBuf>,
}

/// Render a single tile to PNG bytes. Used both for the request path and
/// for prefetches — moka dedupes concurrent calls for the same key, so
/// it's safe to call this from multiple tasks at once.
async fn render_tile_png(
    style: Arc<PathBuf>,
    (z, x, y): TileKey,
) -> Result<TileBytes, RenderError> {
    let image = SingleThreadedRenderPool::global_pool()
        .render_tile((*style).clone(), z, x, y)
        .await
        .map_err(RenderError::Pool)?;

    let mut png = Vec::new();
    image
        .as_image()
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(RenderError::Encode)?;
    Ok(Arc::new(png))
}

/// 8 same-zoom neighbours, clipped to the valid tile range for `z`.
/// We deliberately stay on the same zoom level — fanning across zooms
/// would multiply load. Adjust as needed.
fn neighbours((z, x, y): TileKey) -> Vec<TileKey> {
    let max = if z >= 32 { u32::MAX } else { (1u32 << z).saturating_sub(1) };
    let mut out = Vec::with_capacity(8);
    for dx in -1i32..=1 {
        for dy in -1i32..=1 {
            if dx == 0 && dy == 0 {
                continue;
            }
            let nx = x as i64 + dx as i64;
            let ny = y as i64 + dy as i64;
            if nx < 0 || ny < 0 || nx > max as i64 || ny > max as i64 {
                continue;
            }
            #[allow(clippy::cast_sign_loss, clippy::cast_possible_truncation)]
            out.push((z, nx as u32, ny as u32));
        }
    }
    out
}

/// Spawn cache-populating renders for the given keys, returning
/// immediately. Each spawned task is a no-op if the key is already
/// cached or in flight (moka's `try_get_with` deduplicates).
fn spawn_prefetch(state: AppState, keys: Vec<TileKey>) {
    for key in keys {
        let cache = state.cache.clone();
        let style = state.style.clone();
        tokio::spawn(async move {
            // Errors are swallowed: a failed prefetch shouldn't break the
            // user request, and `try_get_with` does not cache the error.
            let _ = cache.try_get_with(key, render_tile_png(style, key)).await;
        });
    }
}

async fn rendered_style_tile(
    State(state): State<AppState>,
    Path((z, x, y)): Path<(u8, u32, u32)>,
) -> Result<Response, StatusCode> {
    let key = (z, x, y);

    // Kick off neighbour renders before awaiting the requested tile, so
    // they get queued onto the render pool concurrently with our own
    // request rather than waiting for our render to finish first.
    spawn_prefetch(state.clone(), neighbours(key));

    let bytes = state
        .cache
        .try_get_with(key, render_tile_png(state.style.clone(), key))
        .await
        .map_err(|e| {
            eprintln!("render failed for {key:?}: {e}");
            StatusCode::INTERNAL_SERVER_ERROR
        })?;

    let body = axum::body::Body::from((*bytes).clone());
    Ok(Response::builder()
        .header(header::CONTENT_TYPE, "image/png")
        .header(header::CACHE_CONTROL, "max-age=3600")
        .body(body)
        .unwrap())
}

async fn index() -> Html<&'static str> {
    Html(include_str!("index.html"))
}

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .join("tests")
        .join("fixtures")
        .join(name)
}

#[tokio::main]
async fn main() {
    let style = fixture_path("maplibre_demo.json");
    assert!(style.is_file(), "fixture style not found at {}", style.display());

    let state = AppState {
        // Bound entry count — tune to your tile size and memory budget.
        // 1024 entries * ~50KB/PNG ≈ 50MB. Use weighted eviction for a
        // real byte budget.
        cache: Cache::builder()
            .max_capacity(1024)
            .time_to_live(std::time::Duration::from_secs(3600))
            .build(),
        style: Arc::new(style),
    };

    let addr = "127.0.0.1:3000";
    println!("Server running on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let app = Router::new()
        .route("/", get(index))
        .route("/{z}/{x}/{y}", get(rendered_style_tile))
        .with_state(state);
    axum::serve(listener, app).await.unwrap();
}

#[derive(Debug, thiserror::Error)]
enum RenderError {
    #[error("render pool error: {0}")]
    Pool(#[from] maplibre_native::SingleThreadedRenderPoolError),
    #[error("PNG encode error: {0}")]
    Encode(#[from] image::ImageError),
}
