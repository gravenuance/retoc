//! Marvel Rivals: reproduces natimerry/retoc-rivals (`retoc-rivals-cli pack`) byte for byte.

pub mod bulk_map;
pub mod material_tags;

use crate::AesKey;
use crate::compression::CompressionMethod;
use crate::iostore_writer::IoStoreWriterOptions;
use anyhow::{Context as _, Result};
use fs_err as fs;
use std::io::BufWriter;
use std::path::Path;

/// The game's public content key, used for its own containers and for mod pak stubs.
const AES_KEY_HEX: &str = "0C263D8C22DCB085894899C3A3796383E9BF9DE0CBFB08C9BF2DEF2E84F29D74";

pub fn aes_key() -> AesKey {
    AES_KEY_HEX.parse().expect("built-in Marvel Rivals AES key is valid hex")
}

/// The pak cipher for a key given as hex (optionally 0x-prefixed) or base64. repak-rivals parses pak
/// keys with every 4-byte word byte-reversed, so its stubs are encrypted with this form, not with the
/// form the IoStore containers use.
fn pak_cipher(key: &str) -> Result<aes::Aes256> {
    use aes::cipher::KeyInit;
    use base64::{Engine as _, engine::general_purpose};
    let mut bytes = hex::decode(key.strip_prefix("0x").unwrap_or(key))
        .ok()
        .filter(|bytes| bytes.len() == 32)
        .or_else(|| general_purpose::STANDARD_NO_PAD.decode(key.trim_end_matches('=')).ok().filter(|bytes| bytes.len() == 32))
        .context("invalid AES key: expected 32 bytes as hex or base64")?;
    bytes.chunks_mut(4).for_each(|word| word.reverse());
    Ok(aes::Aes256::new_from_slice(&bytes).expect("key length was checked above"))
}

pub(super) fn writer_options(compression: Option<CompressionMethod>, obfuscate_with: Option<AesKey>) -> IoStoreWriterOptions {
    IoStoreWriterOptions {
        compression,
        oodle_compressor: oodle_loader::Compressor::Kraken,
        compress_container_header: false,
        full_chunk_hash: false,
        mark_compressed_chunks: false,
        fallback_to_raw_on_compression_error: false,
        encryption_key: obfuscate_with,
    }
}

/// The game has no MaterialTagPlugin or RivalsMeshMaterialManager module, so editor-only carrier
/// classes from those plugins are loaded as the engine's AssetUserData instead.
pub(super) fn remap_import(package_name: &str, full_import_name: &str) -> Option<&'static str> {
    let package_name = package_name.to_ascii_lowercase();
    let full_import_name = full_import_name.to_ascii_lowercase();
    let is_plugin_import = ["/materialtagplugin", "/rivalsmeshmaterialmanager"].iter().any(|plugin| package_name.contains(plugin) || full_import_name.contains(plugin));
    if !is_plugin_import {
        return None;
    }
    Some(if full_import_name.contains("default__") {
        "/Script/Engine.Default__AssetUserData"
    } else if full_import_name.contains("materialtagassetuserdata") || full_import_name.contains("hiddenmaterialsassetuserdata") {
        "/Script/Engine.AssetUserData"
    } else {
        "/Script/Engine"
    })
}

/// Writes the empty companion `.pak` the game needs to mount an IoStore mod: V11, index encrypted
/// with the game key, path hash seed 0. Overwrites an existing file, as retoc-rivals does.
pub fn write_companion_pak(path: &Path, mount_point: &str) -> Result<()> {
    write_pak_stub(path, mount_point, Some(AES_KEY_HEX))
}

/// Writes an empty V11 pak stub with path hash seed 0. With `key` (hex or base64) its index is
/// encrypted exactly as retoc-rivals encrypts it; without one it is plain.
pub fn write_pak_stub(path: &Path, mount_point: &str, key: Option<&str>) -> Result<()> {
    let mut builder = repak::PakBuilder::new();
    if let Some(key) = key {
        builder = builder.key(pak_cipher(key)?).variant(repak::PakVariant::MarvelRivals);
    }
    let file = fs::File::create(path)?;
    builder
        .writer(&mut BufWriter::new(file), repak::Version::V11, mount_point.to_string(), Some(0))
        .write_index()
        .with_context(|| format!("failed to write companion pak {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn plugin_imports_become_engine_asset_user_data() {
        assert_eq!(remap_import("/MaterialTagPlugin", "/MaterialTagPlugin"), Some("/Script/Engine"));
        assert_eq!(remap_import("/Script/MaterialTagPlugin", "/Script/MaterialTagPlugin.MaterialTagAssetUserData"), Some("/Script/Engine.AssetUserData"));
        assert_eq!(remap_import("/Script/MaterialTagPlugin", "/Script/MaterialTagPlugin.Default__MaterialTagAssetUserData"), Some("/Script/Engine.Default__AssetUserData"));
        assert_eq!(remap_import("/Script/RivalsMeshMaterialManager", "/Script/RivalsMeshMaterialManager.HiddenMaterialsAssetUserData"), Some("/Script/Engine.AssetUserData"));
        assert_eq!(remap_import("/Script/RivalsMeshMaterialManager", "/Script/RivalsMeshMaterialManager.SomethingElse"), Some("/Script/Engine"));
    }

    /// retoc-rivals' own stub for mount point "../../../" (retoc-rivals-cli 3.9.2 `pack` output).
    const RETOC_RIVALS_STUB: &[u8] = include_bytes!("../../../tests/marvel_rivals/retoc_rivals_companion_stub.pak");

    fn stub(name: &str, key: Option<&str>) -> Vec<u8> {
        let path = std::env::temp_dir().join(format!("retoc-stub-test-{name}.pak"));
        write_pak_stub(&path, "../../../", key).expect("stub written");
        let bytes = fs::read(&path).expect("stub read");
        fs::remove_file(&path).ok();
        bytes
    }

    #[test]
    fn stub_matches_retoc_rivals_for_every_key_spelling() {
        assert_eq!(stub("hex", Some(AES_KEY_HEX)), RETOC_RIVALS_STUB);
        assert_eq!(stub("hex0x", Some(&format!("0x{AES_KEY_HEX}"))), RETOC_RIVALS_STUB);
        assert_eq!(stub("base64", Some("DCY9jCLcsIWJSJnDo3ljg+m/neDL+wjJvy3vLoTynXQ=")), RETOC_RIVALS_STUB);
    }

    #[test]
    fn stub_without_key_is_plain_and_bad_keys_are_refused() {
        let plain = stub("plain", None);
        assert_ne!(plain, RETOC_RIVALS_STUB);
        assert!(write_pak_stub(&std::env::temp_dir().join("retoc-stub-test-bad.pak"), "../../../", Some("0x1234")).is_err());
    }

    #[test]
    fn other_imports_are_untouched() {
        assert_eq!(remap_import("/Script/Engine", "/Script/Engine.SkeletalMesh"), None);
        assert_eq!(remap_import("/Game/Marvel/Mesh", "/Game/Marvel/Mesh.Mesh"), None);
    }
}
