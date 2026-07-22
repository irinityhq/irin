//! CloudEvents 1.0 profile envelope for IRIN comms v0.1.
//!
//! Wire shape (`EnvelopeWrapper` ⇒ JSON):
//!
//! ```json
//! {
//!   "v": 1,
//!   "envelope": {
//!     "specversion": "1.0",
//!     "id": "<hex32>",
//!     "source": "urn:irin:sentinel:<name>",
//!     "type": "irin.escalation.v0.1" | "irin.directive.v0.1",
//!     "time": "2026-05-12T19:02:14Z",
//!     "datacontenttype": "application/json",
//!     "data": {
//!       "contract": "irin.comms.v0.1",
//!       "kind": "Escalation" | "Directive",
//!       "tenant": "...",
//!       "ttl_seconds": 60,
//!       "budget_hint": "...",
//!       "reply_to": "...",
//!       "payload": { ... }
//!     }
//!   }
//! }
//! ```
//!
//! See:
//! - `COMMS_CONTRACT.md` (irin.comms.v0.1 spine)
//! - CloudEvents 1.0 §3.1 (required context attributes)
//! - Grok G6: source URI scheme `urn:irin:sentinel:{name}`

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::time::{SystemTime, UNIX_EPOCH};

pub const COMMS_CONTRACT_VERSION: &str = "irin.comms.v0.1";
pub const CE_SPECVERSION: &str = "1.0";
pub const CE_DATACONTENTTYPE: &str = "application/json";
pub const ENVELOPE_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum EnvelopeKind {
    Escalation,
    Directive,
}

impl EnvelopeKind {
    pub fn type_id(&self) -> &'static str {
        match self {
            EnvelopeKind::Escalation => "irin.escalation.v0.1",
            EnvelopeKind::Directive => "irin.directive.v0.1",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CommsData {
    pub contract: String,
    pub kind: EnvelopeKind,
    pub tenant: String,
    pub ttl_seconds: u64,
    pub budget_hint: String,
    pub reply_to: String,
    pub payload: Value,
}

/// CloudEvents 1.0 envelope with the IRIN profile.
///
/// Serialized as the CE wire form (see module docs). `kind` drives the
/// `type` attribute; the in-spine duplicate inside `data.kind` mirrors it
/// for downstream readers who only deserialize the data payload.
#[derive(Debug, Clone)]
pub struct CommsEnvelope {
    pub id: String,
    pub source: String,
    pub kind: EnvelopeKind,
    pub time: String,
    pub data: CommsData,
}

impl CommsEnvelope {
    pub fn builder(kind: EnvelopeKind) -> CommsEnvelopeBuilder {
        CommsEnvelopeBuilder::new(kind)
    }

    pub fn wrap(self) -> EnvelopeWrapper {
        EnvelopeWrapper {
            v: ENVELOPE_SCHEMA_VERSION,
            envelope: self,
        }
    }
}

/// Schema-versioned outer wrapper: `{"v":1, "envelope":{...}}`.
///
/// `watch_fires.envelope_json` remains the
/// raw sentinel `Escalation` audit payload, but new CDC-produced
/// `pending_escalations.envelope_json` rows wrap that raw payload in this
/// formal `irin.comms.v0.1` envelope. The CDC transform and legacy/raw
/// compatibility shim live in `watch::runner`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnvelopeWrapper {
    pub v: u32,
    pub envelope: CommsEnvelope,
}

pub struct CommsEnvelopeBuilder {
    kind: EnvelopeKind,
    sentinel_name: Option<String>,
    tenant: Option<String>,
    ttl_seconds: Option<u64>,
    budget_hint: Option<String>,
    reply_to: Option<String>,
    data: Option<Value>,
    id_override: Option<String>,
    time_override: Option<String>,
}

impl CommsEnvelopeBuilder {
    fn new(kind: EnvelopeKind) -> Self {
        Self {
            kind,
            sentinel_name: None,
            tenant: None,
            ttl_seconds: None,
            budget_hint: None,
            reply_to: None,
            data: None,
            id_override: None,
            time_override: None,
        }
    }

    pub fn sentinel_name(mut self, name: &str) -> Self {
        self.sentinel_name = Some(name.to_string());
        self
    }

    pub fn tenant(mut self, tenant: &str) -> Self {
        self.tenant = Some(tenant.to_string());
        self
    }

    pub fn ttl_seconds(mut self, ttl: u64) -> Self {
        self.ttl_seconds = Some(ttl);
        self
    }

    pub fn budget_hint(mut self, hint: &str) -> Self {
        self.budget_hint = Some(hint.to_string());
        self
    }

    pub fn reply_to(mut self, addr: &str) -> Self {
        self.reply_to = Some(addr.to_string());
        self
    }

    pub fn data(mut self, payload: Value) -> Self {
        self.data = Some(payload);
        self
    }

