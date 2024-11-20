use egui::{pos2, Color32, Context, Mesh, Rect, Vec2};
use egui::{ColorImage, TextureHandle};
use futures::channel::mpsc::{channel, Receiver, Sender};
use image::ImageError;
use lru::LruCache;

use crate::download::{download_continuously, HttpOptions};
use crate::io::Runtime;
use crate::mercator::TileId;
use crate::sources::{Attribution, TileSource};

pub(crate) fn rect(screen_position: Vec2, tile_size: f64) -> Rect {
    Rect::from_min_size(screen_position.to_pos2(), Vec2::splat(tile_size as f32))
}

#[derive(Clone)]
pub struct Texture(TextureHandle);

impl Texture {
    pub fn new(image: &[u8], ctx: &Context) -> Result<Self, ImageError> {
        let image = image::load_from_memory(image)?.to_rgba8();
        let pixels = image.as_flat_samples();
        let image = ColorImage::from_rgba_unmultiplied(
            [image.width() as _, image.height() as _],
            pixels.as_slice(),
        );

        Ok(Self::from_color_image(image, ctx))
    }

    /// Load the texture from egui's [`ColorImage`].
    pub fn from_color_image(color_image: ColorImage, ctx: &Context) -> Self {
        Self(ctx.load_texture("image", color_image, Default::default()))
    }

    pub(crate) fn size(&self) -> Vec2 {
        self.0.size_vec2()
    }

    pub(crate) fn mesh_with_uv(&self, screen_position: Vec2, tile_size: f64, uv: Rect) -> Mesh {
        self.mesh_with_rect_and_uv(rect(screen_position, tile_size), uv)
    }

    pub(crate) fn mesh_with_rect(&self, rect: Rect) -> Mesh {
        let mut mesh = Mesh::with_texture(self.0.id());
        mesh.add_rect_with_uv(
            rect,
            Rect::from_min_max(pos2(0., 0.0), pos2(1.0, 1.0)),
            Color32::WHITE,
        );
        mesh
    }

    pub(crate) fn mesh_with_rect_and_uv(&self, rect: Rect, uv: Rect) -> Mesh {
        let mut mesh = Mesh::with_texture(self.0.id());
        mesh.add_rect_with_uv(rect, uv, Color32::WHITE);
        mesh
    }
}

/// Texture with UV coordinates.
pub struct TextureWithUv {
    pub texture: Texture,
    pub uv: Rect,
}

pub trait Tiles {
    fn at(&mut self, tile_id: TileId) -> Option<TextureWithUv>;
    fn attribution(&self) -> Attribution;
    fn tile_size(&self) -> u32;
}

/// Downloads the tiles via HTTP. It must persist between frames.
pub struct HttpTiles {
    attribution: Attribution,

    cache: LruCache<TileId, Option<Texture>>,

    /// Tiles to be downloaded by the IO thread.
    request_tx: Sender<TileId>,

    /// Tiles that got downloaded and should be put in the cache.
    tile_rx: Receiver<(TileId, Texture)>,

    #[allow(dead_code)] // Significant Drop
    runtime: Runtime,

    tile_size: u32,

    max_zoom: u8,
}

impl HttpTiles {
    /// Construct new [`Tiles`] with default [`HttpOptions`].
    pub fn new<S>(source: S, egui_ctx: Context) -> Self
    where
        S: TileSource + Send + 'static,
    {
        Self::with_options(source, HttpOptions::default(), egui_ctx)
    }

    /// Construct new [`Tiles`] with supplied [`HttpOptions`].
    pub fn with_options<S>(source: S, http_options: HttpOptions, egui_ctx: Context) -> Self
    where
        S: TileSource + Send + 'static,
    {
        // Minimum value which didn't cause any stalls while testing.
        let channel_size = 20;

        let (request_tx, request_rx) = channel(channel_size);
        let (tile_tx, tile_rx) = channel(channel_size);
        let attribution = source.attribution();
        let tile_size = source.tile_size();
        let max_zoom = source.max_zoom();

        let runtime = Runtime::new(download_continuously(
            source,
            http_options,
            request_rx,
            tile_tx,
            egui_ctx,
        ));

        #[allow(clippy::unwrap_used)]
        let cap = std::num::NonZeroUsize::new(256).unwrap();

        Self {
            attribution,
            cache: LruCache::new(cap), // Just arbitrary value which seemed right.
            request_tx,
            tile_rx,
            runtime,
            tile_size,
            max_zoom,
        }
    }

    fn put_next_downloaded_tile_in_cache(&mut self) {
        // This is called every frame, so take just one at the time.
        match self.tile_rx.try_next() {
            Ok(Some((tile_id, tile))) => {
                self.cache.put(tile_id, Some(tile));
            }
            Err(_) => {
                // Just ignore. It means that no new tile was downloaded.
            }
            Ok(None) => {
                log::error!("IO thread is dead")
            }
        }
    }

