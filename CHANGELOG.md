# Changelog

## 0.1.0

First standalone release of the Linux device services extracted from NabOS.

- Typed D-Bus APIs and optional HTTP for network, audio, configuration, clock,
  SSH, voice, maintenance and RAUC updates.
- Linux binaries for ARMv6 hard-float (Pi Zero), ARM64 and x86_64.
- SHA-256 checksums, corresponding source archive and cargo-dist manifest.

ARMv6 binaries require glibc 2.41; ARM64 and x86_64 binaries require glibc 2.39
or later. Linux deployment also requires the services and permissions documented
in the README. Release checks exercise ARMv6 with QEMU ARM1176 and the native
ARM64/x86_64 executables; real device boot, read-only root operation, audio and
RAUC rollback still require hardware qualification.
