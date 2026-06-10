//! The `.dpdf` container — a zip of `model.json` + `img/` assets, plus its load path.
//!
//! ## Why a hand-rolled STORE-only zip (no `zip` crate)
//!
//! A `.dpdf` is `model.json` plus a handful of `img/*.png|jpg|svg`. The image bytes are
//! ALREADY compressed (PNG/JPEG), so deflate buys nothing there; `model.json` does compress,
//! but the text-only profile is already a few MB even on a 1,500-page scan (the size story is
//! assets, which the storage modes make the user's explicit choice). Against that, pulling in
//! the `zip` crate (and its deflate/CRC stack, plus a non-trivial transitive tree) to save a
//! STORE-only archive is unjustified weight on a wheel we keep small and pure-Rust.
//!
//! So this writes a minimal **STORE-only** zip by hand: local file headers + a central
//! directory + EOCD, CRC-32 computed inline (~30 lines). It is fully deterministic — entries
//! in a fixed order, no timestamps in the zip records (DOS time fields zeroed), so
//! save → load → save is byte-identical. The format is a strict subset of PKZIP that every
//! unzip tool and `zipfile` reads. (If real compression is ever wanted, `flate2` is already
//! in the tree — add a DEFLATE method then; STORE stays the correct default for assets.)
//!
//! ## Asset storage modes
//!
//! Each [`Asset`] carries a `storage` mode the writer honours:
//! - `embedded` — bytes written into the container under `img/…`.
//! - `external` — bytes written to a sibling directory next to the `.dpdf`, referenced.
//! - `dropped`  — no bytes; only the stub (hash + dims + `regen`) stays in `model.json`. A
//!   named, reversible hole.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

use super::{AssetStorage, DocModel};

/// The JSON member name inside the container.
const MODEL_JSON: &str = "model.json";

/// Asset bytes to write, keyed by the asset id (which doubles as the in-container path, e.g.
/// `img/fig_03.png`). Only `embedded`/`external` assets need an entry; `dropped` assets are
/// stub-only and carry no bytes.
pub(crate) type AssetBytes = BTreeMap<String, Vec<u8>>;

/// Serialize a [`DocModel`] to CANONICAL JSON: pretty-printed (readable as a Tier-1 artifact)
/// with object keys SORTED at every level. serde_json preserves struct field order, but our
/// maps are already `BTreeMap` (sorted) — the one remaining nondeterminism would be float
/// formatting, which serde_json renders deterministically. Sorting is enforced by routing
/// through `serde_json::Value` and a key-sorting serializer so the bytes are stable
/// regardless of struct field declaration order changing in future.
pub(crate) fn to_canonical_json(model: &DocModel) -> Result<Vec<u8>, String> {
    // Round-trip through Value, then serialize with sorted keys. `serde_json::Value`'s Map is
    // a BTreeMap by default (the crate's default feature set), giving sorted keys for free;
    // we assert that by serializing the Value, which iterates the map in key order.
    let value: serde_json::Value = serde_json::to_value(model).map_err(|e| e.to_string())?;
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, serde_json::ser::PrettyFormatter::new());
    use serde::Serialize;
    value.serialize(&mut ser).map_err(|e| e.to_string())?;
    buf.push(b'\n'); // trailing newline — stable, and friendly to text tools
    Ok(buf)
}

/// Save a model to a `.dpdf` file at `path`. `assets` supplies the bytes for any
/// `embedded`/`external` asset; `external_dir`, when given, is where `external` assets are
/// written (defaults to a sibling `<stem>_assets/` next to the `.dpdf`). Returns the written
/// path. Convenience wrapper over [`save_with_members`] with no extra members.
pub(crate) fn save(model: &DocModel, path: &Path, assets: &AssetBytes, external_dir: Option<&Path>) -> Result<(), String> {
    save_with_members(model, path, assets, &AssetBytes::new(), external_dir)
}

