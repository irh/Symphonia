// Sonata
// Copyright (c) 2019 The Sonata Project Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use std::io;

use sonata_core::errors::{Result, decode_error, unsupported_error};
use sonata_core::io::*;

mod frames;

#[derive(Debug)]
enum TagSizeRestriction {
    Max128Frames1024KiB,
    Max64Frames128KiB,
    Max32Frames40KiB,
    Max32Frames4KiB,
}

#[derive(Debug)]
enum TextEncodingRestriction {
    None,
    Utf8OrIso88591,
}

#[derive(Debug)]
enum TextFieldSize {
    None,
    Max1024Characters,
    Max128Characters,
    Max30Characters,
}

#[derive(Debug)]
enum ImageEncodingRestriction {
    None,
    PngOrJpegOnly,
}

#[derive(Debug)]
enum ImageSizeRestriction {
    None,
    LessThan256x256,
    LessThan64x64,
    Exactly64x64,
}

#[derive(Debug)]
struct Header {
    major_version: u8,
    minor_version: u8,
    size: u32,
    unsynchronisation: bool,
    has_extended_header: bool,
    experimental: bool,
    has_footer: bool,
}

#[derive(Debug)]
struct Restrictions {
    tag_size: TagSizeRestriction,
    text_encoding: TextEncodingRestriction,
    text_field_size: TextFieldSize,
    image_encoding: ImageEncodingRestriction,
    image_size: ImageSizeRestriction,
}

#[derive(Debug)]
struct ExtendedHeader {
    /// ID3v2.3 only, the number of padding bytes.
    padding_size: Option<u32>,
    /// ID3v2.3+, a CRC32 checksum of the Tag.
    crc32: Option<u32>,
    /// ID3v2.4 only, is this Tag an update to an earlier Tag.
    is_update: Option<bool>,
    /// ID3v2.4 only, Tag modification restrictions.
    restrictions: Option<Restrictions>,
}

fn read_syncsafe_leq32<B: Bytestream>(reader: &mut B, bit_width: u32) -> Result<u32> {
    debug_assert!(bit_width <= 32);

    let mut result = 0u32;
    let mut bits_read = 0;

    while bits_read < bit_width {
        bits_read += 7;
        result |= ((reader.read_u8()? & 0x7f) as u32) << (bit_width - bits_read);
    }

    Ok(result & (0xffffffff >> (32 - bit_width)))
}

fn decode_unsynchronisation<'a>(buf: &'a mut [u8]) -> &'a mut [u8] {
    let len = buf.len();
    let mut src = 0;
    let mut dst = 0;

    // Decode the unsynchronisation scheme in-place.
    while src < len - 1 {
        buf[dst] = buf[src];
        dst += 1;
        src += 1;

        if buf[src - 1] == 0xff && buf[src] == 0x00 {
            src += 1;
        }
    }

    if src < len {
        buf[dst] = buf[src];
        dst += 1;
    }

    &mut buf[..dst]
}

struct UnsyncStream<B: Bytestream + FiniteStream> {
    inner: B,
    byte: u8,
}

impl<B: Bytestream + FiniteStream> UnsyncStream<B> {
    pub fn new(inner: B) -> Self {
        UnsyncStream {
            inner,
            byte: 0,
        }
    }
}

impl<B: Bytestream + FiniteStream> FiniteStream for UnsyncStream<B> {
    #[inline(always)]
    fn len(&self) -> u64 {
        self.inner.len()
    }

    #[inline(always)]
    fn bytes_read(&self) -> u64 {
        self.inner.bytes_read()
    }
    
    #[inline(always)]
    fn bytes_available(&self) -> u64 {
        self.inner.bytes_available()
    }
}

impl<B: Bytestream + FiniteStream> Bytestream for UnsyncStream<B> {

    fn read_byte(&mut self) -> io::Result<u8> {
        let last = self.byte;

        self.byte = self.inner.read_byte()?;

        // If the last byte was 0xff, and the current byte is 0x00, the current byte should be dropped and the next 
        // byte read instead.
        if last == 0xff && self.byte == 0x00 {
            self.byte = self.inner.read_byte()?;
        }

        Ok(self.byte)
    }

    fn read_double_bytes(&mut self) -> io::Result<[u8; 2]> {
        Ok([
            self.read_byte()?,
            self.read_byte()?,
        ])
    }

    fn read_triple_bytes(&mut self) -> io::Result<[u8; 3]> {
        Ok([
            self.read_byte()?,
            self.read_byte()?,
            self.read_byte()?,
        ])
    }

