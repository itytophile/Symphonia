// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::{decode_error, Result};
use symphonia_core::io::ReadBytes;

use crate::atoms::{Atom, AtomHeader};

/// Movie extends header atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct MehdAtom {
    /// Fragment duration.
    pub fragment_duration: u64,
}

impl Atom for MehdAtom {
    async fn read<B: ReadBytes>(reader: &mut B, mut header: AtomHeader) -> Result<Self> {
        let (version, _) = header.read_extended_header(reader).await?;

        let fragment_duration = match version {
            0 => u64::from(reader.read_be_u32().await?),
            1 => reader.read_be_u64().await?,
            _ => {
                return decode_error("isomp4: invalid mehd version");
            }
        };

        Ok(MehdAtom { fragment_duration })
    }
}
