# Project-local release commands. Requires just and Nushell.
set positional-arguments
set script-interpreter := ["nu"]

[private]
[script]
default:
    just --list

# Run tests, lint, release-helper tests, and source-package verification.
[script]
check:
    nu release/check.nu

# Preview a SemVer bump; --apply commits/tags it, --release also publishes a GitHub Release.
[script]
version kind *args:
    def --wrapped main [...args] { nu release/version.nu ...$args }
