use pyo3::prelude::*;

pub mod config;
pub mod embedding;
pub mod storage;
pub mod types;
pub mod vector;

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", "0.1.0")?;
    Ok(())
}
