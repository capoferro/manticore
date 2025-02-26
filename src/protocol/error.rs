// Copyright lowRISC contributors.
// Licensed under the Apache License, Version 2.0, see LICENSE for details.
// SPDX-License-Identifier: Apache-2.0

//! Error definitions for Cerberus messages.

use core::convert::TryFrom;
use core::convert::TryInto;

use crate::crypto;
use crate::io::ReadInt as _;
use crate::io::ReadZero;
use crate::io::Write;
use crate::mem::Arena;
use crate::mem::OutOfMemory;
use crate::protocol::wire;
use crate::protocol::wire::FromWire;
use crate::protocol::wire::ToWire;
use crate::protocol::CommandType;
use crate::protocol::Message;
use crate::session;

#[cfg(doc)]
use crate::protocol;

/// An uninterpreted Cerberus Error.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct RawError {
    /// What kind of error this is.
    pub code: u8,
    /// A fixed array of "extra data" that can come with an error code.
    pub data: [u8; 4],
}

impl<'wire> FromWire<'wire> for RawError {
    fn from_wire<R: ReadZero<'wire> + ?Sized>(
        r: &mut R,
        _: &'wire dyn Arena,
    ) -> Result<Self, wire::Error> {
        let code = r.read_le()?;
        let mut data = [0; 4];
        r.read_bytes(&mut data)?;

        Ok(Self { code, data })
    }
}

impl ToWire for RawError {
    fn to_wire<W: Write>(&self, mut w: W) -> Result<(), wire::Error> {
        w.write_le(self.code)?;
        w.write_bytes(&self.data[..])?;
        Ok(())
    }
}

/// An "empty" response, indicating only that a request was executed
/// successfully.
///
/// At the Cerberus wire level, this is actually a [`RawError`] with code `0`.
///
/// This command corresponds to [`CommandType::Error`] and does not have a
/// request counterpart.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Ack;

impl Message<'_> for Ack {
    type CommandType = CommandType;
    const TYPE: CommandType = CommandType::Error;
}

impl<'wire> FromWire<'wire> for Ack {
    fn from_wire<R: ReadZero<'wire> + ?Sized>(
        r: &mut R,
        a: &'wire dyn Arena,
    ) -> Result<Self, wire::Error> {
        RawError::from_wire(r, a)?.try_into()
    }
}

impl ToWire for Ack {
    fn to_wire<W: Write>(&self, w: W) -> Result<(), wire::Error> {
        RawError::from(*self).to_wire(w)
    }
}

impl From<Ack> for RawError {
    fn from(_: Ack) -> RawError {
        RawError {
            code: 0,
            data: [0; 4],
        }
    }
}

impl TryFrom<RawError> for Ack {
    type Error = wire::Error;
    fn try_from(e: RawError) -> Result<Ack, wire::Error> {
        match e {
            RawError {
                code: 0,
                data: [0, 0, 0, 0],
            } => Ok(Ack),
            _ => Err(wire::Error::OutOfRange),
        }
    }
}

/// A Cerberus error.
///
/// These errors can either be "generic", meaning they are not specific to a
/// particular message type; specific to a particular message type, or unknown.
///
/// For the time being, Manticore uses the following wire format for its errors:
/// - Errors that Cerberus specified are encoded as-is.
/// - Manticore-defined generic errors are encoded as an `Unspecified` error
///   where the first byte of the payload specifies which error it is.
/// - Manticore-defined, message-specific errors are encoded as an `Unspecified`
///   error with the first byte of the payload `0xff` and the second by specifying
///   the error.
/// - All other `Unspecified` errors are encoded as-is.
///
/// This type will typically be accessed via the [`protocol::Error`] alias.
///
/// [`protocol::Error`]: super::Error
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum Error<E> {
    /// Indicates that the device is "busy", usually meaning that other
    /// commands are being serviced.
    Busy,

    /// Indicates that resources were exhausted during processing of a
    /// request. This can include memory exhaustion.
    ///
    /// This is a Manticore-specific error.
    ResourceLimit,

    /// Indicates that a request's structure was malformed.
    ///
    /// This is a Manticore-specific error.
    Malformed,

    /// Indicates that the request included an index (such as a certificate
    /// slot, a PMR index, or a port) which was out of range.
    ///
    /// This is a Manticore-specific error.
    OutOfRange,

    /// Indicates that some kind of internal error occured; this likely
    /// indicates a bug in the implementation.
    ///
    /// All `manticore::crypto` errors get folded into this error by default.
    ///
    /// This is a Manticore-specific error.
    Internal,

    /// An error specific to a particular message type, if there are any.
    Specific(E),

    /// Indicates an unspecified, vendor-defined error, which may include
    /// extra unformatted data.
    Unspecified([u8; 4]),

    /// An error that Manticore does not understand.
    Unknown(RawError),
}

