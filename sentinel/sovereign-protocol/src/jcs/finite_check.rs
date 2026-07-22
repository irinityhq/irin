//! Non-finite float guard for the JCS signing path (W5 P0).
//!
//! `serde_json` silently collapses every non-finite float (`NaN`,
//! `+Infinity`, `-Infinity`) to JSON `null` at the `to_value` boundary, so by
//! the time [`crate::jcs::to_jcs_bytes`] holds a `serde_json::Value` the
//! information that a field was non-finite is already lost — a `Value::Null` is
//! indistinguishable from an intentional `null`. To return a clean error instead
//! of silently signing a money/cost record with a collapsed field, we run the
//! TYPED value through this validation-only [`serde::Serializer`] first. It
//! produces no output: it visits every leaf and returns `Err(NonFinite)` the
//! moment a non-finite f32/f64 is reached, and `Ok(())` otherwise.
//!
//! This is deliberately a separate, tiny serializer rather than a feature flag
//! on serde_json: it keeps the guard explicit, dependency-free, and unit-testable
//! independent of the canonical encoder.

use serde::{Serialize, ser};
use std::fmt::Display;

use crate::jcs::JcsError;

/// Validate that `value` contains no non-finite float anywhere in its tree.
/// Returns `Ok(())` if every float leaf is finite (or there are no floats).
pub(crate) fn check_finite<T: Serialize + ?Sized>(value: &T) -> Result<(), JcsError> {
    value.serialize(FiniteCheck).map_err(JcsError::from)
}

/// A `serde::Serializer` that emits nothing and only fails on non-finite floats.
pub(crate) struct FiniteCheck;

/// The error type for [`FiniteCheck`]. `Custom` carries serde's own error
/// messages (e.g. from a `Serialize` impl) so they are not lost; `NonFinite`
/// is the signal we actually act on.
#[derive(Debug)]
pub(crate) enum FiniteCheckError {
    NonFinite,
    Custom(String),
}

impl Display for FiniteCheckError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FiniteCheckError::NonFinite => write!(f, "non-finite float"),
            FiniteCheckError::Custom(m) => write!(f, "{m}"),
        }
    }
}

impl std::error::Error for FiniteCheckError {}

impl ser::Error for FiniteCheckError {
    fn custom<T: Display>(msg: T) -> Self {
        FiniteCheckError::Custom(msg.to_string())
    }
}

impl From<FiniteCheckError> for JcsError {
    fn from(e: FiniteCheckError) -> Self {
        match e {
            FiniteCheckError::NonFinite => JcsError::NonFinite,
            // A custom error during the finite walk means the value's own
            // Serialize impl failed; surface it as a serde error to preserve the
            // message. (serde_json::to_value would also fail on it downstream.)
            FiniteCheckError::Custom(m) => {
                JcsError::Serde(<serde_json::Error as serde::de::Error>::custom(m))
            }
        }
    }
}

type R = Result<(), FiniteCheckError>;

impl ser::Serializer for FiniteCheck {
    type Ok = ();
    type Error = FiniteCheckError;
    type SerializeSeq = Self;
    type SerializeTuple = Self;
    type SerializeTupleStruct = Self;
    type SerializeTupleVariant = Self;
    type SerializeMap = Self;
    type SerializeStruct = Self;
    type SerializeStructVariant = Self;

    fn serialize_f32(self, v: f32) -> R {
        if v.is_finite() {
            Ok(())
        } else {
            Err(FiniteCheckError::NonFinite)
        }
    }
    fn serialize_f64(self, v: f64) -> R {
        if v.is_finite() {
            Ok(())
        } else {
            Err(FiniteCheckError::NonFinite)
        }
    }