    fn request_tile(&mut self, tile_id: TileId) -> Option<Texture> {
        self.cache.get(&tile_id).cloned().unwrap_or_else(|| {
            if let Ok(()) = self.request_tx.try_send(tile_id) {
                log::trace!("Requested tile: {:?}", tile_id);

                // None acts as a placeholder for the tile, preventing multiple
                // requests for the same tile.
                self.cache.put(tile_id, None);
            } else {
                log::debug!("Request queue is full.");
            }
            None
        })
    }

    /// Find tile with a different zoom, which could be used as a placeholder.
    fn placeholder_with_different_zoom(&mut self, tile_id: TileId) -> Option<TextureWithUv> {
        // Currently, only a single zoom level down is supported.

        let (zoomed_tile_id, uv) = interpolate_higher_zoom(tile_id, tile_id.zoom.checked_sub(1)?);

        if let Some(Some(texture)) = self.cache.get(&zoomed_tile_id) {
            Some(TextureWithUv {
                texture: texture.clone(),
                uv,
            })
        } else {
            None
        }
    }
}

/// Take a piece of a tile with higher zoom level and use it as a tile with lower zoom level.
fn interpolate_higher_zoom(tile_id: TileId, available_zoom: u8) -> (TileId, Rect) {
    assert!(tile_id.zoom > available_zoom);

    let dzoom = 2u32.pow((tile_id.zoom - available_zoom) as u32);

    let x = (tile_id.x / dzoom, tile_id.x % dzoom);
    let y = (tile_id.y / dzoom, tile_id.y % dzoom);

    let zoomed_tile_id = TileId {
        x: x.0,
        y: y.0,
        zoom: available_zoom,
    };

    let z = (dzoom as f32).recip();

    let uv = Rect::from_min_max(
        pos2(x.1 as f32 * z, y.1 as f32 * z),
        pos2(x.1 as f32 * z + z, y.1 as f32 * z + z),
    );

    (zoomed_tile_id, uv)
}

impl Tiles for HttpTiles {
    /// Attribution of the source this tile cache pulls images from. Typically,
    /// this should be displayed somewhere on the top of the map widget.
    fn attribution(&self) -> Attribution {
        self.attribution.clone()
    }