    fn read_quad_bytes(&mut self) -> io::Result<[u8; 4]> {
        Ok([
            self.read_byte()?,
            self.read_byte()?,
            self.read_byte()?,
            self.read_byte()?,
        ])
    }

    fn read_buf_bytes(&mut self, buf: &mut [u8]) -> io::Result<()> {
        let len = buf.len();

        if len > 0 { 
            // Fill the provided buffer directly from the underlying reader.
            self.inner.read_buf_bytes(buf)?;

            // If the last seen byte was 0xff, and the first byte in buf is 0x00, skip the first byte of buf.
            let mut src = if self.byte == 0xff && buf[0] == 0x00 { 1 } else { 0 };
            let mut dst = 0;

            // Record the last byte in buf to continue unsychronisation streaming later.
            self.byte = buf[len - 1];

            // Decode the unsynchronisation scheme in-place.
            while src < len - 1 {
                buf[dst] = buf[src];
                dst += 1;
                src += 1;

                if buf[src - 1] == 0xff && buf[src] == 0x00 {
                    src += 1;
                }
            }

            // When the final two src bytes are [ 0xff, 0x00 ], src will always equal len. Therefore, if src < len, then
            // the final byte should always be copied to dst.
            if src < len {
                buf[dst] = buf[src];
                dst += 1;
            }

            // If dst < len, then buf is not full. Read the remaining bytes manually to completely fill buf.
            while dst < len {
                buf[dst] = self.read_byte()?;
                dst += 1;
            }
        }

        Ok(())
    }

    fn ignore_bytes(&mut self, count: u64) -> io::Result<()> {
        for _ in 0..count {
            self.inner.read_byte()?;
        }
        Ok(())
    }
}

/// Read the header of an ID3v2 (verions 2.2+) tag.
fn read_id3v2_header<B: Bytestream>(reader: &mut B) -> Result<Header> {
    let marker = reader.read_triple_bytes()?;

    if marker != *b"ID3" {
        return unsupported_error("Not an ID3v2 tag.");
    }

    let major_version = reader.read_u8()?;
    let minor_version = reader.read_u8()?;
    let flags = reader.read_u8()?;
    let size = read_syncsafe_leq32(reader, 28)?;

    let mut header = Header {
        major_version,
        minor_version,
        size,
        unsynchronisation: false,
        has_extended_header: false,
        experimental: false,
        has_footer: false,
    };

    // Major and minor version numbers should never equal 0xff as per the specification. 
    if major_version == 0xff || minor_version == 0xff {
        return decode_error("Invalid version number(s).");
    }

    // Only support versions 2.2.x (first version) to 2.4.x (latest version as of May 2019) of the specification.
    if major_version < 2 || major_version > 4 {
        return unsupported_error("Unsupported ID3v2 version.");
    }

    // Version 2.2 of the standard specifies a compression flag bit, but does not specify a compression standard. 
    // Future versions of the standard remove this feature and repurpose this bit for other features. Since there is
    // no way to know how to handle the remaining tag data, return an unsupported error.
    if major_version == 2 && (flags & 0x40) != 0 {
        return unsupported_error("ID3v2.2 compression is not supported.");
    }

    // With the exception of the compression flag in version 2.2, flags were added sequentially each major version.
    // Check each bit sequentially as they appear in each version.
    if major_version >= 2 {
        header.unsynchronisation = flags & 0x80 != 0;
    }

    if major_version >= 3 {
        header.has_extended_header = flags & 0x40 != 0;
        header.experimental = flags & 0x20 != 0;
    }

    if major_version >= 4 {
        header.has_footer = flags & 0x10 != 0;
    }

    Ok(header)
}

/// Read the extended header of an ID3v2.3 tag.
fn read_id3v2p3_extended_header<B: Bytestream>(reader: &mut B) -> Result<ExtendedHeader> {
    let size = reader.read_u32()?;
    let flags = reader.read_u16()?;
    let padding_size = reader.read_u32()?;

    let mut header = ExtendedHeader {
        padding_size: Some(padding_size),
        crc32: None,
        is_update: None,
        restrictions: None,
    };

    // CRC32 flag.
    if (flags & 0x8000) == 0x8000 {
        header.crc32 = Some(reader.read_u32()?);
    }

    Ok(header)
}

