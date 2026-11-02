//! The attachment blob store: images normalised once, then kept by content address.
//!
//! Every image the app keeps goes through [`normalise`] first: its format is read from its
//! content (PNG, JPEG, WebP or GIF, nothing else), it is decoded under hard limits so a
//! decompression bomb fails cleanly, scaled down to [`MAX_EDGE`] on its long edge and encoded
//! again as PNG (JPEG when the PNG would be too large). Encoding again drops every piece of
//! metadata the original carried (EXIF and its GPS position, text chunks, colour profiles).
//!
//! [`BlobStore`] keeps the result as `<sha256>.<ext>` in a private folder (`0700`, files `0600`),
//! written whole or not at all. Events only ever carry the [`AttachmentRef`]; the bytes are read
//! back through [`BlobStore::read`], which checks them against the ref's id. [`BlobStore::sweep`]
//! removes the blobs no event refers to any more.

use std::collections::HashSet;
use std::fmt;
use std::io::{self, Cursor, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use agent_core::attachment::{AttachmentKind, AttachmentRef, Origin, PromptAttachment};
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::error::LimitErrorKind;
use image::imageops::FilterType;
use image::{DynamicImage, ImageDecoder, ImageError, ImageFormat, ImageReader, Limits, RgbImage};
use sha2::{Digest, Sha256};

use crate::fsutil::effective_uid;

/// The largest input accepted, in bytes: a file or a paste over it is refused unread.
pub const MAX_INPUT_BYTES: u64 = 20 * 1024 * 1024;
/// The largest side an input may have, in pixels.
pub const MAX_SIDE: u32 = 16_384;
/// The most memory the decoder may take for one image.
pub const MAX_DECODE_ALLOC: u64 = 512 * 1024 * 1024;
/// The long edge a stored image is scaled down to.
pub const MAX_EDGE: u32 = 2048;
/// A PNG over this many bytes is stored as JPEG instead.
pub const MAX_PNG_BYTES: usize = 3_932_160;
/// The JPEG quality of that fallback.
const JPEG_QUALITY: u8 = 85;
/// The prefix and suffix of this module's temp files, the only other names [`BlobStore::sweep`]
/// touches.
const TEMP_PREFIX: &str = ".attach-";
const TEMP_SUFFIX: &str = ".tmp";

/// Why an image was not taken. Its text is shown to the user as is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttachError {
    /// Not a PNG, JPEG, WebP or GIF (by content, whatever its name says).
    NotAnImage,
    /// Over [`MAX_INPUT_BYTES`].
    TooLarge,
    /// Over [`MAX_SIDE`] pixels a side, or more than the decoder may allocate.
    TooLargeToOpen,
    /// The decoder or the file failed.
    Unreadable(String),
    /// The image was read but could not be saved.
    NotSaved(String),
}

impl fmt::Display for AttachError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotAnImage => write!(f, "Not an image (PNG, JPEG, WebP or GIF)"),
            Self::TooLarge => write!(f, "Image too large (over 20 MB)"),
            Self::TooLargeToOpen => {
                write!(f, "Image too large to open (over {MAX_SIDE} pixels a side)")
            }
            Self::Unreadable(why) => write!(f, "Couldn't read the image: {why}"),
            Self::NotSaved(why) => write!(f, "Couldn't save the image: {why}"),
        }
    }
}

impl std::error::Error for AttachError {}

impl From<ImageError> for AttachError {
    fn from(e: ImageError) -> Self {
        match e {
            ImageError::Limits(l) => match l.kind() {
                LimitErrorKind::DimensionError | LimitErrorKind::InsufficientMemory => {
                    Self::TooLargeToOpen
                }
                _ => Self::Unreadable(l.to_string()),
            },
            ImageError::Unsupported(_) => Self::NotAnImage,
            other => Self::Unreadable(other.to_string()),
        }
    }
}

/// A normalised image: its ref and the bytes to store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Normalised {
    pub meta: AttachmentRef,
    pub bytes: Vec<u8>,
}