    #[doc(hidden)]
    pub fn id(mut self, id: &str) -> Self {
        self.id_override = Some(id.to_string());
        self
    }

    #[doc(hidden)]
    pub fn time(mut self, time: &str) -> Self {
        self.time_override = Some(time.to_string());
        self
    }

    /// Builds the envelope, or reports which required field is missing.
    ///
    /// T33: this was five `.expect()` panics. The wire shape is unchanged —
    /// the same five fields are required; failure now surfaces as an error
    /// naming the field instead of crashing the caller's process.
    pub fn build(self) -> Result<CommsEnvelope, EnvelopeBuildError> {
        let sentinel_name = self
            .sentinel_name
            .ok_or(EnvelopeBuildError::MissingField("sentinel_name"))?;
        let tenant = self
            .tenant
            .ok_or(EnvelopeBuildError::MissingField("tenant"))?;
        let ttl_seconds = self
            .ttl_seconds
            .ok_or(EnvelopeBuildError::MissingField("ttl_seconds"))?;
        let budget_hint = self
            .budget_hint
            .ok_or(EnvelopeBuildError::MissingField("budget_hint"))?;
        let reply_to = self
            .reply_to
            .ok_or(EnvelopeBuildError::MissingField("reply_to"))?;
        let payload = self
            .data
            .unwrap_or_else(|| Value::Object(Default::default()));

        Ok(CommsEnvelope {
            id: self.id_override.unwrap_or_else(random_id_hex32),
            source: format!("urn:irin:sentinel:{sentinel_name}"),
            kind: self.kind,
            time: self.time_override.unwrap_or_else(now_rfc3339_utc),
            data: CommsData {
                contract: COMMS_CONTRACT_VERSION.to_string(),
                kind: self.kind,
                tenant,
                ttl_seconds,
                budget_hint,
                reply_to,
                payload,
            },
        })
    }
}

/// A required builder field was not set before `build()` (T33).
///
/// Mirrors the crate's `JcsError` convention: plain enum, hand-rolled
/// `Display`, no derive-macro dependency.
///
/// `non_exhaustive`: this type is re-exported from a public crate; adding a
/// variant later must not be a breaking change for external matchers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EnvelopeBuildError {
    /// The named required field was never set on the builder.
    MissingField(&'static str),
}

impl std::fmt::Display for EnvelopeBuildError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EnvelopeBuildError::MissingField(field) => {
                write!(f, "CommsEnvelope: {field} is required")
            }
        }
    }
}

impl std::error::Error for EnvelopeBuildError {}

// ----- CloudEvents wire-form serde -----

impl Serialize for CommsEnvelope {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = ser.serialize_struct("CommsEnvelope", 7)?;
        s.serialize_field("specversion", CE_SPECVERSION)?;
        s.serialize_field("id", &self.id)?;
        s.serialize_field("source", &self.source)?;
        s.serialize_field("type", self.kind.type_id())?;
        s.serialize_field("time", &self.time)?;
        s.serialize_field("datacontenttype", CE_DATACONTENTTYPE)?;
        s.serialize_field("data", &self.data)?;
        s.end()
    }
}

#[derive(Deserialize)]
struct CeWire {
    specversion: String,
    id: String,
    source: String,
    #[serde(rename = "type")]
    type_: String,
    time: String,
    datacontenttype: String,
    data: CommsData,
}

impl<'de> Deserialize<'de> for CommsEnvelope {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = CeWire::deserialize(d)?;
        if w.specversion != CE_SPECVERSION {
            return Err(serde::de::Error::custom(format!(
                "CE specversion mismatch: expected {CE_SPECVERSION}, got {}",
                w.specversion
            )));
        }
        if w.datacontenttype != CE_DATACONTENTTYPE {
            return Err(serde::de::Error::custom(format!(
                "CE datacontenttype mismatch: expected {CE_DATACONTENTTYPE}, got {}",
                w.datacontenttype
            )));
        }
        let kind = match w.type_.as_str() {
            "irin.escalation.v0.1" => EnvelopeKind::Escalation,
            "irin.directive.v0.1" => EnvelopeKind::Directive,
            other => {
                return Err(serde::de::Error::custom(format!(
                    "unknown envelope type: {other}"
                )));
            }
        };
        Ok(CommsEnvelope {
            id: w.id,
            source: w.source,
            kind,
            time: w.time,
            data: w.data,
        })
    }
}

// ----- helpers -----

fn random_id_hex32() -> String {
    use rand_core::{OsRng, RngCore};
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    let mut s = String::with_capacity(32);
    for b in bytes {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

fn now_rfc3339_utc() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0) as i64;
    epoch_secs_to_rfc3339(secs)
}

