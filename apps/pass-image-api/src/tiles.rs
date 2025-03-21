// ! # Tiles
// ! Provides functions for retrieving and mosaicing
// tile imagery from public tile imagery sources.

use crate::coordinates::{
    lat_long_and_image_size_to_bounding_box, lat_long_to_tile_coords, ConstrainedTileBox, LatLong,
    TileCoordinate,
};

use actix_web_opentelemetry::ClientExt;
use anyhow::Result;
use awc::http::header::CONTENT_TYPE;
use awc::http::StatusCode;
use bytes::Bytes;
use futures::stream::{self, StreamExt};
use image::{DynamicImage, GenericImage, ImageBuffer};
use log::debug;
use opentelemetry::trace::{SpanKind, Status, TraceContextExt, Tracer};
use opentelemetry::{global, Context};
use std::borrow::Borrow;
use std::collections::{HashMap, HashSet};
use std::io::Cursor;

#[derive(Copy, Clone)]
pub enum TileSet {
    Osm,
    Swisstopo,
}

impl TileSet {
    fn url_pattern(&self) -> &str {
        match self {
            TileSet::Osm => "https://tile.openstreetmap.org/{z}/{x}/{y}.png",
            TileSet::Swisstopo => "https://wmts.geo.admin.ch/1.0.0/ch.swisstopo.landeskarte-farbe-10/default/current/3857/{z}/{x}/{y}.png"
        }
    }
}

// Fetches a single tile from a given TileSet
// Fetches a single tile from a given TileSet
async fn fetch_tile(t: TileSet, x: u32, y: u32, z: u32, cx: Context) -> Result<Bytes> {
    // Format the URL for the requested tile (zoom, x, y)
    let url = t
        .url_pattern()
        .replace("{z}", &z.to_string())
        .replace("{x}", &x.to_string())
        .replace("{y}", &y.to_string());

    let client = awc::Client::new();

    // Make an HTTP GET request to fetch the tile
    let mut response = client
        .get(&url)
        .insert_header(("User-Agent", "dd-sdlc-demo"))
        .trace_request_with_context(cx.clone())
        .send()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to send request to {}: {}", url, e))?;

    // Check if the response status is a success
    if response.status() != StatusCode::OK {
        return Err(anyhow::anyhow!(
            "Request to {} failed with status: {}",
            url,
            response.status()
        ));
    }

    // Check the content type
    let content_type = response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|val| val.to_str().ok())
        .unwrap_or("")
        .to_string();

    if content_type != "image/png" {
        return Err(anyhow::anyhow!(
            "Unexpected content type from {}: {}",
            url,
            content_type
        ));
    }

    // Extract and return the body as bytes
    response
        .body()
        .await
        .map_err(|e| anyhow::anyhow!("Failed to read response body from {}: {}", url, e))
}

// Fetches all of the tiles within a TileBox
// Note - we assume that a TileBox is only 2D - e.g., all tiles
// are within the same zoom level.
async fn fetch_tile_box(
    tileset: TileSet,
    top_left: &TileCoordinate,
    bottom_right: &TileCoordinate,
) -> Result<HashMap<(u32, u32, u32), Bytes>> {
    // Create a manual span for this function
    // This span will be the parent of all outgoing calls
    let tracer = global::tracer("fetch_image_tracer");
    let span = tracer
        .span_builder("fetch_image")
        .with_kind(SpanKind::Internal)
        .start(&tracer);

    let cx = Context::current_with_span(span);
    let ctx = cx.borrow();

    // Collect all tile coordinates in the bounding box
    let mut tile_coords = Vec::new();

    for x in top_left.x.floor() as u32..=bottom_right.x.ceil() as u32 {
        for y in top_left.y.floor() as u32..=bottom_right.y.ceil() as u32 {
            tile_coords.push((x, y, top_left.z));
        }
    }

    // Fetch all tiles in parallel, but fail if any tile fetch fails
    let mut tile_map = HashMap::new();

    let tile_fetches = stream::iter(tile_coords.into_iter().map(|tile| {
        // For each tile, fetch the corresponding tile asynchronously
        async move {
            fetch_tile(tileset, tile.0, tile.1, tile.2, ctx.clone())
                .await
                .map(|bytes| (tile, bytes))
        }
    }))
    .buffer_unordered(10) // Limit to 10 concurrent requests
    .collect::<Vec<_>>() // Collect all results (errors or successes)
    .await;

    // Check for any errors in the results
    for tile_result in tile_fetches {
        match tile_result {
            Ok((tile, bytes)) => {
                tile_map.insert((tile.0, tile.1, tile.2), bytes); // Insert the successful result into the map
            }
            Err(e) => {
                // If any tile fetch fails, set the span status to Error and return the error
                cx.span().set_status(Status::Error {
                    description: e.to_string().into(),
                });
                return Err(e);
            }
        }
    }

    // Set the span status to OK and end the span
    cx.span().set_status(Status::Ok);
    cx.span().end();

    Ok(tile_map)
}

// Fetches an image centered at the given point, using the provided TileSet.
pub async fn fetch_image_from_point(
    center: LatLong,
    radius_km: f32,
    image_size: u32,
    tileset: TileSet,
) -> Result<Bytes> {
    // Find the center
    let tile_box = lat_long_and_image_size_to_bounding_box(center, radius_km, image_size);

    // Fetch the image
    fetch_image(tileset, &tile_box).await
}

