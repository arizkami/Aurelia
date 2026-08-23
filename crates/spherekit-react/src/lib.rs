//! Rust-side host tree for the TypeScript React renderer.
//!
//! The TypeScript side owns React reconciliation. After each commit it sends a
//! [`NativeTree`] snapshot to this crate. The host validates the snapshot,
//! retains it, indexes it and lowers it to native SphereKit elements. Keeping
//! the boundary as data rather than React internals is what makes the backend
//! usable from V8, from an FFI host, or from a test harness with no JavaScript
//! anywhere near it.
//!
//! ## The shape of a frame
//!
//! ```text
//!   React commit  ->  ReactHost::commit        validate, retain, index
//!                          |
//!                          v
//!                     ui_element_with_events   cascade + lower to elements
//!                          |
//!                          v
//!                     UiTree -> layout -> paint
//!                          |
//!   React callback  <-  EventQueue::drain      native events, by node id
//! ```
//!
//! Every arrow is synchronous and single-threaded. Nothing here is `Send`, and
//! nothing here needs to be: a React root, its elements and its event queue all
//! belong to the UI thread.
//!
//! ## Styling
//!
//! Lowering resolves every node through the shared [`spherekit_css`] cascade,
//! threading the ancestor chain so combinators work, resolving the interaction
//! variants so `:hover` and `:active` reach the paint style, and inheriting
//! typography down the tree the way CSS does. A native `div()` built by hand
//! and a React `<View>` resolve through the same stylesheet and land on the
//! same [`spherekit_ui::PaintStyle`].
//!
//! ## Extending the host
//!
//! ```
//! use spherekit_react::{ReactHost, node_builder};
//! use spherekit_ui::{IntoElement, ParentElement, div, label};
//!
//! let mut host = ReactHost::new();
//! host.register_host_type(
//!     "gauge",
//!     node_builder(|cx, node| {
//!         cx.apply(div().id(node.id)).child(label("gauge")).into_element()
//!     }),
//! );
//! host.commit_json(r#"{"revision":1,"children":[{"id":1,"type":"gauge"}]}"#).unwrap();
//! assert!(host.contains(1));
//! ```
//!
//! An unregistered type is not an error: it lowers to a plain container with
//! its children inside it, so a React bundle built against a newer host
//! degrades instead of taking the window down.

#![deny(missing_docs)]

mod events;
mod host;
mod lower;
mod tree;

pub use events::{EventQueue, HostEvent};
pub use host::{ReactHost, node_builder};
pub use lower::{LowerContext, NodeBuilder};
pub use tree::{MAX_NODE_COUNT, MAX_TREE_DEPTH, NativeNode, NativeTree, ReactHostError};

pub use spherekit_ui::AnyElement;
