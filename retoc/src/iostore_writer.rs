use crate::{
    AesKey, EIoChunkType, EIoContainerFlags, FPackageId, UEPath, UEPathBuf, align_usize,
    chunk_id::FIoChunkIdRaw,
    compression::{CompressionMethod, compress_with_oodle_compressor},
    container_header::{EIoContainerHeaderVersion, FIoContainerHeader, StoreEntry},
};
use crate::{EIoStoreTocVersion, FIoChunkHash, FIoChunkId, FIoContainerId, FIoOffsetAndLength, FIoStoreTocCompressedBlockEntry, FIoStoreTocEntryMeta, FIoStoreTocEntryMetaFlags, Toc, ser::*};
use anyhow::{Context, Result};
use fs_err as fs;
use std::io::Cursor;
use std::{
    io::{BufWriter, Seek, Write},
    path::{Path, PathBuf},
};

/// How chunks are stored. `Default` is retoc's own output; `GameVariant::writer_options` gives a game's.
#[derive(Debug, Clone)]
pub struct IoStoreWriterOptions {
    /// Block compression. A block that does not shrink is stored raw.
    pub compression: Option<CompressionMethod>,
    pub oodle_compressor: oodle_loader::Compressor,
    pub compress_container_header: bool,
    /// Store the whole 32-byte blake3 digest; otherwise the first 20 bytes, zero-padded.
    pub full_chunk_hash: bool,
    /// Set the Compressed meta flag on chunks that have at least one compressed block.
    pub mark_compressed_chunks: bool,
    /// Store a block raw when the compressor fails, instead of failing the write.
    pub fallback_to_raw_on_compression_error: bool,
    /// Encrypt every block, zero-padded to 16 bytes, and set the Encrypted container flag. The directory
    /// index is written in plaintext, as retoc-rivals does.
    pub encryption_key: Option<AesKey>,
}
impl Default for IoStoreWriterOptions {
    fn default() -> Self {
        Self {
            compression: Some(CompressionMethod::Oodle),
            oodle_compressor: oodle_loader::Compressor::Mermaid,
            compress_container_header: true,
            full_chunk_hash: true,
            mark_compressed_chunks: true,
            fallback_to_raw_on_compression_error: true,
            encryption_key: None,
        }
    }
}

pub struct IoStoreWriter {
    #[allow(unused)]
    toc_path: PathBuf,
    toc_stream: BufWriter<fs::File>,
    cas_stream: BufWriter<fs::File>,
    toc: Toc,
    container_header: Option<FIoContainerHeader>,
    options: IoStoreWriterOptions,
}

