//! retoc-rivals replaces the bulk data map with one entry covering the whole `.ubulk` when any
//! `.ubulk`-resident resource does not fit inside it. Cooked packages never trigger this; it is kept
//! so conversions match retoc-rivals. One deliberate difference: a resource whose offset plus size
//! overflows counts as not fitting here, where retoc-rivals' wrapping sum treats it as fitting.

use crate::legacy_asset::FLegacyPackageHeader;
use crate::zen::FBulkDataMapEntry;

const BULKDATA_FORCE_INLINE_PAYLOAD: u32 = 0x40;
const BULKDATA_PAYLOAD_IN_SEPARATE_FILE: u32 = 0x100;
const BULKDATA_OPTIONAL_PAYLOAD: u32 = 0x800;
const WHOLE_FILE_ENTRY_FLAGS: u32 = 0x0001_0501;

pub(in crate::game) fn apply_fallback(package: &FLegacyPackageHeader, bulk_data: Option<&[u8]>, zen_bulk_data: &mut Vec<FBulkDataMapEntry>) {
    if let Some(entry) = fallback_entry(package, bulk_data) {
        *zen_bulk_data = vec![entry];
    }
}

fn fallback_entry(package: &FLegacyPackageHeader, bulk_data: Option<&[u8]>) -> Option<FBulkDataMapEntry> {
    let ubulk_size = i64::try_from(bulk_data?.len()).ok().filter(|&size| size > 0)?;
    if package.data_resources.is_empty() {
        return None;
    }
    let all_fit = package
        .data_resources
        .iter()
        .filter(|r| r.legacy_bulk_data_flags & BULKDATA_PAYLOAD_IN_SEPARATE_FILE != 0 && r.legacy_bulk_data_flags & (BULKDATA_OPTIONAL_PAYLOAD | BULKDATA_FORCE_INLINE_PAYLOAD) == 0)
        .all(|r| r.serial_offset.checked_add(r.serial_size).is_some_and(|end| end <= ubulk_size));
    (!all_fit).then_some(FBulkDataMapEntry {
        serial_offset: 0,
        duplicate_serial_offset: -1,
        serial_size: ubulk_size,
        flags: WHOLE_FILE_ENTRY_FLAGS,
        cooked_index: 0,
        pad: [0; 3],
    })
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::legacy_asset::FObjectDataResource;

    fn resource(serial_offset: i64, serial_size: i64, flags: u32) -> FObjectDataResource {
        FObjectDataResource {
            serial_offset,
            serial_size,
            legacy_bulk_data_flags: flags,
            ..Default::default()
        }
    }

    fn package(resources: Vec<FObjectDataResource>) -> FLegacyPackageHeader {
        FLegacyPackageHeader { data_resources: resources, ..Default::default() }
    }

    #[test]
    fn resources_that_fit_keep_the_map() {
        let package = package(vec![resource(0, 16, 0x100), resource(16, 16, 0x100)]);
        assert!(fallback_entry(&package, Some(&[0; 32])).is_none());
    }

    #[test]
    fn overflowing_ubulk_resource_collapses_the_map() {
        let package = package(vec![resource(0, 16, 0x100), resource(16, 17, 0x100)]);
        let entry = fallback_entry(&package, Some(&[0; 32])).expect("fallback");
        assert_eq!((entry.serial_offset, entry.duplicate_serial_offset, entry.serial_size, entry.flags), (0, -1, 32, 0x0001_0501));
    }

    #[test]
    fn optional_and_inline_resources_are_not_checked_against_ubulk() {
        let package = package(vec![resource(100, 16, 0x100 | 0x800), resource(100, 16, 0x100 | 0x40), resource(100, 16, 0)]);
        assert!(fallback_entry(&package, Some(&[0; 32])).is_none());
    }

    #[test]
    fn no_ubulk_or_hostile_values_do_not_panic() {
        let package = package(vec![resource(i64::MAX, i64::MAX, 0x100)]);
        assert!(fallback_entry(&package, None).is_none());
        assert!(fallback_entry(&package, Some(&[])).is_none());
        assert!(fallback_entry(&package, Some(&[0; 4])).is_some());
    }
}
