use std::collections::BTreeMap;
use std::fs;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Once;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt as _;
use futures_util::stream::BoxStream;
use image::ColorType;
use image::ImageDecoder as _;
use image::ImageFormat;
use image::codecs::jpeg::JpegDecoder;
use image::codecs::jpeg::JpegEncoder;
use lopdf::content::{Content, Operation};
use lopdf::{Document as LoDocument, Object, Stream, dictionary};
use scanbus_core::{PageFormat, ProfileKind, ProfileProcessor, ProfileResult, RawPage, Value};
use tempfile::Builder as TempDirBuilder;
use tokio::sync::Mutex;

const IMAGE_FORMAT: &str = "format";
const IMAGE_QUALITY: &str = "quality";
const DOCUMENT_FORMAT: &str = "format";
const DOCUMENT_MULTI_PAGE: &str = "multi_page";
const OUTPUT_FOLDER: &str = "output_folder";
const DOCUMENT_SPOOL_DIR: &str = "scanbus-document-spool";

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
            document: Arc::new(DocumentProcessor::new()),
        }
    }

    pub fn with_store_path(store_path: PathBuf) -> Self {
        let loaded = load_options(&store_path).unwrap_or_else(|_| default_options());

        Self {
            store_path: Some(store_path),
            options: Mutex::new(loaded),
            image: Arc::new(ImageProcessor),
            document: Arc::new(DocumentProcessor::new()),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OutputFormat {
    Jpeg,
    Png,
}

impl OutputFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Jpeg => "jpg",
            Self::Png => "png",
        }
    }
}

#[async_trait::async_trait]
impl ProfileProcessor for ImageProcessor {
    async fn process(
        &self,
        mut pages: BoxStream<'static, RawPage>,
        options: &BTreeMap<String, Value>,
    ) -> Result<ProfileResult, String> {
        let format = image_format(options)?;
        let quality = image_quality(options)?;
        let output_dir = output_dir(options, ProfileKind::Image)?;
        ensure_output_dir(&output_dir)?;

        let stamp = scan_stamp();
        let mut written = Vec::new();

        while let Some(page) = pages.next().await {
            let name = format!("scan-{stamp}-p{:03}.{}", page.index + 1, format.extension());
            let path = unique_path(output_dir.join(name));
            let encoded = encode_for_output(&page, format, quality)?;
            fs::write(&path, &encoded).map_err(|error| {
                format!("cannot write image page to {}: {error}", path.display())
            })?;
            written.push(path.to_string_lossy().to_string());
        }

        Ok(ProfileResult::Image { paths: written })
    }
}

fn encode_for_output(page: &RawPage, output: OutputFormat, quality: u8) -> Result<Vec<u8>, String> {
    if matches!(
        (output, page.format),
        (OutputFormat::Jpeg, PageFormat::Jpeg)
    ) || matches!((output, page.format), (OutputFormat::Png, PageFormat::Png))
    {
        return Ok(page.data.clone());
    }

    let decoded = decode_page(page)?;

    match output {
        OutputFormat::Jpeg => {
            let mut out = Vec::new();
            let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
            encoder.encode_image(&decoded).map_err(|error| {
                format!("cannot encode page {} to JPEG: {error}", page.index + 1)
            })?;
            Ok(out)
        }
        OutputFormat::Png => {
            let mut cursor = Cursor::new(Vec::new());
            decoded
                .write_to(&mut cursor, ImageFormat::Png)
                .map_err(|error| {
                    format!("cannot encode page {} to PNG: {error}", page.index + 1)
                })?;
            Ok(cursor.into_inner())
        }
    }
}

fn decode_page(page: &RawPage) -> Result<image::DynamicImage, String> {
    image::load_from_memory_with_format(&page.data, image_format_from_page(page.format)).map_err(
        |error| {
            format!(
                "cannot decode page {} with input format {}: {error}",
                page.index + 1,
                page.format
            )
        },
    )
}

const fn image_format_from_page(format: PageFormat) -> ImageFormat {
    match format {
        PageFormat::Pnm => ImageFormat::Pnm,
        PageFormat::Jpeg => ImageFormat::Jpeg,
        PageFormat::Png => ImageFormat::Png,
        PageFormat::Tiff => ImageFormat::Tiff,
    }
}