/// Read the extended header of an ID3v2.4 tag.
fn read_id3v2p4_extended_header<B: Bytestream>(reader: &mut B) -> Result<ExtendedHeader> {
    let size = read_syncsafe_leq32(reader, 28)?;
    
    if reader.read_u8()? != 1 {
        return decode_error("Extended flags should have a length of 1.");
    }

    let flags = reader.read_u8()?;

    let mut header = ExtendedHeader {
        padding_size: None,
        crc32: None,
        is_update: Some(false),
        restrictions: None,
    };

    // Tag is an update flag.
    if (flags & 0x40) == 0x40 {
        let len = reader.read_u8()?;
        if len != 1 {
            return decode_error("Is update extended flag has invalid size.");
        }

        header.is_update = Some(true);
    }

    // CRC32 flag.
    if (flags & 0x20) == 0x20 {
        let len = reader.read_u8()?;
        if len != 5 {
            return decode_error("CRC32 extended flag has invalid size.");
        }

        header.crc32 = Some(read_syncsafe_leq32(reader, 32)?);
    }

    // Restrictions flag.
    if (flags & 0x10) == 0x10 {
        let len = reader.read_u8()?;
        if len != 1 {
            return decode_error("Restrictions extended flag has invalid size.");
        }

        let restrictions = reader.read_u8()?;

        let tag_size = match (restrictions & 0xc0) >> 6 {
            0 => TagSizeRestriction::Max128Frames1024KiB,
            1 => TagSizeRestriction::Max64Frames128KiB,
            2 => TagSizeRestriction::Max32Frames40KiB,
            3 => TagSizeRestriction::Max32Frames4KiB,
            _ => unreachable!(),
        };

        let text_encoding = match (restrictions & 0x40) >> 5 {
            0 => TextEncodingRestriction::None,
            1 => TextEncodingRestriction::Utf8OrIso88591,
            _ => unreachable!(),
        };

        let text_field_size = match (restrictions & 0x18) >> 3 {
            0 => TextFieldSize::None,
            1 => TextFieldSize::Max1024Characters,
            2 => TextFieldSize::Max128Characters,
            3 => TextFieldSize::Max30Characters,
            _ => unreachable!(),
        };

        let image_encoding = match (restrictions & 0x04) >> 2 {
            0 => ImageEncodingRestriction::None,
            1 => ImageEncodingRestriction::PngOrJpegOnly,
            _ => unreachable!(),
        };

        let image_size = match restrictions & 0x03 {
            0 => ImageSizeRestriction::None,
            1 => ImageSizeRestriction::LessThan256x256,
            2 => ImageSizeRestriction::LessThan64x64,
            3 => ImageSizeRestriction::Exactly64x64,
            _ => unreachable!(),
        };

        header.restrictions = Some(Restrictions {
            tag_size,
            text_encoding,
            text_field_size,
            image_encoding,
            image_size,
        })
    }

    Ok(header)
}

fn read_id3v2_body<B: Bytestream + FiniteStream>(mut reader: B, header: &Header) -> Result<()> {
    // If there is an extended header, read and parse it based on the major version of the tag.
    if header.has_extended_header {
        let extended = match header.major_version {
            3 => read_id3v2p3_extended_header(&mut reader)?,
            4 => read_id3v2p4_extended_header(&mut reader)?,
            _ => unreachable!(),
        };
        eprintln!("{:#?}", &extended);
    }

    let min_frame_size = match header.major_version {
        2 => 6,
        3 | 4 => 10,
        _ => unreachable!()
    };

    loop {
        // Read frames based on the major version of the tag.
        let frame = match header.major_version {
            2 => frames::read_id3v2p2_frame(&mut reader)?,
            3 => frames::read_id3v2p3_frame(&mut reader)?,
            4 => frames::read_id3v2p4_frame(&mut reader)?,
            _ => break,
        };

        // Read frames until either the padding has been reached explicity (all 0 tag identifier), or there is not 
        // enough bytes available in the tag for another frame.
        if reader.bytes_available() < min_frame_size {
            break;
        }
    }

    Ok(())
}

pub fn read_id3v2<B: Bytestream>(reader: &mut B) -> Result<()> {
    // Read the (sorta) version agnostic tag header.
    let header = read_id3v2_header(reader)?;
    eprintln!("{:#?}", &header);

    // The header specified the byte length of the contents of the ID3v2 tag (excluding the header), use a scoped
    // reader to ensure we don't exceed that length, and to determine if there are no more frames left to parse.
    let scoped = ScopedStream::new(reader, header.size as u64);

    // If the unsynchronisation flag is set in the header, all tag data must be passed through the unsynchronisation 
    // decoder before being read for verions < 4 of ID3v2.
    if header.unsynchronisation && header.major_version < 4 {
        read_id3v2_body(UnsyncStream::new(scoped), &header)
    }
    // Otherwise, read the data as-is. Individual frames may be unsynchronised for major versions >= 4.
    else {
        read_id3v2_body(scoped, &header)
    }
}