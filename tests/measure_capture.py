"""Linux six-frame debug capture; configure a software ICD before running.

Uses the caller's driver environment. NOT a native startup benchmark.
"""
import json, os, pathlib, resource, subprocess, time
root = pathlib.Path(__file__).resolve().parents[1]
os.chdir(root)
env = dict(os.environ)
env.setdefault('WGPU_BACKEND', 'vulkan')
env.setdefault('MESA_SHADER_CACHE_DISABLE', 'true')
(root / 'artifacts').mkdir(exist_ok=True)
binary = root / 'target/debug/rust-notepad-prototype'
started = time.perf_counter()
run = subprocess.run([str(binary), '--screenshot', 'artifacts/measured.png', 'examples/welcome.txt'], env=env, capture_output=True, text=True, check=True)
result = {'kind': 'debug build; caller-configured Vulkan driver; six real UI frames plus PNG encoding; not native startup or steady-state memory',
          'elapsed_seconds': time.perf_counter()-started,
          'peak_child_rss_kib': resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss,
          'binary_bytes': binary.stat().st_size, 'stdout': run.stdout.strip()}
(root / 'artifacts/capture-measurement.json').write_text(json.dumps(result, indent=2)+'\n')
print(json.dumps(result, indent=2))
