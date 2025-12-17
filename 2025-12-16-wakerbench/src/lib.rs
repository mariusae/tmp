use pyo3::prelude::*;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

// =============================================================================
// Approach 1: FD-based wakeup (no GIL acquisition on Rust side)
// =============================================================================

/// A waker that uses a raw file descriptor to wake up the Python event loop.
/// This avoids acquiring the GIL on the Rust side.
#[pyclass]
struct FdWaker {
    write_fd: RawFd,
    read_fd: RawFd,
    // Store owned FDs to ensure they're closed on drop
    #[allow(dead_code)]
    owned_write: Option<OwnedFd>,
    #[allow(dead_code)]
    owned_read: Option<OwnedFd>,
}

#[pymethods]
impl FdWaker {
    #[new]
    fn new() -> PyResult<Self> {
        let mut fds = [0 as RawFd; 2];
        let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if result != 0 {
            return Err(PyErr::new::<pyo3::exceptions::PyOSError, _>(
                "Failed to create pipe",
            ));
        }

        // Set non-blocking on read end
        unsafe {
            let flags = libc::fcntl(fds[0], libc::F_GETFL);
            libc::fcntl(fds[0], libc::F_SETFL, flags | libc::O_NONBLOCK);
        }

        let owned_read = unsafe { OwnedFd::from_raw_fd(fds[0]) };
        let owned_write = unsafe { OwnedFd::from_raw_fd(fds[1]) };

        Ok(Self {
            read_fd: fds[0],
            write_fd: fds[1],
            owned_read: Some(owned_read),
            owned_write: Some(owned_write),
        })
    }

    /// Get the read file descriptor for registering with the event loop
    fn get_read_fd(&self) -> RawFd {
        self.read_fd
    }

    /// Drain any pending bytes from the pipe (call this in the callback)
    fn drain(&self) -> PyResult<()> {
        let mut buf = [0u8; 64];
        loop {
            let result = unsafe {
                libc::read(self.read_fd, buf.as_mut_ptr() as *mut libc::c_void, buf.len())
            };
            if result <= 0 {
                break;
            }
        }
        Ok(())
    }
}

/// Holder for the write FD that can be sent across threads
struct FdWakerHandle {
    write_fd: RawFd,
}

unsafe impl Send for FdWakerHandle {}
unsafe impl Sync for FdWakerHandle {}

impl FdWakerHandle {
    fn wake(&self) {
        let buf = [1u8; 1];
        unsafe {
            libc::write(self.write_fd, buf.as_ptr() as *const libc::c_void, 1);
        }
    }
}

/// Create an FD-based waker
#[pyfunction]
fn create_fd_waker() -> PyResult<FdWaker> {
    FdWaker::new()
}

/// Spawn a Rust OS thread that will wake up Python via the FD after an optional delay.
/// This does NOT acquire the GIL.
#[pyfunction]
fn fd_wakeup_from_thread(waker: &FdWaker, delay_micros: u64) {
    let handle = FdWakerHandle {
        write_fd: waker.write_fd,
    };

    std::thread::spawn(move || {
        if delay_micros > 0 {
            std::thread::sleep(Duration::from_micros(delay_micros));
        }
        handle.wake();
    });
}

// =============================================================================
// Approach 2: call_soon_threadsafe wakeup (acquires GIL on Rust side)
// =============================================================================

/// A waker that uses call_soon_threadsafe to wake up the Python event loop.
/// This DOES acquire the GIL on the Rust side.
#[pyclass]
struct CallbackWaker {
    // Store the Python callback and event loop
    callback: PyObject,
    event_loop: PyObject,
}

#[pymethods]
impl CallbackWaker {
    #[new]
    fn new(callback: PyObject, event_loop: PyObject) -> Self {
        Self {
            callback,
            event_loop,
        }
    }
}

/// Holder for the callback waker that can be sent across threads
struct CallbackWakerHandle {
    callback: PyObject,
    event_loop: PyObject,
}

unsafe impl Send for CallbackWakerHandle {}

impl CallbackWakerHandle {
    fn wake(&self) {
        // This ACQUIRES THE GIL from the Rust thread
        Python::with_gil(|py| {
            // Call event_loop.call_soon_threadsafe(callback)
            let _ = self
                .event_loop
                .call_method1(py, "call_soon_threadsafe", (&self.callback,));
        });
    }
}

/// Create a callback-based waker
#[pyfunction]
fn create_callback_waker(callback: PyObject, event_loop: PyObject) -> CallbackWaker {
    CallbackWaker::new(callback, event_loop)
}

