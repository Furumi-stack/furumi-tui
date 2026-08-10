//! Local, offline-first music embeddings and exact cosine search.
//!
//! SQLite owns the durable vectors. The in-memory index is deliberately
//! replaceable: it is rebuilt for the active profile and never becomes a
//! second source of truth.

use std::collections::{HashMap, VecDeque};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use anyhow::{Context as _, Result};
use futures_util::StreamExt as _;
use rodio::{Decoder, Source as _};
use rustfft::FftPlanner;
use rustfft::num_complex::Complex;
use sha2::{Digest as _, Sha256};
use tokio::io::AsyncWriteExt as _;
use tract_onnx::prelude::*;
use tract_onnx::tract_core::dims;

use crate::app::event::AppEvent;
use crate::config::settings::SimilaritySettings;
use crate::library::models::TrackItem;
use crate::library::{Library, StoredEmbedding};

const SAMPLE_RATE: usize = 16_000;
const FRAME_SIZE: usize = 512;
const HOP_SIZE: usize = 256;
const MEL_BANDS: usize = 96;
const PATCH_FRAMES: usize = 128;
const PATCH_HOP: usize = 62;
const EMBEDDING_DIMENSIONS: usize = 1280;
const MODEL_BATCH: usize = 8;
const MAX_MODEL_BYTES: usize = 64 * 1024 * 1024;
const RESULT_LIMIT: usize = 50;
const PEER_CANDIDATE_MAX_PER_ARTIST: usize = 10;
const NEAR_DUPLICATE_COSINE: f32 = 0.995;
const FULL_TRACK_MAX_SECONDS: u32 = 5 * 60;
const LONG_TRACK_WINDOW_SECONDS: u32 = 60;
const LONG_TRACK_WINDOWS: usize = 3;

pub const DEFAULT_MODEL_ID: &str = "discogs-effnet-bsdynamic-1";
pub const DEFAULT_PROFILE_ID: &str = "furumi-full-track-v1";

#[derive(Debug, Clone, Copy)]
pub struct ProfileSpec {
    pub id: &'static str,
    pub title: &'static str,
}

pub const PROFILES: &[ProfileSpec] = &[ProfileSpec {
    id: DEFAULT_PROFILE_ID,
    title: "Full track / balanced long track",
}];

#[derive(Debug, Clone, Copy)]
pub struct ModelSpec {
    pub id: &'static str,
    pub version: &'static str,
    pub filename: &'static str,
    pub url: &'static str,
    pub sha256: &'static str,
    pub dimensions: usize,
    pub license: &'static str,
}

pub const MODELS: &[ModelSpec] = &[ModelSpec {
    id: DEFAULT_MODEL_ID,
    version: "1",
    filename: "discogs-effnet-bsdynamic-1.onnx",
    url: "https://essentia.upf.edu/models/feature-extractors/discogs-effnet/discogs-effnet-bsdynamic-1.onnx",
    sha256: "a280825b334797cf677939db8cd5762c0392aedd0ca6415dbc1cd083f045e43c",
    dimensions: EMBEDDING_DIMENSIONS,
    license: "CC BY-NC-SA 4.0 (or proprietary from MTG)",
}];

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Phase {
    #[default]
    Disabled,
    Downloading,
    Loading,
    Processing,
    Ready,
    Error,
}

