//! A deployment that puts names to faces still answers its operator.
//!
//! Recognition needs a vision embedder registered on the store at the writable
//! open. Answering `vigil why`/`events`/`stats` while the runtime is up needs
//! an owner handler registered on the store at the writable open. There is only
//! ever ONE writable open — contextdb's single-owner model refuses a second —
//! so a runtime with recognition switched on must not have to choose which of
//! the two its open carries. If it did, an operator running recognition would
//! lose the live command line, or a deployment that wants the live command line
//! would have to turn recognition off; neither is a trade the product offers.
//!
//! Context-graph already carries the composed door
//! (`Store::open_with_control_handler_and_embedder_registrations`). What is
//! pinned here is vigil's side of it: one vigil call that opens the recognition
//! store AND registers the owner handler, and a store handle on which the two
//! planes stay independent — serving an owner frame embeds nothing, embedding
//! runs no handler, and neither has to be torn down for the other to work.
//!
//! Every assertion is a counter read after a deterministic drive; nothing waits
//! on a clock, and the embedder double is content-faithful (identical bytes →
//! identical vector) rather than rigged.
//!
//! ## Vigil contract this file pins
//!
//! ```ignore
//! vigil::recognition::open_store_with_embedder_and_owner_handler(
//!     path: &std::path::Path,
//!     embedding_space_id: &str,
//!     embedder: std::sync::Arc<dyn context_graph::Embedder>,
//!     handler: context_graph::ControlHandler,
//! ) -> Result<context_graph::Store, String>
//! ```
//!
//! — the same shape as today's `open_store_with_embedder`, plus the handler,
//! resolving to context-graph's composed door. The recognition-free open keeps
//! its own door; nothing here asks recognition to be on for the owner route to
//! work.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use context_graph::owner_control::request_owner;
use context_graph::{
    ContextId, ControlHandler, CreateContext, DistanceMetric, Embedder, EmbeddingInput,
    EmbeddingOutput, EmbeddingSpace, EvidenceKind, EvidenceProducer, Store,
};
use vigil::recognition::{match_crop_for_class, open_store_with_embedder_and_owner_handler};

/// The deployment's own vision space, so a vector counted in it can only have
/// come from the registration this open carried.
const SPACE: &str = "vigil_site_vision_owner_route";
const DIM: usize = 768;

/// Content-faithful deterministic double that counts what it was asked to
/// embed. Identical bytes give identical vectors; different bytes give
/// effectively orthogonal ones. Never a rigged shape.
struct CountingEmbedder {
    embeds: Arc<AtomicUsize>,
}

fn hash_vector(bytes: &[u8]) -> Vec<f32> {
    let mut vector = vec![0.0f32; DIM];
    let mut state: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        state = state
            .wrapping_mul(0x100_0000_01b3)
            .wrapping_add(*byte as u64);
        let slot = (state as usize) % DIM;
        vector[slot] += ((state >> 32) as f32 / u32::MAX as f32) - 0.5;
    }
    let norm = vector.iter().map(|x| x * x).sum::<f32>().sqrt().max(1e-6);
    vector.iter_mut().for_each(|x| *x /= norm);
    vector
}

fn producer() -> EvidenceProducer {
    EvidenceProducer {
        system: "vigil-test".to_string(),
        model_name: "hash-double".to_string(),
        model_version: "1".to_string(),
        pipeline_version: "1".to_string(),
    }
}

impl Embedder for CountingEmbedder {
    fn embed(&self, input: EmbeddingInput) -> context_graph::Result<EmbeddingOutput> {
        self.embeds.fetch_add(1, Ordering::SeqCst);
        let bytes = match input {
            EmbeddingInput::ImageBytes(bytes) => bytes,
            EmbeddingInput::ImageFrame { uri } => std::fs::read(&uri).unwrap_or_default(),
            other => {
                return Err(context_graph::CgError::Engine(format!(
                    "this deployment embeds images only, got {other:?}"
                )));
            }
        };
        Ok(EmbeddingOutput {
            embedding_space_id: SPACE.to_string(),
            dimension: DIM,
            producer: producer(),
            vector: hash_vector(&bytes),
        })
    }

