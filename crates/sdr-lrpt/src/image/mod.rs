//! Image-assembly + PNG export for Meteor LRPT.
//!
//! Stage 4 of the receive pipeline. Consumes [`super::ccsds::ImagePacket`]s
//! from the framing layer, decodes them as Meteor reduced-JPEG
//! ([`jpeg::JpegDecoder`]), accumulates per-channel scan-line
//! buffers ([`composite::ImageAssembler`]), and exposes PNG export
//! ([`png_export::save_channel`] / [`png_export::save_composite`]).

pub mod composite;
pub mod jpeg;
pub mod png_export;

pub use composite::{ChannelBuffer, IMAGE_WIDTH, ImageAssembler, MCUS_PER_LINE};
pub use jpeg::{Block8x8, JpegDecoder, JpegError, MCU_SAMPLES, MCU_SIDE};
pub use png_export::{PngExportError, save_channel, save_composite};
