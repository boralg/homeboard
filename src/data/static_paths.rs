use rust_embed::RustEmbed;
use sha2::{Digest, Sha256};

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Asset;

impl Asset {
    fn get_hashed_url(file: &str) -> String {
        if let Some(content) = Asset::get(file) {
            let hash = Sha256::digest(&content.data);
            let hash_str = format!("{:x}", hash);
            format!("/assets/{}?v={}", file, &hash_str[..8])
        } else {
            panic!("/assets/{} not found", file);
        }
    }
}

pub struct StaticPaths {
    // pub logo_path: String,
}

impl StaticPaths {
    pub fn new() -> Self {
        Self {
            // logo_path: Asset::get_hashed_url("images/flake_white.png"),
        }
    }
}
