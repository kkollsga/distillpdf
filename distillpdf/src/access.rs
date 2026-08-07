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
enum ObjectOwner {
    Eager {
        document: Arc<Document>,
        id: ObjectId,
    },
    #[allow(dead_code)] // constructed by the L3 indexed adapter
    Owned { object: Arc<Object>, id: ObjectId },
}

#[derive(Clone)]
enum PathStep {
    DictionaryKey(Vec<u8>),
    StreamDictionaryKey(Vec<u8>),
    ArrayIndex(usize),
}

#[derive(Clone)]
pub(crate) struct ObjectHandle {
    owner: ObjectOwner,
    path: Vec<PathStep>,
}

/// A typed object handle that can only be inspected as a stream.
#[derive(Clone)]
pub(crate) struct StreamHandle {
    object: ObjectHandle,
}

/// A dictionary view whose root object owner remains pinned for every short read.
#[derive(Clone)]
pub(crate) struct DictionaryHandle {
    object: ObjectHandle,
}

impl DictionaryHandle {
    fn new(object: ObjectHandle) -> Result<Self, AccessError> {
        let id = object.root_id();
        if !object.read(|value| value.as_dict().is_ok())? {
            return Err(AccessError::object(id, "resolved object is not a dictionary"));
        }
        Ok(Self { object })
    }

    pub(crate) fn read<R>(&self, inspect: impl FnOnce(&Dictionary) -> R) -> Result<R, AccessError> {
        let id = self.object.root_id();
        self.object.read(|value| value.as_dict().map(inspect))?.map_err(|error| AccessError::object(id, error))
    }

    #[allow(dead_code)] // resource consumers migrate incrementally through L2b
    pub(crate) fn entry(
        &self,
        access: &dyn DocumentAccess,
        key: &[u8],
    ) -> Result<ObjectHandle, AccessError> {
        self.object.dictionary_entry(access, key)
    }
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
    fn eager(document: Arc<Document>, id: ObjectId) -> Self {
        Self {
            owner: ObjectOwner::Eager { document, id },
            path: Vec::new(),
        }
    }

    /// Inspect the resolved value without allowing its borrow to escape the handle.
    pub(crate) fn read<R>(&self, inspect: impl FnOnce(&Object) -> R) -> Result<R, AccessError> {
        let (mut object, id) = match &self.owner {
            ObjectOwner::Eager { document, id } => {
                let object = document
                    .get_object(*id)
                    .map_err(|error| AccessError::object(*id, error))?;
                (object, *id)
            }
            ObjectOwner::Owned { object, id } => (object.as_ref(), *id),
        };
        for step in &self.path {
            object = match step {
                PathStep::DictionaryKey(key) => object
                    .as_dict()
                    .and_then(|dictionary| dictionary.get(key))
                    .map_err(|error| AccessError::object(id, error))?,
                PathStep::StreamDictionaryKey(key) => object
                    .as_stream()
                    .and_then(|stream| stream.dict.get(key))
                    .map_err(|error| AccessError::object(id, error))?,
                PathStep::ArrayIndex(index) => object
                    .as_array()
                    .ok()
                    .and_then(|array| array.get(*index))
                    .ok_or_else(|| AccessError::object(id, format!("array index {index} is out of bounds")))?,
            };
        }
        Ok(inspect(object))
    }

    #[allow(dead_code)] // constructed by the L3 indexed adapter; unit-tested in L2
    fn owned(id: ObjectId, object: Object) -> Self {
        Self {
            owner: ObjectOwner::Owned {
                object: Arc::new(object),
                id,
            },
            path: Vec::new(),
        }
    }

    fn child(&self, access: &dyn DocumentAccess, step: PathStep) -> Result<Self, AccessError> {
        let reference = {
            let mut path = self.path.clone();
            path.push(step.clone());
            let candidate = Self {
                owner: self.owner.clone(),
                path,
            };
            candidate.read(|object| object.as_reference().ok())?
        };
        if let Some(id) = reference {
            access.object(id)
        } else {
            let mut path = self.path.clone();
            path.push(step);
            Ok(Self {
                owner: self.owner.clone(),
                path,
            })
        }
    }

