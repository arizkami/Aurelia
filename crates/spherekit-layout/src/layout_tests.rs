//! End-to-end tests of the layout engine over the retained tree.
//!
//! These exercise the crate the way a UI framework does: build a tree, run a
//! pass, read rectangles back, invalidate something, run again. The unit tests
//! next to each module cover the pieces; these cover the contract.

use spherekit_core::{LayoutError, Length, NodeId, Point, Px, Rect, Size, px, relative, size};

use crate::dirty::DirtyFlags;
use crate::engine::{AvailableSpace, LayoutEngine, Measure, MeasureRequest, NoMeasure};
use crate::style::{Align, Distribute, FlexDirection, FlexWrap, Overflow, Style};
use crate::taffy_backend::TaffyLayoutEngine;
use crate::tree::LayoutTree;

/// Comparison tolerance. Flexbox distributes free space in `f32`, so thirds of a
/// pixel are expected and exact equality would only test the FPU.
const EPS: f32 = 0.01;

#[track_caller]
fn close(actual: Px, expected: f32) {
    assert!(
        (actual.get() - expected).abs() < EPS,
        "expected {expected}, got {actual} (delta {})",
        (actual.get() - expected).abs()
    );
}

#[track_caller]
fn assert_rect(actual: Rect<Px>, x: f32, y: f32, w: f32, h: f32) {
    let expected = [x, y, w, h];
    let got = [
        actual.origin.x.get(),
        actual.origin.y.get(),
        actual.size.width.get(),
        actual.size.height.get(),
    ];
    for i in 0..4 {
        assert!(
            (got[i] - expected[i]).abs() < EPS,
            "component {i}: expected {expected:?}, got {got:?}"
        );
    }
}

fn full() -> Style {
    Style::row().with_size(size(relative(1.0), relative(1.0)))
}

fn bounds(tree: &LayoutTree, id: NodeId) -> Rect<Px> {
    tree.layout(id).expect("node has no layout").bounds
}

fn absolute(tree: &LayoutTree, id: NodeId) -> Rect<Px> {
    tree.layout(id).expect("node has no layout").absolute_bounds
}

/// Builds a root sized to `viewport` plus an engine, and runs one pass.
fn laid_out(
    build: impl FnOnce(&mut LayoutTree, NodeId),
) -> (LayoutTree, TaffyLayoutEngine, NodeId) {
    laid_out_in(size(px(300.0), px(200.0)), full(), build)
}

fn laid_out_in(
    viewport: Size<Px>,
    root_style: Style,
    build: impl FnOnce(&mut LayoutTree, NodeId),
) -> (LayoutTree, TaffyLayoutEngine, NodeId) {
    let mut tree = LayoutTree::new();
    let root = tree.insert(root_style);
    build(&mut tree, root);
    let mut engine = TaffyLayoutEngine::new();
    engine.compute(&mut tree, viewport).expect("layout failed");
    (tree, engine, root)
}

// ------------------------------------------------------------------ flexbox

#[test]
fn a_row_of_three_splits_the_parent_width() {
    let mut kids = Vec::new();
    let (tree, _, _) = laid_out(|tree, root| {
        for _ in 0..3 {
            kids.push(tree.insert_child(root, Style::DEFAULT.with_flex_grow(1.0)).unwrap());
        }
    });
    for (i, id) in kids.iter().enumerate() {
        assert_rect(bounds(&tree, *id), 100.0 * i as f32, 0.0, 100.0, 200.0);
    }
}

#[test]
fn gap_is_taken_out_of_the_shared_space_once_per_gutter() {
    let mut kids = Vec::new();
    let (tree, _, _) =
        laid_out_in(size(px(320.0), px(100.0)), full().with_gap(px(10.0)), |tree, root| {
            for _ in 0..3 {
                kids.push(tree.insert_child(root, Style::DEFAULT.with_flex_grow(1.0)).unwrap());
            }
        });
    // Two gutters between three children: (320 - 20) / 3.
    close(bounds(&tree, kids[0]).size.width, 100.0);
    close(bounds(&tree, kids[1]).origin.x, 110.0);
    close(bounds(&tree, kids[2]).origin.x, 220.0);
    close(bounds(&tree, kids[2]).max_x(), 320.0);
}

#[test]
fn flex_grow_distributes_leftover_space_proportionally() {
    let mut kids = Vec::new();
    let (tree, _, _) = laid_out(|tree, root| {
        for grow in [1.0, 2.0, 3.0] {
            kids.push(tree.insert_child(root, Style::DEFAULT.with_flex_grow(grow)).unwrap());
        }
    });
    close(bounds(&tree, kids[0]).size.width, 50.0);
    close(bounds(&tree, kids[1]).size.width, 100.0);
    close(bounds(&tree, kids[2]).size.width, 150.0);
}

#[test]
fn a_zero_grow_child_keeps_its_size_while_others_expand() {
    let mut kids = Vec::new();
    let (tree, _, _) = laid_out(|tree, root| {
        kids.push(
            tree.insert_child(root, Style::DEFAULT.with_width(Length::Px(px(60.0)))).unwrap(),
        );
        kids.push(tree.insert_child(root, Style::DEFAULT.with_flex_grow(1.0)).unwrap());
    });
    close(bounds(&tree, kids[0]).size.width, 60.0);
    close(bounds(&tree, kids[1]).size.width, 240.0);
}

