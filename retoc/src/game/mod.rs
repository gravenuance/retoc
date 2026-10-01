//! Game-specific behaviour, selected at runtime. `Standard` is retoc's own behaviour; every other
//! variant reproduces what a game's own tooling produces and keeps its code in a submodule, so the
//! shared conversion and writer code only carries small, marked hook calls.

pub mod marvel_rivals;

use crate::AesKey;
use crate::compression::CompressionMethod;
use crate::iostore_writer::IoStoreWriterOptions;
use crate::legacy_asset::FLegacyPackageHeader;
use crate::logging::Log;
use crate::zen::FBulkDataMapEntry;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum GameVariant {
    /// retoc's own output
    #[default]
    Standard,
    /// Byte-identical to natimerry/retoc-rivals `pack` output, including its MaterialTags patch
    #[value(name = "rivals", alias = "marvel-rivals")]
    MarvelRivals,
}

/// Choices in legacy-to-zen package conversion where a game's reference packer differs from retoc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZenConversionRules {
    /// Package and export names are stored exactly as the legacy package splits them. When false, a
    /// name is formatted to a string and split again, so "SK_1033_1033001" becomes ("SK_1033", 1033002).
    pub exact_names: bool,
    /// The package's own name is added to the name map after every other name, as the editor does.
    /// When false it is added right after the copied names, before imports and exports are converted.
    pub package_name_stored_last: bool,
    /// Imported packages are sorted by package ID, as the editor does. When false they keep first-use order.
    pub sort_imported_packages: bool,
    /// Only an export's dependency on itself with the same command type is dropped, and serialize-on-create
    /// is added only before NoExportInfo. When false, every dependency between an export's own nodes is
    /// dropped and serialize-on-create is always added first.
    pub exact_self_dependencies: bool,
    /// Script imports are kept in dependency bundles. When false only package imports are.
    pub script_import_dependencies: bool,
}

impl GameVariant {
    pub fn zen_conversion_rules(self) -> ZenConversionRules {
        match self {
            GameVariant::Standard => ZenConversionRules {
                exact_names: true,
                package_name_stored_last: true,
                sort_imported_packages: true,
                exact_self_dependencies: true,
                script_import_dependencies: true,
            },
            GameVariant::MarvelRivals => ZenConversionRules {
                exact_names: false,
                package_name_stored_last: false,
                sort_imported_packages: false,
                exact_self_dependencies: false,
                script_import_dependencies: false,
            },
        }
    }

    /// Container writer settings. `compression` of `None` keeps the variant's default; `obfuscate_with`
    /// encrypts every compression block with the key (supported by `MarvelRivals` only).
    pub fn writer_options(self, compression: Option<Option<CompressionMethod>>, obfuscate_with: Option<AesKey>) -> anyhow::Result<IoStoreWriterOptions> {
        match self {
            GameVariant::Standard => {
                anyhow::ensure!(obfuscate_with.is_none(), "obfuscation is only supported with --game rivals");
                let defaults = IoStoreWriterOptions::default();
                Ok(IoStoreWriterOptions {
                    compression: compression.unwrap_or(defaults.compression),
                    ..defaults
                })
            }
            GameVariant::MarvelRivals => Ok(marvel_rivals::writer_options(compression.unwrap_or(Some(CompressionMethod::Oodle)), obfuscate_with)),
        }
    }

    /// AES key used when the user supplies none, for reading inputs and writing the companion pak.
    pub fn default_aes_key(self) -> Option<AesKey> {
        match self {
            GameVariant::Standard => None,
            GameVariant::MarvelRivals => Some(marvel_rivals::aes_key()),
        }
    }

    /// Replaces an import the game does not know with one it does. Returns the script object path to import instead.
    pub fn remap_import(self, package_name: &str, full_import_name: &str) -> Option<&'static str> {
        match self {
            GameVariant::Standard => None,
            GameVariant::MarvelRivals => marvel_rivals::remap_import(package_name, full_import_name),
        }
    }

    /// Game-specific edits to a legacy package before it is converted, after the zen summary is set up.
    /// Returns the patched exports buffer when the exports changed.
    pub fn patch_legacy_package(self, package: &mut FLegacyPackageHeader, exports: &[u8], bulk_data: Option<&[u8]>, zen_bulk_data: &mut Vec<FBulkDataMapEntry>, log: &Log) -> Option<Vec<u8>> {
        match self {
            GameVariant::Standard => None,
            GameVariant::MarvelRivals => {
                marvel_rivals::bulk_map::apply_fallback(package, bulk_data, zen_bulk_data);
                marvel_rivals::material_tags::patch_package(package, exports, log)
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn standard_rules_keep_retoc_fixes_and_rivals_rules_turn_them_off() {
        let standard = GameVariant::Standard.zen_conversion_rules();
        assert!(standard.exact_names && standard.package_name_stored_last && standard.sort_imported_packages && standard.exact_self_dependencies && standard.script_import_dependencies);
        let rivals = GameVariant::MarvelRivals.zen_conversion_rules();
        assert!(!rivals.exact_names && !rivals.package_name_stored_last && !rivals.sort_imported_packages && !rivals.exact_self_dependencies && !rivals.script_import_dependencies);
    }

    #[test]
    fn standard_writer_options_are_the_writer_defaults() {
        let options = GameVariant::Standard.writer_options(None, None).unwrap();
        assert_eq!(options.compression, Some(CompressionMethod::Oodle));
        assert!(matches!(options.oodle_compressor, oodle_loader::Compressor::Mermaid));
        assert!(options.compress_container_header);
        assert!(options.full_chunk_hash);
        assert!(options.mark_compressed_chunks);
        assert!(options.fallback_to_raw_on_compression_error);
        assert!(options.encryption_key.is_none());
    }

    #[test]
    fn rivals_writer_options_match_retoc_rivals() {
        let options = GameVariant::MarvelRivals.writer_options(None, None).unwrap();
        assert_eq!(options.compression, Some(CompressionMethod::Oodle));
        assert!(matches!(options.oodle_compressor, oodle_loader::Compressor::Kraken));
        assert!(!options.compress_container_header);
        assert!(!options.full_chunk_hash);
        assert!(!options.mark_compressed_chunks);
        assert!(!options.fallback_to_raw_on_compression_error);
        assert!(options.encryption_key.is_none());

        let uncompressed = GameVariant::MarvelRivals.writer_options(Some(None), None).unwrap();
        assert_eq!(uncompressed.compression, None);
    }

    #[test]
    fn obfuscation_is_rejected_for_standard() {
        let key = marvel_rivals::aes_key();
        assert!(GameVariant::Standard.writer_options(None, Some(key.clone())).is_err());
        assert!(GameVariant::MarvelRivals.writer_options(None, Some(key)).unwrap().encryption_key.is_some());
    }

    #[test]
    fn default_aes_key_only_for_rivals() {
        assert!(GameVariant::Standard.default_aes_key().is_none());
        assert!(GameVariant::MarvelRivals.default_aes_key().is_some());
    }
}