// Howard Hinnant's civil_from_days algorithm.
// (https://howardhinnant.github.io/date_algorithms.html)
fn epoch_secs_to_rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let tod = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };

    let h = tod / 3600;
    let mi = (tod % 3600) / 60;
    let se = tod % 60;
    format!("{:04}-{:02}-{:02}T{:02}:{:02}:{:02}Z", y, m, d, h, mi, se)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc3339_epoch_zero_is_unix_epoch() {
        assert_eq!(epoch_secs_to_rfc3339(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn rfc3339_known_date_2024_03_05() {
        // 2024-03-05 12:34:56 UTC = 1_709_642_096
        // (19723 days 1970→2024 + 64 days into 2024 = 19787 days; + 45296s)
        assert_eq!(epoch_secs_to_rfc3339(1_709_642_096), "2024-03-05T12:34:56Z");
    }

    #[test]
    fn random_ids_are_32_hex_chars() {
        let id = random_id_hex32();
        assert_eq!(id.len(), 32);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    // ----- T33: build() -> Result -----

    fn full_builder() -> CommsEnvelopeBuilder {
        CommsEnvelope::builder(EnvelopeKind::Escalation)
            .sentinel_name("test-sentinel")
            .tenant("sovereign")
            .ttl_seconds(300)
            .budget_hint("none")
            .reply_to("urn:irin:gateway:test")
    }

    #[test]
    fn build_succeeds_with_all_required_fields() {
        let env = full_builder().build().expect("all fields set");
        assert_eq!(env.source, "urn:irin:sentinel:test-sentinel");
        assert_eq!(env.data.tenant, "sovereign");
        assert_eq!(env.data.ttl_seconds, 300);

        // Pin the success path onto the CE wire form (full-shape coverage
        // lives in the golden fixtures; this catches a build()-level drift).
        let wire = serde_json::to_value(&env).expect("serializes");
        assert_eq!(wire["specversion"], CE_SPECVERSION);
        assert_eq!(wire["type"], "irin.escalation.v0.1");
        assert_eq!(wire["datacontenttype"], CE_DATACONTENTTYPE);
        assert_eq!(wire["data"]["contract"], COMMS_CONTRACT_VERSION);
    }

    #[test]
    fn build_missing_sentinel_name_names_the_field() {
        let err = CommsEnvelope::builder(EnvelopeKind::Escalation)
            .tenant("sovereign")
            .ttl_seconds(300)
            .budget_hint("none")
            .reply_to("urn:irin:gateway:test")
            .build()
            .unwrap_err();
        assert_eq!(err, EnvelopeBuildError::MissingField("sentinel_name"));
        assert_eq!(err.to_string(), "CommsEnvelope: sentinel_name is required");
    }

    #[test]
    fn build_missing_tenant_names_the_field() {
        let err = CommsEnvelope::builder(EnvelopeKind::Directive)
            .sentinel_name("test-sentinel")
            .ttl_seconds(300)
            .budget_hint("none")
            .reply_to("urn:irin:gateway:test")
            .build()
            .unwrap_err();
        assert_eq!(err, EnvelopeBuildError::MissingField("tenant"));
        assert_eq!(err.to_string(), "CommsEnvelope: tenant is required");
    }

    #[test]
    fn build_missing_ttl_seconds_names_the_field() {
        let err = CommsEnvelope::builder(EnvelopeKind::Escalation)
            .sentinel_name("test-sentinel")
            .tenant("sovereign")
            .budget_hint("none")
            .reply_to("urn:irin:gateway:test")
            .build()
            .unwrap_err();
        assert_eq!(err, EnvelopeBuildError::MissingField("ttl_seconds"));
        assert_eq!(err.to_string(), "CommsEnvelope: ttl_seconds is required");
    }

    #[test]
    fn build_missing_budget_hint_names_the_field() {
        let err = CommsEnvelope::builder(EnvelopeKind::Escalation)
            .sentinel_name("test-sentinel")
            .tenant("sovereign")
            .ttl_seconds(300)
            .reply_to("urn:irin:gateway:test")
            .build()
            .unwrap_err();
        assert_eq!(err, EnvelopeBuildError::MissingField("budget_hint"));
        assert_eq!(err.to_string(), "CommsEnvelope: budget_hint is required");
    }

    #[test]
    fn build_missing_reply_to_names_the_field() {
        let err = CommsEnvelope::builder(EnvelopeKind::Escalation)
            .sentinel_name("test-sentinel")
            .tenant("sovereign")
            .ttl_seconds(300)
            .budget_hint("none")
            .build()
            .unwrap_err();
        assert_eq!(err, EnvelopeBuildError::MissingField("reply_to"));
        assert_eq!(err.to_string(), "CommsEnvelope: reply_to is required");
    }

    #[test]
    fn build_data_is_optional_defaults_to_empty_object() {
        let env = full_builder().build().expect("data is optional");
        assert_eq!(env.data.payload, Value::Object(Default::default()));
    }
}
