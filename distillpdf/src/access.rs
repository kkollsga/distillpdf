//! Owned PDF object/source access boundary.
//!
//! Extraction code must not retain a borrow into lopdf's eager `Document`: L3 replaces the
//! backend with on-demand owned resolution, where no such document-wide borrow exists.  Short
//! reads therefore happen through [`ObjectHandle::read`], while values that escape a read are
//! explicitly owned.  The eager implementation remains the compatibility oracle through L9.

use lopdf::{BytesSource, Dictionary, Document, Object, ObjectId, RandomAccessSource, SourceResult};
use std::fmt;
use std::sync::Arc;

/// One page entry, detached from the backend's page-map allocation.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct PageRef {
    pub(crate) number: u32,
    pub(crate) id: ObjectId,
}

/// A stable internal access failure. L2d expands this into the complete suppression key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AccessError {
    pub(crate) object: ObjectId,
    pub(crate) detail: String,
}

impl AccessError {
    fn object(object: ObjectId, error: impl fmt::Display) -> Self {
        Self {
            object,
            detail: error.to_string(),
        }
    }
}

impl fmt::Display for AccessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "object {} {}: {}",
            self.object.0, self.object.1, self.detail
        )
    }
}

/// A resolved object whose storage remains pinned for the lifetime of this handle.
///
/// `Eager` stores the document and requested id rather than cloning an arbitrarily large object
/// graph. `Owned` is the lazy-reader shape: one independently owned resolved object. Neither
/// variant exposes an object-derived borrow beyond the closure passed to [`Self::read`].
#[derive(Clone)]
pub(crate) enum ObjectHandle {
    Eager {
        document: Arc<Document>,
        id: ObjectId,
    },
    #[allow(dead_code)] // constructed by the L3 indexed adapter
    Owned(Arc<Object>),
}

/// A typed object handle that can only be inspected as a stream.
#[derive(Clone)]
pub(crate) struct StreamHandle {
    object: ObjectHandle,
}

impl StreamHandle {
    fn new(id: ObjectId, object: ObjectHandle) -> Result<Self, AccessError> {
        let is_stream = object.read(|value| value.as_stream().is_ok())?;
        if !is_stream {
            return Err(AccessError::object(id, "resolved object is not a stream"));
        }
        Ok(Self { object })
    }

    /// Inspect the stream while its object owner is pinned. A type mismatch degrades to `None`.
    pub(crate) fn read<R>(&self, inspect: impl FnOnce(&lopdf::Stream) -> R) -> Option<R> {
        self.object
            .read(|value| value.as_stream().ok().map(inspect))
            .ok()
            .flatten()
    }
}

impl ObjectHandle {
    /// Inspect the resolved value without allowing its borrow to escape the handle.
    pub(crate) fn read<R>(&self, inspect: impl FnOnce(&Object) -> R) -> Result<R, AccessError> {
        match self {
            Self::Eager { document, id } => {
                let object = document
                    .get_object(*id)
                    .map_err(|error| AccessError::object(*id, error))?;
                Ok(inspect(object))
            }
            Self::Owned(object) => Ok(inspect(object)),
        }
    }

    #[allow(dead_code)] // constructed by the L3 indexed adapter; unit-tested in L2
    fn owned(object: Object) -> Self {
        Self::Owned(Arc::new(object))
    }
}

/// Backend-neutral access to immutable PDF objects, pages and source bytes.
///
/// The trait is object-safe so eager and lazy implementations are runtime-selectable. Resolved
/// objects cross this boundary only as handles; raw bytes cross as an immutable random-access
/// source rather than a document-wide `&[u8]`.
pub(crate) trait DocumentAccess: Send + Sync {
    fn object(&self, id: ObjectId) -> Result<ObjectHandle, AccessError>;
    fn stream(&self, id: ObjectId) -> Result<StreamHandle, AccessError> {
        StreamHandle::new(id, self.object(id)?)
    }
    fn pages(&self) -> Result<Vec<PageRef>, AccessError>;
    /// Every indexed indirect object id in deterministic order.
    fn object_ids(&self) -> Vec<ObjectId>;
    /// Page `/Resources` dictionaries in outermost-to-page overlay order.
    fn page_resource_chain(&self, page: ObjectId) -> Result<Vec<Dictionary>, AccessError>;
    #[allow(dead_code)] // raw-recovery consumers migrate in L2b
    fn source(&self) -> Arc<dyn RandomAccessSource>;
}

/// The behavior-preserving adapter over lopdf's fully loaded object graph.
#[derive(Clone)]
pub(crate) struct EagerDocumentAdapter {
    document: Arc<Document>,
    #[allow(dead_code)] // retained now so L2b recovery never reintroduces raw Vec borrowing
    source: Arc<dyn RandomAccessSource>,
}

impl EagerDocumentAdapter {
    pub(crate) fn new(document: Arc<Document>, raw: Arc<[u8]>) -> Self {
        Self {
            document,
            source: Arc::new(BytesSource::new(raw)),
        }
    }
}

/// Test-only bridge for pre-boundary fixtures that build lopdf documents in memory.
#[allow(dead_code)] // compatibility fixture bridge; production never clones a Document here
pub(crate) fn test_adapter(document: &Document) -> EagerDocumentAdapter {
    EagerDocumentAdapter::new(Arc::new(document.clone()), Arc::from(&b""[..]))
}