    /// A dictionary entry that stays attached to the root object which owns an inline value.
    #[allow(dead_code)] // consumer migrations begin in L2b
    pub(crate) fn dictionary_entry(
        &self,
        access: &dyn DocumentAccess,
        key: &[u8],
    ) -> Result<Self, AccessError> {
        let step = self.read(|object| match object {
            Object::Dictionary(_) => Some(PathStep::DictionaryKey(key.to_vec())),
            Object::Stream(_) => Some(PathStep::StreamDictionaryKey(key.to_vec())),
            _ => None,
        })?;
        self.child(
            access,
            step.ok_or_else(|| AccessError::object(self.root_id(), "object has no dictionary"))?,
        )
    }

    /// An array entry that stays attached to the root object which owns it.
    #[allow(dead_code)] // consumer migrations begin in L2b
    pub(crate) fn array_entry(
        &self,
        access: &dyn DocumentAccess,
        index: usize,
    ) -> Result<Self, AccessError> {
        self.child(access, PathStep::ArrayIndex(index))
    }

    fn root_id(&self) -> ObjectId {
        match &self.owner {
            ObjectOwner::Eager { id, .. } | ObjectOwner::Owned { id, .. } => *id,
        }
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
    fn page_resource_chain(&self, page: ObjectId) -> Result<Vec<DictionaryHandle>, AccessError>;
    #[allow(dead_code)] // raw-recovery consumers migrate in L2b
    fn source(&self) -> Arc<dyn RandomAccessSource>;
    /// Perform the one explicit whole-source scan permitted to the recovery index.
    #[allow(dead_code)] // the L2b recovery index is the first production caller
    fn scan_source(&self, limit: u64) -> SourceResult<Vec<u8>> {
        let source = self.source();
        let length = source.len()?;
        source.read_range(0, length, limit)
    }
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
#[cfg(test)]
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
        Ok(ObjectHandle::eager(Arc::clone(&self.document), id))
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

    fn page_resource_chain(&self, page: ObjectId) -> Result<Vec<DictionaryHandle>, AccessError> {
        let (own, inherited) = self
            .document
            .get_page_resources(page)
            .map_err(|error| AccessError::object(page, error))?;
        let mut out: Vec<DictionaryHandle> = inherited
            .iter()
            .rev()
            .filter_map(|id| DictionaryHandle::new(self.object(*id).ok()?).ok())
            .collect();
        if own.is_some() {
            let page_handle = self.object(page)?;
            if let Ok(resources) = page_handle.dictionary_entry(self, b"Resources") {
                if let Ok(resources) = DictionaryHandle::new(resources) {
                    out.push(resources);
                }
            }
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
pub(crate) mod tests {
    use super::*;
    use lopdf::dictionary;
    use lopdf::SourceError;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    pub(crate) struct AccessCounts {
        pub(crate) opens: AtomicU64,
        pub(crate) object_reads: AtomicU64,
        pub(crate) object_lists: AtomicU64,
        pub(crate) page_reads: AtomicU64,
        pub(crate) resource_reads: AtomicU64,
        pub(crate) source_requests: AtomicU64,
        pub(crate) source_reads: AtomicU64,
        pub(crate) source_scans: AtomicU64,
        pub(crate) max_request: AtomicU64,
    }

    struct CountingSource {
        inner: Arc<dyn RandomAccessSource>,
        counts: Arc<AccessCounts>,
        fail_reads: bool,
    }

    impl RandomAccessSource for CountingSource {
        fn len(&self) -> SourceResult<u64> {
            self.inner.len()
        }

        fn read_at(&self, offset: u64, out: &mut [u8]) -> SourceResult<usize> {
            self.counts.source_reads.fetch_add(1, Ordering::Relaxed);
            self.counts
                .max_request
                .fetch_max(out.len() as u64, Ordering::Relaxed);
            if self.fail_reads {
                return Err(SourceError::UnexpectedEof {
                    offset,
                    expected: out.len() as u64,
                    actual: 0,
                });
            }
            self.inner.read_at(offset, out)
        }

        fn validate_unchanged(&self) -> SourceResult<()> {
            self.inner.validate_unchanged()
        }
    }

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub(crate) enum FaultPoint {
        Object,
        Pages,
        Resources,
        Source,
    }

    /// Test-only counter/fault wrapper. Every future boundary operation must pass through this
    /// shape so L2d can prove suppression without falling back to the eager backend.
    pub(crate) struct FaultAccess {
        inner: Arc<dyn DocumentAccess>,
        fault: Option<FaultPoint>,
        pub(crate) counts: Arc<AccessCounts>,
    }

    impl FaultAccess {
        pub(crate) fn new(
            inner: Arc<dyn DocumentAccess>,
            fault: Option<FaultPoint>,
            counts: Arc<AccessCounts>,
        ) -> Self {
            Self {
                inner,
                fault,
                counts,
            }
        }

        fn failure(&self, point: FaultPoint, object: ObjectId) -> Result<(), AccessError> {
            if self.fault == Some(point) {
                Err(AccessError::object(object, format!("injected {point:?} failure")))
            } else {
                Ok(())
            }
        }
    }

    impl DocumentAccess for FaultAccess {
        fn object(&self, id: ObjectId) -> Result<ObjectHandle, AccessError> {
            self.counts.object_reads.fetch_add(1, Ordering::Relaxed);
            self.failure(FaultPoint::Object, id)?;
            self.inner.object(id)
        }

        fn pages(&self) -> Result<Vec<PageRef>, AccessError> {
            self.counts.page_reads.fetch_add(1, Ordering::Relaxed);
            self.failure(FaultPoint::Pages, (0, 0))?;
            self.inner.pages()
        }

        fn object_ids(&self) -> Vec<ObjectId> {
            self.counts.object_lists.fetch_add(1, Ordering::Relaxed);
            self.inner.object_ids()
        }

        fn page_resource_chain(
            &self,
            page: ObjectId,
        ) -> Result<Vec<DictionaryHandle>, AccessError> {
            self.counts.resource_reads.fetch_add(1, Ordering::Relaxed);
            self.failure(FaultPoint::Resources, page)?;
            self.inner.page_resource_chain(page)
        }

        fn source(&self) -> Arc<dyn RandomAccessSource> {
            self.counts.source_requests.fetch_add(1, Ordering::Relaxed);
            Arc::new(CountingSource {
                inner: self.inner.source(),
                counts: Arc::clone(&self.counts),
                fail_reads: self.fault == Some(FaultPoint::Source),
            })
        }

        fn scan_source(&self, limit: u64) -> SourceResult<Vec<u8>> {
            self.counts.source_scans.fetch_add(1, Ordering::Relaxed);
            let source = self.source();
            let length = source.len()?;
            source.read_range(0, length, limit)
        }
    }

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
        let handle = ObjectHandle::owned((7, 0), Object::String(
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

    #[test]
    fn nested_handles_pin_inline_values_and_resolve_nested_references() {
        let nested = Object::Dictionary(lopdf::dictionary! {
            "Direct" => Object::Array(vec![Object::String(
                b"inline".to_vec(),
                lopdf::StringFormat::Literal,
            )]),
            "Indirect" => Object::Reference((1, 0)),
        });
        let (adapter, ids) = adapter(vec![Object::Integer(42), nested], b"");
        let root = adapter.object(ids[1]).unwrap();
        let direct = root
            .dictionary_entry(&adapter, b"Direct")
            .unwrap()
            .array_entry(&adapter, 0)
            .unwrap();
        let indirect = root.dictionary_entry(&adapter, b"Indirect").unwrap();
        drop(root);
        assert_eq!(
            direct.read(|object| object.as_str().unwrap().to_vec()).unwrap(),
            b"inline"
        );
        assert_eq!(indirect.read(|object| object.as_i64().unwrap()).unwrap(), 42);
    }

    #[test]
    fn resource_chain_handles_pin_inline_page_and_indirect_parent_dictionaries() {
        let mut document = Document::with_version("1.7");
        let outer_resources = document.add_object(Object::Dictionary(dictionary! {
            "Outer" => Object::Integer(1),
        }));
        let pages = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Pages",
            "Kids" => Object::Array(Vec::new()),
            "Count" => 1,
            "Resources" => Object::Reference(outer_resources),
        }));
        let page = document.add_object(Object::Dictionary(dictionary! {
            "Type" => "Page",
            "Parent" => Object::Reference(pages),
            "Resources" => Object::Dictionary(dictionary! {
                "Inner" => Object::Integer(2),
            }),
        }));
        document
            .get_object_mut(pages)
            .unwrap()
            .as_dict_mut()
            .unwrap()
            .set("Kids", Object::Array(vec![Object::Reference(page)]));
        let adapter = EagerDocumentAdapter::new(Arc::new(document), Arc::from(&b""[..]));
        let resources = adapter.page_resource_chain(page).unwrap();
        assert_eq!(resources.len(), 2);
        drop(adapter);
        assert!(resources[0].read(|dict| dict.has(b"Outer")).unwrap());
        assert!(resources[1].read(|dict| dict.has(b"Inner")).unwrap());
    }

    #[test]
    fn fault_access_injects_each_operation_and_counts_bounded_source_reads() {
        let (adapter, ids) = adapter(vec![Object::Integer(7)], b"abcdef");
        for point in [
            FaultPoint::Object,
            FaultPoint::Pages,
            FaultPoint::Resources,
            FaultPoint::Source,
        ] {
            let counts = Arc::new(AccessCounts::default());
            counts.opens.fetch_add(1, Ordering::Relaxed);
            let fault = FaultAccess::new(Arc::new(adapter.clone()), Some(point), counts);
            let error = match point {
                FaultPoint::Object => fault.object(ids[0]).err().unwrap(),
                FaultPoint::Pages => fault.pages().err().unwrap(),
                FaultPoint::Resources => fault.page_resource_chain(ids[0]).err().unwrap(),
                FaultPoint::Source => {
                    let source_error = fault.source().read_range(0, 1, 1).err().unwrap();
                    assert!(matches!(source_error, SourceError::UnexpectedEof { .. }));
                    assert_eq!(fault.counts.source_reads.load(Ordering::Relaxed), 1);
                    continue;
                }
            };
            assert!(error.detail.contains("injected"));
            assert_eq!(fault.counts.opens.load(Ordering::Relaxed), 1);
        }

        let counts = Arc::new(AccessCounts::default());
        counts.opens.fetch_add(1, Ordering::Relaxed);
        let counted = FaultAccess::new(Arc::new(adapter), None, counts);
        assert_eq!(counted.object_ids(), ids);
        assert_eq!(counted.counts.object_lists.load(Ordering::Relaxed), 1);
        assert_eq!(counted.source().read_range(1, 3, 3).unwrap(), b"bcd");
        assert_eq!(counted.counts.source_requests.load(Ordering::Relaxed), 1);
        assert_eq!(counted.counts.source_reads.load(Ordering::Relaxed), 1);
        assert_eq!(counted.counts.max_request.load(Ordering::Relaxed), 3);
        assert_eq!(counted.counts.source_scans.load(Ordering::Relaxed), 0);
        assert_eq!(counted.scan_source(6).unwrap(), b"abcdef");
        assert_eq!(counted.counts.source_scans.load(Ordering::Relaxed), 1);
    }
}
