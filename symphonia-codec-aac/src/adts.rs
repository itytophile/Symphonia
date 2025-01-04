// Symphonia
// Copyright (c) 2019-2022 The Project Symphonia Developers.
//
// This Source Code Form is subject to the terms of the Mozilla Public
// License, v. 2.0. If a copy of the MPL was not distributed with this
// file, You can obtain one at https://mozilla.org/MPL/2.0/.

use futures_util::future::BoxFuture;
use futures_util::io::AllowStdIo;
use futures_util::FutureExt;
use symphonia_core::errors::{unsupported_error, Error};
use symphonia_core::{async_trait, support_format};

use symphonia_core::audio::Channels;
use symphonia_core::codecs::audio::well_known::CODEC_ID_AAC;
use symphonia_core::codecs::audio::AudioCodecParameters;
use symphonia_core::codecs::CodecParameters;
use symphonia_core::errors::{decode_error, seek_error, Result, SeekErrorKind};
use symphonia_core::formats::probe::{ProbeFormatData, ProbeableFormat, Score, Scoreable};
use symphonia_core::formats::well_known::FORMAT_ID_ADTS;
use symphonia_core::formats::{
    prelude::*, AsyncFormatReader, BlockingFormatReader, FormatReaderInfo,
};
use symphonia_core::io::*;
use symphonia_core::meta::{Metadata, MetadataLog};

use std::io::SeekFrom;

use super::common::{map_to_channels, M4AType, AAC_CHANNELS, AAC_SAMPLE_RATES, M4A_TYPES};

use log::{debug, info};

const SAMPLES_PER_AAC_PACKET: u64 = 1024;

const ADTS_FORMAT_INFO: FormatInfo = FormatInfo {
    format: FORMAT_ID_ADTS,
    short_name: "aac",
    long_name: "Audio Data Transport Stream (native AAC)",
};

/// Audio Data Transport Stream (ADTS) format reader.
///
/// `AdtsReader` implements a demuxer for ADTS (AAC native frames).
pub struct AsyncAdtsReader<'s> {
    reader: AsyncMediaSourceStream<'s>,
    tracks: Vec<Track>,
    chapters: Option<ChapterGroup>,
    metadata: MetadataLog,
    first_frame_pos: u64,
    next_packet_ts: u64,
}

pub type AdtsReader<'s> = BlockingFormatReader<AsyncAdtsReader<'s>>;

pub async fn try_new_async(
    mut mss: AsyncMediaSourceStream<'_>,
    opts: FormatOptions,
) -> Result<AsyncAdtsReader<'_>> {
    let header = AdtsHeader::read(&mut mss).await?;

    // Rewind back to the start of the frame.
    mss.seek_buffered_rev(usize::from(header.header_len()));

    // Use the header to populate the codec parameters.
    let mut codec_params = AudioCodecParameters::new();

    codec_params.for_codec(CODEC_ID_AAC).with_sample_rate(header.sample_rate);

    if let Some(channels) = header.channels {
        codec_params.with_channels(channels);
    }

    // Populat the track.
    let mut track = Track::new(0);
    track.with_codec_params(CodecParameters::Audio(codec_params));

    let first_frame_pos = mss.pos();

    if let Some(n_frames) = approximate_frame_count(&mut mss).await? {
        info!("estimating duration from bitrate, may be inaccurate for vbr files");
        track.with_num_frames(n_frames);
    }

    Ok(AsyncAdtsReader {
        reader: mss,
        tracks: vec![track],
        chapters: opts.external_data.chapters,
        metadata: opts.external_data.metadata.unwrap_or_default(),
        first_frame_pos,
        next_packet_ts: 0,
    })
}

pub fn try_new<'s>(
    source: impl MediaSource + 's,
    options: MediaSourceStreamOptions,
    opts: FormatOptions,
) -> Result<AdtsReader<'s>> {
    Ok(BlockingFormatReader::new(
        try_new_async(
            AsyncMediaSourceStream::new(Box::pin(AllowStdIo::new(source)), options),
            opts,
        )
        .now_or_never()
        .unwrap()?,
    ))
}

