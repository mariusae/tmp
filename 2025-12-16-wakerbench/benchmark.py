#!/usr/bin/env python3
"""
Benchmark comparing different approaches to waking up Python async tasks from Rust.

Both approaches:
- Python drives the benchmark loop and waits for wakeups
- Rust OS thread sends the notification
- We measure the latency Python experiences

Approach 1: FD-based wakeup
    - Python registers a pipe FD with loop.add_reader()
    - Rust thread writes a byte to the pipe (NO GIL acquisition)
    - Python event loop wakes up

Approach 2: call_soon_threadsafe wakeup
    - Python provides a callback and event loop to Rust
    - Rust thread acquires GIL and calls loop.call_soon_threadsafe(callback)
    - Python event loop wakes up
"""

import asyncio
import time
import statistics
import wakerbench


async def bench_fd_wakeup(iterations: int) -> tuple[float, list[float]]:
    """
    Benchmark FD-based wakeup.

    Rust thread writes to pipe -> Python event loop wakes up.
    No GIL acquisition on Rust side.
    """
    loop = asyncio.get_event_loop()
    waker = wakerbench.create_fd_waker()

    latencies = []
    wakeup_event = asyncio.Event()

    def on_readable():
        waker.drain()
        wakeup_event.set()

    loop.add_reader(waker.get_read_fd(), on_readable)

    try:
        for _ in range(iterations):
            wakeup_event.clear()
            start = time.perf_counter_ns()

            # Rust OS thread will write to the pipe (no GIL)
            wakerbench.fd_wakeup_from_thread(waker, 0)

            await wakeup_event.wait()
            end = time.perf_counter_ns()
            latencies.append(end - start)
    finally:
        loop.remove_reader(waker.get_read_fd())

    return statistics.mean(latencies), latencies


async def bench_callback_wakeup(iterations: int) -> tuple[float, list[float]]:
    """
    Benchmark call_soon_threadsafe wakeup.

    Rust thread acquires GIL -> calls loop.call_soon_threadsafe(callback) -> Python wakes up.
    GIL IS acquired on Rust side.
    """
    loop = asyncio.get_event_loop()
    wakeup_event = asyncio.Event()

    def on_wakeup():
        wakeup_event.set()

    waker = wakerbench.create_callback_waker(on_wakeup, loop)
    latencies = []

    for _ in range(iterations):
        wakeup_event.clear()
        start = time.perf_counter_ns()

        # Rust OS thread will acquire GIL and call call_soon_threadsafe
        wakerbench.callback_wakeup_from_thread(waker, 0)

        await wakeup_event.wait()
        end = time.perf_counter_ns()
        latencies.append(end - start)

    return statistics.mean(latencies), latencies


async def bench_pure_python_wakeup(iterations: int) -> tuple[float, list[float]]:
    """
    Benchmark pure Python thread-to-async wakeup for comparison.

    Uses run_in_executor which internally uses call_soon_threadsafe.
    """
    import concurrent.futures

    loop = asyncio.get_event_loop()
    executor = concurrent.futures.ThreadPoolExecutor(max_workers=1)
    latencies = []

    def thread_work():
        pass

    for _ in range(iterations):
        start = time.perf_counter_ns()
        await loop.run_in_executor(executor, thread_work)
        end = time.perf_counter_ns()
        latencies.append(end - start)

    executor.shutdown(wait=True)
    return statistics.mean(latencies), latencies


def print_stats(name: str, latencies: list[float]):
    """Print detailed statistics for a benchmark."""
    mean = statistics.mean(latencies)
    median = statistics.median(latencies)
    stdev = statistics.stdev(latencies) if len(latencies) > 1 else 0
    min_val = min(latencies)
    max_val = max(latencies)
    p99 = sorted(latencies)[int(len(latencies) * 0.99)] if len(latencies) >= 100 else max_val

    print(f"\n{name}:")
    print(f"  Mean:   {mean / 1000:8.1f} µs")
    print(f"  Median: {median / 1000:8.1f} µs")
    print(f"  Stdev:  {stdev / 1000:8.1f} µs")
    print(f"  Min:    {min_val / 1000:8.1f} µs")
    print(f"  Max:    {max_val / 1000:8.1f} µs")
    if len(latencies) >= 100:
        print(f"  P99:    {p99 / 1000:8.1f} µs")


async def main():
    iterations = 100

    print("=" * 60)
    print("Wakeup Latency Benchmark")
    print("=" * 60)
    print(f"\nIterations: {iterations}")
    print("\nScenario: Python waits, Rust OS thread sends notification")

    # Warmup
    print("\nWarming up...")
    await bench_fd_wakeup(10)
    await bench_callback_wakeup(10)
    await bench_pure_python_wakeup(10)

    # Run benchmarks
    print("\nRunning benchmarks...")

    print("\n  FD-based (no GIL on Rust side)...")
    fd_mean, fd_latencies = await bench_fd_wakeup(iterations)

    print("  call_soon_threadsafe (GIL on Rust side)...")
    cb_mean, cb_latencies = await bench_callback_wakeup(iterations)

    print("  Pure Python (run_in_executor)...")
    py_mean, py_latencies = await bench_pure_python_wakeup(iterations)

    # Print results
    print("\n" + "=" * 60)
    print("Results")
    print("=" * 60)

    print_stats("FD-based (no GIL)", fd_latencies)
    print_stats("call_soon_threadsafe (GIL)", cb_latencies)
    print_stats("Pure Python (executor)", py_latencies)

    # Summary
    print("\n" + "=" * 60)
    print("Summary")
    print("=" * 60)
    print(f"\n{'Approach':<35} {'Mean':>10} {'Relative':>10}")
    print("-" * 55)
    print(f"{'FD-based (no GIL)':<35} {fd_mean/1000:>8.1f} µs {'1.0x':>10}")
    print(f"{'call_soon_threadsafe (GIL)':<35} {cb_mean/1000:>8.1f} µs {cb_mean/fd_mean:>9.1f}x")
    print(f"{'Pure Python (executor)':<35} {py_mean/1000:>8.1f} µs {py_mean/fd_mean:>9.1f}x")

    if cb_mean > fd_mean:
        print(f"\nFD-based is {cb_mean / fd_mean:.1f}x faster than call_soon_threadsafe")
    else:
        print(f"\ncall_soon_threadsafe is {fd_mean / cb_mean:.1f}x faster than FD-based")


if __name__ == "__main__":
    asyncio.run(main())