/// Reads `input` as an image, scales it to fit [`MAX_EDGE`] and encodes it again (PNG, or JPEG
/// when the PNG is over [`MAX_PNG_BYTES`]). `name` is the original file name, kept redacted and
/// reduced to its last component; `origin` says who put the image in the thread.
pub fn normalise(
    input: &[u8],
    name: Option<&str>,
    origin: Origin,
) -> Result<Normalised, AttachError> {
    if input.len() as u64 > MAX_INPUT_BYTES {
        return Err(AttachError::TooLarge);
    }
    let format = image::guess_format(input).map_err(|_| AttachError::NotAnImage)?;
    if !matches!(
        format,
        ImageFormat::Png | ImageFormat::Jpeg | ImageFormat::WebP | ImageFormat::Gif
    ) {
        return Err(AttachError::NotAnImage);
    }
    let image = decode(input, format)?;
    let image = fit(image);
    let (width, height) = (image.width(), image.height());
    let (bytes, mime) = encode(image)?;
    let id = hex_sha256(&bytes);
    Ok(Normalised {
        meta: AttachmentRef {
            id,
            kind: AttachmentKind::Image,
            mime: mime.to_owned(),
            width,
            height,
            bytes: bytes.len() as u64,
            name: name.and_then(display_name),
            origin,
        },
        bytes,
    })
}

/// Decodes the first frame under the limits, upright (an EXIF orientation is applied, since the
/// tag itself does not survive).
fn decode(input: &[u8], format: ImageFormat) -> Result<DynamicImage, AttachError> {
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_DECODE_ALLOC);
    let mut reader = ImageReader::with_format(Cursor::new(input), format);
    reader.limits(limits);
    // `into_decoder` checks the sides; the output buffer is checked here, before it is made.
    let mut decoder = reader.into_decoder()?;
    if decoder.total_bytes() > MAX_DECODE_ALLOC {
        return Err(AttachError::TooLargeToOpen);
    }
    let orientation = decoder.orientation().ok();
    let mut image = DynamicImage::from_decoder(decoder)?;
    if let Some(o) = orientation {
        image.apply_orientation(o);
    }
    Ok(image)
}

/// Scales `image` down so its long edge is at most [`MAX_EDGE`], keeping its aspect. A very
/// large image is first averaged down to twice that (cheap, and no large float buffer), then
/// filtered to size.
fn fit(image: DynamicImage) -> DynamicImage {
    let long = image.width().max(image.height());
    if long <= MAX_EDGE {
        return image;
    }
    let image = if long > 2 * MAX_EDGE {
        image.thumbnail(2 * MAX_EDGE, 2 * MAX_EDGE)
    } else {
        image
    };
    image.resize(MAX_EDGE, MAX_EDGE, FilterType::Lanczos3)
}

/// The stored bytes and their type: PNG when it fits [`MAX_PNG_BYTES`], else JPEG on white.
fn encode(image: DynamicImage) -> Result<(Vec<u8>, &'static str), AttachError> {
    // Eight bits a channel is all the store keeps; it also halves a 16-bit PNG.
    let image = match image {
        DynamicImage::ImageLuma8(_)
        | DynamicImage::ImageLumaA8(_)
        | DynamicImage::ImageRgb8(_)
        | DynamicImage::ImageRgba8(_) => image,
        other if other.color().has_alpha() => DynamicImage::ImageRgba8(other.to_rgba8()),
        other => DynamicImage::ImageRgb8(other.to_rgb8()),
    };
    let mut png = Vec::new();
    image
        .write_with_encoder(PngEncoder::new(&mut png))
        .map_err(|e| AttachError::Unreadable(e.to_string()))?;
    if png.len() <= MAX_PNG_BYTES {
        return Ok((png, "image/png"));
    }
    drop(png);
    let flat = match image {
        DynamicImage::ImageLuma8(_) | DynamicImage::ImageRgb8(_) => image,
        other if !other.color().has_alpha() => DynamicImage::ImageRgb8(other.to_rgb8()),
        other => DynamicImage::ImageRgb8(on_white(&other)),
    };
    let mut jpeg = Vec::new();
    flat.write_with_encoder(JpegEncoder::new_with_quality(&mut jpeg, JPEG_QUALITY))
        .map_err(|e| AttachError::Unreadable(e.to_string()))?;
    Ok((jpeg, "image/jpeg"))
}

