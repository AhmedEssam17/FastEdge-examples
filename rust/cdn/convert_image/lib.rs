use proxy_wasm::traits::*;
use proxy_wasm::types::*;
use image::*;
use std::{env, env::VarError, io::Cursor, str::from_utf8};

const MAX_IMAGE_DIMENSION: u32 = 10_000;
const MAX_CROP_PERCENT: f32 = 100.0;

#[derive(Debug, Clone)]
struct ResizeParams {
    width: Option<u32>,
    height: Option<u32>,
    fit: FitMode,
}

#[derive(Debug, Clone, Copy)]
struct CropParams {
    mode: CropMode,
    anchor_x: CropAnchor,
    anchor_y: CropAnchor,
}

#[derive(Debug, Clone, Copy)]
enum FitMode {
    Fit,      // crop from all sides to exact dimensions
    Bounds,   // compress proportionally using larger dimension
    Cover,    // compress proportionally using smaller dimension
    Force,    // ignore aspect ratio, force exact dimensions
}

impl Default for FitMode {
    fn default() -> Self {
        FitMode::Fit
    }
}

#[derive(Debug, Clone, Copy)]
enum CropMode {
    AspectRatio { width_ratio: u32, height_ratio: u32 },
    Dimensions { width: u32, height: u32 },
}

#[derive(Debug, Clone, Copy)]
enum CropAnchor {
    Center,
    Pixels(u32),
    PercentOfImage(f32),
    OffsetPercent(f32),
}

impl Default for CropAnchor {
    fn default() -> Self {
        CropAnchor::Center
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TargetFormat {
    Avif,
    Webp,
}

#[derive(Debug, Clone)]
struct RequestedFormats {
    order: Vec<TargetFormat>,
}

impl RequestedFormats {
    fn empty() -> Self {
        Self { order: Vec::new() }
    }

    fn is_empty(&self) -> bool {
        self.order.is_empty()
    }

    fn include(&mut self, format: TargetFormat) {
        if !self.order.contains(&format) {
            self.order.push(format);
        }
    }

    fn iter(&self) -> impl Iterator<Item = TargetFormat> + '_ {
        self.order.iter().copied()
    }
}

impl CropParams {
    fn cache_key(&self) -> String {
        match self.mode {
            CropMode::AspectRatio {
                width_ratio,
                height_ratio,
            } => format!("{}:{}", width_ratio, height_ratio),
            CropMode::Dimensions { width, height } => {
                let mut parts = vec![width.to_string(), height.to_string()];
                if let Some(token) = Self::anchor_token('x', self.anchor_x) {
                    parts.push(token);
                }
                if let Some(token) = Self::anchor_token('y', self.anchor_y) {
                    parts.push(token);
                }
                parts.join(",")
            }
        }
    }

    fn operation_descriptor(&self) -> String {
        match self.mode {
            CropMode::AspectRatio {
                width_ratio,
                height_ratio,
            } => format!("crop:ratio{}:{}", width_ratio, height_ratio),
            CropMode::Dimensions { width, height } => {
                let mut parts = vec![format!("{}x{}", width, height)];
                if let Some(token) = Self::anchor_token('x', self.anchor_x) {
                    parts.push(token);
                }
                if let Some(token) = Self::anchor_token('y', self.anchor_y) {
                    parts.push(token);
                }
                format!("crop:{}", parts.join("|"))
            }
        }
    }

    fn anchor_token(axis: char, anchor: CropAnchor) -> Option<String> {
        match anchor {
            CropAnchor::Center => None,
            CropAnchor::Pixels(px) => Some(format!("{}{}", axis, px)),
            CropAnchor::PercentOfImage(percent) => {
                Some(format!("{}{}p", axis, format_percent(percent)))
            }
            CropAnchor::OffsetPercent(percent) => {
                Some(format!("offset-{}{}", axis, format_percent(percent)))
            }
        }
    }
}

proxy_wasm::main! {{
    proxy_wasm::set_log_level(LogLevel::Trace);
    proxy_wasm::set_root_context(|_| -> Box<dyn RootContext> { Box::new(HttpBodyRoot) });
}}

struct HttpBodyRoot;

impl Context for HttpBodyRoot {}

impl RootContext for HttpBodyRoot {
    fn get_type(&self) -> Option<ContextType> {
        Some(ContextType::HttpContext)
    }

