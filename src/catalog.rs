//! Model catalogs from models.dev, a public catalog of providers and models
//! that needs no key. No harness runs for this: the factory reads the catalog
//! itself, keeps it for an hour, and falls back to the last copy it saved.

use crate::{
    harness::{CatalogSource, ModelInfo},
    microvm::assets,
};
use serde_json::Value;
use std::{
    io,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::{fs, sync::Mutex};

const URL: &str = "https://models.dev/api.json";
const FRESH: Duration = Duration::from_secs(60 * 60);
/// After a failed fetch, the saved copy serves this long before trying again.
const RETRY: Duration = Duration::from_secs(5 * 60);

struct Cached {
    data: Arc<Value>,
    fresh_until: Instant,
}

pub struct Catalog {
    /// The last catalog fetched, for when models.dev cannot be reached.
    file: PathBuf,
    cached: Mutex<Option<Cached>>,
}

/// `claude-opus-4-5-20251101` is a dated snapshot of `claude-opus-4-5`.
fn dated_twin<'a>(id: &'a str, models: &serde_json::Map<String, Value>) -> Option<&'a str> {
    let (base, date) = id.rsplit_once('-')?;
    (date.len() == 8 && date.bytes().all(|b| b.is_ascii_digit()) && models.contains_key(base))
        .then_some(base)
}

impl Catalog {
    pub fn new(data_dir: &Path) -> Self {
        Self {
            file: data_dir.join("catalog").join("models.dev.json"),
            cached: Mutex::new(None),
        }
    }

    async fn fetch(&self) -> io::Result<Value> {
        let raw = assets::run("curl", &["-fsSL", "--max-time", "30", URL]).await?;
        let data = serde_json::from_str(&raw).map_err(io::Error::other)?;
        if let Some(dir) = self.file.parent() {
            fs::create_dir_all(dir).await?;
        }
        let partial = self.file.with_extension("partial");
        fs::write(&partial, &raw).await?;
        fs::rename(&partial, &self.file).await?;
        Ok(data)
    }

    async fn saved(&self) -> io::Result<Value> {
        let raw = fs::read_to_string(&self.file).await?;
        serde_json::from_str(&raw).map_err(io::Error::other)
    }

    async fn data(&self) -> io::Result<Arc<Value>> {
        let mut cached = self.cached.lock().await;
        if let Some(cached) = cached.as_ref()
            && Instant::now() < cached.fresh_until
        {
            return Ok(cached.data.clone());
        }
        let (data, fresh_for) = match self.fetch().await {
            Ok(data) => (Arc::new(data), FRESH),
            Err(error) => match cached.as_ref() {
                Some(stale) => (stale.data.clone(), RETRY),
                None => match self.saved().await {
                    Ok(data) => (Arc::new(data), RETRY),
                    Err(_) => {
                        return Err(io::Error::other(format!(
                            "models.dev could not be reached and no earlier copy is saved: {error}"
                        )));
                    }
                },
            },
        };
        *cached = Some(Cached {
            data: data.clone(),
            fresh_until: Instant::now() + fresh_for,
        });
        Ok(data)
    }

    /// The source's models, newest first, without dated snapshots of models
    /// that are also listed undated.
    pub async fn models(&self, source: &CatalogSource) -> io::Result<Vec<ModelInfo>> {
        let data = self.data().await?;
        let Some(models) = data[source.provider]["models"].as_object() else {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("models.dev has no provider {}", source.provider),
            ));
        };
        let mut listed: Vec<(&str, ModelInfo)> = models
            .iter()
            .filter(|(id, _)| dated_twin(id, models).is_none())
            .map(|(id, model)| {
                let info = ModelInfo {
                    provider: source.provider.to_owned(),
                    id: id.clone(),
                    name: model["name"].as_str().unwrap_or(id).to_owned(),
                    context_window: model["limit"]["context"].as_u64(),
                    reasoning: model["reasoning"] == true,
                    base_url: Some(source.base_url.to_owned()),
                };
                (model["release_date"].as_str().unwrap_or(""), info)
            })
            .collect();
        listed.sort_by(|(a, a_info), (b, b_info)| b.cmp(a).then(a_info.name.cmp(&b_info.name)));
        Ok(listed.into_iter().map(|(_, info)| info).collect())
    }

    /// The effort levels models.dev lists for the model. A model without any
    /// gets the single level "default", which leaves it to the harness.
    pub async fn levels(&self, source: &CatalogSource, model: &str) -> io::Result<Vec<String>> {
        let data = self.data().await?;
        let entry = &data[source.provider]["models"][model];
        if entry.is_null() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("models.dev has no model {model} for {}", source.provider),
            ));
        }
        let levels: Vec<String> = entry["reasoning_options"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|option| option["type"] == "effort")
            .flat_map(|option| option["values"].as_array().into_iter().flatten())
            .filter_map(|level| level.as_str().map(str::to_owned))
            .collect();
        Ok(if levels.is_empty() {
            vec!["default".into()]
        } else {
            levels
        })
    }
}
