use futures_util::StreamExt;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::mpsc::Sender;
use tokio::fs::{self, File};
use tokio::io::AsyncWriteExt;
use tokio::task::JoinSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DownloadableWhisperModel {
    pub family: &'static str,
    pub model_id: &'static str,
    pub quantization: &'static str,
    pub size_label: &'static str,
    pub size_bytes: u64,
    pub repo: &'static str,
}

impl DownloadableWhisperModel {
    pub fn filename(&self) -> String {
        format!("ggml-{}.bin", self.model_id)
    }

    pub fn temp_filename(&self) -> String {
        format!("{}.part", self.filename())
    }

    pub fn destination_path(&self, destination: &Path) -> PathBuf {
        destination.join(self.filename())
    }

    pub fn temp_path(&self, destination: &Path) -> PathBuf {
        destination.join(self.temp_filename())
    }

    pub fn download_url(&self) -> String {
        format!(
            "https://huggingface.co/{}/resolve/main/ggml-{}.bin",
            self.repo, self.model_id
        )
    }

    pub fn matches_query(&self, query: &str) -> bool {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return true;
        }

        let haystacks = [
            self.family.to_lowercase(),
            self.model_id.to_lowercase(),
            self.quantization.to_lowercase(),
            self.size_label.to_lowercase(),
        ];
        haystacks.iter().any(|haystack| haystack.contains(&query))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhisperModelDownloadStatus {
    Idle,
    Queued,
    Downloading {
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    },
    Downloaded,
    Skipped,
    Failed(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WhisperModelDownloadEvent {
    Queued {
        model_id: String,
    },
    Started {
        model_id: String,
        total_bytes: Option<u64>,
    },
    Progress {
        model_id: String,
        downloaded_bytes: u64,
        total_bytes: Option<u64>,
    },
    Downloaded {
        model_id: String,
        destination: PathBuf,
    },
    Skipped {
        model_id: String,
        destination: PathBuf,
    },
    Failed {
        model_id: String,
        error: String,
    },
}

pub const WHISPER_MODEL_DOWNLOADS: &[DownloadableWhisperModel] = &[
    DownloadableWhisperModel {
        family: "Tiny",
        model_id: "tiny",
        quantization: "Default",
        size_label: "77.7 MB",
        size_bytes: 77_700_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Tiny",
        model_id: "tiny.en",
        quantization: "Default",
        size_label: "77.7 MB",
        size_bytes: 77_700_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Tiny",
        model_id: "tiny-q5_1",
        quantization: "Q5_1",
        size_label: "32.2 MB",
        size_bytes: 32_200_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Tiny",
        model_id: "tiny.en-q5_1",
        quantization: "Q5_1",
        size_label: "32.2 MB",
        size_bytes: 32_200_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Tiny",
        model_id: "tiny-q8_0",
        quantization: "Q8_0",
        size_label: "43.5 MB",
        size_bytes: 43_500_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Tiny",
        model_id: "tiny.en-q8_0",
        quantization: "Q8_0",
        size_label: "43.6 MB",
        size_bytes: 43_600_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Base",
        model_id: "base",
        quantization: "Default",
        size_label: "148 MB",
        size_bytes: 148_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Base",
        model_id: "base.en",
        quantization: "Default",
        size_label: "148 MB",
        size_bytes: 148_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Base",
        model_id: "base-q5_1",
        quantization: "Q5_1",
        size_label: "59.7 MB",
        size_bytes: 59_700_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Base",
        model_id: "base.en-q5_1",
        quantization: "Q5_1",
        size_label: "59.7 MB",
        size_bytes: 59_700_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Base",
        model_id: "base-q8_0",
        quantization: "Q8_0",
        size_label: "81.8 MB",
        size_bytes: 81_800_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Base",
        model_id: "base.en-q8_0",
        quantization: "Q8_0",
        size_label: "81.8 MB",
        size_bytes: 81_800_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Small",
        model_id: "small",
        quantization: "Default",
        size_label: "488 MB",
        size_bytes: 488_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Small",
        model_id: "small.en",
        quantization: "Default",
        size_label: "488 MB",
        size_bytes: 488_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Small",
        model_id: "small.en-tdrz",
        quantization: "Default",
        size_label: "465 MB",
        size_bytes: 465_000_000,
        repo: "akashmjn/tinydiarize-whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Small",
        model_id: "small-q5_1",
        quantization: "Q5_1",
        size_label: "190 MB",
        size_bytes: 190_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Small",
        model_id: "small.en-q5_1",
        quantization: "Q5_1",
        size_label: "190 MB",
        size_bytes: 190_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Small",
        model_id: "small-q8_0",
        quantization: "Q8_0",
        size_label: "264 MB",
        size_bytes: 264_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Small",
        model_id: "small.en-q8_0",
        quantization: "Q8_0",
        size_label: "264 MB",
        size_bytes: 264_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Medium",
        model_id: "medium",
        quantization: "Default",
        size_label: "1.53 GB",
        size_bytes: 1_530_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Medium",
        model_id: "medium.en",
        quantization: "Default",
        size_label: "1.53 GB",
        size_bytes: 1_530_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Medium",
        model_id: "medium-q5_0",
        quantization: "Q5_0",
        size_label: "539 MB",
        size_bytes: 539_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Medium",
        model_id: "medium.en-q5_0",
        quantization: "Q5_0",
        size_label: "539 MB",
        size_bytes: 539_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Medium",
        model_id: "medium-q8_0",
        quantization: "Q8_0",
        size_label: "823 MB",
        size_bytes: 823_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Medium",
        model_id: "medium.en-q8_0",
        quantization: "Q8_0",
        size_label: "823 MB",
        size_bytes: 823_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Large-v1",
        model_id: "large-v1",
        quantization: "Default",
        size_label: "3.09 GB",
        size_bytes: 3_090_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Large-v2",
        model_id: "large-v2",
        quantization: "Default",
        size_label: "3.09 GB",
        size_bytes: 3_090_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Large-v2",
        model_id: "large-v2-q5_0",
        quantization: "Q5_0",
        size_label: "1.08 GB",
        size_bytes: 1_080_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Large-v2",
        model_id: "large-v2-q8_0",
        quantization: "Q8_0",
        size_label: "1.66 GB",
        size_bytes: 1_660_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Large-v3",
        model_id: "large-v3",
        quantization: "Default",
        size_label: "3.1 GB",
        size_bytes: 3_100_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Large-v3",
        model_id: "large-v3-q5_0",
        quantization: "Q5_0",
        size_label: "1.08 GB",
        size_bytes: 1_080_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Large-v3-turbo",
        model_id: "large-v3-turbo",
        quantization: "Default",
        size_label: "1.62 GB",
        size_bytes: 1_620_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Large-v3-turbo",
        model_id: "large-v3-turbo-q5_0",
        quantization: "Q5_0",
        size_label: "574 MB",
        size_bytes: 574_000_000,
        repo: "ggerganov/whisper.cpp",
    },
    DownloadableWhisperModel {
        family: "Large-v3-turbo",
        model_id: "large-v3-turbo-q8_0",
        quantization: "Q8_0",
        size_label: "874 MB",
        size_bytes: 874_000_000,
        repo: "ggerganov/whisper.cpp",
    },
];

pub fn whisper_download_cache_dir() -> PathBuf {
    glib::user_cache_dir().join("whisper")
}

pub fn list_downloadable_whisper_models() -> &'static [DownloadableWhisperModel] {
    WHISPER_MODEL_DOWNLOADS
}

pub fn model_selection_after_refresh(
    available_models: &[PathBuf],
    configured_model: Option<&str>,
) -> String {
    if let Some(configured_model) = configured_model {
        if available_models
            .iter()
            .any(|path| path.to_string_lossy() == configured_model)
        {
            return configured_model.to_string();
        }
        return configured_model.to_string();
    }

    available_models
        .first()
        .map(|path| path.to_string_lossy().to_string())
        .unwrap_or_else(|| "Other...".to_string())
}

pub fn download_whisper_models(
    models: Vec<DownloadableWhisperModel>,
    destination: PathBuf,
    max_parallel: usize,
    sender: Sender<WhisperModelDownloadEvent>,
) -> std::thread::JoinHandle<()> {
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build();

        match runtime {
            Ok(runtime) => runtime.block_on(download_whisper_models_async(
                models,
                destination,
                max_parallel,
                sender,
            )),
            Err(error) => {
                for model in models {
                    let _ = sender.send(WhisperModelDownloadEvent::Failed {
                        model_id: model.model_id.to_string(),
                        error: error.to_string(),
                    });
                }
            }
        }
    })
}

