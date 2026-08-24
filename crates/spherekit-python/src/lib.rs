//! # spherekit-python
//!
//! SphereKit for Python: a native extension module that opens a real SphereKit
//! window and paints a tree the Python side describes.
//!
//! ## The shape of the boundary
//!
//! ```text
//! Python                          Rust (this crate)
//! ------                          -----------------
//! render()  -> JSON string  ->    ReactHost::commit_json
//!                                 ReactHost::ui_element_with_events
//!                                 SphereKitSurface::render
//! on_event(name, id, payload) <-  EventQueue drained once a frame
//! ```
//!
//! **Python describes, it does not draw.** A frame is one JSON string: the same
//! committed-tree format the React renderer produces, lowered by the same
//! [`spherekit_react::ReactHost`], styled by the same
//! [`spherekit_css`] cascade. That is the whole reason this crate is small.
//! Re-implementing a widget table for Python would have produced a second set
//! of prop names, a second styling story and a second place for bugs to live;
//! reusing the host means a Python application and a React application go
//! through the *same* code and cannot drift.
//!
//! ## Why a whole tree, every frame
//!
//! Not a mutation stream. The host takes a complete snapshot, so it can never
//! observe a half-built frame, and Python never has to describe a diff — which
//! is the part a binding usually gets wrong. A frame whose JSON is byte-identical
//! to the last is not committed at all, so an idle window costs one `render()`
//! call, one string comparison and no layout.
//!
//! ## Threading
//!
//! The window runs on the thread that called [`run`], because that is what every
//! desktop platform requires of an event loop. The GIL is released while the
//! loop is blocked and re-acquired for each `render()` and each event, so a
//! Python thread doing background work keeps running.

#![deny(missing_docs)]

mod runner;

use pyo3::exceptions::{PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyList, PyModule};
use serde_json::Value;

pub use runner::{PythonApp, RunConfig};

/// The engine version this extension was built from.
#[pyfunction]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The host node types the tree may use, for a Python side that wants to check.
///
/// Read from one place rather than copied into the Python package: a list that
/// has to be kept in step by hand is a list that is wrong after the next widget
/// is added.
#[pyfunction]
fn node_types() -> Vec<&'static str> {
    runner::NODE_TYPES.to_vec()
}

/// Validates a tree without opening a window, and returns how many nodes it has.
///
/// The one piece of the pipeline a Python test can reach without a GPU, and the
/// piece most worth reaching: a tree that fails to commit fails silently at
/// runtime — the window simply keeps painting the previous frame — so a test
/// that never commits anything would pass while the application showed nothing.
#[pyfunction]
fn check_tree(json: &str) -> PyResult<usize> {
    let mut host = spherekit_react::ReactHost::new();
    host.commit_json(json).map_err(|error| PyValueError::new_err(error.to_string()))?;
    Ok(host.node_count())
}

/// Parses a stylesheet and returns how many rules it holds.
///
/// Same reasoning as [`check_tree`]: a malformed stylesheet leaves the previous
/// one installed, so the failure is a window that looks unstyled rather than an
/// error anyone sees.
#[pyfunction]
fn check_css(css: &str) -> PyResult<usize> {
    let sheet = spherekit_css::Stylesheet::parse(css)
        .map_err(|error| PyValueError::new_err(error.to_string()))?;
    Ok(sheet.rule_count())
}

/// Opens a window and runs the event loop until it closes.
///
/// `render` is called once per frame and must return the tree as a JSON string.
/// `on_event` is called with `(name, node_id, payload)` for every native event,
/// where `payload` is a `dict` or `None`.
///
/// Blocks until the window is closed, [`quit`] is called, or `frame_limit`
/// frames have been drawn — the last of which is what lets the demo run in CI
/// without anybody clicking anything.
#[pyfunction]
#[pyo3(signature = (render, on_event, *, title=None, width=None, height=None, css=None, dark=None, frame_limit=None))]
#[allow(clippy::too_many_arguments)]
fn run(
    python: Python<'_>,
    render: Py<PyAny>,
    on_event: Py<PyAny>,
    title: Option<String>,
    width: Option<f32>,
    height: Option<f32>,
    css: Option<String>,
    dark: Option<bool>,
    frame_limit: Option<u64>,
) -> PyResult<()> {
    if !render.bind(python).is_callable() {
        return Err(PyValueError::new_err("render must be callable"));
    }
    if !on_event.bind(python).is_callable() {
        return Err(PyValueError::new_err("on_event must be callable"));
    }
    let config = RunConfig {
        title: title.unwrap_or_else(|| "SphereKit".to_owned()),
        width: width.unwrap_or(900.0),
        height: height.unwrap_or(620.0),
        css: css.unwrap_or_default(),
        dark: dark.unwrap_or(true),
        frame_limit,
    };
    // Parsed here rather than inside the loop: a stylesheet that does not parse
    // is a mistake in the application, and reporting it as a Python exception
    // at start-up is worth more than a window that opens unstyled.
    if !config.css.is_empty() {
        spherekit_css::Stylesheet::parse(&config.css)
            .map_err(|error| PyValueError::new_err(format!("stylesheet: {error}")))?;
    }

    runner::clear_quit();
    // The GIL is dropped for the duration of the loop and taken back per call,
    // so a Python thread started before `run` keeps running while the window is
    // idle. Without this the loop would hold the GIL forever and every other
    // thread would stop at its next bytecode.
    let result = python.detach(|| runner::run_loop(config, render, on_event));
    result.map_err(|error| PyRuntimeError::new_err(error))
}