// Fetches an image at the given point using the provided TileSet and ConstrainedTileBox
// This function will fetch enough tiles around the given point to allow it to crop the resulting
// image down to ensure we have enough pixels to cover the requested resolution.
async fn fetch_image(tileset: TileSet, tile_box: &ConstrainedTileBox) -> Result<Bytes> {
    // Fetch all tiles in the bounding box
    let tiles = fetch_tile_box(
        tileset,
        &tile_box.tile_box.top_left,
        &tile_box.tile_box.bottom_right,
    )
    .await?;

    // Each tile is 256x256 pixels
    let tile_size = 256;

    // Calculate the total number of tiles in x and y directions
    let unique_x: HashSet<u32> = tiles.keys().map(|(x, _, _)| *x).collect::<HashSet<u32>>();
    let num_tiles_x = unique_x.len() as u32;
    let unique_y: HashSet<u32> = tiles.keys().map(|(_, y, _)| *y).collect::<HashSet<u32>>();
    let num_tiles_y = unique_y.len() as u32;

    // Create a new empty image with dimensions for all tiles
    let img_width = num_tiles_x * tile_size;
    let img_height = num_tiles_y * tile_size;

    let meter = global::meter("processing_time_meter");
    let processing_time = meter.f64_histogram("processing_time").init();
    let start = std::time::Instant::now();

    let mut full_image = ImageBuffer::new(img_width, img_height);

    // Draw each tile into the final image
    for (tile_coord, tile_bytes) in tiles {
        let tile_img = image::load_from_memory(&tile_bytes).expect("I can load my tiles");

        let x_offset = (tile_coord.0 - tile_box.tile_box.top_left.x.floor() as u32) * tile_size;
        let y_offset = (tile_coord.1 - tile_box.tile_box.top_left.y.floor() as u32) * tile_size;

        full_image
            .copy_from(&tile_img.to_rgba8(), x_offset, y_offset)
            .unwrap();
    }

    // What's the full size of our output image?
    let full_image_width =
        (tile_box.tile_box.bottom_right.x - tile_box.tile_box.top_left.x) * 256.0;
    let full_image_height =
        (tile_box.tile_box.bottom_right.y - tile_box.tile_box.top_left.y) * 256.0;
    debug!(
        "Full image size: {}x{}",
        full_image_width, full_image_height
    );

    // Work out the offsets from the left and top of the image, so that we can
    let center_pos_abs =
        lat_long_to_tile_coords(&tile_box.center, tile_box.tile_box.bottom_right.z);
    let center_x_tile_offset = center_pos_abs.x - tile_box.tile_box.outer_top_left().0 as f32;
    let center_y_tile_offset = center_pos_abs.y - tile_box.tile_box.outer_top_left().1 as f32;
    let center_x_px = (center_x_tile_offset * 256.0) as u32;
    let center_y_px = (center_y_tile_offset * 256.0) as u32;

    // Offset in by half the targeted radius, in pixels
    // We can then use the full radius as the width and height, and we end up centered where we should
    // be centered
    let offset_left = center_x_px - (tile_box.inner_size_px.0 / 2);
    let offset_top = center_y_px - (tile_box.inner_size_px.1 / 2);

    debug!("Center: {0}, {1}", center_x_px, center_y_px);
    debug!("Offset: {0}, {1}", offset_left, offset_top);
    debug!(
        "W/h   : {0}, {1}",
        tile_box.inner_size_px.0, tile_box.inner_size_px.1
    );

    // Crop the image back in so we're centered where we want to be
    let mut png_buffer = Vec::new();
    DynamicImage::ImageRgba8(full_image)
        .crop_imm(
            offset_left, // X offset
            offset_top,  // Y offset
            tile_box.inner_size_px.0,
            tile_box.inner_size_px.1,
        )
        .write_to(&mut Cursor::new(&mut png_buffer), image::ImageFormat::Png)
        .expect("I can write a PNG");

    let buffer_to_bytes = Bytes::from(png_buffer);

    processing_time.record(start.elapsed().as_secs_f64(), &[]);

    // Return the image as Bytes
    Ok(buffer_to_bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coordinates::{lat_long_and_image_size_to_bounding_box, LatLong};
    use image::GenericImageView;
    use std::env;
    use std::fs::File;
    use std::io::Write;

    #[tokio::test]
    async fn test_fetch_tile() {
        let tile = (3366, 2431);
        let zoom = 12;
        let cx = Context::current();

        // Replace the base URL with mockito’s server URL
        let result = fetch_tile(TileSet::Osm, tile.0, tile.1, zoom, cx).await;

        // Assert the result is Ok and contains the correct number of bytes
        assert!(result.is_ok());
        let bytes = result.unwrap();
        assert!(bytes.len() > 1000);
    }

    #[tokio::test]
    async fn test_fetch_image_from_lat_lon_box() {
        // Set up the lat/long and radius
        let point = LatLong(-31.9514, 115.8617);
        let radius_km = 1.0;

        // Use lat_lon_and_radius_to_tile_box to calculate the bounding box for tiles
        let tile_box = lat_long_and_image_size_to_bounding_box(point, radius_km, 1024);

        // Generate the image using fetch_image
        let result = fetch_image(TileSet::Osm, &tile_box).await;
        assert!(result.is_ok(), "Fetching image failed");

        let image_bytes = result.unwrap();

        // Load the image from the bytes to check its dimensions
        let img = image::load_from_memory(&image_bytes).expect("Failed to load image from bytes");

        // Check that the image is at least 1000x1000 pixels
        let (width, height) = img.dimensions();
        assert!(
            width >= 1000 && height >= 1000,
            "Generated image is smaller than expected: {}x{}",
            width,
            height
        );

        // Create a temporary directory and file to store the image
        let dir = env::current_dir().expect("I can get my cwd");
        let file_path = dir.join("test_image.png");

        // Write the image to the temporary file
        let mut file = File::create(file_path.clone()).expect("Failed to create temp file");
        file.write_all(&image_bytes)
            .expect("Failed to write image to temp file");

        debug!("Image saved to: {:?}", file_path);
    }
}