    fn create_http_context(&self, _: u32) -> Option<Box<dyn HttpContext>> {
        Some(Box::new(HttpBody))
    }
}

struct HttpBody;

impl Context for HttpBody {}

impl HttpContext for HttpBody {
    fn on_http_request_headers(&mut self, _: usize, _: bool) -> Action
    {
        // this header is used to select correct image version from cache
        self.add_http_request_header("Image-Format", "original");
        
        // Parse resize parameters from query string
        let resize_params = self.parse_resize_params();
        let resize_requested = resize_params.width.is_some() || resize_params.height.is_some();
        if resize_requested {
            
            // Update cache key to include resize parameters
            let cache_key = format!("w{}_h{}_{:?}", 
                resize_params.width.unwrap_or(0),
                resize_params.height.unwrap_or(0),
                resize_params.fit
            );
            self.add_http_request_header("Image-Resize", &cache_key);
            println!("Image-Resize header set to: {}", cache_key);
        }

        println!("Resize params: {:?}", resize_params);

        // Parse crop parameters from query string
        let crop_params = self.parse_crop_params();
        let crop_requested = crop_params.is_some();
        if let Some(crop) = crop_params {
            let cache_key = crop.cache_key();
            self.add_http_request_header("Image-Crop", &cache_key);
            println!("Image-Crop header set to: {}", cache_key);
        }

        // get extension
        let Some(ext)= self.get_property(vec!["request.extension"]) else {
            println!("No extension in request path, not transforming");
            return Action::Continue;
        };
        let Ok(ext) = from_utf8(&ext) else {
            println!("Invalid UTF-8 in request extension, not transforming");
            return Action::Continue;
        };
        if ext.is_empty() {
            println!("No extension in request path, not transforming");
            return Action::Continue;
        }

        // FORMATS_TO_TRANSFORM contains list of file extensions to transfor
        // note that jpg and jpeg are different extensions
        let Ok(image_list) = str_param("FORMATS_TO_TRANSFORM") else {
            println!("FORMATS_TO_TRANSFORM param is not set, not transforming");
            return Action::Continue;
        };
        if !image_list.split(',').any(|entry| entry == ext) {
            println!("extension {} is not in the list of formats to transform: {}, not transforming", ext, image_list);
            return Action::Continue;
        }

        // requests from User agents that match substrings in the IGNORED_UA_LIST param are not transformed
        let Some(ua) = self.get_http_request_header("User-Agent") else {
            println!("User-Agent header is not set, not transforming");
            return Action::Continue;
        };
        if let Ok(ua_to_ignore) = str_param("IGNORED_UA_LIST") {
            if ua_to_ignore.split(",").any(|entry| ua.contains(entry)) {
                println!("User-Agent is in ignore list, not transforming");
                return Action::Continue;
            }
        }

        let convert_to_avif = bool_param("CONVERT_TO_AVIF", true);
        let convert_to_webp = bool_param("CONVERT_TO_WEBP", false);
        let requested_formats = self.parse_requested_formats();
        let accept_header = self.get_http_request_header("Accept");
        let target_format = self.select_target_format(
            convert_to_avif,
            convert_to_webp,
            &requested_formats,
            accept_header.as_deref(),
        );

        match target_format {
            Some(TargetFormat::Avif) => {
                println!("Selected AVIF conversion");
                self.set_http_request_header("Image-Format", Some("image/avif"));
            }
            Some(TargetFormat::Webp) => {
                println!("Selected WebP conversion");
                self.set_http_request_header("Image-Format", Some("image/webp"));
            }
            None => {
                if resize_requested || crop_requested {
                    println!("No format conversion selected; processing original image");
                    self.set_http_request_header("Image-Format", Some("original-with-processing"));
                }
            }
        }

        Action::Continue
    }