#[test]
fn flex_shrink_absorbs_overflow_proportionally() {
    let mut kids = Vec::new();
    // Two 100 px children in a 100 px row: 100 px of overflow to absorb.
    let (tree, _, _) = laid_out_in(size(px(100.0), px(50.0)), full(), |tree, root| {
        for shrink in [1.0, 1.0] {
            kids.push(
                tree.insert_child(
                    root,
                    Style::DEFAULT.with_width(Length::Px(px(100.0))).with_flex_shrink(shrink),
                )
                .unwrap(),
            );
        }
    });
    close(bounds(&tree, kids[0]).size.width, 50.0);
    close(bounds(&tree, kids[1]).size.width, 50.0);
}

#[test]
fn zero_shrink_children_are_allowed_to_overflow() {
    let mut kids = Vec::new();
    let (tree, _, _) = laid_out_in(size(px(100.0), px(50.0)), full(), |tree, root| {
        for _ in 0..2 {
            kids.push(
                tree.insert_child(
                    root,
                    Style::DEFAULT.with_width(Length::Px(px(100.0))).with_flex_shrink(0.0),
                )
                .unwrap(),
            );
        }
    });
    close(bounds(&tree, kids[0]).size.width, 100.0);
    close(bounds(&tree, kids[1]).origin.x, 100.0);
}

#[test]
fn percentages_resolve_against_the_parent_and_auto_shrink_wraps() {
    let mut sized = None;
    let mut auto = None;
    let (tree, _, _) = laid_out(|tree, root| {
        sized = Some(
            tree.insert_child(root, Style::DEFAULT.with_size(size(relative(0.25), relative(0.5))))
                .unwrap(),
        );
        // Auto width with a fixed-width child shrink-wraps to the child.
        let holder = tree.insert_child(root, Style::column()).unwrap();
        tree.insert_child(holder, Style::DEFAULT.with_px_size(37.0, 11.0)).unwrap();
        auto = Some(holder);
    });
    assert_rect(bounds(&tree, sized.unwrap()), 0.0, 0.0, 75.0, 100.0);
    close(bounds(&tree, auto.unwrap()).size.width, 37.0);
}

#[test]
fn nested_column_inside_a_row_lays_out_on_both_axes() {
    let mut column = None;
    let mut rows = Vec::new();
    let (tree, _, _) = laid_out(|tree, root| {
        tree.insert_child(root, Style::DEFAULT.with_width(Length::Px(px(100.0)))).unwrap();
        let col = tree.insert_child(root, Style::column().with_flex_grow(1.0)).unwrap();
        for _ in 0..2 {
            rows.push(tree.insert_child(col, Style::DEFAULT.with_flex_grow(1.0)).unwrap());
        }
        column = Some(col);
    });
    // The column occupies the leftover 200 px of the 300 px row, full height.
    assert_rect(bounds(&tree, column.unwrap()), 100.0, 0.0, 200.0, 200.0);
    // Its own children split its height, and are positioned relative to it.
    assert_rect(bounds(&tree, rows[0]), 0.0, 0.0, 200.0, 100.0);
    assert_rect(bounds(&tree, rows[1]), 0.0, 100.0, 200.0, 100.0);
    // ...but their absolute rectangles carry the column's offset.
    assert_rect(absolute(&tree, rows[1]), 100.0, 100.0, 200.0, 100.0);
}

#[test]
fn align_and_justify_place_a_single_child() {
    let mut child = None;
    let (tree, _, _) = laid_out(|tree, root| {
        child = Some(tree.insert_child(root, Style::DEFAULT.with_px_size(50.0, 40.0)).unwrap());
        tree.set_style(
            root,
            full().with_justify_content(Distribute::Center).with_align_items(Align::Center),
        )
        .unwrap();
    });
    assert_rect(bounds(&tree, child.unwrap()), 125.0, 80.0, 50.0, 40.0);
}

#[test]
fn reversed_direction_places_the_first_child_last() {
    let mut kids = Vec::new();
    let (tree, _, _) = laid_out_in(
        size(px(300.0), px(100.0)),
        full().with_flex_direction(FlexDirection::RowReverse),
        |tree, root| {
            for _ in 0..2 {
                kids.push(
                    tree.insert_child(root, Style::DEFAULT.with_width(Length::Px(px(100.0))))
                        .unwrap(),
                );
            }
        },
    );
    close(bounds(&tree, kids[0]).origin.x, 200.0);
    close(bounds(&tree, kids[1]).origin.x, 100.0);
}

#[test]
fn flex_wrap_moves_overflowing_items_onto_a_new_line() {
    let mut kids = Vec::new();
    let (tree, _, _) = laid_out_in(
        size(px(100.0), px(100.0)),
        full()
            .with_flex_wrap(FlexWrap::Wrap)
            .with_align_content(Distribute::Start)
            .with_align_items(Align::Start),
        |tree, root| {
            for _ in 0..3 {
                kids.push(
                    tree.insert_child(
                        root,
                        Style::DEFAULT.with_px_size(40.0, 20.0).with_flex_shrink(0.0),
                    )
                    .unwrap(),
                );
            }
        },
    );
    assert_rect(bounds(&tree, kids[0]), 0.0, 0.0, 40.0, 20.0);
    assert_rect(bounds(&tree, kids[1]), 40.0, 0.0, 40.0, 20.0);
    // The third does not fit on the first line, so it starts the second.
    assert_rect(bounds(&tree, kids[2]), 0.0, 20.0, 40.0, 20.0);
}