    fn supported_inputs(&self) -> Vec<EvidenceKind> {
        vec![EvidenceKind::ImageFrame]
    }

    fn embedding_space(&self) -> EmbeddingSpace {
        EmbeddingSpace {
            embedding_space_id: SPACE.to_string(),
            dimension: DIM,
            metric: DistanceMetric::Cosine,
            model_family: "vision".to_string(),
            model_name: "hash-double".to_string(),
            model_version: "1".to_string(),
            supported_evidence_kinds: vec![EvidenceKind::ImageFrame],
            default_for_evidence_kind: None,
            bindings: Vec::new(),
        }
    }
}

fn counting_handler() -> (ControlHandler, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let handler: ControlHandler = Arc::new(move |frame: String| {
        counter.fetch_add(1, Ordering::SeqCst);
        format!("served-by=owner\n{}", frame.trim())
    });
    (handler, calls)
}

fn site_context(store: &Store) -> ContextId {
    store
        .create_context(CreateContext {
            name: "recognition-owner-route-site".to_string(),
            labels: Vec::new(),
            properties: BTreeMap::new(),
        })
        .expect("the site context this deployment's sightings belong to")
        .id
}

#[test]
fn a_recognition_runtime_answers_the_owner_route_and_keeps_embedding_on_the_same_handle() {
    let dir = tempfile::tempdir().expect("temporary deployment directory");
    let store_path = dir.path().join("data").join("store.contextgraph");
    let embeds = Arc::new(AtomicUsize::new(0));
    let (handler, owner_calls) = counting_handler();

    let store = open_store_with_embedder_and_owner_handler(
        &store_path,
        SPACE,
        Arc::new(CountingEmbedder {
            embeds: Arc::clone(&embeds),
        }),
        handler,
    )
    .expect("one writable open carries recognition and the owner handler together");
    let context_id = site_context(&store);

    let answer = request_owner(&store_path, "stats \n")
        .expect("a recognition-enabled runtime still answers the owner route");
    assert!(
        answer.contains("stats"),
        "the owner's own handler must have produced the answer; got {answer:?}"
    );
    assert_eq!(
        owner_calls.load(Ordering::SeqCst),
        1,
        "exactly one frame reached the owner handler"
    );
    assert_eq!(
        embeds.load(Ordering::SeqCst),
        0,
        "serving an owner frame must not run the vision embedder"
    );

    // The recognition plane, on the very handle that just served the operator.
    let outcome = match_crop_for_class(
        &store,
        &CountingEmbedder {
            embeds: Arc::clone(&embeds),
        },
        SPACE,
        context_id,
        b"a-crop-nobody-is-enrolled-against",
        0.6,
        "person",
    )
    .expect("recognition still matches while the owner handler is registered");
    assert_eq!(
        outcome.name, None,
        "an unenrolled crop matches nobody — what is pinned here is that the search RAN against \
         the registered space, not what it found; got {outcome:?}"
    );
    assert_eq!(
        outcome.embedding_space_id, SPACE,
        "the match ran in the space this open registered"
    );
    assert_eq!(
        embeds.load(Ordering::SeqCst),
        1,
        "the embedder must still be usable on this handle after the handler served"
    );
    assert_eq!(
        owner_calls.load(Ordering::SeqCst),
        1,
        "embedding must not run the owner handler"
    );

    let second = request_owner(&store_path, "events \n")
        .expect("the owner route stays open after recognition work on the same handle");
    assert!(
        second.contains("events"),
        "the second owner frame must reach the same handler; got {second:?}"
    );
    assert_eq!(
        owner_calls.load(Ordering::SeqCst),
        2,
        "both owner frames reached the handler, with recognition work between them"
    );
    assert_eq!(
        embeds.load(Ordering::SeqCst),
        1,
        "the second owner frame must not run the vision embedder either"
    );
}