    fn on_http_response_headers(&mut self, _: usize, _: bool) -> Action
    {
        // only process 200 responses
        if let Some(status) = self.rsp_status() {
            if status != 200 {
                println!("Response status is {} instead of expected 200, not transforming", status);
                return Action::Continue;
            }
        } else {
            println!("Response status is not set, not transforming");
            return Action::Continue;
        }

        // if "Image-Format" request header is not set, don't convert the image
        let Some(content_type) = self.get_http_request_header("Image-Format") else {
            return Action::Continue;
        };
        self.add_http_response_header("Vary", "Image-Format");

        let mut operations: Vec<String> = Vec::new();
        match content_type.as_str() {
            "image/avif" => {
                operations.push("convert:avif".to_string());
            }
            "image/webp" => {
                operations.push("convert:webp".to_string());
            }
            "original-with-processing" => {}
            "original" => {
                return Action::Continue;
            }
            other => {
                println!("Unsupported Image-Format header value: {}", other);
                return Action::Continue;
            }
        }

        if self.get_http_request_header("Image-Resize").is_some() {
            self.add_http_response_header("Vary", "Image-Resize");
            
            // Parse the resize header to determine operations
            let resize_params = self.parse_resize_params();
            if resize_params.width.is_some() || resize_params.height.is_some() {
                let resize_info = match (resize_params.width, resize_params.height) {
                    (Some(w), Some(h)) => format!("resize:{}x{}:{:?}", w, h, resize_params.fit),
                    (Some(w), None) => format!("resize:{}x*", w),
                    (None, Some(h)) => format!("resize:*x{}", h),
                    _ => "resize".to_string(),
                };
                operations.push(resize_info);
            }
        }

        if self.get_http_request_header("Image-Crop").is_some() {
            self.add_http_response_header("Vary", "Image-Crop");
            if let Some(crop) = self.parse_crop_params() {
                operations.push(crop.operation_descriptor());
            }
        }

        if !operations.is_empty() {
            let operations_header = operations.join(",");
            println!("X-Img-Operations header set to: {}", operations_header);
            self.set_http_response_header("X-Img-Operations", Some(operations_header.as_str()));
        }

        self.set_http_response_header("Content-Length", None);
        self.set_http_response_header("Transfer-Encoding", Some("Chunked"));
        
        // Set content type based on whether we're converting to AVIF or WebP or keeping original
        if content_type == "image/avif" {
            self.set_http_response_header("Content-Type", Some("image/avif"));
        } else if content_type == "image/webp" {
            self.set_http_response_header("Content-Type", Some("image/webp"));
        } else if content_type == "original-with-processing" {
            // Content-Type will be determined from the actual image in the body phase
        }

        // indicate to on_http_response_body that transformation is needed
        self.set_property(vec!["response.content-type"], Some(content_type.as_bytes()));

        Action::Continue
    }