impl Scoreable for AsyncAdtsReader<'_> {
    fn score<'a>(
        mut src: ScopedStream<&'a mut AsyncMediaSourceStream<'_>>,
    ) -> BoxFuture<'a, Result<Score>> {
        async move {
            // Read the first (assumed) ADTS header.
            let hdr1 = AdtsHeader::read_no_resync(&mut src).await?;

            // Since the first header was read successfully, this may be an ADTS audio format. However,
            // if there is enough data left to read the frame body and another frame header, then a
            // higher confidence may be gained. If there is not enough data left, return a partially
            // confident score.
            let payload_len = hdr1.payload_len();

            if src.bytes_available() < u64::from(payload_len + AdtsHeader::SIZE_WITH_CRC) {
                return Ok(Score::Supported(127));
            }

            src.ignore_bytes(u64::from(payload_len)).await?;

            let _ = AdtsHeader::read_no_resync(&mut src).await?;

            Ok(Score::Supported(255))
        }
        .boxed()
    }
}

#[derive(Debug)]
#[allow(dead_code)]
struct AdtsHeader {
    /// Audio profile.
    profile: M4AType,
    /// Audio channel configuration.
    channels: Option<Channels>,
    /// The sample rate in Hertz.
    sample_rate: u32,
    /// The length of the ADTS frame in bytes including the sync word, header, and payload. Maximum
    /// value is 8kB.
    frame_len: u16,
    /// An optional CRC.
    crc: Option<u16>,
}

impl AdtsHeader {
    /// The size of the an ADTS header CRC.
    pub const CRC_SIZE: u16 = 2;
    /// The size of a ADTS header including the sync word and no a CRC.
    pub const SIZE_NO_CRC: u16 = 7;
    /// The size of a ADTS header including the sync word and CRC.
    pub const SIZE_WITH_CRC: u16 = Self::SIZE_NO_CRC + Self::CRC_SIZE;

    /// Read the body of a header at the current position of the reader.
    async fn read_body<B: ReadBytes>(reader: &mut B, has_crc: bool) -> Result<Self> {
        // The length of the header.
        let len = if has_crc { Self::SIZE_WITH_CRC } else { Self::SIZE_NO_CRC };

        // Read the body of the header (no sync word).
        let mut buf = [0; 7];
        reader.read_buf_exact(&mut buf[..usize::from(len - 2)]).await?;

        let mut bs = BitReaderLtr::new(&buf);

        // Profile.
        let profile = M4A_TYPES[bs.read_bits_leq32(2)? as usize + 1];

        // Sample rate index.
        let sample_rate = match bs.read_bits_leq32(4)? as usize {
            15 => return decode_error("adts: forbidden sample rate"),
            13 | 14 => return decode_error("adts: reserved sample rate"),
            idx => AAC_SAMPLE_RATES[idx],
        };

        // Private bit.
        bs.ignore_bit()?;

        // Channel configuration.
        let channels = match bs.read_bits_leq32(3)? {
            0 => None,
            idx => map_to_channels(AAC_CHANNELS[idx as usize]),
        };

        // Originality, Home, Copyrighted ID bit, Copyright ID start bits. Only used for encoding.
        bs.ignore_bits(4)?;

        // The frame length = sync word + header + payload.
        let frame_len = bs.read_bits_leq32(13)? as u16;

        // The frame length must be large enough for the header.
        if frame_len < len {
            return decode_error("adts: invalid adts frame length");
        }

        // Buffer fullness.
        let _fullness = bs.read_bits_leq32(11)?;

        // Number of raw data blocks (AAC packets).
        let raw_data_blocks = bs.read_bits_leq32(2)? + 1;

        if raw_data_blocks > 1 {
            // TODO: Support multiple AAC packets per ADTS packet.
            return unsupported_error("adts: only 1 aac frame per adts packet is supported");
        }

        // The CRC, if the CRC is provided.
        let crc = if has_crc { Some(bs.read_bits_leq32(16)? as u16) } else { None };

        Ok(AdtsHeader { profile, channels, sample_rate, frame_len, crc })
    }

    /// Returns true if the provided word is a valid sync word.
    #[inline(always)]
    fn is_sync_word(sync: u16) -> bool {
        (sync & 0xfff6) == 0xfff0
    }

    /// Resync the reader to the next sync word.
    async fn sync<B: ReadBytes>(reader: &mut B) -> Result<u16> {
        let mut sync = 0;

        while !Self::is_sync_word(sync) {
            sync = (sync << 8) | u16::from(reader.read_u8().await?);
        }

        Ok(sync)
    }

