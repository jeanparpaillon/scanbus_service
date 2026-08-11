use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use scanbus_core::{ProfileKind, ProfileProcessor, ProfileResult, RawPage, Value};
use tokio::sync::Mutex;

const IMAGE_FORMAT: &str = "format";
const IMAGE_QUALITY: &str = "quality";
const DOCUMENT_FORMAT: &str = "format";
const DOCUMENT_MULTI_PAGE: &str = "multi_page";
const OUTPUT_FOLDER: &str = "output_folder";

/// Profile defaults, persistence and processors.
pub struct ProfileRegistry {
    store_path: Option<PathBuf>,
    options: Mutex<BTreeMap<ProfileKind, BTreeMap<String, Value>>>,
    image: Arc<ImageProcessor>,
    document: Arc<DocumentProcessor>,
}

impl ProfileRegistry {
    /// Real daemon registry with on-disk persistence.
    pub fn new() -> Self {
        Self::with_store_path(default_store_path())
    }

    /// Test registry that never writes to disk.
    pub fn ephemeral() -> Self {
        Self {
            store_path: None,
            options: Mutex::new(default_options()),
            image: Arc::new(ImageProcessor),
            document: Arc::new(DocumentProcessor),
        }
    }

    pub fn with_store_path(store_path: PathBuf) -> Self {
        let loaded = load_options(&store_path).unwrap_or_else(|_| default_options());

        Self {
            store_path: Some(store_path),
            options: Mutex::new(loaded),
            image: Arc::new(ImageProcessor),
            document: Arc::new(DocumentProcessor),
        }
    }

    /// Profiles currently registered as `Profile1` objects.
    pub fn registered_profiles(&self) -> Vec<ProfileKind> {
        vec![ProfileKind::Image, ProfileKind::Document]
    }

    pub async fn profile_types(&self) -> Vec<String> {
        self.registered_profiles()
            .into_iter()
            .map(|kind| kind.as_str().to_owned())
            .collect()
    }

    pub async fn options_for(&self, kind: ProfileKind) -> Option<BTreeMap<String, Value>> {
        self.options.lock().await.get(&kind).cloned()
    }

    pub async fn set_options(
        &self,
        kind: ProfileKind,
        options: BTreeMap<String, Value>,
    ) -> Result<(), String> {
        validate_options(kind, &options)?;

        {
            let mut state = self.options.lock().await;
            state.insert(kind, options);
            if let Some(path) = &self.store_path {
                persist_options(path, &state)?;
            }
        }

        Ok(())
    }

    pub async fn resolve_options(
        &self,
        profile: Option<ProfileKind>,
        button_options: BTreeMap<String, Value>,
    ) -> BTreeMap<String, Value> {
        let Some(profile) = profile else {
            return BTreeMap::new();
        };

        let mut merged = self
            .options
            .lock()
            .await
            .get(&profile)
            .cloned()
            .unwrap_or_default();

        for (key, value) in button_options {
            merged.insert(key, value);
        }

        merged
    }

    pub async fn process(
        &self,
        profile: Option<ProfileKind>,
        pages: BoxStream<'static, RawPage>,
        options: &BTreeMap<String, Value>,
    ) -> Result<BTreeMap<String, Value>, String> {
        let Some(profile) = profile else {
            return Ok(BTreeMap::new());
        };

        let result = match profile {
            ProfileKind::Image => self.image.process(pages, options).await?,
            ProfileKind::Document => self.document.process(pages, options).await?,
            ProfileKind::Email | ProfileKind::Ocr => {
                return Err(format!(
                    "profile {profile:?} is not implemented yet; supported profiles are image and document"
                ));
            }
        };

        Ok(result.to_job_result())
    }
}

impl Default for ProfileRegistry {
    fn default() -> Self {
        Self::new()
    }
}

struct ImageProcessor;

#[async_trait::async_trait]
impl ProfileProcessor for ImageProcessor {
    async fn process(
        &self,
        mut pages: BoxStream<'static, RawPage>,
        options: &BTreeMap<String, Value>,
    ) -> Result<ProfileResult, String> {
        let format = image_format(options)?;
        let output_dir = output_dir(options, ProfileKind::Image)?;
        ensure_output_dir(&output_dir)?;

        let stamp = scan_stamp();
        let mut written = Vec::new();

        while let Some(page) = pages.next().await {
            let name = format!("scan-{stamp}-p{:03}.{format}", page.index + 1);
            let path = unique_path(output_dir.join(name));
            fs::write(&path, &page.data)
                .map_err(|error| format!("cannot write image page to {}: {error}", path.display()))?;
            written.push(path.to_string_lossy().to_string());
        }

        Ok(ProfileResult::Image { paths: written })
    }
}

struct DocumentProcessor;

