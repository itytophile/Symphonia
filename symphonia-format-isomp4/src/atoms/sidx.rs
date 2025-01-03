// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use symphonia_core::errors::{decode_error, Error, Result};
use symphonia_core::io::ReadBytes;

use crate::atoms::{Atom, AtomHeader};

#[derive(Debug)]
pub enum ReferenceType {
    Segment,
    Media,
}

#[allow(dead_code)]
#[derive(Debug)]
pub struct SidxReference {
    pub reference_type: ReferenceType,
    pub reference_size: u32,
    pub subsegment_duration: u32,
    // pub starts_with_sap: bool,
    // pub sap_type: u8,
    // pub sap_delta_time: u32,
}

/// Segment index atom.
#[allow(dead_code)]
#[derive(Debug)]
pub struct SidxAtom {
    pub reference_id: u32,
    pub timescale: u32,
    pub earliest_pts: u64,
    pub first_offset: u64,
    pub references: Vec<SidxReference>,
}

impl Atom for SidxAtom {
    async fn read<B: ReadBytes>(reader: &mut B, mut header: AtomHeader) -> Result<Self> {
        let (version, _) = header.read_extended_header(reader).await?;

        let reference_id = reader.read_be_u32().await?;
        let timescale = reader.read_be_u32().await?;

        // The anchor point for segment offsets is the first byte after this atom.
        let anchor = header
            .atom_len()
            .map(|atom_len| header.atom_pos() + atom_len.get())
            .ok_or(Error::DecodeError("isomp4 (sidx): expected atom size to be known"))?;

        let (earliest_pts, first_offset) = match version {
            0 => (
                u64::from(reader.read_be_u32().await?),
                anchor + u64::from(reader.read_be_u32().await?),
            ),
            1 => (reader.read_be_u64().await?, anchor + reader.read_be_u64().await?),
            _ => {
                return decode_error("isomp4: invalid sidx version");
            }
        };

        let _reserved = reader.read_be_u16().await?;
        let reference_count = reader.read_be_u16().await?;

        let mut references = Vec::new();

        for _ in 0..reference_count {
            let reference = reader.read_be_u32().await?;
            let subsegment_duration = reader.read_be_u32().await?;

            let reference_type = match (reference & 0x8000_0000) != 0 {
                false => ReferenceType::Media,
                true => ReferenceType::Segment,
            };

            let reference_size = reference & !0x8000_0000;

            // Ignore SAP
            let _ = reader.read_be_u32().await?;

            references.push(SidxReference { reference_type, reference_size, subsegment_duration });
        }

        Ok(SidxAtom { reference_id, timescale, earliest_pts, first_offset, references })
    }
}
