param(
    [ValidateSet("lint", "check")]
    [string]$Mode = "check"
)

$ErrorActionPreference = "Stop"

function Invoke-Checked {
    param([string]$Command, [string[]]$Arguments)

    & $Command @Arguments
    if ($LASTEXITCODE -ne 0) {
        throw "command failed with exit code $LASTEXITCODE`: $Command $($Arguments -join ' ')"
    }
}

# Fork change (bd herdr-zs55, ADR-0020 amendment 1 in the herdr-AGENT wrapper): every cargo
# command runs once. Only the zig-fetch-retry prefetch, which the workflows run before this
# script, retries a transient Zig package download; a failed clippy is a verdict and is never
# run again, so no build output is classified here.
# Offline suite: scripts/test_windows_check.sh.
Invoke-Checked cargo @("fmt", "--check")
Invoke-Checked cargo @(
    "clippy",
    "--all-targets",
    "--locked",
    "--",
    "-D",
    "warnings"
)

if ($Mode -eq "lint") {
    return
}

Invoke-Checked just @("test")
Invoke-Checked cargo @("build", "--locked")