/// Save a model PLUS arbitrary verbatim container members (e.g. `embeddings/<id>.bin` vector
/// matrices). `extra_members` are written into the zip byte-for-byte alongside `model.json` and
/// the embedded asset bytes — they are ARTIFACTS the model references but does not derive, so
/// the container carries them losslessly and save→load→save stays byte-identical. Member names
/// must not collide with `model.json` or any embedded asset id.
pub(crate) fn save_with_members(
    model: &DocModel,
    path: &Path,
    assets: &AssetBytes,
    extra_members: &AssetBytes,
    external_dir: Option<&Path>,
) -> Result<(), String> {
    // Index-coverage guard (honest-coverage north star): the stored indexes must equal a fresh
    // derive (no drift) AND every block must be reachable from the page index and from a
    // section or the explicit unsectioned bucket. A violation is a typed save error, never a
    // silently-shipped coverage hole. Cheap (a re-derive + set compares).
    super::validate_indexes(model)?;
    // Embedding-space guard: every declared space must have its member present in
    // `extra_members`, of the exact expected byte length (rows × dim × 4). A space whose vectors
    // are missing or the wrong size is a typed save error, never a silently-shipped half-record.
    validate_embeddings(model, extra_members)?;
    let json = to_canonical_json(model)?;

    // Partition assets by storage mode (the model's asset table is authoritative).
    let mut embedded: Vec<(&str, &[u8])> = Vec::new();
    for a in &model.assets {
        match a.storage {
            AssetStorage::Embedded => {
                if let Some(bytes) = assets.get(&a.id) {
                    embedded.push((a.id.as_str(), bytes.as_slice()));
                }
                // (A missing-bytes embedded asset is a caller error, but we don't fabricate
                // bytes — the stub still records the hole.)
            }
            AssetStorage::External => {
                if let Some(bytes) = assets.get(&a.id) {
                    let dir = match external_dir {
                        Some(d) => d.to_path_buf(),
                        None => sibling_assets_dir(path),
                    };
                    let dest = dir.join(&a.id);
                    if let Some(parent) = dest.parent() {
                        std::fs::create_dir_all(parent).map_err(|e| format!("mkdir {parent:?}: {e}"))?;
                    }
                    std::fs::write(&dest, bytes).map_err(|e| format!("write {dest:?}: {e}"))?;
                }
            }
            AssetStorage::Dropped => {} // stub-only, no bytes
        }
    }
    // Deterministic entry order: model.json first, then every other member (embedded assets +
    // extra members such as embedding bins) sorted by name. Sorting the merged set keeps the
    // archive byte-stable regardless of whether a member is an asset or an artifact.
    let mut rest: Vec<(&str, &[u8])> = embedded;
    for (name, bytes) in extra_members {
        rest.push((name.as_str(), bytes.as_slice()));
    }
    rest.sort_by(|a, b| a.0.cmp(b.0));
    let mut entries: Vec<(&str, &[u8])> = vec![(MODEL_JSON, json.as_slice())];
    entries.extend(rest);

    let zip = write_store_zip(&entries);
    std::fs::write(path, zip).map_err(|e| format!("write {path:?}: {e}"))?;
    Ok(())
}

/// Validate that every declared [`EmbeddingSpace`] has its vector member present and correctly
/// sized: the member bytes must be exactly `chunk_ids.len() * dimension * 4` (little-endian
/// f32, row-major). A missing or mis-sized member is a typed save error — the honest-coverage
/// rule applied to vectors: a space is fully present or it is not written.
fn validate_embeddings(model: &DocModel, members: &AssetBytes) -> Result<(), String> {
    for sp in &model.embedding_spaces {
        let bytes = members.get(&sp.member).ok_or_else(|| {
            format!("embedding space {:?}: member {:?} not provided", sp.id, sp.member)
        })?;
        let expected = sp.chunk_ids.len() * sp.dimension as usize * 4;
        if bytes.len() != expected {
            return Err(format!(
                "embedding space {:?}: member {:?} is {} bytes, expected {} ({} chunks × {} dim × 4)",
                sp.id, sp.member, bytes.len(), expected, sp.chunk_ids.len(), sp.dimension
            ));
        }
    }
    Ok(())
}