#[test]
fn space_between_pushes_the_outer_items_to_the_edges() {
    let mut kids = Vec::new();
    let (tree, _, _) = laid_out_in(
        size(px(300.0), px(100.0)),
        full().with_justify_content(Distribute::SpaceBetween),
        |tree, root| {
            for _ in 0..2 {
                kids.push(
                    tree.insert_child(
                        root,
                        Style::DEFAULT.with_width(Length::Px(px(50.0))).with_flex_shrink(0.0),
                    )
                    .unwrap(),
                );
            }
        },
    );
    close(bounds(&tree, kids[0]).origin.x, 0.0);
    close(bounds(&tree, kids[1]).origin.x, 250.0);
}

#[test]
fn align_self_overrides_the_containers_align_items() {
    let mut default_child = None;
    let mut overridden = None;
    let (tree, _, _) = laid_out_in(
        size(px(300.0), px(200.0)),
        full().with_align_items(Align::Start),
        |tree, root| {
            default_child =
                Some(tree.insert_child(root, Style::DEFAULT.with_px_size(20.0, 40.0)).unwrap());
            overridden = Some(
                tree.insert_child(
                    root,
                    Style::DEFAULT.with_px_size(20.0, 40.0).with_align_self(Align::End),
                )
                .unwrap(),
            );
        },
    );
    close(bounds(&tree, default_child.unwrap()).origin.y, 0.0);
    close(bounds(&tree, overridden.unwrap()).origin.y, 160.0);
}

// ------------------------------------------------------------- box model

#[test]
fn padding_border_and_margin_each_shrink_the_content_exactly_once() {
    let mut child = None;
    let (tree, _, _) = laid_out_in(
        size(px(300.0), px(200.0)),
        full().with_padding(crate::edges_px(10.0)).with_border(crate::edges_px(5.0)),
        |tree, root| {
            child = Some(
                tree.insert_child(
                    root,
                    Style::DEFAULT.with_flex_grow(1.0).with_margin(crate::edges_px(20.0)),
                )
                .unwrap(),
            );
        },
    );
    let child = child.unwrap();
    // 5 border + 10 padding + 20 margin = 35 on every side.
    assert_rect(bounds(&tree, child), 35.0, 35.0, 300.0 - 70.0, 200.0 - 70.0);

    let root_layout = tree.layout(tree.roots()[0]).unwrap();
    close(root_layout.border.left, 5.0);
    close(root_layout.padding.left, 10.0);
    close(root_layout.content_box().size.width, 270.0);
    close(tree.layout(child).unwrap().margin.left, 20.0);
}

#[test]
fn a_border_box_size_includes_its_own_padding_and_border() {
    // 100 px wide means 100 px wide, whatever the border is. Content-box sizing
    // would make this 130.
    let mut child = None;
    let (tree, _, _) = laid_out(|tree, root| {
        child = Some(
            tree.insert_child(
                root,
                Style::DEFAULT
                    .with_width(Length::Px(px(100.0)))
                    .with_padding(crate::edges_px(10.0))
                    .with_border(crate::edges_px(5.0)),
            )
            .unwrap(),
        );
    });
    close(bounds(&tree, child.unwrap()).size.width, 100.0);
    close(tree.layout(child.unwrap()).unwrap().content_box().size.width, 70.0);
}

#[test]
fn min_and_max_sizes_clamp_the_resolved_size() {
    let mut clamped_high = None;
    let mut clamped_low = None;
    let (tree, _, _) = laid_out(|tree, root| {
        clamped_high = Some(
            tree.insert_child(
                root,
                Style::DEFAULT
                    .with_flex_grow(1.0)
                    .with_max_size(size(Length::Px(px(50.0)), Length::Auto)),
            )
            .unwrap(),
        );
        clamped_low = Some(
            tree.insert_child(
                root,
                Style::DEFAULT
                    .with_width(Length::Px(px(10.0)))
                    .with_flex_shrink(0.0)
                    .with_min_size(size(Length::Px(px(40.0)), Length::Auto)),
            )
            .unwrap(),
        );
    });
    close(bounds(&tree, clamped_high.unwrap()).size.width, 50.0);
    close(bounds(&tree, clamped_low.unwrap()).size.width, 40.0);
}

#[test]
fn aspect_ratio_derives_the_unknown_axis() {
    let mut child = None;
    let (tree, _, _) = laid_out(|tree, root| {
        child = Some(
            tree.insert_child(
                root,
                Style::DEFAULT.with_width(Length::Px(px(80.0))).with_aspect_ratio(2.0),
            )
            .unwrap(),
        );
        tree.set_style(root, full().with_align_items(Align::Start)).unwrap();
    });
    assert_rect(bounds(&tree, child.unwrap()), 0.0, 0.0, 80.0, 40.0);
}