    /// Read a header from the current position of the reader.
    async fn read_no_resync<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        let sync = reader.read_be_u16().await?;

        if !Self::is_sync_word(sync) {
            return decode_error("adts: invalid frame sync word");
        }

        // "Protection absent" set to 0 if CRC is present.
        Self::read_body(reader, sync & 1 == 0).await
    }

    /// Resync the reader if required, and read a header.
    async fn read<B: ReadBytes>(reader: &mut B) -> Result<Self> {
        let sync = AdtsHeader::sync(reader).await?;

        // "Protection absent" set to 0 if CRC is present.
        Self::read_body(reader, sync & 1 == 0).await
    }

    /// Get the length of the header including the sync word.
    #[inline]
    fn header_len(&self) -> u16 {
        Self::SIZE_NO_CRC + if self.crc.is_some() { Self::CRC_SIZE } else { 0 }
    }

    /// Get the length of the payload.
    #[inline]
    fn payload_len(&self) -> u16 {
        self.frame_len - self.header_len()
    }
}

impl ProbeableFormat<'_> for AsyncAdtsReader<'_> {
    fn try_probe_new<'s>(
        mss: AsyncMediaSourceStream<'s>,
        opts: FormatOptions,
    ) -> BoxFuture<'s, Result<Box<dyn AsyncFormatReader + 's>>> {
        async move { Ok(Box::new(try_new_async(mss, opts).await?) as Box<dyn AsyncFormatReader>) }
            .boxed()
    }

    fn probe_data() -> &'static [ProbeFormatData] {
        &[support_format!(
            ADTS_FORMAT_INFO,
            &["aac"],
            &["audio/aac"],
            &[
                &[0xff, 0xf1], // MPEG 4 without CRC
                &[0xff, 0xf0], // MPEG 4 with CRC
                &[0xff, 0xf9], // MPEG 2 without CRC
                &[0xff, 0xf8], // MPEG 2 with CRC
            ]
        )]
    }
}

impl FormatReaderInfo for AsyncAdtsReader<'_> {
    fn format_info(&self) -> &FormatInfo {
        &ADTS_FORMAT_INFO
    }

    fn metadata(&mut self) -> Metadata<'_> {
        self.metadata.metadata()
    }

    fn chapters(&self) -> Option<&ChapterGroup> {
        self.chapters.as_ref()
    }

    fn tracks(&self) -> &[Track] {
        &self.tracks
    }
}