#[async_trait::async_trait]
impl ProfileProcessor for DocumentProcessor {
    async fn process(
        &self,
        mut pages: BoxStream<'static, RawPage>,
        options: &BTreeMap<String, Value>,
    ) -> Result<ProfileResult, String> {
        if !document_is_pdf(options) {
            return Err("document format must be \"pdf\"".to_owned());
        }

        let multi_page = document_multi_page(options)?;
        let output_dir = output_dir(options, ProfileKind::Document)?;
        ensure_output_dir(&output_dir)?;

        let mut buffers = Vec::new();
        while let Some(page) = pages.next().await {
            buffers.push(page.data);
            if !multi_page {
                break;
            }
        }

        if buffers.is_empty() {
            return Err("the scan delivered no pages; nothing to write".to_owned());
        }

        let stamp = scan_stamp();
        let path = unique_path(output_dir.join(format!("scan-{stamp}.pdf")));

        // Minimal placeholder payload: enough for a file the client can consume by path.
        let mut payload = b"%PDF-1.4\n% scanbus\n".to_vec();
        for chunk in buffers {
            payload.extend_from_slice(&chunk);
            payload.push(b'\n');
        }
        fs::write(&path, payload)
            .map_err(|error| format!("cannot write document to {}: {error}", path.display()))?;

        Ok(ProfileResult::Document {
            path: path.to_string_lossy().to_string(),
        })
    }
}

fn validate_options(kind: ProfileKind, options: &BTreeMap<String, Value>) -> Result<(), String> {
    match kind {
        ProfileKind::Image => {
            for (key, value) in options {
                match key.as_str() {
                    IMAGE_FORMAT => {
                        let Value::Str(format) = value else {
                            return Err("image option format must be a string".to_owned());
                        };
                        if !matches!(format.as_str(), "jpeg" | "jpg" | "png") {
                            return Err(format!(
                                "image format must be one of jpeg/jpg/png, got {format:?}"
                            ));
                        }
                    }
                    IMAGE_QUALITY => {
                        let quality = as_u64(value)
                            .ok_or_else(|| "image option quality must be an integer".to_owned())?;
                        if !(1..=100).contains(&quality) {
                            return Err(format!("image quality must be in 1..=100, got {quality}"));
                        }
                    }
                    OUTPUT_FOLDER => {
                        if !matches!(value, Value::Str(_)) {
                            return Err("image option output_folder must be a string".to_owned());
                        }
                    }
                    other => return Err(format!("unknown image option {other:?}")),
                }
            }
        }
        ProfileKind::Document => {
            for (key, value) in options {
                match key.as_str() {
                    DOCUMENT_FORMAT => {
                        let Value::Str(format) = value else {
                            return Err("document option format must be a string".to_owned());
                        };
                        if format != "pdf" {
                            return Err(format!("document format must be \"pdf\", got {format:?}"));
                        }
                    }
                    DOCUMENT_MULTI_PAGE => {
                        if !matches!(value, Value::Bool(_)) {
                            return Err("document option multi_page must be a boolean".to_owned());
                        }
                    }
                    OUTPUT_FOLDER => {
                        if !matches!(value, Value::Str(_)) {
                            return Err("document option output_folder must be a string".to_owned());
                        }
                    }
                    other => return Err(format!("unknown document option {other:?}")),
                }
            }
        }
        ProfileKind::Email | ProfileKind::Ocr => {
            return Err(format!("profile {} is not implemented", kind.as_str()));
        }
    }

    Ok(())
}

fn image_format(options: &BTreeMap<String, Value>) -> Result<&'static str, String> {
    let Some(value) = options.get(IMAGE_FORMAT) else {
        return Ok("jpeg");
    };

    let Value::Str(format) = value else {
        return Err("image option format must be a string".to_owned());
    };

    match format.as_str() {
        "jpeg" | "jpg" => Ok("jpg"),
        "png" => Ok("png"),
        other => Err(format!("image format must be one of jpeg/jpg/png, got {other:?}")),
    }
}

fn document_is_pdf(options: &BTreeMap<String, Value>) -> bool {
    match options.get(DOCUMENT_FORMAT) {
        Some(Value::Str(format)) => format == "pdf",
        Some(_) => false,
        None => true,
    }
}

fn document_multi_page(options: &BTreeMap<String, Value>) -> Result<bool, String> {
    match options.get(DOCUMENT_MULTI_PAGE) {
        Some(Value::Bool(value)) => Ok(*value),
        Some(_) => Err("document option multi_page must be a boolean".to_owned()),
        None => Ok(true),
    }
}

fn output_dir(options: &BTreeMap<String, Value>, kind: ProfileKind) -> Result<PathBuf, String> {
    if let Some(Value::Str(path)) = options.get(OUTPUT_FOLDER)
        && !path.trim().is_empty()
    {
        return Ok(PathBuf::from(path));
    }

    let root = default_output_root(kind)?;
    let profile_name = kind.as_str();
    Ok(root.join("scanbus").join(profile_name))
}

fn ensure_output_dir(path: &Path) -> Result<(), String> {
    fs::create_dir_all(path)
        .map_err(|error| format!("cannot create output directory {}: {error}", path.display()))
}

fn unique_path(path: PathBuf) -> PathBuf {
    if !path.exists() {
        return path;
    }

    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "scan".to_owned());
    let ext = path.extension().map(|e| e.to_string_lossy().to_string());

    for attempt in 1..=u32::MAX {
        let candidate_name = match &ext {
            Some(ext) => format!("{stem}-{attempt}.{ext}"),
            None => format!("{stem}-{attempt}"),
        };
        let candidate = path.with_file_name(candidate_name);
        if !candidate.exists() {
            return candidate;
        }
    }

    path
}