struct DocumentProcessor {
    spool_root: PathBuf,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PdfColorSpace {
    DeviceGray,
    DeviceRgb,
}

impl PdfColorSpace {
    fn as_pdf_name(self) -> &'static str {
        match self {
            Self::DeviceGray => "DeviceGray",
            Self::DeviceRgb => "DeviceRGB",
        }
    }
}

#[derive(Debug)]
struct SpoolPage {
    path: PathBuf,
    width_px: u32,
    height_px: u32,
    dpi: u32,
    color_space: PdfColorSpace,
}

impl DocumentProcessor {
    fn new() -> Self {
        let spool_root = std::env::temp_dir().join(DOCUMENT_SPOOL_DIR);
        static SWEEP_ON_START: Once = Once::new();
        SWEEP_ON_START.call_once(|| {
            let _ = sweep_spool_root(&spool_root);
        });
        Self { spool_root }
    }
}

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

        fs::create_dir_all(&self.spool_root).map_err(|error| {
            format!(
                "cannot create document spool root {}: {error}",
                self.spool_root.display()
            )
        })?;
        let spool = TempDirBuilder::new()
            .prefix("job-")
            .tempdir_in(&self.spool_root)
            .map_err(|error| {
                format!(
                    "cannot create per-job spool directory in {}: {error}",
                    self.spool_root.display()
                )
            })?;

        let mut spooled = Vec::new();
        while let Some(page) = pages.next().await {
            let entry = spool_page_to_jpeg(spool.path(), &page)?;
            spooled.push(entry);
        }

        if spooled.is_empty() {
            return Err("the scan delivered no pages; nothing to write".to_owned());
        }

        let stamp = scan_stamp();
        let mut out_paths = Vec::new();

        if multi_page {
            let path = unique_path(output_dir.join(format!("scan-{stamp}.pdf")));
            write_pdf_document(&spooled, &path)?;
            out_paths.push(path.to_string_lossy().to_string());
        } else {
            for (index, page) in spooled.iter().enumerate() {
                let path =
                    unique_path(output_dir.join(format!("scan-{stamp}-p{:03}.pdf", index + 1)));
                write_pdf_document(std::slice::from_ref(page), &path)?;
                out_paths.push(path.to_string_lossy().to_string());
            }
        }

        Ok(ProfileResult::Document { paths: out_paths })
    }
}

fn write_pdf_document(pages: &[SpoolPage], output_path: &Path) -> Result<(), String> {
    let mut doc = LoDocument::with_version("1.5");
    let pages_id = doc.new_object_id();
    let mut kids = Vec::with_capacity(pages.len());

    for (index, page) in pages.iter().enumerate() {
        let image_name = format!("Im{}", index + 1);
        let image_object_name = Object::Name(image_name.as_bytes().to_vec());
        let image_bytes = fs::read(&page.path).map_err(|error| {
            format!("cannot read spooled page {}: {error}", page.path.display())
        })?;

        let image_stream = Stream::new(
            dictionary! {
                "Type" => "XObject",
                "Subtype" => "Image",
                "Width" => i64::from(page.width_px),
                "Height" => i64::from(page.height_px),
                "ColorSpace" => page.color_space.as_pdf_name(),
                "BitsPerComponent" => 8,
                "Filter" => "DCTDecode",
            },
            image_bytes,
        );
        let image_id = doc.add_object(image_stream);

        let width_pt = pixels_to_points(page.width_px, page.dpi)?;
        let height_pt = pixels_to_points(page.height_px, page.dpi)?;
        let content = Content {
            operations: vec![
                Operation::new("q", vec![]),
                Operation::new(
                    "cm",
                    vec![
                        width_pt.into(),
                        0.into(),
                        0.into(),
                        height_pt.into(),
                        0.into(),
                        0.into(),
                    ],
                ),
                Operation::new("Do", vec![image_object_name]),
                Operation::new("Q", vec![]),
            ],
        };
        let content_stream = Stream::new(
            dictionary! {},
            content.encode().map_err(|error| {
                format!(
                    "cannot encode PDF content stream for page {}: {error}",
                    index + 1
                )
            })?,
        );
        let content_id = doc.add_object(content_stream);

        let resources = dictionary! {
            "XObject" => dictionary! {
                image_name.as_str() => Object::Reference(image_id),
            }
        };

        let page_dict = dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages_id),
            "MediaBox" => vec![0.into(), 0.into(), width_pt.into(), height_pt.into()],
            "Resources" => resources,
            "Contents" => Object::Reference(content_id),
        };

        let page_id = doc.add_object(page_dict);
        kids.push(Object::Reference(page_id));
    }

    doc.objects.insert(
        pages_id,
        Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(kids),
            "Count" => i64::try_from(pages.len()).unwrap_or(i64::MAX),
        }),
    );

    let catalog_id = doc.add_object(dictionary! {
        "Type" => "Catalog",
        "Pages" => Object::Reference(pages_id),
    });
    doc.trailer.set("Root", Object::Reference(catalog_id));

    doc.save(output_path).map_err(|error| {
        format!(
            "cannot write document to {}: {error}",
            output_path.display()
        )
    })?;

    Ok(())
}

