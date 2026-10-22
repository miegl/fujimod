//! Local-future ownership and Python conversions; camera behavior stays in remote.
use fujimod_remote::{self as remote, *};
use pyo3::{
    exceptions::{PyRuntimeError, PyValueError},
    prelude::*,
    types::{PyBytes, PyList, PyTuple},
};
use std::{
    cell::RefCell,
    collections::BTreeMap,
    rc::Rc,
    sync::Arc,
    task::{Context, Wake, Waker},
};
mod task;
use task::{Output, Task};

fn error(message: String) -> PyErr {
    PyRuntimeError::new_err(message)
}
fn decode<T: serde::de::DeserializeOwned>(json: &str) -> PyResult<T> {
    serde_json::from_str(json).map_err(|e| PyValueError::new_err(e.to_string()))
}
type Slot = Rc<RefCell<Option<Box<dyn Remote>>>>;
#[pyclass(unsendable, module = "fujimod._native.remote")]
struct Camera {
    slot: Slot,
    facts: Rc<Facts>,
    device_ids: BTreeMap<String, usize>,
}
struct Lease {
    slot: Slot,
    remote: Option<Box<dyn Remote>>,
}
impl Lease {
    fn new(slot: &Slot) -> PyResult<Self> {
        let remote = slot
            .borrow_mut()
            .take()
            .ok_or_else(|| error("Camera is busy with another operation".into()))?;
        Ok(Self {
            slot: slot.clone(),
            remote: Some(remote),
        })
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        *self.slot.borrow_mut() = self.remote.take();
    }
}
#[cfg(all(feature = "emulator", not(target_os = "emscripten")))]
struct PythonBoard(Py<crate::emulator::board::Board>);
#[cfg(all(feature = "emulator", not(target_os = "emscripten")))]
impl remote::board::SessionOwner for PythonBoard {
    fn with_session<T>(
        &mut self,
        call: impl FnOnce(&mut fujimod_emulator::session::Session) -> Result<T>,
    ) -> Result<T> {
        Python::attach(|py| {
            let mut board = self
                .0
                .bind(py)
                .try_borrow_mut()
                .map_err(|e| e.to_string())?;
            call(board.session_mut().map_err(|e| e.to_string())?)
        })
    }
}
#[pymethods]
impl Camera {
    #[staticmethod]
    fn from_ff80(
        session: &crate::usb::asynchronous::AsyncSession,
        facts: &str,
        read_bytes: u32,
        write_bytes: u32,
        event_bytes: u32,
        poll_seconds: f64,
        output: String,
    ) -> PyResult<Self> {
        use std::{num::NonZeroU32, time::Duration};
        let nz = |v| {
            NonZeroU32::new(v)
                .ok_or_else(|| PyValueError::new_err("Transport limits must be nonzero"))
        };
        if !poll_seconds.is_finite() || poll_seconds <= 0. || poll_seconds > 60. {
            return Err(PyValueError::new_err(
                "Poll interval must be in (0, 60] seconds",
            ));
        }
        let facts: Facts = decode(facts)?;
        let options = remote::ff80::Options {
            memory_limits: fujimod_usb::ff80::commands::memory::DebugMemoryLimits {
                read: nz(read_bytes)?,
                write: nz(write_bytes)?,
            },
            event_bytes: nz(event_bytes)?,
            poll_interval: Duration::from_secs_f64(poll_seconds),
            output,
        };
        let remote = remote::Ff80Remote::new(session.remote_lease()?, facts.clone(), options)
            .map_err(error)?;
        Ok(Self {
            slot: Rc::new(RefCell::new(Some(Box::new(remote)))),
            facts: Rc::new(facts),
            device_ids: BTreeMap::new(),
        })
    }
    fn close(&self) -> PyResult<()> {
        self.slot
            .borrow_mut()
            .take()
            .ok_or_else(|| error("Camera is busy or closed".into()))?;
        Ok(())
    }
    fn wait_until(&self, reference: &str, predicate: Py<PyAny>, deadline: u64) -> PyResult<Task> {
        let reference: Reference = decode(reference)?;
        let mut lease = Lease::new(&self.slot)?;
        Ok(Task::new(Box::pin(async move {
            let r = lease.remote.as_deref_mut().ok_or("Camera lost its owner")?;
            let last = Rc::new(RefCell::new(None));
            let observed = last.clone();
            let condition = format!("Python predicate on {reference:?}");
            let predicate = Rc::new(predicate);
            remote::operations::wait_until(
                r,
                move |view| {
                    let reference = reference.clone();
                    let predicate = predicate.clone();
                    let observed = observed.clone();
                    Box::pin(async move {
                        let read_result = view.read(&reference).await?;
                        let satisfied = Python::attach(|py| -> PyResult<bool> {
                            let json = serde_json::to_string(&read_result)
                                .map_err(|e| error(e.to_string()))?;
                            let value = py
                                .import("fujimod.remote")?
                                .getattr("_convert")?
                                .call1((json,))?;
                            predicate.bind(py).call1((value,))?.extract()
                        })
                        .map_err(|e| e.to_string())?;
                        *observed.borrow_mut() = Some(read_result);
                        Ok(satisfied)
                    })
                },
                deadline,
                &condition,
            )
            .await?;
            let read_result = last
                .borrow_mut()
                .take()
                .ok_or("Predicate completed without a read result")?;
            Ok(Output::RemoteReadResult(read_result))
        })))
    }
    fn wait_until_memory(
        &self,
        ranges: &str,
        predicate: Py<PyAny>,
        deadline: u64,
        condition: String,
    ) -> PyResult<Task> {
        let ranges: Vec<(u64, usize)> = decode(ranges)?;
        if ranges.is_empty() {
            return Err(PyValueError::new_err(
                "At least one memory range is required",
            ));
        }
        let mut lease = Lease::new(&self.slot)?;
        Ok(Task::new(Box::pin(async move {
            let r = lease.remote.as_deref_mut().ok_or("Camera lost its owner")?;
            let predicate = Rc::new(predicate);
            let last = Rc::new(RefCell::new(None));
            let observed = last.clone();
            remote::operations::wait_until(
                r,
                move |view| {
                    let ranges = ranges.clone();
                    let predicate = predicate.clone();
                    let observed = observed.clone();
                    Box::pin(async move {
                        let mut reads = Vec::with_capacity(ranges.len());
                        for &(address, size) in &ranges {
                            reads.push(view.read_memory(address, size).await?);
                        }
                        let satisfied = Python::attach(|py| -> PyResult<bool> {
                            let values = PyList::empty(py);
                            for read in &reads {
                                values.append((read.address, PyBytes::new(py, &read.bytes)))?;
                            }
                            predicate.bind(py).call1((values,))?.extract()
                        })
                        .map_err(|error| error.to_string())?;
                        *observed.borrow_mut() = Some(reads);
                        Ok(satisfied)
                    })
                },
                deadline,
                &condition,
            )
            .await?;
            Ok(Output::MemoryReads(
                last.borrow_mut()
                    .take()
                    .ok_or("Predicate completed without memory reads")?,
            ))
        })))
    }
    fn wait_until_stable_memory_task(
        &self,
        ranges: &str,
        stable_spans: &str,
        equals: &str,
        stable_comparisons: usize,
        deadline: u64,
        condition: String,
    ) -> PyResult<Task> {
        let ranges: Vec<(u64, usize)> = decode(ranges)?;
        let stable_spans: Vec<(u64, usize)> = decode(stable_spans)?;
        let equals: Vec<(u64, Vec<u8>)> = decode(equals)?;
        let mut lease = Lease::new(&self.slot)?;
        Ok(Task::new(Box::pin(async move {
            let r = lease.remote.as_deref_mut().ok_or("Camera lost its owner")?;
            let reads = remote::operations::wait_until_stable_memory(
                r,
                &ranges,
                &stable_spans,
                &equals,
                stable_comparisons,
                deadline,
                &condition,
            )
            .await?;
            Ok(Output::MemoryReads(reads))
        })))
    }
    fn wait_reads(&self, reads: &str, deadline: u64) -> PyResult<Task> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum DeviceWatch {
            Id(usize),
            Name(String),
        }
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Reads {
            stores: Vec<(u64, u64)>,
            devices: Vec<DeviceWatch>,
        }
        let observed: Reads = decode(reads)?;
        let mut reads = ReadSet {
            stores: observed.stores,
            devices: Default::default(),
        };
        for watch in observed.devices {
            let id = match watch {
                DeviceWatch::Id(id) => id,
                DeviceWatch::Name(name) => *self.device_ids.get(&name).ok_or_else(|| {
                    PyValueError::new_err(format!("Unknown observed device {name}"))
                })?,
            };
            reads.devices.insert(id);
        }
        let mut lease = Lease::new(&self.slot)?;
        Ok(Task::new(Box::pin(async move {
            lease
                .remote
                .as_deref_mut()
                .ok_or("Camera lost its owner")?
                .wait(&reads, deadline)
                .await?;
            Ok(Output::Unit)
        })))
    }
    fn set_input(&self, reference: &str, state: &str, deadline: u64) -> PyResult<Task> {
        let reference: Reference = decode(reference)?;
        let Reference::Control(control) = reference else {
            return Err(PyValueError::new_err("Input requires a control"));
        };
        let state: InputState = decode(state)?;
        let mut lease = Lease::new(&self.slot)?;
        Ok(Task::new(Box::pin(async move {
            lease
                .remote
                .as_deref_mut()
                .ok_or("Camera lost its owner")?
                .set_input(&Input { control, state }, deadline)
                .await?;
            Ok(Output::Unit)
        })))
    }
    #[getter]
    fn now(&self) -> PyResult<u64> {
        self.slot
            .borrow()
            .as_ref()
            .map(|r| r.now())
            .ok_or_else(|| error("Camera is busy".into()))
    }
    fn operation(
        &self,
        action: String,
        reference: &str,
        deadline: u64,
        steps: i32,
    ) -> PyResult<Task> {
        let reference: Option<Reference> = decode(reference)?;
        let mut lease = Lease::new(&self.slot)?;
        let facts = self.facts.clone();
        Ok(Task::new(Box::pin(async move {
            let r = lease
                .remote
                .as_deref_mut()
                .ok_or("Camera operation lost its owner")?;
            match action.as_str() {
                "read" => Ok(Output::RemoteReadResult(
                    r.read(reference.as_ref().ok_or("read requires a reference")?)
                        .await?,
                )),
                "navigate" => match reference {
                    Some(Reference::Menu(item)) => Ok(Output::RemoteReadResult(
                        remote::operations::navigate(r, &facts, &item, deadline).await?,
                    )),
                    _ => Err("navigate requires a menu reference".into()),
                },
                "select" => match reference {
                    Some(Reference::Option(choice)) => {
                        remote::operations::select(r, &facts, &choice, deadline).await?;
                        Ok(Output::Unit)
                    }
                    _ => Err("select requires an option reference".into()),
                },
                "press" | "turn" => match reference {
                    Some(Reference::Control(control)) => {
                        if action == "press" {
                            remote::operations::press(r, &control, deadline).await?;
                        } else {
                            remote::operations::turn(r, &control, steps, deadline).await?;
                        }
                        Ok(Output::Unit)
                    }
                    _ => Err("input requires a control reference".into()),
                },
                "screenshot" => Ok(Output::Capture(
                    remote::operations::screenshot(r, deadline).await?,
                )),
                "frame" => Ok(Output::Capture(r.frame().await?)),
                "wait" => {
                    let read_result = r
                        .read(reference.as_ref().ok_or("wait requires a reference")?)
                        .await?;
                    r.wait(&read_result.reads, deadline).await?;
                    Ok(Output::Unit)
                }
                _ => Err(format!("Unknown camera operation {action}")),
            }
        })))
    }
}
#[pyfunction]
fn facts_from_selection(selection: &str, manifest: &str) -> PyResult<String> {
    let selection = decode(selection)?;
    let manifest = decode(manifest)?;
    let facts = Facts::from_selection(&selection, manifest).map_err(error)?;
    serde_json::to_string(&facts).map_err(|e| error(e.to_string()))
}
pub(crate) fn register(py: Python<'_>, parent: &Bound<'_, PyModule>) -> PyResult<()> {
    let module = PyModule::new(py, "remote")?;
    module.add_function(wrap_pyfunction!(facts_from_selection, &module)?)?;
    module.add_class::<Camera>()?;
    module.add_class::<Task>()?;
    parent.add_submodule(&module)
}

