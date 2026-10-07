//! MaterialTags: the game's `FSkeletalMaterial` carries an `FGameplayTagContainer` after the 40 bytes a
//! stock UE 5.3 cook writes. This finds the first SkeletalMesh export, locates its material array by
//! layout, and appends a container to every material. Tags come from an editor carrier export
//! (`MaterialTagAssetUserData`) in the same package: every `MaterialTag.*` name found after a
//! `MaterialSlotName` property is given to that slot. Arrays already in the game layout are left alone.
//!
//! Ported from natimerry/retoc-rivals so the patched bytes match it exactly. The export data is hostile
//! input: every read is bounds-checked and a failed match leaves the package unchanged. Like retoc-rivals,
//! only export offsets and sizes move with the growth; inline data resources inside or after the mesh keep
//! their offsets.

use crate::info;
use crate::legacy_asset::{FLegacyPackageHeader, FMinimalName};
use crate::logging::Log;

const LEGACY_SKELETAL_MATERIAL_SIZE: usize = 40;
const EMPTY_TAG_SKELETAL_MATERIAL_SIZE: usize = 44;
const MAX_SKELETAL_MATERIALS: i32 = 128;
const MAX_MATERIAL_TAGS_PER_SLOT: i32 = 64;
const LOWEST_IMPORT_INDEX: i32 = -10_000;
const TAG_PREFIX: &str = "MaterialTag.";
const CARRIER_CLASS: &str = "MaterialTagAssetUserData";

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct MaterialSlotTags {
    slot_name: String,
    tag_names: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MaterialArrayLayout {
    /// 40-byte entries, as a stock cook writes them
    Legacy,
    /// 44-byte entries whose tag containers are all empty
    PaddedEmpty,
    /// Entries followed by a tag container with at least one tag somewhere in the array
    PaddedTagged,
}

#[derive(Debug, Clone, Copy)]
struct MaterialArray {
    offset: usize,
    count: usize,
    layout: MaterialArrayLayout,
    score: usize,
    byte_len: usize,
}

/// Returns the patched exports buffer, with the SkeletalMesh export's size and later exports' offsets
/// updated in `package`, or `None` when nothing was patched.
pub(in crate::game) fn patch_package(package: &mut FLegacyPackageHeader, exports: &[u8], log: &Log) -> Option<Vec<u8>> {
    let package_name = package.summary.package_name.clone();
    let names = package.name_map.copy_raw_names();

    let mesh_index = find_export_by_class(package, "SkeletalMesh")?;
    let Some(mesh_range) = export_range(package, mesh_index, exports.len()) else {
        info!(log, "[MaterialTags] {package_name} - SkeletalMesh export out of bounds");
        return None;
    };

    let tags = find_carrier_export(package)
        .and_then(|carrier| export_range(package, carrier, exports.len()))
        .and_then(|range| exports.get(range))
        .and_then(|carrier_data| scan_slot_tags(carrier_data, &names))
        .unwrap_or_default();
    if tags.is_empty() {
        info!(log, "[MaterialTags] {package_name} - Will patch with null containers (no tags found)");
    } else {
        let total_tags: usize = tags.iter().map(|t| t.tag_names.len()).sum();
        info!(log, "[MaterialTags] {package_name} - Found {total_tags} tag(s) across {} slot(s)", tags.len());
    }

    let mesh_data = exports.get(mesh_range.clone())?;
    let patched_mesh = patch_mesh_materials(mesh_data, &names, &tags, &package_name, log)?;

    let size_diff = i64::try_from(patched_mesh.len()).ok()? - i64::try_from(mesh_data.len()).ok()?;
    let mesh_serial_offset = package.exports[mesh_index].serial_offset;
    let new_mesh_size = package.exports[mesh_index].serial_size.checked_add(size_diff)?;
    // Exports after the mesh move later by its growth; checked before anything changes.
    if package.exports.iter().any(|e| e.serial_offset > mesh_serial_offset && e.serial_offset.checked_add(size_diff).is_none()) {
        return None;
    }
    package.exports[mesh_index].serial_size = new_mesh_size;
    for export in package.exports.iter_mut().filter(|e| e.serial_offset > mesh_serial_offset) {
        export.serial_offset += size_diff;
    }

    let mut patched_exports = Vec::with_capacity(exports.len().saturating_add(patched_mesh.len()).saturating_sub(mesh_data.len()));
    patched_exports.extend_from_slice(&exports[..mesh_range.start]);
    patched_exports.extend_from_slice(&patched_mesh);
    patched_exports.extend_from_slice(&exports[mesh_range.end..]);
    info!(log, "[MaterialTags] {package_name} - Patched, size change: +{size_diff} bytes");
    Some(patched_exports)
}

fn name_at(package: &FLegacyPackageHeader, name: FMinimalName) -> Option<String> {
    package.name_map.get(name).ok().map(|n| n.into_owned())
}

fn class_name(package: &FLegacyPackageHeader, export_index: usize) -> Option<String> {
    let class_index = package.exports.get(export_index)?.class_index;
    if !class_index.is_import() {
        return None;
    }
    let import = package.imports.get(class_index.to_import_index() as usize)?;
    name_at(package, import.object_name)
}

fn find_export_by_class(package: &FLegacyPackageHeader, class: &str) -> Option<usize> {
    (0..package.exports.len()).find(|&i| class_name(package, i).as_deref() == Some(class))
}

/// to-legacy maps the carrier class to AssetUserData but keeps the export's name, so the name is checked first.
fn find_carrier_export(package: &FLegacyPackageHeader) -> Option<usize> {
    (0..package.exports.len()).find(|&i| {
        let is_carrier_name = name_at(package, package.exports[i].object_name).is_some_and(|name| name == CARRIER_CLASS || name.starts_with("MaterialTagAssetUserData_"));
        is_carrier_name || class_name(package, i).as_deref() == Some(CARRIER_CLASS)
    })
}

/// Byte range of an export inside the `.uexp` buffer, if it lies entirely inside it.
fn export_range(package: &FLegacyPackageHeader, export_index: usize, exports_len: usize) -> Option<std::ops::Range<usize>> {
    let export = package.exports.get(export_index)?;
    let start = usize::try_from(export.serial_offset.checked_sub(i64::from(package.summary.versioning_info.total_header_size))?).ok()?;
    let end = start.checked_add(usize::try_from(export.serial_size).ok()?)?;
    (end <= exports_len).then_some(start..end)
}

fn read_i32_at(data: &[u8], offset: usize) -> Option<i32> {
    let bytes = data.get(offset..offset.checked_add(4)?)?;
    Some(i32::from_le_bytes(bytes.try_into().ok()?))
}

/// Reads an FName (index, number) and formats it the way the engine displays it.
fn read_name_at(data: &[u8], names: &[String], offset: usize) -> Option<String> {
    let index = usize::try_from(read_i32_at(data, offset)?).ok()?;
    let number = read_i32_at(data, offset.checked_add(4)?)?;
    let bare_name = names.get(index)?;
    Some(if number != 0 { format!("{bare_name}_{}", i64::from(number) - 1) } else { bare_name.clone() })
}

/// Finds each tagged `MaterialSlotName` NameProperty in the carrier export and collects the slot name and
/// the `MaterialTag.*` names that follow it. retoc-rivals falls back to a second parser when no such
/// property exists; that parser can only return no tags or never terminate, so no tags is used here.
fn scan_slot_tags(data: &[u8], names: &[String]) -> Option<Vec<MaterialSlotTags>> {
    let slot_property_name = i32::try_from(names.iter().position(|n| n.eq_ignore_ascii_case("MaterialSlotName"))?).ok()?;
    let name_property = i32::try_from(names.iter().position(|n| n.eq_ignore_ascii_case("NameProperty"))?).ok()?;

    let slot_property_offsets: Vec<usize> = (0..data.len().saturating_sub(16))
        .filter(|&offset| read_i32_at(data, offset) == Some(slot_property_name) && read_i32_at(data, offset + 4) == Some(0) && read_i32_at(data, offset + 8) == Some(name_property) && read_i32_at(data, offset + 12) == Some(0))
        .collect();

    // The property value sits after the tag header; its exact offset depends on the tag layout.
    const VALUE_OFFSETS: [usize; 4] = [24, 25, 32, 33];
    let mut entries = Vec::new();
    for (i, &property_offset) in slot_property_offsets.iter().enumerate() {
        let next_property_offset = slot_property_offsets.get(i + 1).copied().unwrap_or(data.len());
        let slot_name = VALUE_OFFSETS.iter().filter_map(|value_offset| read_name_at(data, names, property_offset + value_offset)).find(|name| name != "None" && !name.starts_with(TAG_PREFIX));
        let Some(slot_name) = slot_name else {
            continue;
        };

        let mut tag_names: Vec<String> = Vec::new();
        for offset in property_offset.saturating_add(24)..next_property_offset.min(data.len()).saturating_sub(8) {
            if let Some(tag) = read_name_at(data, names, offset).filter(|name| name.starts_with(TAG_PREFIX))
                && !tag_names.contains(&tag)
            {
                tag_names.push(tag);
            }
        }
        entries.push(MaterialSlotTags { slot_name, tag_names });
    }
    (!entries.is_empty()).then_some(entries)
}

/// Returns the mesh export with a tag container after every material, or `None` when no material array
/// was found or the array already has the game layout with tags in it.
fn patch_mesh_materials(data: &[u8], names: &[String], tags: &[MaterialSlotTags], package_name: &str, log: &Log) -> Option<Vec<u8>> {
    let expected_slot_names: Vec<&str> = tags.iter().map(|t| t.slot_name.as_str()).collect();
    let Some(array) = find_material_array(data, names, &expected_slot_names, package_name, log) else {
        info!(log, "[MaterialTags] {package_name} - Could not find valid FSkeletalMaterial array");
        return None;
    };
    let source_stride = match array.layout {
        MaterialArrayLayout::Legacy => LEGACY_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedEmpty => EMPTY_TAG_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedTagged => {
            info!(log, "[MaterialTags] {package_name} - Skipping (prepatched, {} material(s))", array.count);
            return None;
        }
    };
    let materials_end = array.offset.checked_add(array.count.checked_mul(source_stride)?)?;

    let mut patched = Vec::with_capacity(data.len() + array.count * 4);
    patched.extend_from_slice(data.get(..array.offset)?);
    let (mut tagged_materials, mut injected_tags) = (0usize, 0usize);
    for material_index in 0..array.count {
        let entry_offset = array.offset + material_index * source_stride;
        patched.extend_from_slice(data.get(entry_offset..entry_offset + LEGACY_SKELETAL_MATERIAL_SIZE)?);

        let slot_name = read_name_at(data, names, entry_offset + 4).unwrap_or_default();
        let tag_indices: Vec<i32> = tags
            .iter()
            .filter(|t| t.slot_name.eq_ignore_ascii_case(&slot_name))
            .flat_map(|t| &t.tag_names)
            .filter_map(|tag| names.iter().position(|n| n == tag))
            .filter_map(|index| i32::try_from(index).ok())
            .collect();
        if !tag_indices.is_empty() {
            tagged_materials += 1;
            injected_tags += tag_indices.len();
        }

        patched.extend_from_slice(&i32::try_from(tag_indices.len()).ok()?.to_le_bytes());
        for tag_index in tag_indices {
            patched.extend_from_slice(&tag_index.to_le_bytes());
            patched.extend_from_slice(&0i32.to_le_bytes());
        }
    }
    patched.extend_from_slice(data.get(materials_end..)?);

    info!(
        log,
        "[MaterialTags] {package_name} - Added FGameplayTagContainer to {} material(s), injected {injected_tags} tag(s) into {tagged_materials} material(s), size change: +{} bytes",
        array.count,
        patched.len().saturating_sub(data.len())
    );
    Some(patched)
}

/// The earliest array of any layout: it follows the properties, and a large mesh's render data can hold bytes that pass
/// as a short array of any layout. Ties go to the padded readings, since a one-entry padded array also reads as a stock one.
fn find_material_array(data: &[u8], names: &[String], expected_slot_names: &[&str], package_name: &str, log: &Log) -> Option<MaterialArray> {
    let find = |layout| find_first_array(data, names, expected_slot_names, layout);
    let tagged = find(MaterialArrayLayout::PaddedTagged).filter(|array| array.byte_len > array.count * EMPTY_TAG_SKELETAL_MATERIAL_SIZE);
    let (array, description) = [(tagged, "prepatched tagged"), (find(MaterialArrayLayout::PaddedEmpty), "prepatched"), (find(MaterialArrayLayout::Legacy), "legacy")]
        .into_iter()
        .filter_map(|(array, description)| array.map(|array| (array, description)))
        .min_by_key(|(array, _)| array.offset)?;
    info!(log, "[MaterialTags] {package_name} - Found {description} FSkeletalMaterial array at {:#X}: {} material(s), matched {} slot(s)", array.offset, array.count, array.score);
    Some(array)
}

/// The first offset holding a plausible material count followed by that many valid entries of `layout`.
/// When slot names are expected, an array must contain at least one of them.
fn find_first_array(data: &[u8], names: &[String], expected_slot_names: &[&str], layout: MaterialArrayLayout) -> Option<MaterialArray> {
    let min_stride = match layout {
        MaterialArrayLayout::Legacy => LEGACY_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedEmpty | MaterialArrayLayout::PaddedTagged => EMPTY_TAG_SKELETAL_MATERIAL_SIZE,
    };
    let last_count_offset = data.len().checked_sub(4 + min_stride)?;
    (4..=last_count_offset).find_map(|count_offset| {
        let count = read_i32_at(data, count_offset).filter(|&c| c > 0 && c <= MAX_SKELETAL_MATERIALS)?;
        let offset = count_offset + 4;
        if !read_i32_at(data, offset).is_some_and(|package_index| (LOWEST_IMPORT_INDEX..0).contains(&package_index)) {
            return None;
        }
        let count = count as usize;
        let byte_len = match layout {
            MaterialArrayLayout::PaddedTagged => tagged_array_len(data, offset, count, names.len())?,
            _ => fixed_stride_array_len(data, offset, count, min_stride, layout == MaterialArrayLayout::PaddedEmpty, names.len())?,
        };
        let score = if expected_slot_names.is_empty() { 0 } else { count_expected_slots(data, offset, count, layout, names, expected_slot_names) };
        if !expected_slot_names.is_empty() && score == 0 {
            return None;
        }
        Some(MaterialArray { offset, count, layout, score, byte_len })
    })
}

/// An import (or null) material, and in-range indices for the slot name and the imported slot name.
fn is_valid_material_entry(data: &[u8], offset: usize, name_count: usize) -> bool {
    let name_in_range = |at: usize| read_i32_at(data, at).and_then(|i| usize::try_from(i).ok()).is_some_and(|i| i < name_count);
    read_i32_at(data, offset).is_some_and(|package_index| (LOWEST_IMPORT_INDEX..=0).contains(&package_index)) && name_in_range(offset + 4) && name_in_range(offset + 12)
}

fn fixed_stride_array_len(data: &[u8], offset: usize, count: usize, stride: usize, empty_tag_containers: bool, name_count: usize) -> Option<usize> {
    let byte_len = count.checked_mul(stride)?;
    if offset.checked_add(byte_len)? > data.len() {
        return None;
    }
    let all_valid = (0..count)
        .map(|i| offset + i * stride)
        .all(|entry| is_valid_material_entry(data, entry, name_count) && (!empty_tag_containers || read_i32_at(data, entry + LEGACY_SKELETAL_MATERIAL_SIZE) == Some(0)));
    all_valid.then_some(byte_len)
}

/// Byte length of an array of entries each followed by a tag container, or `None` if any entry is invalid.
fn tagged_array_len(data: &[u8], offset: usize, count: usize, name_count: usize) -> Option<usize> {
    let mut cursor = offset;
    for _ in 0..count {
        if cursor.checked_add(EMPTY_TAG_SKELETAL_MATERIAL_SIZE)? > data.len() || !is_valid_material_entry(data, cursor, name_count) {
            return None;
        }
        let tag_count = read_i32_at(data, cursor + LEGACY_SKELETAL_MATERIAL_SIZE).filter(|&c| (0..=MAX_MATERIAL_TAGS_PER_SLOT).contains(&c))? as usize;
        let tags_offset = cursor + EMPTY_TAG_SKELETAL_MATERIAL_SIZE;
        let tags_end = tags_offset.checked_add(tag_count * 8)?;
        if tags_end > data.len() {
            return None;
        }
        let tags_valid = (0..tag_count).all(|t| {
            let tag = tags_offset + t * 8;
            read_i32_at(data, tag).and_then(|i| usize::try_from(i).ok()).is_some_and(|i| i < name_count) && read_i32_at(data, tag + 4).is_some_and(|number| number >= 0)
        });
        if !tags_valid {
            return None;
        }
        cursor = tags_end;
    }
    Some(cursor - offset)
}

fn count_expected_slots(data: &[u8], offset: usize, count: usize, layout: MaterialArrayLayout, names: &[String], expected_slot_names: &[&str]) -> usize {
    let is_expected = |slot_name: &str| expected_slot_names.iter().any(|expected| expected.eq_ignore_ascii_case(slot_name));
    let stride = match layout {
        MaterialArrayLayout::Legacy => LEGACY_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedEmpty => EMPTY_TAG_SKELETAL_MATERIAL_SIZE,
        MaterialArrayLayout::PaddedTagged => {
            let mut cursor = offset;
            let mut score = 0;
            for _ in 0..count {
                let Some(slot_name) = read_name_at(data, names, cursor + 4) else {
                    break;
                };
                score += usize::from(is_expected(&slot_name));
                let Some(tag_count) = read_i32_at(data, cursor + LEGACY_SKELETAL_MATERIAL_SIZE).filter(|&c| (0..=MAX_MATERIAL_TAGS_PER_SLOT).contains(&c)) else {
                    break;
                };
                cursor += EMPTY_TAG_SKELETAL_MATERIAL_SIZE + tag_count as usize * 8;
            }
            return score;
        }
    };
    (0..count).filter_map(|i| read_name_at(data, names, offset + i * stride + 4)).filter(|slot_name| is_expected(slot_name)).count()
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::legacy_asset::{FObjectExport, FObjectImport, FPackageNameMap};
    use crate::zen::FPackageIndex;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| n.to_string()).collect()
    }

    fn i32s(values: &[i32]) -> Vec<u8> {
        values.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// A stock 40-byte FSkeletalMaterial: material, slot name, imported slot name, 20 bytes of UV data.
    fn material(package_index: i32, slot_name_index: i32) -> Vec<u8> {
        let mut entry = i32s(&[package_index, slot_name_index, 0, slot_name_index, 0]);
        entry.extend_from_slice(&[0x11; 20]);
        entry
    }

    fn legacy_mesh(materials: &[Vec<u8>]) -> Vec<u8> {
        let mut data = i32s(&[0, materials.len() as i32]);
        materials.iter().for_each(|m| data.extend_from_slice(m));
        data.extend_from_slice(&[0xAB; 4]);
        data
    }

    fn slot(slot_name: &str, tag_names: &[&str]) -> MaterialSlotTags {
        MaterialSlotTags {
            slot_name: slot_name.to_string(),
            tag_names: names(tag_names),
        }
    }

    #[test]
    fn stock_array_gets_an_empty_container_per_material() {
        let names = names(&["None", "SlotA", "SlotB"]);
        let data = legacy_mesh(&[material(-1, 1), material(-2, 2)]);
        let patched = patch_mesh_materials(&data, &names, &[], "/Game/Mesh", &Log::no_log()).expect("patched");

        let mut expected = i32s(&[0, 2]);
        expected.extend(material(-1, 1));
        expected.extend(i32s(&[0]));
        expected.extend(material(-2, 2));
        expected.extend(i32s(&[0]));
        expected.extend([0xAB; 4]);
        assert_eq!(patched, expected);
    }

    #[test]
    fn stock_array_wins_over_a_later_padded_lookalike() {
        // A large mesh's render data can hold bytes that pass as a one-entry padded array (seen at 0x5B4915 in a lobby mesh)
        let names = names(&["None", "SlotA", "SlotB"]);
        let mut data = legacy_mesh(&[material(-1, 1), material(-2, 2)]);
        data.extend([0xAB; 60]);
        data.extend(i32s(&[1]));
        data.extend(material(-3, 1));
        data.extend(i32s(&[0]));
        let patched = patch_mesh_materials(&data, &names, &[], "/Game/Mesh", &Log::no_log()).expect("patched");

        let mut expected = i32s(&[0, 2]);
        expected.extend(material(-1, 1));
        expected.extend(i32s(&[0]));
        expected.extend(material(-2, 2));
        expected.extend(i32s(&[0]));
        expected.extend(&data[88..]);
        assert_eq!(patched, expected);
    }

    #[test]
    fn one_entry_padded_array_is_not_padded_again() {
        // At its offset a one-entry padded array also reads as a stock one
        let names = names(&["None", "SlotA"]);
        let mut data = i32s(&[0, 1]);
        data.extend(material(-1, 1));
        data.extend(i32s(&[0]));
        data.extend([0xAB; 4]);
        let patched = patch_mesh_materials(&data, &names, &[], "/Game/Mesh", &Log::no_log());
        assert!(patched.is_none_or(|p| p == data));
    }

    #[test]
    fn tags_go_to_the_matching_slot_of_a_stock_array() {
        let names = names(&["None", "SlotA", "SlotB", "MaterialTag.Glasses"]);
        let data = legacy_mesh(&[material(-1, 1), material(-2, 2)]);
        let patched = patch_mesh_materials(&data, &names, &[slot("slota", &["MaterialTag.Glasses"])], "/Game/Mesh", &Log::no_log()).expect("patched");

        let mut expected = i32s(&[0, 2]);
        expected.extend(material(-1, 1));
        expected.extend(i32s(&[1, 3, 0]));
        expected.extend(material(-2, 2));
        expected.extend(i32s(&[0]));
        expected.extend([0xAB; 4]);
        assert_eq!(patched, expected);
    }

    #[test]
    fn with_tags_only_an_array_holding_a_tagged_slot_is_patched() {
        let names = names(&["None", "Other", "SlotA", "MaterialTag.Glasses"]);
        // An earlier array-shaped run (slot "Other") comes before the real material array (slot "SlotA")
        let mut data = i32s(&[0, 1]);
        data.extend(material(-1, 1));
        data.extend(i32s(&[1]));
        data.extend(material(-2, 2));
        data.extend([0xAB; 4]);
        let tags = [slot("SlotA", &["MaterialTag.Glasses"])];

        let patched = patch_mesh_materials(&data, &names, &tags, "/Game/Mesh", &Log::no_log()).expect("patched");
        let mut expected = data[..52].to_vec();
        expected.extend(material(-2, 2));
        expected.extend(i32s(&[1, 3, 0]));
        expected.extend([0xAB; 4]);
        assert_eq!(patched, expected);

        // No array holds the tagged slot: nothing is patched
        assert_eq!(patch_mesh_materials(&data, &names, &[slot("Missing", &["MaterialTag.Glasses"])], "/Game/Mesh", &Log::no_log()), None);
    }

    #[test]
    fn empty_containers_are_refilled_with_tags() {
        // The retoc-rivals test case: 44-byte entries whose containers are empty
        let names = names(&["None", "SlotA", "SlotB", "MaterialTag.Glasses"]);
        let mut data = i32s(&[0, 2]);
        data.extend(material(-1, 1));
        data.extend(i32s(&[0]));
        data.extend(material(-2, 2));
        data.extend(i32s(&[0]));
        let tags = [slot("SlotA", &["MaterialTag.Glasses"]), slot("SlotB", &[])];

        let patched = patch_mesh_materials(&data, &names, &tags, "/Game/Mesh", &Log::no_log()).expect("patched");
        assert_eq!(patched.len(), data.len() + 8);
        assert_eq!(read_i32_at(&patched, 48), Some(1));
        assert_eq!(read_i32_at(&patched, 52), Some(3));
        assert_eq!(read_i32_at(&patched, 56), Some(0));
        assert_eq!(read_i32_at(&patched, 100), Some(0));

        // Patching the result again finds tags already there and leaves it alone
        assert_eq!(patch_mesh_materials(&patched, &names, &tags, "/Game/Mesh", &Log::no_log()), None);
    }

    #[test]
    fn hostile_arrays_are_skipped_without_panicking() {
        let names = names(&["None", "SlotA"]);
        let log = Log::no_log();
        let cases: Vec<Vec<u8>> = vec![
            vec![],
            i32s(&[0, 1]),
            i32s(&[0, i32::MAX, -1, 1, 0, 1, 0]),
            i32s(&[0, -5, -1, 1, 0, 1, 0]),
            // Count claims more materials than the data holds
            {
                let mut d = i32s(&[0, 3]);
                d.extend(material(-1, 1));
                d.extend(i32s(&[i32::MAX]));
                d
            },
            // Slot name index outside the name map
            legacy_mesh(&[material(-1, 99)]),
            // Material is an export, not an import
            legacy_mesh(&[material(5, 1)]),
        ];
        for data in cases {
            assert_eq!(patch_mesh_materials(&data, &names, &[], "/Game/Mesh", &log), None, "{data:02X?}");
        }
        // A tag container with a hostile count is not treated as the game layout
        let mut padded = i32s(&[0, 1]);
        padded.extend(material(-1, 1));
        padded.extend(i32s(&[1_000_000, 1, 0]));
        assert!(tagged_array_len(&padded, 8, 1, names.len()).is_none());
        assert_eq!(read_name_at(&i32s(&[1, i32::MIN]), &names, 0), Some(format!("SlotA_{}", i64::from(i32::MIN) - 1)));
    }

    /// Tagged-property bytes of a MaterialSlotName NameProperty whose value is `slot_name_index`.
    fn slot_property(slot_name_index: i32) -> Vec<u8> {
        // name, number, type, number, size, array index, then the value FName at +24
        i32s(&[1, 0, 2, 0, 8, 0, slot_name_index, 0])
    }

    const CARRIER_NAMES: [&str; 7] = ["None", "MaterialSlotName", "NameProperty", "SlotA", "SlotB", "MaterialTag.Glasses", "MaterialTag.Hat"];

    fn carrier_data() -> Vec<u8> {
        let mut data = slot_property(3);
        data.extend(i32s(&[7, 5, 0, 6, 0, 5, 0, 0]));
        data.extend(slot_property(4));
        data.extend(i32s(&[0, 0, 0]));
        data
    }

    #[test]
    fn carrier_slots_and_their_tags_are_collected() {
        let tags = scan_slot_tags(&carrier_data(), &names(&CARRIER_NAMES)).expect("slots");
        assert_eq!(tags, vec![slot("SlotA", &["MaterialTag.Glasses", "MaterialTag.Hat"]), slot("SlotB", &[])]);
    }

    #[test]
    fn carrier_without_slot_properties_has_no_tags() {
        assert_eq!(scan_slot_tags(&i32s(&[5, 0, 6, 0]), &names(&CARRIER_NAMES)), None);
        assert_eq!(scan_slot_tags(&carrier_data(), &names(&["None", "SlotA"])), None);
    }

    const HEADER_SIZE: i32 = 100;

    /// Package with a SkeletalMesh export, a carrier export and a trailing export, in that order.
    fn package(mesh: &[u8], carrier: &[u8]) -> (FLegacyPackageHeader, Vec<u8>) {
        let mut package_names = names(&CARRIER_NAMES);
        package_names.extend(names(&["SkeletalMesh", "MaterialTagAssetUserData", "Trailing"]));
        let name = |n: &str| FMinimalName {
            index: package_names.iter().position(|p| p == n).unwrap() as i32,
            number: 0,
        };
        let export = |object_name, class_index, offset: usize, size: usize| FObjectExport {
            object_name,
            class_index,
            serial_offset: i64::from(HEADER_SIZE) + offset as i64,
            serial_size: size as i64,
            ..Default::default()
        };
        let mut package = FLegacyPackageHeader {
            imports: vec![FObjectImport {
                object_name: name("SkeletalMesh"),
                ..Default::default()
            }],
            exports: vec![
                export(name("SlotA"), FPackageIndex::create_import(0), 0, mesh.len()),
                export(name("MaterialTagAssetUserData"), FPackageIndex::create_null(), mesh.len(), carrier.len()),
                export(name("Trailing"), FPackageIndex::create_null(), mesh.len() + carrier.len(), 4),
            ],
            ..Default::default()
        };
        package.name_map = FPackageNameMap::create_from_names(package_names);
        package.summary.package_name = "/Game/Mesh".to_string();
        package.summary.versioning_info.total_header_size = HEADER_SIZE;
        let exports = [mesh, carrier, &[0xCD; 4]].concat();
        (package, exports)
    }

    #[test]
    fn package_patch_injects_carrier_tags_and_moves_later_exports() {
        let mesh = legacy_mesh(&[material(-1, 3), material(-1, 4)]);
        let carrier = carrier_data();
        let (mut package, exports) = package(&mesh, &carrier);

        let patched = patch_package(&mut package, &exports, &Log::no_log()).expect("patched");

        let mut expected_mesh = i32s(&[0, 2]);
        expected_mesh.extend(material(-1, 3));
        expected_mesh.extend(i32s(&[2, 5, 0, 6, 0]));
        expected_mesh.extend(material(-1, 4));
        expected_mesh.extend(i32s(&[0]));
        expected_mesh.extend([0xAB; 4]);
        assert_eq!(patched, [expected_mesh.as_slice(), &carrier, &[0xCD; 4]].concat());
        assert_eq!(package.exports[0].serial_size, mesh.len() as i64 + 24);
        assert_eq!(package.exports[1].serial_offset, i64::from(HEADER_SIZE) + mesh.len() as i64 + 24);
        assert_eq!(package.exports[2].serial_offset, i64::from(HEADER_SIZE) + (mesh.len() + carrier.len()) as i64 + 24);
    }

    #[test]
    fn package_without_skeletal_mesh_or_with_out_of_range_export_is_untouched() {
        let mesh = legacy_mesh(&[material(-1, 3)]);
        let (mut package, exports) = package(&mesh, &carrier_data());
        package.exports[0].class_index = FPackageIndex::create_null();
        assert_eq!(patch_package(&mut package, &exports, &Log::no_log()), None);

        for (offset, size) in [(0, mesh.len() as i64), (i64::from(HEADER_SIZE), i64::MAX), (i64::from(HEADER_SIZE), -1), (i64::MAX, 4)] {
            let (mut package, exports) = self::package(&mesh, &carrier_data());
            package.exports[0].serial_offset = offset;
            package.exports[0].serial_size = size;
            assert_eq!(patch_package(&mut package, &exports, &Log::no_log()), None, "offset {offset} size {size}");
            assert_eq!(package.exports[1].serial_offset, i64::from(HEADER_SIZE) + mesh.len() as i64);
        }
    }
}