#[test]
fn absolute_children_are_placed_against_the_parents_padding_box() {
    let mut outer = None;
    let mut inner = None;
    let (tree, _, _) = laid_out_in(
        size(px(300.0), px(200.0)),
        full().with_border(crate::edges_px(5.0)).with_padding(crate::edges_px(10.0)),
        |tree, root| {
            let o = tree
                .insert_child(
                    root,
                    Style::row().with_flex_grow(1.0).with_border(crate::edges_px(4.0)),
                )
                .unwrap();
            // Absolute against `o`, not against the root: every node is a
            // containing block in SphereKit.
            let i = tree
                .insert_child(o, Style::DEFAULT.absolute_at(7.0, 9.0).with_px_size(20.0, 20.0))
                .unwrap();
            outer = Some(o);
            inner = Some(i);
        },
    );
    let outer = outer.unwrap();
    let inner = inner.unwrap();
    // Inset is measured from inside the border of the direct parent.
    assert_rect(bounds(&tree, inner), 4.0 + 7.0, 4.0 + 9.0, 20.0, 20.0);
    // ...which for the absolute rect stacks on top of the parent's own offset.
    let outer_abs = absolute(&tree, outer);
    assert_rect(
        absolute(&tree, inner),
        outer_abs.origin.x.get() + 11.0,
        outer_abs.origin.y.get() + 13.0,
        20.0,
        20.0,
    );
}

#[test]
fn absolute_children_reserve_no_space_in_the_flow() {
    let mut flowed = None;
    let (tree, _, _) = laid_out(|tree, root| {
        tree.insert_child(root, Style::DEFAULT.absolute_at(0.0, 0.0).with_px_size(100.0, 100.0))
            .unwrap();
        flowed = Some(tree.insert_child(root, Style::DEFAULT.with_flex_grow(1.0)).unwrap());
    });
    // The in-flow sibling still gets the whole row.
    assert_rect(bounds(&tree, flowed.unwrap()), 0.0, 0.0, 300.0, 200.0);
}

#[test]
fn display_none_removes_a_subtree_from_layout_entirely() {
    let mut hidden = None;
    let mut hidden_child = None;
    let mut visible = None;
    let (tree, _, _) = laid_out(|tree, root| {
        let h = tree.insert_child(root, Style::hidden().with_px_size(100.0, 100.0)).unwrap();
        hidden_child = Some(tree.insert_child(h, Style::DEFAULT.with_px_size(50.0, 50.0)).unwrap());
        hidden = Some(h);
        visible = Some(tree.insert_child(root, Style::DEFAULT.with_flex_grow(1.0)).unwrap());
    });
    assert_rect(bounds(&tree, hidden.unwrap()), 0.0, 0.0, 0.0, 0.0);
    assert_rect(bounds(&tree, hidden_child.unwrap()), 0.0, 0.0, 0.0, 0.0);
    // The hidden node contributes nothing, so its sibling gets everything.
    assert_rect(bounds(&tree, visible.unwrap()), 0.0, 0.0, 300.0, 200.0);
}

#[test]
fn block_display_stacks_children_vertically() {
    let mut kids = Vec::new();
    let (tree, _, _) = laid_out_in(
        size(px(300.0), px(200.0)),
        Style::block().with_size(size(relative(1.0), relative(1.0))),
        |tree, root| {
            for _ in 0..2 {
                kids.push(
                    tree.insert_child(root, Style::block().with_height(Length::Px(px(30.0))))
                        .unwrap(),
                );
            }
        },
    );
    assert_rect(bounds(&tree, kids[0]), 0.0, 0.0, 300.0, 30.0);
    assert_rect(bounds(&tree, kids[1]), 0.0, 30.0, 300.0, 30.0);
}

// --------------------------------------------------------------- measuring

/// A measure function that reports a fixed size for one node.
struct FixedMeasure {
    target: NodeId,
    size: Size<Px>,
    calls: usize,
    last_available: Option<Size<AvailableSpace>>,
}

impl Measure for FixedMeasure {
    fn measure(&mut self, request: MeasureRequest<'_>) -> Size<Px> {
        self.calls += 1;
        self.last_available = Some(request.available);
        if request.node == self.target { self.size } else { Size::ZERO }
    }
}

#[test]
fn a_measure_function_supplies_the_intrinsic_size_of_a_leaf() {
    let mut tree = LayoutTree::new();
    let root = tree.insert(full().with_align_items(Align::Start));
    let text = tree.insert_child(root, Style::DEFAULT).unwrap();
    let mut measure = FixedMeasure {
        target: text,
        size: size(px(42.0), px(17.0)),
        calls: 0,
        last_available: None,
    };

    let mut engine = TaffyLayoutEngine::new();
    engine.compute_with_measure(&mut tree, size(px(300.0), px(200.0)), &mut measure).unwrap();

    assert_rect(bounds(&tree, text), 0.0, 0.0, 42.0, 17.0);
    assert!(measure.calls > 0, "the measure hook was never invoked");
    assert!(measure.last_available.is_some());
}

