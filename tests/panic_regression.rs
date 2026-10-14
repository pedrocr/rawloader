use std::io::Cursor;

// === The 3 original fuzzing-discovered panics ===

#[test]
fn fujifilm_short_header_no_panic() {
    let loader = rawloader::RawLoader::new();
    let result = loader.decode(&mut Cursor::new(&b"FUJIFILM"[..]), false);
    assert!(result.is_err());
}

#[test]
fn x3f_crafted_offset_no_panic() {
    let loader = rawloader::RawLoader::new();
    let mut input = vec![0xf1u8; 257];
    input[0..4].copy_from_slice(b"FOVb");
    let result = loader.decode(&mut Cursor::new(&input[..]), false);
    assert!(result.is_err());
}

#[test]
fn tiff_large_ifd_count_no_panic() {
    let loader = rawloader::RawLoader::new();
    let mut input = vec![0u8; 1026];
    input[0..4].copy_from_slice(&[0x49, 0x49, 0x2a, 0x00]); // LE TIFF
    input[4..8].copy_from_slice(&[0x08, 0x00, 0x00, 0x00]); // IFD offset 8
    input[8..10].copy_from_slice(&[0x88, 0x00]); // 136 entries
    let result = loader.decode(&mut Cursor::new(&input[..]), false);
    assert!(result.is_err());
}

// === Hardened parser returns errors, not garbage ===

#[test]
fn truncated_tiff_returns_error() {
    let loader = rawloader::RawLoader::new();
    // Valid TIFF header pointing to IFD at offset 8, but nothing there
    let input = [0x49, 0x49, 0x2a, 0x00, 0x08, 0x00, 0x00, 0x00];
    let result = loader.decode(&mut Cursor::new(&input[..]), false);
    assert!(result.is_err());
}

#[test]
fn tiff_entry_data_offset_past_buffer() {
    let loader = rawloader::RawLoader::new();
    let mut input = vec![0u8; 64];
    input[0..4].copy_from_slice(&[0x49, 0x49, 0x2a, 0x00]); // LE TIFF
    input[4..8].copy_from_slice(&[0x08, 0x00, 0x00, 0x00]); // IFD at offset 8
    input[8..10].copy_from_slice(&[0x01, 0x00]); // 1 entry
    input[10..12].copy_from_slice(&[0x00, 0x01]); // tag = ImageWidth
    input[12..14].copy_from_slice(&[0x04, 0x00]); // type = LONG
    input[14..18].copy_from_slice(&[0x01, 0x00, 0x00, 0x00]); // count = 1
    input[18..22].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF]); // data offset past buffer
    let result = loader.decode(&mut Cursor::new(&input[..]), false);
    assert!(result.is_err());
}
