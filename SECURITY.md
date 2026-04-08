# Security Policy

## Supported Versions

Only the latest release on the `main` branch is supported with security updates. We do not backport fixes to older versions.

| Branch | Supported |
|--------|-----------|
| `main` | Yes |
| Other  | No  |

## Reporting a Vulnerability

**Please do not open a public issue for security vulnerabilities.**

Use GitHub's private vulnerability reporting to submit a report:

1. Go to the [Security tab](https://github.com/jasonherald/rtl-sdr/security)
2. Click **"Report a vulnerability"**
3. Provide a description, steps to reproduce, and any relevant details

### What to expect

- **Acknowledgment** within 48 hours
- **Assessment** of severity and impact within 1 week
- **Fix or mitigation** as soon as practical, depending on severity
- **Disclosure** 90 days after the fix is released, or immediately if the vulnerability is already public
- Credit in the fix commit (unless you prefer to remain anonymous)

## Security Scanning

This project uses automated security scanning across multiple layers:

| Tool | Integration | Coverage |
|------|-------------|----------|
| [cargo-audit](https://rustsec.org/) | GitHub Actions (PR + weekly) | Known CVEs in Rust dependencies (RustSec advisory database) |
| [cargo-deny](https://embarkstudios.github.io/cargo-deny/) | GitHub Actions (PR + weekly) | License compliance, duplicate crates, source restrictions |
| [CodeRabbit](https://coderabbit.ai/) | GitHub App (PR review) | AI-assisted code review with OSV dependency scanning |

## Scope

This project is a software-defined radio application (Rust port of SDR++). It:

- Communicates with RTL-SDR USB hardware via libusb
- Receives and demodulates radio signals (FM, AM, SSB, CW)
- Renders spectrum/waterfall displays via OpenGL
- Outputs audio via PipeWire
- Reads/writes configuration files and frequency bookmarks (JSON)
- Accepts network IQ streams (TCP/UDP)

Vulnerabilities in USB communication, network protocol handling, file parsing, or configuration handling are in scope.

### Out of scope

- Bugs in SDR++ C++ source (report upstream)
- Bugs in RTL-SDR hardware firmware
- Denial of service via trivially malformed input (e.g., empty files)
- Vulnerabilities in PipeWire, Mesa, GTK, or system libraries — report upstream
- Social engineering or phishing
- Issues requiring physical access to the machine