#[test]
fn a_measured_leaf_is_still_subject_to_its_own_style() {
    let mut tree = LayoutTree::new();
    let root = tree.insert(full().with_align_items(Align::Start));
    let text = tree
        .insert_child(root, Style::DEFAULT.with_max_size(size(Length::Px(px(20.0)), Length::Auto)))
        .unwrap();
    let mut measure = FixedMeasure {
        target: text,
        size: size(px(42.0), px(17.0)),
        calls: 0,
        last_available: None,
    };
    let mut engine = TaffyLayoutEngine::new();
    engine.compute_with_measure(&mut tree, size(px(300.0), px(200.0)), &mut measure).unwrap();
    close(bounds(&tree, text).size.width, 20.0);
}

#[test]
fn a_measure_function_that_returns_garbage_is_reported_not_propagated() {
    let mut tree = LayoutTree::new();
    let root = tree.insert(full());
    let text = tree.insert_child(root, Style::DEFAULT).unwrap();
    let mut measure = |_req: MeasureRequest<'_>| size(px(f32::NAN), px(10.0));

    let mut engine = TaffyLayoutEngine::new();
    let err = engine
        .compute_with_measure(&mut tree, size(px(300.0), px(200.0)), &mut measure)
        .unwrap_err();
    match err {
        LayoutError::InvalidMeasure(id) => assert_eq!(id, text),
        other => panic!("expected InvalidMeasure, got {other:?}"),
    }
    // Nothing was written, so no NaN geometry escaped into the tree.
    assert_rect(bounds(&tree, text), 0.0, 0.0, 0.0, 0.0);
}

#[test]
fn a_negative_measurement_is_rejected_too() {
    let mut tree = LayoutTree::new();
    let root = tree.insert(full());
    tree.insert_child(root, Style::DEFAULT).unwrap();
    let mut measure = |_req: MeasureRequest<'_>| size(px(-1.0), px(10.0));
    let mut engine = TaffyLayoutEngine::new();
    assert!(matches!(
        engine.compute_with_measure(&mut tree, size(px(10.0), px(10.0)), &mut measure),
        Err(LayoutError::InvalidMeasure(_))
    ));
}

#[test]
fn text_dirty_forces_a_re_measure_and_nothing_else_does() {
    let mut tree = LayoutTree::new();
    let root = tree.insert(full().with_align_items(Align::Start));
    let text = tree.insert_child(root, Style::DEFAULT).unwrap();
    let mut measure = FixedMeasure {
        target: text,
        size: size(px(10.0), px(10.0)),
        calls: 0,
        last_available: None,
    };
    let mut engine = TaffyLayoutEngine::new();
    let viewport = size(px(300.0), px(200.0));

    engine.compute_with_measure(&mut tree, viewport, &mut measure).unwrap();
    close(bounds(&tree, text).size.width, 10.0);

    // The content changed but nobody said so: the cached measurement stands.
    measure.size = size(px(30.0), px(10.0));
    measure.calls = 0;
    engine.compute_with_measure(&mut tree, viewport, &mut measure).unwrap();
    assert_eq!(measure.calls, 0, "an unannounced text change must not cost a measure");
    close(bounds(&tree, text).size.width, 10.0);

    // Announcing it re-runs the measure and the geometry follows.
    tree.mark_dirty(text, DirtyFlags::TEXT);
    engine.compute_with_measure(&mut tree, viewport, &mut measure).unwrap();
    assert!(measure.calls > 0, "TEXT must invalidate the intrinsic size");
    close(bounds(&tree, text).size.width, 30.0);
}

#[test]
fn nodes_with_children_are_never_measured() {
    let mut tree = LayoutTree::new();
    let root = tree.insert(full());
    let container = tree.insert_child(root, Style::row().with_flex_grow(1.0)).unwrap();
    let leaf = tree.insert_child(container, Style::DEFAULT).unwrap();

    let mut seen: Vec<NodeId> = Vec::new();
    let mut measure = |req: MeasureRequest<'_>| {
        seen.push(req.node);
        Size::ZERO
    };
    let mut engine = TaffyLayoutEngine::new();
    engine.compute_with_measure(&mut tree, size(px(100.0), px(100.0)), &mut measure).unwrap();

    assert!(seen.contains(&leaf));
    assert!(!seen.contains(&container), "a container must not be asked for an intrinsic size");
    assert!(!seen.contains(&root));
}

// ------------------------------------------------------ the headline tests

/// root -> [a1 -> a2 -> a3, b1 -> b2 -> b3]
struct TwoBranches {
    tree: LayoutTree,
    engine: TaffyLayoutEngine,
    root: NodeId,
    a: [NodeId; 3],
    b: [NodeId; 3],
}

fn two_branches() -> TwoBranches {
    let mut tree = LayoutTree::new();
    let root = tree.insert(full());
    let branch = |tree: &mut LayoutTree| {
        let top = tree.insert_child(root, Style::column().with_flex_grow(1.0)).unwrap();
        let mid = tree.insert_child(top, Style::column().with_flex_grow(1.0)).unwrap();
        let leaf = tree.insert_child(mid, Style::DEFAULT.with_flex_grow(1.0)).unwrap();
        [top, mid, leaf]
    };
    let a = branch(&mut tree);
    let b = branch(&mut tree);
    let mut engine = TaffyLayoutEngine::new();
    engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();
    TwoBranches { tree, engine, root, a, b }
}

