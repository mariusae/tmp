# wakerbench

Benchmarking different approaches to waking up Python async tasks from Rust.

## Background

When building Python extensions in Rust that need to wake up Python's asyncio event loop from a background thread, there are two main approaches:

1. **call_soon_threadsafe** - The standard Python approach. The Rust thread acquires the GIL and calls `loop.call_soon_threadsafe(callback)`.

2. **FD-based wakeup** - Use a raw file descriptor (pipe) registered with `loop.add_reader()`. The Rust thread simply writes a byte to the pipe - no GIL acquisition needed.

## Results

On Apple M1 (macOS), with Python driving the benchmark and Rust OS threads sending notifications:

| Approach | Mean Latency | Relative |
|----------|--------------|----------|
| FD-based (no GIL) | ~48 µs | 1.0x |
| call_soon_threadsafe (GIL) | ~56 µs | 1.2x |
| Pure Python (executor) | ~44 µs | 0.9x |

**Key finding**: The FD-based approach is only ~1.2x faster than call_soon_threadsafe in this single-threaded benchmark. The main benefit of FD-based wakeup is avoiding GIL contention under high concurrency.

## How It Works

### Benchmark Scenario

Both approaches are tested with the same scenario:
1. Python waits for a wakeup (using `asyncio.Event`)
2. Rust spawns an OS thread
3. The Rust thread sends a notification to wake up Python
4. Python measures the latency

### FD-based Approach (No GIL)

```python
# Python side
waker = wakerbench.create_fd_waker()
loop.add_reader(waker.get_read_fd(), on_readable)

# When Rust writes to the pipe, on_readable is called
def on_readable():
    waker.drain()
    event.set()
```

```rust
// Rust side - NO GIL needed!
fn wake(&self) {
    let buf = [1u8; 1];
    unsafe {
        libc::write(self.write_fd, buf.as_ptr() as *const libc::c_void, 1);
    }
}
```

### call_soon_threadsafe Approach (Acquires GIL)

```python
# Python side
def on_wakeup():
    event.set()

waker = wakerbench.create_callback_waker(on_wakeup, loop)
```

```rust
// Rust side - ACQUIRES GIL
fn wake(&self) {
    Python::with_gil(|py| {
        self.event_loop
            .call_method1(py, "call_soon_threadsafe", (&self.callback,));
    });
}
```

## Building

### Prerequisites

- Rust (1.70+)
- Python 3.10+
- maturin (`cargo install maturin --no-default-features`)

### Build Steps

```bash
# Create and activate a virtual environment
python3 -m venv venv
source venv/bin/activate

# Build and install the extension
maturin develop --release
```

## Running Benchmarks

```bash
source venv/bin/activate
python benchmark.py
```

## When to Use Each Approach

### FD-based Approach
**Best for:**
- High-concurrency scenarios where GIL contention is a concern
- Systems with many Rust threads waking up Python simultaneously
- When you need the lowest possible latency variance

**Trade-offs:**
- More setup code required
- Need to manage FD lifecycle
- Platform-specific (uses pipes)

### call_soon_threadsafe Approach
**Best for:**
- Simpler integration requirements
- When GIL contention isn't a bottleneck
- Better compatibility with Python tooling/debugging

**Trade-offs:**
- Acquires GIL from non-Python thread
- May cause contention under high concurrency

## Project Structure

```
wakerbench/
├── Cargo.toml           # Rust dependencies
├── pyproject.toml       # Maturin/Python build config
├── src/
│   └── lib.rs           # Rust extension module
├── benchmark.py         # Python benchmark script
└── README.md            # This file
```

## License

MIT