pub async fn download_whisper_models_async(
    models: Vec<DownloadableWhisperModel>,
    destination: PathBuf,
    max_parallel: usize,
    sender: Sender<WhisperModelDownloadEvent>,
) {
    let client = match reqwest::Client::builder()
        .user_agent("whisper-gtk/0.1")
        .build()
    {
        Ok(client) => client,
        Err(error) => {
            for model in models {
                let _ = sender.send(WhisperModelDownloadEvent::Failed {
                    model_id: model.model_id.to_string(),
                    error: error.to_string(),
                });
            }
            return;
        }
    };

    let download_one = move |model: DownloadableWhisperModel,
                             destination: PathBuf,
                             sender: Sender<WhisperModelDownloadEvent>| {
        let client = client.clone();
        async move { download_one_model(&client, model, destination, sender).await }
    };

    download_whisper_models_with(models, destination, max_parallel, sender, download_one).await;
}

pub async fn download_whisper_models_with<F, Fut>(
    models: Vec<DownloadableWhisperModel>,
    destination: PathBuf,
    max_parallel: usize,
    sender: Sender<WhisperModelDownloadEvent>,
    download_one: F,
) where
    F: Fn(DownloadableWhisperModel, PathBuf, Sender<WhisperModelDownloadEvent>) -> Fut
        + Clone
        + Send
        + Sync
        + 'static,
    Fut: Future<Output = io::Result<()>> + Send + 'static,
{
    let max_parallel = max_parallel.max(1);
    if let Err(error) = std::fs::create_dir_all(&destination) {
        for model in models {
            let _ = sender.send(WhisperModelDownloadEvent::Failed {
                model_id: model.model_id.to_string(),
                error: error.to_string(),
            });
        }
        return;
    }

    let mut pending_models = Vec::new();
    for model in models {
        let final_path = model.destination_path(&destination);
        if final_path.is_file() {
            let _ = sender.send(WhisperModelDownloadEvent::Skipped {
                model_id: model.model_id.to_string(),
                destination: final_path,
            });
            continue;
        }

        let _ = sender.send(WhisperModelDownloadEvent::Queued {
            model_id: model.model_id.to_string(),
        });
        pending_models.push(model);
    }

    let mut active = JoinSet::new();
    let mut active_count = 0usize;

    for model in pending_models {
        while active_count >= max_parallel {
            if active.join_next().await.is_some() {
                active_count = active_count.saturating_sub(1);
            }
        }

        let sender = sender.clone();
        let destination = destination.clone();
        let download_one = download_one.clone();
        active.spawn(async move {
            let _ = download_one(model, destination, sender).await;
        });
        active_count += 1;
    }

    while active_count > 0 {
        if active.join_next().await.is_some() {
            active_count = active_count.saturating_sub(1);
        }
    }
}