impl<'wire, E: SpecificError> FromWire<'wire> for Error<E> {
    fn from_wire<R: ReadZero<'wire> + ?Sized>(
        r: &mut R,
        a: &'wire dyn Arena,
    ) -> Result<Self, wire::Error> {
        let error = RawError::from_wire(r, a)?;

        match error {
            RawError {
                code: 3,
                data: [0, 0, 0, 0],
            } => Ok(Self::Busy),
            RawError {
                code: 4,
                data: [b, 0, 0, 0],
            } => match b {
                1 => Ok(Self::ResourceLimit),
                2 => Ok(Self::Malformed),
                3 => Ok(Self::OutOfRange),
                4 => Ok(Self::Internal),
                _ => Err(wire::Error::OutOfRange),
            },
            RawError {
                code: 4,
                data: [0xff, code, 0, 0],
            } => Ok(Self::Specific(E::from_raw(code)?)),
            RawError { code: 4, data } => Ok(Self::Unspecified(data)),
            _ => Ok(Self::Unknown(error)),
        }
    }
}

impl<E: SpecificError> ToWire for Error<E> {
    fn to_wire<W: Write>(&self, w: W) -> Result<(), wire::Error> {
        let raw = match self {
            Self::Busy => RawError {
                code: 3,
                data: [0; 4],
            },
            Self::ResourceLimit => RawError {
                code: 4,
                data: [1, 0, 0, 0],
            },
            Self::Malformed => RawError {
                code: 4,
                data: [2, 0, 0, 0],
            },
            Self::OutOfRange => RawError {
                code: 4,
                data: [3, 0, 0, 0],
            },
            Self::Internal => RawError {
                code: 4,
                data: [4, 0, 0, 0],
            },
            Self::Specific(e) => RawError {
                code: 4,
                data: [0xff, e.to_raw()?, 0, 0],
            },
            Self::Unspecified(data) => RawError {
                code: 4,
                data: *data,
            },
            Self::Unknown(e) => *e,
        };

        raw.to_wire(w)
    }
}

impl<E> From<OutOfMemory> for Error<E> {
    fn from(_: OutOfMemory) -> Self {
        Self::ResourceLimit
    }
}

impl<E> From<crypto::csrng::Error> for Error<E> {
    fn from(_: crypto::csrng::Error) -> Self {
        Self::Internal
    }
}

impl<E> From<crypto::hash::Error> for Error<E> {
    fn from(_: crypto::hash::Error) -> Self {
        Self::Internal
    }
}

impl<E> From<crypto::sig::Error> for Error<E> {
    fn from(_: crypto::sig::Error) -> Self {
        Self::Internal
    }
}

impl<E> From<session::Error> for Error<E> {
    fn from(_: session::Error) -> Self {
        Self::Internal
    }
}

impl<E: SpecificError> From<E> for Error<E> {
    fn from(e: E) -> Self {
        Self::Specific(e)
    }
}

/// A type that describes a message-specific error.
///
/// This trait is an implementation detail.
#[doc(hidden)]
pub trait SpecificError: Sized {
    /// Converts an error into this error type, if it represents one.
    fn from_raw(code: u8) -> Result<Self, wire::Error>;

    /// Copies this error into an error code.
    fn to_raw(&self) -> Result<u8, wire::Error>;
}

/// An empty [`SpecificError`], for use with messages without interesting error
/// messages.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum NoSpecificError {}
impl SpecificError for NoSpecificError {
    fn from_raw(_: u8) -> Result<Self, wire::Error> {
        Err(wire::Error::OutOfRange)
    }

    fn to_raw(&self) -> Result<u8, wire::Error> {
        match *self {}
    }
}

/// Helper for creating specific errors. Compare `wire_enum!`.
macro_rules! specific_error {
    (
        $(#[$emeta:meta])*
        $vis:vis enum $name:ident {$(
            $(#[$vmeta:meta])*
            $var:ident = $code:literal
        ),* $(,)*}
    ) => {
        #[derive(Clone, Copy, PartialEq, Eq, Debug)]
        #[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
        $(#[$emeta])*
        $vis enum $name {$(
            $(#[$vmeta])*
            $var,
        )*}

        impl $crate::protocol::error::SpecificError for $name {
            fn from_raw(code: u8) -> Result<Self, $crate::protocol::wire::Error> {
                match code {
                    $($code => Ok(Self::$var),)*
                    _ => Err($crate::protocol::wire::Error::OutOfRange),
                }
            }
            fn to_raw(&self) -> Result<u8, $crate::protocol::wire::Error> {
                match self {$(
                    Self::$var => Ok($code),
                )*}
            }
        }
    };
}

specific_error! {
    /// Errors specific to the [`protocol::Challenge`] and related messages.
    pub enum ChallengeError {
        /// The requested certificate chain does not exist.
        UnknownChain = 0x00,
    }
}
