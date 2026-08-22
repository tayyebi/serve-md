//! Minimal file-extension -> MIME type table for serving static assets.

use std::path::Path;

pub fn guess(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        "html" | "htm" => "text/html; charset=utf-8",
        "md" | "markdown" => "text/markdown; charset=utf-8",
        "txt" => "text/plain; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "json" => "application/json",
        "xml" => "application/xml",
        "csv" => "text/csv; charset=utf-8",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "ico" => "image/x-icon",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "ogv" => "video/ogg",
        "mov" => "video/quicktime",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "ogg" => "audio/ogg",
        "oga" => "audio/ogg",
        "flac" => "audio/flac",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "ttf" => "font/ttf",
        "otf" => "font/otf",
        "eot" => "application/vnd.ms-fontobject",
        "pdf" => "application/pdf",
        "wasm" => "application/wasm",
        "zip" => "application/zip",
        _ => "application/octet-stream",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guesses_known_extensions() {
        assert_eq!(guess(Path::new("a.css")), "text/css; charset=utf-8");
        assert_eq!(guess(Path::new("a.png")), "image/png");
        assert_eq!(guess(Path::new("favicon.ico")), "image/x-icon");
        assert_eq!(guess(Path::new("clip.mp4")), "video/mp4");
    }

    #[test]
    fn falls_back_to_octet_stream() {
        assert_eq!(guess(Path::new("a.unknownext")), "application/octet-stream");
        assert_eq!(guess(Path::new("noext")), "application/octet-stream");
    }
}