    /// Return a tile if already in cache, schedule a download otherwise.
    fn at(&mut self, tile_id: TileId) -> Option<TextureWithUv> {
        self.put_next_downloaded_tile_in_cache();

        if tile_id.zoom <= self.max_zoom {
            self.request_tile(tile_id)
                .map(|texture| TextureWithUv {
                    texture,
                    uv: Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)),
                })
                .or_else(|| self.placeholder_with_different_zoom(tile_id))
        } else {
            let (zoomed_tile_id, uv) = interpolate_higher_zoom(tile_id, self.max_zoom);

            self.request_tile(zoomed_tile_id)
                .map(|texture| TextureWithUv {
                    texture: texture.clone(),
                    uv,
                })
        }
    }

    fn tile_size(&self) -> u32 {
        self.tile_size
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hypermocker::{
        hyper::header::{self, HeaderValue},
        Bytes, StatusCode,
    };
    use std::time::Duration;

    static TILE_ID: TileId = TileId {
        x: 1,
        y: 2,
        zoom: 3,
    };

    struct TestSource {
        base_url: String,
    }

    impl TestSource {
        pub fn new(base_url: String) -> Self {
            Self { base_url }
        }
    }

    impl TileSource for TestSource {
        fn tile_url(&self, tile_id: TileId) -> String {
            format!(
                "{}/{}/{}/{}.png",
                self.base_url, tile_id.zoom, tile_id.x, tile_id.y
            )
        }

        fn attribution(&self) -> Attribution {
            Attribution {
                text: "",
                url: "",
                logo_light: None,
                logo_dark: None,
            }
        }
    }

    /// Creates [`hypermocker::Mock`], and function mapping `TileId` to its URL.
    async fn hypermocker_mock() -> (hypermocker::Server, TestSource) {
        let server = hypermocker::Server::bind().await;
        let url = format!("http://localhost:{}", server.port());
        (server, TestSource::new(url))
    }

    async fn assert_tile_to_become_available_eventually(tiles: &mut HttpTiles, tile_id: TileId) {
        log::info!("Waiting for {:?} to become available.", tile_id);
        while tiles.at(tile_id).is_none() {
            // Need to yield to the runtime for things to move.
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    #[tokio::test]
    async fn download_single_tile() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        let mut anticipated = server.anticipate("/3/1/2.png").await;

        let mut tiles = HttpTiles::new(source, Context::default());

        // First query start the download, but it will always return None.
        assert!(tiles.at(TILE_ID).is_none());

        let request = anticipated.expect().await;
        assert_eq!(
            request.headers().get(header::USER_AGENT),
            Some(&HeaderValue::from_static(concat!(
                "walkers",
                "/",
                env!("CARGO_PKG_VERSION"),
            )))
        );

        // Eventually it gets downloaded and become available in cache.
        anticipated
            .respond(include_bytes!("../assets/blank-255-tile.png"))
            .await;
        assert_tile_to_become_available_eventually(&mut tiles, TILE_ID).await;
    }

    #[tokio::test]
    async fn custom_user_agent_header() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        let mut anticipated = server.anticipate("/3/1/2.png").await;

        let mut tiles = HttpTiles::with_options(
            source,
            HttpOptions {
                cache: None,
                user_agent: Some(crate::HeaderValue::from_static("MyApp")),
            },
            Context::default(),
        );

        // Initiate the download.
        tiles.at(TILE_ID);

        let request = anticipated.expect().await;
        assert_eq!(
            request.headers().get(header::USER_AGENT),
            Some(&HeaderValue::from_static("MyApp"))
        );
    }

    #[tokio::test]
    async fn there_can_be_6_simultaneous_downloads_at_most() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        let mut tiles = HttpTiles::new(source, Context::default());

        // First download is started immediately.
        let mut first_outstanding_request = server.anticipate(format!("/3/1/2.png")).await;
        assert!(tiles.at(TILE_ID).is_none());
        first_outstanding_request.expect().await;

        let tile_ids: Vec<_> = (2..7).map(|x| TileId { x, y: 1, zoom: 1 }).collect();

        // Rest of the downloads are started right away too, but they remain active.
        let mut requests = Vec::new();
        for tile_id in tile_ids {
            let mut request = server.anticipate(format!("/1/{}/1.png", tile_id.x)).await;
            assert!(tiles.at(tile_id).is_none());
            request.expect().await;
            requests.push(request);
        }

        // Last download is NOT started, because we are at the limit of concurrent downloads.
        assert!(tiles
            .at(TileId {
                x: 99,
                y: 99,
                zoom: 1
            })
            .is_none());

        // Make sure it does not come.
        tokio::time::sleep(Duration::from_secs(1)).await;

        // Last download will start as soon as one of the previous ones are responded to.
        let mut awaiting_request = server.anticipate("/1/99/99.png".to_string()).await;

        first_outstanding_request
            .respond(Bytes::from_static(include_bytes!(
                "../assets/blank-255-tile.png"
            )))
            .await;

        awaiting_request.expect().await;
    }

    async fn assert_tile_is_empty_forever(tiles: &mut HttpTiles) {
        // Should be None now, and forever.
        assert!(tiles.at(TILE_ID).is_none());
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert!(tiles.at(TILE_ID).is_none());
    }

    #[tokio::test]
    async fn tile_is_empty_forever_if_http_returns_error() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        let mut tiles = HttpTiles::new(source, Context::default());
        server
            .anticipate("/3/1/2.png")
            .await
            .respond_with_status(StatusCode::NOT_FOUND)
            .await;

        assert_tile_is_empty_forever(&mut tiles).await;
    }

    #[tokio::test]
    async fn tile_is_empty_forever_if_http_returns_no_body() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        let mut tiles = HttpTiles::new(source, Context::default());
        server
            .anticipate("/3/1/2.png")
            .await
            .respond_with_status(StatusCode::OK)
            .await;

        assert_tile_is_empty_forever(&mut tiles).await;
    }

    #[tokio::test]
    async fn tile_is_empty_forever_if_http_returns_garbage() {
        let _ = env_logger::try_init();

        let (server, source) = hypermocker_mock().await;
        let mut tiles = HttpTiles::new(source, Context::default());
        server
            .anticipate("/3/1/2.png")
            .await
            .respond("definitely not an image")
            .await;

        assert_tile_is_empty_forever(&mut tiles).await;
    }

    /// Tile source, which gives invalid urls.
    struct GarbageSource;

    impl TileSource for GarbageSource {
        fn tile_url(&self, _: TileId) -> String {
            "totally invalid url".to_string()
        }

        fn attribution(&self) -> Attribution {
            Attribution {
                text: "",
                url: "",
                logo_light: None,
                logo_dark: None,
            }
        }
    }

    #[tokio::test]
    async fn tile_is_empty_forever_if_http_can_not_even_connect() {
        let _ = env_logger::try_init();
        let mut tiles = HttpTiles::new(GarbageSource, Context::default());
        assert_tile_is_empty_forever(&mut tiles).await;
    }
}
