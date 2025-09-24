use proxy_wasm::traits::*;
use proxy_wasm::types::*;
use image::*;
use std::{env, env::VarError, io::Cursor, str::from_utf8};

#[derive(Debug, Clone)]
struct ResizeParams {
    width: Option<u32>,
    height: Option<u32>,
    fit: FitMode,
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
        if resize_params.width.is_some() || resize_params.height.is_some() {
            
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

        // Check if AVIF conversion is enabled
        let convert_to_avif = match str_param("CONVERT_TO_AVIF") {
            Ok(val) => val.to_lowercase() == "true" || val == "1",
            Err(_) => true, // Default to true for backward compatibility
        };
        
        // Set the target format based on environment variable
        if convert_to_avif {
            self.set_http_request_header("Image-Format", Some("image/avif"));
        } else {
            // Keep original format, but still indicate processing needed for resize
            self.set_http_request_header("Image-Format", Some("original-with-processing"));
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
        // instruct cache to vary by this header so "original" and "image/avif" are cached separately
        self.add_http_response_header("Vary", "Image-Format");
        
        // Add resize parameters to Vary header if present and set operations header
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
                self.add_http_response_header("X-Img-Operations", &resize_info);
            }
        }

        if content_type == "original" {
            return Action::Continue;
        };

        // image to be transformed, set headers accordingly
        self.set_http_response_header("Content-Length", None);
        self.set_http_response_header("Transfer-Encoding", Some("Chunked"));
        
        // Set content type based on whether we're converting to AVIF or keeping original
        if content_type == "image/avif" {
            self.set_http_response_header("Content-Type", Some("image/avif"));
        } else if content_type == "original-with-processing" {
            // Keep original content type, will be determined from the actual image
            // Content-Type will be set in the response body phase
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
        let process_original = content_type == "original-with-processing";
        
        if !convert_to_avif && !process_original {
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
            } else {
                println!("Saving in original format...");
                // Determine original format from the image and save accordingly
                self.save_in_original_format(&img, &mut out)
            };

            match res {
                Ok(_) => {
                    if convert_to_avif {
                        println!("AVIF encoding successful: {} bytes -> {} bytes", body_size, out.len());
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
                                    if w > 0 && w <= 10000 { // reasonable limits
                                        params.width = Some(w);
                                    }
                                }
                            }
                            "height" => {
                                if let Ok(h) = value.parse::<u32>() {
                                    if h > 0 && h <= 10000 { // reasonable limits
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
