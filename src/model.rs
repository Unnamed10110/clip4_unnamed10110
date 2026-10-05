//! Core data model (spec section 7). Pure: no Win32 handles, no I/O.

use crate::preview;
use crate::search::SearchIndex;
use std::sync::Arc;

// Standard clipboard format ids (winuser.h).
pub const CF_TEXT: u32 = 1;
pub const CF_BITMAP: u32 = 2;
pub const CF_METAFILEPICT: u32 = 3;
pub const CF_OEMTEXT: u32 = 7;
pub const CF_DIB: u32 = 8;
pub const CF_PALETTE: u32 = 9;
pub const CF_UNICODETEXT: u32 = 13;
pub const CF_ENHMETAFILE: u32 = 14;
pub const CF_HDROP: u32 = 15;
pub const CF_LOCALE: u32 = 16;
pub const CF_DIBV5: u32 = 17;

// Registered format names. Persisted by NAME, never by numeric id (spec 7.1).
pub const FMT_HTML: &str = "HTML Format";
pub const FMT_RTF: &str = "Rich Text Format";
pub const FMT_PNG: &str = "PNG";
pub const FMT_DROPEFFECT: &str = "Preferred DropEffect";

/// Payloads at or above this size live in `blobs\<sha1>` (spec 8.3).
pub const BLOB_THRESHOLD: usize = 256 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum FormatKey {
    /// `CF_*` constant (< 0xC000).
    Standard(u32),
    /// Registered format NAME.
    Registered(String),
}

impl FormatKey {
    pub fn reg(name: &str) -> Self {
        FormatKey::Registered(name.to_string())
    }
    pub fn is_std(&self, cf: u32) -> bool {
        matches!(self, FormatKey::Standard(x) if *x == cf)
    }
    /// Case-insensitive name comparison for registered formats.
    pub fn is_named(&self, name: &str) -> bool {
        matches!(self, FormatKey::Registered(n) if n.eq_ignore_ascii_case(name))
    }
    pub fn is_image(&self) -> bool {
        self.is_std(CF_DIB)
            || self.is_std(CF_DIBV5)
            || self.is_std(CF_BITMAP)
            || self.is_std(CF_ENHMETAFILE)
            || self.is_std(CF_METAFILEPICT)
            || self.is_named(FMT_PNG)
    }
    /// Display name for logs and labels (never content).
    pub fn label(&self) -> String {
        match self {
            FormatKey::Standard(id) => match *id {
                CF_TEXT => "CF_TEXT".into(),
                CF_BITMAP => "CF_BITMAP".into(),
                CF_DIB => "CF_DIB".into(),
                CF_UNICODETEXT => "CF_UNICODETEXT".into(),
                CF_ENHMETAFILE => "CF_ENHMETAFILE".into(),
                CF_HDROP => "CF_HDROP".into(),
                CF_DIBV5 => "CF_DIBV5".into(),
                other => format!("CF_{other}"),
            },
            FormatKey::Registered(n) => n.clone(),
        }
    }
}

#[derive(Clone, Debug)]
pub enum Payload {
    Inline(Arc<[u8]>),
    /// Dehydrated: bytes live in `blobs\<sha1-hex>`.
    OnDisk { sha1: [u8; 20], len: u64 },
}

