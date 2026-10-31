//! SymbolId - High-performance internal symbol identifier
//!
//! Uses SlotMap for O(1) operations with generation counting for dangling detection.

use slotmap::KeyData;
use std::fmt;

/// High-performance internal symbol ID
///
/// # Properties
/// - O(1) comparison and hashing
/// - Generation counter for dangling reference detection
/// - 8 bytes fixed size
///
/// # Important: Always obtain via SymbolRegistry
///
/// SymbolId must be obtained through `SymbolRegistry::register()` or
/// `SymbolRegistry::lookup()`. Direct construction is prohibited.
///
/// ```ignore
/// // Correct usage
/// let id = registry.register(path, kind)?;
/// let id = registry.lookup(&path)?;
///
/// // Prohibited: direct construction
/// // let id = SymbolId::default();  // Compiles but forbidden
/// ```
///
/// # Stability
/// SymbolId はセッション内でのみ安定。サーバー再起動で値が変わるため、
/// セッション跨ぎでのキャッシュや永続化には使用不可。
/// 永続的な参照が必要な場合は UUID (`MatchResult.uuid`) を使用すること。
///
/// # Thread Safety
/// SymbolId is Copy and can be safely shared across threads.
/// However, the SymbolRegistry that owns the symbol data
/// must be properly synchronized.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
#[derive(Default)]
pub struct SymbolId(KeyData);

// SlotMap Key trait implementation
// SAFETY: SymbolId is a newtype wrapper around KeyData with #[repr(transparent)]
unsafe impl slotmap::Key for SymbolId {
    fn data(&self) -> KeyData {
        self.0
    }
}

impl From<KeyData> for SymbolId {
    fn from(k: KeyData) -> Self {
        Self(k)
    }
}

#[cfg(feature = "schemars")]
impl schemars::JsonSchema for SymbolId {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        std::borrow::Cow::Borrowed("SymbolId")
    }

    fn json_schema(generator: &mut schemars::SchemaGenerator) -> schemars::Schema {
        // SymbolId is represented as a string in JSON (e.g., "165v1")
        generator.subschema_for::<String>()
    }
}

impl SymbolId {
    /// Parse SymbolId from string representation.
    ///
    /// Accepts formats:
    /// - `"165v1"` - compact format (index `v` version)
    /// - `"SymbolId(165v1)"` - debug format
    ///
    /// # Examples
    /// ```
    /// # use ryo_symbol::SymbolId;
    /// let id = SymbolId::parse("165v1");
    /// let id = SymbolId::parse("SymbolId(165v1)");
    /// ```
    /// Parse a symbol id string (hir-check POC valid-edit probe).
    pub fn parse(s: &str) -> Option<Self> {
        // Strip "SymbolId(" prefix and ")" suffix if present
        let inner = s
            .strip_prefix("SymbolId(")
            .and_then(|s| s.strip_suffix(')'))
            .unwrap_or(s);

        // Parse "indexVversion" format (e.g., "165v1")
        let (idx_str, ver_str) = inner.split_once('v')?;
        let idx: u32 = idx_str.parse().ok()?;
        let ver: u32 = ver_str.parse().ok()?;

        // SlotMap KeyData uses ffi format: (version << 32) | index
        // But version must be non-zero
        if ver == 0 {
            return None;
        }

        let ffi = ((ver as u64) << 32) | (idx as u64);
        Some(KeyData::from_ffi(ffi).into())
    }

    /// Get the index component of this SymbolId.
    fn index(&self) -> u32 {
        self.0.as_ffi() as u32
    }

    /// Get the version component of this SymbolId.
    fn version(&self) -> u32 {
        (self.0.as_ffi() >> 32) as u32
    }
}

// ============================================================================
// Display/Debug implementations
// ============================================================================

/// Display: compact format "165v1"
///
/// Use this for user-facing output and serialization.
impl fmt::Display for SymbolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}v{}", self.index(), self.version())
    }
}

/// Debug: wrapped format "SymbolId(165v1)"
///
/// Overrides the default SlotMap KeyData debug output for cleaner logs.
impl fmt::Debug for SymbolId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "SymbolId({}v{})", self.index(), self.version())
    }
}

// ============================================================================
// Serialize/Deserialize - TODO: Integrate with persistent UUID keys
// ============================================================================