fn spool_page_to_jpeg(spool_dir: &Path, page: &RawPage) -> Result<SpoolPage, String> {
    let path = spool_dir.join(format!("p{:06}.jpg", page.index + 1));
    let (jpeg, width_px, height_px, color_space) = page_to_pdf_jpeg(page)?;

    fs::write(&path, jpeg)
        .map_err(|error| format!("cannot write spooled page {}: {error}", path.display()))?;

    Ok(SpoolPage {
        path,
        width_px,
        height_px,
        dpi: page.resolution_dpi,
        color_space,
    })
}

fn page_to_pdf_jpeg(page: &RawPage) -> Result<(Vec<u8>, u32, u32, PdfColorSpace), String> {
    if page.resolution_dpi == 0 {
        return Err(format!(
            "page {} has invalid resolution 0 dpi",
            page.index + 1
        ));
    }

    if page.format == PageFormat::Jpeg {
        let cursor = Cursor::new(page.data.as_slice());
        let decoder = JpegDecoder::new(cursor)
            .map_err(|error| format!("cannot parse JPEG page {}: {error}", page.index + 1))?;
        let (width_px, height_px) = decoder.dimensions();
        let color_space = pdf_color_space_for_jpeg(decoder.color_type()).ok_or_else(|| {
            format!(
                "unsupported JPEG color model on page {}: {:?}",
                page.index + 1,
                decoder.color_type()
            )
        })?;
        return Ok((page.data.clone(), width_px, height_px, color_space));
    }

    let decoded = decode_page(page)?;
    let width_px = decoded.width();
    let height_px = decoded.height();
    let rgb = decoded.to_rgb8();

    let mut out = Vec::new();
    let mut encoder = JpegEncoder::new_with_quality(&mut out, 90);
    encoder
        .encode(
            rgb.as_raw(),
            width_px,
            height_px,
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|error| format!("cannot encode page {} to JPEG: {error}", page.index + 1))?;

    Ok((out, width_px, height_px, PdfColorSpace::DeviceRgb))
}

fn pdf_color_space_for_jpeg(color_type: ColorType) -> Option<PdfColorSpace> {
    match color_type {
        ColorType::L8 | ColorType::L16 => Some(PdfColorSpace::DeviceGray),
        ColorType::Rgb8 | ColorType::Rgb16 => Some(PdfColorSpace::DeviceRgb),
        _ => None,
    }
}

fn pixels_to_points(pixels: u32, dpi: u32) -> Result<f32, String> {
    if dpi == 0 {
        return Err("dpi must be greater than zero".to_owned());
    }

    Ok((pixels as f32 * 72.0) / dpi as f32)
}

