// Copyright lowRISC contributors.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

//! `Challenge` request and response.
//!
//! This module provides the Cerberus challenge command.

use core::convert::TryInto as _;

use crate::io::read::ReadZeroExt as _;
use crate::io::ReadInt as _;
use crate::io::ReadZero;
use crate::io::Write;
use crate::mem::Arena;
use crate::protocol::error::ChallengeError;
use crate::protocol::wire;
use crate::protocol::wire::FromWire;
use crate::protocol::wire::ToWire;
use crate::protocol::CommandType;

protocol_struct! {
    /// A command for challenging an RoT.
    type Challenge;
    type Error = ChallengeError;
    const TYPE: CommandType = Challenge;

    struct Request<'wire> {
        /// The slot number of the chain to read from.
        pub slot: u8,
        /// A requester-chosen random nonce.
        #[cfg_attr(feature = "serde", serde(
            serialize_with = "crate::serde::se_hexstring",
        ))]
        #[@static(cfg_attr(feature = "serde", serde(
            deserialize_with = "crate::serde::de_hexstring",
        )))]
        pub nonce: &'wire [u8; 32],
    }

    fn Request::from_wire(r, arena) {
        let slot = r.read_le()?;
        let _: u8 = r.read_le()?;
        let nonce = r.read_object::<[u8; 32]>(arena)?;
        Ok(Self { slot, nonce })
    }

    fn Request::to_wire(&self, w) {
        w.write_le(self.slot)?;
        w.write_le(0u8)?;
        w.write_bytes(self.nonce)?;
        Ok(())
    }

    struct Response<'wire> {
        /// The "to be signed" portion.
        #[cfg_attr(feature = "serde", serde(flatten))]
        #[@static(cfg_attr(feature = "serde", serde(flatten)))]
        pub tbs: ChallengeResponseTbs<'wire>,
        /// The challenge signature.
        ///
        /// This is a signature over the concatenation of the corresponding
        /// request and the response up to the signature.
        #[cfg_attr(feature = "serde", serde(
            serialize_with = "crate::serde::se_hexstring",
        ))]
        #[@static(cfg_attr(feature = "serde", serde(
            deserialize_with = "crate::serde::de_hexstring",
        )))]
        pub signature: &'wire [u8],
    }

    fn Response::from_wire(r, arena) {
        let tbs = ChallengeResponseTbs::from_wire(r, arena)?;
        let signature = r.read_slice::<u8>(r.remaining_data(), arena)?;
        Ok(Self { tbs, signature })
    }

    fn Response::to_wire(&self, w) {
        self.tbs.to_wire(&mut w)?;
        w.write_bytes(self.signature)?;
        Ok(())
    }
}

#[cfg(feature = "arbitrary-derive")]
use libfuzzer_sys::arbitrary::{self, Arbitrary};

derive_borrowed! {
    /// The portion of the [`Challenge`] response that is incorporated into
    /// the signature.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    #[cfg_attr(feature = "serde", derive(serde::Serialize))]
    #[@static(
        derive(Clone, PartialEq, Eq, Debug),
        cfg_attr(feature = "serde", derive(serde::Deserialize)),
        cfg_attr(feature = "arbitrary-derive", derive(Arbitrary)),
    )]
    pub struct ChallengeResponseTbs<'wire> {
        /// The slot number of the chain to read from.
        pub slot: u8,
        /// The "certificate slot mask" (Cerberus does not elaborate further).
        ///
        /// Manticore ignores this value.
        pub slot_mask: u8,
        /// The minimum and maximum versions of Cerberus supported.
        ///
        /// Manticore ignores this value.
        pub protocol_range: (u8, u8),
        /// A responder-chosen random nonce.
        #[cfg_attr(feature = "serde", serde(
            serialize_with = "crate::serde::se_hexstring",
        ))]
        #[@static(cfg_attr(feature = "serde", serde(
            deserialize_with = "crate::serde::de_hexstring",
        )))]
        pub nonce: &'wire [u8; 32],
        /// The number of "components" used to generate PMR0.
        pub pmr0_components: u8,
        /// The value of the PMR0 measurement.
        #[cfg_attr(feature = "serde", serde(
            serialize_with = "crate::serde::se_hexstring",
        ))]
        #[@static(cfg_attr(feature = "serde", serde(
            deserialize_with = "crate::serde::de_hexstring",
        )))]
        pub pmr0: &'wire [u8],
    }
}

