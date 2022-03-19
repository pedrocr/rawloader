use crate::decoders::{basics::*, tiff::*};
use std::collections::*;

/// EXIF information access interface
pub trait ExifInfo {
  fn get_uint(&self, tag: Tag) -> Option<u32>;
  fn get_rational(&self, tag: Tag) -> Option<f32>;
  fn get_str(&self, tag: Tag) -> Option<&str>;
  fn to_string(&self, tag: Tag) -> Option<String>;
  fn get_tags(&self) -> Vec<Tag>;

  fn make_clone(&self) -> Box<dyn ExifInfo>;
  fn make_fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result;
}

impl core::fmt::Debug for dyn ExifInfo {
  fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    self.make_fmt(f)
  }
}

impl Clone for Box<dyn ExifInfo> {
  fn clone(&self) -> Self {
    self.make_clone()
  }
}

#[derive(Clone, Debug)]
struct NativeExitItem {
  typ: u16,
  data: Vec<u8>,
  endian: Endian,
}

#[derive(Debug)]
pub struct NativeExifInfo {
  items: HashMap<u16, NativeExitItem>,
}

impl NativeExifInfo {
  pub (crate) fn new(ifd: &TiffIFD) -> Box<NativeExifInfo> {
    let exif_tags = [
      Tag::ExifVersion,
      Tag::Artist, Tag::Make, Tag::Model, Tag::Orientation, Tag::Software,
      Tag::ShutterSpeedValue, Tag::ApertureValue, Tag::ExposureBiasValue,
      Tag::ExposureTime, Tag::ISOSpeed, Tag::FNumber,
      Tag::FocalLength, Tag::ExposureProgram,
      Tag::MeteringMode, Tag::Flash,
      Tag::Copyright, Tag::DateTimeOriginal,
      Tag::DateTimeDigitized, Tag::DateTime,
      Tag::LensMake, Tag::LensModel, Tag::LensSerialNumber,
    ];

    let mut items = HashMap::new();

    for tag in exif_tags {
      if let Some(entry) = ifd.find_entry(tag) {
        items.insert(tag as u16, NativeExitItem {
          typ: entry.typ,
          data: entry.data.to_vec(),
          endian: entry.endian,
        });
      }
    }

    Box::new(NativeExifInfo { items })
  }
}

impl ExifInfo for NativeExifInfo {
  fn get_uint(&self, tag: Tag) -> Option<u32> {
    match self.items.get(&(tag as u16)) {
      Some(item) =>
        Some(get_u32_entry_val(item.typ, &item.data, item.endian, 0)),
      None =>
        None,
    }
  }

  fn get_rational(&self, tag: Tag) -> Option<f32> {
    match self.items.get(&(tag as u16)) {
      Some(item) =>
        Some(get_f32_entry_val(item.typ, &item.data, item.endian, 0)),
      None =>
        None,
    }
  }

  fn get_str(&self, tag: Tag) -> Option<&str> {
    match self.items.get(&(tag as u16)) {
      Some(item) =>
        Some(get_str_entry_val(item.typ, &item.data)),
      None =>
        None,
    }
  }

  fn to_string(&self, tag: Tag) -> Option<String> {
    match self.items.get(&(tag as u16)) {
      Some(item) =>
        Some(get_entry_val_as_string(
          item.typ,
          &item.data,
          item.endian,
          0
        )),
      None =>
        None,
    }
  }

  fn get_tags(&self) -> Vec<Tag> {
    self.items
      .keys()
      .map(|v| Tag::n(*v).unwrap())
      .collect()
  }

  fn make_clone(&self) -> Box<dyn ExifInfo> {
    Box::new(NativeExifInfo {
      items: self.items.clone(),
    })
  }

  fn make_fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
    writeln!(f, "{:?}", self.items)
  }
}