fn sweep_spool_root(root: &Path) -> Result<(), String> {
    if !root.exists() {
        return Ok(());
    }

    let entries = fs::read_dir(root)
        .map_err(|error| format!("cannot list spool root {}: {error}", root.display()))?;

    for entry in entries {
        let entry = entry
            .map_err(|error| format!("cannot read spool root entry {}: {error}", root.display()))?;
        let path = entry.path();
        if path.is_dir() {
            fs::remove_dir_all(&path).map_err(|error| {
                format!("cannot remove stale spool {}: {error}", path.display())
            })?;
        } else {
            fs::remove_file(&path).map_err(|error| {
                format!("cannot remove stale spool file {}: {error}", path.display())
            })?;
        }
    }

    Ok(())
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

fn image_format(options: &BTreeMap<String, Value>) -> Result<OutputFormat, String> {
    let Some(value) = options.get(IMAGE_FORMAT) else {
        return Ok(OutputFormat::Jpeg);
    };

    let Value::Str(format) = value else {
        return Err("image option format must be a string".to_owned());
    };

    match format.as_str() {
        "jpeg" | "jpg" => Ok(OutputFormat::Jpeg),
        "png" => Ok(OutputFormat::Png),
        other => Err(format!(
            "image format must be one of jpeg/jpg/png, got {other:?}"
        )),
    }
}

fn image_quality(options: &BTreeMap<String, Value>) -> Result<u8, String> {
    let Some(value) = options.get(IMAGE_QUALITY) else {
        return Ok(90);
    };

    let quality =
        as_u64(value).ok_or_else(|| "image option quality must be an integer".to_owned())?;
    u8::try_from(quality).map_err(|_| format!("image quality must be in 1..=100, got {quality}"))
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
    if let Some(path) = std::env::var_os("XDG_CONFIG_HOME") {
        return PathBuf::from(path).join("scanbus").join("profiles.json");
    }

    home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".config")
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
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "cannot create profile store directory {}: {error}",
                parent.display()
            )
        })?;
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
    use futures_util::stream;
    use image::{ImageBuffer, Rgba};
    use lopdf::Object as PdfObject;
    use scanbus_core::ProfileProcessor;

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

        assert_eq!(
            resolved.get(IMAGE_FORMAT),
            Some(&Value::Str("png".to_owned()))
        );
        assert_eq!(resolved.get(IMAGE_QUALITY), Some(&Value::U64(90)));
    }

    fn tiny_jpeg() -> Vec<u8> {
        let pixels: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(2, 2, Rgba([10, 20, 30, 255]));
        let mut out = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut out, 85);
        encoder
            .encode_image(&image::DynamicImage::ImageRgba8(pixels))
            .unwrap();
        out
    }

    fn tiny_png() -> Vec<u8> {
        let pixels: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(2, 2, Rgba([220, 120, 90, 255]));
        let mut cursor = Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(pixels)
            .write_to(&mut cursor, ImageFormat::Png)
            .unwrap();
        cursor.into_inner()
    }

    #[tokio::test]
    async fn image_profile_passes_through_when_input_and_output_match() {
        let dir = std::env::temp_dir().join(format!(
            "scanbus-image-pass-through-{}-{}",
            std::process::id(),
            scan_stamp()
        ));
        fs::create_dir_all(&dir).unwrap();

        let jpeg = tiny_jpeg();
        let page = RawPage {
            index: 0,
            format: PageFormat::Jpeg,
            resolution_dpi: 300,
            data: jpeg.clone(),
        };

        let options = BTreeMap::from([
            (IMAGE_FORMAT.to_owned(), Value::Str("jpeg".to_owned())),
            (
                OUTPUT_FOLDER.to_owned(),
                Value::Str(dir.to_string_lossy().to_string()),
            ),
        ]);

        let result = ImageProcessor
            .process(Box::pin(stream::iter([page])), &options)
            .await
            .unwrap();

        let ProfileResult::Image { paths } = result else {
            panic!("unexpected profile result");
        };
        let written = std::fs::read(&paths[0]).unwrap();
        assert_eq!(
            written, jpeg,
            "matching JPEG input must be passed through unchanged"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn image_profile_converts_when_output_differs() {
        let dir = std::env::temp_dir().join(format!(
            "scanbus-image-convert-{}-{}",
            std::process::id(),
            scan_stamp()
        ));
        fs::create_dir_all(&dir).unwrap();

        let options = BTreeMap::from([
            (IMAGE_FORMAT.to_owned(), Value::Str("png".to_owned())),
            (
                OUTPUT_FOLDER.to_owned(),
                Value::Str(dir.to_string_lossy().to_string()),
            ),
        ]);
        let page = RawPage {
            index: 0,
            format: PageFormat::Jpeg,
            resolution_dpi: 300,
            data: tiny_jpeg(),
        };

        let result = ImageProcessor
            .process(Box::pin(stream::iter([page])), &options)
            .await
            .unwrap();
        let ProfileResult::Image { paths } = result else {
            panic!("unexpected profile result");
        };

        let written = std::fs::read(&paths[0]).unwrap();
        let format = image::guess_format(&written).unwrap();
        assert_eq!(format, ImageFormat::Png);

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn image_profile_uses_page_numbers_in_sort_order() {
        let dir = std::env::temp_dir().join(format!(
            "scanbus-image-page-numbers-{}-{}",
            std::process::id(),
            scan_stamp()
        ));
        fs::create_dir_all(&dir).unwrap();

        let options = BTreeMap::from([
            (IMAGE_FORMAT.to_owned(), Value::Str("png".to_owned())),
            (
                OUTPUT_FOLDER.to_owned(),
                Value::Str(dir.to_string_lossy().to_string()),
            ),
        ]);

        let pages = (0..10).map(|index| RawPage {
            index,
            format: PageFormat::Png,
            resolution_dpi: 300,
            data: tiny_png(),
        });

        let result = ImageProcessor
            .process(Box::pin(stream::iter(pages)), &options)
            .await
            .unwrap();
        let ProfileResult::Image { paths } = result else {
            panic!("unexpected profile result");
        };

        let names: Vec<String> = paths
            .into_iter()
            .map(|path| {
                PathBuf::from(path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        let sorted = {
            let mut copy = names.clone();
            copy.sort();
            copy
        };
        assert_eq!(
            names, sorted,
            "filenames should already be lexicographically sorted by page index"
        );
        assert!(names[0].contains("p001"));
        assert!(names[9].contains("p010"));

        let _ = fs::remove_dir_all(dir);
    }

    fn jpeg_with_dimensions(width: u32, height: u32, color: [u8; 4]) -> Vec<u8> {
        let pixels: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_pixel(width, height, Rgba(color));
        let mut out = Vec::new();
        let mut encoder = JpegEncoder::new_with_quality(&mut out, 85);
        encoder
            .encode_image(&image::DynamicImage::ImageRgba8(pixels))
            .unwrap();
        out
    }

    fn as_f32(value: &PdfObject) -> Option<f32> {
        match value {
            PdfObject::Integer(v) => Some(*v as f32),
            PdfObject::Real(v) => Some(*v),
            _ => None,
        }
    }

    #[tokio::test]
    async fn document_profile_writes_a_real_pdf_with_expected_page_size() {
        let dir = std::env::temp_dir().join(format!(
            "scanbus-document-pdf-{}-{}",
            std::process::id(),
            scan_stamp()
        ));
        fs::create_dir_all(&dir).unwrap();

        let options = BTreeMap::from([(
            OUTPUT_FOLDER.to_owned(),
            Value::Str(dir.to_string_lossy().to_string()),
        )]);
        let page = RawPage {
            index: 0,
            format: PageFormat::Jpeg,
            resolution_dpi: 300,
            data: jpeg_with_dimensions(300, 600, [60, 100, 200, 255]),
        };

        let result = DocumentProcessor::new()
            .process(Box::pin(stream::iter([page])), &options)
            .await
            .unwrap();
        let ProfileResult::Document { paths } = result else {
            panic!("unexpected profile result");
        };
        assert_eq!(paths.len(), 1);

        let pdf = LoDocument::load(&paths[0]).unwrap();
        let pages = pdf.get_pages();
        assert_eq!(pages.len(), 1);

        let page_id = pages[&1];
        let page_dict = pdf.get_object(page_id).unwrap().as_dict().unwrap();
        let media_box = page_dict.get(b"MediaBox").unwrap().as_array().unwrap();

        let width_pt = as_f32(&media_box[2]).unwrap();
        let height_pt = as_f32(&media_box[3]).unwrap();
        assert!(
            (width_pt - 72.0).abs() < 0.2,
            "unexpected width: {width_pt}"
        );
        assert!(
            (height_pt - 144.0).abs() < 0.2,
            "unexpected height: {height_pt}"
        );

        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn document_profile_embeds_jpeg_input_without_reencoding() {
        let dir = std::env::temp_dir().join(format!(
            "scanbus-document-jpeg-pass-through-{}-{}",
            std::process::id(),
            scan_stamp()
        ));
        fs::create_dir_all(&dir).unwrap();

        let jpeg = jpeg_with_dimensions(64, 64, [10, 40, 90, 255]);
        let page = RawPage {
            index: 0,
            format: PageFormat::Jpeg,
            resolution_dpi: 300,
            data: jpeg.clone(),
        };
        let options = BTreeMap::from([(
            OUTPUT_FOLDER.to_owned(),
            Value::Str(dir.to_string_lossy().to_string()),
        )]);

        let result = DocumentProcessor::new()
            .process(Box::pin(stream::iter([page])), &options)
            .await
            .unwrap();
        let ProfileResult::Document { paths } = result else {
            panic!("unexpected profile result");
        };

        let pdf = LoDocument::load(&paths[0]).unwrap();
        let mut found = false;
        for object in pdf.objects.values() {
            let PdfObject::Stream(stream) = object else {
                continue;
            };

            let Ok(PdfObject::Name(subtype)) = stream.dict.get(b"Subtype") else {
                continue;
            };
            if subtype.as_slice() != b"Image" {
                continue;
            }

            let Ok(PdfObject::Name(filter)) = stream.dict.get(b"Filter") else {
                continue;
            };
            if filter.as_slice() != b"DCTDecode" {
                continue;
            }

            if stream.content == jpeg {
                found = true;
                break;
            }
        }

        assert!(found, "could not find the original JPEG stream in the PDF");
        let _ = fs::remove_dir_all(dir);
    }

    #[tokio::test]
    async fn document_profile_multi_page_false_writes_one_pdf_per_page() {
        let dir = std::env::temp_dir().join(format!(
            "scanbus-document-single-per-page-{}-{}",
            std::process::id(),
            scan_stamp()
        ));
        fs::create_dir_all(&dir).unwrap();

        let options = BTreeMap::from([
            (DOCUMENT_MULTI_PAGE.to_owned(), Value::Bool(false)),
            (
                OUTPUT_FOLDER.to_owned(),
                Value::Str(dir.to_string_lossy().to_string()),
            ),
        ]);
        let pages = (0..3).map(|index| RawPage {
            index,
            format: PageFormat::Jpeg,
            resolution_dpi: 300,
            data: jpeg_with_dimensions(32, 48, [20 + index as u8, 80, 140, 255]),
        });

        let result = DocumentProcessor::new()
            .process(Box::pin(stream::iter(pages)), &options)
            .await
            .unwrap();
        let ProfileResult::Document { paths } = result else {
            panic!("unexpected profile result");
        };

        assert_eq!(paths.len(), 3);
        for path in &paths {
            let doc = LoDocument::load(path).unwrap();
            assert_eq!(doc.get_pages().len(), 1);
        }

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn sweep_spool_root_removes_stale_entries() {
        let root = std::env::temp_dir().join(format!(
            "scanbus-document-sweep-{}-{}",
            std::process::id(),
            scan_stamp()
        ));
        fs::create_dir_all(root.join("orphan-a")).unwrap();
        fs::create_dir_all(root.join("orphan-b")).unwrap();
        fs::write(root.join("orphan-file"), b"x").unwrap();

        sweep_spool_root(&root).unwrap();
        let remaining = fs::read_dir(&root).unwrap().count();
        assert_eq!(remaining, 0);

        let _ = fs::remove_dir_all(root);
    }
}
