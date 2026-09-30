#!/usr/bin/env python3
"""Generate/check the wire contract from a daemon on a disposable private bus."""
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import xml.etree.ElementTree as ET

ROOT = "/io/github/guilhem/DeviceCore1"
NAME = "io.github.guilhem.DeviceCore1"
repo = Path(__file__).resolve().parent.parent
binary = os.environ.get("DEVICE_CORE_BIN", str(repo / "target/debug/device-core"))
with tempfile.TemporaryDirectory(prefix="device-contract-") as temp:
    bus = subprocess.Popen(["dbus-daemon", "--session", "--nofork", "--print-address=1"], stdout=subprocess.PIPE, text=True)
    daemon = None
    try:
        address = bus.stdout.readline().strip()
        if not address:
            raise RuntimeError("private bus failed to start")
        env = os.environ.copy()
        for key in tuple(env):
            if key.startswith("DEVICE_CORE_"):
                del env[key]
        env.update(DEVICE_CORE_BUS_ADDRESS=address, DEVICE_CORE_DATA_DIR=temp,
                   DEVICE_CORE_NETWORK_GUARD=temp + "/network.lock")
        daemon = subprocess.Popen([binary, "--simulate"], env=env)
        def introspect(path):
            return subprocess.run(["gdbus", "introspect", "--address", address, "--dest", NAME,
                                   "--object-path", path, "--xml"], capture_output=True, text=True, timeout=5)
        deadline = time.monotonic() + 15
        while True:
            result = introspect(ROOT)
            if result.returncode == 0:
                break
            if daemon.poll() is not None or time.monotonic() >= deadline:
                raise RuntimeError(result.stderr)
            time.sleep(0.05)
        contract = ET.Element("node", name=ROOT)
        for domain in ["", "Audio", "Config", "Network", "System", "Updates", "Voice"]:
            result = introspect(ROOT + ("/" + domain if domain else ""))
            result.check_returncode()
            source = ET.fromstring(result.stdout)
            node = contract if not domain else ET.SubElement(contract, "node", name=domain)
            for interface in source.findall("interface"):
                if interface.attrib["name"].startswith(NAME + "."):
                    node.append(interface)
        # The callback belongs to clients; the service's object server cannot introspect it.
        node = ET.SubElement(contract, "node", name="Agent")
        interface = ET.SubElement(node, "interface", name=NAME + ".Agent")
        for method, inputs, outputs in [
            ("Acquire", [("operation", "s")], [("token", "s")]),
            ("Release", [("token", "s")], []),
            ("Abort", [("operation", "s")], []),
        ]:
            element = ET.SubElement(interface, "method", name=method)
            for direction, arguments in [("in", inputs), ("out", outputs)]:
                for name, signature in arguments:
                    ET.SubElement(element, "arg", name=name, type=signature, direction=direction)
        # D-Bus interface member order is not semantically significant.
        for interface in contract.iter("interface"):
            interface[:] = sorted(interface, key=lambda e: (e.tag, e.attrib.get("name", "")))
        ET.indent(contract, space="  ")
        xml = '<?xml version="1.0"?>\n' + ET.tostring(contract, encoding="unicode") + "\n"
        output = repo / "docs/dbus.xml"
        if "--check" in sys.argv:
            if output.read_text() != xml:
                raise RuntimeError("docs/dbus.xml differs from the running D-Bus contract")
        else:
            output.write_text(xml)
    finally:
        for process in [daemon, bus]:
            if process is not None:
                process.terminate()
                try:
                    process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
