//! Document asset URLs, redirects, and bounded local/HTTP/data reads.

use std::collections::HashMap;
use std::io::Read;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use ureq::ResponseExt;
use url::Url;

const MAX_ASSET_BYTES: u64 = 64 * 1024 * 1024;

pub struct Asset {
    pub url: Url,
    pub bytes: Vec<u8>,
}

pub struct Assets {
    agent: ureq::Agent,
    cache: Mutex<HashMap<Url, Option<Arc<Asset>>>>,
}

impl Assets {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            .timeout_global(Some(Duration::from_secs(30)))
            .build();
        Self {
            agent: config.into(),
            cache: Mutex::new(HashMap::new()),
        }
    }

    pub fn fetch(&self, base: &Url, reference: &str) -> Option<Arc<Asset>> {
        let mut url = base.join(reference.trim()).ok()?;
        url.set_fragment(None);
        if let Some(found) = self.cache.lock().ok()?.get(&url) {
            return found.clone();
        }
        let asset = self.read(&url).map(Arc::new);
        self.cache.lock().ok()?.insert(url, asset.clone());
        asset
    }

    fn read(&self, url: &Url) -> Option<Asset> {
        match url.scheme() {
            "file" => {
                let file = std::fs::File::open(url.to_file_path().ok()?).ok()?;
                let mut bytes = Vec::new();
                file.take(MAX_ASSET_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .ok()?;
                ((bytes.len() as u64) <= MAX_ASSET_BYTES).then(|| Asset {
                    url: url.clone(),
                    bytes,
                })
            }
            "http" | "https" => {
                let mut response = self.agent.get(url.as_str()).call().ok()?;
                let final_url = Url::parse(&response.get_uri().to_string()).ok()?;
                let bytes = response
                    .body_mut()
                    .with_config()
                    .limit(MAX_ASSET_BYTES)
                    .read_to_vec()
                    .ok()?;
                Some(Asset {
                    url: final_url,
                    bytes,
                })
            }
            "data" => super::image::decode_data_uri(url.as_str()).map(|bytes| Asset {
                url: url.clone(),
                bytes,
            }),
            _ => None,
        }
    }
}
