//! Reads the few PE fields the Windows executable tests check: the subsystem
//! and the resource tree. Shared into test crates by `#[path]`; no
//! dependencies and not a production module.

use std::{
    fs,
    io::{Error, ErrorKind, Result},
    path::Path,
};

/// Resource type of a group of icon images, the one `LoadImageW` finds.
pub const RT_GROUP_ICON: u32 = 14;
/// Resource type of an application manifest.
pub const RT_MANIFEST: u32 = 24;

const DOS_SIGNATURE: &[u8] = b"MZ";
const PE_OFFSET_FIELD: usize = 0x3C;
const PE_SIGNATURE: &[u8] = b"PE\0\0";
const COFF_HEADER_LEN: usize = 20;
const PE32_MAGIC: u16 = 0x10B;
const PE32_PLUS_MAGIC: u16 = 0x20B;
const SUBSYSTEM_OFFSET: usize = 68;
const RESOURCE_DIRECTORY_INDEX: usize = 2;
const DATA_DIRECTORY_ENTRY_LEN: usize = 8;
const SECTION_HEADER_LEN: usize = 40;
const RESOURCE_DIRECTORY_LEN: usize = 16;
const RESOURCE_ENTRY_LEN: usize = 8;
const HIGH_BIT: u32 = 0x8000_0000;

/// The optional header's Subsystem field: 2 is a windowed program, 3 a console one.
pub fn subsystem(path: &Path) -> Result<u16> {
    let image = fs::read(path)?;
    let headers = Headers::locate(&image)?;
    read_u16(&image, headers.optional + SUBSYSTEM_OFFSET)
}

/// Whether the resource tree has any resource of type `type_id`.
pub fn has_resource_type(path: &Path, type_id: u32) -> Result<bool> {
    let image = fs::read(path)?;
    let Some(root) = resource_root(&image)? else {
        return Ok(false);
    };
    Ok(entries(&image, root, root)?
        .iter()
        .any(|entry| entry.id == Some(type_id)))
}

/// Whether the resource tree has an icon group with the numeric name `id`.
pub fn has_icon_group(path: &Path, id: u32) -> Result<bool> {
    let image = fs::read(path)?;
    let Some(root) = resource_root(&image)? else {
        return Ok(false);
    };
    let Some(icons) = entries(&image, root, root)?
        .into_iter()
        .find(|entry| entry.id == Some(RT_GROUP_ICON))
    else {
        return Ok(false);
    };
    let Some(names) = icons.subdirectory else {
        return Err(invalid("the icon group type entry is not a directory"));
    };
    Ok(entries(&image, root, root + names)?
        .iter()
        .any(|entry| entry.id == Some(id)))
}

/// Every resource of type `type_id`, in every language: its numeric name, or
/// `None` for one named by a string, and its data.
pub fn resources(path: &Path, type_id: u32) -> Result<Vec<(Option<u32>, Vec<u8>)>> {
    let image = fs::read(path)?;
    let headers = Headers::locate(&image)?;
    let Some(root) = resource_root(&image)? else {
        return Ok(Vec::new());
    };
    let Some(names) = entries(&image, root, root)?
        .into_iter()
        .find(|entry| entry.id == Some(type_id))
    else {
        return Ok(Vec::new());
    };
    let names = names
        .subdirectory
        .ok_or_else(|| invalid("a resource type entry is not a directory"))?;
    let mut found = Vec::new();
    for name in entries(&image, root, root + names)? {
        let languages = name
            .subdirectory
            .ok_or_else(|| invalid("a resource name entry is not a directory"))?;
        for language in entries(&image, root, root + languages)? {
            if language.subdirectory.is_some() {
                return Err(invalid("a resource language entry is a directory"));
            }
            let entry = root + language.data;
            let start = file_offset(&image, &headers, read_u32(&image, entry)?)?;
            let len = offset(read_u32(&image, entry + 4)?)?;
            found.push((name.id, bytes(&image, start, len)?.to_vec()));
        }
    }
    Ok(found)
}

struct Headers {
    signature: usize,
    optional: usize,
    magic: u16,
}

impl Headers {
    fn locate(image: &[u8]) -> Result<Self> {
        if !image.starts_with(DOS_SIGNATURE) {
            return Err(invalid("missing the MZ DOS signature"));
        }
        let signature = offset(read_u32(image, PE_OFFSET_FIELD)?)?;
        if bytes(image, signature, PE_SIGNATURE.len())? != PE_SIGNATURE {
            return Err(invalid("missing the PE\\0\\0 signature"));
        }
        let optional = signature + PE_SIGNATURE.len() + COFF_HEADER_LEN;
        let magic = read_u16(image, optional)?;
        if magic != PE32_MAGIC && magic != PE32_PLUS_MAGIC {
            return Err(invalid("optional header magic is neither PE32 nor PE32+"));
        }
        Ok(Self {
            signature,
            optional,
            magic,
        })
    }
}

