//! Python callbacks and value conversion for the runtime's XP4 Session.
mod driver_observation;
mod observation;
use fujimod_emulator::board::xp4::{Definition, Session};
use fujimod_emulator_cpu::{Register, arm};
use pyo3::{
    PyTraverseError, PyVisit,
    prelude::*,
    types::{PyBytes, PyDict, PyList},
};
use std::{cell::RefCell, collections::BTreeMap, rc::Rc};

pub(super) struct Xp4Session {
    inner: Session,
    handler: Rc<RefCell<Option<Py<PyAny>>>>,
}

macro_rules! forward {
    ($name:ident($($argument:ident: $kind:ty),* $(,)?) -> $result:ty) => {
        pub(super) fn $name(&self, $($argument: $kind),*) -> PyResult<$result> {
            self.inner.$name($($argument),*).map_err(crate::emulator::error)
        }
    };
}

impl Xp4Session {
    pub(super) fn from_definition(
        definition: &str,
        storage_image: Option<Vec<u8>>,
    ) -> PyResult<Self> {
        let definition: Definition = serde_json::from_str(definition)
            .map_err(|error| pyo3::exceptions::PyValueError::new_err(error.to_string()))?;
        let inner = Session::new(definition, storage_image).map_err(crate::emulator::error)?;
        let handler = Rc::new(RefCell::new(None::<Py<PyAny>>));
        let observer = handler.clone();
        inner.set_instruction_observer(Some(Rc::new(move |core, address| {
            Python::attach(|py| {
                let callback = observer
                    .borrow()
                    .as_ref()
                    .map(|callback| callback.clone_ref(py));
                if let Some(callback) = callback {
                    callback
                        .call1(py, (core, address))
                        .map_err(|error| error.to_string())?;
                }
                Ok(())
            })
        })));
        Ok(Self { inner, handler })
    }
    pub(super) fn __traverse__(&self, visit: PyVisit<'_>) -> Result<(), PyTraverseError> {
        if let Ok(handler) = self.handler.try_borrow()
            && let Some(handler) = handler.as_ref()
        {
            visit.call(handler)?;
        }
        Ok(())
    }
    pub(super) fn __clear__(&self) {
        self.handler.borrow_mut().take();
    }
    pub(super) fn set_pc_handler(&self, callback: Option<Py<PyAny>>) {
        *self.handler.borrow_mut() = callback;
    }
    pub(super) fn ram_bytes(&self) -> usize {
        self.inner.ram_bytes()
    }
    pub(super) fn instructions(&self) -> Vec<u64> {
        self.inner.instructions()
    }
    pub(super) fn now(&self) -> u64 {
        self.inner.now()
    }
    pub(super) fn quiescent(&self) -> bool {
        self.inner.quiescent()
    }
    forward!(run_board(limit: u64, quantum: usize) -> ());
    forward!(read_register(core: usize, register: Register) -> u64);
    forward!(write_register(core: usize, register: Register, value: u64) -> ());
    forward!(coprocessor(core: usize, selector: arm::Coprocessor, value: Option<u64>) -> u64);
    forward!(
        driver_guard_begin(
            executable: Vec<(u64, u64)>,
            readable: Vec<(u64, u64)>,
            writable: Vec<(u64, u64)>,
            return_address: u64,
            store_limit: usize,
            core: usize,
        ) -> ()
    );
    pub(super) fn driver_guard_end(&self) {
        self.inner.driver_guard_end();
    }
    pub(super) fn driver_guard_state<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<Option<Bound<'py, PyDict>>> {
        self.inner
            .driver_guard_state()
            .map(|saved| driver_observation::state(py, saved))
            .transpose()
    }
    forward!(instruction_points(core: usize, addresses: Vec<u64>) -> ());
    forward!(
        function_watch_begin(
            entries: Vec<u64>,
            quota: u64,
            edge_limit: usize,
            state_limit: usize,
            return_sites: Option<Vec<u64>>,
            quota_per_edge: bool,
        ) -> ()
    );
    pub(super) fn function_watch_end(&self) {
        self.inner.function_watch_end();
    }
    pub(super) fn function_watch_state<'py>(
        &self,
        py: Python<'py>,
    ) -> PyResult<Option<Bound<'py, PyDict>>> {
        self.inner
            .function_watch_state()
            .map(|saved| crate::emulator::python_observation::snapshot(py, &saved))
            .transpose()
    }
    pub(super) fn reg_read(&self, core: usize, register: i32) -> PyResult<u32> {
        let value = self.read_register(
            core,
            crate::emulator::client_register(register).map_err(crate::emulator::error)?,
        )?;
        u32::try_from(value)
            .map_err(|_| crate::emulator::error("ARMv7-A scalar register exceeds 32 bits".into()))
    }
    pub(super) fn reg_write(&self, core: usize, register: i32, value: u32) -> PyResult<()> {
        self.write_register(
            core,
            crate::emulator::client_register(register).map_err(crate::emulator::error)?,
            u64::from(value),
        )
    }
    forward!(cpr_read(core: usize, crn: u32, crm: u32, opc1: u32, opc2: u32) -> u64);
    pub(super) fn read<'py>(
        &self,
        py: Python<'py>,
        address: usize,
        size: usize,
    ) -> PyResult<Bound<'py, PyBytes>> {
        self.inner
            .validate_read(address, size)
            .map_err(crate::emulator::error)?;
        PyBytes::new_with(py, size, |out| {
            self.inner
                .read(address, out)
                .map_err(crate::emulator::error)
        })
    }
    forward!(write(address: usize, data: &[u8]) -> ());
    pub(super) fn read_virtual<'py>(
        &self,
        py: Python<'py>,
        core: usize,
        address: u32,
        size: usize,
    ) -> PyResult<Bound<'py, PyBytes>> {
        let bytes = self
            .inner
            .read_virtual(core, address, size)
            .map_err(crate::emulator::error)?;
        Ok(PyBytes::new(py, &bytes))
    }
    forward!(run(core: usize, budget: usize) -> ());
    pub(super) fn idle(&self) -> bool {
        self.inner.idle()
    }
    forward!(set_adc_input(channel: usize, sample: u16) -> ());
    forward!(set_gpio_input(port: usize, pin: usize, high: bool) -> ());
    forward!(set_usb_connected(connected: bool) -> ());
    forward!(usb_bus_reset() -> ());
    forward!(usb_transaction(physical_endpoint: usize, setup: bool, out: Option<Vec<u8>>) -> u64);
    forward!(usb_reply_json(id: u64) -> Option<String>);
    forward!(usb_consume_reply(id: u64) -> ());
    pub(super) fn scanout_frames<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        observation::frames(py, self.inner.scanout_frames())
    }
    pub(super) fn device_state_json(&self) -> BTreeMap<String, String> {
        self.inner.device_state_json()
    }
    pub(super) fn interrupt_state<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        observation::interrupts(
            py,
            self.inner
                .interrupt_state()
                .map_err(crate::emulator::error)?,
        )
    }
    pub(super) fn device_records<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyList>> {
        observation::records(py, self.inner.device_records())
    }
    pub(super) fn state<'py>(
        &self,
        py: Python<'py>,
        observations: bool,
    ) -> PyResult<Bound<'py, PyDict>> {
        observation::state(py, self.inner.state(observations))
    }
    pub(super) fn frontier(
        &self,
        by_count: bool,
    ) -> Vec<crate::emulator::discovery::FrontierEntry> {
        self.inner
            .frontier(by_count)
            .into_iter()
            .map(Into::into)
            .collect()
    }
    pub(super) fn discovery_shadow(&self) -> Vec<(u64, u8)> {
        self.inner.discovery_shadow()
    }
    pub(super) fn interventions(&self) -> Vec<crate::emulator::discovery::Intervention> {
        self.inner
            .interventions()
            .into_iter()
            .map(Into::into)
            .collect()
    }
}
