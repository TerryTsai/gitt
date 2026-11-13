#!/bin/sh
# Full-system ARM64 compatibility checks. Requires Python 3, Git (fixtures only),
# cpio, gzip, and qemu-system-aarch64; never installs packages or requires root.
exec python3 - "$@" <<'PY'
import argparse
import gzip
import hashlib
import inspect
import json
from pathlib import Path
import re
import shutil
import struct
import subprocess
import sys
import tarfile
import tempfile
import time
import urllib.request


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verified_archive(path):
    checksum = Path(str(path) + ".sha256")
    fields = checksum.read_text().split()
    if len(fields) != 2 or fields[1].lstrip("*") != path.name:
        raise RuntimeError(f"Unexpected checksum file: {checksum}")
    actual = digest(path)
    if actual != fields[0]:
        raise RuntimeError(f"SHA-256 mismatch: {path}")
    return actual


parser = argparse.ArgumentParser(description="Boot a disposable ARM64 Linux guest and exercise the release binary, including Landlock and creation seccomp. No host mounts enter the guest. All temporary files are retained.")
parser.add_argument("--binary", type=Path, required=True)
parser.add_argument("--rootfs", type=Path, required=True, help="Official Alpine ARM64 minirootfs tar.gz, with adjacent .sha256")
parser.add_argument("--netboot", type=Path, required=True, help="Official matching Alpine ARM64 netboot tar.gz, with adjacent .sha256")
parser.add_argument("--qemu", default="qemu-system-aarch64")
parser.add_argument("--output", type=Path, required=True, help="New result directory; existing destinations are refused")
parser.add_argument("--port", type=int, default=18083)
args = parser.parse_args()
repository = Path.cwd()
if not (repository / "tests/smoke.py").is_file():
    raise SystemExit("Run this script from the gitt repository root.")
args.binary = args.binary.resolve()
args.rootfs = args.rootfs.resolve()
args.netboot = args.netboot.resolve()
args.output = args.output.resolve()
args.output.mkdir(parents=True, exist_ok=False)
work = Path(tempfile.mkdtemp(prefix="gitt-arm-validation-run-"))
root = work / "rootfs"
root.mkdir()
report = {
    "schema": 1,
    "purpose": "ARM64 Linux compatibility and security-boundary checks, not hardware performance measurements",
    "work_directory": str(work),
    "binary_sha256": digest(args.binary),
    "archives": {args.rootfs.name: verified_archive(args.rootfs), args.netboot.name: verified_archive(args.netboot)},
    "qemu": subprocess.check_output([args.qemu, "--version"], text=True).splitlines()[0],
    "machine": "virt", "cpu": "cortex-a57", "acceleration": "TCG", "vcpus": 2, "memory_mib": 512,
    "server_uid": 65534,
    "host_mounts_in_guest": False,
}
with tarfile.open(args.rootfs) as archive:
    # These are checksum-verified, official distribution archives. The rootfs
    # intentionally includes normal absolute /bin and /usr symlinks.
    options = {"filter": "fully_trusted"} if "filter" in inspect.signature(archive.extractall).parameters else {}
    archive.extractall(root, **options)
boot = work / "boot"
boot.mkdir()
with tarfile.open(args.netboot) as archive:
    for name in ["vmlinuz-virt", "initramfs-virt"]:
        stream = archive.extractfile("boot/" + name)
        assert stream
        with (boot / name).open("wb") as destination:
            shutil.copyfileobj(stream, destination)
    config_names = [n for n in archive.getnames() if n.startswith("boot/config-") and n.endswith("-virt")]
    if len(config_names) != 1:
        raise RuntimeError("Expected one virt kernel configuration")
    stream = archive.extractfile(config_names[0])
    assert stream
    config = stream.read()
    (args.output / "kernel.config").write_bytes(config)
    for setting in [b"CONFIG_SECURITY_LANDLOCK=y", b"CONFIG_SECCOMP_FILTER=y"]:
        if setting not in config.splitlines():
            raise RuntimeError(f"Guest kernel lacks {setting!r}")

# Alpine's EFI zboot wrapper contains a gzip-compressed native ARM64 Image.
# Header layout: linux/drivers/firmware/efi/libstub/zboot-header.S.
kernel = (boot / "vmlinuz-virt").read_bytes()
if kernel[4:8] == b"zimg":
    offset, length = struct.unpack_from("<II", kernel, 8)
    if kernel[24:28] != b"gzip":
        raise RuntimeError("Only gzip EFI zboot kernels are supported")
    kernel = gzip.decompress(kernel[offset:offset + length])