#[test]
fn a_paint_dirty_node_costs_zero_layout_work() {
    let TwoBranches { mut tree, mut engine, a, .. } = two_branches();
    let before_passes = engine.stats().passes;

    // A VU meter repainting 60 times a second.
    for _ in 0..60 {
        tree.mark_dirty(a[2], DirtyFlags::PAINT);
        engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();
        assert_eq!(engine.stats().nodes_laid_out, 0);
        assert!(engine.last_laid_out().is_empty());
        tree.clear_paint_dirty();
    }
    assert_eq!(engine.stats().passes, before_passes, "no pass should have run at all");
    assert_eq!(engine.stats().skipped_passes, 60);
}

#[test]
fn a_transform_dirty_node_costs_zero_layout_work() {
    let TwoBranches { mut tree, mut engine, a, .. } = two_branches();
    tree.mark_dirty(a[2], DirtyFlags::TRANSFORM);
    engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();
    assert_eq!(engine.stats().nodes_laid_out, 0);
}

#[test]
fn a_paint_only_restyle_costs_zero_layout_work() {
    let TwoBranches { mut tree, mut engine, a, .. } = two_branches();
    let style = tree.style(a[2]).unwrap().clone();
    tree.set_style(a[2], style.with_opacity(0.3).with_z_index(4)).unwrap();
    engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();
    assert_eq!(engine.stats().nodes_laid_out, 0);
}

#[test]
fn a_layout_dirty_node_touches_only_its_own_branch() {
    let TwoBranches { mut tree, mut engine, root, a, b } = two_branches();
    let before: Vec<Rect<Px>> = b.iter().map(|id| bounds(&tree, *id)).collect();

    tree.mark_dirty(a[2], DirtyFlags::LAYOUT);
    engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();

    let touched = engine.last_laid_out();
    assert!(!touched.is_empty(), "the dirty node must have been laid out");
    let allowed = [root, a[0], a[1], a[2]];
    for id in touched {
        assert!(
            allowed.contains(id),
            "relaid out {id:?}, which is outside the dirty node's ancestor chain and subtree"
        );
    }
    for id in &b {
        assert!(!touched.contains(id), "unrelated sibling branch {id:?} was relaid out");
    }
    // ...and the untouched branch kept its geometry.
    for (id, was) in b.iter().zip(before) {
        assert_eq!(bounds(&tree, *id), was);
    }
}

#[test]
fn a_full_invalidation_costs_much_more_than_one_branch() {
    let TwoBranches { mut tree, mut engine, a, .. } = two_branches();

    tree.mark_dirty(a[2], DirtyFlags::LAYOUT);
    engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();
    let incremental = engine.stats().nodes_laid_out;

    engine.invalidate();
    engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();
    let full = engine.stats().nodes_laid_out;

    assert!(incremental > 0);
    assert!(
        full > incremental,
        "a full invalidation ({full}) should cost more than one branch ({incremental})"
    );
}

#[test]
fn a_viewport_change_forces_a_pass_even_when_nothing_is_dirty() {
    let TwoBranches { mut tree, mut engine, .. } = two_branches();
    assert!(!tree.needs_layout());

    engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();
    assert_eq!(engine.stats().nodes_laid_out, 0);

    engine.compute(&mut tree, size(px(400.0), px(200.0))).unwrap();
    assert!(engine.stats().nodes_laid_out > 0);
    close(bounds(&tree, tree.roots()[0]).size.width, 400.0);
}

#[test]
fn a_structural_change_forces_a_pass_and_keeps_unrelated_caches() {
    let TwoBranches { mut tree, mut engine, root, b, .. } = two_branches();
    let before = bounds(&tree, b[2]);

    let extra = tree.insert(Style::DEFAULT.with_flex_grow(1.0));
    tree.add_child(root, extra).unwrap();
    engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();

    // Three branches now share the row, so the old one really did move.
    assert_ne!(bounds(&tree, b[2]), before);
    close(bounds(&tree, extra).size.width, 100.0);
}

#[test]
fn removing_a_subtree_does_not_leave_stale_mirror_state() {
    let TwoBranches { mut tree, mut engine, a, b, .. } = two_branches();
    tree.remove(a[0]).unwrap();
    engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();
    // The survivor now owns the whole row.
    close(bounds(&tree, b[0]).size.width, 300.0);
    assert!(!engine.last_laid_out().contains(&a[0]));
}

#[test]
fn compute_clears_the_layout_flags_but_not_the_paint_flags() {
    let TwoBranches { mut tree, mut engine, a, .. } = two_branches();
    tree.mark_dirty(a[2], DirtyFlags::LAYOUT | DirtyFlags::PAINT);
    engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();

    assert!(!tree.needs_layout());
    assert_eq!(tree.dirty(a[2]), DirtyFlags::PAINT);
    assert!(tree.needs_paint());
}

// --------------------------------------------------------------- scrolling