    fn on_http_response_body(&mut self, body_size: usize, end_of_stream: bool) -> Action
    {
        if !end_of_stream { // wait till we get complete body
            return Action::Pause;
        }

        let Some(content_type)= self.get_property(vec!["response.content-type"]) else {
            return Action::Continue;
        };

        let Ok(content_type) = from_utf8(&content_type) else {
            // should never happen
            println!("Invalid UTF-8 in Content-Type");
            self.send_http_response(500, vec![], None);
            return Action::Pause;
        };

        let convert_to_avif = content_type == "image/avif";
        let convert_to_webp = content_type == "image/webp";
        let process_original = content_type == "original-with-processing";
        
        if !convert_to_avif && !convert_to_webp && !process_original {
            println!("Content-Type {} is not supported, not transforming", content_type);
            return Action::Continue;
        }

        if let Some(body_bytes) = self.get_http_response_body(0, body_size) {
            println!("Processing response body of {} bytes", body_size);
            let buf = body_bytes.as_bytes();
            let mut img = match load_from_memory(buf) {
                Ok(i) => {
                    println!("Successfully loaded image: {}x{}", i.width(), i.height());
                    i
                },
                Err(e) => {
                    println!("cannot load image to memory {}, not converting", e);
                    return Action::Continue
                }
            };

            // Apply resize if parameters are present - parse directly from query
            let resize_params = self.parse_resize_params();
            println!("Parsed resize params in response body: {:?}", resize_params);

            let crop_params = self.parse_crop_params();
            println!("Parsed crop params in response body: {:?}", crop_params);

            if let Some(crop) = crop_params {
                let original_size = (img.width(), img.height());
                match self.apply_crop(img, crop) {
                    Ok(cropped) => {
                        println!(
                            "Cropped from {}x{} to {}x{}",
                            original_size.0,
                            original_size.1,
                            cropped.width(),
                            cropped.height()
                        );
                        img = cropped;
                    }
                    Err(e) => {
                        println!("Failed to apply crop: {}", e);
                        return Action::Continue;
                    }
                }
            }

            if resize_params.width.is_some() || resize_params.height.is_some() {
                println!("Applying resize: {:?}", resize_params);
                let original_size = (img.width(), img.height());
                img = self.apply_resize(img, &resize_params);
                println!("Resized from {}x{} to {}x{}", original_size.0, original_size.1, img.width(), img.height());
            } else {
                println!("No resize parameters found, skipping resize");
            }

            let mut out = Vec::new();
            let mut c = Cursor::new(&mut out);
            
            let res = if convert_to_avif {
                println!("Starting AVIF encoding...");
                img.write_with_encoder(
                    codecs::avif::AvifEncoder::new_with_speed_quality(
                        &mut c,
                        u8_param("AVIF_SPEED", 1, 10, 5),
                        u8_param("AVIF_QUALITY", 1, 100, 70))
                )
            } else if convert_to_webp {
                println!("Starting WebP encoding...");
                img.write_with_encoder(
                    codecs::webp::WebPEncoder::new_lossless(&mut c)
                )
            } else {
                println!("Saving in original format...");
                // Determine original format from the image and save accordingly
                self.save_in_original_format(&img, &mut out)
            };

            match res {
                Ok(_) => {
                    if convert_to_avif {
                        println!("AVIF encoding successful: {} bytes -> {} bytes", body_size, out.len());
                    } else if convert_to_webp {
                        println!("WebP encoding successful: {} bytes -> {} bytes", body_size, out.len());
                    } else {
                        println!("Original format processing successful: {} bytes -> {} bytes", body_size, out.len());
                        // Set the correct content-type for original format
                        self.set_original_content_type();
                    }
                    
                    println!("Setting response body with {} bytes", out.len());
                    if out.is_empty() {
                        println!("ERROR: Output buffer is empty!");
                        return Action::Continue;
                    }
                    
                    // Try to update headers - this might cause HTTP/2 protocol issues
                    let content_length = out.len().to_string();
                    println!("Attempting to set Content-Length header to: {}", content_length);
                    
                    // Don't modify headers in response body phase - this might cause HTTP/2 issues
                    // self.set_http_response_header("Content-Length", Some(&content_length));
                    // self.set_http_response_header("Transfer-Encoding", None);
                    
                    // Set the response body - replace the entire body
                    println!("About to call set_http_response_body with offset=0, size={}, new_data_len={}", body_size, out.len());
                    self.set_http_response_body(0, body_size, &out);
                    println!("set_http_response_body call completed");
                }
                Err(e) => {
                    if convert_to_avif {
                        println!("AVIF encoding failed: {}", e);
                    } else if convert_to_webp {
                        println!("WebP encoding failed: {}", e);
                    } else {
                        println!("Original format processing failed: {}", e);
                    }
                    // Return original body on encoding failure
                    return Action::Continue;
                }
            }
        } else {
            println!("No response body to transform");
        }

        Action::Continue
    }
}

impl HttpBody {
    fn rsp_status(&mut self) -> Option<u16> {
        if let Some(status)= self.get_property(vec!["response.status"]) {
            if status.len() != 2 {
                println!("HTTP status property is not 2 bytes");
                return None;
            }
            return Some(u16::from_be_bytes([status[0], status[1]]));
        }
        None
    }
    
