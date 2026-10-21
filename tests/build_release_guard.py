"""Linux helper: build the configured release profile; stop if filesystem free space is unsafe."""
import os, pathlib, shutil, signal, subprocess, time
root = pathlib.Path(__file__).resolve().parents[1]
os.chdir(root)
env = dict(os.environ)
(root / 'artifacts').mkdir(exist_ok=True)
with open('artifacts/release-build.log', 'w') as log:
    process = subprocess.Popen(['cargo','build','--release','--locked','-j','2'], env=env, stdout=log, stderr=subprocess.STDOUT, start_new_session=True)
    while process.poll() is None:
        free = shutil.disk_usage(root).free
        print(f'Free space: {free//(1024*1024)} MiB', flush=True)
        if free < 250*1024*1024:
            os.killpg(process.pid, signal.SIGTERM)
            process.wait()
            raise SystemExit('Stopped build to preserve 250 MiB free disk threshold')
        time.sleep(8)
    print(f'Build exit: {process.returncode}')
    raise SystemExit(process.returncode)