/// Default sibling directory for `external` assets: `<dpdf-stem>_assets/` next to the file.
fn sibling_assets_dir(dpdf: &Path) -> std::path::PathBuf {
    let stem = dpdf.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "dpdf".into());
    let parent = dpdf.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}_assets"))
}

/// Load a model from a `.dpdf` file — reads and parses `model.json`. (Asset bytes are read on
/// demand by the accessors; Wave 1's load path returns the model + the in-container asset
/// bytes map.)
pub(crate) fn load(path: &Path) -> Result<(DocModel, AssetBytes), String> {
    let data = std::fs::read(path).map_err(|e| format!("read {path:?}: {e}"))?;
    let members = read_store_zip(&data)?;
    let json = members.get(MODEL_JSON).ok_or_else(|| format!("{MODEL_JSON} missing from container"))?;
    let model: DocModel = serde_json::from_slice(json).map_err(|e| format!("parse {MODEL_JSON}: {e}"))?;
    let mut assets: AssetBytes = BTreeMap::new();
    for (name, bytes) in members {
        if name != MODEL_JSON {
            assets.insert(name, bytes);
        }
    }
    Ok((model, assets))
}

// ---- minimal STORE-only zip --------------------------------------------------

/// CRC-32 (IEEE, the zip variant), computed without a table (compact; the data here is small
/// — model.json + a few images — so the per-byte bit loop is fine and keeps the code tiny).
fn crc32(data: &[u8]) -> u32 {
    let mut crc: u32 = 0xFFFF_FFFF;
    for &b in data {
        crc ^= b as u32;
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xEDB8_8320 & mask);
        }
    }
    !crc
}

/// Write a STORE-only (uncompressed) zip from `(name, bytes)` entries, in the given order.
/// Deterministic: DOS date/time fields are zeroed (no wall-clock), version/flags fixed.
fn write_store_zip(entries: &[(&str, &[u8])]) -> Vec<u8> {
    let mut out = Vec::new();
    // (offset_of_local_header, name, crc, size) for the central directory.
    let mut dir: Vec<(u32, &str, u32, u32)> = Vec::new();

    for &(name, data) in entries {
        let offset = out.len() as u32;
        let crc = crc32(data);
        let size = data.len() as u32;
        let nb = name.as_bytes();
        // Local file header (signature 0x04034b50).
        out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // general purpose flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method 0 = STORE
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time (zeroed → deterministic)
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date (zeroed → deterministic)
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes()); // compressed size == size (STORE)
        out.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra field length
        out.extend_from_slice(nb);
        out.extend_from_slice(data);
        dir.push((offset, name, crc, size));
    }

    let cd_start = out.len() as u32;
    for &(offset, name, crc, size) in &dir {
        let nb = name.as_bytes();
        // Central directory header (signature 0x02014b50).
        out.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
        out.extend_from_slice(&20u16.to_le_bytes()); // version made by
        out.extend_from_slice(&20u16.to_le_bytes()); // version needed
        out.extend_from_slice(&0u16.to_le_bytes()); // flags
        out.extend_from_slice(&0u16.to_le_bytes()); // method STORE
        out.extend_from_slice(&0u16.to_le_bytes()); // mod time
        out.extend_from_slice(&0u16.to_le_bytes()); // mod date
        out.extend_from_slice(&crc.to_le_bytes());
        out.extend_from_slice(&size.to_le_bytes()); // compressed size
        out.extend_from_slice(&size.to_le_bytes()); // uncompressed size
        out.extend_from_slice(&(nb.len() as u16).to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes()); // extra len
        out.extend_from_slice(&0u16.to_le_bytes()); // comment len
        out.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        out.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        out.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        out.extend_from_slice(&offset.to_le_bytes()); // local header offset
        out.extend_from_slice(nb);
    }
    let cd_size = out.len() as u32 - cd_start;

    // End of central directory (signature 0x06054b50).
    out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // disk number
    out.extend_from_slice(&0u16.to_le_bytes()); // cd start disk
    out.extend_from_slice(&(dir.len() as u16).to_le_bytes()); // entries this disk
    out.extend_from_slice(&(dir.len() as u16).to_le_bytes()); // total entries
    out.extend_from_slice(&cd_size.to_le_bytes());
    out.extend_from_slice(&cd_start.to_le_bytes());
    out.extend_from_slice(&0u16.to_le_bytes()); // comment len
    let _ = out.flush();
    out
}

