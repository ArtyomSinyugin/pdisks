#!/usr/bin/env python3
"""Boot a fresh, network-free C4 VM; never pass a host device or shared directory to QEMU.

Requires a trusted ALT rescue ISO with /boot/{vmlinuz,initrd.img}, static busybox,
QEMU, xorriso, cpio, zstd, and the installed libblockdev/LVM/ext4 stack.
"""

import argparse
import gzip
import pathlib
import re
import shutil
import subprocess
import tempfile


def run(*args, **kwargs):
    """Run an explicit argv, failing on every bootstrap error."""
    return subprocess.run(args, check=True, **kwargs)


def copy_with_libraries(source, root):
    """Copy a trusted local ELF and its resolved dependencies, dereferencing SONAME links."""
    source = pathlib.Path(source).resolve()
    destination = root / str(source).lstrip("/")
    destination.parent.mkdir(parents=True, exist_ok=True)
    shutil.copy2(source, destination)
    result = run("ldd", str(source), capture_output=True, text=True)
    if "not found" in result.stdout:
        raise RuntimeError(result.stdout)
    for library in re.findall(r"(?:=>\s*)?(/\S+)", result.stdout):
        path = pathlib.Path(library)
        if path.is_file():
            target = root / library.lstrip("/")
            target.parent.mkdir(parents=True, exist_ok=True)
            shutil.copy2(path, target)
    return destination


def pack(root, target):
    """Pack this new private tree without requiring host root ownership."""
    with target.open("wb") as output:
        find = subprocess.Popen(["find", ".", "-print0"], cwd=root, stdout=subprocess.PIPE)
        cpio = subprocess.Popen(
            ["cpio", "--null", "-o", "-H", "newc", "--owner=0:0"],
            cwd=root, stdin=find.stdout, stdout=subprocess.PIPE,
        )
        find.stdout.close()
        try:
            run("gzip", "-1", stdin=cpio.stdout, stdout=output)
        finally:
            cpio.stdout.close()
        if cpio.wait() or find.wait():
            raise RuntimeError("initramfs packaging failed")


