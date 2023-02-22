#![allow(clippy::borrow_deref_ref)] // clippy warns about code generated by #[pymethods]

use super::{IncomingEvent, OperatorEvent, StopReason, Tracer};
use dora_core::{
    config::{NodeId, OperatorId},
    descriptor::source_is_url,
};
use dora_download::download_file;
use dora_operator_api_python::metadata_to_pydict;
use dora_operator_api_types::DoraStatus;
use eyre::{bail, eyre, Context, Result};
use pyo3::{
    pyclass,
    types::IntoPyDict,
    types::{PyBytes, PyDict},
    Py, Python,
};
use std::{
    borrow::Cow,
    panic::{catch_unwind, AssertUnwindSafe},
    path::Path,
};
use tokio::sync::mpsc::{Receiver, Sender};

fn traceback(err: pyo3::PyErr) -> eyre::Report {
    Python::with_gil(|py| {
        eyre::Report::msg(format!(
            "{}\n{err}",
            err.traceback(py)
                .expect("PyError should have a traceback")
                .format()
                .expect("Traceback could not be formatted")
        ))
    })
}

#[tracing::instrument(skip(events_tx, incoming_events, tracer))]
pub fn run(
    node_id: &NodeId,
    operator_id: &OperatorId,
    source: &str,
    events_tx: Sender<OperatorEvent>,
    mut incoming_events: Receiver<IncomingEvent>,
    tracer: Tracer,
) -> eyre::Result<()> {
    let path = if source_is_url(source) {
        let target_path = Path::new("build")
            .join(node_id.to_string())
            .join(format!("{}.py", operator_id));
        // try to download the shared library
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(download_file(source, &target_path))
            .wrap_err("failed to download Python operator")?;
        target_path
    } else {
        Path::new(source).to_owned()
    };

    if !path.exists() {
        bail!("No python file exists at {}", path.display());
    }
    let path = path
        .canonicalize()
        .wrap_err_with(|| format!("no file found at `{}`", path.display()))?;
    let path_cloned = path.clone();

    let send_output = SendOutputCallback {
        events_tx: events_tx.clone(),
    };

    let init_operator = move |py: Python| {
        if let Some(parent_path) = path.parent() {
            let parent_path = parent_path
                .to_str()
                .ok_or_else(|| eyre!("module path is not valid utf8"))?;
            let sys = py.import("sys").wrap_err("failed to import `sys` module")?;
            let sys_path = sys
                .getattr("path")
                .wrap_err("failed to import `sys.path` module")?;
            let sys_path_append = sys_path
                .getattr("append")
                .wrap_err("`sys.path.append` was not found")?;
            sys_path_append
                .call1((parent_path,))
                .wrap_err("failed to append module path to python search path")?;
        }

        let module_name = path
            .file_stem()
            .ok_or_else(|| eyre!("module path has no file stem"))?
            .to_str()
            .ok_or_else(|| eyre!("module file stem is not valid utf8"))?;
        let module = py.import(module_name).map_err(traceback)?;
        let operator_class = module
            .getattr("Operator")
            .wrap_err("no `Operator` class found in module")?;

        let locals = [("Operator", operator_class)].into_py_dict(py);
        let operator = py
            .eval("Operator()", None, Some(locals))
            .map_err(traceback)?;
        Result::<_, eyre::Report>::Ok(Py::from(operator))
    };

    let python_runner = move || {
        let operator =
            Python::with_gil(init_operator).wrap_err("failed to init python operator")?;

        let reason = loop {
            let Some(event) = incoming_events.blocking_recv() else { break StopReason::InputsClosed };

            match event {
                IncomingEvent::Input {
                    input_id,
                    mut metadata,
                    data,
                } => {
                    #[cfg(feature = "tracing")]
                    let (_child_cx, string_cx) = {
                        use dora_tracing::{deserialize_context, serialize_context};
                        use opentelemetry::trace::TraceContextExt;
                        use opentelemetry::{trace::Tracer, Context as OtelContext};

                        let cx = deserialize_context(&metadata.parameters.open_telemetry_context);
                        let span = tracer.start_with_context(format!("{}", input_id), &cx);

                        let child_cx = OtelContext::current_with_span(span);
                        let string_cx = serialize_context(&child_cx);
                        (child_cx, string_cx)
                    };

                    #[cfg(not(feature = "tracing"))]
                    let string_cx = {
                        let () = tracer;
                        "".to_string()
                    };
                    metadata.parameters.open_telemetry_context = Cow::Owned(string_cx);

                    let status = Python::with_gil(|py| -> Result<i32> {
                        // We need to create a new scoped `GILPool` because the dora-runtime
                        // is currently started through a `start_runtime` wrapper function,
                        // which is annotated with `#[pyfunction]`. This attribute creates an
                        // initial `GILPool` that lasts for the entire lifetime of the `dora-runtime`.
                        // However, we want the `PyBytes` created below to be freed earlier.
                        // creating a new scoped `GILPool` tied to this closure, will free `PyBytes`
                        // at the end of the closure.
                        // See https://github.com/PyO3/pyo3/pull/2864 and
                        // https://github.com/PyO3/pyo3/issues/2853 for more details.
                        let pool = unsafe { py.new_pool() };
                        let py = pool.python();
                        let input_dict = PyDict::new(py);

                        input_dict.set_item("id", input_id.as_str())?;
                        if let Some(data) = data {
                            let bytes = PyBytes::new(py, &data);
                            input_dict.set_item("data", bytes)?;
                        }
                        input_dict.set_item("metadata", metadata_to_pydict(&metadata, py))?;

                        let status_enum = operator
                            .call_method1(py, "on_input", (input_dict, send_output.clone()))
                            .map_err(traceback)?;
                        let status_val = Python::with_gil(|py| status_enum.getattr(py, "value"))
                            .wrap_err("on_input must have enum return value")?;
                        Python::with_gil(|py| status_val.extract(py))
                            .wrap_err("on_input has invalid return value")
                    })?;
                    match status {
                        s if s == DoraStatus::Continue as i32 => {} // ok
                        s if s == DoraStatus::Stop as i32 => break StopReason::ExplicitStop,
                        s if s == DoraStatus::StopAll as i32 => break StopReason::ExplicitStopAll,
                        other => bail!("on_input returned invalid status {other}"),
                    }
                }
                IncomingEvent::Stop => {}
            }
        };

        // Dropping the operator using Python garbage collector.
        // Locking the GIL for immediate release.
        Python::with_gil(|_py| {
            drop(operator);
        });

        Result::<_, eyre::Report>::Ok(reason)
    };

    let closure = AssertUnwindSafe(|| {
        python_runner()
            .wrap_err_with(|| format!("error in Python module at {}", path_cloned.display()))
    });

    match catch_unwind(closure) {
        Ok(Ok(reason)) => {
            let _ = events_tx.blocking_send(OperatorEvent::Finished { reason });
        }
        Ok(Err(err)) => {
            let _ = events_tx.blocking_send(OperatorEvent::Error(err));
        }
        Err(panic) => {
            let _ = events_tx.blocking_send(OperatorEvent::Panic(panic));
        }
    }

    Ok(())
}

#[pyclass]
#[derive(Clone)]
struct SendOutputCallback {
    events_tx: Sender<OperatorEvent>,
}

#[allow(unsafe_op_in_unsafe_fn)]
mod callback_impl {

    use crate::operator::OperatorEvent;

    use super::SendOutputCallback;
    use dora_operator_api_python::pydict_to_metadata;
    use eyre::{eyre, Context, Result};
    use pyo3::{
        pymethods,
        types::{PyBytes, PyDict},
    };

    #[pymethods]
    impl SendOutputCallback {
        fn __call__(
            &mut self,
            output: &str,
            data: &PyBytes,
            metadata: Option<&PyDict>,
        ) -> Result<()> {
            let data = data.as_bytes();
            let metadata = pydict_to_metadata(metadata)
                .wrap_err("Could not parse metadata.")?
                .into_owned();

            let event = OperatorEvent::Output {
                output_id: output.to_owned().into(),
                metadata,
                data: data.to_owned(),
            };

            self.events_tx
                .blocking_send(event)
                .map_err(|_| eyre!("failed to send output to runtime"))?;

            Ok(())
        }
    }
}