impl IoStoreWriter {
    pub fn new<P: AsRef<Path>>(toc_path: P, toc_version: EIoStoreTocVersion, container_header_version: Option<EIoContainerHeaderVersion>, mount_point: UEPathBuf) -> Result<Self> {
        Self::with_options(toc_path, toc_version, container_header_version, mount_point, IoStoreWriterOptions::default())
    }
    pub fn with_options<P: AsRef<Path>>(toc_path: P, toc_version: EIoStoreTocVersion, container_header_version: Option<EIoContainerHeaderVersion>, mount_point: UEPathBuf, options: IoStoreWriterOptions) -> Result<Self> {
        let toc_path = toc_path.as_ref().to_path_buf();
        let name = toc_path.file_stem().unwrap().to_string_lossy();
        let toc_stream = BufWriter::new(fs::File::create(&toc_path)?);
        let cas_stream = BufWriter::new(fs::File::create(toc_path.with_extension("ucas"))?);

        let mut toc = Toc::new();
        // Real containers (both the base game's own and working third-party mods) use 128KiB
        // blocks - confirmed by diffing a real container's TOC header against ours byte-for-byte.
        toc.compression_block_size = 0x20000;
        toc.version = toc_version;
        toc.container_id = FIoContainerId::from_name(&name);
        toc.directory_index.mount_point = mount_point;
        toc.partition_size = u64::MAX;
        if options.encryption_key.is_some() {
            toc.container_flags |= EIoContainerFlags::Encrypted;
        }

        let container_header = container_header_version.map(|v| FIoContainerHeader::new(v, toc.container_id));

        Ok(Self {
            toc_path,
            toc_stream,
            cas_stream,
            toc,
            container_header,
            options,
        })
    }
    pub fn write_chunk_raw(&mut self, chunk_id_raw: FIoChunkIdRaw, path: Option<&UEPath>, data: &[u8]) -> Result<()> {
        self.write_chunk(FIoChunkId::from_raw(chunk_id_raw, self.toc.version), path, data)
    }
    pub fn write_chunk(&mut self, chunk_id: FIoChunkId, path: Option<&UEPath>, data: &[u8]) -> Result<()> {
        self.write_chunk_with_compression(chunk_id, path, data, self.options.compression)
    }
    fn write_chunk_with_compression(&mut self, chunk_id: FIoChunkId, path: Option<&UEPath>, data: &[u8], compression: Option<CompressionMethod>) -> Result<()> {
        if let Some(path) = path {
            let index = &mut self.toc.directory_index;
            let relative_path = path.strip_prefix(&index.mount_point).with_context(|| format!("mount point {} does not contain path {path}", index.mount_point))?;
            index.add_file(relative_path, self.toc.chunks.len() as u32);
        }

        // Registered with the first chunk, so a container without chunks lists no method.
        if let Some(method) = self.options.compression
            && self.toc.compression_methods.is_empty()
        {
            self.toc.compression_methods.push(method);
        }

        let mut offset = self.cas_stream.stream_position()?;

        let start_block = self.toc.compression_blocks.len();

        let mut hasher = blake3::Hasher::new();
        let mut any_block_compressed = false;
        let mut compress_buf = Vec::new();
        let mut encrypt_buf = Vec::new();
        for block in data.chunks(self.toc.compression_block_size as usize) {
            hasher.update(block);
            let uncompressed_size = block.len() as u32;

            compress_buf.clear();
            let compressed = match compression {
                Some(method) => self.compress_block(method, block, &mut compress_buf)?,
                None => false,
            };
            let (bytes_to_write, compression_method_index) = if compressed && compress_buf.len() < block.len() {
                any_block_compressed = true;
                (compress_buf.as_slice(), 1u8) // index into toc.compression_methods (1-based; 0 is "None")
            } else {
                (block, 0u8)
            };
            // The recorded size stays unpadded; readers round it up to the AES block size.
            let compressed_size = bytes_to_write.len() as u32;
            let bytes_to_write = match &self.options.encryption_key {
                Some(key) => {
                    encrypt_padded(key, bytes_to_write, &mut encrypt_buf);
                    encrypt_buf.as_slice()
                }
                None => bytes_to_write,
            };

            self.cas_stream.write_all(bytes_to_write)?;
            self.toc.compression_blocks.push(FIoStoreTocCompressedBlockEntry::new(offset, compressed_size, uncompressed_size, compression_method_index));
            offset += bytes_to_write.len() as u64;
        }
        let hash = hasher.finalize();
        let mut chunk_hash = FIoChunkHash::from_blake3(hash.as_bytes());
        if !self.options.full_chunk_hash {
            chunk_hash.0[20..].fill(0);
        }
        let mut flags = FIoStoreTocEntryMetaFlags::empty();
        if any_block_compressed && self.options.mark_compressed_chunks {
            flags |= FIoStoreTocEntryMetaFlags::Compressed;
        }
        let meta = FIoStoreTocEntryMeta { chunk_hash, flags };

        let offset_and_length = FIoOffsetAndLength::new(start_block as u64 * self.toc.compression_block_size as u64, data.len() as u64);

        self.toc.chunks.push(chunk_id.with_version(self.toc.version));
        self.toc.chunk_offset_lengths.push(offset_and_length);
        self.toc.chunk_metas.push(meta);

        Ok(())
    }

    /// Compresses `block` into `out`. Returns false when the block has to be stored raw.
    fn compress_block(&self, method: CompressionMethod, block: &[u8], out: &mut Vec<u8>) -> Result<bool> {
        match compress_with_oodle_compressor(method, self.options.oodle_compressor, block, &mut *out) {
            Ok(()) => Ok(true),
            Err(_) if self.options.fallback_to_raw_on_compression_error => Ok(false),
            Err(err) => Err(err.context(format!("failed to compress a block with {method:?}"))),
        }
    }

