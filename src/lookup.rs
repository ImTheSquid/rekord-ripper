pub struct LookupResponse {
    name: String,
    artist: String,
    duration: usize,
}

const APPLE_MUSIC_URL_TEMPLATE: &str = "https://itunes.apple.com/lookup?id={}";

pub fn search_apple_music(query: &str) -> anyhow::Result<LookupResponse> {
    // ureq::get(uri)
    todo!()
}