impl DocumentAccess for EagerDocumentAdapter {
    fn object(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
        // Validate now so a successfully-created handle is never a deferred missing-object
        // surprise. The immutable eager document makes the same lookup stable at read time.
        self.document
            .get_object(id)
            .map_err(|error| AccessError::object(id, error))?;
        Ok(ObjectHandle::Eager {
            document: Arc::clone(&self.document),
            id,
        })
    }

    fn pages(&self) -> Result<Vec<PageRef>, AccessError> {
        Ok(self
            .document
            .get_pages()
            .into_iter()
            .map(|(number, id)| PageRef { number, id })
            .collect())
    }

    fn object_ids(&self) -> Vec<ObjectId> {
        self.document.objects.keys().copied().collect()
    }

    fn page_resource_chain(&self, page: ObjectId) -> Result<Vec<Dictionary>, AccessError> {
        let (own, inherited) = self
            .document
            .get_page_resources(page)
            .map_err(|error| AccessError::object(page, error))?;
        let mut out: Vec<Dictionary> = inherited
            .iter()
            .rev()
            .filter_map(|id| self.document.get_dictionary(*id).ok().cloned())
            .collect();
        if let Some(resources) = own {
            out.push(resources.clone());
        }
        Ok(out)
    }

    fn source(&self) -> Arc<dyn RandomAccessSource> {
        Arc::clone(&self.source)
    }
}

/// Inspect a direct object as-is or resolve a reference through the selected backend first.
///
/// This is the owned-boundary replacement for helpers returning `Option<&Object>`. The callback
/// result cannot borrow from its argument, so arrays, dictionaries, streams, names and strings
/// remain pinned for the complete short read and cannot escape accidentally.
pub(crate) fn read_resolved<R>(
    access: &dyn DocumentAccess,
    object: &Object,
    inspect: impl FnOnce(&Object) -> R,
) -> Result<R, AccessError> {
    match object {
        Object::Reference(id) => access.object(*id)?.read(inspect),
        direct => Ok(inspect(direct)),
    }
}

/// An immutable encoded-source slice that carries its source owner with it.
#[derive(Clone)]
#[allow(dead_code)] // encoded stream descriptors start returning this in L3
pub(crate) struct SourceRange {
    source: Arc<dyn RandomAccessSource>,
    offset: u64,
    length: u64,
}

#[allow(dead_code)]
impl SourceRange {
    pub(crate) fn new(source: Arc<dyn RandomAccessSource>, offset: u64, length: u64) -> Self {
        Self {
            source,
            offset,
            length,
        }
    }

    /// Materialize the range only after enforcing the caller's explicit allocation limit.
    pub(crate) fn read(&self, limit: u64) -> SourceResult<Vec<u8>> {
        self.source.read_range(self.offset, self.length, limit)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn adapter(objects: Vec<Object>, raw: &[u8]) -> (EagerDocumentAdapter, Vec<ObjectId>) {
        let mut document = Document::with_version("1.7");
        let ids = objects
            .into_iter()
            .map(|object| document.add_object(object))
            .collect();
        (
            EagerDocumentAdapter::new(Arc::new(document), Arc::from(raw)),
            ids,
        )
    }

    #[test]
    fn direct_and_reference_chains_match_eager_dereference() {
        let (adapter, ids) = adapter(
            vec![
                Object::Integer(41),
                Object::Reference((1, 0)),
                Object::Reference((2, 0)),
            ],
            b"source",
        );
        assert_eq!(
            read_resolved(&adapter, &Object::Integer(7), |o| o.as_i64().unwrap()).unwrap(),
            7
        );
        assert_eq!(
            adapter
                .object(ids[2])
                .unwrap()
                .read(|o| o.as_i64().unwrap())
                .unwrap(),
            41
        );
    }

    #[test]
    fn dangling_cycle_over_limit_and_generation_mismatch_are_errors() {
        let mut objects = vec![Object::Reference((999, 0)), Object::Reference((2, 0))];
        // Lopdf follows at most 128 references. Make a separate 130-hop chain.
        for number in 3..=132_u32 {
            objects.push(Object::Reference((number + 1, 0)));
        }
        objects.push(Object::Integer(9));
        let (adapter, ids) = adapter(objects, b"");

        assert!(adapter.object(ids[0]).is_err());
        assert!(adapter.object(ids[1]).is_err()); // 2 0 R points to itself
        assert!(adapter.object(ids[2]).is_err());
        assert!(adapter.object((ids.last().unwrap().0, 1)).is_err());
    }

    #[test]
    fn owned_handle_and_source_range_keep_their_owners_alive() {
        let handle = ObjectHandle::owned(Object::String(
            b"owned".to_vec(),
            lopdf::StringFormat::Literal,
        ));
        assert_eq!(
            handle.read(|o| o.as_str().unwrap().to_vec()).unwrap(),
            b"owned"
        );

        let (adapter, _) = adapter(Vec::new(), b"abcdef");
        let range = SourceRange::new(adapter.source(), 2, 3);
        drop(adapter);
        assert_eq!(range.read(3).unwrap(), b"cde");
        assert!(range.read(2).is_err());
    }

    #[test]
    fn typed_stream_handles_reject_non_streams() {
        let (adapter, ids) = adapter(vec![Object::Integer(1)], b"");
        assert!(adapter.stream(ids[0]).is_err());
    }
}