impl ChallengeResponseTbs<'_> {
    /// Runs `f` with this message "serialized" as an iovec.
    ///
    /// The main purpose of this function is for implementing signing of the
    /// challenge response without needless allocation.
    pub(crate) fn as_iovec_with<R>(
        &self,
        f: impl FnOnce([&[u8]; 4]) -> R,
    ) -> R {
        f([
            &[
                self.slot,
                self.slot_mask,
                self.protocol_range.0,
                self.protocol_range.1,
                // The two reserved bytes.
                0,
                0,
            ],
            self.nonce,
            &[self.pmr0_components, self.pmr0.len() as u8],
            self.pmr0,
        ])
    }
}

impl<'wire> FromWire<'wire> for ChallengeResponseTbs<'wire> {
    fn from_wire<R: ReadZero<'wire> + ?Sized>(
        r: &mut R,
        arena: &'wire dyn Arena,
    ) -> Result<Self, wire::Error> {
        let slot = r.read_le()?;
        let slot_mask = r.read_le()?;
        let min_version = r.read_le()?;
        let max_version = r.read_le()?;
        let _: u16 = r.read_le()?;

        let nonce = r.read_object::<[u8; 32]>(arena)?;

        let pmr0_components = r.read_le()?;
        let pmr0_len = r.read_le::<u8>()?;
        let pmr0 = r.read_slice::<u8>(pmr0_len as usize, arena)?;

        Ok(Self {
            slot,
            slot_mask,
            protocol_range: (min_version, max_version),
            nonce,
            pmr0_components,
            pmr0,
        })
    }
}

impl ToWire for ChallengeResponseTbs<'_> {
    fn to_wire<W: Write>(&self, mut w: W) -> Result<(), wire::Error> {
        w.write_le(self.slot)?;
        w.write_le(self.slot_mask)?;
        w.write_le(self.protocol_range.0)?;
        w.write_le(self.protocol_range.1)?;
        w.write_le(0u16)?;
        w.write_bytes(self.nonce)?;
        w.write_le(self.pmr0_components)?;
        w.write_le::<u8>(
            self.pmr0
                .len()
                .try_into()
                .map_err(|_| wire::Error::OutOfRange)?,
        )?;
        w.write_bytes(self.pmr0)?;
        Ok(())
    }
}

#[cfg(test)]
mod test {
    use super::*;

    round_trip_test! {
        request_round_trip: {
            bytes: &[
                0x01, 0x00,  // Slot #, reserved.

                // Nonce.
                0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
                0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
                0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
                0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77, 0x77,
            ],
            json: r#"{
                "slot": 1,
                "nonce": "7777777777777777777777777777777777777777777777777777777777777777"
            }"#,
            value: ChallengeRequest {
                slot: 1,
                nonce: &[0x77; 32],
            },
        },
        response_round_trup: {
            bytes: &[
                0x01, 0xff, 0x05, 0x07,  // Slot #, slot mask, min, max.
                0x00, 0x00,              // Reserved.

                // Nonce.
                0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd,
                0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd,
                0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd,
                0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd, 0xdd,

                0x0a, 0x04,  // Component #, PMR len.
                b'p', b'm', b'r', b'0',

                // Signature.
                b'e', b'c', b'd', b's', b'a',
            ],
            json: r#"{
                "slot": 1,
                "slot_mask": 255,
                "protocol_range": [5, 7],
                "nonce": "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                "pmr0_components": 10,
                "pmr0": "706d7230",
                "signature": "6563647361"
            }"#,
            value: ChallengeResponse {
                tbs: ChallengeResponseTbs {
                    slot: 1,
                    slot_mask: 255,
                    protocol_range: (5, 7),
                    nonce: &[0xdd; 32],
                    pmr0_components: 10,
                    pmr0: b"pmr0",
                },
                signature: b"ecdsa",
            },
        },
    }
}