    pub fn write_package_chunk(&mut self, chunk_id: FIoChunkId, path: Option<&UEPath>, data: &[u8], store_entry: &StoreEntry) -> Result<()> {
        let container_header = self.container_header.as_mut().expect("FIoContainerHeader is required to write package chunks");
        container_header.add_package(FPackageId(chunk_id.get_chunk_id()), store_entry.clone());
        self.write_chunk(chunk_id, path, data)
    }
    pub fn add_localized_package(&mut self, package_culture: &str, source_package_name: &str, localized_package_id: FPackageId) -> Result<()> {
        let container_header = self.container_header.as_mut().expect("FIoContainerHeader is required to add localized packages");
        container_header.add_localized_package(package_culture, source_package_name, localized_package_id)
    }
    pub fn add_package_redirect(&mut self, source_package_name: &str, redirect_package_id: FPackageId) -> Result<()> {
        let container_header = self.container_header.as_mut().expect("FIoContainerHeader is required to add package redirects");
        container_header.add_package_redirect(source_package_name, redirect_package_id)
    }
    pub fn container_version(&self) -> EIoStoreTocVersion {
        self.toc.version
    }
    pub fn container_header_version(&self) -> EIoContainerHeaderVersion {
        self.container_header.as_ref().unwrap().version
    }
    pub fn finalize(mut self) -> Result<()> {
        if let Some(container_header) = &self.container_header {
            let mut chunk_buffer = vec![];
            container_header.serialize(&mut Cursor::new(&mut chunk_buffer))?;
            // container header is always aligned for AES for some reason
            chunk_buffer.resize(align_usize(chunk_buffer.len(), 16), 0);

            let chunk_id = FIoChunkId::create(container_header.container_id.0, 0, EIoChunkType::ContainerHeader);
            let compression = if self.options.compress_container_header { self.options.compression } else { None };
            self.write_chunk_with_compression(chunk_id, None, &chunk_buffer, compression)?;
        }
        self.toc_stream.ser(&self.toc)?;
        self.cas_stream.flush()?;
        self.toc_stream.flush()?;
        Ok(())
    }
}

