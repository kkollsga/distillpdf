//! Closed, product-owned failure facts for cache-neutral object-stream preparation.

#![allow(dead_code)] // Gate 2 types are consumed by Gate 3 publication.

use crate::access::{AccessError, AccessKind};
use lopdf::{
    encryption::DecryptionError, DecompressError, IndexedReaderError, MissingNormalObjectReason,
    ObjectId, ObjectLimitProvenance, SourceError,
};

pub(crate) const FAILURE_OWNER_BASE_BYTES: u64 = 256;
const OBJECT_WINDOW_BYTES: u64 = 4 * 1024 * 1024;
const CONTAINER_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
const UNSUPPORTED_FILTER: &str =
    "object-stream filter chains or predictors outside plain/FlateDecode";
const UNSUPPORTED_LENGTH: &str = "object streams without a bounded nonnegative /Length";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ObjStmFailureClass {
    PersistentNative,
    PersistentAboveCap,
    FlightOnly,
    ExactKeyInvariant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ScalarPhase {
    PermitNotEmpty,
    IndirectHeader,
    ScalarFrame,
    ScalarFrameGrowth,
    ScalarAstEnvelope,
    ScalarStreamEndMarker,
    ObjectStreamEndMarker,
    ObjectStreamEncoded,
    ObjectStreamDecryptionOverlap,
    ObjectStreamDecodeEnvelope,
    ObjectStreamDecompressedGrowth,
    ObjectStreamHeaderIndex,
    MeasuredScalar,
    MeasuredObjectStreamDictionary,
    MeasuredObjectStreamPlaintext,
    MeasuredObjectStreamDecryptedDictionary,
    MeasuredObjectStreamCacheEntry,
    ObjectStreamDecodedCapacity,
    Unknown,
}

impl ScalarPhase {
    fn classify(phase: &'static str) -> Self {
        match phase {
            "permit-not-empty" => Self::PermitNotEmpty,
            "indirect-header" => Self::IndirectHeader,
            "scalar-frame" => Self::ScalarFrame,
            "scalar-frame-growth" => Self::ScalarFrameGrowth,
            "scalar-ast-envelope" => Self::ScalarAstEnvelope,
            "scalar-stream-end-marker" => Self::ScalarStreamEndMarker,
            "object-stream-end-marker" => Self::ObjectStreamEndMarker,
            "object-stream-encoded" => Self::ObjectStreamEncoded,
            "object-stream-decryption-overlap" => Self::ObjectStreamDecryptionOverlap,
            "object-stream-decode-envelope" => Self::ObjectStreamDecodeEnvelope,
            "object-stream-decompressed-growth" => Self::ObjectStreamDecompressedGrowth,
            "object-stream-header-index" => Self::ObjectStreamHeaderIndex,
            "measured-scalar" => Self::MeasuredScalar,
            "measured-object-stream-dictionary" => Self::MeasuredObjectStreamDictionary,
            "measured-object-stream-plaintext" => Self::MeasuredObjectStreamPlaintext,
            "measured-object-stream-decrypted-dictionary" => {
                Self::MeasuredObjectStreamDecryptedDictionary
            }
            "measured-object-stream-cache-entry" => Self::MeasuredObjectStreamCacheEntry,
            "object-stream-decoded-capacity" => Self::ObjectStreamDecodedCapacity,
            _ => Self::Unknown,
        }
    }

    fn is_admitted(self) -> bool {
        matches!(
            self,
            Self::IndirectHeader
                | Self::ScalarFrame
                | Self::ScalarFrameGrowth
                | Self::ScalarAstEnvelope
                | Self::ScalarStreamEndMarker
                | Self::ObjectStreamEndMarker
                | Self::ObjectStreamEncoded
                | Self::ObjectStreamDecryptionOverlap
                | Self::ObjectStreamDecodeEnvelope
                | Self::ObjectStreamDecompressedGrowth
                | Self::ObjectStreamHeaderIndex
        )
    }
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum MissingAtXref {
    HeaderProbeLimit {
        offset: u64,
        limit: u64,
    },
    HeaderMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    GenerationMismatch {
        requested: ObjectId,
        indexed: u16,
        actual: ObjectId,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum LimitProvenance {
    FrameNeedMoreAtMaximum,
    SourceExhaustedAtMaximum,
    ArithmeticInvariant,
    Unknown,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum StableInner {
    InvalidObjectStream(String),
    InvalidStream(String),
    InvalidOffset(usize),
    Ascii85(&'static str),
    AsciiHex(&'static str),
    Predictor(&'static str),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum StableDecryption {
    NotDecryptable,
    InvalidKeyLength,
    InvalidCipherTextLength,
    Padding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InvariantStage {
    PermitOrMeasurement,
    ObjectLimitProvenance,
    StreamSpan,
    ObjectStreamBatchSetup,
    ObjectStreamCacheBypass,
    RetainedWeightOverflow,
    PayloadMismatch,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum ObjStmFact {
    MissingNormal(ObjectId),
    MissingAtXref {
        id: ObjectId,
        reason: MissingAtXref,
    },
    GenerationMismatch {
        id: ObjectId,
        indexed: u16,
    },
    IndirectMismatch {
        expected: ObjectId,
        actual: ObjectId,
    },
    InvalidIndirect {
        id: ObjectId,
        offset: u64,
        incomplete: bool,
    },
    ObjectLimit {
        id: ObjectId,
        limit: u64,
        provenance: LimitProvenance,
    },
    ScalarLimit {
        actual_id: ObjectId,
        requested: u64,
        limit: u64,
        phase: ScalarPhase,
    },
    ScalarCancelled {
        actual_id: ObjectId,
        phase: ScalarPhase,
    },
    ScalarClosed {
        actual_id: ObjectId,
        phase: ScalarPhase,
    },
    StreamLimit {
        id: ObjectId,
        length: u64,
        limit: u64,
    },
    MissingEndstream(ObjectId),
    ContainerNotStream(ObjectId),
    UnsupportedFilter(ObjectId),
    UnsupportedLength(ObjectId),
    ObjectStreamCause {
        container: ObjectId,
        cause: StableInner,
    },
    ObjectDecryption {
        container: ObjectId,
        cause: StableDecryption,
    },
    Invariant {
        container: ObjectId,
        stage: InvariantStage,
    },
    FlightAccess {
        actual_id: Option<ObjectId>,
        kind: AccessKind,
        detail: String,
    },
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) struct ObjStmFailureTemplate {
    class: ObjStmFailureClass,
    fact: ObjStmFact,
}

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum FailurePayload {
    Access(AccessError),
    ObjStm(ObjStmFailureTemplate),
}

impl ObjStmFailureTemplate {
    pub(crate) fn class(&self) -> ObjStmFailureClass {
        self.class
    }
    pub(crate) fn fact(&self) -> &ObjStmFact {
        &self.fact
    }
    pub(crate) fn dynamic_capacity(&self) -> Option<u64> {
        let capacity = match &self.fact {
            ObjStmFact::ObjectStreamCause {
                cause: StableInner::InvalidObjectStream(value) | StableInner::InvalidStream(value),
                ..
            } => value.capacity(),
            ObjStmFact::FlightAccess { detail, .. } => detail.capacity(),
            _ => 0,
        };
        u64::try_from(capacity).ok()
    }

    #[cfg(test)]
    pub(crate) fn dynamic_allocation(&self) -> Option<(usize, u64)> {
        let value = match &self.fact {
            ObjStmFact::ObjectStreamCause {
                cause: StableInner::InvalidObjectStream(value) | StableInner::InvalidStream(value),
                ..
            } => value,
            ObjStmFact::FlightAccess { detail, .. } => detail,
            _ => return None,
        };
        Some((
            value.as_ptr() as usize,
            u64::try_from(value.capacity()).ok()?,
        ))
    }

    pub(crate) fn invariant_stage(&self) -> Option<InvariantStage> {
        if self.class != ObjStmFailureClass::ExactKeyInvariant {
            return None;
        }
        match &self.fact {
            ObjStmFact::ObjectLimit { .. } => Some(InvariantStage::ObjectLimitProvenance),
            ObjStmFact::ScalarLimit { .. } => Some(InvariantStage::PermitOrMeasurement),
            ObjStmFact::StreamLimit { .. } => Some(InvariantStage::StreamSpan),
            ObjStmFact::Invariant { stage, .. } => Some(*stage),
            _ => None,
        }
    }
}

impl FailurePayload {
    pub(crate) fn retained_weight(&self) -> Result<u64, RetainedWeightError> {
        let dynamic = match self {
            Self::Access(value) => {
                u64::try_from(value.detail.capacity()).map_err(|_| RetainedWeightError::Overflow)?
            }
            Self::ObjStm(value) => value
                .dynamic_capacity()
                .ok_or(RetainedWeightError::Overflow)?,
        };
        checked_retained_weight(dynamic)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum RetainedWeightError {
    Overflow,
    OverAttempt { weight: u64, limit: u64 },
}

fn checked_retained_weight(dynamic: u64) -> Result<u64, RetainedWeightError> {
    let weight = FAILURE_OWNER_BASE_BYTES
        .checked_add(dynamic)
        .ok_or(RetainedWeightError::Overflow)?;
    if weight > CONTAINER_LIMIT_BYTES {
        Err(RetainedWeightError::OverAttempt {
            weight,
            limit: CONTAINER_LIMIT_BYTES,
        })
    } else {
        Ok(weight)
    }
}

pub(crate) fn classify(container: ObjectId, error: IndexedReaderError) -> ObjStmFailureTemplate {
    let result = match error {
        IndexedReaderError::MissingNormalObject { id } => persistent(ObjStmFact::MissingNormal(id)),
        IndexedReaderError::MissingNormalObjectAtXref { id, reason } => match reason {
            MissingNormalObjectReason::HeaderProbeLimit { offset, limit } => {
                persistent(ObjStmFact::MissingAtXref {
                    id,
                    reason: MissingAtXref::HeaderProbeLimit { offset, limit },
                })
            }
            MissingNormalObjectReason::HeaderMismatch { expected, actual } => {
                persistent(ObjStmFact::MissingAtXref {
                    id,
                    reason: MissingAtXref::HeaderMismatch { expected, actual },
                })
            }
            MissingNormalObjectReason::GenerationMismatch {
                requested,
                indexed,
                actual,
            } => persistent(ObjStmFact::MissingAtXref {
                id,
                reason: MissingAtXref::GenerationMismatch {
                    requested,
                    indexed,
                    actual,
                },
            }),
            #[allow(unreachable_patterns)]
            reason => flight(
                Some(container),
                AccessKind::Backend,
                IndexedReaderError::MissingNormalObjectAtXref { id, reason }.to_string(),
            ),
        },
        IndexedReaderError::GenerationMismatch { id, indexed } => {
            persistent(ObjStmFact::GenerationMismatch { id, indexed })
        }
        IndexedReaderError::IndirectObjectMismatch { expected, actual } => {
            persistent(ObjStmFact::IndirectMismatch { expected, actual })
        }
        IndexedReaderError::InvalidIndirectObject { id, offset } => {
            persistent(ObjStmFact::InvalidIndirect {
                id,
                offset,
                incomplete: false,
            })
        }
        IndexedReaderError::IncompleteObject { id, offset } => {
            persistent(ObjStmFact::InvalidIndirect {
                id,
                offset,
                incomplete: true,
            })
        }
        IndexedReaderError::ObjectLimitExceeded {
            id,
            limit,
            provenance,
        } => classify_object_limit(container, id, limit, provenance),
        IndexedReaderError::ScalarResourceLimit {
            id,
            requested,
            limit,
            phase,
        } => {
            let phase = ScalarPhase::classify(phase);
            let class = if phase.is_admitted() {
                if limit == CONTAINER_LIMIT_BYTES && requested > limit {
                    ObjStmFailureClass::PersistentAboveCap
                } else if limit == CONTAINER_LIMIT_BYTES {
                    ObjStmFailureClass::ExactKeyInvariant
                } else {
                    ObjStmFailureClass::FlightOnly
                }
            } else {
                ObjStmFailureClass::ExactKeyInvariant
            };
            (
                class,
                ObjStmFact::ScalarLimit {
                    actual_id: id,
                    requested,
                    limit,
                    phase,
                },
            )
        }
        IndexedReaderError::ScalarResolutionCancelled { id, phase } => (
            if ScalarPhase::classify(phase) == ScalarPhase::Unknown {
                return finish(flight(
                    Some(id),
                    AccessKind::ResourceLimit,
                    IndexedReaderError::ScalarResolutionCancelled { id, phase }.to_string(),
                ));
            } else {
                ObjStmFailureClass::FlightOnly
            },
            ObjStmFact::ScalarCancelled {
                actual_id: id,
                phase: ScalarPhase::classify(phase),
            },
        ),
        IndexedReaderError::ScalarResolutionClosed { id, phase } => (
            if ScalarPhase::classify(phase) == ScalarPhase::Unknown {
                return finish(flight(
                    Some(id),
                    AccessKind::ResourceLimit,
                    IndexedReaderError::ScalarResolutionClosed { id, phase }.to_string(),
                ));
            } else {
                ObjStmFailureClass::FlightOnly
            },
            ObjStmFact::ScalarClosed {
                actual_id: id,
                phase: ScalarPhase::classify(phase),
            },
        ),
        IndexedReaderError::StreamLimitExceeded { id, length, limit } => {
            let class = if length > limit && limit == CONTAINER_LIMIT_BYTES {
                ObjStmFailureClass::PersistentAboveCap
            } else if length <= limit {
                ObjStmFailureClass::ExactKeyInvariant
            } else {
                ObjStmFailureClass::FlightOnly
            };
            (class, ObjStmFact::StreamLimit { id, length, limit })
        }
        IndexedReaderError::MissingEndstream { id } => persistent(ObjStmFact::MissingEndstream(id)),
        IndexedReaderError::ObjectStreamContainerNotStream { container, .. } => {
            persistent(ObjStmFact::ContainerNotStream(container))
        }
        IndexedReaderError::UnsupportedBoundedScalar { id, reason }
            if reason == UNSUPPORTED_FILTER =>
        {
            persistent(ObjStmFact::UnsupportedFilter(id))
        }
        IndexedReaderError::UnsupportedBoundedScalar { id, reason }
            if reason == UNSUPPORTED_LENGTH =>
        {
            (
                ObjStmFailureClass::FlightOnly,
                ObjStmFact::UnsupportedLength(id),
            )
        }
        IndexedReaderError::ObjectStreamMember {
            id,
            container: physical,
            index,
            source,
        } => classify_inner(container, id, physical, index, source),
        IndexedReaderError::ObjectDecryption { id, source } => match stable_decryption(source) {
            Ok(cause) => persistent(ObjStmFact::ObjectDecryption {
                container: id,
                cause,
            }),
            Err(source) => flight(
                Some(container),
                AccessKind::ObjectDecryption,
                IndexedReaderError::ObjectDecryption { id, source }.to_string(),
            ),
        },
        IndexedReaderError::ObjectStreamBatchSetup { container, .. } => {
            invariant(container, InvariantStage::ObjectStreamBatchSetup)
        }
        IndexedReaderError::ObjectStreamCacheBypass { container } => {
            invariant(container, InvariantStage::ObjectStreamCacheBypass)
        }
        IndexedReaderError::Source(source) => {
            let kind = source_kind(&source);
            flight(Some(container), kind, source.to_string())
        }
        other => {
            let kind = indexed_kind(&other);
            flight(Some(container), kind, other.to_string())
        }
    };
    finish(result)
}

fn finish(result: (ObjStmFailureClass, ObjStmFact)) -> ObjStmFailureTemplate {
    ObjStmFailureTemplate {
        class: result.0,
        fact: result.1,
    }
}

fn classify_object_limit(
    container: ObjectId,
    id: ObjectId,
    limit: u64,
    provenance: ObjectLimitProvenance,
) -> (ObjStmFailureClass, ObjStmFact) {
    let provenance = match provenance {
        ObjectLimitProvenance::FrameNeedMoreAtMaximum => LimitProvenance::FrameNeedMoreAtMaximum,
        ObjectLimitProvenance::SourceExhaustedAtMaximum => {
            LimitProvenance::SourceExhaustedAtMaximum
        }
        ObjectLimitProvenance::ArithmeticInvariant => LimitProvenance::ArithmeticInvariant,
        #[allow(unreachable_patterns)]
        value => {
            if limit != CONTAINER_LIMIT_BYTES {
                return flight(
                    Some(container),
                    AccessKind::ResourceLimit,
                    IndexedReaderError::ObjectLimitExceeded {
                        id,
                        limit,
                        provenance: value,
                    }
                    .to_string(),
                );
            }
            LimitProvenance::Unknown
        }
    };
    let class = if provenance == LimitProvenance::ArithmeticInvariant {
        ObjStmFailureClass::ExactKeyInvariant
    } else if limit == CONTAINER_LIMIT_BYTES
        && provenance == LimitProvenance::FrameNeedMoreAtMaximum
    {
        ObjStmFailureClass::PersistentAboveCap
    } else if limit == CONTAINER_LIMIT_BYTES {
        ObjStmFailureClass::FlightOnly
    } else if limit == OBJECT_WINDOW_BYTES && provenance == LimitProvenance::FrameNeedMoreAtMaximum
    {
        ObjStmFailureClass::PersistentNative
    } else {
        ObjStmFailureClass::FlightOnly
    };
    (
        class,
        ObjStmFact::ObjectLimit {
            id,
            limit,
            provenance,
        },
    )
}

fn classify_inner(
    _container: ObjectId,
    _id: ObjectId,
    physical: ObjectId,
    _index: u32,
    source: lopdf::Error,
) -> (ObjStmFailureClass, ObjStmFact) {
    let stable = match stable_inner(source) {
        Ok(cause) => cause,
        Err(source) => return flight(None, AccessKind::Backend, source.to_string()),
    };
    persistent(ObjStmFact::ObjectStreamCause {
        container: physical,
        cause: stable,
    })
}

fn stable_inner(source: lopdf::Error) -> Result<StableInner, lopdf::Error> {
    match source {
        lopdf::Error::InvalidObjectStream(value) => Ok(StableInner::InvalidObjectStream(value)),
        lopdf::Error::InvalidStream(value) => Ok(StableInner::InvalidStream(value)),
        lopdf::Error::InvalidOffset(value) => Ok(StableInner::InvalidOffset(value)),
        lopdf::Error::Decompress(source) => {
            stable_decompress(source).map_err(lopdf::Error::Decompress)
        }
        source @ (lopdf::Error::Unimplemented(_)
        | lopdf::Error::ObjectType { .. }
        | lopdf::Error::DictType { .. }
        | lopdf::Error::AlreadyEncrypted
        | lopdf::Error::CharacterEncoding
        | lopdf::Error::Parse(_)
        | lopdf::Error::Decryption(_)
        | lopdf::Error::DictKey(_)
        | lopdf::Error::InvalidInlineImage(_)
        | lopdf::Error::InvalidOutline(_)
        | lopdf::Error::IO(_)
        | lopdf::Error::NoOutline
        | lopdf::Error::NotEncrypted
        | lopdf::Error::InvalidPassword
        | lopdf::Error::MissingXrefEntry
        | lopdf::Error::ObjectNotFound(_)
        | lopdf::Error::ReferenceCycle(_)
        | lopdf::Error::PageNumberNotFound(_)
        | lopdf::Error::NumericCast(_)
        | lopdf::Error::ReferenceLimit
        | lopdf::Error::RecursionLimit
        | lopdf::Error::TextStringDecode
        | lopdf::Error::Xref(_)
        | lopdf::Error::IndirectObject { .. }
        | lopdf::Error::ObjectIdMismatch
        | lopdf::Error::Syntax(_)
        | lopdf::Error::ToUnicodeCMap(_)
        | lopdf::Error::TryFromInt(_)
        | lopdf::Error::UnsupportedSecurityHandler(_)
        | lopdf::Error::InvalidEncodingDifferenceCode { .. }
        | lopdf::Error::InvalidEncodingDifferenceGlyph { .. }) => Err(source),
    }
}

fn stable_decompress(source: DecompressError) -> Result<StableInner, DecompressError> {
    match source {
        DecompressError::Ascii85(value) => Ok(StableInner::Ascii85(value)),
        DecompressError::AsciiHex(value) => Ok(StableInner::AsciiHex(value)),
        DecompressError::Predictor(value) => Ok(StableInner::Predictor(value)),
        source @ DecompressError::MemoryLimitExceeded { .. } => Err(source),
    }
}

fn stable_decryption(source: DecryptionError) -> Result<StableDecryption, DecryptionError> {
    match source {
        DecryptionError::NotDecryptable => Ok(StableDecryption::NotDecryptable),
        DecryptionError::InvalidKeyLength => Ok(StableDecryption::InvalidKeyLength),
        DecryptionError::InvalidCipherTextLength => Ok(StableDecryption::InvalidCipherTextLength),
        DecryptionError::Padding => Ok(StableDecryption::Padding),
        source @ (DecryptionError::MissingEncryptDictionary
        | DecryptionError::MissingVersion
        | DecryptionError::MissingRevision
        | DecryptionError::MissingOwnerPassword
        | DecryptionError::MissingUserPassword
        | DecryptionError::MissingPermissions
        | DecryptionError::MissingFileID
        | DecryptionError::InvalidHashLength
        | DecryptionError::InvalidPermissionLength
        | DecryptionError::InvalidVersion
        | DecryptionError::InvalidRevision
        | DecryptionError::InvalidType
        | DecryptionError::IncorrectPassword
        | DecryptionError::UnsupportedEncryption
        | DecryptionError::UnsupportedVersion
        | DecryptionError::UnsupportedRevision
        | DecryptionError::StringPrep(_)) => Err(source),
    }
}

fn persistent(fact: ObjStmFact) -> (ObjStmFailureClass, ObjStmFact) {
    (ObjStmFailureClass::PersistentNative, fact)
}
fn invariant(container: ObjectId, stage: InvariantStage) -> (ObjStmFailureClass, ObjStmFact) {
    (
        ObjStmFailureClass::ExactKeyInvariant,
        ObjStmFact::Invariant { container, stage },
    )
}
fn flight(
    actual_id: Option<ObjectId>,
    kind: AccessKind,
    detail: String,
) -> (ObjStmFailureClass, ObjStmFact) {
    (
        ObjStmFailureClass::FlightOnly,
        ObjStmFact::FlightAccess {
            actual_id,
            kind,
            detail,
        },
    )
}
fn source_kind(error: &SourceError) -> AccessKind {
    match error {
        SourceError::SourceChanged => AccessKind::SourceChanged,
        SourceError::RangeOverflow { .. } | SourceError::OutOfBounds { .. } => AccessKind::Bounds,
        SourceError::ReadLimitExceeded { .. }
        | SourceError::PlatformLimitExceeded { .. }
        | SourceError::AllocationFailed { .. } => AccessKind::ResourceLimit,
        SourceError::UnexpectedEof { .. }
        | SourceError::InvalidReadCount { .. }
        | SourceError::Io(_) => AccessKind::SourceIo,
        _ => AccessKind::Backend,
    }
}
fn indexed_kind(error: &IndexedReaderError) -> AccessKind {
    match error {
        IndexedReaderError::StructureLimitExceeded { .. }
        | IndexedReaderError::EntryLimitExceeded { .. }
        | IndexedReaderError::RevisionLimitExceeded { .. }
        | IndexedReaderError::IndirectHeaderLimitExceeded { .. }
        | IndexedReaderError::ObjectLimitExceeded { .. }
        | IndexedReaderError::ScalarResourceLimit { .. }
        | IndexedReaderError::ScalarResolutionCancelled { .. }
        | IndexedReaderError::ScalarResolutionClosed { .. }
        | IndexedReaderError::StreamLimitExceeded { .. }
        | IndexedReaderError::ResolutionDepthExceeded { .. }
        | IndexedReaderError::ObjectStreamCacheBypass { .. }
        | IndexedReaderError::PageCountLimitExceeded { .. } => AccessKind::ResourceLimit,
        IndexedReaderError::StartXrefOutOfBounds { .. }
        | IndexedReaderError::NegativeStreamLength { .. } => AccessKind::Bounds,
        IndexedReaderError::NotScalarObject { .. }
        | IndexedReaderError::NotStreamObject { .. }
        | IndexedReaderError::UnsupportedBoundedScalar { .. } => AccessKind::Type,
        IndexedReaderError::PasswordRequired => AccessKind::PasswordRequired,
        IndexedReaderError::InvalidPassword => AccessKind::InvalidPassword,
        IndexedReaderError::Encryption(_) => AccessKind::Encryption,
        IndexedReaderError::InvalidEncryptDictionary => AccessKind::InvalidEncryptDictionary,
        IndexedReaderError::ObjectDecryption { .. } => AccessKind::ObjectDecryption,
        _ => AccessKind::Backend,
    }
}

const _: () = assert!(std::mem::size_of::<FailurePayload>() <= FAILURE_OWNER_BASE_BYTES as usize);
const _: () =
    assert!(std::mem::size_of::<ObjStmFailureTemplate>() <= FAILURE_OWNER_BASE_BYTES as usize);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_cap_phase_stream_and_provenance_matrix() {
        let id = (6, 0);
        for phase in [
            "indirect-header",
            "scalar-frame",
            "scalar-frame-growth",
            "scalar-ast-envelope",
            "scalar-stream-end-marker",
            "object-stream-end-marker",
            "object-stream-encoded",
            "object-stream-decryption-overlap",
            "object-stream-decode-envelope",
            "object-stream-decompressed-growth",
            "object-stream-header-index",
        ] {
            assert_eq!(
                classify(
                    id,
                    IndexedReaderError::ScalarResourceLimit {
                        id,
                        requested: CONTAINER_LIMIT_BYTES + 1,
                        limit: CONTAINER_LIMIT_BYTES,
                        phase
                    }
                )
                .class(),
                ObjStmFailureClass::PersistentAboveCap
            );
            assert_eq!(
                classify(
                    id,
                    IndexedReaderError::ScalarResourceLimit {
                        id,
                        requested: CONTAINER_LIMIT_BYTES,
                        limit: CONTAINER_LIMIT_BYTES,
                        phase
                    }
                )
                .class(),
                ObjStmFailureClass::ExactKeyInvariant
            );
            assert_eq!(
                classify(
                    id,
                    IndexedReaderError::ScalarResourceLimit {
                        id,
                        requested: CONTAINER_LIMIT_BYTES,
                        limit: CONTAINER_LIMIT_BYTES - 1,
                        phase
                    }
                )
                .class(),
                ObjStmFailureClass::FlightOnly
            );
        }
        for phase in [
            "permit-not-empty",
            "measured-scalar",
            "measured-object-stream-dictionary",
            "measured-object-stream-plaintext",
            "measured-object-stream-decrypted-dictionary",
            "measured-object-stream-cache-entry",
            "object-stream-decoded-capacity",
            "future",
        ] {
            for (requested, limit) in [
                (CONTAINER_LIMIT_BYTES + 1, CONTAINER_LIMIT_BYTES),
                (CONTAINER_LIMIT_BYTES, CONTAINER_LIMIT_BYTES),
                (CONTAINER_LIMIT_BYTES + 1, CONTAINER_LIMIT_BYTES - 1),
            ] {
                assert_eq!(
                    classify(
                        id,
                        IndexedReaderError::ScalarResourceLimit {
                            id,
                            requested,
                            limit,
                            phase,
                        }
                    )
                    .class(),
                    ObjStmFailureClass::ExactKeyInvariant
                );
            }
        }
        for (limit, provenance, class) in [
            (
                CONTAINER_LIMIT_BYTES,
                ObjectLimitProvenance::FrameNeedMoreAtMaximum,
                ObjStmFailureClass::PersistentAboveCap,
            ),
            (
                CONTAINER_LIMIT_BYTES,
                ObjectLimitProvenance::SourceExhaustedAtMaximum,
                ObjStmFailureClass::FlightOnly,
            ),
            (
                CONTAINER_LIMIT_BYTES,
                ObjectLimitProvenance::ArithmeticInvariant,
                ObjStmFailureClass::ExactKeyInvariant,
            ),
            (
                OBJECT_WINDOW_BYTES,
                ObjectLimitProvenance::FrameNeedMoreAtMaximum,
                ObjStmFailureClass::PersistentNative,
            ),
            (
                OBJECT_WINDOW_BYTES,
                ObjectLimitProvenance::SourceExhaustedAtMaximum,
                ObjStmFailureClass::FlightOnly,
            ),
            (
                OBJECT_WINDOW_BYTES,
                ObjectLimitProvenance::ArithmeticInvariant,
                ObjStmFailureClass::ExactKeyInvariant,
            ),
        ] {
            assert_eq!(
                classify(
                    id,
                    IndexedReaderError::ObjectLimitExceeded {
                        id,
                        limit,
                        provenance
                    }
                )
                .class(),
                class
            );
        }
        for (length, limit, class) in [
            (
                CONTAINER_LIMIT_BYTES + 1,
                CONTAINER_LIMIT_BYTES,
                ObjStmFailureClass::PersistentAboveCap,
            ),
            (
                CONTAINER_LIMIT_BYTES,
                CONTAINER_LIMIT_BYTES,
                ObjStmFailureClass::ExactKeyInvariant,
            ),
            (
                CONTAINER_LIMIT_BYTES + 1,
                CONTAINER_LIMIT_BYTES - 1,
                ObjStmFailureClass::FlightOnly,
            ),
        ] {
            assert_eq!(
                classify(
                    id,
                    IndexedReaderError::StreamLimitExceeded { id, length, limit }
                )
                .class(),
                class
            );
        }
    }

    #[test]
    fn dynamic_weight_is_exact_and_stable_inner_is_closed() {
        let mut value = String::with_capacity(191);
        value.push_str("bad object stream");
        let capacity = value.capacity() as u64;
        let payload = FailurePayload::ObjStm(classify(
            (6, 0),
            IndexedReaderError::ObjectStreamMember {
                id: (6, 0),
                container: (6, 0),
                index: 0,
                source: lopdf::Error::InvalidObjectStream(value),
            },
        ));
        assert_eq!(
            payload.retained_weight(),
            Ok(FAILURE_OWNER_BASE_BYTES + capacity)
        );
        assert!(matches!(
            payload,
            FailurePayload::ObjStm(ObjStmFailureTemplate {
                class: ObjStmFailureClass::PersistentNative,
                ..
            })
        ));

        let mut detail = String::with_capacity(257);
        detail.push_str("raw failure");
        let capacity = detail.capacity() as u64;
        let access = FailurePayload::Access(AccessError {
            phase: crate::access::AccessPhase::Resolve,
            page: None,
            object: (6, 0),
            kind: AccessKind::Backend,
            detail,
        });
        assert_eq!(
            access.retained_weight(),
            Ok(FAILURE_OWNER_BASE_BYTES + capacity)
        );
        assert_eq!(
            checked_retained_weight(u64::MAX),
            Err(RetainedWeightError::Overflow)
        );
        assert_eq!(
            checked_retained_weight(CONTAINER_LIMIT_BYTES),
            Err(RetainedWeightError::OverAttempt {
                weight: CONTAINER_LIMIT_BYTES + FAILURE_OWNER_BASE_BYTES,
                limit: CONTAINER_LIMIT_BYTES,
            })
        );
    }

    #[test]
    fn exhaustive_reachable_sources_inner_decryption_and_structured_facts() {
        let id = (6, 0);
        let sources = [
            SourceError::RangeOverflow {
                offset: 1,
                length: 2,
            },
            SourceError::OutOfBounds {
                offset: 1,
                length: 2,
                source_len: 1,
            },
            SourceError::ReadLimitExceeded {
                requested: 2,
                limit: 1,
            },
            SourceError::PlatformLimitExceeded {
                requested: 2,
                limit: 1,
            },
            SourceError::AllocationFailed { requested: 2 },
            SourceError::UnexpectedEof {
                offset: 1,
                expected: 2,
                actual: 1,
            },
            SourceError::InvalidReadCount {
                returned: 2,
                buffer_len: 1,
            },
            SourceError::Io(std::io::Error::other("injected")),
            SourceError::SourceChanged,
        ];
        for source in sources {
            let value = classify(id, IndexedReaderError::Source(source));
            assert_eq!(value.class(), ObjStmFailureClass::FlightOnly);
            assert!(matches!(value.fact(), ObjStmFact::FlightAccess { .. }));
            assert!(value.dynamic_capacity().unwrap() > 0);
        }

        let stable = [
            lopdf::Error::InvalidObjectStream("bad".to_string()),
            lopdf::Error::InvalidStream("bad".to_string()),
            lopdf::Error::InvalidOffset(1),
            lopdf::Error::Decompress(DecompressError::Ascii85("bad")),
            lopdf::Error::Decompress(DecompressError::AsciiHex("bad")),
            lopdf::Error::Decompress(DecompressError::Predictor("bad")),
        ];
        for source in stable {
            assert_eq!(
                classify(
                    id,
                    IndexedReaderError::ObjectStreamMember {
                        id,
                        container: id,
                        index: 0,
                        source,
                    }
                )
                .class(),
                ObjStmFailureClass::PersistentNative
            );
        }
        for source in [
            lopdf::Error::Decompress(DecompressError::MemoryLimitExceeded { limit: 8 }),
            lopdf::Error::IO(std::io::Error::other("injected")),
            lopdf::Error::Unimplemented("injected"),
            lopdf::Error::ObjectType {
                expected: "stream",
                found: "array",
            },
            lopdf::Error::ObjectNotFound((91, 2)),
            lopdf::Error::ReferenceCycle((92, 3)),
            lopdf::Error::Syntax("injected".to_string()),
        ] {
            assert_eq!(
                classify(
                    id,
                    IndexedReaderError::ObjectStreamMember {
                        id,
                        container: id,
                        index: 0,
                        source,
                    }
                )
                .class(),
                ObjStmFailureClass::FlightOnly
            );
        }

        for source in [
            DecryptionError::NotDecryptable,
            DecryptionError::InvalidKeyLength,
            DecryptionError::InvalidCipherTextLength,
            DecryptionError::Padding,
        ] {
            assert_eq!(
                classify(id, IndexedReaderError::ObjectDecryption { id, source }).class(),
                ObjStmFailureClass::PersistentNative
            );
        }
        assert_eq!(
            classify(
                id,
                IndexedReaderError::ObjectDecryption {
                    id,
                    source: DecryptionError::MissingEncryptDictionary,
                }
            )
            .class(),
            ObjStmFailureClass::FlightOnly
        );
        for source in [
            DecryptionError::MissingVersion,
            DecryptionError::MissingRevision,
            DecryptionError::MissingOwnerPassword,
            DecryptionError::MissingUserPassword,
            DecryptionError::MissingPermissions,
            DecryptionError::MissingFileID,
            DecryptionError::InvalidHashLength,
            DecryptionError::InvalidPermissionLength,
            DecryptionError::InvalidVersion,
            DecryptionError::InvalidRevision,
            DecryptionError::InvalidType,
            DecryptionError::IncorrectPassword,
            DecryptionError::UnsupportedEncryption,
            DecryptionError::UnsupportedVersion,
            DecryptionError::UnsupportedRevision,
        ] {
            assert_eq!(
                classify(id, IndexedReaderError::ObjectDecryption { id, source }).class(),
                ObjStmFailureClass::FlightOnly
            );
        }

        assert_eq!(
            classify(
                id,
                IndexedReaderError::UnsupportedBoundedScalar {
                    id,
                    reason: UNSUPPORTED_FILTER,
                }
            )
            .class(),
            ObjStmFailureClass::PersistentNative
        );
        assert_eq!(
            classify(
                id,
                IndexedReaderError::UnsupportedBoundedScalar {
                    id,
                    reason: UNSUPPORTED_LENGTH,
                }
            )
            .class(),
            ObjStmFailureClass::FlightOnly
        );

        let reasons = [
            MissingNormalObjectReason::HeaderProbeLimit {
                offset: 1,
                limit: 2,
            },
            MissingNormalObjectReason::HeaderMismatch {
                expected: id,
                actual: (7, 0),
            },
            MissingNormalObjectReason::GenerationMismatch {
                requested: id,
                indexed: 0,
                actual: (6, 1),
            },
        ];
        for reason in reasons {
            let value = classify(
                id,
                IndexedReaderError::MissingNormalObjectAtXref { id, reason },
            );
            assert_eq!(value.class(), ObjStmFailureClass::PersistentNative);
            assert!(matches!(value.fact(), ObjStmFact::MissingAtXref { .. }));
        }

        let first = classify(
            (6, 0),
            IndexedReaderError::ObjectStreamMember {
                id: (71, 0),
                container: (6, 0),
                index: 3,
                source: lopdf::Error::IO(std::io::Error::other("same inner")),
            },
        );
        let second = classify(
            (99, 0),
            IndexedReaderError::ObjectStreamMember {
                id: (812, 7),
                container: (99, 0),
                index: 44,
                source: lopdf::Error::IO(std::io::Error::other("same inner")),
            },
        );
        assert_eq!(first, second);
        let ObjStmFact::FlightAccess {
            actual_id, detail, ..
        } = first.fact()
        else {
            panic!("unstable member cause must be neutral flight access")
        };
        assert_eq!(*actual_id, None);
        assert!(!detail.chars().any(|value| value.is_ascii_digit()));

        for error in [
            IndexedReaderError::ScalarResolutionCancelled {
                id: (77, 3),
                phase: "future-phase",
            },
            IndexedReaderError::ScalarResolutionClosed {
                id: (77, 3),
                phase: "future-phase",
            },
        ] {
            let value = classify((6, 0), error);
            let ObjStmFact::FlightAccess {
                actual_id, detail, ..
            } = value.fact()
            else {
                panic!("unknown scalar phase must stay charged flight access")
            };
            assert_eq!(*actual_id, Some((77, 3)));
            assert_eq!(detail.matches("future-phase").count(), 1);
        }
    }

    #[test]
    fn reachable_outer_errors_have_an_explicit_classification_table() {
        let id = (6, 0);
        let cases = [
            (
                IndexedReaderError::InvalidHeader { limit: 8 },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::InvalidStartXref { limit: 8 },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::StartXrefOutOfBounds {
                    offset: 9,
                    logical_len: 8,
                },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::InvalidXref { offset: 1 },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::IncompleteXref { offset: 1 },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::InvalidTrailer { offset: 1 },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::StructureLimitExceeded {
                    structure: "xref",
                    limit: 8,
                },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::EntryLimitExceeded { count: 9, limit: 8 },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::RevisionLimitExceeded { limit: 8 },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::InvalidTrailerOffset { key: "Prev" },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::XrefDecompression(lopdf::Error::InvalidOffset(1)),
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::MissingNormalObject { id },
                ObjStmFailureClass::PersistentNative,
            ),
            (
                IndexedReaderError::GenerationMismatch { id, indexed: 1 },
                ObjStmFailureClass::PersistentNative,
            ),
            (
                IndexedReaderError::IndirectHeaderLimitExceeded {
                    offset: 1,
                    limit: 8,
                },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::IndirectObjectMismatch {
                    expected: id,
                    actual: (7, 0),
                },
                ObjStmFailureClass::PersistentNative,
            ),
            (
                IndexedReaderError::InvalidIndirectObject { id, offset: 1 },
                ObjStmFailureClass::PersistentNative,
            ),
            (
                IndexedReaderError::IncompleteObject { id, offset: 1 },
                ObjStmFailureClass::PersistentNative,
            ),
            (
                IndexedReaderError::NotScalarObject { id },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::NotStreamObject { id },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::NegativeStreamLength { id, length: -1 },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::MissingEndstream { id },
                ObjStmFailureClass::PersistentNative,
            ),
            (
                IndexedReaderError::ResolutionCycle { id },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::ResolutionDepthExceeded { limit: 8 },
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::ObjectStreamContainerNotStream { id, container: id },
                ObjStmFailureClass::PersistentNative,
            ),
            (
                IndexedReaderError::ObjectStreamBatchSetup {
                    container: id,
                    source: lopdf::Error::InvalidOffset(1),
                },
                ObjStmFailureClass::ExactKeyInvariant,
            ),
            (
                IndexedReaderError::ObjectStreamCacheBypass { container: id },
                ObjStmFailureClass::ExactKeyInvariant,
            ),
            (
                IndexedReaderError::PasswordRequired,
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::InvalidPassword,
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::Encryption(lopdf::Error::InvalidOffset(1)),
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::InvalidEncryptDictionary,
                ObjStmFailureClass::FlightOnly,
            ),
            (
                IndexedReaderError::PageCountLimitExceeded { limit: 8 },
                ObjStmFailureClass::FlightOnly,
            ),
        ];
        for (error, expected) in cases {
            assert_eq!(classify(id, error).class(), expected);
        }
    }
}