#[test]
fn scrolling_shifts_absolute_rects_without_dirtying_layout() {
    let mut rows = Vec::new();
    let mut list = None;
    let (mut tree, mut engine, _) =
        laid_out_in(size(px(200.0), px(100.0)), full(), |tree, root| {
            let l = tree
                .insert_child(
                    root,
                    Style::column().with_flex_grow(1.0).with_overflow(Overflow::Scroll),
                )
                .unwrap();
            for _ in 0..4 {
                rows.push(
                    tree.insert_child(
                        l,
                        Style::DEFAULT.with_height(Length::Px(px(40.0))).with_flex_shrink(0.0),
                    )
                    .unwrap(),
                );
            }
            list = Some(l);
        });
    let list = list.unwrap();

    // 4 rows of 40 px inside a 100 px viewport.
    close(tree.layout(list).unwrap().content_size.height, 160.0);
    assert_eq!(tree.max_scroll_offset(list), size(Px::ZERO, px(60.0)));
    assert_rect(absolute(&tree, rows[1]), 0.0, 40.0, 200.0, 40.0);

    tree.set_scroll_offset(list, size(Px::ZERO, px(40.0))).unwrap();
    assert_rect(absolute(&tree, rows[1]), 0.0, 0.0, 200.0, 40.0);
    // Parent-relative geometry is untouched: only the window moved.
    assert_rect(bounds(&tree, rows[1]), 0.0, 40.0, 200.0, 40.0);

    assert!(!tree.needs_layout(), "scrolling must not schedule layout");
    assert_eq!(tree.dirty(list), DirtyFlags::PAINT);
    engine.compute(&mut tree, size(px(200.0), px(100.0))).unwrap();
    assert_eq!(engine.stats().nodes_laid_out, 0);
}

#[test]
fn scroll_offsets_are_clamped_to_the_content() {
    let mut list = None;
    let (mut tree, _, _) = laid_out_in(size(px(200.0), px(100.0)), full(), |tree, root| {
        let l = tree
            .insert_child(root, Style::column().with_flex_grow(1.0).with_overflow(Overflow::Scroll))
            .unwrap();
        for _ in 0..3 {
            tree.insert_child(
                l,
                Style::DEFAULT.with_height(Length::Px(px(40.0))).with_flex_shrink(0.0),
            )
            .unwrap();
        }
        list = Some(l);
    });
    let list = list.unwrap();
    tree.set_scroll_offset(list, size(px(-50.0), px(9999.0))).unwrap();
    assert_eq!(tree.scroll_offset(list), size(Px::ZERO, px(20.0)));

    tree.scroll_by(list, size(Px::ZERO, px(-100.0))).unwrap();
    assert_eq!(tree.scroll_offset(list), Size::ZERO);
}

#[test]
fn scrolling_a_stale_handle_is_an_error_not_a_panic() {
    let mut tree = LayoutTree::new();
    let node = tree.insert(Style::DEFAULT);
    tree.remove(node).unwrap();
    assert!(matches!(
        tree.set_scroll_offset(node, size(Px::ZERO, px(1.0))),
        Err(LayoutError::NodeNotFound(_))
    ));
}

// -------------------------------------------------------- degenerate input

#[test]
fn an_empty_tree_computes_without_complaint() {
    let mut tree = LayoutTree::new();
    let mut engine = TaffyLayoutEngine::new();
    engine.compute(&mut tree, size(px(100.0), px(100.0))).unwrap();
    assert_eq!(engine.stats().nodes_laid_out, 0);
    assert_eq!(tree.hit_test(Point::new(px(1.0), px(1.0))), None);
}

#[test]
fn a_zero_sized_viewport_does_not_panic_or_produce_negative_boxes() {
    let mut kids = Vec::new();
    let (tree, _, root) =
        laid_out_in(Size::ZERO, full().with_padding(crate::edges_px(10.0)), |tree, root| {
            for _ in 0..3 {
                kids.push(tree.insert_child(root, Style::DEFAULT.with_flex_grow(1.0)).unwrap());
            }
        });
    // Padding sets a floor on a border box: 10 px on each side cannot be
    // squeezed out of existence, so the root is 20x20 in a 0x0 viewport.
    assert_rect(bounds(&tree, root), 0.0, 0.0, 20.0, 20.0);
    for id in &kids {
        let r = bounds(&tree, *id);
        assert!(r.size.width.get() >= 0.0 && r.size.height.get() >= 0.0, "negative extent: {r:?}");
    }
    // A content box can never be inverted, even when padding exceeds the box.
    let content = tree.layout(root).unwrap().content_box();
    assert!(content.size.width.get() >= 0.0 && content.size.height.get() >= 0.0);
}

#[test]
fn a_non_finite_viewport_is_treated_as_zero_rather_than_poisoning_the_tree() {
    let mut tree = LayoutTree::new();
    let root = tree.insert(full());
    let child = tree.insert_child(root, Style::DEFAULT.with_flex_grow(1.0)).unwrap();
    let mut engine = TaffyLayoutEngine::new();
    engine.compute(&mut tree, size(px(f32::NAN), px(f32::INFINITY))).unwrap();
    let r = bounds(&tree, child);
    assert!(r.size.width.is_finite() && r.size.height.is_finite(), "non-finite geometry: {r:?}");
}