    // All non-float scalars: nothing to check.
    fn serialize_bool(self, _v: bool) -> R {
        Ok(())
    }
    fn serialize_i8(self, _v: i8) -> R {
        Ok(())
    }
    fn serialize_i16(self, _v: i16) -> R {
        Ok(())
    }
    fn serialize_i32(self, _v: i32) -> R {
        Ok(())
    }
    fn serialize_i64(self, _v: i64) -> R {
        Ok(())
    }
    fn serialize_i128(self, _v: i128) -> R {
        Ok(())
    }
    fn serialize_u8(self, _v: u8) -> R {
        Ok(())
    }
    fn serialize_u16(self, _v: u16) -> R {
        Ok(())
    }
    fn serialize_u32(self, _v: u32) -> R {
        Ok(())
    }
    fn serialize_u64(self, _v: u64) -> R {
        Ok(())
    }
    fn serialize_u128(self, _v: u128) -> R {
        Ok(())
    }
    fn serialize_char(self, _v: char) -> R {
        Ok(())
    }
    fn serialize_str(self, _v: &str) -> R {
        Ok(())
    }
    fn serialize_bytes(self, _v: &[u8]) -> R {
        Ok(())
    }
    fn serialize_none(self) -> R {
        Ok(())
    }
    fn serialize_unit(self) -> R {
        Ok(())
    }
    fn serialize_unit_struct(self, _name: &'static str) -> R {
        Ok(())
    }
    fn serialize_unit_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
    ) -> R {
        Ok(())
    }

    fn serialize_some<T: ?Sized + Serialize>(self, value: &T) -> R {
        value.serialize(self)
    }
    fn serialize_newtype_struct<T: ?Sized + Serialize>(self, _name: &'static str, value: &T) -> R {
        value.serialize(self)
    }
    fn serialize_newtype_variant<T: ?Sized + Serialize>(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        value: &T,
    ) -> R {
        value.serialize(self)
    }

    fn serialize_seq(self, _len: Option<usize>) -> Result<Self::SerializeSeq, Self::Error> {
        Ok(self)
    }
    fn serialize_tuple(self, _len: usize) -> Result<Self::SerializeTuple, Self::Error> {
        Ok(self)
    }
    fn serialize_tuple_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleStruct, Self::Error> {
        Ok(self)
    }
    fn serialize_tuple_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeTupleVariant, Self::Error> {
        Ok(self)
    }
    fn serialize_map(self, _len: Option<usize>) -> Result<Self::SerializeMap, Self::Error> {
        Ok(self)
    }
    fn serialize_struct(
        self,
        _name: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStruct, Self::Error> {
        Ok(self)
    }
    fn serialize_struct_variant(
        self,
        _name: &'static str,
        _variant_index: u32,
        _variant: &'static str,
        _len: usize,
    ) -> Result<Self::SerializeStructVariant, Self::Error> {
        Ok(self)
    }
}

// All compound impls recurse into their elements with a fresh FiniteCheck and
// short-circuit on the first non-finite leaf.

impl ser::SerializeSeq for FiniteCheck {
    type Ok = ();
    type Error = FiniteCheckError;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> R {
        value.serialize(FiniteCheck)
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl ser::SerializeTuple for FiniteCheck {
    type Ok = ();
    type Error = FiniteCheckError;
    fn serialize_element<T: ?Sized + Serialize>(&mut self, value: &T) -> R {
        value.serialize(FiniteCheck)
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl ser::SerializeTupleStruct for FiniteCheck {
    type Ok = ();
    type Error = FiniteCheckError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> R {
        value.serialize(FiniteCheck)
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl ser::SerializeTupleVariant for FiniteCheck {
    type Ok = ();
    type Error = FiniteCheckError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, value: &T) -> R {
        value.serialize(FiniteCheck)
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl ser::SerializeMap for FiniteCheck {
    type Ok = ();
    type Error = FiniteCheckError;
    fn serialize_key<T: ?Sized + Serialize>(&mut self, key: &T) -> R {
        // Keys are stringified by serde_json; still walk them (cannot be a bare
        // float key in JSON, but recursing is harmless and complete).
        key.serialize(FiniteCheck)
    }
    fn serialize_value<T: ?Sized + Serialize>(&mut self, value: &T) -> R {
        value.serialize(FiniteCheck)
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl ser::SerializeStruct for FiniteCheck {
    type Ok = ();
    type Error = FiniteCheckError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, _key: &'static str, value: &T) -> R {
        value.serialize(FiniteCheck)
    }
    fn end(self) -> R {
        Ok(())
    }
}

impl ser::SerializeStructVariant for FiniteCheck {
    type Ok = ();
    type Error = FiniteCheckError;
    fn serialize_field<T: ?Sized + Serialize>(&mut self, _key: &'static str, value: &T) -> R {
        value.serialize(FiniteCheck)
    }
    fn end(self) -> R {
        Ok(())
    }
}