impl Phase {
    pub fn label(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Downloading => "downloading model",
            Self::Loading => "loading model",
            Self::Processing => "processing",
            Self::Ready => "ready",
            Self::Error => "error",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SimilarityStatus {
    pub phase: Phase,
    pub active_profile: Option<String>,
    pub target_profile: Option<String>,
    pub model: String,
    pub total_tracks: usize,
    pub completed_tracks: usize,
    pub failed_tracks: usize,
    pub stored_vectors: usize,
    pub stored_bytes: u64,
    pub current_track: Option<String>,
    pub last_error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct QueryVector {
    pub profile_id: String,
    pub vector: Vec<f32>,
    pub source_content_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct SimilarTrack {
    pub track: TrackItem,
    pub score: f32,
    pub embedding_signature: [u8; music_dht::similarity::SIMILARITY_SIGNATURE_BYTES],
}

#[derive(Default)]
struct Index {
    profile_id: Option<String>,
    entries: Vec<StoredEmbedding>,
}

type RunnableModel = Arc<TypedRunnableModel>;

pub struct Manager {
    library: Arc<Library>,
    event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
    settings: Mutex<SimilaritySettings>,
    workers: AtomicUsize,
    generation: AtomicU64,
    pipeline_running: AtomicBool,
    rescan_requested: AtomicBool,
    status: Mutex<SimilarityStatus>,
    index: RwLock<Index>,
    model: Mutex<Option<(String, RunnableModel)>>,
    model_dir: PathBuf,
}

impl Manager {
    pub fn new(
        library: Arc<Library>,
        event_tx: tokio::sync::mpsc::UnboundedSender<AppEvent>,
        settings: SimilaritySettings,
    ) -> Arc<Self> {
        let model_dir = crate::config::project_dirs()
            .map(|dirs| dirs.cache_dir().join("similarity-models"))
            .unwrap_or_else(|| PathBuf::from("similarity-models"));
        let mut index = Index::default();
        if let Some(profile_id) = settings.active_profile.as_deref() {
            match library.load_similarity_index(profile_id) {
                Ok(entries) => {
                    index.profile_id = Some(profile_id.to_string());
                    index.entries = entries;
                }
                Err(err) => tracing::warn!(%err, "similarity index restore failed"),
            }
        }
        let target_profile = model_by_id(&settings.model)
            .filter(|_| profile_by_id(&settings.profile).is_some())
            .map(|model| profile_fingerprint(model, &settings.profile));
        let restored_profile_is_current = index.profile_id == target_profile;
        let status = SimilarityStatus {
            phase: if !settings.enabled {
                Phase::Disabled
            } else if restored_profile_is_current {
                Phase::Ready
            } else {
                Phase::Loading
            },
            active_profile: index.profile_id.clone(),
            target_profile,
            model: settings.model.clone(),
            ..SimilarityStatus::default()
        };
        Arc::new(Self {
            library,
            event_tx,
            workers: AtomicUsize::new(settings.workers.clamp(1, 16)),
            generation: AtomicU64::new(0),
            pipeline_running: AtomicBool::new(false),
            rescan_requested: AtomicBool::new(false),
            settings: Mutex::new(settings),
            status: Mutex::new(status),
            index: RwLock::new(index),
            model: Mutex::new(None),
            model_dir,
        })
    }

    pub fn settings(&self) -> SimilaritySettings {
        lock(&self.settings).clone()
    }

    pub fn status(&self) -> SimilarityStatus {
        lock(&self.status).clone()
    }

    pub fn network_allowed(&self) -> bool {
        let settings = lock(&self.settings);
        settings.enabled && settings.federation_consent
    }

    pub fn apply(self: &Arc<Self>, settings: SimilaritySettings) {
        self.workers
            .store(settings.workers.clamp(1, 16), Ordering::Release);
        let previous = std::mem::replace(&mut *lock(&self.settings), settings.clone());
        if !settings.enabled {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.update_status(|status| {
                status.phase = Phase::Disabled;
                status.current_track = None;
                status.target_profile = None;
                status.last_error = None;
            });
            return;
        }
        if !previous.enabled
            || previous.model != settings.model
            || previous.profile != settings.profile
        {
            self.generation.fetch_add(1, Ordering::AcqRel);
            self.start();
        }
    }

    /// Requests a scan without cancelling useful work already in progress.
    /// Bursts of library-change notifications collapse into one follow-up
    /// pass, so metadata refreshes cannot repeatedly restart the model.
    pub fn start(self: &Arc<Self>) {
        self.rescan_requested.store(true, Ordering::Release);
        if self.pipeline_running.swap(true, Ordering::AcqRel) {
            return;
        }
        let this = Arc::clone(self);
        tokio::spawn(async move {
            loop {
                // This pass covers every notification received before it
                // starts. A notification during the pass requests one more.
                this.rescan_requested.store(false, Ordering::Release);
                let generation = this.generation.load(Ordering::Acquire);
                if let Err(err) = this.run_pipeline(generation).await
                    && this.generation.load(Ordering::Acquire) == generation
                {
                    tracing::error!(%err, "similarity pipeline failed");
                    this.update_status(|status| {
                        status.phase = Phase::Error;
                        status.current_track = None;
                        status.last_error = Some(format!("{err:#}"));
                    });
                }

                if this.rescan_requested.load(Ordering::Acquire) {
                    continue;
                }

                this.pipeline_running.store(false, Ordering::Release);
                // Close the small race between checking the request flag and
                // releasing ownership of the worker. If another worker has
                // already claimed it, that worker owns the pending pass.
                if this.rescan_requested.swap(false, Ordering::AcqRel)
                    && !this.pipeline_running.swap(true, Ordering::AcqRel)
                {
                    continue;
                }
                break;
            }
        });
    }

    pub fn clear(self: &Arc<Self>) {
        self.generation.fetch_add(1, Ordering::AcqRel);
        let this = Arc::clone(self);
        tokio::spawn(async move {
            let library = Arc::clone(&this.library);
            let result = tokio::task::spawn_blocking(move || library.clear_similarity_embeddings())
                .await
                .context("embedding clear task failed")
                .and_then(|result| result);
            match result {
                Ok(()) => {
                    *write(&this.index) = Index::default();
                    lock(&this.settings).active_profile = None;
                    this.update_status(|status| {
                        *status = SimilarityStatus {
                            phase: if this.settings().enabled {
                                Phase::Loading
                            } else {
                                Phase::Disabled
                            },
                            model: this.settings().model,
                            ..SimilarityStatus::default()
                        };
                    });
                    let _ = this
                        .event_tx
                        .send(AppEvent::SimilarityProfileActivated(None));
                    if this.settings().enabled {
                        this.start();
                    }
                }
                Err(err) => this.update_status(|status| {
                    status.phase = Phase::Error;
                    status.last_error = Some(format!("clear failed: {err:#}"));
                }),
            }
        });
    }

    pub fn query_for_track(&self, track_id: i64) -> Result<QueryVector> {
        let profile_id = read(&self.index)
            .profile_id
            .clone()
            .context("no similarity profile is ready yet")?;
        let vector = self
            .library
            .similarity_embedding(track_id, &profile_id)?
            .context("this track has not been processed yet")?;
        let source_track = self.library.tracks_by_ids(&[track_id])?.into_iter().next();
        let source_content_id = source_track.as_ref().and_then(|track| {
            track
                .content_id
                .clone()
                .or_else(|| crate::library::audio_content_id(&track.file_path))
        });
        Ok(QueryVector {
            profile_id,
            vector,
            source_content_id,
        })
    }

    pub fn search_track(
        &self,
        track_id: i64,
        limit: usize,
    ) -> Result<(Vec<SimilarTrack>, QueryVector)> {
        let query = self.query_for_track(track_id)?;
        let matches = self.search_vector(
            &query.profile_id,
            &query.vector,
            Some(track_id),
            query.source_content_id.as_deref(),
            limit,
        )?;
        Ok((matches, query))
    }

    pub fn search_vector(
        &self,
        profile_id: &str,
        vector: &[f32],
        exclude_track_id: Option<i64>,
        exclude_content_id: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SimilarTrack>> {
        let settings = lock(&self.settings);
        let minimum_score = settings.minimum_score;
        let max_tracks_per_artist = settings.max_tracks_per_artist;
        drop(settings);
        self.search_vector_with_policy(
            profile_id,
            vector,
            exclude_track_id,
            exclude_content_id,
            limit,
            minimum_score,
            max_tracks_per_artist,
        )
    }

    /// Returns a wider, policy-neutral candidate set to a remote requester.
    /// The requester applies its own score threshold and artist diversity
    /// limit; neither value is part of embedding compatibility.
    pub(crate) fn search_vector_for_peer(
        &self,
        profile_id: &str,
        vector: &[f32],
        limit: usize,
    ) -> Result<Vec<SimilarTrack>> {
        self.search_vector_with_policy(
            profile_id,
            vector,
            None,
            None,
            limit,
            -1.0,
            PEER_CANDIDATE_MAX_PER_ARTIST,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn search_vector_with_policy(
        &self,
        profile_id: &str,
        vector: &[f32],
        exclude_track_id: Option<i64>,
        exclude_content_id: Option<&str>,
        limit: usize,
        minimum_score: f32,
        max_tracks_per_artist: usize,
    ) -> Result<Vec<SimilarTrack>> {
        anyhow::ensure!(
            !vector.is_empty() && vector.len() <= 4096,
            "wrong embedding dimensions"
        );
        anyhow::ensure!(
            vector.iter().all(|value| value.is_finite()),
            "invalid embedding"
        );
        let index = read(&self.index);
        anyhow::ensure!(
            index.profile_id.as_deref() == Some(profile_id),
            "the requested similarity profile is not active"
        );
        let mut scores: Vec<(i64, f32, &str, &[f32])> = index
            .entries
            .iter()
            .filter(|entry| {
                Some(entry.track_id) != exclude_track_id && entry.vector.len() == vector.len()
            })
            .map(|entry| {
                (
                    entry.track_id,
                    dot(vector, &entry.vector),
                    entry.artist_key.as_str(),
                    entry.vector.as_slice(),
                )
            })
            .filter(|(_, score, _, _)| score.is_finite() && *score >= minimum_score)
            .collect();
        scores.sort_by(|left, right| right.1.total_cmp(&left.1));

        // Pull a wider candidate set, then cap each primary artist so a large
        // discography cannot fill the whole result page.
        let mut artist_counts: HashMap<String, usize> = HashMap::new();
        let mut kept_vectors = vec![vector];
        let mut selected = Vec::new();
        for (track_id, score, artist, candidate_vector) in scores {
            if is_near_duplicate(candidate_vector, &kept_vectors) {
                continue;
            }
            let count = artist_counts.entry(artist.to_string()).or_default();
            if !artist.is_empty() && *count >= max_tracks_per_artist {
                continue;
            }
            *count += 1;
            let embedding_signature = music_dht::similarity::embedding_signature(candidate_vector)?;
            kept_vectors.push(candidate_vector);
            selected.push((track_id, score, embedding_signature));
            if selected.len() >= limit.min(RESULT_LIMIT) {
                break;
            }
        }
        drop(index);

        let ids: Vec<i64> = selected.iter().map(|(id, _, _)| *id).collect();
        let tracks = self.library.tracks_by_ids(&ids)?;
        let by_id: HashMap<i64, TrackItem> =
            tracks.into_iter().map(|track| (track.id, track)).collect();
        Ok(selected
            .into_iter()
            .filter_map(|(id, score, signature)| {
                by_id
                    .get(&id)
                    .cloned()
                    .map(|track| (track, score, signature))
            })
            .filter(|(track, _, _)| {
                !exclude_content_id
                    .is_some_and(|source| track.content_id.as_deref() == Some(source))
            })
            .map(|(track, score, embedding_signature)| SimilarTrack {
                track,
                score,
                embedding_signature,
            })
            .collect())
    }

    async fn run_pipeline(self: &Arc<Self>, generation: u64) -> Result<()> {
        let settings = self.settings();
        if !settings.enabled {
            return Ok(());
        }
        let spec = model_by_id(&settings.model)
            .with_context(|| format!("unknown similarity model '{}'", settings.model))?;
        anyhow::ensure!(
            profile_by_id(&settings.profile).is_some(),
            "unknown preprocessing profile '{}'",
            settings.profile
        );
        let profile_id = profile_fingerprint(spec, &settings.profile);
        self.library.ensure_similarity_profile(
            &profile_id,
            spec.id,
            spec.version,
            spec.sha256,
            &settings.profile,
            spec.dimensions,
        )?;
        let stats = self.library.similarity_storage_stats(&profile_id)?;
        self.update_status(|status| {
            status.target_profile = Some(profile_id.clone());
            status.model = spec.id.to_string();
            status.total_tracks = stats.total_tracks;
            status.completed_tracks = stats.embedded_tracks;
            status.stored_vectors = stats.stored_vectors;
            status.stored_bytes = stats.stored_bytes;
            status.failed_tracks = 0;
            status.current_track = None;
            status.last_error = None;
        });

        let mut pending: VecDeque<_> = self.library.pending_similarity_tracks(&profile_id)?.into();
        if pending.is_empty() {
            self.ensure_generation(generation)?;
            return self.activate_profile(profile_id);
        }

        let model_path = self.ensure_model(spec, generation).await?;
        self.ensure_generation(generation)?;
        let model = self.load_model(&profile_id, &model_path).await?;
        self.ensure_generation(generation)?;
        self.update_status(|status| status.phase = Phase::Processing);

        let mut jobs = tokio::task::JoinSet::new();
        while !pending.is_empty() || !jobs.is_empty() {
            self.ensure_generation(generation)?;
            let workers = self.workers.load(Ordering::Acquire).clamp(1, 16);
            while jobs.len() < workers {
                let Some(track) = pending.pop_front() else {
                    break;
                };
                self.update_status(|status| status.current_track = Some(track.title.clone()));
                let library = Arc::clone(&self.library);
                let model = Arc::clone(&model);
                let profile_id = profile_id.clone();
                jobs.spawn_blocking(move || {
                    let started = Instant::now();
                    let result =
                        embed_track(&model, Path::new(&track.file_path), track.duration_seconds)
                            .and_then(|vector| {
                                library.store_similarity_embedding(&track, &profile_id, &vector)
                            });
                    (track, result, started.elapsed())
                });
            }
            let Some(result) = jobs.join_next().await else {
                continue;
            };
            let (track, result, elapsed) = result.context("embedding worker panicked")?;
            self.ensure_generation(generation)?;
            match result {
                Ok(()) => {
                    tracing::info!(
                        track_id = track.id,
                        title = %track.title,
                        elapsed_ms = elapsed.as_millis(),
                        profile = %profile_id,
                        "track embedding calculated"
                    );
                    self.update_status(|status| status.completed_tracks += 1);
                }
                Err(err) => {
                    tracing::warn!(track_id = track.id, title = %track.title, %err, "track embedding failed");
                    self.update_status(|status| {
                        status.failed_tracks += 1;
                        status.last_error = Some(format!("{}: {err:#}", track.title));
                    });
                }
            }
        }
        self.ensure_generation(generation)?;
        self.activate_profile(profile_id)
    }

    fn activate_profile(&self, profile_id: String) -> Result<()> {
        let entries = self.library.load_similarity_index(&profile_id)?;
        let total_tracks = self
            .library
            .similarity_storage_stats(&profile_id)?
            .total_tracks;
        anyhow::ensure!(
            total_tracks == 0 || !entries.is_empty(),
            "no tracks could be processed"
        );
        *write(&self.index) = Index {
            profile_id: Some(profile_id.clone()),
            entries,
        };
        let profile_changed = {
            let mut settings = lock(&self.settings);
            let changed = settings.active_profile.as_deref() != Some(&profile_id);
            settings.active_profile = Some(profile_id.clone());
            changed
        };
        let stats = self.library.similarity_storage_stats(&profile_id)?;
        self.update_status(|status| {
            status.phase = Phase::Ready;
            status.active_profile = Some(profile_id.clone());
            status.target_profile = Some(profile_id.clone());
            status.total_tracks = stats.total_tracks;
            status.completed_tracks = stats.embedded_tracks;
            status.stored_vectors = stats.stored_vectors;
            status.stored_bytes = stats.stored_bytes;
            status.current_track = None;
        });
        if profile_changed {
            let _ = self
                .event_tx
                .send(AppEvent::SimilarityProfileActivated(Some(profile_id)));
        }
        Ok(())
    }

    fn ensure_generation(&self, generation: u64) -> Result<()> {
        anyhow::ensure!(
            self.generation.load(Ordering::Acquire) == generation,
            "similarity processing superseded by newer settings"
        );
        Ok(())
    }

    async fn ensure_model(&self, spec: &ModelSpec, generation: u64) -> Result<PathBuf> {
        tokio::fs::create_dir_all(&self.model_dir).await?;
        let path = self.model_dir.join(spec.filename);
        if path.exists() {
            let verify_path = path.clone();
            let expected = spec.sha256.to_string();
            let valid = tokio::task::spawn_blocking(move || sha256_file(&verify_path))
                .await
                .context("model hash task failed")??
                == expected;
            if valid {
                return Ok(path);
            }
            tokio::fs::remove_file(&path).await?;
        }

        self.update_status(|status| status.phase = Phase::Downloading);
        let response = reqwest::get(spec.url).await?.error_for_status()?;
        let tmp = path.with_extension(format!("part-{}-{generation}", std::process::id()));
        let mut file = tokio::fs::File::create(&tmp).await?;
        let mut hasher = Sha256::new();
        let mut received = 0usize;
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk?;
            received = received.saturating_add(chunk.len());
            anyhow::ensure!(
                received <= MAX_MODEL_BYTES,
                "model download exceeds size limit"
            );
            hasher.update(&chunk);
            file.write_all(&chunk).await?;
        }
        file.flush().await?;
        drop(file);
        let actual = format!("{:x}", hasher.finalize());
        if actual != spec.sha256 {
            let _ = tokio::fs::remove_file(&tmp).await;
            anyhow::bail!("downloaded model hash mismatch");
        }
        if let Err(err) = tokio::fs::rename(&tmp, &path).await {
            // A superseding pipeline may have installed the same verified
            // artifact first. This is expected on platforms where rename
            // does not replace an existing destination.
            if path.exists() {
                let _ = tokio::fs::remove_file(&tmp).await;
            } else {
                return Err(err.into());
            }
        }
        Ok(path)
    }

    async fn load_model(&self, profile_id: &str, path: &Path) -> Result<RunnableModel> {
        if let Some((cached_profile, model)) = lock(&self.model).as_ref()
            && cached_profile == profile_id
        {
            return Ok(Arc::clone(model));
        }
        self.update_status(|status| status.phase = Phase::Loading);
        let path = path.to_path_buf();
        let model = tokio::task::spawn_blocking(move || load_onnx(&path))
            .await
            .context("model loading task failed")??;
        *lock(&self.model) = Some((profile_id.to_string(), Arc::clone(&model)));
        Ok(model)
    }

    fn update_status(&self, update: impl FnOnce(&mut SimilarityStatus)) {
        let snapshot = {
            let mut status = lock(&self.status);
            let previous = status.clone();
            update(&mut status);
            (*status != previous).then(|| status.clone())
        };
        if let Some(snapshot) = snapshot {
            let _ = self.event_tx.send(AppEvent::SimilarityStatus(snapshot));
        }
    }
}

pub fn model_by_id(id: &str) -> Option<&'static ModelSpec> {
    MODELS.iter().find(|model| model.id == id)
}

pub fn profile_by_id(id: &str) -> Option<&'static ProfileSpec> {
    PROFILES.iter().find(|profile| profile.id == id)
}

pub fn profile_details(profile_id: &str, model_id: &str) -> Option<String> {
    let profile = profile_by_id(profile_id)?;
    let dimensions = model_by_id(model_id)
        .map(|model| model.dimensions.to_string())
        .unwrap_or_else(|| "model-defined".to_string());
    Some(format!(
        "{}\n\nTrack selection:\n• Up to {} seconds: entire track.\n• Longer: {} × {}-second windows (start, middle, end).\n\nAudio: mono, {} Hz; 16-tap windowed-sinc resampling.\nSpectrogram: Hann window; FFT {}, hop {} samples (16 ms).\nMel: {} Slaney bands, 0–8 kHz, unit-triangle normalization.\nCompression: log10(1 + 10000 × energy).\nPatches: {} frames (~2.05 s), hop {} frames (~0.99 s).\nAggregation: mean of patch embeddings, then L2 normalization.\nOutput dimensions for selected model: {}.\n\nCompatibility includes the exact model version and SHA-256; peers compare only matching profiles.\n\nEnter / Esc: close",
        profile.title,
        FULL_TRACK_MAX_SECONDS,
        LONG_TRACK_WINDOWS,
        LONG_TRACK_WINDOW_SECONDS,
        SAMPLE_RATE,
        FRAME_SIZE,
        HOP_SIZE,
        MEL_BANDS,
        PATCH_FRAMES,
        PATCH_HOP,
        dimensions,
    ))
}

pub fn profile_fingerprint(model: &ModelSpec, profile: &str) -> String {
    let contract = format!(
        "furumi-similarity-v1\nmodel={}\nversion={}\nsha256={}\nprofile={}\ninput={}-mono-windowed-sinc16\nselection=full-to-{}s-else-first-middle-last-{}s\nframe={}\nhop={}\nmel=slaney-{}-unit-tri\npatch={}\npatch-hop={}\naggregate=mean-l2\ndimensions={}",
        model.id,
        model.version,
        model.sha256,
        profile,
        SAMPLE_RATE,
        FULL_TRACK_MAX_SECONDS,
        LONG_TRACK_WINDOW_SECONDS,
        FRAME_SIZE,
        HOP_SIZE,
        MEL_BANDS,
        PATCH_FRAMES,
        PATCH_HOP,
        model.dimensions
    );
    format!("sim1:{}", blake3::hash(contract.as_bytes()).to_hex())
}

fn load_onnx(path: &Path) -> Result<RunnableModel> {
    let model = tract_onnx::onnx().model_for_path(path)?;
    let batch = model.sym("batch_size");
    let model = model
        .with_input_fact(0, f32::fact(dims!(batch, PATCH_FRAMES, MEL_BANDS)).into())?
        .into_optimized()?
        .into_runnable()?;
    Ok(model)
}

fn embed_track(model: &RunnableModel, path: &Path, duration_seconds: f64) -> Result<Vec<f32>> {
    let signal = decode_mono_16k(path, duration_seconds)?;
    let mel = mel_spectrogram(&signal)?;
    anyhow::ensure!(
        mel.len() >= PATCH_FRAMES,
        "track is too short for the model"
    );
    let starts: Vec<usize> = (0..=mel.len() - PATCH_FRAMES).step_by(PATCH_HOP).collect();
    let mut sum = vec![0.0f32; EMBEDDING_DIMENSIONS];
    let mut count = 0usize;
    for batch in starts.chunks(MODEL_BATCH) {
        let mut input = vec![0.0f32; MODEL_BATCH * PATCH_FRAMES * MEL_BANDS];
        for (batch_index, &start) in batch.iter().enumerate() {
            let offset = batch_index * PATCH_FRAMES * MEL_BANDS;
            for frame in 0..PATCH_FRAMES {
                let dst = offset + frame * MEL_BANDS;
                input[dst..dst + MEL_BANDS].copy_from_slice(&mel[start + frame]);
            }
        }
        let tensor = Tensor::from_shape(&[MODEL_BATCH, PATCH_FRAMES, MEL_BANDS], &input)?;
        let outputs = model.run(tvec!(tensor.into_tvalue()))?;
        let embedding = outputs
            .iter()
            .find(|output| output.len() == MODEL_BATCH * EMBEDDING_DIMENSIONS)
            .context("model did not return its 1280-dimensional embedding output")?
            .to_plain_array_view::<f32>()?;
        let values = embedding
            .as_slice()
            .context("model embedding output is not contiguous")?;
        for batch_index in 0..batch.len() {
            let row = &values
                [batch_index * EMBEDDING_DIMENSIONS..(batch_index + 1) * EMBEDDING_DIMENSIONS];
            for (total, value) in sum.iter_mut().zip(row) {
                *total += *value;
            }
            count += 1;
        }
    }
    anyhow::ensure!(count > 0, "model produced no patches");
    for value in &mut sum {
        *value /= count as f32;
    }
    normalize(&mut sum)?;
    Ok(sum)
}

fn decode_mono_16k(path: &Path, duration_seconds: f64) -> Result<Vec<f32>> {
    if duration_seconds.is_finite() && duration_seconds > f64::from(FULL_TRACK_MAX_SECONDS) {
        let window = f64::from(LONG_TRACK_WINDOW_SECONDS);
        let starts = [
            0.0,
            (duration_seconds / 2.0 - window / 2.0).max(0.0),
            (duration_seconds - window).max(0.0),
        ];
        let mut selected = Vec::new();
        for start in starts {
            selected.extend(decode_mono_window(path, start, Some(window))?);
        }
        anyhow::ensure!(!selected.is_empty(), "decoded track is empty");
        return Ok(selected);
    }
    decode_mono_window(path, 0.0, None)
}

fn decode_mono_window(
    path: &Path,
    start_seconds: f64,
    length_seconds: Option<f64>,
) -> Result<Vec<f32>> {
    let file = File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let mut decoder =
        Decoder::try_from(file).with_context(|| format!("decoding {}", path.display()))?;
    let channels = decoder.channels().get() as usize;
    let source_rate = decoder.sample_rate().get() as usize;
    if start_seconds > 0.0 {
        decoder
            .try_seek(std::time::Duration::from_secs_f64(start_seconds))
            .with_context(|| format!("seeking {}", path.display()))?;
    }
    let max_samples =
        length_seconds.map(|seconds| (seconds * source_rate as f64).ceil() as usize * channels);
    let mut mono = Vec::new();
    let mut channel_sum = 0.0f32;
    let mut channel_index = 0usize;
    for (sample_index, sample) in decoder.enumerate() {
        if max_samples.is_some_and(|limit| sample_index >= limit) {
            break;
        }
        channel_sum += sample;
        channel_index += 1;
        if channel_index == channels {
            mono.push(channel_sum / channels as f32);
            channel_sum = 0.0;
            channel_index = 0;
        }
    }
    anyhow::ensure!(!mono.is_empty(), "decoded track is empty");
    if source_rate == SAMPLE_RATE {
        return Ok(mono);
    }
    Ok(resample_sinc(&mono, source_rate, SAMPLE_RATE))
}

fn resample_sinc(input: &[f32], source_rate: usize, target_rate: usize) -> Vec<f32> {
    if input.len() < 2 || source_rate == 0 {
        return input.to_vec();
    }
    let output_len = input
        .len()
        .saturating_mul(target_rate)
        .checked_div(source_rate)
        .unwrap_or(0)
        .max(1);
    let ratio = source_rate as f64 / target_rate as f64;
    let cutoff = (target_rate as f64 / source_rate as f64).min(1.0) * 0.95;
    const HALF_TAPS: isize = 8;
    (0..output_len)
        .map(|index| {
            let position = index as f64 * ratio;
            let center = position.floor() as isize;
            let mut value = 0.0f64;
            let mut weight_sum = 0.0f64;
            for sample_index in center - HALF_TAPS + 1..=center + HALF_TAPS {
                if sample_index < 0 || sample_index >= input.len() as isize {
                    continue;
                }
                let distance = position - sample_index as f64;
                let phase = std::f64::consts::PI * distance * cutoff;
                let sinc = if phase.abs() < 1e-12 {
                    1.0
                } else {
                    phase.sin() / phase
                };
                let window_position = distance / HALF_TAPS as f64;
                let window = if window_position.abs() <= 1.0 {
                    0.5 + 0.5 * (std::f64::consts::PI * window_position).cos()
                } else {
                    0.0
                };
                let weight = cutoff * sinc * window;
                value += input[sample_index as usize] as f64 * weight;
                weight_sum += weight;
            }
            if weight_sum.abs() < 1e-12 {
                input[center.clamp(0, input.len() as isize - 1) as usize]
            } else {
                (value / weight_sum) as f32
            }
        })
        .collect()
}

fn mel_spectrogram(signal: &[f32]) -> Result<Vec<[f32; MEL_BANDS]>> {
    let frame_count = 1 + signal
        .len()
        .saturating_sub(FRAME_SIZE / 2)
        .div_ceil(HOP_SIZE);
    let filters = mel_filters();
    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(FRAME_SIZE);
    let mut mel = Vec::with_capacity(frame_count);
    let mut spectrum = vec![Complex::new(0.0f32, 0.0); FRAME_SIZE];
    for frame_index in 0..frame_count {
        let start = frame_index as isize * HOP_SIZE as isize - (FRAME_SIZE / 2) as isize;
        for (index, value) in spectrum.iter_mut().enumerate() {
            let source = start + index as isize;
            let sample = if source >= 0 {
                signal.get(source as usize).copied().unwrap_or(0.0)
            } else {
                0.0
            };
            let window = 0.5
                - 0.5 * (2.0 * std::f32::consts::PI * index as f32 / (FRAME_SIZE - 1) as f32).cos();
            *value = Complex::new(sample * window, 0.0);
        }
        fft.process(&mut spectrum);
        let powers: Vec<f32> = spectrum[..=FRAME_SIZE / 2]
            .iter()
            .map(|value| value.norm_sqr())
            .collect();
        let mut bands = [0.0f32; MEL_BANDS];
        for (band, weights) in filters.iter().enumerate() {
            let energy: f32 = powers
                .iter()
                .zip(weights)
                .map(|(power, weight)| power * weight)
                .sum();
            bands[band] = (1.0 + 10_000.0 * energy.max(0.0)).log10();
        }
        mel.push(bands);
    }
    Ok(mel)
}

fn mel_filters() -> Vec<Vec<f32>> {
    let low = hz_to_mel_slaney(0.0);
    let high = hz_to_mel_slaney((SAMPLE_RATE / 2) as f32);
    let points: Vec<f32> = (0..MEL_BANDS + 2)
        .map(|index| mel_to_hz_slaney(low + (high - low) * index as f32 / (MEL_BANDS + 1) as f32))
        .collect();
    let frequency_scale = (SAMPLE_RATE as f32 / 2.0) / (FRAME_SIZE / 2) as f32;
    (0..MEL_BANDS)
        .map(|band| {
            let left = points[band];
            let center = points[band + 1];
            let right = points[band + 2];
            let area = ((center - left) + (right - center)) / 2.0;
            (0..=FRAME_SIZE / 2)
                .map(|bin| {
                    let frequency = bin as f32 * frequency_scale;
                    let triangle = if frequency < left || frequency > right {
                        0.0
                    } else if frequency < center {
                        (frequency - left) / (center - left)
                    } else {
                        (right - frequency) / (right - center)
                    };
                    triangle.max(0.0) / area
                })
                .collect()
        })
        .collect()
}

fn hz_to_mel_slaney(hz: f32) -> f32 {
    if hz < 1000.0 {
        hz / (200.0 / 3.0)
    } else {
        15.0 + 27.0 * (hz / 1000.0).ln() / 6.4f32.ln()
    }
}

fn mel_to_hz_slaney(mel: f32) -> f32 {
    if mel < 15.0 {
        mel * (200.0 / 3.0)
    } else {
        1000.0 * (6.4f32.ln() * (mel - 15.0) / 27.0).exp()
    }
}

fn normalize(vector: &mut [f32]) -> Result<()> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    anyhow::ensure!(
        norm.is_finite() && norm > f32::EPSILON,
        "zero or invalid embedding"
    );
    for value in vector {
        *value /= norm;
    }
    Ok(())
}

fn dot(left: &[f32], right: &[f32]) -> f32 {
    left.iter().zip(right).map(|(a, b)| a * b).sum()
}

fn is_near_duplicate(candidate: &[f32], kept: &[&[f32]]) -> bool {
    kept.iter()
        .any(|existing| dot(candidate, existing) >= NEAR_DUPLICATE_COSINE)
}

fn sha256_file(path: &Path) -> Result<String> {
    use std::io::Read as _;

    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(format!("{:x}", hasher.finalize()))
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn read<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn write<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_test_dir(label: &str) -> PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("furumi-{label}-{}-{unique}", std::process::id()))
    }

    #[test]
    fn profile_fingerprint_changes_with_contract() {
        let model = &MODELS[0];
        let first = profile_fingerprint(model, DEFAULT_PROFILE_ID);
        let second = profile_fingerprint(model, "another-profile");
        assert_ne!(first, second);
        assert_eq!(first, profile_fingerprint(model, DEFAULT_PROFILE_ID));
    }

    #[test]
    fn profile_details_describe_the_processing_contract() {
        let details = profile_details(DEFAULT_PROFILE_ID, DEFAULT_MODEL_ID).unwrap();
        assert!(details.contains("Up to 300 seconds"));
        assert!(details.contains("16000 Hz"));
        assert!(details.contains("1280"));
    }

    #[test]
    fn vectors_are_normalized() {
        let mut vector = vec![3.0, 4.0];
        normalize(&mut vector).unwrap();
        assert!((dot(&vector, &vector) - 1.0).abs() < 1e-6);
    }

    #[test]
    fn near_duplicate_embeddings_are_filtered_but_distinct_tracks_remain() {
        let query = [1.0, 0.0, 0.0];
        let near_duplicate = [0.99995, 0.01, 0.0];
        let distinct = [0.0, 1.0, 0.0];
        assert!(is_near_duplicate(&near_duplicate, &[&query]));
        assert!(!is_near_duplicate(&distinct, &[&query]));
    }

    #[tokio::test]
    async fn repeated_rescans_keep_an_up_to_date_profile_ready() {
        let directory = unique_test_dir("similarity-stable-status");
        let library = Arc::new(Library::open(&directory.join("library.db")).unwrap());
        let profile_id = profile_fingerprint(&MODELS[0], DEFAULT_PROFILE_ID);
        let settings = SimilaritySettings {
            enabled: true,
            active_profile: Some(profile_id),
            ..SimilaritySettings::default()
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let manager = Manager::new(Arc::clone(&library), event_tx, settings);

        assert_eq!(manager.status().phase, Phase::Ready);
        for _ in 0..32 {
            manager.start();
        }
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while manager.pipeline_running.load(Ordering::Acquire) {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert_eq!(manager.status().phase, Phase::Ready);
        while let Ok(event) = event_rx.try_recv() {
            if let AppEvent::SimilarityStatus(status) = event {
                assert_eq!(status.phase, Phase::Ready);
            }
        }

        drop(manager);
        drop(library);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn resampling_keeps_a_constant_signal() {
        let output = resample_sinc(&vec![0.25; 441], 44_100, 16_000);
        assert_eq!(output.len(), 160);
        assert!(output.iter().all(|value| (*value - 0.25).abs() < 1e-6));
    }

    /// Manual compatibility check for the separately downloaded model:
    /// `FURUMI_TEST_MODEL=/path/model.onnx cargo test onnx_model_smoke -- --ignored`.
    #[test]
    #[ignore]
    fn onnx_model_smoke() {
        let path = std::env::var_os("FURUMI_TEST_MODEL")
            .map(PathBuf::from)
            .expect("set FURUMI_TEST_MODEL");
        let model = load_onnx(&path).unwrap();
        let mut input = vec![0.0f32; MODEL_BATCH * PATCH_FRAMES * MEL_BANDS];
        input[0] = 1.0;
        let tensor = Tensor::from_shape(&[MODEL_BATCH, PATCH_FRAMES, MEL_BANDS], &input).unwrap();
        let outputs = model.run(tvec!(tensor.clone().into_tvalue())).unwrap();
        assert!(
            outputs
                .iter()
                .any(|output| output.len() == MODEL_BATCH * EMBEDDING_DIMENSIONS)
        );
        let started = Instant::now();
        for _ in 0..5 {
            model.run(tvec!(tensor.clone().into_tvalue())).unwrap();
        }
        eprintln!(
            "five warm batch-{MODEL_BATCH} runs: {:.3}s",
            started.elapsed().as_secs_f64()
        );
    }
}