/// `image` composited onto white: JPEG has no alpha.
fn on_white(image: &DynamicImage) -> RgbImage {
    let rgba = image.to_rgba8();
    RgbImage::from_fn(rgba.width(), rgba.height(), |x, y| {
        let [r, g, b, a] = rgba.get_pixel(x, y).0;
        let a = u16::from(a);
        let over = |c: u8| ((u16::from(c) * a + 255 * (255 - a) + 127) / 255) as u8;
        image::Rgb([over(r), over(g), over(b)])
    })
}

/// The name kept for an image: the last component of `name`, redacted, without control
/// characters; `None` when nothing is left.
fn display_name(name: &str) -> Option<String> {
    let last = name.rsplit('/').next().unwrap_or("");
    let redacted = agent_core::redact::redact(last);
    let clean: String = redacted
        .rsplit('/')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| !c.is_control())
        .collect();
    let clean = clean.trim();
    (!clean.is_empty() && clean != "." && clean != "..").then(|| clean.to_owned())
}

/// Lowercase hex SHA-256 of `bytes`: an attachment's id.
pub fn hex_sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Whether `name` is one of the store's blobs: `<64 hex>.png` or `<64 hex>.jpg`. The id, if so.
fn blob_id(name: &str) -> Option<&str> {
    let (id, ext) = name.split_once('.')?;
    (AttachmentRef::valid_id(id) && matches!(ext, "png" | "jpg")).then_some(id)
}

/// Whether `name` is one of this module's temp files.
fn is_own_temp(name: &str) -> bool {
    name.starts_with(TEMP_PREFIX) && name.ends_with(TEMP_SUFFIX)
}

/// What [`BlobStore::sweep`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct SweepReport {
    /// Blobs and stale temp files removed.
    pub removed: usize,
    /// Blobs left: referenced, or too recent to judge.
    pub kept: usize,
}

/// The folder of stored attachments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobStore {
    dir: PathBuf,
}

impl BlobStore {
    /// `$XDG_DATA_HOME/agent-terminal/attachments`, falling back to
    /// `~/.local/share/agent-terminal/attachments`, from this process's environment. `None` with
    /// no usable home.
    pub fn default_location() -> Option<PathBuf> {
        Self::location_for(
            std::env::var("XDG_DATA_HOME").ok().as_deref(),
            std::env::var("HOME").ok().as_deref(),
        )
    }

