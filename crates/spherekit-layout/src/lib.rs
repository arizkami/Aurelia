//! # spherekit-layout
//!
//! The retained layout tree of `SphereKit`: styles, structure,
//! invalidation, geometry, scrolling and hit testing.
//!
//! ```
//! use spherekit_layout::{LayoutEngine, LayoutTree, Style, TaffyLayoutEngine};
//! use spherekit_core::{Length, px, relative, size};
//!
//! let mut tree = LayoutTree::new();
//! let root = tree.insert(Style::row().with_size(size(relative(1.0), relative(1.0))));
//! let meter = tree.insert_child(root, Style::DEFAULT.with_width(Length::Px(px(24.0)))).unwrap();
//! let fader = tree.insert_child(root, Style::DEFAULT.with_flex_grow(1.0)).unwrap();
//!
//! let mut engine = TaffyLayoutEngine::new();
//! engine.compute(&mut tree, size(px(200.0), px(100.0))).unwrap();
//!
//! assert_eq!(tree.layout(meter).unwrap().bounds.size.width, px(24.0));
//! assert_eq!(tree.layout(fader).unwrap().bounds.size.width, px(176.0));
//! ```
//!
//! ## The design in one paragraph
//!
//! A [`LayoutTree`] holds nodes keyed by the generational
//! [`NodeId`](spherekit_core::NodeId) from `spherekit-core`, so a widget can hold a
//! handle across a rebuild without risking that it silently starts referring to
//! something else. A [`LayoutEngine`] turns styles into geometry; the shipped one
//! is [`TaffyLayoutEngine`], and no `taffy` type appears anywhere in this crate's
//! public API, so the backend stays replaceable. Every node's geometry is stored
//! both parent-relative and absolute, because painting and hit testing both want
//! the absolute form and recomputing it per query would mean walking to the root
//! on every mouse move.
//!
//! ## Invalidation is the point
//!
//! A plug-in editor repaints its meters at the display refresh rate and changes
//! its layout roughly never. [`DirtyFlags`] therefore separates "the geometry may
//! have moved" from "only the pixels changed", and
//! [`LayoutEngine::compute`] returns without touching a node when nothing is
//! layout-dirty. [`LayoutStats::nodes_laid_out`] and
//! [`TaffyLayoutEngine::last_laid_out`] exist so that this is measured rather
//! than believed:
//!
//! ```
//! # use spherekit_layout::{DirtyFlags, LayoutEngine, LayoutTree, Style, TaffyLayoutEngine};
//! # use spherekit_core::{px, relative, size};
//! # let mut tree = LayoutTree::new();
//! # let root = tree.insert(Style::row().with_size(size(relative(1.0), relative(1.0))));
//! # let meter = tree.insert_child(root, Style::DEFAULT.with_flex_grow(1.0)).unwrap();
//! # let mut engine = TaffyLayoutEngine::new();
//! # engine.compute(&mut tree, size(px(200.0), px(100.0))).unwrap();
//! tree.mark_dirty(meter, DirtyFlags::PAINT);
//! engine.compute(&mut tree, size(px(200.0), px(100.0))).unwrap();
//! assert_eq!(engine.stats().nodes_laid_out, 0);
//! ```
//!
//! ## Where this crate deliberately differs from CSS
//!
//! * There is no `position: static`. Every node is a containing block for its
//!   absolutely positioned children, so "the nearest positioned ancestor" is
//!   always the direct parent and inserting a wrapper cannot teleport a popup.
//! * [`z_index`](Style::z_index) orders a node among its siblings and nothing
//!   else. There are no stacking contexts to reason about.
//! * Sizes are always border-box.
//! * Layout output is not rounded. Logical pixels are not device pixels; the
//!   renderer rounds once, in device space.

#![deny(missing_docs)]
#![warn(clippy::doc_markdown)]

pub mod dirty;
pub mod engine;
pub mod style;
pub mod taffy_backend;
pub mod tree;

mod hit;

#[cfg(test)]
mod layout_tests;

pub use dirty::DirtyFlags;
pub use engine::{AvailableSpace, LayoutEngine, LayoutStats, Measure, MeasureRequest, NoMeasure};
pub use style::{
    Align, Display, Distribute, FlexDirection, FlexWrap, Overflow, Position, Style, edges_all,
    edges_px, edges_symmetric, size_px,
};
pub use taffy_backend::TaffyLayoutEngine;
pub use tree::{Ancestors, ComputedLayout, LayoutTree};

/// Everything a typical consumer of this crate needs, in one import.
pub mod prelude {
    pub use crate::dirty::DirtyFlags;
    pub use crate::engine::{AvailableSpace, LayoutEngine, LayoutStats, Measure, MeasureRequest};
    pub use crate::style::{
        Align, Display, Distribute, FlexDirection, FlexWrap, Overflow, Position, Style, edges_all,
        edges_px, edges_symmetric, size_px,
    };
    pub use crate::taffy_backend::TaffyLayoutEngine;
    pub use crate::tree::{ComputedLayout, LayoutTree};
}