/// AES-256-ECB over `data` zero-padded to a multiple of 16 bytes, written to `out`.
fn encrypt_padded(key: &AesKey, data: &[u8], out: &mut Vec<u8>) {
    use aes::cipher::BlockEncrypt;
    out.clear();
    out.extend_from_slice(data);
    out.resize(align_usize(data.len(), 16), 0);
    for block in out.chunks_mut(16) {
        key.0.encrypt_block(block.into());
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fs_err as fs;

    #[test]
    fn test_write_container() -> Result<()> {
        fs::create_dir("out").ok();
        let mut writer = IoStoreWriter::new("out/new.utoc", EIoStoreTocVersion::PerfectHashWithOverflow, Some(EIoContainerHeaderVersion::OptionalSegmentPackages), "../../..".into())?;

        let data = fs::read("tests/UE5.3/ScriptObjects.bin")?;
        writer.write_chunk_raw(FIoChunkIdRaw { id: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5] }, Some(UEPath::new("../../../asdf/asdf/dasf/script_objects.bin")), &data)?;
        writer.finalize()?;
        Ok(())
    }

    const TEST_KEY: &str = "0x000102030405060708090A0B0C0D0E0F101112131415161718191A1B1C1D1E1F";
    const MOUNT_POINT: &str = "../../../";

    /// A fresh directory per test, so tests can run in parallel.
    fn test_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("retoc-writer-test-{name}"));
        fs::remove_dir_all(&dir).ok();
        fs::create_dir_all(&dir).expect("create test dir");
        dir
    }

    fn chunk_id(index: u8) -> FIoChunkIdRaw {
        FIoChunkIdRaw { id: [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, index, 5] }
    }

    /// Writes one container holding `chunks` (relative path, data) and returns its .utoc path.
    fn write_container(name: &str, options: IoStoreWriterOptions, header: Option<EIoContainerHeaderVersion>, chunks: &[(&str, &[u8])]) -> Result<PathBuf> {
        let toc_path = test_dir(name).join(format!("{name}.utoc"));
        let mut writer = IoStoreWriter::with_options(&toc_path, EIoStoreTocVersion::PerfectHashWithOverflow, header, MOUNT_POINT.into(), options)?;
        for (index, (path, data)) in chunks.iter().enumerate() {
            writer.write_chunk_raw(chunk_id(index as u8), Some(UEPath::new(&format!("{MOUNT_POINT}{path}"))), data)?;
        }
        writer.finalize()?;
        Ok(toc_path)
    }

    fn read_toc(toc_path: &Path, key: Option<&str>) -> Result<Toc> {
        let mut config = crate::Config::default();
        if let Some(key) = key {
            config.aes_keys.insert(Default::default(), key.parse()?);
        }
        std::io::BufReader::new(fs::File::open(toc_path)?).de_ctx(std::sync::Arc::new(config))
    }

    fn read_header(toc_path: &Path) -> Result<crate::FIoStoreTocHeader> {
        std::io::BufReader::new(fs::File::open(toc_path)?).de()
    }

    fn uncompressed() -> IoStoreWriterOptions {
        IoStoreWriterOptions { compression: None, ..Default::default() }
    }

    #[test]
    fn chunk_hash_is_full_blake3_or_truncated_to_20_bytes() -> Result<()> {
        // blake3("abc")
        let digest = hex::decode("6437b3ac38465133ffb63b75273a8db548c558465d79db03fd359c6cd5bd9d85")?;

        let full = read_toc(&write_container("hash_full", uncompressed(), None, &[("a.bin", b"abc")])?, None)?;
        assert_eq!(full.chunk_metas[0].chunk_hash.0.as_slice(), digest.as_slice());

        let options = IoStoreWriterOptions { full_chunk_hash: false, ..uncompressed() };
        let truncated = read_toc(&write_container("hash_truncated", options, None, &[("a.bin", b"abc")])?, None)?;
        assert_eq!(&truncated.chunk_metas[0].chunk_hash.0[..20], &digest[..20]);
        assert_eq!(truncated.chunk_metas[0].chunk_hash.0[20..], [0; 12]);
        Ok(())
    }

    #[test]
    fn compressed_meta_flag_follows_the_option() -> Result<()> {
        let data = [0u8; 4096];
        for (mark, name) in [(true, "flag_on"), (false, "flag_off")] {
            let options = IoStoreWriterOptions {
                compression: Some(CompressionMethod::Zlib),
                mark_compressed_chunks: mark,
                ..Default::default()
            };
            let toc = read_toc(&write_container(name, options, None, &[("a.bin", &data)])?, None)?;
            assert_eq!(toc.compression_methods, vec![CompressionMethod::Zlib]);
            assert_eq!(toc.compression_blocks[0].get_compression_method_index(), 1, "the block itself is compressed either way");
            assert_eq!(toc.chunk_metas[0].flags.contains(FIoStoreTocEntryMetaFlags::Compressed), mark);
        }
        Ok(())
    }

    #[test]
    fn container_header_compression_follows_the_option() -> Result<()> {
        let header = Some(EIoContainerHeaderVersion::OptionalSegmentPackages);
        for (compress_header, expected_method, name) in [(true, 1, "header_compressed"), (false, 0, "header_raw")] {
            let options = IoStoreWriterOptions {
                compression: Some(CompressionMethod::Zlib),
                compress_container_header: compress_header,
                ..Default::default()
            };
            let toc = read_toc(&write_container(name, options, header, &[("a.bin", &[0u8; 4096])])?, None)?;
            let header_index = toc.chunks.iter().position(|c| c.get_chunk_type() == EIoChunkType::ContainerHeader).expect("container header chunk");
            let header_block = toc.chunk_offset_lengths[header_index].get_offset() / u64::from(toc.compression_block_size);
            assert_eq!(toc.compression_blocks[header_block as usize].get_compression_method_index(), expected_method);
            assert_eq!(toc.compression_blocks[0].get_compression_method_index(), 1, "data chunks stay compressed");
        }
        Ok(())
    }

    #[test]
    fn encrypted_blocks_are_padded_and_record_the_unpadded_size() -> Result<()> {
        use aes::cipher::BlockDecrypt;
        let options = IoStoreWriterOptions {
            encryption_key: Some(TEST_KEY.parse()?),
            ..uncompressed()
        };
        let toc_path = write_container("encrypted", options, None, &[("dir/a.bin", b"hello")])?;

        let header = read_header(&toc_path)?;
        assert!(header.container_flags.contains(EIoContainerFlags::Encrypted));
        let mut cas = fs::read(toc_path.with_extension("ucas"))?;
        assert_eq!(cas.len(), 16, "5 bytes padded to one AES block");
        let key: AesKey = TEST_KEY.parse()?;
        key.0.decrypt_block(cas.as_mut_slice().into());
        assert_eq!(cas, [b"hello".as_slice(), &[0; 11]].concat());

        let toc = read_toc(&toc_path, Some(TEST_KEY))?;
        assert_eq!(toc.compression_blocks[0].get_compressed_size(), 5);
        assert_eq!(toc.compression_blocks[0].get_uncompressed_size(), 5);
        Ok(())
    }

    #[test]
    fn only_a_plaintext_index_is_read_without_decrypting() -> Result<()> {
        use aes::cipher::BlockEncrypt;
        let mut index = crate::FIoDirectoryIndexResource::default();
        index.mount_point = MOUNT_POINT.into();
        index.add_file(UEPath::new("dir/a.bin"), 0);
        let mut plain = vec![];
        index.ser(&mut std::io::Cursor::new(&mut plain))?;
        assert!(crate::looks_like_plaintext_index(&plain));

        let key: AesKey = TEST_KEY.parse()?;
        let mut encrypted = plain.clone();
        encrypted.resize(align_usize(plain.len(), 16), 0);
        encrypted.chunks_mut(16).for_each(|block| key.0.encrypt_block(block.into()));
        assert!(!crate::looks_like_plaintext_index(&encrypted));

        // 10 chars "../../../" + NUL
        let fstring = |len: i32, text: &[u8]| [len.to_le_bytes().as_slice(), text].concat();
        assert!(crate::looks_like_plaintext_index(&fstring(10, b"../../../\0")));
        assert!(crate::looks_like_plaintext_index(&fstring(-3, &[b'a', 0, b'b', 0, 0, 0])), "UTF-16 mount point");
        for (case, bytes) in [
            ("empty", vec![]),
            ("zero length", fstring(0, b"")),
            ("length past the end", fstring(11, b"../../../\0")),
            ("no terminator", fstring(9, b"../../../")),
            ("control character", fstring(3, b"a\x01\0")),
            ("i32::MIN length", fstring(i32::MIN, b"ab")),
            ("truncated length", vec![10, 0]),
        ] {
            assert!(!crate::looks_like_plaintext_index(&bytes), "{case}");
        }
        Ok(())
    }

    /// The writer leaves the index in plaintext; the reader must accept it whether or not its length
    /// happens to be a multiple of the AES block size.
    #[test]
    fn plaintext_index_of_encrypted_container_reads_back() -> Result<()> {
        let mut seen_aligned = false;
        let mut seen_unaligned = false;
        for name_len in 1..=16 {
            let path = format!("dir/{}.bin", "x".repeat(name_len));
            let options = IoStoreWriterOptions {
                encryption_key: Some(TEST_KEY.parse()?),
                ..uncompressed()
            };
            let toc_path = write_container(&format!("index_{name_len}"), options, None, &[(&path, b"hello")])?;

            let aligned = read_header(&toc_path)?.directory_index_size % 16 == 0;
            seen_aligned |= aligned;
            seen_unaligned |= !aligned;
            let utoc = fs::read(&toc_path)?;
            assert!(utoc.windows(name_len + 4).any(|w| w == format!("{}.bin", "x".repeat(name_len)).as_bytes()), "index is stored in plaintext");

            let container = crate::iostore::IoStoreContainer::open(
                &toc_path,
                std::sync::Arc::new({
                    let mut config = crate::Config::default();
                    config.aes_keys.insert(Default::default(), TEST_KEY.parse()?);
                    config
                }),
            )?;
            use crate::iostore::IoStoreTrait;
            let chunk = container.chunks().next().expect("one chunk");
            assert_eq!(chunk.path().as_deref(), Some(format!("{MOUNT_POINT}{path}").as_str()), "aligned index: {aligned}");
            assert_eq!(container.read(chunk.id())?, b"hello");
        }
        assert!(seen_aligned && seen_unaligned, "both index alignments must be exercised");
        Ok(())
    }
}
