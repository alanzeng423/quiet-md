//! Bidirectional sync between local Quiet.md DocxXML documents and Feishu /
//! Lark cloud documents (and wiki pages), without any GPUI dependency.
//!
//! - [`link`]: parses `/docx/` and `/wiki/` links (and bare tokens).
//! - [`codec`]: normalizes cloud DocxXML (`id`, `seq-marker`, `rgb()`, media tokens).
//! - [`planner`]: wraps [`crate::merge::merge3`] with fold/cover preservation,
//!   plans minimal incremental block [`PushOp`]s (preserving Feishu comments),
//!   and checks cloud-unsupported local extensions.
//! - [`state`]: per-root local sync state (`quiet-md/feishu/<root-key>/`).
//! - [`transport`]: [`FeishuTransport`] abstraction + [`LarkCliTransport`] backend.
//! - [`engine`]: high-level import, probe, pull, push and create workflows.

pub mod codec;
pub mod engine;
pub mod link;
pub mod planner;
pub mod state;
pub mod transport;

pub use codec::{CloudDoc, MediaMap, cloud_xml, image_tokens, is_blank, keys, localize_media, new_local_images};
pub use engine::{ImportOutcome, PullOutcome, PushOutcome, check_remote_changed, create_remote, import_from_link, pull_doc, push_doc};
pub use link::{PastedLink, docx_token_of, parse_link, wiki_kind};
pub use planner::{MergeOutcome, PushOp, blockers, blockers_in, merge_docs, plan_push, plan_push_sent};
pub use state::{FeishuLink, SyncDir};
pub use transport::{AuthInfo, CreatedDoc, DocProbe, FeishuTransport, Identity, RawCloudDoc, TransportError, WikiTarget};
#[cfg(not(target_family = "wasm"))]
pub use transport::LarkCliTransport;

#[cfg(test)]
mod tests;