async fn download_one_model(
    client: &reqwest::Client,
    model: DownloadableWhisperModel,
    destination: PathBuf,
    sender: Sender<WhisperModelDownloadEvent>,
) -> io::Result<()> {
    let final_path = model.destination_path(&destination);
    let temp_path = model.temp_path(&destination);
    let outcome = async {
        let response = client
            .get(model.download_url())
            .send()
            .await
            .map_err(|error| io::Error::other(error.to_string()))?;

        if !response.status().is_success() {
            return Err(io::Error::other(format!(
                "download request failed with status {}",
                response.status()
            )));
        }

        let total_bytes = response.content_length().or(Some(model.size_bytes));
        let _ = sender.send(WhisperModelDownloadEvent::Started {
            model_id: model.model_id.to_string(),
            total_bytes,
        });

        let mut file = File::create(&temp_path).await?;
        let mut downloaded_bytes = 0u64;
        let mut stream = response.bytes_stream();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|error| io::Error::other(error.to_string()))?;
            file.write_all(&chunk).await?;
            downloaded_bytes += chunk.len() as u64;
            let _ = sender.send(WhisperModelDownloadEvent::Progress {
                model_id: model.model_id.to_string(),
                downloaded_bytes,
                total_bytes,
            });
        }

        file.flush().await?;
        drop(file);

        fs::rename(&temp_path, &final_path).await?;
        Ok(())
    }
    .await;

    match outcome {
        Ok(()) => {
            let _ = sender.send(WhisperModelDownloadEvent::Downloaded {
                model_id: model.model_id.to_string(),
                destination: final_path,
            });
            Ok(())
        }
        Err(error) => {
            let _ = fs::remove_file(&temp_path).await;
            let error = error.to_string();
            let _ = sender.send(WhisperModelDownloadEvent::Failed {
                model_id: model.model_id.to_string(),
                error: error.clone(),
            });
            Err(io::Error::other(error))
        }
    }
}
