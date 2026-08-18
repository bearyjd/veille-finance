//! Optional LLM narration (PRP §8). The narrator is a pure function of the
//! findings and deltas for one tenant and one period; it receives no
//! credentials and no data beyond what the digest already shows. Failure is
//! non-fatal by construction: every error path returns `None` and the digest
//! ships with its plain templated rendering.

pub mod llm;

pub use llm::{LlmConfig, narrate};