#[async_trait]
impl AsyncFormatReader for AsyncAdtsReader<'_> {
    async fn next_packet(&mut self) -> Result<Option<Packet>> {
        // Parse the header to get the calculated frame size.
        let header = match AdtsHeader::read(&mut self.reader).await {
            Ok(header) => header,
            Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                // ADTS streams have no well-defined end, so when no more frames can be read,
                // consider the stream ended.
                return Ok(None);
            }
            Err(err) => return Err(err),
        };

        // TODO: Support multiple AAC packets per ADTS packet.

        let ts = self.next_packet_ts;

        self.next_packet_ts += SAMPLES_PER_AAC_PACKET;

        Ok(Some(Packet::new_from_boxed_slice(
            0,
            ts,
            SAMPLES_PER_AAC_PACKET,
            self.reader.read_boxed_slice_exact(usize::from(header.payload_len())).await?,
        )))
    }

    async fn seek(&mut self, _mode: SeekMode, to: SeekTo) -> Result<SeekedTo> {
        // Get the timestamp of the desired audio frame.
        let required_ts = match to {
            // Frame timestamp given.
            SeekTo::TimeStamp { ts, .. } => ts,
            // Time value given, calculate frame timestamp using the timebase.
            SeekTo::Time { time, .. } => {
                // Use the timebase to calculate the frame timestamp. If timebase is not known, the
                // seek cannot be completed.
                if let Some(tb) = self.tracks[0].time_base {
                    tb.calc_timestamp(time)
                }
                else {
                    return seek_error(SeekErrorKind::Unseekable);
                }
            }
        };

        debug!("seeking to ts={}", required_ts);

        // If the desired timestamp is less-than the next packet timestamp, attempt to seek
        // to the start of the stream.
        if required_ts < self.next_packet_ts {
            // If the reader is not seekable then only forward seeks are possible.
            if self.reader.is_seekable() {
                let seeked_pos = self.reader.seek(SeekFrom::Start(self.first_frame_pos)).await?;

                // Since the elementary stream has no timestamp information, the position seeked
                // to must be exactly as requested.
                if seeked_pos != self.first_frame_pos {
                    return seek_error(SeekErrorKind::Unseekable);
                }
            }
            else {
                return seek_error(SeekErrorKind::ForwardOnly);
            }

            // Successfuly seeked to the start of the stream, reset the next packet timestamp.
            self.next_packet_ts = 0;
        }

        // Parse frames from the stream until the frame containing the desired timestamp is
        // reached.
        loop {
            // Parse the next frame header.
            let header = match AdtsHeader::read(&mut self.reader).await {
                Ok(header) => header,
                Err(Error::IoError(err)) if err.kind() == std::io::ErrorKind::UnexpectedEof => {
                    // ADTS streams have no well-defined end, so if no more frames can be read then
                    // assume the seek position is out-of-range.
                    return seek_error(SeekErrorKind::OutOfRange);
                }
                Err(err) => return Err(err),
            };

            // TODO: Support multiple AAC packets per ADTS packet.

            // If the next frame's timestamp would exceed the desired timestamp, rewind back to the
            // start of this frame and end the search.
            if self.next_packet_ts + SAMPLES_PER_AAC_PACKET > required_ts {
                self.reader.seek_buffered_rev(usize::from(header.header_len()));
                break;
            }

            // Otherwise, ignore the frame body.
            self.reader.ignore_bytes(u64::from(header.payload_len())).await?;

            // Increment the timestamp for the next packet.
            self.next_packet_ts += SAMPLES_PER_AAC_PACKET;
        }

        debug!(
            "seeked to ts={} (delta={})",
            self.next_packet_ts,
            required_ts as i64 - self.next_packet_ts as i64
        );

        Ok(SeekedTo { track_id: 0, required_ts, actual_ts: self.next_packet_ts })
    }
}

async fn approximate_frame_count(
    mut source: &mut AsyncMediaSourceStream<'_>,
) -> Result<Option<u64>> {
    let original_pos = source.pos();
    let total_len = match source.byte_len() {
        Some(len) => len - original_pos,
        _ => return Ok(None),
    };

    let mut parsed_n_frames = 0;
    let mut n_bytes = 0;

    if !source.is_seekable() {
        // The maximum length in bytes of frames to consume from the stream to sample.
        const MAX_LEN: u64 = 16 * 1024;

        source.ensure_seekback_buffer(MAX_LEN as usize);
        let mut scoped_stream = ScopedStream::new(&mut source, MAX_LEN);

        loop {
            let Ok(header) = AdtsHeader::read(&mut scoped_stream).await
            else {
                break;
            };

            if scoped_stream.ignore_bytes(u64::from(header.payload_len())).await.is_err() {
                break;
            }

            parsed_n_frames += 1;
            n_bytes += u64::from(header.frame_len);
        }

        let _ = source.seek_buffered(original_pos);
    }
    else {
        // The number of points to sample within the stream.
        const NUM_SAMPLE_POINTS: u64 = 4;

        let step = (total_len - original_pos) / NUM_SAMPLE_POINTS;

        // Skip the first sample point (start of file) since it is an outlier.
        for new_pos in (original_pos..total_len - step).step_by(step as usize).skip(1) {
            let res = source.seek(SeekFrom::Start(new_pos)).await;
            if res.is_err() {
                break;
            }

            for _ in 0..=100 {
                let header = match AdtsHeader::read(&mut source).await {
                    Ok(header) => header,
                    _ => break,
                };

                parsed_n_frames += 1;
                n_bytes += u64::from(header.frame_len);
            }
        }

        let _ = source.seek(SeekFrom::Start(original_pos)).await?;
    }

    debug!("adts: parsed {} of {} bytes to approximate duration", n_bytes, total_len);

    match parsed_n_frames {
        0 => Ok(None),
        _ => Ok(Some(total_len / (n_bytes / parsed_n_frames) * SAMPLES_PER_AAC_PACKET)),
    }
}