if kernel[56:60] != b"ARM\x64":
    raise RuntimeError("Not a native ARM64 Linux Image")
(boot / "Image").write_bytes(kernel)
with gzip.open(boot / "initramfs-virt", "rb") as stream:
    subprocess.run(["cpio", "-idm", "--no-absolute-filenames", "usr/lib/modules/*"], input=stream.read(), cwd=root, check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
(root / "lib/modules").symlink_to("../usr/lib/modules")
shutil.copy2(args.binary, root / "usr/bin/gitt")
with (args.output / "fixtures.log").open("w") as log:
    subprocess.run([sys.executable, "scripts/fixtures.py", str(root / "fixtures"), "--repositories", "7", "--security", "--large-mib", "9"], check=True, stdout=log)
    subprocess.run([sys.executable, "scripts/native-limit-fixture.py", str(root / "fixtures/repositories/delta-bomb.git")], check=True, stdout=log)
manifest = json.loads((root / "fixtures/manifest.json").read_text())
metadata = manifest["metadata_paths"] + ["delta-bomb.git/config", "delta-bomb.git/HEAD"]
(root / "validation-original.sha256").write_text("".join(f"{digest(root / 'fixtures/repositories' / name)}  /fixtures/repositories/{name}\n" for name in metadata))
permission_paths = {"/fixtures/repositories"}
for name in metadata:
    path = Path("/fixtures/repositories") / name
    permission_paths.add(str(path))
    permission_paths.update(str(parent) for parent in path.parents if str(parent).startswith("/fixtures/repositories"))
(root / "validation-permission-paths").write_text("\n".join(sorted(permission_paths)) + "\n")
origin = f"http://127.0.0.1:{args.port}"
init = r'''#!/bin/sh
export PATH=/usr/sbin:/usr/bin:/sbin:/bin
mount -t proc proc /proc
mount -t sysfs sysfs /sys
mount -t devtmpfs devtmpfs /dev
mkdir -p /sys/kernel/security /dev/pts /run /tmp
mount -t securityfs securityfs /sys/kernel/security
mount -t devpts devpts /dev/pts
chmod 1777 /tmp
printf '\nGITT_ARM_VM_BEGIN\n'
uname -a
cat /etc/alpine-release
printf 'Enabled Linux security modules: '
cat /sys/kernel/security/lsm
printf '\n'
modprobe virtio_net
ip link set lo up
ip link set eth0 up
ip addr add 10.0.2.15/24 dev eth0
ip route add default via 10.0.2.2
printf 'Git executable available: '
command -v git || echo no
sha256sum /usr/bin/gitt
chown -R nobody:nobody /fixtures
chmod 0755 /fixtures
while IFS= read -r item; do stat -c '%a:%u:%g' "$item"; done < /validation-permission-paths > /permissions-before
su -s /bin/sh -c 'exec /usr/bin/gitt /fixtures/repositories --bind 0.0.0.0:18080 --origin ORIGIN_PLACEHOLDER' nobody &
sleep 2
for pid in $(pidof gitt); do
  printf 'Server process %s:\n' "$pid"
  grep -E '^(Name|Uid|Gid|NoNewPrivs|Seccomp):' /proc/$pid/status
done
printf 'GITT_ARM_VM_SERVICE_LAUNCHED\n'
while [ ! -d /fixtures/repositories/arm-validation-finished ]; do sleep 1; done
failed=0
sha256sum -c /validation-original.sha256 || failed=1
while IFS= read -r item; do stat -c '%a:%u:%g' "$item"; done < /validation-permission-paths > /permissions-after
cmp /permissions-before /permissions-after || failed=1
for marker in filter-executed diff-executed fsmonitor-executed hook-executed; do
  if [ -e "/fixtures/repositories/security/filters/.git/$marker" ]; then echo "Unexpected executed marker: $marker"; failed=1; fi
done
created_count=0
for created in /fixtures/repositories/created-by-smoke-*; do
  [ -d "$created" ] || continue
  created_count=$((created_count + 1))
  for kind in bare checkout; do
    if [ "$kind" = bare ]; then metadata="$created/bare.git"; branch=main; bare=true
    else metadata="$created/checkout with spaces/.git"; branch=trunk; bare=false; fi
    [ "$(cat "$metadata/HEAD")" = "ref: refs/heads/$branch" ] || failed=1
    grep -Eq "bare[[:space:]]*=[[:space:]]*$bare" "$metadata/config" || failed=1
    if grep -E 'updateInstead|denyCurrentBranch' "$metadata/config"; then failed=1; fi
    for item in "$metadata" "$metadata/HEAD" "$metadata/config"; do
      stat -c 'Created %n %a:%u:%g' "$item"
      mode=$(stat -c '%a' "$item")
      [ $((0$mode & 020)) -ne 0 ] || failed=1
    done
  done
done
[ "$created_count" -ge 1 ] || failed=1
[ "$(stat -c '%a:%u' /fixtures/repositories/.gitt-create-private-65534)" = '700:65534' ] || failed=1
printf 'GITT_ARM_VM_FILESYSTEM_CHECKS=%s\n' "$failed"
sync
poweroff -f
'''.replace("ORIGIN_PLACEHOLDER", origin)
(root / "init").write_text(init)
(root / "init").chmod(0o755)
image = work / "initramfs.cpio.gz"
with image.open("wb") as destination:
    subprocess.run(["bash", "-o", "pipefail", "-c", "find . -print0 | cpio --null -o --format=newc --owner=0:0 2>/dev/null | gzip -1"], cwd=root, stdout=destination, check=True)
command = [args.qemu, "-machine", "virt", "-cpu", "cortex-a57", "-accel", "tcg", "-smp", "2", "-m", "512M", "-kernel", str(boot / "Image"), "-initrd", str(image), "-append", "console=ttyAMA0 rdinit=/init panic=-1 loglevel=5", "-display", "none", "-monitor", "none", "-serial", "file:" + str(args.output / "console.log"), "-netdev", f"user,id=gittnet,hostfwd=tcp:127.0.0.1:{args.port}-:18080", "-device", "virtio-net-pci,netdev=gittnet,romfile=", "-no-reboot"]
report["qemu_command"] = command
process = subprocess.Popen(command, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
try:
    deadline = time.monotonic() + 180
    while True:
        if process.poll() is not None:
            raise RuntimeError("QEMU exited before readiness: " + process.stderr.read().decode(errors="replace"))
        console_path = args.output / "console.log"
        console = console_path.read_text() if console_path.exists() else ""
        # Never send tests to an unrelated listener if QEMU could not claim the
        # chosen port: readiness must first come from this guest's own serial log.
        if "GITT_ARM_VM_SERVICE_LAUNCHED" in console and report["binary_sha256"] in console:
            try:
                with urllib.request.urlopen(origin + "/", timeout=2) as response:
                    if response.status == 200:
                        break
            except (OSError, TimeoutError):
                pass
        if time.monotonic() > deadline:
            raise RuntimeError("ARM guest did not become ready within 180 seconds")
        time.sleep(.2)
    with (args.output / "smoke.log").open("w") as log:
        result = subprocess.run([sys.executable, "tests/smoke.py", "--base-url", origin, "--origin", origin, "--fixtures", str(root / "fixtures"), "--http-only", "--native-delta"], stdout=log, stderr=subprocess.STDOUT, timeout=180)
    report["smoke_exit_code"] = result.returncode
    sys.path.insert(0, str(repository / "tests"))
    import smoke
    client = smoke.Client(origin, origin)
    token = smoke.Document(client.request("/new")[2]).inputs["csrf"]
    status, _, _ = client.request("/new", form={"path": "arm-validation-finished", "kind": "bare", "branch": "main", "csrf": token})
    if status != 303:
        raise RuntimeError(f"Could not request final guest checks: {status}")
    process.wait(timeout=30)
    console = (args.output / "console.log").read_text()
    report["filesystem_checks_passed"] = "GITT_ARM_VM_FILESYSTEM_CHECKS=0" in console
    report["landlock_abi"] = next((int(line.split("Landlock ABI ")[1].split(")")[0]) for line in console.splitlines() if "Landlock ABI " in line), None)
    report["kernel"] = next((line for line in console.splitlines() if line.startswith("Linux ")), None)
    report["unprivileged_server"] = bool(re.search(r"^Uid:\s+65534\s+65534\s+65534\s+65534$", console, re.M))
    report["no_new_privileges"] = bool(re.search(r"^NoNewPrivs:\s+1$", console, re.M))
    report["no_git_executable"] = "Git executable available: no" in console
    report["passed"] = result.returncode == 0 and report["filesystem_checks_passed"] and (report["landlock_abi"] or 0) >= 3 and report["unprivileged_server"] and report["no_new_privileges"] and report["no_git_executable"]
finally:
    if process.poll() is None:
        process.terminate()
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
    report["qemu_exit_code"] = process.returncode
    (args.output / "report.json").write_text(json.dumps(report, indent=2) + "\n")
print(json.dumps(report, indent=2))
raise SystemExit(0 if report.get("passed") else 1)
PY