    /// [`BlobStore::default_location`] for the given values, so tests never read the real home.
    /// Like the thread store, only an absolute `xdg_data_home` counts (empty and relative values
    /// are ignored, as the XDG spec requires).
    pub fn location_for(xdg_data_home: Option<&str>, home: Option<&str>) -> Option<PathBuf> {
        let base = match xdg_data_home.filter(|v| Path::new(v).is_absolute()) {
            Some(xdg) => PathBuf::from(xdg),
            None => PathBuf::from(home.filter(|h| !h.is_empty())?).join(".local/share"),
        };
        Some(base.join(crate::uninstall::APP_DIR).join("attachments"))
    }

    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Creates the folder (`0700`) when missing and checks it is a real folder owned by this
    /// user, never a link; an existing one is narrowed to `0700`.
    fn ensure_dir(&self) -> io::Result<()> {
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&self.dir)?;
        self.check_dir()?;
        let meta = std::fs::symlink_metadata(&self.dir)?;
        if meta.mode() & 0o077 != 0 {
            std::fs::set_permissions(&self.dir, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(())
    }

    /// Refuses a folder that is a link, not a folder, or owned by someone else.
    fn check_dir(&self) -> io::Result<()> {
        let meta = std::fs::symlink_metadata(&self.dir)?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!(
                    "{} is not a folder (a link is never followed)",
                    self.dir.display()
                ),
            ));
        }
        if meta.uid() != effective_uid() {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("{} is owned by another user", self.dir.display()),
            ));
        }
        Ok(())
    }

    /// Stores `n` as `<id>.<ext>`: written to a new private temp file, synced, then renamed into
    /// place. A blob already there with the same size is kept (the name is its content's hash).
    pub fn put(&self, n: &Normalised) -> io::Result<PathBuf> {
        let target = self.path_of(&n.meta).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "not a stored image type")
        })?;
        if hex_sha256(&n.bytes) != n.meta.id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the image does not match its id",
            ));
        }
        self.ensure_dir()?;
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let tmp = self.dir.join(format!(
            "{TEMP_PREFIX}{}-{}-{nanos}{TEMP_SUFFIX}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let result = (|| {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&tmp)?;
            file.write_all(&n.bytes)?;
            file.sync_all()?;
            drop(file);
            match std::fs::symlink_metadata(&target) {
                Ok(m) if m.file_type().is_file() && m.len() == n.bytes.len() as u64 => {
                    std::fs::remove_file(&tmp)
                }
                // Missing, damaged or not a file: the rename replaces the entry itself, never
                // what a link there points at.
                _ => std::fs::rename(&tmp, &target),
            }
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&tmp);
        } else {
            let _ = std::fs::File::open(&self.dir).and_then(|d| d.sync_all());
        }
        result.map(|()| target)
    }

    /// Where `r` is stored; `None` when it is not a ref the store wrote.
    pub fn path_of(&self, r: &AttachmentRef) -> Option<PathBuf> {
        r.file_name().map(|name| self.dir.join(name))
    }

    /// The stored bytes of `r`, checked against its id. A link, a non-regular file, one over
    /// [`MAX_INPUT_BYTES`] or one whose hash differs is an error.
    pub fn read(&self, r: &AttachmentRef) -> io::Result<Vec<u8>> {
        let path = self
            .path_of(r)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a stored image"))?;
        self.check_dir()?;
        let bytes = read_regular(&path, libc::O_NOFOLLOW)?;
        if hex_sha256(&bytes) != r.id {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} does not match its id", path.display()),
            ));
        }
        Ok(bytes)
    }

    /// `r` ready to hand to an adapter: its checked bytes as base64, and its path.
    pub fn prompt_attachment(&self, r: &AttachmentRef) -> io::Result<PromptAttachment> {
        use base64::Engine as _;
        let bytes = self.read(r)?;
        let path = self
            .path_of(r)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "not a stored image"))?;
        Ok(PromptAttachment {
            meta: r.clone(),
            path: path.to_string_lossy().into_owned(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        })
    }

    /// Reads a file the user chose (a link is followed: the user picked it), normalises it under
    /// its file name and stores it. Never for a path an agent supplied.
    pub fn ingest_path(&self, path: &Path, origin: Origin) -> Result<Normalised, AttachError> {
        let bytes = read_regular(path, 0).map_err(|e| match e.kind() {
            io::ErrorKind::FileTooLarge => AttachError::TooLarge,
            _ => AttachError::Unreadable(e.to_string()),
        })?;
        let name = path.file_name().and_then(|n| n.to_str());
        self.ingest_bytes(&bytes, name, origin)
    }

    /// Normalises `bytes` and stores the result.
    pub fn ingest_bytes(
        &self,
        bytes: &[u8],
        name: Option<&str>,
        origin: Origin,
    ) -> Result<Normalised, AttachError> {
        let n = normalise(bytes, name, origin)?;
        self.put(&n)
            .map_err(|e| AttachError::NotSaved(e.to_string()))?;
        Ok(n)
    }

    /// Removes every blob not in `referenced` (ids) and older than `grace`, and this module's
    /// temp files older than `grace`. Only names the store writes are looked at, only regular
    /// files (never a link, never a folder), and only in the folder itself.
    pub fn sweep(&self, referenced: &HashSet<String>, grace: Duration) -> io::Result<SweepReport> {
        let mut report = SweepReport::default();
        match self.check_dir() {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(report),
            Err(e) => return Err(e),
        }
        let now = SystemTime::now();
        for entry in std::fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            let blob = blob_id(name);
            if blob.is_none() && !is_own_temp(name) {
                continue;
            }
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.file_type().is_file() {
                continue;
            }
            let old = meta
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age >= grace);
            let keep = blob.is_some_and(|id| referenced.contains(id)) || !old;
            if keep {
                if blob.is_some() {
                    report.kept += 1;
                }
                continue;
            }
            match std::fs::remove_file(&path) {
                Ok(()) => report.removed += 1,
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e),
            }
        }
        Ok(report)
    }
}