/// Spawn a Rust OS thread that will wake up Python via call_soon_threadsafe after an optional delay.
/// This ACQUIRES the GIL from the Rust thread.
#[pyfunction]
fn callback_wakeup_from_thread(py: Python<'_>, waker: &CallbackWaker, delay_micros: u64) {
    let handle = CallbackWakerHandle {
        callback: waker.callback.clone_ref(py),
        event_loop: waker.event_loop.clone_ref(py),
    };

    std::thread::spawn(move || {
        if delay_micros > 0 {
            std::thread::sleep(Duration::from_micros(delay_micros));
        }
        handle.wake();
    });
}

// =============================================================================
// Throughput benchmark: measure how many wakeups per second each approach can do
// =============================================================================

/// Spawn a Rust thread that sends N wakeups as fast as possible via FD.
/// Returns immediately. Use this for throughput testing.
#[pyfunction]
fn fd_wakeup_burst(waker: &FdWaker, count: usize) {
    let handle = FdWakerHandle {
        write_fd: waker.write_fd,
    };

    std::thread::spawn(move || {
        for _ in 0..count {
            handle.wake();
        }
    });
}

/// Spawn a Rust thread that sends N wakeups as fast as possible via call_soon_threadsafe.
/// Returns immediately. Use this for throughput testing.
#[pyfunction]
fn callback_wakeup_burst(py: Python<'_>, waker: &CallbackWaker, count: usize) {
    let handle = CallbackWakerHandle {
        callback: waker.callback.clone_ref(py),
        event_loop: waker.event_loop.clone_ref(py),
    };

    std::thread::spawn(move || {
        for _ in 0..count {
            handle.wake();
        }
    });
}

// =============================================================================
// Latency benchmark helpers
// =============================================================================

/// Shared counter for coordinating benchmark iterations
#[pyclass]
struct BenchCoordinator {
    counter: Arc<AtomicU64>,
}

#[pymethods]
impl BenchCoordinator {
    #[new]
    fn new() -> Self {
        Self {
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    fn get_count(&self) -> u64 {
        self.counter.load(Ordering::SeqCst)
    }

    fn reset(&self) {
        self.counter.store(0, Ordering::SeqCst);
    }
}

/// Spawn a thread that will perform `iterations` wakeups with a small delay between each.
/// Each wakeup increments the coordinator's counter, allowing Python to verify receipt.
#[pyfunction]
fn fd_wakeup_sequence(waker: &FdWaker, coordinator: &BenchCoordinator, iterations: usize) {
    let handle = FdWakerHandle {
        write_fd: waker.write_fd,
    };
    let counter = coordinator.counter.clone();

    std::thread::spawn(move || {
        for _ in 0..iterations {
            counter.fetch_add(1, Ordering::SeqCst);
            handle.wake();
            // Small delay to allow Python to process
            std::thread::sleep(Duration::from_micros(100));
        }
    });
}

#[pyfunction]
fn callback_wakeup_sequence(
    py: Python<'_>,
    waker: &CallbackWaker,
    coordinator: &BenchCoordinator,
    iterations: usize,
) {
    let handle = CallbackWakerHandle {
        callback: waker.callback.clone_ref(py),
        event_loop: waker.event_loop.clone_ref(py),
    };
    let counter = coordinator.counter.clone();

    std::thread::spawn(move || {
        for _ in 0..iterations {
            counter.fetch_add(1, Ordering::SeqCst);
            handle.wake();
            // Small delay to allow Python to process
            std::thread::sleep(Duration::from_micros(100));
        }
    });
}

#[pymodule]
fn wakerbench(m: &Bound<'_, PyModule>) -> PyResult<()> {
    // FD-based approach
    m.add_class::<FdWaker>()?;
    m.add_function(wrap_pyfunction!(create_fd_waker, m)?)?;
    m.add_function(wrap_pyfunction!(fd_wakeup_from_thread, m)?)?;
    m.add_function(wrap_pyfunction!(fd_wakeup_burst, m)?)?;
    m.add_function(wrap_pyfunction!(fd_wakeup_sequence, m)?)?;

    // Callback-based approach
    m.add_class::<CallbackWaker>()?;
    m.add_function(wrap_pyfunction!(create_callback_waker, m)?)?;
    m.add_function(wrap_pyfunction!(callback_wakeup_from_thread, m)?)?;
    m.add_function(wrap_pyfunction!(callback_wakeup_burst, m)?)?;
    m.add_function(wrap_pyfunction!(callback_wakeup_sequence, m)?)?;

    // Coordination
    m.add_class::<BenchCoordinator>()?;

    Ok(())
}
