// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::{decode_error, Result};
use symphonia_core::io::ReadBytes;

use crate::atoms::{Atom, AtomHeader};

fn parse_language(code: u16) -> String {
    // An ISO language code outside of these bounds is not valid.
    if code < 0x400 || code > 0x7fff {
        String::new()
    }
    else {
        let chars = [
            ((code >> 10) & 0x1f) as u8 + 0x60,
            ((code >> 5) & 0x1f) as u8 + 0x60,
            ((code >> 0) & 0x1f) as u8 + 0x60,
        ];

        String::from_utf8_lossy(&chars).to_string()
    }
}

/// Media header atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct MdhdAtom {
    /// Creation time.
    pub ctime: u64,
    /// Modification time.
    pub mtime: u64,
    /// Timescale.
    pub timescale: u32,
    /// Duration of the media in timescale units.
    pub duration: u64,
    /// Language.
    pub language: String,
}

impl Atom for MdhdAtom {
    async fn read<B: ReadBytes>(reader: &mut B, mut header: AtomHeader) -> Result<Self> {
        let (version, _) = header.read_extended_header(reader).await?;

        let mut mdhd =
            MdhdAtom { ctime: 0, mtime: 0, timescale: 0, duration: 0, language: String::new() };

        match version {
            0 => {
                mdhd.ctime = u64::from(reader.read_be_u32().await?);
                mdhd.mtime = u64::from(reader.read_be_u32().await?);
                mdhd.timescale = reader.read_be_u32().await?;
                // 0xffff_ffff is a special case.
                mdhd.duration = match reader.read_be_u32().await? {
                    u32::MAX => u64::MAX,
                    duration => u64::from(duration),
                };
            }
            1 => {
                mdhd.ctime = reader.read_be_u64().await?;
                mdhd.mtime = reader.read_be_u64().await?;
                mdhd.timescale = reader.read_be_u32().await?;
                mdhd.duration = reader.read_be_u64().await?;
            }
            _ => {
                return decode_error("isomp4: invalid mdhd version");
            }
        }

        mdhd.language = parse_language(reader.read_be_u16().await?);

        // Quality
        let _ = reader.read_be_u16().await?;

        Ok(mdhd)
    }
}