/// Reads `path` whole when it is a regular file of at most [`MAX_INPUT_BYTES`], opened without
/// blocking (a FIFO never stalls the read) and with `flags` added. Over the cap is
/// `ErrorKind::FileTooLarge`.
fn read_regular(path: &Path, flags: libc::c_int) -> io::Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NONBLOCK | flags)
        .open(path)?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("{} is not a regular file", path.display()),
        ));
    }
    if meta.len() > MAX_INPUT_BYTES {
        return Err(io::Error::new(io::ErrorKind::FileTooLarge, "over 20 MB"));
    }
    let mut bytes = Vec::with_capacity(meta.len() as usize);
    file.take(MAX_INPUT_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_INPUT_BYTES {
        return Err(io::Error::new(io::ErrorKind::FileTooLarge, "over 20 MB"));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::symlink;

    use image::{GenericImageView, ImageEncoder, Rgb, RgbImage, Rgba, RgbaImage};

    use super::*;

    fn png_of(w: u32, h: u32) -> Vec<u8> {
        let img = RgbImage::from_fn(w, h, |x, y| Rgb([(x % 256) as u8, (y % 256) as u8, 7]));
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgb8)
            .expect("encode");
        out
    }

    /// Pseudo-random bytes (an LCG): an image that compresses badly.
    fn noise_png(w: u32, h: u32) -> Vec<u8> {
        let mut s: u32 = 0x1234_5678;
        let img = RgbaImage::from_fn(w, h, |_, _| {
            s = s.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [a, b, c, _] = s.to_le_bytes();
            Rgba([a, b, c, 128])
        });
        let mut out = Vec::new();
        PngEncoder::new(&mut out)
            .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
            .expect("encode");
        out
    }

    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut c = (data.len() as u32).to_be_bytes().to_vec();
        c.extend_from_slice(kind);
        c.extend_from_slice(data);
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        c.extend_from_slice(&crc32(&body).to_be_bytes());
        c
    }

    /// A PNG whose header claims `w`×`h`: decoding must refuse it before allocating.
    fn png_claiming(w: u32, h: u32) -> Vec<u8> {
        let mut ihdr = w.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut out = b"\x89PNG\r\n\x1a\n".to_vec();
        out.extend(chunk(b"IHDR", &ihdr));
        out.extend(chunk(
            b"IDAT",
            &[0x78, 0x9c, 0x03, 0x00, 0x00, 0x00, 0x00, 0x01],
        ));
        out.extend(chunk(b"IEND", &[]));
        out
    }

    const SECRET_TEXT: &[u8] = b"GPS 45.5017N 73.5673W secret-camera-serial";

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn garbage_oversize_and_bombs_are_refused() {
        assert_eq!(
            normalise(b"hello, not an image", None, Origin::User),
            Err(AttachError::NotAnImage)
        );
        // A format the store does not take, by content.
        assert_eq!(
            normalise(b"BM\0\0\0\0\0\0\0\0\0\0\0\0\0\0", None, Origin::User),
            Err(AttachError::NotAnImage)
        );
        let big = vec![0u8; MAX_INPUT_BYTES as usize + 1];
        assert_eq!(
            normalise(&big, None, Origin::User),
            Err(AttachError::TooLarge)
        );
        assert_eq!(
            normalise(&png_claiming(MAX_SIDE + 1, 1), None, Origin::User),
            Err(AttachError::TooLargeToOpen)
        );
        // Within the side limit, but its pixels would take more than the decoder may allocate.
        assert_eq!(
            normalise(&png_claiming(16_000, 16_000), None, Origin::User),
            Err(AttachError::TooLargeToOpen)
        );
        let truncated = &png_of(64, 64)[..60];
        assert!(matches!(
            normalise(truncated, None, Origin::User),
            Err(AttachError::Unreadable(_))
        ));
        assert_eq!(
            AttachError::TooLargeToOpen.to_string(),
            "Image too large to open (over 16384 pixels a side)"
        );
    }

    #[test]
    fn a_large_image_is_scaled_to_the_long_edge_keeping_its_aspect() {
        let n = normalise(&png_of(4100, 1025), Some("wide.png"), Origin::User).expect("norm");
        assert_eq!((n.meta.width, n.meta.height), (2048, 512));
        assert_eq!(n.meta.mime, "image/png");
        let back = image::load_from_memory(&n.bytes).expect("decode");
        assert_eq!(back.dimensions(), (2048, 512));
        assert_eq!(n.meta.bytes, n.bytes.len() as u64);
        assert_eq!(n.meta.id, hex_sha256(&n.bytes));
        assert!(AttachmentRef::valid_id(&n.meta.id));

        let small = normalise(&png_of(30, 20), None, Origin::ToolResult).expect("norm");
        assert_eq!((small.meta.width, small.meta.height), (30, 20));
        assert_eq!(small.meta.origin, Origin::ToolResult);
        assert_eq!(small.meta.name, None);
    }

    #[test]
    fn an_animated_gif_keeps_its_first_frame() {
        use image::codecs::gif::GifEncoder;
        use image::{Delay, Frame};
        let frame = |c: [u8; 4]| {
            Frame::from_parts(
                RgbaImage::from_pixel(6, 4, Rgba(c)),
                0,
                0,
                Delay::from_numer_denom_ms(100, 1),
            )
        };
        let mut gif = Vec::new();
        {
            let mut enc = GifEncoder::new(&mut gif);
            enc.encode_frames([frame([255, 0, 0, 255]), frame([0, 0, 255, 255])])
                .expect("gif");
        }
        let n = normalise(&gif, Some("anim.gif"), Origin::User).expect("norm");
        assert_eq!(n.meta.mime, "image/png");
        let back = image::load_from_memory(&n.bytes)
            .expect("decode")
            .to_rgba8();
        assert_eq!(back.dimensions(), (6, 4));
        assert_eq!(back.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn an_image_too_large_as_png_is_stored_as_jpeg_on_white() {
        let n = normalise(&noise_png(1600, 1600), None, Origin::User).expect("norm");
        assert_eq!(n.meta.mime, "image/jpeg");
        assert_eq!(&n.bytes[..2], &[0xFF, 0xD8]);
        assert!(n.bytes.len() <= MAX_PNG_BYTES);
        let back = image::load_from_memory(&n.bytes).expect("decode");
        assert_eq!(back.dimensions(), (1600, 1600));
        assert!(!back.color().has_alpha());
        // Transparent black on white is white.
        let clear = RgbaImage::from_pixel(1, 1, Rgba([0, 0, 0, 0]));
        assert_eq!(
            on_white(&DynamicImage::ImageRgba8(clear)).get_pixel(0, 0).0,
            [255, 255, 255]
        );
    }

    #[test]
    fn metadata_does_not_survive() {
        // PNG: a text chunk and an EXIF chunk after the header.
        let plain = png_of(40, 30);
        let mut tagged = plain[..33].to_vec();
        tagged.extend(chunk(
            b"tEXt",
            &[b"Comment\0".as_slice(), SECRET_TEXT].concat(),
        ));
        tagged.extend(chunk(b"eXIf", &[b"MM\0*".as_slice(), SECRET_TEXT].concat()));
        tagged.extend_from_slice(&plain[33..]);
        assert!(image::load_from_memory(&tagged).is_ok(), "fixture decodes");
        let n = normalise(&tagged, None, Origin::User).expect("norm");
        for needle in [b"tEXt".as_slice(), b"eXIf", b"iTXt", b"zTXt", SECRET_TEXT] {
            assert!(
                !has(&n.bytes, needle),
                "{}",
                String::from_utf8_lossy(needle)
            );
        }

        // JPEG: an APP1 EXIF segment after the start-of-image marker.
        let mut jpeg = Vec::new();
        RgbImage::from_pixel(16, 16, Rgb([10, 200, 30]))
            .write_with_encoder(JpegEncoder::new_with_quality(&mut jpeg, 90))
            .expect("jpeg");
        let payload = [b"Exif\0\0MM\0*".as_slice(), SECRET_TEXT].concat();
        let mut app1 = vec![0xFF, 0xE1];
        app1.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        app1.extend_from_slice(&payload);
        let tagged = [&jpeg[..2], &app1, &jpeg[2..]].concat();
        let n = normalise(&tagged, None, Origin::User).expect("norm");
        assert!(!has(&n.bytes, b"Exif"));
        assert!(!has(&n.bytes, SECRET_TEXT));
    }

    #[test]
    fn the_name_is_a_redacted_file_name_only() {
        let n = normalise(
            &png_of(4, 4),
            Some("/home/u/secret dir/shot.png"),
            Origin::User,
        )
        .expect("norm");
        assert_eq!(n.meta.name.as_deref(), Some("shot.png"));
        for empty in ["", "  ", "/tmp/", "..", "a/.."] {
            assert_eq!(display_name(empty), None, "{empty:?}");
        }
        let token = "ghp_0123456789abcdefghijABCDEFGHIJ012345.png";
        let kept = display_name(token).expect("name");
        assert!(!kept.contains("0123456789abcdefghij"), "{kept}");
    }

    fn store() -> (tempfile::TempDir, BlobStore) {
        let tmp = tempfile::tempdir().expect("tmp");
        let store = BlobStore::new(tmp.path().join("data").join("attachments"));
        (tmp, store)
    }

    fn names(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir)
            .expect("dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        v.sort();
        v
    }

    #[test]
    fn put_is_private_idempotent_and_read_back_checked() {
        let (_tmp, store) = store();
        let n = store
            .ingest_bytes(&png_of(8, 8), Some("a.png"), Origin::User)
            .expect("ingest");
        let path = store.path_of(&n.meta).expect("path");
        assert_eq!(path, store.dir().join(format!("{}.png", n.meta.id)));
        assert_eq!(store.put(&n).expect("again"), path);
        assert_eq!(names(store.dir()), [format!("{}.png", n.meta.id)]);
        let mode = |p: &Path| std::fs::metadata(p).expect("meta").permissions().mode() & 0o777;
        assert_eq!(mode(&path), 0o600);
        assert_eq!(mode(store.dir()), 0o700);
        assert_eq!(store.read(&n.meta).expect("read"), n.bytes);
        let sent = store.prompt_attachment(&n.meta).expect("prompt");
        assert_eq!(sent.path, path.to_string_lossy());
        assert!(sent
            .data_url()
            .starts_with("data:image/png;base64,iVBORw0KGgo"));

        // A ref that is not a content address never becomes a path.
        let mut bad = n.meta.clone();
        bad.id = "../../etc/passwd".into();
        assert_eq!(store.path_of(&bad), None);
        assert!(store.read(&bad).is_err());
        // Bytes that do not match their id are never stored.
        let mut forged = n.clone();
        forged.bytes.push(0);
        assert!(store.put(&forged).is_err());
    }

    #[test]
    fn read_refuses_a_link_and_a_tampered_file() {
        let (tmp, store) = store();
        let n = store
            .ingest_bytes(&png_of(8, 8), None, Origin::User)
            .expect("ingest");
        let path = store.path_of(&n.meta).expect("path");
        std::fs::write(&path, b"tampered").expect("write");
        let err = store.read(&n.meta).expect_err("tampered");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);

        std::fs::remove_file(&path).expect("rm");
        let elsewhere = tmp.path().join("real.png");
        std::fs::write(&elsewhere, &n.bytes).expect("write");
        symlink(&elsewhere, &path).expect("link");
        assert!(store.read(&n.meta).is_err(), "a link is never followed");
    }

    #[test]
    fn put_refuses_a_linked_folder() {
        let tmp = tempfile::tempdir().expect("tmp");
        let real = tmp.path().join("real");
        std::fs::create_dir(&real).expect("dir");
        let link = tmp.path().join("attachments");
        symlink(&real, &link).expect("link");
        let store = BlobStore::new(&link);
        let n = normalise(&png_of(8, 8), None, Origin::User).expect("norm");
        assert!(store.put(&n).is_err());
        assert!(names(&real).is_empty());
        assert!(store.sweep(&HashSet::new(), Duration::ZERO).is_err());
    }

    #[test]
    fn ingest_path_reads_only_a_regular_file() {
        let (tmp, store) = store();
        let file = tmp.path().join("picked.png");
        std::fs::write(&file, png_of(10, 10)).expect("write");
        let n = store.ingest_path(&file, Origin::User).expect("ingest");
        assert_eq!(n.meta.name.as_deref(), Some("picked.png"));
        assert!(store.path_of(&n.meta).expect("path").is_file());

        let fifo = tmp.path().join("fifo.png");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes()).expect("cstr");
        // SAFETY: a valid NUL-terminated path.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        assert!(matches!(
            store.ingest_path(&fifo, Origin::User),
            Err(AttachError::Unreadable(_))
        ));
        assert!(matches!(
            store.ingest_path(tmp.path(), Origin::User),
            Err(AttachError::Unreadable(_))
        ));
        let huge = tmp.path().join("huge.png");
        let f = std::fs::File::create(&huge).expect("create");
        f.set_len(MAX_INPUT_BYTES + 1).expect("len");
        assert_eq!(
            store.ingest_path(&huge, Origin::User),
            Err(AttachError::TooLarge)
        );
    }

    fn age(path: &Path, by: Duration) {
        let f = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NOFOLLOW)
            .open(path)
            .expect("open");
        f.set_modified(SystemTime::now() - by).expect("mtime");
    }

    #[test]
    fn sweep_removes_only_old_unreferenced_blobs_of_its_own() {
        let (tmp, store) = store();
        let grace = Duration::from_secs(3600);
        let old = Duration::from_secs(7200);
        let mut blobs = Vec::new();
        for side in [5, 6, 7, 8] {
            let n = store
                .ingest_bytes(&png_of(side, side), None, Origin::User)
                .expect("ingest");
            blobs.push(store.path_of(&n.meta).expect("path"));
        }
        let [unreferenced, referenced, fresh, _kept] = &blobs[..] else {
            panic!("four blobs");
        };
        for p in [unreferenced, referenced] {
            age(p, old);
        }
        let id_of = |p: &Path| {
            p.file_stem()
                .and_then(|s| s.to_str())
                .expect("stem")
                .to_owned()
        };
        let mut keep: HashSet<String> = HashSet::new();
        keep.insert(id_of(referenced));
        keep.insert(id_of(&blobs[3]));

        // Foreign names, an old link with a blob's name, a folder, a stale and a fresh temp.
        let dir = store.dir();
        let foreign = dir.join("notes.txt");
        std::fs::write(&foreign, b"x").expect("write");
        age(&foreign, old);
        let upper = dir.join(format!("{}.png", "A".repeat(64)));
        std::fs::write(&upper, b"x").expect("write");
        age(&upper, old);
        let target = tmp.path().join("target");
        std::fs::write(&target, b"x").expect("write");
        age(&target, old);
        let link = dir.join(format!("{}.jpg", "b".repeat(64)));
        symlink(&target, &link).expect("link");
        std::fs::create_dir(dir.join(format!("{}.png", "c".repeat(64)))).expect("dir");
        let stale_tmp = dir.join(format!("{TEMP_PREFIX}1-2-3{TEMP_SUFFIX}"));
        std::fs::write(&stale_tmp, b"x").expect("write");
        age(&stale_tmp, old);
        let fresh_tmp = dir.join(format!("{TEMP_PREFIX}4-5-6{TEMP_SUFFIX}"));
        std::fs::write(&fresh_tmp, b"x").expect("write");

        let report = store.sweep(&keep, grace).expect("sweep");
        assert_eq!(
            report,
            SweepReport {
                removed: 2,
                kept: 3
            }
        );
        assert!(!unreferenced.exists());
        assert!(!stale_tmp.exists());
        for p in [
            referenced, fresh, &blobs[3], &foreign, &upper, &fresh_tmp, &target,
        ] {
            assert!(p.exists(), "{}", p.display());
        }
        assert!(std::fs::symlink_metadata(&link).is_ok(), "the link stays");

        let missing = BlobStore::new(tmp.path().join("none"));
        assert_eq!(
            missing.sweep(&HashSet::new(), grace).expect("sweep"),
            SweepReport::default()
        );
    }

    #[test]
    fn the_default_location_follows_xdg_and_needs_a_home() {
        assert_eq!(
            BlobStore::location_for(Some("/x/data"), Some("/h")),
            Some(PathBuf::from("/x/data/agent-terminal/attachments"))
        );
        for xdg in [None, Some(""), Some("relative")] {
            assert_eq!(
                BlobStore::location_for(xdg, Some("/h")),
                Some(PathBuf::from("/h/.local/share/agent-terminal/attachments"))
            );
        }
        assert_eq!(BlobStore::location_for(None, None), None);
        assert_eq!(BlobStore::location_for(None, Some("")), None);
    }
}