impl Payload {
    pub fn inline(v: Vec<u8>) -> Self {
        Payload::Inline(Arc::from(v.into_boxed_slice()))
    }
    pub fn len(&self) -> u64 {
        match self {
            Payload::Inline(b) => b.len() as u64,
            Payload::OnDisk { len, .. } => *len,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn bytes(&self) -> Option<&[u8]> {
        match self {
            Payload::Inline(b) => Some(b),
            Payload::OnDisk { .. } => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Text,
    Image,
    Files,
    Other,
}

impl Kind {
    pub fn label(self) -> &'static str {
        match self {
            Kind::Text => "Text",
            Kind::Image => "Image",
            Kind::Files => "Files",
            Kind::Other => "Other",
        }
    }
}

/// Source of dehydrated payload bytes. Implemented by the Win32 blob store; tests
/// use an in-memory map. `read_head` returns a cleartext header (DIB headers only)
/// without decrypting the whole file.
pub trait BlobSource {
    fn read_all(&self, sha1: &[u8; 20]) -> Option<Vec<u8>>;
    fn read_head(&self, sha1: &[u8; 20]) -> Option<Vec<u8>>;
}

/// A [`BlobSource`] that has nothing (used when no blob directory is available).
pub struct NoBlobs;
impl BlobSource for NoBlobs {
    fn read_all(&self, _: &[u8; 20]) -> Option<Vec<u8>> {
        None
    }
    fn read_head(&self, _: &[u8; 20]) -> Option<Vec<u8>> {
        None
    }
}

#[derive(Clone)]
pub struct Item {
    pub id: u64,
    /// Milliseconds since the Unix epoch (UTC).
    pub unix_ms: i64,
    pub pinned: bool,
    pub primary: FormatKey,
    pub formats: Vec<(FormatKey, Payload)>,
    /// Up to 300 chars, see spec 7.2. CR/LF/TAB are kept; the renderer flattens them.
    pub preview: String,
    pub kind: Kind,
    pub index: Arc<SearchIndex>,
}

impl std::fmt::Debug for Item {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never print content (spec 19.3).
        f.debug_struct("Item")
            .field("id", &self.id)
            .field("pinned", &self.pinned)
            .field("kind", &self.kind)
            .field("formats", &self.formats.len())
            .finish()
    }
}

/// Primary-format priority, spec 6.4.
const PRIORITY: [u32; 8] = [
    CF_HDROP,
    CF_UNICODETEXT,
    CF_TEXT,
    CF_DIBV5,
    CF_DIB,
    CF_BITMAP,
    CF_ENHMETAFILE,
    CF_METAFILEPICT,
];

pub fn pick_primary(formats: &[(FormatKey, Payload)]) -> FormatKey {
    for cf in PRIORITY {
        if formats.iter().any(|(k, _)| k.is_std(cf)) {
            return FormatKey::Standard(cf);
        }
    }
    for name in [FMT_HTML, FMT_RTF, FMT_PNG] {
        if formats.iter().any(|(k, _)| k.is_named(name)) {
            return FormatKey::reg(name);
        }
    }
    formats
        .first()
        .map(|(k, _)| k.clone())
        .unwrap_or(FormatKey::Standard(0))
}

pub fn kind_of(primary: &FormatKey) -> Kind {
    if primary.is_std(CF_HDROP) {
        Kind::Files
    } else if primary.is_std(CF_UNICODETEXT)
        || primary.is_std(CF_TEXT)
        || primary.is_named(FMT_HTML)
        || primary.is_named(FMT_RTF)
    {
        Kind::Text
    } else if primary.is_image() {
        Kind::Image
    } else {
        Kind::Other
    }
}

impl Item {
    /// Builds an item and derives primary/kind/preview/search index from `formats`.
    /// Payloads that are `OnDisk` are ignored for derivation; callers that need
    /// previews of dehydrated data pass stand-in `Inline` payloads and then overwrite
    /// `item.formats` (see `codec::decode`).
    pub fn new(id: u64, unix_ms: i64, pinned: bool, formats: Vec<(FormatKey, Payload)>) -> Item {
        let primary = pick_primary(&formats);
        let kind = kind_of(&primary);
        let preview = preview::build_preview(&formats, &primary);
        let text = preview::search_text(&formats, &primary);
        let index = Arc::new(SearchIndex::build(&text));
        Item {
            id,
            unix_ms,
            pinned,
            primary,
            formats,
            preview,
            kind,
            index,
        }
    }

    pub fn payload(&self, key: &FormatKey) -> Option<&Payload> {
        self.formats.iter().find(|(k, _)| k == key).map(|(_, p)| p)
    }

    pub fn payload_named(&self, name: &str) -> Option<&Payload> {
        self.formats.iter().find(|(k, _)| k.is_named(name)).map(|(_, p)| p)
    }

    pub fn payload_std(&self, cf: u32) -> Option<&Payload> {
        self.formats.iter().find(|(k, _)| k.is_std(cf)).map(|(_, p)| p)
    }

    /// Best available text (full, not truncated) from inline payloads; spec 7.2 order.
    pub fn text(&self) -> Option<String> {
        preview::best_text(&self.formats)
    }

    pub fn has_text(&self) -> bool {
        self.formats
            .iter()
            .any(|(k, _)| k.is_std(CF_UNICODETEXT) || k.is_std(CF_TEXT))
    }

    /// Every on-disk blob this item references.
    pub fn blob_refs(&self) -> impl Iterator<Item = &[u8; 20]> {
        self.formats.iter().filter_map(|(_, p)| match p {
            Payload::OnDisk { sha1, .. } => Some(sha1),
            _ => None,
        })
    }
}