// TODO: Proper serialization design
//
// ## Problem
// SymbolId is a SlotMap key and is **session-local** (volatile).
// Serializing "165v1" and deserializing in another process is meaningless
// because SlotMap generates new indices/versions per session.
//
// ## Design Requirements
// 1. **Persistent Key**: Assign UUID or stable hash to each symbol
// 2. **Bidirectional Mapping**: SymbolId ↔ UUID in SymbolRegistry
// 3. **Contextual Deserialization**: Deserialize with SymbolRegistry context
//    - Serialize: SymbolId → UUID (via registry.uuid(id))
//    - Deserialize: UUID → SymbolId (via registry.lookup_by_uuid(uuid))
//
// ## Temporary Implementation
// The following impls serialize to "165v1" format for **debugging only**.
// They will fail silently when used across sessions/processes.
// Do NOT rely on these for production persistence.

use serde::{Deserialize, Deserializer, Serialize, Serializer};

impl Serialize for SymbolId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // WARNING: Session-local representation only
        // TODO: Replace with UUID serialization
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for SymbolId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        // WARNING: Session-local representation only
        // TODO: Replace with UUID → SymbolId lookup via registry
        let s = String::deserialize(deserializer)?;
        Self::parse(&s).ok_or_else(|| serde::de::Error::custom(format!("Invalid SymbolId: {}", s)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use slotmap::SlotMap;

    #[test]
    fn test_symbol_id_basic() {
        let mut map: SlotMap<SymbolId, &str> = SlotMap::with_key();

        let id1 = map.insert("foo");
        let id2 = map.insert("bar");

        assert_ne!(id1, id2);
        assert_eq!(map.get(id1), Some(&"foo"));
        assert_eq!(map.get(id2), Some(&"bar"));
    }

    #[test]
    fn test_symbol_id_generation() {
        let mut map: SlotMap<SymbolId, &str> = SlotMap::with_key();

        let id1 = map.insert("foo");
        map.remove(id1);

        // New insert gets a different ID (different generation)
        let id2 = map.insert("bar");
        assert_ne!(id1, id2);

        // Old ID no longer valid
        assert!(map.get(id1).is_none());
    }

    #[test]
    fn test_parse_compact() {
        let id = SymbolId::parse("165v1");
        assert!(id.is_some());
    }

    #[test]
    fn test_parse_debug_format() {
        let id = SymbolId::parse("SymbolId(165v1)");
        assert!(id.is_some());
    }

    #[test]
    fn test_parse_invalid() {
        assert!(SymbolId::parse("").is_none());
        assert!(SymbolId::parse("invalid").is_none());
        assert!(SymbolId::parse("165v0").is_none()); // version 0 is invalid
    }

    #[test]
    fn test_display_format() {
        let mut map: SlotMap<SymbolId, &str> = SlotMap::with_key();
        let id = map.insert("test");
        let display = format!("{}", id);
        // Should be "indexVversion" format
        assert!(display.contains('v'), "Display format should contain 'v'");
        assert!(
            !display.contains("SymbolId"),
            "Display should not contain 'SymbolId'"
        );
    }

    #[test]
    fn test_debug_format() {
        let mut map: SlotMap<SymbolId, &str> = SlotMap::with_key();
        let id = map.insert("test");
        let debug = format!("{:?}", id);
        // Should be "SymbolId(indexVversion)" format
        assert!(
            debug.starts_with("SymbolId("),
            "Debug format should start with 'SymbolId('"
        );
        assert!(debug.ends_with(')'), "Debug format should end with ')'");
    }

    #[test]
    fn test_display_debug_roundtrip() {
        let mut map: SlotMap<SymbolId, &str> = SlotMap::with_key();
        let id = map.insert("test");

        // Display format should be parseable
        let display = format!("{}", id);
        let parsed_from_display = SymbolId::parse(&display);
        assert_eq!(
            Some(id),
            parsed_from_display,
            "Should parse from display format"
        );

        // Debug format should be parseable
        let debug = format!("{:?}", id);
        let parsed_from_debug = SymbolId::parse(&debug);
        assert_eq!(
            Some(id),
            parsed_from_debug,
            "Should parse from debug format"
        );
    }

    #[test]
    fn test_serde_roundtrip() {
        let mut map: SlotMap<SymbolId, &str> = SlotMap::with_key();
        let id = map.insert("test");

        // Serialize
        let json = serde_json::to_string(&id).unwrap();

        // Deserialize
        let deserialized: SymbolId = serde_json::from_str(&json).unwrap();

        // WARNING: This test only works within the same session
        // because SymbolId is session-local (SlotMap key)
        assert_eq!(id, deserialized);
    }
}