/// Asks the window to close at the next turn of the loop.
///
/// A flag rather than an immediate exit: it is called from inside a Python
/// callback, which is itself called from inside the event loop, and tearing the
/// loop down from within one of its own callbacks is how a GPU surface outlives
/// the window it borrows.
#[pyfunction]
fn quit() {
    runner::request_quit();
}

/// Converts a JSON value into the nearest Python object.
///
/// Objects become `dict`, arrays `list`, numbers `int` or `float`. Used for
/// event payloads, which are small and flat — this is not a general-purpose
/// decoder and does not try to be.
pub(crate) fn value_to_python<'py>(python: Python<'py>, value: &Value) -> PyResult<Bound<'py, PyAny>> {
    Ok(match value {
        Value::Null => python.None().into_bound(python),
        Value::Bool(b) => b.into_pyobject(python)?.to_owned().into_any(),
        Value::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(i), _) => i.into_pyobject(python)?.into_any(),
            (None, Some(f)) => f.into_pyobject(python)?.into_any(),
            // A number that is neither: only a `u64` past `i64::MAX` can be
            // this, and it round-trips through its text far better than it
            // would through a silent truncation.
            _ => number.to_string().into_pyobject(python)?.into_any(),
        },
        Value::String(s) => s.into_pyobject(python)?.into_any(),
        Value::Array(items) => {
            let list = PyList::empty(python);
            for item in items {
                list.append(value_to_python(python, item)?)?;
            }
            list.into_any()
        }
        Value::Object(entries) => {
            let dict = PyDict::new(python);
            for (key, item) in entries {
                dict.set_item(key, value_to_python(python, item)?)?;
            }
            dict.into_any()
        }
    })
}

/// The extension module, imported as `_spherekit`.
#[pymodule]
fn spherekit_python(module: &Bound<'_, PyModule>) -> PyResult<()> {
    module.add_function(wrap_pyfunction!(version, module)?)?;
    module.add_function(wrap_pyfunction!(node_types, module)?)?;
    module.add_function(wrap_pyfunction!(check_tree, module)?)?;
    module.add_function(wrap_pyfunction!(check_css, module)?)?;
    module.add_function(wrap_pyfunction!(run, module)?)?;
    module.add_function(wrap_pyfunction!(quit, module)?)?;
    module.add("__doc__", "Native bindings for the SphereKit engine.")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_well_formed_tree_commits_and_reports_its_size() {
        // A label is a `text` node with a `#text` child, not a `text` node
        // carrying a string: only `#text` may hold one, and the host refuses
        // the commit outright if that is got wrong.
        let json = r##"{"revision":1,"children":[
            {"id":1,"type":"view","props":{},"children":[
                {"id":2,"type":"text","props":{},"children":[
                    {"id":3,"type":"#text","text":"hello","props":{},"children":[]}
                ]}
            ]}
        ]}"##;
        Python::initialize();
        assert_eq!(check_tree(json).expect("the tree commits"), 3);
    }

    #[test]
    fn a_label_that_carries_its_own_text_is_refused() {
        // The mistake a hand-written tree makes first, and the reason the
        // Python side builds labels rather than leaving it to the caller.
        Python::initialize();
        let json = r#"{"revision":1,"children":[
            {"id":1,"type":"text","text":"hello","props":{},"children":[]}
        ]}"#;
        assert!(check_tree(json).is_err());
    }

    #[test]
    fn a_tree_with_duplicate_identities_is_refused() {
        // The failure mode this guards: a Python side that reuses an id makes
        // two nodes indistinguishable to the host, and the window would keep
        // painting the previous frame with no error anywhere.
        Python::initialize();
        let json = r#"{"revision":1,"children":[
            {"id":1,"type":"view","props":{},"children":[]},
            {"id":1,"type":"view","props":{},"children":[]}
        ]}"#;
        assert!(check_tree(json).is_err());
    }

    #[test]
    fn malformed_json_is_an_error_rather_than_a_panic() {
        Python::initialize();
        assert!(check_tree("{not json").is_err());
    }

    #[test]
    fn a_stylesheet_reports_its_rule_count() {
        Python::initialize();
        assert_eq!(check_css(".a { color: red } .b { opacity: 0.5 }").expect("parses"), 2);
    }

    #[test]
    fn every_advertised_node_type_is_one_the_host_lowers() {
        // The list Python checks against has to be the list the host answers
        // to. A name in one and not the other is a widget that silently becomes
        // a plain box.
        Python::initialize();
        for kind in runner::NODE_TYPES {
            let json = format!(
                r#"{{"revision":1,"children":[{{"id":1,"type":"{kind}","props":{{}},"children":[]}}]}}"#
            );
            assert_eq!(check_tree(&json).expect("the node commits"), 1, "{kind} did not commit");
        }
    }

    #[test]
    fn json_values_survive_the_crossing() {
        // Payloads carry the numbers a slider and a text field report, and a
        // float arriving as an int is a volume that snaps to zero.
        Python::initialize();
        Python::attach(|python| {
            let value: Value = serde_json::from_str(r#"{"value":0.25,"text":"a","on":true}"#)
                .expect("parses");
            let object = value_to_python(python, &value).expect("converts");
            let dict = object.cast::<PyDict>().expect("an object becomes a dict");
            let volume: f64 =
                dict.get_item("value").unwrap().unwrap().extract().expect("a float stays a float");
            assert!((volume - 0.25).abs() < 1e-9);
            let text: String = dict.get_item("text").unwrap().unwrap().extract().expect("a string");
            assert_eq!(text, "a");
            let on: bool = dict.get_item("on").unwrap().unwrap().extract().expect("a bool");
            assert!(on);
        });
    }
}