    fn parse_resize_params(&self) -> ResizeParams {
        let mut params = ResizeParams {
            width: None,
            height: None,
            fit: FitMode::default(),
        };
        
        // Get query string from request
        if let Some(query_bytes) = self.get_property(vec!["request.query"]) {
            if let Ok(query) = from_utf8(&query_bytes) {
                // Parse query parameters
                for param in query.split('&') {
                    let parts: Vec<&str> = param.split('=').collect();
                    if parts.len() == 2 {
                        let key = parts[0];
                        let value = parts[1];
                        
                        match key {
                            "width" => {
                                if let Ok(w) = value.parse::<u32>() {
                                    if w > 0 && w <= MAX_IMAGE_DIMENSION {
                                        params.width = Some(w);
                                    }
                                }
                            }
                            "height" => {
                                if let Ok(h) = value.parse::<u32>() {
                                    if h > 0 && h <= MAX_IMAGE_DIMENSION {
                                        params.height = Some(h);
                                    }
                                }
                            }
                            "fit" => {
                                params.fit = match value {
                                    "bounds" => FitMode::Bounds,
                                    "cover" => FitMode::Cover,
                                    "force" => FitMode::Force,
                                    _ => FitMode::Fit, // default
                                };
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
        
        params
    }
    
    
    fn apply_resize(&self, img: DynamicImage, params: &ResizeParams) -> DynamicImage {
        match (params.width, params.height) {
            (Some(width), Some(height)) => {
                // Both width and height specified, use fit mode
                match params.fit {
                    FitMode::Fit => {
                        // Crop to exact dimensions from center
                        img.resize_to_fill(width, height, image::imageops::FilterType::Lanczos3)
                    }
                    FitMode::Bounds => {
                        // Resize maintaining aspect ratio, fit within bounds
                        img.resize(width, height, image::imageops::FilterType::Lanczos3)
                    }
                    FitMode::Cover => {
                        // Resize to cover the area, may crop
                        img.resize_to_fill(width, height, image::imageops::FilterType::Lanczos3)
                    }
                    FitMode::Force => {
                        // Force exact dimensions, ignore aspect ratio
                        img.resize_exact(width, height, image::imageops::FilterType::Lanczos3)
                    }
                }
            }
            (Some(width), None) => {
                // Only width specified, maintain aspect ratio
                let height = (img.height() as f32 * width as f32 / img.width() as f32) as u32;
                img.resize(width, height, image::imageops::FilterType::Lanczos3)
            }
            (None, Some(height)) => {
                // Only height specified, maintain aspect ratio
                let width = (img.width() as f32 * height as f32 / img.height() as f32) as u32;
                img.resize(width, height, image::imageops::FilterType::Lanczos3)
            }
            (None, None) => img, // No resize needed
        }
    }
    
    fn parse_crop_params(&self) -> Option<CropParams> {
        let query_bytes = self.get_property(vec!["request.query"])?;
        let query = from_utf8(&query_bytes).ok()?;

        for param in query.split('&') {
            let mut parts = param.splitn(2, '=');
            let key = parts.next()?.trim();
            if key != "crop" {
                continue;
            }

            let value = parts.next().unwrap_or("").trim();
            if value.is_empty() {
                println!("Crop parameter present but empty");
                return None;
            }

            match parse_crop_value(value) {
                Ok(crop) => {
                    println!("Parsed crop params: {:?}", crop);
                    return Some(crop);
                }
                Err(err) => {
                    println!("Invalid crop parameter '{}': {}", value, err);
                    return None;
                }
            }
        }

        None
    }

    fn apply_crop(&self, img: DynamicImage, params: CropParams) -> Result<DynamicImage, String> {
        let image_width = img.width();
        let image_height = img.height();

        if image_width == 0 || image_height == 0 {
            return Err("Image has invalid dimensions".to_string());
        }

        let (mut target_width, mut target_height) = match params.mode {
            CropMode::AspectRatio {
                width_ratio,
                height_ratio,
            } => {
                let ratio = width_ratio as f32 / height_ratio as f32;
                let image_ratio = image_width as f32 / image_height as f32;

                if image_ratio > ratio {
                    let target_height = image_height;
                    let target_width = (target_height as f32 * ratio).round() as u32;
                    (target_width.max(1), target_height.max(1))
                } else {
                    let target_width = image_width;
                    let target_height = (target_width as f32 / ratio).round() as u32;
                    (target_width.max(1), target_height.max(1))
                }
            }
            CropMode::Dimensions { width, height } => (width.max(1), height.max(1)),
        };

        if target_width > image_width {
            println!(
                "Requested crop width {target_width} exceeds image width {image_width}, clamping"
            );
            target_width = image_width;
        }

        if target_height > image_height {
            println!(
                "Requested crop height {target_height} exceeds image height {image_height}, clamping"
            );
            target_height = image_height;
        }

        if target_width == 0 || target_height == 0 {
            return Err("Calculated crop dimensions are zero".to_string());
        }

        let left = self.resolve_anchor(params.anchor_x, image_width, target_width);
        let top = self.resolve_anchor(params.anchor_y, image_height, target_height);

        println!(
            "Cropping image at ({left}, {top}) with size {target_width}x{target_height}"
        );

        Ok(img.crop_imm(left, top, target_width, target_height))
    }

    fn resolve_anchor(
        &self,
        anchor: CropAnchor,
        image_dimension: u32,
        target_dimension: u32,
    ) -> u32 {
        let max_offset = image_dimension.saturating_sub(target_dimension);

        match anchor {
            CropAnchor::Center => max_offset / 2,
            CropAnchor::Pixels(px) => px.min(max_offset),
            CropAnchor::PercentOfImage(percent) => {
                let offset = (image_dimension as f32 * (percent / 100.0)).round();
                clamp_offset(offset, max_offset)
            }
            CropAnchor::OffsetPercent(percent) => {
                let leftover = image_dimension.saturating_sub(target_dimension);
                let offset = (leftover as f32 * (percent / 100.0)).round();
                clamp_offset(offset, max_offset)
            }
        }
    }

    fn save_in_original_format(&self, img: &DynamicImage, out: &mut Vec<u8>) -> Result<(), image::ImageError> {
        use std::io::Cursor;
        
        // Get the original file extension to determine format
        let format = if let Some(ext_bytes) = self.get_property(vec!["request.extension"]) {
            if let Ok(ext) = from_utf8(&ext_bytes) {
                match ext.to_lowercase().as_str() {
                    "jpg" | "jpeg" => image::ImageFormat::Jpeg,
                    "png" => image::ImageFormat::Png,
                    _ => image::ImageFormat::Jpeg, // Default to JPEG
                }
            } else {
                image::ImageFormat::Jpeg // Default to JPEG
            }
        } else {
            image::ImageFormat::Jpeg // Default to JPEG
        };
        
        let mut cursor = Cursor::new(out);
        
        match format {
            image::ImageFormat::Jpeg => {
                img.write_to(&mut cursor, image::ImageFormat::Jpeg)?;
            }
            image::ImageFormat::Png => {
                img.write_to(&mut cursor, image::ImageFormat::Png)?;
            }
            _ => {
                img.write_to(&mut cursor, image::ImageFormat::Jpeg)?;
            }
        }
        
        Ok(())
    }
    
    fn set_original_content_type(&self) {
        // Set the correct MIME type based on file extension
        if let Some(ext_bytes) = self.get_property(vec!["request.extension"]) {
            if let Ok(ext) = from_utf8(&ext_bytes) {
                let mime_type = match ext.to_lowercase().as_str() {
                    "jpg" | "jpeg" => "image/jpeg",
                    "png" => "image/png",
                    _ => "image/jpeg", // Default to JPEG
                };
                // Note: This might not work due to HTTP/2 header restrictions
                // but we'll try to set it anyway
                println!("Setting Content-Type to: {}", mime_type);
                // self.set_http_response_header("Content-Type", Some(mime_type));
            }
        }
    }

    fn select_target_format(
        &self,
        convert_to_avif: bool,
        convert_to_webp: bool,
        requested_formats: &RequestedFormats,
        accept_header: Option<&str>,
    ) -> Option<TargetFormat> {
        if !convert_to_avif && !convert_to_webp {
            println!("All format conversions disabled via environment variables");
            return None;
        }

        let accept_raw = accept_header.unwrap_or("");
        if !accept_raw.is_empty() {
            println!("Client Accept header: {}", accept_raw);
        }

        let accept = accept_raw.to_ascii_lowercase();
        let supports_avif = accept_header_allows(&accept, "image/avif");
        let supports_webp = accept_header_allows(&accept, "image/webp");
        let accepts_any_image = accept_header_allows(&accept, "image/*") || accept_header_allows(&accept, "*/*");

        let mut ordered_candidates: Vec<TargetFormat> = Vec::new();

        // Respect requested order when provided
        if !requested_formats.is_empty() {
            for format in requested_formats.iter() {
                match format {
                    TargetFormat::Avif if convert_to_avif => ordered_candidates.push(TargetFormat::Avif),
                    TargetFormat::Webp if convert_to_webp => ordered_candidates.push(TargetFormat::Webp),
                    _ => {}
                }
            }
        } else {
            // No fmt parameter: fall back to default preference (AVIF first)
            if convert_to_avif {
                ordered_candidates.push(TargetFormat::Avif);
            }
            if convert_to_webp {
                ordered_candidates.push(TargetFormat::Webp);
            }
        }

        for candidate in &ordered_candidates {
            let supported = match candidate {
                TargetFormat::Avif => supports_avif || accepts_any_image || accept.is_empty(),
                TargetFormat::Webp => supports_webp || accepts_any_image || accept.is_empty(),
            };

            if supported {
                return Some(*candidate);
            }

            println!(
                "Accept header does not permit {:?}; evaluating next candidate",
                candidate
            );
        }

        println!("Accept header does not permit requested formats; serving original image");
        None
    }

    fn parse_requested_formats(&self) -> RequestedFormats {
        let mut requested = RequestedFormats::empty();

        if let Some(query_bytes) = self.get_property(vec!["request.query"]) {
            if let Ok(query) = from_utf8(&query_bytes) {
                for param in query.split('&') {
                    let mut parts = param.splitn(2, '=');
                    let key = parts.next().unwrap_or("").trim();
                    if key != "fmt" {
                        continue;
                    }

                    let value = parts.next().unwrap_or("");
                    if value.is_empty() {
                        continue;
                    }

                    for token in value.split(',').map(|t| t.trim().to_ascii_lowercase()) {
                        match token.as_str() {
                            "avif" => requested.include(TargetFormat::Avif),
                            "webp" => requested.include(TargetFormat::Webp),
                            other if other.contains(' ') => {
                                for inner in other.split_whitespace() {
                                    match inner {
                                        "avif" => requested.include(TargetFormat::Avif),
                                        "webp" => requested.include(TargetFormat::Webp),
                                        _ => {}
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }

        requested
    }
}

fn str_param(name: &str) -> Result<String, VarError>
{
    let val = env::var(name)?;
    if val.is_empty() {
        return Err(VarError::NotPresent);
    }

    Ok(val)
}

fn u8_param(name: &str, min: u8, max: u8, default: u8) -> u8
{
    let Ok(val) = env::var(name) else {
        println!("Param {} is not set, using default value {}", name, default);
        return default;
    };
    if val.is_empty() {
        println!("Param {} is not set, using default value {}", name, default);
        return default;
    }

    let val = match val.parse() {
        Err(_) => {
            println!("Param {} is not a valid number, using default value {}", name, default);
            return default;
        }
        Ok(v) => v,
    };
    if val < min {
        println!("Param {} is below minimum {}, using default value {}", name, min, default);
        return default;
    }
    if val > max {
        println!("Param {} is above maximum {}, using default value {}", name, max, default);
        return default;
    }

    val
}

fn bool_param(name: &str, default: bool) -> bool
{
    match env::var(name) {
        Ok(val) => {
            if val.is_empty() {
                println!(
                    "Param {} is empty, using default value {}",
                    name,
                    default
                );
                return default;
            }
            match val.to_ascii_lowercase().as_str() {
                "1" | "true" | "yes" | "on" => true,
                "0" | "false" | "no" | "off" => false,
                other => {
                    println!(
                        "Param {} has invalid boolean value '{}', using default {}",
                        name,
                        other,
                        default
                    );
                    default
                }
            }
        }
        Err(_) => default,
    }
}

fn accept_header_allows(accept: &str, needle: &str) -> bool
{
    accept.split(',').any(|segment| {
        let trimmed = segment.trim();
        let media_type = trimmed.split_once(';').map(|(m, _)| m.trim()).unwrap_or(trimmed);
        media_type == needle
    })
}

fn parse_crop_value(value: &str) -> Result<CropParams, String> {
    let tokens: Vec<&str> = value.split(',').map(|t| t.trim()).filter(|t| !t.is_empty()).collect();
    if tokens.is_empty() {
        return Err("no crop tokens provided".to_string());
    }

    if tokens.len() == 1 && tokens[0].contains(':') {
        let ratio_parts: Vec<&str> = tokens[0].split(':').collect();
        if ratio_parts.len() != 2 {
            return Err("invalid aspect ratio format".to_string());
        }
        let width_ratio = ratio_parts[0]
            .parse::<u32>()
            .map_err(|_| "invalid width ratio".to_string())?;
        let height_ratio = ratio_parts[1]
            .parse::<u32>()
            .map_err(|_| "invalid height ratio".to_string())?;
        if width_ratio == 0 || height_ratio == 0 {
            return Err("aspect ratios must be greater than zero".to_string());
        }
        return Ok(CropParams {
            mode: CropMode::AspectRatio {
                width_ratio,
                height_ratio,
            },
            anchor_x: CropAnchor::Center,
            anchor_y: CropAnchor::Center,
        });
    }

    if tokens.len() < 2 {
        return Err("crop requires at least width and height".to_string());
    }

    let width = tokens[0]
        .parse::<u32>()
        .map_err(|_| "invalid crop width".to_string())?;
    let height = tokens[1]
        .parse::<u32>()
        .map_err(|_| "invalid crop height".to_string())?;
    if width == 0 || height == 0 {
        return Err("crop width and height must be greater than zero".to_string());
    }

    let mut anchor_x = CropAnchor::Center;
    let mut anchor_y = CropAnchor::Center;

    for token in tokens.iter().skip(2) {
        if token.starts_with('x') && !token.contains("offset-") {
            anchor_x = parse_anchor_value(*token, Axis::Horizontal)?;
        } else if token.starts_with('y') && !token.contains("offset-") {
            anchor_y = parse_anchor_value(*token, Axis::Vertical)?;
        } else if token.starts_with("offset-x") {
            let value = token.trim_start_matches("offset-x");
            anchor_x = parse_offset_anchor(value, Axis::Horizontal)?;
        } else if token.starts_with("offset-y") {
            let value = token.trim_start_matches("offset-y");
            anchor_y = parse_offset_anchor(value, Axis::Vertical)?;
        } else {
            println!("Ignoring unrecognized crop token '{token}'");
        }
    }

    Ok(CropParams {
        mode: CropMode::Dimensions { width, height },
        anchor_x,
        anchor_y,
    })
}

#[derive(Debug, Clone, Copy)]
enum Axis {
    Horizontal,
    Vertical,
}

fn parse_anchor_value(token: &str, axis: Axis) -> Result<CropAnchor, String> {
    let numeric_part = &token[1..]; // remove leading x or y
    if numeric_part.ends_with('p') {
        let percent_str = &numeric_part[..numeric_part.len() - 1];
        let percent = percent_str
            .parse::<f32>()
            .map_err(|_| format!("invalid percentage in {} position", axis_name(axis)))?;
        if !(0.0..=MAX_CROP_PERCENT).contains(&percent) {
            return Err(format!(
                "percentage in {} position must be between 0 and {}",
                axis_name(axis), MAX_CROP_PERCENT
            ));
        }
        return Ok(CropAnchor::PercentOfImage(percent));
    }

    let pixels = numeric_part
        .parse::<u32>()
        .map_err(|_| format!("invalid pixel offset in {} position", axis_name(axis)))?;
    Ok(CropAnchor::Pixels(pixels))
}

fn parse_offset_anchor(token: &str, axis: Axis) -> Result<CropAnchor, String> {
    let numeric_part = token.trim();
    let percent = numeric_part
        .parse::<f32>()
        .map_err(|_| format!("invalid offset percentage in {} position", axis_name(axis)))?;
    if !(0.0..=MAX_CROP_PERCENT).contains(&percent) {
        return Err(format!(
            "offset percentage in {} position must be between 0 and {}",
            axis_name(axis), MAX_CROP_PERCENT
        ));
    }
    Ok(CropAnchor::OffsetPercent(percent))
}

fn axis_name(axis: Axis) -> &'static str {
    match axis {
        Axis::Horizontal => "horizontal",
        Axis::Vertical => "vertical",
    }
}

fn clamp_offset(offset: f32, max_offset: u32) -> u32 {
    offset
        .max(0.0)
        .min(max_offset as f32)
        .round()
        .clamp(0.0, max_offset as f32) as u32
}

fn format_percent(value: f32) -> String {
    if (value - value.round()).abs() < f32::EPSILON {
        format!("{}", value.round() as i32)
    } else {
        format!("{:.2}", value)
    }
}