#[cfg(all(feature = "emulator", not(target_os = "emscripten")))]
#[pymethods]
impl Camera {
    #[staticmethod]
    fn from_board(
        board: Py<crate::emulator::board::Board>,
        facts: &str,
        options: &str,
    ) -> PyResult<Self> {
        let facts: Facts = decode(facts)?;
        let device_ids = Python::attach(|py| -> PyResult<BTreeMap<String, usize>> {
            let mut owner = board.bind(py).try_borrow_mut()?;
            Ok(owner
                .session_mut()?
                .nodes
                .iter()
                .map(|(name, id)| (name.clone(), *id))
                .collect())
        })?;
        let remote = remote::BoardRemote::new(PythonBoard(board), facts.clone(), decode(options)?)
            .map_err(error)?;
        Ok(Self {
            slot: Rc::new(RefCell::new(Some(Box::new(remote)))),
            facts: Rc::new(facts),
            device_ids,
        })
    }
}

#[cfg(feature = "testing")]
#[pymethods]
impl Camera {
    #[staticmethod]
    #[pyo3(signature = (session, poll_seconds=0.001))]
    fn synthetic_ff80(
        session: &crate::usb::asynchronous::AsyncSession,
        poll_seconds: f64,
    ) -> PyResult<Self> {
        let facts =
            serde_json::to_string(&remote::testing::facts()).map_err(|e| error(e.to_string()))?;
        Self::from_ff80(session, &facts, 4, 4, 64, poll_seconds, "lcd".into())
    }
    #[staticmethod]
    fn synthetic() -> PyResult<Self> {
        let facts = remote::testing::facts();
        let remote = remote::connection::Connection::connect(
            remote::testing::Synthetic::new(),
            facts.clone(),
        )
        .map_err(error)?;
        Ok(Self {
            slot: Rc::new(RefCell::new(Some(Box::new(remote)))),
            facts: Rc::new(facts),
            device_ids: BTreeMap::from([("scanout".into(), 7)]),
        })
    }
}