/// File offset of the resource tree's root directory, or `None` without one.
fn resource_root(image: &[u8]) -> Result<Option<usize>> {
    let headers = Headers::locate(image)?;
    let (count_offset, directories_offset) = if headers.magic == PE32_MAGIC {
        (92, 96)
    } else {
        (108, 112)
    };
    let directory_count = offset(read_u32(image, headers.optional + count_offset)?)?;
    if directory_count <= RESOURCE_DIRECTORY_INDEX {
        return Ok(None);
    }
    let entry =
        headers.optional + directories_offset + RESOURCE_DIRECTORY_INDEX * DATA_DIRECTORY_ENTRY_LEN;
    let address = read_u32(image, entry)?;
    if address == 0 || read_u32(image, entry + 4)? == 0 {
        return Ok(None);
    }
    file_offset(image, &headers, address).map(Some)
}

/// Maps a relative virtual address to its file offset through the section table.
fn file_offset(image: &[u8], headers: &Headers, address: u32) -> Result<usize> {
    let section_count = read_u16(image, headers.signature + 6)?;
    let optional_len = usize::from(read_u16(image, headers.signature + 20)?);
    let table = headers.optional + optional_len;
    for index in 0..usize::from(section_count) {
        let section = table + index * SECTION_HEADER_LEN;
        let virtual_size = read_u32(image, section + 8)?;
        let virtual_address = read_u32(image, section + 12)?;
        let raw_size = read_u32(image, section + 16)?;
        let raw_pointer = read_u32(image, section + 20)?;
        let within = address
            .checked_sub(virtual_address)
            .filter(|delta| *delta < virtual_size.max(raw_size));
        if let Some(delta) = within {
            return offset(raw_pointer)?
                .checked_add(offset(delta)?)
                .ok_or_else(|| invalid("resource offset overflows"));
        }
    }
    Err(invalid("no section holds the resource table"))
}

struct Entry {
    /// The numeric ID, or `None` for an entry named by a string.
    id: Option<u32>,
    /// Offset of the child directory from the resource root, for a branch.
    subdirectory: Option<usize>,
    /// Offset of the data entry from the resource root, for a leaf.
    data: usize,
}

fn entries(image: &[u8], root: usize, directory: usize) -> Result<Vec<Entry>> {
    let named = usize::from(read_u16(image, directory + 12)?);
    let numbered = usize::from(read_u16(image, directory + 14)?);
    (0..named + numbered)
        .map(|index| {
            let entry = directory + RESOURCE_DIRECTORY_LEN + index * RESOURCE_ENTRY_LEN;
            let name = read_u32(image, entry)?;
            let data = read_u32(image, entry + 4)?;
            let subdirectory = if data & HIGH_BIT == 0 {
                None
            } else {
                let child = offset(data & !HIGH_BIT)?;
                bytes(image, root + child, RESOURCE_DIRECTORY_LEN)?;
                Some(child)
            };
            Ok(Entry {
                id: (name & HIGH_BIT == 0).then_some(name),
                subdirectory,
                data: offset(data & !HIGH_BIT)?,
            })
        })
        .collect()
}

fn offset(value: u32) -> Result<usize> {
    usize::try_from(value).map_err(|_| invalid("offset does not fit in memory"))
}

fn bytes(image: &[u8], offset: usize, len: usize) -> Result<&[u8]> {
    offset
        .checked_add(len)
        .and_then(|end| image.get(offset..end))
        .ok_or_else(|| {
            Error::new(
                ErrorKind::UnexpectedEof,
                format!("image ends before byte {offset} + {len}"),
            )
        })
}

fn read_u16(image: &[u8], offset: usize) -> Result<u16> {
    let mut value = [0; 2];
    value.copy_from_slice(bytes(image, offset, 2)?);
    Ok(u16::from_le_bytes(value))
}

fn read_u32(image: &[u8], offset: usize) -> Result<u32> {
    let mut value = [0; 4];
    value.copy_from_slice(bytes(image, offset, 4)?);
    Ok(u32::from_le_bytes(value))
}

fn invalid(reason: &'static str) -> Error {
    Error::new(ErrorKind::InvalidData, reason)
}
