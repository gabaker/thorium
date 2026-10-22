//! Python functions for agentic tool specs

use pyo3::exceptions::PyValueError;
use pyo3::{PyResult, pyfunction};

/// Get the portability report of a JSON encoded agentic tool spec as JSON
///
/// The spec may be a full tool spec (`{"Agentic": {...}}`) or a bare agentic tool, and the report
/// lists whether its input schema can be sent in strict mode and which lossy rewrites a Gemini
/// schema needs.
///
/// # Arguments
///
/// * `spec` - The JSON encoded spec
#[pyfunction]
pub fn tool_portability(spec: &str) -> PyResult<String> {
    // build the report, raising a ValueError for a spec that isn't an agentic tool
    crate::models::tool_portability_json(spec).map_err(PyValueError::new_err)
}