fn scan_stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn default_store_path() -> PathBuf {
    if let Some(path) = std::env::var_os("XDG_STATE_HOME") {
        return PathBuf::from(path).join("scanbus").join("profiles.json");
    }

    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".local")
        .join("state")
        .join("scanbus")
        .join("profiles.json")
}

fn load_options(path: &Path) -> Result<BTreeMap<ProfileKind, BTreeMap<String, Value>>, String> {
    let bytes = fs::read(path)
        .map_err(|error| format!("cannot read profile store {}: {error}", path.display()))?;
    let loaded: BTreeMap<ProfileKind, BTreeMap<String, Value>> = serde_json::from_slice(&bytes)
        .map_err(|error| format!("cannot parse profile store {}: {error}", path.display()))?;

    // Keep defaults for missing profiles and merge saved overrides over them.
    let mut merged = default_options();
    for (kind, options) in loaded {
        merged.insert(kind, options);
    }
    Ok(merged)
}

fn persist_options(
    path: &Path,
    options: &BTreeMap<ProfileKind, BTreeMap<String, Value>>,
) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create profile store directory {}: {error}", parent.display()))?;
    }

    let tmp = path.with_extension("json.tmp");
    let payload = serde_json::to_vec_pretty(options)
        .map_err(|error| format!("cannot serialize profile options: {error}"))?;
    fs::write(&tmp, payload)
        .map_err(|error| format!("cannot write profile store {}: {error}", tmp.display()))?;
    fs::rename(&tmp, path)
        .map_err(|error| format!("cannot replace profile store {}: {error}", path.display()))
}

fn default_options() -> BTreeMap<ProfileKind, BTreeMap<String, Value>> {
    BTreeMap::from([
        (
            ProfileKind::Image,
            BTreeMap::from([
                (IMAGE_FORMAT.to_owned(), Value::Str("jpeg".to_owned())),
                (IMAGE_QUALITY.to_owned(), Value::U64(90)),
            ]),
        ),
        (
            ProfileKind::Document,
            BTreeMap::from([
                (DOCUMENT_FORMAT.to_owned(), Value::Str("pdf".to_owned())),
                (DOCUMENT_MULTI_PAGE.to_owned(), Value::Bool(true)),
            ]),
        ),
    ])
}

fn default_output_root(kind: ProfileKind) -> Result<PathBuf, String> {
    let key = match kind {
        ProfileKind::Image => "XDG_PICTURES_DIR",
        ProfileKind::Document => "XDG_DOCUMENTS_DIR",
        ProfileKind::Email | ProfileKind::Ocr => {
            return Err(format!("profile {} is not implemented", kind.as_str()));
        }
    };

    if let Some(path) = xdg_dir_from_file(key) {
        return Ok(path);
    }

    home_dir().ok_or_else(|| "HOME is not set; cannot resolve output directory".to_owned())
}

fn xdg_dir_from_file(key: &str) -> Option<PathBuf> {
    let config_home = std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| home_dir().map(|home| home.join(".config")))?;

    let path = config_home.join("user-dirs.dirs");
    let text = fs::read_to_string(path).ok()?;

    for line in text.lines() {
        let line = line.trim();
        if !line.starts_with(key) {
            continue;
        }
        let (_, value) = line.split_once('=')?;
        let value = value.trim().trim_matches('"');

        if let Some(home) = home_dir() {
            let expanded = value.replace("$HOME", &home.to_string_lossy());
            return Some(PathBuf::from(expanded));
        }
    }

    None
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

fn as_u64(value: &Value) -> Option<u64> {
    match value {
        Value::U64(v) => Some(*v),
        Value::I64(v) if *v >= 0 => Some(*v as u64),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_have_the_two_registered_profiles() {
        let defaults = default_options();
        assert!(defaults.contains_key(&ProfileKind::Image));
        assert!(defaults.contains_key(&ProfileKind::Document));
    }

    #[test]
    fn collision_policy_never_overwrites() {
        let dir = std::env::temp_dir().join(format!(
            "scanbus-profile-collision-{}-{}",
            std::process::id(),
            scan_stamp()
        ));
        fs::create_dir_all(&dir).unwrap();

        let first = dir.join("scan.pdf");
        fs::write(&first, b"a").unwrap();
        let second = unique_path(first.clone());
        assert_ne!(second, first);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn button_options_override_profile_defaults_key_by_key() {
        let profiles = ProfileRegistry::ephemeral();
        let resolved = profiles
            .resolve_options(
                Some(ProfileKind::Image),
                BTreeMap::from([(IMAGE_FORMAT.to_owned(), Value::Str("png".to_owned()))]),
            )
            .await;

        assert_eq!(resolved.get(IMAGE_FORMAT), Some(&Value::Str("png".to_owned())));
        assert_eq!(resolved.get(IMAGE_QUALITY), Some(&Value::U64(90)));
    }
}