def main():
    """Build and run the bounded recipe exclusively against a newly allocated regular image."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rescue-iso", required=True, type=pathlib.Path)
    parser.add_argument("--case", choices=["success", "mount-target-change"], default="success")
    args = parser.parse_args()
    iso = args.rescue_iso.resolve(strict=True)
    repo = pathlib.Path(__file__).resolve().parents[3]
    run("cargo", "build", "-p", "storage-runtime", "--example", "c4_vm", "--offline", cwd=repo)
    base = pathlib.Path(tempfile.mkdtemp(prefix="disksgraph-c4-"))
    print(f"Artifacts: {base}", flush=True)
    root = base / "root"
    for directory in ["bin", "usr/bin", "usr/sbin", "usr/lib64", "lib64", "lib/modules", "proc", "sys", "dev", "run", "tmp", "mnt/c4", "etc/lvm"]:
        (root / directory).mkdir(parents=True, exist_ok=True)
    for name, iso_path in [("vmlinuz", "/boot/vmlinuz"), ("rescue-initrd", "/boot/initrd.img")]:
        run("xorriso", "-osirrox", "on", "-indev", str(iso), "-extract", iso_path, str(base / name), stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    modules = base / "modules"
    modules.mkdir()
    with gzip.open(base / "rescue-initrd", "rb") as archive, (base / "initrd.cpio").open("wb") as output:
        shutil.copyfileobj(archive, output)
    with (base / "initrd.cpio").open("rb") as archive:
        run("cpio", "-id", "--no-absolute-filenames", "usr/lib/modules/*", cwd=modules, stdin=archive, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    names = ["virtio_ring", "virtio", "virtio_pci_modern_dev", "virtio_pci_legacy_dev", "virtio_pci", "scsi_common", "scsi_mod", "sd_mod", "virtio_scsi", "dm-mod"]
    for name in names:
        matches = list(modules.rglob(f"{name}.ko.zst"))
        if len(matches) != 1:
            raise RuntimeError(f"expected one kernel module: {name}, found {matches}")
        run("zstd", "-dq", str(matches[0]), "-o", str(root / "lib/modules" / f"{name}.ko"))
    busybox = pathlib.Path(shutil.which("busybox") or "/usr/bin/busybox")
    binary_kind = run("file", str(busybox), capture_output=True, text=True).stdout
    if not any(kind in binary_kind for kind in ["statically linked", "static-pie linked"]):
        raise RuntimeError("static busybox required")
    shutil.copy2(busybox, root / "usr/bin/busybox")
    for name in ["sh", "mount", "sleep", "insmod", "poweroff", "cat", "sync"]:
        (root / "bin" / name).symlink_to("/usr/bin/busybox")
    (root / "sbin").symlink_to("usr/sbin")
    for tool in ["lvm", "dmsetup", "mke2fs", "tune2fs", "dumpe2fs", "e2fsck", "blkid"]:
        copy_with_libraries(pathlib.Path("/usr/sbin") / tool, root)
    for name in ["pvcreate", "vgcreate", "lvcreate", "pvs", "vgs", "lvs", "lvchange", "vgchange"]:
        (root / "usr/sbin" / name).symlink_to("lvm")
    (root / "usr/sbin/mkfs.ext4").symlink_to("mke2fs")
    for library in ["libbd_lvm.so.3", "libbd_fs.so.3", "libblkid.so.1", "libmount.so.1"]:
        # Keep the SONAME path expected by the guest registration as well as resolved dependencies.
        source = pathlib.Path("/usr/lib64") / library
        copy_with_libraries(source, root)
        shutil.copy2(source, root / "usr/lib64" / library)
    binary = copy_with_libraries(repo / "target/debug/examples/c4_vm", root)
    shutil.move(binary, root / "usr/bin/c4_vm")
    run("strip", str(root / "usr/bin/c4_vm"))
    shutil.copy2("/etc/mke2fs.conf", root / "etc/mke2fs.conf")
    (root / "etc/lvm/lvm.conf").write_text("devices { use_devicesfile=0 }\nactivation { udev_sync=0 udev_rules=0 }\n")
    (root / "init").write_text("#!/bin/sh\nexport PATH=/usr/sbin:/usr/bin:/bin\nmount -t proc proc /proc\nmount -t sysfs sysfs /sys\nmount -t devtmpfs devtmpfs /dev\n" + "".join(f"insmod /lib/modules/{name}.ko || poweroff -f\n" for name in names) + f"sleep 1\n/usr/bin/c4_vm {'' if args.case == 'success' else args.case}\nstatus=$?\necho C4_EXIT=$status\ncat /run/c4-execution/active-attempt.json\nsync\npoweroff -f\n")
    (root / "init").chmod(0o755)
    pack(root, base / "guest.cpio.gz")
    disk = base / "data.raw"
    with disk.open("xb") as output:
        output.truncate(512 * 1024 * 1024)
    log = base / "serial.log"
    with log.open("w") as output:
        run("qemu-system-x86_64", "-accel", "tcg", "-cpu", "max", "-m", "768", "-smp", "2", "-nodefaults", "-display", "none", "-serial", "stdio", "-no-reboot", "-kernel", str(base / "vmlinuz"), "-initrd", str(base / "guest.cpio.gz"), "-append", "console=ttyS0 quiet panic=-1 random.trust_cpu=on disksgraph.c4=1", "-drive", f"file={disk},format=raw,if=none,id=c4", "-device", "virtio-scsi-pci,id=scsi", "-device", "scsi-hd,drive=c4,bus=scsi.0,serial=DISKSGRAPH_C4_TEST,wwn=0x5000c40000000001", stdout=output, stderr=subprocess.STDOUT, timeout=180)
    result = log.read_text()
    print(result)
    sentinel = "C4_VERTICAL_VERIFIED" if args.case == "success" else "C4_EXPECTED_FAILURE_VERIFIED"
    if f"{sentinel}\n" not in result or "C4_EXIT=0\n" not in result:
        raise SystemExit(f"C4 experiment failed; retained {log} and {disk}")


if __name__ == "__main__":
    main()
