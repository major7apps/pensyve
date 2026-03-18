use pyo3::prelude::*;

pub mod config;
pub mod storage;
pub mod types;

#[pymodule]
fn _core(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", "0.1.0")?;
    Ok(())
}