/// Read a STORE-only zip's members from the LOCAL file headers (sufficient for our own
/// archives — we never compress, never use data descriptors). Returns `{name: bytes}`.
/// Rejects any compressed entry (method != 0) loudly rather than returning garbage.
fn read_store_zip(data: &[u8]) -> Result<BTreeMap<String, Vec<u8>>, String> {
    let mut out = BTreeMap::new();
    let mut i = 0usize;
    while i + 4 <= data.len() {
        let sig = u32::from_le_bytes([data[i], data[i + 1], data[i + 2], data[i + 3]]);
        if sig != 0x0403_4b50 {
            break; // reached the central directory / EOCD
        }
        if i + 30 > data.len() {
            return Err("truncated local header".into());
        }
        let rd16 = |off: usize| u16::from_le_bytes([data[i + off], data[i + off + 1]]) as usize;
        let rd32 = |off: usize| u32::from_le_bytes([data[i + off], data[i + off + 1], data[i + off + 2], data[i + off + 3]]) as usize;
        let method = rd16(8);
        let comp_size = rd32(18);
        let name_len = rd16(26);
        let extra_len = rd16(28);
        let name_start = i + 30;
        let data_start = name_start + name_len + extra_len;
        if data_start + comp_size > data.len() {
            return Err("truncated entry data".into());
        }
        if method != 0 {
            return Err(format!("unsupported zip method {method} (only STORE is supported)"));
        }
        let name = String::from_utf8_lossy(&data[name_start..name_start + name_len]).into_owned();
        let bytes = data[data_start..data_start + comp_size].to_vec();
        out.insert(name, bytes);
        i = data_start + comp_size;
    }
    if out.is_empty() {
        return Err("no zip entries found".into());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::*;
    use super::*;

    fn tiny_model() -> DocModel {
        let blocks = vec![Block {
            id: "b0001".into(),
            kind: BlockKind::Para,
            text: "hello".into(),
            page: 1,
            section: None,
            bbox: None,
            confidence: NATIVE_CONFIDENCE,
            ocr_pass: None,
            heading_level: None,
            cells: None,
            image: None,
            label: None,
            caption: None,
            list_ordered: None,
            el_group: None,
            table_header: None,
            table_grid: None,
            table_caption: None,
            el_html: None,
        }];
        let indexes = derive_indexes(&blocks);
        DocModel {
            schema_version: SCHEMA_VERSION,
            source: Source {
                file: "x.pdf".into(),
                sha256: "ab".into(),
                pages: 1,
                distillpdf: "0.0.0".into(),
                generated_at: "2026-06-10T00:00:00Z".into(),
            },
            metadata: Metadata::default(),
            pages: vec![Page { n: 1, width_pts: 612.0, height_pts: 792.0, labels: BTreeMap::new(), ocr_decision: None, active_ocr_pass: None }],
            ocr_passes: Vec::new(),
            sections: Vec::new(),
            blocks,
            indexes,
            assets: Vec::new(),
            chunks: None,
            embedding_spaces: Vec::new(),
            links: Vec::new(),
            named_dests: Vec::new(),
            toc: Vec::new(),
        }
    }

    #[test]
    fn crc32_matches_known_vector() {
        // CRC-32 of "123456789" is the standard 0xCBF43926.
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn zip_roundtrip_and_determinism() {
        let entries: Vec<(&str, &[u8])> = vec![("model.json", b"{}" as &[u8]), ("img/a.png", b"\x89PNG")];
        let a = write_store_zip(&entries);
        let b = write_store_zip(&entries);
        assert_eq!(a, b, "zip writer must be deterministic");
        let members = read_store_zip(&a).unwrap();
        assert_eq!(members.get("model.json").unwrap().as_slice(), b"{}");
        assert_eq!(members.get("img/a.png").unwrap().as_slice(), b"\x89PNG");
    }

    #[test]
    fn save_load_is_byte_identical() {
        let dir = std::env::temp_dir().join(format!("dpdf_test_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.dpdf");
        let model = tiny_model();
        save(&model, &path, &AssetBytes::new(), None).unwrap();
        let first = std::fs::read(&path).unwrap();
        let (loaded, _assets) = load(&path).unwrap();
        assert_eq!(loaded, model, "model must round-trip through the container");
        // save → load → save is byte-identical.
        save(&loaded, &path, &AssetBytes::new(), None).unwrap();
        let second = std::fs::read(&path).unwrap();
        assert_eq!(first, second, "save→load→save must be byte-identical");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn dropped_asset_keeps_stub_with_hash_and_dims() {
        let mut model = tiny_model();
        model.assets.push(Asset {
            id: "img/fig_01.png".into(),
            kind: AssetKind::Figure,
            storage: AssetStorage::Dropped,
            sha256: Some("deadbeef".into()),
            bytes: Some(1234),
            width: Some(640),
            height: Some(480),
            regen: Some(Regen { page: 1, dpi: Some(300) }),
        });
        model.indexes = derive_indexes(&model.blocks);
        let dir = std::env::temp_dir().join(format!("dpdf_drop_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.dpdf");
        save(&model, &path, &AssetBytes::new(), None).unwrap();
        let (loaded, assets) = load(&path).unwrap();
        // No bytes in the container for a dropped asset, but the stub survives intact.
        assert!(assets.is_empty(), "dropped asset must not write bytes into the container");
        let a = &loaded.assets[0];
        assert_eq!(a.storage, AssetStorage::Dropped);
        assert_eq!(a.sha256.as_deref(), Some("deadbeef"));
        assert_eq!(a.width, Some(640));
        assert_eq!(a.regen.as_ref().unwrap().dpi, Some(300));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_rejects_drifted_indexes() {
        // The save-time guard: if a block is added without re-deriving indexes, save errors
        // (no silent coverage hole). Hand-build a model whose blocks and indexes disagree.
        let mut model = tiny_model();
        model.blocks.push(Block {
            id: "b0002".into(),
            kind: BlockKind::Para,
            text: "orphan".into(),
            page: 1,
            section: None,
            bbox: None,
            confidence: NATIVE_CONFIDENCE,
            ocr_pass: None,
            heading_level: None,
            cells: None,
            image: None,
            label: None,
            caption: None,
            list_ordered: None,
            el_group: None,
            table_header: None,
            table_grid: None,
            table_caption: None,
            el_html: None,
        });
        // (indexes still reflect only b0001 — drift)
        let dir = std::env::temp_dir().join(format!("dpdf_drift_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.dpdf");
        let err = save(&model, &path, &AssetBytes::new(), None).unwrap_err();
        assert!(err.contains("drift"), "expected a drift error, got {err:?}");
        // reindex repairs it; save then succeeds and validates clean.
        super::super::reindex(&mut model);
        save(&model, &path, &AssetBytes::new(), None).unwrap();
        std::fs::remove_dir_all(&dir).ok();
    }

    fn f32_le_bytes(rows: &[Vec<f32>]) -> Vec<u8> {
        let mut out = Vec::new();
        for row in rows {
            for &v in row {
                out.extend_from_slice(&v.to_le_bytes());
            }
        }
        out
    }

    #[test]
    fn embedding_member_round_trips_and_stays_byte_identical() {
        // A model with one embedding space + its f32 matrix as a verbatim container member must
        // round-trip AND keep save→load→save byte-identity (the artifact-carry invariant).
        let mut model = tiny_model();
        let vectors = vec![vec![0.1f32, 0.2, 0.3, 0.4]];
        let member = "embeddings/e1.bin".to_string();
        model.embedding_spaces.push(EmbeddingSpace {
            id: "e1".into(),
            model: "BAAI/bge-m3".into(),
            dimension: 4,
            normalized: true,
            member: member.clone(),
            chunk_ids: vec!["c0001".into()],
            generated_at: "2026-06-10T00:00:00Z".into(),
            distillpdf_version: "0.0.0".into(),
        });
        let mut extras = AssetBytes::new();
        extras.insert(member.clone(), f32_le_bytes(&vectors));
        let dir = std::env::temp_dir().join(format!("dpdf_emb_space_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.dpdf");
        save_with_members(&model, &path, &AssetBytes::new(), &extras, None).unwrap();
        let first = std::fs::read(&path).unwrap();
        let (loaded, members) = load(&path).unwrap();
        assert_eq!(loaded.embedding_spaces[0].member, member);
        assert_eq!(members.get(&member).unwrap(), &f32_le_bytes(&vectors));
        // The space-carrying load splits members from assets at the Python boundary; here we
        // confirm a re-save with the same members is byte-identical.
        save_with_members(&loaded, &path, &AssetBytes::new(), &members, None).unwrap();
        let second = std::fs::read(&path).unwrap();
        assert_eq!(first, second, "save→load→save with embeddings must be byte-identical");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn save_rejects_missing_or_missized_embedding_member() {
        let mut model = tiny_model();
        model.embedding_spaces.push(EmbeddingSpace {
            id: "e1".into(),
            model: "BAAI/bge-m3".into(),
            dimension: 4,
            normalized: true,
            member: "embeddings/e1.bin".into(),
            chunk_ids: vec!["c0001".into()],
            generated_at: "t".into(),
            distillpdf_version: "0".into(),
        });
        let dir = std::env::temp_dir().join(format!("dpdf_emb_bad_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.dpdf");
        // Missing member: loud error.
        let err = save_with_members(&model, &path, &AssetBytes::new(), &AssetBytes::new(), None).unwrap_err();
        assert!(err.contains("not provided"), "expected missing-member error, got {err:?}");
        // Wrong size (3 floats, expected 4): loud error.
        let mut extras = AssetBytes::new();
        extras.insert("embeddings/e1.bin".into(), f32_le_bytes(&[vec![0.0, 0.0, 0.0]]));
        let err = save_with_members(&model, &path, &AssetBytes::new(), &extras, None).unwrap_err();
        assert!(err.contains("expected"), "expected size-mismatch error, got {err:?}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn embedded_asset_bytes_round_trip() {
        let mut model = tiny_model();
        model.assets.push(Asset {
            id: "img/fig_01.png".into(),
            kind: AssetKind::Figure,
            storage: AssetStorage::Embedded,
            sha256: None,
            bytes: Some(4),
            width: None,
            height: None,
            regen: None,
        });
        model.indexes = derive_indexes(&model.blocks);
        let mut bytes = AssetBytes::new();
        bytes.insert("img/fig_01.png".into(), b"\x89PNG".to_vec());
        let dir = std::env::temp_dir().join(format!("dpdf_emb_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("m.dpdf");
        save(&model, &path, &bytes, None).unwrap();
        let (_loaded, assets) = load(&path).unwrap();
        assert_eq!(assets.get("img/fig_01.png").unwrap().as_slice(), b"\x89PNG");
        std::fs::remove_dir_all(&dir).ok();
    }
}