#[test]
fn a_detached_root_is_still_laid_out_against_the_viewport() {
    let mut tree = LayoutTree::new();
    let a = tree.insert(full());
    let b = tree.insert(full());
    let mut engine = TaffyLayoutEngine::new();
    engine.compute(&mut tree, size(px(120.0), px(60.0))).unwrap();
    assert_rect(bounds(&tree, a), 0.0, 0.0, 120.0, 60.0);
    assert_rect(bounds(&tree, b), 0.0, 0.0, 120.0, 60.0);
}

#[test]
fn a_wide_tree_of_five_hundred_nodes_computes_and_hit_tests() {
    let mut tree = LayoutTree::new();
    let root = tree.insert(Style::column().with_size(size(relative(1.0), relative(1.0))));
    let mut leaves = Vec::new();
    for _ in 0..25 {
        let row = tree.insert_child(root, Style::row().with_flex_grow(1.0)).unwrap();
        for _ in 0..20 {
            leaves.push(tree.insert_child(row, Style::DEFAULT.with_flex_grow(1.0)).unwrap());
        }
    }
    assert_eq!(tree.len(), 1 + 25 + 500);

    let mut engine = TaffyLayoutEngine::new();
    engine.compute(&mut tree, size(px(1000.0), px(500.0))).unwrap();

    close(bounds(&tree, leaves[0]).size.width, 50.0);
    close(bounds(&tree, leaves[0]).size.height, 20.0);
    // The last leaf is in the bottom-right corner.
    let last = absolute(&tree, *leaves.last().unwrap());
    close(last.max_x(), 1000.0);
    close(last.max_y(), 500.0);
    assert_eq!(tree.hit_test(Point::new(px(999.0), px(499.0))), Some(*leaves.last().unwrap()));
}

#[test]
fn a_deep_chain_computes_without_overflowing_the_stack() {
    // Deep enough that a naive per-node recursion in *our* code would be a
    // problem; the backend's own recursion is bounded by the same depth.
    let mut tree = LayoutTree::new();
    let root = tree.insert(Style::block().with_size(size(relative(1.0), Length::Auto)));
    let mut current = root;
    for _ in 0..300 {
        current =
            tree.insert_child(current, Style::block().with_height(Length::Px(px(1.0)))).unwrap();
    }
    let mut engine = TaffyLayoutEngine::new();
    engine.compute(&mut tree, size(px(100.0), px(100.0))).unwrap();
    close(bounds(&tree, current).size.height, 1.0);
    // The absolute pass is iterative, so the deepest node still has a rectangle.
    assert_rect(absolute(&tree, current), 0.0, 0.0, 100.0, 1.0);
    // Every box in the chain is 1 px tall at y = 0, so the point is inside all
    // 301 of them; the hit-test walk is iterative and returns the whole chain.
    let chain = tree.hit_test_all(Point::new(px(1.0), px(0.5)));
    assert_eq!(chain.len(), 301);
    assert_eq!(chain.first(), Some(&root));
    assert_eq!(chain.last(), Some(&current));
}

#[test]
fn one_engine_driving_two_trees_never_mixes_their_caches() {
    // Both trees mint the same `NodeId`s from zero and can trivially share a
    // structure epoch, so an engine keyed only on the epoch would hand tree B
    // tree A's geometry.
    let mut engine = TaffyLayoutEngine::new();

    let mut a = LayoutTree::new();
    let a_root = a.insert(full());
    let a_child = a.insert_child(a_root, Style::DEFAULT.with_flex_grow(1.0)).unwrap();

    let mut b = LayoutTree::new();
    let b_root = b.insert(full());
    let b_child = b.insert_child(b_root, Style::DEFAULT.with_width(Length::Px(px(10.0)))).unwrap();
    assert_eq!(a.structure_epoch(), b.structure_epoch(), "the epochs really do collide");
    assert_ne!(a.id(), b.id());
    assert_eq!(a_child.index(), b_child.index(), "the ids really do collide");

    let viewport = size(px(300.0), px(200.0));
    for _ in 0..3 {
        engine.compute(&mut a, viewport).unwrap();
        engine.compute(&mut b, viewport).unwrap();
    }
    close(bounds(&a, a_child).size.width, 300.0);
    close(bounds(&b, b_child).size.width, 10.0);
}

#[test]
fn repeated_identical_passes_are_free() {
    let TwoBranches { mut tree, mut engine, .. } = two_branches();
    for _ in 0..10 {
        engine.compute(&mut tree, size(px(300.0), px(200.0))).unwrap();
        assert_eq!(engine.stats().nodes_laid_out, 0);
    }
    assert_eq!(engine.stats().skipped_passes, 10);
}

#[test]
fn no_measure_is_the_default_and_gives_leaves_zero_intrinsic_size() {
    let mut tree = LayoutTree::new();
    let root = tree.insert(full().with_align_items(Align::Start));
    let leaf = tree.insert_child(root, Style::DEFAULT).unwrap();
    let mut engine = TaffyLayoutEngine::new();
    engine.compute_with_measure(&mut tree, size(px(50.0), px(50.0)), &mut NoMeasure).unwrap();
    assert_rect(bounds(&tree, leaf), 0.0, 0.0, 0.0, 0.0);
}
